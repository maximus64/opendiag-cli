use super::{style, Context, EcuCommand, Result};
use opendiag::diag;
pub(super) fn run(context: &mut Context, action: &EcuCommand) -> Result {
    match action {
        EcuCommand::Scan => {
            let results = opendiag::scan::ecus(&mut context.protocol, &context.cancel)?;
            if results.is_empty() {
                return Err(
                    opendiag::DiagError::Timeout("No ECU replied to discovery".into()).into(),
                );
            }
            for result in results {
                match result.ecu {
                    Some(ecu) => println!(
                        "{}  {}",
                        style::heading(context.module_heading(ecu)),
                        style::muted(format!("response 0x{:X}", result.response))
                    ),
                    None => println!(
                        "{}  {}",
                        style::muted("Unrecognized responder"),
                        style::key(format!("0x{:X}", result.response))
                    ),
                }
            }
        }
        EcuCommand::Info { ecu } => {
            let target = context.target(Some(ecu))?;
            let mut info = diag::services(&target.diag, &mut context.protocol)?.identification()?;
            if let (Some(vehicle), Some(slot)) = (&mut context.vehicle, &target.slot) {
                let ecu = vehicle.resolve_ecu(slot, &mut context.protocol, &context.cancel)?;
                info.variant = Some(format!(
                    "{}{}",
                    ecu.selection
                        .definition
                        .as_deref()
                        .unwrap_or("unidentified"),
                    if ecu.selection.matched {
                        ""
                    } else {
                        " (unmatched)"
                    }
                ));
            }
            println!("{}", style::heading(context.module_heading(target.address)));
            if target.diag == "obd" && info.vin.is_none() && info.calibration.is_none() {
                println!(
                    "  {}",
                    style::muted("VIN/calibration information unavailable via OBD")
                );
            }
            for (name, value) in [
                ("Definition", info.variant),
                ("VIN", info.vin),
                ("Calibration", info.calibration),
            ] {
                if let Some(value) = value {
                    println!(
                        "  {} {}",
                        style::label(format!("{name:<11}")),
                        style::value(value)
                    );
                }
            }
            for (id, bytes) in info.identification {
                println!(
                    "  {}  {}  {}",
                    style::key(format!("0x{id:04X}")),
                    style::value(diag::text(&bytes)),
                    style::muted(opendiag::utils::bytes_to_hex(&bytes))
                );
            }
        }
    }
    Ok(())
}
