use super::{style, usage, Context, LiveArgs, Result};
use opendiag::{
    datalog::LogOptions,
    obd::pids::{get_pid_definition, oxygen},
    sampler::{Sample, SampleSink, SignalInfo},
    DiagError,
};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    time::{Duration, Instant},
};

enum PidSelection {
    All,
    Field(&'static str),
}

fn selection(name: &str, pid: u8, layout: oxygen::Layout) -> Option<PidSelection> {
    if opendiag::parse_hex_u32(name).ok() == Some(u32::from(pid)) {
        return Some(PidSelection::All);
    }
    let definition = get_pid_definition(1, pid)?;
    let base = oxygen::name(pid, layout).unwrap_or_else(|| definition.name.into());
    if name.eq_ignore_ascii_case(&base) || name.eq_ignore_ascii_case(definition.name) {
        return Some(PidSelection::All);
    }
    for field in oxygen::fields(pid) {
        let column = format!("{base}{}", field.suffix);
        let generic_column = format!("{}{}", definition.name, field.suffix);
        if name.eq_ignore_ascii_case(&column) || name.eq_ignore_ascii_case(&generic_column) {
            return Some(PidSelection::Field(field.key));
        }
    }
    // Preserve the original current-family lambda selector.
    if (0x34..=0x3B).contains(&pid)
        && name.eq_ignore_ascii_case(&base.replace("_WR_CURRENT", "_WR_LAMBDA"))
    {
        return Some(PidSelection::Field("lambda"));
    }
    None
}

fn is_obd_name(name: &str) -> bool {
    (0..=255).any(|pid| {
        [
            oxygen::Layout::Unknown,
            oxygen::Layout::TwoBanks,
            oxygen::Layout::FourBanks,
        ]
        .into_iter()
        .any(|layout| selection(name, pid, layout).is_some())
    })
}

fn selected_signals(
    ecu: opendiag::obd::addressing::EcuAddress,
    supported: &[u8],
    name: Option<&str>,
) -> Vec<SignalInfo> {
    let layout = oxygen::Layout::from_supported(supported);
    let mut signals = Vec::new();
    for pid in supported {
        let selected = match name {
            Some(name) => selection(name, *pid, layout),
            None => Some(PidSelection::All),
        };
        let Some(field) = selected else { continue };
        signals.extend(
            SignalInfo::pid_fields(ecu, *pid, layout)
                .into_iter()
                .filter(|signal| match (&signal.signal, &field) {
                    (
                        opendiag::sampler::Signal::ObdPid { field: actual, .. },
                        PidSelection::Field(wanted),
                    ) => actual.as_deref() == Some(*wanted),
                    _ => true,
                }),
        );
    }
    signals
}

pub(super) fn signals(
    context: &mut Context,
    names: &[String],
    selector: Option<&str>,
    default_all: bool,
) -> Result<Vec<SignalInfo>> {
    let wants_obd = default_all && names.is_empty() || names.iter().any(|name| is_obd_name(name));
    let mut signals = Vec::new();
    let mut support = BTreeMap::new();
    let mut obd_bus = None;
    if wants_obd {
        let targets = context.obd_targets(selector)?;
        let bus = context.protocol.bus().clone();
        obd_bus = Some(bus.clone());
        for ecu in targets {
            support.insert(ecu, context.protocol.supported_pids_for(ecu, 1)?);
        }
        if names.is_empty() {
            for (ecu, pids) in &support {
                for mut signal in selected_signals(*ecu, pids, None) {
                    signal.bus = Some(bus.clone());
                    signals.push(signal);
                }
            }
        }
    }
    if names.iter().any(|name| !is_obd_name(name)) {
        context.resolve_slots()?;
    }
    for name in names {
        if is_obd_name(name) {
            let mut found = false;
            for (ecu, supported) in &support {
                for mut signal in selected_signals(*ecu, supported, Some(name)) {
                    signal.bus = obd_bus.clone();
                    signals.push(signal);
                    found = true;
                }
            }
            if !found {
                return Err(usage(format!("No selected OBD ECU supports {name}")).into());
            }
        } else {
            let vehicle = context.vehicle.as_ref().ok_or_else(|| {
                usage(format!(
                    "Unknown OBD signal {name}; select --vehicle for ECU-defined signals"
                ))
            })?;
            signals.push(vehicle.resolve_signal(name).map_err(usage)?);
        }
    }
    if signals.is_empty() {
        return Err(usage("No supported signals found").into());
    }
    let multiple_obd_ecus = support.len() > 1;
    for signal in &mut signals {
        if multiple_obd_ecus {
            if let opendiag::sampler::Signal::ObdPid { ecu, .. } = signal.signal {
                signal.name = format!("{ecu}.{}", signal.name);
            }
        }
    }
    Ok(signals)
}
pub(super) struct TableSink<W: Write> {
    pub out: W,
    pub redraw: bool,
    color: bool,
    names: Vec<String>,
    width: usize,
    start: Instant,
    samples: u64,
    lines: usize,
}
impl<W: Write> TableSink<W> {
    pub fn new(out: W, redraw: bool, color: bool) -> Self {
        Self {
            out,
            redraw,
            color,
            names: Vec::new(),
            width: 0,
            start: Instant::now(),
            samples: 0,
            lines: 0,
        }
    }
}
impl<W: Write> SampleSink for TableSink<W> {
    fn begin(&mut self, signals: &[SignalInfo]) -> std::result::Result<(), DiagError> {
        self.names = signals.iter().map(|s| s.name.clone()).collect();
        // Size the name column to the signals actually selected.
        self.width = self
            .names
            .iter()
            .map(String::len)
            .max()
            .unwrap_or(0)
            .clamp(16, 40);
        Ok(())
    }
    fn sample(&mut self, sample: &Sample) -> std::result::Result<(), DiagError> {
        self.samples += 1;
        if self.redraw && self.lines > 0 {
            write!(self.out, "\x1b[{}A\x1b[J", self.lines)?;
        }
        let header = format!(
            "Sample {}  {} ms  {:.1} updates/s",
            self.samples,
            sample.elapsed_ms,
            self.samples as f64 / self.start.elapsed().as_secs_f64().max(0.001)
        );
        if self.color {
            writeln!(self.out, "{}", style::muted(&header))?;
        } else {
            writeln!(self.out, "{header}")?;
        }
        for (name, value) in self.names.iter().zip(&sample.values) {
            // Pad before styling: escape sequences take no columns of their own.
            let name = format!("{name:<width$}", width = self.width);
            let reading = value.as_ref().map(ToString::to_string);
            match (self.color, reading) {
                (true, Some(reading)) => writeln!(
                    self.out,
                    "  {} {}",
                    style::label(name),
                    style::value(reading)
                )?,
                (true, None) => writeln!(
                    self.out,
                    "  {} {}",
                    style::label(name),
                    style::muted("\u{2014}")
                )?,
                (false, reading) => writeln!(
                    self.out,
                    "  {name} {}",
                    reading.as_deref().unwrap_or("\u{2014}")
                )?,
            }
        }
        self.lines = self.names.len() + 1;
        self.out.flush()?;
        Ok(())
    }
}
pub(super) fn run(context: &mut Context, args: &LiveArgs) -> Result {
    let signals = signals(context, &args.signals, args.target.ecu.as_deref(), true)?;
    let mut sink = TableSink::new(
        io::stdout(),
        !args.once && io::stdout().is_terminal(),
        style::enabled(),
    );
    opendiag::sampler::run(
        &mut context.protocol,
        &signals,
        &mut sink,
        &LogOptions {
            interval: if args.once {
                Duration::ZERO
            } else {
                Duration::from_millis(args.interval_ms)
            },
            samples: args.once.then_some(1),
            duration: None,
        },
        &context.cancel,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendiag::{obd::addressing::EcuAddress, sampler::Signal};

    #[test]
    fn oxygen_names_resolve_against_each_ecus_layout() {
        let ecu = EcuAddress::Byte(0x10);
        for (supported, expected_pid) in [
            (vec![0x13, 0x16, 0x18], 0x18),
            (vec![0x1D, 0x16, 0x18], 0x16),
        ] {
            let signals = selected_signals(ecu, &supported, Some("O2_B2S1"));
            assert_eq!(signals.len(), 2);
            assert_eq!(signals[0].name, "O2_B2S1");
            assert_eq!(signals[1].name, "O2_B2S1_STFT");
            for signal in &signals {
                assert!(matches!(signal.signal, Signal::ObdPid { pid, .. } if pid == expected_pid));
            }
            assert_eq!(
                selected_signals(ecu, &supported, Some("O2_B2S1_STFT")).len(),
                1
            );
        }
        assert!(selected_signals(ecu, &[0x18], Some("O2_B2S1")).is_empty());
        assert!(selected_signals(ecu, &[0x13, 0x1D, 0x18], Some("O2_B2S1")).is_empty());
        let neutral = selected_signals(ecu, &[0x18], Some("0x18"));
        assert_eq!(neutral[0].name, "O2_SENSOR_5");
        let generic_trim = selected_signals(ecu, &[0x13, 0x18], Some("O2_SENSOR_5_STFT"));
        assert_eq!(generic_trim.len(), 1);
        assert_eq!(generic_trim[0].name, "O2_B2S1_STFT");
    }

    #[test]
    fn wideband_aliases_and_component_selectors_remain_available() {
        let ecu = EcuAddress::Byte(0x10);
        for name in ["O2_B2S1_WR_LAMBDA", "O2_B2S1_WR_CURRENT_LAMBDA"] {
            assert!(is_obd_name(name));
            let signals = selected_signals(ecu, &[0x13, 0x38], Some(name));
            assert_eq!(signals.len(), 1);
            assert!(matches!(&signals[0].signal,
                Signal::ObdPid { pid: 0x38, field: Some(field), .. } if field == "lambda"));
        }
        assert_eq!(selected_signals(ecu, &[0x13, 0x38], Some("0x38")).len(), 2);
        assert!(is_obd_name("O2_B4S2_WR_CURRENT"));
        assert!(is_obd_name("ENGINE_RPM"));
        assert!(!is_obd_name("engine.did_2610"));
    }
}
