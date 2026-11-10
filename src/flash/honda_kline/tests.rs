use super::*;
use crate::flash::{self, RunOptions, Selection};
use crate::link::{BusSpec, ResponseFrame};
use crate::test_support::{
    accord_replies, x31_image, FakeAdapter, MockLink, RecordingEvents, ScriptedInteraction,
};
use std::rc::Rc;

fn prepared() -> PreparedFlash {
    flash::get_registry()
        .prepare(x31_image(), FlashOptions::default(), Selection::default())
        .unwrap()
}

fn response(header: u8, command: Option<u8>, data: &[u8]) -> Vec<u8> {
    let mut reply = vec![header, 0];
    if let Some(command) = command {
        reply.push(command);
    }
    reply.extend_from_slice(data);
    reply[1] = (reply.len() + 1) as u8;
    reply.push(checksum(&reply));
    reply
}

fn identity(replies: &mut Vec<Vec<u8>>) {
    identity_reporting(replies, b"37805-RAD-3350");
}

fn identity_reporting(replies: &mut Vec<Vec<u8>>, calibration: &[u8]) {
    // RAD dispatcher 0x66f5a returns EF without a status byte for AF.
    for payload in [&[0x41, 0, 0, 0, 0, 0][..], &[0xEF][..]] {
        let mut reply = vec![0x48, 0x6B, OBD_ADDRESS];
        reply.extend_from_slice(payload);
        reply.push(reply.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)));
        replies.push(reply);
    }
    replies.push(response(0x0D, Some(1), calibration));
}

fn transfer_setup(replies: &mut Vec<Vec<u8>>, pacing: u16) {
    for (command, data) in [
        (0x0B, &[0][..]),
        (0x0E, &[][..]),
        (1, &pacing.to_be_bytes()[..]),
    ] {
        replies.push(response(0x0E, Some(command), data));
    }
}

fn entry(replies: &mut Vec<Vec<u8>>, pacing: u16) {
    identity(replies);
    replies.push(response(0x0D, Some(2), &[0]));
    replies.push(response(0x0D, Some(3), &[0]));
    replies.push(response(0x0E, Some(1), &[0]));
    replies.push(response(0x0E, Some(2), &[0, 0x1D, 0x5C]));
    replies.push(response(0x0E, Some(3), &[0]));
    transfer_setup(replies, pacing);
}

fn erase(replies: &mut Vec<Vec<u8>>) {
    replies.push(response(0x0E, Some(4), &[0]));
    replies.push(response(0x0E, Some(5), &[0]));
}

fn finish(replies: &mut Vec<Vec<u8>>) {
    finish_reporting(replies, b"37805-RAD-3350");
}

fn finish_reporting(replies: &mut Vec<Vec<u8>>, calibration: &[u8]) {
    for (command, status) in [
        (8, 0),
        (9, 0),
        (0x0A, 0),
        (0x0C, 0),
        (0x0D, 0x0F),
        (1, 0x0F),
        (0x0F, 0),
        (1, 0x0F),
        (0x10, 0),
    ] {
        replies.push(response(0x0E, Some(command), &[status]));
    }
    identity_reporting(replies, calibration);
}

fn fixture(replies: Vec<Vec<u8>>) -> (ObdProtocol, FakeAdapter, MockLink, MockLink) {
    fixture_with_calibration(replies, accord_replies("0904"))
}

fn fixture_with_calibration(
    replies: Vec<Vec<u8>>,
    mode09: Vec<ResponseFrame>,
) -> (ObdProtocol, FakeAdapter, MockLink, MockLink) {
    let adapter = FakeAdapter::new();
    adapter.programming_voltage_readback(5000);
    let bus = BusSpec::standard(BusKind::Iso9141);
    let obd = MockLink::on_bus(bus.clone(), &[]);
    obd.script_functional(mode09);
    let mut raw_bus = bus.clone();
    raw_bus.framing = Framing::Raw;
    let raw = MockLink::on_bus(raw_bus, &[]);
    for data in replies {
        raw.script_reply(ResponseFrame { id: 0, data });
    }
    adapter.offer_link(raw.clone());
    adapter.offer_link(MockLink::on_bus(bus, &[]));
    let protocol = ObdProtocol::new(Rc::new(adapter.clone()), obd.boxed());
    (protocol, adapter, obd, raw)
}

