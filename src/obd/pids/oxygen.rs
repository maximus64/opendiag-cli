//! SAE J1979-DA oxygen sensor locations, measurements and PID 4F scaling.
// J1979-DA OCT2011: tables B20–B22, B24, B30–B31, B40–B41 and B60.
use super::{PidCategory, PidDefinition};
use crate::value::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Layout {
    #[default]
    Unknown,
    TwoBanks,
    FourBanks,
}

impl Layout {
    pub fn from_supported(pids: &[u8]) -> Self {
        match (pids.contains(&0x13), pids.contains(&0x1D)) {
            (true, false) => Self::TwoBanks,
            (false, true) => Self::FourBanks,
            _ => Self::Unknown,
        }
    }

    fn sensor(self, index: u8) -> String {
        match self {
            Self::TwoBanks => format!("B{}S{}", index / 4 + 1, index % 4 + 1),
            Self::FourBanks => format!("B{}S{}", index / 2 + 1, index % 2 + 1),
            Self::Unknown => format!("SENSOR_{}", index + 1),
        }
    }
}

pub fn is_sensor(pid: u8) -> bool {
    matches!(pid, 0x14..=0x1B | 0x24..=0x2B | 0x34..=0x3B)
}

pub fn uses_scaling(pid: u8) -> bool {
    matches!(pid, 0x0B | 0x24..=0x2B | 0x34..=0x3B | 0x44)
}

pub fn name(pid: u8, layout: Layout) -> Option<String> {
    let (index, suffix) = match pid {
        0x14..=0x1B => (pid - 0x14, ""),
        0x24..=0x2B => (pid - 0x24, "_WR_VOLTAGE"),
        0x34..=0x3B => (pid - 0x34, "_WR_CURRENT"),
        _ => return None,
    };
    Some(format!("O2_{}{suffix}", layout.sensor(index)))
}

pub struct Field {
    pub key: &'static str,
    pub suffix: &'static str,
    pub unit: &'static str,
}

pub fn fields(pid: u8) -> &'static [Field] {
    match pid {
        0x14..=0x1B => &[
            Field {
                key: "voltage",
                suffix: "",
                unit: "V",
            },
            Field {
                key: "short_term_fuel_trim",
                suffix: "_STFT",
                unit: "%",
            },
        ],
        0x24..=0x2B => &[
            Field {
                key: "lambda",
                suffix: "_LAMBDA",
                unit: "ratio",
            },
            Field {
                key: "voltage",
                suffix: "",
                unit: "V",
            },
        ],
        0x34..=0x3B => &[
            Field {
                key: "lambda",
                suffix: "_LAMBDA",
                unit: "ratio",
            },
            Field {
                key: "current",
                suffix: "",
                unit: "mA",
            },
        ],
        0x4F => &[
            Field {
                key: "lambda",
                suffix: "_LAMBDA",
                unit: "ratio",
            },
            Field {
                key: "voltage",
                suffix: "_VOLTAGE",
                unit: "V",
            },
            Field {
                key: "current",
                suffix: "_CURRENT",
                unit: "mA",
            },
            Field {
                key: "manifold_pressure",
                suffix: "_MAP",
                unit: "kPa",
            },
        ],
        _ => &[],
    }
}

pub fn conventional(data: &[u8]) -> Value {
    let [a, b, ..] = data else {
        return Value::Unavailable;
    };
    Value::fields([
        ("voltage", Value::number(f64::from(*a) / 200.0, "V")),
        (
            "short_term_fuel_trim",
            if *b == 0xFF {
                Value::Unavailable
            } else {
                Value::number((f64::from(*b) - 128.0) * 100.0 / 128.0, "%")
            },
        ),
    ])
}

fn lambda(raw: u16, maximum: u8) -> f64 {
    if maximum == 0 {
        f64::from(raw) / 32768.0
    } else {
        f64::from(raw) * f64::from(maximum) / 65535.0
    }
}

pub fn wide(pid: u8, data: &[u8], limits: [u8; 4]) -> Value {
    let [a, b, c, d, ..] = data else {
        return Value::Unavailable;
    };
    let ratio = lambda(u16::from_be_bytes([*a, *b]), limits[0]);
    let raw = u16::from_be_bytes([*c, *d]);
    let (key, value) = if (0x24..=0x2B).contains(&pid) {
        let voltage = if limits[1] == 0 {
            f64::from(raw) / 8192.0
        } else {
            f64::from(raw) * f64::from(limits[1]) / 65535.0
        };
        ("voltage", Value::number(voltage, "V"))
    } else {
        let maximum = if limits[2] == 0 {
            128.0
        } else {
            f64::from(limits[2])
        };
        let current = (f64::from(raw) - 32768.0) * maximum / 32768.0;
        ("current", Value::number(current, "mA"))
    };
    Value::fields([("lambda", Value::number(ratio, "ratio")), (key, value)])
}

