use super::*;
use crate::test_support::accord_messages;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

struct State {
    now: Instant,
    calls: Vec<String>,
    failure: Option<&'static str>,
    keys: [u8; 2],
    fast_reply: Option<RxMsg>,
    fast_requests: Vec<Vec<u8>>,
    fast_statuses: VecDeque<i32>,
    full: usize,
    exchanges: VecDeque<Vec<RxMsg>>,
    rx: VecDeque<RxMsg>,
    sent: Vec<Vec<u8>>,
}

#[derive(Clone)]
struct Fake(Rc<RefCell<State>>);
impl Fake {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(State {
            now: Instant::now(),
            calls: Vec::new(),
            failure: None,
            keys: [8, 8],
            fast_reply: None,
            fast_requests: Vec::new(),
            fast_statuses: VecDeque::new(),
            full: 0,
            exchanges: VecDeque::new(),
            rx: VecDeque::new(),
            sent: Vec::new(),
        })))
    }
    fn call(&self, name: &str) -> Result<(), DiagError> {
        let mut state = self.0.borrow_mut();
        state.calls.push(name.into());
        if state.failure == Some(name) {
            return Err(DiagError::Adapter(format!("injected {name} failure")));
        }
        Ok(())
    }
    fn script(&self, request: &str) {
        self.0
            .borrow_mut()
            .exchanges
            .push_back(accord_messages(request).into_iter().map(rx).collect());
    }
    fn inits(&self) -> usize {
        self.0
            .borrow()
            .calls
            .iter()
            .filter(|call| *call == "init:33")
            .count()
    }
}

impl KLineIo for Fake {
    fn connect(&self, _: &BusSpec, protocol: u32, flags: u32) -> Result<u32, DiagError> {
        self.call(&format!("connect:{protocol:X}:{flags:X}"))?;
        Ok(1)
    }
    fn configure(&self, _: u32, _: &[(u32, u32)]) -> Result<(), DiagError> {
        self.call("config")
    }
    fn filter(&self, _: u32, protocol: u32) -> Result<u32, DiagError> {
        self.call(&format!("filter:{protocol:X}"))?;
        Ok(2)
    }
    fn stop_filter(&self, _: u32, _: u32) -> Result<(), DiagError> {
        self.call("stop")
    }
    fn initialize(&self, _: u32, address: u8) -> Result<[u8; 2], DiagError> {
        self.call(&format!("init:{address:02X}"))?;
        self.wait(Duration::from_millis(2500));
        Ok(self.0.borrow().keys)
    }
    fn fast_initialize(&self, _: u32, request: &[u8]) -> Result<RxMsg, DiagError> {
        self.call("fast_init")?;
        self.wait(Duration::from_millis(100));
        let mut state = self.0.borrow_mut();
        state.fast_requests.push(request.to_vec());
        if let Some(code) = state.fast_statuses.pop_front() {
            if code != STATUS_NOERROR {
                return Err(DiagError::PassThru {
                    call: "PassThruIoctl(FAST_INIT)",
                    code,
                    detail: String::new(),
                });
            }
        }
        Ok(state
            .fast_reply
            .clone()
            .unwrap_or_else(|| kwp_rx(vec![0x83, 0xF1, 0x11, 0xC1, state.keys[0], state.keys[1]])))
    }
    fn clear(&self, _: u32, queue: u32) -> Result<(), DiagError> {
        self.call("clear")?;
        if queue == CLEAR_RX_QUEUE {
            self.0.borrow_mut().rx.clear();
        }
        Ok(())
    }
    fn queue(&self, _: u32, protocol: u32, data: &[u8]) -> Result<(), DiagError> {
        self.call(&format!("queue:{protocol:X}"))?;
        let mut state = self.0.borrow_mut();
        if state.full > 0 {
            state.full -= 1;
            return Err(DiagError::PassThru {
                call: "queue",
                code: ERR_BUFFER_FULL,
                detail: String::new(),
            });
        }
        state.sent.push(data.to_vec());
        if let Some(replies) = state.exchanges.pop_front() {
            state.rx.extend(replies);
        }
        Ok(())
    }
    fn read(&self, _: u32, timeout: Duration) -> Result<Option<RxMsg>, DiagError> {
        self.call("read")?;
        let reply = self.0.borrow_mut().rx.pop_front();
        self.wait(if reply.is_some() {
            Duration::from_millis(20).min(timeout)
        } else {
            timeout
        });
        Ok(reply)
    }
    fn disconnect(&self, _: u32) -> Result<(), DiagError> {
        self.call("disconnect")
    }
    fn warn(&self, _: &str) {}
    fn now(&self) -> Instant {
        self.0.borrow().now
    }
    fn wait(&self, duration: Duration) {
        self.0.borrow_mut().now += duration;
    }
}