fn run(
    protocol: &mut ObdProtocol,
    interaction: &mut ScriptedInteraction,
) -> Result<Preflight, DiagError> {
    flash::run(
        protocol,
        &prepared(),
        &RunOptions {
            verified: true,
            ..Default::default()
        },
        interaction,
        &Cancel::default(),
    )
}

#[test]
fn raw_firmware_preserves_addresses_and_encrypts_sparse_records() {
    let mut bytes = vec![0xFF; 384];
    bytes[0] = 0x80;
    bytes[383] = 0x42;
    let records = raw_records(&bytes, 0x8000, [0x12, 0x34, 0x56]).unwrap();
    assert_eq!(
        records.iter().map(|r| r.address).collect::<Vec<_>>(),
        [0x8000, 0x8100]
    );
    for record in records {
        for (index, encrypted) in record.encrypted.iter().enumerate() {
            let plain = (encrypted ^ 0x56).wrapping_sub(0x12).wrapping_add(0x34);
            assert_eq!(plain, bytes[(record.address - 0x8000) as usize + index]);
        }
    }
    for (start, length) in [
        (0, 128),
        (0x7F80, 128),
        (0x8001, 128),
        (0xFFF80, 256),
        (0x8000, 127),
    ] {
        assert!(raw_records(&vec![0; length], start, [0; 3]).is_err());
    }
}

#[test]
fn full_sequence_uses_same_channel_until_aftercheck_and_releases_voltage() {
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    erase(&mut replies);
    replies.push(response(0x0E, Some(6), &[2]));
    replies.push(response(0x0E, Some(6), &[0]));
    finish(&mut replies);
    let (mut protocol, adapter, obd, raw) = fixture(replies);
    let events = RecordingEvents::default();
    protocol.set_events(Box::new(events.clone()));
    run(&mut protocol, &mut ScriptedInteraction::new(true)).unwrap();
    assert_eq!(adapter.battery_reads(), 1);
    assert_eq!(adapter.voltage_calls(), [(12, 5000), (12, VOLTAGE_PIN_OFF)]);
    assert!(obd.is_dropped());
    assert!(raw.is_dropped());
    assert_eq!(raw.unused_replies(), 0);
    assert_eq!(adapter.connects().len(), 2);
    assert_eq!(protocol.bus().framing, Framing::Diagnostic);
    let sent = raw.sent();
    let timings = raw.sent_timings();
    assert_eq!(timings[0]["P4_MIN"], 10);
    for (frame, timing) in sent.iter().zip(timings) {
        if frame.starts_with("7E") {
            assert_eq!(timing["P3_MIN"], 60);
        }
        if frame.starts_with("7E8B3606") {
            assert_eq!(timing["P4_MIN"], 0);
        }
        if frame.starts_with("7E05360F") {
            assert_eq!(timing["P4_MIN"], 2);
        }
    }
    assert!(sent
        .iter()
        .all(|frame| !frame.starts_with("F0") && !frame.starts_with("F5")));
    assert_eq!(
        sent.iter()
            .filter(|frame| frame.starts_with("7E8B3606"))
            .count(),
        2
    );
    assert!(events
        .0
        .borrow()
        .iter()
        .any(|event| event == "progress: 256/256"));
}

