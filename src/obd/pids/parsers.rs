use crate::utils::bytes_to_hex;

/// Default parser for unknown PIDs, returns raw hex.
pub fn default_parser(data: &[u8]) -> String {
    bytes_to_hex(data)
}

/// Generic parser for formulas like (A * 100) / 255
pub fn parse_a_times_100_div_255(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as f32;
    format!("{:.2}", (a * 100.0) / 255.0)
}

/// Generic parser for formulas like A - 40 (temps)
pub fn parse_a_minus_40(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as i16;
    (a - 40).to_string()
}

/// Generic parser for single-byte identity values (A)
pub fn parse_identity(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    data[0].to_string()
}

/// Specific parser for RPM: (256*A + B) / 4
pub fn parse_rpm(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    (((256 * a) + b) / 4).to_string()
}

/// Parses the complex bit-encoded response for PID 01 - Monitor Status (Corrected).
pub fn parse_monitor_status(data: &[u8]) -> String {
    if data.len() < 4 {
        return "N/A - Invalid data length".to_string();
    }

    let a = data[0];
    let b = data[1];
    let c = data[2];
    let d = data[3];

    let mut report = Vec::new();

    // --- Byte A: MIL Status and DTC Count (This part remains the same) ---
    let mil_on = (a & 0x80) != 0;
    let dtc_count = a & 0x7F;
    report.push(format!("\n  MIL Status: {}", if mil_on { "ON" } else { "OFF" }));
    report.push(format!("  DTC Count: {}", dtc_count));

    // --- Helper to format monitor status ---
    // For completeness bits, a '0' means complete and a '1' means incomplete.
    // For availability bits, a '1' means available/supported.
    let get_status = |is_available: bool, is_incomplete: bool| -> &'static str {
        if !is_available {
            "Not Available"
        } else if !is_incomplete {
            "Complete"
        } else {
            "Incomplete"
        }
    };
    
    // --- Byte B: Common Test Indicators ---
    report.push("\n  --- Common Monitors ---".to_string());
    report.push(format!("  Components:        {}", get_status((b & (1 << 2)) != 0, (b & (1 << 6)) != 0)));
    report.push(format!("  Fuel System:       {}", get_status((b & (1 << 1)) != 0, (b & (1 << 5)) != 0)));
    report.push(format!("  Misfire:           {}", get_status((b & (1 << 0)) != 0, (b & (1 << 4)) != 0)));
    
    // --- Bytes C & D: Spark Ignition Monitors ---
    report.push("\n  --- Spark Ignition Monitors ---".to_string());
    report.push(format!("  EGR/VVT System:    {}", get_status((c & (1 << 7)) != 0, (d & (1 << 7)) != 0)));
    report.push(format!("  Oxygen Sensor Htr: {}", get_status((c & (1 << 6)) != 0, (d & (1 << 6)) != 0)));
    report.push(format!("  Oxygen Sensor:     {}", get_status((c & (1 << 5)) != 0, (d & (1 << 5)) != 0)));
    report.push(format!("  Gas Particulate F: {}", get_status((c & (1 << 4)) != 0, (d & (1 << 4)) != 0)));
    report.push(format!("  Secondary Air Sys: {}", get_status((c & (1 << 3)) != 0, (d & (1 << 3)) != 0)));
    report.push(format!("  Evaporative System:{}", get_status((c & (1 << 2)) != 0, (d & (1 << 2)) != 0)));
    report.push(format!("  Heated Catalyst:   {}", get_status((c & (1 << 1)) != 0, (d & (1 << 1)) != 0)));
    report.push(format!("  Catalyst:          {}", get_status((c & (1 << 0)) != 0, (d & (1 << 0)) != 0)));

    // Join all the lines into a single string for display
    report.join("\n")
}

/// Parser for fuel trim percentage: ((A - 128) * 100) / 128
pub fn parse_fuel_trim(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as i16;
    let result = ((a - 128) as f32 * 100.0) / 128.0;
    format!("{:.2}", result)
}

/// Parser for oxygen sensor voltage: A / 200.0
pub fn parse_oxygen_sensor_voltage(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as f32;
    format!("{:.3}", a / 200.0)
}

/// Parser for manifold pressure: A (kPa)
pub fn parse_manifold_pressure(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    data[0].to_string()
}

/// Parser for timing advance: (A - 128) / 2
pub fn parse_timing_advance(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as i16;
    let result = (a - 128) as f32 / 2.0;
    format!("{:.1}", result)
}

/// Parser for intake air temperature: A - 40
pub fn parse_intake_air_temp(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as i16;
    (a - 40).to_string()
}

/// Parser for MAF air flow: (256*A + B) / 100
pub fn parse_maf_flow(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    let result = ((256 * a) + b) as f32 / 100.0;
    format!("{:.2}", result)
}

