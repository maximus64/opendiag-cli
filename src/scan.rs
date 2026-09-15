use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::Duration;

use crate::errors::DiagError;

#[derive(Debug, serde::Serialize)]
pub struct ScanResult {
    pub ecu: Option<crate::obd::addressing::EcuAddress>,
    pub response: u32,
}
pub fn ecus(
    protocol: &mut crate::ObdProtocol,
    cancel: &crate::Cancel,
) -> Result<Vec<ScanResult>, DiagError> {
    let mut responses = std::collections::BTreeSet::new();
    let mut requests = vec![vec![0x01, 0x00], vec![0x09, 0x00]];
    if protocol.bus().is_can() {
        requests.push(vec![0x3E, 0x00]);
    }
    for request in requests {
        cancel.check()?;
        for reply in protocol.broadcast(&request, None)? {
            responses.insert(reply.id);
        }
    }
    Ok(responses
        .into_iter()
        .map(|response| ScanResult {
            ecu: protocol.ecu_for_response(response),
            response,
        })
        .collect())
}
#[derive(Debug, serde::Serialize)]
pub struct DidScan {
    pub values: BTreeMap<u16, Vec<u8>>,
    pub stats: ScanStats,
    pub elapsed: Duration,
    pub cancelled: bool,
}
pub fn dids(
    protocol: &mut crate::ObdProtocol,
    diag: &str,
    start: u16,
    end: u16,
    timing: bool,
    cancel: &crate::Cancel,
) -> Result<DidScan, DiagError> {
    if start > end {
        return Err(DiagError::InvalidCommand("DID start exceeds end".into()));
    }
    let wall = std::time::Instant::now();
    let mut stats = ScanStats::new(timing);
    let mut values = BTreeMap::new();
    for did in start..=end {
        if cancel.is_cancelled() {
            break;
        }
        let started = std::time::Instant::now();
        let result =
            crate::diag::services(diag, protocol)?.read_data(crate::diag::DataId::Common(did));
        stats.record(did, &result, started.elapsed());
        match result {
            Ok(data) => {
                values.insert(did, data);
            }
            Err(
                DiagError::UdsNegativeResponse { .. }
                | DiagError::Timeout(_)
                | DiagError::InvalidResponse(_),
            ) => {}
            Err(error) => return Err(error),
        }
        protocol.emit(crate::events::Event::Progress {
            done: did as u64 - start as u64 + 1,
            total: end as u64 - start as u64 + 1,
            unit: crate::events::ProgressUnit::Items,
        });
    }
    Ok(DidScan {
        values,
        stats,
        elapsed: wall.elapsed(),
        cancelled: cancel.is_cancelled(),
    })
}

#[derive(Debug, serde::Serialize)]
pub struct ScanStats {
    positive: u32,
    negative: BTreeMap<u8, u32>,
    timeouts: u32,
    errors: u32,
    examples: Vec<(u16, String)>,
    latencies: Option<Vec<Duration>>,
}

impl ScanStats {
    pub fn new(timing: bool) -> Self {
        Self {
            positive: 0,
            negative: BTreeMap::new(),
            timeouts: 0,
            errors: 0,
            examples: Vec::new(),
            latencies: timing.then(Vec::new),
        }
    }

    pub fn record<T>(&mut self, did: u16, result: &Result<T, DiagError>, elapsed: Duration) {
        if let Some(latencies) = &mut self.latencies {
            latencies.push(elapsed);
        }
        match result {
            Ok(_) => self.positive += 1,
            Err(DiagError::UdsNegativeResponse { code, .. }) => {
                *self.negative.entry(*code).or_default() += 1;
            }
            Err(error) => {
                if matches!(error, DiagError::Timeout(_)) {
                    self.timeouts += 1;
                } else {
                    self.errors += 1;
                }
                if self.examples.len() < 5 {
                    self.examples.push((did, error.to_string()));
                }
            }
        }
    }

    pub fn write_summary(&mut self, out: &mut impl Write, wall: Duration) -> io::Result<()> {
        let negative: u32 = self.negative.values().sum();
        let attempted = self.positive + negative + self.timeouts + self.errors;
        writeln!(
            out,
            "Scan: attempted={} positive={} negative={} timeouts={} errors={} elapsed={:.3}s",
            attempted,
            self.positive,
            negative,
            self.timeouts,
            self.errors,
            wall.as_secs_f64()
        )?;
        for (code, count) in &self.negative {
            writeln!(out, "  NRC 0x{code:02X}: {count}")?;
        }
        for (did, error) in &self.examples {
            writeln!(out, "  ID 0x{did:04X}: {error}")?;
        }
        if let Some(latencies) = &mut self.latencies {
            if !latencies.is_empty() {
                latencies.sort_unstable();
                let total: Duration = latencies.iter().sum();
                // Nearest-rank percentiles; all outcomes contribute, including timeouts.
                let percentile = |percent: usize| {
                    latencies[(latencies.len() * percent).div_ceil(100) - 1].as_secs_f64() * 1000.0
                };
                writeln!(
                    out,
                    "Exchange latency: mean={:.3}ms p50={:.3}ms p95={:.3}ms max={:.3}ms",
                    total.as_secs_f64() * 1000.0 / latencies.len() as f64,
                    percentile(50),
                    percentile(95),
                    percentile(100)
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_replies_are_separate_from_failures_and_timeouts_count_in_latency() {
        let mut stats = ScanStats::new(true);
        stats.record(0, &Ok::<_, DiagError>(vec![1]), Duration::from_millis(2));
        let rejected: Result<Vec<u8>, _> = Err(DiagError::UdsNegativeResponse {
            code: 0x31,
            description: "request out of range",
        });
        stats.record(1, &rejected, Duration::from_millis(4));
        stats.record::<Vec<u8>>(
            2,
            &Err(DiagError::Timeout("no reply".into())),
            Duration::from_millis(3000),
        );
        stats.record::<Vec<u8>>(
            3,
            &Err(DiagError::InvalidResponse("wrong DID".into())),
            Duration::from_millis(6),
        );
        let mut output = Vec::new();
        stats
            .write_summary(&mut output, Duration::from_millis(3015))
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("attempted=4 positive=1 negative=1 timeouts=1 errors=1"));
        assert!(output.contains("NRC 0x31: 1"));
        assert!(output.contains("ID 0x0002: no reply"));
        assert!(output.contains("mean=753.000ms p50=4.000ms p95=3000.000ms max=3000.000ms"));
    }
}
