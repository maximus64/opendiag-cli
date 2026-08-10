use super::{FlashStrategy, StrategyFactory, FlashOptions};
use super::parsers::HondaRwdParser;
use super::parsers::honda_rwd::HondaRwdParseResult;
use std::path::Path;
use crate::obd::protocol::{ObdProtocol, VOLTAGE_PIN_OFF};
use crate::obd::uds as uds;
use crate::utils::bytes_to_hex;
use crate::errors::DiagError;
use std::time::Duration;
use std::thread::sleep;

/// OBD connector pin 12 — Honda's WEN (Write enable) line.
const PROGRAMMING_VOLTAGE_PIN: u32 = 12;

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
        println!("[HONDA_CAN] Disabling programming voltage...");
        if let Err(e) = self.protocol.set_programming_voltage(self.pin, VOLTAGE_PIN_OFF) {
            eprintln!("[HONDA_CAN] WARNING: Failed to turn off programming voltage: {}", e);
        }
    }
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
        },
        4 => {
            let k1 = u16::from_be_bytes([key[0], key[1]]);
            let k2 = u16::from_be_bytes([key[2], key[3]]);

            (((seed as u32 * k2 as u32) ^ (seed as u32 + k1 as u32)) & 0xFFFF) as u16
        },
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
}

impl<'a> HondaCanBusStrategy<'a> {
    pub fn new(protocol: &'a mut ObdProtocol) -> Self {
        Self {
            protocol
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
            println!("[HONDA_CAN] Using security key from --security-key");
            return Ok(key.clone());
        }

        if let Some(path) = &opts.tokens {
            let data = std::fs::read(path)
                .map_err(|e| DiagError::InvalidResponse(format!("Failed to read --tokens file: {}", e)))?;
            let rwd = HondaRwdParser::new()
                .parse(&data)
                .map_err(|e| DiagError::InvalidResponse(format!("Failed to parse --tokens file: {}", e)))?;
            if let Some(token) = Self::lookup_token(&rwd, calibration_id) {
                println!("[HONDA_CAN] Using security token from --tokens for version: {}", calibration_id);
                return Ok(token);
            }
            return Err(Self::no_token_error(&rwd, calibration_id));
        }

        if let Some(rwd) = embedded {
            if let Some(token) = Self::lookup_token(rwd, calibration_id) {
                println!("[HONDA_CAN] Found compatible security token for version: {}", calibration_id);
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
        let mut msg = format!("No security token for ECU version '{}'. Available versions:", calibration_id);
        for version_key in rwd.compatible_versions.keys() {
            msg.push_str(&format!("\n  - {}", String::from_utf8_lossy(version_key)));
        }
        DiagError::InvalidResponse(msg)
    }

    /// Flashes a Honda `.rwd` container file.
    fn flash_rwd_file(&mut self, image_path: &Path, opts: &FlashOptions) -> Result<(), DiagError> {
        println!("[HONDA_CAN] Parsing Honda RWD file: {:?}", image_path);

        let data = std::fs::read(image_path)
            .map_err(|e| DiagError::InvalidResponse(format!("Failed to read Honda RWD file: {}", e)))?;

        let rwd_data = HondaRwdParser::new()
            .parse(&data)
            .map_err(|e| DiagError::InvalidResponse(format!("Failed to parse Honda RWD file: {}", e)))?;

        println!("[HONDA_CAN] Honda RWD file parsed successfully:");
        println!("[HONDA_CAN]   {:<22} {:#010x}", "CAN Address:", rwd_data.can_address);
        println!("[HONDA_CAN]   {:<22} {:#010x}", "Firmware Address:", rwd_data.firmware.start);
        println!("[HONDA_CAN]   {:<22} {:#010x} ({} bytes)", "Firmware Size:", rwd_data.firmware.size, rwd_data.firmware.size);
        println!("[HONDA_CAN]   {:<22} {}", "Compatible Versions:", rwd_data.compatible_versions.len());
        println!("[HONDA_CAN]   {:<22} {}", "Decryption Key:", bytes_to_hex(&rwd_data.firmware.decryption_key));
        println!("[HONDA_CAN]   {:<22} {:#010x}", "Checksum:", rwd_data.checksum);

        // The container names the ECU it belongs to, so target that address
        // rather than whatever --id happened to be passed. Do it before reading
        // the calibration id, so the id we match tokens against comes from the
        // ECU we are actually about to write.
        if self.protocol.header_id() != Some(rwd_data.can_address) {
            if let Some(current) = self.protocol.header_id() {
                println!(
                    "[HONDA_CAN] NOTE: --id {:#010x} does not match the file's target {:#010x}; using the file's.",
                    current, rwd_data.can_address
                );
            }
            self.protocol.set_header_id(rwd_data.can_address)?;
        }

        let calibration_id = self.read_calibration_id()?;
        println!("[HONDA_CAN] ECU calibration id: {}", calibration_id);

        let unlock_secret = self.resolve_unlock_secret(&calibration_id, opts, Some(&rwd_data))?;

        let job = FlashJob {
            // The container carries the address; allow an explicit override.
            start_address: opts.start_address.unwrap_or(rwd_data.firmware.start),
            firmware: rwd_data.firmware.data,
            decryption_key: rwd_data.firmware.decryption_key,
            unlock_secret,
        };

        self.perform_flash(job)
    }

    /// Flashes a raw `.bin` image (plaintext firmware, no embedded metadata).
    fn flash_bin_file(&mut self, image_path: &Path, opts: &FlashOptions) -> Result<(), DiagError> {
        println!("[HONDA_CAN] Reading raw firmware image: {:?}", image_path);

        let firmware = std::fs::read(image_path)
            .map_err(|e| DiagError::InvalidResponse(format!("Failed to read firmware image: {}", e)))?;

        let start_address = opts.start_address.ok_or_else(|| DiagError::InvalidCommand(
            "Raw .bin flashing requires --start-address <hex>".into(),
        ))?;

        let calibration_id = self.read_calibration_id()?;
        println!("[HONDA_CAN] ECU calibration id: {}", calibration_id);

        let unlock_secret = self.resolve_unlock_secret(&calibration_id, opts, None)?;

        println!("[HONDA_CAN] Firmware size: {} bytes", firmware.len());
        println!("[HONDA_CAN] Start address: {:#010x}", start_address);

        let job = FlashJob {
            start_address,
            firmware,
            // Plaintext image: write a zero key so the ECU does not decrypt.
            decryption_key: vec![0u8; 3],
            unlock_secret,
        };

        self.perform_flash(job)
    }

    /// The shared programming sequence used by every source format.
    ///
    /// Applies programming voltage and hands off to [`Self::run_programming_sequence`].
    /// The voltage is held by a guard, so it comes back off however that call
    /// ends — success, error, or panic.
    fn perform_flash(&mut self, job: FlashJob) -> Result<(), DiagError> {
        // Apply 5V on Pin 12 - PWE to able to flash
        let power = ProgrammingVoltage::apply(&mut *self.protocol, PROGRAMMING_VOLTAGE_PIN, 5000)?;
        sleep(VOLTAGE_SETTLE_TIME); // wait for voltage to stabilize

        let result = Self::run_programming_sequence(power.protocol, &job);

        // Drop the voltage before reporting, so the last thing printed on a
        // successful flash is the success and not the teardown.
        drop(power);
        result?;

        println!("Done!");
        Ok(())
    }

    /// Unlocks via SecurityAccess, enters the programming session, erases,
    /// writes the decryption key, then downloads and transfers the firmware.
    ///
    /// Runs with programming voltage applied, so every failure in here must be
    /// an error rather than a panic: the caller's guard can only drop the
    /// voltage if the stack unwinds through it.
    fn run_programming_sequence(protocol: &mut ObdProtocol, job: &FlashJob) -> Result<(), DiagError> {
        uds::diagnostic_session(protocol, uds::SessionType::ExtendedDiagnostic)?;

        let seed_bytes: [u8; 2] = uds::security_request_seed(protocol, uds::AccessType::RequestSeed)?
            .try_into()
            .map_err(|_| DiagError::InvalidResponse("Unexpected SecurityAccess seed length".into()))?;
        let seed = u16::from_be_bytes(seed_bytes);
        println!("[HONDA_CAN] SecurityAccess seed: 0x{:04x}", seed);

        let unlock_key: u16 = calculate_unlock_key(seed, &job.unlock_secret)?;
        println!("[HONDA_CAN] Sending unlock key: 0x{:04x}", unlock_key);
        uds::security_send_key(protocol, uds::AccessType::SendKey, &unlock_key.to_be_bytes())?;

        println!("Request PROGRAMMING Session...");
        uds::diagnostic_session(protocol, uds::SessionType::Programming)?;

        println!("[HONDA_CAN] Erase memory ...");
        uds::routine_control_with_timeout(protocol, uds::RoutineControlType::Start, uds::RoutineIdentifier::EraseMemory, None, Some(Duration::from_secs(20)))?;

        println!("[HONDA_CAN] Writing fw encryption key {:?} ...", job.decryption_key);
        uds::write_data_by_identifier(protocol, 0xF101, &job.decryption_key)?;

        println!("[HONDA_CAN] Request Download ...");
        let max_blk_bytes = uds::request_download(protocol, job.start_address, job.firmware.len() as u32)?;
        let max_block_size = match max_blk_bytes.first() {
            // 0x20: the length is given as a big-endian u16.
            Some(0x20) => {
                let temp: [u8; 2] = max_blk_bytes
                    .get(1..3)
                    .and_then(|s| s.try_into().ok())
                    .ok_or_else(|| DiagError::InvalidResponse("Malformed DL max block value".into()))?;
                u16::from_be_bytes(temp) as usize
            }
            Some(other) => {
                return Err(DiagError::InvalidResponse(format!(
                    "Unsupported DL max block value type: 0x{:02X}", other
                )));
            }
            None => {
                return Err(DiagError::InvalidResponse("Empty RequestDownload parameters".into()));
            }
        };

        println!("[HONDA_CAN] Max block size from ECU: {} bytes", max_block_size);
        if max_block_size <= 2 {
            return Err(DiagError::InvalidResponse("ECU reported unusable max block size".into()));
        }

        println!("Perform Write firmware to ECU...");
        let fw_data = &job.firmware;

        // Account for service id and block sequence count (one byte each)
        let chunk_size = max_block_size - 2;

        for (i, chunk_data) in fw_data.chunks(chunk_size).enumerate() {
            let block_counter: u8 = ((i + 1) & 0xFF) as u8; // Block counter rolls over at 255

            let address = i * chunk_size;
            println!("[HONDA_CAN] Programming offset: 0x{:08x} / 0x{:08x} ", address, fw_data.len());

            // Transfer data chunk to ECU
            uds::transfer_data(protocol, block_counter, chunk_data)?;
        }

        println!("Exit transfer...");
        uds::request_transfer_exit(protocol)?;

        println!("Check programming dependencies...");
        uds::routine_control_with_timeout(protocol, uds::RoutineControlType::Start, uds::RoutineIdentifier::CheckProgrammingDependencies, None, Some(Duration::from_secs(10)))?;

        Ok(())
    }
}

impl<'a> FlashStrategy for HondaCanBusStrategy<'a> {
    fn ecu_type(&self) -> &'static str {
        "HONDA_CAN"
    }

    fn info(&mut self) -> Result<String, DiagError> {
        let calibration_id = self.read_calibration_id()?;

        let vin_buff = uds::read_data_by_identifier(self.protocol, uds::DataIdentifier::Vin as u16)?;
        // Honda prefixes the VIN with its length. Fall back to the whole buffer
        // if that byte is missing or overruns what the ECU actually sent.
        let vin = match vin_buff.split_first() {
            Some((&len, rest)) if (len as usize) <= rest.len() => {
                String::from_utf8_lossy(&rest[..len as usize]).to_string()
            }
            _ => String::from_utf8_lossy(&vin_buff).trim_matches('\0').to_string(),
        };

        Ok(format!(
            "Honda CAN:\n\
            \tECU calibration id: {}\n\
            \tVIN: {}",
            calibration_id,
            vin
        ))
    }

    fn dry_run(&mut self, image_path: &Path, _opts: &FlashOptions) -> Result<(), DiagError> {
        // Validate file exists and maybe size limits later.
        if !image_path.exists() {
            return Err(DiagError::InvalidResponse(format!("File not found: {:?}", image_path)));
        }
        println!("[HONDA_CANBUS] Dry-run validation OK for {:?}", image_path);
        Ok(())
    }

    fn flash(&mut self, image_path: &Path, opts: &FlashOptions) -> Result<(), DiagError> {
        if !image_path.exists() {
            return Err(DiagError::InvalidResponse(format!("File not found: {:?}", image_path)));
        }
        println!("[HONDA_CAN] flash {:?}", image_path);

        // Dispatch on extension: .rwd containers vs. raw .bin images.
        let extension = image_path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase());

        match extension.as_deref() {
            Some("rwd") => self.flash_rwd_file(image_path, opts),
            Some("bin") => self.flash_bin_file(image_path, opts),
            _ => Err(DiagError::InvalidCommand(format!(
                "Unsupported firmware file type: {:?}. Expected .rwd or .bin",
                image_path
            ))),
        }
    }
}


