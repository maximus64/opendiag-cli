use serialport::{SerialPort as SerialDevicePort, ClearBuffer};
use std::time::Duration;
use std::io::{Error, ErrorKind};
use crate::ports::OpenDiagPort;

#[cfg(debug_assertions)]
macro_rules! debug_hexdump {
    ($x:expr) => { hexdump( $x ); }
}

pub struct SerialPort {
    port: Box<dyn SerialDevicePort>,
}

impl SerialPort {
    pub fn new(port_name: &str, baud_rate: u32) -> Result<Self, Error> {
        let port = serialport::new(port_name, baud_rate)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| Error::new(ErrorKind::Other, e))?;

        Ok(Self {
            port: port,
        })
    }
}

impl OpenDiagPort for SerialPort {
    fn write_data(&mut self, data: &[u8]) -> Result<(), Error> {
        // println!("[SERIAL] Writing {} bytes", data.len());
        // debug_hexdump!(data);
        
        self.port.write_all(data)
    }

    fn read_data(&mut self, data: &mut [u8]) -> Result<usize, Error> {
        let len = self.port.read(data)?;

        // println!("[SERIAL] Read {} bytes", len);
        // debug_hexdump!(&data[..len]);
        Ok(len)
    }

    fn clear_input(&mut self) -> Result<(), Error> {
        self.port.clear(ClearBuffer::Input).map_err(|e| Error::new(std::io::ErrorKind::Other, format!("Failed to get data: {}", e)))
    }

    fn set_baudrate(&mut self,baud_rate: u32) -> Result<(), Error> {
        self.port.set_baud_rate(baud_rate).map_err(|e| Error::new(std::io::ErrorKind::Other, format!("Failed to set baud: {}", e)))
    }
}
