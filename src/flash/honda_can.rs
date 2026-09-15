use super::parsers::honda_rwd::HondaRwdParseResult;
use super::parsers::HondaRwdParser;
use super::{FlashOptions, FlashPermit, FlashStrategy, ImageInfo, PreparedFlash, StrategyFactory};
use crate::errors::DiagError;
use crate::obd::protocol::{ObdProtocol, VOLTAGE_PIN_OFF};
use crate::obd::uds;
use crate::{
    diag::EcuInfo,
    events::{Cancel, Event, ProgressUnit},
    obd::addressing::EcuAddress,
    preflight::{CheckStatus, Preflight},
    vehicle::SystemVoltage,
};
#[cfg(test)]
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

/// OBD connector pin 12 — Honda's WEN (Write enable) line.
const PROGRAMMING_VOLTAGE_PIN: u32 = 12;

/// The write-enable voltage, in mV.
const PROGRAMMING_VOLTAGE_MV: u32 = 5000;

/// How far the measured programming voltage may stray from the request, in
/// percent, before the flash is abandoned. To be checked against a bench
/// measurement.
const PROGRAMMING_VOLTAGE_TOLERANCE_PERCENT: u32 = 10;

// Application policy per 12 V of nominal lead-acid battery voltage.
// These are not ECU-specific programming limits.
const BATTERY_MIN_MV: u32 = 12_000;
const BATTERY_RECOMMENDED_MIN_MV: u32 = 12_400;
const BATTERY_MAX_MV: u32 = 15_000;

/// How long to let programming voltage settle before talking to the ECU.
/// Zero under test, where the fake ECU has no rail to charge and the suite
/// would otherwise sit through it once per flash test.
#[cfg(not(test))]
const VOLTAGE_SETTLE_TIME: Duration = Duration::from_secs(1);
#[cfg(test)]
const VOLTAGE_SETTLE_TIME: Duration = Duration::ZERO;

/// Holds programming voltage on for as long as it is alive and drops it on the
/// way out — including when the flash sequence panics or returns early. Leaving
/// the pin energised after an aborted flash is the one failure mode worth
/// spending a destructor on.
struct ProgrammingVoltage<'p> {
    protocol: &'p mut ObdProtocol,
    pin: u32,
}

impl<'p> ProgrammingVoltage<'p> {
    fn apply(protocol: &'p mut ObdProtocol, pin: u32, millivolts: u32) -> Result<Self, DiagError> {
        protocol.set_programming_voltage(pin, millivolts)?;
        Ok(Self { protocol, pin })
    }
}

impl Drop for ProgrammingVoltage<'_> {
    fn drop(&mut self) {
        self.protocol.emit(Event::Detail(
            "[HONDA_CAN] Disabling programming voltage...",
        ));
        if let Err(e) = self
            .protocol
            .set_programming_voltage(self.pin, VOLTAGE_PIN_OFF)
        {
            self.protocol.emit(Event::Warning(&format!(
                "Failed to turn off programming voltage: {e}"
            )));
        }
    }
}

