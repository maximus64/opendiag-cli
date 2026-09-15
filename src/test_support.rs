//! Shared test fakes. Compiled only under `cfg(test)`.
//!
//! Everything here works in whole diagnostic messages, the level the transport
//! traits in `link.rs` expose:
//!
//! * [`MockLink`] replays scripted replies and records what was sent, so
//!   protocol/UDS tests assert on request payloads.
//! * [`FakeAdapter`] records programming-voltage calls and hands out links.
//! * [`FakeEcu`] is a stateful ECU simulator: it answers UDS requests according
//!   to the flash state machine and refuses operations attempted out of order.
//!   It is both the link and the adapter, so flash tests see the programming
//!   voltage it was given. It is what makes a whole-flash test possible.
//!
//! [`RwdFile`] builds `.rwd` container bytes for tests that need a real file.

use std::cell::{Ref, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use crate::errors::DiagError;
use crate::link::{Adapter, BusSpec, DeviceOps, DiagLink, IdWidth, ResponseFrame};
use crate::obd::ObdProtocol;
use crate::utils::{bytes_to_hex, hex_to_bytes};

pub fn accord_messages(request: &str) -> Vec<Vec<u8>> {
    captured_messages(
        include_str!("../tests/fixtures/accord-2005-iso9141.toml"),
        request,
    )
}

pub fn ford_messages(request: &str) -> Vec<Vec<u8>> {
    captured_messages(
        include_str!("../tests/fixtures/ford-1997-pwm.toml"),
        request,
    )
}

pub fn gm_messages(request: &str) -> Vec<Vec<u8>> {
    captured_messages(include_str!("../tests/fixtures/gm-2002-vpw.toml"), request)
}

pub fn kwp_emulator_messages(request: &str) -> Vec<Vec<u8>> {
    captured_messages(
        include_str!("../tests/fixtures/emulator-kwp-5baud.toml"),
        request,
    )
}

pub fn kwp_emulator_replies(request: &str) -> Vec<ResponseFrame> {
    captured_replies(kwp_emulator_messages(request))
}

#[cfg(feature = "j2534")]
pub fn kwp_fast_emulator_messages(request: &str) -> Vec<Vec<u8>> {
    captured_messages(
        include_str!("../tests/fixtures/emulator-kwp-fast.toml"),
        request,
    )
}

pub fn gm_replies(request: &str) -> Vec<crate::link::ResponseFrame> {
    captured_replies(gm_messages(request))
}

fn captured_messages(text: &str, request: &str) -> Vec<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Exchange {
        request: String,
        responses: Vec<String>,
    }
    #[derive(serde::Deserialize)]
    struct Capture {
        exchange: Vec<Exchange>,
    }
    let capture: Capture = toml::from_str(text).unwrap();
    capture
        .exchange
        .into_iter()
        .find(|exchange| exchange.request == request)
        .unwrap()
        .responses
        .iter()
        .map(|data| hex_to_bytes(data).unwrap())
        .collect()
}

pub fn accord_replies(request: &str) -> Vec<crate::link::ResponseFrame> {
    captured_replies(accord_messages(request))
}

pub fn ford_replies(request: &str) -> Vec<crate::link::ResponseFrame> {
    captured_replies(ford_messages(request))
}

fn captured_replies(messages: Vec<Vec<u8>>) -> Vec<crate::link::ResponseFrame> {
    messages
        .into_iter()
        .map(|data| crate::link::ResponseFrame {
            id: u32::from_be_bytes([0, data[0], data[1], data[2]]),
            data: data[3..].to_vec(),
        })
        .collect()
}

#[derive(Default, Clone)]
pub struct RecordingEvents(pub Rc<RefCell<Vec<String>>>);
impl crate::events::Events for RecordingEvents {
    fn emit(&mut self, event: crate::events::Event<'_>) {
        use crate::events::Event;
        self.0.borrow_mut().push(match event {
            Event::Step(s) => format!("step: {s}"),
            Event::Warning(s) => format!("warning: {s}"),
            Event::Detail(s) => format!("detail: {s}"),
            Event::Progress { done, total, .. } => format!("progress: {done}/{total}"),
        });
    }
}
pub struct ScriptedInteraction {
    pub confirmed: usize,
    pub proceed: bool,
    pub answers: VecDeque<crate::events::Answer>,
}
impl ScriptedInteraction {
    pub fn new(proceed: bool) -> Self {
        Self {
            confirmed: 0,
            proceed,
            answers: VecDeque::new(),
        }
    }
}
impl crate::events::Interaction for ScriptedInteraction {
    fn confirm(&mut self, _: &crate::preflight::Preflight) -> bool {
        self.confirmed += 1;
        self.proceed
    }
    fn prompt(&mut self, _: &str) -> crate::events::Answer {
        self.answers
            .pop_front()
            .unwrap_or(crate::events::Answer::Abort)
    }
}