#[test]
fn confirmation_dry_run_trust_and_cancellation_never_enter_programming() {
    for case in 0..4 {
        let (mut protocol, adapter, _, raw) = fixture(Vec::new());
        let mut interaction = ScriptedInteraction::new(case != 0);
        let options = RunOptions {
            verified: case != 2,
            dry_run: case == 1,
            ..Default::default()
        };
        let cancel = Cancel::default();
        if case == 3 {
            cancel.cancel();
        }
        let result = flash::run(
            &mut protocol,
            &prepared(),
            &options,
            &mut interaction,
            &cancel,
        );
        assert_eq!(result.is_ok(), case == 1);
        assert!(adapter.voltage_calls().is_empty());
        assert!(adapter.connects().is_empty());
        assert!(raw.sent().is_empty());
    }
}

#[test]
fn malformed_reply_and_entry_rejection_never_send_erase() {
    for corrupt in [false, true] {
        let mut replies = Vec::new();
        identity(&mut replies);
        let mut rejection = response(0x0D, Some(2), &[0xFE]);
        if corrupt {
            *rejection.last_mut().unwrap() ^= 1;
        }
        replies.push(rejection);
        let (mut protocol, adapter, _, raw) = fixture(replies);
        assert!(run(&mut protocol, &mut ScriptedInteraction::new(true)).is_err());
        assert_eq!(adapter.voltage_calls(), [(12, 5000), (12, VOLTAGE_PIN_OFF)]);
        assert!(raw
            .sent()
            .iter()
            .all(|frame| !frame.starts_with("7E063604")));
    }
}

#[test]
fn recovery_status_uses_loader_timing_after_an_ambiguous_entry() {
    let mut replies = Vec::new();
    identity(&mut replies);
    replies.push(response(0x0D, Some(2), &[0]));
    let mut damaged_ack = response(0x0D, Some(3), &[0]);
    *damaged_ack.last_mut().unwrap() ^= 1;
    replies.push(damaged_ack);
    replies.push(response(0x0E, Some(1), &[0x10]));
    let (mut protocol, adapter, _, raw) = fixture(replies);
    let mut interaction = ScriptedInteraction::new(true);
    interaction.recovery.push_back(FlashRecovery::Status);
    assert!(run(&mut protocol, &mut interaction).is_err());
    assert_eq!(raw.unused_replies(), 0);
    assert!(raw.sent().last().unwrap().starts_with("7E06360100"));
    let timings = raw.sent_timings();
    assert_eq!(timings[timings.len() - 2]["P4_MIN"], 0);
    assert_eq!(timings.last().unwrap()["P3_MIN"], 60);
    assert_eq!(timings.last().unwrap()["P4_MIN"], 2);
    assert_eq!(adapter.voltage_calls(), [(12, 5000), (12, VOLTAGE_PIN_OFF)]);
    assert!(raw
        .sent()
        .iter()
        .all(|frame| !frame.starts_with("7E063604")));
}

#[test]
fn failed_transfer_recovers_retained_image_after_deferred_cancellation() {
    struct CancelOnWrite(Cancel);
    impl crate::events::Events for CancelOnWrite {
        fn emit(&mut self, event: Event<'_>) {
            if matches!(event, Event::Step("Writing firmware")) {
                self.0.cancel();
            }
        }
    }
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    erase(&mut replies);
    replies.push(response(0x0E, Some(6), &[0xFF]));
    replies.push(response(0x0E, Some(1), &[0xFA]));
    transfer_setup(&mut replies, 0);
    erase(&mut replies);
    replies.push(response(0x0E, Some(6), &[2]));
    replies.push(response(0x0E, Some(6), &[0]));
    finish(&mut replies);
    let (mut protocol, adapter, _, raw) = fixture(replies);
    let cancel = Cancel::default();
    protocol.set_events(Box::new(CancelOnWrite(cancel.clone())));
    let mut interaction = ScriptedInteraction::new(true);
    interaction.recovery.push_back(FlashRecovery::Retry);
    flash::run(
        &mut protocol,
        &prepared(),
        &RunOptions {
            verified: true,
            ..Default::default()
        },
        &mut interaction,
        &cancel,
    )
    .unwrap();
    assert!(cancel.is_cancelled());
    assert!(interaction.recovery.is_empty());
    assert_eq!(adapter.battery_reads(), 1);
    let write_timings: Vec<_> = raw
        .sent()
        .iter()
        .zip(raw.sent_timings())
        .filter(|(frame, _)| frame.starts_with("7E8B3606"))
        .map(|(_, timing)| timing["P4_MIN"])
        .collect();
    assert_eq!(write_timings, [0, 2, 2]);
    assert_eq!(
        adapter.voltage_calls(),
        [(12, 5000), (12, 5000), (12, VOLTAGE_PIN_OFF)]
    );
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E053602"))
            .count(),
        1
    );
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E063604"))
            .count(),
        2
    );
}

