use crate::errors::DiagError;
use crate::obd::protocol::IsoTpTransport;
use std::time::Duration;
use crate::utils::hexdump;
use crate::ObdProtocol;

// Using Rust enums to represent the various identifier types
// #[repr(u16)] allows us to specify the underlying type
#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(u16)]
pub enum DataIdentifier {
    BootSoftwareIdentification = 0xF180,
    ApplicationSoftwareIdentification = 0xF181,
    ApplicationDataIdentification = 0xF182,
    BootSoftwareFingerprint = 0xF183,
    ApplicationSoftwareFingerprint = 0xF184,
    ApplicationDataFingerprint = 0xF185,
    ActiveDiagnosticSession = 0xF186,
    VehicleManufacturerSparePartNumber = 0xF187,
    VehicleManufacturerEcuSoftwareNumber = 0xF188,
    VehicleManufacturerEcuSoftwareVersionNumber = 0xF189,
    SystemSupplierIdentifier = 0xF18A,
    EcuManufacturingDate = 0xF18B,
    EcuSerialNumber = 0xF18C,
    SupportedFunctionalUnits = 0xF18D,
    VehicleManufacturerKitAssemblyPartNumber = 0xF18E,
    Vin = 0xF190,
    VehicleManufacturerEcuHardwareNumber = 0xF191,
    SystemSupplierEcuHardwareNumber = 0xF192,
    SystemSupplierEcuHardwareVersionNumber = 0xF193,
    SystemSupplierEcuSoftwareNumber = 0xF194,
    SystemSupplierEcuSoftwareVersionNumber = 0xF195,
    ExhaustRegulationOrTypeApprovalNumber = 0xF196,
    SystemNameOrEngineType = 0xF197,
    RepairShopCodeOrTesterSerialNumber = 0xF198,
    ProgrammingDate = 0xF199,
    CalibrationRepairShopCodeOrCalibrationEquipmentSerialNumber = 0xF19A,
    CalibrationDate = 0xF19B,
    CalibrationEquipmentSoftwareNumber = 0xF19C,
    EcuInstallationDate = 0xF19D,
    OdxFile = 0xF19E,
    Entity = 0xF19F,
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(u8)]
pub enum SessionType {
    Default = 1,
    Programming = 2,
    ExtendedDiagnostic = 3,
    SafetySystemDiagnostic = 4,
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(u8)]
pub enum AccessType {
    RequestSeed = 1,
    SendKey = 2,
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(u16)]
pub enum RoutineIdentifier {
    EraseMemory = 0xFF00,
    CheckProgrammingDependencies = 0xFF01,
    EraseMirrorMemoryDtcs = 0xFF02,
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(u8)]
pub enum RoutineControlType {
    Start = 1,
    Stop = 2,
    RequestResults = 3,
}

// Function to get negative response code descriptions, similar to the Python dictionary
fn get_negative_response_description(code: u8) -> &'static str {
    match code {
        0x00 => "positive response",
        0x10 => "general reject",
        0x11 => "service not supported",
        0x12 => "sub-function not supported",
        0x13 => "incorrect message length or invalid format",
        0x14 => "response too long",
        0x21 => "busy repeat request",
        0x22 => "conditions not correct",
        0x24 => "request sequence error",
        0x25 => "no response from subnet component",
        0x26 => "failure prevents execution of requested action",
        0x31 => "request out of range",
        0x33 => "security access denied",
        0x35 => "invalid key",
        0x36 => "exceed number of attempts",
        0x37 => "required time delay not expired",
        0x70 => "upload download not accepted",
        0x71 => "transfer data suspended",
        0x72 => "general programming failure",
        0x73 => "wrong block sequence counter",
        0x78 => "request correctly received - response pending",
        0x7e => "sub-function not supported in active session",
        0x7f => "service not supported in active session",
        // ... (add other codes as needed)
        _ => "unknown negative response code",
    }
}