/// The response header scripted replies arrive on (ECU 0x01 on 29-bit CAN).
pub const MOCK_RESPONSE_ID: u32 = 0x18DAF101;

// ---------------------------------------------------------------- mock link

#[derive(Default)]
struct MockLinkState {
    sent: Vec<Vec<u8>>,
    targets: Vec<u32>,
    pairs: Vec<(u32, u32)>,
    replies: VecDeque<ResponseFrame>,
    functional_requests: Vec<Vec<u8>>,
    functional_replies: VecDeque<Vec<ResponseFrame>>,
    resets: usize,
    dropped: bool,
}

/// A link that replays scripted replies.
///
/// Cloning shares state, so a clone can be boxed into an [`ObdProtocol`] while
/// the test keeps a handle to inspect what was sent. An exhausted script reads
/// as "no reply" (a timeout), never as a reply.
#[derive(Clone)]
pub struct MockLink {
    bus: BusSpec,
    state: Rc<RefCell<MockLinkState>>,
    /// Only the boxed copy marks the link dropped, so tests can check that a
    /// replaced link really was closed.
    owned: bool,
}

impl MockLink {
    /// A 29-bit CAN link whose replies are the given hex payloads.
    pub fn new(replies: &[&str]) -> Self {
        Self::on_bus(BusSpec::obd_can(IdWidth::Bits29, 500_000), replies)
    }

    pub fn on_bus(bus: BusSpec, replies: &[&str]) -> Self {
        let state = MockLinkState {
            replies: replies
                .iter()
                .map(|hex| ResponseFrame {
                    id: MOCK_RESPONSE_ID,
                    data: hex_to_bytes(hex).expect("hex reply"),
                })
                .collect(),
            ..Default::default()
        };
        Self {
            bus,
            state: Rc::new(RefCell::new(state)),
            owned: false,
        }
    }

    /// Payloads sent so far, as hex, in order.
    pub fn sent(&self) -> Vec<String> {
        self.state
            .borrow()
            .sent
            .iter()
            .map(|p| bytes_to_hex(p))
            .collect()
    }

    /// Every `set_target`, in order.
    pub fn targets(&self) -> Vec<u32> {
        self.state.borrow().targets.clone()
    }
    pub fn pairs(&self) -> Vec<(u32, u32)> {
        self.state.borrow().pairs.clone()
    }

    /// Queues the replies for the next functional request.
    pub fn script_functional(&self, replies: Vec<ResponseFrame>) {
        self.state
            .borrow_mut()
            .functional_replies
            .push_back(replies);
    }

    pub fn script_reply(&self, reply: ResponseFrame) {
        self.state.borrow_mut().replies.push_back(reply);
    }

    /// Functional requests sent so far, as hex.
    pub fn functional_requests(&self) -> Vec<String> {
        self.state
            .borrow()
            .functional_requests
            .iter()
            .map(|p| bytes_to_hex(p))
            .collect()
    }

    /// Scripted replies that were never consumed.
    #[allow(dead_code)]
    pub fn unused_replies(&self) -> usize {
        self.state.borrow().replies.len()
    }

    /// True once the copy handed to the protocol has been dropped.
    pub fn is_dropped(&self) -> bool {
        self.state.borrow().dropped
    }

    /// The copy to hand to the protocol; dropping it marks the link closed.
    pub fn boxed(&self) -> Box<dyn DiagLink> {
        Box::new(MockLink {
            bus: self.bus.clone(),
            state: Rc::clone(&self.state),
            owned: true,
        })
    }
}

impl Drop for MockLink {
    fn drop(&mut self) {
        if self.owned {
            self.state.borrow_mut().dropped = true;
        }
    }
}

impl DiagLink for MockLink {
    fn bus(&self) -> &BusSpec {
        &self.bus
    }

    fn max_payload(&self) -> usize {
        if self.bus.is_can() {
            4095
        } else {
            7
        }
    }

