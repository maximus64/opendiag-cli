use crate::errors::DiagError;
use crate::link::{Adapter, BusSpec, DeviceOps, DiagLink};
use crate::obd::addressing::{Addressing, EcuAddress};
use crate::obd::dtc::get_dtc_description;
use crate::obd::pids::get_pid_definition;
use crate::obd::pids::oxygen;
use crate::utils::bytes_to_hex;
use crate::{
    events::{Cancel, Event, Events, Interaction, NoEvents},
    preflight::Preflight,
    value::Value,
};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub use crate::link::ResponseFrame;

pub const VOLTAGE_PIN_OFF: u32 = crate::j2534::consts::PIN_OFF;
pub const VOLTAGE_SHORT_TO_GROUND: u32 = crate::j2534::consts::SHORT_TO_GROUND;

/// Every J1962 pin J2534 allows programming voltage on: 6, 9 and 11–14 high
/// side, 15 low side. No other pin is addressable, so there is nothing to clear
/// anywhere else.
const PROGRAMMING_VOLTAGE_PINS: [u32; 7] = [6, 9, 11, 12, 13, 14, 15];

/// How long a request waits for its reply unless the caller says otherwise.
pub const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait after a "response pending" (NRC 0x78) — UDS P2*.
pub const PENDING_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

// The debug version
#[cfg(debug_assertions)]
macro_rules! debug_print {
    ($( $args:expr ),*) => { eprintln!( $( $args ),* ); }
}

// Non-debug version
#[cfg(not(debug_assertions))]
macro_rules! debug_print {
    ($( $args:expr ),*) => {};
}

/// Diagnostic Trouble Code (DTC) structure
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiagnosticTroubleCode {
    pub code: String,
    pub description: String,
}

/// Stands in for the link while [`ObdProtocol::switch_bus`] replaces it, so
/// the old bus is closed before the new one opens (adapters often allow only
/// one channel per protocol).
struct Disconnected(BusSpec);

impl DiagLink for Disconnected {
    fn bus(&self) -> &BusSpec {
        &self.0
    }
    fn max_payload(&self) -> usize {
        0
    }
    fn set_target(&mut self, _: u32) -> Result<(), DiagError> {
        Err(DiagError::Adapter("No bus is connected".into()))
    }
    fn send(&mut self, _: &[u8]) -> Result<(), DiagError> {
        Err(DiagError::Adapter("No bus is connected".into()))
    }
    fn receive(&mut self, _: Duration) -> Result<Option<ResponseFrame>, DiagError> {
        Err(DiagError::Adapter("No bus is connected".into()))
    }
    fn request_functional(
        &mut self,
        _: &[u8],
        _: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        Err(DiagError::Adapter("No bus is connected".into()))
    }
    fn reset(&mut self) -> Result<(), DiagError> {
        Ok(())
    }
}

