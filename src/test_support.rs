//! Shared test fakes. Compiled only under `cfg(test)`.
//!
//! Three fakes, because the code under test has three natural seams:
//!
//! * [`MockPort`] overrides `send_command`, so protocol/UDS tests script whole
//!   ELM327 responses and assert on the command strings that were sent.
//! * [`ByteScriptPort`] implements only the raw byte methods, leaving the real
//!   `OpenDiagPort::send_command` default in play — that is what exercises the
//!   `>`-prompt handshake itself.
//! * [`FakeEcu`] is a stateful ECU simulator: it reassembles ISO-TP requests,
//!   answers them according to the UDS state machine, and refuses operations
//!   attempted out of order. It is what makes a whole-flash test possible.
//!
//! [`RwdFile`] builds `.rwd` container bytes for tests that need a real file.

use std::cell::{Ref, RefCell};
use std::collections::VecDeque;
use std::io::{Error, ErrorKind};
use std::rc::Rc;
use std::time::Duration;

use crate::obd::ObdProtocol;
use crate::ports::OpenDiagPort;
use crate::utils::{bytes_to_hex, hex_to_bytes};

#[derive(Default)]
struct MockInner {
    commands: Vec<String>,
    responses: VecDeque<String>,
}

/// A fake adapter that replays scripted responses at the `send_command` level.
///
/// Cloning shares state, so a clone can be boxed into an [`ObdProtocol`] while
/// the test keeps a handle to inspect the commands afterwards.
#[derive(Clone, Default)]
pub struct MockPort {
    inner: Rc<RefCell<MockInner>>,
}

impl MockPort {
    pub fn new(responses: &[&str]) -> Self {
        Self {
            inner: Rc::new(RefCell::new(MockInner {
                commands: Vec::new(),
                responses: responses.iter().map(|s| s.to_string()).collect(),
            })),
        }
    }

    /// Commands sent so far, in order.
    pub fn commands(&self) -> Vec<String> {
        self.inner.borrow().commands.clone()
    }

    /// Scripted responses that were never consumed.
    pub fn unused_responses(&self) -> usize {
        self.inner.borrow().responses.len()
    }
}

impl OpenDiagPort for MockPort {
    fn write_data(&mut self, _data: &[u8]) -> Result<(), Error> {
        unreachable!("MockPort overrides send_command; use ByteScriptPort for byte-level tests")
    }

    fn read_data(&mut self, _data: &mut [u8]) -> Result<usize, Error> {
        unreachable!("MockPort overrides send_command; use ByteScriptPort for byte-level tests")
    }

    fn clear_input(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn set_baudrate(&mut self, _baud_rate: u32) -> Result<(), Error> {
        Ok(())
    }

    fn send_command(&mut self, command: &str, _timeout: Option<Duration>) -> Result<String, Error> {
        let mut inner = self.inner.borrow_mut();
        inner.commands.push(command.to_string());
        inner.responses.pop_front().ok_or_else(|| {
            Error::new(
                ErrorKind::TimedOut,
                format!("no scripted response left for command '{}'", command),
            )
        })
    }
}

/// Builds an [`ObdProtocol`] over a [`MockPort`], returning the port handle too.
pub fn protocol_with(responses: &[&str]) -> (ObdProtocol, MockPort) {
    let port = MockPort::new(responses);
    (ObdProtocol::new(Box::new(port.clone())), port)
}

/// A byte-level fake: each `read_data` call yields the next scripted chunk, so
/// tests can split a response across reads the way a real serial port does.
/// An empty chunk simulates a per-read timeout; so does running out of chunks.
#[derive(Default)]
pub struct ByteScriptPort {
    chunks: VecDeque<Vec<u8>>,
    pub written: Vec<u8>,
    pub clear_calls: usize,
}

impl ByteScriptPort {
    pub fn new(chunks: &[&str]) -> Self {
        Self {
            chunks: chunks.iter().map(|c| c.as_bytes().to_vec()).collect(),
            written: Vec::new(),
            clear_calls: 0,
        }
    }
}

impl OpenDiagPort for ByteScriptPort {
    fn write_data(&mut self, data: &[u8]) -> Result<(), Error> {
        self.written.extend_from_slice(data);
        Ok(())
    }

