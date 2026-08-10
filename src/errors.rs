use std::fmt;

#[derive(Debug)]
pub enum DiagError {
    // A variant for I/O errors that wraps the original std::io::Error
    PortError(std::io::Error),
    // A variant for when a device times out
    //Timeout,
    // A variant for when a response from the device can't be parsed
    InvalidResponse(String),
    // A variant for when an invalid command is sent
    InvalidCommand(String),
}

// Implement the Display trait for user-friendly error messages
impl fmt::Display for DiagError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DiagError::PortError(io_err) => write!(f, "Port I/O error: {}", io_err),
            //DiagError::Timeout => write!(f, "Device did not respond in time."),
            DiagError::InvalidResponse(resp) => write!(f, "Received invalid response: '{}'", resp),
            DiagError::InvalidCommand(cmd) => write!(f, "An invalid command was used: '{}'", cmd),
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
