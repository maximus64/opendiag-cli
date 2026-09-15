use super::{style, Context, LogArgs, Result};
use opendiag::{
    datalog::{CsvSink, LogOptions},
    sampler::SignalInfo,
};
use std::time::Duration;
pub(super) fn run(context: &mut Context, args: &LogArgs) -> Result {
    let mut signals = if args.signals.is_empty() {
        Vec::new()
    } else {
        super::live::signals(context, &args.signals, args.target.ecu.as_deref(), false)?
    };
    if !args.fields.is_empty() {
        let ecu = context.target(args.target.ecu.as_deref())?;
        for field in &args.fields {
            let mut signal = SignalInfo::did(ecu.address, field.clone());
            signal.bus = Some(context.protocol.bus().clone());
            if let opendiag::sampler::Signal::Did { diag, .. } = &mut signal.signal {
                *diag = ecu.diag.clone();
            }
            signals.push(signal);
        }
    }
    let mut names = std::collections::BTreeSet::new();
    for signal in &signals {
        if !names.insert(&signal.name) {
            return Err(super::usage(format!("Duplicate CSV column {}", signal.name)).into());
        }
    }
    let file = std::fs::File::create(&args.output)?;
    let mut sink = CsvSink { out: file };
    let stats = opendiag::sampler::run(
        &mut context.protocol,
        &signals,
        &mut sink,
        &LogOptions {
            interval: Duration::from_millis(args.interval_ms),
            samples: args.samples,
            duration: args.duration.map(Duration::from_secs),
        },
        &context.cancel,
    )?;
    eprintln!(
        "{}",
        style::ok(format!(
            "Wrote {} samples to {} {}",
            style::value(stats.samples),
            style::value(args.output.display()),
            style::muted(format!("({:.1} updates/s)", stats.rate))
        ))
    );
    Ok(())
}
