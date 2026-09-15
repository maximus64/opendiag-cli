//! The boundary between diagnostics and the adapter.
//!
//! Everything above this module (`ObdProtocol`, UDS, flashing, the CLI) works
//! in whole diagnostic messages on a [`BusSpec`]. Everything below it (the J2534
//! backend today, others later) implements [`Adapter`], [`DiagLink`] and
//! [`DeviceOps`]. Nothing above knows which API version or vendor is in use, and
//! nothing below knows about vehicles.

use std::fmt;
use std::time::Duration;

use crate::errors::DiagError;
use crate::obd::addressing::{Addressing, EcuAddress};

/// One diagnostic message from the bus: who sent it and what it said.
///
/// `id` is the header it arrived on — a CAN ID, or (on K-Line/J1850) a
/// 3-byte header in the low 24 bits. `data` is the payload with framing,
/// padding and checksums already removed.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ResponseFrame {
    pub id: u32,
    pub data: Vec<u8>,
}

/// CAN identifier width.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum IdWidth {
    Bits11,
    Bits29,
}

/// How an ISO 14230 (KWP2000) session is initialised.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum KwpInit {
    Fast,
    FiveBaud,
}

/// Which kind of vehicle bus, and its addressing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum BusKind {
    /// ISO 15765 (ISO-TP) over classic CAN.
    IsoTp {
        id: IdWidth,
    },
    Iso9141,
    Iso14230 {
        init: KwpInit,
    },
    J1850Vpw,
    J1850Pwm,
}

/// Everything needed to open a vehicle bus.
///
/// Built by the CLI, auto-detection or a vehicle profile — never hard-coded in a
/// session. A car-maker bus at a non-OBD bitrate is just another value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct BusSpec {
    pub kind: BusKind,
    pub bitrate: u32,
    /// J1962 connector pins.
    pub pins: Vec<u32>,
    pub init_address: Option<u8>,
    /// Adapter timing overrides. K-Line P values use half-millisecond units.
    pub timing: std::collections::BTreeMap<String, u32>,
}

impl BusSpec {
    /// OBD-II CAN (ISO 15765-4) on pins 6/14.
    pub fn obd_can(id: IdWidth, bitrate: u32) -> Self {
        BusSpec {
            kind: BusKind::IsoTp { id },
            bitrate,
            pins: vec![6, 14],
            init_address: None,
            timing: Default::default(),
        }
    }

    /// The standard bitrate and pins for a bus kind.
    pub fn standard(kind: BusKind) -> Self {
        let (bitrate, pins) = match kind {
            BusKind::IsoTp { id } => return Self::obd_can(id, 500_000),
            BusKind::Iso9141 | BusKind::Iso14230 { .. } => (10_400, vec![7]),
            BusKind::J1850Vpw => (10_400, vec![2]),
            BusKind::J1850Pwm => (41_600, vec![2, 10]),
        };
        BusSpec {
            kind,
            bitrate,
            pins,
            init_address: if matches!(kind, BusKind::Iso9141 | BusKind::Iso14230 { .. }) {
                Some(0x33)
            } else {
                None
            },
            timing: Default::default(),
        }
    }

    /// The `--protocol` names, in the order they're listed.
    pub const PROTOCOL_NAMES: &'static [&'static str] = &[
        "can11",
        "can29",
        "iso9141",
        "kwp-fast",
        "kwp-5baud",
        "j1850vpw",
        "j1850pwm",
    ];

    /// Builds a spec from `--protocol` and an optional `--bitrate`.
    pub fn from_cli(protocol: &str, bitrate: Option<u32>) -> Result<Self, String> {
        let kind = match protocol.to_ascii_lowercase().as_str() {
            "can11" => BusKind::IsoTp {
                id: IdWidth::Bits11,
            },
            "can29" => BusKind::IsoTp {
                id: IdWidth::Bits29,
            },
            "iso9141" => BusKind::Iso9141,
            "kwp-fast" => BusKind::Iso14230 {
                init: KwpInit::Fast,
            },
            "kwp-5baud" => BusKind::Iso14230 {
                init: KwpInit::FiveBaud,
            },
            "j1850vpw" => BusKind::J1850Vpw,
            "j1850pwm" => BusKind::J1850Pwm,
            other => {
                return Err(format!(
                    "Unknown protocol `{}` (expected one of: {})",
                    other,
                    Self::PROTOCOL_NAMES.join(", ")
                ))
            }
        };
        let mut spec = Self::standard(kind);
        if let Some(bitrate) = bitrate {
            if bitrate == 0 {
                return Err("--bitrate must be greater than zero".into());
            }
            spec.bitrate = bitrate;
        }
        Ok(spec)
    }

    pub fn addressing(&self) -> Addressing {
        Addressing::from(self)
    }

    pub fn is_can(&self) -> bool {
        matches!(self.kind, BusKind::IsoTp { .. })
    }

    /// Whether this build has an adapter backend for this bus.
    pub fn is_implemented(&self) -> bool {
        matches!(
            self.kind,
            BusKind::IsoTp { .. }
                | BusKind::Iso9141
                | BusKind::Iso14230 { .. }
                | BusKind::J1850Pwm
                | BusKind::J1850Vpw
        )
    }
}

impl fmt::Display for BusSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.kind {
            BusKind::IsoTp {
                id: IdWidth::Bits11,
            } => "CAN 11-bit",
            BusKind::IsoTp {
                id: IdWidth::Bits29,
            } => "CAN 29-bit",
            BusKind::Iso9141 => "ISO 9141-2",
            BusKind::Iso14230 {
                init: KwpInit::Fast,
            } => "ISO 14230-4 (fast init)",
            BusKind::Iso14230 {
                init: KwpInit::FiveBaud,
            } => "ISO 14230-4 (5-baud init)",
            BusKind::J1850Vpw => "SAE J1850 VPW",
            BusKind::J1850Pwm => "SAE J1850 PWM",
        };
        write!(f, "{} at {} bit/s", name, self.bitrate)
    }
}

