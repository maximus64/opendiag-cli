use std::collections::HashMap;
use super::{PidDefinition, PidCategory};
use super::parsers::*;

/// Returns all standard OBD-II PIDs
pub fn get_standard_pids() -> HashMap<u8, PidDefinition> {
    let mut pids = HashMap::new();

    // --- Diagnostics PIDs ---
    pids.insert(0x01, PidDefinition {
        name: "MONITOR_STATUS",
        description: "Monitor status since DTCs cleared",
        unit: "", // The unit is the formatted report itself
        category: PidCategory::Diagnostics,
        parser: parse_monitor_status,
    });

    pids.insert(0x03, PidDefinition {
        name: "FUEL_SYSTEM_STATUS",
        description: "Fuel system status",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: default_parser, // Complex bit field, showing raw for now
    });

    // --- Engine PIDs ---
    pids.insert(0x04, PidDefinition {
        name: "ENGINE_LOAD",
        description: "Calculated Engine Load",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_a_times_100_div_255,
    });

    pids.insert(0x05, PidDefinition {
        name: "COOLANT_TEMP",
        description: "Engine Coolant Temperature",
        unit: "°C",
        category: PidCategory::Engine,
        parser: parse_a_minus_40,
    });

    pids.insert(0x06, PidDefinition {
        name: "SHORT_FUEL_TRIM_1",
        description: "Short term fuel trim—Bank 1",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_fuel_trim,
    });

    pids.insert(0x07, PidDefinition {
        name: "LONG_FUEL_TRIM_1",
        description: "Long term fuel trim—Bank 1",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_fuel_trim,
    });

    pids.insert(0x08, PidDefinition {
        name: "SHORT_FUEL_TRIM_2",
        description: "Short term fuel trim—Bank 2",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_fuel_trim,
    });

    pids.insert(0x09, PidDefinition {
        name: "LONG_FUEL_TRIM_2",
        description: "Long term fuel trim—Bank 2",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_fuel_trim,
    });

    pids.insert(0x0B, PidDefinition {
        name: "INTAKE_MANIFOLD_PRESSURE",
        description: "Intake manifold absolute pressure",
        unit: "kPa",
        category: PidCategory::Engine,
        parser: parse_manifold_pressure,
    });

    pids.insert(0x0C, PidDefinition {
        name: "ENGINE_RPM",
        description: "Engine RPM",
        unit: "rpm",
        category: PidCategory::Engine,
        parser: parse_rpm,
    });

    pids.insert(0x0E, PidDefinition {
        name: "TIMING_ADVANCE",
        description: "Timing advance",
        unit: "° before TDC",
        category: PidCategory::Engine,
        parser: parse_timing_advance,
    });

    pids.insert(0x0F, PidDefinition {
        name: "INTAKE_AIR_TEMP",
        description: "Intake air temperature",
        unit: "°C",
        category: PidCategory::Engine,
        parser: parse_intake_air_temp,
    });

    pids.insert(0x10, PidDefinition {
        name: "MAF_FLOW",
        description: "MAF air flow rate",
        unit: "g/s",
        category: PidCategory::Engine,
        parser: parse_maf_flow,
    });

    pids.insert(0x11, PidDefinition {
        name: "THROTTLE_POS",
        description: "Throttle Position",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_a_times_100_div_255,
    });

    pids.insert(0x13, PidDefinition {
        name: "O2_SENSORS_PRESENT",
        description: "Oxygen sensors present",
        unit: "",
        category: PidCategory::Engine,
        parser: default_parser, // Bit field
    });

    pids.insert(0x15, PidDefinition {
        name: "O2_B1S2",
        description: "Oxygen Sensor 2 - Bank 1",
        unit: "V",
        category: PidCategory::Engine,
        parser: parse_oxygen_sensor_voltage,
    });

    pids.insert(0x19, PidDefinition {
        name: "O2_B2S2",
        description: "Oxygen Sensor 2 - Bank 2",
        unit: "V",
        category: PidCategory::Engine,
        parser: parse_oxygen_sensor_voltage,
    });

    pids.insert(0x1C, PidDefinition {
        name: "OBD_STANDARDS",
        description: "OBD standards this vehicle conforms to",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: parse_obd_standards,
    });

    pids.insert(0x1F, PidDefinition {
        name: "RUN_TIME",
        description: "Run time since engine start",
        unit: "seconds",
        category: PidCategory::Engine,
        parser: parse_distance_with_mil, // Same formula: 256*A + B
    });

    pids.insert(0x21, PidDefinition {
        name: "DISTANCE_WITH_MIL",
        description: "Distance traveled with MIL on",
        unit: "km",
        category: PidCategory::Diagnostics,
        parser: parse_distance_with_mil,
    });

    pids.insert(0x2C, PidDefinition {
        name: "COMMANDED_EGR",
        description: "Commanded EGR",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_commanded_egr,
    });

    pids.insert(0x2D, PidDefinition {
        name: "EGR_ERROR",
        description: "EGR Error",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_egr_error,
    });

    pids.insert(0x2E, PidDefinition {
        name: "COMMANDED_EVAP_PURGE",
        description: "Commanded evaporative purge",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_a_times_100_div_255,
    });

    pids.insert(0x2F, PidDefinition {
        name: "FUEL_LEVEL",
        description: "Fuel Tank Level Input",
        unit: "%",
        category: PidCategory::Chassis,
        parser: parse_fuel_level,
    });

    pids.insert(0x30, PidDefinition {
        name: "WARM_UPS_SINCE_CODES_CLEARED",
        description: "Warm-ups since codes cleared",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: parse_identity,
    });

    pids.insert(0x31, PidDefinition {
        name: "DISTANCE_SINCE_CODES_CLEARED",
        description: "Distance traveled since codes cleared",
        unit: "km",
        category: PidCategory::Diagnostics,
        parser: parse_distance_with_mil, // Same formula: 256*A + B
    });

    pids.insert(0x32, PidDefinition {
        name: "EVAP_VAPOR_PRESSURE",
        description: "Evap. System Vapor Pressure",
        unit: "Pa",
        category: PidCategory::Engine,
        parser: default_parser, // Complex signed value
    });

    pids.insert(0x33, PidDefinition {
        name: "BAROMETRIC_PRESSURE",
        description: "Absolute Barometric Pressure",
        unit: "kPa",
        category: PidCategory::Environment,
        parser: parse_barometric_pressure,
    });

    pids.insert(0x34, PidDefinition {
        name: "O2_B1S1_WR_LAMBDA",
        description: "Oxygen Sensor 1 - Bank 1 (WR lambda)",
        unit: "ratio",
        category: PidCategory::Engine,
        parser: parse_oxygen_sensor_wr_lambda,
    });

    pids.insert(0x38, PidDefinition {
        name: "O2_B2S1_WR_LAMBDA", 
        description: "Oxygen Sensor 1 - Bank 2 (WR lambda)",
        unit: "ratio",
        category: PidCategory::Engine,
        parser: parse_oxygen_sensor_wr_lambda,
    });

    pids.insert(0x3C, PidDefinition {
        name: "CATALYST_TEMP_B1S1",
        description: "Catalyst Temperature: Bank 1, Sensor 1",
        unit: "°C",
        category: PidCategory::Engine,
        parser: parse_catalyst_temp,
    });

    pids.insert(0x3D, PidDefinition {
        name: "CATALYST_TEMP_B2S1",
        description: "Catalyst Temperature: Bank 2, Sensor 1", 
        unit: "°C",
        category: PidCategory::Engine,
        parser: parse_catalyst_temp,
    });

    pids.insert(0x41, PidDefinition {
        name: "MONITOR_STATUS_DRIVE_CYCLE",
        description: "Monitor status this drive cycle",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: default_parser, // Complex bit field
    });

    pids.insert(0x42, PidDefinition {
        name: "CONTROL_MODULE_VOLTAGE",
        description: "Control module voltage",
        unit: "V",
        category: PidCategory::Diagnostics,
        parser: parse_control_module_voltage,
    });

    pids.insert(0x43, PidDefinition {
        name: "ABSOLUTE_LOAD_VALUE",
        description: "Absolute load value",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_absolute_load,
    });

    pids.insert(0x44, PidDefinition {
        name: "FUEL_AIR_COMMANDED_EQUIV_RATIO",
        description: "Fuel–Air commanded equivalence ratio",
        unit: "ratio",
        category: PidCategory::Engine,
        parser: parse_air_fuel_ratio,
    });

    pids.insert(0x45, PidDefinition {
        name: "RELATIVE_THROTTLE_POS",
        description: "Relative throttle position",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_relative_throttle,
    });

    pids.insert(0x47, PidDefinition {
        name: "ABSOLUTE_THROTTLE_POS_B",
        description: "Absolute throttle position B",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_a_times_100_div_255,
    });

    pids.insert(0x49, PidDefinition {
        name: "ACCELERATOR_PEDAL_POS_D",
        description: "Accelerator pedal position D",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_accelerator_pedal,
    });

    pids.insert(0x4A, PidDefinition {
        name: "ACCELERATOR_PEDAL_POS_E",
        description: "Accelerator pedal position E",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_accelerator_pedal,
    });

    pids.insert(0x4C, PidDefinition {
        name: "COMMANDED_THROTTLE_ACTUATOR",
        description: "Commanded throttle actuator",
        unit: "%",
        category: PidCategory::Engine,
        parser: parse_a_times_100_div_255,
    });

    // --- Chassis PIDs ---
    pids.insert(0x0D, PidDefinition {
        name: "VEHICLE_SPEED",
        description: "Vehicle Speed",
        unit: "km/h",
        category: PidCategory::Chassis,
        parser: parse_identity, // Byte A is the value
    });

    pids
}