    fn set_target(&mut self, request_header: u32) -> Result<(), DiagError> {
        self.state.borrow_mut().targets.push(request_header);
        Ok(())
    }
    fn set_target_pair(&mut self, request: u32, response: u32) -> Result<(), DiagError> {
        self.state.borrow_mut().pairs.push((request, response));
        self.set_target(request)
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        self.state.borrow_mut().sent.push(payload.to_vec());
        Ok(())
    }

    fn send_functional_to_target(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        let mut state = self.state.borrow_mut();
        state.functional_requests.push(payload.to_vec());
        if let Some(replies) = state.functional_replies.pop_front() {
            state.replies.extend(replies);
        }
        Ok(())
    }

    fn receive(&mut self, _timeout: Duration) -> Result<Option<ResponseFrame>, DiagError> {
        Ok(self.state.borrow_mut().replies.pop_front())
    }

    fn request_functional(
        &mut self,
        payload: &[u8],
        _window: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        let mut state = self.state.borrow_mut();
        state.functional_requests.push(payload.to_vec());
        Ok(state.functional_replies.pop_front().unwrap_or_default())
    }

    fn reset(&mut self) -> Result<(), DiagError> {
        self.state.borrow_mut().resets += 1;
        Ok(())
    }
}

// ------------------------------------------------------------- fake adapter

#[derive(Default)]
struct FakeAdapterState {
    voltage_calls: Vec<(u32, u32)>,
    refused_pins: Vec<(u32, i32)>,
    connects: Vec<BusSpec>,
    links: VecDeque<MockLink>,
}

/// An adapter that records programming-voltage calls and hands out links.
#[derive(Clone, Default)]
pub struct FakeAdapter {
    state: Rc<RefCell<FakeAdapterState>>,
}

impl FakeAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `set_programming_voltage` on `pin` fail with J2534 `code`.
    pub fn refuse_pin(&self, pin: u32, code: i32) {
        self.state.borrow_mut().refused_pins.push((pin, code));
    }

    /// Every programming-voltage call as (pin, value), including refused ones.
    pub fn voltage_calls(&self) -> Vec<(u32, u32)> {
        self.state.borrow().voltage_calls.clone()
    }

    /// The next link `connect` returns.
    pub fn offer_link(&self, link: MockLink) {
        self.state.borrow_mut().links.push_back(link);
    }

    pub fn connects(&self) -> Vec<BusSpec> {
        self.state.borrow().connects.clone()
    }
}

impl DeviceOps for FakeAdapter {
    fn set_programming_voltage(&self, pin: u32, millivolts: u32) -> Result<(), DiagError> {
        let mut state = self.state.borrow_mut();
        state.voltage_calls.push((pin, millivolts));
        match state.refused_pins.iter().find(|(p, _)| *p == pin) {
            Some(&(_, code)) => Err(DiagError::PassThru {
                call: "PassThruSetProgrammingVoltage",
                code,
                detail: String::new(),
            }),
            None => Ok(()),
        }
    }

    fn read_battery_mv(&self) -> Result<u32, DiagError> {
        Ok(12_400)
    }

    fn read_programming_voltage_mv(&self) -> Result<u32, DiagError> {
        Ok(0)
    }
}

impl Adapter for FakeAdapter {
    fn supports(&self, _bus: &BusSpec) -> Option<bool> {
        None
    }

    fn connect(&self, bus: &BusSpec) -> Result<Box<dyn DiagLink>, DiagError> {
        let mut state = self.state.borrow_mut();
        state.connects.push(bus.clone());
        let link = state
            .links
            .pop_front()
            .ok_or_else(|| DiagError::Adapter("no link offered".into()))?;
        Ok(link.boxed())
    }
}

/// Builds an [`ObdProtocol`] over a [`MockLink`] replying with the given hex
/// payloads, returning the link handle too.
pub fn protocol_with(replies: &[&str]) -> (ObdProtocol, MockLink) {
    let link = MockLink::new(replies);
    (
        ObdProtocol::new(Rc::new(FakeAdapter::new()), link.boxed()),
        link,
    )
}

/// Like [`protocol_with`], over an adapter the test keeps a handle to.
pub fn protocol_on_adapter(adapter: &FakeAdapter, replies: &[&str]) -> (ObdProtocol, MockLink) {
    let link = MockLink::new(replies);
    (
        ObdProtocol::new(Rc::new(adapter.clone()), link.boxed()),
        link,
    )
}

/// Builds an [`ObdProtocol`] whose adapter and link are both `ecu`.
pub fn protocol_on(ecu: &FakeEcu) -> ObdProtocol {
    ObdProtocol::new(Rc::new(ecu.clone()), Box::new(ecu.clone()))
}

