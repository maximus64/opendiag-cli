//! Selection and authorization are shared by every frontend and strategy.
use crate::{
    diag::EcuInfo,
    events::{Cancel, Interaction},
    obd::addressing::EcuAddress,
    preflight::{CheckStatus, Preflight},
    vehicle::SystemVoltage,
    DiagError, ObdProtocol,
};
use std::path::{Path, PathBuf};
pub mod honda_can;
pub mod parsers;

#[derive(Debug, Clone, Default)]
pub struct FlashOptions {
    pub start_address: Option<u32>,
    pub security_key: Option<Vec<u8>>,
    pub tokens: Option<PathBuf>,
    /// Nominal battery voltage; vehicle workflows use the selected definition.
    pub system_voltage: SystemVoltage,
    pub(crate) token_data: Option<Vec<u8>>,
}
impl FlashOptions {
    pub fn snapshot_tokens(mut self) -> Result<Self, DiagError> {
        if let Some(path) = &self.tokens {
            if self.token_data.is_none() {
                let data = std::fs::read(path)
                    .map_err(|e| DiagError::InvalidFile(format!("{}: {e}", path.display())))?;
                parsers::HondaRwdParser::new()
                    .parse(&data)
                    .map_err(DiagError::InvalidFile)?;
                self.token_data = Some(data);
            }
        }
        Ok(self)
    }
}
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageInfo {
    pub format: &'static str,
    pub target: Option<EcuAddress>,
    pub compatible: Vec<String>,
    pub size: usize,
    pub start: Option<u32>,
}
/// Immutable after selection: confirmation cannot race another file read.
#[derive(Debug)]
pub struct PreparedFlash {
    pub(crate) image: ImageInfo,
    pub(crate) target: EcuAddress,
    pub(crate) strategy: String,
    pub(crate) vehicle: Option<String>,
    pub(crate) bytes: Vec<u8>,
    pub(crate) options: FlashOptions,
    pub(crate) raw: bool,
}
impl PreparedFlash {
    pub fn image(&self) -> &ImageInfo {
        &self.image
    }
    pub fn target(&self) -> EcuAddress {
        self.target
    }
    pub fn strategy(&self) -> &str {
        &self.strategy
    }
    pub fn is_raw(&self) -> bool {
        self.raw
    }
    pub fn reselect(
        &self,
        selection: Selection<'_>,
        start_address: Option<u32>,
    ) -> Result<Self, DiagError> {
        let mut options = self.options.clone();
        if let Some(start) = start_address {
            options.start_address = Some(start);
        }
        get_registry().prepare(self.bytes.clone(), options, selection)
    }
}
#[derive(Default)]
pub struct Selection<'a> {
    pub vehicle: Option<&'a str>,
    pub ecu: Option<EcuAddress>,
    pub default_ecu: Option<EcuAddress>,
    pub strategy: Option<&'a str>,
}
#[derive(Default)]
pub struct RunOptions {
    pub dry_run: bool,
    pub allow_untested: bool,
    pub verified_on: Vec<String>,
    pub expected: Vec<(u16, String)>,
    pub target_unmatched: bool,
}
#[derive(Default)]
pub struct VehicleRunOptions {
    pub dry_run: bool,
    pub allow_untested: bool,
    pub start_address: Option<u32>,
}
pub fn run_for_vehicle(
    protocol: &mut ObdProtocol,
    vehicle: &mut crate::vehicle::SelectedVehicle<'_>,
    slot: &str,
    request: &PreparedFlash,
    options: &VehicleRunOptions,
    interaction: &mut dyn Interaction,
    cancel: &Cancel,
) -> Result<Preflight, DiagError> {
    let ecu = vehicle.resolve_for_write(slot, protocol, cancel)?;
    let definition =
        ecu.definition.flash.as_ref().ok_or_else(|| {
            DiagError::Unsupported("Matched ECU variant has no flash entry".into())
        })?;
    let mut request = request.reselect(
        Selection {
            vehicle: Some(&vehicle.id),
            ecu: Some(ecu.address),
            default_ecu: Some(ecu.address),
            strategy: Some(&definition.strategy),
        },
        options.start_address.or(definition.start_address),
    )?;
    request.options.system_voltage = vehicle.vehicle.system_voltage;
    let options = RunOptions {
        dry_run: options.dry_run,
        allow_untested: options.allow_untested,
        target_unmatched: !ecu.selection.matched,
        verified_on: if definition.status == crate::vehicle::ecu_def::Status::Verified {
            definition.verified_on.clone()
        } else {
            Vec::new()
        },
        expected: definition
            .expect
            .iter()
            .map(|rule| (rule.data, rule.pattern.clone()))
            .collect(),
    };
    run(protocol, &request, &options, interaction, cancel)
}
/// Only the orchestration function creates this after preflight and confirmation.
pub struct FlashPermit {
    _private: (),
}
pub trait FlashStrategy {
    fn ecu_type(&self) -> &'static str;
    fn info(&mut self) -> Result<EcuInfo, DiagError>;
    fn preflight(&mut self, request: &PreparedFlash) -> Result<Preflight, DiagError>;
    fn execute(&mut self, permit: FlashPermit, cancel: &Cancel) -> Result<(), DiagError>;
}
pub trait StrategyFactory: Send + Sync {
    fn name(&self) -> &'static str;
    fn inspect(&self, image: &[u8]) -> Result<Option<ImageInfo>, DiagError>;
    fn options(&self) -> &'static [&'static str];
    fn create_strategy<'a>(&self, protocol: &'a mut ObdProtocol) -> Box<dyn FlashStrategy + 'a>;
}
pub struct StrategyRegistry {
    factories: Vec<Box<dyn StrategyFactory>>,
}
impl Default for StrategyRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl StrategyRegistry {
    pub fn new() -> Self {
        Self {
            factories: vec![Box::new(honda_can::HondaCanBusStrategyFactory)],
        }
    }
    pub fn empty() -> Self {
        Self {
            factories: Vec::new(),
        }
    }
    pub fn register(&mut self, factory: Box<dyn StrategyFactory>) {
        self.factories.push(factory);
    }
    pub fn factory(&self, name: &str) -> Option<&dyn StrategyFactory> {
        self.factories
            .iter()
            .find(|f| f.name().eq_ignore_ascii_case(name))
            .map(|f| &**f)
    }
    pub fn strategy_names(&self) -> Vec<&'static str> {
        self.factories.iter().map(|f| f.name()).collect()
    }
    pub fn supports_strategy(&self, name: &str) -> bool {
        self.factory(name).is_some()
    }
    pub fn create_strategy<'a>(
        &self,
        name: &str,
        protocol: &'a mut ObdProtocol,
    ) -> Option<Box<dyn FlashStrategy + 'a>> {
        self.factory(name).map(|f| f.create_strategy(protocol))
    }
    pub fn prepare(
        &self,
        bytes: Vec<u8>,
        mut options: FlashOptions,
        selection: Selection<'_>,
    ) -> Result<PreparedFlash, DiagError> {
        let mut claims = Vec::new();
        for factory in &self.factories {
            if let Some(info) = factory.inspect(&bytes)? {
                claims.push((factory.name(), info));
            }
        }
        let raw = claims.is_empty();
        let (strategy, image) = if raw {
            if selection.vehicle.is_none() {
                return Err(DiagError::InvalidCommand("A raw binary does not identify its vehicle. Select --vehicle, and --ecu if several ECUs are flashable.".into()));
            }
            let name = selection.strategy.ok_or_else(|| {
                DiagError::InvalidCommand("The selected vehicle/ECU has no flash strategy".into())
            })?;
            (
                name.to_string(),
                ImageInfo {
                    format: "raw",
                    target: None,
                    compatible: Vec::new(),
                    size: bytes.len(),
                    start: options.start_address,
                },
            )
        } else if claims.len() == 1 {
            let (name, image) = claims.remove(0);
            (name.to_string(), image)
        } else {
            let preferred = selection.strategy.ok_or_else(|| {
                DiagError::InvalidCommand(format!(
                    "Several strategies recognize the file: {}. Select its vehicle and ECU.",
                    claims
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            let (_, image) = claims
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(preferred))
                .ok_or_else(|| {
                    DiagError::InvalidCommand(
                        "The vehicle's strategy does not recognize this file".into(),
                    )
                })?;
            (preferred.to_string(), image)
        };
        if let Some(declared) = selection.strategy {
            if !strategy.eq_ignore_ascii_case(declared) {
                return Err(DiagError::InvalidCommand(format!("File strategy {strategy} disagrees with the selected vehicle's strategy {declared}")));
            }
        }
        let factory = self.factory(&strategy).ok_or_else(|| {
            DiagError::Unsupported(format!("No flash strategy implementation for {strategy}"))
        })?;
        if options.tokens.is_some() && !factory.options().contains(&"tokens") {
            return Err(DiagError::InvalidCommand(format!(
                "--tokens is not supported by {strategy}"
            )));
        }
        let target = image
            .target
            .or(selection.ecu)
            .or(selection.default_ecu)
            .ok_or_else(|| {
                DiagError::InvalidCommand("Select the flash target with --ecu".into())
            })?;
        for candidate in [image.target, selection.ecu, selection.default_ecu]
            .into_iter()
            .flatten()
        {
            if target != candidate {
                return Err(DiagError::InvalidCommand(format!("Flash target conflict: this file/vehicle is for {target}, but the selected ECU is {candidate}")));
            }
        }
        if image.size == 0 || image.size > u32::MAX as usize {
            return Err(DiagError::InvalidFile(
                "Firmware must contain 1..=u32::MAX bytes".into(),
            ));
        }
        let start = options.start_address.or(image.start).ok_or_else(|| DiagError::InvalidCommand("Raw binary flashing requires --start-address or flash.start_address in the ECU definition".into()))?;
        if u32::try_from(image.size)
            .ok()
            .and_then(|size| start.checked_add(size))
            .is_none()
        {
            return Err(DiagError::InvalidFile(
                "Firmware address range overflows 32 bits".into(),
            ));
        }
        options.start_address = Some(start);
        if let Some(secret) = &options.security_key {
            if strategy.eq_ignore_ascii_case("honda_can")
                && (secret.len() != 4 && secret.len() != 6
                    || secret.len() == 6 && secret[4..] == [0, 0])
            {
                return Err(DiagError::InvalidCommand(
                    "Honda --security-key must contain 4 or 6 bytes, with a nonzero K3 modulus"
                        .into(),
                ));
            }
        }
        options = options.snapshot_tokens()?;
        if raw
            && options.security_key.is_none()
            && options.tokens.is_none()
            && strategy.eq_ignore_ascii_case("honda_can")
        {
            return Err(DiagError::InvalidCommand(
                "Raw Honda firmware needs --security-key or --tokens".into(),
            ));
        }
        Ok(PreparedFlash {
            image,
            target,
            strategy,
            vehicle: selection.vehicle.map(str::to_string),
            bytes,
            options,
            raw,
        })
    }
}
static REGISTRY: std::sync::OnceLock<StrategyRegistry> = std::sync::OnceLock::new();
pub fn get_registry() -> &'static StrategyRegistry {
    REGISTRY.get_or_init(StrategyRegistry::new)
}
pub fn strategy_names() -> Vec<&'static str> {
    get_registry().strategy_names()
}
pub fn supports_strategy(name: &str) -> bool {
    get_registry().supports_strategy(name)
}
pub fn create_strategy<'a>(
    name: &str,
    protocol: &'a mut ObdProtocol,
) -> Option<Box<dyn FlashStrategy + 'a>> {
    get_registry().create_strategy(name, protocol)
}
pub fn prepare_file(
    path: &Path,
    options: FlashOptions,
    selection: Selection<'_>,
) -> Result<PreparedFlash, DiagError> {
    let bytes = std::fs::read(path)
        .map_err(|e| DiagError::InvalidFile(format!("{}: {e}", path.display())))?;
    get_registry().prepare(bytes, options, selection)
}
pub fn run(
    protocol: &mut ObdProtocol,
    request: &PreparedFlash,
    options: &RunOptions,
    interaction: &mut dyn Interaction,
    cancel: &Cancel,
) -> Result<Preflight, DiagError> {
    cancel.check()?;
    protocol.set_ecu(request.target)?;
    let mut checks = Vec::new();
    for (did, pattern) in &options.expected {
        let identity =
            crate::diag::text(&crate::obd::uds::read_data_by_identifier(protocol, *did)?);
        checks.push((
            format!("identity {did:04X}"),
            format!("{identity} (expected {pattern})"),
            if matches_pattern(pattern, &identity) {
                CheckStatus::Ok
            } else {
                CheckStatus::Fail
            },
        ));
    }
    let mut strategy = create_strategy(&request.strategy, protocol).ok_or_else(|| {
        DiagError::Unsupported(format!("No flash strategy for {}", request.strategy))
    })?;
    let mut preflight = strategy.preflight(request)?;
    for (name, value, status) in checks {
        preflight.add(name, value, status);
    }
    if let Some(vehicle) = &request.vehicle {
        preflight.add("vehicle", vehicle, CheckStatus::Info);
    }
    if request.raw {
        preflight.add(
            "image compatibility",
            "Raw image compatibility cannot be verified; using your selected vehicle and ECU",
            CheckStatus::Warn,
        );
    }
    if options.target_unmatched {
        preflight.add(
            "variant",
            "No ECU variant matched; destructive operations are refused",
            CheckStatus::Fail,
        );
    }
    if options.verified_on.is_empty() {
        preflight.add(
            "status",
            if options.allow_untested {
                "untested (--allow-untested)"
            } else {
                "untested; use --allow-untested to run this procedure"
            },
            if options.allow_untested {
                CheckStatus::Warn
            } else {
                CheckStatus::Fail
            },
        );
    } else {
        preflight.add(
            "status",
            format!("verified on {}", options.verified_on.join(", ")),
            CheckStatus::Ok,
        );
    }
    if options.dry_run {
        return Ok(preflight);
    }
    preflight.authorize(interaction, cancel)?;
    strategy.execute(FlashPermit { _private: () }, cancel)?;
    Ok(preflight)
}
/// Literal segments separated by '*' only; no regular expressions or code.
pub fn matches_pattern(pattern: &str, value: &str) -> bool {
    let segments: Vec<_> = pattern.split('*').collect();
    if segments.len() == 1 {
        return pattern == value;
    }
    let Some(mut remainder) = value.strip_prefix(segments[0]) else {
        return false;
    };
    for part in &segments[1..segments.len() - 1] {
        let Some(index) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[index + part.len()..];
    }
    remainder.ends_with(segments[segments.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, Events};
    use crate::test_support::{protocol_on, FakeEcu, RwdFile, ScriptedInteraction};

    fn fixture() -> (PreparedFlash, FakeEcu) {
        let rwd = RwdFile::default();
        let ecu = FakeEcu::new(0x18DAF101, &rwd.versions[0], &rwd.tokens[0]);
        let prepared = get_registry()
            .prepare(rwd.encode(), FlashOptions::default(), Selection::default())
            .unwrap();
        (prepared, ecu)
    }
    #[test]
    fn refusing_confirmation_and_failed_checks_never_erase() {
        let (request, ecu) = fixture();
        let mut protocol = protocol_on(&ecu);
        let mut ui = ScriptedInteraction::new(false);
        assert!(matches!(
            run(
                &mut protocol,
                &request,
                &RunOptions {
                    allow_untested: true,
                    ..Default::default()
                },
                &mut ui,
                &Cancel::default()
            ),
            Err(DiagError::Cancelled)
        ));
        assert_eq!(ui.confirmed, 1);
        assert!(!ecu.state().erased);
        let mut ui = ScriptedInteraction::new(true);
        assert!(run(
            &mut protocol,
            &request,
            &RunOptions::default(),
            &mut ui,
            &Cancel::default()
        )
        .is_err());
        assert_eq!(ui.confirmed, 0);
        assert!(!ecu.state().erased);
    }
    #[test]
    fn an_explicit_security_key_cannot_bypass_container_compatibility() {
        let rwd = RwdFile::default();
        let ecu = FakeEcu::new(0x18DAF101, b"37805-OTHER", &rwd.tokens[0]);
        let request = get_registry()
            .prepare(
                rwd.encode(),
                FlashOptions {
                    security_key: Some(rwd.tokens[0].clone()),
                    ..Default::default()
                },
                Selection::default(),
            )
            .unwrap();
        let mut ui = ScriptedInteraction::new(true);
        assert!(run(
            &mut protocol_on(&ecu),
            &request,
            &RunOptions {
                allow_untested: true,
                ..Default::default()
            },
            &mut ui,
            &Cancel::default()
        )
        .is_err());
        assert_eq!(ui.confirmed, 0);
        assert!(!ecu.state().erased);
    }
    #[test]
    fn cancellation_during_transfer_is_refused_until_dependencies_are_checked() {
        struct Canceller(Cancel);
        impl Events for Canceller {
            fn emit(&mut self, event: Event<'_>) {
                if matches!(event, Event::Progress { .. }) {
                    self.0.cancel();
                }
            }
        }
        let (request, ecu) = fixture();
        let mut protocol = protocol_on(&ecu);
        let cancel = Cancel::default();
        protocol.set_events(Box::new(Canceller(cancel.clone())));
        run(
            &mut protocol,
            &request,
            &RunOptions {
                allow_untested: true,
                ..Default::default()
            },
            &mut ScriptedInteraction::new(true),
            &cancel,
        )
        .unwrap();
        assert!(cancel.is_cancelled());
        assert!(ecu.state().transfer_exited && ecu.state().dependencies_checked);
    }

    #[test]
    fn battery_outside_limits_after_confirmation_stops_before_programming() {
        struct ChangeBattery {
            ecu: FakeEcu,
            millivolts: u32,
        }
        impl Interaction for ChangeBattery {
            fn confirm(&mut self, _: &Preflight) -> bool {
                self.ecu.measure_battery_as(self.millivolts);
                true
            }
            fn prompt(&mut self, _: &str) -> crate::events::Answer {
                crate::events::Answer::Abort
            }
        }
        for (system, initial, after_confirmation) in [
            (SystemVoltage::V12, 12_400, 11_999),
            (SystemVoltage::V12, 12_400, 15_001),
            (SystemVoltage::V24, 24_800, 23_999),
            (SystemVoltage::V24, 24_800, 30_001),
        ] {
            let (mut request, ecu) = fixture();
            request.options.system_voltage = system;
            ecu.measure_battery_as(initial);
            let err = run(
                &mut protocol_on(&ecu),
                &request,
                &RunOptions {
                    allow_untested: true,
                    ..Default::default()
                },
                &mut ChangeBattery {
                    ecu: ecu.clone(),
                    millivolts: after_confirmation,
                },
                &Cancel::default(),
            )
            .unwrap_err();

            assert!(matches!(err, DiagError::PreflightFailed(_)), "{err}");
            assert!(err.to_string().contains("battery"), "{err}");
            let st = ecu.state();
            assert!(!st.erased);
            assert!(!st.unlocked);
            assert!(st.programming_voltage.is_empty());
            assert!(st.requests.iter().all(|request| request[0] == 0x22));
        }
    }

    #[test]
    fn selection_uses_contents_rejects_malformed_containers_and_conflicts() {
        let rwd = RwdFile::default();
        assert!(get_registry()
            .factory("honda_can")
            .unwrap()
            .inspect(b"random")
            .unwrap()
            .is_none());
        assert!(get_registry()
            .factory("honda_can")
            .unwrap()
            .inspect(&[0x5A, 0x0D, 0x0A])
            .is_err());
        let mut missing_target = rwd.encode();
        let mut index = 3;
        for _ in 0..2 {
            let count = missing_target[index];
            index += 1;
            for _ in 0..count {
                let len = missing_target[index] as usize;
                index += 1 + len;
            }
        }
        // Keep a syntactically valid third header whose only entry is empty.
        let offset = index + 2;
        missing_target.remove(offset);
        missing_target[index + 1] = 0;
        assert!(get_registry()
            .factory("honda_can")
            .unwrap()
            .inspect(&missing_target)
            .unwrap_err()
            .to_string()
            .contains("Missing target ECU"));
        let other = EcuAddress::Can {
            request: 0x18DA10F1,
            response: 0x18DAF110,
        };
        assert!(get_registry()
            .prepare(
                rwd.encode(),
                FlashOptions::default(),
                Selection {
                    ecu: Some(other),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("conflict"));
        let file = tempfile::Builder::new().suffix(".bin").tempfile().unwrap();
        std::fs::write(file.path(), rwd.encode()).unwrap();
        assert_eq!(
            prepare_file(file.path(), FlashOptions::default(), Selection::default())
                .unwrap()
                .image
                .format,
            "honda-rwd"
        );
    }
    #[test]
    fn raw_images_require_explicit_vehicle_and_have_no_extra_override() {
        let options = FlashOptions {
            start_address: Some(0x10000),
            security_key: Some(vec![0x11; 4]),
            ..Default::default()
        };
        let ecu = EcuAddress::Can {
            request: 0x18DA01F1,
            response: 0x18DAF101,
        };
        assert!(get_registry()
            .prepare(
                vec![1; 32],
                options.clone(),
                Selection {
                    ecu: Some(ecu),
                    strategy: Some("honda_can"),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("--vehicle"));
        let request = get_registry()
            .prepare(
                vec![1; 32],
                options,
                Selection {
                    vehicle: Some("honda/odyssey-rl3"),
                    ecu: Some(ecu),
                    strategy: Some("honda_can"),
                    ..Default::default()
                },
            )
            .unwrap();
        let ecu = FakeEcu::new(0x18DAF101, b"37805-RGL-A030", &[0x11; 4]);
        let result = run(
            &mut protocol_on(&ecu),
            &request,
            &RunOptions {
                dry_run: true,
                allow_untested: true,
                ..Default::default()
            },
            &mut ScriptedInteraction::new(true),
            &Cancel::default(),
        )
        .unwrap();
        assert!(result
            .checks
            .iter()
            .any(|c| c.name == "image compatibility" && c.status == CheckStatus::Warn));
        assert!(!ecu.state().erased);
    }

    #[test]
    fn vehicle_reselection_adds_verification_and_identity_checks_to_a_prepared_image() {
        let rwd = RwdFile {
            can_offset: 0x11,
            versions: vec![b"37805-RZA-A560".to_vec()],
            ..Default::default()
        };
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), rwd.encode()).unwrap();
        let prepared =
            prepare_file(file.path(), FlashOptions::default(), Selection::default()).unwrap();
        assert!(prepared.vehicle.is_none());
        file.close().unwrap();

        let mut vehicle = crate::vehicle::SelectedVehicle::new(
            crate::vehicle::catalog::embedded(),
            "honda/crv-re3",
            Some(2008),
        )
        .unwrap();
        let ecu = FakeEcu::new(0x18DAF111, &rwd.versions[0], &rwd.tokens[0]);
        let mut ui = ScriptedInteraction::new(false);
        let preflight = run_for_vehicle(
            &mut protocol_on(&ecu),
            &mut vehicle,
            "engine",
            &prepared,
            &VehicleRunOptions {
                dry_run: true,
                ..Default::default()
            },
            &mut ui,
            &Cancel::default(),
        )
        .unwrap();

        preflight.validate().unwrap();
        assert!(preflight.checks.iter().any(|check| {
            check.name == "status"
                && check.status == CheckStatus::Ok
                && check.value.starts_with("verified on ")
        }));
        assert!(preflight
            .checks
            .iter()
            .any(|check| { check.name == "identity F181" && check.status == CheckStatus::Ok }));
        assert!(preflight
            .checks
            .iter()
            .any(|check| { check.name == "vehicle" && check.value == "honda/crv-re3" }));
        assert_eq!(ui.confirmed, 0);
        assert!(!ecu.state().erased);
        assert!(ecu.state().programming_voltage.is_empty());
    }

    #[test]
    fn changing_firmware_and_token_files_during_confirmation_cannot_change_the_job() {
        struct ChangeFile {
            path: PathBuf,
        }
        impl Interaction for ChangeFile {
            fn confirm(&mut self, _: &Preflight) -> bool {
                std::fs::write(&self.path, b"different contents").unwrap();
                true
            }
            fn prompt(&mut self, _: &str) -> crate::events::Answer {
                crate::events::Answer::Abort
            }
        }
        let rwd = RwdFile::default();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), rwd.encode()).unwrap();
        let prepared =
            prepare_file(file.path(), FlashOptions::default(), Selection::default()).unwrap();
        let ecu = FakeEcu::new(0x18DAF101, &rwd.versions[0], &rwd.tokens[0]);
        run(
            &mut protocol_on(&ecu),
            &prepared,
            &RunOptions {
                allow_untested: true,
                ..Default::default()
            },
            &mut ChangeFile {
                path: file.path().into(),
            },
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(ecu.state().received_firmware, rwd.firmware);
        let raw = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(raw.path(), &rwd.firmware).unwrap();
        std::fs::write(file.path(), rwd.encode()).unwrap();
        let prepared = prepare_file(
            raw.path(),
            FlashOptions {
                start_address: Some(rwd.start),
                tokens: Some(file.path().into()),
                ..Default::default()
            },
            Selection {
                vehicle: Some("test/car"),
                ecu: Some(EcuAddress::Can {
                    request: rwd.can_address(),
                    response: 0x18DAF101,
                }),
                strategy: Some("honda_can"),
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(file.path(), b"changed before reselection").unwrap();
        let prepared = prepared
            .reselect(
                Selection {
                    vehicle: Some("test/car"),
                    ecu: Some(prepared.target()),
                    strategy: Some("honda_can"),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let ecu = FakeEcu::new(0x18DAF101, &rwd.versions[0], &rwd.tokens[0]);
        run(
            &mut protocol_on(&ecu),
            &prepared,
            &RunOptions {
                allow_untested: true,
                ..Default::default()
            },
            &mut ChangeFile {
                path: raw.path().into(),
            },
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(ecu.state().received_firmware, rwd.firmware);
    }
    #[test]
    fn strategy_specific_options_are_not_silently_ignored() {
        struct Factory;
        impl StrategyFactory for Factory {
            fn name(&self) -> &'static str {
                "test"
            }
            fn inspect(&self, image: &[u8]) -> Result<Option<ImageInfo>, DiagError> {
                Ok(Some(ImageInfo {
                    format: "test",
                    target: Some(EcuAddress::Can {
                        request: 0x7E0,
                        response: 0x7E8,
                    }),
                    compatible: Vec::new(),
                    size: image.len(),
                    start: Some(0),
                }))
            }
            fn options(&self) -> &'static [&'static str] {
                &[]
            }
            fn create_strategy<'a>(&self, _: &'a mut ObdProtocol) -> Box<dyn FlashStrategy + 'a> {
                unreachable!()
            }
        }
        let mut registry = StrategyRegistry::empty();
        registry.register(Box::new(Factory));
        let error = registry
            .prepare(
                vec![1; 32],
                FlashOptions {
                    tokens: Some("missing.rwd".into()),
                    ..Default::default()
                },
                Selection::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("--tokens is not supported"));
    }
}