#[test]
fn transfer_timeout_does_not_resend_and_stopping_releases_voltage() {
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    erase(&mut replies);
    let (mut protocol, adapter, _, raw) = fixture(replies);
    assert!(matches!(
        run(&mut protocol, &mut ScriptedInteraction::new(true)),
        Err(DiagError::Timeout(_))
    ));
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E8B3606"))
            .count(),
        1
    );
    assert_eq!(adapter.voltage_calls(), [(12, 5000), (12, VOLTAGE_PIN_OFF)]);
    assert_eq!(adapter.connects().len(), 1);
}

#[test]
fn raw_input_requires_start_and_snapshots_external_tokens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tokens.rwd");
    std::fs::write(&path, x31_image()).unwrap();
    let options = FlashOptions {
        tokens: Some(path.clone()),
        start_address: Some(0x8000),
        ..Default::default()
    };
    let selection = || Selection {
        vehicle: Some("honda/accord-2005"),
        ecu: Some(EcuAddress::Byte(9)),
        strategy: Some("honda_kline"),
        ..Default::default()
    };
    let request = flash::get_registry()
        .prepare(vec![0xA5; 128], options.clone(), selection())
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(request.reselect(selection(), None).is_ok());
    assert!(flash::get_registry()
        .prepare(
            vec![0; 128],
            FlashOptions {
                security_key: Some(vec![0; 6]),
                ..Default::default()
            },
            selection()
        )
        .is_err());
}

#[test]
fn busy_polls_and_adjacent_resynchronization_follow_the_loader_statuses() {
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    replies.push(response(0x0E, Some(4), &[0x15]));
    replies.push(response(0x0E, Some(4), &[0]));
    replies.push(response(0x0E, Some(5), &[1]));
    replies.push(response(0x0E, Some(5), &[0]));
    replies.push(response(0x0E, Some(6), &[3, 0xFF, 0xF8]));
    replies.push(response(0x0E, Some(6), &[0]));
    finish(&mut replies);
    let (mut protocol, _, _, raw) = fixture(replies);
    run(&mut protocol, &mut ScriptedInteraction::new(true)).unwrap();
    let sent = raw.sent();
    assert_eq!(
        sent.iter()
            .filter(|frame| frame.starts_with("7E063604"))
            .count(),
        2
    );
    assert_eq!(
        sent.iter()
            .filter(|frame| frame.starts_with("7E053605"))
            .count(),
        2
    );
    assert_eq!(
        sent.iter()
            .filter(|frame| frame.starts_with("7E8B3606"))
            .count(),
        2
    );
    assert_eq!(raw.unused_replies(), 0);
}

#[test]
fn paced_transfer_polls_each_record_and_omits_the_final_unpaced_poll() {
    let mut replies = Vec::new();
    entry(&mut replies, 1);
    erase(&mut replies);
    for _ in 0..2 {
        replies.push(response(0x0E, Some(6), &[0]));
        replies.push(response(0x0E, Some(7), &[0]));
        replies.push(response(0x0E, Some(8), &[0]));
    }
    let mut completion = Vec::new();
    finish(&mut completion);
    replies.extend(completion.into_iter().skip(1));
    let (mut protocol, _, _, raw) = fixture(replies);
    run(&mut protocol, &mut ScriptedInteraction::new(true)).unwrap();
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E053607"))
            .count(),
        2
    );
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E053608"))
            .count(),
        2
    );
    assert_eq!(raw.unused_replies(), 0);
}

