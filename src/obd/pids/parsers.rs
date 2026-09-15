use crate::utils::bytes_to_hex;
use crate::value::Value;

/// Default parser for unknown PIDs, returns raw hex.
pub fn default_parser(data: &[u8]) -> Value {
    Value::Text(bytes_to_hex(data))
}

pub fn parse_calibration_ids(data: &[u8]) -> Value {
    let Some((&count, records)) = data.split_first() else {
        return Value::Unavailable;
    };
    if count == 0 || records.len() != usize::from(count) * 16 {
        return Value::Unavailable;
    }
    let mut ids = Vec::new();
    for record in records.as_chunks::<16>().0 {
        let Ok(text) = std::str::from_utf8(record) else {
            return Value::Unavailable;
        };
        let text = text.trim_end_matches(['\0', ' ']);
        if text.is_empty()
            || !text
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Value::Unavailable;
        }
        ids.push(text);
    }
    Value::Text(ids.join(", "))
}

pub fn parse_calibration_verification_numbers(data: &[u8]) -> Value {
    let Some((&count, records)) = data.split_first() else {
        return Value::Unavailable;
    };
    if count == 0 || records.len() != usize::from(count) * 4 {
        return Value::Unavailable;
    }
    Value::Text(
        records
            .as_chunks::<4>()
            .0
            .iter()
            .map(|record| bytes_to_hex(record.as_slice()))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod calibration_tests {
    use super::*;

    #[test]
    fn calibration_parsers_reject_incomplete_records_and_bad_counts() {
        let mut id = vec![1];
        id.extend_from_slice(b"12216125\0\0\0\0\0\0\0\0");
        assert_eq!(parse_calibration_ids(&id), Value::Text("12216125".into()));
        for count in [0, 2] {
            id[0] = count;
            assert_eq!(parse_calibration_ids(&id), Value::Unavailable);
        }
        id[0] = 1;
        id[3] = 0xFF;
        assert_eq!(parse_calibration_ids(&id), Value::Unavailable);
        assert_eq!(parse_calibration_ids(&[]), Value::Unavailable);
        assert_eq!(parse_calibration_ids(&[1, b'A']), Value::Unavailable);
        for data in [&[][..], &[0], &[1, 0, 0, 0], &[2, 0, 0, 0, 1]] {
            assert_eq!(
                parse_calibration_verification_numbers(data),
                Value::Unavailable
            );
        }
        assert_eq!(
            parse_calibration_verification_numbers(&[1, 0, 0, 0x65, 0x53]),
            Value::Text("00006553".into())
        );
    }
}

/// Generic parser for formulas like (A * 100) / 255
pub fn parse_a_times_100_div_255(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a * 100.0 / 255.0, "%")
}

/// Generic parser for formulas like A - 40 (temps)
pub fn parse_a_minus_40(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a - 40.0, "°C")
}

/// Generic parser for single-byte identity values (A)
pub fn parse_identity(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a, "")
}

/// Specific parser for RPM: (256*A + B) / 4
pub fn parse_rpm(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word / 4.0, "rpm")
}

/// Parses the complex bit-encoded response for PID 01 - Monitor Status (Corrected).
pub fn parse_monitor_status(data: &[u8]) -> Value {
    Value::Text((|| -> String {
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
        report.push(format!(
            "\n  MIL Status: {}",
            if mil_on { "ON" } else { "OFF" }
        ));
        report.push(format!("  DTC Count: {dtc_count}"));

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
        report.push(format!(
            "  Components:        {}",
            get_status((b & (1 << 2)) != 0, (b & (1 << 6)) != 0)
        ));
        report.push(format!(
            "  Fuel System:       {}",
            get_status((b & (1 << 1)) != 0, (b & (1 << 5)) != 0)
        ));
        report.push(format!(
            "  Misfire:           {}",
            get_status((b & (1 << 0)) != 0, (b & (1 << 4)) != 0)
        ));

        // --- Bytes C & D: Spark Ignition Monitors ---
        report.push("\n  --- Spark Ignition Monitors ---".to_string());
        report.push(format!(
            "  EGR/VVT System:    {}",
            get_status((c & (1 << 7)) != 0, (d & (1 << 7)) != 0)
        ));
        report.push(format!(
            "  Oxygen Sensor Htr: {}",
            get_status((c & (1 << 6)) != 0, (d & (1 << 6)) != 0)
        ));
        report.push(format!(
            "  Oxygen Sensor:     {}",
            get_status((c & (1 << 5)) != 0, (d & (1 << 5)) != 0)
        ));
        report.push(format!(
            "  Gas Particulate F: {}",
            get_status((c & (1 << 4)) != 0, (d & (1 << 4)) != 0)
        ));
        report.push(format!(
            "  Secondary Air Sys: {}",
            get_status((c & (1 << 3)) != 0, (d & (1 << 3)) != 0)
        ));
        report.push(format!(
            "  Evaporative System:{}",
            get_status((c & (1 << 2)) != 0, (d & (1 << 2)) != 0)
        ));
        report.push(format!(
            "  Heated Catalyst:   {}",
            get_status((c & (1 << 1)) != 0, (d & (1 << 1)) != 0)
        ));
        report.push(format!(
            "  Catalyst:          {}",
            get_status((c & (1 << 0)) != 0, (d & (1 << 0)) != 0)
        ));

        // Join all the lines into a single string for display
        report.join("\n")
    })())
}

