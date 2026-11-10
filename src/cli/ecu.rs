use super::{
    interaction::{show_preflight, TerminalInteraction},
    style, usage, Context, EcuCommand, Result,
};
use opendiag::{
    backup::{self, EcuBackup, RestoreOptions},
    diag,
    vehicle::SelectedVehicle,
    DiagError,
};
use std::path::Path;

/// Refuses, before the adapter is opened, an ECU whose definitions offer no backup.
pub(super) fn validate_backup(vehicle: &SelectedVehicle<'_>, selector: &str) -> Result {
    let name = vehicle.ecu_name(selector).map_err(usage)?;
    fn supports(vehicle: &SelectedVehicle<'_>, id: &str) -> std::result::Result<bool, DiagError> {
        if vehicle.catalog.definition(id)?.backup.is_some() {
            return Ok(true);
        }
        for child in vehicle.catalog.children(id)? {
            if supports(vehicle, &child)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    let slots = vehicle.vehicle.modules[&name].entries();
    for slot in slots {
        for id in slot.definition_ids() {
            if supports(vehicle, id)? {
                return Ok(());
            }
        }
    }
    let label = slots
        .iter()
        .find_map(|slot| slot.label.clone())
        .unwrap_or(name);
    Err(usage(format!(
        "Backup isn't available for {label} on {} {}",
        vehicle.vehicle.make, vehicle.vehicle.model
    ))
    .into())
}

fn backup_ecu(
    context: &mut Context,
    selector: &str,
    output: &Path,
    allow_untested: bool,
) -> Result {
    let vehicle = context.vehicle.as_mut().ok_or_else(|| {
        usage("Backup needs a vehicle definition: identification did not find one; select --vehicle ID")
    })?;
    let slot = vehicle.ecu_name(selector).map_err(usage)?;
    let backup = backup::backup(
        &mut context.protocol,
        vehicle,
        &slot,
        allow_untested,
        &mut TerminalInteraction::new(false),
        &context.cancel,
    )?;
    backup.save_new(output)?;
    println!(
        "{} {}",
        style::ok("Saved backup"),
        style::value(format!(
            "{slot} \u{b7} {} \u{b7} {} \u{b7} {}",
            backup.vehicle.vin.as_deref().unwrap_or("no VIN"),
            backup.ecu.software,
            backup.created
        ))
    );
    println!("  {}", style::key(output.display()));
    println!(
        "{} install the replacement ECU, then run\n  opendiag --vehicle {} ecu restore {slot} -i {}",
        style::heading("Next:"),
        vehicle.id,
        output.display()
    );
    Ok(())
}

struct RestoreArgs<'a> {
    input: &'a Path,
    options: RestoreOptions,
    yes: bool,
}
fn restore_ecu(context: &mut Context, selector: &str, args: RestoreArgs<'_>) -> Result {
    let slot = context
        .vehicle
        .as_ref()
        .ok_or_else(|| {
            usage("Restore needs a vehicle definition: identification did not find one; select --vehicle ID")
        })?
        .ecu_name(selector)
        .map_err(usage)?;
    let backup = EcuBackup::load(args.input).map_err(usage)?;
    // The safety copy goes next to the backup being restored.
    let safety_copy_dir = args
        .input
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let vehicle = context.vehicle.as_mut().unwrap();
    let report = backup::restore(
        &mut context.protocol,
        vehicle,
        &slot,
        &backup,
        safety_copy_dir,
        &args.options,
        &mut TerminalInteraction::new(args.yes),
        &context.cancel,
    )?;
    if args.options.dry_run {
        show_preflight(&report.preflight);
        println!("{}", style::ok("Dry run complete; nothing was written"));
    } else if report.written == 0 {
        println!(
            "{}",
            style::ok("Nothing to write: the ECU already holds this backup")
        );
    } else {
        println!(
            "{}",
            style::ok(format!(
                "Restore complete and verified ({} values written)",
                report.written
            ))
        );
        if let Some(path) = &report.safety_copy {
            println!(
                "  {} {}",
                style::label("Previous data saved in"),
                style::key(path.display())
            );
        }
        println!("{} {}", style::heading("Next:"), report.next_steps);
    }
    Ok(())
}

pub(super) fn run(context: &mut Context, action: &EcuCommand) -> Result {
    match action {
        EcuCommand::Backup {
            ecu,
            output,
            allow_untested,
        } => return backup_ecu(context, ecu, output, *allow_untested),
        EcuCommand::Restore {
            ecu,
            input,
            dry_run,
            yes,
            allow_untested,
            skip_battery_check,
        } => {
            return restore_ecu(
                context,
                ecu,
                RestoreArgs {
                    input,
                    options: RestoreOptions {
                        dry_run: *dry_run,
                        allow_untested: *allow_untested,
                        skip_battery_check: *skip_battery_check,
                    },
                    yes: *yes,
                },
            )
        }
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
            let mut backup = None;
            if let (Some(vehicle), Some(slot)) = (&mut context.vehicle, &target.slot) {
                let ecu = vehicle.resolve_ecu(slot, &mut context.protocol, &context.cancel)?;
                backup = ecu
                    .definition
                    .backup
                    .filter(|_| ecu.selection.matched)
                    .map(|b| format!("available ({})", b.status));
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
                ("Backup", backup),
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