#[test]
fn nonadjacent_resync_is_rejected_and_voltage_is_released() {
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    erase(&mut replies);
    replies.push(response(0x0E, Some(6), &[3, 0x55, 0x55]));
    let (mut protocol, adapter, _, raw) = fixture(replies);
    let error = run(&mut protocol, &mut ScriptedInteraction::new(true)).unwrap_err();
    assert!(error.to_string().contains("resync tag"));
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E8B3606"))
            .count(),
        1
    );
    assert_eq!(adapter.voltage_calls().last(), Some(&(12, VOLTAGE_PIN_OFF)));
}

#[test]
fn fragmented_replies_and_unrelated_commands_are_received_without_resending() {
    let mut spec = BusSpec::standard(BusKind::Iso9141);
    spec.framing = Framing::Raw;
    let link = MockLink::on_bus(spec, &[]);
    link.script_reply(ResponseFrame {
        id: 0,
        data: response(0x0E, Some(9), &[0]),
    });
    let wanted = response(0x0E, Some(6), &[2]);
    for part in wanted.chunks(2) {
        link.script_reply(ResponseFrame {
            id: 0,
            data: part.to_vec(),
        });
    }
    let adapter = FakeAdapter::new();
    let mut protocol = ObdProtocol::new(Rc::new(adapter), link.boxed());
    let mut strategy = HondaKLineStrategy::new(&mut protocol);
    assert_eq!(strategy.command(6, &[1], REPLY_TIMEOUT).unwrap(), [2]);
    assert_eq!(link.sent().len(), 1);
}

/// The Python prototype produces these local artifacts; no firmware is embedded in tests.
#[test]
#[ignore = "requires OPENDIAG_X31_REFERENCE with image.rwd, image.bin and packets.bin"]
fn local_x31_and_raw_image_match_every_prototype_transfer_packet() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("OPENDIAG_X31_REFERENCE").expect("set OPENDIAG_X31_REFERENCE"),
    );
    let rwd = parse_x31(&std::fs::read(directory.join("image.rwd")).unwrap()).unwrap();
    let raw = std::fs::read(directory.join("image.bin")).unwrap();
    let records = raw_records(&raw, 0x8000, rwd.decryption_key).unwrap();
    assert_eq!(records, rwd.records);
    let mut bus = BusSpec::standard(BusKind::Iso9141);
    bus.framing = Framing::Raw;
    let link = MockLink::on_bus(bus, &[]);
    for _ in &records {
        link.script_reply(ResponseFrame {
            id: 0,
            data: response(0x0E, Some(6), &[0]),
        });
    }
    let mut protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
    let mut strategy = HondaKLineStrategy::new(&mut protocol);
    for index in 0..records.len() {
        strategy
            .command(6, &record_payload(&records, index), REPLY_TIMEOUT)
            .unwrap();
    }
    let actual: Vec<u8> = link
        .sent()
        .iter()
        .flat_map(|hex| crate::utils::hex_to_bytes(hex).unwrap())
        .collect();
    assert_eq!(
        actual,
        std::fs::read(directory.join("packets.bin")).unwrap()
    );
}

