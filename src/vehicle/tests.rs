use super::*;
use crate::link::{Adapter, IdWidth};
use crate::test_support::{
    protocol_on, FakeAdapter, FakeEcu, MockLink, RecordingEvents, ScriptedInteraction,
};
use std::rc::Rc;
const VEHICLE: &str = "make='Test'\nmodel='Car'\nyears=[{from=2000,to=2010}]\n[bus.main]\nprotocol='can29'\n[module.engine]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA01F1\n";
const BASE: &str = "title='PCM'\ndiag='uds'\n";

#[test]
fn vehicle_system_voltage_defaults_to_twelve_and_accepts_only_twelve_or_twenty_four() {
    for (setting, expected) in [
        ("", SystemVoltage::V12),
        ("system_voltage=12\n", SystemVoltage::V12),
        ("system_voltage=24\n", SystemVoltage::V24),
    ] {
        let text = format!("{setting}{VEHICLE}");
        let catalog = Catalog::from_files(
            &[("test/car", &text)],
            &[("test/pcm", BASE)],
            hooks::Hooks::default(),
        );
        catalog.validate().unwrap();
        let vehicle = catalog.vehicle("test/car").unwrap();
        assert_eq!(vehicle.system_voltage, expected);
        let encoded = toml::to_string(&vehicle).unwrap();
        let decoded: vehicle::Vehicle = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.system_voltage, expected);
    }
    for value in ["0", "6", "48", "-12", "256", "12.0", "'24'", "true"] {
        let text = format!("system_voltage={value}\n{VEHICLE}");
        let catalog = Catalog::from_files(
            &[("test/car", &text)],
            &[("test/pcm", BASE)],
            hooks::Hooks::default(),
        );
        assert!(catalog.validate().is_err(), "accepted {value}");
    }
}

#[test]
fn vehicle_system_voltage_reaches_flash_preflight_and_execution() {
    use crate::flash::{self, FlashOptions, Selection, VehicleRunOptions};
    use crate::test_support::RwdFile;

    let rwd = RwdFile::default();
    let prepared = flash::get_registry()
        .prepare(rwd.encode(), FlashOptions::default(), Selection::default())
        .unwrap();
    let definition = format!(
        "{BASE}match=[{{data=0xF181,pattern='37805-RGL-*'}}]\n[flash]\nstrategy='honda_can'\nstatus='untested'\n"
    );
    for (setting, mv, nominal) in [("", 12_400, 12), ("system_voltage=24\n", 24_800, 24)] {
        let text = format!("{setting}{VEHICLE}");
        let catalog = Catalog::from_files(
            &[("test/car", &text)],
            &[("test/pcm", &definition)],
            hooks::Hooks::default(),
        );
        catalog.validate().unwrap();
        let mut vehicle = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
        let ecu = FakeEcu::new(0x18DAF101, &rwd.versions[0], &rwd.tokens[0]);
        ecu.measure_battery_as(mv);
        let mut ui = ScriptedInteraction::new(true);
        let preflight = flash::run_for_vehicle(
            &mut protocol_on(&ecu),
            &mut vehicle,
            "engine",
            &prepared,
            &VehicleRunOptions {
                allow_untested: true,
                ..Default::default()
            },
            &mut ui,
            &Cancel::default(),
        )
        .unwrap();
        let battery = preflight
            .checks
            .iter()
            .find(|check| check.name == "battery")
            .unwrap();
        assert_eq!(battery.status, CheckStatus::Ok);
        assert!(battery.value.contains(&format!("{nominal} V system")));
        assert_eq!(ui.confirmed, 1);
        let state = ecu.state();
        assert_eq!(state.received_firmware, rwd.firmware);
        assert!(state.transfer_exited && state.dependencies_checked);
        assert_eq!(
            state.programming_voltage,
            [(12, 5000), (12, crate::obd::protocol::VOLTAGE_PIN_OFF)]
        );
    }
}

#[test]
fn ford_profile_is_manual_obd_only_and_does_not_require_a_vin() {
    let mut vehicle = SelectedVehicle::new(catalog::embedded(), "ford/f150-1997", None).unwrap();
    assert!(!vehicle.needs_year());
    let bus = vehicle.initial_bus().unwrap().unwrap();
    assert_eq!(bus, BusSpec::standard(crate::link::BusKind::J1850Pwm));
    let link = MockLink::on_bus(bus, &[]);
    let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
    let ecu = vehicle
        .resolve_ecu("engine", &mut protocol, &Cancel::default())
        .unwrap();
    assert_eq!(ecu.address, EcuAddress::Byte(0x10));
    assert_eq!(ecu.definition.diag.as_deref(), Some("obd"));
    assert!(ecu.definition.flash.is_none());
    assert!(vehicle.service_names().unwrap().is_empty());
    assert!(link.sent().is_empty() && link.functional_requests().is_empty());
    assert!(catalog::INDEX
        .iter()
        .find(|entry| entry.id == "ford/f150-1997")
        .unwrap()
        .vin
        .is_empty());
}

