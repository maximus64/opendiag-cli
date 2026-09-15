use super::{style, usage, Result, VehicleCommand};
use opendiag::vehicle::{
    catalog::{self, INDEX},
    years, SelectedVehicle,
};
/// An indented attribute of the module above it.
fn detail(name: &str, value: impl std::fmt::Display) {
    println!("    {} {value}", style::label(format!("{name}:")));
}
/// One line of `vehicle list`/`vehicle search`, aligned on the definition ID.
fn row(entry: &catalog::VehicleIndex) -> String {
    format!(
        "{}  {} {}",
        style::key(format!("{:<28}", entry.id)),
        style::value(format!("{} {}", entry.make, entry.model)),
        style::muted(years::format(entry.years))
    )
}
pub(super) fn run(action: &VehicleCommand, selected: Option<&str>) -> Result {
    match action {
        VehicleCommand::List { make, year } => {
            for entry in INDEX.iter().filter(|entry| {
                make.as_ref()
                    .is_none_or(|make| entry.make.eq_ignore_ascii_case(make))
                    && year.is_none_or(|year| years::contains(entry.years, year))
            }) {
                println!("{}", row(entry));
            }
        }
        VehicleCommand::Search { text } => {
            for entry in INDEX {
                if format!("{} {} {}", entry.id, entry.make, entry.model)
                    .to_lowercase()
                    .contains(&text.to_lowercase())
                {
                    println!("{}", row(entry));
                }
            }
        }
        VehicleCommand::Show { id } => {
            let id = id.as_deref().or(selected).ok_or_else(|| {
                usage("No vehicle identified. Select --vehicle ID or use `vehicle show ID`.")
            })?;
            let vehicle = SelectedVehicle::new(catalog::embedded(), id, None).map_err(usage)?;
            println!(
                "{}: {} ({})",
                style::key(id),
                style::heading(
                    [
                        vehicle.vehicle.make.as_str(),
                        vehicle.vehicle.model.as_str(),
                        vehicle.vehicle.chassis.as_deref().unwrap_or("")
                    ]
                    .join(" ")
                    .trim_end()
                ),
                style::value(years::format(&vehicle.vehicle.years))
            );
            println!(
                "  {} {}",
                style::label("system voltage:"),
                style::value(format!("{} V", vehicle.vehicle.system_voltage.volts()))
            );
            for (name, versions) in &vehicle.vehicle.modules {
                for slot in versions.entries() {
                    let bus = vehicle.vehicle.buses[&slot.bus].spec().map_err(usage)?;
                    let address = slot.address(&bus).map_err(usage)?;
                    if slot.uses.is_none() {
                        let label = slot
                            .label
                            .as_deref()
                            .ok_or_else(|| usage(format!("Module {name} needs a label or uses")))?;
                        println!(
                            "  {}: {} {}",
                            style::key(name),
                            style::value(label),
                            style::muted(format!("{address} on {bus}"))
                        );
                        if let Some(years) = &slot.years {
                            detail("model years", years::format(years));
                        }
                        continue;
                    }
                    if let Some(years) = &slot.years {
                        println!(
                            "  {}: {}",
                            style::key(name),
                            style::muted(format!("model years {}", years::format(years)))
                        );
                    }
                    for id in slot.definition_ids() {
                        let definition = vehicle.catalog.definition(id)?;
                        println!(
                            "  {}: {} {}",
                            style::key(name),
                            style::value(definition.title),
                            style::muted(format!("{id} on {bus}"))
                        );
                        detail("address", address);
                        if slot.definition_ids().len() > 1 {
                            detail("candidate", "requires ECU identification");
                        }
                        if let Some(flash) = definition.flash {
                            detail("flash", format!("{} ({})", flash.strategy, flash.status));
                        }
                        for signal in definition.signals {
                            detail(
                                "signal",
                                format!(
                                    "{name}.{} {}",
                                    signal.field.split('=').next().unwrap_or(""),
                                    signal.unit
                                ),
                            );
                        }
                        for (service_name, service) in definition.services {
                            detail(
                                "service",
                                format!(
                                    "{name}.{service_name} \u{2014} {} ({}, {})",
                                    service.title, service.safety, service.status
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
