use super::*;
use crate::events::{Answer, Events};
use crate::test_support::{protocol_on, FakeEcu, RecordingEvents, ScriptedInteraction};
use crate::vehicle::catalog;

const SOFTWARE: &[u8] = b"37805-RGW-A960";

fn odyssey() -> SelectedVehicle<'static> {
    SelectedVehicle::new(catalog::embedded(), "honda/odyssey-rl3", Some(2008)).unwrap()
}

/// An Odyssey engine ECU holding `base + index` in its EEPROM. By default it
/// refuses `11 01`, so access needs a power cycle.
fn ecu(software: &[u8], base: u16) -> FakeEcu {
    let ecu = FakeEcu::new(0x18DAF111, software, &[0; 4]);
    ecu.fill_eeprom(base);
    ecu
}

/// The person at the bench: agrees to every prompt, switches the ECU off and
/// on when asked to, and answers the final question with `confirm`.
struct Bench {
    ecu: FakeEcu,
    confirm: bool,
    confirmed: usize,
    prompts: Vec<String>,
    /// `10 03` polls the ECU still answers after the power-cycle prompt, and
    /// how many it then misses while off.
    power_cycle: (usize, usize),
    /// Something done to the ECU while the final question is open.
    on_confirm: Option<Box<dyn FnMut()>>,
}
fn bench(ecu: &FakeEcu, confirm: bool) -> Bench {
    Bench {
        ecu: ecu.clone(),
        confirm,
        confirmed: 0,
        prompts: Vec::new(),
        power_cycle: (0, 1),
        on_confirm: None,
    }
}
impl Bench {
    fn power_cycle_prompts(&self) -> usize {
        self.prompts
            .iter()
            .filter(|p| p.contains("Turn the ECU off"))
            .count()
    }
}
impl Interaction for Bench {
    fn confirm(&mut self, _: &Preflight) -> bool {
        self.confirmed += 1;
        if let Some(action) = &mut self.on_confirm {
            action();
        }
        self.confirm
    }
    fn prompt(&mut self, text: &str) -> Answer {
        self.prompts.push(text.into());
        if text.contains("Turn the ECU off") {
            let (answered, off) = self.power_cycle;
            self.ecu.power_cycle_after(answered, off);
        }
        Answer::Continue
    }
}

fn ba_writes(ecu: &FakeEcu) -> usize {
    ecu.state()
        .requests
        .iter()
        .filter(|r| r.starts_with(&[0xBA, 0x15]))
        .count()
}
fn unlock_requests(ecu: &FakeEcu) -> usize {
    ecu.state()
        .requests
        .iter()
        .filter(|r| r.starts_with(&[0x27]))
        .count()
}

fn take_backup(ecu: &FakeEcu) -> EcuBackup {
    backup(
        &mut protocol_on(ecu),
        &mut odyssey(),
        "engine",
        true,
        &mut bench(ecu, true),
        &Cancel::default(),
    )
    .unwrap()
}

#[test]
fn backup_reads_every_value_twice_and_records_the_ecu() {
    let donor = ecu(SOFTWARE, 0x1000);
    let backup = take_backup(&donor);

    assert_eq!(backup.format, FORMAT);
    assert_eq!(backup.vehicle.id, "honda/odyssey-rl3");
    assert_eq!(backup.vehicle.vin.as_deref(), Some("1HGCM82633A004352"));
    assert_eq!(backup.ecu.slot, "engine");
    assert_eq!(backup.ecu.definition, "honda/37820-RGW");
    assert_eq!(backup.ecu.software, "37805-RGW-A960");
    assert_eq!(backup.ecu.strategy, "honda_ba");
    assert_eq!(backup.blocks.len(), 2);
    assert_eq!(backup.blocks[0].words[0], 0x1000);
    assert_eq!(backup.blocks[1].words[255], 0x1000 + 511);
    backup.validate().unwrap();

    let state = donor.state();
    let reads = state
        .requests
        .iter()
        .filter(|r| r.starts_with(&[0xBA, 0x11]));
    assert_eq!(reads.count(), 1024);
    assert!(state.ba_writes.is_empty());
    assert!(
        state.protocol_violations.is_empty(),
        "{:?}",
        state.protocol_violations
    );
}

