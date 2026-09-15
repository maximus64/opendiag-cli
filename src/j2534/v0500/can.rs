//! ISO 15765 over CAN on a J2534 v05.00 device.
//!
//! One physical `CAN` channel per session. Physical requests go through an
//! `ISO15765_LOGICAL` channel for the current target, so segmentation, flow
//! control and STmin happen in the adapter. Broadcasts go out as one raw frame
//! on the physical channel, with a pass filter for the replies; the spec
//! (§6.9) lets the physical channel do that while logical channels stay open.

use std::time::{Duration, Instant};

use crate::errors::DiagError;
use crate::events::Event;
use crate::j2534::consts::*;
use crate::j2534::v0500::device::Device;
use crate::j2534::v0500::ffi::{Iso15765ChannelDescriptor, RxMsg};
use crate::link::{BusKind, BusSpec, DiagLink, IdWidth, ResponseFrame};
use crate::obd::addressing::Addressing;

/// Largest ISO-TP payload (12-bit length field).
const MAX_ISOTP_PAYLOAD: usize = 4095;

/// How long to keep retrying a queue that's still busy with the previous
/// message before giving up.
const QUEUE_RETRY_LIMIT: Duration = Duration::from_secs(2);

/// Status bits that mark echoes and indications rather than messages.
const NOT_A_MESSAGE: u32 = TX_MSG_TYPE | START_OF_MESSAGE | TX_INDICATION_SUCCESS;

/// The logical channel for one target ECU.
struct Target {
    channel: u32,
    request: u32,
    response: u32,
}

pub struct CanSession {
    device: Device,
    bus: BusSpec,
    addressing: Addressing,
    /// `CAN_29BIT_ID` or 0, for every message and filter on this bus.
    id_flags: u32,
    physical: u32,
    target: Option<Target>,
    /// Whether an exchange may have left a reply behind on the target's queue:
    /// one that timed out, one that failed, or a broadcast the target answered
    /// too. Flushing the queue is a round trip on an adapter that speaks over
    /// a link, and a request is only a few, so `send` pays for it only when
    /// one of those has actually happened.
    stale_rx: bool,
}

fn be32(id: u32) -> [u8; 4] {
    id.to_be_bytes()
}

/// Splits a CAN-family message into its ID and payload.
fn split_id(data: &[u8]) -> Option<(u32, &[u8])> {
    let (id, rest) = data.split_first_chunk::<4>()?;
    Some((u32::from_be_bytes(*id) & 0x1FFF_FFFF, rest))
}

/// True for echoes and indications, which carry no reply.
pub(crate) fn is_indication(msg: &RxMsg) -> bool {
    msg.rx_status & NOT_A_MESSAGE != 0
}

/// "Nice to have" IOCTLs: a driver that doesn't implement them is fine.
fn tolerate_unsupported(result: Result<(), DiagError>) -> Result<(), DiagError> {
    match result {
        Err(DiagError::PassThru {
            code: ERR_NOT_SUPPORTED | ERR_IOCTL_ID_NOT_SUPPORTED | ERR_IOCTL_PARAM_ID_NOT_SUPPORTED,
            ..
        }) => Ok(()),
        other => other,
    }
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().min(u32::MAX as u128) as u32
}

impl CanSession {
    pub fn open(device: Device, bus: BusSpec) -> Result<Self, DiagError> {
        let BusKind::IsoTp { id } = bus.kind else {
            return Err(DiagError::Adapter(format!("{bus} is not a CAN bus")));
        };
        let id_flags = if id == IdWidth::Bits29 {
            CAN_29BIT_ID
        } else {
            0
        };
        let lib = &device.inner.lib;
        let physical = lib.connect(
            device.inner.id,
            PROTOCOL_CAN,
            id_flags,
            bus.bitrate,
            &bus.pins,
        )?;
        Ok(CanSession {
            addressing: Addressing::from(&bus),
            device,
            bus,
            id_flags,
            physical,
            target: None,
            stale_rx: false,
        })
    }

