//! Manufacturer diagnostics use typed calls; unsupported protocols refuse explicitly.
use crate::{
    obd::protocol::DiagnosticTroubleCode,
    obd::{uds, ObdProtocol},
    DiagError,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize)]
pub struct EcuInfo {
    pub identification: BTreeMap<u16, Vec<u8>>,
    pub calibration: Option<String>,
    pub vin: Option<String>,
    pub variant: Option<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum DataId {
    Common(u16),
    Local(u8),
}
#[derive(Debug, Clone, Copy)]
pub enum Session {
    Default,
    Programming,
    Extended,
    Numeric(u8),
}
#[derive(Debug, Clone, Copy)]
pub enum RoutineOp {
    Start,
    Stop,
    Results,
}
#[derive(Debug, Clone, Copy)]
pub enum ResetKind {
    Hard,
    KeyOffOn,
    Soft,
}
pub trait SeedKey {
    fn key(&self, seed: &[u8]) -> Result<Vec<u8>, DiagError>;
}

pub trait DiagServices {
    fn session(&mut self, session: Session) -> Result<(), DiagError>;
    fn security(&mut self, level: u8, key: &dyn SeedKey) -> Result<(), DiagError>;
    fn read_data(&mut self, id: DataId) -> Result<Vec<u8>, DiagError>;
    fn write_data(&mut self, id: DataId, data: &[u8]) -> Result<(), DiagError>;
    fn routine(&mut self, op: RoutineOp, id: u16, data: &[u8]) -> Result<Vec<u8>, DiagError>;
    fn ecu_reset(&mut self, kind: ResetKind) -> Result<(), DiagError>;
    fn identification(&mut self) -> Result<EcuInfo, DiagError>;
    fn read_dtcs(&mut self) -> Result<Vec<DiagnosticTroubleCode>, DiagError>;
    fn clear_dtcs(&mut self) -> Result<(), DiagError>;
    fn request(&mut self, data: &[u8], expect: &[u8]) -> Result<Vec<u8>, DiagError>;
}

pub struct Uds<'a>(pub &'a mut ObdProtocol);
pub fn services<'a>(
    name: &str,
    protocol: &'a mut ObdProtocol,
) -> Result<Box<dyn DiagServices + 'a>, DiagError> {
    match name {
        "uds" => Ok(Box::new(Uds(protocol))),
        "obd" => Ok(Box::new(ObdOnly(protocol))),
        other => Err(DiagError::Unsupported(format!(
            "Application protocol {other} does not provide manufacturer diagnostics in this build"
        ))),
    }
}