#[test]
fn cancellation_during_writing_waits_for_completion_and_application_return() {
    struct CancelDuringWrite(Cancel);
    impl crate::events::Events for CancelDuringWrite {
        fn emit(&mut self, event: Event<'_>) {
            if matches!(event, Event::Progress { done: 128, .. }) {
                self.0.cancel();
            }
        }
    }
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    erase(&mut replies);
    replies.push(response(0x0E, Some(6), &[2]));
    replies.push(response(0x0E, Some(6), &[0]));
    finish(&mut replies);
    let (mut protocol, adapter, _, raw) = fixture(replies);
    let cancel = Cancel::default();
    protocol.set_events(Box::new(CancelDuringWrite(cancel.clone())));
    flash::run(
        &mut protocol,
        &prepared(),
        &RunOptions {
            verified: true,
            ..Default::default()
        },
        &mut ScriptedInteraction::new(true),
        &cancel,
    )
    .unwrap();
    assert!(cancel.is_cancelled());
    assert_eq!(raw.unused_replies(), 0);
    assert_eq!(adapter.voltage_calls().last(), Some(&(12, VOLTAGE_PIN_OFF)));
    assert_eq!(protocol.bus().framing, Framing::Diagnostic);
}

#[test]
fn failed_aftercheck_does_not_offer_a_new_erase() {
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    erase(&mut replies);
    replies.push(response(0x0E, Some(6), &[2]));
    replies.push(response(0x0E, Some(6), &[0]));
    for (command, status) in [
        (8, 0),
        (9, 0),
        (0x0A, 0),
        (0x0C, 0),
        (0x0D, 0x0F),
        (1, 0xFF),
    ] {
        replies.push(response(0x0E, Some(command), &[status]));
    }
    let (mut protocol, adapter, _, raw) = fixture(replies);
    let mut interaction = ScriptedInteraction::new(true);
    interaction.recovery.push_back(FlashRecovery::Retry);
    assert!(run(&mut protocol, &mut interaction).is_err());
    assert_eq!(interaction.recovery.len(), 1);
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E063604"))
            .count(),
        1
    );
    assert_eq!(adapter.voltage_calls().last(), Some(&(12, VOLTAGE_PIN_OFF)));
}

#[test]
fn explicit_key_does_not_bypass_x31_calibration_compatibility() {
    let mut bytes = x31_image();
    let offset = bytes
        .windows(14)
        .position(|part| part == b"37805-RAD-3350")
        .unwrap();
    bytes[offset..offset + 14].copy_from_slice(b"37805-RAD-9999");
    let end = bytes.len() - 4;
    let checksum: u32 = bytes[..end].iter().map(|&b| u32::from(b)).sum();
    bytes[end..].copy_from_slice(&checksum.to_le_bytes());
    let request = flash::get_registry()
        .prepare(
            bytes,
            FlashOptions {
                security_key: Some(vec![0; 6]),
                ..Default::default()
            },
            Selection::default(),
        )
        .unwrap();
    let (mut protocol, adapter, _, raw) = fixture(Vec::new());
    let error = flash::run(
        &mut protocol,
        &request,
        &RunOptions {
            verified: true,
            ..Default::default()
        },
        &mut ScriptedInteraction::new(true),
        &Cancel::default(),
    )
    .unwrap_err();
    assert!(matches!(error, DiagError::PreflightFailed(_)));
    assert!(error.to_string().contains("compatibility"));
    assert!(adapter.voltage_calls().is_empty());
    assert!(raw.sent().is_empty());
}

fn programmed_reporting(calibration: &[u8]) -> Vec<Vec<u8>> {
    let mut replies = Vec::new();
    entry(&mut replies, 0);
    erase(&mut replies);
    replies.push(response(0x0E, Some(6), &[2]));
    replies.push(response(0x0E, Some(6), &[0]));
    finish_reporting(&mut replies, calibration);
    replies
}

#[test]
fn update_to_a_calibration_outside_the_install_list_succeeds() {
    let (mut protocol, adapter, _, raw) = fixture(programmed_reporting(b"37805-RAD-3360"));
    run(&mut protocol, &mut ScriptedInteraction::new(true)).unwrap();
    assert_eq!(raw.unused_replies(), 0);
    assert_eq!(adapter.voltage_calls().last(), Some(&(12, VOLTAGE_PIN_OFF)));
    assert_eq!(protocol.bus().framing, Framing::Diagnostic);
}

