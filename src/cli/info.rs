use super::{style, Context, Result, TargetArgs};
use std::fmt::Display;

/// A label and its value, padded before styling so escapes take no columns.
fn field(name: &str, value: impl Display) -> String {
    format!("{} {}", style::label(format!("{name:<8}")), value)
}
pub(super) fn run(context: &mut Context, target: &TargetArgs) -> Result {
    let targets = context.obd_targets(target.ecu.as_deref())?;
    for result in context.protocol.vehicle_info(&targets, &context.cancel)? {
        println!("{}", style::heading(context.module_heading(result.ecu)));
        if result.values.is_empty() {
            println!(
                "  {}",
                style::muted("No Mode 09 vehicle information available")
            );
        }
        for value in result.values {
            println!(
                "  {} {}",
                style::label(format!("{:<24}", value.name)),
                style::value(value.value)
            );
        }
    }
    Ok(())
}
pub(super) fn show_identification(result: &opendiag::identify::Identification) {
    println!("{}", field("Bus", style::value(&result.bus)));
    println!("{}", field("VIN", style::value(&result.vin.vin)));
    if let Some(make) = &result.vin.make {
        println!("{}", field("Make", style::value(make)));
    }
    if let Some(entry) = &result.selection {
        println!(
            "{}",
            field(
                "Vehicle",
                format!(
                    "{} {}{}",
                    style::key(&entry.vehicle),
                    style::value(entry.year),
                    if result.cache_hit {
                        format!(" {}", style::muted("(cached)"))
                    } else {
                        String::new()
                    }
                )
            )
        );
    } else if result.candidates.is_empty() {
        println!(
            "{}",
            field(
                "Vehicle",
                style::muted("No embedded VIN match. Select --vehicle ID; use `vehicle list`.")
            )
        );
    } else {
        println!(
            "{}",
            field(
                "Vehicle",
                style::muted(format!(
                    "No unique ECU match. Candidates: {}. Select --vehicle ID.",
                    result.candidates.join(", ")
                ))
            )
        );
    }
}