#[test]
fn obd_identity_sources_are_strict_and_cannot_be_flash_expectations() {
    for rule in [
        "{ pattern='X' }",
        "{ data=4, obd=4, pattern='X' }",
        "{ obd=6, pattern='X' }",
        "{ obd=4, typo=1, pattern='X' }",
    ] {
        assert!(
            fixture(&format!("{BASE}match=[{rule}]\n"))
                .validate()
                .is_err(),
            "{rule}"
        );
    }
    let obd = "title='OBD ECU'\ndiag='obd'\nmatch=[{obd=4,pattern='CAL-*'}]\n";
    fixture(obd).validate().unwrap();
    assert!(
        fixture("title='OBD ECU'\ndiag='obd'\nmatch=[{data=0xF181,pattern='CAL-*'}]")
            .validate()
            .is_err()
    );
    let flash = format!("{BASE}[flash]\nstrategy='honda_can'\nstatus='untested'\nexpect=[{{obd=4,pattern='CAL-*'}}]");
    assert!(fixture(&flash).validate().is_err());
}

#[test]
fn accord_family_does_not_match_another_calibration_or_multiple_records() {
    use crate::test_support::accord_replies;
    for multiple in [false, true] {
        let bus = BusSpec::standard(crate::link::BusKind::Iso9141);
        let link = MockLink::on_bus(bus, &[]);
        let mut replies = accord_replies("0904");
        if multiple {
            let mut extra = replies.clone();
            for reply in &mut extra {
                reply.data[2] += 4;
            }
            replies.extend(extra);
        } else {
            // A complete, differently identified calibration.
            replies[1].data[5] = b'X';
        }
        link.script_functional(replies);
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let mut vehicle =
            SelectedVehicle::new(catalog::embedded(), "honda/accord-2005", Some(2005)).unwrap();
        let ecu = vehicle
            .resolve_ecu("engine", &mut protocol, &Cancel::default())
            .unwrap();
        assert!(!ecu.selection.matched);
        assert!(ecu.definition.flash.is_none());
        assert!(link.sent().is_empty());
    }
}
#[test]
fn can_obd_identity_rejects_a_matching_but_truncated_record() {
    let catalog = fixture("title='OBD ECU'\ndiag='obd'\nmatch=[{obd=4,pattern='CAL-*'}]");
    for (body, matched) in [
        ("43414C2D", false),
        ("43414C2D313233340000000000000000", true),
    ] {
        let response = format!("490401{body}");
        let link = MockLink::new(&[&response]);
        let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
        let mut vehicle = SelectedVehicle::new(&catalog, "test/car", Some(2005)).unwrap();
        let ecu = vehicle
            .resolve_ecu("engine", &mut protocol, &Cancel::default())
            .unwrap();
        assert_eq!(ecu.selection.matched, matched);
        assert_eq!(link.sent(), ["0904"]);
    }
}

fn fixture(def: &str) -> Catalog {
    Catalog::from_files(
        &[("test/car", VEHICLE)],
        &[("test/pcm", def)],
        hooks::Hooks::default(),
    )
}

#[test]
fn every_embedded_file_is_valid_and_vehicle_ids_are_separate() {
    catalog::embedded().validate().unwrap();
    assert!(catalog::embedded().vehicle("honda/37820-RGW").is_err());
    let selected = SelectedVehicle::new(catalog::embedded(), "honda/odyssey-rl3", None).unwrap();
    assert!(selected.service_names().unwrap().is_empty());
    assert!(selected.signal_names().unwrap().is_empty());
    assert!(catalog::INDEX
        .iter()
        .any(|entry| entry.id == "honda/odyssey-rl3"));
    for entry in catalog::INDEX {
        let vehicle = catalog::embedded().vehicle(entry.id).unwrap();
        assert_eq!(entry.years, vehicle.years);
    }
}
#[test]
fn vehicle_schema_rejects_old_ecu_tables() {
    for text in [
        VEHICLE.replace("[module.engine]", "[ecu.engine]"),
        format!("{VEHICLE}[ecu.other]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA10F1\n"),
    ] {
        let error = toml::from_str::<Vehicle>(&text).unwrap_err().to_string();
        assert!(error.contains("unknown field `ecu`"), "{error}");
    }
}

