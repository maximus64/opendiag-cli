pub mod honda;
use crate::{
    diag::SeedKey,
    events::{Cancel, Interaction},
    DiagError, ObdProtocol,
};
use std::collections::BTreeMap;
pub type Procedure = fn(&mut ObdProtocol, &mut dyn Interaction, &Cancel) -> Result<(), DiagError>;
pub type KeyAlgorithm = fn(&[u8]) -> Result<Vec<u8>, DiagError>;
#[derive(Clone)]
pub struct Hooks {
    procedures: BTreeMap<String, (Procedure, bool)>,
    algorithms: BTreeMap<String, KeyAlgorithm>,
}
impl Default for Hooks {
    fn default() -> Self {
        let mut hooks = Self {
            procedures: BTreeMap::new(),
            algorithms: BTreeMap::new(),
        };
        hooks.register_procedure("honda::relearn_idle", honda::relearn_idle, false);
        hooks
    }
}
struct Algorithm(KeyAlgorithm);
impl SeedKey for Algorithm {
    fn key(&self, seed: &[u8]) -> Result<Vec<u8>, DiagError> {
        (self.0)(seed)
    }
}
impl Hooks {
    pub fn register_procedure(&mut self, name: &str, function: Procedure, implemented: bool) {
        self.procedures.insert(name.into(), (function, implemented));
    }
    pub fn register_algorithm(&mut self, name: &str, function: KeyAlgorithm) {
        self.algorithms.insert(name.into(), function);
    }
    pub fn contains_procedure(&self, name: &str) -> bool {
        self.procedures.contains_key(name)
    }
    pub fn contains_algorithm(&self, name: &str) -> bool {
        self.algorithms.contains_key(name)
    }
    pub fn implemented(&self, name: &str) -> bool {
        self.procedures
            .get(name)
            .is_some_and(|(_, implemented)| *implemented)
    }
    pub fn algorithm(&self, name: &str) -> Result<Box<dyn SeedKey>, DiagError> {
        self.algorithms
            .get(name)
            .map(|f| Box::new(Algorithm(*f)) as Box<dyn SeedKey>)
            .ok_or_else(|| {
                DiagError::Unsupported(format!("Security algorithm {name} needs a newer opendiag"))
            })
    }
    pub fn run(
        &self,
        name: &str,
        protocol: &mut ObdProtocol,
        ui: &mut dyn Interaction,
        cancel: &Cancel,
    ) -> Result<(), DiagError> {
        self.procedures
            .get(name)
            .ok_or_else(|| {
                DiagError::Unsupported(format!("Procedure {name} needs a newer opendiag"))
            })?
            .0(protocol, ui, cancel)
    }
}
