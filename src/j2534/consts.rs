//! SAE J2534-1 v05.00 constants, as defined by the spec and mirrored from the
//! OpenDiag driver's `include/j2534.h`. Only values this tool uses are listed;
//! add others here, by name, rather than as magic numbers at the call site.

// ------------------------------------------------------------ return values

pub const STATUS_NOERROR: i32 = 0x00;
pub const ERR_NOT_SUPPORTED: i32 = 0x01;
pub const ERR_BUFFER_EMPTY: i32 = 0x10;
pub const ERR_BUFFER_FULL: i32 = 0x11;
pub const ERR_PIN_NOT_SUPPORTED: i32 = 0x13;
pub const ERR_TIMEOUT: i32 = 0x09;
pub const ERR_DEVICE_NOT_CONNECTED: i32 = 0x08;
pub const ERR_DEVICE_IN_USE: i32 = 0x0E;
pub const ERR_OPEN_FAILED: i32 = 0x22;
pub const ERR_INIT_FAILED: i32 = 0x21;
pub const ERR_IOCTL_ID_NOT_SUPPORTED: i32 = 0x0F;
pub const ERR_IOCTL_PARAM_ID_NOT_SUPPORTED: i32 = 0x1E;
pub const ERR_PIN_IN_USE: i32 = 0x20;

/// The spec's name for a return value, for error messages.
pub fn error_name(code: i32) -> &'static str {
    match code {
        0x00 => "STATUS_NOERROR",
        0x01 => "ERR_NOT_SUPPORTED",
        0x02 => "ERR_INVALID_CHANNEL_ID",
        0x03 => "ERR_PROTOCOL_ID_NOT_SUPPORTED",
        0x04 => "ERR_NULL_PARAMETER",
        0x05 => "ERR_IOCTL_VALUE_NOT_SUPPORTED",
        0x06 => "ERR_FLAG_NOT_SUPPORTED",
        0x07 => "ERR_FAILED",
        0x08 => "ERR_DEVICE_NOT_CONNECTED",
        0x09 => "ERR_TIMEOUT",
        0x0A => "ERR_INVALID_MSG",
        0x0B => "ERR_TIME_INTERVAL_NOT_SUPPORTED",
        0x0C => "ERR_EXCEEDED_LIMIT",
        0x0D => "ERR_INVALID_MSG_ID",
        0x0E => "ERR_DEVICE_IN_USE",
        0x0F => "ERR_IOCTL_ID_NOT_SUPPORTED",
        0x10 => "ERR_BUFFER_EMPTY",
        0x11 => "ERR_BUFFER_FULL",
        0x12 => "ERR_BUFFER_OVERFLOW",
        0x13 => "ERR_PIN_NOT_SUPPORTED",
        0x14 => "ERR_RESOURCE_CONFLICT",
        0x15 => "ERR_MSG_PROTOCOL_ID",
        0x16 => "ERR_INVALID_FILTER_ID",
        0x17 => "ERR_MSG_NOT_ALLOWED",
        0x18 => "ERR_NOT_UNIQUE",
        0x19 => "ERR_BAUDRATE_NOT_SUPPORTED",
        0x1A => "ERR_INVALID_DEVICE_ID",
        0x1B => "ERR_DEVICE_NOT_OPEN",
        0x1C => "ERR_NULL_REQUIRED",
        0x1D => "ERR_FILTER_TYPE_NOT_SUPPORTED",
        0x1E => "ERR_IOCTL_PARAM_ID_NOT_SUPPORTED",
        0x1F => "ERR_VOLTAGE_IN_USE",
        0x20 => "ERR_PIN_IN_USE",
        0x21 => "ERR_INIT_FAILED",
        0x22 => "ERR_OPEN_FAILED",
        0x23 => "ERR_BUFFER_TOO_SMALL",
        0x24 => "ERR_LOG_CHAN_NOT_ALLOWED",
        0x25 => "ERR_SELECT_TYPE_NOT_SUPPORTED",
        0x26 => "ERR_CONCURRENT_API_CALL",
        0x27 => "ERR_INVALID_CHANNEL_DESCRIPTOR",
        _ => "unknown J2534 error",
    }
}

// ------------------------------------------------------------- protocol IDs