fn bus() -> BusSpec {
    BusSpec::standard(BusKind::Iso9141)
}
fn kwp_bus() -> BusSpec {
    BusSpec::from_cli("kwp-5baud", None).unwrap()
}
fn kwp_rx(data: Vec<u8>) -> RxMsg {
    RxMsg {
        protocol_id: PROTOCOL_ISO14230,
        ..rx(data)
    }
}
fn rx(data: Vec<u8>) -> RxMsg {
    RxMsg {
        protocol_id: PROTOCOL_ISO9141,
        rx_status: 0,
        extra_data_index: data.len(),
        data,
    }
}
fn exchange(
    session: &mut KLineSession<Fake>,
    request: &[u8],
) -> Result<Vec<ResponseFrame>, DiagError> {
    session.request_functional(request, Duration::from_secs(3))
}

#[test]
fn kwp_capture_uses_length_headers_and_preserves_numbered_replies() {
    use crate::test_support::kwp_emulator_messages;
    let io = Fake::new();
    io.0.borrow_mut().keys = [0xE9, 0x8F];
    let mut session = KLineSession::open(io.clone(), kwp_bus()).unwrap();
    for request in ["0100", "010C", "0902"] {
        let messages = kwp_emulator_messages(request);
        io.0.borrow_mut()
            .exchanges
            .push_back(messages.iter().cloned().map(kwp_rx).collect());
        let replies =
            exchange(&mut session, &crate::utils::hex_to_bytes(request).unwrap()).unwrap();
        assert_eq!(replies.len(), messages.len());
        for (reply, raw) in replies.iter().zip(messages) {
            assert_eq!(reply.data, raw[3..]);
            assert_eq!(Addressing::Iso14230.responder_address(reply.id), Some(0x11));
        }
    }
    exchange(&mut session, &[3]).unwrap();
    assert_eq!(
        io.0.borrow().sent,
        [
            vec![0xC2, 0x33, 0xF1, 1, 0],
            vec![0xC2, 0x33, 0xF1, 1, 0x0C],
            vec![0xC2, 0x33, 0xF1, 9, 2],
            vec![0xC1, 0x33, 0xF1, 3],
        ]
    );
    assert_eq!(io.0.borrow().calls[0], "connect:4:1000");
    assert!(io.0.borrow().calls.contains(&"filter:4".into()));
    assert!(io.0.borrow().calls.contains(&"queue:4".into()));
    assert_eq!(io.inits(), 1);
    drop(session);
    assert!(io
        .0
        .borrow()
        .calls
        .ends_with(&["stop".into(), "disconnect".into()]));
}

#[test]
fn kwp_keys_are_limited_to_obd_keywords_and_failed_opens_release_the_channel() {
    for first in [0xE9, 0x6B, 0x6D, 0xEF] {
        let io = Fake::new();
        io.0.borrow_mut().keys = [first, 0x8F];
        assert!(KLineSession::open(io, kwp_bus()).is_ok());
    }
    for keys in [[8, 8], [0x94, 0x94], [0xD9, 0x8F], [0x69, 0x8F], [0xE9, 0]] {
        let io = Fake::new();
        io.0.borrow_mut().keys = keys;
        let before = io.now();
        assert!(KLineSession::open(io.clone(), kwp_bus()).is_err());
        assert!(io.0.borrow().sent.is_empty());
        assert!(io
            .0
            .borrow()
            .calls
            .ends_with(&["stop".into(), "disconnect".into()]));
        assert!(io.now().duration_since(before) >= SESSION_IDLE);
    }
}

