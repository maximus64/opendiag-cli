//! RAD application programming through Honda's K-Line resident loader.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::parsers::honda_rwd::{HondaRwdParser, HondaRwdRecord, HondaRwdX31, X31_BLOCK_SIZE};
use super::{FlashOptions, FlashPermit, FlashStrategy, ImageInfo, PreparedFlash, StrategyFactory};
use crate::{
    diag::EcuInfo,
    events::{Cancel, Event, FlashRecovery, Interaction, ProgressUnit},
    link::{BusKind, Framing},
    obd::{addressing::EcuAddress, protocol::VOLTAGE_PIN_OFF},
    preflight::{CheckStatus, Preflight},
    utils::{checksum8 as checksum, format_duration, trim_padding},
    DiagError, ObdProtocol,
};

const TARGET: u8 = 0x36;
const OBD_ADDRESS: u8 = 0x09;
const APPLICATION_START: u32 = 0x8000;
const APPLICATION_END: u32 = 0x100000;
const PROGRAMMING_PIN: u32 = 12;
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

fn parse_x31(bytes: &[u8]) -> Result<HondaRwdX31, DiagError> {
    HondaRwdParser::new()
        .parse(bytes)
        .and_then(|rwd| rwd.into_x31())
        .map_err(DiagError::InvalidFile)
}

fn validate_package(rwd: &HondaRwdX31) -> Result<(), DiagError> {
    if rwd.target != TARGET
        || rwd
            .compatible_versions
            .keys()
            .any(|id| !id.starts_with("37805-RAD-"))
    {
        return Err(DiagError::InvalidFile(
            "This strategy requires a RAD x31 image targeting 0x36".into(),
        ));
    }
    if rwd
        .records
        .iter()
        .any(|r| r.address < APPLICATION_START || !r.address.is_multiple_of(X31_BLOCK_SIZE as u32))
    {
        return Err(DiagError::InvalidFile(
            "RAD records must be 128-byte aligned and must not overwrite the resident loader"
                .into(),
        ));
    }
    Ok(())
}

fn validate_input(bytes: &[u8], options: &FlashOptions, raw: bool) -> Result<(), DiagError> {
    if let Some(secret) = &options.security_key {
        if secret.len() != 6 {
            return Err(DiagError::InvalidCommand(
                "Honda K-Line --security-key must contain six bytes".into(),
            ));
        }
    }
    if let Some(tokens) = &options.token_data {
        validate_package(&parse_x31(tokens)?)?;
    }
    if raw {
        let start = options.start_address.ok_or_else(|| {
            DiagError::InvalidCommand("RAD raw firmware requires --start-address".into())
        })?;
        validate_raw_range(bytes.len(), start)?;
        if options.security_key.is_none() && options.token_data.is_none() {
            return Err(DiagError::InvalidCommand(
                "Raw Honda firmware needs --security-key or --tokens".into(),
            ));
        }
    } else {
        let rwd = parse_x31(bytes)?;
        validate_package(&rwd)?;
        if options.start_address != rwd.records.first().map(|r| r.address) {
            return Err(DiagError::InvalidCommand(
                "An x31 RWD contains sparse addresses; --start-address cannot relocate it".into(),
            ));
        }
    }
    Ok(())
}

fn validate_raw_range(size: usize, start: u32) -> Result<(), DiagError> {
    let end = u32::try_from(size)
        .ok()
        .and_then(|size| start.checked_add(size));
    if size == 0
        || !size.is_multiple_of(X31_BLOCK_SIZE)
        || start < APPLICATION_START
        || !start.is_multiple_of(X31_BLOCK_SIZE as u32)
        || end.is_none_or(|end| end > APPLICATION_END)
    {
        return Err(DiagError::InvalidFile(
            "RAD raw firmware must use complete 128-byte blocks within 0x8000..0x100000".into(),
        ));
    }
    Ok(())
}

