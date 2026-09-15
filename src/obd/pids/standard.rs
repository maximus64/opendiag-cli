use super::parsers::*;
use super::{PidCategory, PidDefinition};
use std::collections::HashMap;

/// Returns all standard OBD-II PIDs
pub fn get_standard_pids() -> HashMap<u8, PidDefinition> {
    let mut pids = HashMap::new();

    // --- Diagnostics PIDs ---
    pids.insert(
        0x01,
        PidDefinition {
            len: Some(4),
            name: "MONITOR_STATUS",
            description: "Monitor status since DTCs cleared",
            unit: "", // The unit is the formatted report itself
            category: PidCategory::Diagnostics,
            parser: parse_monitor_status,
        },
    );

    pids.insert(
        0x03,
        PidDefinition {
            len: Some(2),
            name: "FUEL_SYSTEM_STATUS",
            description: "Fuel system status",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: default_parser, // Complex bit field, showing raw for now
        },
    );

    // --- Engine PIDs ---
    pids.insert(
        0x04,
        PidDefinition {
            len: Some(1),
            name: "ENGINE_LOAD",
            description: "Calculated Engine Load",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_a_times_100_div_255,
        },
    );

    pids.insert(
        0x05,
        PidDefinition {
            len: Some(1),
            name: "COOLANT_TEMP",
            description: "Engine Coolant Temperature",
            unit: "°C",
            category: PidCategory::Engine,
            parser: parse_a_minus_40,
        },
    );

    pids.insert(
        0x06,
        PidDefinition {
            len: Some(1),
            name: "SHORT_FUEL_TRIM_1",
            description: "Short term fuel trim—Bank 1",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_fuel_trim,
        },
    );

    pids.insert(
        0x07,
        PidDefinition {
            len: Some(1),
            name: "LONG_FUEL_TRIM_1",
            description: "Long term fuel trim—Bank 1",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_fuel_trim,
        },
    );

    pids.insert(
        0x08,
        PidDefinition {
            len: Some(1),
            name: "SHORT_FUEL_TRIM_2",
            description: "Short term fuel trim—Bank 2",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_fuel_trim,
        },
    );

    pids.insert(
        0x09,
        PidDefinition {
            len: Some(1),
            name: "LONG_FUEL_TRIM_2",
            description: "Long term fuel trim—Bank 2",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_fuel_trim,
        },
    );

    pids.insert(
        0x0B,
        PidDefinition {
            len: Some(1),
            name: "INTAKE_MANIFOLD_PRESSURE",
            description: "Intake manifold absolute pressure",
            unit: "kPa",
            category: PidCategory::Engine,
            parser: parse_manifold_pressure,
        },
    );

    pids.insert(
        0x0C,
        PidDefinition {
            len: Some(2),
            name: "ENGINE_RPM",
            description: "Engine RPM",
            unit: "rpm",
            category: PidCategory::Engine,
            parser: parse_rpm,
        },
    );

    pids.insert(
        0x0E,
        PidDefinition {
            len: Some(1),
            name: "TIMING_ADVANCE",
            description: "Timing advance",
            unit: "° before TDC",
            category: PidCategory::Engine,
            parser: parse_timing_advance,
        },
    );

    pids.insert(
        0x0F,
        PidDefinition {
            len: Some(1),
            name: "INTAKE_AIR_TEMP",
            description: "Intake air temperature",
            unit: "°C",
            category: PidCategory::Engine,
            parser: parse_intake_air_temp,
        },
    );

    pids.insert(
        0x10,
        PidDefinition {
            len: Some(2),
            name: "MAF_FLOW",
            description: "MAF air flow rate",
            unit: "g/s",
            category: PidCategory::Engine,
            parser: parse_maf_flow,
        },
    );

    pids.insert(
        0x11,
        PidDefinition {
            len: Some(1),
            name: "THROTTLE_POS",
            description: "Throttle Position",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_a_times_100_div_255,
        },
    );

    pids.insert(
        0x1C,
        PidDefinition {
            len: Some(1),
            name: "OBD_STANDARDS",
            description: "OBD standards this vehicle conforms to",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: parse_obd_standards,
        },
    );

    pids.insert(
        0x1F,
        PidDefinition {
            len: Some(2),
            name: "RUN_TIME",
            description: "Run time since engine start",
            unit: "seconds",
            category: PidCategory::Engine,
            parser: parse_distance_with_mil, // Same formula: 256*A + B
        },
    );

    pids.insert(
        0x21,
        PidDefinition {
            len: Some(2),
            name: "DISTANCE_WITH_MIL",
            description: "Distance traveled with MIL on",
            unit: "km",
            category: PidCategory::Diagnostics,
            parser: parse_distance_with_mil,
        },
    );

    pids.insert(
        0x2C,
        PidDefinition {
            len: Some(1),
            name: "COMMANDED_EGR",
            description: "Commanded EGR",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_commanded_egr,
        },
    );

    pids.insert(
        0x2D,
        PidDefinition {
            len: Some(1),
            name: "EGR_ERROR",
            description: "EGR Error",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_egr_error,
        },
    );

    pids.insert(
        0x2E,
        PidDefinition {
            len: Some(1),
            name: "COMMANDED_EVAP_PURGE",
            description: "Commanded evaporative purge",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_a_times_100_div_255,
        },
    );

    pids.insert(
        0x2F,
        PidDefinition {
            len: Some(1),
            name: "FUEL_LEVEL",
            description: "Fuel Tank Level Input",
            unit: "%",
            category: PidCategory::Chassis,
            parser: parse_fuel_level,
        },
    );

    pids.insert(
        0x30,
        PidDefinition {
            len: Some(1),
            name: "WARM_UPS_SINCE_CODES_CLEARED",
            description: "Warm-ups since codes cleared",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: parse_identity,
        },
    );

    pids.insert(
        0x31,
        PidDefinition {
            len: Some(2),
            name: "DISTANCE_SINCE_CODES_CLEARED",
            description: "Distance traveled since codes cleared",
            unit: "km",
            category: PidCategory::Diagnostics,
            parser: parse_distance_with_mil, // Same formula: 256*A + B
        },
    );

    pids.insert(
        0x32,
        PidDefinition {
            len: Some(1),
            name: "EVAP_VAPOR_PRESSURE",
            description: "Evap. System Vapor Pressure",
            unit: "Pa",
            category: PidCategory::Engine,
            parser: default_parser, // Complex signed value
        },
    );

    pids.insert(
        0x33,
        PidDefinition {
            len: Some(1),
            name: "BAROMETRIC_PRESSURE",
            description: "Absolute Barometric Pressure",
            unit: "kPa",
            category: PidCategory::Environment,
            parser: parse_barometric_pressure,
        },
    );

    pids.insert(
        0x3C,
        PidDefinition {
            len: Some(2),
            name: "CATALYST_TEMP_B1S1",
            description: "Catalyst Temperature: Bank 1, Sensor 1",
            unit: "°C",
            category: PidCategory::Engine,
            parser: parse_catalyst_temp,
        },
    );

    pids.insert(
        0x3D,
        PidDefinition {
            len: Some(2),
            name: "CATALYST_TEMP_B2S1",
            description: "Catalyst Temperature: Bank 2, Sensor 1",
            unit: "°C",
            category: PidCategory::Engine,
            parser: parse_catalyst_temp,
        },
    );

    pids.insert(
        0x41,
        PidDefinition {
            len: Some(4),
            name: "MONITOR_STATUS_DRIVE_CYCLE",
            description: "Monitor status this drive cycle",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: default_parser, // Complex bit field
        },
    );

    pids.insert(
        0x42,
        PidDefinition {
            len: Some(2),
            name: "CONTROL_MODULE_VOLTAGE",
            description: "Control module voltage",
            unit: "V",
            category: PidCategory::Diagnostics,
            parser: parse_control_module_voltage,
        },
    );

    pids.insert(
        0x43,
        PidDefinition {
            len: Some(2),
            name: "ABSOLUTE_LOAD_VALUE",
            description: "Absolute load value",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_absolute_load,
        },
    );

    pids.insert(
        0x44,
        PidDefinition {
            len: Some(2),
            name: "FUEL_AIR_COMMANDED_EQUIV_RATIO",
            description: "Fuel–Air commanded equivalence ratio",
            unit: "ratio",
            category: PidCategory::Engine,
            parser: parse_air_fuel_ratio,
        },
    );

    pids.insert(
        0x45,
        PidDefinition {
            len: Some(1),
            name: "RELATIVE_THROTTLE_POS",
            description: "Relative throttle position",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_relative_throttle,
        },
    );

    pids.insert(
        0x47,
        PidDefinition {
            len: Some(1),
            name: "ABSOLUTE_THROTTLE_POS_B",
            description: "Absolute throttle position B",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_a_times_100_div_255,
        },
    );

    pids.insert(
        0x49,
        PidDefinition {
            len: Some(1),
            name: "ACCELERATOR_PEDAL_POS_D",
            description: "Accelerator pedal position D",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_accelerator_pedal,
        },
    );

    pids.insert(
        0x4A,
        PidDefinition {
            len: Some(1),
            name: "ACCELERATOR_PEDAL_POS_E",
            description: "Accelerator pedal position E",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_accelerator_pedal,
        },
    );

    pids.insert(
        0x4C,
        PidDefinition {
            len: Some(1),
            name: "COMMANDED_THROTTLE_ACTUATOR",
            description: "Commanded throttle actuator",
            unit: "%",
            category: PidCategory::Engine,
            parser: parse_a_times_100_div_255,
        },
    );

    // --- Chassis PIDs ---
    pids.insert(
        0x0D,
        PidDefinition {
            len: Some(1),
            name: "VEHICLE_SPEED",
            description: "Vehicle Speed",
            unit: "km/h",
            category: PidCategory::Chassis,
            parser: parse_identity, // Byte A is the value
        },
    );

    super::oxygen::register(&mut pids);
    pids
}