pub const PROTOCOL_J1850VPW: u32 = 0x01;
pub const PROTOCOL_J1850PWM: u32 = 0x02;
pub const PROTOCOL_ISO9141: u32 = 0x03;
pub const PROTOCOL_ISO14230: u32 = 0x04;
pub const PROTOCOL_CAN: u32 = 0x05;
pub const PROTOCOL_ISO15765_LOGICAL: u32 = 0x200;

// ------------------------------------------------------------------ connect

pub const J1962_CONNECTOR: u32 = 0x01;
pub const K_LINE_ONLY: u32 = 0x1000;

// ---------------------------------------------------------------- tx flags

pub const ISO15765_FRAME_PAD: u32 = 0x40;
pub const CAN_29BIT_ID: u32 = 0x100;

// ---------------------------------------------------------------- rx status

/// The message is an echo of one this client transmitted.
pub const TX_MSG_TYPE: u32 = 0x01;
/// Indication that a multi-frame reception has started.
pub const START_OF_MESSAGE: u32 = 0x02;
/// Indication that a transmission completed.
pub const TX_INDICATION_SUCCESS: u32 = 0x08;
pub const RX_BREAK: u32 = 0x04;
pub const RX_ERROR: u32 = 0x20;
pub const BUFFER_OVERFLOW: u32 = 0x40;
pub const TX_FAILED: u32 = 0x200;

// ------------------------------------------------------------------ filters

pub const PASS_FILTER: u32 = 0x01;

// ---------------------------------------------------------------- voltages

/// Programming voltage sentinel: release the pin (high impedance).
pub const PIN_OFF: u32 = 0xFFFF_FFFF;
/// Programming voltage sentinel: short the pin to ground (pin 15 only).
pub const SHORT_TO_GROUND: u32 = 0xFFFF_FFFE;

// ------------------------------------------------------------------ IOCTLs

pub const SET_CONFIG: u32 = 0x02;
pub const FIVE_BAUD_INIT: u32 = 0x04;
pub const FAST_INIT: u32 = 0x05;
pub const READ_PIN_VOLTAGE: u32 = 0x03;
pub const CLEAR_TX_QUEUE: u32 = 0x07;
pub const CLEAR_RX_QUEUE: u32 = 0x08;
pub const CLEAR_FUNCT_MSG_LOOKUP_TABLE: u32 = 0x0B;
pub const ADD_TO_FUNCT_MSG_LOOKUP_TABLE: u32 = 0x0C;
pub const READ_PROG_VOLTAGE: u32 = 0x0E;
/// SAE J2534-2 extension; optional, and absent on many adapters.
pub const GET_DEVICE_INFO: u32 = 0x800C;

// ------------------------------------------------- SET_CONFIG parameters

pub const ISO15765_BS: u32 = 0x1E;
pub const NODE_ADDRESS: u32 = 0x04;
pub const NETWORK_LINE: u32 = 0x05;
pub const ISO15765_STMIN: u32 = 0x1F;
pub const P1_MAX: u32 = 0x07;
pub const P2_MAX: u32 = 0x09;
pub const P3_MIN: u32 = 0x0A;
pub const P4_MIN: u32 = 0x0C;

// ------------------------------------------- GET_DEVICE_INFO parameters

pub const CAN_SUPPORTED: u32 = 0x06;
pub const J1850PWM_SUPPORTED: u32 = 0x02;
pub const J1850VPW_SUPPORTED: u32 = 0x03;
pub const ISO9141_SUPPORTED: u32 = 0x04;
pub const ISO14230_SUPPORTED: u32 = 0x05;
/// `SPARAM.Value` on input: ask about the whole device, not one resource.
pub const ENTIRE_DEVICE: u32 = 0x0001_0000;

// ------------------------------------------------------------------ limits

/// Largest message the spec lets one `PASSTHRU_MSG` carry; receive buffers are
/// this size so no legal message is ever `ERR_BUFFER_TOO_SMALL`.
pub const MAX_MESSAGE_BYTES: usize = 4128;

/// OpenDiag refuses `PassThruReadMsgs` timeouts above 30 s; longer waits loop.
pub const MAX_READ_TIMEOUT_MS: u32 = 30_000;
