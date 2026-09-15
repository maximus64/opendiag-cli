//! An open J2534 v05.00 device: the [`Adapter`] and [`DeviceOps`] the rest of
//! the tool talks to.

use crate::events::{Event, Events, NoEvents};
use std::cell::RefCell;
use std::rc::Rc;

use crate::errors::DiagError;
use crate::j2534::consts::*;
use crate::j2534::discovery::{self, Selection};
use crate::j2534::v0500::can::CanSession;
use crate::j2534::v0500::ffi::PassThruLib;
use crate::j2534::v0500::j1850::J1850Session;
use crate::j2534::v0500::kline::KLineSession;
use crate::link::{Adapter, BusKind, BusSpec, DeviceOps, DiagLink};

/// Owns the library and the device handle; closing happens when the last
/// [`Device`] clone (including those held by open sessions) is dropped, so a
/// session can never outlive the device it runs on.
pub(crate) struct Inner {
    pub(crate) lib: PassThruLib,
    pub(crate) id: u32,
    name: String,
    declared_protocols: Vec<(String, bool)>,
    events: RefCell<Box<dyn Events>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Err(e) = self.lib.close(self.id) {
            self.events
                .borrow_mut()
                .emit(Event::Warning(&format!("closing {}: {}", self.name, e)));
        }
    }
}

/// A cheap, cloneable handle to an open device.
#[derive(Clone)]
pub struct Device {
    pub(crate) inner: Rc<Inner>,
}

impl Device {
    /// Loads the selected library, finds the device and opens it.
    pub fn open(selection: &Selection) -> Result<Device, DiagError> {
        Self::open_with_events(selection, Box::new(NoEvents))
    }
    pub fn open_with_events(
        selection: &Selection,
        mut events: Box<dyn Events>,
    ) -> Result<Device, DiagError> {
        if let Some(path) = &selection.driver_config {
            std::env::set_var(discovery::OPENDIAG_DRIVER_CONFIG_ENV, path);
        }
        let lib = PassThruLib::load(&selection.library)?;

        let devices = lib.scan_devices()?;
        if devices.is_empty() {
            return Err(DiagError::Adapter(format!(
                "The driver {} found no device. Check that the adapter is connected and the driver is configured{}.",
                selection.library.display(),
                opendiag_hint(selection)
            )));
        }
        let chosen = discovery::pick_device(&devices, selection.device.as_deref())
            .map_err(DiagError::Adapter)?;
        let name = chosen.name.clone();
        if let Some(problem) = compatibility_problem(chosen.dll_fw_status) {
            events.emit(Event::Warning(&format!("{name}: {problem}")));
        }

        let id = lib.open(&name).map_err(with_open_hint)?;
        Ok(Device {
            inner: Rc::new(Inner {
                lib,
                id,
                name,
                declared_protocols: selection.declared_protocols.clone(),
                events: RefCell::new(events),
            }),
        })
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }
    pub fn emit(&self, event: Event<'_>) {
        self.inner.events.borrow_mut().emit(event);
    }

    /// Firmware, DLL and API version strings.
    pub fn version(&self) -> Result<(String, String, String), DiagError> {
        self.inner.lib.read_version(self.inner.id)
    }
}

/// The driver's own verdict on whether it and the adapter firmware match.
pub fn compatibility_problem(dll_fw_status: u32) -> Option<&'static str> {
    match dll_fw_status {
        2 => Some("the driver and adapter firmware are not compatible"),
        3 => Some("the driver is not compatible with this adapter firmware; update the driver"),
        4 => Some("the adapter firmware is not compatible with this driver; update the firmware"),
        _ => None, // 0 unknown, 1 compatible
    }
}

/// For OpenDiag: where its driver reads its settings, since a missing or wrong
/// `opendiag.ini` is the usual reason a scan comes back empty.
fn opendiag_hint(selection: &Selection) -> String {
    let env = std::env::var(discovery::OPENDIAG_DRIVER_CONFIG_ENV).ok();
    match discovery::opendiag_ini_path(selection.driver_config.as_deref(), env.as_deref()) {
        (Some(path), why) => format!(" (OpenDiag reads {} — {})", path.display(), why),
        (None, _) => String::new(),
    }
}

/// An open that times out or can't reach the adapter is most often another
/// client holding OpenDiag's single data link.
fn with_open_hint(error: DiagError) -> DiagError {
    match error {
        DiagError::PassThru { call, code, detail }
            if matches!(
                code,
                ERR_DEVICE_NOT_CONNECTED | ERR_TIMEOUT | ERR_OPEN_FAILED
            ) =>
        {
            DiagError::PassThru {
                call,
                code,
                detail: format!(
                    "{}{}If another program or a phone app is connected to the adapter, close it \
                     (OpenDiag: `link drop` in the adapter's debug shell).",
                    detail,
                    if detail.is_empty() { "" } else { ". " }
                ),
            }
        }
        other => other,
    }
}

impl DeviceOps for Device {
    fn set_programming_voltage(&self, pin: u32, millivolts: u32) -> Result<(), DiagError> {
        self.inner
            .lib
            .set_programming_voltage(self.inner.id, pin, millivolts)
    }

    fn read_battery_mv(&self) -> Result<u32, DiagError> {
        self.inner.lib.read_pin_voltage(self.inner.id, 16)
    }

    fn read_programming_voltage_mv(&self) -> Result<u32, DiagError> {
        self.inner.lib.read_programming_voltage(self.inner.id)
    }
}

impl Adapter for Device {
    fn supports(&self, bus: &BusSpec) -> Option<bool> {
        let (capability, names): (_, &[&str]) = match bus.kind {
            BusKind::IsoTp { .. } => (CAN_SUPPORTED, &["ISO15765", "CAN"]),
            BusKind::Iso9141 => (ISO9141_SUPPORTED, &["ISO9141"]),
            BusKind::Iso14230 { .. } => (ISO14230_SUPPORTED, &["ISO14230"]),
            BusKind::J1850Pwm => (J1850PWM_SUPPORTED, &["J1850PWM"]),
            BusKind::J1850Vpw => (J1850VPW_SUPPORTED, &["J1850VPW"]),
        };
        match self.inner.lib.device_supports(self.inner.id, capability) {
            Ok(Some(answer)) => Some(answer),
            // Not reported by the driver: fall back to what the registry declares.
            _ => self
                .inner
                .declared_protocols
                .iter()
                .find(|(name, _)| names.contains(&name.as_str()))
                .map(|&(_, on)| on),
        }
    }

    fn connect(&self, bus: &BusSpec) -> Result<Box<dyn DiagLink>, DiagError> {
        if self.supports(bus) == Some(false) {
            return Err(DiagError::Adapter(format!(
                "{} can't open {}",
                self.inner.name, bus
            )));
        }
        match bus.kind {
            BusKind::IsoTp { .. } => Ok(Box::new(CanSession::open(self.clone(), bus.clone())?)),
            BusKind::Iso9141 | BusKind::Iso14230 { .. } => {
                Ok(Box::new(KLineSession::open(self.clone(), bus.clone())?))
            }
            BusKind::J1850Pwm | BusKind::J1850Vpw => {
                Ok(Box::new(J1850Session::open(self.clone(), bus.clone())?))
            }
        }
    }
}
