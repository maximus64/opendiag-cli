//! Choosing which J2534 library, and which device in it, to use.
//!
//! Precedence, highest first: `--driver`, `OPENDIAG_J2534_LIB`, `--adapter`,
//! the config's `default_adapter`, then the platform default. On Windows that
//! default is the spec's `PassThruSupport` registry; elsewhere the spec defines
//! nothing, so we load OpenDiag's library by name through the system loader.

use std::path::{Path, PathBuf};

use crate::config::{AdapterEntry, Config};
use crate::j2534::v0500::ffi::{ApiVersion, DeviceInfo};

/// Environment variable naming a J2534 library to load, like `--driver`.
pub const LIBRARY_ENV: &str = "OPENDIAG_J2534_LIB";

/// Environment variable the OpenDiag driver reads its `opendiag.ini` path from.
pub const OPENDIAG_DRIVER_CONFIG_ENV: &str = "OPENDIAG_DRIVER_CONFIG";

/// `PassThruSupport` registry value names for the protocol flags.
#[cfg_attr(not(windows), allow(dead_code))]
pub const PROTOCOL_FLAGS: &[&str] = &[
    "CAN", "ISO15765", "ISO9141", "ISO14230", "J1850VPW", "J1850PWM",
];

/// One installed adapter from the Windows registry.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryEntry {
    /// Subkey name, e.g. `OpenDIAG - OpenDIAG`.
    pub key: String,
    /// Which `PassThruSupport.xx.xx` key it was found under.
    pub api: ApiVersion,
    pub name: String,
    pub vendor: String,
    pub library: PathBuf,
    /// Declared protocol flags; a missing flag means "not declared".
    pub protocols: Vec<(String, bool)>,
}

impl RegistryEntry {
    fn matches(&self, wanted: &str) -> bool {
        self.key.eq_ignore_ascii_case(wanted)
            || self.name.eq_ignore_ascii_case(wanted)
            || (!wanted.is_empty() && self.vendor.to_lowercase().contains(&wanted.to_lowercase()))
    }
}

/// Where the chosen library came from.
#[derive(Debug, Clone, PartialEq)]
pub enum LibrarySource {
    Flag,
    Environment,
    ConfigAdapter(String),
    Registry(String),
    SystemLoader,
}

impl LibrarySource {
    pub fn describe(&self) -> String {
        match self {
            LibrarySource::Flag => "--driver".into(),
            LibrarySource::Environment => LIBRARY_ENV.into(),
            LibrarySource::ConfigAdapter(name) => format!("config adapter `{name}`"),
            LibrarySource::Registry(key) => format!("Windows registry `{key}`"),
            LibrarySource::SystemLoader => "system library search path".into(),
        }
    }
}

/// The library to load and what to do with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub library: PathBuf,
    pub source: LibrarySource,
    /// Device name to open; `None` means "the only one there is".
    pub device: Option<String>,
    /// OpenDiag only: exported as `OPENDIAG_DRIVER_CONFIG` before loading.
    pub driver_config: Option<PathBuf>,
    /// What the registry declares about the adapter's protocols, if anything.
    pub declared_protocols: Vec<(String, bool)>,
}

/// Everything selection depends on, gathered by the caller so this stays pure.
pub struct SelectionInputs<'a> {
    pub driver_flag: Option<&'a Path>,
    pub device_flag: Option<&'a str>,
    pub adapter_flag: Option<&'a str>,
    pub env_library: Option<&'a str>,
    pub config: &'a Config,
    pub registry: &'a [RegistryEntry],
}

/// The OpenDiag library's file name, for the system loader to find.
pub fn default_library_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("libopendiag.dylib")
    } else if cfg!(windows) {
        None // Windows uses the registry
    } else {
        Some("libopendiag.so")
    }
}

