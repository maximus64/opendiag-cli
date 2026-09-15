//! Raw SAE J2534-1 v05.00 bindings, loaded at runtime from a vendor library.
//!
//! The structs mirror the spec's `#pragma pack(1)` layouts and the function
//! pointers use `extern "system"` (`__stdcall` on 32-bit Windows, the C ABI
//! everywhere else). Nothing is linked at build time: the library is chosen per
//! run, so a missing or wrong-version library is an ordinary error.
//!
//! Every wrapper turns a non-zero status into [`DiagError::PassThru`] carrying
//! `PassThruGetLastError`, because that text is often the only place a driver
//! says *why* a call failed.

use std::ffi::{c_char, c_void, CString};
use std::path::Path;

use libloading::Library;

use crate::errors::DiagError;
use crate::j2534::consts::*;

// ------------------------------------------------------------------ structs

#[repr(C, packed)]
pub struct PassThruMsg {
    pub protocol_id: u32,
    pub msg_handle: u32,
    pub rx_status: u32,
    pub tx_flags: u32,
    pub timestamp: u32,
    pub data_length: u32,
    pub extra_data_index: u32,
    pub data_buffer: *mut u8,
    pub data_buffer_size: u32,
}

#[repr(C, packed)]
pub struct SDevice {
    pub device_name: [c_char; 80],
    pub device_available: u32,
    pub device_dll_fw_status: u32,
    pub device_connect_media: u32,
    pub device_connect_speed: u32,
    pub device_signal_quality: u32,
    pub device_signal_strength: u32,
}

/// Passed **by value** to `PassThruConnect` and `PassThruSetProgrammingVoltage`.
/// Its fields are naturally aligned, so packed and unpacked layouts agree on
/// every target; the hardware test still checks the call end to end.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ResourceStruct {
    pub connector: u32,
    pub num_of_resources: u32,
    pub resource_list_ptr: *mut u32,
}

#[repr(C, packed)]
#[derive(Default)]
pub struct Iso15765ChannelDescriptor {
    pub local_tx_flags: u32,
    pub remote_tx_flags: u32,
    pub local_address: [u8; 5],
    pub remote_address: [u8; 5],
}

#[repr(C, packed)]
pub struct SConfig {
    pub parameter: u32,
    pub value: u32,
}

#[repr(C, packed)]
pub struct SConfigList {
    pub num_of_params: u32,
    pub config_ptr: *mut SConfig,
}

#[repr(C, packed)]
pub struct SByteArray {
    pub num_of_bytes: u32,
    pub byte_ptr: *mut u8,
}

#[repr(C, packed)]
pub struct SParam {
    pub parameter: u32,
    pub value: u32,
    pub supported: u32,
}

#[repr(C, packed)]
pub struct SParamList {
    pub num_of_params: u32,
    pub param_ptr: *mut SParam,
}

// --------------------------------------------------------- function types

type ScanForDevicesFn = unsafe extern "system" fn(*mut u32) -> i32;
type GetNextDeviceFn = unsafe extern "system" fn(*mut SDevice) -> i32;
type OpenFn = unsafe extern "system" fn(*const c_char, *mut u32) -> i32;
type CloseFn = unsafe extern "system" fn(u32) -> i32;
type ConnectFn = unsafe extern "system" fn(u32, u32, u32, u32, ResourceStruct, *mut u32) -> i32;
type DisconnectFn = unsafe extern "system" fn(u32) -> i32;
type LogicalConnectFn = unsafe extern "system" fn(u32, u32, u32, *mut c_void, *mut u32) -> i32;
type LogicalDisconnectFn = unsafe extern "system" fn(u32) -> i32;
type ReadMsgsFn = unsafe extern "system" fn(u32, *mut PassThruMsg, *mut u32, u32) -> i32;
type QueueMsgsFn = unsafe extern "system" fn(u32, *mut PassThruMsg, *mut u32) -> i32;
type StartMsgFilterFn =
    unsafe extern "system" fn(u32, u32, *mut PassThruMsg, *mut PassThruMsg, *mut u32) -> i32;
