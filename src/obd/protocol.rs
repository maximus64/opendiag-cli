use crate::errors::DiagError;
use crate::utils::bytes_to_hex;
use crate::ports::OpenDiagPort;
use crate::obd::pids::{get_pid_definition};
use crate::obd::dtc::{get_dtc_description};
use crate::obd::addressing::Addressing;
use std::time::Duration;
use std::thread::sleep;
use colored::Colorize;

pub const VOLTAGE_PIN_OFF: u32 = 0xffffffff;
pub const VOLTAGE_SHORT_TO_GROUND: u32 = 0xfffffffe;

/// Every OBD pin the adapter can drive programming voltage on: high-side on
/// 6, 9, and 11–14, low-side on 15. No other pin is addressable, so there is
/// nothing to clear anywhere else.
const PROGRAMMING_VOLTAGE_PINS: [u32; 7] = [6, 9, 11, 12, 13, 14, 15];

// The debug version
#[cfg(debug_assertions)]
macro_rules! debug_print {
    ($( $args:expr ),*) => { println!( $( $args ),* ); }
}

#[cfg(debug_assertions)]
macro_rules! debug_hexdump {
    ($x:expr) => { hexdump( $x ); }
}

// Non-debug version
#[cfg(not(debug_assertions))]
macro_rules! debug_print {
    ($( $args:expr ),*) => {}
}

#[cfg(not(debug_assertions))]
macro_rules! debug_hexdump {
    ($x:expr) => {}
}


/// Response frame structure for OBD communication
#[derive(Debug)]
pub struct ResponseFrame {
    pub id: u32,
    pub data: Vec<u8>,
}

/// Diagnostic Trouble Code (DTC) structure
#[derive(Debug, Clone)]
pub struct DiagnosticTroubleCode {
    pub code: String,
    pub description: String,
}

/// OBD protocol implementation
/// This handles the core OBD-II communication logic
pub struct ObdProtocol {
    port: Box<dyn OpenDiagPort>,
    /// The CAN header last set with `ATSH`, so callers that know the right
    /// target (a firmware container naming its ECU, say) can tell whether they
    /// are about to redirect traffic away from what the user asked for.
    header_id: Option<u32>,
    /// The addressing rules `reset()` put the adapter into. Header widths and
    /// the request/response header relationship both come from here, so adding
    /// a protocol means teaching `Addressing` about it rather than hunting for
    /// hardcoded CAN arithmetic.
    addressing: Addressing,
}

impl ObdProtocol {
    pub fn new(port: Box<dyn OpenDiagPort>) -> Self {
        // Matches the protocol `reset()` selects; nothing is on the wire until
        // then, so this is only the starting assumption.
        ObdProtocol { port, header_id: None, addressing: Addressing::Can29Bit }
    }

    /// Reset the OBD device
    pub fn reset(&mut self) -> Result<(), DiagError> {
        let resp = self.port.send_command("ATZ", None)?;
        let lines = resp.split('\r')
            .map(|f| f.trim())
            .filter(|f| !f.is_empty())
            .collect::<Vec<&str>>();
        let answer_to_reset = lines.last().ok_or("Unknown reset response")?;
        println!("Answer to reset: {}", answer_to_reset);

        // Switch to higher baud rate
        //self.port.send_command("AT BRD 23", None)?;
        //self.port.set_baudrate(115200)?;

        // let mut temp_buffer = [0; 128];
        // //self.port.read_data(&mut temp_buffer)?;

        // self.port.send_command("", None)?;

        self.port.send_command("ATE0", None)?;
        self.port.send_command("ATH1", None)?;
        self.port.send_command("ATS0", None)?;
        self.port.send_command("ATSP7", None)?;
        self.addressing = Addressing::Can29Bit;
        //self.port.send_command("ATST FF", None)?;
        self.port.send_command("ATCAF0", None)?;

        Ok(())
    }

    /// Drops programming voltage on every pin the adapter can drive.
    ///
    /// Call this once after connecting. A flash releases its own pin through a
    /// guard, but a process killed with SIGINT never unwinds, so a run that was
    /// interrupted mid-flash can leave a pin live. Clearing all of them keeps
    /// this free of per-vehicle knowledge — which pin a given ECU needs is the
    /// flash strategy's business, not the protocol layer's.
    pub fn clear_all_programming_voltage(&mut self) -> Result<(), DiagError> {
        for pin in PROGRAMMING_VOLTAGE_PINS {
            self.set_programming_voltage(pin, VOLTAGE_PIN_OFF)?;
        }
        Ok(())
    }


    pub fn set_header_id(&mut self, id: u32) -> Result<(), DiagError> {
        // `ATSH` takes the header at its protocol's own width: eight hex digits
        // for 29-bit CAN, three for 11-bit, six for the three-byte headers of
        // K-Line and J1850.
        let header = self.addressing.format_header(id);
        self.port.send_command(&format!("ATSH {header}"), None)?;
        self.header_id = Some(id);
        Ok(())
    }

    /// The CAN header currently in effect, if one has been set this session.
    pub fn header_id(&self) -> Option<u32> {
        self.header_id
    }

    /// The addressing rules in force, for callers that need to translate
    /// between response and request headers.
    pub fn addressing(&self) -> Addressing {
        self.addressing
    }

    /// Read voltage from the device
    pub fn read_voltage(&mut self) -> Result<String, DiagError> {
        let resp = self.port.send_command("ATRV", None)?;
        Ok(resp)
    }