/// Chooses the library, following the precedence in the module docs.
pub fn select(inputs: &SelectionInputs) -> Result<Selection, String> {
    let simple = |library: PathBuf, source: LibrarySource| Selection {
        library,
        source,
        device: inputs.device_flag.map(str::to_string),
        driver_config: None,
        declared_protocols: Vec::new(),
    };

    if let Some(path) = inputs.driver_flag {
        return Ok(simple(
            crate::config::expand_tilde(path),
            LibrarySource::Flag,
        ));
    }
    if let Some(path) = inputs.env_library.filter(|p| !p.is_empty()) {
        return Ok(simple(
            crate::config::expand_tilde(Path::new(path)),
            LibrarySource::Environment,
        ));
    }

    let adapter = match inputs.adapter_flag {
        Some(name) => Some(name),
        None => inputs.config.default_adapter.as_deref(),
    };
    if let Some(name) = adapter {
        let entry = inputs.config.adapters.get(name).ok_or_else(|| {
            let known: Vec<&str> = inputs.config.adapters.keys().map(String::as_str).collect();
            format!(
                "No adapter `{}` in {} (known: {})",
                name,
                inputs.config.path.display(),
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )
        })?;
        return select_from_entry(name, entry, inputs);
    }

    platform_default(None, inputs)
}

fn select_from_entry(
    name: &str,
    entry: &AdapterEntry,
    inputs: &SelectionInputs,
) -> Result<Selection, String> {
    let device = inputs
        .device_flag
        .map(str::to_string)
        .or_else(|| entry.device.clone());
    if let Some(library) = &entry.driver {
        return Ok(Selection {
            library: library.clone(),
            source: LibrarySource::ConfigAdapter(name.to_string()),
            device,
            driver_config: entry.driver_config.clone(),
            declared_protocols: Vec::new(),
        });
    }
    if entry.registry.is_some() && !cfg!(windows) {
        return Err(format!(
            "[adapters.{name}] uses `registry`, which only exists on Windows"
        ));
    }
    let mut selection = platform_default(entry.registry.as_deref(), inputs)?;
    if selection.device.is_none() {
        selection.device = device;
    }
    if selection.driver_config.is_none() {
        selection.driver_config = entry.driver_config.clone();
    }
    Ok(selection)
}

/// The registry on Windows, the system loader elsewhere. `wanted` narrows the
/// registry to a key, name or vendor; without it, `--device` does.
fn platform_default(wanted: Option<&str>, inputs: &SelectionInputs) -> Result<Selection, String> {
    if let Some(name) = default_library_name() {
        return Ok(Selection {
            library: PathBuf::from(name),
            source: LibrarySource::SystemLoader,
            device: inputs.device_flag.map(str::to_string),
            driver_config: None,
            declared_protocols: Vec::new(),
        });
    }

    select_registry(wanted, inputs.device_flag, inputs.registry)
}