fn requests(ecu: &FakeEcu, prefix: &[u8]) -> usize {
    ecu.state()
        .requests
        .iter()
        .filter(|r| r.starts_with(prefix))
        .count()
}
fn run_backup(ecu: &FakeEcu, ui: &mut dyn Interaction) -> Result<EcuBackup, DiagError> {
    backup(
        &mut protocol_on(ecu),
        &mut odyssey(),
        "engine",
        true,
        ui,
        &Cancel::default(),
    )
}

/// Captured from a 2008 CR-V (37805-RZA-A560): a length byte, the VIN and
/// NUL padding.
const CAPTURED_F190: &str = "1133435A5245333833373847373039333837000000";

#[test]
fn a_length_prefixed_vin_is_recorded_without_its_prefix() {
    let donor = ecu(SOFTWARE, 0);
    donor.reply_to_vin_with(&crate::utils::hex_to_bytes(CAPTURED_F190).unwrap());
    let backup = take_backup(&donor);
    assert_eq!(backup.vehicle.vin.as_deref(), Some("3CZRE38378G709387"));
}

#[test]
fn a_restart_grants_access_without_a_power_cycle() {
    // Quiet for two polls while rebooting, or back before the first poll.
    for quiet in [2, 0] {
        let donor = ecu(SOFTWARE, 0x1000);
        donor.allow_restart(quiet, true);
        let mut ui = bench(&donor, true);
        let backup = run_backup(&donor, &mut ui).unwrap();
        assert_eq!(backup.blocks[0].words[0], 0x1000);
        assert_eq!(ui.prompts.len(), 1, "{:?}", ui.prompts);
        assert!(ui.prompts[0].contains("engine is not running"));
        assert_eq!(requests(&donor, &[0x11, 0x01]), 1);
        assert_eq!(requests(&donor, &[0x27, 0x61]), 1);
        assert!(donor.state().protocol_violations.is_empty());
    }
}

#[test]
fn an_untested_backup_is_refused_before_unlocking_without_allow_untested() {
    let donor = ecu(SOFTWARE, 0);
    donor.allow_restart(0, true);
    let mut ui = bench(&donor, true);
    let error = backup(
        &mut protocol_on(&donor),
        &mut odyssey(),
        "engine",
        false,
        &mut ui,
        &Cancel::default(),
    )
    .unwrap_err();
    assert!(matches!(error, DiagError::PreflightFailed(_)), "{error}");
    assert!(error.to_string().contains("--allow-untested"), "{error}");
    assert!(ui.prompts.is_empty());
    assert_eq!(requests(&donor, &[0x11]), 0);
    assert_eq!(unlock_requests(&donor), 0);
}

#[test]
fn an_untested_backup_warns_but_still_reads() {
    let donor = ecu(SOFTWARE, 0);
    let mut protocol = protocol_on(&donor);
    let events = RecordingEvents::default();
    protocol.set_events(Box::new(events.clone()));
    backup(
        &mut protocol,
        &mut odyssey(),
        "engine",
        true,
        &mut bench(&donor, true),
        &Cancel::default(),
    )
    .unwrap();
    let events = events.0.borrow();
    let warning = events.iter().find(|e| e.contains("untested")).unwrap();
    assert!(warning.starts_with("warning:"), "{warning}");
    assert!(warning.contains("honda/37820-RGW"), "{warning}");
}

#[test]
fn a_refused_restart_falls_back_to_a_power_cycle() {
    let donor = ecu(SOFTWARE, 0);
    let mut ui = bench(&donor, true);
    let mut protocol = protocol_on(&donor);
    let events = RecordingEvents::default();
    protocol.set_events(Box::new(events.clone()));
    backup(
        &mut protocol,
        &mut odyssey(),
        "engine",
        true,
        &mut ui,
        &Cancel::default(),
    )
    .unwrap();
    assert_eq!(ui.power_cycle_prompts(), 1);
    assert!(events
        .0
        .borrow()
        .iter()
        .any(|e| e.contains("refused to restart")));
}