    fn lib(&self) -> &crate::j2534::v0500::ffi::PassThruLib {
        &self.device.inner.lib
    }

    fn close_target(&mut self) {
        if let Some(target) = self.target.take() {
            if let Err(e) = self.lib().logical_disconnect(target.channel) {
                self.device
                    .emit(Event::Warning(&format!("closing the ISO-TP channel: {e}")));
            }
        }
    }

    /// Queues one message, retrying while the previous one on the channel is
    /// still going out (`ERR_BUFFER_FULL`).
    fn queue(&self, channel: u32, protocol: u32, flags: u32, data: &[u8]) -> Result<(), DiagError> {
        let start = Instant::now();
        loop {
            match self.lib().queue(channel, protocol, flags, data) {
                Err(DiagError::PassThru {
                    code: ERR_BUFFER_FULL,
                    ..
                }) if start.elapsed() < QUEUE_RETRY_LIMIT => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                other => return other,
            }
        }
    }

    fn send_to(&mut self, request: u32, payload: &[u8]) -> Result<(), DiagError> {
        let channel = self
            .target
            .as_ref()
            .ok_or_else(|| DiagError::InvalidCommand("No target ECU set (--ecu)".into()))?
            .channel;
        if payload.is_empty() || payload.len() > MAX_ISOTP_PAYLOAD {
            return Err(DiagError::InvalidCommand(format!(
                "Request must be 1..={} bytes, got {}",
                MAX_ISOTP_PAYLOAD,
                payload.len()
            )));
        }
        let mut data = be32(request).to_vec();
        data.extend_from_slice(payload);
        // Discard anything left by a failed exchange before sending again.
        if self.stale_rx {
            tolerate_unsupported(self.lib().clear_queue(channel, CLEAR_RX_QUEUE))?;
            self.stale_rx = false;
        }
        let queued = self.queue(
            channel,
            PROTOCOL_ISO15765_LOGICAL,
            self.id_flags | ISO15765_FRAME_PAD,
            &data,
        );
        if queued.is_err() {
            self.stale_rx = true;
        }
        queued
    }

    /// Collects every single-frame reply on the physical channel until
    /// `deadline`.
    fn collect_broadcast_replies(
        &self,
        deadline: Instant,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        let mut replies = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(replies);
            }
            let Some(msg) = self.lib().read(self.physical, millis(left).max(1))? else {
                continue;
            };
            if is_indication(&msg) {
                continue;
            }
            let Some((id, frame)) = split_id(&msg.data) else {
                continue;
            };
            // A Single Frame: PCI high nibble 0, low nibble the length.
            if let Some((&pci, rest)) = frame.split_first() {
                let len = (pci & 0x0F) as usize;
                if pci >> 4 == 0 && len > 0 && rest.len() >= len {
                    replies.push(ResponseFrame {
                        id,
                        data: rest[..len].to_vec(),
                    });
                }
            }
        }
    }
}

impl DiagLink for CanSession {
    fn bus(&self) -> &BusSpec {
        &self.bus
    }

    fn max_payload(&self) -> usize {
        MAX_ISOTP_PAYLOAD
    }