/// Factory for creating Honda CAN bus strategy instances
pub struct HondaCanBusStrategyFactory;

impl StrategyFactory for HondaCanBusStrategyFactory {
    fn ecu_type(&self) -> &'static str {
        "HONDA_CAN"
    }
    
    fn create_strategy<'a>(&self, protocol: &'a mut ObdProtocol) -> Box<dyn FlashStrategy + 'a> {
        Box::new(HondaCanBusStrategy::new(protocol))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeEcu, MockPort, RwdFile};

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
        let expected = ((0xFFFFu32 * 0xFFFFu32) ^ (0xFFFFu32 + 0xFFFFu32)) & 0xFFFF;
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
    /// `.rwd` file on disk all the way down to individual CAN frames.
    ///
    /// Everything in between is the production path: the `.rwd` parser, token
    /// lookup, the SecurityAccess key math, ISO-TP segmentation and reassembly,
    /// and the UDS request/response layer. The fake ECU rejects a wrong unlock
    /// key, an out-of-order block counter, and any step attempted before its
    /// prerequisites, so the sequence itself is under test — not just the
    /// number of bytes that arrived.
    #[test]
    fn flashes_an_rwd_file_end_to_end() {
        let rwd = RwdFile::default();
        let file = write_temp(".rwd", &rwd.encode());

        assert_eq!(rwd.can_address(), ECU_REQUEST_ID, "test fixture sanity check");

        let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &rwd.tokens[0]);
        let mut protocol = ObdProtocol::new(Box::new(ecu.clone()));
        // Point at the wrong ECU on purpose: the container names its own target
        // and is expected to override this.
        protocol.set_header_id(0x18DA0FF1).unwrap();

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        strategy
            .flash(file.path(), &FlashOptions::default())
            .expect("flash should succeed");

        let st = ecu.state();

        assert!(
            st.protocol_violations.is_empty(),
            "ECU rejected part of the sequence: {:?}",
            st.protocol_violations
        );

        // The firmware arrived intact, at the address the container declared.
        assert_eq!(st.received_firmware, rwd.firmware, "firmware must round-trip");
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
            vec![0x18DA0FF1, ECU_REQUEST_ID],
            "the container's CAN address must override --id"
        );

        // Programming voltage went on for the flash and back off afterwards.
        assert_eq!(
            st.programming_voltage,
            vec![(12, 5000), (12, VOLTAGE_PIN_OFF)],
            "pin 12 must be energised then released"
        );
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
        let mut protocol = ObdProtocol::new(Box::new(ecu.clone()));
        protocol.set_header_id(ECU_REQUEST_ID).unwrap();

        let opts = FlashOptions {
            start_address: Some(0x0002_0000),
            security_key: Some(secret),
            tokens: None,
        };

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        strategy.flash(file.path(), &opts).expect("flash should succeed");

        let st = ecu.state();
        assert!(st.protocol_violations.is_empty(), "{:?}", st.protocol_violations);
        assert_eq!(st.received_firmware, firmware);
        assert_eq!(st.download_address, 0x0002_0000);
        assert_eq!(
            st.decryption_key,
            vec![0u8; 3],
            "a plaintext image must write a zero key so the ECU does not decrypt"
        );
        assert_eq!(st.headers_set, vec![ECU_REQUEST_ID], "no container to override --id");
    }

    /// A `.bin` has no embedded address, so the flash must refuse to start
    /// rather than guess one — and must not touch the ECU on the way out.
    #[test]
    fn raw_bin_without_a_start_address_is_refused_before_touching_the_ecu() {
        let file = write_temp(".bin", &[0xAA; 32]);
        let ecu = FakeEcu::new(ECU_RESPONSE_ID, CALIBRATION_ID, &[0x11, 0x22, 0x33, 0x44]);
        let mut protocol = ObdProtocol::new(Box::new(ecu.clone()));
        protocol.set_header_id(ECU_REQUEST_ID).unwrap();

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        let err = strategy
            .flash(file.path(), &FlashOptions::default())
            .unwrap_err();

        assert!(matches!(err, DiagError::InvalidCommand(_)), "got {err}");
        let st = ecu.state();
        assert!(st.programming_voltage.is_empty(), "voltage must never have been applied");
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
        let mut protocol = ObdProtocol::new(Box::new(ecu.clone()));

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        let err = strategy
            .flash(file.path(), &FlashOptions::default())
            .unwrap_err();

        assert!(
            err.to_string().contains("No security token for ECU version"),
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
        let file = write_temp(".bin", &vec![0x5Au8; 64]);

        let ecu = FakeEcu::new(
            ECU_RESPONSE_ID,
            CALIBRATION_ID,
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
        );
        let mut protocol = ObdProtocol::new(Box::new(ecu.clone()));

        let opts = FlashOptions {
            start_address: Some(0x0001_0000),
            // Right shape, wrong constants.
            security_key: Some(vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE]),
            tokens: None,
        };

        let mut strategy = HondaCanBusStrategy::new(&mut protocol);
        let err = strategy.flash(file.path(), &opts).unwrap_err();

        assert!(err.to_string().contains("0x35"), "expected NRC 0x35, got {err}");
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

    /// The whole point of the guard: voltage comes back off even when the
    /// programming sequence dies in a way no error path can catch.
    #[test]
    fn programming_voltage_is_dropped_even_when_the_flash_panics() {
        let port = MockPort::new(&["OK", "OK"]);
        let mut protocol = ObdProtocol::new(Box::new(port.clone()));

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
            port.commands(),
            vec!["ATPROGVC00001388", "ATPROGVCFFFFFFFF"],
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