/// Picks one v05.00 adapter from the registry. Platform-independent so it can
/// be tested anywhere; only Windows ever has entries.
fn select_registry(
    wanted: Option<&str>,
    device_flag: Option<&str>,
    registry: &[RegistryEntry],
) -> Result<Selection, String> {
    let filter = wanted.or(device_flag);
    let matching: Vec<&RegistryEntry> = registry
        .iter()
        .filter(|entry| filter.is_none_or(|f| entry.matches(f)))
        .collect();
    let usable: Vec<&RegistryEntry> = matching
        .iter()
        .copied()
        .filter(|e| e.api == ApiVersion::V0500)
        .collect();

    match usable.as_slice() {
        [entry] => Ok(Selection {
            library: entry.library.clone(),
            source: LibrarySource::Registry(entry.key.clone()),
            // A registry match by name says nothing about the device name.
            device: if wanted.is_some() { device_flag.map(str::to_string) } else { None },
            driver_config: None,
            declared_protocols: entry.protocols.clone(),
        }),
        [] => {
            let mut message = String::from("No J2534 v05.00 adapter found in the Windows registry");
            if let Some(f) = filter {
                message.push_str(&format!(" matching `{f}`"));
            }
            let legacy: Vec<&str> = matching
                .iter()
                .filter(|e| e.api == ApiVersion::V0404)
                .map(|e| e.key.as_str())
                .collect();
            if !legacy.is_empty() {
                message.push_str(&format!(
                    ". Found v04.04-only adapters, not supported yet: {}",
                    legacy.join(", ")
                ));
            }
            message.push_str(". Use --driver <path> or an [adapters] entry in the config file.");
            Err(message)
        }
        several => Err(format!(
            "Several J2534 adapters are installed: {}. Pick one with --device <name> or an [adapters] entry.",
            several.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Picks the device to open from a scan.
///
/// An exact (case-insensitive) name match wins. Without a name, a single
/// device is used, preferring one that reports itself available.
pub fn pick_device<'a>(
    devices: &'a [DeviceInfo],
    wanted: Option<&str>,
) -> Result<&'a DeviceInfo, String> {
    const DEVICE_AVAILABLE: u32 = 1;

    if let Some(name) = wanted {
        if let Some(device) = devices.iter().find(|d| d.name.eq_ignore_ascii_case(name)) {
            return Ok(device);
        }
        if devices.len() != 1 {
            return Err(format!(
                "No device named `{}` (found: {})",
                name,
                device_names(devices)
            ));
        }
    }
    match devices {
        [] => Err(
            "The driver found no device. Is the adapter connected, and is the driver configured?"
                .into(),
        ),
        [only] => Ok(only),
        several => {
            let available: Vec<&DeviceInfo> = several
                .iter()
                .filter(|d| d.available == DEVICE_AVAILABLE)
                .collect();
            match available.as_slice() {
                [only] => Ok(only),
                _ => Err(format!(
                    "Several devices found ({}); pick one with --device",
                    device_names(devices)
                )),
            }
        }
    }
}

fn device_names(devices: &[DeviceInfo]) -> String {
    if devices.is_empty() {
        return "none".into();
    }
    devices
        .iter()
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `opendiag.ini` the OpenDiag driver will read, and why: the adapter's
/// `driver_config`, then `OPENDIAG_DRIVER_CONFIG`, then its system-wide file.
pub fn opendiag_ini_path(
    driver_config: Option<&Path>,
    env: Option<&str>,
) -> (Option<PathBuf>, &'static str) {
    if let Some(path) = driver_config {
        return (Some(path.to_path_buf()), "adapter driver_config");
    }
    if let Some(path) = env.filter(|p| !p.is_empty()) {
        return (Some(PathBuf::from(path)), OPENDIAG_DRIVER_CONFIG_ENV);
    }
    (opendiag_system_ini(), "OpenDiag system file")
}

fn opendiag_system_ini() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(PathBuf::from(
            "/Library/Application Support/OpenDIAG/opendiag.ini",
        ))
    } else if cfg!(windows) {
        std::env::var_os("PROGRAMDATA")
            .map(|dir| PathBuf::from(dir).join("OpenDIAG").join("opendiag.ini"))
    } else {
        Some(PathBuf::from("/etc/opendiag/opendiag.ini"))
    }
}

/// Installed adapters from `HKLM\SOFTWARE\PassThruSupport.05.00` and `.04.04`,
/// read through the 32-bit registry view the spec uses.
#[cfg(windows)]
pub fn registry_entries() -> Vec<RegistryEntry> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY};
    use winreg::RegKey;

    let flags = KEY_READ | KEY_WOW64_32KEY;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let mut entries = Vec::new();
    for (root, api) in [
        ("SOFTWARE\\PassThruSupport.05.00", ApiVersion::V0500),
        ("SOFTWARE\\PassThruSupport.04.04", ApiVersion::V0404),
    ] {
        let Ok(base) = hklm.open_subkey_with_flags(root, flags) else {
            continue;
        };
        for key in base.enum_keys().flatten() {
            let Ok(sub) = base.open_subkey_with_flags(&key, flags) else {
                continue;
            };
            let Ok(library) = sub.get_value::<String, _>("FunctionLibrary") else {
                continue;
            };
            let protocols = PROTOCOL_FLAGS
                .iter()
                .filter_map(|flag| {
                    sub.get_value::<u32, _>(flag)
                        .ok()
                        .map(|v| (flag.to_string(), v != 0))
                })
                .collect();
            entries.push(RegistryEntry {
                name: sub
                    .get_value::<String, _>("Name")
                    .unwrap_or_else(|_| key.clone()),
                vendor: sub.get_value::<String, _>("Vendor").unwrap_or_default(),
                key,
                api,
                library: PathBuf::from(library),
                protocols,
            });
        }
    }
    entries
}