#[test]
fn fast_init_capture_opens_kwp_and_recovers_after_idle_and_reset() {
    use crate::test_support::kwp_fast_emulator_messages;
    let io = Fake::new();
    io.0.borrow_mut().fast_reply = Some(kwp_rx(kwp_fast_emulator_messages("81")[0].clone()));
    let mut session =
        KLineSession::open(io.clone(), BusSpec::from_cli("kwp-fast", None).unwrap()).unwrap();
    for request in ["0100", "010C", "0100"] {
        let messages = kwp_fast_emulator_messages(request);
        io.0.borrow_mut()
            .exchanges
            .push_back(messages.iter().cloned().map(kwp_rx).collect());
        let replies =
            exchange(&mut session, &crate::utils::hex_to_bytes(request).unwrap()).unwrap();
        assert_eq!(replies[0].data, messages[0][3..]);
        if request == "010C" {
            session.reset().unwrap();
        } else {
            io.wait(Duration::from_secs(6));
        }
    }
    assert_eq!(io.inits(), 0);
    assert_eq!(
        io.0.borrow().fast_requests,
        vec![vec![0xC1, 0x33, 0xF1, 0x81]; 3]
    );
    assert_eq!(io.0.borrow().sent.len(), 3);
    drop(session);
    assert!(io
        .0
        .borrow()
        .calls
        .ends_with(&["stop".into(), "disconnect".into()]));
}

#[test]
fn fast_init_requires_a_valid_start_communication_reply_before_sending_obd() {
    let valid = kwp_rx(vec![0x83, 0xF1, 0x11, 0xC1, 0xEF, 0x8F]);
    let mut invalid = vec![
        kwp_rx(vec![0x83, 0xF1, 0x11, 0x7F, 0x81, 0x11]),
        kwp_rx(vec![0x82, 0xF1, 0x11, 0xC1, 0xEF]),
        kwp_rx(vec![0x83, 0xF1, 0x11, 0xC1, 0xEF]),
        kwp_rx(vec![0x83, 0xF1, 0x11, 0xC1, 8, 8]),
        kwp_rx(vec![0x83, 0x10, 0x11, 0xC1, 0xEF, 0x8F]),
    ];
    let mut wrong_protocol = valid.clone();
    wrong_protocol.protocol_id = PROTOCOL_ISO9141;
    invalid.push(wrong_protocol);
    let mut bad_index = valid.clone();
    bad_index.extra_data_index += 1;
    invalid.push(bad_index);
    for status in [
        TX_MSG_TYPE,
        START_OF_MESSAGE,
        RX_ERROR,
        RX_BREAK,
        BUFFER_OVERFLOW,
        TX_FAILED,
    ] {
        let mut reply = valid.clone();
        reply.rx_status = status;
        invalid.push(reply);
    }
    for reply in invalid {
        let io = Fake::new();
        io.0.borrow_mut().fast_reply = Some(reply);
        assert!(
            KLineSession::open(io.clone(), BusSpec::from_cli("kwp-fast", None).unwrap()).is_err()
        );
        assert!(io.0.borrow().sent.is_empty());
        assert_eq!(io.inits(), 0);
        assert!(io
            .0
            .borrow()
            .calls
            .ends_with(&["stop".into(), "disconnect".into()]));
    }
}

#[test]
fn fast_init_retries_only_init_failure_once_after_a_quiet_interval() {
    for (statuses, success, attempts) in [
        (vec![ERR_INIT_FAILED], true, 2),
        (vec![ERR_INIT_FAILED, ERR_INIT_FAILED], false, 2),
        (vec![ERR_DEVICE_NOT_CONNECTED], false, 1),
    ] {
        let io = Fake::new();
        io.0.borrow_mut().keys = [0xEF, 0x8F];
        io.0.borrow_mut().fast_statuses = statuses.into();
        let before = io.now();
        let result = KLineSession::open(io.clone(), BusSpec::from_cli("kwp-fast", None).unwrap());
        assert_eq!(result.is_ok(), success);
        assert_eq!(io.0.borrow().fast_requests.len(), attempts);
        assert_eq!(io.inits(), 0);
        assert!(io.0.borrow().sent.is_empty());
        if attempts == 2 {
            assert!(io.now().duration_since(before) >= SESSION_IDLE);
        }
        drop(result);
        assert!(io
            .0
            .borrow()
            .calls
            .ends_with(&["stop".into(), "disconnect".into()]));
    }
}