fn raw_records(bytes: &[u8], start: u32, key: [u8; 3]) -> Result<Vec<HondaRwdRecord>, DiagError> {
    validate_raw_range(bytes.len(), start)?;
    let count = bytes.len() / X31_BLOCK_SIZE;
    let mut records = Vec::new();
    for (index, block) in bytes.as_chunks::<X31_BLOCK_SIZE>().0.iter().enumerate() {
        if index != 0 && index + 1 != count && block.iter().all(|&b| b == 0xFF) {
            continue;
        }
        let mut encrypted = [0; X31_BLOCK_SIZE];
        for (output, &input) in encrypted.iter_mut().zip(block) {
            *output = input.wrapping_add(key[0]).wrapping_sub(key[1]) ^ key[2];
        }
        records.push(HondaRwdRecord {
            address: start + (index * X31_BLOCK_SIZE) as u32,
            encrypted,
        });
    }
    Ok(records)
}

fn loader_key(seed: u16, secret: [u8; 6]) -> u16 {
    let a = u16::from_be_bytes([secret[0], secret[1]]);
    let b = u16::from_be_bytes([secret[2], secret[3]]);
    let c = u16::from_be_bytes([secret[4], secret[5]]);
    let product = u32::from(seed.wrapping_add(a)) * u32::from(b);
    if c == 0 {
        product as u16
    } else {
        (product % u32::from(c)) as u16
    }
}

fn record_payload(records: &[HondaRwdRecord], index: usize) -> Vec<u8> {
    let record = &records[index];
    let next = records.get(index + 1).map_or(0, |record| record.address);
    let mut body = Vec::with_capacity(X31_BLOCK_SIZE + 6);
    body.extend_from_slice(&((record.address >> 4) as u16).to_be_bytes());
    body.extend_from_slice(&record.encrypted);
    body.extend_from_slice(&((next >> 4) as u16).to_be_bytes());
    let check = body
        .iter()
        .fold(0u16, |sum, &b| sum.wrapping_sub(u16::from(b)));
    body.extend_from_slice(&check.to_be_bytes());
    body
}

fn wait(duration: Duration) {
    #[cfg(not(test))]
    std::thread::sleep(duration);
    #[cfg(test)]
    let _ = duration;
}

struct FlashJob {
    calibration: String,
    secret: [u8; 6],
    cipher_key: [u8; 3],
    records: Vec<HondaRwdRecord>,
}

pub struct HondaKLineStrategy<'a> {
    protocol: &'a mut ObdProtocol,
    job: Option<FlashJob>,
    voltage_active: bool,
    loader_entered: bool,
    completed: bool,
}

impl<'a> HondaKLineStrategy<'a> {
    pub fn new(protocol: &'a mut ObdProtocol) -> Self {
        Self {
            protocol,
            job: None,
            voltage_active: false,
            loader_entered: false,
            completed: false,
        }
    }

    fn voltage(&mut self, enabled: bool) -> Result<(), DiagError> {
        // An uncertain enable RPC must still be followed by an off attempt.
        if enabled {
            self.voltage_active = true;
        }
        self.protocol.set_programming_voltage(
            PROGRAMMING_PIN,
            if enabled { 5000 } else { VOLTAGE_PIN_OFF },
        )?;
        self.voltage_active = enabled;
        Ok(())
    }

    fn timing(&mut self, p3: u32, p4: u32) -> Result<(), DiagError> {
        self.protocol.set_link_timing(&BTreeMap::from([
            ("P3_MIN".into(), p3),
            ("P4_MIN".into(), p4),
        ]))
    }

    fn iso_exchange(&mut self, prefix: &[u8], reply_len: usize) -> Result<(), DiagError> {
        let mut packet = prefix.to_vec();
        packet.push(checksum(prefix).wrapping_neg());
        self.protocol.reset()?;
        self.protocol.send(&packet)?;
        let expected = [0x48, 0x6B, OBD_ADDRESS, prefix[3].wrapping_add(0x40)];
        let pid = (prefix[3] == 1).then_some(prefix[4]);
        let mut reply = Vec::new();
        let deadline = Instant::now() + REPLY_TIMEOUT;
        while Instant::now() < deadline {
            let Some(frame) = self
                .protocol
                .receive(deadline.saturating_duration_since(Instant::now()))?
            else {
                break;
            };
            reply.extend(frame.data);
            let shared = reply.len().min(expected.len());
            if reply.len() > reply_len
                || reply[..shared] != expected[..shared]
                || pid.is_some_and(|pid| reply.get(4).is_some_and(|&echo| echo != pid))
            {
                return Err(DiagError::InvalidResponse(
                    "Invalid Honda protocol-switch reply".into(),
                ));
            }
            // A partial payload can match the checksum; use the known response length.
            if reply.len() == reply_len {
                if checksum(&reply[..reply_len - 1]).wrapping_neg() != reply[reply_len - 1] {
                    return Err(DiagError::InvalidResponse(
                        "Honda protocol-switch reply checksum mismatch".into(),
                    ));
                }
                return Ok(());
            }
        }
        Err(DiagError::Timeout(format!(
            "Honda protocol switch timed out ({} partial bytes)",
            reply.len()
        )))
    }

