//! Honda engine ECUs that keep their persistent data in serial EEPROM.
//!
//! Most of Honda SH2 CAN bus ECU stores its VIN, immobilizer pairing,
//! mileage and learned values in GPIO-driven serial EEPROM outside the
//! processor's address map. Only the proprietary "BA" service reaches it,
//! one 16-bit word at a time, after Honda security access level 0x61 and
//! vendor session 0x60:
//!
//! ```text
//! read : BA 11 00 00 <channel> <word>            -> FA 11 11 07 <lo> <hi> <sum>
//! write: BA 15 00 00 <channel> <word> <lo> <hi>  -> FA 15 15 <len> ... <sum>
//! ```
//!
//! `<len>` is the whole reply's length and `<sum>` makes the reply's bytes sum
//! to zero. The reply does not echo the channel or word, so each request waits
//! for its own reply and every write is read back. The erase-all service
//! (`BA 18`) is never sent.
//!
//! The ECU grants security access only shortly after it starts. Unlocking first
//! asks it to restart (`11 01`, hard reset). If it refuses, or still refuses
//! the seed afterwards, the user is asked to power-cycle it instead. A key is
//! only sent for a granted seed; it is always computed, never guessed, and
//! never retried: a wrong key locks the ECU until it is power-cycled.
use super::{BackupStrategy, BlockLayout};
use crate::utils::checksum8 as checksum;
use crate::{
    events::{Answer, Cancel, Event, Interaction},
    obd::uds::{self, call_raw},
    DiagError, ObdProtocol,
};
use std::time::{Duration, Instant};

pub struct HondaBa;

/// EEPROM channels 0 and 1. Channel 2 is not copied until its contents are known.
const LAYOUT: &[BlockLayout] = &[
    BlockLayout {
        id: "0",
        words: 256,
    },
    BlockLayout {
        id: "1",
        words: 256,
    },
];
const SOFTWARE_DID: u16 = 0xF181;
const SESSION_VENDOR: u8 = 0x60;
const SECURITY_SEED: u8 = 0x61;
const SECURITY_KEY: u8 = 0x62;
const REQUEST_TAG: u8 = 0xBA;
const REPLY_TAG: u8 = 0xFA;
const SERVICE_READ: u8 = 0x11;
const SERVICE_WRITE: u8 = 0x15;

/// How long the user has to power-cycle the ECU after answering the prompt.
const POWER_CYCLE_DEADLINE: Duration = Duration::from_secs(90);
/// How long a requested restart may take before the ECU answers again.
const RESTART_DEADLINE: Duration = Duration::from_secs(10);
/// One poll for the ECU while it is off or booting.
const POLL_TIMEOUT: Duration = Duration::from_millis(150);
/// Pause between polls while the ECU is still on.
#[cfg(not(test))]
const STILL_ON_PAUSE: Duration = Duration::from_millis(200);
#[cfg(test)]
const STILL_ON_PAUSE: Duration = Duration::ZERO;
/// Pause between polls after a restart request, short enough to see the ECU
/// drop off the bus while it reboots.
#[cfg(not(test))]
const RESTART_PAUSE: Duration = Duration::from_millis(20);
#[cfg(test)]
const RESTART_PAUSE: Duration = Duration::ZERO;
/// A restart can be quicker than one poll. After this long without seeing the
/// ECU go quiet, its next answer is taken to come from the restarted ECU.
#[cfg(not(test))]
const RESTART_QUIET_GRACE: Duration = Duration::from_millis(500);
#[cfg(test)]
const RESTART_QUIET_GRACE: Duration = Duration::ZERO;