#[test]
fn a_restart_that_does_not_open_access_falls_back_without_sending_a_key() {
    let donor = ecu(SOFTWARE, 0);
    donor.allow_restart(1, false);
    let mut ui = bench(&donor, true);
    run_backup(&donor, &mut ui).unwrap();
    assert_eq!(ui.power_cycle_prompts(), 1);
    assert_eq!(requests(&donor, &[0x27, 0x61]), 2, "seed after each start");
    assert_eq!(
        requests(&donor, &[0x27, 0x62]),
        1,
        "a key only for a granted seed"
    );
    assert!(donor.state().protocol_violations.is_empty());
}

#[test]
fn a_power_cycle_waits_for_the_ecu_to_turn_off_and_back_on() {
    let donor = ecu(SOFTWARE, 0);
    let mut ui = bench(&donor, true);
    // Still on for the first two polls, then off for three.
    ui.power_cycle = (2, 3);
    run_backup(&donor, &mut ui).unwrap();
    let state = donor.state();
    let reset = state
        .requests
        .iter()
        .position(|r| r[..] == [0x11, 0x01])
        .unwrap();
    let seed = state
        .requests
        .iter()
        .position(|r| r[..] == [0x27, 0x61])
        .unwrap();
    let polls = state.requests[reset..seed]
        .iter()
        .filter(|r| r[..] == [0x10, 0x03])
        .count();
    assert_eq!(polls, 2 + 3 + 1, "on, off, then answering after power-up");
    assert!(state.protocol_violations.is_empty());
}

#[test]
fn declining_a_prompt_cancels_before_security_access() {
    // The restart prompt, then (after a refused restart) the power-cycle prompt.
    for continued in [0, 1] {
        let donor = ecu(SOFTWARE, 0);
        let mut ui = ScriptedInteraction::new(true);
        ui.answers.extend(vec![Answer::Continue; continued]);
        ui.answers.push_back(Answer::Abort);
        let result = run_backup(&donor, &mut ui);
        assert!(matches!(result, Err(DiagError::Cancelled)));
        assert_eq!(unlock_requests(&donor), 0);
        assert_eq!(
            requests(&donor, &[0x11]),
            continued,
            "no restart unless agreed"
        );
    }
}

#[test]
fn a_value_that_changes_between_reads_fails_the_backup() {
    let donor = ecu(SOFTWARE, 0);
    donor.make_unstable(1, 0x20);
    let error = backup(
        &mut protocol_on(&donor),
        &mut odyssey(),
        "engine",
        true,
        &mut bench(&donor, true),
        &Cancel::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("block 1 value 0x20"), "{error}");
}

#[test]
fn ecus_without_a_backup_strategy_or_matching_variant_are_refused_before_unlocking() {
    for software in [&b"37805-RGL-A000"[..], b"37805-XYZ-0000"] {
        let other = ecu(software, 0);
        let error = backup(
            &mut protocol_on(&other),
            &mut odyssey(),
            "engine",
            true,
            &mut bench(&other, true),
            &Cancel::default(),
        )
        .unwrap_err();
        assert!(matches!(error, DiagError::Unsupported(_)), "{error}");
        assert_eq!(unlock_requests(&other), 0);
    }
}

#[test]
fn backup_files_round_trip_as_json_and_are_never_overwritten() {
    let backup = take_backup(&ecu(SOFTWARE, 0xA1B2));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new").join("old-ecu.json");

    backup.save_new(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"A1B2\""), "values are 4-digit hex");
    assert_eq!(EcuBackup::load(&path).unwrap(), backup);
    assert!(backup.save_new(&path).is_err(), "an existing file is kept");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn safety_copies_are_named_after_the_ecu_and_never_overwritten() {
    let copy = take_backup(&ecu(SOFTWARE, 0));
    let dir = tempfile::tempdir().unwrap();
    let first = save_safety_copy(dir.path(), &copy).unwrap();
    let second = save_safety_copy(dir.path(), &copy).unwrap();
    let stamp = file_stamp(&copy.created).unwrap();
    assert_eq!(
        first,
        dir.path()
            .join(format!("engine-before-restore-{stamp}.json"))
    );
    assert_eq!(
        second,
        dir.path()
            .join(format!("engine-before-restore-{stamp}-2.json"))
    );
    assert_eq!(EcuBackup::load(&second).unwrap(), copy);
}

