//! Diagnostic addressing: turning a header the adapter printed into "which ECU
//! is this, and what header do I send to talk back to it".
//!
//! The request and response headers of a diagnostic exchange are not the same
//! value. On 29-bit CAN a request to ECU `0x01` goes out as `18DA01F1` (target
//! ECU, source `F1` = tester) and the reply arrives on `18DAF101` (target
//! tester, source ECU). `--ecu` sets the *request* header, while a scan sees only
//! *response* headers, so the two have to be converted into each other rather
//! than printed interchangeably.
//!
//! CAN and legacy OBD headers have separate addressing rules. Backend
//! availability is a property of `BusSpec`; header recognition here does not
//! imply that a transport or manufacturer addressing scheme is implemented.

use crate::link::{BusKind, BusSpec, IdWidth};

/// An ECU's identity is always interpreted in the context of its bus.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum EcuAddress {
    Can { request: u32, response: u32 },
    Byte(u8),
}
impl std::fmt::Display for EcuAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Can { request, .. } => write!(
                f,
                "0x{:0width$X}",
                request,
                width = if *request <= 0x7FF { 3 } else { 8 }
            ),
            Self::Byte(byte) => write!(f, "0x{byte:02X}"),
        }
    }
}

/// Which physical layer and addressing rules are in force. Derived from the
/// session's [`BusSpec`]; the bitrate plays no part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// `ATSP1` — SAE J1850 PWM, 41.6 kbps. Three-byte J2178 header.
    J1850Pwm,
    /// `ATSP2` — SAE J1850 VPW, 10.4 kbps. Three-byte J2178 header.
    J1850Vpw,
    /// `ATSP3` — ISO 9141-2. Three-byte header, `68 6A F1` requests.
    Iso9141_2,
    /// `ATSP4`/`ATSP5` — ISO 14230-4 (KWP2000). Three-byte header.
    Iso14230,
    /// `ATSP6` — ISO 15765-4 CAN, 11-bit IDs. `7E0`+n / `7E8`+n.
    Can11Bit,
    /// `ATSP7` — ISO 15765-4 CAN, 29-bit IDs. `18DAxxF1` / `18DAF1xx`.
    Can29Bit,
}

/// The tester's own address in normal fixed addressing, on every protocol that
/// uses a J2178-style address byte.
const TESTER_ADDRESS: u8 = 0xF1;

/// 29-bit physical addressing prefix: `18DA` followed by target and source.
const CAN29_PHYSICAL_PREFIX: u32 = 0x18DA_0000;

/// 29-bit functional (broadcast) request header — every legislated OBD ECU
/// answers this one.
const CAN29_FUNCTIONAL_REQUEST: u32 = 0x18DB_33F1;

/// 29-bit response prefix: `18DA`, target = tester (`F1`), then the ECU.
const CAN29_RESPONSE_PREFIX: u32 = 0x18DA_F100;

/// 11-bit functional request header (ISO 15765-4).
const CAN11_FUNCTIONAL_REQUEST: u32 = 0x7DF;

/// 11-bit physical request headers for the eight legislated OBD ECUs start
/// here; each answers 8 higher (`7E0` → `7E8`).
const CAN11_FIRST_REQUEST: u32 = 0x7E0;
const CAN11_FIRST_RESPONSE: u32 = 0x7E8;
const CAN11_RESPONSE_OFFSET: u32 = 8;

impl From<&BusSpec> for Addressing {
    fn from(bus: &BusSpec) -> Self {
        match bus.kind {
            BusKind::IsoTp {
                id: IdWidth::Bits29,
            } => Addressing::Can29Bit,
            BusKind::IsoTp {
                id: IdWidth::Bits11,
            } => Addressing::Can11Bit,
            BusKind::Iso9141 => Addressing::Iso9141_2,
            BusKind::Iso14230 { .. } => Addressing::Iso14230,
            BusKind::J1850Vpw => Addressing::J1850Vpw,
            BusKind::J1850Pwm => Addressing::J1850Pwm,
        }
    }
}

