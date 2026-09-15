//! CSV output for sampled signals and parsing of UDS data identifier (DID) fields.
//!
//! [`CsvSink`] implements [`crate::sampler::SampleSink`] for both OBD signals
//! and DID fields. The shared [`crate::sampler`] groups reads and sends each
//! completed sample to the sink. Rows are flushed as they are written, keeping
//! completed samples available when logging stops.
//!
//! For DID fields, the user selects a whole response or a slice of one.
//! [`run_with_cancel`] turns those fields into signals and starts the sampler.
//!
//! Field syntax, with numbers accepted as `0x` hex or decimal like every other
//! flag:
//!
//! | Spec              | Column value                                        |
//! | ----------------- | --------------------------------------------------- |
//! | `0x2600`          | the whole response, as hex                          |
//! | `0x2610:4`        | byte 4, unsigned decimal                            |
//! | `0x2610:4:2`      | 2 bytes from offset 4, big-endian unsigned (1-8)    |
//! | `0x2610:4.7`      | bit 7 (the MSB) of byte 4, `0` or `1`               |
//! | `rpm=0x2610:4:2`  | any of the above, with `rpm` as the column header   |
//!
//! Offsets count from the first data byte, i.e. after the `62 hi lo` echo.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::errors::DiagError;
use crate::obd::ObdProtocol;
use crate::utils::bytes_to_hex;

/// Which part of a DID's response a field reports.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub enum Extract {
    /// The whole response, hex encoded.
    Raw,
    /// `len` bytes from `offset`, read as a big-endian unsigned integer.
    Unsigned { offset: usize, len: usize },
    /// One bit of the byte at `offset`; bit 0 is the least significant.
    Bit { offset: usize, bit: u8 },
}

/// One CSV column.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Field {
    pub name: String,
    pub did: u16,
    pub extract: Extract,
}

impl Field {
    /// Formats this field out of a DID response, or `None` if the response is
    /// too short to contain it.
    pub fn value(&self, data: &[u8]) -> Option<String> {
        match self.extract {
            Extract::Raw => Some(bytes_to_hex(data)),
            Extract::Unsigned { offset, len } => {
                let bytes = data.get(offset..offset.checked_add(len)?)?;
                let value = bytes.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64);
                Some(value.to_string())
            }
            Extract::Bit { offset, bit } => {
                data.get(offset).map(|byte| ((byte >> bit) & 1).to_string())
            }
        }
    }
}

/// Parses one field spec; see the module docs for the syntax.
pub fn parse_field(spec: &str) -> Result<Field, String> {
    parse_field_inner(spec.trim()).map_err(|e| format!("invalid field '{}': {}", spec.trim(), e))
}

fn parse_field_inner(spec: &str) -> Result<Field, String> {
    let (name, body) = match spec.split_once('=') {
        Some((name, body)) => {
            let name = name.trim();
            if name.is_empty() {
                return Err("empty column name before '='".into());
            }
            (Some(name), body.trim())
        }
        None => (None, spec),
    };

    let (location, bit) = match body.split_once('.') {
        Some((location, bit)) => (location, Some(bit)),
        None => (body, None),
    };
    let parts: Vec<&str> = location.split(':').collect();

    let did = crate::parse_hex_u32(parts[0]).map_err(|e| format!("DID: {e}"))?;
    let did = u16::try_from(did).map_err(|_| format!("DID 0x{did:X} is wider than 16 bits"))?;

    let number = |what: &str, s: &str| -> Result<usize, String> {
        crate::parse_hex_u32(s)
            .map(|v| v as usize)
            .map_err(|e| format!("{what}: {e}"))
    };

    let extract = match (&parts[1..], bit) {
        ([], None) => Extract::Raw,
        ([offset], None) => Extract::Unsigned {
            offset: number("offset", offset)?,
            len: 1,
        },
        ([offset, len], None) => {
            let len = number("length", len)?;
            if !(1..=8).contains(&len) {
                return Err(format!("length {len} is outside 1-8 bytes"));
            }
            Extract::Unsigned {
                offset: number("offset", offset)?,
                len,
            }
        }
        ([offset], Some(bit)) => {
            let bit = number("bit", bit)?;
            if bit > 7 {
                return Err(format!("bit {bit} is outside 0-7"));
            }
            Extract::Bit {
                offset: number("offset", offset)?,
                bit: bit as u8,
            }
        }
        ([], Some(_)) => return Err("a bit needs a byte offset, e.g. 0x2610:4.7".into()),
        ([_, _], Some(_)) => return Err("a bit is taken from a single byte, not a length".into()),
        _ => return Err("expected DID[:OFFSET[:LENGTH]] or DID:OFFSET.BIT".into()),
    };

    let name = match name {
        Some(name) => name.to_string(),
        None => match extract {
            Extract::Raw => format!("0x{did:04X}"),
            Extract::Unsigned { offset, len: 1 } => format!("0x{did:04X}:{offset}"),
            Extract::Unsigned { offset, len } => format!("0x{did:04X}:{offset}:{len}"),
            Extract::Bit { offset, bit } => format!("0x{did:04X}:{offset}.{bit}"),
        },
    };

    Ok(Field { name, did, extract })
}