    fn initialize(&mut self) -> Result<(), DiagError> {
        self.timing(110, 10)?;
        if self.protocol.initialize_link()? != [8, 8] {
            return Err(DiagError::InvalidResponse(
                "RAD initialization requires keywords 08 08".into(),
            ));
        }
        self.iso_exchange(&[0x68, 0x6A, 0xF1, 1, 0], 10)?;
        self.iso_exchange(
            &[0x68, 0x6A, 0xF5, 0xAF, 0xBF, 0xB3, 0xB2, 0xC1, 0xDB, 0xB3],
            5,
        )?;
        self.timing(4, 0)?;
        wait(Duration::from_millis(100));
        Ok(())
    }

    fn exchange(
        &mut self,
        header: u8,
        command: Option<u8>,
        data: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, DiagError> {
        let mut packet = vec![header, 0];
        if let Some(command) = command {
            packet.extend_from_slice(&[TARGET, command]);
        }
        packet[1] = u8::try_from(packet.len() + data.len() + 1)
            .map_err(|_| DiagError::InvalidCommand("Honda request exceeds 255 bytes".into()))?;
        packet.extend_from_slice(data);
        packet.push(checksum(&packet));
        self.protocol.reset()?;
        self.protocol.send(&packet)?;
        let wanted_header = header - if command.is_some() { 0x70 } else { 0xF0 };
        let mut pending = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Some(frame) = self
                .protocol
                .receive(deadline.saturating_duration_since(Instant::now()))?
            else {
                break;
            };
            pending.extend(frame.data);
            while pending.len() >= 2 {
                if !matches!(pending[0], 0 | 5 | 6 | 7 | 0x0D | 0x0E) {
                    return Err(DiagError::InvalidResponse(
                        "Unexpected Honda response header".into(),
                    ));
                }
                let length = usize::from(pending[1]);
                let minimum = if matches!(pending[0], 0x0D | 0x0E) {
                    4
                } else {
                    3
                };
                if length < minimum {
                    return Err(DiagError::InvalidResponse(
                        "Invalid Honda response length".into(),
                    ));
                }
                if pending.len() < length {
                    break;
                }
                let reply = &pending[..length];
                if checksum(reply) != 0 {
                    return Err(DiagError::InvalidResponse(
                        "Honda response checksum mismatch".into(),
                    ));
                }
                if reply[0] == wanted_header && command.is_none_or(|command| reply[2] == command) {
                    if pending.len() != length {
                        return Err(DiagError::InvalidResponse(
                            "Trailing bytes after Honda reply".into(),
                        ));
                    }
                    return Ok(reply[if command.is_some() { 3 } else { 2 }..length - 1].to_vec());
                }
                pending.drain(..length);
            }
        }
        Err(DiagError::Timeout(format!(
            "Honda command {header:02X}/{command:02X?} timed out ({} partial bytes)",
            pending.len()
        )))
    }