#[test]
fn failed_fast_init_releases_resources_without_falling_back_to_five_baud() {
    let io = Fake::new();
    io.0.borrow_mut().failure = Some("fast_init");
    assert!(KLineSession::open(io.clone(), BusSpec::from_cli("kwp-fast", None).unwrap()).is_err());
    assert_eq!(io.inits(), 0);
    assert!(io.0.borrow().sent.is_empty());
    assert!(io
        .0
        .borrow()
        .calls
        .ends_with(&["stop".into(), "disconnect".into()]));

    io.0.borrow_mut().failure = None;
    io.0.borrow_mut().keys = [0xEF, 0x8F];
    let mut spec = BusSpec::from_cli("kwp-fast", None).unwrap();
    spec.init_address = Some(0x09);
    spec.pins = vec![7, 15];
    let _session = KLineSession::open(io.clone(), spec).unwrap();
    assert_eq!(io.0.borrow().fast_requests, [vec![0xC1, 9, 0xF1, 0x81]]);
    assert!(io.0.borrow().calls.contains(&"connect:4:0".into()));
}

#[test]
fn kwp_rejects_malformed_lengths_wrong_protocols_and_unrelated_addresses() {
    for data in [
        vec![0x86, 0xF1, 0x11, 0x41, 0, 0, 0, 0],
        vec![0x83, 0xF1, 0x11, 0x41, 0x0C, 0, 0],
        vec![0x80, 0xF1, 0x11, 3, 0x41, 0x0D, 0],
        vec![0x88, 0xF1, 0x11, 0x41, 0, 0, 0, 0, 0, 0, 0],
    ] {
        assert!(decode(kwp_rx(data), PROTOCOL_ISO14230).is_err());
    }
    for header in [[0xC3, 0x33, 0xF1], [0x83, 0x10, 0x11], [0x83, 0xF1, 0xF1]] {
        let mut data = header.to_vec();
        data.extend([0x41, 0x0D, 0]);
        assert!(decode(kwp_rx(data), PROTOCOL_ISO14230).unwrap().is_none());
    }
    let mut message = kwp_rx(vec![0x83, 0xF1, 0x11, 0x41, 0x0D, 0]);
    message.data.push(0xD9);
    assert_eq!(
        decode(message.clone(), PROTOCOL_ISO14230)
            .unwrap()
            .unwrap()
            .data,
        [0x41, 0x0D, 0]
    );
    message.protocol_id = PROTOCOL_ISO9141;
    assert!(decode(message, PROTOCOL_ISO14230).is_err());
}

#[test]
fn kwp_reinitializes_after_timeout_and_idle_and_groups_multiple_ecus() {
    use crate::test_support::kwp_emulator_messages;
    let io = Fake::new();
    io.0.borrow_mut().keys = [0xE9, 0x8F];
    let mut session = KLineSession::open(io.clone(), kwp_bus()).unwrap();
    assert!(exchange(&mut session, &[1, 0]).unwrap().is_empty());
    for _ in 0..2 {
        let good = kwp_emulator_messages("0100")[0].clone();
        let mut second = good.clone();
        second[2] = 0x18;
        let mut stale = good.clone();
        stale[4] = 0x20;
        io.0.borrow_mut()
            .exchanges
            .push_back(vec![kwp_rx(stale), kwp_rx(good), kwp_rx(second)]);
        let replies = exchange(&mut session, &[1, 0]).unwrap();
        assert_eq!(
            replies.iter().map(|reply| reply.id).collect::<Vec<_>>(),
            [0x86F111, 0x86F118]
        );
        io.wait(Duration::from_secs(6));
    }
    assert_eq!(io.inits(), 3);
    assert_eq!(io.0.borrow().sent.len(), 3);
}

