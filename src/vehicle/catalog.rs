use super::{
    ecu_def::{EcuDef, IdentitySource, Safety, Status},
    hooks::Hooks,
    vehicle::Vehicle,
    years::{self, YearRange},
};
use crate::{flash::matches_pattern, DiagError};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, serde::Serialize)]
pub struct VehicleIndex {
    pub id: &'static str,
    pub make: &'static str,
    pub model: &'static str,
    pub years: &'static [YearRange],
    pub vin: &'static [&'static str],
}
include!(concat!(env!("OUT_DIR"), "/catalog.rs"));

pub struct Catalog {
    vehicles: BTreeMap<String, String>,
    ecus: BTreeMap<String, String>,
    inheritance: Option<BTreeMap<String, Option<String>>>,
    variants: Option<BTreeSet<String>>,
    pub hooks: Hooks,
}
fn invalid(file: &str, key: &str, message: impl std::fmt::Display) -> DiagError {
    DiagError::InvalidFile(format!("{file}: {key}: {message}"))
}
impl Catalog {
    pub fn from_files(vehicles: &[(&str, &str)], ecus: &[(&str, &str)], hooks: Hooks) -> Self {
        Self {
            vehicles: vehicles
                .iter()
                .map(|(id, text)| (id.to_string(), text.to_string()))
                .collect(),
            ecus: ecus
                .iter()
                .map(|(id, text)| (id.to_string(), text.to_string()))
                .collect(),
            hooks,
            inheritance: None,
            variants: None,
        }
    }
    pub fn fingerprint(&self) -> String {
        // Stable FNV-1a identifies catalog changes; this is cache invalidation, not authentication.
        let mut hash = 0xcbf29ce484222325u64;
        for byte in env!("CARGO_PKG_VERSION").bytes() {
            hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
        }
        for (id, text) in self.vehicles.iter().chain(self.ecus.iter()) {
            for byte in id.bytes().chain([0]).chain(text.bytes()).chain([0]) {
                hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
            }
        }
        format!("{hash:016x}")
    }
    pub fn vehicle_ids(&self) -> impl Iterator<Item = &str> {
        self.vehicles.keys().map(String::as_str)
    }
    pub fn vehicle(&self, id: &str) -> Result<Vehicle, DiagError> {
        let text = self.vehicles.get(id).ok_or_else(|| DiagError::InvalidCommand(format!("Unknown vehicle {id}. Use `opendiag vehicle list` or `vehicle search`. ECU definition IDs are not vehicle IDs.")))?;
        toml::from_str(text).map_err(|e| invalid(&format!("vehicles/{id}.toml"), "definition", e))
    }
    fn raw_definition(&self, id: &str) -> Result<EcuDef, DiagError> {
        let text = self.ecus.get(id).ok_or_else(|| {
            invalid(
                &format!("ecus/{id}.toml"),
                "reference",
                "missing ECU definition",
            )
        })?;
        toml::from_str(text).map_err(|e| invalid(&format!("ecus/{id}.toml"), "definition", e))
    }
    pub fn definition(&self, id: &str) -> Result<EcuDef, DiagError> {
        self.resolve(id, &mut BTreeSet::new())
    }
    fn resolve(&self, id: &str, seen: &mut BTreeSet<String>) -> Result<EcuDef, DiagError> {
        if !seen.insert(id.into()) {
            return Err(invalid(
                &format!("ecus/{id}.toml"),
                "extends",
                "inheritance cycle",
            ));
        }
        let mut child = self.raw_definition(id)?;
        if let Some(parent_id) = &child.extends {
            let parent = self.resolve(parent_id, seen)?;
            if child.diag.is_none() {
                child.diag = parent.diag;
            }
            if child.flash.is_none() {
                child.flash = parent.flash;
            }
            let mut signals: BTreeMap<_, _> = parent
                .signals
                .into_iter()
                .map(|s| (s.field.split('=').next().unwrap_or("").to_string(), s))
                .collect();
            for signal in child.signals {
                signals.insert(signal.field.split('=').next().unwrap_or("").into(), signal);
            }
            child.signals = signals.into_values().collect();
            let mut services = parent.services;
            services.extend(child.services);
            child.services = services;
            let mut security = parent.security;
            security.extend(child.security);
            child.security = security;
            let mut dtc = parent.dtc;
            dtc.extend(child.dtc);
            child.dtc = dtc;
            let mut translations = parent.translations;
            translations.extend(child.translations);
            child.translations = translations;
        }
        seen.remove(id);
        Ok(child)
    }
    pub fn children(&self, id: &str) -> Result<Vec<String>, DiagError> {
        if let Some(inheritance) = &self.inheritance {
            return Ok(inheritance
                .iter()
                .filter(|(key, parent)| {
                    parent.as_deref() == Some(id)
                        && self
                            .variants
                            .as_ref()
                            .is_some_and(|variants| variants.contains(*key))
                })
                .map(|(id, _)| id.clone())
                .collect());
        }
        let mut result = Vec::new();
        for key in self.ecus.keys() {
            let definition = self.raw_definition(key)?;
            if definition.extends.as_deref() == Some(id) && !definition.matches.is_empty() {
                result.push(key.clone());
            }
        }
        Ok(result)
    }
    pub fn identity_rules(&self, id: &str) -> Result<BTreeSet<IdentitySource>, DiagError> {
        fn collect(
            catalog: &Catalog,
            id: &str,
            seen: &mut BTreeSet<String>,
            rules: &mut BTreeSet<IdentitySource>,
        ) -> Result<(), DiagError> {
            if !seen.insert(id.into()) {
                return Err(invalid(id, "extends", "inheritance cycle"));
            }
            let mut ancestor = Some(id.to_string());
            let mut lineage = BTreeSet::new();
            while let Some(id) = ancestor {
                if !lineage.insert(id.clone()) {
                    return Err(invalid(&id, "extends", "inheritance cycle"));
                }
                let definition = catalog.raw_definition(&id)?;
                rules.extend(definition.matches.iter().map(|m| m.source));
                ancestor = definition.extends;
            }
            for child in catalog.children(id)? {
                collect(catalog, &child, seen, rules)?;
            }
            Ok(())
        }
        let mut rules = BTreeSet::new();
        collect(self, id, &mut BTreeSet::new(), &mut rules)?;
        Ok(rules)
    }
    pub fn match_variant(
        &self,
        base: &str,
        identity: &BTreeMap<IdentitySource, String>,
    ) -> Result<(String, bool), DiagError> {
        if !self.identity_matches(base, identity)? {
            return Ok((base.into(), false));
        }
        let children = self.children(base)?;
        if children.is_empty() {
            return Ok((base.into(), true));
        }
        let mut matching = Vec::new();
        for child in children {
            if self.identity_matches(&child, identity)? {
                matching.push(child);
            }
        }
        match matching.as_slice() {
            [] => Ok((base.into(), false)),
            [child] => self.match_variant(child, identity),
            _ => Err(invalid(
                base,
                "match",
                format!("multiple variants match: {}", matching.join(", ")),
            )),
        }
    }
    fn identity_matches(
        &self,
        id: &str,
        identity: &BTreeMap<IdentitySource, String>,
    ) -> Result<bool, DiagError> {
        let mut ancestor = Some(id.to_string());
        let mut seen = BTreeSet::new();
        while let Some(id) = ancestor {
            if !seen.insert(id.clone()) {
                return Err(invalid(&id, "extends", "inheritance cycle"));
            }
            let definition = self.raw_definition(&id)?;
            if !definition.matches.iter().all(|m| {
                identity
                    .get(&m.source)
                    .is_some_and(|value| matches_pattern(&m.pattern, value))
            }) {
                return Ok(false);
            }
            ancestor = definition.extends;
        }
        Ok(true)
    }
    fn validate_candidates(&self, ids: &[String], file: &str, key: &str) -> Result<(), DiagError> {
        let definitions = ids
            .iter()
            .map(|id| self.definition(id))
            .collect::<Result<Vec<_>, _>>()?;
        for (id, definition) in ids.iter().zip(&definitions) {
            if definition.matches.is_empty() {
                return Err(invalid(
                    file,
                    key,
                    format!("candidate {id} must declare identity patterns"),
                ));
            }
        }
        for (index, (left, a)) in ids.iter().zip(&definitions).enumerate() {
            for (right, b) in ids[index + 1..].iter().zip(&definitions[index + 1..]) {
                let disjoint = a.matches.iter().any(|a| {
                    b.matches
                        .iter()
                        .any(|b| a.source == b.source && !patterns_overlap(&a.pattern, &b.pattern))
                });
                if !disjoint {
                    return Err(invalid(
                        file,
                        key,
                        format!("overlapping variants {left} and {right}"),
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), DiagError> {
        self.validate_with_legacy(false)
    }
    pub fn validate_vehicle(&self, id: &str) -> Result<(), DiagError> {
        let vehicle = self.vehicle(id)?;
        fn collect(
            catalog: &Catalog,
            id: &str,
            seen: &mut BTreeSet<String>,
        ) -> Result<(), DiagError> {
            if !seen.insert(id.to_string()) {
                return Ok(());
            }
            let definition = catalog.raw_definition(id)?;
            if let Some(parent) = definition.extends {
                collect(catalog, &parent, seen)?;
            }
            for child in catalog.children(id)? {
                collect(catalog, &child, seen)?;
            }
            Ok(())
        }
        let mut needed = BTreeSet::new();
        for versions in vehicle.modules.values() {
            for slot in versions.entries() {
                for definition in slot.definition_ids() {
                    collect(self, definition, &mut needed)?;
                }
            }
        }
        let subset = Self {
            vehicles: BTreeMap::from([(id.to_string(), self.vehicles[id].clone())]),
            ecus: needed
                .into_iter()
                .map(|id| (id.clone(), self.ecus[&id].clone()))
                .collect(),
            inheritance: None,
            variants: None,
            hooks: self.hooks.clone(),
        };
        subset.validate()
    }
    /// Legacy validation is exposed for backend development and message-level tests.
    pub fn validate_with_legacy(&self, legacy: bool) -> Result<(), DiagError> {
        for id in self.ecus.keys() {
            let file = format!("ecus/{id}.toml");
            let raw = self.raw_definition(id)?;
            let def = self.definition(id)?;
            if def.title.trim().is_empty() {
                return Err(invalid(&file, "title", "must be nonempty"));
            }
            let diag = def
                .diag
                .as_deref()
                .ok_or_else(|| invalid(&file, "diag", "required unless inherited"))?;
            if !(matches!(diag, "obd" | "uds") || legacy && matches!(diag, "kwp2000" | "j2190")) {
                return Err(invalid(
                    &file,
                    "diag",
                    format!("{diag} not implemented in this build"),
                ));
            }
            for m in &def.matches {
                if diag == "obd" && matches!(m.source, IdentitySource::Data { .. }) {
                    return Err(invalid(
                        &file,
                        "match",
                        "OBD identity requires an obd=2 or obd=4 source",
                    ));
                }
                if m.pattern.is_empty() {
                    return Err(invalid(&file, "match.pattern", "must be nonempty"));
                }
            }
            let mut names = BTreeSet::new();
            for signal in &raw.signals {
                let field = crate::datalog::parse_field(&signal.field)
                    .map_err(|e| invalid(&file, "signal.field", e))?;
                if !signal.field.contains('=') || !names.insert(field.name) {
                    return Err(invalid(
                        &file,
                        "signal.field",
                        "signal requires a unique NAME=DID field",
                    ));
                }
                if !signal.scale.is_finite() || !signal.offset.is_finite() {
                    return Err(invalid(&file, "signal.scale/offset", "must be finite"));
                }
            }
            for (level, name) in &def.security {
                if level
                    .parse::<u8>()
                    .ok()
                    .is_none_or(|v| v == 0 || v >= 0x7F || v % 2 == 0)
                    || !self.hooks.contains_algorithm(name)
                {
                    return Err(invalid(
                        &file,
                        "security",
                        format!("invalid level or unknown algorithm {level} = {name}"),
                    ));
                }
            }
            if let Some(flash) = &def.flash {
                if !crate::flash::get_registry().supports_strategy(&flash.strategy) {
                    return Err(invalid(
                        &file,
                        "flash.strategy",
                        format!("unknown strategy {}", flash.strategy),
                    ));
                }
                trust(&file, "flash.verified_on", flash.status, &flash.verified_on)?;
                if flash.expect.iter().any(|m| m.pattern.is_empty()) {
                    return Err(invalid(&file, "flash.expect", "patterns must be nonempty"));
                }
            }
            for (name, service) in &def.services {
                let key = format!("service.{name}");
                if name.is_empty() || name.contains('.') || service.title.trim().is_empty() {
                    return Err(invalid(
                        &file,
                        &key,
                        "name and title must be nonempty; name must not contain '.'",
                    ));
                }
                if service.run.is_some() == service.steps.is_some() {
                    return Err(invalid(&file, &key, "requires exactly one of run or steps"));
                }
                trust(
                    &file,
                    &format!("{key}.verified_on"),
                    service.status,
                    &service.verified_on,
                )?;
                if let Some(run) = &service.run {
                    if !self.hooks.contains_procedure(run) {
                        return Err(invalid(
                            &file,
                            &format!("{key}.run"),
                            format!("unknown hook {run}"),
                        ));
                    }
                }
                if let Some(steps) = &service.steps {
                    if steps.is_empty() {
                        return Err(invalid(&file, &format!("{key}.steps"), "must be nonempty"));
                    }
                    for (index, step) in steps.iter().enumerate() {
                        step.validate(&def, &self.hooks)
                            .map_err(|e| invalid(&file, &format!("{key}.steps[{index}]"), e))?;
                    }
                    if service.safety == Safety::ReadOnly && steps.iter().any(|s| s.mutates()) {
                        return Err(invalid(
                            &file,
                            &format!("{key}.safety"),
                            "mutating steps cannot be read-only",
                        ));
                    }
                }
            }
            self.validate_candidates(&self.children(id)?, &file, "match")?;
        }
        for id in self.vehicles.keys() {
            let file = format!("vehicles/{id}.toml");
            let vehicle = self.vehicle(id)?;
            if vehicle.make.is_empty() || vehicle.model.is_empty() {
                return Err(invalid(&file, "make/model", "invalid vehicle metadata"));
            }
            years::validate(&vehicle.years).map_err(|e| invalid(&file, "years", e))?;
            if vehicle.vin.iter().any(|v| {
                v.is_empty()
                    || v.len() > 17
                    || !v
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                    || v.contains(['I', 'O', 'Q'])
            }) {
                return Err(invalid(&file, "vin", "invalid VIN prefix"));
            }
            for (name, bus) in &vehicle.buses {
                let spec = bus
                    .spec()
                    .map_err(|e| invalid(&file, &format!("bus.{name}"), e))?;
                if !spec.is_implemented() && !legacy {
                    return Err(invalid(
                        &file,
                        &format!("bus.{name}.protocol"),
                        format!("{} not implemented in this build", bus.protocol),
                    ));
                }
            }
            for (name, versions) in &vehicle.modules {
                let key = format!("module.{name}");
                let mut ranges = Vec::new();
                if versions.entries().is_empty() {
                    return Err(invalid(&file, &key, "empty slot versions"));
                }
                for slot in versions.entries() {
                    if slot
                        .label
                        .as_ref()
                        .is_some_and(|label| label.trim().is_empty())
                    {
                        return Err(invalid(&file, &format!("{key}.label"), "must be nonempty"));
                    }
                    if slot.uses.is_none() && slot.label.is_none() {
                        return Err(invalid(
                            &file,
                            &format!("{key}.label"),
                            "required for a module without uses",
                        ));
                    }
                    let bus = vehicle
                        .buses
                        .get(&slot.bus)
                        .ok_or_else(|| invalid(&file, &format!("{key}.bus"), "missing bus"))?
                        .spec()
                        .map_err(|e| invalid(&file, &key, e))?;
                    slot.address(&bus).map_err(|e| invalid(&file, &key, e))?;
                    if let Some(uses) = &slot.uses {
                        let ids = uses.ids();
                        let uses_key = format!("{key}.uses");
                        if ids.is_empty() {
                            return Err(invalid(&file, &uses_key, "must contain a definition"));
                        }
                        if ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
                            return Err(invalid(
                                &file,
                                &uses_key,
                                "duplicate candidate definitions",
                            ));
                        }
                        let definitions = ids
                            .iter()
                            .map(|id| {
                                self.definition(id)
                                    .map_err(|e| invalid(&file, &uses_key, e))
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        if ids.len() > 1 {
                            self.validate_candidates(ids, &file, &uses_key)?;
                            if definitions
                                .iter()
                                .any(|def| def.diag != definitions[0].diag)
                            {
                                return Err(invalid(
                                    &file,
                                    &uses_key,
                                    "candidates must use the same diagnostic protocol",
                                ));
                            }
                        }
                        let diag = definitions[0].diag.as_deref().unwrap_or("obd");
                        let fits = match diag {
                            "uds" => bus.is_can(),
                            "obd" => true,
                            "kwp2000" => matches!(
                                bus.kind,
                                crate::link::BusKind::Iso14230 { .. }
                                    | crate::link::BusKind::IsoTp { .. }
                            ),
                            "j2190" => matches!(
                                bus.kind,
                                crate::link::BusKind::J1850Vpw | crate::link::BusKind::J1850Pwm
                            ),
                            _ => false,
                        };
                        if !fits {
                            return Err(invalid(
                                &file,
                                &key,
                                format!("diag {diag} does not fit bus {}", slot.bus),
                            ));
                        }
                    }
                    let slot_years = slot.years.as_deref().unwrap_or(&vehicle.years);
                    let years_key = format!("{key}.years");
                    years::validate(slot_years).map_err(|e| invalid(&file, &years_key, e))?;
                    for range in slot_years {
                        let covered = (range.from..=range.to)
                            .all(|year| years::contains(&vehicle.years, year));
                        if !covered || ranges.iter().any(|other: &YearRange| range.overlaps(other))
                        {
                            return Err(invalid(
                                &file,
                                &years_key,
                                "overlapping or out-of-range model years",
                            ));
                        }
                    }
                    ranges.extend_from_slice(slot_years);
                }
            }
        }
        Ok(())
    }
}
fn trust(file: &str, key: &str, status: Status, verified_on: &[String]) -> Result<(), DiagError> {
    if status == Status::Verified
        && (verified_on.is_empty() || verified_on.iter().any(|s| s.trim().is_empty()))
    {
        return Err(invalid(
            file,
            key,
            "verified requires nonempty verification evidence",
        ));
    }
    if status == Status::Untested && !verified_on.is_empty() {
        return Err(invalid(
            file,
            key,
            "untested must not carry verification evidence",
        ));
    }
    Ok(())
}
fn patterns_overlap(a: &str, b: &str) -> bool {
    if !a.contains('*') {
        return matches_pattern(b, a);
    }
    if !b.contains('*') {
        return matches_pattern(a, b);
    }
    let a = a.split('*').next().unwrap();
    let b = b.split('*').next().unwrap();
    a.starts_with(b) || b.starts_with(a)
}
pub fn embedded() -> &'static Catalog {
    static CATALOG: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut catalog = Catalog::from_files(VEHICLE_FILES, ECU_FILES, Hooks::default());
        catalog.inheritance = Some(
            ECU_INHERITANCE
                .iter()
                .map(|(id, parent)| (id.to_string(), parent.map(str::to_string)))
                .collect(),
        );
        catalog.variants = Some(ECU_VARIANTS.iter().map(|id| id.to_string()).collect());
        catalog
    })
}