#[test]
fn module_versions_select_diagnostics_only_for_years_with_uses() {
    for diagnostic in [false, true] {
        let first = if diagnostic {
            VEHICLE.to_string()
        } else {
            VEHICLE.replace("uses='test/pcm'", "label='Early engine'")
        };
        let text = first.replace("[module.engine]", "[[module.engine]]")
            + "years=[{from=2000,to=2004}]\n\
               [[module.engine]]\nlabel='Late engine'\nbus='main'\nrequest=0x18DA01F1\n\
               years=[{from=2005,to=2010}]\n";
        let catalog = Catalog::from_files(
            &[("test/car", &text)],
            &[("test/pcm", BASE)],
            hooks::Hooks::default(),
        );
        catalog.validate_vehicle("test/car").unwrap();
        let selected = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
        assert_eq!(selected.needs_year(), diagnostic);
        if diagnostic {
            assert!(selected.slots().is_err());
        } else {
            assert!(selected.slots().unwrap().is_empty());
        }
        for year in [2002, 2008] {
            let selected = SelectedVehicle::new(&catalog, "test/car", Some(year)).unwrap();
            let has_diagnostics = diagnostic && year == 2002;
            assert_eq!(
                selected.slots().unwrap().contains_key("engine"),
                has_diagnostics
            );
            assert_eq!(selected.base_ecu("engine").is_ok(), has_diagnostics);
            assert_eq!(selected.ecu_name("engine").is_ok(), has_diagnostics);
            assert_eq!(selected.ecu_name("0x18DA01F1").is_ok(), has_diagnostics);
            let label = selected.module_label_on(
                &BusSpec::obd_can(IdWidth::Bits29, 500_000),
                EcuAddress::Can {
                    request: 0x18DA01F1,
                    response: 0x18DAF101,
                },
            );
            assert_eq!(
                label,
                if year == 2008 {
                    Some("Late engine")
                } else if diagnostic {
                    None
                } else {
                    Some("Early engine")
                }
            );
        }
    }
}

#[test]
fn module_labels_are_scoped_to_bus_address_and_year() {
    let vehicle = VEHICLE.replace("[module.engine]", "[module.engine]\nlabel='Engine (ECU)'")
        + "[module.transmission]\nlabel='Transmission (TCU)'\nbus='main'\nrequest=0x18DA1DF1\nresponse=0x18DAF11D\nyears=[{from=2000,to=2003},{from=2006,to=2010}]\n\
           [bus.secondary]\nprotocol='can29'\npins=[3,11]\n\
           [module.secondary]\nlabel='Secondary module'\nbus='secondary'\nrequest=0x18DA1DF1\n";
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    catalog.validate().unwrap();
    let bus = BusSpec::obd_can(IdWidth::Bits29, 500_000);
    let address = EcuAddress::Can {
        request: 0x18DA1DF1,
        response: 0x18DAF11D,
    };
    for (year, expected) in [
        (None, None),
        (Some(2000), Some("Transmission (TCU)")),
        (Some(2003), Some("Transmission (TCU)")),
        (Some(2004), None),
        (Some(2005), None),
        (Some(2006), Some("Transmission (TCU)")),
        (Some(2010), Some("Transmission (TCU)")),
    ] {
        let selected = SelectedVehicle::new(&catalog, "test/car", year).unwrap();
        assert_eq!(selected.module_label_on(&bus, address), expected);
        let engine = selected.base_ecu("engine").unwrap();
        assert_eq!(
            selected.module_label_on(&bus, engine.address),
            Some("Engine (ECU)")
        );
        let mut secondary = bus.clone();
        secondary.pins = vec![3, 11];
        assert_eq!(
            selected.module_label_on(&secondary, address),
            Some("Secondary module")
        );
        assert_eq!(
            selected.module_label_on(&BusSpec::obd_can(IdWidth::Bits29, 250_000), address),
            None
        );
        assert_eq!(
            selected.module_label_on(&BusSpec::obd_can(IdWidth::Bits11, 500_000), address),
            None
        );
        assert_eq!(
            selected.module_label_on(
                &bus,
                EcuAddress::Can {
                    request: 0x18DA1DF1,
                    response: 0x18DAF112
                }
            ),
            None
        );
    }
}

#[test]
fn conflicting_module_labels_are_not_guessed() {
    let vehicle = VEHICLE.replace("[module.engine]", "[module.engine]\nlabel='Engine (ECU)'")
        + "[module.other]\nlabel='Other module'\nbus='main'\nrequest=0x18DA01F1\n";
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    catalog.validate().unwrap();
    let selected = SelectedVehicle::new(&catalog, "test/car", Some(2008)).unwrap();
    let engine = selected.base_ecu("engine").unwrap();
    assert_eq!(selected.module_label_on(&engine.bus, engine.address), None);
}

#[test]
fn invalid_module_metadata_names_the_file_and_key() {
    let module =
        "[module.transmission]\nlabel='Transmission (TCU)'\nbus='main'\nrequest=0x18DA1DF1\n";
    for text in [
        module.replace("label='Transmission (TCU)'\n", ""),
        module.replace("label='Transmission (TCU)'", "label='  '"),
        module.replace("bus='main'", "bus='missing'"),
        module.replace("request=0x18DA1DF1", "request=0xFFFFFFFF"),
        module.replace("request=0x18DA1DF1", "address=0x1D"),
        format!("{module}years=[]\n"),
        format!("{module}years=[{{from=1999,to=2000}}]\n"),
        format!("{module}years=[{{from=2009,to=2008}}]\n"),
        format!("{module}uses=[]\n"),
        format!("{module}uses=['test/pcm','test/pcm']\n"),
        format!("{module}uses='test/missing'\n"),
    ] {
        let vehicle = format!("{VEHICLE}{text}");
        let catalog = Catalog::from_files(
            &[("test/car", &vehicle)],
            &[("test/pcm", BASE)],
            hooks::Hooks::default(),
        );
        let error = catalog.validate().unwrap_err().to_string();
        assert!(
            error.contains("vehicles/test/car.toml") && error.contains("module.transmission"),
            "{error}"
        );
    }
    let with_flash = format!("{VEHICLE}{module}flash={{strategy='honda_can',status='verified'}}\n");
    assert!(toml::from_str::<Vehicle>(&with_flash).is_err());
    let vehicle = VEHICLE.replace("[module.engine]", "[module.engine]\nlabel=' '");
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    assert!(catalog
        .validate()
        .unwrap_err()
        .to_string()
        .contains("module.engine.label"));
}

