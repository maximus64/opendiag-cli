//! Embedded definitions supply vehicle functions without frontend changes.
pub mod catalog;
pub mod ecu_def;
pub mod hooks;
pub mod steps;
#[allow(clippy::module_inception)] // File layout follows the two-layer catalog: vehicle/vehicle.rs.
pub mod vehicle;
pub mod years;
use crate::{
    diag::{self, DataId},
    events::{Cancel, Event, Interaction},
    link::BusSpec,
    obd::addressing::EcuAddress,
    preflight::{CheckStatus, Preflight},
    sampler::SignalInfo,
    DiagError, ObdProtocol,
};
use catalog::Catalog;
use ecu_def::{EcuDef, Safety, Status};
use std::collections::{BTreeMap, BTreeSet};
pub use vehicle::SystemVoltage;
use vehicle::{ModuleSlot, Vehicle};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VariantSelection {
    /// Independent candidates have no definition until an identity guard matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    pub matched: bool,
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolvedEcu {
    pub name: String,
    pub address: EcuAddress,
    pub bus: BusSpec,
    pub selection: VariantSelection,
    pub definition: EcuDef,
}
pub struct SelectedVehicle<'a> {
    pub id: String,
    pub vehicle: Vehicle,
    pub year: Option<u16>,
    pub catalog: &'a Catalog,
    resolved: BTreeMap<String, ResolvedEcu>,
    restored: BTreeSet<String>,
}
impl<'a> SelectedVehicle<'a> {
    pub fn new(catalog: &'a Catalog, id: &str, year: Option<u16>) -> Result<Self, DiagError> {
        if !cfg!(test) {
            catalog.validate_vehicle(id)?;
        }
        let vehicle = catalog.vehicle(id)?;
        if year.is_some_and(|year| !years::contains(&vehicle.years, year)) {
            return Err(DiagError::InvalidCommand(format!(
                "Model year is outside {} for {id}",
                years::format(&vehicle.years)
            )));
        }
        Ok(Self {
            id: id.into(),
            vehicle,
            year,
            catalog,
            resolved: BTreeMap::new(),
            restored: BTreeSet::new(),
        })
    }
    /// Select diagnostic modules; labels alone never grant ECU capabilities.
    pub fn slots(&self) -> Result<BTreeMap<String, ModuleSlot>, DiagError> {
        let mut result = BTreeMap::new();
        for (name, versions) in &self.vehicle.modules {
            if versions.entries().iter().all(|slot| slot.uses.is_none()) {
                continue;
            }
            let applicable: Vec<_> = versions
                .entries()
                .iter()
                .filter(|slot| {
                    self.year.is_none_or(|year| {
                        let ranges = slot.years.as_deref().unwrap_or(&self.vehicle.years);
                        years::contains(ranges, year)
                    })
                })
                .collect();
            match applicable.as_slice() {
                [slot] if slot.uses.is_some() => {
                    result.insert(name.clone(), (*slot).clone());
                }
                [] | [_] => {}
                _ => return Err(DiagError::InvalidCommand(format!(
                    "Module {name} has several model-year definitions. Identify the vehicle VIN to choose one."
                ))),
            }
        }
        Ok(result)
    }
    /// Display metadata never resolves variants or grants diagnostic capabilities.
    pub fn module_label_on(&self, bus: &BusSpec, address: EcuAddress) -> Option<&str> {
        let applicable = |bus_name: &str, ranges: Option<&[years::YearRange]>| {
            let year_matches = match (self.year, ranges) {
                (Some(year), Some(ranges)) => years::contains(ranges, year),
                (None, Some(_)) => false,
                (_, None) => true,
            };
            year_matches
                && self
                    .vehicle
                    .buses
                    .get(bus_name)
                    .and_then(|b| b.spec().ok())
                    .as_ref()
                    == Some(bus)
        };
        let mut labels = self
            .vehicle
            .modules
            .values()
            .flat_map(|versions| versions.entries())
            .filter(|slot| {
                applicable(&slot.bus, slot.years.as_deref())
                    && slot.address(bus).ok() == Some(address)
            })
            .filter_map(|slot| slot.label.as_deref());
        let first = labels.next()?;
        labels.all(|label| label == first).then_some(first)
    }
    /// An unspecified bitrate follows the detected OBD bus; explicit data stays authoritative.
    pub fn bind_obd_bus(&mut self, bus: &BusSpec) -> Result<(), DiagError> {
        for definition in self.vehicle.buses.values_mut() {
            let spec = definition.spec().map_err(DiagError::InvalidFile)?;
            if spec.kind == bus.kind && definition.bitrate.is_none() {
                definition.bitrate = Some(bus.bitrate);
            }
        }
        Ok(())
    }
    pub fn initial_bus(&self) -> Result<Option<BusSpec>, DiagError> {
        if self.needs_year() {
            let implemented: Vec<_> = self
                .vehicle
                .buses
                .values()
                .map(|bus| bus.spec().map_err(DiagError::InvalidFile))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(BusSpec::is_implemented)
                .collect();
            return Ok(if implemented.len() == 1 {
                implemented.into_iter().next()
            } else {
                None
            });
        }
        let ecus = self.current_ecus()?;
        Ok(ecus
            .iter()
            .find(|ecu| ecu.name == "engine" && ecu.bus.is_implemented())
            .or_else(|| {
                ecus.iter().find(|ecu| {
                    ecu.definition.diag.as_deref() == Some("obd") && ecu.bus.is_implemented()
                })
            })
            .or_else(|| ecus.iter().find(|ecu| ecu.bus.is_implemented()))
            .or_else(|| ecus.first())
            .map(|ecu| ecu.bus.clone()))
    }
    pub fn needs_year(&self) -> bool {
        self.year.is_none()
            && self.vehicle.modules.values().any(|versions| {
                versions.entries().len() > 1
                    && versions.entries().iter().any(|slot| slot.uses.is_some())
            })
    }
    pub fn set_year_from_vin(&mut self, vin: &crate::identify::VinInfo) -> Result<(), DiagError> {
        let years: Vec<_> = vin
            .possible_years
            .iter()
            .filter(|year| years::contains(&self.vehicle.years, **year))
            .copied()
            .collect();
        if years.len() != 1 {
            return Err(DiagError::InvalidCommand(format!(
                "VIN year does not uniquely fit selected vehicle {}",
                self.id
            )));
        }
        self.year = Some(years[0]);
        Ok(())
    }
    pub fn base_ecu(&self, name: &str) -> Result<ResolvedEcu, DiagError> {
        let mut slots = self.slots()?;
        let slot = slots.remove(name).ok_or_else(|| {
            DiagError::InvalidCommand(format!(
                "Unknown ECU {name}; choices: {}",
                slots.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })?;
        let bus = self.vehicle.buses[&slot.bus]
            .spec()
            .map_err(DiagError::InvalidCommand)?;
        let address = slot.address(&bus).map_err(DiagError::InvalidCommand)?;
        let ids = slot.definition_ids();
        let definitions = ids
            .iter()
            .map(|id| self.catalog.definition(id))
            .collect::<Result<Vec<_>, _>>()?;
        let first = definitions.first().ok_or_else(|| {
            DiagError::InvalidFile(format!("ECU {name} has no candidate definitions"))
        })?;
        let definition = if definitions.len() == 1 {
            first.clone()
        } else {
            // Only agreed probing/staging settings are available before identification.
            let flash = first.flash.as_ref().filter(|first| {
                definitions.iter().all(|definition| {
                    definition.flash.as_ref().is_some_and(|flash| {
                        flash.strategy.eq_ignore_ascii_case(&first.strategy)
                            && flash.start_address == first.start_address
                    })
                })
            });
            EcuDef {
                title: "Unidentified ECU".into(),
                diag: first.diag.clone(),
                flash: flash.map(|flash| ecu_def::FlashDef {
                    strategy: flash.strategy.clone(),
                    start_address: flash.start_address,
                    status: Status::Untested,
                    verified_on: Vec::new(),
                    expect: Vec::new(),
                }),
                ..Default::default()
            }
        };
        Ok(ResolvedEcu {
            name: name.into(),
            address,
            bus,
            selection: VariantSelection {
                definition: (ids.len() == 1).then(|| ids[0].clone()),
                matched: false,
            },
            definition,
        })
    }
    pub fn ecu_name(&self, value: &str) -> Result<String, DiagError> {
        if self.vehicle.modules.get(value).is_some_and(|versions| {
            versions.entries().iter().any(|slot| {
                slot.uses.is_some()
                    && self.year.is_none_or(|year| {
                        years::contains(slot.years.as_deref().unwrap_or(&self.vehicle.years), year)
                    })
            })
        }) {
            return Ok(value.into());
        }
        let numeric = crate::parse_hex_u32(value).map_err(|_| {
            DiagError::InvalidCommand(format!(
                "Unknown ECU {value}; use a slot name or its full bus address"
            ))
        })?;
        let mut matches = Vec::new();
        for (name, versions) in &self.vehicle.modules {
            for slot in versions.entries() {
                if slot.uses.is_none() {
                    continue;
                }
                if let Some(year) = self.year {
                    let ranges = slot.years.as_deref().unwrap_or(&self.vehicle.years);
                    if !years::contains(ranges, year) {
                        continue;
                    }
                }
                let bus = self.vehicle.buses[&slot.bus]
                    .spec()
                    .map_err(DiagError::InvalidFile)?;
                let request = match slot.address(&bus).map_err(DiagError::InvalidFile)? {
                    EcuAddress::Can { request, .. } => request,
                    EcuAddress::Byte(byte) => byte as u32,
                };
                if numeric == request && !matches.contains(name) {
                    matches.push(name.clone());
                }
            }
        }
        match matches.as_slice() {
            [name] => Ok(name.clone()),
            [] => Err(DiagError::InvalidCommand(format!(
                "ECU {value} is not a slot in {}",
                self.id
            ))),
            _ => Err(DiagError::InvalidCommand(format!(
                "Address {value} appears on several buses: {}; select an ECU name",
                matches.join(", ")
            ))),
        }
    }
    pub fn activate(protocol: &mut ObdProtocol, ecu: &ResolvedEcu) -> Result<(), DiagError> {
        if protocol.bus() != &ecu.bus {
            protocol.emit(Event::Step(&format!(
                "Connecting to {} for {}",
                ecu.bus, ecu.name
            )));
            protocol.switch_bus(&ecu.bus)?;
        }
        protocol.set_ecu(ecu.address)
    }
    pub fn resolve_ecu(
        &mut self,
        name: &str,
        protocol: &mut ObdProtocol,
        cancel: &Cancel,
    ) -> Result<ResolvedEcu, DiagError> {
        if let Some(ecu) = self.resolved.get(name) {
            Self::activate(protocol, ecu)?;
            return Ok(ecu.clone());
        }
        let mut ecu = self.base_ecu(name)?;
        Self::activate(protocol, &ecu)?;
        let slots = self.slots()?;
        let candidates = slots[name].definition_ids();
        let mut rules = BTreeSet::new();
        for candidate in candidates {
            rules.extend(self.catalog.identity_rules(candidate)?);
        }
        let mut identity = BTreeMap::new();
        for source in rules {
            cancel.check()?;
            let result = match source {
                ecu_def::IdentitySource::Data { data } => {
                    diag::services(ecu.definition.diag.as_deref().unwrap_or("obd"), protocol)?
                        .read_data(DataId::Common(data))
                }
                ecu_def::IdentitySource::Obd { obd } => {
                    protocol.read_mode09(ecu.address, obd).and_then(|data| {
                        let record_length = if obd == 2 { 17 } else { 16 };
                        if data.first() != Some(&1) || data.len() != record_length + 1 {
                            return Err(DiagError::InvalidResponse(
                                "Identity matching requires one complete Mode 09 record".into(),
                            ));
                        }
                        Ok(data[1..].to_vec())
                    })
                }
            };
            match result {
                Ok(bytes) => {
                    identity.insert(source, diag::text(&bytes));
                }
                Err(
                    DiagError::UdsNegativeResponse { .. }
                    | DiagError::Timeout(_)
                    | DiagError::InvalidResponse(_),
                ) => {}
                Err(e) => return Err(e),
            }
        }
        let mut matches = Vec::new();
        for candidate in candidates {
            let (definition, matched) = self.catalog.match_variant(candidate, &identity)?;
            if matched || candidates.len() == 1 {
                matches.push(VariantSelection {
                    definition: Some(definition),
                    matched,
                });
            }
        }
        match matches.as_slice() {
            [] => {}
            [selection] => {
                ecu.definition = self
                    .catalog
                    .definition(selection.definition.as_deref().unwrap())?;
                ecu.selection = selection.clone();
            }
            _ => {
                return Err(DiagError::InvalidFile(format!(
                    "ECU {name}: multiple candidate definitions match"
                )))
            }
        }
        self.resolved.insert(name.into(), ecu.clone());
        Ok(ecu)
    }
    pub fn variants(&self) -> BTreeMap<String, VariantSelection> {
        self.resolved
            .iter()
            .map(|(name, ecu)| (name.clone(), ecu.selection.clone()))
            .collect()
    }
    /// Cached identities may be reused for reads; writes recheck the live ECU first.
    pub fn resolve_for_write(
        &mut self,
        name: &str,
        protocol: &mut ObdProtocol,
        cancel: &Cancel,
    ) -> Result<ResolvedEcu, DiagError> {
        if self.restored.remove(name) {
            self.resolved.remove(name);
        }
        self.resolve_ecu(name, protocol, cancel)
    }
    pub fn restore_variants(
        &mut self,
        variants: &BTreeMap<String, VariantSelection>,
    ) -> Result<(), DiagError> {
        for (name, selection) in variants {
            let mut ecu = self.base_ecu(name)?;
            let slots = self.slots()?;
            let candidates = slots[name].definition_ids();
            let Some(definition) = &selection.definition else {
                if selection.matched || candidates.len() == 1 {
                    return Err(DiagError::InvalidFile(
                        "Cached ECU selection is missing its definition".into(),
                    ));
                }
                self.resolved.insert(name.clone(), ecu);
                self.restored.insert(name.clone());
                continue;
            };
            if candidates.len() > 1 && !selection.matched {
                return Err(DiagError::InvalidFile(
                    "Cached independent candidate is not matched".into(),
                ));
            }
            let mut cursor = definition.clone();
            let mut seen = std::collections::BTreeSet::new();
            while !candidates.contains(&cursor) {
                if !seen.insert(cursor.clone()) {
                    return Err(DiagError::InvalidFile(
                        "Cached variant inheritance cycle".into(),
                    ));
                }
                cursor = self.catalog.definition(&cursor)?.extends.ok_or_else(|| {
                    DiagError::InvalidFile(
                        "Cached variant is not in this vehicle's ECU family".into(),
                    )
                })?;
            }
            ecu.definition = self.catalog.definition(definition)?;
            ecu.selection = selection.clone();
            self.resolved.insert(name.clone(), ecu);
            self.restored.insert(name.clone());
        }
        Ok(())
    }
    pub fn current_ecus(&self) -> Result<Vec<ResolvedEcu>, DiagError> {
        self.slots()?
            .keys()
            .map(|name| {
                self.resolved
                    .get(name)
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| self.base_ecu(name))
            })
            .collect()
    }
    pub fn service_names(&self) -> Result<Vec<String>, DiagError> {
        Ok(self
            .current_ecus()?
            .into_iter()
            .flat_map(|ecu| {
                ecu.definition
                    .services
                    .keys()
                    .map(|name| format!("{}.{name}", ecu.name))
                    .collect::<Vec<_>>()
            })
            .collect())
    }
    pub fn signal_names(&self) -> Result<Vec<String>, DiagError> {
        let mut names = Vec::new();
        for ecu in self.current_ecus()? {
            for signal in ecu.definition.signals {
                names.push(format!(
                    "{}.{}",
                    ecu.name,
                    crate::datalog::parse_field(&signal.field)
                        .map_err(DiagError::InvalidCommand)?
                        .name
                ));
            }
        }
        Ok(names)
    }
    pub fn resolve_signal(&self, name: &str) -> Result<SignalInfo, DiagError> {
        let qualified = resolve_name(name, &self.signal_names()?)?;
        let (slot, short) = qualified.split_once('.').unwrap();
        let ecu = self
            .current_ecus()?
            .into_iter()
            .find(|e| e.name == slot)
            .unwrap();
        let definition = ecu
            .definition
            .signals
            .iter()
            .find(|s| s.field.split('=').next() == Some(short))
            .unwrap();
        let mut signal = SignalInfo::did(
            ecu.address,
            crate::datalog::parse_field(&definition.field).map_err(DiagError::InvalidCommand)?,
        );
        signal.name = qualified;
        signal.bus = Some(ecu.bus);
        signal.unit = definition.unit.clone();
        signal.scale = definition.scale;
        signal.offset = definition.offset;
        if let crate::sampler::Signal::Did { diag, .. } = &mut signal.signal {
            *diag = ecu.definition.diag.unwrap_or_else(|| "obd".into());
        }
        Ok(signal)
    }
    pub fn run_service(
        &mut self,
        name: &str,
        protocol: &mut ObdProtocol,
        ui: &mut dyn Interaction,
        cancel: &Cancel,
        allow_untested: bool,
    ) -> Result<(), DiagError> {
        // Resolve each slot once; variant services can introduce or override names.
        for slot in self.slots()?.keys() {
            self.resolve_ecu(slot, protocol, cancel)?;
        }
        let qualified = resolve_name(name, &self.service_names()?)?;
        let (slot, short) = qualified.split_once('.').unwrap();
        let mut ecu = self.resolve_ecu(slot, protocol, cancel)?;
        if ecu.definition.services[short].safety == Safety::Destructive {
            ecu = self.resolve_for_write(slot, protocol, cancel)?;
        }
        let service = ecu.definition.services.get(short).ok_or_else(|| {
            DiagError::InvalidCommand(format!("The live ECU variant does not provide {qualified}"))
        })?;
        if let Some(run) = &service.run {
            if !self.catalog.hooks.implemented(run) {
                return Err(DiagError::Unsupported(format!(
                    "{qualified} is not implemented"
                )));
            }
        }
        if let Some(steps) = &service.steps {
            for step in steps {
                step.validate(&ecu.definition, &self.catalog.hooks)
                    .map_err(DiagError::InvalidFile)?;
            }
        }
        let mut preflight = Preflight::new(&service.title, &service.description);
        preflight.add("vehicle", &self.id, CheckStatus::Info);
        preflight.add(
            "ECU",
            format!(
                "{} {} ({})",
                ecu.name,
                ecu.address,
                ecu.selection
                    .definition
                    .as_deref()
                    .unwrap_or("unidentified")
            ),
            CheckStatus::Info,
        );
        if service.safety == Safety::Destructive && !ecu.selection.matched {
            preflight.add(
                "variant",
                "No matching ECU variant; destructive operation refused",
                CheckStatus::Fail,
            );
        }
        match service.status {
            Status::Untested => preflight.add(
                "status",
                if allow_untested {
                    "untested (--allow-untested)"
                } else {
                    "untested"
                },
                if service.safety == Safety::Destructive && !allow_untested {
                    CheckStatus::Fail
                } else {
                    CheckStatus::Warn
                },
            ),
            Status::Verified => preflight.add(
                "status",
                format!("verified on {}", service.verified_on.join(", ")),
                CheckStatus::Ok,
            ),
        }
        if service.safety == Safety::ReadOnly {
            preflight.validate()?;
            cancel.check()?;
        } else {
            preflight.authorize(ui, cancel)?;
        }
        if let Some(run) = &service.run {
            self.catalog.hooks.run(run, protocol, ui, cancel)
        } else {
            for step in service
                .steps
                .as_ref()
                .ok_or_else(|| DiagError::InvalidFile("Service has no procedure".into()))?
            {
                step.execute(&ecu.definition, protocol, &self.catalog.hooks, ui, cancel)?;
            }
            Ok(())
        }
    }
    pub fn dtc_description(&self, ecu: EcuAddress, code: &str) -> Option<String> {
        let matching: Vec<_> = self
            .current_ecus()
            .ok()?
            .into_iter()
            .filter(|e| e.address == ecu)
            .collect();
        if matching.len() != 1 {
            return None;
        }
        matching[0].definition.dtc.get(code).cloned()
    }
    pub fn dtc_description_on(&self, bus: &BusSpec, ecu: EcuAddress, code: &str) -> Option<String> {
        self.current_ecus()
            .ok()?
            .into_iter()
            .find(|e| e.address == ecu && &e.bus == bus)?
            .definition
            .dtc
            .get(code)
            .cloned()
    }
}
pub fn resolve_name(name: &str, names: &[String]) -> Result<String, DiagError> {
    if names.iter().any(|candidate| candidate == name) {
        return Ok(name.into());
    }
    let matches: Vec<_> = names
        .iter()
        .filter(|candidate| {
            candidate
                .split_once('.')
                .is_some_and(|(_, short)| short == name)
        })
        .cloned()
        .collect();
    match matches.as_slice() {
        [qualified] => Ok(qualified.clone()),
        [] => Err(DiagError::InvalidCommand(format!(
            "Unknown name {name}. Choices: {}",
            names.join(", ")
        ))),
        _ => Err(DiagError::InvalidCommand(format!(
            "Ambiguous name {name}; choose {}",
            matches.join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests;