pub fn clear_with_confirmation(
    name: &str,
    protocol: &mut ObdProtocol,
    ui: &mut dyn crate::Interaction,
    cancel: &crate::Cancel,
) -> Result<(), DiagError> {
    let mut preflight = crate::preflight::Preflight::new(
        "Clear diagnostic trouble codes",
        "This clears trouble codes and freeze-frame data.",
    );
    if let Some(ecu) = protocol.target() {
        preflight.add("ECU", ecu.to_string(), crate::preflight::CheckStatus::Info);
    }
    preflight.authorize(ui, cancel)?;
    services(name, protocol)?.clear_dtcs()
}
struct ObdOnly<'a>(&'a mut ObdProtocol);
fn obd_unsupported() -> DiagError {
    DiagError::Unsupported(
        "This ECU definition provides OBD only, without manufacturer diagnostic services".into(),
    )
}
impl DiagServices for ObdOnly<'_> {
    fn session(&mut self, _: Session) -> Result<(), DiagError> {
        Err(obd_unsupported())
    }
    fn security(&mut self, _: u8, _: &dyn SeedKey) -> Result<(), DiagError> {
        Err(obd_unsupported())
    }
    fn read_data(&mut self, _: DataId) -> Result<Vec<u8>, DiagError> {
        Err(obd_unsupported())
    }
    fn write_data(&mut self, _: DataId, _: &[u8]) -> Result<(), DiagError> {
        Err(obd_unsupported())
    }
    fn routine(&mut self, _: RoutineOp, _: u16, _: &[u8]) -> Result<Vec<u8>, DiagError> {
        Err(obd_unsupported())
    }
    fn ecu_reset(&mut self, _: ResetKind) -> Result<(), DiagError> {
        Err(obd_unsupported())
    }
    fn request(&mut self, _: &[u8], _: &[u8]) -> Result<Vec<u8>, DiagError> {
        Err(obd_unsupported())
    }
    fn identification(&mut self) -> Result<EcuInfo, DiagError> {
        let ecu = self
            .0
            .target()
            .ok_or_else(|| DiagError::InvalidCommand("Select an ECU for identification".into()))?;
        let mut info = EcuInfo::default();
        for result in self.0.vehicle_info(&[ecu], &crate::Cancel::default())? {
            for value in result.values {
                match value.pid {
                    2 => info.vin = Some(value.value.to_string()),
                    4 => info.calibration = Some(value.value.to_string()),
                    _ => {}
                }
            }
        }
        Ok(info)
    }
    fn read_dtcs(&mut self) -> Result<Vec<DiagnosticTroubleCode>, DiagError> {
        let ecu = self
            .0
            .target()
            .ok_or_else(|| DiagError::InvalidCommand("Select an ECU to read DTCs".into()))?;
        Ok(self
            .0
            .read_obd_dtcs(&[ecu], false, &crate::Cancel::default())?
            .remove(0)
            .dtcs)
    }
    fn clear_dtcs(&mut self) -> Result<(), DiagError> {
        if !self.0.bus().is_can() {
            return Err(DiagError::Unsupported(
                "A legacy OBD clear affects all OBD ECUs; use the confirmed OBD operation".into(),
            ));
        }
        self.0.clear_dtc()
    }
}
fn common(id: DataId) -> Result<u16, DiagError> {
    match id {
        DataId::Common(id) => Ok(id),
        DataId::Local(_) => Err(DiagError::Unsupported(
            "UDS has no local data identifiers".into(),
        )),
    }
}
pub fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).trim_matches('\0').to_string()
}
impl DiagServices for Uds<'_> {
    fn session(&mut self, session: Session) -> Result<(), DiagError> {
        let id = match session {
            Session::Default => 1,
            Session::Programming => 2,
            Session::Extended => 3,
            Session::Numeric(id) => id,
        };
        let response = uds::call_raw(self.0, &[0x10, id])?;
        if response.get(1) != Some(&id) {
            return Err(DiagError::InvalidResponse(
                "Session response has an unexpected echo".into(),
            ));
        }
        Ok(())
    }
    fn security(&mut self, level: u8, algorithm: &dyn SeedKey) -> Result<(), DiagError> {
        if level == 0 || level >= 0x7F || level.is_multiple_of(2) {
            return Err(DiagError::InvalidCommand(
                "Security level must be an odd request-seed subfunction in 1..0x7F".into(),
            ));
        }
        let seed = uds::call_raw(self.0, &[0x27, level])?;
        if seed.get(1) != Some(&level) || seed.len() < 3 {
            return Err(DiagError::InvalidResponse(
                "Malformed security seed response".into(),
            ));
        }
        if seed[2..].iter().all(|b| *b == 0) {
            return Ok(());
        }
        let mut request = vec![0x27, level + 1];
        request.extend(algorithm.key(&seed[2..])?);
        let response = uds::call_raw(self.0, &request)?;
        if response.get(1) != Some(&(level + 1)) {
            return Err(DiagError::InvalidResponse(
                "Unexpected security-access echo".into(),
            ));
        }
        Ok(())
    }
    fn read_data(&mut self, id: DataId) -> Result<Vec<u8>, DiagError> {
        uds::read_data_by_identifier(self.0, common(id)?)
    }
    fn write_data(&mut self, id: DataId, data: &[u8]) -> Result<(), DiagError> {
        let id = common(id)?;
        let mut request = vec![0x2E];
        request.extend(id.to_be_bytes());
        request.extend(data);
        let response = uds::call_raw(self.0, &request)?;
        if response.get(..3)
            != Some(
                &request[..3]
                    .iter()
                    .enumerate()
                    .map(|(i, b)| if i == 0 { 0x6E } else { *b })
                    .collect::<Vec<_>>()[..],
            )
        {
            return Err(DiagError::InvalidResponse(
                "WriteDataByIdentifier unexpected echo".into(),
            ));
        }
        Ok(())
    }
    fn routine(&mut self, op: RoutineOp, id: u16, data: &[u8]) -> Result<Vec<u8>, DiagError> {
        let op = match op {
            RoutineOp::Start => 1,
            RoutineOp::Stop => 2,
            RoutineOp::Results => 3,
        };
        let mut request = vec![0x31, op];
        request.extend(id.to_be_bytes());
        request.extend(data);
        let response = uds::call_raw(self.0, &request)?;
        if response.len() < 4 || response[1..4] != request[1..4] {
            return Err(DiagError::InvalidResponse(
                "RoutineControl unexpected echo".into(),
            ));
        }
        Ok(response[4..].to_vec())
    }
    fn ecu_reset(&mut self, kind: ResetKind) -> Result<(), DiagError> {
        let id = match kind {
            ResetKind::Hard => 1,
            ResetKind::KeyOffOn => 2,
            ResetKind::Soft => 3,
        };
        let response = uds::call_raw(self.0, &[0x11, id])?;
        if response.get(1) != Some(&id) {
            return Err(DiagError::InvalidResponse(
                "ECU reset unexpected echo".into(),
            ));
        }
        Ok(())
    }
    fn identification(&mut self) -> Result<EcuInfo, DiagError> {
        let mut info = EcuInfo::default();
        for id in [0xF181, 0xF187, 0xF18C, 0xF190] {
            match self.read_data(DataId::Common(id)) {
                Ok(data) => {
                    if id == 0xF181 {
                        info.calibration = Some(text(&data));
                    }
                    if id == 0xF190 {
                        info.vin = Some(text(&data));
                    }
                    info.identification.insert(id, data);
                }
                Err(DiagError::UdsNegativeResponse {
                    code: 0x11 | 0x12 | 0x31,
                    ..
                }) => {}
                Err(e) => return Err(e),
            }
        }
        if info.identification.is_empty() {
            return Err(DiagError::Unsupported(
                "ECU answered none of the standard identification DIDs".into(),
            ));
        }
        Ok(info)
    }
    fn read_dtcs(&mut self) -> Result<Vec<DiagnosticTroubleCode>, DiagError> {
        let response = uds::call_raw(self.0, &[0x19, 0x02, 0xFF])?;
        if response.len() < 3 || response[1] != 2 || (response.len() - 3) % 4 != 0 {
            return Err(DiagError::InvalidResponse(
                "Malformed UDS DTC response".into(),
            ));
        }
        Ok(response[3..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|record| DiagnosticTroubleCode {
                code: format!("0x{}", crate::utils::bytes_to_hex(&record[..3])),
                description: format!("UDS DTC (status 0x{:02X})", record[3]),
            })
            .collect())
    }
    fn clear_dtcs(&mut self) -> Result<(), DiagError> {
        uds::call_raw(self.0, &[0x14, 0xFF, 0xFF, 0xFF]).map(|_| ())
    }
    fn request(&mut self, data: &[u8], expect: &[u8]) -> Result<Vec<u8>, DiagError> {
        if data.is_empty() || expect.is_empty() {
            return Err(DiagError::InvalidCommand(
                "A declarative request needs nonempty request and expected reply bytes".into(),
            ));
        }
        let response = uds::call_raw(self.0, data)?;
        if !response.starts_with(expect) {
            return Err(DiagError::InvalidResponse(
                "Manufacturer request did not return its expected prefix".into(),
            ));
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{protocol_on, protocol_with, FakeEcu};
    #[test]
    fn service_calls_wait_on_pending_without_resending_and_check_the_echo() {
        let (mut protocol, link) =
            protocol_with(&["7F1078", "5003", "7F2278", "62F181414243", "6EF102"]);
        let mut services = Uds(&mut protocol);
        services.session(Session::Extended).unwrap();
        assert_eq!(services.read_data(DataId::Common(0xF181)).unwrap(), b"ABC");
        assert!(services.write_data(DataId::Common(0xF101), &[1]).is_err());
        assert_eq!(link.sent(), ["1003", "22F181", "2EF10101"]);
    }
    #[test]
    fn identification_returns_structured_data_from_the_fake_ecu() {
        let ecu = FakeEcu::new(0x18DAF101, b"37805-ABC", &[1; 4]);
        let mut protocol = protocol_on(&ecu);
        protocol.set_header_id(0x18DA01F1).unwrap();
        let info = Uds(&mut protocol).identification().unwrap();
        assert_eq!(info.calibration.as_deref(), Some("37805-ABC"));
        assert_eq!(info.vin.as_deref(), Some("1HGCM82633A004352"));
    }
}