impl Addressing {
    pub fn parse_ecu(&self, value: u32) -> Result<EcuAddress, crate::DiagError> {
        match self {
            Self::Can11Bit | Self::Can29Bit => {
                self.validate_request_header(value)
                    .map_err(crate::DiagError::InvalidCommand)?;
                Ok(EcuAddress::Can {
                    request: value,
                    response: self.response_for_request(value).unwrap(),
                })
            }
            _ => u8::try_from(value).map(EcuAddress::Byte).map_err(|_| {
                crate::DiagError::InvalidCommand(format!(
                    "ECU address 0x{value:X} does not fit a byte"
                ))
            }),
        }
    }
    pub fn display_ecu(&self, ecu: &EcuAddress) -> String {
        ecu.to_string()
    }
    /// How many hex digits of a printed line belong to the header.
    ///
    /// The adapter prints headers without separators when `ATH1` is on, so this
    /// is what tells frame parsing where the header stops and data begins. It is
    /// counted in hex digits, not bytes, because 11-bit CAN headers are three
    /// digits — an odd number.
    pub fn header_hex_digits(&self) -> usize {
        match self {
            Addressing::Can29Bit => 8,
            Addressing::Can11Bit => 3,
            Addressing::J1850Pwm
            | Addressing::J1850Vpw
            | Addressing::Iso9141_2
            | Addressing::Iso14230 => 6,
        }
    }

    /// The functional request header that solicits a reply from every ECU.
    ///
    /// `None` where the protocol is not implemented yet.
    pub fn functional_request(&self) -> Option<u32> {
        match self {
            Addressing::Can29Bit => Some(CAN29_FUNCTIONAL_REQUEST),
            Addressing::Can11Bit => Some(CAN11_FUNCTIONAL_REQUEST),
            Addressing::Iso9141_2 | Addressing::J1850Vpw => Some(0x686AF1),
            Addressing::J1850Pwm => Some(0x616AF1),
            Addressing::Iso14230 => Some(0xC033F1),
        }
    }

    /// The (mask, pattern) pair that admits every physical reply to this
    /// tester — what a broadcast listens for.
    pub fn response_filter(&self) -> Option<(u32, u32)> {
        match self {
            Addressing::Can29Bit => Some((0xFFFF_FF00, CAN29_RESPONSE_PREFIX)),
            Addressing::Can11Bit => Some((0x7F8, CAN11_FIRST_RESPONSE)),
            Addressing::Iso9141_2 | Addressing::J1850Vpw => Some((0xFFFF00, 0x486B00)),
            Addressing::J1850Pwm => Some((0xFFFF00, 0x416B00)),
            Addressing::Iso14230 => Some((0xC0FF00, 0x80F100)),
        }
    }

    /// The header replies to `request` arrive on.
    ///
    /// 29-bit: target and source swap (`18DA01F1` → `18DAF101`). 11-bit: the
    /// reply is 8 higher (`7E0` → `7E8`); ISO 15765-4 fixes that for the OBD
    /// range and most manufacturers follow it elsewhere.
    pub fn response_for_request(&self, request: u32) -> Option<u32> {
        match self {
            Addressing::Can29Bit => {
                let source = request & 0xFF;
                let target = (request >> 8) & 0xFF;
                if request & 0xFFFF_0000 != CAN29_PHYSICAL_PREFIX || source != TESTER_ADDRESS as u32
                {
                    return None;
                }
                Some(CAN29_RESPONSE_PREFIX | target)
            }
            Addressing::Can11Bit if request + CAN11_RESPONSE_OFFSET <= 0x7FF => {
                Some(request + CAN11_RESPONSE_OFFSET)
            }
            _ => None,
        }
    }

    /// Checks that a request header fits this protocol, so a 29-bit `--ecu`
    /// isn't silently truncated on an 11-bit bus.
    pub fn validate_request_header(&self, header: u32) -> Result<(), String> {
        let max = match self {
            Addressing::Can29Bit => 0x1FFF_FFFF,
            Addressing::Can11Bit => 0x7FF,
            _ => 0xFF_FFFF,
        };
        if header > max {
            return Err(format!(
                "--ecu 0x{header:X} doesn't fit {self:?} (maximum 0x{max:X})"
            ));
        }
        if self.response_for_request(header).is_none()
            && matches!(self, Addressing::Can29Bit | Addressing::Can11Bit)
        {
            return Err(format!(
                "--ecu 0x{:X} isn't a physical request header for {:?}; expected e.g. 0x{}",
                header,
                self,
                self.format_header(self.physical_request(0x00).unwrap_or(0))
            ));
        }
        Ok(())
    }