/// Parser for OBD standards (PID 0x1C)
pub fn parse_obd_standards(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    match data[0] {
        0x01 => "OBD-II as defined by the CARB".to_string(),
        0x02 => "OBD as defined by the EPA".to_string(),
        0x03 => "OBD and OBD-II".to_string(),
        0x04 => "OBD-I".to_string(),
        0x05 => "Not OBD compliant".to_string(),
        0x06 => "EOBD (Europe)".to_string(),
        0x07 => "EOBD and OBD-II".to_string(),
        0x08 => "EOBD and OBD".to_string(),
        0x09 => "EOBD, OBD and OBD II".to_string(),
        0x0A => "JOBD (Japan)".to_string(),
        0x0B => "JOBD and OBD II".to_string(),
        0x0C => "JOBD and EOBD".to_string(),
        0x0D => "JOBD, EOBD, and OBD II".to_string(),
        x => format!("Unknown standard: 0x{:02X}", x),
    }
}

/// Parser for distance with MIL on: (256*A + B)
pub fn parse_distance_with_mil(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    ((256 * a) + b).to_string()
}

/// Parser for fuel rail pressure: (256*A + B) * 3
pub fn parse_fuel_rail_pressure(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    (((256 * a) + b) * 3).to_string()
}

/// Parser for commanded EGR: (A * 100) / 255
pub fn parse_commanded_egr(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as f32;
    format!("{:.2}", (a * 100.0) / 255.0)
}

/// Parser for EGR error: ((A - 128) * 100) / 128
pub fn parse_egr_error(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as i16;
    let result = ((a - 128) as f32 * 100.0) / 128.0;
    format!("{:.2}", result)
}

/// Parser for fuel level input: (A * 100) / 255
pub fn parse_fuel_level(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as f32;
    format!("{:.2}", (a * 100.0) / 255.0)
}

/// Parser for barometric pressure: A
pub fn parse_barometric_pressure(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    data[0].to_string()
}

/// Parser for catalyst temperature: ((256*A + B) / 10) - 40
pub fn parse_catalyst_temp(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    let result = (((256 * a) + b) as f32 / 10.0) - 40.0;
    format!("{:.1}", result)
}

/// Parser for control module voltage: (256*A + B) / 1000
pub fn parse_control_module_voltage(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    let result = ((256 * a) + b) as f32 / 1000.0;
    format!("{:.3}", result)
}

/// Parser for absolute load value: ((256*A + B) * 100) / 255
pub fn parse_absolute_load(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    let result = (((256 * a) + b) as f32 * 100.0) / 255.0;
    format!("{:.2}", result)
}

/// Parser for commanded air-fuel equivalence ratio: (256*A + B) / 32768
pub fn parse_air_fuel_ratio(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    let result = ((256 * a) + b) as f32 / 32768.0;
    format!("{:.4}", result)
}

/// Parser for relative throttle position: (A * 100) / 255
pub fn parse_relative_throttle(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as f32;
    format!("{:.2}", (a * 100.0) / 255.0)
}

/// Parser for ambient air temperature: A - 40
pub fn parse_ambient_air_temp(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as i16;
    (a - 40).to_string()
}

/// Parser for accelerator pedal position: (A * 100) / 255
pub fn parse_accelerator_pedal(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as f32;
    format!("{:.2}", (a * 100.0) / 255.0)
}

/// Parser for hybrid battery pack remaining life: (A * 100) / 255
pub fn parse_battery_life(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as f32;
    format!("{:.2}", (a * 100.0) / 255.0)
}

/// Parser for engine oil temperature: A - 40
pub fn parse_oil_temp(data: &[u8]) -> String {
    if data.is_empty() { return "N/A".to_string(); }
    let a = data[0] as i16;
    (a - 40).to_string()
}

/// Parser for fuel injection timing: (((256*A + B) - 26880) / 128)
pub fn parse_fuel_injection_timing(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    let result = (((256 * a) + b) as i32 - 26880) as f32 / 128.0;
    format!("{:.2}", result)
}

/// Parser for engine fuel rate: (256*A + B) / 20
pub fn parse_fuel_rate(data: &[u8]) -> String {
    if data.len() < 2 { return "N/A".to_string(); }
    let a = data[0] as u16;
    let b = data[1] as u16;
    let result = ((256 * a) + b) as f32 / 20.0;
    format!("{:.2}", result)
}

