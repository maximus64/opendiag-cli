use super::*;
use crate::test_support::{ford_messages, gm_messages};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

struct State {
    now: Instant,
    calls: Vec<String>,
    failure: Option<&'static str>,
    configs: Vec<(u32, u32)>,
    addresses: Vec<u8>,
    full: usize,
    exchanges: VecDeque<Vec<RxMsg>>,
    rx: VecDeque<RxMsg>,
    sent: Vec<Vec<u8>>,
    buses: Vec<BusSpec>,
    filters: Vec<BusSpec>,
    protocols: Vec<u32>,
}

#[derive(Clone)]
struct Fake(Rc<RefCell<State>>);
impl Fake {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(State {
            now: Instant::now(),
            calls: Vec::new(),
            failure: None,
            configs: Vec::new(),
            addresses: Vec::new(),
            full: 0,
            exchanges: VecDeque::new(),
            rx: VecDeque::new(),
            sent: Vec::new(),
            buses: Vec::new(),
            filters: Vec::new(),
            protocols: Vec::new(),
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
            .push_back(ford_messages(request).into_iter().map(rx).collect());
    }
}

impl J1850Io for Fake {
    fn connect(&self, bus: &BusSpec) -> Result<u32, DiagError> {
        self.call("connect")?;
        self.0.borrow_mut().buses.push(bus.clone());
        Ok(1)
    }
    fn configure(&self, _: u32, params: &[(u32, u32)]) -> Result<(), DiagError> {
        self.call("configure")?;
        self.0.borrow_mut().configs.extend_from_slice(params);
        Ok(())
    }
    fn functional_addresses(&self, _: u32, addresses: &[u8]) -> Result<(), DiagError> {
        self.call("addresses")?;
        self.0.borrow_mut().addresses = addresses.to_vec();
        Ok(())
    }
    fn filter(&self, _: u32, bus: &BusSpec) -> Result<u32, DiagError> {
        self.call("filter")?;
        self.0.borrow_mut().filters.push(bus.clone());
        Ok(2)
    }
    fn stop_filter(&self, _: u32, _: u32) -> Result<(), DiagError> {
        self.call("stop")
    }
    fn clear(&self, _: u32, queue: u32) -> Result<(), DiagError> {
        self.call(if queue == CLEAR_RX_QUEUE {
            "clear-rx"
        } else {
            "clear-tx"
        })?;
        if queue == CLEAR_RX_QUEUE {
            self.0.borrow_mut().rx.clear();
        }
        Ok(())
    }
    fn queue(&self, _: u32, protocol: u32, data: &[u8]) -> Result<(), DiagError> {
        self.call("queue")?;
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
        state.protocols.push(protocol);
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
    BusSpec::standard(BusKind::J1850Pwm)
}
fn rx(data: Vec<u8>) -> RxMsg {
    RxMsg {
        protocol_id: PROTOCOL_J1850PWM,
        rx_status: 0,
        extra_data_index: data.len(),
        data,
    }
}
fn exchange(
    session: &mut J1850Session<Fake>,
    request: &[u8],
) -> Result<Vec<ResponseFrame>, DiagError> {
    session.request_functional(request, Duration::from_secs(3))
}

#[test]
fn ford_capture_uses_pwm_headers_and_explicit_ifr_configuration() {
    let io = Fake::new();
    let mut session = J1850Session::open(io.clone(), bus()).unwrap();
    assert_eq!(
        io.0.borrow().configs,
        [(NODE_ADDRESS, 0xF1), (NETWORK_LINE, 0)]
    );
    assert_eq!(io.0.borrow().addresses, [0x6B]);
    for request in ["0100", "0101", "0105", "010C", "010D"] {
        io.script(request);
        let payload = crate::utils::hex_to_bytes(request).unwrap();
        let replies = exchange(&mut session, &payload).unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, 0x416B10);
        let mut expected = vec![0x61, 0x6A, 0xF1];
        expected.extend_from_slice(&payload);
        assert_eq!(io.0.borrow().sent.last(), Some(&expected));
    }
    drop(session);
    assert!(io
        .0
        .borrow()
        .calls
        .ends_with(&["stop".into(), "disconnect".into()]));
}

#[test]
fn setup_failures_release_every_owned_resource() {
    for failure in ["configure", "addresses", "filter", "clear-tx", "clear-rx"] {
        let io = Fake::new();
        io.0.borrow_mut().failure = Some(failure);
        assert!(J1850Session::open(io.clone(), bus()).is_err());
        let state = io.0.borrow();
        assert_eq!(state.calls.last().unwrap(), "disconnect");
        assert_eq!(
            state.calls.contains(&"stop".into()),
            failure.starts_with("clear")
        );
    }
}

#[test]
fn unsupported_bus_settings_fail_before_connecting() {
    for variant in 0..5 {
        let io = Fake::new();
        let mut spec = bus();
        match variant {
            0 => spec.kind = BusKind::J1850Vpw,
            1 => spec.bitrate = 10400,
            2 => spec.pins = vec![2],
            3 => spec.init_address = Some(0x33),
            _ => {
                spec.timing.insert("P2_MAX".into(), 100);
            }
        }
        assert!(J1850Session::open(io.clone(), spec).is_err());
        assert!(io.0.borrow().calls.is_empty());
    }
}

#[test]
fn invalid_requests_and_manufacturer_traffic_do_not_transmit() {
    let io = Fake::new();
    let mut session = J1850Session::open(io.clone(), bus()).unwrap();
    session.set_target(0x10).unwrap();
    assert!(session.set_target(0x100).is_err());
    assert!(session.send(&[0x22, 0xF1, 0x90]).is_err());
    assert!(exchange(&mut session, &[]).is_err());
    assert!(exchange(&mut session, &[0; 8]).is_err());
    assert!(session.request_functional(&[1, 0], Duration::ZERO).is_err());
    assert!(io.0.borrow().sent.is_empty());
}

#[test]
fn multi_ecu_replies_keep_all_frames_and_ignore_echoes_and_other_pids() {
    let io = Fake::new();
    let mut session = J1850Session::open(io.clone(), bus()).unwrap();
    let first = rx(vec![0x41, 0x6B, 0x10, 0x43, 1, 1]);
    let second = rx(vec![0x41, 0x6B, 0x10, 0x43, 2, 2]);
    let other = rx(vec![0x41, 0x6B, 0x18, 0x43, 3, 3]);
    let mut echo = first.clone();
    echo.rx_status = TX_MSG_TYPE;
    let stale = rx(ford_messages("0100")[0].clone());
    io.0.borrow_mut()
        .exchanges
        .push_back(vec![echo, stale, first, second, other]);
    let replies = exchange(&mut session, &[3]).unwrap();
    assert_eq!(replies.len(), 3);
    assert_eq!(
        replies.iter().map(|reply| reply.id).collect::<Vec<_>>(),
        [0x416B10, 0x416B10, 0x416B18]
    );
    assert!(matches_request(&[1, 5], &[0x7F, 1, 0x12]));
    assert!(!matches_request(&[1, 5], &[0x41, 0x0C, 0, 0]));
}

#[test]
fn silence_is_not_retried_and_the_next_exchange_discards_late_data() {
    let io = Fake::new();
    let mut session = J1850Session::open(io.clone(), bus()).unwrap();
    assert!(exchange(&mut session, &[9, 0]).unwrap().is_empty());
    assert_eq!(io.0.borrow().sent.len(), 1);
    io.0.borrow_mut()
        .rx
        .push_back(rx(vec![0x41, 0x6B, 0x18, 0x41, 0, 0, 0, 0, 0]));
    io.script("0100");
    let before = io.now();
    let replies = exchange(&mut session, &[1, 0]).unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].id, 0x416B10);
    assert!(io.now().duration_since(before) >= 2 * REPLY_QUIET);
    session.reset().unwrap();
    assert!(session.recover);
}