/// The level 0x62 key for a level 0x61 seed: `seed × 33 / 4`, 16 bits.
pub(crate) fn key(seed: u16) -> u16 {
    (u32::from(seed) * 33 / 4) as u16
}
pub(crate) fn read_request(channel: u8, word: u8) -> [u8; 6] {
    [REQUEST_TAG, SERVICE_READ, 0, 0, channel, word]
}
pub(crate) fn write_request(channel: u8, word: u8, value: u16) -> [u8; 8] {
    let [lo, hi] = value.to_le_bytes();
    [REQUEST_TAG, SERVICE_WRITE, 0, 0, channel, word, lo, hi]
}
/// Checks a BA reply's framing and returns its data bytes.
pub(crate) fn reply_data(service: u8, reply: &[u8]) -> Result<&[u8], DiagError> {
    let invalid = |what: &str| {
        Err(DiagError::InvalidResponse(format!(
            "BA {service:02X} reply {}: {what}",
            crate::utils::bytes_to_hex(reply)
        )))
    };
    if reply.len() < 5 {
        return invalid("too short");
    }
    if reply[0] != REPLY_TAG {
        return invalid("wrong tag");
    }
    if reply[1] != service || reply[2] != service {
        return invalid("answers another service");
    }
    if usize::from(reply[3]) != reply.len() {
        return invalid("length byte disagrees with the reply");
    }
    let (body, sum) = reply.split_at(reply.len() - 1);
    if checksum(body) != sum[0] {
        return invalid("bad checksum");
    }
    Ok(&reply[4..reply.len() - 1])
}

fn channel_and_word(block: usize, word: usize) -> Result<(u8, u8), DiagError> {
    match (u8::try_from(block), u8::try_from(word)) {
        (Ok(channel), Ok(word)) if block < LAYOUT.len() => Ok((channel, word)),
        _ => Err(DiagError::InvalidCommand(format!(
            "No EEPROM value at block {block}, word {word}"
        ))),
    }
}

/// A positive reply whose sub-function echoes the request.
fn expect_echo(protocol: &mut ObdProtocol, request: &[u8]) -> Result<Vec<u8>, DiagError> {
    let reply = call_raw(protocol, request)?;
    if reply.get(1) != request.get(1) {
        return Err(DiagError::InvalidResponse(format!(
            "Reply {} does not echo {}",
            crate::utils::bytes_to_hex(&reply),
            crate::utils::bytes_to_hex(request)
        )));
    }
    Ok(reply)
}

/// How the ECU is being started again.
#[derive(Clone, Copy, PartialEq)]
enum Start {
    /// The user switches it off and on.
    PowerCycle,
    /// It was asked to restart.
    Restart,
}

/// Polls `10 03` until the ECU has gone quiet (off or rebooting) and answers
/// again, returning how long that took.
fn wait_for_start(
    protocol: &mut ObdProtocol,
    cancel: &Cancel,
    start: Start,
) -> Result<Duration, DiagError> {
    let (deadline, pause, grace) = match start {
        Start::PowerCycle => (POWER_CYCLE_DEADLINE, STILL_ON_PAUSE, None),
        Start::Restart => (RESTART_DEADLINE, RESTART_PAUSE, Some(RESTART_QUIET_GRACE)),
    };
    let begin = Instant::now();
    let mut seen_off = false;
    let mut warned_on = false;
    loop {
        cancel.check()?;
        let elapsed = begin.elapsed();
        if elapsed > deadline {
            return Err(DiagError::Timeout(match start {
                Start::PowerCycle => format!(
                    "The ECU was not power-cycled within {} s",
                    deadline.as_secs()
                ),
                Start::Restart => format!(
                    "The ECU did not answer within {} s of restarting",
                    deadline.as_secs()
                ),
            }));
        }
        let answered = match uds::call_raw_with_timeout(
            protocol,
            &[0x10, uds::SessionType::ExtendedDiagnostic as u8],
            Some(POLL_TIMEOUT),
        ) {
            Ok(_) => true,
            // Off, booting or not ready yet.
            Err(
                DiagError::Timeout(_)
                | DiagError::UdsNegativeResponse { .. }
                | DiagError::InvalidResponse(_),
            ) => false,
            Err(error) => return Err(error),
        };
        let restarted = seen_off || grace.is_some_and(|grace| elapsed >= grace);
        match (answered, restarted) {
            (true, true) => return Ok(begin.elapsed()),
            (true, false) => {
                if start == Start::PowerCycle && !warned_on {
                    protocol.emit(Event::Step("Waiting for the ECU to be turned off"));
                    warned_on = true;
                }
                cancel.wait(pause);
            }
            (false, _) => {
                if !seen_off && start == Start::PowerCycle {
                    protocol.emit(Event::Step("ECU is off; turn it back on now"));
                }
                seen_off = true;
            }
        }
    }
}