    fn command(
        &mut self,
        command: u8,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, DiagError> {
        let started = Instant::now();
        loop {
            let data = self.exchange(0x7E, Some(command), payload, timeout)?;
            // These OEM statuses explicitly request repetition. A transport timeout
            // or any other reply never causes an automatic resend.
            let (limit, delay) = match (command, data.as_slice()) {
                (4, [0x15]) => (60, Duration::ZERO),
                (5, [1]) => (600, Duration::from_secs(1)),
                (8, [1]) => (60, Duration::ZERO),
                (0x0A, [1]) => (11, Duration::from_millis(100)),
                _ => return Ok(data),
            };
            if started.elapsed() >= Duration::from_secs(limit) {
                return Err(DiagError::Timeout(format!(
                    "Honda command {command:02X} remained busy for {limit} seconds"
                )));
            }
            wait(delay);
        }
    }

    fn expect(&mut self, command: u8, payload: &[u8], status: u8) -> Result<(), DiagError> {
        let reply = self.command(command, payload, REPLY_TIMEOUT)?;
        expect_reply(&reply, &[status], command)
    }

    fn identify(&mut self) -> Result<String, DiagError> {
        self.initialize()?;
        let reply = self.exchange(0x7D, Some(1), &[0], REPLY_TIMEOUT)?;
        let text = std::str::from_utf8(&reply)
            .map_err(|_| DiagError::InvalidResponse("Non-ASCII Honda calibration".into()))?;
        let calibration = trim_padding(text);
        if !calibration.is_ascii() || !calibration.starts_with("37805-RAD-") {
            return Err(DiagError::InvalidResponse(
                "Application did not identify a RAD calibration".into(),
            ));
        }
        Ok(calibration.into())
    }

    /*
        fn authorize_f7(&mut self) -> Result<(), DiagError> {
            self.protocol
                .emit(Event::Step("Applying 0xF7 authorization"));
            let state = self.exchange(0xF0, None, &[5], REPLY_TIMEOUT)?;
            match state.as_slice() {
                [0 | 3] => return Ok(()),
                [1 | 2] => {}
                _ => {
                    return Err(DiagError::InvalidResponse(
                        "Unknown authorization state".into(),
                    ))
                }
            }
            let seed = self.exchange(0xF5, None, &[0xFF], REPLY_TIMEOUT)?;
            let seed =
                u16::from_be_bytes(seed.try_into().map_err(|_| {
                    DiagError::InvalidResponse("Invalid authorization seed".into())
                })?);
            if seed != 0 {
                let reply = self.exchange(
                    0xF7,
                    None,
                    &authorization_key(seed).to_be_bytes(),
                    REPLY_TIMEOUT,
                )?;
                expect_reply(&reply, &[0], 0xF7)?;
            }
            let reply = self.exchange(0xF6, None, &[1], REPLY_TIMEOUT)?;
            expect_reply(&reply, &[0], 0xF6)?;
            wait(Duration::from_millis(100));
            let reply = self.exchange(0xF0, None, &[5], REPLY_TIMEOUT)?;
            expect_reply(&reply, &[3], 0xF0)
        }
    */

    fn enter(&mut self, cancel: &Cancel) -> Result<(), DiagError> {
        self.protocol
            .emit(Event::Step("Entering programming session"));
        cancel.check()?;
        wait(Duration::from_millis(100));
        self.voltage(true)?;
        wait(Duration::from_millis(100));
        let measured = self.protocol.read_programming_voltage_mv()?;
        if measured.abs_diff(5000) > 500 {
            return Err(DiagError::Adapter(format!(
                "Programming voltage measured {measured} mV; expected 5000 mV ±10%"
            )));
        }
        for (command, magic) in [(2, b"PGM"), (3, b"-FI")] {
            cancel.check()?;
            // A lost stage-two acknowledgement may still mean the loader was entered.
            if command == 3 {
                self.loader_entered = true;
            }
            let reply = self.exchange(0x7D, Some(command), magic, REPLY_TIMEOUT)?;
            if reply != [0] {
                self.loader_entered = false;
                let reason = match reply.as_slice() {
                    [0x11] => "FWE is low; check pin 12 programming-enable wiring",
                    [0x10] => "application entry gate is not satisfied",
                    [0xFE] => "authorization gate is not satisfied; check the matched immobilizer/key setup or explicit authorization",
                    [0x1F] => "stage one is not armed",
                    _ => "unrecognized programming-entry rejection",
                };
                return Err(DiagError::InvalidResponse(format!(
                    "{reason} ({reply:02X?}); erase was not sent"
                )));
            }
        }
        wait(Duration::from_millis(500));
        self.timing(60, 0)?;
        self.expect(1, &[0], 0)?;
        self.protocol
            .emit(Event::Step("Waiting for the resident loader"));
        wait(Duration::from_secs(11));
        Ok(())
    }

    fn prepare_transfer(&mut self, key: [u8; 3]) -> Result<u16, DiagError> {
        let data = [key[0], key[1], key[2], !key[0], !key[1], !key[2]];
        self.expect(0x0B, &data, 0)?;
        let reply = self.command(0x0E, &[1, 0], REPLY_TIMEOUT)?;
        expect_reply(&reply, &[], 0x0E)?;
        let reply = self.command(1, &[1], REPLY_TIMEOUT)?;
        Ok(u16::from_be_bytes(reply.try_into().map_err(|_| {
            DiagError::InvalidResponse("Invalid loader pacing response".into())
        })?))
    }

    fn authenticate(&mut self, job: &FlashJob) -> Result<u16, DiagError> {
        let reply = self.command(2, &[], Duration::from_secs(12))?;
        let [_, high, low] = reply.as_slice() else {
            return Err(DiagError::InvalidResponse(
                "Expected three-byte loader seed response".into(),
            ));
        };
        let seed = u16::from_be_bytes([*high, *low]);
        if seed == 0 {
            return Err(DiagError::InvalidResponse(
                "Loader returned an all-zero seed".into(),
            ));
        }
        self.expect(3, &loader_key(seed, job.secret).to_be_bytes(), 0)?;
        self.prepare_transfer(job.cipher_key)
    }

    fn poll_pair(
        &mut self,
        command: u8,
        poll: u8,
        payload: &[u8],
        delay: Duration,
        done: u8,
    ) -> Result<(), DiagError> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            self.expect(command, payload, 0)?;
            wait(delay);
            let reply = self.command(poll, &[], REPLY_TIMEOUT)?;
            if reply == [done] {
                return Ok(());
            }
            if reply != [0x12] {
                return Err(DiagError::InvalidResponse(format!(
                    "Loader poll {poll:02X}: {reply:02X?}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(DiagError::Timeout(format!(
                    "Loader command/poll {command:02X}/{poll:02X} timed out"
                )));
            }
        }
    }

    fn transfer(
        &mut self,
        records: &[HondaRwdRecord],
        pacing_ms: u16,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        self.protocol.emit(Event::Step("Writing firmware"));
        let mut index = 0;
        let mut attempts = vec![0u8; records.len()];
        let mut mismatched = false;
        let mut warned = false;
        self.progress(0, records.len());
        while index < records.len() {
            if cancel.is_cancelled() && !warned {
                self.protocol.emit(Event::Warning(
                    "Cancellation deferred until firmware writing and post-flash checks finish",
                ));
                warned = true;
            }
            attempts[index] += 1;
            if attempts[index] > 4 {
                return Err(DiagError::InvalidResponse(format!(
                    "Loader retry limit exceeded at record {index}"
                )));
            }
            let mut reply =
                self.command(6, &record_payload(records, index), Duration::from_secs(3))?;
            let last = index + 1 == records.len();
            if pacing_ms != 0 {
                if reply == [2] && last {
                    return Err(DiagError::InvalidResponse(
                        "Loader requested more records after the final record".into(),
                    ));
                }
                if reply == [0] {
                    let deadline = Instant::now() + Duration::from_secs(60);
                    loop {
                        reply = self.command(7, &[], REPLY_TIMEOUT)?;
                        if matches!(reply.as_slice(), [3, _, _]) {
                            break;
                        }
                        expect_reply(&reply, &[0], 7)?;
                        wait(Duration::from_millis(u64::from(pacing_ms)));
                        reply = self.command(8, &[], REPLY_TIMEOUT)?;
                        if reply == [0] {
                            break;
                        }
                        if reply != [0x12] || Instant::now() >= deadline {
                            return Err(DiagError::InvalidResponse(format!(
                                "Loader record poll 08: {reply:02X?}"
                            )));
                        }
                    }
                }
            }
            if let [3, high, low] = reply.as_slice() {
                let tag = u32::from(u16::from_be_bytes([*high, *low]));
                let wanted = (index.saturating_sub(1)..=(index + 1).min(records.len() - 1))
                    .find(|&candidate| records[candidate].address >> 4 == tag)
                    .ok_or_else(|| {
                        DiagError::InvalidResponse(format!(
                            "Unexpected resync tag {tag:04X} at record {index}"
                        ))
                    })?;
                self.protocol.emit(Event::Detail(&format!(
                    "Loader resynchronized from record {index} to {wanted}"
                )));
                index = wanted;
                mismatched = false;
                continue;
            }
            if reply != [0] && reply != [2] {
                return Err(DiagError::InvalidResponse(format!(
                    "Record {index} rejected: {reply:02X?}"
                )));
            }
            if pacing_ms == 0 && reply != [if last { 0 } else { 2 }] {
                if mismatched {
                    return Err(DiagError::InvalidResponse(format!(
                        "Repeated terminal/continue status mismatch at record {index}"
                    )));
                }
                mismatched = true;
                continue;
            }
            mismatched = false;
            index += 1;
            self.progress(index, records.len());
        }
        if pacing_ms == 0 {
            let reply = self.command(8, &[], Duration::from_secs(60))?;
            expect_reply(&reply, &[0], 8)?;
        }
        Ok(())
    }

    fn progress(&mut self, done: usize, total: usize) {
        self.protocol.emit(Event::Progress {
            done: (done * X31_BLOCK_SIZE) as u64,
            total: (total * X31_BLOCK_SIZE) as u64,
            unit: ProgressUnit::Bytes,
        });
    }

    fn write_application(
        &mut self,
        job: &FlashJob,
        pacing: u16,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        self.protocol
            .emit(Event::Step("Erasing application memory"));
        self.poll_pair(4, 5, &[0xFF], Duration::from_secs(1), 0)?;
        self.transfer(&job.records, pacing, cancel)?;
        self.protocol
            .emit(Event::Step("Checking programmed firmware"));
        self.poll_pair(9, 0x0A, &[], Duration::from_millis(100), 0)?;
        self.poll_pair(0x0C, 0x0D, &[], Duration::from_millis(500), 0x0F)?;
        self.completed = true;
        self.voltage(false)?;
        self.protocol
            .emit(Event::Step("Returning to the application"));
        self.timing(60, 2)?;
        self.expect(1, &[0], 0x0F)?;
        self.expect(0x0F, &[], 0)?;
        self.expect(1, &[0], 0x0F)?;
        self.expect(0x10, &[], 0)?;
        wait(Duration::from_secs(1));
        // The image lists the calibrations it installs over, so the new ID need not appear.
        let calibration = self.identify()?;
        self.protocol.emit(Event::Detail(&format!(
            "Application responded with {calibration}; full flash readback was not performed"
        )));
        Ok(())
    }

    fn recover(&mut self, job: &FlashJob, cancel: &Cancel) -> Result<(), DiagError> {
        self.protocol
            .emit(Event::Step("Recovering the retained firmware image"));
        self.timing(60, 2)?;
        self.voltage(true)?;
        wait(Duration::from_millis(100));
        let state = self.command(1, &[0], REPLY_TIMEOUT)?;
        if !matches!(state.as_slice(), [0x0D | 0x10 | 0x20 | 0x30 | 0xFA | 0xFB]) {
            return Err(DiagError::InvalidResponse(format!(
                "Unsupported loader recovery state {state:02X?}"
            )));
        }
        let pacing = self.prepare_transfer(job.cipher_key)?;
        self.write_application(job, pacing, cancel)
    }

    fn run_job(
        &mut self,
        job: &FlashJob,
        interaction: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        let bus = self.protocol.bus().clone();
        let mut raw = bus.clone();
        raw.framing = Framing::Raw;
        raw.timing = BTreeMap::from([
            ("P1_MAX".into(), 40),
            ("P2_MAX".into(), 600),
            ("P3_MIN".into(), 60),
            ("P4_MIN".into(), 2),
        ]);
        self.protocol.switch_bus(&raw)?;
        let result = (|| {
            let calibration = self.identify()?;
            if calibration != job.calibration {
                return Err(DiagError::PreflightFailed(format!(
                    "ECU calibration changed after preflight: {} -> {calibration}",
                    job.calibration
                )));
            }
            cancel.check()?;
            self.enter(cancel)?;
            cancel.check()?;
            let pacing = self.authenticate(job)?;
            // The last safe boundary. `recover` shares `write_application` and must
            // stay usable after a deferred cancellation, so this check stays here.
            cancel.check()?;
            self.write_application(job, pacing, cancel)
        })();
        let mut result = result;
        while let Err(error) = &result {
            if !self.loader_entered || self.completed || !self.voltage_active {
                break;
            }
            // Cancellation is only honored before erase, so there is nothing to recover.
            if matches!(error, DiagError::Cancelled) {
                self.protocol.emit(Event::Warning(
                    "Cancelled before erase; the application was not erased, but the ECU is still in its loader.",
                ));
                break;
            }
            self.protocol.emit(Event::Warning("Pin 12 remains enabled. Keep the same ECU and its power connected. Choosing stop releases pin 12."));
            let reason = error.to_string();
            match interaction.flash_recovery(&reason) {
                FlashRecovery::Stop => break,
                FlashRecovery::Status => {
                    let status = self
                        .timing(60, 2)
                        .and_then(|()| self.command(1, &[0], REPLY_TIMEOUT));
                    match status {
                        Ok(state) => self
                            .protocol
                            .emit(Event::Step(&format!("Loader state: {state:02X?}"))),
                        Err(error) => self.protocol.emit(Event::Warning(&error.to_string())),
                    }
                }
                FlashRecovery::Retry => result = self.recover(job, cancel),
            }
        }
        if self.voltage_active {
            if let Err(error) = self.voltage(false) {
                self.protocol.emit(Event::Warning(&format!(
                    "Failed to release programming voltage: {error}"
                )));
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
        // Do not initialize an interrupted loader as if it were an OBD application.
        if result.is_ok() || !self.loader_entered {
            let restored = self
                .protocol
                .switch_bus(&bus)
                .and_then(|()| self.protocol.set_ecu(EcuAddress::Byte(OBD_ADDRESS)));
            if result.is_ok() {
                restored?;
            }
        }
        result
    }
}

fn expect_reply(reply: &[u8], expected: &[u8], command: u8) -> Result<(), DiagError> {
    if reply == expected {
        Ok(())
    } else {
        Err(DiagError::InvalidResponse(format!(
            "Honda command {command:02X}: expected {expected:02X?}, received {reply:02X?}"
        )))
    }
}

impl Drop for HondaKLineStrategy<'_> {
    fn drop(&mut self) {
        if self.voltage_active {
            if let Err(error) = self.voltage(false) {
                self.protocol.emit(Event::Warning(&format!(
                    "Failed to release programming voltage: {error}"
                )));
            }
        }
    }
}