pub fn get_vechicle_info_pids() -> HashMap<u8, PidDefinition> {
    let mut pids = HashMap::new();

    pids.insert(0x02, PidDefinition {
        name: "VIN",
        description: "Vehicle Identification Number",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: |data: &[u8]| {
            // VIN is ASCII characters in the response data
            let vin_bytes = &data[1..]; // Skip the first byte (mode)
            match std::str::from_utf8(vin_bytes) {
                Ok(vin_str) => vin_str.trim_end_matches(char::from(0)).to_string(), // Remove trailing nulls
                Err(_) => "Invalid UTF-8 in VIN".to_string(),
            }
        },
    });

    pids.insert(0x04, PidDefinition {
        name: "CALIBRATION_ID",
        description: "Calibration Identification",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: |data: &[u8]| {
            // Calibration ID is ASCII characters in the response data
            let cal_bytes = &data[1..]; // Skip the first byte (mode)
            match std::str::from_utf8(cal_bytes) {
                Ok(cal_str) => cal_str.trim_end_matches(char::from(0)).to_string(), // Remove trailing nulls
                Err(_) => "Invalid UTF-8 in Calibration ID".to_string(),
            }
        },
    });

    pids.insert(0x06, PidDefinition {
        name: "CVN",
        description: "Calibration Verification Number",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: default_parser, // Hex representation
    });

    pids.insert(0x08, PidDefinition {
        name: "IN_USE_PERFORMANCE_TRACKING",
        description: "In-use performance tracking",
        unit: "",
        category: PidCategory::Diagnostics,
        parser: default_parser, // Complex bit field
    });


    pids
}