/// Refuses to go on when the adapter measures a voltage that isn't what was
/// requested: an ECU erased without a real write-enable may not reprogram.
/// Adapters that can't measure it get a warning instead.
fn check_programming_voltage(
    protocol: &mut ObdProtocol,
    requested_mv: u32,
) -> Result<(), DiagError> {
    use crate::j2534::consts::{ERR_IOCTL_ID_NOT_SUPPORTED, ERR_NOT_SUPPORTED};

    let measured = match protocol.read_programming_voltage_mv() {
        Ok(mv) => mv,
        Err(DiagError::PassThru { code, .. })
            if code == ERR_NOT_SUPPORTED || code == ERR_IOCTL_ID_NOT_SUPPORTED =>
        {
            protocol.emit(Event::Warning(
                "the adapter can't measure programming voltage; continuing unchecked",
            ));
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let allowed = requested_mv * PROGRAMMING_VOLTAGE_TOLERANCE_PERCENT / 100;
    if measured.abs_diff(requested_mv) > allowed {
        return Err(DiagError::Adapter(format!(
            "Programming voltage measured {measured} mV, expected {requested_mv} mV ±{PROGRAMMING_VOLTAGE_TOLERANCE_PERCENT}%; not flashing"
        )));
    }
    protocol.emit(Event::Detail(&format!(
        "[HONDA_CAN] Programming voltage measured: {measured} mV"
    )));
    Ok(())
}

fn battery_voltage_check(
    protocol: &mut ObdProtocol,
    system: SystemVoltage,
) -> Result<(String, CheckStatus), DiagError> {
    let nominal = system.volts();
    let scale = u32::from(nominal) / 12;
    let minimum = BATTERY_MIN_MV * scale;
    let recommended_minimum = BATTERY_RECOMMENDED_MIN_MV * scale;
    let maximum = BATTERY_MAX_MV * scale;
    let mv = match protocol.read_battery_mv() {
        Ok(mv) => mv,
        Err(error) if error.kind() == crate::errors::ErrorKind::Unsupported => {
            return Ok((
                format!("unavailable: {error}; verify supply voltage for the {nominal} V system before programming"),
                CheckStatus::Warn,
            ));
        }
        Err(error) => return Err(error),
    };
    let voltage = mv as f64 / 1000.0;
    if mv < minimum {
        return Ok((
            format!(
                "{voltage:.2} V (below {:.2} V minimum for a {nominal} V system; charge the battery or connect a regulated programming power supply)",
                minimum as f64 / 1000.0
            ),
            CheckStatus::Fail,
        ));
    }
    if mv > maximum {
        return Ok((
            format!(
                "{voltage:.2} V (above {:.2} V maximum for a {nominal} V system; check the supply voltage)",
                maximum as f64 / 1000.0
            ),
            CheckStatus::Fail,
        ));
    }
    if mv < recommended_minimum {
        return Ok((
            format!("{voltage:.2} V (low margin for a {nominal} V system; charge the battery or connect a regulated programming power supply)"),
            CheckStatus::Warn,
        ));
    }
    Ok((
        format!(
            "{voltage:.2} V (within {:.2}-{:.2} V preflight range for a {nominal} V system)",
            recommended_minimum as f64 / 1000.0,
            maximum as f64 / 1000.0
        ),
        CheckStatus::Ok,
    ))
}

fn calculate_unlock_key(seed: u16, key: &[u8]) -> Result<u16, DiagError> {
    let sa_key = match key.len() {
        6 => {
            let k1 = u16::from_be_bytes([key[0], key[1]]);
            let k2 = u16::from_be_bytes([key[2], key[3]]);
            let k3 = u16::from_be_bytes([key[4], key[5]]);
            if k3 == 0 {
                return Err(DiagError::InvalidResponse(
                    "Security key modulus (K3) is zero".to_string(),
                ));
            }
            (((seed as u32 * k2 as u32 % k3 as u32) ^ (seed as u32 + k1 as u32)) & 0xFFFF) as u16
        }
        4 => {
            let k1 = u16::from_be_bytes([key[0], key[1]]);
            let k2 = u16::from_be_bytes([key[2], key[3]]);

            (((seed as u32 * k2 as u32) ^ (seed as u32 + k1 as u32)) & 0xFFFF) as u16
        }
        _ => {
            return Err(DiagError::InvalidResponse("Invalid Key length".to_string()));
        }
    };

    Ok(sa_key)
}

/// A normalized flash request, independent of whether the firmware came from a
/// `.rwd` container or a raw `.bin`. Both source paths build one of these and
/// hand it to [`HondaCanBusStrategy::perform_flash`].
struct FlashJob {
    system_voltage: SystemVoltage,
    /// Download start address for RequestDownload.
    start_address: u32,
    /// The raw firmware bytes to transfer.
    firmware: Vec<u8>,
    /// Value written to DID 0xF101 before download. For encrypted `.rwd`
    /// firmware this is the real decryption key; for a plaintext `.bin` it is
    /// zeros, telling the ECU not to decrypt.
    decryption_key: Vec<u8>,
    /// The SecurityAccess secret (K1/K2/K3 constants) fed into
    /// [`calculate_unlock_key`] together with the ECU seed.
    unlock_secret: Vec<u8>,
}

pub struct HondaCanBusStrategy<'a> {
    protocol: &'a mut ObdProtocol,
    job: Option<FlashJob>,
}

impl<'a> HondaCanBusStrategy<'a> {
    pub fn new(protocol: &'a mut ObdProtocol) -> Self {
        Self {
            protocol,
            job: None,
        }
    }

    /// Reads the live ECU application-software (calibration) identifier.
    fn read_calibration_id(&mut self) -> Result<String, DiagError> {
        let raw = uds::read_data_by_identifier(
            self.protocol,
            uds::DataIdentifier::ApplicationSoftwareIdentification as u16,
        )?;
        Ok(String::from_utf8_lossy(&raw).trim_matches('\0').to_string())
    }

    /// Resolves the SecurityAccess secret, in priority order:
    /// 1. `--security-key` (explicit constants) — always wins.
    /// 2. `--tokens <file.rwd>` — token table matched by the live calibration id.
    /// 3. `embedded` — the token table from the `.rwd` being flashed (rwd path only).
    ///
    /// Fails loudly if none of the sources yields a token.
    fn resolve_unlock_secret(
        &mut self,
        calibration_id: &str,
        opts: &FlashOptions,
        embedded: Option<&HondaRwdParseResult>,
    ) -> Result<Vec<u8>, DiagError> {
        if let Some(key) = &opts.security_key {
            if key.len() != 4 && key.len() != 6 {
                return Err(DiagError::InvalidCommand(format!(
                    "--security-key must be 4 or 6 bytes, got {}",
                    key.len()
                )));
            }
            self.protocol.emit(Event::Detail(
                "[HONDA_CAN] Using security key from --security-key",
            ));
            return Ok(key.clone());
        }

        if let Some(data) = &opts.token_data {
            let rwd = HondaRwdParser::new().parse(data).map_err(|e| {
                DiagError::InvalidResponse(format!("Failed to parse --tokens file: {e}"))
            })?;
            if let Some(token) = Self::lookup_token(&rwd, calibration_id) {
                self.protocol.emit(Event::Detail(&format!(
                    "[HONDA_CAN] Using security token from --tokens for version: {calibration_id}"
                )));
                return Ok(token);
            }
            return Err(Self::no_token_error(&rwd, calibration_id));
        }

        if let Some(rwd) = embedded {
            if let Some(token) = Self::lookup_token(rwd, calibration_id) {
                self.protocol.emit(Event::Detail(&format!(
                    "[HONDA_CAN] Found compatible security token for version: {calibration_id}"
                )));
                return Ok(token);
            }
            return Err(Self::no_token_error(rwd, calibration_id));
        }

        Err(DiagError::InvalidCommand(
            "No security-access secret available. Provide --security-key <hex> or --tokens <file.rwd>".into(),
        ))
    }

    /// Looks up the security token for a calibration id in a parsed `.rwd` table.
    fn lookup_token(rwd: &HondaRwdParseResult, calibration_id: &str) -> Option<Vec<u8>> {
        rwd.compatible_versions
            .get(calibration_id.as_bytes())
            .cloned()
    }

    /// Builds a descriptive error listing the versions a token table does cover.
    fn no_token_error(rwd: &HondaRwdParseResult, calibration_id: &str) -> DiagError {
        let mut msg =
            format!("No security token for ECU version '{calibration_id}'. Available versions:");
        for version_key in rwd.compatible_versions.keys() {
            msg.push_str(&format!("\n  - {}", String::from_utf8_lossy(version_key)));
        }
        DiagError::InvalidResponse(msg)
    }

    /// The shared programming sequence used by every source format.
    ///
    /// Applies programming voltage and hands off to [`Self::run_programming_sequence`].
    /// The voltage is held by a guard, so it comes back off however that call
    /// ends — success, error, or panic.
    fn perform_flash(&mut self, job: FlashJob, cancel: &Cancel) -> Result<(), DiagError> {
        // Confirmation may have left the ignition on long enough to drain the battery.
        let (battery, status) = battery_voltage_check(self.protocol, job.system_voltage)?;
        match status {
            CheckStatus::Fail => {
                return Err(DiagError::PreflightFailed(format!("battery: {battery}")));
            }
            CheckStatus::Warn => self
                .protocol
                .emit(Event::Warning(&format!("Battery: {battery}"))),
            _ => {}
        }

        // Apply 5V on Pin 12 - PWE to able to flash
        let power = ProgrammingVoltage::apply(
            &mut *self.protocol,
            PROGRAMMING_VOLTAGE_PIN,
            PROGRAMMING_VOLTAGE_MV,
        )?;
        sleep(VOLTAGE_SETTLE_TIME); // wait for voltage to stabilize

        // Nothing has been erased yet, so a bad supply stops the flash cleanly.
        let result = check_programming_voltage(power.protocol, PROGRAMMING_VOLTAGE_MV)
            .and_then(|()| Self::run_programming_sequence(power.protocol, &job, cancel));

        // Drop the voltage before reporting, so the last thing printed on a
        // successful flash is the success and not the teardown.
        drop(power);
        result?;

        self.protocol.emit(Event::Detail("Done!"));
        Ok(())
    }

    /// Unlocks via SecurityAccess, enters the programming session, erases,
    /// writes the decryption key, then downloads and transfers the firmware.
    ///
    /// Runs with programming voltage applied, so every failure in here must be
    /// an error rather than a panic: the caller's guard can only drop the
    /// voltage if the stack unwinds through it.
    fn run_programming_sequence(
        protocol: &mut ObdProtocol,
        job: &FlashJob,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        cancel.check()?;
        uds::diagnostic_session(protocol, uds::SessionType::ExtendedDiagnostic)?;
        cancel.check()?;

        let seed_bytes: [u8; 2] =
            uds::security_request_seed(protocol, uds::AccessType::RequestSeed)?
                .try_into()
                .map_err(|_| {
                    DiagError::InvalidResponse("Unexpected SecurityAccess seed length".into())
                })?;
        let seed = u16::from_be_bytes(seed_bytes);
        protocol.emit(Event::Detail(
            "Preparing security and encryption parameters",
        ));

        let unlock_key: u16 = calculate_unlock_key(seed, &job.unlock_secret)?;
        protocol.emit(Event::Detail(
            "Preparing security and encryption parameters",
        ));
        cancel.check()?;
        uds::security_send_key(
            protocol,
            uds::AccessType::SendKey,
            &unlock_key.to_be_bytes(),
        )?;

        protocol.emit(Event::Step("Entering programming session"));
        cancel.check()?;
        uds::diagnostic_session(protocol, uds::SessionType::Programming)?;
        cancel.check()?;

        protocol.emit(Event::Step("Erasing memory"));
        uds::routine_control_with_timeout(
            protocol,
            uds::RoutineControlType::Start,
            uds::RoutineIdentifier::EraseMemory,
            None,
            Some(Duration::from_secs(20)),
        )?;

        protocol.emit(Event::Detail(
            "Preparing security and encryption parameters",
        ));
        uds::write_data_by_identifier(protocol, 0xF101, &job.decryption_key)?;

        protocol.emit(Event::Step("Preparing firmware download"));
        let max_blk_bytes =
            uds::request_download(protocol, job.start_address, job.firmware.len() as u32)?;
        let max_block_size = match max_blk_bytes.first() {
            // 0x20: the length is given as a big-endian u16.
            Some(0x20) => {
                let temp: [u8; 2] = max_blk_bytes
                    .get(1..3)
                    .and_then(|s| s.try_into().ok())
                    .ok_or_else(|| {
                        DiagError::InvalidResponse("Malformed DL max block value".into())
                    })?;
                u16::from_be_bytes(temp) as usize
            }
            Some(other) => {
                return Err(DiagError::InvalidResponse(format!(
                    "Unsupported DL max block value type: 0x{other:02X}"
                )));
            }
            None => {
                return Err(DiagError::InvalidResponse(
                    "Empty RequestDownload parameters".into(),
                ));
            }
        };

        protocol.emit(Event::Detail(&format!(
            "[HONDA_CAN] Max block size from ECU: {max_block_size} bytes"
        )));
        if max_block_size <= 2 {
            return Err(DiagError::InvalidResponse(
                "ECU reported unusable max block size".into(),
            ));
        }

        protocol.emit(Event::Step("Writing firmware"));
        let fw_data = &job.firmware;

        // Account for service id and block sequence count (one byte each)
        let chunk_size = max_block_size.min(protocol.max_payload()) - 2;
        let mut warned = false;
        protocol.emit(Event::Progress {
            done: 0,
            total: fw_data.len() as u64,
            unit: ProgressUnit::Bytes,
        });

        for (i, chunk_data) in fw_data.chunks(chunk_size).enumerate() {
            if cancel.is_cancelled() && !warned {
                protocol.emit(Event::Warning("Cannot stop now: the ECU has no firmware until the write and dependency checks finish"));
                warned = true;
            }
            let block_counter: u8 = ((i + 1) & 0xFF) as u8; // Block counter rolls over at 255

            let address = i * chunk_size;
            protocol.emit(Event::Detail(&format!(
                "[HONDA_CAN] Programming offset: 0x{:08x} / 0x{:08x} ",
                address,
                fw_data.len()
            )));

            // Transfer data chunk to ECU
            uds::transfer_data(protocol, block_counter, chunk_data)?;
            protocol.emit(Event::Progress {
                done: (address + chunk_data.len()) as u64,
                total: fw_data.len() as u64,
                unit: ProgressUnit::Bytes,
            });
        }

        protocol.emit(Event::Step("Finishing firmware transfer"));
        uds::request_transfer_exit(protocol)?;
        if cancel.is_cancelled() && !warned {
            protocol.emit(Event::Warning(
                "Cannot stop until programming dependency checks finish",
            ));
        }

        protocol.emit(Event::Step("Checking programming dependencies"));
        uds::routine_control_with_timeout(
            protocol,
            uds::RoutineControlType::Start,
            uds::RoutineIdentifier::CheckProgrammingDependencies,
            None,
            Some(Duration::from_secs(10)),
        )?;

        Ok(())
    }
}

impl<'a> FlashStrategy for HondaCanBusStrategy<'a> {
    fn ecu_type(&self) -> &'static str {
        "HONDA_CAN"
    }

    fn info(&mut self) -> Result<EcuInfo, DiagError> {
        let calibration = self.read_calibration_id()?;
        let bytes = uds::read_data_by_identifier(self.protocol, uds::DataIdentifier::Vin as u16)?;
        let vin = match bytes.split_first() {
            Some((&len, rest)) if len as usize <= rest.len() => {
                crate::diag::text(&rest[..len as usize])
            }
            _ => crate::diag::text(&bytes),
        };
        Ok(EcuInfo {
            calibration: Some(calibration),
            vin: Some(vin),
            ..Default::default()
        })
    }

    fn preflight(&mut self, request: &PreparedFlash) -> Result<Preflight, DiagError> {
        self.job = None;
        let mut preflight = Preflight::new("Flash ECU firmware", "This erases and rewrites the ECU. Interrupting it can leave the vehicle unable to start.");
        preflight.add(
            "file",
            format!(
                "{} (honda_can), {} bytes, target {}",
                request.image.format, request.image.size, request.target
            ),
            CheckStatus::Ok,
        );
        let calibration = self.read_calibration_id()?;
        preflight.add(
            "memory range",
            format!(
                "0x{:08X}..0x{:08X}",
                request.options.start_address.unwrap(),
                request.options.start_address.unwrap() + request.image.size as u32
            ),
            CheckStatus::Info,
        );
        if !request.raw {
            preflight.add(
                "checksum",
                "RWD checksum verification is not implemented",
                CheckStatus::Warn,
            );
            if request.image.start != request.options.start_address {
                preflight.add(
                    "start address",
                    "The requested start address overrides the container's address",
                    CheckStatus::Warn,
                );
            }
        }
        preflight.add(
            "ECU",
            &calibration,
            if calibration.starts_with("37805-") {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            },
        );
        let container = if request.raw {
            None
        } else {
            Some(
                HondaRwdParser::new()
                    .parse(&request.bytes)
                    .map_err(DiagError::InvalidFile)?,
            )
        };
        if let Some(rwd) = &container {
            let compatible = rwd.compatible_versions.contains_key(calibration.as_bytes());
            preflight.add(
                "compatibility",
                if compatible {
                    "ECU calibration is listed in the file".to_string()
                } else {
                    format!("ECU calibration {calibration} is not in the file's compatibility list")
                },
                if compatible {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Fail
                },
            );
        }
        let secret = self
            .resolve_unlock_secret(&calibration, &request.options, container.as_ref())
            .and_then(|secret| calculate_unlock_key(0, &secret).map(|_| secret));
        let unlock_secret = match secret {
            Ok(secret) => {
                preflight.add(
                    "key",
                    "security secret resolved for this ECU",
                    CheckStatus::Ok,
                );
                Some(secret)
            }
            Err(error) => {
                preflight.add("key", error.to_string(), CheckStatus::Fail);
                None
            }
        };
        let (battery, status) =
            battery_voltage_check(self.protocol, request.options.system_voltage)?;
        preflight.add("battery", battery, status);
        let seconds = request.image.size as f64 * 8.0 / self.protocol.bus().bitrate as f64;
        preflight.add("duration", format!("at least {seconds:.1} s at {} bit/s; erase time and protocol overhead are additional", self.protocol.bus().bitrate), CheckStatus::Info);
        if let Some(unlock_secret) = unlock_secret {
            let (firmware, decryption_key) = match container {
                Some(rwd) => (rwd.firmware.data, rwd.firmware.decryption_key),
                None => (request.bytes.clone(), vec![0; 3]),
            };
            self.job = Some(FlashJob {
                system_voltage: request.options.system_voltage,
                start_address: request.options.start_address.unwrap(),
                firmware,
                decryption_key,
                unlock_secret,
            });
        }
        Ok(preflight)
    }

    fn execute(&mut self, _permit: FlashPermit, cancel: &Cancel) -> Result<(), DiagError> {
        let job = self.job.take().ok_or_else(|| {
            DiagError::InvalidCommand("Flash preflight has not prepared a firmware job".into())
        })?;
        cancel.check()?;
        self.perform_flash(job, cancel)
    }
}

