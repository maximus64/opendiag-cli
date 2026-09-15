use std::fmt;

#[derive(Debug)]
pub enum DiagError {
    Cancelled,
    Unsupported(String),
    InvalidFile(String),
    PreflightFailed(String),
    // A variant for I/O errors that wraps the original std::io::Error
    PortError(std::io::Error),
    /// Nothing arrived in time; the message says what was being waited for.
    Timeout(String),
    // A variant for when a response from the device can't be parsed
    InvalidResponse(String),
    /// A valid UDS reply rejecting the requested operation.
    UdsNegativeResponse {
        code: u8,
        description: &'static str,
    },
    // A variant for when an invalid command is sent
    InvalidCommand(String),
    /// The adapter or its driver can't be used (wrong library, no device...).
    Adapter(String),
    /// A J2534 call failed. `detail` is the driver's `PassThruGetLastError`
    /// text, which is often the only place the actual reason is given.
    PassThru {
        call: &'static str,
        code: i32,
        detail: String,
    },
}

// Implement the Display trait for user-friendly error messages
impl fmt::Display for DiagError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DiagError::Cancelled => f.write_str("Operation cancelled"),
            DiagError::Unsupported(message)
            | DiagError::InvalidFile(message)
            | DiagError::PreflightFailed(message) => f.write_str(message),
            DiagError::PortError(io_err) => write!(f, "Port I/O error: {io_err}"),
            DiagError::Timeout(what) => write!(f, "{what}"),
            DiagError::InvalidResponse(resp) => write!(f, "Received invalid response: '{resp}'"),
            DiagError::UdsNegativeResponse { code, description } => {
                write!(f, "UDS negative response NRC=0x{code:02X} - {description}")
            }
            DiagError::InvalidCommand(cmd) => write!(f, "An invalid command was used: '{cmd}'"),
            DiagError::Adapter(msg) => write!(f, "{msg}"),
            DiagError::PassThru { call, code, detail } => {
                write!(
                    f,
                    "{} failed: {} (0x{:02X})",
                    call,
                    crate::j2534::consts::error_name(*code),
                    code
                )?;
                if !detail.is_empty() {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
        }
    }
}

// Implement the standard Error trait
impl std::error::Error for DiagError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // This links the underlying I/O error
            DiagError::PortError(io_err) => Some(io_err),
            // These errors don't have an underlying cause
            _ => None,
        }
    }
}

// Implement `From<std::io::Error>` to make the `?` operator work seamlessly
impl From<std::io::Error> for DiagError {
    fn from(err: std::io::Error) -> Self {
        DiagError::PortError(err)
    }
}

impl From<&str> for DiagError {
    fn from(err: &str) -> Self {
        DiagError::InvalidCommand(err.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum ErrorKind {
    NoAdapter,
    AdapterBusy,
    NoResponse,
    EcuRefused,
    InvalidFile,
    Unsupported,
    Cancelled,
    Other,
}
impl DiagError {
    pub fn kind(&self) -> ErrorKind {
        use crate::j2534::consts::*;
        match self {
            Self::Cancelled => ErrorKind::Cancelled,
            Self::Unsupported(_) => ErrorKind::Unsupported,
            Self::InvalidFile(_) => ErrorKind::InvalidFile,
            Self::Timeout(_) => ErrorKind::NoResponse,
            Self::UdsNegativeResponse { .. } | Self::PreflightFailed(_) => ErrorKind::EcuRefused,
            Self::Adapter(_) => ErrorKind::NoAdapter,
            Self::PassThru { code, .. } => match *code {
                ERR_DEVICE_IN_USE | ERR_PIN_IN_USE => ErrorKind::AdapterBusy,
                ERR_TIMEOUT => ErrorKind::NoResponse,
                ERR_DEVICE_NOT_CONNECTED | ERR_OPEN_FAILED => ErrorKind::NoAdapter,
                ERR_NOT_SUPPORTED | ERR_IOCTL_ID_NOT_SUPPORTED | ERR_PIN_NOT_SUPPORTED => {
                    ErrorKind::Unsupported
                }
                _ => ErrorKind::Other,
            },
            _ => ErrorKind::Other,
        }
    }
}