    /// Set programming voltage on the device
    pub fn set_programming_voltage(&mut self, pin: u32, voltage: u32) -> Result<(), DiagError> {
        let cmd = format!("ATPROGV{:X}{:08X}", pin, voltage);
        let resp = self.port.send_command(&cmd, None)?;
        Ok(())
    }

    /// Reads all supported PIDs from the ECU
    pub fn read_supported_pids(&mut self, sid: u8) -> Result<Vec<u8>, DiagError> {
        let mut supported_pids = Vec::new();
        let mut current_pid_base: u8 = 0x00;

        loop {
            // Construct the OBD-II request frame for supported PIDs.
            let request_frame: [u8; 2] = [sid, current_pid_base];

            println!("Querying for PIDs starting at 0x{:02X}...", current_pid_base);

            let response = match self.transfer(&request_frame) {
                Ok(resp) => resp,
                Err(e) => {
                    // If the first query (PID 0x00) fails, we can't proceed.
                    if current_pid_base == 0x00 {
                        return Err(e);
                    } else {
                        // For subsequent PIDs (0x20, 0x40, etc.), an error likely means it's not supported.
                        println!("No response for PID 0x{:02X}, assuming end of supported list.", current_pid_base);
                        break;
                    }
                }
            };

            // A valid Mode 01 response starts with 0x41 and echoes the requested PID.
            if response.data.len() < 6 || response.data[0] != (sid + 0x40) || response.data[1] != current_pid_base {
                return Err(DiagError::InvalidResponse(format!(
                    "Unexpected response for supported PIDs query at 0x{:02X}", current_pid_base
                )));
            }

            // The next 4 bytes are the bitmap for the PIDs.
            let bitmap = &response.data[2..6];

            let mut next_pid_group_supported = false;

            for (i, &byte) in bitmap.iter().enumerate() {
                for j in 0..8 {
                    if (byte >> (7 - j)) & 1 == 1 {
                        let pid = current_pid_base + (i as u8 * 8) + (j as u8) + 1;
                        if (i == 3) && (j == 7) {
                            // The last bit of the bitmap indicates the next group is available.
                            next_pid_group_supported = true;
                        }
                        else {
                            supported_pids.push(pid);
                        }
                    }
                }
            }

            if next_pid_group_supported {
                current_pid_base += 0x20;
                // Stop after checking 0xC0 to prevent potential infinite loops on some ECUs.
                if current_pid_base >= 0xE0 {
                    break;
                }
            } else {
                break; // No more supported PID groups to check.
            }
        }

        supported_pids.sort();
        Ok(supported_pids)
    }

    /// Reads all supported PIDs and their current values.
    pub fn read_all_vehicle_data(&mut self, sid: u8) -> Result<Vec<(String, String)>, DiagError> {
        println!("First, finding all supported PIDs...");
        let supported_pids = self.read_supported_pids(sid)?;
        let mut results = Vec::new();

        println!("Found {} supported PIDs. Now reading values...", supported_pids.len());
        for pid in supported_pids {
            // Look up the PID definition from our registry
            let definition = get_pid_definition(sid, pid);
            let request_frame: [u8; 2] = [sid, pid];

            match self.transfer(&request_frame) {
                Ok(data) => {
                    if let Some(def) = definition {
                        // We have a definition, so parse the data and add it
                        let parsed_value = (def.parser)(&data.data[2..]);
                        let final_value = format!("{} {}", parsed_value, def.unit);
                        println!("  0x{:02X}: {}: {}", pid, def.description.cyan(), final_value.green());
                        results.push((def.description.to_string(), final_value));
                    } else {
                        // No definition, just show the raw hex data
                        let raw_hex = bytes_to_hex(&data.data[2..]);
                        println!("  0x{:02X}: Unknown. Raw: {}", pid, raw_hex.yellow());
                        results.push((format!("Unknown PID 0x{:02X}", pid), raw_hex));
                    }
                },
                Err(e) => {
                    let name = definition.map_or_else(|| format!("PID 0x{:02X}", pid), |d| d.description.to_string());
                    eprintln!("  {} for {}: {}", "Failed to read".red(), name, e);
                }
            }
        }
        Ok(results)
    }

    /// Convert DTC bytes to DTC code string
    fn decode_dtc_bytes(bytes: &[u8]) -> Result<String, DiagError> {
        if bytes.len() < 2 {
            return Err(DiagError::InvalidResponse("DTC data too short".into()));
        }

        let first_byte = bytes[0];
        let second_byte = bytes[1];

        // Extract the first two bits for the DTC prefix
        let prefix = match first_byte >> 6 {
            0 => "P", // Powertrain
            1 => "C", // Chassis
            2 => "B", // Body
            3 => "U", // Network/Communication
            _ => return Err(DiagError::InvalidResponse("Invalid DTC prefix".into())),
        };

        // Extract the remaining bits for the DTC number
        let code_number = ((first_byte & 0x3F) as u16) << 8 | second_byte as u16;
        
        Ok(format!("{}{:04X}", prefix, code_number))
    }