/// Parser for wide-range oxygen sensor (lambda): 4-byte format
/// Bytes A,B: Equivalence Ratio (lambda) = (256*A + B) * 2/65536
/// Bytes C,D: Voltage = (256*C + D) * 8/65536
pub fn parse_oxygen_sensor_wr_lambda(data: &[u8]) -> String {
    if data.len() < 4 { return "N/A".to_string(); }
    
    let a = data[0] as u16;
    let b = data[1] as u16;
    let c = data[2] as u16;
    let d = data[3] as u16;
    
    // Calculate equivalence ratio (lambda)
    let lambda = ((256 * a) + b) as f32 * 2.0 / 65536.0;
    
    // Calculate voltage  
    let voltage = ((256 * c) + d) as f32 * 8.0 / 65536.0;
    
    format!("λ={:.4}, V={:.3}V", lambda, voltage)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One representative of each formula shape, checked at both ends of the
    /// range — these are the numbers a user reads off the dashboard.
    #[test]
    fn scaling_formulas_match_the_obd_spec() {
        // (256*A + B) / 4
        assert_eq!(parse_rpm(&[0x00, 0x00]), "0");
        assert_eq!(parse_rpm(&[0x1A, 0xF8]), "1726");
        // A - 40, so raw 0 is -40C
        assert_eq!(parse_a_minus_40(&[0x00]), "-40");
        assert_eq!(parse_a_minus_40(&[0xFF]), "215");
        // (A * 100) / 255
        assert_eq!(parse_a_times_100_div_255(&[0xFF]), "100.00");
        assert_eq!(parse_a_times_100_div_255(&[0x80]), "50.20");
        // ((A - 128) * 100) / 128, signed either side of centre
        assert_eq!(parse_fuel_trim(&[0x80]), "0.00");
        assert_eq!(parse_fuel_trim(&[0x00]), "-100.00");
        assert_eq!(parse_fuel_trim(&[0xFF]), "99.22");
        // (A - 128) / 2
        assert_eq!(parse_timing_advance(&[0x80]), "0.0");
        assert_eq!(parse_timing_advance(&[0x00]), "-64.0");
        // (256*A + B) / 100
        assert_eq!(parse_maf_flow(&[0x0F, 0xA0]), "40.00");
        // ((256*A + B) / 10) - 40
        assert_eq!(parse_catalyst_temp(&[0x10, 0x69]), "380.1");
        // (256*A + B) / 1000
        assert_eq!(parse_control_module_voltage(&[0x37, 0x1A]), "14.106");
    }

    /// Every parser is handed `&data[2..]` straight out of the response, so a
    /// short or empty ECU reply must degrade to "N/A" rather than panic.
    #[test]
    fn short_data_never_panics() {
        let two_byte_parsers: [fn(&[u8]) -> String; 4] = [
            parse_rpm,
            parse_maf_flow,
            parse_catalyst_temp,
            parse_control_module_voltage,
        ];
        for parser in two_byte_parsers {
            assert_eq!(parser(&[]), "N/A");
            assert_eq!(parser(&[0x12]), "N/A");
        }

        let one_byte_parsers: [fn(&[u8]) -> String; 3] =
            [parse_identity, parse_fuel_trim, parse_a_minus_40];
        for parser in one_byte_parsers {
            assert_eq!(parser(&[]), "N/A");
        }

        assert!(parse_monitor_status(&[0x01, 0x02]).starts_with("N/A"));
        assert_eq!(parse_oxygen_sensor_wr_lambda(&[0x01, 0x02, 0x03]), "N/A");
    }

    /// PID 0x01 packs the MIL lamp and the stored-DTC count into byte A; the
    /// read-dtc flow and this display must agree on that count.
    #[test]
    fn monitor_status_reports_mil_state_and_dtc_count() {
        let on = parse_monitor_status(&[0x83, 0x07, 0x00, 0x00]);
        assert!(on.contains("MIL Status: ON"), "{on}");
        assert!(on.contains("DTC Count: 3"), "{on}");

        let off = parse_monitor_status(&[0x00, 0x00, 0x00, 0x00]);
        assert!(off.contains("MIL Status: OFF"), "{off}");
        assert!(off.contains("DTC Count: 0"), "{off}");
    }

    /// Availability and completeness live in different bytes/bits, and the
    /// completeness bit is inverted (0 = complete).
    #[test]
    fn monitor_status_distinguishes_unavailable_from_incomplete() {
        // B: misfire (bit 0) available, complete (bit 4 clear).
        let s = parse_monitor_status(&[0x00, 0x01, 0x00, 0x00]);
        assert!(s.contains("Misfire:           Complete"), "{s}");

        // B: misfire available and incomplete (bit 4 set).
        let s = parse_monitor_status(&[0x00, 0x11, 0x00, 0x00]);
        assert!(s.contains("Misfire:           Incomplete"), "{s}");

        // Not supported at all.
        let s = parse_monitor_status(&[0x00, 0x00, 0x00, 0x00]);
        assert!(s.contains("Misfire:           Not Available"), "{s}");
    }

    #[test]
    fn unknown_obd_standard_falls_back_to_raw_value() {
        assert_eq!(parse_obd_standards(&[0x06]), "EOBD (Europe)");
        assert_eq!(parse_obd_standards(&[0xEE]), "Unknown standard: 0xEE");
        assert_eq!(parse_obd_standards(&[]), "N/A");
    }
}