#[test]
fn captured_accord_messages_keep_all_vin_and_calibration_parts() {
    let io = Fake::new();
    let mut session = KLineSession::open(io.clone(), bus()).unwrap();
    for (request, count) in [("0100", 1), ("0900", 1), ("0902", 5), ("0904", 4)] {
        io.script(request);
        let payload = crate::utils::hex_to_bytes(request).unwrap();
        let replies = exchange(&mut session, &payload).unwrap();
        assert_eq!(replies.len(), count);
        assert!(replies.iter().all(|reply| reply.id == 0x486B09));
    }
    assert_eq!(io.inits(), 1);
    assert_eq!(io.0.borrow().sent[0], [0x68, 0x6A, 0xF1, 1, 0]);
    assert_eq!(io.0.borrow().calls[0], "connect:3:1000");
    drop(session);
    assert!(io
        .0
        .borrow()
        .calls
        .ends_with(&["stop".into(), "disconnect".into()]));
}

#[test]
fn setup_failures_release_the_channel_and_any_created_filter() {
    for failure in ["config", "filter:3", "init:33", "clear"] {
        let io = Fake::new();
        io.0.borrow_mut().failure = Some(failure);
        let mut bus = bus();
        bus.timing.insert("P4_MIN".into(), 10);
        assert!(KLineSession::open(io.clone(), bus).is_err());
        let state = io.0.borrow();
        assert_eq!(state.calls.last().unwrap(), "disconnect");
        assert_eq!(
            state.calls.contains(&"stop".into()),
            matches!(failure, "init:33" | "clear")
        );
    }
}

#[test]
fn pins_keys_and_target_validation_precede_diagnostic_transmission() {
    let io = Fake::new();
    let mut spec = bus();
    spec.pins = vec![7, 15];
    let mut session = KLineSession::open(io.clone(), spec).unwrap();
    assert_eq!(io.0.borrow().calls[0], "connect:3:0");
    session.set_target(9).unwrap();
    assert!(session.set_target(256).is_err());
    assert!(session.send(&[0x22, 0xF1, 0x81]).is_err());
    assert!(exchange(&mut session, &[]).is_err());
    assert!(exchange(&mut session, &[0; 8]).is_err());
    assert!(io.0.borrow().sent.is_empty());
    let io = Fake::new();
    io.0.borrow_mut().keys = [0xEF, 0x8F];
    assert!(KLineSession::open(io.clone(), bus()).is_err());
    assert_eq!(io.0.borrow().calls.last().unwrap(), "disconnect");
    let io = Fake::new();
    let mut spec = bus();
    spec.pins = vec![15];
    assert!(KLineSession::open(io.clone(), spec).is_err());
    assert!(io.0.borrow().calls.is_empty());
}

#[test]
fn echoes_and_unrelated_pids_do_not_replace_multi_ecu_replies() {
    let io = Fake::new();
    let mut session = KLineSession::open(io.clone(), bus()).unwrap();
    let good = accord_messages("0100")[0].clone();
    let mut echo = rx(good.clone());
    echo.rx_status = TX_MSG_TYPE;
    let mut other = good.clone();
    other[2] = 0x10;
    let mut stale = good.clone();
    stale[4] = 0x20;
    io.0.borrow_mut()
        .exchanges
        .push_back(vec![echo, rx(stale), rx(good), rx(other)]);
    let result = exchange(&mut session, &[1, 0]).unwrap();
    assert_eq!(
        result.iter().map(|r| r.id).collect::<Vec<_>>(),
        [0x486B09, 0x486B10]
    );
}

#[test]
fn timeout_is_not_retried_and_next_request_reinitializes_after_quiet() {
    let io = Fake::new();
    let mut session = KLineSession::open(io.clone(), bus()).unwrap();
    assert!(exchange(&mut session, &[1, 0]).unwrap().is_empty());
    assert_eq!(io.0.borrow().sent.len(), 1);
    assert_eq!(io.inits(), 1);
    let before = io.now();
    // A late reply from the timed-out exchange must be cleared before retrying the session.
    io.0.borrow_mut()
        .rx
        .push_back(rx(accord_messages("0100")[0].clone()));
    io.script("0100");
    assert_eq!(exchange(&mut session, &[1, 0]).unwrap().len(), 1);
    assert_eq!(io.inits(), 2);
    assert!(io.now().duration_since(before) >= Duration::from_millis(4500));
}

