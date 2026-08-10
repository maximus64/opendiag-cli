use std::collections::HashMap; 
use std::sync::LazyLock;

pub mod parsers;
pub mod standard;

#[derive(Debug, Clone, PartialEq)]
pub enum PidCategory {
    Engine,
    //FuelSystem,
    //Emissions,
    //Transmission,
    Chassis,
    //Body,
    Diagnostics,
    Environment,
    //ManufacturerSpecific,
}

/// Defines an OBD-II Parameter ID (PID).
#[derive(Clone)]
pub struct PidDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub unit: &'static str,
    pub category: PidCategory,
    /// A function pointer to the specific parser for this PID's response data.
    pub parser: fn(&[u8]) -> String,
}

static PID_DEFINITIONS: LazyLock<HashMap<u8, PidDefinition>> = LazyLock::new(|| {
    standard::get_standard_pids()
});

static VEHICLE_INFO_DEFINITIONS: LazyLock<HashMap<u8, PidDefinition>> = LazyLock::new(|| {
    standard::get_vechicle_info_pids()
});

/// Convenience function to get a PID definition
pub fn get_pid_definition(sid: u8, pid: u8) -> Option<&'static PidDefinition> {
    match sid {
        0x01 => PID_DEFINITIONS.get(&pid),
        0x09 => VEHICLE_INFO_DEFINITIONS.get(&pid),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same PID number means different things under mode 01 and mode 09,
    /// so lookups must be routed by service id and unknown modes must miss.
    #[test]
    fn pid_lookup_is_scoped_to_the_service_id() {
        let rpm = get_pid_definition(0x01, 0x0C).expect("mode 01 PID 0x0C = RPM");
        assert_eq!(rpm.unit, "rpm");
        assert_eq!((rpm.parser)(&[0x1A, 0xF8]), "1726");

        let vin = get_pid_definition(0x09, 0x02).expect("mode 09 PID 0x02 = VIN");
        assert_eq!(vin.name, "VIN");

        assert!(get_pid_definition(0x09, 0x0C).is_none());
        assert!(get_pid_definition(0x03, 0x0C).is_none());
    }
}