pub fn get_vechicle_info_pids() -> HashMap<u8, PidDefinition> {
    let mut pids = HashMap::new();

    pids.insert(
        0x02,
        PidDefinition {
            len: None,
            name: "VIN",
            description: "Vehicle Identification Number",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: |data: &[u8]| {
                // VIN is ASCII characters in the response data
                let vin_bytes = data.get(1..).unwrap_or_default(); // Skip the first byte (mode)
                match std::str::from_utf8(vin_bytes) {
                    Ok(vin_str) => crate::value::Value::Text(
                        vin_str.trim_end_matches(char::from(0)).to_string(),
                    ), // Remove trailing nulls
                    Err(_) => crate::value::Value::Text("Invalid UTF-8 in VIN".to_string()),
                }
            },
        },
    );

    pids.insert(
        0x04,
        PidDefinition {
            len: None,
            name: "CALIBRATION_ID",
            description: "Calibration Identification",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: parse_calibration_ids,
        },
    );

    pids.insert(
        0x06,
        PidDefinition {
            len: None,
            name: "CVN",
            description: "Calibration Verification Number",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: parse_calibration_verification_numbers,
        },
    );

    pids.insert(
        0x08,
        PidDefinition {
            len: None,
            name: "IN_USE_PERFORMANCE_TRACKING",
            description: "In-use performance tracking",
            unit: "",
            category: PidCategory::Diagnostics,
            parser: default_parser, // Complex bit field
        },
    );

    pids
}
