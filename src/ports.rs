use std::io::{Error, ErrorKind};
use std::time::{Duration, Instant};

pub mod serial;
pub mod tcp;


pub trait OpenDiagPort {
    fn write_data(&mut self, data: &[u8]) -> Result<(), Error>;
    fn read_data(&mut self, data: &mut [u8]) -> Result<usize, Error>;
    fn clear_input(&mut self) -> Result<(), Error>;
    fn set_baudrate(&mut self,baud_rate: u32) -> Result<(), Error>;

    /// Sends a command, waits for a response terminated by '>', and returns the response.
    fn send_command(
        &mut self,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<String, Error> {
        // 1. Prepare the command and set the timeout.
        let full_command = format!("{}\r", command);
        let read_timeout = timeout.unwrap_or_else(|| Duration::from_secs(3));

        // 2. Clear any lingering data in the read buffer before sending.
        // This is good practice to ensure we only read the response to our command.
        self.clear_input()?;

        // 3. Send the command.
        self.write_data(full_command.as_bytes())?;

        // 4. Loop to read the response.
        let mut response_bytes: Vec<u8> = Vec::new();
        let mut temp_buffer = [0; 128];
        let start_time = Instant::now();

        loop {
            // Check for total timeout.
            if start_time.elapsed() >= read_timeout {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    "Timed out waiting for '>' prompt.",
                ));
            }

            match self.read_data(&mut temp_buffer) {
                Ok(bytes_read) => {
                    response_bytes.extend_from_slice(&temp_buffer[..bytes_read]);
                    // Check if the response terminator is present.
                    if response_bytes.contains(&b'>') {
                        break; // Success, exit the loop.
                    }
                }
                Err(ref e) if e.kind() == ErrorKind::TimedOut => {
                    // Per-read timeout is expected, just continue waiting.
                    continue;
                }
                Err(e) => {
                    // Any other I/O error is fatal.
                    return Err(e);
                }
            }
        }

        // 5. Convert the raw bytes to a String, cleaning up the response.
        // The OBD-II response often includes the original command and the '>' prompt.
        // We will trim these to return only the useful data.
        let response_string = String::from_utf8_lossy(&response_bytes).to_string();
        
        // Trim whitespace and the final '>' character.
        Ok(response_string.trim().trim_end_matches('>').to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ByteScriptPort;

    /// Splits a raw ELM327 response the way `ObdProtocol` does downstream.
    fn lines(response: &str) -> Vec<&str> {
        response
            .split('\r')
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect()
    }

    #[test]
    fn appends_cr_clears_input_and_strips_the_prompt() {
        let mut port = ByteScriptPort::new(&["ATZ\rELM327 v1.5\r\r>"]);

        let resp = port.send_command("ATZ", None).unwrap();

        assert_eq!(port.written, b"ATZ\r", "command must be CR terminated");
        assert_eq!(port.clear_calls, 1, "stale input must be flushed first");
        assert!(!resp.contains('>'), "prompt must not leak into the response");
        assert_eq!(lines(&resp), vec!["ATZ", "ELM327 v1.5"]);
    }

    /// A real port hands back a response a few bytes at a time; `send_command`
    /// must keep reading until the `>` prompt shows up rather than returning
    /// the first partial chunk.
    #[test]
    fn accumulates_across_reads_until_the_prompt_arrives() {
        let mut port = ByteScriptPort::new(&["18DAF1", "01064100", "80000001\r", ">"]);

        let resp = port.send_command("0100", None).unwrap();

        assert_eq!(lines(&resp), vec!["18DAF10106410080000001"]);
    }

    /// Per-read timeouts are normal and must not abort the exchange; only the
    /// overall deadline does.
    #[test]
    fn tolerates_per_read_timeouts_before_the_prompt() {
        // The empty chunk makes the first read fail with ErrorKind::TimedOut.
        let mut port = ByteScriptPort::new(&["", "41 00 BE\r>"]);

        let resp = port.send_command("0100", Some(Duration::from_secs(1))).unwrap();

        assert_eq!(lines(&resp), vec!["41 00 BE"]);
    }

    #[test]
    fn times_out_when_the_prompt_never_arrives() {
        let mut port = ByteScriptPort::new(&["41 00 BE\r"]); // no '>' ever sent

        let err = port
            .send_command("0100", Some(Duration::from_millis(50)))
            .unwrap_err();

        assert_eq!(err.kind(), ErrorKind::TimedOut);
    }

    #[test]
    fn propagates_non_timeout_io_errors() {
        struct BrokenPort;
        impl OpenDiagPort for BrokenPort {
            fn write_data(&mut self, _: &[u8]) -> Result<(), Error> { Ok(()) }
            fn read_data(&mut self, _: &mut [u8]) -> Result<usize, Error> {
                Err(Error::new(ErrorKind::BrokenPipe, "cable yanked"))
            }
            fn clear_input(&mut self) -> Result<(), Error> { Ok(()) }
            fn set_baudrate(&mut self, _: u32) -> Result<(), Error> { Ok(()) }
        }

        let err = BrokenPort.send_command("ATZ", None).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::BrokenPipe);
    }
}