/// Asks the ECU to restart and waits until it answers again. `false` when it
/// refuses to restart.
fn restart(protocol: &mut ObdProtocol, cancel: &Cancel) -> Result<bool, DiagError> {
    protocol.emit(Event::Step("Restarting the ECU"));
    expect_echo(
        protocol,
        &[0x10, uds::SessionType::ExtendedDiagnostic as u8],
    )?;
    let reset = uds::ResetType::Hard as u8;
    // A short wait: the ECU may reboot without replying, and every
    // millisecond spent waiting is taken from the access window.
    match uds::call_raw_with_timeout(protocol, &[0x11, reset], Some(POLL_TIMEOUT)) {
        Ok(reply) if reply.get(1) == Some(&reset) => {}
        Ok(reply) => {
            return Err(DiagError::InvalidResponse(format!(
                "ECU reset reply {} does not echo 11 {reset:02X}",
                crate::utils::bytes_to_hex(&reply)
            )))
        }
        Err(DiagError::UdsNegativeResponse { code, .. }) => {
            protocol.emit(Event::Warning(&format!(
                "The ECU refused to restart (NRC 0x{code:02X}); it needs a power cycle instead"
            )));
            return Ok(false);
        }
        Err(DiagError::Timeout(_)) => {}
        Err(error) => return Err(error),
    }
    let took = wait_for_start(protocol, cancel, Start::Restart)?;
    protocol.emit(Event::Detail(&format!(
        "[HONDA_BA] ECU answered {} ms after the restart request",
        took.as_millis()
    )));
    // Replies to earlier polls may still be queued.
    protocol.reset()?;
    Ok(true)
}

fn request_seed(protocol: &mut ObdProtocol) -> Result<u16, DiagError> {
    match expect_echo(protocol, &[0x27, SECURITY_SEED])?.as_slice() {
        [_, _, hi, lo] => Ok(u16::from_be_bytes([*hi, *lo])),
        _ => Err(DiagError::InvalidResponse(
            "SecurityAccess seed must be 2 bytes".into(),
        )),
    }
}

/// Sends the key for a granted seed and enters the supplier session.
fn grant(protocol: &mut ObdProtocol, seed: u16) -> Result<(), DiagError> {
    let [hi, lo] = key(seed).to_be_bytes();
    expect_echo(protocol, &[0x27, SECURITY_KEY, hi, lo])?;
    expect_echo(protocol, &[0x10, SESSION_VENDOR])?;
    protocol.emit(Event::Step("ECU access granted"));
    Ok(())
}

