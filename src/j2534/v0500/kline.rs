//! ISO 9141-2 and ISO 14230-4 OBD on a J2534 physical channel. The adapter owns byte timing,
//! initialization waveforms and checksums; this layer owns diagnostic exchanges.

use std::time::{Duration, Instant};

use super::{device::Device, ffi::RxMsg};
use crate::{
    events::Event,
    j2534::consts::*,
    link::{BusKind, BusSpec, DiagLink, KwpInit, ResponseFrame},
    obd::addressing::Addressing,
    DiagError,
};

const MAX_PAYLOAD: usize = 7;
const REPLY_QUIET: Duration = Duration::from_millis(300);
const SESSION_IDLE: Duration = Duration::from_secs(5);
const REINITIALIZE_AFTER: Duration = Duration::from_secs(4);
const QUEUE_WAIT: Duration = Duration::from_secs(2);

// A small boundary for testing lifecycle failures against the same session code.
pub(crate) trait KLineIo {
    fn connect(&self, bus: &BusSpec, protocol: u32, flags: u32) -> Result<u32, DiagError>;
    fn configure(&self, channel: u32, params: &[(u32, u32)]) -> Result<(), DiagError>;
    fn filter(&self, channel: u32, protocol: u32) -> Result<u32, DiagError>;
    fn stop_filter(&self, channel: u32, filter: u32) -> Result<(), DiagError>;
    fn initialize(&self, channel: u32, address: u8) -> Result<[u8; 2], DiagError>;
    fn fast_initialize(&self, channel: u32, request: &[u8]) -> Result<RxMsg, DiagError>;
    fn clear(&self, channel: u32, queue: u32) -> Result<(), DiagError>;
    fn queue(&self, channel: u32, protocol: u32, data: &[u8]) -> Result<(), DiagError>;
    fn read(&self, channel: u32, timeout: Duration) -> Result<Option<RxMsg>, DiagError>;
    fn disconnect(&self, channel: u32) -> Result<(), DiagError>;
    fn warn(&self, message: &str);
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn wait(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

impl KLineIo for Device {
    fn connect(&self, bus: &BusSpec, protocol: u32, flags: u32) -> Result<u32, DiagError> {
        self.inner
            .lib
            .connect(self.inner.id, protocol, flags, bus.bitrate, &bus.pins)
    }
    fn configure(&self, channel: u32, params: &[(u32, u32)]) -> Result<(), DiagError> {
        self.inner.lib.set_config(channel, params)
    }
    fn filter(&self, channel: u32, protocol: u32) -> Result<u32, DiagError> {
        let (mask, pattern) = if protocol == PROTOCOL_ISO14230 {
            ([0xC0, 0xFF, 0], [0x80, 0xF1, 0])
        } else {
            ([0xFF, 0xFF, 0], [0x48, 0x6B, 0])
        };
        self.inner
            .lib
            .start_pass_filter(channel, protocol, 0, &mask, &pattern)
    }
    fn stop_filter(&self, channel: u32, filter: u32) -> Result<(), DiagError> {
        self.inner.lib.stop_filter(channel, filter)
    }
    fn initialize(&self, channel: u32, address: u8) -> Result<[u8; 2], DiagError> {
        self.inner.lib.five_baud_init(channel, address)
    }
    fn fast_initialize(&self, channel: u32, request: &[u8]) -> Result<RxMsg, DiagError> {
        self.inner
            .lib
            .fast_init(channel, PROTOCOL_ISO14230, request)
    }
    fn clear(&self, channel: u32, queue: u32) -> Result<(), DiagError> {
        self.inner.lib.clear_queue(channel, queue)
    }
    fn queue(&self, channel: u32, protocol: u32, data: &[u8]) -> Result<(), DiagError> {
        self.inner.lib.queue(channel, protocol, 0, data)
    }
    fn read(&self, channel: u32, timeout: Duration) -> Result<Option<RxMsg>, DiagError> {
        let millis = timeout.as_millis().clamp(1, MAX_READ_TIMEOUT_MS as u128) as u32;
        self.inner.lib.read(channel, millis)
    }
    fn disconnect(&self, channel: u32) -> Result<(), DiagError> {
        self.inner.lib.disconnect(channel)
    }
    fn warn(&self, message: &str) {
        self.emit(Event::Warning(message));
    }
}

pub(crate) struct KLineSession<I: KLineIo> {
    io: I,
    bus: BusSpec,
    protocol: u32,
    channel: u32,
    filter: Option<u32>,
    initialized: bool,
    last_activity: Option<Instant>,
    reply_quiet: Duration,
}

fn timing(bus: &BusSpec) -> Result<Vec<(u32, u32)>, DiagError> {
    bus.timing
        .iter()
        .map(|(name, &value)| {
            let parameter = match name.as_str() {
                "P1_MAX" => P1_MAX,
                "P2_MAX" => P2_MAX,
                "P3_MIN" => P3_MIN,
                "P4_MIN" => P4_MIN,
                _ => {
                    return Err(DiagError::Unsupported(format!(
                        "K-Line timing override {name} is not supported"
                    )))
                }
            };
            let minimum = u32::from(parameter == P1_MAX);
            if value < minimum || value > 0xFFFF {
                return Err(DiagError::InvalidCommand(format!(
                    "{name} must be {minimum}..65535 in half-millisecond units"
                )));
            }
            Ok((parameter, value))
        })
        .collect()
}

impl<I: KLineIo> KLineSession<I> {
    pub(crate) fn open(io: I, bus: BusSpec) -> Result<Self, DiagError> {
        let protocol = match bus.kind {
            BusKind::Iso9141 => PROTOCOL_ISO9141,
            BusKind::Iso14230 { .. } => PROTOCOL_ISO14230,
            _ => {
                return Err(DiagError::Unsupported(
                    "Use an ISO 9141-2 or KWP2000 bus".into(),
                ))
            }
        };
        if bus.bitrate == 0 {
            return Err(DiagError::InvalidCommand(
                "K-Line requires a nonzero bitrate".into(),
            ));
        }
        let flags = match bus.pins.as_slice() {
            [7] => K_LINE_ONLY,
            [7, 15] => 0,
            _ => {
                return Err(DiagError::Unsupported(
                    "K-Line requires pins [7] or [7, 15]".into(),
                ))
            }
        };
        let params = timing(&bus)?;
        let quiet_half_ms = u64::from(bus.timing.get("P1_MAX").copied().unwrap_or(40))
            + u64::from(bus.timing.get("P2_MAX").copied().unwrap_or(100));
        let reply_quiet = REPLY_QUIET.max(Duration::from_micros(quiet_half_ms * 500));
        let channel = io.connect(&bus, protocol, flags)?;
        let mut session = Self {
            io,
            bus,
            protocol,
            channel,
            filter: None,
            initialized: false,
            last_activity: None,
            reply_quiet,
        };
        // Construct the owner before any fallible setup so Drop also closes failed opens.
        if !params.is_empty() {
            session.io.configure(channel, &params)?;
        }
        session.filter = Some(session.io.filter(channel, protocol)?);
        session.initialize()?;
        Ok(session)
    }

    fn initialize(&mut self) -> Result<(), DiagError> {
        self.initialized = false;
        if let Some(last) = self.last_activity {
            // Let an interrupted/expired exchange leave communication mode before waking it.
            self.io
                .wait((last + SESSION_IDLE).saturating_duration_since(self.io.now()));
        }
        self.io.clear(self.channel, CLEAR_TX_QUEUE)?;
        self.io.clear(self.channel, CLEAR_RX_QUEUE)?;
        self.last_activity = Some(self.io.now());
        let address = self.bus.init_address.unwrap_or(0x33);
        let result = match self.bus.kind {
            BusKind::Iso14230 {
                init: KwpInit::Fast,
            } => self.initialize_fast(address),
            _ => self.io.initialize(self.channel, address),
        };
        self.last_activity = Some(self.io.now());
        let keys = result?;
        // ISO 14230-4 section 4.4 permits keywords 2025, 2027, 2029 and 2031.
        let valid_keys = if self.protocol == PROTOCOL_ISO14230 {
            matches!(keys, [0xE9 | 0x6B | 0x6D | 0xEF, 0x8F])
        } else {
            matches!(keys, [0x08, 0x08] | [0x94, 0x94])
        };
        if !valid_keys {
            // The handshake may have opened another protocol's session. Let it expire
            // before detection attempts a fresh initialization on this same K-Line.
            self.io.wait(SESSION_IDLE);
            return Err(DiagError::Unsupported(format!(
                "Key bytes {:02X} {:02X} do not identify {} OBD",
                keys[0], keys[1], self.bus
            )));
        }
        // Some adapters adjust timing after interpreting the key bytes.
        let params = timing(&self.bus)?;
        if !params.is_empty() {
            self.io.configure(self.channel, &params)?;
        }
        self.initialized = true;
        Ok(())
    }

    fn initialize_fast(&mut self, address: u8) -> Result<[u8; 2], DiagError> {
        let request = [0xC1, address, 0xF1, 0x81];
        let reply = match self.io.fast_initialize(self.channel, &request) {
            Err(DiagError::PassThru {
                code: ERR_INIT_FAILED,
                ..
            }) => {
                self.io.warn(
                    "KWP fast initialization failed; retrying once after session idle expiry",
                );
                // A lost reply may still have opened a session in the ECU.
                self.io.wait(SESSION_IDLE);
                self.io.clear(self.channel, CLEAR_TX_QUEUE)?;
                self.io.clear(self.channel, CLEAR_RX_QUEUE)?;
                self.io.fast_initialize(self.channel, &request)?
            }
            result => result?,
        };
        fast_init_keys(reply)
    }

    fn begin(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        if payload.is_empty() || payload.len() > MAX_PAYLOAD {
            return Err(DiagError::InvalidCommand(
                "K-Line OBD payload must contain 1..7 bytes".into(),
            ));
        }
        if !self.initialized
            || self
                .last_activity
                .is_none_or(|last| self.io.now().duration_since(last) >= REINITIALIZE_AFTER)
        {
            self.initialize()?;
        }
        if let Some(last) = self.last_activity {
            self.io
                .wait((last + self.reply_quiet).saturating_duration_since(self.io.now()));
        }
        self.io.clear(self.channel, CLEAR_RX_QUEUE)?;
        let mut data = if self.protocol == PROTOCOL_ISO14230 {
            vec![0xC0 | payload.len() as u8, 0x33, 0xF1]
        } else {
            vec![0x68, 0x6A, 0xF1]
        };
        data.extend_from_slice(payload);
        let deadline = self.io.now() + QUEUE_WAIT;
        loop {
            match self.io.queue(self.channel, self.protocol, &data) {
                Err(DiagError::PassThru {
                    code: ERR_BUFFER_FULL,
                    ..
                }) if self.io.now() < deadline => {
                    self.io.wait(Duration::from_millis(2));
                }
                result => {
                    // Queue acceptance is not bus completion; errors make the session uncertain.
                    self.last_activity = Some(self.io.now());
                    return result;
                }
            }
        }
    }

    fn next(&mut self, deadline: Instant) -> Result<Option<ResponseFrame>, DiagError> {
        while self.io.now() < deadline {
            let timeout = deadline.saturating_duration_since(self.io.now());
            if let Some(message) = self.io.read(self.channel, timeout)? {
                if let Some(frame) = decode(message, self.protocol)? {
                    self.last_activity = Some(self.io.now());
                    return Ok(Some(frame));
                }
            } else if self.io.now() < deadline {
                self.io.wait(Duration::from_millis(1));
            }
        }
        Ok(None)
    }

    fn exchange(
        &mut self,
        payload: &[u8],
        window: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        if window.is_zero() {
            return Err(DiagError::InvalidCommand(
                "Reply window must be nonzero".into(),
            ));
        }
        self.begin(payload)?;
        let deadline = self.io.now() + window;
        let mut idle_deadline = deadline;
        let mut replies = Vec::new();
        loop {
            match self.next(idle_deadline.min(deadline))? {
                Some(frame) => {
                    // Traffic from any ECU extends the quiet interval; unrelated data is not a reply.
                    idle_deadline = self.io.now() + self.reply_quiet;
                    if matches_request(payload, &frame.data) {
                        replies.push(frame);
                    }
                }
                None => {
                    if !replies.is_empty() && self.io.now() >= deadline && idle_deadline > deadline
                    {
                        return Err(DiagError::Timeout(
                            "K-Line replies did not finish within the collection window".into(),
                        ));
                    }
                    if replies.is_empty() || self.io.now() >= deadline {
                        self.initialized = false;
                    }
                    return Ok(replies);
                }
            }
        }
    }
}

fn fast_init_keys(message: RxMsg) -> Result<[u8; 2], DiagError> {
    let reply = decode(message, PROTOCOL_ISO14230)?.ok_or_else(|| {
        DiagError::InvalidResponse(
            "KWP fast initialization returned no ECU reply to this tester".into(),
        )
    })?;
    match reply.data.as_slice() {
        [0xC1, first, second] => Ok([*first, *second]),
        _ => Err(DiagError::InvalidResponse(
            "KWP fast initialization requires a StartCommunication response with two key bytes"
                .into(),
        )),
    }
}

fn matches_request(request: &[u8], reply: &[u8]) -> bool {
    if reply.first() == Some(&0x7F) {
        return reply.len() >= 3 && reply[1] == request[0];
    }
    reply.first().copied() == request[0].checked_add(0x40)
        && (request.len() < 2 || reply.get(1) == request.get(1))
}

fn decode(message: RxMsg, protocol: u32) -> Result<Option<ResponseFrame>, DiagError> {
    if message.rx_status & (RX_BREAK | RX_ERROR | BUFFER_OVERFLOW | TX_FAILED) != 0 {
        return Err(DiagError::Adapter(format!(
            "K-Line transport error, RX status 0x{:08X}",
            message.rx_status
        )));
    }
    if message.rx_status & (TX_MSG_TYPE | START_OF_MESSAGE | TX_INDICATION_SUCCESS) != 0 {
        return Ok(None);
    }
    if message.protocol_id != protocol {
        return Err(DiagError::InvalidResponse(
            "Message protocol does not match the K-Line channel".into(),
        ));
    }
    let data = message
        .data
        .get(..message.extra_data_index)
        .ok_or_else(|| DiagError::InvalidResponse("Invalid K-Line extra-data index".into()))?;
    if !(4..=3 + MAX_PAYLOAD).contains(&data.len()) {
        return Err(DiagError::InvalidResponse(
            "Truncated or oversized K-Line OBD reply".into(),
        ));
    }
    let id = u32::from_be_bytes([0, data[0], data[1], data[2]]);
    let addressing = if protocol == PROTOCOL_ISO14230 {
        Addressing::Iso14230
    } else {
        Addressing::Iso9141_2
    };
    if addressing.responder_address(id).is_none() {
        return Ok(None);
    }
    if protocol == PROTOCOL_ISO14230 && usize::from(data[0] & 0x3F) != data.len() - 3 {
        return Err(DiagError::InvalidResponse(
            "KWP2000 OBD format-byte length does not match the payload".into(),
        ));
    }
    Ok(Some(ResponseFrame {
        id,
        data: data[3..].to_vec(),
    }))
}

impl<I: KLineIo> DiagLink for KLineSession<I> {
    fn bus(&self) -> &BusSpec {
        &self.bus
    }
    fn max_payload(&self) -> usize {
        MAX_PAYLOAD
    }
    fn set_target(&mut self, address: u32) -> Result<(), DiagError> {
        self.bus.addressing().parse_ecu(address).map(|_| ())
    }
    fn send(&mut self, _: &[u8]) -> Result<(), DiagError> {
        Err(DiagError::Unsupported(
            "K-Line supports functional OBD requests; manufacturer addressing is not implemented"
                .into(),
        ))
    }
    fn receive(&mut self, _: Duration) -> Result<Option<ResponseFrame>, DiagError> {
        Err(DiagError::Unsupported(
            "Use a functional OBD exchange on K-Line".into(),
        ))
    }
    fn request_functional(
        &mut self,
        payload: &[u8],
        window: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        let result = self.exchange(payload, window);
        if result.is_err() {
            self.initialized = false;
        }
        result
    }
    fn reset(&mut self) -> Result<(), DiagError> {
        self.initialized = false;
        self.io.clear(self.channel, CLEAR_TX_QUEUE)?;
        self.io.clear(self.channel, CLEAR_RX_QUEUE)
    }
}

impl<I: KLineIo> Drop for KLineSession<I> {
    fn drop(&mut self) {
        if let Some(filter) = self.filter.take() {
            if let Err(error) = self.io.stop_filter(self.channel, filter) {
                self.io.warn(&format!("closing the K-Line filter: {error}"));
            }
        }
        if let Err(error) = self.io.disconnect(self.channel) {
            self.io
                .warn(&format!("closing the K-Line channel: {error}"));
        }
    }
}

#[cfg(test)]
mod tests;