// ---------------------------------------------------------------- .rwd files

/// Encodes one `.rwd` header: a count byte followed by length-prefixed entries.
pub fn rwd_header(entries: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![entries.len() as u8];
    for e in entries {
        out.push(e.len() as u8);
        out.extend_from_slice(e);
    }
    out
}

/// A `.rwd` container described in terms of what it means, so tests can vary
/// one field without hand-assembling the whole byte layout.
pub struct RwdFile {
    /// Header 2 byte 0: OR'd into `0x18DA00F1` to give the target CAN address.
    pub can_offset: u8,
    /// Header 3: calibration ids this firmware may be flashed over. NUL padding
    /// is added here rather than by the builder, since stripping it is part of
    /// what the parser is expected to do.
    pub versions: Vec<Vec<u8>>,
    /// Header 4: security-access secrets, paired with `versions` by index.
    pub tokens: Vec<Vec<u8>>,
    /// Header 5: the firmware decryption key written to DID 0xF101.
    pub decryption_key: Vec<u8>,
    pub start: u32,
    pub firmware: Vec<u8>,
    pub checksum: u32,
}

impl Default for RwdFile {
    fn default() -> Self {
        Self {
            can_offset: 0x01,
            versions: vec![b"37805-RGL-A030\0".to_vec()],
            tokens: vec![vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66]],
            decryption_key: vec![0xDE, 0xAD, 0xBE],
            start: 0x0001_0000,
            firmware: vec![0xA5; 600],
            checksum: 0x1234_5678,
        }
    }
}

impl RwdFile {
    pub fn encode(&self) -> Vec<u8> {
        let version_refs: Vec<&[u8]> = self.versions.iter().map(|v| v.as_slice()).collect();
        let token_refs: Vec<&[u8]> = self.tokens.iter().map(|v| v.as_slice()).collect();

        let mut f = vec![0x5A, 0x0D, 0x0A];
        f.extend(rwd_header(&[b"header0"]));
        f.extend(rwd_header(&[]));
        f.extend(rwd_header(&[&[self.can_offset]]));
        f.extend(rwd_header(&version_refs));
        f.extend(rwd_header(&token_refs));
        f.extend(rwd_header(&[&self.decryption_key]));
        f.extend_from_slice(&self.start.to_be_bytes());
        f.extend_from_slice(&(self.firmware.len() as u32).to_be_bytes());
        f.extend_from_slice(&self.firmware);
        f.extend_from_slice(&self.checksum.to_le_bytes());
        f
    }

    /// The CAN address this file targets, as the parser will compute it.
    pub fn can_address(&self) -> u32 {
        0x18DA00F1 | ((self.can_offset as u32) << 8)
    }
}

// ----------------------------------------------------------------- fake ECU

/// What the ECU has been asked to do so far, and what it has been told.
#[derive(Default)]
pub struct EcuState {
    // --- configuration ---
    response_id: u32,
    calibration_id: Vec<u8>,
    vin: Vec<u8>,
    /// The K1/K2/K3 constants the unlock key is validated against.
    security_secret: Vec<u8>,
    seed: u16,
    max_block_size: u16,
    transfer_response_override: Option<(u8, Vec<u8>)>,
    battery_mv: u32,

    // --- observations the test asserts on ---
    /// Every request payload, in order.
    pub requests: Vec<Vec<u8>>,
    /// Every programming-voltage call as (pin, millivolts), in order.
    pub programming_voltage: Vec<(u32, u32)>,
    /// Every `set_target`, in order.
    pub headers_set: Vec<u32>,
    pub unlocked: bool,
    pub session: u8,
    pub erased: bool,
    pub decryption_key: Vec<u8>,
    pub download_address: u32,
    pub download_len: u32,
    pub received_firmware: Vec<u8>,
    pub transfer_exited: bool,
    pub dependencies_checked: bool,
    /// Set if the tester ever sent a wrong unlock key or an out-of-order block.
    pub protocol_violations: Vec<String>,

    // --- internal ---
    expected_block_counter: u8,
    /// Replies not yet received by the tester.
    outbox: VecDeque<Vec<u8>>,
    /// The programming voltage currently applied, 0 when off.
    applied_voltage: u32,
    /// What the adapter reports measuring, when it differs from the request.
    measured_voltage_override: Option<u32>,
}

