//! Adapter choices and configuration-file discovery for the CLI.
//!
//! The CLI loads `config.toml` into plain values for
//! [`crate::j2534::discovery`]. Diagnostic workflows receive the selected
//! adapter and bus rather than reading the configuration file themselves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::Value;

/// The schema version this build understands.
pub const CONFIG_VERSION: i64 = 1;

/// Environment variable that overrides the config file location.
pub const CONFIG_ENV: &str = "OPENDIAG_CONFIG";

/// Top-level sections reserved for later phases. Accepted silently so a newer
/// config file doesn't produce noise on an older build.
const RESERVED_SECTIONS: &[&str] = &["bus", "vehicle", "log", "units", "datalog"];

/// One `[adapters.<name>]` entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdapterEntry {
    /// Path to the J2534 library to load.
    pub driver: Option<PathBuf>,
    /// For the OpenDiag driver: its `opendiag.ini`, passed on through
    /// `OPENDIAG_DRIVER_CONFIG`.
    pub driver_config: Option<PathBuf>,
    /// J2534 device name to open, when a library offers several.
    pub device: Option<String>,
    /// Windows only: a `PassThruSupport` registry key or `Name` to use instead
    /// of a path.
    pub registry: Option<String>,
}

/// The parsed file plus where it came from.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub path: PathBuf,
    /// False when the file doesn't exist; every value is then a default.
    pub exists: bool,
    pub default_adapter: Option<String>,
    pub adapters: BTreeMap<String, AdapterEntry>,
    /// Unknown keys, reported to the user but not fatal.
    pub warnings: Vec<String>,
}

/// Where a config path came from, for `config show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    Flag,
    Environment,
    Default,
}

impl PathSource {
    pub fn describe(self) -> &'static str {
        match self {
            PathSource::Flag => "--config",
            PathSource::Environment => CONFIG_ENV,
            PathSource::Default => "default location",
        }
    }
}

/// The OS-standard location: `~/.config/opendiag/config.toml` on Linux,
/// `~/Library/Application Support/opendiag/config.toml` on macOS and
/// `%APPDATA%\opendiag\config.toml` on Windows.
pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("opendiag").join("config.toml"))
}

/// Picks the config file: `--config`, then `OPENDIAG_CONFIG`, then the default.
pub fn resolve_path(
    flag: Option<&Path>,
    env: Option<&str>,
) -> Result<(PathBuf, PathSource), String> {
    if let Some(path) = flag {
        return Ok((expand_tilde(path), PathSource::Flag));
    }
    if let Some(path) = env.filter(|p| !p.is_empty()) {
        return Ok((expand_tilde(Path::new(path)), PathSource::Environment));
    }
    default_path()
        .map(|p| (p, PathSource::Default))
        .ok_or_else(|| "Cannot find the user's config folder; pass --config".to_string())
}

/// Expands a leading `~` to the home directory. Config files are hand-written,
/// and `~` is the natural way to write a path there.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        if let Some(home) = dirs::home_dir() {
            return home.join(text[1..].trim_start_matches(['/', '\\']));
        }
    }
    path.to_path_buf()
}

/// Loads the file. A missing file is fine (all defaults); a malformed one is
/// an error naming the file and line.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config {
                path: path.to_path_buf(),
                ..Config::default()
            });
        }
        Err(e) => return Err(format!("Cannot read {}: {}", path.display(), e)),
    };
    let mut config = parse(&text).map_err(|e| format!("{}: {}", path.display(), e))?;
    config.path = path.to_path_buf();
    config.exists = true;
    Ok(config)
}

/// Parses config text. Separate from [`load`] so tests needn't touch disk.
pub fn parse(text: &str) -> Result<Config, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut config = Config::default();

    for (key, value) in &table {
        match key.as_str() {
            "version" => {
                let version = value.as_integer().ok_or("`version` must be a number")?;
                if version > CONFIG_VERSION {
                    return Err(format!(
                        "config version {version} is newer than this build understands ({CONFIG_VERSION})"
                    ));
                }
            }
            "default_adapter" => {
                config.default_adapter = Some(
                    value
                        .as_str()
                        .ok_or("`default_adapter` must be a string")?
                        .to_string(),
                );
            }
            "adapters" => {
                let adapters = value.as_table().ok_or("`adapters` must be a table")?;
                for (name, entry) in adapters {
                    let entry = parse_adapter(name, entry, &mut config.warnings)?;
                    config.adapters.insert(name.clone(), entry);
                }
            }
            reserved if RESERVED_SECTIONS.contains(&reserved) => {}
            other => config.warnings.push(format!("unknown key `{other}`")),
        }
    }

    if let Some(name) = &config.default_adapter {
        if !config.adapters.contains_key(name) {
            return Err(format!(
                "default_adapter `{name}` has no [adapters.{name}] section"
            ));
        }
    }
    Ok(config)
}