    /// Read stored Diagnostic Trouble Codes (DTCs)
    pub fn read_dtc(&mut self, pending: bool) -> Result<Vec<DiagnosticTroubleCode>, DiagError> {       
        // Mode 03: Request stored DTCs
        // Mode 07: Request pending DTCs
        let request_frame: [u8; 1] = if pending {[0x07]} else {[0x03]};
        let response = self.transfer(&request_frame)?;

        // Validate response
        if response.data.is_empty() || 
           response.data[0] != if pending {0x47} else {0x43}  {
            return Err(DiagError::InvalidResponse("Invalid DTC response".into()));
        }

        let mut dtcs = Vec::new();

        // Check if we have at least the DTC count byte
        if response.data.len() < 2 {
            return Err(DiagError::InvalidResponse("Response too short for DTC count".into()));
        }

        // Extract DTC count (byte after 0x43)
        let dtc_count = response.data[1];
        debug_print!("ECU reports {} DTC(s)", dtc_count);
        
        // Check if there are no DTCs stored
        if dtc_count == 0 {
            debug_print!("No DTCs stored in ECU");
            return Ok(dtcs);
        }

        // Extract DTC data (skip the mode response byte 0x43 and DTC count byte)
        let dtc_data = &response.data[2..];
        
        // Validate we have the expected amount of DTC data (each DTC is 2 bytes)
        let expected_bytes = dtc_count as usize * 2;
        if dtc_data.len() < expected_bytes {
            return Err(DiagError::InvalidResponse(
                format!("Expected {} DTC bytes but got {}", expected_bytes, dtc_data.len())
            ));
        }
        
        // Each DTC is 2 bytes. Only the codes the ECU actually announced are
        // real; anything past them is CAN frame padding, which is often 0x55 or
        // 0xAA rather than zeros and would otherwise decode as phantom DTCs.
        for chunk in dtc_data[..expected_bytes].chunks(2) {
            if chunk.len() == 2 && !(chunk[0] == 0x00 && chunk[1] == 0x00) {
                match Self::decode_dtc_bytes(chunk) {
                    Ok(code) => {
                        let description = get_dtc_description(&code);
                        dtcs.push(DiagnosticTroubleCode {
                            code: code.clone(),
                            description: description.to_string(),
                        });
                    }
                    Err(e) => {
                        return Err(DiagError::InvalidResponse(
                            format!("Failed to decode DTC bytes {:02X}{:02X}: {}", chunk[0], chunk[1], e)
                        ));
                    }
                }
            }
        }

        // With padding excluded above, a shortfall means the ECU announced more
        // codes than it sent — report it instead of panicking on the user.
        if dtcs.len() != dtc_count as usize {
            return Err(DiagError::InvalidResponse(format!(
                "ECU reported {} DTC(s) but only {} could be decoded",
                dtc_count,
                dtcs.len()
            )));
        }
        Ok(dtcs)
    }

    /// Clear stored Diagnostic Trouble Codes (DTCs)
    pub fn clear_dtc(&mut self) -> Result<(), DiagError> {        
        // Mode 04: Clear DTCs and stored values
        let request_frame: [u8; 1] = [0x04];
        let response = self.transfer(&request_frame)?;

        // Validate response
        if response.data.is_empty() || response.data[0] != 0x44 {
            return Err(DiagError::InvalidResponse("Invalid clear DTC response".into()));
        }

        Ok(())
    }

    /// Sends a request and receives the full response using a compliant ISO-TP transport.
    /// This is the primary entry point for all diagnostic requests.
    pub fn transfer(&mut self, out_frame: &[u8]) -> Result<ResponseFrame, DiagError> {
        self.transfer_with_timeout(out_frame, None)
    }

    pub fn transfer_with_timeout(&mut self, out_frame: &[u8], timeout: Option<Duration>) -> Result<ResponseFrame, DiagError> {
        if out_frame.is_empty() {
            return Err(DiagError::InvalidCommand("Request frame cannot be empty".into()));
        }

        // Route to the appropriate handler based on payload size.
        if out_frame.len() <= 7 {
            self.transfer_iso_tp_single_frame(out_frame, timeout)
        } else {
            self.transfer_iso_tp_multi_frame(out_frame, timeout)
        }
    }

    /// Handles the simple send-then-receive flow for a Single-Frame message.
    fn transfer_iso_tp_single_frame(&mut self, payload: &[u8], timeout: Option<Duration>) -> Result<ResponseFrame, DiagError> {
        // A Single Frame's PCI is `0x0` + the length (1-7 bytes).
        let mut frame = vec![(0x00 | payload.len() as u8)];
        frame.extend_from_slice(payload);

        let hex = bytes_to_hex(&frame);
        let response_strings = self.port.send_command(&hex, timeout)?;
        
        // Now, call the dedicated reader to process the ECU's response.
        self.parse_iso_tp_response(&response_strings)
    }

    /// Decodes the STmin byte from a Flow Control frame into a Duration.
    fn decode_st_min(&self, st_min_byte: u8) -> Result<Duration, DiagError> {
        match st_min_byte {
            // Values 0x00 to 0x7F represent delay in milliseconds (0ms - 127ms)
            0x00..=0x7F => Ok(Duration::from_millis(st_min_byte as u64)),
            
            // Values 0xF1 to 0xF9 represent delay in microseconds (100us - 900us)
            0xF1..=0xF9 => Ok(Duration::from_micros((st_min_byte - 0xF0) as u64 * 100)),

            // Other values are reserved and should be treated as an error.
            _ => Err(DiagError::InvalidResponse(format!(
                "Invalid or reserved STmin value: 0x{:02X}", st_min_byte
            ))),
        }
    }