/// Adapter-level operations. J2534 takes a device handle for these, not a
/// channel, which is why they can run before any bus is open.
pub trait DeviceOps {
    /// Programming voltage in mV on a J1962 pin, or the off/ground sentinels
    /// (`VOLTAGE_PIN_OFF`, `VOLTAGE_SHORT_TO_GROUND`).
    fn set_programming_voltage(&self, pin: u32, millivolts: u32) -> Result<(), DiagError>;
    /// Vehicle battery voltage (J1962 pin 16), in mV.
    fn read_battery_mv(&self) -> Result<u32, DiagError>;
    /// The programming voltage the adapter actually measures, in mV.
    fn read_programming_voltage_mv(&self) -> Result<u32, DiagError>;
}

/// An open adapter that can bring up vehicle buses. This is the only way a
/// bus gets opened: the CLI, auto-detection and multi-bus vehicles all go
/// through [`Adapter::connect`].
pub trait Adapter: DeviceOps {
    /// `Some(false)` when the adapter definitely can't open this bus, `None`
    /// when it doesn't say.
    fn supports(&self, bus: &BusSpec) -> Option<bool>;
    fn connect(&self, bus: &BusSpec) -> Result<Box<dyn DiagLink>, DiagError>;
}

/// Traffic on one open bus, in whole messages.
pub trait DiagLink {
    fn bus(&self) -> &BusSpec;
    /// Largest request payload one message can carry.
    fn max_payload(&self) -> usize;
    /// The physical request header for subsequent [`DiagLink::send`]s (what
    /// `--ecu` names).
    fn set_target(&mut self, request_header: u32) -> Result<(), DiagError>;
    fn set_target_pair(&mut self, request: u32, response: u32) -> Result<(), DiagError> {
        if self.bus().addressing().response_for_request(request) != Some(response) {
            return Err(DiagError::Unsupported(
                "This link does not support a custom response header".into(),
            ));
        }
        self.set_target(request)
    }
    fn set_ecu(&mut self, ecu: EcuAddress) -> Result<(), DiagError> {
        match ecu {
            EcuAddress::Can { request, response } => self.set_target_pair(request, response),
            EcuAddress::Byte(address) => self.set_target(address as u32),
        }
    }
    /// Sends one complete request to the current target.
    fn send(&mut self, payload: &[u8]) -> Result<(), DiagError>;
    /// Sends a single-frame functional request while retaining the current
    /// target's reply filter and physical flow-control address. Replies,
    /// including segmented ones, are collected with `receive`.
    fn send_functional_to_target(&mut self, _payload: &[u8]) -> Result<(), DiagError> {
        Err(DiagError::Unsupported(
            "This link cannot receive targeted replies to a functional request".into(),
        ))
    }
    /// The next complete message from the current target, or `None` if
    /// nothing arrived within `timeout`. Separate from `send` because one
    /// request can bring several replies ("response pending", or multi-message
    /// answers on K-Line).
    fn receive(&mut self, timeout: Duration) -> Result<Option<ResponseFrame>, DiagError>;
    /// A functional (broadcast) request; every reply within `window`.
    fn request_functional(
        &mut self,
        payload: &[u8],
        window: Duration,
    ) -> Result<Vec<ResponseFrame>, DiagError>;
    /// Flushes queued traffic.
    fn reset(&mut self) -> Result<(), DiagError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_protocols_use_the_obd_defaults() {
        assert_eq!(
            BusSpec::from_cli("can29", None).unwrap(),
            BusSpec::obd_can(IdWidth::Bits29, 500_000)
        );
        assert_eq!(
            BusSpec::from_cli("CAN11", Some(250_000)).unwrap(),
            BusSpec::obd_can(IdWidth::Bits11, 250_000)
        );

        let kwp = BusSpec::from_cli("kwp-fast", None).unwrap();
        assert_eq!((kwp.bitrate, kwp.pins), (10_400, vec![7]));
        assert_eq!(
            BusSpec::from_cli("j1850pwm", None).unwrap().pins,
            vec![2, 10]
        );
    }

    #[test]
    fn every_listed_protocol_name_parses() {
        for name in BusSpec::PROTOCOL_NAMES {
            assert!(BusSpec::from_cli(name, None).is_ok(), "{name}");
        }
    }

    #[test]
    fn rejects_unknown_protocols_and_zero_bitrates() {
        assert!(BusSpec::from_cli("flexray", None)
            .unwrap_err()
            .contains("can11"));
        assert!(BusSpec::from_cli("can29", Some(0)).is_err());
    }

    #[test]
    fn addressing_follows_the_bus_not_the_bitrate() {
        assert_eq!(
            BusSpec::obd_can(IdWidth::Bits29, 250_000).addressing(),
            Addressing::Can29Bit
        );
        assert_eq!(
            BusSpec::obd_can(IdWidth::Bits11, 500_000).addressing(),
            Addressing::Can11Bit
        );
        assert_eq!(
            BusSpec::from_cli("kwp-5baud", None).unwrap().addressing(),
            Addressing::Iso14230
        );
    }

    #[test]
    fn displays_kind_and_bitrate() {
        assert_eq!(
            BusSpec::obd_can(IdWidth::Bits29, 500_000).to_string(),
            "CAN 29-bit at 500000 bit/s"
        );
    }
}