/// PID 4F zero bytes request nominal scaling; report their raw zero values.
fn scaling_limits(data: &[u8]) -> Value {
    let [a, b, c, d, ..] = data else {
        return Value::Unavailable;
    };
    Value::fields([
        ("lambda", Value::number(f64::from(*a), "ratio")),
        ("voltage", Value::number(f64::from(*b), "V")),
        ("current", Value::number(f64::from(*c), "mA")),
        (
            "manifold_pressure",
            Value::number(f64::from(*d) * 10.0, "kPa"),
        ),
    ])
}

pub fn scaled_value(pid: u8, data: &[u8], limits: [u8; 4]) -> Option<Value> {
    match pid {
        0x24..=0x2B | 0x34..=0x3B => Some(wide(pid, data, limits)),
        0x44 if data.len() >= 2 => Some(Value::number(
            lambda(u16::from_be_bytes([data[0], data[1]]), limits[0]),
            "ratio",
        )),
        0x0B if !data.is_empty() && limits[3] != 0 => Some(Value::number(
            f64::from(data[0]) * f64::from(limits[3]) * 10.0 / 255.0,
            "kPa",
        )),
        _ => None,
    }
}

fn locations(data: &[u8], layout: Layout) -> Value {
    let Some(bits) = data.first() else {
        return Value::Unavailable;
    };
    let sensors: Vec<_> = (0..8)
        .filter(|index| bits & (1 << index) != 0)
        .map(|index| layout.sensor(index))
        .collect();
    Value::Text(if sensors.is_empty() {
        "None".into()
    } else {
        sensors.join(", ")
    })
}

pub fn register(pids: &mut HashMap<u8, PidDefinition>) {
    let conventional_names = [
        "O2_SENSOR_1",
        "O2_SENSOR_2",
        "O2_SENSOR_3",
        "O2_SENSOR_4",
        "O2_SENSOR_5",
        "O2_SENSOR_6",
        "O2_SENSOR_7",
        "O2_SENSOR_8",
    ];
    let voltage_names = [
        "O2_SENSOR_1_WR_VOLTAGE",
        "O2_SENSOR_2_WR_VOLTAGE",
        "O2_SENSOR_3_WR_VOLTAGE",
        "O2_SENSOR_4_WR_VOLTAGE",
        "O2_SENSOR_5_WR_VOLTAGE",
        "O2_SENSOR_6_WR_VOLTAGE",
        "O2_SENSOR_7_WR_VOLTAGE",
        "O2_SENSOR_8_WR_VOLTAGE",
    ];
    let current_names = [
        "O2_SENSOR_1_WR_CURRENT",
        "O2_SENSOR_2_WR_CURRENT",
        "O2_SENSOR_3_WR_CURRENT",
        "O2_SENSOR_4_WR_CURRENT",
        "O2_SENSOR_5_WR_CURRENT",
        "O2_SENSOR_6_WR_CURRENT",
        "O2_SENSOR_7_WR_CURRENT",
        "O2_SENSOR_8_WR_CURRENT",
    ];
    for (base, names, len, description, parser) in [
        (
            0x14,
            conventional_names,
            2,
            "Oxygen sensor voltage and short-term fuel trim",
            conventional as fn(&[u8]) -> Value,
        ),
        (
            0x24,
            voltage_names,
            4,
            "Wideband oxygen sensor lambda and voltage",
            (|data: &[u8]| wide(0x24, data, [0; 4])) as fn(&[u8]) -> Value,
        ),
        (
            0x34,
            current_names,
            4,
            "Wideband oxygen sensor lambda and current",
            (|data: &[u8]| wide(0x34, data, [0; 4])) as fn(&[u8]) -> Value,
        ),
    ] {
        for (index, name) in names.into_iter().enumerate() {
            pids.insert(
                base + index as u8,
                PidDefinition {
                    len: Some(len),
                    name,
                    description,
                    unit: "",
                    category: PidCategory::Engine,
                    parser,
                },
            );
        }
    }
    for (pid, name, layout) in [
        (0x13, "O2_SENSORS_PRESENT", Layout::TwoBanks),
        (0x1D, "O2_SENSORS_PRESENT_4_BANKS", Layout::FourBanks),
    ] {
        pids.insert(
            pid,
            PidDefinition {
                len: Some(1),
                name,
                description: "Oxygen sensor locations",
                unit: "",
                category: PidCategory::Engine,
                parser: match layout {
                    Layout::TwoBanks => |data| locations(data, Layout::TwoBanks),
                    _ => |data| locations(data, Layout::FourBanks),
                },
            },
        );
    }
    pids.insert(
        0x4F,
        PidDefinition {
            len: Some(4),
            name: "O2_SCALING_MAX",
            description: "Oxygen sensor and MAP scaling maxima (zero selects nominal scaling)",
            unit: "",
            category: PidCategory::Engine,
            parser: scaling_limits,
        },
    );
}

#[cfg(test)]
mod tests;
