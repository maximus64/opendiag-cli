use clap::{Args, CommandFactory, Parser, Subcommand};
use opendiag::{
    identify,
    link::{Adapter, BusSpec},
    obd::addressing::EcuAddress,
    vehicle::{catalog, SelectedVehicle},
    Cancel, DiagError, ObdProtocol,
};
use std::{path::PathBuf, rc::Rc};
mod adapter;
mod config;
mod datalog;
mod did;
mod dtc;
mod ecu;
mod flash;
mod info;
mod interaction;
mod live;
mod progress;
mod service;
mod style;
mod vehicle;

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Debug)]
pub struct UsageError(pub String);
impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UsageError {}
/// Renders a failed run for stderr, after `run` resolved the color choice.
pub fn error_line(error: &dyn std::error::Error) -> String {
    style::fail(error)
}
fn usage(error: impl std::fmt::Display) -> UsageError {
    UsageError(error.to_string())
}
fn number(s: &str) -> std::result::Result<u32, String> {
    opendiag::parse_hex_u32(s)
}
fn did_number(s: &str) -> std::result::Result<u16, String> {
    u16::try_from(number(s)?).map_err(|_| "DID exceeds 16 bits".into())
}
#[derive(Debug, Clone)]
struct HexBytes(Vec<u8>);
fn bytes(s: &str) -> std::result::Result<HexBytes, String> {
    opendiag::utils::hex_to_bytes(
        s.strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s),
    )
    .map(HexBytes)
    .map_err(|e| e.to_string())
}

#[derive(Parser, Debug)]
#[command(
    name = "opendiag",
    version,
    about = "Open-source vehicle diagnostics over SAE J2534 pass-thru adapters"
)]
pub struct Cli {
    #[arg(short, long, global = true, help = "Show protocol-level detail")]
    pub verbose: bool,
    #[arg(
        long,
        global = true,
        value_name = "WHEN",
        value_enum,
        default_value = "auto",
        help = "Colorize output (honours NO_COLOR)"
    )]
    pub color: style::ColorChoice,
    #[arg(long, global = true, value_parser = ["can11", "can29", "iso9141", "kwp-5baud", "kwp-fast", "j1850pwm", "j1850vpw"], help = "Vehicle bus (default: selected vehicle, otherwise detected)")]
    pub protocol: Option<String>,
    #[arg(long, global = true, value_parser = number, help = "Bus bitrate in bits/s")]
    pub bitrate: Option<u32>,
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help = "Config file (or OPENDIAG_CONFIG)"
    )]
    pub config: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help = "Adapter from [adapters.NAME] in the config"
    )]
    pub adapter: Option<String>,
    #[arg(
        long,
        global = true,
        value_name = "LIBRARY",
        help = "J2534 v05.00 library (or OPENDIAG_J2534_LIB)"
    )]
    pub driver: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        help = "J2534 device name when the library offers several"
    )]
    pub device: Option<String>,
    #[arg(
        long,
        global = true,
        value_name = "ID",
        help = "Vehicle definition ID; use vehicle list/search"
    )]
    pub vehicle: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand, Debug)]