#[test]
fn collection_deadline_never_reports_partial_replies_as_complete() {
    let io = Fake::new();
    let mut session = KLineSession::open(io.clone(), bus()).unwrap();
    io.script("0902");
    let result = session.request_functional(&[9, 2], Duration::from_millis(60));
    assert!(matches!(result, Err(DiagError::Timeout(_))));
    assert!(!session.initialized);
    assert_eq!(io.0.borrow().sent.len(), 1);
}

#[test]
fn timing_and_initialization_address_are_applied_explicitly() {
    let io = Fake::new();
    let mut spec = bus();
    spec.init_address = Some(0x09);
    spec.timing.insert("P4_MIN".into(), 0);
    assert_eq!(timing(&spec).unwrap(), [(P4_MIN, 0)]);
    let session = KLineSession::open(io.clone(), spec).unwrap();
    assert!(io.0.borrow().calls.contains(&"init:09".into()));
    assert_eq!(
        io.0.borrow()
            .calls
            .iter()
            .filter(|call| *call == "config")
            .count(),
        2
    );
    drop(session);
    for (key, value) in [("P2_STAR", 100), ("P1_MAX", 0), ("P1_MAX", 65536)] {
        let io = Fake::new();
        let mut spec = bus();
        spec.timing.insert(key.into(), value);
        assert!(KLineSession::open(io.clone(), spec).is_err());
        assert!(io.0.borrow().calls.is_empty());
    }
}

#[test]
fn idle_session_and_reset_both_require_fresh_initialization() {
    let io = Fake::new();
    let mut session = KLineSession::open(io.clone(), bus()).unwrap();
    io.wait(Duration::from_secs(6));
    io.script("0100");
    exchange(&mut session, &[1, 0]).unwrap();
    assert_eq!(io.inits(), 2);
    session.reset().unwrap();
    io.script("0100");
    exchange(&mut session, &[1, 0]).unwrap();
    assert_eq!(io.inits(), 3);
}

#[test]
fn transport_failure_is_not_silence_or_a_successful_ecu_reply() {
    for status in [RX_BREAK, RX_ERROR, BUFFER_OVERFLOW, TX_FAILED | TX_MSG_TYPE] {
        let io = Fake::new();
        let mut session = KLineSession::open(io.clone(), bus()).unwrap();
        let mut reply = rx(accord_messages("0100")[0].clone());
        reply.rx_status = status;
        io.0.borrow_mut().exchanges.push_back(vec![reply]);
        assert!(exchange(&mut session, &[1, 0]).is_err());
        assert!(!session.initialized);
        assert_eq!(io.0.borrow().sent.len(), 1);
    }
}

#[test]
fn receive_lengths_and_extra_checksum_bytes_are_checked() {
    let data = accord_messages("0100")[0].clone();
    let mut extra = rx(data.clone());
    extra.data.push(0xB3);
    assert_eq!(
        decode(extra, PROTOCOL_ISO9141).unwrap().unwrap().data,
        data[3..]
    );
    for len in [0, 2, 3, 11] {
        assert!(decode(rx(vec![0; len]), PROTOCOL_ISO9141).is_err());
    }
    let mut invalid = rx(data);
    invalid.extra_data_index += 1;
    assert!(decode(invalid, PROTOCOL_ISO9141).is_err());
}

#[test]
fn queue_full_retries_are_bounded_and_do_not_duplicate_an_accepted_request() {
    let io = Fake::new();
    let mut session = KLineSession::open(io.clone(), bus()).unwrap();
    io.0.borrow_mut().full = 2;
    io.script("0100");
    exchange(&mut session, &[1, 0]).unwrap();
    assert_eq!(io.0.borrow().sent.len(), 1);
    io.0.borrow_mut().full = usize::MAX;
    let before = io.now();
    assert!(exchange(&mut session, &[1, 0]).is_err());
    assert!(io.now().duration_since(before) < Duration::from_secs(3));
    assert_eq!(io.0.borrow().sent.len(), 1);
}
