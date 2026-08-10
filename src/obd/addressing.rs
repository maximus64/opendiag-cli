//! Diagnostic addressing: turning a header the adapter printed into "which ECU
//! is this, and what header do I send to talk back to it".
//!
//! The request and response headers of a diagnostic exchange are not the same
//! value. On 29-bit CAN a request to ECU `0x01` goes out as `18DA01F1` (target
//! ECU, source `F1` = tester) and the reply arrives on `18DAF101` (target
//! tester, source ECU). `--id` sets the *request* header, while a scan sees only
//! *response* headers, so the two have to be converted into each other rather
//! than printed interchangeably.
//!
//! Only 29-bit CAN is implemented — that is the one protocol `reset()` selects
//! (`ATSP7`). The other variants exist so that K-Line and J1850 support is a
//! matter of filling in match arms the compiler already points at, and so that
//! nothing in the meantime silently applies CAN arithmetic to a header that is
//! not CAN: every method returns `None` for a protocol that is not wired up yet.

/// Which physical layer and addressing rules are in force.
///
/// The variants line up with the ELM327-style `ATSP` protocol numbers, so this
/// can be derived from whichever `ATSP` the session issued.
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

impl Addressing {
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
            _ => None,
        }
    }

    /// The ECU address that sent a frame, given the header it arrived on.
    ///
    /// `None` if the protocol is not implemented, or if the header is not a
    /// response to this tester — a header that does not fit the scheme cannot be
    /// turned into an address, and guessing one would produce a request header
    /// aimed at nothing.
    pub fn responder_address(&self, header: u32) -> Option<u8> {
        match self {
            Addressing::Can29Bit => {
                let target = ((header >> 8) & 0xFF) as u8;
                if header & 0xFFFF_0000 != CAN29_PHYSICAL_PREFIX || target != TESTER_ADDRESS {
                    return None;
                }
                Some((header & 0xFF) as u8)
            }
            _ => None,
        }
    }

    /// The physical request header that addresses `ecu` — the value to pass to
    /// `--id`, and what `ATSH` is given.
    pub fn physical_request(&self, ecu: u8) -> Option<u32> {
        match self {
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
        // the user has to hand --id 18DA01F1.
        let addr = Addressing::Can29Bit.responder_address(0x18DAF101).unwrap();

        assert_eq!(addr, 0x01);
        assert_eq!(Addressing::Can29Bit.physical_request(addr), Some(0x18DA01F1));
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
    /// extract. Deriving one anyway would print a `--id` aimed at nothing.
    #[test]
    fn can29_rejects_headers_that_are_not_replies_to_the_tester() {
        for header in [
            0x18DB33F1, // functional request, not a response
            0x18DA01F1, // a request header, tester and ECU the other way round
            0x18DE_F101, // right shape, wrong prefix
            0x000007E8, // an 11-bit CAN response
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
    fn unimplemented_protocols_decline_rather_than_guess() {
        for addressing in [
            Addressing::J1850Pwm,
            Addressing::J1850Vpw,
            Addressing::Iso9141_2,
            Addressing::Iso14230,
            Addressing::Can11Bit,
        ] {
            assert_eq!(addressing.functional_request(), None, "{addressing:?}");
            assert_eq!(addressing.responder_address(0x486B10), None, "{addressing:?}");
            assert_eq!(addressing.physical_request(0x10), None, "{addressing:?}");
        }
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