/// Parser for fuel trim percentage: ((A - 128) * 100) / 128
pub fn parse_fuel_trim(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number((a - 128.0) * 100.0 / 128.0, "%")
}

/// Parser for oxygen sensor voltage: A / 200.0
pub fn parse_oxygen_sensor_voltage(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a / 200.0, "V")
}

/// Parser for manifold pressure: A (kPa)
pub fn parse_manifold_pressure(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a, "kPa")
}

/// Parser for timing advance: (A - 128) / 2
pub fn parse_timing_advance(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number((a - 128.0) / 2.0, "° before TDC")
}

/// Parser for intake air temperature: A - 40
pub fn parse_intake_air_temp(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a - 40.0, "°C")
}

/// Parser for MAF air flow: (256*A + B) / 100
pub fn parse_maf_flow(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word / 100.0, "g/s")
}

/// Parser for OBD standards (PID 0x1C)
pub fn parse_obd_standards(data: &[u8]) -> Value {
    Value::Text((|| -> String {
        if data.is_empty() {
            return "N/A".to_string();
        }
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
            x => format!("Unknown standard: 0x{x:02X}"),
        }
    })())
}

/// Parser for distance with MIL on: (256*A + B)
pub fn parse_distance_with_mil(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word, "km")
}

/// Parser for fuel rail pressure: (256*A + B) * 3
pub fn parse_fuel_rail_pressure(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word * 3.0, "kPa")
}

/// Parser for commanded EGR: (A * 100) / 255
pub fn parse_commanded_egr(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a * 100.0 / 255.0, "%")
}

/// Parser for EGR error: ((A - 128) * 100) / 128
pub fn parse_egr_error(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number((a - 128.0) * 100.0 / 128.0, "%")
}

/// Parser for fuel level input: (A * 100) / 255
pub fn parse_fuel_level(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a * 100.0 / 255.0, "%")
}

/// Parser for barometric pressure: A
pub fn parse_barometric_pressure(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a, "kPa")
}

/// Parser for catalyst temperature: ((256*A + B) / 10) - 40
pub fn parse_catalyst_temp(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word / 10.0 - 40.0, "°C")
}

/// Parser for control module voltage: (256*A + B) / 1000
pub fn parse_control_module_voltage(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word / 1000.0, "V")
}

/// Parser for absolute load value: ((256*A + B) * 100) / 255
pub fn parse_absolute_load(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word * 100.0 / 255.0, "%")
}

/// Parser for commanded air-fuel equivalence ratio: (256*A + B) / 32768
pub fn parse_air_fuel_ratio(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word / 32768.0, "ratio")
}

/// Parser for relative throttle position: (A * 100) / 255
pub fn parse_relative_throttle(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a * 100.0 / 255.0, "%")
}

/// Parser for ambient air temperature: A - 40
pub fn parse_ambient_air_temp(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a - 40.0, "°C")
}

/// Parser for accelerator pedal position: (A * 100) / 255
pub fn parse_accelerator_pedal(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a * 100.0 / 255.0, "%")
}

/// Parser for hybrid battery pack remaining life: (A * 100) / 255
pub fn parse_battery_life(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a * 100.0 / 255.0, "%")
}

/// Parser for engine oil temperature: A - 40
pub fn parse_oil_temp(data: &[u8]) -> Value {
    if data.is_empty() {
        return Value::Text("N/A".into());
    }
    let a = data[0] as f64;
    Value::number(a - 40.0, "°C")
}

/// Parser for fuel injection timing: (((256*A + B) - 26880) / 128)
pub fn parse_fuel_injection_timing(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number((word - 26880.0) / 128.0, "°")
}

/// Parser for engine fuel rate: (256*A + B) / 20
pub fn parse_fuel_rate(data: &[u8]) -> Value {
    if data.len() < 2 {
        return Value::Text("N/A".into());
    }
    let word = u16::from_be_bytes([data[0], data[1]]) as f64;
    Value::number(word / 20.0, "L/h")
}

