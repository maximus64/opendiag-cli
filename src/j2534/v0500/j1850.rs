//! J1850 PWM and VPW OBD. The adapter owns arbitration, CRC and in-frame acknowledgments.

use std::time::{Duration, Instant};

use super::{device::Device, ffi::RxMsg};
use crate::{
    events::Event,
    j2534::consts::*,
    link::{BusKind, BusSpec, DiagLink, ResponseFrame},
    DiagError,
};

const REPLY_QUIET: Duration = Duration::from_millis(200);
const QUEUE_WAIT: Duration = Duration::from_secs(2);

fn protocol_id(kind: BusKind) -> u32 {
    match kind {
        BusKind::J1850Pwm => PROTOCOL_J1850PWM,
        BusKind::J1850Vpw => PROTOCOL_J1850VPW,
        _ => unreachable!("J1850Session validates the bus kind before using it"),
    }
}

fn max_payload(kind: BusKind) -> usize {
    // Standard OBD allows seven data bytes on PWM and eight on VPW.
    if kind == BusKind::J1850Pwm {
        7
    } else {
        8
    }
}

pub(crate) trait J1850Io {
    fn connect(&self, bus: &BusSpec) -> Result<u32, DiagError>;
    fn configure(&self, channel: u32, params: &[(u32, u32)]) -> Result<(), DiagError>;
    fn functional_addresses(&self, channel: u32, addresses: &[u8]) -> Result<(), DiagError>;
    fn filter(&self, channel: u32, bus: &BusSpec) -> Result<u32, DiagError>;
    fn stop_filter(&self, channel: u32, filter: u32) -> Result<(), DiagError>;
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

impl J1850Io for Device {
    fn connect(&self, bus: &BusSpec) -> Result<u32, DiagError> {
        self.inner.lib.connect(
            self.inner.id,
            protocol_id(bus.kind),
            0,
            bus.bitrate,
            &bus.pins,
        )
    }
    fn configure(&self, channel: u32, params: &[(u32, u32)]) -> Result<(), DiagError> {
        self.inner.lib.set_config(channel, params)
    }
    fn functional_addresses(&self, channel: u32, addresses: &[u8]) -> Result<(), DiagError> {
        self.inner.lib.set_functional_addresses(channel, addresses)
    }
    fn filter(&self, channel: u32, bus: &BusSpec) -> Result<u32, DiagError> {
        let (mask, pattern) = bus.addressing().response_filter().unwrap();
        self.inner.lib.start_pass_filter(
            channel,
            protocol_id(bus.kind),
            0,
            &mask.to_be_bytes()[1..],
            &pattern.to_be_bytes()[1..],
        )
    }