/// The unlock key the ECU expects, computed independently of the flasher's own
/// implementation so the two have to agree on the algorithm.
fn expected_unlock_key(seed: u16, secret: &[u8]) -> u16 {
    let k1 = u16::from_be_bytes([secret[0], secret[1]]) as u32;
    let k2 = u16::from_be_bytes([secret[2], secret[3]]) as u32;
    let seed = seed as u32;
    let mixed = if secret.len() == 6 {
        let k3 = u16::from_be_bytes([secret[4], secret[5]]) as u32;
        (seed * k2 % k3) ^ (seed + k1)
    } else {
        (seed * k2) ^ (seed + k1)
    };
    (mixed & 0xFFFF) as u16
}

impl EcuState {
    fn violation(&mut self, what: &str) -> Vec<u8> {
        self.protocol_violations.push(what.to_string());
        vec![0x7F, 0x00, 0x22] // conditions not correct
    }

    /// Handles one fully reassembled UDS request, returning the response payload.
    fn handle_uds(&mut self, req: &[u8]) -> Vec<u8> {
        match req[0] {
            // DiagnosticSessionControl
            0x10 => {
                self.session = req[1];
                vec![0x50, req[1], 0x00, 0x32, 0x01, 0xF4]
            }
            // SecurityAccess
            0x27 if req[1] == 0x01 => {
                let mut r = vec![0x67, 0x01];
                r.extend_from_slice(&self.seed.to_be_bytes());
                r
            }
            0x27 if req[1] == 0x02 => {
                let expected = expected_unlock_key(self.seed, &self.security_secret);
                let got = u16::from_be_bytes([req[2], req[3]]);
                if got == expected {
                    self.unlocked = true;
                    vec![0x67, 0x02]
                } else {
                    self.protocol_violations.push(format!(
                        "wrong unlock key: got {got:#06x}, want {expected:#06x}"
                    ));
                    vec![0x7F, 0x27, 0x35] // invalid key
                }
            }
            // ReadDataByIdentifier
            0x22 => {
                let did = u16::from_be_bytes([req[1], req[2]]);
                let mut r = vec![0x62, req[1], req[2]];
                match did {
                    0xF181 => r.extend_from_slice(&self.calibration_id),
                    0xF190 => {
                        r.extend_from_slice(&self.vin);
                    }
                    _ => return vec![0x7F, 0x22, 0x31], // request out of range
                }
                r
            }
            // WriteDataByIdentifier
            0x2E => {
                if !self.unlocked {
                    return self.violation("WriteDataByIdentifier before SecurityAccess");
                }
                self.decryption_key = req[3..].to_vec();
                vec![0x6E, req[1], req[2]]
            }
            // RoutineControl
            0x31 => {
                let routine = u16::from_be_bytes([req[2], req[3]]);
                match routine {
                    0xFF00 => {
                        if !self.unlocked {
                            return self.violation("EraseMemory before SecurityAccess");
                        }
                        if self.session != 0x02 {
                            return self.violation("EraseMemory outside the programming session");
                        }
                        self.erased = true;
                    }
                    0xFF01 => {
                        if !self.transfer_exited {
                            return self
                                .violation("CheckProgrammingDependencies before TransferExit");
                        }
                        self.dependencies_checked = true;
                    }
                    _ => return vec![0x7F, 0x31, 0x31],
                }
                vec![0x71, req[1], req[2], req[3], 0x00]
            }
            // RequestDownload
            0x34 => {
                if !self.erased {
                    return self.violation("RequestDownload before EraseMemory");
                }
                self.download_address = u32::from_be_bytes([req[3], req[4], req[5], req[6]]);
                self.download_len = u32::from_be_bytes([req[7], req[8], req[9], req[10]]);
                self.expected_block_counter = 1;
                let mut r = vec![0x74, 0x20];
                r.extend_from_slice(&self.max_block_size.to_be_bytes());
                r
            }
            // TransferData
            0x36 => {
                if self.download_len == 0 {
                    return self.violation("TransferData before RequestDownload");
                }
                // The whole request, service id and counter included, has to fit
                // in the block size this ECU advertised in RequestDownload.
                if req.len() > self.max_block_size as usize {
                    let msg = format!(
                        "TransferData of {} bytes exceeds the advertised max block size of {}",
                        req.len(),
                        self.max_block_size
                    );
                    return self.violation(&msg);
                }
                if self.received_firmware.len() + req.len() - 2 > self.download_len as usize {
                    return self.violation("TransferData wrote past the requested download length");
                }
                if req[1] != self.expected_block_counter {
                    return self.violation(&format!(
                        "block counter out of order: got {}, want {}",
                        req[1], self.expected_block_counter
                    ));
                }
                self.expected_block_counter = self.expected_block_counter.wrapping_add(1);
                self.received_firmware.extend_from_slice(&req[2..]);
                if let Some((counter, response)) = &self.transfer_response_override {
                    if *counter == req[1] {
                        return response.clone();
                    }
                }
                vec![0x76, req[1]]
            }
            // RequestTransferExit
            0x37 => {
                self.transfer_exited = true;
                vec![0x77]
            }
            other => vec![0x7F, other, 0x11], // service not supported
        }
    }
}