impl BackupStrategy for HondaBa {
    fn name(&self) -> &'static str {
        "honda_ba"
    }
    fn layout(&self) -> &'static [BlockLayout] {
        LAYOUT
    }
    fn contents(&self) -> &'static str {
        "VIN, immobilizer pairing, mileage, learned values"
    }
    fn software(&self, protocol: &mut ObdProtocol) -> Result<String, DiagError> {
        let software = crate::diag::text(&uds::read_data_by_identifier(protocol, SOFTWARE_DID)?)
            .trim()
            .to_string();
        if software.is_empty() {
            return Err(DiagError::InvalidResponse(
                "The ECU reported an empty software number".into(),
            ));
        }
        Ok(software)
    }
    fn unlock(
        &self,
        protocol: &mut ObdProtocol,
        ui: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        if ui.prompt("To grant access, the ECU will restart. Make sure the engine is not running.")
            != Answer::Continue
        {
            return Err(DiagError::Cancelled);
        }
        if restart(protocol, cancel)? {
            match request_seed(protocol) {
                Ok(seed) => return grant(protocol, seed),
                Err(DiagError::UdsNegativeResponse { code, .. }) => {
                    protocol.emit(Event::Warning(&format!(
                        "The ECU refused access after restarting (NRC 0x{code:02X}); it needs a power cycle instead"
                    )));
                }
                Err(error) => return Err(error),
            }
        }
        if ui.prompt(
            "Turn the ECU off (ignition off, or switch off the bench supply), answer y, then turn it back on.",
        ) != Answer::Continue
        {
            return Err(DiagError::Cancelled);
        }
        wait_for_start(protocol, cancel, Start::PowerCycle)?;
        protocol.reset()?;
        let seed = request_seed(protocol)?;
        grant(protocol, seed)
    }
    fn read_word(
        &self,
        protocol: &mut ObdProtocol,
        block: usize,
        word: usize,
    ) -> Result<u16, DiagError> {
        let (channel, word) = channel_and_word(block, word)?;
        let reply = call_raw(protocol, &read_request(channel, word))?;
        match reply_data(SERVICE_READ, &reply)? {
            [lo, hi] => Ok(u16::from_le_bytes([*lo, *hi])),
            data => Err(DiagError::InvalidResponse(format!(
                "BA 11 reply carries {} data bytes, expected 2",
                data.len()
            ))),
        }
    }
    fn write_word(
        &self,
        protocol: &mut ObdProtocol,
        block: usize,
        word: usize,
        value: u16,
    ) -> Result<(), DiagError> {
        let (channel, word) = channel_and_word(block, word)?;
        let reply = call_raw(protocol, &write_request(channel, word, value))?;
        reply_data(SERVICE_WRITE, &reply)?;
        Ok(())
    }
    fn risk(&self) -> &'static str {
        "This replaces the ECU's VIN and immobilizer key pairing. Do not turn the ECU off until the restore finishes."
    }
    fn next_steps(&self) -> &'static str {
        "Turn the ignition off for 10 s, turn it on, and check that the immobilizer light goes out before starting the engine."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_seed_times_33_over_4_in_16_bits() {
        assert_eq!(key(0x0000), 0x0000);
        assert_eq!(key(0x1234), (0x1234u32 * 33 / 4) as u16);
        // 0xFFFF × 33 / 4 overflows 16 bits; only the low 16 bits are sent.
        assert_eq!(key(0xFFFF), ((0xFFFFu32 * 33 / 4) & 0xFFFF) as u16);
    }

    #[test]
    fn requests_carry_channel_word_and_little_endian_value() {
        assert_eq!(read_request(1, 0x2A), [0xBA, 0x11, 0, 0, 1, 0x2A]);
        assert_eq!(
            write_request(0, 0xFF, 0xA1B2),
            [0xBA, 0x15, 0, 0, 0, 0xFF, 0xB2, 0xA1]
        );
    }

    fn reply(service: u8, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xFA, service, service, (data.len() + 5) as u8];
        bytes.extend_from_slice(data);
        bytes.push(checksum(&bytes));
        bytes
    }

    #[test]
    fn a_framed_reply_yields_its_data() {
        let bytes = reply(0x11, &[0xB2, 0xA1]);
        assert_eq!(bytes.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0);
        assert_eq!(reply_data(0x11, &bytes).unwrap(), &[0xB2, 0xA1]);
        assert_eq!(reply_data(0x15, &reply(0x15, &[])).unwrap(), &[] as &[u8]);
    }

    #[test]
    fn malformed_replies_are_rejected() {
        let good = reply(0x11, &[0xB2, 0xA1]);
        let mut bad_sum = good.clone();
        *bad_sum.last_mut().unwrap() ^= 1;
        let mut bad_length = good.clone();
        bad_length[3] += 1;
        let mut bad_tag = good.clone();
        bad_tag[0] = 0xFB;
        let mut mixed_service = good.clone();
        mixed_service[2] = 0x15;
        for (name, bytes) in [
            ("checksum", bad_sum),
            ("length", bad_length),
            ("tag", bad_tag),
            ("service echo", mixed_service),
            ("short", vec![0xFA, 0x11, 0x11, 0x04]),
        ] {
            assert!(reply_data(0x11, &bytes).is_err(), "{name}");
        }
        assert!(reply_data(0x15, &good).is_err(), "another service's reply");
    }
}