#[cfg(test)]
impl HondaCanBusStrategy<'_> {
    fn flash(&mut self, path: &Path, options: &FlashOptions) -> Result<(), DiagError> {
        let selected = self
            .protocol
            .header_id()
            .map(|id| self.protocol.addressing().parse_ecu(id))
            .transpose()?;
        let request = super::prepare_file(
            path,
            options.clone(),
            super::Selection {
                vehicle: Some("test-vehicle"),
                ecu: selected,
                strategy: Some("honda_can"),
                ..Default::default()
            },
        )?;
        let mut ui = crate::test_support::ScriptedInteraction::new(true);
        super::run(
            self.protocol,
            &request,
            &super::RunOptions {
                allow_untested: true,
                ..Default::default()
            },
            &mut ui,
            &Cancel::default(),
        )
        .map(|_| ())
    }
}

/// Factory for creating Honda CAN bus strategy instances
pub struct HondaCanBusStrategyFactory;

impl StrategyFactory for HondaCanBusStrategyFactory {
    fn name(&self) -> &'static str {
        "honda_can"
    }
    fn options(&self) -> &'static [&'static str] {
        &["tokens"]
    }
    fn inspect(&self, bytes: &[u8]) -> Result<Option<ImageInfo>, DiagError> {
        if !bytes.starts_with(&[0x5A, 0x0D, 0x0A]) {
            return Ok(None);
        }
        let rwd = HondaRwdParser::new()
            .parse(bytes)
            .map_err(DiagError::InvalidFile)?;
        let mut compatible: Vec<_> = rwd
            .compatible_versions
            .keys()
            .map(|s| crate::diag::text(s))
            .collect();
        compatible.sort();
        let target = EcuAddress::Can {
            request: rwd.can_address,
            response: crate::obd::addressing::Addressing::Can29Bit
                .response_for_request(rwd.can_address)
                .unwrap(),
        };
        Ok(Some(ImageInfo {
            format: "honda-rwd",
            target: Some(target),
            compatible,
            size: rwd.firmware.data.len(),
            start: Some(rwd.firmware.start),
        }))
    }
    fn create_strategy<'a>(&self, protocol: &'a mut ObdProtocol) -> Box<dyn FlashStrategy + 'a> {
        Box::new(HondaCanBusStrategy::new(protocol))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        protocol_on, protocol_on_adapter, FakeAdapter, FakeEcu, RecordingEvents, RwdFile,
    };

    /// The SecurityAccess response the ECU expects, derived from its seed and
    /// the K1/K2/K3 constants in the `.rwd` token table. Getting this wrong
    /// burns one of the ECU's limited unlock attempts, so both key widths are
    /// pinned to hand-computed values.
    #[test]
    fn six_byte_key_mixes_seed_with_k1_k2_k3() {
        // k1=0x0011, k2=0x0022, k3=0x0033, seed=0x1234
        // (0x1234 * 0x22 % 0x33) ^ (0x1234 + 0x0011) = 34 ^ 0x1245 = 0x1267
        let key = [0x00, 0x11, 0x00, 0x22, 0x00, 0x33];
        assert_eq!(calculate_unlock_key(0x1234, &key).unwrap(), 0x1267);
    }

    #[test]
    fn four_byte_key_skips_the_modulus() {
        // (0x1234 * 0x22) ^ (0x1234 + 0x0011) = 0x26AE8 ^ 0x1245, low 16 bits.
        let key = [0x00, 0x11, 0x00, 0x22];
        assert_eq!(calculate_unlock_key(0x1234, &key).unwrap(), 0x78AD);
    }

    /// The intermediate product overflows 16 bits, so the arithmetic has to
    /// happen in u32 and only the final result may be masked.
    #[test]
    fn large_seeds_do_not_overflow() {
        let key = [0xFF, 0xFF, 0xFF, 0xFF];
        let expected = 0xFFFF;
        assert_eq!(calculate_unlock_key(0xFFFF, &key).unwrap() as u32, expected);
    }

    // ------------------------------------------------------------ end to end

    const CALIBRATION_ID: &[u8] = b"37805-RGL-A030";
    /// The ECU the default test file targets: 0x18DA00F1 | (can_offset << 8).
    const ECU_REQUEST_ID: u32 = 0x18DA01F1;
    /// The address that ECU answers on.
    const ECU_RESPONSE_ID: u32 = 0x18DAF101;

    fn write_temp(suffix: &str, bytes: &[u8]) -> tempfile::NamedTempFile {
        // The strategy dispatches on the extension, so it has to be a real one.
        let file = tempfile::Builder::new()
            .suffix(suffix)
            .tempfile()
            .expect("create temp file");
        std::fs::write(file.path(), bytes).expect("write temp file");
        file
    }

    /// Drives `HondaCanBusStrategy::flash` against a simulated ECU, from a real
    /// `.rwd` file on disk down to the UDS messages on the link.
    ///
    /// Everything in between is the production path: the `.rwd` parser, token
    /// lookup, the SecurityAccess key math, the programming-voltage check and
    /// the UDS request/response layer. (ISO-TP framing is the adapter's job; the
    /// CAN session's own test covers it against the real driver.) The fake ECU rejects a wrong unlock
    /// key, an out-of-order block counter, and any step attempted before its
    /// prerequisites, so the sequence itself is under test — not just the
    /// number of bytes that arrived.
    #[test]
    fn flashes_an_rwd_file_end_to_end() {
        let rwd = RwdFile::default();
        let file = write_temp(".rwd", &rwd.encode());

        assert_eq!(
            rwd.can_address(),
            ECU_REQUEST_ID,
            "test fixture sanity check"
        );

        let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &rwd.tokens[0]);
        let mut protocol = protocol_on(&ecu);
        let events = RecordingEvents::default();
        protocol.set_events(Box::new(events.clone()));
        // Point at the wrong ECU on purpose: the container names its own target
        // and is expected to override this.
        protocol.set_header_id(ECU_REQUEST_ID).unwrap();

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        strategy
            .flash(file.path(), &FlashOptions::default())
            .expect("flash should succeed");

        let recorded = events.0.borrow();
        let progress: Vec<_> = recorded
            .iter()
            .filter(|event| event.starts_with("progress:"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            progress,
            [
                "progress: 0/600",
                "progress: 256/600",
                "progress: 512/600",
                "progress: 600/600"
            ]
        );
        let finished = recorded
            .iter()
            .position(|event| event == "progress: 600/600")
            .unwrap();
        assert_eq!(recorded[finished + 1], "step: Finishing firmware transfer");

        let st = ecu.state();

        assert!(
            st.protocol_violations.is_empty(),
            "ECU rejected part of the sequence: {:?}",
            st.protocol_violations
        );

        // The firmware arrived intact, at the address the container declared.
        assert_eq!(
            st.received_firmware, rwd.firmware,
            "firmware must round-trip"
        );
        assert_eq!(st.download_address, rwd.start);
        assert_eq!(st.download_len, rwd.firmware.len() as u32);

        // The ECU was unlocked, erased, keyed and finalised.
        assert!(st.unlocked, "SecurityAccess must have succeeded");
        assert!(st.erased, "memory must have been erased before download");
        assert_eq!(st.decryption_key, rwd.decryption_key);
        assert!(st.transfer_exited, "TransferExit must have been sent");
        assert!(st.dependencies_checked, "dependencies must be checked");
        assert_eq!(st.session, 0x02, "must end in the programming session");

        // Traffic was redirected to the ECU the file names.
        assert_eq!(
            st.headers_set,
            vec![ECU_REQUEST_ID],
            "the prepared target must agree with the selected ECU"
        );

        // Programming voltage went on for the flash and back off afterwards.
        assert_eq!(
            st.programming_voltage,
            vec![(12, 5000), (12, VOLTAGE_PIN_OFF)],
            "pin 12 must be energised then released"
        );
    }

    #[test]
    fn invalid_block_acknowledgements_stop_the_flash_and_release_programming_voltage() {
        for response in [vec![0x76], vec![0x76, 0x01]] {
            let rwd = RwdFile::default();
            let file = write_temp(".rwd", &rwd.encode());
            let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &rwd.tokens[0]);
            ecu.reply_to_transfer_with(0x02, &response);
            let mut protocol = protocol_on(&ecu);
            let events = RecordingEvents::default();
            protocol.set_events(Box::new(events.clone()));

            let err = HondaCanBusStrategy::new(&mut protocol)
                .flash(file.path(), &FlashOptions::default())
                .unwrap_err();

            assert!(matches!(err, DiagError::InvalidResponse(_)), "{err}");
            assert!(err.to_string().contains("block counter"), "{err}");
            let recorded = events.0.borrow();
            let progress: Vec<_> = recorded
                .iter()
                .filter(|event| event.starts_with("progress:"))
                .map(String::as_str)
                .collect();
            assert_eq!(
                progress,
                ["progress: 0/600", "progress: 256/600"],
                "a rejected acknowledgement must not advance progress"
            );
            let st = ecu.state();
            assert!(st.erased, "the failure must occur during programming");
            assert!(st.protocol_violations.is_empty());
            let counters: Vec<_> = st
                .requests
                .iter()
                .filter(|request| request.first() == Some(&0x36))
                .map(|request| request[1])
                .collect();
            assert_eq!(counters, [0x01, 0x02], "no retry or subsequent block");
            assert_eq!(st.requests.last().unwrap()[..2], [0x36, 0x02]);
            assert!(st.received_firmware.len() < rwd.firmware.len());
            assert!(!st.transfer_exited);
            assert!(!st.dependencies_checked);
            assert_eq!(
                st.programming_voltage,
                vec![(12, 5000), (12, VOLTAGE_PIN_OFF)]
            );
        }
    }

    /// The same flow for a raw `.bin`, which carries no metadata: the address
    /// and the security secret come from the command line, and the ECU must be
    /// told not to decrypt.
    #[test]
    fn flashes_a_raw_bin_end_to_end() {
        let firmware: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8).collect();
        let file = write_temp(".bin", &firmware);
        let secret = vec![0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66];

        let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &secret);
        let mut protocol = protocol_on(&ecu);
        protocol.set_header_id(ECU_REQUEST_ID).unwrap();

        let opts = FlashOptions {
            start_address: Some(0x0002_0000),
            security_key: Some(secret),
            tokens: None,
            ..Default::default()
        };

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        strategy
            .flash(file.path(), &opts)
            .expect("flash should succeed");

        let st = ecu.state();
        assert!(
            st.protocol_violations.is_empty(),
            "{:?}",
            st.protocol_violations
        );
        assert_eq!(st.received_firmware, firmware);
        assert_eq!(st.download_address, 0x0002_0000);
        assert_eq!(
            st.decryption_key,
            vec![0u8; 3],
            "a plaintext image must write a zero key so the ECU does not decrypt"
        );
        assert_eq!(
            st.headers_set,
            vec![ECU_REQUEST_ID],
            "no container to override --id"
        );
    }

    /// A `.bin` has no embedded address, so the flash must refuse to start
    /// rather than guess one — and must not touch the ECU on the way out.
    #[test]
    fn raw_bin_without_a_start_address_is_refused_before_touching_the_ecu() {
        let file = write_temp(".bin", &[0xAA; 32]);
        let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &[0x11, 0x22, 0x33, 0x44]);
        let mut protocol = protocol_on(&ecu);
        protocol.set_header_id(ECU_REQUEST_ID).unwrap();

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        let err = strategy
            .flash(file.path(), &FlashOptions::default())
            .unwrap_err();

        assert!(matches!(err, DiagError::InvalidCommand(_)), "got {err}");
        let st = ecu.state();
        assert!(
            st.programming_voltage.is_empty(),
            "voltage must never have been applied"
        );
        assert!(!st.erased, "the ECU must be untouched");
    }

    /// If the ECU runs a calibration the file does not list, there is no token
    /// to unlock it with — the flash must stop before erasing anything.
    #[test]
    fn refuses_to_flash_an_ecu_the_file_does_not_cover() {
        let rwd = RwdFile::default();
        let file = write_temp(".rwd", &rwd.encode());

        // The ECU reports a calibration the file's token table does not carry.
        let ecu = FakeEcu::new(ECU_RESPONSE_ID, b"37805-XXX-X000", &rwd.tokens[0]);
        let mut protocol = protocol_on(&ecu);

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        let err = strategy
            .flash(file.path(), &FlashOptions::default())
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("No security token for ECU version"),
            "got {err}"
        );
        let st = ecu.state();
        assert!(!st.unlocked);
        assert!(!st.erased, "nothing may be erased without a working unlock");
        assert!(st.programming_voltage.is_empty());
    }

    /// The unlock key is the one thing the ECU checks arithmetically. Feeding
    /// the flasher the wrong constants must fail, which is what proves the
    /// success case above really exercises the key math.
    #[test]
    fn a_wrong_security_key_is_rejected_by_the_ecu() {
        let file = write_temp(".bin", &[0x5Au8; 64]);

        let ecu = FakeEcu::new(
            ECU_RESPONSE_ID,
            CALIBRATION_ID,
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
        );
        let mut protocol = protocol_on(&ecu);
        protocol.set_header_id(ECU_REQUEST_ID).unwrap();

        let opts = FlashOptions {
            start_address: Some(0x0001_0000),
            // Right shape, wrong constants.
            security_key: Some(vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE]),
            tokens: None,
            ..Default::default()
        };

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        let err = strategy.flash(file.path(), &opts).unwrap_err();

        assert!(
            err.to_string().contains("0x35"),
            "expected NRC 0x35, got {err}"
        );
        assert!(err.to_string().contains("invalid key"), "got {err}");

        let st = ecu.state();
        assert!(!st.unlocked);
        assert!(!st.erased, "a failed unlock must not reach EraseMemory");
        assert_eq!(
            st.programming_voltage,
            vec![(12, 5000), (12, VOLTAGE_PIN_OFF)],
            "voltage must be released even though the flash failed"
        );
    }

    /// A supply that doesn't reach the requested voltage must stop the flash
    /// before anything is erased — and still release the pin.
    #[test]
    fn a_wrong_programming_voltage_stops_the_flash_before_erase() {
        let rwd = RwdFile::default();
        let file = write_temp(".rwd", &rwd.encode());
        let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &rwd.tokens[0]);
        ecu.measure_programming_voltage_as(3_900);
        let mut protocol = protocol_on(&ecu);

        let err = HondaCanBusStrategy::new(&mut protocol)
            .flash(file.path(), &FlashOptions::default())
            .unwrap_err();

        assert!(err.to_string().contains("3900 mV"), "{err}");
        let st = ecu.state();
        assert!(!st.erased, "nothing may be erased on a bad supply");
        assert_eq!(
            st.programming_voltage,
            vec![(12, 5000), (12, VOLTAGE_PIN_OFF)]
        );
    }

    #[test]
    fn battery_preflight_classifies_twelve_and_twenty_four_volt_supply_boundaries() {
        let rwd = RwdFile::default();
        for (system, readings) in [
            (
                SystemVoltage::V12,
                [
                    (0, CheckStatus::Fail),
                    (11_999, CheckStatus::Fail),
                    (12_000, CheckStatus::Warn),
                    (12_290, CheckStatus::Warn),
                    (12_399, CheckStatus::Warn),
                    (12_400, CheckStatus::Ok),
                    (13_800, CheckStatus::Ok),
                    (15_000, CheckStatus::Ok),
                    (15_001, CheckStatus::Fail),
                    (24_000, CheckStatus::Fail),
                ],
            ),
            (
                SystemVoltage::V24,
                [
                    (0, CheckStatus::Fail),
                    (23_999, CheckStatus::Fail),
                    (24_000, CheckStatus::Warn),
                    (24_580, CheckStatus::Warn),
                    (24_799, CheckStatus::Warn),
                    (24_800, CheckStatus::Ok),
                    (27_600, CheckStatus::Ok),
                    (30_000, CheckStatus::Ok),
                    (30_001, CheckStatus::Fail),
                    (12_400, CheckStatus::Fail),
                ],
            ),
        ] {
            let request = crate::flash::get_registry()
                .prepare(
                    rwd.encode(),
                    FlashOptions {
                        system_voltage: system,
                        ..Default::default()
                    },
                    crate::flash::Selection::default(),
                )
                .unwrap();
            for (mv, expected) in readings {
                let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &rwd.tokens[0]);
                ecu.measure_battery_as(mv);
                let mut protocol = protocol_on(&ecu);
                protocol.set_ecu(request.target()).unwrap();

                let preflight = HondaCanBusStrategy::new(&mut protocol)
                    .preflight(&request)
                    .unwrap();
                let battery = preflight
                    .checks
                    .iter()
                    .find(|check| check.name == "battery")
                    .unwrap();

                assert_eq!(battery.status, expected, "{mv} mV: {}", battery.value);
                assert_eq!(preflight.validate().is_err(), expected == CheckStatus::Fail);
                assert!(!ecu.state().erased);
                assert!(ecu.state().programming_voltage.is_empty());
            }
        }
    }

    #[test]
    fn battery_voltage_outside_limits_blocks_an_authorized_flash() {
        let rwd = RwdFile::default();
        let file = write_temp(".rwd", &rwd.encode());
        for (system, mv) in [
            (SystemVoltage::V12, 11_999),
            (SystemVoltage::V12, 15_001),
            (SystemVoltage::V24, 23_999),
            (SystemVoltage::V24, 30_001),
        ] {
            let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &rwd.tokens[0]);
            ecu.measure_battery_as(mv);
            let mut protocol = protocol_on(&ecu);

            let err = HondaCanBusStrategy::new(&mut protocol)
                .flash(
                    file.path(),
                    &FlashOptions {
                        system_voltage: system,
                        ..Default::default()
                    },
                )
                .unwrap_err();

            assert!(matches!(err, DiagError::PreflightFailed(_)), "{err}");
            assert!(err.to_string().contains("battery"), "{err}");
            let st = ecu.state();
            assert!(!st.erased);
            assert!(!st.unlocked);
            assert!(st.programming_voltage.is_empty());
        }
    }

    /// Within tolerance is fine: real supplies are never exact.
    #[test]
    fn a_programming_voltage_within_ten_percent_is_accepted() {
        let rwd = RwdFile::default();
        let file = write_temp(".rwd", &rwd.encode());
        let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &rwd.tokens[0]);
        ecu.measure_programming_voltage_as(5_450);
        let mut protocol = protocol_on(&ecu);

        HondaCanBusStrategy::new(&mut protocol)
            .flash(file.path(), &FlashOptions::default())
            .expect("5.45 V is within 10% of 5 V");
        assert!(ecu.state().erased);
    }

    /// The whole point of the guard: voltage comes back off even when the
    /// programming sequence dies in a way no error path can catch.
    #[test]
    fn programming_voltage_is_dropped_even_when_the_flash_panics() {
        let adapter = FakeAdapter::new();
        let (mut protocol, _link) = protocol_on_adapter(&adapter, &[]);

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // keep the test output clean
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _power =
                ProgrammingVoltage::apply(&mut protocol, PROGRAMMING_VOLTAGE_PIN, 5000).unwrap();
            panic!("simulated failure mid-flash");
        }));
        std::panic::set_hook(previous_hook);

        assert!(outcome.is_err(), "the panic should have propagated");
        assert_eq!(
            adapter.voltage_calls(),
            vec![(12, 5000), (12, VOLTAGE_PIN_OFF)],
            "pin 12 energised to 5000mV, then switched off on unwind"
        );
    }

    /// A zero modulus would panic on the divide; the flash path must reject the
    /// token table instead.
    #[test]
    fn rejects_a_zero_modulus() {
        let key = [0x00, 0x11, 0x00, 0x22, 0x00, 0x00];
        assert!(calculate_unlock_key(0x1234, &key).is_err());
    }

    #[test]
    fn rejects_key_lengths_other_than_four_or_six() {
        for len in [0usize, 1, 2, 3, 5, 7, 8] {
            assert!(
                calculate_unlock_key(0x1234, &vec![0xAA; len]).is_err(),
                "{len}-byte key must be rejected"
            );
        }
    }
}