    fn set_target(&mut self, request_header: u32) -> Result<(), DiagError> {
        self.addressing
            .validate_request_header(request_header)
            .map_err(DiagError::InvalidCommand)?;
        let response = self
            .addressing
            .response_for_request(request_header)
            .ok_or_else(|| {
                DiagError::InvalidCommand(format!(
                    "No response header known for request 0x{request_header:X}"
                ))
            })?;
        self.set_target_pair(request_header, response)
    }
    fn set_target_pair(&mut self, request_header: u32, response: u32) -> Result<(), DiagError> {
        let max = if self.id_flags == 0 {
            0x7FF
        } else {
            0x1FFF_FFFF
        };
        if request_header > max || response > max {
            return Err(DiagError::InvalidCommand(
                "CAN request/response headers exceed this bus's identifier width".into(),
            ));
        }
        if self
            .target
            .as_ref()
            .is_some_and(|t| t.request == request_header && t.response == response)
        {
            return Ok(());
        }
        self.close_target();

        let mut descriptor = Iso15765ChannelDescriptor {
            local_tx_flags: self.id_flags,
            remote_tx_flags: self.id_flags | ISO15765_FRAME_PAD,
            ..Default::default()
        };
        descriptor.local_address[..4].copy_from_slice(&be32(response));
        descriptor.remote_address[..4].copy_from_slice(&be32(request_header));
        let channel = self.lib().logical_connect(
            self.physical,
            PROTOCOL_ISO15765_LOGICAL,
            0,
            &mut descriptor,
        )?;
        // Our flow control: no block limit, no gap between frames.
        if let Err(error) = tolerate_unsupported(
            self.lib()
                .set_config(channel, &[(ISO15765_BS, 0), (ISO15765_STMIN, 0)]),
        ) {
            if let Err(close) = self.lib().logical_disconnect(channel) {
                self.device.emit(Event::Warning(&format!(
                    "Closing failed logical channel: {close}"
                )));
            }
            return Err(error);
        }

        self.target = Some(Target {
            channel,
            request: request_header,
            response,
        });
        // A channel this new cannot be holding anything from before it.
        self.stale_rx = false;
        Ok(())
    }

    fn send(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        let request = self
            .target
            .as_ref()
            .ok_or_else(|| DiagError::InvalidCommand("No target ECU set (--ecu)".into()))?
            .request;
        self.send_to(request, payload)
    }

    fn send_functional_to_target(&mut self, payload: &[u8]) -> Result<(), DiagError> {
        if payload.is_empty() || payload.len() > 7 {
            return Err(DiagError::InvalidCommand(
                "A functional CAN request must contain 1-7 bytes".into(),
            ));
        }
        let request = self.addressing.functional_request().unwrap();
        // J2534 permits a different destination for single-frame messages.
        // Keep the logical descriptor's physical address for flow control.
        self.send_to(request, payload)
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<ResponseFrame>, DiagError> {
        let channel = self
            .target
            .as_ref()
            .ok_or_else(|| DiagError::InvalidCommand("No target ECU set (--ecu)".into()))?
            .channel;
        let mut deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // The answer may still be on its way; whoever asks next has to
                // flush it before it is read as the reply to their request.
                self.stale_rx = true;
                return Ok(None);
            }
            let read = self.lib().read(channel, millis(left).max(1));
            let Some(msg) = read.inspect_err(|_| {
                self.stale_rx = true;
            })?
            else {
                continue;
            };
            if msg.rx_status & START_OF_MESSAGE != 0 {
                // A long reply has started arriving; give it the full timeout
                // again to finish rather than cutting it off mid-transfer.
                deadline = Instant::now() + timeout;
                continue;
            }
            if is_indication(&msg) {
                continue;
            }
            if let Some((id, payload)) = split_id(&msg.data) {
                return Ok(Some(ResponseFrame {
                    id,
                    data: payload.to_vec(),
                }));
            }
        }
    }

    fn request_functional(
        &mut self,
        payload: &[u8],
        window: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError> {
        if payload.is_empty() || payload.len() > 7 {
            return Err(DiagError::InvalidCommand(
                "A broadcast must fit one CAN frame (1-7 bytes)".into(),
            ));
        }
        let functional = self.addressing.functional_request().ok_or_else(|| {
            DiagError::InvalidCommand(format!("No functional address on {}", self.bus))
        })?;
        let (mask, pattern) = self.addressing.response_filter().ok_or_else(|| {
            DiagError::InvalidCommand(format!("No response filter for {}", self.bus))
        })?;

        // The target answers a broadcast on its own response header, so a
        // reply to this lands on the open logical channel's queue as well.
        self.stale_rx = self.target.is_some();
        let filter = self.lib().start_pass_filter(
            self.physical,
            PROTOCOL_CAN,
            self.id_flags,
            &be32(mask),
            &be32(pattern),
        )?;
        let result = (|| {
            tolerate_unsupported(self.lib().clear_queue(self.physical, CLEAR_RX_QUEUE))?;
            // ID, Single Frame PCI, payload, padded to a full 8-byte frame.
            let mut frame = be32(functional).to_vec();
            frame.push(payload.len() as u8);
            frame.extend_from_slice(payload);
            frame.resize(4 + 8, 0x00);
            self.queue(self.physical, PROTOCOL_CAN, self.id_flags, &frame)?;
            self.collect_broadcast_replies(Instant::now() + window)
        })();
        let stopped = self.lib().stop_filter(self.physical, filter);
        let replies = result?;
        stopped?;
        Ok(replies)
    }

    fn reset(&mut self) -> Result<(), DiagError> {
        let mut channels = vec![self.physical];
        if let Some(target) = &self.target {
            channels.push(target.channel);
        }
        for channel in channels {
            tolerate_unsupported(self.lib().clear_queue(channel, CLEAR_TX_QUEUE))?;
            tolerate_unsupported(self.lib().clear_queue(channel, CLEAR_RX_QUEUE))?;
        }
        self.stale_rx = false;
        Ok(())
    }
}