#[test]
fn application_that_is_not_rad_after_programming_is_rejected_without_a_new_erase() {
    let (mut protocol, adapter, _, raw) = fixture(programmed_reporting(b"37805-XYZ-0000"));
    let mut interaction = ScriptedInteraction::new(true);
    interaction.recovery.push_back(FlashRecovery::Retry);
    assert!(matches!(
        run(&mut protocol, &mut interaction),
        Err(DiagError::InvalidResponse(_))
    ));
    assert_eq!(interaction.recovery.len(), 1);
    assert_eq!(
        raw.sent()
            .iter()
            .filter(|frame| frame.starts_with("7E063604"))
            .count(),
        1
    );
    assert_eq!(adapter.voltage_calls().last(), Some(&(12, VOLTAGE_PIN_OFF)));
}

#[test]
fn space_padded_calibration_matches_across_identification_reads() {
    let mut replies = programmed_reporting(b"37805-RAD-3350  ");
    replies[2] = response(0x0D, Some(1), b"37805-RAD-3350  ");
    let space_padded = accord_replies("0904")
        .into_iter()
        .map(|mut frame| {
            let padding = frame.data.iter().rev().take_while(|&&b| b == 0).count();
            let end = frame.data.len();
            frame.data[end - padding..].fill(b' ');
            frame
        })
        .collect();
    let (mut protocol, _, _, raw) = fixture_with_calibration(replies, space_padded);
    run(&mut protocol, &mut ScriptedInteraction::new(true)).unwrap();
    assert_eq!(raw.unused_replies(), 0);
}

fn switch_link(frames: &[&[u8]]) -> (ObdProtocol, MockLink) {
    let mut spec = BusSpec::standard(BusKind::Iso9141);
    spec.framing = Framing::Raw;
    let link = MockLink::on_bus(spec, &[]);
    for data in frames {
        link.script_reply(ResponseFrame {
            id: 0,
            data: data.to_vec(),
        });
    }
    let protocol = ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed());
    (protocol, link)
}

#[test]
fn fragmented_protocol_switch_replies_are_assembled_without_resending() {
    let mut replies = Vec::new();
    identity(&mut replies);
    for (prefix, reply, length) in [
        (&[0x68, 0x6A, 0xF1, 1, 0][..], &replies[0], 10),
        (
            &[0x68, 0x6A, 0xF5, 0xAF, 0xBF, 0xB3, 0xB2, 0xC1, 0xDB, 0xB3][..],
            &replies[1],
            5,
        ),
    ] {
        for size in 1..reply.len() {
            let (first, second) = reply.split_at(size);
            let (mut protocol, link) = switch_link(&[first, second]);
            HondaKLineStrategy::new(&mut protocol)
                .iso_exchange(prefix, length)
                .unwrap();
            assert_eq!(link.sent().len(), 1);
            assert_eq!(link.unused_replies(), 0);
        }
    }
}

#[test]
fn a_partial_reply_with_a_matching_checksum_cannot_finish_the_exchange() {
    let prefix = [0x68, 0x6A, 0xF1, 1, 0];
    for index in 5..9 {
        let mut reply = vec![0x48, 0x6B, OBD_ADDRESS, 0x41, 0, 0, 0, 0, 0];
        reply[index] = 0xFD;
        reply.push(checksum(&reply).wrapping_neg());
        let (first, second) = reply.split_at(index + 1);
        let (mut protocol, link) = switch_link(&[first, second]);
        HondaKLineStrategy::new(&mut protocol)
            .iso_exchange(&prefix, 10)
            .unwrap();
        assert_eq!(link.sent().len(), 1);
        assert_eq!(link.unused_replies(), 0);

        let (mut protocol, link) = switch_link(&[first]);
        assert!(matches!(
            HondaKLineStrategy::new(&mut protocol).iso_exchange(&prefix, 10),
            Err(DiagError::Timeout(_))
        ));
        assert_eq!(link.sent().len(), 1);
    }
}