enum Command {
    /// Detect the bus, read the VIN and identify the vehicle
    #[command(display_order = 8)]
    Identify {
        #[arg(long)]
        vin: Option<String>,
    },
    /// Read or clear diagnostic trouble codes
    #[command(display_order = 5)]
    Dtc {
        #[command(subcommand)]
        action: DtcCommand,
    },
    /// Repeatedly read signals; use --once for a snapshot
    #[command(display_order = 10)]
    Live(LiveArgs),
    /// Read Mode 09 vehicle information
    #[command(display_order = 9)]
    Info(TargetArgs),
    /// Log OBD or vehicle signals and DID fields to CSV
    #[command(display_order = 3)]
    Datalog(LogArgs),
    /// List or run procedures supplied by the vehicle's ECU definitions
    #[command(display_order = 11)]
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Inspect, verify and program firmware (raw images require --vehicle)
    #[command(display_order = 7)]
    Flash(FlashArgs),
    /// Find and inspect embedded vehicle definitions
    #[command(display_order = 12)]
    Vehicle {
        #[command(subcommand)]
        action: VehicleCommand,
    },
    /// Discover ECUs or read their identification
    #[command(display_order = 6)]
    Ecu {
        #[command(subcommand)]
        action: EcuCommand,
    },
    /// Read or scan data identifiers
    #[command(display_order = 4)]
    Did {
        #[command(subcommand)]
        action: DidCommand,
    },
    /// Find or manage J2534 adapters
    #[command(display_order = 0)]
    Adapter {
        #[command(subcommand)]
        action: AdapterCommand,
    },
    /// Manage the adapter configuration
    #[command(display_order = 2)]
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Generate shell completions
    #[command(display_order = 1)]
    Completions { shell: clap_complete::Shell },
}
#[derive(Args, Debug, Default)]
struct TargetArgs {
    #[arg(long, value_name = "NAME|ADDRESS")]
    ecu: Option<String>,
}
#[derive(Subcommand, Debug)]
enum DtcCommand {
    Read {
        #[arg(long)]
        pending: bool,
        #[command(flatten)]
        target: TargetArgs,
    },
    Clear {
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        target: TargetArgs,
    },
}
#[derive(Args, Debug)]
struct LiveArgs {
    signals: Vec<String>,
    #[command(flatten)]
    target: TargetArgs,
    #[arg(long)]
    once: bool,
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
}
#[derive(Args, Debug)]
struct LogArgs {
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long = "signal")]
    signals: Vec<String>,
    #[arg(short = 'f', long = "field", value_parser = opendiag::datalog::parse_field)]
    fields: Vec<opendiag::datalog::Field>,
    #[arg(long)]
    fields_file: Option<PathBuf>,
    #[command(flatten)]
    target: TargetArgs,
    #[arg(long, default_value_t = 100)]
    interval_ms: u64,
    #[arg(long)]
    samples: Option<u64>,
    #[arg(long, value_name = "SECONDS")]
    duration: Option<u64>,
}
#[derive(Subcommand, Debug)]
enum ServiceCommand {
    List,
    Run {
        name: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        allow_untested: bool,
    },
}
#[derive(Args, Debug)]
struct FlashArgs {
    file: PathBuf,
    #[command(flatten)]
    target: TargetArgs,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    allow_untested: bool,
    #[arg(long, value_parser = number)]
    start_address: Option<u32>,
    #[arg(long, value_parser = bytes)]
    security_key: Option<HexBytes>,
    #[arg(long)]
    tokens: Option<PathBuf>,
}
#[derive(Subcommand, Debug)]
enum VehicleCommand {
    List {
        #[arg(long)]
        make: Option<String>,
        #[arg(long)]
        year: Option<u16>,
    },
    Search {
        text: String,
    },
    Show {
        id: Option<String>,
    },
}
#[derive(Subcommand, Debug)]
enum EcuCommand {
    Scan,
    #[command(display_order = 9)]
    Info {
        ecu: String,
    },
}
#[derive(Subcommand, Debug)]
enum DidCommand {
    Read {
        #[arg(required = true, value_parser = did_number)]
        dids: Vec<u16>,
        #[command(flatten)]
        target: TargetArgs,
    },
    Scan {
        #[arg(long, default_value = "0", value_parser = did_number)]
        start: u16,
        #[arg(long, default_value = "0xffff", value_parser = did_number)]
        end: u16,
        #[arg(long)]
        timing: bool,
        #[command(flatten)]
        target: TargetArgs,
    },
}
#[derive(Subcommand, Debug)]
enum AdapterCommand {
    List {
        #[arg(long)]
        probe: bool,
    },
    Voltage,
    Reset,
}
#[derive(Subcommand, Debug)]
enum ConfigAction {
    Path,
    Show,
    Init,
}