#[test]
fn crv_transmission_label_adds_no_operational_ecu() {
    catalog::embedded()
        .validate_vehicle("honda/crv-re3")
        .unwrap();
    let selected = SelectedVehicle::new(catalog::embedded(), "honda/crv-re3", Some(2008)).unwrap();
    let serialized: toml::Value = toml::to_string(&selected.vehicle).unwrap().parse().unwrap();
    assert!(serialized.get("ecu").is_none());
    assert_eq!(
        serialized["module"]["engine"]["uses"].as_str(),
        Some("honda/37820-RZA")
    );
    assert_eq!(
        serialized["module"]["transmission"]["label"].as_str(),
        Some("Transmission (TCU)")
    );
    let bus = BusSpec::obd_can(IdWidth::Bits29, 500_000);
    let address = EcuAddress::Can {
        request: 0x18DA1DF1,
        response: 0x18DAF11D,
    };
    assert_eq!(
        selected.module_label_on(&bus, address),
        Some("Transmission (TCU)")
    );
    assert_eq!(
        selected
            .slots()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["engine"]
    );
    assert_eq!(selected.current_ecus().unwrap().len(), 1);
    assert!(selected.base_ecu("transmission").is_err());
    assert!(selected.ecu_name("transmission").is_err());
    assert!(selected.ecu_name("0x18DA1DF1").is_err());
    assert!(selected.service_names().unwrap().is_empty());
    assert!(selected.signal_names().unwrap().is_empty());
    assert_eq!(
        selected
            .base_ecu("engine")
            .unwrap()
            .definition
            .flash
            .unwrap()
            .status,
        Status::Verified
    );
}