/// Drops programming voltage on every pin J2534 can drive, best effort.
///
/// Call this once after opening the adapter, before connecting a bus. A flash
/// releases its own pin through a guard, but a process killed with SIGINT never
/// unwinds, and on some adapters an unplugged USB cable doesn't release pins
/// either. Pins an adapter can't drive are skipped silently; a pin another
/// client holds is reported but doesn't stop the run.
pub fn clear_all_programming_voltage(device: &dyn DeviceOps) -> Vec<String> {
    use crate::j2534::consts::{ERR_NOT_SUPPORTED, ERR_PIN_IN_USE, ERR_PIN_NOT_SUPPORTED};
    let mut warnings = Vec::new();

    for pin in PROGRAMMING_VOLTAGE_PINS {
        match device.set_programming_voltage(pin, VOLTAGE_PIN_OFF) {
            Ok(()) => {}
            Err(DiagError::PassThru { code, .. }) if code == ERR_PIN_NOT_SUPPORTED || code == ERR_NOT_SUPPORTED => {}
            Err(DiagError::PassThru { code: ERR_PIN_IN_USE, .. }) => warnings.push(format!("pin {pin} is held by another client (e.g. the adapter's debug shell); leaving it alone")),
            Err(e) => warnings.push(format!("couldn't release pin {pin}: {e}")),
        }
    }
    warnings
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PidValue {
    pub pid: u8,
    pub name: String,
    pub value: Value,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct EcuDtcs {
    pub ecu: EcuAddress,
    pub dtcs: Vec<DiagnosticTroubleCode>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct VehicleInfo {
    pub ecu: EcuAddress,
    pub values: Vec<PidValue>,
}

/// OBD protocol implementation
/// This handles the core OBD-II communication logic
pub struct ObdProtocol {
    adapter: Rc<dyn Adapter>,
    link: Box<dyn DiagLink>,
    /// The request header last set, so callers that know the right target (a
    /// firmware container naming its ECU, say) can tell whether they are about
    /// to redirect traffic away from what the user asked for.
    header_id: Option<u32>,
    target: Option<EcuAddress>,
    events: Box<dyn Events>,
    obd_cache: Option<Vec<EcuAddress>>,
    supported_cache: BTreeMap<(EcuAddress, u8), Vec<u8>>,
    scaling_cache: BTreeMap<EcuAddress, [u8; 4]>,
    combined_failed: BTreeSet<EcuAddress>,
    functional_obd: BTreeSet<EcuAddress>,
}

impl ObdProtocol {
    pub fn new(adapter: Rc<dyn Adapter>, link: Box<dyn DiagLink>) -> Self {
        ObdProtocol {
            adapter,
            link,
            header_id: None,
            target: None,
            events: Box::new(NoEvents),
            obd_cache: None,
            supported_cache: BTreeMap::new(),
            scaling_cache: BTreeMap::new(),
            combined_failed: BTreeSet::new(),
            functional_obd: BTreeSet::new(),
        }
    }

    pub fn set_events(&mut self, events: Box<dyn Events>) {
        self.events = events;
    }
    pub fn emit(&mut self, event: Event<'_>) {
        self.events.emit(event);
    }
    pub fn max_payload(&self) -> usize {
        self.link.max_payload()
    }

    /// The bus this session runs on.
    pub fn bus(&self) -> &BusSpec {
        self.link.bus()
    }

    /// The addressing rules in force, for callers that need to translate
    /// between response and request headers.
    pub fn addressing(&self) -> Addressing {
        Addressing::from(self.link.bus())
    }

    /// Moves the session to another bus, e.g. for an ECU that isn't on the OBD
    /// bus. The old bus is closed first, and the target must be set again.
    #[allow(dead_code)] // first used by multi-bus vehicle profiles (phase B)
    pub fn switch_bus(&mut self, bus: &BusSpec) -> Result<(), DiagError> {
        self.link = Box::new(Disconnected(bus.clone()));
        self.header_id = None;
        self.target = None;
        self.obd_cache = None;
        self.supported_cache.clear();
        self.scaling_cache.clear();
        self.combined_failed.clear();
        self.functional_obd.clear();
        self.link = self.adapter.connect(bus)?;
        Ok(())
    }

    /// Flushes anything queued on the bus.
    pub fn reset(&mut self) -> Result<(), DiagError> {
        self.link.reset()
    }

    pub fn set_header_id(&mut self, id: u32) -> Result<(), DiagError> {
        self.link.set_target(id)?;
        self.header_id = Some(id);
        self.target = self.addressing().parse_ecu(id).ok();
        Ok(())
    }

    pub fn target(&self) -> Option<EcuAddress> {
        self.target
    }
    pub fn set_ecu(&mut self, ecu: EcuAddress) -> Result<(), DiagError> {
        if self.target == Some(ecu) {
            return Ok(());
        }
        if self.bus().is_can() != matches!(ecu, EcuAddress::Can { .. }) {
            return Err(DiagError::InvalidCommand(format!(
                "ECU address {ecu} does not belong on {}",
                self.bus()
            )));
        }
        if let EcuAddress::Can { request, response } = ecu {
            let max = if self.addressing() == Addressing::Can11Bit {
                0x7FF
            } else {
                0x1FFF_FFFF
            };
            if request > max || response > max {
                return Err(DiagError::InvalidCommand(
                    "CAN target exceeds the bus identifier width".into(),
                ));
            }
        }
        self.link.set_ecu(ecu)?;
        self.target = Some(ecu);
        self.header_id = Some(match ecu {
            EcuAddress::Can { request, .. } => request,
            EcuAddress::Byte(byte) => byte as u32,
        });
        Ok(())
    }
    pub fn set_target(&mut self, request: u32, response: u32) -> Result<(), DiagError> {
        self.set_ecu(EcuAddress::Can { request, response })
    }

    /// The request header currently in effect, if one has been set.
    pub fn header_id(&self) -> Option<u32> {
        self.header_id
    }

    /// Battery voltage in millivolts; formatting is the frontend's choice.
    pub fn read_battery_mv(&self) -> Result<u32, DiagError> {
        self.adapter.read_battery_mv()
    }

    /// Set programming voltage (mV, or the off/ground sentinels) on a pin.
    pub fn set_programming_voltage(&mut self, pin: u32, voltage: u32) -> Result<(), DiagError> {
        self.adapter.set_programming_voltage(pin, voltage)
    }

    /// The programming voltage the adapter measures, in mV.
    pub fn read_programming_voltage_mv(&mut self) -> Result<u32, DiagError> {
        self.adapter.read_programming_voltage_mv()
    }

    /// Reads all supported PIDs from the ECU
    pub fn read_supported_pids(&mut self, sid: u8) -> Result<Vec<u8>, DiagError> {
        let mut supported_pids = Vec::new();
        let mut current_pid_base: u8 = 0x00;

        loop {
            // Construct the OBD-II request frame for supported PIDs.
            let request_frame: [u8; 2] = [sid, current_pid_base];

            self.emit(Event::Detail(&format!(
                "Querying PIDs starting at 0x{current_pid_base:02X}"
            )));

            let response = match self.obd_transfer(&request_frame) {
                Ok(resp) => resp,
                Err(e) => {
                    // If the first query (PID 0x00) fails, we can't proceed.
                    if current_pid_base == 0x00 {
                        return Err(e);
                    } else {
                        // For subsequent PIDs (0x20, 0x40, etc.), an error likely means it's not supported.
                        self.emit(Event::Warning(&format!(
                            "No response for PID 0x{current_pid_base:02X}; stopping discovery: {e}"
                        )));
                        break;
                    }
                }
            };

            // A valid Mode 01 response starts with 0x41 and echoes the requested PID.
            if response.data.len() < 6
                || response.data[0] != (sid + 0x40)
                || response.data[1] != current_pid_base
            {
                return Err(DiagError::InvalidResponse(format!(
                    "Unexpected response for supported PIDs query at 0x{current_pid_base:02X}"
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
                        } else {
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

    /// Returns decoded values without losing their numeric types.
    pub fn read_all_vehicle_data(&mut self, sid: u8) -> Result<Vec<PidValue>, DiagError> {
        let supported = self.read_supported_pids(sid)?;
        let mut values = Vec::new();
        for pid in supported {
            match self.read_pid(sid, pid) {
                Ok(value) => values.push(value),
                Err(error) => self.emit(Event::Warning(&format!("PID 0x{pid:02X}: {error}"))),
            }
        }
        Ok(values)
    }

    pub fn read_pid(&mut self, sid: u8, pid: u8) -> Result<PidValue, DiagError> {
        if sid == 1 {
            if let Some(ecu) = self.target {
                self.prepare_pid_context(ecu, &[pid])?;
            }
        }
        let data = self.read_pid_raw(sid, pid)?;
        Ok(match self.target {
            Some(ecu) => self.decode_pid_for(ecu, sid, pid, &data),
            None => Self::decode_pid(sid, pid, &data),
        })
    }

    fn read_pid_raw(&mut self, sid: u8, pid: u8) -> Result<Vec<u8>, DiagError> {
        let response = self.obd_transfer(&[sid, pid])?;
        if response.data.len() < 2 || response.data[..2] != [sid + 0x40, pid] {
            return Err(DiagError::InvalidResponse(format!(
                "Unexpected response to PID 0x{pid:02X}"
            )));
        }
        if let Some(len) = get_pid_definition(sid, pid).and_then(|def| def.len) {
            if response.data.len() < len + 2 {
                return Err(DiagError::InvalidResponse(format!(
                    "PID 0x{pid:02X} reply is truncated"
                )));
            }
        }
        Ok(response.data[2..].to_vec())
    }

    /// Decodes without ECU context: oxygen sensor numbers and nominal scaling.
    pub fn decode_pid(sid: u8, pid: u8, data: &[u8]) -> PidValue {
        match get_pid_definition(sid, pid) {
            Some(def) => PidValue {
                pid,
                name: def.name.into(),
                value: (def.parser)(data).with_unit(def.unit),
            },
            None => PidValue {
                pid,
                name: format!("PID_{pid:02X}"),
                value: Value::Text(bytes_to_hex(data)),
            },
        }
    }

    fn decode_pid_for(&self, ecu: EcuAddress, sid: u8, pid: u8, data: &[u8]) -> PidValue {
        let mut value = Self::decode_pid(sid, pid, data);
        if sid == 1 {
            let supported = self
                .supported_cache
                .get(&(ecu, sid))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if let Some(name) = oxygen::name(pid, oxygen::Layout::from_supported(supported)) {
                value.name = name;
            }
            if let Some(limits) = self.scaling_cache.get(&ecu) {
                if let Some(scaled) = oxygen::scaled_value(pid, data, *limits) {
                    value.value = scaled;
                }
            }
        }
        value
    }

    fn prepare_pid_context(&mut self, ecu: EcuAddress, pids: &[u8]) -> Result<(), DiagError> {
        if !pids
            .iter()
            .any(|pid| oxygen::is_sensor(*pid) || oxygen::uses_scaling(*pid))
        {
            return Ok(());
        }
        let supported = self.supported_pids_for(ecu, 1)?;
        if !pids.iter().any(|pid| oxygen::uses_scaling(*pid))
            || !supported.contains(&0x4F)
            || self.scaling_cache.contains_key(&ecu)
        {
            return Ok(());
        }
        let data = if self.bus().is_can() {
            self.set_ecu(ecu)?;
            self.read_pid_raw(1, 0x4F)?
        } else {
            self.broadcast(&[1, 0x4F], None)?
                .into_iter()
                .find(|reply| {
                    self.ecu_for_response(reply.id) == Some(ecu)
                        && reply.data.starts_with(&[0x41, 0x4F])
                })
                .ok_or_else(|| DiagError::Timeout(format!("No PID 4F scaling reply from {ecu}")))?
                .data[2..]
                .to_vec()
        };
        let limits = data.get(..4).ok_or_else(|| {
            DiagError::InvalidResponse("PID 4F scaling reply is truncated".into())
        })?;
        self.scaling_cache.insert(ecu, limits.try_into().unwrap());
        Ok(())
    }

    pub fn read_pids(&mut self, ecu: EcuAddress, pids: &[u8]) -> Result<Vec<PidValue>, DiagError> {
        if !self.bus().is_can() {
            let mut values = self.read_functional_pids(pids)?;
            return values
                .remove(&ecu)
                .ok_or_else(|| DiagError::Timeout(format!("No PID reply from {ecu}")));
        }
        self.set_ecu(ecu)?;
        self.prepare_pid_context(ecu, pids)?;
        let mut results = Vec::new();
        for group in pids.chunks(6) {
            let can_combine = group.len() > 1
                && !self.combined_failed.contains(&ecu)
                && group
                    .iter()
                    .all(|pid| get_pid_definition(1, *pid).and_then(|d| d.len).is_some());
            if can_combine {
                let mut request = vec![1];
                request.extend_from_slice(group);
                let result = self
                    .obd_transfer(&request)
                    .and_then(|response| self.split_pid_reply(ecu, group, &response.data));
                if let Ok(mut values) = result {
                    results.append(&mut values);
                    continue;
                }
                self.combined_failed.insert(ecu);
                self.emit(Event::Warning(&format!("{ecu} did not answer a combined PID request correctly; using one PID per request")));
            }
            for pid in group {
                results.push(self.read_pid(1, *pid)?);
            }
        }
        Ok(results)
    }

    fn split_pid_reply(
        &self,
        ecu: EcuAddress,
        pids: &[u8],
        data: &[u8],
    ) -> Result<Vec<PidValue>, DiagError> {
        if data.first() != Some(&0x41) {
            return Err(DiagError::InvalidResponse(
                "Unexpected combined PID service".into(),
            ));
        }
        let mut cursor = 1;
        let mut values = BTreeMap::new();
        while values.len() < pids.len() {
            let pid = *data.get(cursor).ok_or_else(|| {
                DiagError::InvalidResponse("Combined PID reply is truncated".into())
            })?;
            let len = get_pid_definition(1, pid)
                .and_then(|def| def.len)
                .ok_or_else(|| {
                    DiagError::InvalidResponse(
                        "Combined reply contains a PID of unknown length".into(),
                    )
                })?;
            let body = data.get(cursor + 1..cursor + 1 + len).ok_or_else(|| {
                DiagError::InvalidResponse("Combined PID data is truncated".into())
            })?;
            if !pids.contains(&pid)
                || values
                    .insert(pid, self.decode_pid_for(ecu, 1, pid, body))
                    .is_some()
            {
                return Err(DiagError::InvalidResponse(
                    "Combined reply contains an unexpected or duplicate PID".into(),
                ));
            }
            cursor += 1 + len;
        }
        Ok(pids.iter().map(|pid| values.remove(pid).unwrap()).collect())
    }

    pub fn ecu_for_response(&self, header: u32) -> Option<EcuAddress> {
        let byte = self.addressing().responder_address(header)?;
        if self.bus().is_can() {
            self.addressing()
                .physical_request(byte)
                .and_then(|request| self.addressing().parse_ecu(request).ok())
        } else {
            Some(EcuAddress::Byte(byte))
        }
    }
    pub fn obd_ecus(&mut self) -> Result<Vec<EcuAddress>, DiagError> {
        if let Some(ecus) = &self.obd_cache {
            return Ok(ecus.clone());
        }
        self.emit(Event::Step("Finding OBD ECUs"));
        let replies = self.broadcast(&[1, 0], None)?;
        let mut ecus = BTreeSet::new();
        for reply in replies {
            if reply.data.len() >= 6 && reply.data[..2] == [0x41, 0] {
                if let Some(ecu) = self.ecu_for_response(reply.id) {
                    ecus.insert(ecu);
                }
            }
        }
        if ecus.is_empty() {
            return Err(DiagError::Timeout(
                "No OBD ECU replied. Check the ignition, bus and adapter; use `opendiag ecu scan`."
                    .into(),
            ));
        }
        let ecus: Vec<_> = ecus.into_iter().collect();
        self.obd_cache = Some(ecus.clone());
        Ok(ecus)
    }
    pub fn supported_pids_for(&mut self, ecu: EcuAddress, sid: u8) -> Result<Vec<u8>, DiagError> {
        if let Some(pids) = self.supported_cache.get(&(ecu, sid)) {
            return Ok(pids.clone());
        }
        let pids = if self.bus().is_can() {
            self.set_ecu(ecu)?;
            self.read_supported_pids(sid)?
        } else {
            let mut pids = Vec::new();
            for base in (0..=0xC0).step_by(0x20) {
                let replies = self.broadcast(&[sid, base], None)?;
                let response = replies.into_iter().find(|reply| {
                    self.ecu_for_response(reply.id) == Some(ecu)
                        && reply.data.len() >= 2
                        && reply.data[..2] == [sid + 0x40, base]
                });
                let Some(response) = response else {
                    let message = format!("No supported-PID reply from {ecu} for 0x{base:02X}");
                    if base == 0 {
                        return Err(DiagError::Timeout(message));
                    }
                    self.emit(Event::Warning(&format!(
                        "{message}; stopping discovery and keeping earlier PID groups"
                    )));
                    break;
                };
                if response.data.len() < 6 {
                    return Err(DiagError::InvalidResponse(format!(
                        "Truncated supported-PID bitmap from {ecu} for 0x{base:02X}"
                    )));
                }
                let offset = if sid == 9 {
                    if response.data.len() != 7 || response.data[2] != 1 {
                        return Err(DiagError::InvalidResponse(
                            "Legacy Mode 09 bitmap requires one message and four bitmap bytes"
                                .into(),
                        ));
                    }
                    3
                } else {
                    2
                };
                let bits =
                    u32::from_be_bytes(response.data[offset..offset + 4].try_into().unwrap());
                for index in 0..31 {
                    if bits & (1 << (31 - index)) != 0 {
                        pids.push(base + index + 1);
                    }
                }
                if bits & 1 == 0 {
                    break;
                }
            }
            pids
        };
        self.supported_cache.insert((ecu, sid), pids.clone());
        Ok(pids)
    }
    pub fn read_functional_pids(
        &mut self,
        pids: &[u8],
    ) -> Result<BTreeMap<EcuAddress, Vec<PidValue>>, DiagError> {
        if pids
            .iter()
            .any(|pid| oxygen::is_sensor(*pid) || oxygen::uses_scaling(*pid))
        {
            for ecu in self.obd_ecus()? {
                self.prepare_pid_context(ecu, pids)?;
            }
        }
        let mut values: BTreeMap<EcuAddress, Vec<PidValue>> = BTreeMap::new();
        for pid in pids.iter().copied().collect::<BTreeSet<_>>() {
            for reply in self.broadcast(&[1, pid], None)? {
                if reply.data.len() >= 2 && reply.data[..2] == [0x41, pid] {
                    if get_pid_definition(1, pid)
                        .and_then(|def| def.len)
                        .is_some_and(|len| reply.data.len() < len + 2)
                    {
                        return Err(DiagError::InvalidResponse(format!(
                            "PID 0x{pid:02X} reply is truncated"
                        )));
                    }
                    if let Some(ecu) = self.ecu_for_response(reply.id) {
                        values.entry(ecu).or_default().push(self.decode_pid_for(
                            ecu,
                            1,
                            pid,
                            &reply.data[2..],
                        ));
                    }
                }
            }
        }
        Ok(values)
    }
    pub fn read_obd_dtcs(
        &mut self,
        targets: &[EcuAddress],
        pending: bool,
        cancel: &Cancel,
    ) -> Result<Vec<EcuDtcs>, DiagError> {
        if self.bus().is_can() {
            let mut results = Vec::new();
            for ecu in targets {
                cancel.check()?;
                self.set_ecu(*ecu)?;
                results.push(EcuDtcs {
                    ecu: *ecu,
                    dtcs: self.read_dtc(pending)?,
                });
            }
            return Ok(results);
        }
        cancel.check()?;
        let mode = if pending { 7 } else { 3 };
        let mut results: BTreeMap<EcuAddress, Vec<DiagnosticTroubleCode>> =
            targets.iter().map(|ecu| (*ecu, Vec::new())).collect();
        let mut responders = BTreeSet::new();
        for reply in self.broadcast(&[mode], None)? {
            if let Some(ecu) = self.ecu_for_response(reply.id) {
                if targets.contains(&ecu) {
                    responders.insert(ecu);
                    results
                        .entry(ecu)
                        .or_default()
                        .extend(Self::decode_dtc_response(mode, &reply.data, false)?);
                }
            }
        }
        let missing: Vec<_> = targets
            .iter()
            .copied()
            .filter(|ecu| !responders.contains(ecu))
            .collect();
        if !pending
            && !missing.is_empty()
            && matches!(
                self.addressing(),
                Addressing::Iso9141_2 | Addressing::J1850Pwm | Addressing::J1850Vpw
            )
        {
            // J1979 permits silence with no stored DTCs on these buses. Require
            // a fresh PID 01 count from each missing ECU before accepting it.
            cancel.check()?;
            let mut counts = BTreeMap::new();
            for reply in self.broadcast(&[1, 1], None)? {
                if let Some(ecu) = self.ecu_for_response(reply.id) {
                    if missing.contains(&ecu)
                        && reply.data.len() == 6
                        && reply.data[..2] == [0x41, 1]
                    {
                        let count = reply.data[2] & 0x7F;
                        if counts
                            .insert(ecu, count)
                            .is_some_and(|previous| previous != count)
                        {
                            return Err(DiagError::InvalidResponse(
                                "Conflicting stored-DTC counts".into(),
                            ));
                        }
                    }
                }
            }
            for ecu in &missing {
                if counts.get(ecu) == Some(&0) {
                    responders.insert(*ecu);
                }
            }
        }
        if targets.iter().any(|ecu| !responders.contains(ecu)) {
            return Err(DiagError::Timeout(
                "Not every selected ECU answered the DTC request".into(),
            ));
        }
        Ok(results
            .into_iter()
            .map(|(ecu, dtcs)| EcuDtcs { ecu, dtcs })
            .collect())
    }
    pub fn clear_obd_dtcs(
        &mut self,
        targets: &[EcuAddress],
        interaction: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        if !self.bus().is_can() && self.obd_ecus()?.iter().any(|ecu| !targets.contains(ecu)) {
            return Err(DiagError::Unsupported("Legacy OBD clearing is functional and affects every OBD ECU; a single-ECU clear needs that ECU's manufacturer diagnostic service".into()));
        }
        let mut preflight = Preflight::new(
            "Clear OBD trouble codes",
            "This clears trouble codes and freeze-frame data.",
        );
        preflight.add(
            "ECUs",
            targets
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            crate::preflight::CheckStatus::Info,
        );
        preflight.authorize(interaction, cancel)?;
        if self.bus().is_can() {
            for ecu in targets {
                cancel.check()?;
                self.set_ecu(*ecu)?;
                self.clear_dtc()?;
            }
        } else {
            let replies = self.broadcast(&[4], None)?;
            let accepted: BTreeSet<_> = replies
                .into_iter()
                .filter(|r| r.data.first() == Some(&0x44))
                .filter_map(|r| self.ecu_for_response(r.id))
                .collect();
            if !targets.iter().all(|target| accepted.contains(target)) {
                return Err(DiagError::Timeout(
                    "Not every selected ECU acknowledged clearing codes".into(),
                ));
            }
        }
        Ok(())
    }
    pub fn vehicle_info(
        &mut self,
        targets: &[EcuAddress],
        cancel: &Cancel,
    ) -> Result<Vec<VehicleInfo>, DiagError> {
        let mut results = Vec::new();
        for ecu in targets {
            cancel.check()?;
            let pids = match self.supported_pids_for(*ecu, 9) {
                Ok(pids) => pids,
                Err(error @ DiagError::Timeout(_)) if !self.bus().is_can() => {
                    // An older ECU may omit Mode 09. Distinguish that from a
                    // disconnected ECU with a fresh response from this source.
                    cancel.check()?;
                    let alive = self.broadcast(&[1, 0], None)?.iter().any(|reply| {
                        self.ecu_for_response(reply.id) == Some(*ecu)
                            && reply.data.len() >= 6
                            && reply.data[..2] == [0x41, 0]
                    });
                    if !alive {
                        return Err(error);
                    }
                    self.emit(Event::Warning(&format!(
                        "{ecu} did not provide Mode 09 vehicle information"
                    )));
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            let mut values = Vec::new();
            for pid in pids
                .into_iter()
                .filter(|pid| get_pid_definition(9, *pid).is_some())
            {
                cancel.check()?;
                let data = self.read_mode09(*ecu, pid)?;
                values.push(Self::decode_pid(9, pid, &data));
            }
            results.push(VehicleInfo { ecu: *ecu, values });
        }
        Ok(results)
    }
    /// Mode 09 data, including its record-count byte, on either message format.
    pub fn read_mode09(&mut self, ecu: EcuAddress, pid: u8) -> Result<Vec<u8>, DiagError> {
        if self.bus().is_can() {
            self.set_ecu(ecu)?;
            let response = self.obd_transfer(&[9, pid])?;
            if response.data.len() < 3 || response.data[..2] != [0x49, pid] {
                return Err(DiagError::InvalidResponse(
                    "Unexpected Mode 09 reply".into(),
                ));
            }
            return Ok(response.data[2..].to_vec());
        }
        let relevant: Vec<_> = self
            .broadcast(&[9, pid], None)?
            .into_iter()
            .filter(|reply| {
                self.ecu_for_response(reply.id) == Some(ecu) && reply.data.starts_with(&[0x49, pid])
            })
            .collect();
        if relevant.is_empty() {
            return Err(DiagError::Timeout(format!(
                "No Mode 09 PID 0x{pid:02X} reply from {ecu}"
            )));
        }
        Self::reassemble_mode09(pid, &relevant)
    }

    pub fn reassemble_mode09(pid: u8, frames: &[ResponseFrame]) -> Result<Vec<u8>, DiagError> {
        let mut parts = BTreeMap::new();
        for frame in frames {
            if frame.data.len() < 4 || frame.data[..2] != [0x49, pid] || frame.data[2] == 0 {
                return Err(DiagError::InvalidResponse(
                    "Malformed numbered Mode 09 reply".into(),
                ));
            }
            let body = frame.data[3..].to_vec();
            if let Some(previous) = parts.insert(frame.data[2], body.clone()) {
                if previous != body {
                    return Err(DiagError::InvalidResponse(
                        "Conflicting Mode 09 sequence numbers".into(),
                    ));
                }
            }
        }
        if parts.is_empty() || parts.keys().copied().ne(1..=parts.len() as u8) {
            return Err(DiagError::InvalidResponse(
                "Mode 09 messages have a missing sequence number".into(),
            ));
        }
        let joined: Vec<_> = parts.into_values().flatten().collect();
        if pid == 4 && (joined.is_empty() || joined.len() % 16 != 0) {
            return Err(DiagError::InvalidResponse(
                "Calibration replies must contain complete 16-byte records".into(),
            ));
        }
        if pid == 6 && (joined.is_empty() || joined.len() % 4 != 0) {
            return Err(DiagError::InvalidResponse(
                "CVN replies must contain complete 4-byte records".into(),
            ));
        }
        let body = if pid == 2 {
            joined
                .iter()
                .position(|b| *b != 0)
                .map(|index| &joined[index..])
                .unwrap_or_default()
        } else {
            &joined[..]
        };
        if pid == 2 && body.len() != 17 {
            return Err(DiagError::InvalidResponse(
                "Numbered VIN reply must contain all 17 characters".into(),
            ));
        }
        let count = match pid {
            4 => joined.len() / 16,
            6 => joined.len() / 4,
            _ => 1,
        };
        let mut result = vec![count as u8];
        result.extend_from_slice(body);
        Ok(result)
    }

    pub fn clear_with_confirmation(
        &mut self,
        interaction: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        let preflight = Preflight::new(
            "Clear diagnostic trouble codes",
            "This clears trouble codes and freeze-frame data.",
        );
        preflight.authorize(interaction, cancel)?;
        self.clear_dtc()
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

        Ok(format!("{prefix}{code_number:04X}"))
    }

    /// Read stored Diagnostic Trouble Codes (DTCs)
    pub fn read_dtc(&mut self, pending: bool) -> Result<Vec<DiagnosticTroubleCode>, DiagError> {
        // Mode 03: Request stored DTCs
        // Mode 07: Request pending DTCs
        let mode: u8 = if pending { 0x07 } else { 0x03 };
        let response = self.obd_transfer(&[mode])?;
        // ISO 15765-4 (CAN) replies carry a DTC count after the mode byte; the
        // legacy buses (later phases) don't, and collect several messages.
        Self::decode_dtc_response(mode, &response.data, true)
    }

    /// Decodes one Mode 03/07 reply. `has_count_byte` is true on CAN.
    fn decode_dtc_response(
        mode: u8,
        data: &[u8],
        has_count_byte: bool,
    ) -> Result<Vec<DiagnosticTroubleCode>, DiagError> {
        // Validate response
        if data.is_empty() || data[0] != mode + 0x40 {
            return Err(DiagError::InvalidResponse("Invalid DTC response".into()));
        }

        let mut dtcs = Vec::new();

        let (dtc_count, dtc_data) = if has_count_byte {
            // Check if we have at least the DTC count byte
            if data.len() < 2 {
                return Err(DiagError::InvalidResponse(
                    "Response too short for DTC count".into(),
                ));
            }
            (Some(data[1]), &data[2..])
        } else {
            (None, &data[1..])
        };
        #[cfg(debug_assertions)]
        if let Some(count) = dtc_count {
            debug_print!("ECU reports {} DTC(s)", count);
        }

        // Check if there are no DTCs stored
        if dtc_count == Some(0) {
            debug_print!("No DTCs stored in ECU");
            return Ok(dtcs);
        }

        // Only the codes the ECU actually announced are real; anything past them
        // is CAN frame padding, which is often 0x55 or 0xAA rather than zeros and
        // would otherwise decode as phantom DTCs.
        let expected_bytes = match dtc_count {
            Some(count) => {
                let expected = count as usize * 2;
                if dtc_data.len() < expected {
                    return Err(DiagError::InvalidResponse(format!(
                        "Expected {} DTC bytes but got {}",
                        expected,
                        dtc_data.len()
                    )));
                }
                expected
            }
            None => dtc_data.len() - dtc_data.len() % 2,
        };

        // Each DTC is 2 bytes; 0000 is padding, not a code.
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
                        return Err(DiagError::InvalidResponse(format!(
                            "Failed to decode DTC bytes {:02X}{:02X}: {}",
                            chunk[0], chunk[1], e
                        )));
                    }
                }
            }
        }

        // With padding excluded above, a shortfall means the ECU announced more
        // codes than it sent — report it instead of panicking on the user.
        if let Some(count) = dtc_count {
            if dtcs.len() != count as usize {
                return Err(DiagError::InvalidResponse(format!(
                    "ECU reported {} DTC(s) but only {} could be decoded",
                    count,
                    dtcs.len()
                )));
            }
        }
        Ok(dtcs)
    }

    /// Clear stored Diagnostic Trouble Codes (DTCs)
    pub fn clear_dtc(&mut self) -> Result<(), DiagError> {
        // Mode 04: Clear DTCs and stored values
        let response = self.transfer(&[0x04])?;

        // Validate response
        if response.data.is_empty() || response.data[0] != 0x44 {
            return Err(DiagError::InvalidResponse(
                "Invalid clear DTC response".into(),
            ));
        }

        Ok(())
    }

    /// Sends a request to the current target and returns its reply. This is
    /// the primary entry point for diagnostic requests.
    pub fn transfer(&mut self, out_frame: &[u8]) -> Result<ResponseFrame, DiagError> {
        self.transfer_with_timeout(out_frame, None)
    }

    pub fn transfer_with_timeout(
        &mut self,
        out_frame: &[u8],
        timeout: Option<Duration>,
    ) -> Result<ResponseFrame, DiagError> {
        self.send(out_frame)?;
        let wait = timeout.unwrap_or(DEFAULT_REPLY_TIMEOUT);
        self.receive(wait)?.ok_or_else(|| no_reply(out_frame, wait))
    }

    /// Some OBD emulators provide usable replies only to functional requests.
    /// Learn that behavior per ECU, only for the read services we implement.
    fn obd_transfer(&mut self, request: &[u8]) -> Result<ResponseFrame, DiagError> {
        let read_only = matches!(request, [0x01 | 0x09, _, ..] | [0x03 | 0x07]);
        let Some(ecu @ EcuAddress::Can { response, .. }) = self.target else {
            return self.transfer(request);
        };
        if !read_only || request.len() > 7 || self.ecu_for_response(response) != Some(ecu) {
            return self.transfer(request);
        }
        if self.functional_obd.contains(&ecu) {
            return self.functional_obd_transfer(request, response);
        }
        self.send(request)?;
        match self.receive_obd_reply(request, response) {
            Err(original @ DiagError::Timeout(_)) => {
                let reply = match self.functional_obd_transfer(request, response) {
                    Err(DiagError::Unsupported(_)) => return Err(original),
                    result => result?,
                };
                if reply.data.first() == Some(&(request[0] + 0x40)) {
                    self.functional_obd.insert(ecu);
                    self.emit(Event::Warning(&format!(
                        "{ecu} answered only a functional OBD request; using functional addressing for OBD reads"
                    )));
                }
                Ok(reply)
            }
            result => result,
        }
    }

    fn functional_obd_transfer(
        &mut self,
        request: &[u8],
        response: u32,
    ) -> Result<ResponseFrame, DiagError> {
        self.emit(Event::Detail(&format!(
            "TX functional {} (reply 0x{response:X})",
            bytes_to_hex(request)
        )));
        self.link.send_functional_to_target(request)?;
        self.receive_obd_reply(request, response)
    }

    fn receive_obd_reply(
        &mut self,
        request: &[u8],
        response: u32,
    ) -> Result<ResponseFrame, DiagError> {
        let deadline = Instant::now() + DEFAULT_REPLY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let Some(reply) = self.receive(left)? else {
                break;
            };
            let positive = reply.data.first() == Some(&(request[0] + 0x40))
                && (request.len() == 1 || reply.data.get(1) == request.get(1));
            let negative = reply.data.len() >= 3 && reply.data[..2] == [0x7F, request[0]];
            if reply.id == response && (positive || negative) {
                return Ok(reply);
            }
        }
        Err(no_reply(request, DEFAULT_REPLY_TIMEOUT))
    }

    /// Sends one request to the current target without waiting for a reply.
    pub fn send(&mut self, out_frame: &[u8]) -> Result<(), DiagError> {
        if out_frame.is_empty() {
            return Err(DiagError::InvalidCommand(
                "Request frame cannot be empty".into(),
            ));
        }
        let max = self.link.max_payload();
        if out_frame.len() > max {
            return Err(DiagError::InvalidCommand(format!(
                "Request of {} bytes exceeds the {} byte limit on {}",
                out_frame.len(),
                max,
                self.link.bus()
            )));
        }
        self.emit(Event::Detail(&format!("TX {}", bytes_to_hex(out_frame))));
        self.link.send(out_frame)
    }

    /// The next reply from the current target, or `None` after `timeout`.
    pub fn receive(&mut self, timeout: Duration) -> Result<Option<ResponseFrame>, DiagError> {
        let reply = self.link.receive(timeout)?;
        if let Some(frame) = &reply {
            self.emit(Event::Detail(&format!(
                "RX 0x{:X} {}",
                frame.id,
                bytes_to_hex(&frame.data)
            )));
        }
        Ok(reply)
    }

    /// Sends a request and collects every reply until none has arrived for
    /// `idle`, or `total` has passed. For answers that span several messages,
    /// as OBD replies do on K-Line and J1850.
    #[allow(dead_code)] // first used by legacy-bus decoding (phase C)
    pub fn collect_responses(
        &mut self,
        request: &[u8],
        idle: Duration,
        total: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        self.send(request)?;
        let deadline = Instant::now() + total;
        let mut replies = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(replies);
            }
            match self.link.receive(idle.min(left))? {
                Some(reply) => replies.push(reply),
                None => return Ok(replies),
            }
        }
    }

    /// Functional (broadcast) request; every reply within `timeout`.
    pub fn broadcast(
        &mut self,
        out_frame: &[u8],
        timeout: Option<Duration>,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        if out_frame.is_empty() {
            return Err(DiagError::InvalidCommand(
                "Request frame cannot be empty".into(),
            ));
        }
        if out_frame.len() > self.link.max_payload() {
            return Err(DiagError::InvalidCommand(
                "Functional request exceeds bus payload limit".into(),
            ));
        }
        self.emit(Event::Detail(&format!(
            "TX functional {}",
            bytes_to_hex(out_frame)
        )));
        let replies = self
            .link
            .request_functional(out_frame, timeout.unwrap_or(DEFAULT_REPLY_TIMEOUT))?;
        for reply in &replies {
            self.emit(Event::Detail(&format!(
                "RX 0x{:X} {}",
                reply.id,
                bytes_to_hex(&reply.data)
            )));
        }
        Ok(replies)
    }
}

/// The error for a request that got no reply.
pub fn no_reply(request: &[u8], waited: Duration) -> DiagError {
    DiagError::Timeout(format!(
        "No reply to {} within {} ms",
        bytes_to_hex(&request[..request.len().min(3)]),
        waited.as_millis()
    ))
}

/// A lightweight trait to abstract request/response for the UDS client.
pub trait IsoTpTransport {
    fn send(&mut self, payload: &[u8]) -> Result<(), DiagError>;
    /// The next reply payload, or `None` after `timeout`.
    fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, DiagError>;
}

impl IsoTpTransport for ObdProtocol {
    fn send(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        ObdProtocol::send(self, payload)
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, DiagError> {
        Ok(ObdProtocol::receive(self, timeout)?.map(|frame| frame.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::IdWidth;
    use crate::test_support::{protocol_with, FakeAdapter, MockLink};

    #[test]
    fn can_oxygen_reads_use_discovered_layout_in_combined_and_fallback_replies() {
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        let (mut protocol, link) = protocol_with(&[
            "410000000508", // PIDs 16, 18, 1D: four-bank layout.
            "411600C01880FF",
            "411600C0", // Incomplete combined reply triggers individual reads.
            "411600C0",
            "411880FF",
        ]);
        for _ in 0..2 {
            let values = protocol.read_pids(ecu, &[0x16, 0x18]).unwrap();
            assert_eq!(values[0].name, "O2_B2S1");
            assert_eq!(values[1].name, "O2_B3S1");
            assert_eq!(
                values[0].value.field("short_term_fuel_trim"),
                Some(&Value::number(50.0, "%"))
            );
            assert_eq!(
                values[1].value.field("short_term_fuel_trim"),
                Some(&Value::Unavailable)
            );
        }
        assert_eq!(link.sent(), ["0100", "011618", "011618", "0116", "0118"]);
    }

    #[test]
    fn wideband_scaling_is_cached_per_ecu_and_combined_pid_38_is_four_bytes() {
        let first = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        let second = EcuAddress::Can {
            request: 0x18DA10F1,
            response: 0x18DAF110,
        };
        let adapter = FakeAdapter::new();
        let (mut protocol, link) = crate::test_support::protocol_on_adapter(
            &adapter,
            &[
                "414F0410404D",
                "4124FFFFFFFF3880009C4044FFFF0BFF",
                "413880005C40",
            ],
        );
        protocol
            .supported_cache
            .insert((first, 1), vec![0x13, 0x24, 0x38, 0x4F]);
        let values = protocol
            .read_pids(first, &[0x24, 0x38, 0x44, 0x0B])
            .unwrap();
        assert_eq!(
            values[0].value.field("voltage"),
            Some(&Value::number(16.0, "V"))
        );
        assert_eq!(values[1].name, "O2_B2S1_WR_CURRENT");
        assert_eq!(
            values[1].value.field("current"),
            Some(&Value::number(14.125, "mA"))
        );
        assert_eq!(values[2].value, Value::number(4.0, "ratio"));
        assert_eq!(values[3].value, Value::number(770.0, "kPa"));
        let value = protocol.read_pid(1, 0x38).unwrap();
        assert_eq!(
            value.value.field("current"),
            Some(&Value::number(-17.875, "mA"))
        );
        assert_eq!(link.sent(), ["014F", "012438440B", "0138"]);

        // A different ECU using the four-bank map must not inherit first's limits.
        protocol
            .supported_cache
            .insert((second, 1), vec![0x1D, 0x38]);
        let value = protocol.decode_pid_for(second, 1, 0x38, &[0x80, 0, 0x81, 0]);
        assert_eq!(value.name, "O2_B3S1_WR_CURRENT");
        assert_eq!(
            value.value.field("lambda"),
            Some(&Value::number(1.0, "ratio"))
        );
        assert_eq!(
            value.value.field("current"),
            Some(&Value::number(1.0, "mA"))
        );
        let bus = BusSpec::obd_can(IdWidth::Bits11, 500_000);
        adapter.offer_link(MockLink::on_bus(bus.clone(), &[]));
        protocol.switch_bus(&bus).unwrap();
        assert!(protocol.scaling_cache.is_empty());
        assert!(protocol.supported_cache.is_empty());
    }

    #[test]
    fn advertised_scaling_must_be_read_successfully_before_decoding() {
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        for reply in ["414F0400", "414E04000000"] {
            let (mut protocol, link) = protocol_with(&[reply]);
            protocol
                .supported_cache
                .insert((ecu, 1), vec![0x13, 0x24, 0x4F]);
            assert!(protocol.read_pids(ecu, &[0x24]).is_err());
            assert!(protocol.scaling_cache.is_empty());
            assert_eq!(link.sent(), ["014F"]);
        }
    }

    #[test]
    fn legacy_oxygen_replies_use_each_responders_layout() {
        let link = MockLink::on_bus(BusSpec::from_cli("j1850pwm", None).unwrap(), &[]);
        let first = EcuAddress::Byte(0x10);
        let second = EcuAddress::Byte(0x28);
        link.script_functional(vec![
            ResponseFrame {
                id: 0x416B10,
                data: vec![0x41, 0x18, 0, 0xC0],
            },
            ResponseFrame {
                id: 0x416B28,
                data: vec![0x41, 0x18, 0x80, 0xFF],
            },
        ]);
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        protocol.obd_cache = Some(vec![first, second]);
        protocol
            .supported_cache
            .insert((first, 1), vec![0x13, 0x18]);
        protocol
            .supported_cache
            .insert((second, 1), vec![0x1D, 0x18]);
        let values = protocol.read_functional_pids(&[0x18]).unwrap();
        assert_eq!(values[&first][0].name, "O2_B2S1");
        assert_eq!(values[&second][0].name, "O2_B3S1");
        assert_eq!(
            values[&first][0].value.field("short_term_fuel_trim"),
            Some(&Value::number(50.0, "%"))
        );
        assert_eq!(
            values[&second][0].value.field("short_term_fuel_trim"),
            Some(&Value::Unavailable)
        );
        assert_eq!(link.functional_requests(), ["0118"]);
    }

    #[test]
    fn ford_pwm_capture_discovers_ecu_and_reads_without_vin() {
        use crate::test_support::ford_replies;
        let link = MockLink::on_bus(BusSpec::from_cli("j1850pwm", None).unwrap(), &[]);
        for request in ["0100", "0100", "0105", "010C", "03", "0101"] {
            link.script_functional(ford_replies(request));
        }
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let ecu = EcuAddress::Byte(0x10);
        assert_eq!(protocol.obd_ecus().unwrap(), [ecu]);
        let supported = protocol.supported_pids_for(ecu, 1).unwrap();
        assert!(supported.contains(&0x0C) && supported.contains(&5));
        let values = protocol.read_pids(ecu, &[5, 0x0C]).unwrap();
        assert_eq!(values[0].value, Value::number(-40.0, "°C"));
        assert_eq!(values[1].value, Value::number(0.0, "rpm"));
        let dtcs = protocol
            .read_obd_dtcs(&[ecu], false, &Cancel::default())
            .unwrap();
        assert!(dtcs[0].dtcs.is_empty());
        assert_eq!(
            link.functional_requests(),
            ["0100", "0100", "0105", "010C", "03", "0101"]
        );
        assert!(link.sent().is_empty());
    }

    #[test]
    fn silent_stored_dtcs_require_a_complete_zero_count_from_the_same_ecu() {
        use crate::test_support::ford_replies;
        for case in 0..5 {
            let link = MockLink::on_bus(BusSpec::from_cli("j1850pwm", None).unwrap(), &[]);
            link.script_functional(Vec::new());
            let mut status = ford_replies("0101");
            match case {
                0 => status.clear(),
                1 => status[0].data[2] = 1,
                2 => status[0].id = 0x416B18,
                3 => {
                    status[0].data.pop();
                }
                _ => {
                    let mut conflict = status[0].clone();
                    conflict.data[2] = 1;
                    status.push(conflict);
                }
            }
            link.script_functional(status);
            let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
            assert!(protocol
                .read_obd_dtcs(&[EcuAddress::Byte(0x10)], false, &Cancel::default())
                .is_err());
        }
    }

    #[test]
    fn zero_count_fallback_preserves_other_ecu_codes_and_does_not_apply_to_pending() {
        use crate::test_support::ford_replies;
        let link = MockLink::on_bus(BusSpec::from_cli("j1850pwm", None).unwrap(), &[]);
        link.script_functional(vec![ResponseFrame {
            id: 0x416B18,
            data: vec![0x43, 1, 0x43],
        }]);
        link.script_functional(ford_replies("0101"));
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let dtcs = protocol
            .read_obd_dtcs(
                &[EcuAddress::Byte(0x10), EcuAddress::Byte(0x18)],
                false,
                &Cancel::default(),
            )
            .unwrap();
        assert!(dtcs[0].dtcs.is_empty());
        assert_eq!(dtcs[1].dtcs[0].code, "P0143");
        link.script_functional(Vec::new());
        assert!(protocol
            .read_obd_dtcs(&[EcuAddress::Byte(0x10)], true, &Cancel::default())
            .is_err());
        assert_eq!(link.functional_requests(), ["03", "0101", "07"]);
    }

    #[test]
    fn missing_mode09_is_optional_only_while_that_legacy_ecu_still_answers() {
        use crate::test_support::ford_replies;
        for case in 0..3 {
            let link = MockLink::on_bus(BusSpec::from_cli("j1850pwm", None).unwrap(), &[]);
            link.script_functional(ford_replies("0900"));
            let mut probe = ford_replies("0100");
            match case {
                1 => probe.clear(),
                2 => probe[0].id = 0x416B18,
                _ => {}
            }
            link.script_functional(probe);
            let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
            let result = protocol.vehicle_info(&[EcuAddress::Byte(0x10)], &Cancel::default());
            if case == 0 {
                assert!(result.unwrap()[0].values.is_empty());
            } else {
                assert!(matches!(result, Err(DiagError::Timeout(_))));
            }
            assert_eq!(link.functional_requests(), ["0900", "0100"]);
        }
    }

    #[test]
    fn can_discovers_two_ecus_then_reads_dtcs_physically() {
        let (mut protocol, link) = protocol_with(&[]);
        link.script_reply(ResponseFrame {
            id: 0x18DAF101,
            data: vec![0x43, 1, 1, 0x43],
        });
        link.script_reply(ResponseFrame {
            id: 0x18DAF110,
            data: vec![0x43, 0],
        });
        link.script_functional(vec![
            ResponseFrame {
                id: 0x18DAF101,
                data: vec![0x41, 0, 0, 0, 0, 0],
            },
            ResponseFrame {
                id: 0x18DAF110,
                data: vec![0x41, 0, 0, 0, 0, 0],
            },
        ]);
        let ecus = protocol.obd_ecus().unwrap();
        let results = protocol
            .read_obd_dtcs(&ecus, false, &Cancel::default())
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].dtcs[0].code, "P0143");
        assert!(results[1].dtcs.is_empty());
        assert_eq!(link.functional_requests(), ["0100"]);
        assert_eq!(link.sent(), ["03", "03"]);
    }
    #[test]
    fn combined_pid_replies_split_and_bad_replies_fall_back() {
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        let (mut protocol, link) =
            protocol_with(&["410C1AF80D20", "410C1AF8", "410C1AF8", "410D20"]);
        let values = protocol.read_pids(ecu, &[0x0C, 0x0D]).unwrap();
        assert_eq!(values[0].value, Value::number(1726.0, "rpm"));
        assert_eq!(values[1].value, Value::number(32.0, "km/h"));
        let values = protocol.read_pids(ecu, &[0x0C, 0x0D]).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(link.sent(), ["010C0D", "010C0D", "010C", "010D"]);
    }
    #[test]
    fn legacy_buses_group_multi_message_dtcs_and_read_one_pid_per_request() {
        for name in ["iso9141", "kwp-fast", "kwp-5baud", "j1850vpw", "j1850pwm"] {
            let bus = BusSpec::from_cli(name, None).unwrap();
            let header: u32 = match name {
                "j1850pwm" => 0x416B00,
                "kwp-fast" | "kwp-5baud" => 0x83F100,
                _ => 0x486B00,
            };
            let link = MockLink::on_bus(bus, &[]);
            let frame = |address, data| ResponseFrame {
                id: header | address,
                data,
            };
            link.script_functional(vec![
                frame(0x10, vec![0x41, 0, 0, 0, 0, 0]),
                frame(0x28, vec![0x41, 0, 0, 0, 0, 0]),
            ]);
            link.script_functional(vec![
                frame(0x10, vec![0x43, 0x01, 0x43, 0, 0, 0, 0]),
                frame(0x28, vec![0x43, 0x02, 0x00]),
                frame(0x10, vec![0x43, 0x03, 0x00]),
            ]);
            link.script_functional(vec![frame(0x10, vec![0x41, 0x0C, 0x1A, 0xF8])]);
            link.script_functional(vec![frame(0x10, vec![0x41, 0x0D, 0x20])]);
            let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
            let ecus = protocol.obd_ecus().unwrap();
            assert_eq!(ecus, [EcuAddress::Byte(0x10), EcuAddress::Byte(0x28)]);
            let results = protocol
                .read_obd_dtcs(&ecus, false, &Cancel::default())
                .unwrap();
            assert_eq!(results[0].dtcs.len(), 2, "{name}");
            assert_eq!(results[1].dtcs[0].code, "P0200");
            assert_eq!(
                protocol
                    .read_pids(EcuAddress::Byte(0x10), &[0x0C, 0x0D])
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(link.functional_requests(), ["0100", "03", "010C", "010D"]);
            assert!(link.sent().is_empty());
        }
    }
    #[test]
    fn numbered_vin_messages_reassemble_in_order_and_missing_parts_fail() {
        let vin = b"1HGCM82633A004352";
        let mut padded = vec![0, 0, 0];
        padded.extend(vin);
        let mut frames: Vec<_> = padded
            .chunks(4)
            .enumerate()
            .map(|(index, part)| {
                let mut data = vec![0x49, 2, index as u8 + 1];
                data.extend(part);
                ResponseFrame { id: 0x486B10, data }
            })
            .collect();
        frames.reverse();
        let result = ObdProtocol::reassemble_mode09(2, &frames).unwrap();
        assert_eq!(&result[1..], vin);
        frames.pop();
        assert!(ObdProtocol::reassemble_mode09(2, &frames).is_err());
    }

    // ------------------------------------------------------------ requests

    #[test]
    fn accord_mode09_bitmap_and_numbered_identity_use_the_captured_format() {
        use crate::test_support::accord_replies;
        let link = MockLink::on_bus(BusSpec::from_cli("iso9141", None).unwrap(), &[]);
        for request in ["0900", "0902", "0904"] {
            link.script_functional(accord_replies(request));
        }
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let ecu = EcuAddress::Byte(9);
        assert_eq!(
            protocol.supported_pids_for(ecu, 9).unwrap(),
            (1..=8).collect::<Vec<_>>()
        );
        assert_eq!(
            protocol.supported_pids_for(ecu, 9).unwrap(),
            (1..=8).collect::<Vec<_>>()
        );
        assert_eq!(
            &protocol.read_mode09(ecu, 2).unwrap()[1..],
            b"1HGCM56805A020957"
        );
        assert_eq!(
            crate::diag::text(&protocol.read_mode09(ecu, 4).unwrap()[1..]),
            "37805-RAD-3350"
        );
        assert_eq!(link.functional_requests(), ["0900", "0902", "0904"]);
        assert!(link.sent().is_empty());
    }

    #[test]
    fn gm_vpw_capture_decodes_all_calibration_records_and_empty_dtcs() {
        use crate::test_support::gm_replies;
        let link = MockLink::on_bus(BusSpec::from_cli("j1850vpw", None).unwrap(), &[]);
        for request in ["0900", "0902", "0904", "0906", "03", "0101", "07"] {
            link.script_functional(gm_replies(request));
        }
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let ecu = EcuAddress::Byte(0x10);
        let info = protocol.vehicle_info(&[ecu], &Cancel::default()).unwrap();
        let values = &info[0].values;
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].value, Value::Text("1GNFK16Z32J211066".into()));
        assert_eq!(
            values[1].value,
            Value::Text(
                "12216125, 09357732, 09358972, 09358200, 16264431, 16264614, 09373647, 09359144"
                    .into()
            )
        );
        assert_eq!(
            values[2].value,
            Value::Text(
                "00006553, 0000F822, 00001A48, 0000822A, 00009670, 0000E7CB, 00003D06, 00002F57"
                    .into()
            )
        );
        for pending in [false, true] {
            let dtcs = protocol
                .read_obd_dtcs(&[ecu], pending, &Cancel::default())
                .unwrap();
            assert!(dtcs[0].dtcs.is_empty());
        }
        assert_eq!(
            link.functional_requests(),
            ["0900", "0902", "0904", "0906", "03", "0101", "07"]
        );
        assert!(link.sent().is_empty());
    }

    #[test]
    fn gm_numbered_calibration_records_require_complete_ordered_sequences() {
        use crate::test_support::gm_replies;
        for pid in [4, 6] {
            let mut frames = gm_replies(&format!("09{pid:02X}"));
            let expected = ObdProtocol::reassemble_mode09(pid, &frames).unwrap();
            assert_eq!(expected[0], 8);
            frames.reverse();
            frames.push(frames[0].clone());
            assert_eq!(
                ObdProtocol::reassemble_mode09(pid, &frames).unwrap(),
                expected
            );
            frames.pop();
            let mut short = frames.clone();
            short[0].data.pop();
            assert!(ObdProtocol::reassemble_mode09(pid, &short).is_err());
            frames.remove(1);
            assert!(ObdProtocol::reassemble_mode09(pid, &frames).is_err());
        }
    }

    #[test]
    fn legacy_mode09_rejects_bad_counts_and_incomplete_calibration() {
        use crate::test_support::accord_replies;
        for data in [
            vec![0x49, 0, 1, 0xFF, 0, 0],
            vec![0x49, 0, 2, 0xFF, 0, 0, 0],
        ] {
            let link = MockLink::on_bus(BusSpec::from_cli("iso9141", None).unwrap(), &[]);
            link.script_functional(vec![ResponseFrame { id: 0x486B09, data }]);
            let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
            assert!(protocol.supported_pids_for(EcuAddress::Byte(9), 9).is_err());
        }
        let mut replies = accord_replies("0904");
        replies.pop();
        assert!(ObdProtocol::reassemble_mode09(4, &replies).is_err());
        let mut replies = accord_replies("0902");
        let mut conflict = replies[0].clone();
        conflict.data[3] = 1;
        replies.push(conflict);
        assert!(ObdProtocol::reassemble_mode09(2, &replies).is_err());
    }

    #[test]
    fn mode09_reads_select_the_requested_responder() {
        use crate::test_support::accord_replies;
        let link = MockLink::on_bus(BusSpec::from_cli("iso9141", None).unwrap(), &[]);
        let mut replies = accord_replies("0902");
        let mut other = replies[0].clone();
        other.id = 0x486B10;
        other.data[3] = 0xFF;
        replies.push(other);
        link.script_functional(replies);
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        assert_eq!(
            &protocol.read_mode09(EcuAddress::Byte(9), 2).unwrap()[1..],
            b"1HGCM56805A020957"
        );
    }

    #[test]
    fn functional_pid_reads_reject_truncated_values() {
        let link = MockLink::on_bus(BusSpec::from_cli("iso9141", None).unwrap(), &[]);
        link.script_functional(vec![ResponseFrame {
            id: 0x486B09,
            data: vec![0x41, 0x0C, 0x12],
        }]);
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        assert!(protocol.read_functional_pids(&[0x0C]).is_err());
    }

    #[test]
    fn can_obd_fallback_is_cached_and_preserves_single_pid_fallback() {
        let ecu = EcuAddress::Can {
            request: 0x7E0,
            response: 0x7E8,
        };
        let link = MockLink::on_bus(BusSpec::obd_can(IdWidth::Bits11, 500_000), &[]);
        for data in [
            "410000180000",
            "410C1AF8",
            "410C1AF8",
            "410D09",
            "410C1AF8",
            "410D09",
        ] {
            link.script_functional(vec![ResponseFrame {
                id: 0x7E8,
                data: crate::utils::hex_to_bytes(data).unwrap(),
            }]);
        }
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        assert_eq!(protocol.supported_pids_for(ecu, 1).unwrap(), [0x0C, 0x0D]);
        for _ in 0..2 {
            let values = protocol.read_pids(ecu, &[0x0C, 0x0D]).unwrap();
            assert_eq!(values[0].value, Value::number(1726.0, "rpm"));
            assert_eq!(values[1].value, Value::number(9.0, "km/h"));
        }
        assert_eq!(link.sent(), ["0100"]);
        assert_eq!(
            link.functional_requests(),
            ["0100", "010C0D", "010C", "010D", "010C", "010D"]
        );
    }

    #[test]
    fn can_obd_fallback_matches_the_ecu_service_and_pid() {
        let link = MockLink::on_bus(
            BusSpec::obd_can(IdWidth::Bits11, 500_000),
            &["4120FFFFFFFF"],
        );
        link.script_functional(vec![
            ResponseFrame {
                id: 0x7E9,
                data: vec![0x41, 0x0C, 1, 0],
            },
            ResponseFrame {
                id: 0x7E8,
                data: vec![0x41, 0x0D, 99],
            },
            ResponseFrame {
                id: 0x7E8,
                data: vec![0x7F, 9, 0x12],
            },
            ResponseFrame {
                id: 0x7E8,
                data: vec![0x41, 0x0C, 0x1A, 0xF8],
            },
        ]);
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        protocol.set_header_id(0x7E0).unwrap();
        assert_eq!(
            protocol.read_pid(1, 0x0C).unwrap().value,
            Value::number(1726.0, "rpm")
        );
        assert_eq!(link.sent(), ["010C"]);
        assert_eq!(link.functional_requests(), ["010C"]);
    }

    #[test]
    fn can_obd_fallback_keeps_full_mode09_replies_and_physical_manufacturer_requests() {
        let (mut protocol, link) = protocol_with(&[]);
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        let mut vin = vec![0x49, 2, 1];
        vin.extend_from_slice(b"1HGCM56805A020957");
        link.script_functional(vec![ResponseFrame {
            id: 0x18DAF101,
            data: vin,
        }]);
        assert_eq!(
            &protocol.read_mode09(ecu, 2).unwrap()[1..],
            b"1HGCM56805A020957"
        );
        for request in [&[0x22, 0xF1, 0x90][..], &[4], &[8, 0], &[1; 8]] {
            assert!(protocol.obd_transfer(request).is_err());
        }
        assert_eq!(link.functional_requests(), ["0902"]);
        assert_eq!(
            link.sent(),
            ["0902", "22F190", "04", "0800", "0101010101010101"]
        );
    }

    #[test]
    fn can_obd_fallback_is_scoped_to_standard_ecus_and_cleared_on_bus_change() {
        let bus = BusSpec::obd_can(IdWidth::Bits11, 500_000);
        let link = MockLink::on_bus(bus.clone(), &[]);
        let adapter = FakeAdapter::new();
        let mut protocol = ObdProtocol::new(Rc::new(adapter.clone()), link.boxed());
        for (request, response) in [(0x7E0, 0x7E8), (0x7E1, 0x7E9)] {
            protocol.set_header_id(request).unwrap();
            link.script_functional(vec![ResponseFrame {
                id: response,
                data: vec![0x41, 0x0D, 9],
            }]);
            protocol.read_pid(1, 0x0D).unwrap();
        }
        assert_eq!(link.sent(), ["010D", "010D"]);
        assert_eq!(protocol.functional_obd.len(), 2);
        protocol
            .set_ecu(EcuAddress::Can {
                request: 0x720,
                response: 0x728,
            })
            .unwrap();
        assert!(protocol.read_pid(1, 0x0D).is_err());
        assert_eq!(link.functional_requests(), ["010D", "010D"]);
        let replacement = MockLink::on_bus(bus.clone(), &[]);
        adapter.offer_link(replacement);
        protocol.switch_bus(&bus).unwrap();
        assert!(protocol.functional_obd.is_empty());
        assert!(link.is_dropped());
    }

    #[test]
    fn can_obd_negative_or_missing_replies_do_not_enable_functional_routing() {
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        let (mut protocol, link) = protocol_with(&["7F0112"]);
        protocol.set_ecu(ecu).unwrap();
        assert!(protocol.read_pid(1, 0x0C).is_err());
        assert!(link.functional_requests().is_empty());
        for replies in [
            vec![],
            vec![ResponseFrame {
                id: 0x18DAF101,
                data: vec![0x7F, 1, 0x12],
            }],
            vec![ResponseFrame {
                id: 0x18DAF102,
                data: vec![0x41, 0x0C, 0, 0],
            }],
        ] {
            link.script_functional(replies);
            assert!(protocol.read_pid(1, 0x0C).is_err());
            assert!(protocol.functional_obd.is_empty());
        }
        assert_eq!(link.sent().len(), 4);
        assert_eq!(link.functional_requests().len(), 3);
    }

    #[test]
    fn transfer_sends_the_payload_and_returns_the_reply() {
        let (mut proto, link) = protocol_with(&["4100BE3FA813"]);

        let reply = proto.transfer(&[0x01, 0x00]).unwrap();

        assert_eq!(link.sent(), vec!["0100"]);
        assert_eq!(reply.id, 0x18DAF101);
        assert_eq!(reply.data, vec![0x41, 0x00, 0xBE, 0x3F, 0xA8, 0x13]);
    }

    #[test]
    fn a_missing_reply_is_a_timeout_naming_the_request() {
        let (mut proto, _link) = protocol_with(&[]);

        let err = proto
            .transfer_with_timeout(&[0x22, 0xF1, 0x90], Some(Duration::from_millis(10)))
            .unwrap_err();

        assert!(
            matches!(&err, DiagError::Timeout(m) if m.contains("22F190")),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_empty_request() {
        let (mut proto, link) = protocol_with(&[]);
        assert!(proto.transfer(&[]).is_err());
        assert!(link.sent().is_empty(), "nothing may go out");
    }

    #[test]
    fn rejects_a_request_over_the_bus_limit() {
        let (mut proto, link) = protocol_with(&[]);
        let err = proto.transfer(&vec![0x36; 4096]).unwrap_err();
        assert!(err.to_string().contains("4095"), "{err}");
        assert!(link.sent().is_empty());
    }

    #[test]
    fn collect_responses_stops_when_the_bus_goes_quiet() {
        let (mut proto, link) = protocol_with(&["4302014302", "4300"]);

        let replies = proto
            .collect_responses(&[0x03], Duration::from_millis(5), Duration::from_secs(1))
            .unwrap();

        assert_eq!(link.sent(), vec!["03"]);
        assert_eq!(replies.len(), 2);
    }

    // -------------------------------------------------------------- DTCs

    #[test]
    fn decodes_the_dtc_prefix_from_the_top_two_bits() {
        // Top two bits pick the system letter, the remaining 14 bits the number.
        assert_eq!(
            ObdProtocol::decode_dtc_bytes(&[0x01, 0x43]).unwrap(),
            "P0143"
        );
        assert_eq!(
            ObdProtocol::decode_dtc_bytes(&[0x41, 0x43]).unwrap(),
            "C0143"
        );
        assert_eq!(
            ObdProtocol::decode_dtc_bytes(&[0x81, 0x43]).unwrap(),
            "B0143"
        );
        assert_eq!(
            ObdProtocol::decode_dtc_bytes(&[0xC1, 0x43]).unwrap(),
            "U0143"
        );
        assert!(ObdProtocol::decode_dtc_bytes(&[0x01]).is_err());
    }

    #[test]
    fn read_dtc_requests_mode_03_and_decodes_each_code() {
        let (mut proto, link) = protocol_with(&["430201430200"]);

        let dtcs = proto.read_dtc(false).unwrap();

        assert_eq!(link.sent(), vec!["03"], "mode 03 = stored DTCs");
        assert_eq!(
            dtcs.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(),
            vec!["P0143", "P0200"]
        );
        assert_eq!(dtcs[1].description, "Injector Circuit/Open");
    }

    #[test]
    fn read_dtc_pending_uses_mode_07_and_handles_an_empty_list() {
        let (mut proto, link) = protocol_with(&["4700"]);

        let dtcs = proto.read_dtc(true).unwrap();

        assert_eq!(link.sent(), vec!["07"], "mode 07 = pending DTCs");
        assert!(dtcs.is_empty());
    }

    /// CAN frames are padded out to 8 bytes, often with 0x55 or 0xAA rather
    /// than zeros. Those bytes sit past the announced DTC count and must never
    /// be decoded as extra trouble codes.
    #[test]
    fn read_dtc_ignores_padding_after_the_announced_codes() {
        let (mut proto, _link) = protocol_with(&["430101435555"]);

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
        let (mut proto, _link) = protocol_with(&["430201430000"]);

        let err = proto.read_dtc(false).unwrap_err();

        assert!(
            matches!(&err, DiagError::InvalidResponse(m) if m.contains("only 1 could be decoded")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_dtc_rejects_a_truncated_dtc_list() {
        // Claims 2 DTCs but only carries one 2-byte code.
        let (mut proto, _link) = protocol_with(&["43020143"]);

        assert!(proto.read_dtc(false).is_err());
    }

    /// Legacy buses send no count byte; every non-zero pair is a code.
    #[test]
    fn dtc_replies_without_a_count_byte_decode_every_pair() {
        let dtcs = ObdProtocol::decode_dtc_response(
            0x03,
            &[0x43, 0x01, 0x43, 0x02, 0x00, 0x00, 0x00],
            false,
        )
        .unwrap();
        assert_eq!(
            dtcs.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(),
            vec!["P0143", "P0200"]
        );
    }

    // ------------------------------------------------------- supported PID map

    #[test]
    fn kwp_5baud_capture_discovers_ecu_and_reads_live_data() {
        use crate::test_support::kwp_emulator_replies;
        let link = MockLink::on_bus(BusSpec::from_cli("kwp-5baud", None).unwrap(), &[]);
        for request in [
            "0100", "0100", "0120", "0140", "0160", "0105", "010C", "010D",
        ] {
            link.script_functional(kwp_emulator_replies(request));
        }
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let ecu = EcuAddress::Byte(0x11);
        assert_eq!(protocol.obd_ecus().unwrap(), [ecu]);
        assert_eq!(protocol.supported_pids_for(ecu, 1).unwrap().len(), 93);
        let values = protocol.read_pids(ecu, &[5, 0x0C, 0x0D]).unwrap();
        assert_eq!(values[0].value, Value::number(-40.0, "°C"));
        assert_eq!(values[1].value, Value::number(0.0, "rpm"));
        assert_eq!(values[2].value, Value::number(0.0, "km/h"));
        assert!(link.sent().is_empty());
    }

    #[test]
    fn legacy_discovery_keeps_confirmed_groups_when_a_later_group_is_missing() {
        use crate::test_support::RecordingEvents;
        for (bus, response_id) in [
            ("iso9141", 0x486B10),
            ("kwp-5baud", 0x86F110),
            ("j1850pwm", 0x416B10),
            ("j1850vpw", 0x486B10),
        ] {
            for unrelated_replies in [false, true] {
                let link = MockLink::on_bus(BusSpec::from_cli(bus, None).unwrap(), &[]);
                for base in [0, 0x20, 0x40] {
                    link.script_functional(vec![ResponseFrame {
                        id: response_id,
                        data: vec![0x41, base, 0xFF, 0xFF, 0xFF, 0xFF],
                    }]);
                }
                link.script_functional(if unrelated_replies {
                    vec![
                        ResponseFrame {
                            id: response_id,
                            data: vec![0x41, 0, 0xFF, 0xFF, 0xFF, 0xFF],
                        },
                        ResponseFrame {
                            id: response_id + 1,
                            data: vec![0x41, 0x60, 0xFF, 0xFF, 0xFF, 0xFF],
                        },
                    ]
                } else {
                    Vec::new()
                });
                link.script_functional(vec![ResponseFrame {
                    id: response_id,
                    data: vec![0x41, 0x0C, 0, 9],
                }]);
                let events = RecordingEvents::default();
                let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
                protocol.set_events(Box::new(events.clone()));
                let ecu = EcuAddress::Byte(0x10);
                let expected: Vec<u8> = (1..0x60).filter(|pid| pid % 0x20 != 0).collect();
                for _ in 0..2 {
                    assert_eq!(protocol.supported_pids_for(ecu, 1).unwrap(), expected);
                }
                assert_eq!(
                    protocol.read_pids(ecu, &[0x0C]).unwrap()[0].value,
                    Value::number(2.25, "rpm")
                );
                assert_eq!(
                    link.functional_requests(),
                    ["0100", "0120", "0140", "0160", "010C"]
                );
                let events = events.0.borrow();
                let warnings: Vec<_> = events
                    .iter()
                    .filter(|event| event.starts_with("warning:"))
                    .collect();
                assert_eq!(warnings.len(), 1);
                assert!(warnings[0].contains("0x10 for 0x60"));
            }
        }
    }

    #[test]
    fn missing_first_legacy_bitmap_fails_without_caching() {
        let link = MockLink::on_bus(BusSpec::from_cli("iso9141", None).unwrap(), &[]);
        link.script_functional(Vec::new());
        link.script_functional(vec![ResponseFrame {
            id: 0x486B10,
            data: vec![0x41, 0, 0, 0x10, 0, 0],
        }]);
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let ecu = EcuAddress::Byte(0x10);
        assert!(matches!(
            protocol.supported_pids_for(ecu, 1),
            Err(DiagError::Timeout(_))
        ));
        assert!(protocol.supported_cache.is_empty());
        assert_eq!(protocol.supported_pids_for(ecu, 1).unwrap(), [0x0C]);
        assert_eq!(link.functional_requests(), ["0100", "0100"]);
    }

    #[test]
    fn malformed_legacy_bitmaps_fail_even_after_a_valid_group() {
        for base in [0, 0x20] {
            for (sid, data) in [
                (1, vec![0x41, base, 0, 0, 0]),
                (9, vec![0x49, base, 1, 0, 0, 0]),
                (9, vec![0x49, base, 2, 0, 0, 0, 0]),
            ] {
                let link = MockLink::on_bus(BusSpec::from_cli("iso9141", None).unwrap(), &[]);
                if base != 0 {
                    let data = if sid == 9 {
                        vec![0x49, 0, 1, 0x80, 0, 0, 1]
                    } else {
                        vec![0x41, 0, 0x80, 0, 0, 1]
                    };
                    link.script_functional(vec![ResponseFrame { id: 0x486B10, data }]);
                }
                link.script_functional(vec![ResponseFrame { id: 0x486B10, data }]);
                let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
                assert!(matches!(
                    protocol.supported_pids_for(EcuAddress::Byte(0x10), sid),
                    Err(DiagError::InvalidResponse(_))
                ));
                assert!(protocol.supported_cache.is_empty());
            }
        }
    }

    /// The supported-PID bitmap is MSB-first starting at `base + 1`, and the
    /// very last bit means "ask again at base + 0x20" rather than "PID 0x20".
    #[test]
    fn walks_the_supported_pid_bitmap_across_groups() {
        let (mut proto, link) = protocol_with(&[
            // 41 00 with bit 0 (PID 0x01) and the continuation bit set.
            "410080000001",
            // 41 20 with bit 1 (PID 0x22) only, no continuation.
            "412040000000",
        ]);

        let pids = proto.read_supported_pids(0x01).unwrap();

        assert_eq!(link.sent(), vec!["0100", "0120"]);
        assert_eq!(pids, vec![0x01, 0x22]);
    }

    #[test]
    fn supported_pid_query_fails_loudly_on_a_mismatched_echo() {
        // Responds about PID 0x20 when 0x00 was asked for.
        let (mut proto, _link) = protocol_with(&["412080000000"]);

        assert!(proto.read_supported_pids(0x01).is_err());
    }

    // ------------------------------------------------ adapter and addressing

    /// Recovery for a run killed mid-flash. Every pin J2534 can drive is
    /// cleared, which keeps this free of per-vehicle knowledge; pins the
    /// adapter lacks, or another client holds, don't stop the run.
    #[test]
    fn clearing_programming_voltage_covers_every_pin_and_tolerates_refusals() {
        let adapter = FakeAdapter::new();
        adapter.refuse_pin(9, crate::j2534::consts::ERR_PIN_NOT_SUPPORTED);
        adapter.refuse_pin(13, crate::j2534::consts::ERR_PIN_IN_USE);

        clear_all_programming_voltage(&adapter);

        assert_eq!(
            adapter.voltage_calls(),
            PROGRAMMING_VOLTAGE_PINS
                .iter()
                .map(|&pin| (pin, VOLTAGE_PIN_OFF))
                .collect::<Vec<_>>()
        );
    }

    /// The flash path compares the header it is about to use against the one
    /// already in effect, so the protocol has to remember what it set.
    #[test]
    fn set_header_id_targets_the_link_and_remembers_it() {
        let (mut proto, link) = protocol_with(&[]);

        assert_eq!(proto.header_id(), None, "nothing set before connecting");
        proto.set_header_id(0x18DA01F1).unwrap();

        assert_eq!(link.targets(), vec![0x18DA01F1]);
        assert_eq!(proto.header_id(), Some(0x18DA01F1));
    }

    /// A broadcast goes to the bus's functional address, not the current
    /// target, and leaves the target alone.
    #[test]
    fn broadcast_returns_every_reply_and_keeps_the_target() {
        let (mut proto, link) = protocol_with(&[]);
        proto.set_header_id(0x18DA01F1).unwrap();
        link.script_functional(vec![
            ResponseFrame {
                id: 0x18DAF100,
                data: vec![0x41, 0x00, 0x9B, 0xBE, 0x3F, 0x80],
            },
            ResponseFrame {
                id: 0x18DAF101,
                data: vec![0x41, 0x00, 0x8F, 0x3F, 0x80, 0xAB],
            },
        ]);

        let responses = proto.broadcast(&[0x01, 0x00], None).unwrap();

        assert_eq!(link.functional_requests(), vec!["0100"]);
        assert_eq!(proto.header_id(), Some(0x18DA01F1));
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[1].data, vec![0x41, 0x00, 0x8F, 0x3F, 0x80, 0xAB]);
    }

    #[test]
    fn read_battery_returns_millivolts() {
        let (proto, _link) = protocol_with(&[]);
        assert_eq!(proto.read_battery_mv().unwrap(), 12400);
    }

    #[test]
    fn addressing_follows_the_bus() {
        let (proto, _link) = protocol_with(&[]);
        assert_eq!(proto.addressing(), Addressing::Can29Bit);
    }

    /// Switching bus closes the old link before the new one opens, and forgets
    /// the target, which belonged to the old bus.
    #[test]
    fn switch_bus_reconnects_through_the_adapter() {
        let adapter = FakeAdapter::new();
        let first = MockLink::new(&[]);
        let mut proto = ObdProtocol::new(Rc::new(adapter.clone()), first.boxed());
        proto.set_header_id(0x18DA01F1).unwrap();
        let slow = BusSpec::obd_can(IdWidth::Bits11, 250_000);
        adapter.offer_link(MockLink::on_bus(slow.clone(), &[]));

        proto.switch_bus(&slow).unwrap();

        assert_eq!(adapter.connects(), vec![slow.clone()]);
        assert_eq!(proto.bus(), &slow);
        assert_eq!(proto.header_id(), None);
        assert!(first.is_dropped(), "the old bus must be closed first");
    }
}