/// Reads field specs from a file, one per line. Blank lines and lines starting
/// with `#` are skipped.
pub fn load_fields_file(path: &Path) -> Result<Vec<Field>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    text.lines()
        .enumerate()
        .map(|(i, line)| (i, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(|(i, line)| {
            parse_field(line).map_err(|e| format!("{}:{}: {}", path.display(), i + 1, e))
        })
        .collect()
}

/// When to stop and how fast to go.
pub struct LogOptions {
    /// Minimum time between the starts of consecutive samples. Zero means as
    /// fast as the bus allows.
    pub interval: Duration,
    /// Stop after this many rows.
    pub samples: Option<u64>,
    /// Stop once this much time has passed.
    pub duration: Option<Duration>,
}

/// Quotes a CSV cell if it needs it.
fn csv_escape(cell: &str) -> String {
    if cell.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_string()
    }
}

/// Writes timestamped samples to CSV, flushing the header and each completed row.
/// Missing or unavailable values produce empty cells.
pub struct CsvSink<W: Write> {
    pub out: W,
}
impl<W: Write> crate::sampler::SampleSink for CsvSink<W> {
    fn begin(&mut self, signals: &[crate::sampler::SignalInfo]) -> Result<(), DiagError> {
        let mut header = vec!["unix_ms".to_string(), "elapsed_ms".to_string()];
        header.extend(signals.iter().map(|signal| csv_escape(&signal.name)));
        writeln!(self.out, "{}", header.join(","))?;
        self.out.flush()?;
        Ok(())
    }
    fn sample(&mut self, sample: &crate::sampler::Sample) -> Result<(), DiagError> {
        let mut row = vec![sample.unix_ms.to_string(), sample.elapsed_ms.to_string()];
        row.extend(sample.values.iter().map(|value| {
            let cell = match value {
                Some(crate::value::Value::Number { value, .. }) => value.to_string(),
                Some(crate::value::Value::Unsigned { value, .. }) => value.to_string(),
                Some(crate::value::Value::Text(text)) => text.clone(),
                Some(value @ crate::value::Value::Fields(_)) => value.to_string(),
                Some(crate::value::Value::Unavailable) | None => String::new(),
            };
            csv_escape(&cell)
        }));
        writeln!(self.out, "{}", row.join(","))?;
        self.out.flush()?;
        Ok(())
    }
}