    pub fn broadcast(&mut self, out_frame: &[u8], timeout: Option<Duration>) -> Result<Vec<ResponseFrame>, DiagError> {
        if out_frame.is_empty() {
            return Err(DiagError::InvalidCommand("Request frame cannot be empty".into()));
        }

        if out_frame.len() > 7 {
            return Err(DiagError::InvalidCommand("Multi-Frame broadcast not supported".into()));
        }

        // A Single Frame's PCI is `0x0` + the length (1-7 bytes).
        let mut frame = vec![(0x00 | out_frame.len() as u8)];
        frame.extend_from_slice(out_frame);

        // Address the request functionally rather than trusting whatever header
        // the adapter powered up with or an earlier command left behind — a
        // broadcast sent to one ECU's physical address only ever finds that ECU.
        let functional = self.addressing.functional_request().ok_or_else(|| {
            DiagError::InvalidCommand(format!(
                "Broadcast is not supported on {:?} yet",
                self.addressing
            ))
        })?;
        let restore = self.header_id.filter(|previous| *previous != functional);
        self.set_header_id(functional)?;

        let hex = bytes_to_hex(&frame);
        let result = match self.port.send_command(&hex, timeout) {
            // Hand the buffer to the dedicated reader to process all ECU responses.
            Ok(response_strings) => self.parse_iso_tp_responses(&response_strings),
            Err(err) => Err(err.into()),
        };

        // Leave the session pointed where the caller had it. If the exchange
        // itself failed, that is the error worth reporting.
        if let Some(previous) = restore {
            let restored = self.set_header_id(previous);
            if result.is_ok() {
                restored?;
            }
        }

        result
    }

    /// Receives and assembles all complete ISO-TP messages from the response buffer.
    fn parse_iso_tp_responses(&mut self, response_buffer: &str) -> Result<Vec<ResponseFrame>, DiagError> {
        let frames = self.parse_frame_response(response_buffer);
        let mut complete_messages = Vec::new();

        for frame in frames {
            let pci = frame.data[0];
            let frame_type = pci >> 4;
            let can_id = frame.id;

            match frame_type {
                0 => { // Single Frame
                    let len = (pci & 0x0F) as usize;
                    if frame.data.len() < 1 + len {
                        return Err(DiagError::InvalidResponse("SF has invalid length".into()));
                    }
                    let payload = &frame.data[1..1 + len];
                    complete_messages.push(ResponseFrame { id: can_id, data: payload.to_vec() });
                }
                _ => {
                    return Err(DiagError::InvalidCommand("Only Single Frame broadcast supported".into()));
                }
            }
        }

        Ok(complete_messages)
    }

    /// Handles the complex, interleaved logic for a multi-frame ISO-TP exchange.
    fn transfer_iso_tp_multi_frame(&mut self, payload: &[u8], timeout: Option<Duration>) -> Result<ResponseFrame, DiagError> {
        let mut response_strings: String;

        if payload.len() > 4095 {
            return Err(DiagError::InvalidCommand("Payload exceeds 4095 bytes".into()));
        }

        // 1. Send ONLY the First Frame (FF).
        let total_len = payload.len();
        let mut ff = vec![
            (0x10 | ((total_len >> 8) & 0x0F)) as u8,
            (total_len & 0xFF) as u8,
        ];
        ff.extend_from_slice(&payload[..6]);
        
        let mut hex = bytes_to_hex(&ff);
        hex.push('1'); // expect 1 flow control frame
        response_strings = self.port.send_command(&hex, timeout)?;

        let response_frames = self.parse_frame_response(&response_strings);

        // A malformed reply here is the ECU's doing, not a bug on our side, so
        // it has to surface as an error the caller can clean up after — this
        // runs with programming voltage applied during a flash.
        if response_frames.len() != 1 {
            return Err(DiagError::InvalidResponse(format!(
                "Expected exactly one Flow Control frame after the First Frame, got {}",
                response_frames.len()
            )));
        }

        let mut offset = 6;
        let mut seq: u8 = 1;

        // 2. This outer loop handles multiple data blocks.
        while offset < payload.len() {
            // parse the Flow Control (FC) frame.
            let fc_frame = &response_frames[0];

            if fc_frame.data.len() < 3 || fc_frame.data[0] >> 4 != 3 {
                return Err(DiagError::InvalidResponse(
                    format!("Expected Flow Control frame, but got: {}", bytes_to_hex(&fc_frame.data))
                ));
            }
            
            let flow_status = fc_frame.data[0] & 0x0F;
            if flow_status != 0 { // 0 = Continue To Send (CTS)
                return Err(DiagError::InvalidResponse(format!("ECU sent non-CTS Flow Control status: {}", flow_status)));
            }
            
            // Extract and respect Block Size (BS) and Separation Time (STmin).
            let block_size = fc_frame.data[1] as usize;
            let st_min = self.decode_st_min(fc_frame.data[2])?;

            //debug_print!("Flow Control: BS={}, STmin={:?}", block_size, st_min);
            
            // This inner loop sends the Consecutive Frames (CFs) of the current block.
            let mut frames_in_block = 0;
            while offset < payload.len() {
                if block_size > 0 && frames_in_block >= block_size {
                    break; 
                }

                let mut last_block: bool = false;
                let take = (payload.len() - offset).min(7);

                if offset + take >= payload.len() {
                    last_block = true;
                }

                let mut cf = vec![(0x20 | seq)];
                cf.extend_from_slice(&payload[offset..offset + take]);

                let mut hex = bytes_to_hex(&cf);
                if !last_block {
                    hex.push('0');
                }
                response_strings = self.port.send_command(&hex, timeout)?;

                offset += take;
                seq = (seq + 1) % 16;
                frames_in_block += 1;

                if !st_min.is_zero() {
                    sleep(st_min);
                }
            }
        }

        // 3. After all blocks are sent, listen for the final data response.
        self.parse_iso_tp_response(&response_strings)
    }

