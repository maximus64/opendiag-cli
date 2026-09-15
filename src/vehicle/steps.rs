use super::{ecu_def::EcuDef, hooks::Hooks};
use crate::{
    diag::{self, DataId, ResetKind, RoutineOp, Session},
    events::{Answer, Cancel, Interaction},
    DiagError, ObdProtocol,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SessionValue {
    Name(String),
    Id(u8),
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub prompt: Option<String>,
    #[serde(default)]
    pub translations: BTreeMap<String, String>,
    pub session: Option<SessionValue>,
    pub security: Option<u8>,
    pub routine_start: Option<u16>,
    pub routine_results: Option<u16>,
    pub write_data: Option<u16>,
    pub ecu_reset: Option<String>,
    pub request: Option<String>,
    pub expect: Option<String>,
    pub data: Option<String>,
    #[serde(default)]
    pub local: bool,
    pub wait_ms: Option<u64>,
}
impl Step {
    pub fn mutates(&self) -> bool {
        self.security.is_some()
            || self.routine_start.is_some()
            || self.write_data.is_some()
            || self.ecu_reset.is_some()
            || self.request.is_some()
            || matches!(self.session, Some(SessionValue::Name(ref s)) if s != "default")
            || matches!(self.session, Some(SessionValue::Id(id)) if id != 1)
    }
    pub fn validate(&self, definition: &EcuDef, hooks: &Hooks) -> Result<(), String> {
        let count = [
            self.prompt.is_some(),
            self.session.is_some(),
            self.security.is_some(),
            self.routine_start.is_some(),
            self.routine_results.is_some(),
            self.write_data.is_some(),
            self.ecu_reset.is_some(),
            self.request.is_some(),
            self.wait_ms.is_some(),
        ]
        .into_iter()
        .filter(|v| *v)
        .count();
        if count != 1 {
            return Err("step must contain exactly one operation".into());
        }
        if !self.translations.is_empty() && self.prompt.is_none() {
            return Err("translations are only valid on prompt steps".into());
        }
        if self.prompt.as_ref().is_some_and(|s| s.trim().is_empty())
            || self
                .translations
                .iter()
                .any(|(locale, text)| locale.is_empty() || text.is_empty())
        {
            return Err("prompt and translations must be nonempty".into());
        }
        if self.data.is_some() && self.routine_start.is_none() && self.write_data.is_none() {
            return Err("data is only valid on routine_start or write_data".into());
        }
        if self.expect.is_some() != self.request.is_some() {
            return Err("request requires an expect prefix".into());
        }
        if self.local && self.write_data.is_none() {
            return Err("local is only valid on write_data".into());
        }
        if self.local && definition.diag.as_deref() == Some("uds") {
            return Err("UDS has no local identifiers".into());
        }
        if self.local && self.write_data.is_some_and(|id| id > 255) {
            return Err("local identifier exceeds a byte".into());
        }
        if let Some(level) = self.security {
            if level == 0 || level >= 0x7F || level % 2 == 0 {
                return Err("security level must be an odd seed subfunction in 1..0x7F".into());
            }
            let name = definition
                .security
                .get(&level.to_string())
                .ok_or("security level has no algorithm")?;
            if !hooks.contains_algorithm(name) {
                return Err(format!("unknown security algorithm {name}"));
            }
        }
        if let Some(SessionValue::Name(name)) = &self.session {
            if !matches!(name.as_str(), "default" | "programming" | "extended") {
                return Err(format!("unknown session {name}"));
            }
        }
        if let Some(kind) = &self.ecu_reset {
            if !matches!(kind.as_str(), "hard" | "key-off-on" | "soft") {
                return Err(format!("unknown ecu_reset {kind}"));
            }
        }
        for text in [&self.request, &self.expect, &self.data]
            .into_iter()
            .flatten()
        {
            let bytes = crate::utils::hex_to_bytes(text).map_err(|e| e.to_string())?;
            if bytes.is_empty() && self.data.as_ref() != Some(text) {
                return Err("request/expect must be nonempty".into());
            }
        }
        Ok(())
    }
    pub fn execute(
        &self,
        definition: &EcuDef,
        protocol: &mut ObdProtocol,
        hooks: &Hooks,
        ui: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        cancel.check()?;
        if let Some(prompt) = &self.prompt {
            return if matches!(ui.prompt(prompt), Answer::Continue) {
                cancel.check()
            } else {
                Err(DiagError::Cancelled)
            };
        }
        if let Some(ms) = self.wait_ms {
            cancel.wait(std::time::Duration::from_millis(ms));
            return cancel.check();
        }
        let mut service = diag::services(definition.diag.as_deref().unwrap_or("obd"), protocol)?;
        if let Some(session) = &self.session {
            return service.session(match session {
                SessionValue::Id(id) => Session::Numeric(*id),
                SessionValue::Name(name) => match name.as_str() {
                    "default" => Session::Default,
                    "programming" => Session::Programming,
                    _ => Session::Extended,
                },
            });
        }
        if let Some(level) = self.security {
            return service.security(
                level,
                &*hooks.algorithm(&definition.security[&level.to_string()])?,
            );
        }
        let data = self
            .data
            .as_ref()
            .map(|s| crate::utils::hex_to_bytes(s))
            .transpose()
            .map_err(|e| DiagError::InvalidCommand(e.to_string()))?
            .unwrap_or_default();
        if let Some(id) = self.routine_start {
            service.routine(RoutineOp::Start, id, &data)?;
        } else if let Some(id) = self.routine_results {
            service.routine(RoutineOp::Results, id, &[])?;
        } else if let Some(id) = self.write_data {
            service.write_data(
                if self.local {
                    DataId::Local(id as u8)
                } else {
                    DataId::Common(id)
                },
                &data,
            )?;
        } else if let Some(kind) = &self.ecu_reset {
            service.ecu_reset(match kind.as_str() {
                "hard" => ResetKind::Hard,
                "key-off-on" => ResetKind::KeyOffOn,
                _ => ResetKind::Soft,
            })?;
        } else if let Some(request) = &self.request {
            let bytes = crate::utils::hex_to_bytes(request)
                .map_err(|e| DiagError::InvalidCommand(e.to_string()))?;
            let expect = crate::utils::hex_to_bytes(
                self.expect
                    .as_ref()
                    .ok_or_else(|| DiagError::InvalidCommand("Missing expect prefix".into()))?,
            )
            .map_err(|e| DiagError::InvalidCommand(e.to_string()))?;
            service.request(&bytes, &expect)?;
        }
        Ok(())
    }
}
