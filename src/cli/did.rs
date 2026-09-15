use super::{style, Context, DidCommand, Result};
use opendiag::diag::{self, DataId};
pub(super) fn run(context: &mut Context, action: &DidCommand) -> Result {
    match action {
        DidCommand::Read { dids, target } => {
            let ecu = context.target(target.ecu.as_deref())?;
            for did in dids {
                context.cancel.check()?;
                let bytes = diag::services(&ecu.diag, &mut context.protocol)?
                    .read_data(DataId::Common(*did))?;
                println!(
                    "{}  {}",
                    style::key(format!("0x{did:04X}")),
                    style::value(opendiag::utils::bytes_to_hex(&bytes))
                );
            }
        }
        DidCommand::Scan {
            start,
            end,
            timing,
            target,
        } => {
            let ecu = context.target(target.ecu.as_deref())?;
            let report = opendiag::scan::dids(
                &mut context.protocol,
                &ecu.diag,
                *start,
                *end,
                *timing,
                &context.cancel,
            )?;
            for (did, bytes) in report.values {
                println!(
                    "{}  {}",
                    style::key(format!("0x{did:04X}")),
                    style::value(opendiag::utils::bytes_to_hex(&bytes))
                );
            }
            let mut stats = report.stats;
            stats.write_summary(&mut std::io::stderr(), report.elapsed)?;
            if report.cancelled {
                return Err(opendiag::DiagError::Cancelled.into());
            }
        }
    }
    Ok(())
}