// UDS service IDs
const SID_DIAGNOSTIC_SESSION_CONTROL: u8 = 0x10;
const SID_ECU_RESET: u8 = 0x11;
const SID_SECURITY_ACCESS: u8 = 0x27;
const SID_TESTER_PRESENT: u8 = 0x3E;
const SID_REQUEST_DOWNLOAD: u8 = 0x34;
const SID_TRANSFER_DATA: u8 = 0x36;
const SID_REQUEST_TRANSFER_EXIT: u8 = 0x37;
const SID_ROUTINE_CONTROL: u8 = 0x31;
const SID_READ_DATA_BY_IDENTIFIER: u8 = 0x22;
const SID_WRITE_DATA_BY_IDENTIFIER: u8 = 0x2E;

// Free functions for UDS operations
pub fn diagnostic_session(transport: &mut ObdProtocol, session: SessionType) -> Result<(), DiagError> {
    let req = [SID_DIAGNOSTIC_SESSION_CONTROL, session as u8];
    call_raw(transport, &req)?;
    Ok(())
}

pub fn tester_present(transport: &mut ObdProtocol) -> Result<(), DiagError> {
    let req = [SID_TESTER_PRESENT, 0x00];
    call_raw(transport, &req)?;
    Ok(())
}

pub fn security_request_seed(transport: &mut ObdProtocol, level: AccessType) -> Result<Vec<u8>, DiagError> {
    let req = [SID_SECURITY_ACCESS, level as u8];
    let resp = call_raw(transport, &req)?;
    if resp.len() < 3 { return Err(DiagError::InvalidResponse("Seed response too short".into())); }
    Ok(resp[2..].to_vec())
}

pub fn security_send_key(transport: &mut ObdProtocol, level: AccessType, key: &[u8]) -> Result<(), DiagError> {
    let mut req = Vec::with_capacity(2 + key.len());
    req.push(SID_SECURITY_ACCESS);
    req.push(level as u8);
    req.extend_from_slice(key);
    call_raw(transport, &req)?;
    Ok(())
}

pub fn ecu_reset(transport: &mut ObdProtocol, reset_type: u8) -> Result<(), DiagError> {
    let req = [SID_ECU_RESET, reset_type];
    call_raw(transport, &req)?;
    Ok(())
}

pub fn read_data_by_identifier(transport: &mut ObdProtocol, did: u16) -> Result<Vec<u8>, DiagError> {
    let req = [SID_READ_DATA_BY_IDENTIFIER, (did >> 8) as u8, (did & 0xFF) as u8];
    let resp = call_raw(transport, &req)?;
    if resp.len() < 4 { return Err(DiagError::InvalidResponse("RDBI response too short".into())); }
    if resp[0] != 0x62 || resp[1] != (did >> 8) as u8 || resp[2] != (did & 0xFF) as u8 {
        return Err(DiagError::InvalidResponse("RDBI unexpected echo".into()));
    }
    Ok(resp[3..].to_vec())
}

pub fn write_data_by_identifier(transport: &mut ObdProtocol, did: u16, data: &[u8]) -> Result<(), DiagError> {
    let mut req = Vec::with_capacity(3 + data.len());
    req.push(SID_WRITE_DATA_BY_IDENTIFIER);
    req.push((did >> 8) as u8);
    req.push((did & 0xFF) as u8);
    req.extend_from_slice(data);
    call_raw(transport, &req)?;
    Ok(())
}

pub fn request_download(transport: &mut ObdProtocol, addr: u32, len: u32) -> Result<Vec<u8>, DiagError> {
    let mut req = Vec::with_capacity(3 + 4 + 4);
    req.push(SID_REQUEST_DOWNLOAD);
    req.push(0x00);
    req.push(0x44);
    req.extend_from_slice(&addr.to_be_bytes());
    req.extend_from_slice(&len.to_be_bytes());
    let resp = call_raw(transport, &req)?;
    if resp.is_empty() { return Err(DiagError::InvalidResponse("Empty RequestDownload response".into())); }
    Ok(resp[1..].to_vec())
}

pub fn transfer_data(transport: &mut ObdProtocol, block_counter: u8, chunk: &[u8]) -> Result<(), DiagError> {
    let mut req = Vec::with_capacity(2 + chunk.len());
    req.push(SID_TRANSFER_DATA);
    req.push(block_counter);
    req.extend_from_slice(chunk);
    call_raw(transport, &req)?;
    Ok(())
}

pub fn request_transfer_exit(transport: &mut ObdProtocol) -> Result<(), DiagError> {
    let req = [SID_REQUEST_TRANSFER_EXIT];
    call_raw(transport, &req)?;
    Ok(())
}

