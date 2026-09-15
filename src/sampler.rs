//! One polling engine for terminal tables, CSV and future GUI sinks.
use crate::{
    datalog::{Extract, Field, LogOptions},
    events::{Cancel, Event},
    obd::addressing::EcuAddress,
    value::Value,
    DiagError, ObdProtocol,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
pub enum Signal {
    ObdPid {
        ecu: EcuAddress,
        pid: u8,
        field: Option<String>,
    },
    Did {
        ecu: EcuAddress,
        field: Field,
        diag: String,
    },
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct SignalInfo {
    pub name: String,
    pub signal: Signal,
    pub bus: Option<crate::link::BusSpec>,
    pub unit: String,
    pub scale: f64,
    pub offset: f64,
}
impl SignalInfo {
    pub fn did(ecu: EcuAddress, field: Field) -> Self {
        Self {
            name: field.name.clone(),
            signal: Signal::Did {
                ecu,
                field,
                diag: "uds".into(),
            },
            bus: None,
            unit: String::new(),
            scale: 1.0,
            offset: 0.0,
        }
    }
    pub fn pid(ecu: EcuAddress, pid: u8) -> Self {
        let def = crate::obd::pids::get_pid_definition(1, pid);
        Self {
            name: def
                .map(|def| def.name.to_string())
                .unwrap_or_else(|| format!("PID_{pid:02X}")),
            signal: Signal::ObdPid {
                ecu,
                pid,
                field: None,
            },
            bus: None,
            unit: def.map(|d| d.unit).unwrap_or("").into(),
            scale: 1.0,
            offset: 0.0,
        }
    }

    /// Expand multi-measurement PIDs into numeric columns sharing one request.
    pub fn pid_fields(
        ecu: EcuAddress,
        pid: u8,
        layout: crate::obd::pids::oxygen::Layout,
    ) -> Vec<Self> {
        use crate::obd::pids::oxygen;
        let mut base = Self::pid(ecu, pid);
        if let Some(name) = oxygen::name(pid, layout) {
            base.name = name;
        }
        let fields = oxygen::fields(pid);
        if fields.is_empty() {
            return vec![base];
        }
        fields
            .iter()
            .map(|field| Self {
                name: format!("{}{}", base.name, field.suffix),
                signal: Signal::ObdPid {
                    ecu,
                    pid,
                    field: Some(field.key.into()),
                },
                unit: field.unit.into(),
                ..base.clone()
            })
            .collect()
    }
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct Sample {
    pub unix_ms: u128,
    pub elapsed_ms: u128,
    pub values: Vec<Option<Value>>,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct SampleStats {
    pub samples: u64,
    pub elapsed_ms: u128,
    pub rate: f64,
}
pub trait SampleSink {
    fn begin(&mut self, signals: &[SignalInfo]) -> Result<(), DiagError>;
    fn sample(&mut self, sample: &Sample) -> Result<(), DiagError>;
}
fn recoverable(error: &DiagError) -> bool {
    matches!(
        error,
        DiagError::Timeout(_)
            | DiagError::InvalidResponse(_)
            | DiagError::UdsNegativeResponse { .. }
    ) || matches!(error, DiagError::PortError(e) if e.kind() == std::io::ErrorKind::TimedOut)
}
fn extract(field: &Field, bytes: &[u8]) -> Option<Value> {
    match field.extract {
        Extract::Raw => Some(Value::Text(crate::utils::bytes_to_hex(bytes))),
        Extract::Unsigned { offset, len } => {
            let slice = bytes.get(offset..offset.checked_add(len)?)?;
            Some(Value::Unsigned {
                value: slice
                    .iter()
                    .fold(0u64, |value, byte| (value << 8) | *byte as u64),
                unit: String::new(),
            })
        }
        Extract::Bit { offset, bit } => Some(Value::Unsigned {
            value: ((*bytes.get(offset)? >> bit) & 1) as u64,
            unit: String::new(),
        }),
    }
}
pub fn run(
    protocol: &mut ObdProtocol,
    signals: &[SignalInfo],
    sink: &mut dyn SampleSink,
    options: &LogOptions,
    cancel: &Cancel,
) -> Result<SampleStats, DiagError> {
    if signals.is_empty() {
        return Err(DiagError::InvalidCommand(
            "Select at least one signal".into(),
        ));
    }
    for signal in signals {
        if !signal.scale.is_finite() || !signal.offset.is_finite() {
            return Err(DiagError::InvalidCommand(format!(
                "Signal {} has a non-finite scale or offset",
                signal.name
            )));
        }
    }
    sink.begin(signals)?;
    let start = Instant::now();
    let mut samples = 0;
    let mut warned = BTreeSet::new();
    let initial_bus = protocol.bus().clone();
    loop {
        if cancel.is_cancelled()
            || options.samples.is_some_and(|limit| samples >= limit)
            || options
                .duration
                .is_some_and(|limit| start.elapsed() >= limit)
        {
            break;
        }
        let sample_start = Instant::now();
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let elapsed_ms = start.elapsed().as_millis();
        let mut did_values: BTreeMap<
            (crate::link::BusSpec, EcuAddress, String, u16),
            Option<Vec<u8>>,
        > = BTreeMap::new();
        let mut pid_groups: BTreeMap<(crate::link::BusSpec, EcuAddress), Vec<u8>> = BTreeMap::new();
        for signal in signals {
            if let Signal::ObdPid { ecu, pid, .. } = signal.signal {
                let group = pid_groups
                    .entry((
                        signal.bus.clone().unwrap_or_else(|| initial_bus.clone()),
                        ecu,
                    ))
                    .or_default();
                if !group.contains(&pid) {
                    group.push(pid);
                }
            }
        }
        let mut pid_values = BTreeMap::new();
        let buses: BTreeSet<_> = pid_groups.keys().map(|(bus, _)| bus.clone()).collect();
        for bus in buses {
            if protocol.bus() != &bus {
                protocol.switch_bus(&bus)?;
            }
            if !bus.is_can() {
                let pids: Vec<_> = pid_groups
                    .iter()
                    .filter(|((b, _), _)| b == &bus)
                    .flat_map(|(_, pids)| pids)
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                for pid in pids {
                    match protocol.read_functional_pids(&[pid]) {
                        Ok(values) => {
                            for (ecu, values) in values {
                                for value in values {
                                    pid_values.insert((bus.clone(), ecu, value.pid), value.value);
                                }
                            }
                        }
                        Err(e) if recoverable(&e) => {
                            if warned.insert(format!("pid:{bus}:{pid:02X}")) {
                                protocol.emit(Event::Warning(&format!(
                                    "PID 0x{pid:02X} read failed, sampling empty cells for this PID: {e}"
                                )));
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                continue;
            }
            for ((b, ecu), pids) in &pid_groups {
                if b != &bus {
                    continue;
                }
                match protocol.read_pids(*ecu, pids) {
                    Ok(values) => {
                        for value in values {
                            pid_values.insert((bus.clone(), *ecu, value.pid), value.value);
                        }
                    }
                    Err(e) if recoverable(&e) => {
                        if warned.insert(format!("pid:{bus}:{ecu}")) {
                            protocol.emit(Event::Warning(&format!(
                                "PID read from {ecu} failed, sampling empty cells: {e}"
                            )));
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        let mut values = Vec::new();
        for signal in signals {
            let bus = signal.bus.clone().unwrap_or_else(|| initial_bus.clone());
            let value = match &signal.signal {
                Signal::ObdPid { ecu, pid, field } => pid_values
                    .get(&(bus.clone(), *ecu, *pid))
                    .and_then(|value| match field {
                        Some(field) => value.field(field).cloned(),
                        None => Some(value.clone()),
                    }),
                Signal::Did { ecu, field, diag } => {
                    let key = (bus.clone(), *ecu, diag.clone(), field.did);
                    if !did_values.contains_key(&key) {
                        if protocol.bus() != &bus {
                            protocol.switch_bus(&bus)?;
                        }
                        protocol.set_ecu(*ecu)?;
                        let read = crate::diag::services(diag, protocol)?
                            .read_data(crate::diag::DataId::Common(field.did));
                        let bytes = match read {
                            Ok(bytes) => {
                                if warned.remove(&format!("did:{ecu}:{:04X}", field.did)) {
                                    protocol.emit(Event::Warning(&format!(
                                        "DID 0x{:04X} is reading again",
                                        field.did
                                    )));
                                }
                                Some(bytes)
                            }
                            Err(e) if recoverable(&e) => {
                                if warned.insert(format!("did:{ecu}:{:04X}", field.did)) {
                                    protocol.emit(Event::Warning(&format!(
                                        "DID 0x{:04X} failed, sampling empty cells: {e}",
                                        field.did
                                    )));
                                }
                                None
                            }
                            Err(e) => return Err(e),
                        };
                        did_values.insert(key.clone(), bytes);
                    }
                    did_values[&key].as_ref().and_then(|bytes| {
                        let value = extract(field, bytes);
                        if value.is_none() && warned.insert(format!("field:{}", signal.name)) {
                            protocol.emit(Event::Warning(&format!(
                                "Field '{}' is past the end of DID 0x{:04X} ({} bytes)",
                                signal.name,
                                field.did,
                                bytes.len()
                            )));
                        }
                        value
                    })
                }
            };
            let value = value.map(|value| {
                if signal.scale == 1.0 && signal.offset == 0.0 {
                    return value.with_unit(&signal.unit);
                }
                match value {
                    Value::Number { value, .. } => {
                        Value::number(value * signal.scale + signal.offset, &signal.unit)
                    }
                    Value::Unsigned { value, .. } => {
                        Value::number(value as f64 * signal.scale + signal.offset, &signal.unit)
                    }
                    other => other,
                }
            });
            values.push(value);
        }
        sink.sample(&Sample {
            unix_ms,
            elapsed_ms,
            values,
        })?;
        samples += 1;
        protocol.emit(Event::Progress {
            done: samples,
            total: options.samples.unwrap_or(0),
            unit: crate::events::ProgressUnit::Items,
        });
        if options.samples.is_some_and(|limit| samples >= limit) {
            break;
        }
        if let Some(left) = options.interval.checked_sub(sample_start.elapsed()) {
            let left = options
                .duration
                .map(|limit| left.min(limit.saturating_sub(start.elapsed())))
                .unwrap_or(left);
            cancel.wait(left);
        }
    }
    Ok(SampleStats {
        samples,
        elapsed_ms: start.elapsed().as_millis(),
        rate: if start.elapsed().is_zero() {
            0.0
        } else {
            samples as f64 / start.elapsed().as_secs_f64()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::protocol_with;
    use std::time::Duration;

    #[test]
    fn oxygen_csv_has_numeric_columns_and_reads_each_pid_once() {
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        let (mut protocol, link) = protocol_with(&[
            "410000002110", // PIDs 13, 18, 1C.
            "411800C0",
            "411880FF",
        ]);
        let signals = SignalInfo::pid_fields(ecu, 0x18, crate::obd::pids::oxygen::Layout::TwoBanks);
        let mut out = Vec::new();
        run(
            &mut protocol,
            &signals,
            &mut crate::datalog::CsvSink { out: &mut out },
            &LogOptions {
                interval: Duration::ZERO,
                samples: Some(2),
                duration: None,
            },
            &Cancel::default(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let rows: Vec<_> = text
            .lines()
            .map(|line| line.splitn(3, ',').nth(2).unwrap())
            .collect();
        assert_eq!(rows, ["O2_B2S1,O2_B2S1_STFT", "0,50", "0.64,"]);
        assert_eq!(link.sent(), ["0100", "0118", "0118"]);
    }
    #[test]
    fn cancelling_a_sink_finishes_the_sample_and_flushes_exact_integer_values() {
        struct Sink {
            rows: usize,
            cancel: Cancel,
            values: Vec<Option<Value>>,
        }
        impl SampleSink for Sink {
            fn begin(&mut self, _: &[SignalInfo]) -> Result<(), DiagError> {
                Ok(())
            }
            fn sample(&mut self, sample: &Sample) -> Result<(), DiagError> {
                self.rows += 1;
                self.values = sample.values.clone();
                self.cancel.cancel();
                Ok(())
            }
        }
        let (mut protocol, link) = protocol_with(&["622610FFFFFFFFFFFFFFFF"]);
        let cancel = Cancel::default();
        let mut sink = Sink {
            rows: 0,
            cancel: cancel.clone(),
            values: Vec::new(),
        };
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        let signal = SignalInfo::did(
            ecu,
            crate::datalog::parse_field("counter=0x2610:0:8").unwrap(),
        );
        let stats = run(
            &mut protocol,
            &[signal],
            &mut sink,
            &LogOptions {
                interval: std::time::Duration::ZERO,
                samples: None,
                duration: None,
            },
            &cancel,
        )
        .unwrap();
        assert_eq!(stats.samples, 1);
        assert_eq!(sink.rows, 1);
        assert_eq!(
            sink.values,
            vec![Some(Value::Unsigned {
                value: u64::MAX,
                unit: String::new()
            })]
        );
        assert_eq!(link.sent(), ["222610"]);
    }
    #[test]
    fn legacy_sampling_preserves_valid_pids_and_retries_malformed_values() {
        use crate::link::{BusSpec, ResponseFrame};
        use crate::test_support::{FakeAdapter, MockLink, RecordingEvents};
        let link = MockLink::on_bus(BusSpec::from_cli("iso9141", None).unwrap(), &[]);
        for data in [
            "410C0009",     // RPM is valid even when later PIDs fail.
            "410D",         // Truncated speed.
            "410000180001", // ECU discovery.
            "410000180001", // Supported groups include PID 24 and scaling PID 4F.
            "412010000001",
            "414000020000",
            "414F00",   // Truncated scaling must not produce a wideband value.
            "410C000C", // Next sample retries failed PIDs.
            "410DC4",
            "414F00000000",
            "412480002000",
        ] {
            link.script_functional(vec![ResponseFrame {
                id: 0x486B10,
                data: crate::utils::hex_to_bytes(data).unwrap(),
            }]);
        }
        let events = RecordingEvents::default();
        let mut protocol = ObdProtocol::new(std::rc::Rc::new(FakeAdapter::new()), link.boxed());
        protocol.set_events(Box::new(events.clone()));
        let ecu = EcuAddress::Byte(0x10);
        let mut signals = vec![SignalInfo::pid(ecu, 0x0C), SignalInfo::pid(ecu, 0x0D)];
        signals.extend(SignalInfo::pid_fields(
            ecu,
            0x24,
            crate::obd::pids::oxygen::Layout::Unknown,
        ));
        let mut out = Vec::new();
        run(
            &mut protocol,
            &signals,
            &mut crate::datalog::CsvSink { out: &mut out },
            &LogOptions {
                interval: Duration::ZERO,
                samples: Some(2),
                duration: None,
            },
            &Cancel::default(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let rows: Vec<_> = text
            .lines()
            .skip(1)
            .map(|line| line.splitn(3, ',').nth(2).unwrap())
            .collect();
        assert_eq!(rows, ["2.25,,,", "3,196,1,1"]);
        assert_eq!(
            link.functional_requests(),
            [
                "010C", "010D", "0100", "0100", "0120", "0140", "014F", "010C", "010D", "014F",
                "0124"
            ]
        );
        let events = events.0.borrow();
        assert!(events
            .iter()
            .any(|event| event.contains("PID 0x0D read failed")));
        assert!(events
            .iter()
            .any(|event| event.contains("PID 0x24 read failed")));
    }

    #[test]
    fn legacy_sampling_sends_each_pid_once_and_groups_both_ecus() {
        use crate::link::{BusSpec, ResponseFrame};
        use crate::test_support::{FakeAdapter, MockLink};
        let bus = BusSpec::from_cli("iso9141", None).unwrap();
        let link = MockLink::on_bus(bus, &[]);
        for (pid, body) in [(0x0C, vec![0x1A, 0xF8]), (0x0D, vec![0x20])] {
            let mut data = vec![0x41, pid];
            data.extend(body);
            link.script_functional(vec![
                ResponseFrame {
                    id: 0x486B10,
                    data: data.clone(),
                },
                ResponseFrame { id: 0x486B28, data },
            ]);
        }
        let mut protocol = ObdProtocol::new(std::rc::Rc::new(FakeAdapter::new()), link.boxed());
        let signals = [
            SignalInfo::pid(EcuAddress::Byte(0x10), 0x0C),
            SignalInfo::pid(EcuAddress::Byte(0x28), 0x0C),
            SignalInfo::pid(EcuAddress::Byte(0x10), 0x0D),
        ];
        let mut out = Vec::new();
        run(
            &mut protocol,
            &signals,
            &mut crate::datalog::CsvSink { out: &mut out },
            &LogOptions {
                interval: std::time::Duration::ZERO,
                samples: Some(1),
                duration: None,
            },
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(link.functional_requests(), ["010C", "010D"]);
        assert!(link.sent().is_empty());
        assert!(String::from_utf8(out).unwrap().contains(",1726,1726,32\n"));
    }
}