    fn parse_frame_response(&self, response_strings: &str) -> Vec<ResponseFrame> {
        // The adapter runs the header and the data together, so what separates
        // them is the header width of the protocol in force: eight hex digits
        // on 29-bit CAN, three on 11-bit, six on the three-byte headers. Counted
        // in digits rather than bytes because a three-digit header is not a
        // whole number of bytes.
        let header_digits = self.addressing.header_hex_digits();

        response_strings.split('\r')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && !s.contains('>'))
            .filter_map(|line| {
                if !line.chars().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                let header = line.get(..header_digits)?;
                let data = line.get(header_digits..)?;
                if data.is_empty() {
                    return None; // header with no PCI byte is not a frame
                }
                Some(ResponseFrame {
                    id: u32::from_str_radix(header, 16).ok()?,
                    data: crate::utils::hex_to_bytes(data).ok()?,
                })
            })
            .collect()
    }

    /// Receives and assembles a complete ISO-TP message.
    fn parse_iso_tp_response(&mut self, response_buffer: &str) -> Result<ResponseFrame, DiagError> {
        let frames = self.parse_frame_response(response_buffer);

        let mut can_id_for_mf: u32 = 0; // To store the ID for multi-frame messages
        let mut multi_frame_payload: Option<(usize, Vec<u8>)> = None;
        let mut next_seq: u8 = 0;

        for frame in frames {
            // Skip response pending frames
            if frame.data.len() >= 4 && frame.data[0] == 0x03 && frame.data[1] == 0x7f && frame.data[3] == 0x78 {
                continue;
            }

            let pci = frame.data[0];
            let frame_type = pci >> 4;

            match frame_type {
                0 => { // Single Frame
                    let len = (pci & 0x0F) as usize;
                    if frame.data.len() < 1 + len { 
                        return Err(DiagError::InvalidResponse("SF has invalid length".into())); 
                    }
                    let payload = &frame.data[1..1 + len];

                    // A Single Frame is always a complete message. Return immediately.
                    return Ok(ResponseFrame { id: frame.id, data: payload.to_vec() });
                }
                1 => { // First Frame
                    if frame.data.len() < 2 { 
                        return Err(DiagError::InvalidResponse("FF is too short".into())); 
                    }
                    let total_len = (((pci & 0x0F) as usize) << 8) | (frame.data[1] as usize);
                    let mut buffer = Vec::with_capacity(total_len);
                    buffer.extend_from_slice(&frame.data[2..]);
                    multi_frame_payload = Some((total_len, buffer));
                    next_seq = 1;
                    can_id_for_mf = frame.id;
                }
                2 => { // Consecutive Frame
                    if let Some((total_len, buffer)) = &mut multi_frame_payload {
                        let seq = pci & 0x0F;
                        if seq != next_seq {
                            return Err(DiagError::InvalidResponse(format!("Frame out of sequence. Expected {}, got {}", next_seq, seq)));
                        }
                        next_seq = (next_seq + 1) % 16;
                        buffer.extend_from_slice(&frame.data[1..]);

                        // If we've received all the data, the message is complete. Return.
                        if buffer.len() >= *total_len {
                            buffer.truncate(*total_len);
                            return Ok(ResponseFrame { id: can_id_for_mf, data: buffer.clone() });
                        }
                    }
                }
                _ => continue, // Ignore other frame types like Flow Control
            }
        }

        // This error is returned if the buffer is fully parsed but no complete message was found.
        Err(DiagError::InvalidResponse("No valid or complete ISO-TP message found in response".into()))
    }
}

