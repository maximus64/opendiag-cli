use super::adapter::describe_adapter;
use super::{style, Cli, ConfigAction};
use opendiag::{config, j2534};
/// Resolves and loads the config file, printing any warnings it produced.
pub(super) fn load_config(cli: &Cli) -> Result<(config::Config, config::PathSource), String> {
    let env = std::env::var(config::CONFIG_ENV).ok();
    let (path, source) = config::resolve_path(cli.config.as_deref(), env.as_deref())?;
    let loaded = config::load(&path)?;
    for warning in &loaded.warnings {
        eprintln!("{}", style::warn(format!("{}: {warning}", path.display())));
    }
    Ok((loaded, source))
}

/// Picks the J2534 library from the flags, environment and config.
pub(super) fn select_library(
    cli: &Cli,
    loaded: &config::Config,
    registry: &[j2534::discovery::RegistryEntry],
) -> Result<j2534::discovery::Selection, String> {
    let env_library = std::env::var(j2534::discovery::LIBRARY_ENV).ok();
    j2534::discovery::select(&j2534::discovery::SelectionInputs {
        driver_flag: cli.driver.as_deref(),
        device_flag: cli.device.as_deref(),
        adapter_flag: cli.adapter.as_deref(),
        env_library: env_library.as_deref(),
        config: loaded,
        registry,
    })
}

/// Points the OpenDiag driver at an `opendiag.ini` for this process. The
/// driver reads the variable on every open, so this must run before it.
pub(super) fn apply_driver_config(driver_config: Option<&std::path::Path>) {
    if let Some(path) = driver_config {
        std::env::set_var(j2534::discovery::OPENDIAG_DRIVER_CONFIG_ENV, path);
    }
}

/// Whether a path is there, as its own marked segment: nesting it inside other
/// styled text would end that style at its reset sequence.
fn describe_path_state(path: &std::path::Path) -> String {
    if path.exists() {
        style::ok("exists")
    } else {
        style::warn("missing")
    }
}
/// A left-aligned field of `config show`, padded before styling.
fn field(name: &str, value: impl std::fmt::Display) {
    println!("{} {value}", style::label(format!("{name:<16}")));
}

pub(super) fn run(cli: &Cli, action: &ConfigAction) -> Result<(), Box<dyn std::error::Error>> {
    let env = std::env::var(config::CONFIG_ENV).ok();
    let (path, source) = config::resolve_path(cli.config.as_deref(), env.as_deref())?;
    match action {
        ConfigAction::Path => println!("{}", path.display()),
        ConfigAction::Init => {
            config::init(&path)?;
            println!(
                "{}",
                style::ok(format!("Wrote {}", style::value(path.display())))
            );
        }
        ConfigAction::Show => {
            let (loaded, _) = load_config(cli)?;
            field(
                "config file",
                format!(
                    "{} {} {}",
                    style::value(path.display()),
                    style::muted(format!("({})", source.describe())),
                    describe_path_state(&path)
                ),
            );
            field(
                "default_adapter",
                match loaded.default_adapter.as_deref() {
                    Some(name) => style::key(name).to_string(),
                    None => style::muted("-").to_string(),
                },
            );
            for (name, entry) in &loaded.adapters {
                field(&format!("adapter {name}"), describe_adapter(entry));
            }

            let registry = j2534::discovery::registry_entries();
            match select_library(cli, &loaded, &registry) {
                Ok(selection) => {
                    field(
                        "library",
                        format!(
                            "{} {}",
                            style::value(selection.library.display()),
                            style::muted(format!("({})", selection.source.describe()))
                        ),
                    );
                    field(
                        "device",
                        match selection.device.as_deref() {
                            Some(device) => style::key(device).to_string(),
                            None => style::muted("the only one found").to_string(),
                        },
                    );
                    let env_ini = std::env::var(j2534::discovery::OPENDIAG_DRIVER_CONFIG_ENV).ok();
                    let (ini, why) = j2534::discovery::opendiag_ini_path(
                        selection.driver_config.as_deref(),
                        env_ini.as_deref(),
                    );
                    match ini {
                        Some(ini) => field(
                            "opendiag.ini",
                            format!(
                                "{} {} {}",
                                style::value(ini.display()),
                                style::muted(format!("({why})")),
                                describe_path_state(&ini)
                            ),
                        ),
                        None => field("opendiag.ini", style::muted(format!("unknown ({why})"))),
                    }
                }
                Err(e) => field("library", style::fail(e)),
            }
        }
    }
    Ok(())
}