#[test]
fn queue_backpressure_is_bounded_and_only_retries_unaccepted_messages() {
    let io = Fake::new();
    let mut session = J1850Session::open(io.clone(), bus()).unwrap();
    io.0.borrow_mut().full = 2;
    io.script("0100");
    exchange(&mut session, &[1, 0]).unwrap();
    assert_eq!(io.0.borrow().sent.len(), 1);
    io.0.borrow_mut().full = usize::MAX;
    let before = io.now();
    assert!(exchange(&mut session, &[1, 0]).is_err());
    assert!(io.now().duration_since(before) <= QUEUE_WAIT);
    assert_eq!(io.0.borrow().sent.len(), 1);
}

#[test]
fn partial_collection_and_transport_errors_are_not_successful_replies() {
    let io = Fake::new();
    let mut session = J1850Session::open(io.clone(), bus()).unwrap();
    io.script("0100");
    assert!(session
        .request_functional(&[1, 0], Duration::from_millis(30))
        .is_err());
    assert!(session.recover);
    for status in [
        RX_BREAK,
        RX_ERROR,
        BUFFER_OVERFLOW,
        TX_FAILED,
        TX_FAILED | TX_MSG_TYPE,
    ] {
        let mut error = rx(Vec::new());
        error.rx_status = status;
        io.0.borrow_mut()
            .exchanges
            .push_back(vec![rx(ford_messages("0100")[0].clone()), error]);
        assert!(exchange(&mut session, &[1, 0]).is_err());
    }
}