/// A lightweight trait to abstract ISO-TP style request/response for UDS client.
pub trait IsoTpTransport {
    fn request(&mut self, payload: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, DiagError>;
}

impl IsoTpTransport for ObdProtocol {
    fn request(&mut self, payload: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, DiagError> { 
        self.transfer_with_timeout(payload, timeout).map(|resp| resp.data)
    }
}

// Implement IsoTpTransport for mutable references to the trait object
impl<'a> IsoTpTransport for &'a mut dyn IsoTpTransport {
    fn request(&mut self, payload: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, DiagError> {
        (**self).request(payload, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{protocol_with, MockPort};

    /// The tester-to-ECU CAN id the fake ECU answers on.
    const ECU_ID: u32 = 0x18DAF101;

    fn empty_protocol() -> ObdProtocol {
        protocol_with(&[]).0
    }

    // ---------------------------------------------------------------- receive

    #[test]
    fn parses_a_single_frame_and_drops_adapter_chatter() {
        // Only the middle line is a CAN frame; the rest is ELM327 noise that
        // must never reach the ISO-TP layer.
        let buffer = "SEARCHING...\rOK\r18DAF10106410080000001\rNO DATA\r";
        let mut proto = empty_protocol();

        let frame = proto.parse_iso_tp_response(buffer).unwrap();

        assert_eq!(frame.id, ECU_ID);
        assert_eq!(frame.data, vec![0x41, 0x00, 0x80, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn assembles_a_first_frame_plus_consecutive_frames() {
        // 10-byte message: FF carries 6 bytes, one CF carries the last 4.
        let buffer = "18DAF101100A62F190010203\r18DAF1012104050607\r";
        let mut proto = empty_protocol();

        let frame = proto.parse_iso_tp_response(buffer).unwrap();

        assert_eq!(frame.id, ECU_ID);
        assert_eq!(
            frame.data,
            vec![0x62, 0xF1, 0x90, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]
        );
    }

    /// A CF is allowed to be padded past the declared length; the extra bytes
    /// must be truncated rather than handed to the caller.
    #[test]
    fn truncates_padding_past_the_declared_length() {
        // Declared length 8: FF carries 6, the CF carries 2 real bytes + padding.
        let buffer = "18DAF1011008620102030405\r18DAF10121060755555555\r";
        let mut proto = empty_protocol();

        let frame = proto.parse_iso_tp_response(buffer).unwrap();

        assert_eq!(frame.data.len(), 8);
        assert_eq!(frame.data, vec![0x62, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
    }

    #[test]
    fn rejects_out_of_sequence_consecutive_frames() {
        // Sequence jumps from 1 to 2 (0x22 instead of 0x21).
        let buffer = "18DAF101100A62F190010203\r18DAF1012204050607\r";
        let mut proto = empty_protocol();

        let err = proto.parse_iso_tp_response(buffer).unwrap_err();

        assert!(
            matches!(&err, DiagError::InvalidResponse(m) if m.contains("out of sequence")),
            "unexpected error: {err}"
        );
    }

    /// `7F .. 78` means "response pending" — it must be skipped so the real
    /// answer later in the same buffer is the one that gets returned.
    #[test]
    fn skips_response_pending_frames() {
        let buffer = "18DAF101037F2278\r18DAF101037F2278\r18DAF1010462F19001\r";
        let mut proto = empty_protocol();

        let frame = proto.parse_iso_tp_response(buffer).unwrap();

        assert_eq!(frame.data, vec![0x62, 0xF1, 0x90, 0x01]);
    }

    /// A genuine negative response must NOT be swallowed like a pending one —
    /// the UDS layer is what turns it into an NRC error.
    #[test]
    fn passes_negative_responses_through_to_the_caller() {
        let mut proto = empty_protocol();

        let frame = proto.parse_iso_tp_response("18DAF101037F2231\r").unwrap();

        assert_eq!(frame.data, vec![0x7F, 0x22, 0x31]);
    }

    #[test]
    fn reports_an_empty_or_unusable_buffer() {
        let mut proto = empty_protocol();

        assert!(proto.parse_iso_tp_response("").is_err());
        assert!(proto.parse_iso_tp_response("NO DATA\r").is_err());
        // Single frame claiming 7 bytes but carrying 3.
        assert!(proto.parse_iso_tp_response("18DAF10107620102\r").is_err());
    }

    // ------------------------------------------------------------------- send

    #[test]
    fn single_frame_request_is_prefixed_with_its_length() {
        let (mut proto, port) = protocol_with(&["18DAF1010462F19001\r"]);

        let frame = proto.transfer(&[0x22, 0xF1, 0x90]).unwrap();

        // PCI 0x03 (3 payload bytes) then the payload itself.
        assert_eq!(port.commands(), vec!["0322F190"]);
        assert_eq!(frame.data, vec![0x62, 0xF1, 0x90, 0x01]);
    }

    #[test]
    fn rejects_an_empty_request() {
        let mut proto = empty_protocol();
        assert!(matches!(
            proto.transfer(&[]),
            Err(DiagError::InvalidCommand(_))
        ));
    }

    /// The multi-frame sender is the most intricate path: a First Frame with the
    /// 12-bit length, then Consecutive Frames numbered 1..15. The trailing digit
    /// on the hex string is ELM327 syntax for "how many replies to expect" — `1`
    /// after the FF (the Flow Control frame), nothing after the final CF.
    #[test]
    fn multi_frame_request_sends_first_frame_then_consecutive_frames() {
        let payload: Vec<u8> = (0..17u8).collect(); // 17 bytes -> FF + 2 CFs
        let (mut proto, port) = protocol_with(&[
            "18DAF1013000000000000000\r", // Flow Control: CTS, BS=0, STmin=0
            "",                           // no reply expected after CF 1
            "18DAF1010462F19001\r",       // final response after the last CF
        ]);

        let frame = proto.transfer(&payload).unwrap();

        assert_eq!(
            port.commands(),
            vec![
                // FF: 0x10 | high nibble of len, low byte of len (0x11 = 17),
                // then the first 6 payload bytes, then the "expect 1 FC" digit.
                "10110001020304051",
                // CF 1: 0x21 + next 7 bytes, "expect no reply" digit.
                "21060708090A0B0C0",
                // CF 2 is the last frame, so no trailing digit: the ECU answers.
                "220D0E0F10",
            ]
        );
        assert_eq!(frame.data, vec![0x62, 0xF1, 0x90, 0x01]);
        assert_eq!(port.unused_responses(), 0);
    }

    /// This runs with programming voltage applied during a flash, so a garbled
    /// Flow Control reply has to be an error the caller can unwind from.
    #[test]
    fn multi_frame_reports_a_garbled_flow_control_reply() {
        // Two frames where exactly one Flow Control frame was expected.
        let (mut proto, _port) =
            protocol_with(&["18DAF1013000000000000000\r18DAF1013000000000000000\r"]);

        let err = proto.transfer(&vec![0xAAu8; 10]).unwrap_err();

        assert!(
            matches!(&err, DiagError::InvalidResponse(m) if m.contains("exactly one Flow Control")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_a_payload_over_the_iso_tp_limit() {
        let mut proto = empty_protocol();
        assert!(matches!(
            proto.transfer(&vec![0u8; 4096]),
            Err(DiagError::InvalidCommand(_))
        ));
    }

    #[test]
    fn multi_frame_rejects_a_non_cts_flow_control() {
        // 0x32 = Overflow/abort rather than 0x30 Continue To Send.
        let (mut proto, _port) = protocol_with(&["18DAF1013200000000000000\r"]);

        let err = proto.transfer(&vec![0xAAu8; 10]).unwrap_err();

        assert!(
            matches!(&err, DiagError::InvalidResponse(m) if m.contains("non-CTS")),
            "unexpected error: {err}"
        );
    }

    // -------------------------------------------------------------- STmin/DTC

    #[test]
    fn decodes_st_min_across_both_encodings() {
        let proto = empty_protocol();

        assert_eq!(proto.decode_st_min(0x00).unwrap(), Duration::ZERO);
        assert_eq!(proto.decode_st_min(0x7F).unwrap(), Duration::from_millis(127));
        assert_eq!(proto.decode_st_min(0xF1).unwrap(), Duration::from_micros(100));
        assert_eq!(proto.decode_st_min(0xF9).unwrap(), Duration::from_micros(900));

        // Reserved ranges must be rejected, not silently treated as 0ms.
        assert!(proto.decode_st_min(0x80).is_err());
        assert!(proto.decode_st_min(0xF0).is_err());
        assert!(proto.decode_st_min(0xFA).is_err());
    }

    #[test]
    fn decodes_the_dtc_prefix_from_the_top_two_bits() {
        // Top two bits pick the system letter, the remaining 14 bits the number.
        assert_eq!(ObdProtocol::decode_dtc_bytes(&[0x01, 0x43]).unwrap(), "P0143");
        assert_eq!(ObdProtocol::decode_dtc_bytes(&[0x41, 0x43]).unwrap(), "C0143");
        assert_eq!(ObdProtocol::decode_dtc_bytes(&[0x81, 0x43]).unwrap(), "B0143");
        assert_eq!(ObdProtocol::decode_dtc_bytes(&[0xC1, 0x43]).unwrap(), "U0143");
        assert!(ObdProtocol::decode_dtc_bytes(&[0x01]).is_err());
    }

    #[test]
    fn read_dtc_requests_mode_03_and_decodes_each_code() {
        let (mut proto, port) = protocol_with(&["18DAF10106430201430200\r"]);

        let dtcs = proto.read_dtc(false).unwrap();

        assert_eq!(port.commands(), vec!["0103"], "mode 03 = stored DTCs");
        assert_eq!(
            dtcs.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(),
            vec!["P0143", "P0200"]
        );
        assert_eq!(dtcs[1].description, "Injector Circuit/Open");
    }

    #[test]
    fn read_dtc_pending_uses_mode_07_and_handles_an_empty_list() {
        let (mut proto, port) = protocol_with(&["18DAF101024700\r"]);

        let dtcs = proto.read_dtc(true).unwrap();

        assert_eq!(port.commands(), vec!["0107"], "mode 07 = pending DTCs");
        assert!(dtcs.is_empty());
    }

    /// CAN frames are padded out to 8 bytes, often with 0x55 or 0xAA rather
    /// than zeros. Those bytes sit past the announced DTC count and must never
    /// be decoded as extra trouble codes.
    #[test]
    fn read_dtc_ignores_can_frame_padding_after_the_announced_codes() {
        // 43 01 0143, then 0x55 0x55 padding out to the frame boundary.
        let (mut proto, _port) = protocol_with(&["18DAF10106430101435555\r"]);

        let dtcs = proto.read_dtc(false).unwrap();

        assert_eq!(
            dtcs.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(),
            vec!["P0143"],
            "0x5555 padding must not become a phantom DTC"
        );
    }

    /// An ECU announcing more codes than it sends is a bad response, not a bug
    /// on our side — it must be reported rather than panicking the process.
    #[test]
    fn read_dtc_reports_a_count_that_does_not_match_its_contents() {
        // Claims 2 DTCs, sends one real code and one all-zero entry.
        let (mut proto, _port) = protocol_with(&["18DAF10106430201430000\r"]);

        let err = proto.read_dtc(false).unwrap_err();

        assert!(
            matches!(&err, DiagError::InvalidResponse(m) if m.contains("only 1 could be decoded")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_dtc_rejects_a_truncated_dtc_list() {
        // Claims 2 DTCs but only carries one 2-byte code.
        let (mut proto, _port) = protocol_with(&["18DAF1010443020143\r"]);

        assert!(proto.read_dtc(false).is_err());
    }

    // ------------------------------------------------------- supported PID map

    /// The supported-PID bitmap is MSB-first starting at `base + 1`, and the
    /// very last bit means "ask again at base + 0x20" rather than "PID 0x20".
    #[test]
    fn walks_the_supported_pid_bitmap_across_groups() {
        let (mut proto, port) = protocol_with(&[
            // 41 00 with bit 0 (PID 0x01) and the continuation bit set.
            "18DAF10106410080000001\r",
            // 41 20 with bit 1 (PID 0x22) only, no continuation.
            "18DAF10106412040000000\r",
        ]);

        let pids = proto.read_supported_pids(0x01).unwrap();

        assert_eq!(port.commands(), vec!["020100", "020120"]);
        assert_eq!(pids, vec![0x01, 0x22]);
    }

    #[test]
    fn supported_pid_query_fails_loudly_on_a_mismatched_echo() {
        // Responds about PID 0x20 when 0x00 was asked for.
        let (mut proto, _port) = protocol_with(&["18DAF10106412080000000\r"]);

        assert!(proto.read_supported_pids(0x01).is_err());
    }

    /// Adapter init is generic OBD setup only. Programming voltage is a
    /// separate, explicitly-requested step — no vehicle-specific pin belongs
    /// in this sequence.
    #[test]
    fn reset_does_not_touch_programming_voltage() {
        let (mut proto, port) = protocol_with(&["ELM327 v1.5", "OK", "OK", "OK", "OK", "OK"]);

        proto.reset().unwrap();

        assert_eq!(
            port.commands(),
            vec!["ATZ", "ATE0", "ATH1", "ATS0", "ATSP7", "ATCAF0"]
        );
    }

    /// Recovery for a run killed mid-flash: SIGINT skips the Drop guard, so a
    /// pin can still be live at the next connect. Every pin the adapter can
    /// drive is cleared, which keeps this free of per-vehicle knowledge.
    #[test]
    fn clearing_programming_voltage_covers_every_pin_the_adapter_drives() {
        let responses = vec!["OK"; PROGRAMMING_VOLTAGE_PINS.len()];
        let (mut proto, port) = protocol_with(&responses);

        proto.clear_all_programming_voltage().unwrap();

        // Pins 6, 9, 11-14 (high side) and 15 (low side), as single hex digits.
        assert_eq!(
            port.commands(),
            vec![
                "ATPROGV6FFFFFFFF",
                "ATPROGV9FFFFFFFF",
                "ATPROGVBFFFFFFFF",
                "ATPROGVCFFFFFFFF", // pin 12 — Honda's WEN line, covered
                "ATPROGVDFFFFFFFF", // without this layer knowing why
                "ATPROGVEFFFFFFFF",
                "ATPROGVFFFFFFFFF",
            ]
        );
    }

    /// The flash path compares the header it is about to use against the one
    /// already in effect, so the protocol has to remember what it set.
    #[test]
    fn set_header_id_issues_atsh_and_remembers_it() {
        let (mut proto, port) = protocol_with(&["OK"]);

        assert_eq!(proto.header_id(), None, "nothing set before connecting");
        proto.set_header_id(0x18DA01F1).unwrap();

        assert_eq!(port.commands(), vec!["ATSH 18DA01F1"]);
        assert_eq!(proto.header_id(), Some(0x18DA01F1));
    }

    /// A broadcast has to go to the functional address on purpose. Inheriting
    /// the header of a previous physical command would quietly turn a bus scan
    /// into a scan of one ECU that was already known.
    #[test]
    fn broadcast_addresses_the_functional_id_and_restores_the_previous_header() {
        let (mut proto, port) = protocol_with(&[
            "OK",                                 // ATSH to the functional address
            "18DAF1000641009BBE3F80\r18DAF1010641008F3F80AB\r", // two ECUs answer
            "OK",                                 // ATSH restoring the caller's header
        ]);
        proto.header_id = Some(0x18DA01F1);

        let responses = proto.broadcast(&[0x01, 0x00], None).unwrap();

        assert_eq!(
            port.commands(),
            vec!["ATSH 18DB33F1", "020100", "ATSH 18DA01F1"]
        );
        assert_eq!(proto.header_id(), Some(0x18DA01F1));
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].id, 0x18DAF100);
        assert_eq!(responses[1].id, 0x18DAF101);
        assert_eq!(responses[1].data, vec![0x41, 0x00, 0x8F, 0x3F, 0x80, 0xAB]);
    }

    /// Nothing to put back when the session had no header of its own, so the
    /// scan should not spend a round trip on it.
    #[test]
    fn broadcast_leaves_the_functional_header_in_place_when_there_was_none_before() {
        let (mut proto, port) = protocol_with(&["OK", "18DAF1000641009BBE3F80\r"]);

        proto.broadcast(&[0x01, 0x00], None).unwrap();

        assert_eq!(port.commands(), vec!["ATSH 18DB33F1", "020100"]);
        assert_eq!(proto.header_id(), Some(0x18DB33F1));
    }

    #[test]
    fn mock_port_is_wired_up_for_boxed_use() {
        // Guards the test harness itself: a cloned MockPort must observe the
        // commands sent through the boxed copy inside ObdProtocol.
        let port = MockPort::new(&["18DAF1010144\r"]);
        let mut proto = ObdProtocol::new(Box::new(port.clone()));

        proto.clear_dtc().unwrap();

        assert_eq!(port.commands(), vec!["0104"]);
    }
}