pub fn routine_control_with_timeout(
    transport: &mut ObdProtocol,
    control_type: RoutineControlType,
    routine_id: RoutineIdentifier,
    params: Option<&[u8]>,
    timeout: Option<Duration>,
) -> Result<Vec<u8>, DiagError> {
    let mut req = Vec::with_capacity(3 + params.map_or(0, |p| p.len()));
    req.push(SID_ROUTINE_CONTROL);
    req.push(control_type as u8);
    req.push((routine_id as u16 >> 8) as u8);
    req.push((routine_id as u16 & 0xFF) as u8);
    if let Some(p) = params { req.extend_from_slice(p); }
    let resp = call_raw_with_timeout(transport, &req, timeout)?;
    if resp.len() < 4 { return Err(DiagError::InvalidResponse("RoutineControl response too short".into())); }
    if resp[0] != 0x71 || resp[1] != (control_type as u8) || resp[2] != (routine_id as u16 >> 8) as u8 || resp[3] != (routine_id as u16 & 0xFF) as u8 {
        return Err(DiagError::InvalidResponse("RoutineControl unexpected echo".into()));
    }
    Ok(resp[4..].to_vec())
}

pub fn routine_control(
    transport: &mut ObdProtocol,
    control_type: RoutineControlType,
    routine_id: RoutineIdentifier,
    params: Option<&[u8]>,
) -> Result<Vec<u8>, DiagError> {
    routine_control_with_timeout(transport, control_type, routine_id, params, None)
}

