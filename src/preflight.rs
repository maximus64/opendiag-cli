use crate::{
    events::{Cancel, Interaction},
    vehicle::SystemVoltage,
    DiagError, ObdProtocol,
};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Preflight {
    pub operation: String,
    pub checks: Vec<Check>,
    pub risk: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub value: String,
    pub status: CheckStatus,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CheckStatus {
    Ok,
    Info,
    Warn,
    Fail,
}
impl Preflight {
    pub fn new(operation: impl Into<String>, risk: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            checks: Vec::new(),
            risk: risk.into(),
        }
    }
    pub fn add(&mut self, name: impl Into<String>, value: impl Into<String>, status: CheckStatus) {
        self.checks.push(Check {
            name: name.into(),
            value: value.into(),
            status,
        });
    }
    pub fn validate(&self) -> Result<(), DiagError> {
        let failures: Vec<_> = self
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .map(|c| format!("{}: {}", c.name, c.value))
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(DiagError::PreflightFailed(failures.join("; ")))
        }
    }
    pub fn authorize(
        &self,
        interaction: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        interaction.report(self);
        self.validate()?;
        cancel.check()?;
        if interaction.confirm(self) {
            cancel.check()
        } else {
            Err(DiagError::Cancelled)
        }
    }
}

// Application policy per 12 V of nominal lead-acid battery voltage.
// These are not ECU-specific programming limits.
const BATTERY_MIN_MV: u32 = 12_000;
const BATTERY_RECOMMENDED_MIN_MV: u32 = 12_400;
const BATTERY_MAX_MV: u32 = 15_000;

/// The battery check, with a failure reported as a warning when the caller has
/// declared a supply outside the battery limits acceptable.
pub(crate) fn battery_check(
    protocol: &mut ObdProtocol,
    system: SystemVoltage,
    skip: bool,
) -> Result<(String, CheckStatus), DiagError> {
    let (text, status) = battery_limits(protocol, system)?;
    if skip && status == CheckStatus::Fail {
        return Ok((
            format!("{text}; not enforced by request"),
            CheckStatus::Warn,
        ));
    }
    Ok((text, status))
}

fn battery_limits(
    protocol: &mut ObdProtocol,
    system: SystemVoltage,
) -> Result<(String, CheckStatus), DiagError> {
    let nominal = system.volts();
    let scale = u32::from(nominal) / 12;
    let minimum = BATTERY_MIN_MV * scale;
    let recommended_minimum = BATTERY_RECOMMENDED_MIN_MV * scale;
    let maximum = BATTERY_MAX_MV * scale;
    let mv = match protocol.read_battery_mv() {
        Ok(mv) => mv,
        Err(error) if error.kind() == crate::errors::ErrorKind::Unsupported => {
            return Ok((
                format!("unavailable: {error}; verify supply voltage for the {nominal} V system before programming"),
                CheckStatus::Warn,
            ));
        }
        Err(error) => return Err(error),
    };
    let voltage = mv as f64 / 1000.0;
    if mv < minimum {
        return Ok((
            format!(
                "{voltage:.2} V (below {:.2} V minimum for a {nominal} V system; charge the battery or connect a regulated programming power supply)",
                minimum as f64 / 1000.0
            ),
            CheckStatus::Fail,
        ));
    }
    if mv > maximum {
        return Ok((
            format!(
                "{voltage:.2} V (above {:.2} V maximum for a {nominal} V system; check the supply voltage)",
                maximum as f64 / 1000.0
            ),
            CheckStatus::Fail,
        ));
    }
    if mv < recommended_minimum {
        return Ok((
            format!("{voltage:.2} V (low margin for a {nominal} V system; charge the battery or connect a regulated programming power supply)"),
            CheckStatus::Warn,
        ));
    }
    Ok((
        format!(
            "{voltage:.2} V (within {:.2}-{:.2} V preflight range for a {nominal} V system)",
            recommended_minimum as f64 / 1000.0,
            maximum as f64 / 1000.0
        ),
        CheckStatus::Ok,
    ))
}