/// Wide-range lambda/current response (PIDs 34–3B), with nominal scaling.
pub fn parse_oxygen_sensor_wr_lambda(data: &[u8]) -> Value {
    super::oxygen::wide(0x34, data, [0; 4])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One representative of each formula shape, checked at both ends of the
    /// range — these are the numbers a user reads off the dashboard.
    #[test]
    fn scaling_formulas_match_the_obd_spec() {
        let number = |v: Value| match v {
            Value::Number { value, .. } => value,
            other => panic!("{other:?}"),
        };
        assert_eq!(number(parse_rpm(&[0x1A, 0xF8])), 1726.0);
        assert_eq!(number(parse_rpm(&[0, 1])), 0.25);
        assert_eq!(number(parse_a_minus_40(&[0])), -40.0);
        assert!((number(parse_a_times_100_div_255(&[128])) - 50.196078).abs() < 0.00001);
        assert_eq!(number(parse_fuel_trim(&[255])), 99.21875);
        assert_eq!(number(parse_timing_advance(&[0])), -64.0);
        assert_eq!(number(parse_maf_flow(&[0x0F, 0xA0])), 40.0);
        assert_eq!(number(parse_catalyst_temp(&[0x10, 0x69])), 380.1);
        assert_eq!(number(parse_control_module_voltage(&[0x37, 0x1A])), 14.106);
        assert_eq!(number(parse_fuel_rail_pressure(&[255, 255])), 196605.0);
    }

    /// Every parser is handed `&data[2..]` straight out of the response, so a
    /// short or empty ECU reply must degrade to "N/A" rather than panic.
    #[test]
    fn short_data_never_panics() {
        let two_byte_parsers: [fn(&[u8]) -> Value; 4] = [
            parse_rpm,
            parse_maf_flow,
            parse_catalyst_temp,
            parse_control_module_voltage,
        ];
        for parser in two_byte_parsers {
            assert_eq!(parser(&[]).to_string(), "N/A");
            assert_eq!(parser(&[0x12]).to_string(), "N/A");
        }

        let one_byte_parsers: [fn(&[u8]) -> Value; 3] =
            [parse_identity, parse_fuel_trim, parse_a_minus_40];
        for parser in one_byte_parsers {
            assert_eq!(parser(&[]).to_string(), "N/A");
        }

        assert!(parse_monitor_status(&[0x01, 0x02])
            .to_string()
            .starts_with("N/A"));
        assert_eq!(
            parse_oxygen_sensor_wr_lambda(&[0x01, 0x02, 0x03]).to_string(),
            "N/A"
        );
    }

    /// PID 0x01 packs the MIL lamp and the stored-DTC count into byte A; the
    /// read-dtc flow and this display must agree on that count.
    #[test]
    fn monitor_status_reports_mil_state_and_dtc_count() {
        let on = parse_monitor_status(&[0x83, 0x07, 0x00, 0x00]);
        assert!(on.to_string().contains("MIL Status: ON"), "{on}");
        assert!(on.to_string().contains("DTC Count: 3"), "{on}");

        let off = parse_monitor_status(&[0x00, 0x00, 0x00, 0x00]);
        assert!(off.to_string().contains("MIL Status: OFF"), "{off}");
        assert!(off.to_string().contains("DTC Count: 0"), "{off}");
    }

    /// Availability and completeness live in different bytes/bits, and the
    /// completeness bit is inverted (0 = complete).
    #[test]
    fn monitor_status_distinguishes_unavailable_from_incomplete() {
        // B: misfire (bit 0) available, complete (bit 4 clear).
        let s = parse_monitor_status(&[0x00, 0x01, 0x00, 0x00]);
        assert!(s.to_string().contains("Misfire:           Complete"), "{s}");

        // B: misfire available and incomplete (bit 4 set).
        let s = parse_monitor_status(&[0x00, 0x11, 0x00, 0x00]);
        assert!(
            s.to_string().contains("Misfire:           Incomplete"),
            "{s}"
        );

        // Not supported at all.
        let s = parse_monitor_status(&[0x00, 0x00, 0x00, 0x00]);
        assert!(
            s.to_string().contains("Misfire:           Not Available"),
            "{s}"
        );
    }

    #[test]
    fn unknown_obd_standard_falls_back_to_raw_value() {
        assert_eq!(parse_obd_standards(&[0x06]).to_string(), "EOBD (Europe)");
        assert_eq!(
            parse_obd_standards(&[0xEE]).to_string(),
            "Unknown standard: 0xEE"
        );
        assert_eq!(parse_obd_standards(&[]).to_string(), "N/A");
    }
}