#[test]
fn protocol_switch_rejects_wrong_replies_and_reports_incomplete_ones() {
    let prefix = [0x68, 0x6A, 0xF1, 1, 0];
    let mut replies = Vec::new();
    identity(&mut replies);
    let valid = &replies[0];

    let mut wrong_ecu = valid.clone();
    wrong_ecu[2] = 0x10;
    let mut wrong_pid = valid.clone();
    wrong_pid[4] = 1;
    let mut corrupt = valid.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    let mut oversized = valid.clone();
    oversized.push(0);
    for wrong in [&wrong_ecu, &wrong_pid, &corrupt, &oversized] {
        let (mut protocol, _) = switch_link(&[wrong]);
        assert!(matches!(
            HondaKLineStrategy::new(&mut protocol).iso_exchange(&prefix, 10),
            Err(DiagError::InvalidResponse(_))
        ));
    }

    let truncated = &valid[..3];
    for incomplete in [truncated, &[]] {
        let frames: &[&[u8]] = if incomplete.is_empty() {
            &[]
        } else {
            &[incomplete]
        };
        let (mut protocol, _) = switch_link(frames);
        assert!(matches!(
            HondaKLineStrategy::new(&mut protocol).iso_exchange(&prefix, 10),
            Err(DiagError::Timeout(_))
        ));
    }
}

#[test]
fn cancellation_after_loader_entry_stops_before_erase_without_offering_recovery() {
    struct CancelAt {
        cancel: Cancel,
        trigger: fn(&Event<'_>) -> bool,
        warnings: Rc<std::cell::RefCell<Vec<String>>>,
    }
    impl crate::events::Events for CancelAt {
        fn emit(&mut self, event: Event<'_>) {
            if (self.trigger)(&event) {
                self.cancel.cancel();
            }
            if let Event::Warning(text) = event {
                self.warnings.borrow_mut().push(text.into());
            }
        }
    }
    const SEED_REQUEST: &str = "7E053602";
    let during_loader_wait: fn(&Event<'_>) -> bool =
        |event| matches!(event, Event::Step("Waiting for the resident loader"));
    let during_authentication: fn(&Event<'_>) -> bool = |event| matches!(event, Event::Detail(text) if text.strip_prefix("TX ").is_some_and(|tx| tx.starts_with(SEED_REQUEST)));
    for (trigger, seed_requests) in [(during_loader_wait, 0), (during_authentication, 1)] {
        let mut replies = Vec::new();
        entry(&mut replies, 0);
        erase(&mut replies);
        let (mut protocol, adapter, _, raw) = fixture(replies);
        let cancel = Cancel::default();
        let warnings = Rc::default();
        protocol.set_events(Box::new(CancelAt {
            cancel: cancel.clone(),
            trigger,
            warnings: Rc::clone(&warnings),
        }));
        let mut interaction = ScriptedInteraction::new(true);
        interaction.recovery.push_back(FlashRecovery::Retry);
        let result = flash::run(
            &mut protocol,
            &prepared(),
            &RunOptions {
                verified: true,
                ..Default::default()
            },
            &mut interaction,
            &cancel,
        );
        assert!(matches!(result, Err(DiagError::Cancelled)));
        assert_eq!(interaction.recovery.len(), 1);
        let sent = raw.sent();
        let count = |prefix: &str| sent.iter().filter(|f| f.starts_with(prefix)).count();
        assert_eq!(count(SEED_REQUEST), seed_requests);
        assert_eq!(count("7E063604"), 0);
        assert_eq!(count("7E8B3606"), 0);
        assert_eq!(adapter.voltage_calls(), [(12, 5000), (12, VOLTAGE_PIN_OFF)]);
        assert_eq!(protocol.bus().framing, Framing::Raw);
        assert!(warnings
            .borrow()
            .iter()
            .any(|warning: &String| warning.contains("not erased")));
    }
}
