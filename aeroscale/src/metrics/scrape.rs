//! HTTP scrape of a single backend's Prometheus `/metrics` endpoint.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, Result};
use prometheus_parse::Value;

// ── Types ─────────────────────────────────────────────────────────────────────

/// The kind of a Prometheus metric — used to emit correct `# TYPE` headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    Counter,
    Gauge,
    Untyped,
}

impl SampleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge   => "gauge",
            Self::Untyped => "untyped",
        }
    }
}

/// One scalar data point from a scraped backend.
#[derive(Debug, Clone)]
pub struct RawSample {
    pub metric: String,
    pub value:  f64,
    pub kind:   SampleKind,
    /// Original labels (does NOT include `slot` — that is added at exposition time).
    pub labels: Vec<(String, String)>,
}

// ── Scrape ────────────────────────────────────────────────────────────────────

/// Scrape `http://<ip>:<port>/metrics` and return all scalar samples.
///
/// Histograms and summaries are skipped (not needed for our use case).
/// Errors are propagated so the caller can record the failure per backend.
pub async fn scrape_one(ip: Ipv4Addr, port: u16) -> Result<(Vec<RawSample>, HashMap<String, String>)> {
    let url = format!("http://{ip}:{port}/metrics");

    let body = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?
        .error_for_status()
        .with_context(|| format!("GET {url} returned error status"))?
        .text()
        .await
        .context("Failed to read response body")?;

    parse_text(&body)
}

fn parse_text(body: &str) -> Result<(Vec<RawSample>, HashMap<String, String>)> {
    let scrape = prometheus_parse::Scrape::parse(
        body.lines().map(|s| Ok::<_, std::io::Error>(s.to_owned())),
    )
    .context("Failed to parse Prometheus text")?;

    let docs = scrape.docs.into_iter().collect::<HashMap<_, _>>();

    let mut samples = Vec::new();
    for s in scrape.samples {
        let (value, kind) = match s.value {
            Value::Counter(v)    => (v, SampleKind::Counter),
            Value::Gauge(v)      => (v, SampleKind::Gauge),
            Value::Untyped(v)    => (v, SampleKind::Untyped),
            Value::Histogram(_)
            | Value::Summary(_)  => continue,
        };

        let labels = s.labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        samples.push(RawSample { metric: s.metric, value, kind, labels });
    }

    Ok((samples, docs))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_METRICS: &str = "\
# HELP ftp_sessions_total Total number of active FTP sessions.
# TYPE ftp_sessions_total gauge
ftp_sessions_total 5.0
# HELP ftp_sessions_count Total FTP sessions served.
# TYPE ftp_sessions_count counter
ftp_sessions_count 42.0
# HELP ftp_command_total Total number of commands received.
# TYPE ftp_command_total counter
ftp_command_total{command=\"quit\"} 3.0
ftp_command_total{command=\"list\"} 18.0
";

    #[test]
    fn parses_gauge_and_counters() {
        let (samples, docs) = parse_text(SAMPLE_METRICS).unwrap();
        assert_eq!(samples.len(), 4);

        let active = samples.iter().find(|s| s.metric == "ftp_sessions_total").unwrap();
        assert_eq!(active.kind, SampleKind::Gauge);
        assert_eq!(active.value, 5.0);

        let commands: Vec<_> = samples.iter().filter(|s| s.metric == "ftp_command_total").collect();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].kind, SampleKind::Counter);

        assert!(docs.contains_key("ftp_sessions_total"));
    }
}