impl FlashStrategy for HondaKLineStrategy<'_> {
    fn ecu_type(&self) -> &'static str {
        "HONDA_KLINE"
    }

    fn info(&mut self) -> Result<EcuInfo, DiagError> {
        let data = self
            .protocol
            .read_mode09(EcuAddress::Byte(OBD_ADDRESS), 4)?;
        if data.len() != 17 || data[0] != 1 {
            return Err(DiagError::InvalidResponse(
                "RAD requires one complete Mode 09 calibration record".into(),
            ));
        }
        Ok(EcuInfo {
            calibration: Some(crate::diag::text(&data[1..])),
            ..Default::default()
        })
    }

    fn preflight(&mut self, request: &PreparedFlash) -> Result<Preflight, DiagError> {
        self.job = None;
        validate_input(&request.bytes, &request.options, request.raw)?;
        let bus = self.protocol.bus();
        if bus.kind != BusKind::Iso9141
            || bus.bitrate != 10400
            || bus.pins != [7]
            || bus.init_address.unwrap_or(0x33) != 0x33
            || request.target != EcuAddress::Byte(OBD_ADDRESS)
        {
            return Err(DiagError::Unsupported("RAD programming requires ECU 0x09 on ISO 9141, 10400 bit/s, pin 7 and init address 0x33".into()));
        }
        let calibration = self.info()?.calibration.unwrap();
        let container = if request.raw {
            None
        } else {
            Some(parse_x31(&request.bytes)?)
        };
        let tokens = request
            .options
            .token_data
            .as_deref()
            .map(parse_x31)
            .transpose()?;
        let mut preflight = Preflight::new("Flash ECU firmware", "This erases and rewrites the ECU application. Keep ECU power connected until programming and post-flash checks finish.");
        preflight.add(
            "file",
            format!(
                "{} (honda_kline), {} bytes, ECU {}",
                request.image.format, request.image.size, request.target
            ),
            CheckStatus::Ok,
        );
        preflight.add(
            "ECU",
            &calibration,
            if calibration.starts_with("37805-RAD-") {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            },
        );
        let compatible = container
            .as_ref()
            .map(|rwd| rwd.compatible_versions.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if container.is_some() {
            preflight.add(
                "checksum",
                "x31 additive checksum verified",
                CheckStatus::Ok,
            );
            preflight.add(
                "compatibility",
                "Live calibration must be listed in the image",
                if compatible.contains(&calibration) {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Fail
                },
            );
        }
        let secret =
            if let Some(secret) = &request.options.security_key {
                Some(secret.as_slice().try_into().map_err(|_| {
                    DiagError::InvalidCommand("Expected six security-key bytes".into())
                })?)
            } else {
                tokens
                    .as_ref()
                    .or(container.as_ref())
                    .and_then(|rwd| rwd.compatible_versions.get(&calibration))
                    .copied()
            };
        preflight.add(
            "key",
            if secret.is_some() {
                "Security material resolved for the live calibration"
            } else {
                "No security material matches the live calibration"
            },
            if secret.is_some() {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            },
        );
        let cipher_key = container
            .as_ref()
            .or(tokens.as_ref())
            .map_or([0; 3], |rwd| rwd.decryption_key);
        let records = match container {
            Some(rwd) => rwd.records,
            None => raw_records(
                &request.bytes,
                request.options.start_address.unwrap(),
                cipher_key,
            )?,
        };
        preflight.add(
            "memory range",
            format!(
                "0x{:X}..0x{:X}, {} sparse records",
                records[0].address,
                records.last().unwrap().address + X31_BLOCK_SIZE as u32,
                records.len()
            ),
            CheckStatus::Info,
        );
        if request.raw {
            preflight.add("erase scope", "The entire application is erased; the raw image must contain all firmware needed to boot", CheckStatus::Warn);
        }
        let (battery, status) = crate::preflight::battery_check(
            self.protocol,
            request.options.system_voltage,
            request.options.skip_battery_check,
        )?;
        preflight.add("battery", battery, status);
        let packet_seconds = (139.0 + 5.0) * 10.0 / 10400.0 + 0.030;
        let duration = format_duration((records.len() as f64 * packet_seconds).ceil() as u64);
        preflight.add(
            "duration",
            format!("at least {duration} of transfer, plus erase, ECU processing and checks"),
            CheckStatus::Info,
        );
        if let Some(secret) = secret {
            self.job = Some(FlashJob {
                calibration,
                secret,
                cipher_key,
                records,
            });
        }
        Ok(preflight)
    }

    fn execute(
        &mut self,
        _permit: FlashPermit,
        interaction: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        let job = self.job.take().ok_or_else(|| {
            DiagError::InvalidCommand("Flash preflight has not prepared a firmware job".into())
        })?;
        cancel.check()?;
        self.run_job(&job, interaction, cancel)
    }
}

