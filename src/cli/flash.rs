use super::{
    interaction::{show_preflight, TerminalInteraction},
    style, usage, Context, FlashArgs, Result,
};
use opendiag::{
    flash::{self, FlashOptions, PreparedFlash, RunOptions, Selection},
    link::BusSpec,
    vehicle::SelectedVehicle,
};
pub(super) struct Input {
    prepared: PreparedFlash,
    slot: Option<String>,
}
impl Input {
    fn vehicle_slot(&self, vehicle: &SelectedVehicle<'_>) -> Result<String> {
        if let Some(slot) = &self.slot {
            return Ok(slot.clone());
        }
        // Automatic identification may finish after the image was prepared.
        vehicle
            .ecu_name(&self.prepared.target().to_string())
            .map_err(|error| usage(error).into())
    }
}
pub(super) struct Staged {
    bytes: Vec<u8>,
    options: FlashOptions,
}
impl Staged {
    pub fn load(args: &FlashArgs, explicit_vehicle: bool) -> Result<Self> {
        let bytes = std::fs::read(&args.file)
            .map_err(|e| usage(format!("{}: {e}", args.file.display())))?;
        let mut recognized = false;
        for name in flash::get_registry().strategy_names() {
            if flash::get_registry()
                .factory(name)
                .unwrap()
                .inspect(&bytes)
                .map_err(usage)?
                .is_some()
            {
                recognized = true;
            }
        }
        if !recognized && !explicit_vehicle {
            return Err(usage("A raw binary does not identify its vehicle. Select --vehicle, and --ecu if several ECUs are flashable.").into());
        }
        let mut options = FlashOptions::default();
        options.start_address = args.start_address;
        options.security_key = args.security_key.as_ref().map(|bytes| bytes.0.clone());
        options.tokens = args.tokens.clone();
        Ok(Self {
            bytes,
            options: options.snapshot_tokens().map_err(usage)?,
        })
    }
    pub fn finish(
        self,
        args: &FlashArgs,
        vehicle: Option<&SelectedVehicle<'_>>,
        protocol: Option<&str>,
    ) -> Result<Input> {
        let Self { bytes, mut options } = self;
        let mut slot = None;
        let mut default_ecu = None;
        let mut strategy = None;
        let ecu = if let Some(vehicle) = vehicle {
            let flashable: Vec<_> = vehicle
                .current_ecus()?
                .into_iter()
                .filter(|ecu| ecu.definition.flash.is_some())
                .collect();
            let chosen = if let Some(selector) = args.target.ecu.as_deref() {
                let name = vehicle.ecu_name(selector).map_err(usage)?;
                flashable
                    .into_iter()
                    .find(|ecu| ecu.name == name)
                    .ok_or_else(|| usage(format!("ECU {selector} has no flash entry")))?
            } else if flashable.len() == 1 {
                flashable.into_iter().next().unwrap()
            } else {
                // A recognized container can name one of several flashable slots.
                let mut target = None;
                for name in flash::get_registry().strategy_names() {
                    if let Some(info) = flash::get_registry()
                        .factory(name)
                        .unwrap()
                        .inspect(&bytes)?
                    {
                        target = info.target;
                    }
                }
                let matching: Vec<_> = flashable
                    .into_iter()
                    .filter(|ecu| Some(ecu.address) == target)
                    .collect();
                if matching.len() != 1 {
                    return Err(usage("Select --ecu NAME: the selected vehicle does not supply a unique flash target").into());
                }
                matching.into_iter().next().unwrap()
            };
            let definition = chosen.definition.flash.as_ref().unwrap();
            if options.start_address.is_none() {
                options.start_address = definition.start_address;
            }
            strategy = Some(definition.strategy.clone());
            default_ecu = Some(chosen.address);
            slot = Some(chosen.name);
            if let Some(protocol) = protocol {
                let bus = BusSpec::from_cli(protocol, None).map_err(usage)?;
                if bus.kind != chosen.bus.kind {
                    return Err(
                        usage("--protocol disagrees with the selected vehicle ECU's bus").into(),
                    );
                }
            }
            args.target.ecu.as_ref().map(|_| chosen.address)
        } else {
            args.target
                .ecu
                .as_deref()
                .map(|selector| {
                    let value = super::number(selector).map_err(usage)?;
                    BusSpec::from_cli(
                        protocol.unwrap_or(if value <= 0x7FF { "can11" } else { "can29" }),
                        None,
                    )
                    .map_err(usage)?
                    .addressing()
                    .parse_ecu(value)
                    .map_err(usage)
                })
                .transpose()?
        };
        let prepared = flash::get_registry()
            .prepare(
                bytes,
                options,
                Selection {
                    vehicle: vehicle.map(|v| v.id.as_str()),
                    ecu,
                    default_ecu,
                    strategy: strategy.as_deref(),
                },
            )
            .map_err(usage)?;
        if let Some(protocol) = protocol {
            let bus = BusSpec::from_cli(protocol, None).map_err(usage)?;
            if let opendiag::obd::addressing::EcuAddress::Can { request, .. } = prepared.target() {
                bus.addressing().parse_ecu(request).map_err(usage)?;
            }
        }
        Ok(Input { prepared, slot })
    }
}
pub(super) fn run(context: &mut Context, args: &FlashArgs, input: Input) -> Result {
    let mut ui = TerminalInteraction { yes: args.yes };
    let preflight = if let Some(vehicle) = &mut context.vehicle {
        let slot = input.vehicle_slot(vehicle)?;
        flash::run_for_vehicle(
            &mut context.protocol,
            vehicle,
            &slot,
            &input.prepared,
            &flash::VehicleRunOptions {
                dry_run: args.dry_run,
                allow_untested: args.allow_untested,
                start_address: args.start_address,
            },
            &mut ui,
            &context.cancel,
        )?
    } else {
        flash::run(
            &mut context.protocol,
            &input.prepared,
            &RunOptions {
                dry_run: args.dry_run,
                allow_untested: args.allow_untested,
                ..Default::default()
            },
            &mut ui,
            &context.cancel,
        )?
    };
    if args.dry_run {
        show_preflight(&preflight);
        preflight.validate()?;
        println!(
            "{}",
            style::ok("Dry run complete; firmware was not written")
        );
    } else {
        println!("{}", style::ok("Firmware programming completed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;
    use opendiag::vehicle::catalog;

    fn staged_rwd(target: u8) -> Staged {
        let mut bytes = vec![0x5A, 0x0D, 0x0A];
        let headers: &[&[&[u8]]] = &[
            &[],
            &[],
            &[&[target]],
            &[b"37805-RZA-A560"],
            &[&[0x11, 0x22, 0x33, 0x44]],
            &[&[0, 0, 0]],
        ];
        for header in headers {
            bytes.push(header.len() as u8);
            for entry in *header {
                bytes.push(entry.len() as u8);
                bytes.extend_from_slice(entry);
            }
        }
        bytes.extend_from_slice(&0x8000u32.to_be_bytes());
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(&[0xA5; 4]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        Staged {
            bytes,
            options: FlashOptions::default(),
        }
    }

    fn flash_args() -> FlashArgs {
        let cli = Cli::try_parse_from(["opendiag", "flash", "image.rwd"]).unwrap();
        let Command::Flash(args) = cli.command else {
            panic!("expected flash command");
        };
        args
    }

    #[test]
    fn automatically_identified_vehicle_supplies_the_prepared_targets_slot() {
        let input = staged_rwd(0x11).finish(&flash_args(), None, None).unwrap();
        assert!(input.slot.is_none());
        let vehicle =
            SelectedVehicle::new(catalog::embedded(), "honda/crv-re3", Some(2008)).unwrap();

        assert_eq!(input.vehicle_slot(&vehicle).unwrap(), "engine");
        assert_eq!(input.prepared.target().to_string(), "0x18DA11F1");
    }

    #[test]
    fn identified_vehicle_cannot_silently_replace_an_unknown_image_target() {
        let input = staged_rwd(0x01).finish(&flash_args(), None, None).unwrap();
        let vehicle =
            SelectedVehicle::new(catalog::embedded(), "honda/crv-re3", Some(2008)).unwrap();

        let err = input.vehicle_slot(&vehicle).unwrap_err();

        assert!(err.to_string().contains("not a slot"), "{err}");
        assert_eq!(input.prepared.target().to_string(), "0x18DA01F1");
    }

    #[test]
    fn explicitly_prepared_vehicle_slot_is_preserved() {
        let vehicle =
            SelectedVehicle::new(catalog::embedded(), "honda/crv-re3", Some(2008)).unwrap();
        let input = staged_rwd(0x11)
            .finish(&flash_args(), Some(&vehicle), None)
            .unwrap();

        assert_eq!(input.slot.as_deref(), Some("engine"));
        assert_eq!(input.vehicle_slot(&vehicle).unwrap(), "engine");
    }
}
