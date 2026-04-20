//! Prometheus text exposition — re-emit scraped metrics with a `slot` label.
//!
//! All metrics from every successfully scraped backend are re-exposed with
//! the backend's slot number prepended as the first label.  This keeps the
//! label-set stable across instance rotation (slot numbers never change,
//! instance IDs do).
//!
//! Example output:
//!
//! ```text
//! # HELP ftp_sessions_total Total number of active FTP sessions.
//! # TYPE ftp_sessions_total gauge
//! ftp_sessions_total{slot="0"} 5.0
//! ftp_sessions_total{slot="1"} 3.0
//! # TYPE ftp_command_total counter
//! ftp_command_total{slot="0",command="quit"} 3.0
//! ftp_command_total{slot="1",command="list"} 18.0
//! ```

use std::collections::BTreeMap;
use std::fmt::Write;

use super::{BackendMetrics, scrape::SampleKind};

/// Format all metrics from `backends` as Prometheus text exposition.
pub fn format(backends: &[BackendMetrics]) -> String {
    // Collect metadata: type and help text, keyed by metric name.
    // BTreeMap gives stable alphabetical ordering in the output.
    let mut meta: BTreeMap<&str, (SampleKind, Option<&str>)> = BTreeMap::new();
    for b in backends {
        for s in &b.samples {
            meta.entry(s.metric.as_str())
                .or_insert_with(|| {
                    let help = b.docs.get(&s.metric).map(String::as_str);
                    (s.kind, help)
                });
        }
    }

    let mut out = String::new();

    for (metric_name, (kind, help)) in &meta {
        // HELP line (only when the scraped backend provided one)
        if let Some(h) = help {
            writeln!(out, "# HELP {metric_name} {h}").ok();
        }
        writeln!(out, "# TYPE {metric_name} {}", kind.as_str()).ok();

        for b in backends {
            if b.error.is_some() {
                continue;
            }
            for s in &b.samples {
                if s.metric.as_str() != *metric_name {
                    continue;
                }

                // Build the label string: slot="N" first, then original labels.
                let mut label_str = format!("slot=\"{}\"", b.slot);
                for (k, v) in &s.labels {
                    write!(label_str, ",{k}=\"{v}\"").ok();
                }

                writeln!(out, "{metric_name}{{{label_str}}} {}", s.value).ok();
            }
        }
    }

    // Append a scrape-error metric so consumers can detect failed backends.
    let error_count = backends.iter().filter(|b| b.error.is_some()).count();
    if error_count > 0 {
        writeln!(out, "# HELP aeroftp_scrape_error 1 if the last scrape of this slot failed.").ok();
        writeln!(out, "# TYPE aeroftp_scrape_error gauge").ok();
        for b in backends {
            writeln!(
                out,
                "aeroftp_scrape_error{{slot=\"{}\"}} {}",
                b.slot,
                if b.error.is_some() { 1 } else { 0 }
            )
            .ok();
        }
    }

    out
}