fn parse_adapter(
    name: &str,
    value: &Value,
    warnings: &mut Vec<String>,
) -> Result<AdapterEntry, String> {
    let table = value
        .as_table()
        .ok_or_else(|| format!("[adapters.{name}] must be a table"))?;
    let text = |key: &str, value: &Value| -> Result<String, String> {
        value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("[adapters.{name}] `{key}` must be a string"))
    };

    let mut entry = AdapterEntry::default();
    for (key, value) in table {
        match key.as_str() {
            "driver" => entry.driver = Some(expand_tilde(Path::new(&text(key, value)?))),
            "driver_config" => {
                entry.driver_config = Some(expand_tilde(Path::new(&text(key, value)?)))
            }
            "device" => entry.device = Some(text(key, value)?),
            "registry" => entry.registry = Some(text(key, value)?),
            other => warnings.push(format!("unknown key `{other}` in [adapters.{name}]")),
        }
    }
    if entry.driver.is_some() && entry.registry.is_some() {
        return Err(format!(
            "[adapters.{name}] sets both `driver` and `registry`; use one"
        ));
    }
    Ok(entry)
}

/// The commented starting point `config init` writes.
pub const TEMPLATE: &str = r#"# opendiag configuration
version = 1

# Adapter used when --adapter is not given.
# default_adapter = "opendiag"

# One section per adapter. Pick one with --adapter <name>.
#
# [adapters.opendiag]
# driver = "~/path/to/libopendiag.dylib"         # J2534 v05.00 library
# driver_config = "~/path/to/opendiag.ini"       # OpenDiag only: its opendiag.ini
# device = "OpenDIAG"                            # optional: device name to open
#
# [adapters.shop]                                # Windows: an installed adapter
# registry = "OpenDIAG - OpenDIAG"               # PassThruSupport key or Name
"#;

/// Writes [`TEMPLATE`] to `path`, refusing to replace an existing file.
pub fn init(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists; not overwriting it",
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Cannot create {}: {}", dir.display(), e))?;
    }
    std::fs::write(path, TEMPLATE).map_err(|e| format!("Cannot write {}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adapters_and_the_default() {
        let config = parse(
            r#"
            version = 1
            default_adapter = "bench"
            [adapters.bench]
            driver = "/opt/libopendiag.so"
            driver_config = "/opt/opendiag.ini"
            device = "OpenDIAG"
            [adapters.shop]
            registry = "Vendor - Device"
            "#,
        )
        .unwrap();

        assert_eq!(config.default_adapter.as_deref(), Some("bench"));
        assert_eq!(
            config.adapters["bench"],
            AdapterEntry {
                driver: Some("/opt/libopendiag.so".into()),
                driver_config: Some("/opt/opendiag.ini".into()),
                device: Some("OpenDIAG".into()),
                registry: None,
            }
        );
        assert_eq!(
            config.adapters["shop"].registry.as_deref(),
            Some("Vendor - Device")
        );
        assert!(config.warnings.is_empty());
    }

    #[test]
    fn unknown_keys_warn_and_reserved_sections_are_accepted() {
        let config = parse(
            "colour = \"blue\"\n[units]\nspeed = \"kph\"\n[adapters.a]\ndriver = \"/x\"\nspeed = 1\n",
        )
        .unwrap();

        let mut warnings = config.warnings.clone();
        warnings.sort();
        assert_eq!(
            warnings,
            vec![
                "unknown key `colour`",
                "unknown key `speed` in [adapters.a]"
            ]
        );
    }

    #[test]
    fn malformed_files_name_the_line() {
        let err = parse("version = 1\n[adapters.a\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn rejects_inconsistent_settings() {
        assert!(parse("default_adapter = \"missing\"")
            .unwrap_err()
            .contains("missing"));
        assert!(parse("[adapters.a]\ndriver = \"/x\"\nregistry = \"y\"")
            .unwrap_err()
            .contains("both"));
        assert!(parse("version = 2").unwrap_err().contains("newer"));
        assert!(parse("[adapters.a]\ndriver = 5")
            .unwrap_err()
            .contains("must be a string"));
    }

    #[test]
    fn a_missing_file_is_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = load(&dir.path().join("config.toml")).unwrap();
        assert!(!config.exists);
        assert!(config.adapters.is_empty());
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde(Path::new("~/lib/x.so")), home.join("lib/x.so"));
        assert_eq!(
            expand_tilde(Path::new("/abs/x.so")),
            PathBuf::from("/abs/x.so")
        );
    }

    #[test]
    fn config_path_precedence_is_flag_then_env_then_default() {
        let (path, source) = resolve_path(Some(Path::new("/f.toml")), Some("/e.toml")).unwrap();
        assert_eq!((path, source), (PathBuf::from("/f.toml"), PathSource::Flag));

        let (path, source) = resolve_path(None, Some("/e.toml")).unwrap();
        assert_eq!(
            (path, source),
            (PathBuf::from("/e.toml"), PathSource::Environment)
        );

        let (_, source) = resolve_path(None, None).unwrap();
        assert_eq!(source, PathSource::Default);
    }

    #[test]
    fn init_writes_a_parseable_template_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");

        init(&path).unwrap();
        let config = load(&path).unwrap();
        assert!(config.exists && config.warnings.is_empty());

        assert!(init(&path).unwrap_err().contains("already exists"));
    }
}