/// A stateful ECU that is also the adapter it is reached through.
#[derive(Clone)]
pub struct FakeEcu {
    inner: Rc<RefCell<EcuState>>,
    bus: BusSpec,
}

impl FakeEcu {
    /// An ECU running `calibration_id`, unlockable with `security_secret`.
    pub fn new(response_id: u32, calibration_id: &[u8], security_secret: &[u8]) -> Self {
        Self {
            inner: Rc::new(RefCell::new(EcuState {
                response_id,
                calibration_id: calibration_id.to_vec(),
                vin: b"1HGCM82633A004352".to_vec(),
                security_secret: security_secret.to_vec(),
                seed: 0x1234,
                max_block_size: 0x0102,
                battery_mv: 12_400,
                ..Default::default()
            })),
            bus: BusSpec::obd_can(IdWidth::Bits29, 500_000),
        }
    }

    pub fn state(&self) -> Ref<'_, EcuState> {
        self.inner.borrow()
    }

    /// Makes the adapter report this programming voltage instead of the one
    /// it was asked for, as a faulty supply would.
    pub fn measure_programming_voltage_as(&self, millivolts: u32) {
        self.inner.borrow_mut().measured_voltage_override = Some(millivolts);
    }

    pub fn measure_battery_as(&self, millivolts: u32) {
        self.inner.borrow_mut().battery_mv = millivolts;
    }

    /// Overrides a block's acknowledgement after the ECU accepts its data.
    pub fn reply_to_transfer_with(&self, block_counter: u8, response: &[u8]) {
        self.inner.borrow_mut().transfer_response_override =
            Some((block_counter, response.to_vec()));
    }
}

impl DiagLink for FakeEcu {
    fn bus(&self) -> &BusSpec {
        &self.bus
    }

    fn max_payload(&self) -> usize {
        4095
    }

    fn set_target(&mut self, request_header: u32) -> Result<(), DiagError> {
        self.inner.borrow_mut().headers_set.push(request_header);
        Ok(())
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        let mut st = self.inner.borrow_mut();
        st.requests.push(payload.to_vec());
        let response = st.handle_uds(payload);
        st.outbox.push_back(response);
        Ok(())
    }

    fn receive(&mut self, _timeout: Duration) -> Result<Option<ResponseFrame>, DiagError> {
        let mut st = self.inner.borrow_mut();
        let id = st.response_id;
        Ok(st.outbox.pop_front().map(|data| ResponseFrame { id, data }))
    }

    fn request_functional(
        &mut self,
        _payload: &[u8],
        _window: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        Ok(Vec::new())
    }

    fn reset(&mut self) -> Result<(), DiagError> {
        Ok(())
    }
}

impl DeviceOps for FakeEcu {
    fn set_programming_voltage(&self, pin: u32, millivolts: u32) -> Result<(), DiagError> {
        let mut st = self.inner.borrow_mut();
        st.programming_voltage.push((pin, millivolts));
        st.applied_voltage = if millivolts == crate::obd::protocol::VOLTAGE_PIN_OFF {
            0
        } else {
            millivolts
        };
        Ok(())
    }

    fn read_battery_mv(&self) -> Result<u32, DiagError> {
        Ok(self.inner.borrow().battery_mv)
    }

    fn read_programming_voltage_mv(&self) -> Result<u32, DiagError> {
        let st = self.inner.borrow();
        Ok(st.measured_voltage_override.unwrap_or(st.applied_voltage))
    }
}

impl Adapter for FakeEcu {
    fn supports(&self, _bus: &BusSpec) -> Option<bool> {
        Some(true)
    }

    fn connect(&self, _bus: &BusSpec) -> Result<Box<dyn DiagLink>, DiagError> {
        Ok(Box::new(self.clone()))
    }
}
