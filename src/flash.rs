use crate::obd::protocol::ObdProtocol;
use crate::errors::DiagError;
use std::path::PathBuf;

pub mod honda_can;
pub mod parsers;

/// Options that come from the CLI rather than from the firmware image itself.
///
/// Container formats like `.rwd` embed everything they need, so these are all
/// optional there. Raw `.bin` images carry no metadata, so `start_address` and a
/// source for the security-access secret must be supplied out-of-band.
#[derive(Debug, Clone, Default)]
pub struct FlashOptions {
    /// Download start address. Required for raw `.bin`; overrides the embedded
    /// value for container formats when set.
    pub start_address: Option<u32>,
    /// Explicit security-access secret (the K1/K2/K3 constants), already decoded
    /// from hex. Highest-priority source; wins over any other.
    pub security_key: Option<Vec<u8>>,
    /// Path to a companion `.rwd` whose token table supplies the security secret,
    /// matched against the live ECU calibration id.
    pub tokens: Option<PathBuf>,
}

/// Trait representing a flash-capable ECU strategy. For now it is minimal.
pub trait FlashStrategy {
    fn ecu_type(&self) -> &'static str;
    fn info(&mut self) -> Result<String, DiagError>; // identification info read from the ECU
    fn dry_run(&mut self, image_path: &std::path::Path, opts: &FlashOptions) -> Result<(), DiagError>;
    fn flash(&mut self, image_path: &std::path::Path, opts: &FlashOptions) -> Result<(), DiagError>;
}

/// Trait for strategy factories that can create instances of FlashStrategy
pub trait StrategyFactory: Send + Sync {
    fn ecu_type(&self) -> &'static str;
    fn create_strategy<'a>(&self, protocol: &'a mut ObdProtocol) -> Box<dyn FlashStrategy + 'a>;
}

/// Registry of all available flash strategies
pub struct StrategyRegistry {
    factories: Vec<Box<dyn StrategyFactory>>,
}

impl StrategyRegistry {
    /// Create a new registry with all available strategies
    pub fn new() -> Self {
        let mut registry = Self {
            factories: Vec::new(),
        };
        
        // Register all available strategies here
        registry.register(Box::new(honda_can::HondaCanBusStrategyFactory));
        
        registry
    }
    
    /// Register a new strategy factory
    pub fn register(&mut self, factory: Box<dyn StrategyFactory>) {
        self.factories.push(factory);
    }
    
    /// Get a list of all supported ECU types
    pub fn get_supported_ecus(&self) -> Vec<&'static str> {
        self.factories.iter().map(|f| f.ecu_type()).collect()
    }
    
    /// Check if an ECU type is supported
    pub fn is_supported_ecu(&self, ecu: &str) -> bool {
        self.factories.iter().any(|f| f.ecu_type().eq_ignore_ascii_case(ecu))
    }
    
    /// Create a strategy instance for the given ECU type
    pub fn create_strategy<'a>(
        &self,
        name: &str,
        protocol: &'a mut ObdProtocol,
    ) -> Option<Box<dyn FlashStrategy + 'a>> {
        self.factories
            .iter()
            .find(|f| f.ecu_type().eq_ignore_ascii_case(name))
            .map(|f| f.create_strategy(protocol))
    }
}

/// Global registry instance using OnceLock for thread-safe initialization
static REGISTRY: std::sync::OnceLock<StrategyRegistry> = std::sync::OnceLock::new();

/// Get the global strategy registry
pub fn get_registry() -> &'static StrategyRegistry {
    REGISTRY.get_or_init(|| StrategyRegistry::new())
}

/// Factory for strategies (backward compatibility)
pub fn get_strategy<'a>(
    name: &str,
    protocol: &'a mut ObdProtocol,
) -> Option<Box<dyn FlashStrategy + 'a>> {
    get_registry().create_strategy(name, protocol)
}

/// Get list of supported ECU types
pub fn get_supported_ecus() -> Vec<&'static str> {
    get_registry().get_supported_ecus()
}

/// Check if an ECU type is supported
pub fn is_supported_ecu(ecu: &str) -> bool {
    get_registry().is_supported_ecu(ecu)
}