#[test]
fn inheritance_overrides_named_entries_and_keeps_translated_defaults() {
    let base = format!("{BASE}[service.relearn]\ntitle='Base title'\nsafety='changes-settings'\nstatus='untested'\nsteps=[{{session='default'}}]\n[[signal]]\nfield='temperature=0x100:0'\n[dtc]\nP1456='Base text'\n");
    let child = "title='Variant'\nextends='test/pcm'\nmatch=[{data=0xF181,pattern='ABC*'}]\n[translations.fr]\ntitle='Variante'\n[service.relearn]\ntitle='English title'\nsafety='changes-settings'\nstatus='untested'\nsteps=[{prompt='Press pedal',translations={fr='Appuyez'}},{session='extended'}]\n[service.relearn.translations.fr]\ntitle='Titre'\ndescription='Description'\n[[signal]]\nfield='temperature=0x101:0'\n[dtc]\nP1456='Variant text'\n";
    let catalog = Catalog::from_files(&[("test/car", VEHICLE)], &[("test/pcm", &base), ("test/variant", child), ("test/unrelated", "title='Other'\ndiag='uds'\n[service.unrelated]\ntitle='Other'\nsafety='read-only'\nstatus='untested'\nsteps=[{wait_ms=0}]\n")], hooks::Hooks::default());
    catalog.validate().unwrap();
    let definition = catalog.definition("test/variant").unwrap();
    assert_eq!(definition.diag.as_deref(), Some("uds"));
    assert_eq!(definition.services["relearn"].title, "English title");
    assert_eq!(
        definition.services["relearn"].translations["fr"]
            .title
            .as_deref(),
        Some("Titre")
    );
    assert_eq!(definition.signals.len(), 1);
    assert!(definition.signals[0].field.contains("0x101"));
    let ecu = FakeEcu::new(0x18DAF101, b"ABC1", &[1; 4]);
    let mut selected = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
    selected
        .resolve_ecu("engine", &mut protocol_on(&ecu), &Cancel::default())
        .unwrap();
    assert_eq!(selected.service_names().unwrap(), ["engine.relearn"]);
    assert_eq!(
        selected
            .dtc_description(selected.base_ecu("engine").unwrap().address, "P1456")
            .as_deref(),
        Some("Variant text")
    );
    assert_eq!(
        selected.dtc_description(selected.base_ecu("engine").unwrap().address, "P0143"),
        None
    );
    assert!(crate::obd::dtc::get_dtc_description("P0100").contains("MAF"));
}
#[test]
fn broken_references_cycles_years_and_overlapping_patterns_name_the_file_and_key() {
    let cycle = Catalog::from_files(
        &[],
        &[
            ("test/a", "title='A'\nextends='test/b'"),
            ("test/b", "title='B'\nextends='test/a'"),
        ],
        hooks::Hooks::default(),
    );
    let error = cycle.validate().unwrap_err().to_string();
    assert!(error.contains("ecus/test/a.toml") && error.contains("extends"));
    let missing = Catalog::from_files(&[("test/car", VEHICLE)], &[], hooks::Hooks::default());
    let error = missing.validate().unwrap_err().to_string();
    assert!(error.contains("vehicles/test/car.toml") && error.contains("module.engine.uses"));
    let overlap_vehicle = VEHICLE.replace("[module.engine]", "[[module.engine]]") + "years=[{from=2000,to=2005}]\n[[module.engine]]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA01F1\nyears=[{from=2005,to=2010}]\n";
    let overlap = Catalog::from_files(
        &[("test/car", &overlap_vehicle)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    let error = overlap.validate().unwrap_err().to_string();
    assert!(error.contains("module.engine.years"));
    let variants = Catalog::from_files(
        &[],
        &[
            ("test/pcm", BASE),
            (
                "test/a",
                "title='A'\nextends='test/pcm'\nmatch=[{data=0xF181,pattern='ABC*'}]",
            ),
            (
                "test/b",
                "title='B'\nextends='test/pcm'\nmatch=[{data=0xF181,pattern='ABCD*'}]",
            ),
        ],
        hooks::Hooks::default(),
    );
    assert!(variants
        .validate()
        .unwrap_err()
        .to_string()
        .contains("overlapping variants"));
}
#[test]
fn schema_rejects_unknown_text_keys_invalid_steps_and_false_verification() {
    for (extra, key) in [
        ("[translations.fr]\nwire='oops'\n", "wire"),
        ("[service.bad]\ntitle='Bad'\nsafety='read-only'\nstatus='untested'\nsteps=[{request='1003',expect='5003'}]\n", "safety"),
        ("[service.bad]\ntitle='Bad'\nsafety='destructive'\nstatus='verified'\nsteps=[{session='extended'}]\n", "verified_on"),
        ("[service.bad]\ntitle='Bad'\nsafety='changes-settings'\nstatus='untested'\nsteps=[{session='default',wait_ms=1}]\n", "steps"),
    ] { let error = fixture(&format!("{BASE}{extra}")).validate().unwrap_err().to_string(); assert!(error.contains(key), "{error}"); }
}
#[test]
fn variants_are_selected_once_and_unmatched_bases_refuse_destructive_services() {
    let service = "[service.program]\ntitle='Program'\nsafety='destructive'\nstatus='untested'\nsteps=[{session='programming'}]\n";
    let base = format!("{BASE}{service}");
    let catalog = Catalog::from_files(
        &[("test/car", VEHICLE)],
        &[
            ("test/pcm", &base),
            (
                "test/a",
                "title='A'\nextends='test/pcm'\nmatch=[{data=0xF181,pattern='ABC*'}]",
            ),
        ],
        hooks::Hooks::default(),
    );
    catalog.validate().unwrap();
    for calibration in [b"ABC1".as_slice(), b"XYZ1".as_slice()] {
        let ecu = FakeEcu::new(0x18DAF101, calibration, &[1; 4]);
        let mut protocol = protocol_on(&ecu);
        let mut vehicle = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
        let resolved = vehicle
            .resolve_ecu("engine", &mut protocol, &Cancel::default())
            .unwrap();
        assert_eq!(resolved.selection.matched, calibration.starts_with(b"ABC"));
        vehicle
            .resolve_ecu("engine", &mut protocol, &Cancel::default())
            .unwrap();
        assert_eq!(ecu.state().requests.len(), 1);
        let mut ui = ScriptedInteraction::new(true);
        assert!(vehicle
            .run_service("program", &mut protocol, &mut ui, &Cancel::default(), false)
            .is_err());
        assert_eq!(ui.confirmed, 0);
        assert_eq!(ecu.state().requests.len(), 1);
        let result =
            vehicle.run_service("program", &mut protocol, &mut ui, &Cancel::default(), true);
        assert_eq!(result.is_ok(), calibration.starts_with(b"ABC"));
        assert_eq!(ui.confirmed, usize::from(calibration.starts_with(b"ABC")));
    }
}
#[test]
fn declarative_procedure_confirms_then_sends_exact_requests_and_prompt_abort_stops() {
    let definition = format!("{BASE}[service.setup]\ntitle='Setup'\nsafety='changes-settings'\nstatus='untested'\nsteps=[{{session='extended'}},{{request='1001',expect='5001'}}]\n[service.prompt]\ntitle='Prompt'\nsafety='changes-settings'\nstatus='untested'\nsteps=[{{prompt='Press pedal'}},{{session='programming'}}]\n");
    let catalog = fixture(&definition);
    catalog.validate().unwrap();
    let ecu = FakeEcu::new(0x18DAF101, b"ABC", &[1; 4]);
    let mut protocol = protocol_on(&ecu);
    let mut selected = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
    let mut ui = ScriptedInteraction::new(true);
    selected
        .run_service("setup", &mut protocol, &mut ui, &Cancel::default(), false)
        .unwrap();
    assert_eq!(ui.confirmed, 1);
    assert_eq!(ecu.state().requests, [vec![0x10, 3], vec![0x10, 1]]);
    assert!(matches!(
        selected.run_service("prompt", &mut protocol, &mut ui, &Cancel::default(), false),
        Err(DiagError::Cancelled)
    ));
    assert_eq!(ecu.state().requests.len(), 2);
    let mut refused = ScriptedInteraction::new(false);
    assert!(selected
        .run_service(
            "setup",
            &mut protocol,
            &mut refused,
            &Cancel::default(),
            false
        )
        .is_err());
    assert_eq!(ecu.state().requests.len(), 2);
}
#[test]
fn ambiguous_short_names_list_qualified_choices() {
    let names = vec![
        "engine.relearn".into(),
        "abs.relearn".into(),
        "engine.reset".into(),
    ];
    assert_eq!(resolve_name("reset", &names).unwrap(), "engine.reset");
    assert_eq!(resolve_name("abs.relearn", &names).unwrap(), "abs.relearn");
    let error = resolve_name("relearn", &names).unwrap_err().to_string();
    assert!(error.contains("engine.relearn") && error.contains("abs.relearn"));
}
#[test]
fn invalid_year_ranges_are_rejected_for_vehicles_and_slots() {
    for years in [
        "[]",
        "[2000,2010]",
        "[[2000,2010]]",
        "{from=2000,to=2010}",
        "[{from=2010,to=2000}]",
        "[{from=2000,to=2005},{from=2005,to=2010}]",
        "[{from=2000,to=2009},{from=2002,to=2003}]",
        "[{from=2000}]",
        "[{from=2000,to=2010,until=2011}]",
        "[{from=-1,to=2010}]",
        "[{from=2000,to=65536}]",
    ] {
        for slot in [false, true] {
            let vehicle = if slot {
                format!("{VEHICLE}years={years}\n")
            } else {
                VEHICLE.replace("years=[{from=2000,to=2010}]", &format!("years={years}"))
            };
            let catalog = Catalog::from_files(
                &[("test/car", &vehicle)],
                &[("test/pcm", BASE)],
                hooks::Hooks::default(),
            );
            let error = catalog.validate().unwrap_err().to_string();
            assert!(error.contains("vehicles/test/car.toml"), "{years}: {error}");
            let key = if slot { "module.engine" } else { "years" };
            assert!(error.contains(key), "{years}: {error}");
        }
    }
}

#[test]
fn disjoint_years_select_slots_and_exclude_production_gaps() {
    let vehicle = VEHICLE
        .replace(
            "[{from=2000,to=2010}]",
            "[{from=2000,to=2003},{from=2006,to=2010}]",
        )
        .replace("[module.engine]", "[[module.engine]]")
        + "years=[{from=2000,to=2001},{from=2009,to=2010}]\n\
           [[module.engine]]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA10F1\n\
           years=[{from=2002,to=2003},{from=2006,to=2007}]\n";
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    catalog.validate().unwrap();
    for (year, request) in [
        (2000, 0x18DA01F1),
        (2001, 0x18DA01F1),
        (2002, 0x18DA10F1),
        (2003, 0x18DA10F1),
        (2006, 0x18DA10F1),
        (2007, 0x18DA10F1),
        (2009, 0x18DA01F1),
        (2010, 0x18DA01F1),
    ] {
        let selected = SelectedVehicle::new(&catalog, "test/car", Some(year)).unwrap();
        assert_eq!(selected.slots().unwrap()["engine"].request, Some(request));
        assert_eq!(
            selected.ecu_name(&format!("0x{request:X}")).unwrap(),
            "engine"
        );
        let other = if request == 0x18DA01F1 {
            "0x18DA10F1"
        } else {
            "0x18DA01F1"
        };
        assert!(selected.ecu_name(other).is_err());
    }
    for year in [1999, 2004, 2005, 2011] {
        let error = SelectedVehicle::new(&catalog, "test/car", Some(year))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("2000–2003, 2006–2010"));
    }
    let selected = SelectedVehicle::new(&catalog, "test/car", Some(2008)).unwrap();
    assert!(selected.slots().unwrap().is_empty());
    assert!(selected.ecu_name("0x18DA01F1").is_err());
    assert!(selected.ecu_name("0x18DA10F1").is_err());

    let overlapping = vehicle.replace("from=2006,to=2007", "from=2006,to=2009");
    let catalog = Catalog::from_files(
        &[("test/car", &overlapping)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    assert!(catalog
        .validate()
        .unwrap_err()
        .to_string()
        .contains("module.engine.years"));
}

#[test]
fn slot_years_must_be_covered_by_the_vehicle_union() {
    let vehicle = VEHICLE.replace(
        "[{from=2000,to=2010}]",
        "[{from=2008,to=2010},{from=2000,to=2003},{from=2006,to=2007}]",
    );
    for (slot_years, valid) in [
        ("", true),
        ("years=[{from=2001,to=2002},{from=2006,to=2010}]", true),
        ("years=[{from=2002,to=2006}]", false),
        ("years=[{from=2004,to=2005}]", false),
        ("years=[{from=1999,to=2000}]", false),
        ("years=[{from=2010,to=2011}]", false),
    ] {
        let text = format!("{vehicle}{slot_years}\n");
        let catalog = Catalog::from_files(
            &[("test/car", &text)],
            &[("test/pcm", BASE)],
            hooks::Hooks::default(),
        );
        let result = catalog.validate();
        assert_eq!(result.is_ok(), valid, "{slot_years}: {result:?}");
        if let Err(error) = result {
            assert!(error.to_string().contains("module.engine.years"));
        }
    }
}

#[test]
fn year_ranges_serialize_with_named_endpoints_and_display_gaps() {
    let text = VEHICLE.replace(
        "[{from=2000,to=2010}]",
        "[{from=2000,to=2003},{from=2006,to=2006},{from=2009,to=2010}]",
    ) + "years=[{from=2006,to=2006}]\n";
    let vehicle: Vehicle = toml::from_str(&text).unwrap();
    assert_eq!(years::format(&vehicle.years), "2000–2003, 2006, 2009–2010");
    let serialized = toml::to_string(&vehicle).unwrap();
    let value: toml::Value = serialized.parse().unwrap();
    assert_eq!(value["years"][0]["from"].as_integer(), Some(2000));
    assert_eq!(value["years"][0]["to"].as_integer(), Some(2003));
    assert_eq!(
        value["module"]["engine"]["years"][0]["from"].as_integer(),
        Some(2006)
    );
    let restored: Vehicle = toml::from_str(&serialized).unwrap();
    assert_eq!(restored.years, vehicle.years);
    assert_eq!(
        restored.modules["engine"].entries()[0].years,
        vehicle.modules["engine"].entries()[0].years
    );
}

#[test]
fn year_slot_versions_are_chosen_without_overlap() {
    let vehicle = VEHICLE.replace("[module.engine]", "[[module.engine]]") + "years=[{from=2000,to=2004}]\n[[module.engine]]\nuses='test/pcm'\nbus='main'\nrequest=0x18DA10F1\nyears=[{from=2005,to=2010}]\n";
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    catalog.validate().unwrap();
    assert!(SelectedVehicle::new(&catalog, "test/car", None)
        .unwrap()
        .slots()
        .is_err());
    let mut unknown_year = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
    assert!(unknown_year.initial_bus().unwrap().is_some());
    assert!(unknown_year.needs_year());
    unknown_year
        .set_year_from_vin(&crate::identify::decode_vin("5FNRL386X8B123456").unwrap())
        .unwrap();
    assert_eq!(unknown_year.year, Some(2008));
    assert_eq!(unknown_year.ecu_name("0x18DA10F1").unwrap(), "engine");
    assert!(unknown_year.ecu_name("0x18DA01F1").is_err());
    let selected = SelectedVehicle::new(&catalog, "test/car", Some(2008)).unwrap();
    assert_eq!(
        selected.base_ecu("engine").unwrap().address,
        EcuAddress::Can {
            request: 0x18DA10F1,
            response: 0x18DAF110
        }
    );
}
#[test]
fn mixed_bus_data_is_ready_for_backends_and_switching_reconnects_with_init_address() {
    let vehicle = format!("{VEHICLE}[bus.abs]\nprotocol='kwp-5baud'\ninit_address=0x28\ntiming={{P3_MIN=55}}\n[module.abs]\nuses='test/abs'\nbus='abs'\naddress=0x28\n");
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[
            ("test/pcm", BASE),
            ("test/abs", "title='ABS'\ndiag='kwp2000'"),
        ],
        hooks::Hooks::default(),
    );
    assert!(catalog
        .validate()
        .unwrap_err()
        .to_string()
        .contains("not implemented"));
    catalog.validate_with_legacy(true).unwrap();
    let selected = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
    let adapter = FakeAdapter::new();
    let can = BusSpec::obd_can(IdWidth::Bits29, 500_000);
    adapter.offer_link(MockLink::on_bus(can.clone(), &[]));
    let abs = selected.base_ecu("abs").unwrap();
    adapter.offer_link(MockLink::on_bus(abs.bus.clone(), &[]));
    let mut protocol = ObdProtocol::new(Rc::new(adapter.clone()), adapter.connect(&can).unwrap());
    let events = RecordingEvents::default();
    protocol.set_events(Box::new(events.clone()));
    SelectedVehicle::activate(&mut protocol, &selected.base_ecu("engine").unwrap()).unwrap();
    SelectedVehicle::activate(&mut protocol, &abs).unwrap();
    assert_eq!(adapter.connects().len(), 2);
    assert_eq!(adapter.connects()[1].init_address, Some(0x28));
    assert_eq!(adapter.connects()[1].timing["P3_MIN"], 55);
    assert!(events
        .0
        .borrow()
        .iter()
        .any(|s| s.contains("Connecting") && s.contains("abs")));
}
#[test]
fn raw_identity_expectation_failure_never_authorizes_or_erases() {
    let rwd = crate::test_support::RwdFile::default();
    let ecu = FakeEcu::new(0x18DAF101, &rwd.versions[0], &rwd.tokens[0]);
    let prepared = crate::flash::get_registry()
        .prepare(
            rwd.firmware.clone(),
            crate::flash::FlashOptions {
                start_address: Some(rwd.start),
                security_key: Some(rwd.tokens[0].clone()),
                ..Default::default()
            },
            crate::flash::Selection {
                vehicle: Some("test/car"),
                ecu: Some(EcuAddress::Can {
                    request: 0x18DA01F1,
                    response: 0x18DAF101,
                }),
                strategy: Some("honda_can"),
                ..Default::default()
            },
        )
        .unwrap();
    let mut ui = ScriptedInteraction::new(true);
    let result = crate::flash::run(
        &mut protocol_on(&ecu),
        &prepared,
        &crate::flash::RunOptions {
            expected: vec![(0xF181, "37805-OTHER*".into())],
            allow_untested: true,
            ..Default::default()
        },
        &mut ui,
        &Cancel::default(),
    );
    assert!(result.is_err());
    assert_eq!(ui.confirmed, 0);
    assert!(!ecu.state().erased);
}

#[test]
fn custom_can_response_pairs_are_used_without_standard_address_arithmetic() {
    let vehicle = VEHICLE
        .replace("can29", "can11")
        .replace("request=0x18DA01F1", "request=0x600\nresponse=0x601");
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[("test/pcm", BASE)],
        hooks::Hooks::default(),
    );
    catalog.validate().unwrap();
    let selected = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
    let link = MockLink::on_bus(BusSpec::obd_can(IdWidth::Bits11, 500_000), &[]);
    let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
    SelectedVehicle::activate(&mut protocol, &selected.base_ecu("engine").unwrap()).unwrap();
    assert_eq!(link.pairs(), [(0x600, 0x601)]);
    protocol.set_target(0x600, 0x602).unwrap();
    assert_eq!(link.pairs(), [(0x600, 0x601), (0x600, 0x602)]);
}

#[test]
fn a_cached_variant_is_rechecked_before_a_destructive_service() {
    let base = format!("{BASE}[service.program]\ntitle='Program'\nsafety='destructive'\nstatus='untested'\nsteps=[{{session='programming'}}]\n");
    let catalog = Catalog::from_files(
        &[("test/car", VEHICLE)],
        &[
            ("test/pcm", &base),
            (
                "test/a",
                "title='A'\nextends='test/pcm'\nmatch=[{data=0xF181,pattern='ABC*'}]",
            ),
        ],
        hooks::Hooks::default(),
    );
    let mut selected = SelectedVehicle::new(&catalog, "test/car", None).unwrap();
    selected
        .restore_variants(&BTreeMap::from([(
            "engine".into(),
            VariantSelection {
                definition: Some("test/a".into()),
                matched: true,
            },
        )]))
        .unwrap();
    let ecu = FakeEcu::new(0x18DAF101, b"XYZ", &[1; 4]);
    let mut ui = ScriptedInteraction::new(true);
    let result = selected.run_service(
        "program",
        &mut protocol_on(&ecu),
        &mut ui,
        &Cancel::default(),
        true,
    );
    assert!(result.is_err());
    assert_eq!(ui.confirmed, 0);
    assert_eq!(ecu.state().requests, [vec![0x22, 0xF1, 0x81]]);
}

#[test]
fn definition_reuse_does_not_require_variant_patterns_and_ancestor_guards_are_kept() {
    let base = format!("{BASE}match=[{{data=0xF181,pattern='ABC*'}}]\n[service.reset]\ntitle='Base reset'\nsafety='changes-settings'\nstatus='untested'\nsteps=[{{session='default'}}]\n");
    let child = "title='Reused'\nextends='test/pcm'\n[service.reset]\ntitle='Override'\nsafety='changes-settings'\nstatus='untested'\nsteps=[{session='extended'}]\n";
    let vehicle = VEHICLE.replace("test/pcm", "test/reused");
    let catalog = Catalog::from_files(
        &[("test/car", &vehicle)],
        &[("test/pcm", &base), ("test/reused", child)],
        hooks::Hooks::default(),
    );
    catalog.validate().unwrap();
    assert!(catalog.children("test/pcm").unwrap().is_empty());
    assert_eq!(
        catalog.definition("test/reused").unwrap().services["reset"].title,
        "Override"
    );
    assert_eq!(
        catalog.identity_rules("test/reused").unwrap(),
        std::collections::BTreeSet::from([ecu_def::IdentitySource::Data { data: 0xF181 }])
    );
    assert!(
        !catalog
            .match_variant(
                "test/reused",
                &BTreeMap::from([(ecu_def::IdentitySource::Data { data: 0xF181 }, "XYZ".into())])
            )
            .unwrap()
            .1
    );
    assert!(
        catalog
            .match_variant(
                "test/reused",
                &BTreeMap::from([(
                    ecu_def::IdentitySource::Data { data: 0xF181 },
                    "ABC1".into()
                )])
            )
            .unwrap()
            .1
    );
}
