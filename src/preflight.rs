use crate::{
    events::{Cancel, Interaction},
    DiagError,
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