    fn read_data(&mut self, data: &mut [u8]) -> Result<usize, Error> {
        match self.chunks.pop_front() {
            Some(chunk) if !chunk.is_empty() => {
                let n = chunk.len().min(data.len());
                data[..n].copy_from_slice(&chunk[..n]);
                Ok(n)
            }
            // An empty chunk, or nothing left to hand out, behaves like a port
            // whose per-read timeout expired — `send_command` must tolerate it.
            _ => Err(Error::new(ErrorKind::TimedOut, "no data available")),
        }
    }

    fn clear_input(&mut self) -> Result<(), Error> {
        self.clear_calls += 1;
        Ok(())
    }

    fn set_baudrate(&mut self, _baud_rate: u32) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------- .rwd files

/// Encodes one `.rwd` header: a count byte followed by length-prefixed entries.
pub fn rwd_header(entries: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![entries.len() as u8];
    for e in entries {
        out.push(e.len() as u8);
        out.extend_from_slice(e);
    }
    out
}

/// A `.rwd` container described in terms of what it means, so tests can vary
/// one field without hand-assembling the whole byte layout.
pub struct RwdFile {
    /// Header 2 byte 0: OR'd into `0x18DA00F1` to give the target CAN address.
    pub can_offset: u8,
    /// Header 3: calibration ids this firmware may be flashed over. NUL padding
    /// is added here rather than by the builder, since stripping it is part of
    /// what the parser is expected to do.
    pub versions: Vec<Vec<u8>>,
    /// Header 4: security-access secrets, paired with `versions` by index.
    pub tokens: Vec<Vec<u8>>,
    /// Header 5: the firmware decryption key written to DID 0xF101.
    pub decryption_key: Vec<u8>,
    pub start: u32,
    pub firmware: Vec<u8>,
    pub checksum: u32,
}

impl Default for RwdFile {
    fn default() -> Self {
        Self {
            can_offset: 0x01,
            versions: vec![b"37805-RGL-A030\0".to_vec()],
            tokens: vec![vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66]],
            decryption_key: vec![0xDE, 0xAD, 0xBE],
            start: 0x0001_0000,
            firmware: vec![0xA5; 600],
            checksum: 0x1234_5678,
        }
    }
}

impl RwdFile {
    pub fn encode(&self) -> Vec<u8> {
        let version_refs: Vec<&[u8]> = self.versions.iter().map(|v| v.as_slice()).collect();
        let token_refs: Vec<&[u8]> = self.tokens.iter().map(|v| v.as_slice()).collect();

        let mut f = vec![0x5A, 0x0D, 0x0A];
        f.extend(rwd_header(&[b"header0"]));
        f.extend(rwd_header(&[]));
        f.extend(rwd_header(&[&[self.can_offset]]));
        f.extend(rwd_header(&version_refs));
        f.extend(rwd_header(&token_refs));
        f.extend(rwd_header(&[&self.decryption_key]));
        f.extend_from_slice(&self.start.to_be_bytes());
        f.extend_from_slice(&(self.firmware.len() as u32).to_be_bytes());
        f.extend_from_slice(&self.firmware);
        f.extend_from_slice(&self.checksum.to_le_bytes());
        f
    }

    /// The CAN address this file targets, as the parser will compute it.
    pub fn can_address(&self) -> u32 {
        0x18DA00F1 | ((self.can_offset as u32) << 8)
    }
}

// ----------------------------------------------------------------- fake ECU

/// What the ECU has been asked to do so far, and what it has been told.
#[derive(Default)]
pub struct EcuState {
    // --- configuration ---
    response_id: u32,
    calibration_id: Vec<u8>,
    vin: Vec<u8>,
    /// The K1/K2/K3 constants the unlock key is validated against.
    security_secret: Vec<u8>,
    seed: u16,
    max_block_size: u16,