#[test]
fn malformed_backup_files_are_rejected() {
    let backup = take_backup(&ecu(SOFTWARE, 0));
    let good = serde_json::to_value(&backup).unwrap();
    let dir = tempfile::tempdir().unwrap();
    type Edit = Box<dyn Fn(&mut serde_json::Value)>;
    let edits: Vec<(&str, Edit)> = vec![
        ("unknown field", Box::new(|v| v["checksum"] = "x".into())),
        (
            "format",
            Box::new(|v| v["format"] = "something-else".into()),
        ),
        ("version", Box::new(|v| v["version"] = 2.into())),
        ("created", Box::new(|v| v["created"] = "yesterday".into())),
        (
            "vin",
            Box::new(|v| v["vehicle"]["vin"] = "../../etc".into()),
        ),
        ("slot", Box::new(|v| v["ecu"]["slot"] = "../x".into())),
        (
            "strategy",
            Box::new(|v| v["ecu"]["strategy"] = "nope".into()),
        ),
        (
            "hex",
            Box::new(|v| v["blocks"][0]["words"][3] = "12G4".into()),
        ),
        (
            "short hex",
            Box::new(|v| v["blocks"][0]["words"][3] = "123".into()),
        ),
        (
            "word count",
            Box::new(|v| {
                v["blocks"][1]["words"].as_array_mut().unwrap().pop();
            }),
        ),
        ("block id", Box::new(|v| v["blocks"][1]["id"] = "2".into())),
        (
            "block count",
            Box::new(|v| {
                v["blocks"].as_array_mut().unwrap().pop();
            }),
        ),
    ];
    for (name, edit) in edits {
        let mut value = good.clone();
        edit(&mut value);
        let path = dir.path().join(format!("{name}.json"));
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(
            matches!(EcuBackup::load(&path), Err(DiagError::InvalidFile(_))),
            "{name}"
        );
    }
    let path = dir.path().join("good.json");
    std::fs::write(&path, good.to_string()).unwrap();
    EcuBackup::load(&path).unwrap();
}