type StopMsgFilterFn = unsafe extern "system" fn(u32, u32) -> i32;
type SetProgrammingVoltageFn = unsafe extern "system" fn(u32, ResourceStruct, u32) -> i32;
type ReadVersionFn = unsafe extern "system" fn(u32, *mut c_char, *mut c_char, *mut c_char) -> i32;
type GetLastErrorFn = unsafe extern "system" fn(*mut c_char) -> i32;
type IoctlFn = unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> i32;

/// Which J2534 API a library implements, judged by what it exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiVersion {
    V0500,
    V0404,
}

/// One entry from `PassThruScanForDevices` / `PassThruGetNextDevice`.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    /// `DEVICE_AVAILABLE` (1) or `DEVICE_IN_USE` (2); 0 when unknown.
    pub available: u32,
    /// `DEVICE_DLL_FW_COMPATIBLE` (1) and friends; 0 when unknown.
    pub dll_fw_status: u32,
}

/// A received message with its status bits, data copied out of the buffer.
#[derive(Debug, Clone)]
pub struct RxMsg {
    pub protocol_id: u32,
    pub rx_status: u32,
    pub extra_data_index: usize,
    pub data: Vec<u8>,
}

/// A loaded J2534 v05.00 library.
///
/// The function pointers are only valid while `_library` is loaded; keeping
/// both in one struct, and never handing the pointers out, ties their lifetime
/// to it.
pub struct PassThruLib {
    scan_for_devices: ScanForDevicesFn,
    get_next_device: GetNextDeviceFn,
    open: OpenFn,
    close: CloseFn,
    connect: ConnectFn,
    disconnect: DisconnectFn,
    logical_connect: LogicalConnectFn,
    logical_disconnect: LogicalDisconnectFn,
    read_msgs: ReadMsgsFn,
    queue_msgs: QueueMsgsFn,
    start_msg_filter: StartMsgFilterFn,
    stop_msg_filter: StopMsgFilterFn,
    set_programming_voltage: SetProgrammingVoltageFn,
    read_version: ReadVersionFn,
    get_last_error: GetLastErrorFn,
    ioctl: IoctlFn,
    _library: Library,
}

fn has_symbol(library: &Library, name: &str) -> bool {
    // SAFETY: only the address is looked up; it is never called through this type.
    unsafe { library.get::<*const c_void>(name.as_bytes()).is_ok() }
}

/// The API version `library` implements, from the symbols it exports:
/// v05.00 added `PassThruScanForDevices` and replaced `PassThruWriteMsgs`
/// with `PassThruQueueMsgs`.
fn api_version_of(library: &Library) -> Option<ApiVersion> {
    if has_symbol(library, "PassThruScanForDevices") && has_symbol(library, "PassThruQueueMsgs") {
        Some(ApiVersion::V0500)
    } else if has_symbol(library, "PassThruWriteMsgs") {
        Some(ApiVersion::V0404)
    } else {
        None
    }
}

fn open_library(path: &Path) -> Result<Library, DiagError> {
    // SAFETY: loading a library runs its initialisers. That is inherent to
    // using a vendor J2534 driver, and the path is one the user chose.
    unsafe { Library::new(path) }.map_err(|e| {
        DiagError::Adapter(format!(
            "Cannot load J2534 library {}: {}",
            path.display(),
            e
        ))
    })
}

fn symbol<T: Copy>(library: &Library, name: &str) -> Result<T, DiagError> {
    // SAFETY: `T` is the spec's signature for `name`; the pointer is stored in
    // the same struct as `library` and never outlives it.
    unsafe { library.get::<T>(name.as_bytes()) }
        .map(|s| *s)
        .map_err(|_| DiagError::Adapter(format!("J2534 library does not export {name}")))
}