    // --- observations the test asserts on ---
    pub commands: Vec<String>,
    /// Every `ATPROGV` as (pin, millivolts), in order.
    pub programming_voltage: Vec<(u32, u32)>,
    /// Every `ATSH`, in order.
    pub headers_set: Vec<u32>,
    pub unlocked: bool,
    pub session: u8,
    pub erased: bool,
    pub decryption_key: Vec<u8>,
    pub download_address: u32,
    pub download_len: u32,
    pub received_firmware: Vec<u8>,
    pub transfer_exited: bool,
    pub dependencies_checked: bool,
    /// Set if the tester ever sent a wrong unlock key or an out-of-order block.
    pub protocol_violations: Vec<String>,

    // --- ISO-TP reassembly of the incoming request ---
    pending: Option<(usize, Vec<u8>)>,
    next_cf_seq: u8,
    expected_block_counter: u8,
}

/// The unlock key the ECU expects, computed independently of the flasher's own
/// implementation so the two have to agree on the algorithm.
fn expected_unlock_key(seed: u16, secret: &[u8]) -> u16 {
    let k1 = u16::from_be_bytes([secret[0], secret[1]]) as u32;
    let k2 = u16::from_be_bytes([secret[2], secret[3]]) as u32;
    let seed = seed as u32;
    let mixed = if secret.len() == 6 {
        let k3 = u16::from_be_bytes([secret[4], secret[5]]) as u32;
        (seed * k2 % k3) ^ (seed + k1)
    } else {
        (seed * k2) ^ (seed + k1)
    };
    (mixed & 0xFFFF) as u16
}

impl EcuState {
    fn violation(&mut self, what: &str) -> Vec<u8> {
        self.protocol_violations.push(what.to_string());
        vec![0x7F, 0x00, 0x22] // conditions not correct
    }