struct Target {
    address: EcuAddress,
    diag: String,
    slot: Option<String>,
}
struct Context {
    protocol: ObdProtocol,
    vehicle: Option<SelectedVehicle<'static>>,
    cancel: Cancel,
}
impl Context {
    fn module_heading(&self, address: EcuAddress) -> String {
        module_heading(self.vehicle.as_ref(), self.protocol.bus(), address)
    }
    fn target(&mut self, selector: Option<&str>) -> std::result::Result<Target, DiagError> {
        if let Some(vehicle) = &mut self.vehicle {
            let name = match selector {
                Some(value) => vehicle.ecu_name(value).ok(),
                None => {
                    let names: Vec<_> = vehicle.slots()?.into_keys().collect();
                    if names.len() == 1 {
                        Some(names[0].clone())
                    } else {
                        None
                    }
                }
            };
            if let Some(name) = name {
                let ecu = vehicle.resolve_ecu(&name, &mut self.protocol, &self.cancel)?;
                return Ok(Target {
                    address: ecu.address,
                    diag: ecu.definition.diag.unwrap_or_else(|| "obd".into()),
                    slot: Some(name),
                });
            }
        }
        let selector = selector.ok_or_else(|| {
            DiagError::InvalidCommand(
                "Select --ecu NAME|ADDRESS; identification did not supply a unique ECU".into(),
            )
        })?;
        let value = number(selector).map_err(|_| {
            DiagError::InvalidCommand(format!(
                "Unknown ECU {selector}; select --vehicle to use ECU names"
            ))
        })?;
        let address = self.protocol.addressing().parse_ecu(value)?;
        self.protocol.set_ecu(address)?;
        Ok(Target {
            address,
            diag: if self.protocol.bus().is_can() {
                "uds"
            } else {
                "obd"
            }
            .into(),
            slot: None,
        })
    }
    fn obd_targets(
        &mut self,
        selector: Option<&str>,
    ) -> std::result::Result<Vec<EcuAddress>, DiagError> {
        if let Some(value) = selector {
            Ok(vec![self.target(Some(value))?.address])
        } else {
            self.protocol.obd_ecus()
        }
    }
    fn resolve_slots(&mut self) -> std::result::Result<(), DiagError> {
        if let Some(vehicle) = &mut self.vehicle {
            for name in vehicle.slots()?.keys() {
                vehicle.resolve_ecu(name, &mut self.protocol, &self.cancel)?;
            }
        }
        Ok(())
    }
}

fn module_heading(
    vehicle: Option<&SelectedVehicle<'_>>,
    bus: &BusSpec,
    address: EcuAddress,
) -> String {
    match vehicle.and_then(|vehicle| vehicle.module_label_on(bus, address)) {
        Some(label) => format!("{label} — {address}"),
        None => format!("Module {address}"),
    }
}

fn validate(cli: &mut Cli) -> Result {
    if cli.bitrate == Some(0) {
        return Err(usage("--bitrate must be greater than zero").into());
    }
    let vehicle = cli
        .vehicle
        .as_deref()
        .map(|id| SelectedVehicle::new(catalog::embedded(), id, None))
        .transpose()
        .map_err(usage)?;
    if let Command::Identify { vin: Some(vin) } = &cli.command {
        identify::decode_vin(vin).map_err(usage)?;
    }
    if let Command::Did {
        action: DidCommand::Scan { start, end, .. },
    } = cli.command
    {
        if start > end {
            return Err(usage("--start must not exceed --end").into());
        }
    }
    if let Command::Datalog(args) = &mut cli.command {
        if let Some(file) = args.fields_file.take() {
            args.fields
                .extend(opendiag::datalog::load_fields_file(&file).map_err(usage)?);
        }
        if args.fields.is_empty() && args.signals.is_empty() {
            return Err(usage("Select at least one --signal, --field or --fields-file").into());
        }
        if args.samples == Some(0) || args.duration == Some(0) {
            return Err(usage("--samples and --duration must be greater than zero").into());
        }
    }
    let selector = match &cli.command {
        Command::Dtc {
            action: DtcCommand::Read { target, .. } | DtcCommand::Clear { target, .. },
        }
        | Command::Info(target) => target.ecu.as_deref(),
        Command::Live(args) => args.target.ecu.as_deref(),
        Command::Datalog(args) => args.target.ecu.as_deref(),
        Command::Flash(args) => args.target.ecu.as_deref(),
        Command::Did {
            action: DidCommand::Read { target, .. } | DidCommand::Scan { target, .. },
        } => target.ecu.as_deref(),
        Command::Ecu {
            action: EcuCommand::Info { ecu },
        } => Some(ecu.as_str()),
        _ => None,
    };
    if let Some(selector) = selector {
        if let Some(vehicle) = &vehicle {
            if vehicle.ecu_name(selector).is_err() && number(selector).is_err() {
                return Err(usage(format!("Unknown ECU {selector}")).into());
            }
        } else if number(selector).is_err() && cli.protocol.is_some() {
            return Err(
                usage("ECU names require an identified or explicitly selected --vehicle").into(),
            );
        }
        if let Some(bus) = cli
            .protocol
            .as_deref()
            .map(|name| BusSpec::from_cli(name, cli.bitrate))
            .transpose()
            .map_err(usage)?
        {
            if let Ok(value) = number(selector) {
                if vehicle
                    .as_ref()
                    .is_none_or(|v| v.ecu_name(selector).is_err())
                {
                    bus.addressing().parse_ecu(value).map_err(usage)?;
                }
            }
        }
    }
    if let Some(selector) = selector {
        if let Ok(value) = number(selector) {
            if vehicle
                .as_ref()
                .is_none_or(|v| v.ecu_name(selector).is_err())
            {
                let bus = BusSpec::from_cli(
                    cli.protocol.as_deref().unwrap_or(if value <= 0x7FF {
                        "can11"
                    } else {
                        "can29"
                    }),
                    cli.bitrate,
                )
                .map_err(usage)?;
                bus.addressing().parse_ecu(value).map_err(usage)?;
            }
        }
    }
    Ok(())
}

