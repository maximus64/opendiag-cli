use super::{interaction::TerminalInteraction, style, usage, Context, Result, ServiceCommand};
use opendiag::{
    vehicle::{resolve_name, SelectedVehicle},
    DiagError,
};
pub(super) fn list(vehicle: &SelectedVehicle<'_>) -> Result {
    println!(
        "{} {}",
        style::heading(format!(
            "{} {}",
            vehicle.vehicle.make, vehicle.vehicle.model
        )),
        style::key(&vehicle.id)
    );
    for ecu in vehicle.current_ecus()? {
        println!(
            "  {} \u{2014} {} {}",
            style::key(&ecu.name),
            style::value(ecu.definition.title),
            style::muted(format!(
                "({})",
                ecu.selection
                    .definition
                    .as_deref()
                    .unwrap_or("unidentified")
            ))
        );
        for (name, service) in ecu.definition.services {
            let implemented = service
                .run
                .as_ref()
                .is_none_or(|run| vehicle.catalog.hooks.implemented(run));
            println!(
                "    {}  {}  {}{}",
                style::key(format!("{}.{name}", ecu.name)),
                style::value(service.title),
                style::muted(format!("{}, {}", service.safety, service.status)),
                if implemented {
                    String::new()
                } else {
                    format!("  {}", style::warn("not implemented"))
                }
            );
        }
    }
    Ok(())
}
pub(super) fn validate_before_open(vehicle: &SelectedVehicle<'_>, name: &str) -> Result {
    let names = vehicle.service_names()?;
    let qualified = match resolve_name(name, &names) {
        Ok(qualified) => qualified,
        Err(error) => {
            let mut possible = names;
            fn add(
                vehicle: &SelectedVehicle<'_>,
                slot: &str,
                definition: &str,
                names: &mut Vec<String>,
            ) -> std::result::Result<(), DiagError> {
                names.extend(
                    vehicle
                        .catalog
                        .definition(definition)?
                        .services
                        .keys()
                        .map(|name| format!("{slot}.{name}")),
                );
                for child in vehicle.catalog.children(definition)? {
                    add(vehicle, slot, &child, names)?;
                }
                Ok(())
            }
            for (slot, ecu) in vehicle.slots()? {
                for definition in ecu.definition_ids() {
                    add(vehicle, &slot, definition, &mut possible)?;
                }
            }
            possible.sort();
            possible.dedup();
            match resolve_name(name, &possible) {
                Ok(qualified) => qualified,
                Err(_) => return Err(usage(error).into()),
            }
        }
    };
    let (slot, short) = qualified.split_once('.').unwrap();
    // Identify first only when the family contains an implementation.
    for definition in vehicle.slots()?[slot].definition_ids() {
        if has_service_implementation(vehicle, definition, short)? {
            return Ok(());
        }
    }
    Err(DiagError::Unsupported(format!("{qualified} is not implemented")).into())
}
fn has_service_implementation(
    vehicle: &SelectedVehicle<'_>,
    definition: &str,
    name: &str,
) -> Result<bool> {
    let ecu = vehicle.catalog.definition(definition)?;
    if ecu.services.get(name).is_some_and(|service| {
        service
            .run
            .as_ref()
            .is_none_or(|run| vehicle.catalog.hooks.implemented(run))
    }) {
        return Ok(true);
    }
    for child in vehicle.catalog.children(definition)? {
        if has_service_implementation(vehicle, &child, name)? {
            return Ok(true);
        }
    }
    Ok(false)
}
pub(super) fn run(context: &mut Context, action: &ServiceCommand) -> Result {
    let vehicle = context
        .vehicle
        .as_mut()
        .ok_or_else(|| usage("No vehicle identified; select --vehicle ID to use its services"))?;
    match action {
        ServiceCommand::List => list(vehicle),
        ServiceCommand::Run {
            name,
            yes,
            allow_untested,
        } => {
            vehicle.run_service(
                name,
                &mut context.protocol,
                &mut TerminalInteraction { yes: *yes },
                &context.cancel,
                *allow_untested,
            )?;
            println!("{}", style::ok(format!("{name} completed")));
            Ok(())
        }
    }
}