    /// Handles one fully reassembled UDS request, returning the response payload.
    fn handle_uds(&mut self, req: &[u8]) -> Vec<u8> {
        match req[0] {
            // DiagnosticSessionControl
            0x10 => {
                self.session = req[1];
                vec![0x50, req[1], 0x00, 0x32, 0x01, 0xF4]
            }
            // SecurityAccess
            0x27 if req[1] == 0x01 => {
                let mut r = vec![0x67, 0x01];
                r.extend_from_slice(&self.seed.to_be_bytes());
                r
            }
            0x27 if req[1] == 0x02 => {
                let expected = expected_unlock_key(self.seed, &self.security_secret);
                let got = u16::from_be_bytes([req[2], req[3]]);
                if got == expected {
                    self.unlocked = true;
                    vec![0x67, 0x02]
                } else {
                    self.protocol_violations
                        .push(format!("wrong unlock key: got {:#06x}, want {:#06x}", got, expected));
                    vec![0x7F, 0x27, 0x35] // invalid key
                }
            }
            // ReadDataByIdentifier
            0x22 => {
                let did = u16::from_be_bytes([req[1], req[2]]);
                let mut r = vec![0x62, req[1], req[2]];
                match did {
                    0xF181 => r.extend_from_slice(&self.calibration_id),
                    0xF190 => {
                        r.push(self.vin.len() as u8);
                        r.extend_from_slice(&self.vin);
                    }
                    _ => return vec![0x7F, 0x22, 0x31], // request out of range
                }
                r
            }
            // WriteDataByIdentifier
            0x2E => {
                if !self.unlocked {
                    return self.violation("WriteDataByIdentifier before SecurityAccess");
                }
                self.decryption_key = req[3..].to_vec();
                vec![0x6E, req[1], req[2]]
            }
            // RoutineControl
            0x31 => {
                let routine = u16::from_be_bytes([req[2], req[3]]);
                match routine {
                    0xFF00 => {
                        if !self.unlocked {
                            return self.violation("EraseMemory before SecurityAccess");
                        }
                        if self.session != 0x02 {
                            return self.violation("EraseMemory outside the programming session");
                        }
                        self.erased = true;
                    }
                    0xFF01 => {
                        if !self.transfer_exited {
                            return self.violation("CheckProgrammingDependencies before TransferExit");
                        }
                        self.dependencies_checked = true;
                    }
                    _ => return vec![0x7F, 0x31, 0x31],
                }
                vec![0x71, req[1], req[2], req[3], 0x00]
            }
            // RequestDownload
            0x34 => {
                if !self.erased {
                    return self.violation("RequestDownload before EraseMemory");
                }
                self.download_address = u32::from_be_bytes([req[3], req[4], req[5], req[6]]);
                self.download_len = u32::from_be_bytes([req[7], req[8], req[9], req[10]]);
                self.expected_block_counter = 1;
                let mut r = vec![0x74, 0x20];
                r.extend_from_slice(&self.max_block_size.to_be_bytes());
                r
            }
            // TransferData
            0x36 => {
                if self.download_len == 0 {
                    return self.violation("TransferData before RequestDownload");
                }
                // The whole request, service id and counter included, has to fit
                // in the block size this ECU advertised in RequestDownload.
                if req.len() > self.max_block_size as usize {
                    let msg = format!(
                        "TransferData of {} bytes exceeds the advertised max block size of {}",
                        req.len(),
                        self.max_block_size
                    );
                    return self.violation(&msg);
                }
                if self.received_firmware.len() + req.len() - 2 > self.download_len as usize {
                    return self.violation("TransferData wrote past the requested download length");
                }
                if req[1] != self.expected_block_counter {
                    return self.violation(&format!(
                        "block counter out of order: got {}, want {}",
                        req[1], self.expected_block_counter
                    ));
                }
                self.expected_block_counter = self.expected_block_counter.wrapping_add(1);
                self.received_firmware.extend_from_slice(&req[2..]);
                vec![0x76, req[1]]
            }
            // RequestTransferExit
            0x37 => {
                self.transfer_exited = true;
                vec![0x77]
            }
            other => vec![0x7F, other, 0x11], // service not supported
        }
    }

    /// Wraps a response payload in as many ISO-TP frames as it needs. They are
    /// all returned in one buffer, which is what a real ELM327 hands back once
    /// it has done the flow-control dance on the tester's behalf.
    fn frame_response(&self, payload: &[u8]) -> String {
        let id = self.response_id;
        if payload.len() <= 7 {
            return format!("{:08X}{:02X}{}\r", id, payload.len(), bytes_to_hex(payload));
        }

        let mut out = format!(
            "{:08X}{:02X}{:02X}{}\r",
            id,
            0x10 | (payload.len() >> 8),
            payload.len() & 0xFF,
            bytes_to_hex(&payload[..6])
        );
        let mut offset = 6;
        let mut seq = 1u8;
        while offset < payload.len() {
            let take = (payload.len() - offset).min(7);
            out.push_str(&format!(
                "{:08X}{:02X}{}\r",
                id,
                0x20 | seq,
                bytes_to_hex(&payload[offset..offset + take])
            ));
            offset += take;
            seq = (seq + 1) % 16;
        }
        out
    }
}

/// A stateful ECU behind an ELM327-shaped port.
#[derive(Clone)]
pub struct FakeEcu {
    inner: Rc<RefCell<EcuState>>,
}

impl FakeEcu {
    /// An ECU running `calibration_id`, unlockable with `security_secret`.
    pub fn new(response_id: u32, calibration_id: &[u8], security_secret: &[u8]) -> Self {
        Self {
            inner: Rc::new(RefCell::new(EcuState {
                response_id,
                calibration_id: calibration_id.to_vec(),
                vin: b"1HGCM82633A004352".to_vec(),
                security_secret: security_secret.to_vec(),
                seed: 0x1234,
                max_block_size: 0x0102,
                next_cf_seq: 1,
                ..Default::default()
            })),
        }
    }

