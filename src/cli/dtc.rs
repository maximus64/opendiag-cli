use super::{interaction::TerminalInteraction, style, Context, DtcCommand, Result};
use opendiag::{diag, obd::protocol::EcuDtcs, DiagError};
pub(super) fn run(context: &mut Context, action: &DtcCommand) -> Result {
    match action {
        DtcCommand::Read { pending, target } => {
            let results = if let Some(selector) = target.ecu.as_deref() {
                let ecu = context.target(Some(selector))?;
                if ecu.diag == "obd" {
                    context
                        .protocol
                        .read_obd_dtcs(&[ecu.address], *pending, &context.cancel)?
                } else {
                    if *pending {
                        return Err(DiagError::Unsupported("--pending applies to OBD Mode 07; the selected ECU uses manufacturer diagnostics".into()).into());
                    }
                    vec![EcuDtcs {
                        ecu: ecu.address,
                        dtcs: diag::services(&ecu.diag, &mut context.protocol)?.read_dtcs()?,
                    }]
                }
            } else {
                let targets = context.protocol.obd_ecus()?;
                context
                    .protocol
                    .read_obd_dtcs(&targets, *pending, &context.cancel)?
            };
            for result in results {
                if let Some(vehicle) = &mut context.vehicle {
                    for ecu in vehicle.current_ecus()? {
                        if ecu.address == result.ecu && ecu.bus == *context.protocol.bus() {
                            vehicle.resolve_ecu(
                                &ecu.name,
                                &mut context.protocol,
                                &context.cancel,
                            )?;
                            break;
                        }
                    }
                }
                println!("{}", style::heading(context.module_heading(result.ecu)));
                if result.dtcs.is_empty() {
                    println!("  {}", style::muted("No trouble codes"));
                }
                for dtc in result.dtcs {
                    let description = context
                        .vehicle
                        .as_ref()
                        .and_then(|v| {
                            v.dtc_description_on(context.protocol.bus(), result.ecu, &dtc.code)
                        })
                        .unwrap_or(dtc.description);
                    println!("  {}  {}", style::code(&dtc.code), description);
                }
            }
        }
        DtcCommand::Clear { yes, target } => {
            let mut ui = TerminalInteraction { yes: *yes };
            if let Some(selector) = target.ecu.as_deref() {
                let ecu = context.target(Some(selector))?;
                if ecu.diag == "obd" {
                    context
                        .protocol
                        .clear_obd_dtcs(&[ecu.address], &mut ui, &context.cancel)?;
                } else {
                    diag::clear_with_confirmation(
                        &ecu.diag,
                        &mut context.protocol,
                        &mut ui,
                        &context.cancel,
                    )?;
                }
            } else {
                let targets = context.protocol.obd_ecus()?;
                context
                    .protocol
                    .clear_obd_dtcs(&targets, &mut ui, &context.cancel)?;
            }
            println!("{}", style::ok("Trouble codes cleared"));
        }
    }
    Ok(())
}