impl Drop for CanSession {
    fn drop(&mut self) {
        self.close_target();
        if let Err(e) = self.lib().disconnect(self.physical) {
            self.device
                .emit(Event::Warning(&format!("closing the CAN channel: {e}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_can_id_from_the_payload() {
        let data = [0x18, 0xDA, 0xF1, 0x01, 0x41, 0x00];
        assert_eq!(split_id(&data), Some((0x18DAF101, &[0x41, 0x00][..])));
        assert_eq!(split_id(&[0x07, 0xE8]), None, "too short to carry an ID");
        // A driver may leave flag bits above the 29-bit ID; they aren't part of it.
        assert_eq!(split_id(&[0x98, 0xDA, 0xF1, 0x01]).unwrap().0, 0x18DAF101);
    }

    /// The whole session through a real v05.00 driver, against a simulated ECU
    /// at 18DA10F1 (plus a second one answering broadcasts at 18DAF118).
    ///
    /// Needs `OPENDIAG_J2534_LIB` and a driver configured to reach that ECU —
    /// for OpenDiag without hardware, its host-built firmware peer with an ECU
    /// script on the fake CAN bus.
    #[test]
    #[ignore = "needs a J2534 library reaching the test ECU"]
    fn talks_iso_tp_through_a_real_driver() {
        use crate::j2534::discovery::{LibrarySource, Selection};
        use crate::link::Adapter;

        let library = std::env::var("OPENDIAG_J2534_LIB").expect("set OPENDIAG_J2534_LIB");
        let selection = Selection {
            library: library.into(),
            source: LibrarySource::Environment,
            device: None,
            driver_config: None,
            declared_protocols: Vec::new(),
        };
        let device = Device::open(&selection).unwrap();
        let mut link = device
            .connect(&BusSpec::obd_can(IdWidth::Bits29, 500_000))
            .unwrap();
        let wait = Duration::from_secs(2);

        // Broadcast: both ECUs answer, padding stripped.
        let mut replies = link
            .request_functional(&[0x01, 0x00], Duration::from_millis(500))
            .unwrap();
        replies.sort_by_key(|r| r.id);
        assert_eq!(
            replies.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![0x18DAF110, 0x18DAF118]
        );
        assert_eq!(replies[0].data, vec![0x41, 0x00, 0xBE, 0x3F, 0xA8, 0x13]);

        link.set_target(0x18DA10F1).unwrap();

        // Single frame each way.
        link.send(&[0x01, 0x00]).unwrap();
        let reply = link.receive(wait).unwrap().expect("reply to 01 00");
        assert_eq!(
            (reply.id, reply.data),
            (0x18DAF110, vec![0x41, 0x00, 0xBE, 0x3F, 0xA8, 0x13])
        );

        // Multi-frame reply: the adapter sends our flow control.
        link.send(&[0x22, 0xF1, 0x90]).unwrap();
        let reply = link.receive(wait).unwrap().expect("VIN reply");
        assert_eq!(&reply.data[..3], &[0x62, 0xF1, 0x90]);
        assert_eq!(&reply.data[3..], b"1HGCM82633A004352");

        // Multi-frame request: the adapter waits for the ECU's flow control.
        let mut request = vec![0x34, 0x00, 0x44];
        request.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        request.extend_from_slice(&0x0002_0000u32.to_be_bytes());
        link.send(&request).unwrap();
        assert_eq!(
            link.receive(wait)
                .unwrap()
                .expect("RequestDownload reply")
                .data,
            vec![0x74, 0x20, 0x01, 0x02]
        );

        // "Response pending" arrives as its own messages before the answer.
        link.send(&[0x31, 0x01, 0xFF, 0x00]).unwrap();
        assert_eq!(
            link.receive(wait).unwrap().unwrap().data,
            vec![0x7F, 0x31, 0x78]
        );
        assert_eq!(
            link.receive(wait).unwrap().unwrap().data,
            vec![0x7F, 0x31, 0x78]
        );
        assert_eq!(
            link.receive(wait).unwrap().unwrap().data,
            vec![0x71, 0x01, 0xFF, 0x00, 0x00]
        );

        // Broadcast again with the ISO-TP channel open (§6.9), then check the
        // channel still works.
        let replies = link
            .request_functional(&[0x01, 0x00], Duration::from_millis(500))
            .unwrap();
        assert_eq!(replies.len(), 2, "{replies:?}");
        link.send(&[0x3E, 0x00]).unwrap();
        assert_eq!(link.receive(wait).unwrap().unwrap().data, vec![0x7E, 0x00]);

        // Negative response passes through untouched.
        link.send(&[0x10, 0x02]).unwrap();
        assert_eq!(
            link.receive(wait).unwrap().unwrap().data,
            vec![0x7F, 0x10, 0x11]
        );

        // A reply that lands after its own request timed out must not be read
        // as the answer to the next request. `send` only flushes the queue
        // after an exchange that ended badly, so this is the case that proves
        // it still flushes when it matters.
        link.send(&[0x31, 0x01, 0xFF, 0x00]).unwrap();
        assert!(
            link.receive(Duration::from_millis(1)).unwrap().is_none(),
            "meant to time out"
        );
        link.send(&[0x3E, 0x00]).unwrap();
        assert_eq!(link.receive(wait).unwrap().unwrap().data, vec![0x7E, 0x00]);

        // An ECU that isn't there: no reply, no error.
        link.set_target(0x18DA20F1).unwrap();
        link.send(&[0x01, 0x00]).unwrap();
        assert!(link.receive(Duration::from_millis(300)).unwrap().is_none());
    }

    #[test]
    fn echoes_and_indications_are_not_replies() {
        let msg = |rx_status| RxMsg {
            protocol_id: PROTOCOL_CAN,
            rx_status,
            extra_data_index: 5,
            data: vec![0, 0, 7, 0xE8, 0x41],
        };
        assert!(!is_indication(&msg(0)));
        assert!(is_indication(&msg(TX_MSG_TYPE)));
        assert!(is_indication(&msg(START_OF_MESSAGE)));
        assert!(is_indication(&msg(TX_INDICATION_SUCCESS)));
        assert!(
            !is_indication(&msg(CAN_29BIT_ID)),
            "an ID-width bit is not an indication"
        );
    }
}
