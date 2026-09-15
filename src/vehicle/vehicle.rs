use super::years::YearRange;
use crate::{
    link::{BusKind, BusSpec, IdWidth},
    obd::addressing::EcuAddress,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Nominal battery voltage, serialized as 12 or 24. Defaults to 12 V.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "u8", into = "u8")]
pub enum SystemVoltage {
    #[default]
    V12,
    V24,
}
impl SystemVoltage {
    pub fn volts(self) -> u8 {
        match self {
            Self::V12 => 12,
            Self::V24 => 24,
        }
    }
}
impl TryFrom<u8> for SystemVoltage {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            12 => Ok(Self::V12),
            24 => Ok(Self::V24),
            _ => Err("system_voltage must be 12 or 24"),
        }
    }
}
impl From<SystemVoltage> for u8 {
    fn from(value: SystemVoltage) -> Self {
        value.volts()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusDef {
    pub protocol: String,
    pub bitrate: Option<u32>,
    pub pins: Option<Vec<u32>>,
    pub init_address: Option<u8>,
    #[serde(default)]
    pub timing: BTreeMap<String, u32>,
}
impl BusDef {
    pub fn spec(&self) -> Result<BusSpec, String> {
        let mut spec = BusSpec::from_cli(&self.protocol, self.bitrate)?;
        if self.init_address.is_some()
            && !matches!(spec.kind, BusKind::Iso9141 | BusKind::Iso14230 { .. })
        {
            return Err("init_address is only valid on K-Line".into());
        }
        if let Some(pins) = &self.pins {
            if pins.is_empty() || pins.iter().any(|pin| !(1..=16).contains(pin)) {
                return Err("pins must contain J1962 pins 1..16".into());
            }
            spec.pins = pins.clone();
        }
        if let Some(address) = self.init_address {
            spec.init_address = Some(address);
        }
        for key in self.timing.keys() {
            if !matches!(
                key.as_str(),
                "P1_MIN"
                    | "P1_MAX"
                    | "P2_MIN"
                    | "P2_MAX"
                    | "P3_MIN"
                    | "P3_MAX"
                    | "P4_MIN"
                    | "P4_MAX"
                    | "P2_STAR"
            ) {
                return Err(format!("unknown timing parameter {key}"));
            }
        }
        spec.timing = self.timing.clone();
        Ok(spec)
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum EcuDefinitions {
    One(String),
    Many(Vec<String>),
}
impl EcuDefinitions {
    pub fn ids(&self) -> &[String] {
        match self {
            Self::One(id) => std::slice::from_ref(id),
            Self::Many(ids) => ids,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSlot {
    pub label: Option<String>,
    pub uses: Option<EcuDefinitions>,
    pub bus: String,
    pub request: Option<u32>,
    pub response: Option<u32>,
    pub address: Option<u8>,
    pub years: Option<Vec<YearRange>>,
}
impl ModuleSlot {
    /// ECU definitions linked by this module; empty for display-only metadata.
    pub fn definition_ids(&self) -> &[String] {
        self.uses.as_ref().map_or(&[], EcuDefinitions::ids)
    }
    pub fn address(&self, bus: &BusSpec) -> Result<EcuAddress, String> {
        module_address(bus, self.request, self.response, self.address)
    }
}

fn module_address(
    bus: &BusSpec,
    request: Option<u32>,
    response: Option<u32>,
    address: Option<u8>,
) -> Result<EcuAddress, String> {
    if bus.is_can() {
        if address.is_some() {
            return Err("CAN slot must not have address".into());
        }
        let request = request.ok_or("CAN slot requires request")?;
        let maximum = match bus.kind {
            BusKind::IsoTp {
                id: IdWidth::Bits11,
            } => 0x7FF,
            _ => 0x1FFFFFFF,
        };
        if request > maximum {
            return Err("request exceeds the bus header width".into());
        }
        if response.is_none() {
            bus.addressing().validate_request_header(request)?;
        }
        let response = response
            .or_else(|| bus.addressing().response_for_request(request))
            .ok_or("request has no standard response pair; specify response")?;
        if response > maximum {
            return Err("response exceeds the bus header width".into());
        }
        Ok(EcuAddress::Can { request, response })
    } else {
        if request.is_some() || response.is_some() {
            return Err("legacy slot uses address, not request/response".into());
        }
        Ok(EcuAddress::Byte(
            address.ok_or("legacy slot requires address")?,
        ))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ModuleVersions {
    One(ModuleSlot),
    Many(Vec<ModuleSlot>),
}
impl ModuleVersions {
    pub fn entries(&self) -> &[ModuleSlot] {
        match self {
            Self::One(slot) => std::slice::from_ref(slot),
            Self::Many(slots) => slots,
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vehicle {
    pub make: String,
    pub model: String,
    pub chassis: Option<String>,
    pub years: Vec<YearRange>,
    #[serde(default)]
    pub system_voltage: SystemVoltage,
    #[serde(default)]
    pub markets: Vec<String>,
    #[serde(default)]
    pub vin: Vec<String>,
    #[serde(rename = "bus")]
    pub buses: BTreeMap<String, BusDef>,
    #[serde(rename = "module")]
    pub modules: BTreeMap<String, ModuleVersions>,
}
