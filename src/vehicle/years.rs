use serde::{de, Deserialize, Deserializer, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct YearRange {
    pub from: u16,
    pub to: u16,
}

impl<'de> Deserialize<'de> for YearRange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            from: u16,
            to: u16,
        }

        struct RangeVisitor;
        impl<'de> de::Visitor<'de> for RangeVisitor {
            type Value = YearRange;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a year range with named from and to fields")
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<YearRange, M::Error> {
                let fields = Fields::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(YearRange {
                    from: fields.from,
                    to: fields.to,
                })
            }
        }

        // Derived struct deserialization also accepts positional arrays.
        deserializer.deserialize_map(RangeVisitor)
    }
}

impl YearRange {
    pub fn contains(&self, year: u16) -> bool {
        (self.from..=self.to).contains(&year)
    }

    pub fn overlaps(&self, other: &Self) -> bool {
        self.from <= other.to && other.from <= self.to
    }
}

pub fn contains(ranges: &[YearRange], year: u16) -> bool {
    ranges.iter().any(|range| range.contains(year))
}

pub fn validate(ranges: &[YearRange]) -> Result<(), &'static str> {
    if ranges.is_empty() {
        return Err("must contain at least one year range");
    }
    for (index, range) in ranges.iter().enumerate() {
        if range.from > range.to {
            return Err("year range from must not exceed to");
        }
        if ranges[..index].iter().any(|other| range.overlaps(other)) {
            return Err("model-year ranges must not overlap");
        }
    }
    Ok(())
}

pub fn format(ranges: &[YearRange]) -> String {
    ranges
        .iter()
        .map(|range| {
            if range.from == range.to {
                range.from.to_string()
            } else {
                format!("{}–{}", range.from, range.to)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}
