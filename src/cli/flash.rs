use super::{
    interaction::{show_preflight, TerminalInteraction},
    style, usage, Context, FlashArgs, Result,
};
use opendiag::{
    events::FlashRecovery,
    flash::{self, FlashOptions, PreparedFlash, RunOptions, Selection},
    link::BusSpec,
    vehicle::SelectedVehicle,
};
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    time::Duration,
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
        options.skip_battery_check = args.skip_battery_check;
        Ok(Self {
            bytes,
            options: options
                .snapshot_tokens(flash::get_registry())
                .map_err(usage)?,
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
    let mut ui = TerminalInteraction::new(args.yes);
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

/// Ctrl-C presses so far. `Cancel` stays set after a press deferred during
/// programming, which cannot tell the recovery prompt about a new one.
static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

pub(super) fn note_interrupt() {
    INTERRUPTS.fetch_add(1, Ordering::SeqCst);
}

/// Reads stdin on its own thread so Ctrl-C can end a prompt blocked on input.
/// The channel disconnects when input closes. One reader serves every prompt,
/// and it is only started once a flash needs recovery.
pub(super) fn recovery_input() -> Receiver<String> {
    let (lines, input) = mpsc::channel();
    std::thread::spawn(move || loop {
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {
                if lines.send(line).is_err() {
                    return;
                }
            }
            // Stray bytes on the terminal are not a decision to end recovery.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
            Err(_) => return,
        }
    });
    input
}

pub(super) fn recovery_prompt(reason: &str, input: &Receiver<String>) -> FlashRecovery {
    eprintln!("{}", style::warn(reason));
    eprintln!("Enter status, recover (restart the retained image), or stop (end recovery).");
    eprintln!("Ctrl-C or closing input also stops.");
    let presses = INTERRUPTS.load(Ordering::SeqCst);
    recovery_choice(input, || INTERRUPTS.load(Ordering::SeqCst) != presses)
}

/// No operator can answer once input closes, so holding the session would only
/// keep pin 12 energized until the process is killed, which skips its release.
fn recovery_choice(input: &Receiver<String>, interrupted: impl Fn() -> bool) -> FlashRecovery {
    loop {
        if interrupted() {
            eprintln!("Interrupted; ending recovery.");
            return FlashRecovery::Stop;
        }
        match input.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => match line.trim().to_ascii_lowercase().as_str() {
                "status" => return FlashRecovery::Status,
                "recover" => return FlashRecovery::Retry,
                "stop" => return FlashRecovery::Stop,
                _ => eprintln!("Enter status, recover, or stop."),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("Input closed; ending recovery.");
                return FlashRecovery::Stop;
            }
        }
    }
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
    fn recovery_choice_reads_operator_answers_after_invalid_input() {
        let (lines, input) = mpsc::channel();
        for line in ["nonsense\n", " STATUS \n", "recover\n", "stop\n"] {
            lines.send(line.to_string()).unwrap();
        }
        assert_eq!(recovery_choice(&input, || false), FlashRecovery::Status);
        assert_eq!(recovery_choice(&input, || false), FlashRecovery::Retry);
        assert_eq!(recovery_choice(&input, || false), FlashRecovery::Stop);
    }

    #[test]
    fn recovery_choice_stops_when_input_closes() {
        let (lines, input) = mpsc::channel::<String>();
        drop(lines);
        assert_eq!(recovery_choice(&input, || false), FlashRecovery::Stop);
    }

    #[test]
    fn recovery_choice_stops_on_interrupt_while_waiting_for_input() {
        let (_lines, input) = mpsc::channel::<String>();
        let polls = std::cell::Cell::new(0);
        let interrupted = || {
            polls.set(polls.get() + 1);
            polls.get() > 1
        };
        assert_eq!(recovery_choice(&input, interrupted), FlashRecovery::Stop);
        assert_eq!(polls.get(), 2);
    }

    #[test]
    fn malformed_tokens_are_rejected_before_vehicle_selection() {
        let image = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(image.path(), staged_rwd(0x11).bytes).unwrap();
        let tokens = tempfile::NamedTempFile::new().unwrap();
        let mut args = flash_args();
        args.file = image.path().into();
        args.tokens = Some(tokens.path().into());
        for bytes in [b"unknown".as_slice(), b"1\r\n", b"Z\r\n"] {
            std::fs::write(tokens.path(), bytes).unwrap();
            assert!(Staged::load(&args, true).is_err());
        }
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