pub fn run(mut cli: Cli) -> Result {
    style::configure(cli.color);
    validate(&mut cli)?;
    match &cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(
                *shell,
                &mut Cli::command(),
                "opendiag",
                &mut std::io::stdout(),
            );
            return Ok(());
        }
        Command::Config { action } => return config::run(&cli, action),
        Command::Adapter {
            action: AdapterCommand::List { probe },
        } => return adapter::list(&cli, *probe),
        Command::Vehicle { action }
            if !matches!(action, VehicleCommand::Show { id: None }) || cli.vehicle.is_some() =>
        {
            return vehicle::run(action, cli.vehicle.as_deref())
        }
        Command::Service {
            action: ServiceCommand::List,
        } if cli.vehicle.is_some() => {
            return service::list(&SelectedVehicle::new(
                catalog::embedded(),
                cli.vehicle.as_deref().unwrap(),
                None,
            )?)
        }
        _ => {}
    }
    let mut selected = cli
        .vehicle
        .as_deref()
        .map(|id| SelectedVehicle::new(catalog::embedded(), id, None))
        .transpose()?;
    if let (Some(vehicle), Some(bitrate)) = (&mut selected, cli.bitrate) {
        let primary = vehicle.initial_bus()?.map(|bus| bus.kind);
        let kind = cli
            .protocol
            .as_deref()
            .map(|name| BusSpec::from_cli(name, None))
            .transpose()
            .map_err(usage)?
            .map(|bus| bus.kind)
            .or(primary);
        for bus in vehicle.vehicle.buses.values_mut() {
            if Some(bus.spec().map_err(usage)?.kind) == kind {
                bus.bitrate = Some(bitrate);
            }
        }
    }
    let needs_year = selected
        .as_ref()
        .is_some_and(|vehicle| vehicle.needs_year());
    let mut staged_flash = if let Command::Flash(args) = &cli.command {
        Some(flash::Staged::load(args, cli.vehicle.is_some())?)
    } else {
        None
    };
    let mut flash_input = if !needs_year {
        if let (Command::Flash(args), Some(staged)) = (&cli.command, staged_flash.take()) {
            Some(staged.finish(args, selected.as_ref(), cli.protocol.as_deref())?)
        } else {
            None
        }
    } else {
        None
    };
    // Services declared by a stub fail without connecting to hardware.
    if let (
        Some(vehicle),
        Command::Service {
            action: ServiceCommand::Run { name, .. },
        },
    ) = (&selected, &cli.command)
    {
        if !vehicle.needs_year() {
            service::validate_before_open(vehicle, name)?;
        }
    }
    let cancel = Cancel::default();
    if !cfg!(test) {
        let signal = cancel.clone();
        ctrlc::set_handler(move || signal.cancel())?;
    }
    let (loaded, _) = config::load_config(&cli)?;
    let registry = opendiag::j2534::discovery::registry_entries();
    let selection = config::select_library(&cli, &loaded, &registry)?;
    eprintln!(
        "{}",
        style::info(format!(
            "Opening {} {}",
            style::value(selection.library.display()),
            style::muted(format!("({})", selection.source.describe()))
        ))
    );
    let mut events = interaction::TerminalEvents::new(cli.verbose);
    let device = opendiag::j2534::v0500::device::Device::open_with_events(&selection, Box::new(events.clone())).map_err(|error| {
        if matches!(&error, DiagError::Adapter(message) if message.contains("load")) { DiagError::Adapter(format!("{error}\nSet --driver to your adapter's J2534 v05.00 library, or run `opendiag config init` and set [adapters.NAME].driver. `opendiag adapter list --probe` shows available adapters.")) } else { error }
    })?;
    match device.version() {
        Ok((firmware, driver, api)) => eprintln!(
            "{}",
            style::ok(format!(
                "Adapter {} {}",
                style::value(device.name()),
                style::muted(format!(
                    "J2534 {api} \u{b7} driver {driver} \u{b7} firmware {firmware}"
                ))
            ))
        ),
        Err(e) => eprintln!("{}", style::warn(format!("Adapter {}: {e}", device.name()))),
    }
    for warning in opendiag::obd::protocol::clear_all_programming_voltage(&device) {
        eprintln!("{}", style::warn(warning));
    }
    if let Command::Adapter {
        action: AdapterCommand::Voltage,
    } = cli.command
    {
        use opendiag::link::DeviceOps;
        println!(
            "{}",
            style::value(format!(
                "{:.2} V",
                device.read_battery_mv()? as f64 / 1000.0
            ))
        );
        return Ok(());
    }
    let adapter: Rc<dyn Adapter> = Rc::new(device);
    let explicit_bus = cli
        .protocol
        .as_deref()
        .map(|name| BusSpec::from_cli(name, cli.bitrate))
        .transpose()
        .map_err(usage)?;
    let vehicle_bus = if matches!(
        cli.command,
        Command::Adapter {
            action: AdapterCommand::Reset
        }
    ) {
        Some(BusSpec::from_cli("can29", cli.bitrate).map_err(usage)?)
    } else if let Some(vehicle) = &selected {
        vehicle.initial_bus()?
    } else {
        None
    };
    let chosen_bus = match (explicit_bus, vehicle_bus) {
        (Some(explicit), Some(vehicle)) if explicit.kind == vehicle.kind => Some(vehicle),
        (explicit, vehicle) => explicit.or(vehicle),
    };
    let mut protocol = if let Some(mut bus) = chosen_bus {
        if let Some(bitrate) = cli.bitrate {
            bus.bitrate = bitrate;
        }
        ObdProtocol::new(adapter.clone(), adapter.connect(&bus)?)
    } else {
        let buses = if let Some(bitrate) = cli.bitrate {
            let mut iso9141 = BusSpec::standard(opendiag::link::BusKind::Iso9141);
            iso9141.bitrate = bitrate;
            let kwp = BusSpec::from_cli("kwp-5baud", Some(bitrate)).map_err(usage)?;
            let kwp_fast = BusSpec::from_cli("kwp-fast", Some(bitrate)).map_err(usage)?;
            let mut pwm = BusSpec::standard(opendiag::link::BusKind::J1850Pwm);
            pwm.bitrate = bitrate;
            let mut vpw = BusSpec::standard(opendiag::link::BusKind::J1850Vpw);
            vpw.bitrate = bitrate;
            vec![
                BusSpec::obd_can(opendiag::link::IdWidth::Bits11, bitrate),
                BusSpec::obd_can(opendiag::link::IdWidth::Bits29, bitrate),
                iso9141,
                kwp,
                kwp_fast,
                pwm,
                vpw,
            ]
        } else {
            identify::detection_buses()
        };
        identify::detect(adapter, &buses, &mut events, &cancel)?
    };
    events.finish();
    protocol.set_events(Box::new(events.clone()));
    eprintln!(
        "{}",
        style::ok(format!("Connected {}", style::value(protocol.bus())))
    );
    if needs_year
        && !matches!(
            cli.command,
            Command::Identify { .. } | Command::Adapter { .. }
        )
    {
        let vin = identify::read_vin(&mut protocol, None, &cancel)?;
        let vehicle = selected.as_mut().unwrap();
        vehicle.set_year_from_vin(&vin)?;
        vehicle.bind_obd_bus(protocol.bus())?;
        if let (Command::Flash(args), Some(staged)) = (&cli.command, staged_flash.take()) {
            flash_input = Some(staged.finish(args, selected.as_ref(), cli.protocol.as_deref())?);
        }
    }
    let is_identify = matches!(cli.command, Command::Identify { .. });
    if is_identify || selected.is_none() && !matches!(cli.command, Command::Adapter { .. }) {
        let vin = match &cli.command {
            Command::Identify { vin } => vin.as_deref(),
            _ => None,
        };
        match identify::identify_with_options(
            &mut protocol,
            catalog::embedded(),
            catalog::INDEX,
            &identify::IdentifyOptions {
                vin,
                vehicle: if is_identify {
                    cli.vehicle.as_deref()
                } else {
                    None
                },
            },
            identify::Cache::platform().as_ref(),
            &cancel,
        ) {
            Ok(result) => {
                if let Some(entry) = &result.selection {
                    let mut vehicle = SelectedVehicle::new(
                        catalog::embedded(),
                        &entry.vehicle,
                        Some(entry.year),
                    )?;
                    vehicle.bind_obd_bus(protocol.bus())?;
                    vehicle.restore_variants(&entry.variants)?;
                    selected = Some(vehicle);
                }
                if is_identify {
                    info::show_identification(&result);
                    if let Some(vehicle) = &mut selected {
                        for name in vehicle.slots()?.keys() {
                            let ecu = vehicle.resolve_ecu(name, &mut protocol, &cancel)?;
                            let definition = ecu
                                .selection
                                .definition
                                .as_deref()
                                .unwrap_or("unidentified");
                            println!(
                                "{}  {}{}",
                                style::heading(module_heading(
                                    Some(vehicle),
                                    &ecu.bus,
                                    ecu.address
                                )),
                                style::key(definition),
                                if ecu.selection.matched {
                                    String::new()
                                } else {
                                    format!(" {}", style::muted("(unmatched)"))
                                }
                            );
                        }
                    }
                    return Ok(());
                }
            }
            Err(error)
                if !is_identify
                    && matches!(error, DiagError::Timeout(_) | DiagError::Unsupported(_)) =>
            {
                if cli.verbose {
                    eprintln!(
                        "{}",
                        style::detail(format!("Vehicle identification: {error}"))
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut context = Context {
        protocol,
        vehicle: selected,
        cancel,
    };
    match cli.command {
        Command::Dtc { action } => dtc::run(&mut context, &action),
        Command::Live(args) => live::run(&mut context, &args),
        Command::Info(target) => info::run(&mut context, &target),
        Command::Datalog(args) => datalog::run(&mut context, &args),
        Command::Service { action } => service::run(&mut context, &action),
        Command::Flash(args) => flash::run(&mut context, &args, flash_input.unwrap()),
        Command::Vehicle { action } => {
            vehicle::run(&action, context.vehicle.as_ref().map(|v| v.id.as_str()))
        }
        Command::Ecu { action } => ecu::run(&mut context, &action),
        Command::Did { action } => did::run(&mut context, &action),
        Command::Adapter {
            action: AdapterCommand::Reset,
        } => {
            context.protocol.reset()?;
            println!("{}", style::ok("Bus queues cleared"));
            Ok(())
        }
        _ => unreachable!("offline command already handled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn module_headings_use_vehicle_roles_and_keep_unknown_responders_generic() {
        let bus = BusSpec::obd_can(opendiag::link::IdWidth::Bits29, 500_000);
        let vehicle =
            SelectedVehicle::new(catalog::embedded(), "honda/crv-re3", Some(2008)).unwrap();
        for (request, expected) in [
            (0x18DA1DF1, "Transmission (TCU) — 0x18DA1DF1"),
            (0x18DA11F1, "Engine (ECU) — 0x18DA11F1"),
            (0x18DA10F1, "Module 0x18DA10F1"),
        ] {
            let address = bus.addressing().parse_ecu(request).unwrap();
            assert_eq!(module_heading(Some(&vehicle), &bus, address), expected);
            assert_eq!(
                module_heading(None, &bus, address),
                format!("Module {address}")
            );
        }
        for year in [None, Some(2007), Some(2009)] {
            let vehicle = SelectedVehicle::new(catalog::embedded(), "honda/crv-re3", year).unwrap();
            let address = bus.addressing().parse_ecu(0x18DA1DF1).unwrap();
            assert_eq!(
                module_heading(Some(&vehicle), &bus, address),
                "Module 0x18DA1DF1"
            );
        }
    }

    #[test]
    fn every_grouped_command_parses_and_old_names_and_flags_are_rejected() {
        for args in [
            vec!["identify", "--vin", "1HGCM82633A004352"],
            vec!["dtc", "read", "--pending"],
            vec!["dtc", "clear", "--yes"],
            vec!["live", "ENGINE_RPM", "0x05", "--once", "--ecu", "0x7E0"],
            vec!["info"],
            vec![
                "datalog",
                "-o",
                "drive.csv",
                "--signal",
                "ENGINE_RPM",
                "-f",
                "rpm=0x2610:0:2",
            ],
            vec!["service", "list"],
            vec!["service", "run", "engine.idle-relearn", "--allow-untested"],
            vec![
                "flash",
                "image.bin",
                "--security-key",
                "11223344",
                "--start-address",
                "0x10000",
                "--dry-run",
                "--yes",
            ],
            vec!["vehicle", "list", "--make", "Honda", "--year", "2008"],
            vec!["vehicle", "search", "odyssey"],
            vec!["vehicle", "show", "honda/odyssey-rl3"],
            vec!["ecu", "scan"],
            vec!["ecu", "info", "engine"],
            vec!["did", "read", "0xF181", "61840"],
            vec![
                "did", "scan", "--start", "0xF180", "--end", "0xF190", "--timing",
            ],
            vec!["adapter", "list", "--probe"],
            vec!["adapter", "voltage"],
            vec!["adapter", "reset"],
            vec!["config", "path"],
            vec!["config", "show"],
            vec!["config", "init"],
            vec!["completions", "zsh"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("opendiag").chain(args.clone())).is_ok(),
                "{args:?}"
            );
        }
        for name in [
            "reset",
            "voltage",
            "transfer",
            "pids",
            "read-dtc",
            "read-pending-dtc",
            "clear-dtc",
            "list-ecus",
            "flash-info",
            "read-all-uds-ids",
            "scan-ecu",
            "list-cars",
            "list-services",
            "list-devices",
        ] {
            assert!(Cli::try_parse_from(["opendiag", name]).is_err(), "{name}");
        }
        for args in [
            ["--car", "foo", "info"],
            ["--id", "0x7E0", "info"],
            ["flash", "image.bin", "--force"],
            ["flash", "image.bin", "--unverified"],
            ["info", "--profile", "foo.toml"],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("opendiag").chain(args)).is_err());
        }
    }
    #[test]
    fn plain_live_sink_prints_typed_values_without_terminal_escape_codes() {
        use opendiag::sampler::SampleSink;
        let signal = opendiag::sampler::SignalInfo::pid(
            EcuAddress::Can {
                request: 0x7E0,
                response: 0x7E8,
            },
            0x0C,
        );
        let mut sink = live::TableSink::new(Vec::new(), false, false);
        sink.begin(&[signal]).unwrap();
        sink.sample(&opendiag::sampler::Sample {
            unix_ms: 0,
            elapsed_ms: 10,
            values: vec![Some(opendiag::value::Value::number(1726.25, "rpm"))],
        })
        .unwrap();
        let text = String::from_utf8(sink.out).unwrap();
        assert!(text.contains("ENGINE_RPM") && text.contains("1726.25 rpm"));
        assert!(!text.contains('\x1b'));
    }
}