pub struct HondaKLineStrategyFactory;
impl StrategyFactory for HondaKLineStrategyFactory {
    fn validate_input(
        &self,
        bytes: &[u8],
        options: &FlashOptions,
        raw: bool,
    ) -> Result<(), DiagError> {
        validate_input(bytes, options, raw)
    }
    fn name(&self) -> &'static str {
        "honda_kline"
    }
    fn options(&self) -> &'static [&'static str] {
        &["tokens"]
    }
    fn inspect(&self, bytes: &[u8]) -> Result<Option<ImageInfo>, DiagError> {
        if !bytes.starts_with(b"1\r\n") {
            return Ok(None);
        }
        let rwd = parse_x31(bytes)?;
        validate_package(&rwd)?;
        Ok(Some(ImageInfo {
            format: "honda-rwd-x31",
            target: Some(EcuAddress::Byte(OBD_ADDRESS)),
            compatible: rwd.compatible_versions.keys().cloned().collect(),
            size: rwd.records.len() * X31_BLOCK_SIZE,
            start: rwd.records.first().map(|record| record.address),
        }))
    }
    fn create_strategy<'a>(&self, protocol: &'a mut ObdProtocol) -> Box<dyn FlashStrategy + 'a> {
        Box::new(HondaKLineStrategy::new(protocol))
    }
}

#[cfg(test)]
mod tests;
