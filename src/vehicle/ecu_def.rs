use super::steps::Step;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Translation {
    pub title: Option<String>,
    pub description: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(untagged)]
pub enum IdentitySource {
    Data { data: u16 },
    Obd { obd: u8 },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(try_from = "RawIdentityMatch")]
pub struct IdentityMatch {
    #[serde(flatten)]
    pub source: IdentitySource,
    pub pattern: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIdentityMatch {
    data: Option<u16>,
    obd: Option<u8>,
    pattern: String,
}

impl TryFrom<RawIdentityMatch> for IdentityMatch {
    type Error = &'static str;
    fn try_from(raw: RawIdentityMatch) -> Result<Self, Self::Error> {
        let source = match (raw.data, raw.obd) {
            (Some(data), None) => IdentitySource::Data { data },
            (None, Some(obd @ (2 | 4))) => IdentitySource::Obd { obd },
            _ => {
                return Err(
                    "identity match requires either data=DID or obd=2 (VIN)/4 (calibration ID)",
                )
            }
        };
        Ok(Self {
            source,
            pattern: raw.pattern,
        })
    }
}

/// Flash expectations remain manufacturer data identifiers.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataMatch {
    pub data: u16,
    pub pattern: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Safety {
    ReadOnly,
    ChangesSettings,
    Destructive,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Untested,
    Verified,
}
impl std::fmt::Display for Safety {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReadOnly => "read-only",
            Self::ChangesSettings => "changes-settings",
            Self::Destructive => "destructive",
        })
    }
}
impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Untested => "untested",
            Self::Verified => "verified",
        })
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlashDef {
    pub strategy: String,
    pub status: Status,
    #[serde(default)]
    pub verified_on: Vec<String>,
    pub start_address: Option<u32>,
    #[serde(default)]
    pub expect: Vec<DataMatch>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub translations: BTreeMap<String, Translation>,
    pub safety: Safety,
    pub status: Status,
    #[serde(default)]
    pub verified_on: Vec<String>,
    pub steps: Option<Vec<Step>>,
    pub run: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignalDef {
    pub field: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}
fn one() -> f64 {
    1.0
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EcuDef {
    pub title: String,
    #[serde(default)]
    pub translations: BTreeMap<String, Translation>,
    pub diag: Option<String>,
    pub extends: Option<String>,
    #[serde(default, rename = "match")]
    pub matches: Vec<IdentityMatch>,
    pub flash: Option<FlashDef>,
    #[serde(default)]
    pub security: BTreeMap<String, String>,
    #[serde(default, rename = "signal")]
    pub signals: Vec<SignalDef>,
    #[serde(default, rename = "service")]
    pub services: BTreeMap<String, ServiceDef>,
    #[serde(default)]
    pub dtc: BTreeMap<String, String>,
}
