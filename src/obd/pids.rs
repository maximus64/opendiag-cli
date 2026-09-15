use std::collections::HashMap;
use std::sync::LazyLock;

pub mod oxygen;
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
    pub len: Option<usize>,
    pub parser: fn(&[u8]) -> crate::value::Value,
}

static PID_DEFINITIONS: LazyLock<HashMap<u8, PidDefinition>> =
    LazyLock::new(standard::get_standard_pids);

static VEHICLE_INFO_DEFINITIONS: LazyLock<HashMap<u8, PidDefinition>> =
    LazyLock::new(standard::get_vechicle_info_pids);

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

    #[test]
    fn conventional_oxygen_pids_decode_both_bytes() {
        use crate::{value::Value, ObdProtocol};
        for pid in 0x14..=0x1B {
            let definition = get_pid_definition(1, pid).unwrap();
            assert_eq!(definition.len, Some(2));
            for (data, volts, trim) in [
                ([0x00, 0xC0], 0.0, Value::number(50.0, "%")),
                ([0x80, 0x00], 0.64, Value::number(-100.0, "%")),
                ([0xC8, 0x80], 1.0, Value::number(0.0, "%")),
                ([0xFF, 0xFE], 1.275, Value::number(98.4375, "%")),
                ([0xFF, 0xFF], 1.275, Value::Unavailable),
            ] {
                let decoded = ObdProtocol::decode_pid(1, pid, &data);
                assert_eq!(decoded.name, format!("O2_SENSOR_{}", pid - 0x14 + 1));
                assert_eq!(
                    decoded.value.field("voltage"),
                    Some(&Value::number(volts, "V"))
                );
                assert_eq!(decoded.value.field("short_term_fuel_trim"), Some(&trim));
            }
            assert_eq!((definition.parser)(&[0x80]), Value::Unavailable);
        }
    }

    /// The same PID number means different things under mode 01 and mode 09,
    /// so lookups must be routed by service id and unknown modes must miss.
    #[test]
    fn pid_lookup_is_scoped_to_the_service_id() {
        let rpm = get_pid_definition(0x01, 0x0C).expect("mode 01 PID 0x0C = RPM");
        assert_eq!(rpm.unit, "rpm");
        assert_eq!(
            (rpm.parser)(&[0x1A, 0xF8]),
            crate::value::Value::number(1726.0, "rpm")
        );

        let vin = get_pid_definition(0x09, 0x02).expect("mode 09 PID 0x02 = VIN");
        assert_eq!(vin.name, "VIN");

        assert!(get_pid_definition(0x09, 0x0C).is_none());
        assert!(get_pid_definition(0x03, 0x0C).is_none());
    }
}
