mod honda;

use std::collections::HashMap;
use crate::obd::protocol::ObdProtocol;
use crate::errors::DiagError;

// Type alias for service functions
type ServiceFn = fn(&mut ObdProtocol) -> Result<(), DiagError>;

// Supported cars and their services
pub fn get_supported_cars() -> Vec<&'static str> {
    vec!["honda_odyssey_rl3"]
}

pub fn get_services_for_car(car: &str) -> Option<HashMap<&'static str, ServiceFn>> {
    match car {
        "honda_odyssey_rl3" => {
            let mut services = HashMap::new();
            services.insert("pair-immobilizer", honda::odyssey_rl3::pair_immobilizer as ServiceFn);
            services.insert("abs-bleed", honda::odyssey_rl3::abs_bleed as ServiceFn);
            services.insert("relearn-idle", honda::odyssey_rl3::relearn_idle as ServiceFn);
            Some(services)
        }
        _ => None,
    }
}

pub fn car_to_ecu(car: &str) -> Option<&'static str> {
    match car {
        "honda_odyssey_rl3" => Some("HONDA_CAN"),
        _ => None,
    }
}