    fn stop_filter(&self, channel: u32, filter: u32) -> Result<(), DiagError> {
        self.inner.lib.stop_filter(channel, filter)
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

pub(crate) struct J1850Session<I: J1850Io> {
    io: I,
    bus: BusSpec,
    channel: u32,
    filter: Option<u32>,
    recover: bool,
}

impl<I: J1850Io> J1850Session<I> {
    pub(crate) fn open(io: I, bus: BusSpec) -> Result<Self, DiagError> {
        let expected = match bus.kind {
            BusKind::J1850Pwm | BusKind::J1850Vpw => BusSpec::standard(bus.kind),
            _ => {
                return Err(DiagError::InvalidCommand(
                    "Expected a J1850 PWM or VPW bus".into(),
                ))
            }
        };
        if bus.bitrate != expected.bitrate || bus.pins != expected.pins {
            return Err(DiagError::InvalidCommand(format!(
                "J1850 OBD requires {} on pins {:?}",
                expected, expected.pins,
            )));
        }
        if bus.init_address.is_some() || !bus.timing.is_empty() {
            return Err(DiagError::Unsupported(
                "K-Line initialization and timing overrides do not apply to J1850".into(),
            ));
        }
        let channel = io.connect(&bus)?;
        // Own the channel before setup so any failure also disconnects it.
        let mut session = Self {
            io,
            bus,
            channel,
            filter: None,
            recover: false,
        };
        if session.bus.kind == BusKind::J1850Pwm {
            session
                .io
                .configure(channel, &[(NODE_ADDRESS, 0xF1), (NETWORK_LINE, 0)])?;
            session.io.functional_addresses(channel, &[0x6B])?;
        }
        session.filter = Some(session.io.filter(channel, &session.bus)?);
        session.io.clear(channel, CLEAR_TX_QUEUE)?;
        session.io.clear(channel, CLEAR_RX_QUEUE)?;
        Ok(session)
    }

    fn begin(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        if payload.is_empty() || payload.len() > self.max_payload() {
            return Err(DiagError::InvalidCommand(format!(
                "J1850 OBD payload must contain 1..{} bytes",
                self.max_payload()
            )));
        }
        if self.recover {
            self.io.clear(self.channel, CLEAR_TX_QUEUE)?;
            // A failed exchange may still have a late response in flight.
            self.io.wait(REPLY_QUIET);
        }
        self.io.clear(self.channel, CLEAR_RX_QUEUE)?;
        self.recover = false;
        let header = self.bus.addressing().functional_request().unwrap();
        let mut data = header.to_be_bytes()[1..].to_vec();
        data.extend_from_slice(payload);
        let deadline = self.io.now() + QUEUE_WAIT;
        loop {
            match self
                .io
                .queue(self.channel, protocol_id(self.bus.kind), &data)
            {
                Err(DiagError::PassThru {
                    code: ERR_BUFFER_FULL,
                    ..
                }) if self.io.now() < deadline => {
                    self.io.wait(Duration::from_millis(2));
                }
                result => return result,
            }
        }
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
        let mut quiet_deadline = deadline;
        let mut replies = Vec::new();
        loop {
            let until = quiet_deadline.min(deadline);
            if self.io.now() >= until {
                if !replies.is_empty() && self.io.now() >= deadline && quiet_deadline > deadline {
                    return Err(DiagError::Timeout(
                        "J1850 replies did not finish within the collection window".into(),
                    ));
                }
                return Ok(replies);
            }
            let timeout = until.saturating_duration_since(self.io.now());
            match self.io.read(self.channel, timeout)? {
                Some(message) => {
                    if let Some(frame) = decode(self.bus.kind, message)? {
                        quiet_deadline = self.io.now() + REPLY_QUIET;
                        if matches_request(payload, &frame.data) {
                            replies.push(frame);
                        }
                    }
                }
                None if self.io.now() < until => self.io.wait(Duration::from_millis(1)),
                None => {}
            }
        }
    }
}

fn matches_request(request: &[u8], reply: &[u8]) -> bool {
    if reply.first() == Some(&0x7F) {
        return reply.len() >= 3 && reply[1] == request[0];
    }
    reply.first().copied() == request[0].checked_add(0x40)
        && (request.len() < 2 || reply.get(1) == request.get(1))
}

fn decode(kind: BusKind, message: RxMsg) -> Result<Option<ResponseFrame>, DiagError> {
    if message.rx_status & (RX_BREAK | RX_ERROR | BUFFER_OVERFLOW | TX_FAILED) != 0 {
        return Err(DiagError::Adapter(format!(
            "J1850 transport error, RX status 0x{:08X}",
            message.rx_status
        )));
    }
    if message.rx_status & (TX_MSG_TYPE | START_OF_MESSAGE | TX_INDICATION_SUCCESS) != 0 {
        return Ok(None);
    }
    if message.protocol_id != protocol_id(kind) {
        return Err(DiagError::InvalidResponse(
            "Wrong protocol on the J1850 channel".into(),
        ));
    }
    let data = message
        .data
        .get(..message.extra_data_index)
        .ok_or_else(|| DiagError::InvalidResponse("Invalid J1850 extra-data index".into()))?;
    if !(4..=3 + max_payload(kind)).contains(&data.len()) {
        return Err(DiagError::InvalidResponse(
            "Truncated or oversized J1850 OBD reply".into(),
        ));
    }
    let id = u32::from_be_bytes([0, data[0], data[1], data[2]]);
    if BusSpec::standard(kind)
        .addressing()
        .responder_address(id)
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(ResponseFrame {
        id,
        data: data[3..].to_vec(),
    }))
}

impl<I: J1850Io> DiagLink for J1850Session<I> {
    fn bus(&self) -> &BusSpec {
        &self.bus
    }
    fn max_payload(&self) -> usize {
        max_payload(self.bus.kind)
    }
    fn set_target(&mut self, address: u32) -> Result<(), DiagError> {
        self.bus.addressing().parse_ecu(address).map(|_| ())
    }
    fn send(&mut self, _: &[u8]) -> Result<(), DiagError> {
        Err(DiagError::Unsupported(
            "J1850 supports functional OBD requests; manufacturer addressing is not implemented"
                .into(),
        ))
    }
    fn receive(&mut self, _: Duration) -> Result<Option<ResponseFrame>, DiagError> {
        Err(DiagError::Unsupported(
            "Use a functional OBD exchange on J1850".into(),
        ))
    }
    fn request_functional(
        &mut self,
        payload: &[u8],
        window: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        let result = self.exchange(payload, window);
        self.recover = result.as_ref().map_or(true, Vec::is_empty);
        result
    }
    fn reset(&mut self) -> Result<(), DiagError> {
        self.recover = true;
        self.io.clear(self.channel, CLEAR_TX_QUEUE)?;
        self.io.clear(self.channel, CLEAR_RX_QUEUE)
    }
}

impl<I: J1850Io> Drop for J1850Session<I> {
    fn drop(&mut self) {
        if let Some(filter) = self.filter.take() {
            if let Err(error) = self.io.stop_filter(self.channel, filter) {
                self.io.warn(&format!("closing the J1850 filter: {error}"));
            }
        }
        if let Err(error) = self.io.disconnect(self.channel) {
            self.io.warn(&format!("closing the J1850 channel: {error}"));
        }
    }
}

#[cfg(test)]
mod tests;
