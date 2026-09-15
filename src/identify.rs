//! Read-only bus/VIN identification and a versioned, per-VIN cache.
use crate::{
    diag::{self, DataId},
    events::{Cancel, Event, Events, NoEvents},
    link::{Adapter, BusKind, BusSpec, IdWidth},
    vehicle::{
        catalog::{Catalog, VehicleIndex},
        years, SelectedVehicle, VariantSelection,
    },
    DiagError, ObdProtocol,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VinInfo {
    pub vin: String,
    pub wmi: String,
    pub make: Option<String>,
    pub possible_years: Vec<u16>,
}
pub fn decode_vin(vin: &str) -> Result<VinInfo, DiagError> {
    let vin = vin.trim().to_ascii_uppercase();
    if vin.len() != 17
        || !vin
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        || vin.contains(['I', 'O', 'Q'])
    {
        return Err(DiagError::InvalidCommand(
            "VIN must have 17 letters/digits, excluding I, O and Q".into(),
        ));
    }
    // Year codes repeat every thirty years; retain ambiguity for candidate matching.
    let codes = b"ABCDEFGHJKLMNPRSTVWXY123456789";
    let possible_years = codes
        .iter()
        .position(|b| *b == vin.as_bytes()[9])
        .map(|i| vec![1980 + i as u16, 2010 + i as u16, 2040 + i as u16])
        .unwrap_or_default();
    // Honda's manufacturer VIN submission, NHTSA MID e20b1fa6-8dbc-40ff-9a80-f0306c124149.
    let make = match &vin[..3] {
        "1HG" | "JHM" | "2HG" | "2HK" | "2HJ" | "SHH" | "SHS" | "3HG" | "3CZ" | "5J6" | "5J7"
        | "5KB" | "5FN" | "5FP" | "19X" | "7FA" => Some("Honda".into()),
        "19U" | "19V" | "5J8" | "5J0" | "5KC" | "5FR" | "5FS" | "2HH" | "2HN" | "2HU" => {
            Some("Acura".into())
        }
        // NHTSA vPIC decode of 1GNFK16Z32J211066, retrieved 2026-10-12.
        "1GN" => Some("Chevrolet".into()),
        _ => None,
    };
    Ok(VinInfo {
        wmi: vin[..3].into(),
        vin,
        make,
        possible_years,
    })
}
pub fn detection_buses() -> Vec<BusSpec> {
    [500_000, 250_000]
        .into_iter()
        .flat_map(|bitrate| {
            [IdWidth::Bits11, IdWidth::Bits29].map(|id| BusSpec::obd_can(id, bitrate))
        })
        .chain([
            BusSpec::standard(BusKind::Iso9141),
            BusSpec::standard(BusKind::Iso14230 {
                init: crate::link::KwpInit::FiveBaud,
            }),
            BusSpec::standard(BusKind::Iso14230 {
                init: crate::link::KwpInit::Fast,
            }),
            BusSpec::standard(BusKind::J1850Pwm),
            BusSpec::standard(BusKind::J1850Vpw),
        ])
        .collect()
}
pub fn detect(
    adapter: Rc<dyn Adapter>,
    candidates: &[BusSpec],
    events: &mut dyn Events,
    cancel: &Cancel,
) -> Result<ObdProtocol, DiagError> {
    let mut failures = Vec::new();
    for (index, bus) in candidates.iter().enumerate() {
        cancel.check()?;
        events.emit(Event::Step(&format!("Trying {bus}")));
        events.emit(Event::Progress {
            done: index as u64,
            total: candidates.len() as u64,
            unit: crate::events::ProgressUnit::Items,
        });
        if adapter.supports(bus) == Some(false) {
            continue;
        }
        match adapter.connect(bus) {
            Ok(link) => {
                let mut protocol = ObdProtocol::new(adapter.clone(), link);
                match protocol.obd_ecus() {
                    Ok(_) => {
                        events.emit(Event::Progress {
                            done: (index + 1) as u64,
                            total: candidates.len() as u64,
                            unit: crate::events::ProgressUnit::Items,
                        });
                        return Ok(protocol);
                    }
                    Err(e) => {
                        events.emit(Event::Detail(&format!("{bus}: {e}")));
                        failures.push(format!("{bus}: {e}"));
                    }
                }
            }
            Err(e) => {
                events.emit(Event::Detail(&format!("{bus}: {e}")));
                failures.push(format!("{bus}: {e}"));
            }
        }
    }
    Err(DiagError::Timeout(format!(
        "No OBD bus detected. Check ignition and wiring, or select --protocol can11/can29/iso9141/kwp-5baud/kwp-fast/j1850pwm/j1850vpw. {}",
        failures.join("; ")
    )))
}
fn optional_read(error: &DiagError) -> bool {
    matches!(
        error,
        DiagError::Timeout(_)
            | DiagError::InvalidResponse(_)
            | DiagError::UdsNegativeResponse { .. }
            | DiagError::Unsupported(_)
            | DiagError::InvalidCommand(_)
    )
}
pub fn read_vin(
    protocol: &mut ObdProtocol,
    override_vin: Option<&str>,
    cancel: &Cancel,
) -> Result<VinInfo, DiagError> {
    let supplied = override_vin.map(decode_vin).transpose()?;
    let ecus = protocol.obd_ecus()?;
    let mut observed = BTreeMap::<String, VinInfo>::new();
    for ecu in &ecus {
        cancel.check()?;
        let read = if protocol.bus().is_can() {
            protocol.set_ecu(*ecu)?;
            protocol.read_pid(9, 2).map(|value| value.value.to_string())
        } else {
            protocol
                .read_mode09(*ecu, 2)
                .map(|data| diag::text(&data[1..]))
        };
        match read.and_then(|vin| decode_vin(&vin)) {
            Ok(info) => {
                observed.insert(info.vin.clone(), info);
            }
            Err(e) if optional_read(&e) => {}
            Err(e) => return Err(e),
        }
    }
    if observed.is_empty() && protocol.bus().is_can() {
        for ecu in ecus {
            cancel.check()?;
            protocol.set_ecu(ecu)?;
            match diag::services("uds", protocol)?
                .read_data(DataId::Common(0xF190))
                .and_then(|data| decode_vin(&diag::text(&data)))
            {
                Ok(info) => {
                    observed.insert(info.vin.clone(), info);
                }
                Err(e) if optional_read(&e) => {}
                Err(e) => return Err(e),
            }
        }
    }
    if observed.len() > 1 {
        return Err(DiagError::InvalidResponse(format!(
            "ECUs report conflicting VINs: {}",
            observed.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    if let Some(actual) = observed.into_values().next() {
        if supplied
            .as_ref()
            .is_some_and(|supplied| supplied.vin != actual.vin)
        {
            return Err(DiagError::InvalidCommand(
                "--vin disagrees with the VIN read from the vehicle".into(),
            ));
        }
        return Ok(actual);
    }
    supplied.ok_or_else(|| DiagError::Timeout("No VIN available via Mode 09 or UDS F190. Supply `identify --vin VIN` or select --vehicle manually.".into()))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedVehicle {
    pub schema: u32,
    pub fingerprint: String,
    pub vin: String,
    pub vehicle: String,
    pub year: u16,
    pub variants: BTreeMap<String, VariantSelection>,
}
pub struct Cache {
    root: PathBuf,
}
impl Cache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn platform() -> Option<Self> {
        dirs::cache_dir().map(|root| Self::new(root.join("opendiag/vehicles")))
    }
    fn path(&self, vin: &str) -> Result<PathBuf, DiagError> {
        Ok(self.root.join(format!("{}.toml", decode_vin(vin)?.vin)))
    }
    pub fn load(&self, vin: &str, fingerprint: &str) -> Result<Option<CachedVehicle>, DiagError> {
        let path = self.path(vin)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(DiagError::InvalidFile(format!("{}: {e}", path.display()))),
        };
        let entry: CachedVehicle = match toml::from_str(&text) {
            Ok(entry) => entry,
            Err(_) => return Ok(None),
        };
        Ok(
            (entry.schema == 1 && entry.vin == vin && entry.fingerprint == fingerprint)
                .then_some(entry),
        )
    }
    pub fn save(&self, entry: &CachedVehicle) -> Result<(), DiagError> {
        let path = self.path(&entry.vin)?;
        std::fs::create_dir_all(&self.root).map_err(DiagError::PortError)?;
        let text = toml::to_string(entry).map_err(|e| DiagError::InvalidFile(e.to_string()))?;
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, text).map_err(DiagError::PortError)?;
        // Windows rename cannot replace an existing file. Remove only our own stale cache entry.
        if path.exists() {
            std::fs::remove_file(&path).map_err(DiagError::PortError)?;
        }
        std::fs::rename(temporary, path).map_err(DiagError::PortError)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Identification {
    pub bus: BusSpec,
    pub vin: VinInfo,
    pub candidates: Vec<String>,
    pub selection: Option<CachedVehicle>,
    pub cache_hit: bool,
}
pub fn candidate_ids(index: &[VehicleIndex], vin: &VinInfo) -> Vec<(String, u16)> {
    index
        .iter()
        .filter(|entry| entry.vin.iter().any(|prefix| vin.vin.starts_with(prefix)))
        .flat_map(|entry| {
            vin.possible_years
                .iter()
                .filter(|year| years::contains(entry.years, **year))
                .map(|year| (entry.id.to_string(), *year))
        })
        .collect()
}
pub fn identify(
    protocol: &mut ObdProtocol,
    catalog: &Catalog,
    index: &[VehicleIndex],
    override_vin: Option<&str>,
    cache: Option<&Cache>,
    cancel: &Cancel,
) -> Result<Identification, DiagError> {
    identify_with_options(
        protocol,
        catalog,
        index,
        &IdentifyOptions {
            vin: override_vin,
            vehicle: None,
        },
        cache,
        cancel,
    )
}
#[derive(Default)]
pub struct IdentifyOptions<'a> {
    pub vin: Option<&'a str>,
    pub vehicle: Option<&'a str>,
}
pub fn identify_with_options(
    protocol: &mut ObdProtocol,
    catalog: &Catalog,
    index: &[VehicleIndex],
    options: &IdentifyOptions<'_>,
    cache: Option<&Cache>,
    cancel: &Cancel,
) -> Result<Identification, DiagError> {
    let vin = read_vin(protocol, options.vin, cancel)?;
    let fingerprint = catalog.fingerprint();
    if options.vehicle.is_none() {
        if let Some(cache) = cache {
            match cache.load(&vin.vin, &fingerprint) {
                Ok(Some(entry)) => {
                    let restored = SelectedVehicle::new(catalog, &entry.vehicle, Some(entry.year))
                        .and_then(|mut selected| {
                            selected.bind_obd_bus(protocol.bus())?;
                            selected.restore_variants(&entry.variants)?;
                            Ok(selected)
                        });
                    if restored.is_ok() {
                        return Ok(Identification {
                            bus: protocol.bus().clone(),
                            vin,
                            candidates: vec![entry.vehicle.clone()],
                            selection: Some(entry),
                            cache_hit: true,
                        });
                    }
                    protocol.emit(Event::Warning(
                        "Cached ECU definitions are invalid; identifying again",
                    ));
                }
                Ok(None) => {}
                Err(e) => protocol.emit(Event::Warning(&format!("Vehicle cache unavailable: {e}"))),
            }
        }
    }
    let candidates = if let Some(id) = options.vehicle {
        let vehicle = catalog.vehicle(id)?;
        let years: Vec<_> = vin
            .possible_years
            .iter()
            .filter(|year| years::contains(&vehicle.years, **year))
            .copied()
            .collect();
        if years.len() != 1 {
            return Err(DiagError::InvalidCommand(format!(
                "VIN year does not uniquely fit selected vehicle {id} ({})",
                years::format(&vehicle.years)
            )));
        }
        vec![(id.to_string(), years[0])]
    } else {
        candidate_ids(index, &vin)
    };
    let mut matched = Vec::new();
    for (id, year) in &candidates {
        let mut selected = SelectedVehicle::new(catalog, id, Some(*year))?;
        selected.bind_obd_bus(protocol.bus())?;
        let slots = selected.slots()?;
        let mut all_match = true;
        let original_bus = protocol.bus().clone();
        for slot in slots.keys() {
            if !selected
                .resolve_ecu(slot, protocol, cancel)?
                .selection
                .matched
            {
                all_match = false;
            }
        }
        if protocol.bus() != &original_bus {
            protocol.switch_bus(&original_bus)?;
        }
        if all_match || options.vehicle.is_some() {
            matched.push(CachedVehicle {
                schema: 1,
                fingerprint: fingerprint.clone(),
                vin: vin.vin.clone(),
                vehicle: id.clone(),
                year: *year,
                variants: selected.variants(),
            });
        }
    }
    let selection = if matched.len() == 1 {
        matched.pop()
    } else {
        None
    };
    if let (Some(cache), Some(entry)) = (cache, &selection) {
        if let Err(e) = cache.save(entry) {
            protocol.emit(Event::Warning(&format!(
                "Could not save vehicle cache: {e}"
            )));
        }
    }
    Ok(Identification {
        bus: protocol.bus().clone(),
        vin,
        candidates: candidates.into_iter().map(|(id, _)| id).collect(),
        selection,
        cache_hit: false,
    })
}
pub fn detect_quiet(adapter: Rc<dyn Adapter>, cancel: &Cancel) -> Result<ObdProtocol, DiagError> {
    detect(adapter, &detection_buses(), &mut NoEvents, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        link::{DeviceOps, DiagLink, ResponseFrame},
        test_support::{protocol_with, FakeAdapter, MockLink, RecordingEvents},
        vehicle::{catalog, hooks::Hooks},
    };
    const VIN: &str = "1HGCM82633A004352";

    #[test]
    fn pwm_bus_detection_does_not_require_vin() {
        use crate::test_support::ford_replies;
        let bus = BusSpec::standard(BusKind::J1850Pwm);
        assert!(detection_buses().contains(&bus));
        let link = MockLink::on_bus(bus.clone(), &[]);
        link.script_functional(ford_replies("0100"));
        let adapter = FakeAdapter::new();
        adapter.offer_link(link.clone());
        let detected = detect(
            Rc::new(adapter),
            std::slice::from_ref(&bus),
            &mut NoEvents,
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(detected.bus(), &bus);
        assert_eq!(link.functional_requests(), ["0100"]);
    }

    #[test]
    fn accord_identification_matches_obd_calibration_without_uds_requests() {
        use crate::test_support::accord_replies;
        let bus = BusSpec::standard(BusKind::Iso9141);
        assert!(detection_buses().contains(&bus));
        let link = MockLink::on_bus(bus, &[]);
        for request in ["0100", "0902", "0904"] {
            link.script_functional(accord_replies(request));
        }
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let identified = identify(
            &mut protocol,
            catalog::embedded(),
            catalog::INDEX,
            None,
            None,
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(identified.vin.vin, "1HGCM56805A020957");
        let selected = identified.selection.unwrap();
        assert_eq!(selected.vehicle, "honda/accord-2005");
        assert_eq!(selected.year, 2005);
        assert_eq!(
            selected.variants["engine"].definition.as_deref(),
            Some("honda/37820-RAD")
        );
        assert!(selected.variants["engine"].matched);
        assert_eq!(link.functional_requests(), ["0100", "0902", "0904"]);
        assert!(link.sent().is_empty());
    }

    #[test]
    fn vpw_detection_and_vin_select_the_2002_suburban() {
        use crate::test_support::gm_replies;
        let bus = BusSpec::standard(BusKind::J1850Vpw);
        assert!(detection_buses().contains(&bus));
        let link = MockLink::on_bus(bus.clone(), &[]);
        for request in ["0100", "0902", "0902"] {
            link.script_functional(gm_replies(request));
        }
        let adapter = FakeAdapter::new();
        adapter.offer_link(link.clone());
        let mut protocol =
            detect(Rc::new(adapter), &[bus], &mut NoEvents, &Cancel::default()).unwrap();
        let identified = identify(
            &mut protocol,
            catalog::embedded(),
            catalog::INDEX,
            None,
            None,
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(identified.vin.vin, "1GNFK16Z32J211066");
        assert_eq!(identified.vin.make.as_deref(), Some("Chevrolet"));
        let selected = identified.selection.unwrap();
        assert_eq!(selected.vehicle, "chevrolet/suburban-2002");
        assert_eq!(selected.year, 2002);
        assert_eq!(
            selected.variants["engine"].definition.as_deref(),
            Some("gm/12200411")
        );
        assert!(selected.variants["engine"].matched);
        assert_eq!(link.functional_requests(), ["0100", "0902", "0902"]);
        assert!(link.sent().is_empty());
        for other in ["1GNFK16Z31J211066", "1GNEK13Z32J211066"] {
            assert!(candidate_ids(catalog::INDEX, &decode_vin(other).unwrap()).is_empty());
        }
    }
    fn discovery(link: &MockLink) {
        link.script_functional(vec![ResponseFrame {
            id: 0x18DAF101,
            data: vec![0x41, 0, 0, 0, 0, 0],
        }]);
    }
    #[test]
    fn vin_tables_preserve_year_cycles_and_reject_unsafe_cache_names() {
        let vin = decode_vin(VIN).unwrap();
        assert_eq!(vin.make.as_deref(), Some("Honda"));
        assert_eq!(vin.possible_years, [2003, 2033, 2063]);
        assert!(decode_vin("../../cache.toml").is_err());
        assert!(decode_vin("1IGCM82633A004352").is_err());
        assert_eq!(
            decode_vin("5FNRL386X8B123456").unwrap().possible_years[0],
            2008
        );
        assert!(decode_vin("ZZZCM82633A004352").unwrap().make.is_none());
    }
    #[test]
    fn detection_only_accepts_a_bus_that_answers_including_a_legacy_mock() {
        struct Selective {
            wanted: BusSpec,
            helper: FakeAdapter,
        }
        impl DeviceOps for Selective {
            fn set_programming_voltage(&self, pin: u32, mv: u32) -> Result<(), DiagError> {
                self.helper.set_programming_voltage(pin, mv)
            }
            fn read_battery_mv(&self) -> Result<u32, DiagError> {
                self.helper.read_battery_mv()
            }
            fn read_programming_voltage_mv(&self) -> Result<u32, DiagError> {
                self.helper.read_programming_voltage_mv()
            }
        }
        impl Adapter for Selective {
            fn supports(&self, _: &BusSpec) -> Option<bool> {
                None
            }
            fn connect(&self, bus: &BusSpec) -> Result<Box<dyn DiagLink>, DiagError> {
                let link = MockLink::on_bus(bus.clone(), &[]);
                if bus == &self.wanted {
                    link.script_functional(vec![ResponseFrame {
                        id: match bus.kind {
                            BusKind::IsoTp { .. } => 0x18DAF101,
                            BusKind::Iso14230 { .. } => 0x86F110,
                            _ => 0x486B10,
                        },
                        data: vec![0x41, 0, 0, 0, 0, 0],
                    }]);
                }
                self.helper.offer_link(link);
                self.helper.connect(bus)
            }
        }
        for bus in [
            BusSpec::obd_can(IdWidth::Bits29, 250_000),
            BusSpec::from_cli("iso9141", None).unwrap(),
            BusSpec::from_cli("kwp-5baud", None).unwrap(),
            BusSpec::from_cli("kwp-fast", None).unwrap(),
        ] {
            let adapter = Rc::new(Selective {
                wanted: bus.clone(),
                helper: FakeAdapter::new(),
            });
            let candidates = detection_buses();
            assert!(candidates.iter().all(BusSpec::is_implemented));
            let mut events = RecordingEvents::default();
            let protocol = detect(
                adapter.clone(),
                &candidates,
                &mut events,
                &Cancel::default(),
            )
            .unwrap();
            assert_eq!(protocol.bus(), &bus);
            assert!(adapter.helper.connects().len() > 1);
            assert!(events
                .0
                .borrow()
                .iter()
                .any(|event| event.contains("progress")));
        }
    }
    fn fixture() -> (Catalog, Vec<VehicleIndex>) {
        let vehicle = "make='Test'\nmodel='Car'\nyears=[{from=2003,to=2003}]\nvin=['1HG']\n[bus.main]\nprotocol='can29'\n[module.engine]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA01F1\n";
        let other = vehicle.replace("test/pcm", "other/pcm");
        let catalog = Catalog::from_files(
            &[("test/car", vehicle), ("other/car", &other)],
            &[
                (
                    "test/pcm",
                    "title='PCM'\ndiag='uds'\nmatch=[{data=0xF181,pattern='ABC*'}]",
                ),
                (
                    "other/pcm",
                    "title='PCM'\ndiag='uds'\nmatch=[{data=0xF181,pattern='XYZ*'}]",
                ),
            ],
            Hooks::default(),
        );
        catalog.validate().unwrap();
        let index = vec![
            VehicleIndex {
                id: "test/car",
                make: "Test",
                model: "Car",
                years: &[years::YearRange {
                    from: 2003,
                    to: 2003,
                }],
                vin: &["1HG"],
            },
            VehicleIndex {
                id: "other/car",
                make: "Test",
                model: "Car",
                years: &[years::YearRange {
                    from: 2003,
                    to: 2003,
                }],
                vin: &["1HG"],
            },
        ];
        (catalog, index)
    }
    #[test]
    fn vin_candidates_respect_disjoint_year_boundaries_and_gaps() {
        let (_, mut index) = fixture();
        index.truncate(1);
        index[0].years = &[
            years::YearRange {
                from: 2007,
                to: 2009,
            },
            years::YearRange {
                from: 2012,
                to: 2014,
            },
        ];
        let mut vin = decode_vin(VIN).unwrap();
        for year in 2006..=2015 {
            vin.possible_years = vec![year, year + 30];
            let candidates = candidate_ids(&index, &vin);
            if matches!(year, 2007..=2009 | 2012..=2014) {
                assert_eq!(candidates, [("test/car".into(), year)]);
            } else {
                assert!(candidates.is_empty(), "{year}");
            }
        }
    }

    #[test]
    fn an_explicit_vehicle_rejects_vin_years_in_a_production_gap() {
        let vehicle = "make='Test'\nmodel='Car'\nyears=[{from=2000,to=2002},{from=2004,to=2005}]\n[bus.main]\nprotocol='can29'\n[module.engine]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA01F1\n";
        let catalog = Catalog::from_files(
            &[("test/car", vehicle)],
            &[("test/pcm", "title='PCM'\ndiag='uds'\n")],
            Hooks::default(),
        );
        catalog.validate().unwrap();
        let mut selected = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
        assert!(selected
            .set_year_from_vin(&decode_vin(VIN).unwrap())
            .is_err());
        assert_eq!(selected.year, None);
        let reply = format!("490201{}", crate::utils::bytes_to_hex(VIN.as_bytes()));
        let (mut protocol, link) = protocol_with(&[&reply]);
        discovery(&link);
        let error = identify_with_options(
            &mut protocol,
            &catalog,
            &[],
            &IdentifyOptions {
                vin: None,
                vehicle: Some("test/car"),
            },
            None,
            &Cancel::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2000–2002, 2004–2005"));
        assert_eq!(link.sent(), ["0902"]);
    }

    #[test]
    fn candidates_are_narrowed_by_identity_and_a_cache_hit_skips_identity_reads() {
        let (catalog, index) = fixture();
        let vin_reply = format!("490201{}", crate::utils::bytes_to_hex(VIN.as_bytes()));
        let (mut protocol, link) = protocol_with(&[&vin_reply, "62F18141424331", "62F18141424331"]);
        discovery(&link);
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::new(directory.path().into());
        let result = identify(
            &mut protocol,
            &catalog,
            &index,
            None,
            Some(&cache),
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.selection.unwrap().vehicle, "test/car");
        assert!(!result.cache_hit);
        assert_eq!(link.sent(), ["0902", "22F181", "22F181"]);
        let (mut protocol, link) = protocol_with(&[&vin_reply]);
        discovery(&link);
        let result = identify(
            &mut protocol,
            &catalog,
            &index,
            None,
            Some(&cache),
            &Cancel::default(),
        )
        .unwrap();
        assert!(result.cache_hit);
        assert_eq!(link.sent(), ["0902"]);
        assert!(cache
            .load("1HGCM82633A004353", &catalog.fingerprint())
            .unwrap()
            .is_none());
        assert!(cache.load(VIN, "changed catalog").unwrap().is_none());
    }
    #[test]
    fn vin_falls_back_to_uds_then_manual_input_and_conflicts_are_errors() {
        let uds_reply = format!("62F190{}", crate::utils::bytes_to_hex(VIN.as_bytes()));
        let (mut protocol, link) = protocol_with(&["7F0911", &uds_reply]);
        discovery(&link);
        assert_eq!(
            read_vin(&mut protocol, None, &Cancel::default())
                .unwrap()
                .vin,
            VIN
        );
        assert_eq!(link.sent(), ["0902", "22F190"]);
        let (mut protocol, link) = protocol_with(&["7F0911", "7F2231"]);
        discovery(&link);
        assert_eq!(
            read_vin(&mut protocol, Some(VIN), &Cancel::default())
                .unwrap()
                .vin,
            VIN
        );
        let reply = format!("490201{}", crate::utils::bytes_to_hex(VIN.as_bytes()));
        let (mut protocol, link) = protocol_with(&[&reply]);
        discovery(&link);
        assert!(
            read_vin(&mut protocol, Some("1HGCM82633A004353"), &Cancel::default())
                .unwrap_err()
                .to_string()
                .contains("disagrees")
        );
    }
    #[test]
    fn an_explicit_identify_choice_can_seed_the_cache_without_a_vin_prefix() {
        let vehicle = "make='Test'\nmodel='Car'\nyears=[{from=2003,to=2003}]\nvin=[]\n[bus.main]\nprotocol='can29'\n[module.engine]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA01F1\n";
        let catalog = Catalog::from_files(
            &[("test/car", vehicle)],
            &[(
                "test/pcm",
                "title='PCM'\ndiag='uds'\nmatch=[{data=0xF181,pattern='ABC*'}]",
            )],
            Hooks::default(),
        );
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::new(directory.path().into());
        let reply = format!("490201{}", crate::utils::bytes_to_hex(VIN.as_bytes()));
        let (mut protocol, link) = protocol_with(&[&reply, "62F18141424331"]);
        discovery(&link);
        let result = identify_with_options(
            &mut protocol,
            &catalog,
            &[],
            &IdentifyOptions {
                vin: None,
                vehicle: Some("test/car"),
            },
            Some(&cache),
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(result.selection.unwrap().vehicle, "test/car");
        let (mut protocol, link) = protocol_with(&[&reply]);
        discovery(&link);
        assert!(
            identify(
                &mut protocol,
                &catalog,
                &[],
                None,
                Some(&cache),
                &Cancel::default()
            )
            .unwrap()
            .cache_hit
        );
        assert_eq!(link.sent(), ["0902"]);
    }
}