/// Text from a fixed-size, NUL-terminated `char` buffer.
fn c_text(buffer: &[c_char]) -> String {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl PassThruLib {
    /// Loads a J2534 library. A v04.04 library or a non-J2534 library is an
    /// error naming what was found, rather than a missing-symbol failure.
    pub fn load(path: &Path) -> Result<Self, DiagError> {
        let library = open_library(path)?;
        match api_version_of(&library) {
            Some(ApiVersion::V0500) => {}
            Some(ApiVersion::V0404) => {
                return Err(DiagError::Adapter(format!(
                    "{} is a J2534 v04.04 library; only v05.00 is supported so far",
                    path.display()
                )))
            }
            None => {
                return Err(DiagError::Adapter(format!(
                    "{} does not export the J2534 API",
                    path.display()
                )))
            }
        }

        Ok(Self {
            scan_for_devices: symbol(&library, "PassThruScanForDevices")?,
            get_next_device: symbol(&library, "PassThruGetNextDevice")?,
            open: symbol(&library, "PassThruOpen")?,
            close: symbol(&library, "PassThruClose")?,
            connect: symbol(&library, "PassThruConnect")?,
            disconnect: symbol(&library, "PassThruDisconnect")?,
            logical_connect: symbol(&library, "PassThruLogicalConnect")?,
            logical_disconnect: symbol(&library, "PassThruLogicalDisconnect")?,
            read_msgs: symbol(&library, "PassThruReadMsgs")?,
            queue_msgs: symbol(&library, "PassThruQueueMsgs")?,
            start_msg_filter: symbol(&library, "PassThruStartMsgFilter")?,
            stop_msg_filter: symbol(&library, "PassThruStopMsgFilter")?,
            set_programming_voltage: symbol(&library, "PassThruSetProgrammingVoltage")?,
            read_version: symbol(&library, "PassThruReadVersion")?,
            get_last_error: symbol(&library, "PassThruGetLastError")?,
            ioctl: symbol(&library, "PassThruIoctl")?,
            _library: library,
        })
    }

    /// The driver's description of the most recent failure.
    fn last_error(&self) -> String {
        let mut text = [0 as c_char; 80];
        // SAFETY: the spec requires an 80-character buffer.
        let status = unsafe { (self.get_last_error)(text.as_mut_ptr()) };
        if status == STATUS_NOERROR {
            c_text(&text)
        } else {
            String::new()
        }
    }

    fn check(&self, call: &'static str, status: i32) -> Result<(), DiagError> {
        if status == STATUS_NOERROR {
            Ok(())
        } else {
            Err(DiagError::PassThru {
                call,
                code: status,
                detail: self.last_error(),
            })
        }
    }

    // ------------------------------------------------------------ devices

    /// Every device the library can reach right now.
    pub fn scan_devices(&self) -> Result<Vec<DeviceInfo>, DiagError> {
        let mut count = 0u32;
        // SAFETY: valid out-pointer.
        self.check("PassThruScanForDevices", unsafe {
            (self.scan_for_devices)(&mut count)
        })?;

        let mut devices = Vec::new();
        for _ in 0..count {
            // SAFETY: SDevice is plain data; all-zero is a valid value.
            let mut device: SDevice = unsafe { std::mem::zeroed() };
            // SAFETY: valid out-pointer to a correctly sized struct.
            let status = unsafe { (self.get_next_device)(&mut device) };
            if status == ERR_BUFFER_EMPTY {
                break;
            }
            self.check("PassThruGetNextDevice", status)?;
            let name = device.device_name;
            devices.push(DeviceInfo {
                name: c_text(&name),
                available: device.device_available,
                dll_fw_status: device.device_dll_fw_status,
            });
        }
        Ok(devices)
    }

    /// Opens the named device (the `DeviceName` from a scan).
    pub fn open(&self, device_name: &str) -> Result<u32, DiagError> {
        let name = CString::new(format!("J2534-1:{device_name}"))
            .map_err(|_| DiagError::Adapter("Device name contains a NUL byte".into()))?;
        let mut device_id = 0u32;
        // SAFETY: NUL-terminated name and valid out-pointer.
        self.check("PassThruOpen", unsafe {
            (self.open)(name.as_ptr(), &mut device_id)
        })?;
        Ok(device_id)
    }

    pub fn close(&self, device_id: u32) -> Result<(), DiagError> {
        // SAFETY: plain value argument.
        self.check("PassThruClose", unsafe { (self.close)(device_id) })
    }

    /// Firmware, DLL and API version strings.
    pub fn read_version(&self, device_id: u32) -> Result<(String, String, String), DiagError> {
        let mut firmware = [0 as c_char; 80];
        let mut dll = [0 as c_char; 80];
        let mut api = [0 as c_char; 80];
        // SAFETY: three 80-character buffers, as the spec requires.
        self.check("PassThruReadVersion", unsafe {
            (self.read_version)(
                device_id,
                firmware.as_mut_ptr(),
                dll.as_mut_ptr(),
                api.as_mut_ptr(),
            )
        })?;
        Ok((c_text(&firmware), c_text(&dll), c_text(&api)))
    }

    // ----------------------------------------------------------- channels

    /// Connects a physical channel on the J1962 connector.
    pub fn connect(
        &self,
        device_id: u32,
        protocol_id: u32,
        flags: u32,
        baud_rate: u32,
        pins: &[u32],
    ) -> Result<u32, DiagError> {
        let mut pins = pins.to_vec();
        let resources = ResourceStruct {
            connector: J1962_CONNECTOR,
            num_of_resources: pins.len() as u32,
            resource_list_ptr: pins.as_mut_ptr(),
        };
        let mut channel_id = 0u32;
        // SAFETY: `pins` outlives the call; the struct is passed by value as
        // the spec declares.
        self.check("PassThruConnect", unsafe {
            (self.connect)(
                device_id,
                protocol_id,
                flags,
                baud_rate,
                resources,
                &mut channel_id,
            )
        })?;
        Ok(channel_id)
    }

    pub fn disconnect(&self, channel_id: u32) -> Result<(), DiagError> {
        // SAFETY: plain value argument.
        self.check("PassThruDisconnect", unsafe {
            (self.disconnect)(channel_id)
        })
    }

    pub fn logical_connect(
        &self,
        physical_channel_id: u32,
        protocol_id: u32,
        flags: u32,
        descriptor: &mut Iso15765ChannelDescriptor,
    ) -> Result<u32, DiagError> {
        let mut channel_id = 0u32;
        // SAFETY: the descriptor outlives the call.
        self.check("PassThruLogicalConnect", unsafe {
            (self.logical_connect)(
                physical_channel_id,
                protocol_id,
                flags,
                descriptor as *mut Iso15765ChannelDescriptor as *mut c_void,
                &mut channel_id,
            )
        })?;
        Ok(channel_id)
    }

    pub fn logical_disconnect(&self, channel_id: u32) -> Result<(), DiagError> {
        // SAFETY: plain value argument.
        self.check("PassThruLogicalDisconnect", unsafe {
            (self.logical_disconnect)(channel_id)
        })
    }

    // ----------------------------------------------------------- messages

    /// Queues one message for transmission. `ERR_BUFFER_FULL` comes back as
    /// an error for the caller to retry; it means the previous message on this
    /// channel has not gone out yet.
    pub fn queue(
        &self,
        channel_id: u32,
        protocol_id: u32,
        tx_flags: u32,
        data: &[u8],
    ) -> Result<(), DiagError> {
        let mut buffer = data.to_vec();
        let mut msg = PassThruMsg {
            protocol_id,
            msg_handle: 0,
            rx_status: 0,
            tx_flags,
            timestamp: 0,
            data_length: buffer.len() as u32,
            extra_data_index: buffer.len() as u32,
            data_buffer: buffer.as_mut_ptr(),
            data_buffer_size: buffer.len() as u32,
        };
        let mut count = 1u32;
        // SAFETY: one message whose buffer outlives the call.
        self.check("PassThruQueueMsgs", unsafe {
            (self.queue_msgs)(channel_id, &mut msg, &mut count)
        })?;
        if count != 1 {
            return Err(DiagError::Adapter(
                "PassThruQueueMsgs did not accept the message".into(),
            ));
        }
        Ok(())
    }

    /// Reads at most one message, waiting up to `timeout_ms` (capped at
    /// [`MAX_READ_TIMEOUT_MS`]). `None` when nothing arrived in time.
    pub fn read(&self, channel_id: u32, timeout_ms: u32) -> Result<Option<RxMsg>, DiagError> {
        let mut buffer = vec![0u8; MAX_MESSAGE_BYTES];
        let mut msg = PassThruMsg {
            protocol_id: 0,
            msg_handle: 0,
            rx_status: 0,
            tx_flags: 0,
            timestamp: 0,
            data_length: 0,
            extra_data_index: 0,
            data_buffer: buffer.as_mut_ptr(),
            data_buffer_size: buffer.len() as u32,
        };
        let mut count = 1u32;
        // SAFETY: one message slot whose buffer outlives the call.
        let status = unsafe {
            (self.read_msgs)(
                channel_id,
                &mut msg,
                &mut count,
                timeout_ms.min(MAX_READ_TIMEOUT_MS),
            )
        };
        let got_one = count == 1;
        match status {
            STATUS_NOERROR => {}
            ERR_BUFFER_EMPTY | ERR_TIMEOUT if !got_one => return Ok(None),
            ERR_BUFFER_EMPTY | ERR_TIMEOUT => {}
            other => self.check("PassThruReadMsgs", other)?,
        }
        if !got_one {
            return Ok(None);
        }
        let length = msg.data_length as usize;
        if length > buffer.len() || msg.extra_data_index as usize > length {
            return Err(DiagError::InvalidResponse(
                "J2534 returned invalid message lengths".into(),
            ));
        }
        buffer.truncate(length);
        Ok(Some(RxMsg {
            protocol_id: msg.protocol_id,
            rx_status: msg.rx_status,
            extra_data_index: msg.extra_data_index as usize,
            data: buffer,
        }))
    }

    /// Starts a pass filter on a physical channel; returns its filter id.
    pub fn start_pass_filter(
        &self,
        channel_id: u32,
        protocol_id: u32,
        tx_flags: u32,
        mask: &[u8],
        pattern: &[u8],
    ) -> Result<u32, DiagError> {
        let mut mask_buffer = mask.to_vec();
        let mut pattern_buffer = pattern.to_vec();
        let message = |buffer: &mut Vec<u8>| PassThruMsg {
            protocol_id,
            msg_handle: 0,
            rx_status: 0,
            tx_flags,
            timestamp: 0,
            data_length: buffer.len() as u32,
            extra_data_index: buffer.len() as u32,
            data_buffer: buffer.as_mut_ptr(),
            data_buffer_size: buffer.len() as u32,
        };
        let mut mask_msg = message(&mut mask_buffer);
        let mut pattern_msg = message(&mut pattern_buffer);
        let mut filter_id = 0u32;
        // SAFETY: both messages and their buffers outlive the call.
        self.check("PassThruStartMsgFilter", unsafe {
            (self.start_msg_filter)(
                channel_id,
                PASS_FILTER,
                &mut mask_msg,
                &mut pattern_msg,
                &mut filter_id,
            )
        })?;
        Ok(filter_id)
    }

    pub fn stop_filter(&self, channel_id: u32, filter_id: u32) -> Result<(), DiagError> {
        // SAFETY: plain value arguments.
        self.check("PassThruStopMsgFilter", unsafe {
            (self.stop_msg_filter)(channel_id, filter_id)
        })
    }

    /// Replace the PWM functional addresses acknowledged by the adapter's IFR.
    pub fn set_functional_addresses(
        &self,
        channel_id: u32,
        addresses: &[u8],
    ) -> Result<(), DiagError> {
        if addresses.len() > 8 {
            return Err(DiagError::InvalidCommand(
                "J1850 PWM supports at most eight functional addresses".into(),
            ));
        }
        // SAFETY: this ioctl takes no input or output buffer.
        self.check("PassThruIoctl(CLEAR_FUNCT_MSG_LOOKUP_TABLE)", unsafe {
            (self.ioctl)(
                channel_id,
                CLEAR_FUNCT_MSG_LOOKUP_TABLE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        })?;
        if addresses.is_empty() {
            return Ok(());
        }
        let mut bytes = addresses.to_vec();
        let mut input = SByteArray {
            num_of_bytes: bytes.len() as u32,
            byte_ptr: bytes.as_mut_ptr(),
        };
        // SAFETY: the packed array and its bytes outlive this blocking call.
        self.check("PassThruIoctl(ADD_TO_FUNCT_MSG_LOOKUP_TABLE)", unsafe {
            (self.ioctl)(
                channel_id,
                ADD_TO_FUNCT_MSG_LOOKUP_TABLE,
                &mut input as *mut SByteArray as *mut c_void,
                std::ptr::null_mut(),
            )
        })
    }

    /// The adapter owns the address waveform and keyword handshake.
    pub fn five_baud_init(&self, channel_id: u32, address: u8) -> Result<[u8; 2], DiagError> {
        let mut address = [address];
        let mut keys = [0u8; 2];
        let mut input = SByteArray {
            num_of_bytes: 1,
            byte_ptr: address.as_mut_ptr(),
        };
        let mut output = SByteArray {
            num_of_bytes: 2,
            byte_ptr: keys.as_mut_ptr(),
        };
        // SAFETY: both packed arrays and their buffers outlive the blocking call.
        self.check("PassThruIoctl(FIVE_BAUD_INIT)", unsafe {
            (self.ioctl)(
                channel_id,
                FIVE_BAUD_INIT,
                &mut input as *mut SByteArray as *mut c_void,
                &mut output as *mut SByteArray as *mut c_void,
            )
        })?;
        if output.num_of_bytes != 2 {
            return Err(DiagError::InvalidResponse(
                "5-baud initialization did not return two key bytes".into(),
            ));
        }
        Ok(keys)
    }

    /// The adapter generates the wake-up pulse and sends the supplied message.
    pub fn fast_init(
        &self,
        channel_id: u32,
        protocol_id: u32,
        request: &[u8],
    ) -> Result<RxMsg, DiagError> {
        if request.is_empty() || request.len() > MAX_MESSAGE_BYTES {
            return Err(DiagError::InvalidCommand(
                "FAST_INIT requires a nonempty request that fits the J2534 buffer".into(),
            ));
        }
        let mut request = request.to_vec();
        let mut buffer = vec![0u8; MAX_MESSAGE_BYTES];
        let mut input = PassThruMsg {
            protocol_id,
            msg_handle: 0,
            rx_status: 0,
            tx_flags: 0,
            timestamp: 0,
            data_length: request.len() as u32,
            extra_data_index: request.len() as u32,
            data_buffer: request.as_mut_ptr(),
            data_buffer_size: request.len() as u32,
        };
        let mut output = PassThruMsg {
            protocol_id,
            msg_handle: 0,
            rx_status: 0,
            tx_flags: 0,
            timestamp: 0,
            data_length: 0,
            extra_data_index: 0,
            data_buffer: buffer.as_mut_ptr(),
            data_buffer_size: buffer.len() as u32,
        };
        // SAFETY: both packed messages and their buffers outlive this blocking call.
        self.check("PassThruIoctl(FAST_INIT)", unsafe {
            (self.ioctl)(
                channel_id,
                FAST_INIT,
                &mut input as *mut PassThruMsg as *mut c_void,
                &mut output as *mut PassThruMsg as *mut c_void,
            )
        })?;
        let length = output.data_length as usize;
        if length > buffer.len() || output.extra_data_index as usize > length {
            return Err(DiagError::InvalidResponse(
                "J2534 FAST_INIT returned invalid message lengths".into(),
            ));
        }
        buffer.truncate(length);
        Ok(RxMsg {
            protocol_id: output.protocol_id,
            rx_status: output.rx_status,
            extra_data_index: output.extra_data_index as usize,
            data: buffer,
        })
    }

    /// Sets programming voltage (mV), [`PIN_OFF`] or [`SHORT_TO_GROUND`] on a
    /// J1962 pin.
    pub fn set_programming_voltage(
        &self,
        device_id: u32,
        pin: u32,
        voltage: u32,
    ) -> Result<(), DiagError> {
        let mut pins = [pin];
        let resources = ResourceStruct {
            connector: J1962_CONNECTOR,
            num_of_resources: 1,
            resource_list_ptr: pins.as_mut_ptr(),
        };
        // SAFETY: `pins` outlives the call; struct passed by value per the spec.
        self.check("PassThruSetProgrammingVoltage", unsafe {
            (self.set_programming_voltage)(device_id, resources, voltage)
        })
    }

    /// Voltage on a J1962 pin, in mV (pin 16 is battery).
    pub fn read_pin_voltage(&self, device_id: u32, pin: u32) -> Result<u32, DiagError> {
        let mut pins = [pin];
        let mut resources = ResourceStruct {
            connector: J1962_CONNECTOR,
            num_of_resources: 1,
            resource_list_ptr: pins.as_mut_ptr(),
        };
        let mut millivolts = 0u32;
        // SAFETY: input struct, its pin list and the output all outlive the call.
        self.check("PassThruIoctl(READ_PIN_VOLTAGE)", unsafe {
            (self.ioctl)(
                device_id,
                READ_PIN_VOLTAGE,
                &mut resources as *mut ResourceStruct as *mut c_void,
                &mut millivolts as *mut u32 as *mut c_void,
            )
        })?;
        Ok(millivolts)
    }

    /// The programming voltage actually measured on the active pin, in mV.
    pub fn read_programming_voltage(&self, device_id: u32) -> Result<u32, DiagError> {
        let mut millivolts = 0u32;
        // SAFETY: NULL input as the spec requires, valid output.
        self.check("PassThruIoctl(READ_PROG_VOLTAGE)", unsafe {
            (self.ioctl)(
                device_id,
                READ_PROG_VOLTAGE,
                std::ptr::null_mut(),
                &mut millivolts as *mut u32 as *mut c_void,
            )
        })?;
        Ok(millivolts)
    }

    // ------------------------------------------------------- channel ioctls

    /// `CLEAR_TX_QUEUE` or `CLEAR_RX_QUEUE` on a channel.
    pub fn clear_queue(&self, channel_id: u32, ioctl_id: u32) -> Result<(), DiagError> {
        // SAFETY: both pointers NULL, as the spec requires for these IOCTLs.
        self.check("PassThruIoctl(CLEAR_QUEUE)", unsafe {
            (self.ioctl)(
                channel_id,
                ioctl_id,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        })
    }

    /// `SET_CONFIG` with the given (parameter, value) pairs.
    pub fn set_config(&self, channel_id: u32, params: &[(u32, u32)]) -> Result<(), DiagError> {
        let mut configs: Vec<SConfig> = params
            .iter()
            .map(|&(parameter, value)| SConfig { parameter, value })
            .collect();
        let mut list = SConfigList {
            num_of_params: configs.len() as u32,
            config_ptr: configs.as_mut_ptr(),
        };
        // SAFETY: list and entries outlive the call; output is NULL per the spec.
        self.check("PassThruIoctl(SET_CONFIG)", unsafe {
            (self.ioctl)(
                channel_id,
                SET_CONFIG,
                &mut list as *mut SConfigList as *mut c_void,
                std::ptr::null_mut(),
            )
        })
    }

    /// `GET_DEVICE_INFO` for one capability parameter (e.g. [`CAN_SUPPORTED`]).
    /// `Ok(None)` when the library doesn't implement the IOCTL — it is a
    /// J2534-2 extension, so many adapters won't.
    pub fn device_supports(
        &self,
        device_id: u32,
        parameter: u32,
    ) -> Result<Option<bool>, DiagError> {
        // Asking about the whole device rather than one connector resource.
        let mut param = SParam {
            parameter,
            value: ENTIRE_DEVICE,
            supported: 0,
        };
        let mut list = SParamList {
            num_of_params: 1,
            param_ptr: &mut param,
        };
        // SAFETY: input NULL as required; the list and its entry are the output
        // and outlive the call.
        let status = unsafe {
            (self.ioctl)(
                device_id,
                GET_DEVICE_INFO,
                std::ptr::null_mut(),
                &mut list as *mut SParamList as *mut c_void,
            )
        };
        if matches!(
            status,
            ERR_NOT_SUPPORTED | ERR_IOCTL_ID_NOT_SUPPORTED | ERR_IOCTL_PARAM_ID_NOT_SUPPORTED
        ) {
            return Ok(None);
        }
        self.check("PassThruIoctl(GET_DEVICE_INFO)", status)?;
        let supported = param.supported;
        let value = param.value;
        Ok(Some(supported != 0 && value != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    const PTR: usize = size_of::<*const u8>();

    /// The same layout checks the OpenDiag driver asserts in
    /// `tests/api0500.cpp`. A mismatch here would corrupt every call.
    #[test]
    fn struct_layouts_match_the_spec() {
        assert_eq!(size_of::<PassThruMsg>(), 32 + PTR);
        assert_eq!(size_of::<ResourceStruct>(), 8 + PTR);
        assert_eq!(size_of::<Iso15765ChannelDescriptor>(), 18);
        assert_eq!(size_of::<SDevice>(), 80 + 6 * 4);
        assert_eq!(size_of::<SConfig>(), 8);
        assert_eq!(size_of::<SConfigList>(), 4 + PTR);
        assert_eq!(size_of::<SByteArray>(), 4 + PTR);
        assert_eq!(size_of::<SParam>(), 12);
        assert_eq!(size_of::<SParamList>(), 4 + PTR);
    }

    #[test]
    fn c_text_stops_at_the_first_nul() {
        let mut buffer = [0 as c_char; 8];
        for (slot, byte) in buffer.iter_mut().zip(b"abc\0def") {
            *slot = *byte as c_char;
        }
        assert_eq!(c_text(&buffer), "abc");
    }

    /// End-to-end check against a real v05.00 library, including the two calls
    /// that pass `RESOURCE_STRUCT` by value. Nothing is energised, so it is safe
    /// to run plugged into a car.
    ///
    /// Run with `OPENDIAG_J2534_LIB=<library>` (and, for OpenDiag,
    /// `OPENDIAG_DRIVER_CONFIG=<opendiag.ini>`): `cargo test -- --ignored --test-threads=1`
    /// (the adapter allows one open at a time).
    #[test]
    #[ignore = "needs a J2534 library and an adapter (or the OpenDiag firmware peer)"]
    fn talks_to_a_real_library() {
        let path = std::env::var("OPENDIAG_J2534_LIB").expect("set OPENDIAG_J2534_LIB");
        let lib = PassThruLib::load(Path::new(&path)).unwrap();

        let devices = lib.scan_devices().unwrap();
        assert!(
            !devices.is_empty(),
            "no device found; check the driver's configuration"
        );
        let device = lib.open(&devices[0].name).unwrap();

        let (firmware, dll, api) = lib.read_version(device).unwrap();
        println!("firmware {firmware:?}, dll {dll:?}, api {api:?}");
        println!("battery {} mV", lib.read_pin_voltage(device, 16).unwrap());

        // RESOURCE_STRUCT by value: if the driver misread it, the valid and the
        // invalid pin lists could not give opposite results.
        for baud in [500_000, 250_000] {
            let channel = lib
                .connect(device, PROTOCOL_CAN, CAN_29BIT_ID, baud, &[6, 14])
                .unwrap();
            lib.disconnect(channel).unwrap();
        }
        let err = lib
            .connect(device, PROTOCOL_CAN, CAN_29BIT_ID, 500_000, &[6, 13])
            .unwrap_err();
        assert!(
            matches!(
                err,
                DiagError::PassThru {
                    code: ERR_PIN_NOT_SUPPORTED,
                    ..
                }
            ),
            "{err}"
        );

        lib.set_programming_voltage(device, 12, PIN_OFF).unwrap();
        let err = lib
            .set_programming_voltage(device, 12, SHORT_TO_GROUND)
            .unwrap_err();
        assert!(
            matches!(
                err,
                DiagError::PassThru {
                    code: ERR_PIN_NOT_SUPPORTED,
                    ..
                }
            ),
            "ground is pin 15 only: {err}"
        );

        lib.close(device).unwrap();
    }

    #[test]
    fn loading_a_missing_library_names_the_path() {
        let err = PassThruLib::load(Path::new("/nonexistent/libnothing.dylib"))
            .err()
            .unwrap();
        assert!(
            err.to_string().contains("/nonexistent/libnothing.dylib"),
            "{err}"
        );
    }
}