    pub fn state(&self) -> Ref<'_, EcuState> {
        self.inner.borrow()
    }
}

impl OpenDiagPort for FakeEcu {
    fn write_data(&mut self, _data: &[u8]) -> Result<(), Error> {
        unreachable!("FakeEcu overrides send_command")
    }

    fn read_data(&mut self, _data: &mut [u8]) -> Result<usize, Error> {
        unreachable!("FakeEcu overrides send_command")
    }

    fn clear_input(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn set_baudrate(&mut self, _baud_rate: u32) -> Result<(), Error> {
        Ok(())
    }

    fn send_command(&mut self, command: &str, _timeout: Option<Duration>) -> Result<String, Error> {
        let mut st = self.inner.borrow_mut();
        st.commands.push(command.to_string());

        // --- adapter commands ---
        if command.starts_with("AT") {
            if let Some(rest) = command.strip_prefix("ATSH ") {
                let id = u32::from_str_radix(rest, 16)
                    .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
                st.headers_set.push(id);
            } else if let Some(rest) = command.strip_prefix("ATPROGV") {
                // Formatted as pin (variable width) then voltage (8 hex digits).
                let split = rest.len() - 8;
                let pin = u32::from_str_radix(&rest[..split], 16)
                    .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
                let millivolts = u32::from_str_radix(&rest[split..], 16)
                    .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
                st.programming_voltage.push((pin, millivolts));
            } else if command == "ATZ" {
                return Ok("ATZ\rELM327 v1.5".to_string());
            } else if command == "ATRV" {
                return Ok("12.4V".to_string());
            }
            return Ok("OK".to_string());
        }

        // --- CAN frames ---
        // An odd-length hex string carries the ELM327 "replies to expect" digit;
        // it is adapter syntax, not part of the frame.
        let hex = if command.len() % 2 == 1 {
            &command[..command.len() - 1]
        } else {
            command
        };
        let bytes = hex_to_bytes(hex).map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        if bytes.is_empty() {
            return Err(Error::new(ErrorKind::InvalidData, "empty frame"));
        }

        let pci = bytes[0];
        let request = match pci >> 4 {
            0 => {
                // Single frame: the whole request is here.
                let len = (pci & 0x0F) as usize;
                if bytes.len() < 1 + len {
                    return Err(Error::new(ErrorKind::InvalidData, "short single frame"));
                }
                Some(bytes[1..1 + len].to_vec())
            }
            1 => {
                // First frame: acknowledge with Flow Control and wait for the rest.
                let total = (((pci & 0x0F) as usize) << 8) | bytes[1] as usize;
                st.pending = Some((total, bytes[2..].to_vec()));
                st.next_cf_seq = 1;
                let id = st.response_id;
                return Ok(format!("{:08X}300000\r", id));
            }
            2 => {
                let seq = pci & 0x0F;
                if seq != st.next_cf_seq {
                    let msg = format!(
                        "consecutive frame out of sequence: got {}, want {}",
                        seq, st.next_cf_seq
                    );
                    st.protocol_violations.push(msg);
                }
                st.next_cf_seq = (st.next_cf_seq + 1) % 16;
                match st.pending.take() {
                    Some((total, mut buf)) => {
                        buf.extend_from_slice(&bytes[1..]);
                        if buf.len() >= total {
                            buf.truncate(total);
                            Some(buf)
                        } else {
                            st.pending = Some((total, buf));
                            None
                        }
                    }
                    None => {
                        st.protocol_violations
                            .push("consecutive frame with no first frame".to_string());
                        None
                    }
                }
            }
            _ => None, // flow control from the tester: nothing to answer
        };

        match request {
            // Mid-message: the adapter was told not to expect a reply.
            None => Ok(String::new()),
            Some(req) => {
                let response = st.handle_uds(&req);
                Ok(st.frame_response(&response))
            }
        }
    }
}
