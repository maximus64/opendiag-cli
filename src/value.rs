use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Value {
    Number { value: f64, unit: String },
    Unsigned { value: u64, unit: String },
    Text(String),
    Fields(BTreeMap<String, Value>),
    Unavailable,
}
impl Value {
    pub fn fields<const N: usize>(fields: [(&str, Value); N]) -> Self {
        Self::Fields(
            fields
                .into_iter()
                .map(|(name, value)| (name.into(), value))
                .collect(),
        )
    }
    pub fn field(&self, name: &str) -> Option<&Value> {
        match self {
            Self::Fields(fields) => fields.get(name),
            _ => None,
        }
    }
    pub fn number(value: f64, unit: impl Into<String>) -> Self {
        Self::Number {
            value,
            unit: unit.into(),
        }
    }
    pub fn with_unit(self, unit: &str) -> Self {
        match self {
            Self::Number { value, .. } => Self::number(value, unit),
            Self::Unsigned { value, .. } => Self::Unsigned {
                value,
                unit: unit.into(),
            },
            other => other,
        }
    }
}
/// Decimals a reading is worth printing.
///
/// Readings are scaled byte counters, so the `f64` behind one is usually not
/// the precision of the measurement: `a * 100 / 255` never terminates, and
/// printing it in full gives `8.235294117647058 %`. Terminating scalings such
/// as `word / 4` or `word / 1000` are exact and worth keeping, so prefer the
/// shortest decimal form that is exactly the value, and otherwise round to four
/// significant digits, which never rounds an integer digit away.
fn format_number(value: f64) -> String {
    const MAX_DECIMALS: usize = 6;
    const SIGNIFICANT: i32 = 4;
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return "0".into();
    }
    // `f64`'s own `Display` is the shortest decimal that reads back as this
    // value, so its fractional length is the resolution the value really has.
    let exact = value.to_string();
    if exact
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len())
        <= MAX_DECIMALS
    {
        return exact;
    }
    let magnitude = value.abs().log10().floor() as i32;
    let decimals = (SIGNIFICANT - 1 - magnitude).clamp(0, MAX_DECIMALS as i32) as usize;
    let text = format!("{value:.decimals$}");
    if text.contains('.') {
        // Zeros past the rounded digits would overstate the resolution.
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number { value, unit } => write!(
                f,
                "{}{}{}",
                format_number(*value),
                if unit.is_empty() { "" } else { " " },
                unit
            ),
            Self::Text(text) => f.write_str(text),
            Self::Unavailable => f.write_str("N/A"),
            Self::Fields(fields) => {
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {value}")?;
                }
                Ok(())
            }
            Self::Unsigned { value, unit } => write!(
                f,
                "{}{}{}",
                value,
                if unit.is_empty() { "" } else { " " },
                unit
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_scalings_are_kept_and_repeating_ones_round_to_four_significant_digits() {
        for (value, expected) in [
            // `a * 100 / 255`, the throttle and pedal family.
            (8.235294117647058, "8.235"),
            (0.7843137254901961, "0.7843"),
            (50.19607843137255, "50.2"),
            (100.0, "100"),
            // Terminating scalings stay exact whatever their length.
            (1726.25, "1726.25"),
            (14.106, "14.106"),
            (380.1, "380.1"),
            (99.21875, "99.21875"),
            (-40.0, "-40"),
            (0.0, "0"),
            // Integer digits are never rounded away.
            (196605.0, "196605"),
            // A lambda ratio keeps the resolution its range needs.
            (0.99993896484375, "0.9999"),
        ] {
            assert_eq!(format_number(value), expected, "{value}");
        }
    }

    #[test]
    fn units_follow_the_number_and_fields_format_their_members() {
        assert_eq!(Value::number(8.235294117647058, "%").to_string(), "8.235 %");
        assert_eq!(Value::number(1.5, "").to_string(), "1.5");
        assert_eq!(
            Value::fields([("lambda", Value::number(0.99993896484375, ""))]).to_string(),
            "lambda: 0.9999"
        );
        // Counters are already exact and must not gain a decimal point.
        assert_eq!(
            Value::Unsigned {
                value: 196605,
                unit: "kPa".into()
            }
            .to_string(),
            "196605 kPa"
        );
    }
}