/// The registry only exists on Windows.
#[cfg(not(windows))]
pub fn registry_entries() -> Vec<RegistryEntry> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn inputs<'a>(config: &'a Config, registry: &'a [RegistryEntry]) -> SelectionInputs<'a> {
        SelectionInputs {
            driver_flag: None,
            device_flag: None,
            adapter_flag: None,
            env_library: None,
            config,
            registry,
        }
    }

    fn sample_config() -> Config {
        config::parse(
            r#"
            default_adapter = "bench"
            [adapters.bench]
            driver = "/opt/bench.so"
            driver_config = "/opt/bench.ini"
            device = "Bench"
            [adapters.spare]
            driver = "/opt/spare.so"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn driver_flag_beats_everything() {
        let config = sample_config();
        let mut i = inputs(&config, &[]);
        i.driver_flag = Some(Path::new("/flag.so"));
        i.env_library = Some("/env.so");
        i.adapter_flag = Some("spare");
        i.device_flag = Some("Dev");

        let s = select(&i).unwrap();
        assert_eq!(
            (s.library, s.source),
            (PathBuf::from("/flag.so"), LibrarySource::Flag)
        );
        assert_eq!(s.device.as_deref(), Some("Dev"));
        assert_eq!(
            s.driver_config, None,
            "driver_config belongs to a config adapter"
        );
    }

    #[test]
    fn env_beats_the_config() {
        let config = sample_config();
        let mut i = inputs(&config, &[]);
        i.env_library = Some("/env.so");
        i.adapter_flag = Some("spare");

        let s = select(&i).unwrap();
        assert_eq!(
            (s.library, s.source),
            (PathBuf::from("/env.so"), LibrarySource::Environment)
        );
    }

    #[test]
    fn adapter_flag_beats_the_default_adapter() {
        let config = sample_config();
        let mut i = inputs(&config, &[]);
        i.adapter_flag = Some("spare");

        let s = select(&i).unwrap();
        assert_eq!(s.library, PathBuf::from("/opt/spare.so"));
        assert_eq!(s.source, LibrarySource::ConfigAdapter("spare".into()));
    }

    #[test]
    fn default_adapter_carries_its_device_and_driver_config() {
        let config = sample_config();
        let s = select(&inputs(&config, &[])).unwrap();
        assert_eq!(s.library, PathBuf::from("/opt/bench.so"));
        assert_eq!(s.device.as_deref(), Some("Bench"));
        assert_eq!(s.driver_config, Some(PathBuf::from("/opt/bench.ini")));
    }

    #[test]
    fn device_flag_overrides_the_adapter_device() {
        let config = sample_config();
        let mut i = inputs(&config, &[]);
        i.device_flag = Some("Other");
        assert_eq!(select(&i).unwrap().device.as_deref(), Some("Other"));
    }

    #[test]
    fn an_unknown_adapter_lists_the_known_ones() {
        let config = sample_config();
        let mut i = inputs(&config, &[]);
        i.adapter_flag = Some("nope");
        let err = select(&i).unwrap_err();
        assert!(err.contains("bench, spare"), "{err}");
    }

    #[cfg(not(windows))]
    #[test]
    fn without_config_unix_loads_opendiag_by_name() {
        let config = Config::default();
        let s = select(&inputs(&config, &[])).unwrap();
        assert_eq!(s.source, LibrarySource::SystemLoader);
        assert_eq!(s.library, PathBuf::from(default_library_name().unwrap()));
    }

    fn device(name: &str, available: u32) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            available,
            dll_fw_status: 1,
        }
    }

    #[test]
    fn picks_the_named_device_or_the_only_one() {
        let one = [device("OpenDIAG", 1)];
        assert_eq!(pick_device(&one, None).unwrap().name, "OpenDIAG");
        assert_eq!(
            pick_device(&one, Some("opendiag")).unwrap().name,
            "OpenDIAG"
        );

        let two = [device("A", 2), device("B", 1)];
        assert_eq!(pick_device(&two, Some("a")).unwrap().name, "A");
        assert_eq!(
            pick_device(&two, None).unwrap().name,
            "B",
            "the available one"
        );
        assert!(pick_device(&two, Some("C")).unwrap_err().contains("A, B"));

        let busy = [device("A", 2), device("B", 2)];
        assert!(pick_device(&busy, None).unwrap_err().contains("--device"));
        assert!(pick_device(&[], None).unwrap_err().contains("no device"));
    }

    #[test]
    fn opendiag_ini_follows_driver_config_then_env_then_system() {
        let (path, why) = opendiag_ini_path(Some(Path::new("/a.ini")), Some("/b.ini"));
        assert_eq!(
            (path, why),
            (Some(PathBuf::from("/a.ini")), "adapter driver_config")
        );
        let (path, why) = opendiag_ini_path(None, Some("/b.ini"));
        assert_eq!(
            (path, why),
            (Some(PathBuf::from("/b.ini")), OPENDIAG_DRIVER_CONFIG_ENV)
        );
        let (_, why) = opendiag_ini_path(None, None);
        assert_eq!(why, "OpenDiag system file");
    }

    fn registry_entry(key: &str, api: ApiVersion) -> RegistryEntry {
        RegistryEntry {
            key: key.into(),
            api,
            name: key.split(" - ").last().unwrap().into(),
            vendor: key.split(" - ").next().unwrap().into(),
            library: PathBuf::from(format!("C:\\{key}.dll")),
            protocols: vec![("CAN".into(), true)],
        }
    }

    /// Registry rules are exercised directly, since `select` only reaches them
    /// on Windows.
    #[test]
    fn registry_selection_prefers_v0500_and_explains_v0404() {
        let registry = [
            registry_entry("OpenDIAG - OpenDIAG", ApiVersion::V0500),
            registry_entry("Acme - OldBox", ApiVersion::V0404),
        ];

        let s = select_registry(None, None, &registry).unwrap();
        assert_eq!(
            s.source,
            LibrarySource::Registry("OpenDIAG - OpenDIAG".into())
        );
        assert_eq!(s.declared_protocols, vec![("CAN".to_string(), true)]);

        let err = select_registry(Some("acme"), None, &registry).unwrap_err();
        assert!(
            err.contains("v04.04") && err.contains("Acme - OldBox"),
            "{err}"
        );

        let two = [
            registry[0].clone(),
            registry_entry("Drew - Mongoose", ApiVersion::V0500),
        ];
        assert!(select_registry(None, None, &two)
            .unwrap_err()
            .contains("--device"));
        let s = select_registry(None, Some("mongoose"), &two).unwrap();
        assert_eq!(s.library, PathBuf::from("C:\\Drew - Mongoose.dll"));
    }

    #[test]
    fn registry_entries_match_by_key_name_or_vendor() {
        let entry = registry_entry("Drew Technologies Inc. - MongoosePro", ApiVersion::V0500);
        assert!(entry.matches("Drew Technologies Inc. - MongoosePro"));
        assert!(entry.matches("mongoosepro"));
        assert!(entry.matches("drew"));
        assert!(!entry.matches("tactrix"));
    }
}
