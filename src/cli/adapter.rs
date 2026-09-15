use super::config::{apply_driver_config, load_config, select_library};
use super::{style, Cli};
use opendiag::{config, errors, j2534};
pub(super) fn describe_adapter(entry: &config::AdapterEntry) -> String {
    let mut parts = Vec::new();
    if let Some(driver) = &entry.driver {
        parts.push(format!("driver {}", driver.display()));
    }
    if let Some(registry) = &entry.registry {
        parts.push(format!("registry `{registry}`"));
    }
    if let Some(ini) = &entry.driver_config {
        parts.push(format!("driver_config {}", ini.display()));
    }
    if let Some(device) = &entry.device {
        parts.push(format!("device `{device}`"));
    }
    if parts.is_empty() {
        "platform default".into()
    } else {
        parts.join(", ")
    }
}

pub(super) fn list(cli: &Cli, probe: bool) -> Result<(), Box<dyn std::error::Error>> {
    let (loaded, _) = load_config(cli)?;
    let registry = j2534::discovery::registry_entries();

    println!(
        "{} {} {}",
        style::label("Config file:"),
        style::value(loaded.path.display()),
        style::muted(if loaded.exists {
            "(found)"
        } else {
            "(not found)"
        })
    );
    if !loaded.adapters.is_empty() {
        println!("{}", style::heading("Configured adapters"));
        for (name, entry) in &loaded.adapters {
            let default = if loaded.default_adapter.as_deref() == Some(name.as_str()) {
                format!(" {}", style::muted("(default)"))
            } else {
                String::new()
            };
            println!(
                "  {}{}: {}",
                style::key(name),
                default,
                describe_adapter(entry)
            );
        }
    }
    if cfg!(windows) {
        println!("{}", style::heading("Windows registry"));
        if registry.is_empty() {
            println!("  {}", style::muted("none"));
        }
        for entry in &registry {
            let api = match entry.api {
                j2534::v0500::ffi::ApiVersion::V0500 => style::value("05.00").to_string(),
                j2534::v0500::ffi::ApiVersion::V0404 => style::warn("04.04 (not supported yet)"),
            };
            let declared: Vec<&str> = entry
                .protocols
                .iter()
                .filter(|(_, on)| *on)
                .map(|(name, _)| name.as_str())
                .collect();
            println!(
                "  {}  {api}  {} {}",
                style::key(&entry.key),
                entry.library.display(),
                style::muted(format!("({})", declared.join(", ")))
            );
        }
    }

    let selection = select_library(cli, &loaded, &registry);
    match &selection {
        Ok(s) => println!(
            "{}",
            style::ok(format!(
                "Would use {} {}",
                style::value(s.library.display()),
                style::muted(format!("({})", s.source.describe()))
            ))
        ),
        Err(e) => println!("{}", style::fail(format!("Would use nothing: {e}"))),
    }

    if probe {
        // The selected library first, then any other configured or registered one.
        let mut candidates: Vec<(std::path::PathBuf, Option<std::path::PathBuf>)> = Vec::new();
        if let Ok(s) = &selection {
            candidates.push((s.library.clone(), s.driver_config.clone()));
        }
        for entry in loaded.adapters.values() {
            if let Some(driver) = &entry.driver {
                candidates.push((driver.clone(), entry.driver_config.clone()));
            }
        }
        for entry in registry
            .iter()
            .filter(|e| e.api == j2534::v0500::ffi::ApiVersion::V0500)
        {
            candidates.push((entry.library.clone(), None));
        }
        let mut seen = Vec::new();
        for (library, driver_config) in candidates {
            if seen.contains(&library) {
                continue;
            }
            seen.push(library.clone());
            println!();
            println!(
                "{}",
                style::info(format!("Probing {}", style::value(library.display())))
            );
            apply_driver_config(driver_config.as_deref());
            if let Err(e) = probe_library(&library) {
                println!("  {}", style::fail(e));
            }
        }
    }
    Ok(())
}

/// Loads a library and reports every device it can see.
fn probe_library(library: &std::path::Path) -> Result<(), errors::DiagError> {
    use j2534::consts::{
        CAN_SUPPORTED, ISO14230_SUPPORTED, ISO9141_SUPPORTED, J1850PWM_SUPPORTED,
        J1850VPW_SUPPORTED,
    };

    let lib = j2534::v0500::ffi::PassThruLib::load(library)?;
    let devices = lib.scan_devices()?;
    if devices.is_empty() {
        println!(
            "  {}",
            style::warn("no devices found (is the adapter connected and its driver configured?)")
        );
    }
    for device in devices {
        let available = match device.available {
            1 => style::ok("available"),
            2 => style::warn("in use"),
            _ => style::info("state unknown"),
        };
        println!(
            "  {} {}  {available}",
            style::label("device"),
            style::key(&device.name)
        );
        if let Some(problem) = j2534::v0500::device::compatibility_problem(device.dll_fw_status) {
            println!("    {}", style::warn(problem));
        }
        if device.available == 2 {
            continue;
        }
        let id = lib.open(&device.name)?;
        let report = (|| -> Result<(), errors::DiagError> {
            let (firmware, dll, api) = lib.read_version(id)?;
            println!(
                "    {}",
                style::muted(format!("api {api}, dll {dll}, firmware {firmware}"))
            );
            match lib.read_pin_voltage(id, 16) {
                Ok(mv) => println!(
                    "    {} {}",
                    style::label("battery"),
                    style::value(format!("{:.2} V", mv as f64 / 1000.0))
                ),
                Err(e) => println!("    {}", style::warn(format!("battery: {e}"))),
            }
            for (name, capability) in [
                ("CAN", CAN_SUPPORTED),
                ("ISO9141", ISO9141_SUPPORTED),
                ("ISO14230", ISO14230_SUPPORTED),
                ("J1850PWM", J1850PWM_SUPPORTED),
                ("J1850VPW", J1850VPW_SUPPORTED),
            ] {
                let supported = match lib.device_supports(id, capability)? {
                    Some(true) => style::ok("yes"),
                    Some(false) => style::muted("no").to_string(),
                    None => style::muted("not reported").to_string(),
                };
                println!("    {} {supported}", style::label(format!("{name:<9}")));
            }
            Ok(())
        })();
        lib.close(id)?;
        report?;
    }
    Ok(())
}