    /// The ECU address that sent a frame, given the header it arrived on.
    ///
    /// `None` if the protocol is not implemented, or if the header is not a
    /// response to this tester — a header that does not fit the scheme cannot be
    /// turned into an address, and guessing one would produce a request header
    /// aimed at nothing.
    pub fn responder_address(&self, header: u32) -> Option<u8> {
        match self {
            Addressing::Can11Bit => (CAN11_FIRST_RESPONSE..CAN11_FIRST_RESPONSE + 8)
                .contains(&header)
                .then(|| (header - CAN11_FIRST_RESPONSE) as u8),
            Addressing::Can29Bit => {
                let target = ((header >> 8) & 0xFF) as u8;
                if header & 0xFFFF_0000 != CAN29_PHYSICAL_PREFIX || target != TESTER_ADDRESS {
                    return None;
                }
                Some((header & 0xFF) as u8)
            }
            _ => {
                let (mask, pattern) = self.response_filter()?;
                if header <= 0xFFFFFF && header & mask == pattern && header as u8 != TESTER_ADDRESS
                {
                    Some(header as u8)
                } else {
                    None
                }
            }
        }
    }

    /// The physical request header that addresses `ecu` — the value to pass to
    /// `--ecu`.
    pub fn physical_request(&self, ecu: u8) -> Option<u32> {
        match self {
            Addressing::Can11Bit if ecu < 8 => Some(CAN11_FIRST_REQUEST + ecu as u32),
            Addressing::Can29Bit => {
                Some(CAN29_PHYSICAL_PREFIX | ((ecu as u32) << 8) | TESTER_ADDRESS as u32)
            }
            _ => None,
        }
    }

    /// Render a header the way the protocol writes it, zero-padded to its own
    /// width so a 29-bit CAN ID does not print like a three-byte K-Line header.
    pub fn format_header(&self, header: u32) -> String {
        format!("{:0width$X}", header, width = self.header_hex_digits())
    }
}