#[test]
fn decoder_checks_protocol_lengths_and_excludes_extra_bytes() {
    let frame = ford_messages("0100")[0].clone();
    let mut message = rx(frame.clone());
    message.data.extend([0xAA, 0xF1]);
    assert_eq!(
        decode(BusKind::J1850Pwm, message).unwrap().unwrap().data,
        frame[3..]
    );
    for length in [0, 3, 11] {
        assert!(decode(BusKind::J1850Pwm, rx(vec![0; length])).is_err());
    }
    let mut message = rx(frame.clone());
    message.extra_data_index += 1;
    assert!(decode(BusKind::J1850Pwm, message).is_err());
    let mut message = rx(frame.clone());
    message.protocol_id = PROTOCOL_ISO9141;
    assert!(decode(BusKind::J1850Pwm, message).is_err());
    let mut request = frame;
    request[..3].copy_from_slice(&[0x61, 0x6A, 0xF1]);
    assert!(decode(BusKind::J1850Pwm, rx(request)).unwrap().is_none());
    for status in [TX_MSG_TYPE, START_OF_MESSAGE, TX_INDICATION_SUCCESS] {
        let mut message = rx(Vec::new());
        message.rx_status = status;
        assert!(decode(BusKind::J1850Pwm, message).unwrap().is_none());
    }
}

fn vpw_rx(data: Vec<u8>) -> RxMsg {
    RxMsg {
        protocol_id: PROTOCOL_J1850VPW,
        ..rx(data)
    }
}

#[test]
fn gm_capture_uses_vpw_headers_without_pwm_configuration() {
    let io = Fake::new();
    let spec = BusSpec::standard(BusKind::J1850Vpw);
    let mut session = J1850Session::open(io.clone(), spec.clone()).unwrap();
    assert_eq!(io.0.borrow().buses.as_slice(), std::slice::from_ref(&spec));
    assert_eq!(io.0.borrow().filters, [spec]);
    assert!(io.0.borrow().configs.is_empty());
    assert!(io.0.borrow().addresses.is_empty());
    assert!(!io.0.borrow().calls.contains(&"configure".into()));
    assert!(!io.0.borrow().calls.contains(&"addresses".into()));
    for request in [
        "0100", "0101", "0105", "010C", "010D", "0902", "0904", "0906", "07",
    ] {
        let messages = gm_messages(request);
        io.0.borrow_mut()
            .exchanges
            .push_back(messages.iter().cloned().map(vpw_rx).collect());
        let payload = crate::utils::hex_to_bytes(request).unwrap();
        let replies = exchange(&mut session, &payload).unwrap();
        assert_eq!(replies.len(), messages.len());
        for (reply, message) in replies.iter().zip(&messages) {
            assert_eq!(reply.id, 0x486B10);
            assert_eq!(reply.data, message[3..]);
        }
        let mut expected = vec![0x68, 0x6A, 0xF1];
        expected.extend_from_slice(&payload);
        assert_eq!(io.0.borrow().sent.last(), Some(&expected));
        assert_eq!(io.0.borrow().protocols.last(), Some(&PROTOCOL_J1850VPW));
    }
    assert_eq!(session.max_payload(), 8);
    assert!(exchange(&mut session, &[0; 9]).is_err());
    assert!(session.send(&[0x22, 0xF1, 0x90]).is_err());
    drop(session);
    assert!(io
        .0
        .borrow()
        .calls
        .ends_with(&["stop".into(), "disconnect".into()]));
}

