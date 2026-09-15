use super::*;
use crate::{obd::pids::get_pid_definition, ObdProtocol};

fn number(value: &Value, field: &str) -> f64 {
    match value.field(field) {
        Some(Value::Number { value, .. }) => *value,
        other => panic!("Expected numeric {field}, got {other:?}"),
    }
}

#[test]
fn every_sensor_family_has_complete_definitions_and_rejects_short_data() {
    for base in [0x14, 0x24, 0x34] {
        let len = if base == 0x14 { 2 } else { 4 };
        for pid in base..base + 8 {
            let def = get_pid_definition(1, pid).unwrap();
            assert_eq!(def.len, Some(len));
            assert_eq!(Some(def.name.to_string()), name(pid, Layout::Unknown));
            for short in 0..len {
                assert_eq!((def.parser)(&vec![0; short]), Value::Unavailable);
            }
        }
    }
    assert_eq!(get_pid_definition(1, 0x1D).unwrap().len, Some(1));
    assert_eq!(get_pid_definition(1, 0x4F).unwrap().len, Some(4));
}

#[test]
fn layouts_map_all_eight_sensors_and_do_not_guess_for_ambiguous_support() {
    for (layout, expected) in [
        (
            Layout::TwoBanks,
            [
                "B1S1", "B1S2", "B1S3", "B1S4", "B2S1", "B2S2", "B2S3", "B2S4",
            ],
        ),
        (
            Layout::FourBanks,
            [
                "B1S1", "B1S2", "B2S1", "B2S2", "B3S1", "B3S2", "B4S1", "B4S2",
            ],
        ),
    ] {
        for (index, sensor) in expected.into_iter().enumerate() {
            assert_eq!(
                name(0x14 + index as u8, layout).unwrap(),
                format!("O2_{sensor}")
            );
            assert_eq!(
                name(0x24 + index as u8, layout).unwrap(),
                format!("O2_{sensor}_WR_VOLTAGE")
            );
            assert_eq!(
                name(0x34 + index as u8, layout).unwrap(),
                format!("O2_{sensor}_WR_CURRENT")
            );
        }
    }
    assert_eq!(Layout::from_supported(&[0x13]), Layout::TwoBanks);
    assert_eq!(Layout::from_supported(&[0x1D]), Layout::FourBanks);
    for pids in [&[][..], &[0x13, 0x1D][..]] {
        assert_eq!(Layout::from_supported(pids), Layout::Unknown);
    }
    assert_eq!(
        locations(&[0x11], Layout::TwoBanks).to_string(),
        "B1S1, B2S1"
    );
    assert_eq!(
        locations(&[0x11], Layout::FourBanks).to_string(),
        "B1S1, B3S1"
    );
    assert_eq!(locations(&[], Layout::TwoBanks), Value::Unavailable);
}

#[test]
fn wideband_current_and_voltage_are_distinct_numeric_measurements() {
    for pid in 0x24..=0x2B {
        let value = ObdProtocol::decode_pid(1, pid, &[0x80, 0, 0x40, 0]).value;
        assert_eq!(number(&value, "lambda"), 1.0);
        assert_eq!(number(&value, "voltage"), 2.0);
        assert!(value.field("current").is_none());
        let max = wide(pid, &[0xFF; 4], [0; 4]);
        assert_eq!(number(&max, "lambda"), 1.999969482421875);
        assert_eq!(number(&max, "voltage"), 7.9998779296875);
    }
    for pid in 0x34..=0x3B {
        for (raw, current) in [
            (0x0000u16, -128.0),
            (0x8000, 0.0),
            (0x8100, 1.0),
            (0xFFFF, 127.99609375),
        ] {
            let [c, d] = raw.to_be_bytes();
            let value = ObdProtocol::decode_pid(1, pid, &[0x80, 0, c, d]).value;
            assert_eq!(number(&value, "lambda"), 1.0);
            assert_eq!(number(&value, "current"), current);
            assert!(value.field("voltage").is_none());
        }
    }
}

#[test]
fn pid_4f_scaling_uses_per_field_defaults_and_the_spec_examples() {
    let value = wide(0x24, &[0x7D, 0x00, 0x9C, 0x40], [4, 16, 0, 0]);
    assert!((number(&value, "lambda") - 1.953154802777142).abs() < 1e-12);
    assert!((number(&value, "voltage") - 9.76577401388571).abs() < 1e-12);
    assert_eq!(
        number(&wide(0x34, &[0, 0, 0x9C, 0x40], [0, 0, 64, 0]), "current"),
        14.125
    );
    assert_eq!(
        number(&wide(0x34, &[0, 0, 0x5C, 0x40], [0, 0, 64, 0]), "current"),
        -17.875
    );
    let voltage_only = wide(0x24, &[0x80, 0, 0xFF, 0xFF], [0, 16, 0, 0]);
    assert_eq!(number(&voltage_only, "lambda"), 1.0);
    assert_eq!(number(&voltage_only, "voltage"), 16.0);
    assert_eq!(
        scaled_value(0x0B, &[0xFF], [0, 0, 0, 77]),
        Some(Value::number(770.0, "kPa"))
    );
    assert_eq!(
        scaled_value(0x44, &[0xFF, 0xFF], [4, 0, 0, 0]),
        Some(Value::number(4.0, "ratio"))
    );
    assert_eq!(scaling_limits(&[4, 16, 64]), Value::Unavailable);
}