// Helper functions
fn call_raw_with_timeout(transport: &mut ObdProtocol, req: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, DiagError> {
    let mut tries = 0;
    let pending_retry_limit = 30;
    loop {
        let resp = transport.request(req, timeout)?;
        if resp.is_empty() { return Err(DiagError::InvalidResponse("Empty UDS response".into())); }
        if resp[0] == 0x7F {
            if resp.len() < 3 { return Err(DiagError::InvalidResponse("Malformed negative response".into())); }
            let nrc = resp[2];
            if nrc == 0x78 {
                tries += 1;
                if tries > pending_retry_limit { return Err(DiagError::InvalidResponse("Too many pending responses".into())); }
                hexdump(&resp);
                continue;
            } else {
                return Err(DiagError::InvalidResponse(format!("UDS negative response NRC=0x{:02X} - {}", nrc, get_negative_response_description(nrc))));
            }
        }
        if resp[0] != (req[0] + 0x40) { return Err(DiagError::InvalidResponse("Unexpected positive response SID".into())); }
        return Ok(resp);
    }
}

fn call_raw(transport: &mut ObdProtocol, req: &[u8]) -> Result<Vec<u8>, DiagError> {
    call_raw_with_timeout(transport, req, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::protocol_with;

    #[test]
    fn read_data_by_identifier_frames_the_did_and_strips_the_echo() {
        let (mut proto, port) = protocol_with(&["18DAF1010562F1904142\r"]);

        let value = read_data_by_identifier(&mut proto, DataIdentifier::Vin as u16).unwrap();

        assert_eq!(port.commands(), vec!["0322F190"], "SF PCI, then 22 + DID hi/lo");
        assert_eq!(value, vec![0x41, 0x42], "the 62/DID echo must be stripped");
    }

    /// A response echoing a different DID belongs to some other request;
    /// accepting it would attribute one identifier's data to another.
    #[test]
    fn read_data_by_identifier_rejects_a_mismatched_echo() {
        let (mut proto, _port) = protocol_with(&["18DAF1010562F1914142\r"]);

        assert!(read_data_by_identifier(&mut proto, DataIdentifier::Vin as u16).is_err());
    }

    #[test]
    fn negative_response_becomes_an_error_naming_the_decoded_nrc() {
        let (mut proto, _port) = protocol_with(&["18DAF101037F2233\r"]);

        let err = read_data_by_identifier(&mut proto, DataIdentifier::Vin as u16).unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("0x33"), "should name the NRC: {msg}");
        assert!(msg.contains("security access denied"), "should decode it: {msg}");
    }

    /// Every positive response must carry `request SID + 0x40`; anything else is
    /// a stray frame left over from another exchange.
    #[test]
    fn positive_response_with_the_wrong_sid_is_rejected() {
        // 0x51 (ECUReset response) turning up in answer to a 0x22 request.
        let (mut proto, _port) = protocol_with(&["18DAF101025101\r"]);

        let err = read_data_by_identifier(&mut proto, DataIdentifier::Vin as u16).unwrap_err();

        assert!(err.to_string().contains("Unexpected positive response SID"));
    }

    #[test]
    fn diagnostic_session_sends_the_requested_sub_function() {
        let (mut proto, port) = protocol_with(&["18DAF10106500200000000\r"]);

        diagnostic_session(&mut proto, SessionType::Programming).unwrap();

        assert_eq!(port.commands(), vec!["021002"]);
    }

    #[test]
    fn security_request_seed_strips_the_sid_and_level_echo() {
        let (mut proto, port) = protocol_with(&["18DAF1010467011234\r"]);

        let seed = security_request_seed(&mut proto, AccessType::RequestSeed).unwrap();

        assert_eq!(port.commands(), vec!["022701"]);
        assert_eq!(seed, vec![0x12, 0x34]);
    }

    #[test]
    fn security_send_key_appends_the_key_after_the_access_level() {
        let (mut proto, port) = protocol_with(&["18DAF101026702\r"]);

        security_send_key(&mut proto, AccessType::SendKey, &[0x12, 0x67]).unwrap();

        assert_eq!(port.commands(), vec!["0427021267"]);
    }

    /// RequestDownload is 11 bytes, so it is also the shortest real service that
    /// exercises the multi-frame sender: First Frame with the 12-bit length and
    /// the "expect one Flow Control" digit, then a single Consecutive Frame.
    #[test]
    fn request_download_packs_address_and_length_big_endian_over_two_frames() {
        let (mut proto, port) = protocol_with(&[
            "18DAF1013000000000000000\r", // Flow Control: CTS, BS=0, STmin=0
            "18DAF1010474200202\r",       // 74 20 0202 -> max block size 0x0202
        ]);

        let params = request_download(&mut proto, 0x0001_0000, 0x0002_0000).unwrap();

        assert_eq!(
            port.commands(),
            vec![
                // FF: len 0x00B = 11, then 34 00 44 and the first 3 address bytes.
                "100B3400440001001",
                // CF 1: last address byte + the 4 length bytes.
                "210000020000",
            ]
        );
        assert_eq!(params, vec![0x20, 0x02, 0x02], "SID stripped, LFID kept");
    }

    #[test]
    fn transfer_data_prefixes_the_block_counter() {
        let (mut proto, port) = protocol_with(&["18DAF101027607\r"]);

        transfer_data(&mut proto, 0x07, &[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();

        assert_eq!(port.commands(), vec!["063607DEADBEEF"]);
    }

    /// Erase and CheckProgrammingDependencies are distinguished only by the
    /// echoed routine id, so a mismatch has to be an error: mistaking one for
    /// the other would let a flash continue against an unerased ECU.
    #[test]
    fn routine_control_validates_the_echoed_routine_id() {
        let (mut proto, port) = protocol_with(&["18DAF101057101FF0000\r"]);

        let result = routine_control(
            &mut proto,
            RoutineControlType::Start,
            RoutineIdentifier::EraseMemory,
            None,
        )
        .unwrap();

        assert_eq!(port.commands(), vec!["043101FF00"]);
        assert_eq!(result, vec![0x00], "status byte following the echo");
    }

    #[test]
    fn routine_control_rejects_a_reply_about_a_different_routine() {
        // Asked to erase (FF00), the ECU answers about FF01.
        let (mut proto, _port) = protocol_with(&["18DAF101057101FF0100\r"]);

        assert!(routine_control(
            &mut proto,
            RoutineControlType::Start,
            RoutineIdentifier::EraseMemory,
            None,
        )
        .is_err());
    }

    #[test]
    fn negative_response_codes_are_described() {
        assert_eq!(get_negative_response_description(0x35), "invalid key");
        assert_eq!(
            get_negative_response_description(0x73),
            "wrong block sequence counter"
        );
        assert_eq!(
            get_negative_response_description(0xAB),
            "unknown negative response code"
        );
    }
}