/// Logs DID fields for the selected ECU until a limit or cancellation request,
/// returning the number of completed samples.
///
/// Recoverable read failures leave cells empty; other errors stop sampling.
pub fn run_with_cancel<W: Write>(
    od: &mut ObdProtocol,
    fields: &[Field],
    out: &mut W,
    opts: &LogOptions,
    cancel: &crate::events::Cancel,
) -> Result<u64, DiagError> {
    let ecu = od.target().ok_or_else(|| {
        DiagError::InvalidCommand("Select a DID datalog ECU before sampling".into())
    })?;
    let signals: Vec<_> = fields
        .iter()
        .cloned()
        .map(|field| crate::sampler::SignalInfo::did(ecu, field))
        .collect();
    let result = crate::sampler::run(od, &signals, &mut CsvSink { out }, opts, cancel)?;
    Ok(result.samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::protocol_with;
    fn run<W: Write>(
        od: &mut ObdProtocol,
        fields: &[Field],
        out: &mut W,
        opts: &LogOptions,
    ) -> Result<u64, DiagError> {
        od.set_header_id(0x18DA01F1)?;
        run_with_cancel(od, fields, out, opts, &crate::Cancel::default())
    }

    fn once(samples: u64) -> LogOptions {
        LogOptions {
            interval: Duration::ZERO,
            samples: Some(samples),
            duration: None,
        }
    }

    /// Drops the two timestamp columns, which differ on every run.
    fn without_timestamps(csv: &str) -> Vec<String> {
        csv.lines()
            .map(|line| line.splitn(3, ',').nth(2).unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn parses_every_field_shape_with_a_default_name() {
        assert_eq!(
            parse_field("0x2600").unwrap(),
            Field {
                name: "0x2600".into(),
                did: 0x2600,
                extract: Extract::Raw
            }
        );
        assert_eq!(
            parse_field("0x2610:4").unwrap(),
            Field {
                name: "0x2610:4".into(),
                did: 0x2610,
                extract: Extract::Unsigned { offset: 4, len: 1 }
            }
        );
        assert_eq!(
            parse_field("0x2610:0x10:2").unwrap(),
            Field {
                name: "0x2610:16:2".into(),
                did: 0x2610,
                extract: Extract::Unsigned { offset: 16, len: 2 }
            }
        );
        assert_eq!(
            parse_field("0x2610:4.7").unwrap(),
            Field {
                name: "0x2610:4.7".into(),
                did: 0x2610,
                extract: Extract::Bit { offset: 4, bit: 7 }
            }
        );
    }

    #[test]
    fn a_name_before_equals_becomes_the_column_header() {
        let field = parse_field(" rpm = 0x2610:4:2 ").unwrap();
        assert_eq!(field.name, "rpm");
        assert_eq!(field.extract, Extract::Unsigned { offset: 4, len: 2 });
    }

    #[test]
    fn rejects_malformed_fields() {
        assert!(parse_field("").is_err(), "no DID");
        assert!(parse_field("=0x2600").is_err(), "empty name");
        assert!(parse_field("0x12345").is_err(), "DID wider than 16 bits");
        assert!(parse_field("0x2610:4:0").is_err(), "zero length");
        assert!(parse_field("0x2610:4:9").is_err(), "wider than u64");
        assert!(parse_field("0x2610:4.8").is_err(), "bit past the byte");
        assert!(parse_field("0x2610.3").is_err(), "bit without an offset");
        assert!(parse_field("0x2610:4:2.1").is_err(), "bit with a length");
        assert!(parse_field("0x2610:1:2:3").is_err(), "too many parts");
        assert!(parse_field("0x26ZZ").is_err(), "not a number");
    }

    #[test]
    fn extracts_values_from_a_response() {
        let data = [0xFF, 0xFD, 0xBD, 0x1C, 0x63];

        let value = |spec: &str| parse_field(spec).unwrap().value(&data);

        assert_eq!(value("0x2620"), Some("FFFDBD1C63".into()));
        assert_eq!(value("0x2620:2"), Some("189".into()));
        assert_eq!(
            value("0x2620:3:2"),
            Some("7267".into()),
            "0x1C63 big-endian"
        );
        assert_eq!(value("0x2620:1.0"), Some("1".into()), "0xFD bit 0");
        assert_eq!(value("0x2620:1.1"), Some("0".into()), "0xFD bit 1");
        assert_eq!(value("0x2620:0:8"), None, "runs past the end");
        assert_eq!(value("0x2620:5"), None, "starts past the end");
    }

    #[test]
    fn fields_file_skips_blanks_and_comments_and_names_the_bad_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fields.txt");

        std::fs::write(&path, "# engine\n0x2600\n\n  speed=0x2610:4:2\n").unwrap();
        let fields = load_fields_file(&path).unwrap();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[1].name, "speed");

        std::fs::write(&path, "0x2600\n0x2610:4.9\n").unwrap();
        let err = load_fields_file(&path).unwrap_err();
        assert!(err.contains(":2:"), "should name line 2: {err}");
    }

    #[test]
    fn csv_cells_are_quoted_only_when_needed() {
        assert_eq!(csv_escape("rpm"), "rpm");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    /// Two fields on the same DID must share one read per sample; the scripted
    /// link has exactly one response per DID per sample, so an extra read
    /// would run out of responses.
    #[test]
    fn reads_each_did_once_per_sample_and_writes_a_row_per_sample() {
        let (mut proto, link) = protocol_with(&[
            "62260012FF", // sample 1: 0x2600 -> 12 FF
            "62263034",   // sample 1: 0x2630 -> 34
            "62260013FE", // sample 2
            "62263035",
        ]);
        let fields = vec![
            parse_field("0x2600").unwrap(),
            parse_field("hi=0x2600:0").unwrap(),
            parse_field("0x2630:0.0").unwrap(),
        ];
        let mut out = Vec::new();

        let rows = run(&mut proto, &fields, &mut out, &once(2)).unwrap();

        assert_eq!(rows, 2);
        assert_eq!(link.sent(), vec!["222600", "222630", "222600", "222630"]);
        let csv = String::from_utf8(out).unwrap();
        assert!(csv.starts_with("unix_ms,elapsed_ms,"));
        assert_eq!(
            without_timestamps(&csv),
            vec!["0x2600,hi,0x2630:0.0", "12FF,18,0", "13FE,19,1"]
        );
    }

    /// A DID the ECU refuses, or a field past the end of a short response,
    /// empties its cells but does not stop the log.
    #[test]
    fn unreadable_dids_and_short_responses_leave_empty_cells() {
        let (mut proto, _link) = protocol_with(&[
            "7F2231",   // 0x2600: request out of range
            "62263034", // 0x2630 -> 34
        ]);
        let fields = vec![
            parse_field("0x2600").unwrap(),
            parse_field("0x2630:0").unwrap(),
            parse_field("0x2630:1").unwrap(),
        ];
        let mut out = Vec::new();

        run(&mut proto, &fields, &mut out, &once(1)).unwrap();

        let csv = String::from_utf8(out).unwrap();
        assert_eq!(without_timestamps(&csv)[1], ",52,");
    }
}