/// What an ECU at this address conventionally is.
///
/// There is no standard that binds an address to a function. Addresses
/// `0x00`–`0x07` are the eight legislated OBD ECUs of ISO 15765-4 (the 29-bit
/// counterparts of 11-bit `0x7E0`–`0x7E7`), and that *positioning* is standard,
/// but which one is the engine is convention. The higher addresses are the
/// SAE J2178 assignments that manufacturers mostly follow and sometimes do not.
///
/// Callers must present the result as a guess. `None` means "no idea", which is
/// a better answer than a plausible wrong one.
pub fn describe_address(address: u8) -> Option<&'static str> {
    match address {
        0x00 => Some("OBD ECU #1 (commonly engine)"),
        0x01 => Some("OBD ECU #2 (commonly transmission)"),
        0x02..=0x07 => Some("legislated OBD ECU"),
        0x10 => Some("engine"),
        0x17 => Some("transmission"),
        0x18 => Some("transmission"),
        0x28 => Some("ABS / brakes"),
        0x40 => Some("body controller"),
        0x58 => Some("restraints / airbag"),
        0x60 => Some("instrument cluster"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can29_response_header_yields_the_request_header_to_talk_back_on() {
        // The exact confusion this module exists for: a scan sees 18DAF101 and
        // the user has to hand --ecu 18DA01F1.
        let addr = Addressing::Can29Bit.responder_address(0x18DAF101).unwrap();

        assert_eq!(addr, 0x01);
        assert_eq!(
            Addressing::Can29Bit.physical_request(addr),
            Some(0x18DA01F1)
        );
    }

    #[test]
    fn can29_round_trips_every_address() {
        for ecu in 0..=0xFFu8 {
            let request = Addressing::Can29Bit.physical_request(ecu).unwrap();
            let response = 0x18DAF100 | ecu as u32;

            assert_eq!(request, 0x18DA00F1 | ((ecu as u32) << 8));
            assert_eq!(Addressing::Can29Bit.responder_address(response), Some(ecu));
        }
    }

    /// A header that is not a physical reply to this tester has no address to
    /// extract. Deriving one anyway would print a `--ecu` aimed at nothing.
    #[test]
    fn can29_rejects_headers_that_are_not_replies_to_the_tester() {
        for header in [
            0x18DB33F1,  // functional request, not a response
            0x18DA01F1,  // a request header, tester and ECU the other way round
            0x18DE_F101, // right shape, wrong prefix
            0x000007E8,  // an 11-bit CAN response
        ] {
            assert_eq!(
                Addressing::Can29Bit.responder_address(header),
                None,
                "header {header:08X} should not resolve to an ECU address"
            );
        }
    }

    /// Until a protocol is implemented it must decline, not fall through to CAN
    /// arithmetic on a header that is not CAN.
    #[test]
    fn legacy_addressing_is_available_at_message_level() {
        for addressing in [
            Addressing::J1850Pwm,
            Addressing::J1850Vpw,
            Addressing::Iso9141_2,
            Addressing::Iso14230,
        ] {
            assert!(addressing.functional_request().is_some(), "{addressing:?}");
            let expected = match addressing {
                Addressing::J1850Vpw | Addressing::Iso9141_2 => Some(0x10),
                _ => None,
            };
            assert_eq!(
                addressing.responder_address(0x486B10),
                expected,
                "{addressing:?}"
            );
            assert_eq!(addressing.physical_request(0x10), None, "{addressing:?}");
        }
    }

    #[test]
    fn can11_follows_iso_15765_4() {
        let a = Addressing::Can11Bit;
        assert_eq!(a.functional_request(), Some(0x7DF));
        assert_eq!(a.responder_address(0x7E8), Some(0));
        assert_eq!(a.responder_address(0x7EF), Some(7));
        assert_eq!(a.responder_address(0x7DF), None, "functional, not a reply");
        assert_eq!(a.responder_address(0x18DAF101), None, "a 29-bit header");
        assert_eq!(a.physical_request(1), Some(0x7E1));
        assert_eq!(a.physical_request(8), None, "only eight legislated ECUs");
        assert_eq!(a.response_for_request(0x7E0), Some(0x7E8));
        assert_eq!(
            a.response_for_request(0x726),
            Some(0x72E),
            "manufacturer IDs follow +8 too"
        );
        assert_eq!(a.response_filter(), Some((0x7F8, 0x7E8)));
    }

    #[test]
    fn can29_response_is_target_and_source_swapped() {
        let a = Addressing::Can29Bit;
        assert_eq!(a.response_for_request(0x18DA01F1), Some(0x18DAF101));
        assert_eq!(a.response_for_request(0x18DA10F1), Some(0x18DAF110));
        assert_eq!(
            a.response_for_request(0x18DB33F1),
            None,
            "functional has no single reply"
        );
        assert_eq!(
            a.response_for_request(0x18DAF101),
            None,
            "a reply, not a request"
        );
        assert_eq!(a.response_filter(), Some((0xFFFF_FF00, 0x18DAF100)));
    }

    #[test]
    fn request_headers_are_checked_against_the_bus() {
        assert!(Addressing::Can29Bit
            .validate_request_header(0x18DA01F1)
            .is_ok());
        assert!(Addressing::Can11Bit.validate_request_header(0x7E0).is_ok());
        let err = Addressing::Can11Bit
            .validate_request_header(0x18DA01F1)
            .unwrap_err();
        assert!(err.contains("doesn't fit"), "{err}");
        let err = Addressing::Can29Bit
            .validate_request_header(0x7E0)
            .unwrap_err();
        assert!(err.contains("18DA00F1"), "suggests the right shape: {err}");
    }

    #[test]
    fn headers_print_at_their_own_protocols_width() {
        assert_eq!(Addressing::Can29Bit.format_header(0x18DAF101), "18DAF101");
        assert_eq!(Addressing::Can11Bit.format_header(0x7E8), "7E8");
        assert_eq!(Addressing::Iso9141_2.format_header(0x486B10), "486B10");
    }

    #[test]
    fn unknown_addresses_have_no_description() {
        assert_eq!(describe_address(0x00), Some("OBD ECU #1 (commonly engine)"));
        assert_eq!(describe_address(0x10), Some("engine"));
        assert_eq!(describe_address(0x9C), None);
    }
}
