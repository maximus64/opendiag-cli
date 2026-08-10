use std::io::{Error, ErrorKind, Read, Write};
use std::net::TcpStream;

use crate::ports::OpenDiagPort;

#[cfg(debug_assertions)]
macro_rules! debug_hexdump {
    ($x:expr) => { hexdump( $x ); }
}

pub struct TcpPort {
    stream: TcpStream,
}

impl TcpPort {
    pub fn new(addr: &str) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr).map_err(|e| Error::new(ErrorKind::Other, e))?;
        // stream
        //     .set_read_timeout(Some(Duration::from_millis(5000)))
        //     .map_err(|e| Error::new(ErrorKind::Other, e))?;
        Ok(Self { stream })
    }
}

impl OpenDiagPort for TcpPort {
    fn write_data(&mut self, data: &[u8]) -> Result<(), Error> {
        self.stream.write_all(data)
    }

    fn read_data(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        self.stream.read(buf)
    }

    fn clear_input(&mut self) -> Result<(), Error> {
        let mut buffer = [0; 1024];
        self.stream.set_nonblocking(true)?;
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => break, // No more data
                Ok(_) => continue, // Data read and discarded
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    break; // No more data to read
                }
                Err(e) => {
                    self.stream.set_nonblocking(false)?;
                    return Err(e);
                }
            }
        }
        self.stream.set_nonblocking(false)?;
        Ok(())
    }

    fn set_baudrate(&mut self, _baud_rate: u32) -> Result<(), Error> {
        // Not applicable for TCP
        Ok(())
    }
}