#[test]
fn timestamps_are_utc_rfc3339() {
    assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
    assert_eq!(utc_timestamp(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(utc_timestamp(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(
        file_stamp("2026-11-01T09:12:45Z").as_deref(),
        Some("20261101-0912")
    );
    assert!(utc_now().ends_with('Z') && file_stamp(&utc_now()).is_some());
}

struct Restore {
    target: FakeEcu,
    folder: tempfile::TempDir,
    events: RecordingEvents,
}
impl Restore {
    fn new(target: FakeEcu) -> Self {
        Self {
            target,
            folder: tempfile::tempdir().unwrap(),
            events: RecordingEvents::default(),
        }
    }
    fn run(
        &self,
        backup: &EcuBackup,
        options: RestoreOptions,
        ui: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<RestoreReport, DiagError> {
        let mut protocol = protocol_on(&self.target);
        protocol.set_events(Box::new(self.events.clone()));
        restore(
            &mut protocol,
            &mut odyssey(),
            "engine",
            backup,
            self.folder.path(),
            &options,
            ui,
            cancel,
        )
    }
    fn saved_files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.folder.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect()
    }
    fn events(&self) -> Vec<String> {
        self.events.0.borrow().clone()
    }
    fn bench(&self, confirm: bool) -> Bench {
        bench(&self.target, confirm)
    }
}
const ALLOW: RestoreOptions = RestoreOptions {
    dry_run: false,
    allow_untested: true,
    skip_battery_check: false,
};

#[test]
fn restore_writes_the_backup_saves_a_safety_copy_and_verifies() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let restore = Restore::new(ecu(SOFTWARE, 0x2000));
    let mut ui = restore.bench(true);
    let report = restore
        .run(&backup, ALLOW, &mut ui, &Cancel::default())
        .unwrap();

    assert_eq!(ui.confirmed, 1);
    assert_eq!(report.written, 512);
    for (channel, word) in [(0, 0), (0, 255), (1, 0), (1, 255)] {
        let expected = 0x1000 + u16::from(channel) * 256 + u16::from(word);
        assert_eq!(restore.target.eeprom_word(channel, word), expected);
    }
    let copy = EcuBackup::load(report.safety_copy.as_ref().unwrap()).unwrap();
    assert_eq!(copy.blocks[0].words[0], 0x2000, "the ECU's previous data");
    assert_eq!(restore.saved_files(), [report.safety_copy.unwrap()]);
    assert!(restore.target.state().protocol_violations.is_empty());

    // A second run finds nothing to write.
    let mut ui = restore.bench(true);
    let again = restore
        .run(&backup, ALLOW, &mut ui, &Cancel::default())
        .unwrap();
    assert_eq!(again.written, 0);
    assert_eq!(ui.confirmed, 0);
    assert_eq!(ba_writes(&restore.target), 512);
}

#[test]
fn only_values_that_differ_are_written() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let target = ecu(SOFTWARE, 0x1000);
    target.fill_eeprom(0x1000);
    let mut changed = backup.clone();
    changed.blocks[1].words[7] = 0xBEEF;
    let restore = Restore::new(target);
    let report = restore
        .run(
            &changed,
            ALLOW,
            &mut restore.bench(true),
            &Cancel::default(),
        )
        .unwrap();
    assert_eq!(report.written, 1);
    assert_eq!(restore.target.state().ba_writes, [(1, 7, 0xBEEF)]);
}

#[test]
fn failed_checks_refuse_before_the_ecu_is_unlocked() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let mut other_ecu_type = backup.clone();
    other_ecu_type.ecu.definition = "honda/37820-RGM".into();
    let cases: Vec<(&str, FakeEcu, EcuBackup, RestoreOptions)> = vec![
        (
            "untested",
            ecu(SOFTWARE, 0),
            backup.clone(),
            RestoreOptions::default(),
        ),
        ("software", ecu(b"37805-RGW-A970", 0), backup.clone(), ALLOW),
        ("definition", ecu(SOFTWARE, 0), other_ecu_type, ALLOW),
        ("battery", ecu(SOFTWARE, 0), backup.clone(), ALLOW),
    ];
    for (name, target, backup, options) in cases {
        if name == "battery" {
            target.measure_battery_as(11_000);
        }
        let restore = Restore::new(target);
        let mut ui = restore.bench(true);
        let error = restore
            .run(&backup, options, &mut ui, &Cancel::default())
            .unwrap_err();
        assert!(
            matches!(error, DiagError::PreflightFailed(_)),
            "{name}: {error}"
        );
        assert_eq!(ui.confirmed, 0, "{name}");
        assert_eq!(unlock_requests(&restore.target), 0, "{name}");
        assert_eq!(ba_writes(&restore.target), 0, "{name}");
        assert!(restore.saved_files().is_empty(), "{name}");
    }
}

#[test]
fn skip_battery_check_turns_a_low_supply_into_a_warning() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let target = ecu(SOFTWARE, 0);
    target.measure_battery_as(11_900);
    let restore = Restore::new(target);
    let report = restore
        .run(
            &backup,
            RestoreOptions {
                skip_battery_check: true,
                ..ALLOW
            },
            &mut restore.bench(true),
            &Cancel::default(),
        )
        .unwrap();
    let battery = report.preflight.checks.iter().find(|c| c.name == "battery");
    assert_eq!(battery.unwrap().status, CheckStatus::Warn);
    assert_eq!(report.written, 512);
}

#[test]
fn declined_and_dry_runs_write_nothing() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));

    let declined = Restore::new(ecu(SOFTWARE, 0x2000));
    let mut ui = declined.bench(false);
    let result = declined.run(&backup, ALLOW, &mut ui, &Cancel::default());
    assert!(matches!(result, Err(DiagError::Cancelled)));
    assert_eq!(ui.confirmed, 1);
    assert_eq!(ba_writes(&declined.target), 0);
    assert_eq!(declined.saved_files().len(), 1, "the safety copy is kept");

    let dry = Restore::new(ecu(SOFTWARE, 0x2000));
    let mut ui = dry.bench(true);
    let report = dry
        .run(
            &backup,
            RestoreOptions {
                dry_run: true,
                ..ALLOW
            },
            &mut ui,
            &Cancel::default(),
        )
        .unwrap();
    assert_eq!((report.written, ui.confirmed), (0, 0));
    assert_eq!(ba_writes(&dry.target), 0);
    assert!(dry.saved_files().is_empty());
    let change = report
        .preflight
        .checks
        .iter()
        .find(|c| c.name == "will change");
    assert!(change.unwrap().value.starts_with("512 of 512 values"));
}