#[test]
fn vpw_settings_and_setup_failures_leave_no_channel_open() {
    for failure in ["filter", "clear-tx", "clear-rx"] {
        let io = Fake::new();
        io.0.borrow_mut().failure = Some(failure);
        assert!(J1850Session::open(io.clone(), BusSpec::standard(BusKind::J1850Vpw)).is_err());
        let state = io.0.borrow();
        assert_eq!(state.calls.last().unwrap(), "disconnect");
        assert_eq!(state.calls.contains(&"stop".into()), failure != "filter");
    }
    for variant in 0..4 {
        let io = Fake::new();
        let mut spec = BusSpec::standard(BusKind::J1850Vpw);
        match variant {
            0 => spec.bitrate = 41600,
            1 => spec.pins = vec![2, 10],
            2 => spec.init_address = Some(0x33),
            _ => {
                spec.timing.insert("P2_MAX".into(), 100);
            }
        }
        assert!(J1850Session::open(io.clone(), spec).is_err());
        assert!(io.0.borrow().calls.is_empty());
    }
}

#[test]
fn vpw_decoder_checks_protocol_extra_bytes_and_larger_payload_limit() {
    let kind = BusKind::J1850Vpw;
    let frame = vec![0x48, 0x6B, 0x10, 0x49, 0x08, 1, 2, 3, 4, 5, 6];
    let mut message = vpw_rx(frame.clone());
    message.data.extend([0xAA, 0xF1]);
    assert_eq!(decode(kind, message).unwrap().unwrap().data, frame[3..]);
    assert!(decode(kind, rx(frame.clone())).is_err());
    for length in [0, 3, 12] {
        assert!(decode(kind, vpw_rx(vec![0; length])).is_err());
    }
    let mut message = vpw_rx(frame.clone());
    message.extra_data_index += 1;
    assert!(decode(kind, message).is_err());
    for status in [RX_ERROR, RX_BREAK, BUFFER_OVERFLOW, TX_FAILED | TX_MSG_TYPE] {
        let mut message = vpw_rx(frame.clone());
        message.rx_status = status;
        assert!(decode(kind, message).is_err());
    }
    assert!(decode(kind, vpw_rx(vec![0xE8, 0xFF, 0x10, 3]))
        .unwrap()
        .is_none());
}

#[test]
fn vpw_background_traffic_does_not_extend_obd_collection() {
    let io = Fake::new();
    let mut session = J1850Session::open(io.clone(), BusSpec::standard(BusKind::J1850Vpw)).unwrap();
    let mut messages: Vec<_> = gm_messages("0100").into_iter().map(vpw_rx).collect();
    messages.extend((0..100).map(|_| vpw_rx(vec![0xE8, 0xFF, 0x10, 3])));
    io.0.borrow_mut().exchanges.push_back(messages);
    let before = io.now();
    assert_eq!(exchange(&mut session, &[1, 0]).unwrap().len(), 1);
    assert_eq!(
        io.now().duration_since(before),
        REPLY_QUIET + Duration::from_millis(20)
    );
}