#[test]
fn a_value_that_does_not_read_back_fails_after_writing_the_rest() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let target = ecu(SOFTWARE, 0x2000);
    target.make_read_only(0, 0x10);
    let restore = Restore::new(target);
    let error = restore
        .run(&backup, ALLOW, &mut restore.bench(true), &Cancel::default())
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("1 values did not verify"), "{message}");
    assert!(message.contains("block 0 value 0x10"), "{message}");
    assert_eq!(ba_writes(&restore.target), 512);
}

#[test]
fn a_restore_that_stops_partway_reports_progress_and_the_safety_copy() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let target = ecu(SOFTWARE, 0x2000);
    target.go_silent_after_writes(10);
    let restore = Restore::new(target);
    let error = restore
        .run(&backup, ALLOW, &mut restore.bench(true), &Cancel::default())
        .unwrap_err();
    assert!(matches!(error, DiagError::Timeout(_)), "{error}");
    let copy = restore.saved_files().remove(0);
    let warning = restore
        .events()
        .into_iter()
        .find(|e| e.contains("Restore stopped"))
        .unwrap();
    assert!(warning.contains("after 9 of 512 values"), "{warning}");
    assert!(warning.contains(&copy.display().to_string()), "{warning}");
}

#[test]
fn lapsed_access_after_confirmation_is_regained_before_writing() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let target = ecu(SOFTWARE, 0x2000);
    let handle = target.clone();
    let restore = Restore::new(target);
    let mut ui = restore.bench(true);
    ui.on_confirm = Some(Box::new(move || handle.drop_access()));
    let report = restore
        .run(&backup, ALLOW, &mut ui, &Cancel::default())
        .unwrap();
    assert_eq!(report.written, 512);
    assert_eq!(
        ui.power_cycle_prompts(),
        2,
        "a second power cycle was requested"
    );
    assert!(restore.events().iter().any(|e| e.contains("access lapsed")));
}

#[test]
fn an_ecu_swapped_during_confirmation_is_not_written() {
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let target = ecu(SOFTWARE, 0x2000);
    let handle = target.clone();
    let restore = Restore::new(target);
    let mut ui = restore.bench(true);
    ui.on_confirm = Some(Box::new(move || handle.fill_eeprom(0x3000)));
    let error = restore
        .run(&backup, ALLOW, &mut ui, &Cancel::default())
        .unwrap_err();
    assert!(error.to_string().contains("nothing was written"), "{error}");
    assert_eq!(ba_writes(&restore.target), 0);
}

#[test]
fn cancellation_waits_until_every_value_is_written() {
    struct CancelWhenWriting(Cancel);
    impl Events for CancelWhenWriting {
        fn emit(&mut self, event: Event<'_>) {
            if matches!(event, Event::Step(text) if text.starts_with("Writing")) {
                self.0.cancel();
            }
        }
    }
    let backup = take_backup(&ecu(SOFTWARE, 0x1000));
    let target = ecu(SOFTWARE, 0x2000);
    let cancel = Cancel::default();
    let mut protocol = protocol_on(&target);
    protocol.set_events(Box::new(CancelWhenWriting(cancel.clone())));
    let folder = tempfile::tempdir().unwrap();
    let result = restore(
        &mut protocol,
        &mut odyssey(),
        "engine",
        &backup,
        folder.path(),
        &ALLOW,
        &mut bench(&target, true),
        &cancel,
    );
    assert!(matches!(result, Err(DiagError::Cancelled)));
    assert_eq!(
        ba_writes(&target),
        512,
        "the write phase is never cut short"
    );
    assert_eq!(target.eeprom_word(1, 255), 0x1000 + 511);
}
