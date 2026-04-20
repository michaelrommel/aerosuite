//! Push key FTP metrics to AWS CloudWatch via the PutMetricData Query API.
//!
//! Uses our existing `aws_query` / SigV4 infrastructure — no CloudWatch agent
//! or EMF stdout capture needed.
//!
//! ## Dimensions
//!
//! Most metrics use a single `Slot` dimension so the time series remain stable
//! across instance rotation.
//!
//! `StorTransfers` uses **two** dimensions — `{Slot, Status}` — to preserve the
//! per-status breakdown.  This lets CloudWatch metric math express the failure
//! rate as `failure / (success + failure)` without any server-side aggregation.
//! One data point per status value found in the scrape is pushed; if no
//! transfer data is present yet (freshly started backend) nothing is pushed for
//! this metric and CloudWatch handles the gap gracefully.
//!
//! ## Metric table
//!
//! | CloudWatch name        | Dimensions      | Source Prometheus metric                                 | Unit  |
//! |------------------------|-----------------|----------------------------------------------------------|-------|
//! | `ActiveSessions`       | Slot            | `ftp_sessions_total`                        (gauge)      | Count |
//! | `CumulativeSessions`   | Slot            | `ftp_sessions_count`                        (counter)    | Count |
//! | `BackendWriteBytes`    | Slot            | `ftp_backend_write_bytes`                   (counter)    | Bytes |
//! | `BackendWriteFiles`    | Slot            | `ftp_backend_write_files`                   (counter)    | Count |
//! | `ReceivedBytes`        | Slot            | `ftp_received_bytes{command="stor"}`        (counter)    | Bytes |
//! | `StorTransfers`        | Slot + Status   | `ftp_transferred_total{command="stor"}` per status value | Count |
//! | `PassiveModeCommands`  | Slot            | sum of `ftp_command_total` for epsv + pasv  (counter)    | Count |
//! | `StorCommands`         | Slot            | `ftp_command_total{command="stor"}`         (counter)    | Count |
//! | `ResidentMemoryBytes`  | Slot            | `process_resident_memory_bytes`             (gauge)      | Bytes |
//! | `OpenFileDescriptors`  | Slot            | `process_open_fds`                          (gauge)      | Count |
//! | `MaxFileDescriptors`   | Slot            | `process_max_fds`                           (gauge)      | Count |
//! | `Threads`              | Slot            | `process_threads`                           (gauge)      | Count |
//!
//! Worst-case data points: 20 slots × (11 single-dim + ~2 status values) ≈ 260,
//! well within CloudWatch's 1000-point-per-call limit.

use anyhow::Result;
use tracing::{debug, info};

use aerocore::{aws_query, AwsCredentials};

use super::BackendMetrics;

/// Push session, transfer, and process-health metrics for all backends to CloudWatch.
pub async fn push(
    region:    &str,
    creds:     &AwsCredentials,
    namespace: &str,
    backends:  &[BackendMetrics],
) -> Result<()> {
    // Only push backends that scraped successfully.
    let active_backends: Vec<&BackendMetrics> =
        backends.iter().filter(|b| b.error.is_none()).collect();

    if active_backends.is_empty() {
        debug!("CloudWatch push: no successfully scraped backends — skipping");
        return Ok(());
    }

    let host      = format!("monitoring.{region}.amazonaws.com");
    let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Build params as owned strings first, then borrow as &str slices.
    let mut owned: Vec<(String, String)> = vec![
        ("Action".into(),    "PutMetricData".into()),
        ("Version".into(),   "2010-08-01".into()),
        ("Namespace".into(),  namespace.into()),
    ];

    let mut n = 1usize; // MetricData member index (1-based, per AWS API)

    for b in &active_backends {
        let slot_str = b.slot.to_string();
        let slot_dim = [("Slot", slot_str.as_str())];

        // Find a scalar sample with no labels.  Returns 0.0 when absent so
        // a freshly started backend (no transfers yet) never causes an error.
        let scalar = |metric: &str| -> f64 {
            b.samples.iter()
                .find(|s| s.metric == metric && s.labels.is_empty())
                .map(|s| s.value)
                .unwrap_or(0.0)
        };

        // Find the first sample matching ALL given label key=value pairs.
        // Returns 0.0 when absent.
        let labelled = |metric: &str, want: &[(&str, &str)]| -> f64 {
            b.samples.iter()
                .find(|s| {
                    s.metric == metric
                        && want.iter().all(|(k, v)| {
                            s.labels.iter().any(|(lk, lv)| lk == k && lv == *v)
                        })
                })
                .map(|s| s.value)
                .unwrap_or(0.0)
        };

        // -- FTP session metrics -----------------------------------------------
        add_point(&mut owned, n, "ActiveSessions",     scalar("ftp_sessions_total"), "Count", &slot_dim, &timestamp); n += 1;
        add_point(&mut owned, n, "CumulativeSessions", scalar("ftp_sessions_count"), "Count", &slot_dim, &timestamp); n += 1;

        // -- FTP transfer / throughput metrics --------------------------------
        add_point(&mut owned, n, "BackendWriteBytes", scalar("ftp_backend_write_bytes"),                       "Bytes", &slot_dim, &timestamp); n += 1;
        add_point(&mut owned, n, "BackendWriteFiles", scalar("ftp_backend_write_files"),                       "Count", &slot_dim, &timestamp); n += 1;
        add_point(&mut owned, n, "ReceivedBytes",     labelled("ftp_received_bytes", &[("command", "stor")]), "Bytes", &slot_dim, &timestamp); n += 1;

        // StorTransfers: one data point per status value actually present in
        // the scrape, using {Slot, Status} dimensions.  This preserves the
        // success/failure breakdown so CloudWatch metric math can compute
        // failure rates.  Nothing is pushed when no transfer data exists yet.
        for s in b.samples.iter().filter(|s| {
            s.metric == "ftp_transferred_total"
                && s.labels.iter().any(|(k, v)| k == "command" && v == "stor")
        }) {
            let status = s.labels.iter()
                .find(|(k, _)| k == "status")
                .map(|(_, v)| v.as_str())
                .unwrap_or("unknown");
            let dims = [("Slot", slot_str.as_str()), ("Status", status)];
            add_point(&mut owned, n, "StorTransfers", s.value, "Count", &dims, &timestamp);
            n += 1;
        }

        // PassiveModeCommands: epsv + pasv combined.  Compared against
        // StorCommands this gives the passive/active transfer ratio.
        let passive = labelled("ftp_command_total", &[("command", "epsv")])
                    + labelled("ftp_command_total", &[("command", "pasv")]);
        let stor_cmds = labelled("ftp_command_total", &[("command", "stor")]);
        add_point(&mut owned, n, "PassiveModeCommands", passive,    "Count", &slot_dim, &timestamp); n += 1;
        add_point(&mut owned, n, "StorCommands",        stor_cmds,  "Count", &slot_dim, &timestamp); n += 1;

        // -- Process health metrics --------------------------------------------
        add_point(&mut owned, n, "ResidentMemoryBytes", scalar("process_resident_memory_bytes"), "Bytes", &slot_dim, &timestamp); n += 1;
        add_point(&mut owned, n, "OpenFileDescriptors", scalar("process_open_fds"),              "Count", &slot_dim, &timestamp); n += 1;
        add_point(&mut owned, n, "MaxFileDescriptors",  scalar("process_max_fds"),               "Count", &slot_dim, &timestamp); n += 1;
        add_point(&mut owned, n, "Threads",             scalar("process_threads"),               "Count", &slot_dim, &timestamp); n += 1;
    }

    let params: Vec<(&str, &str)> =
        owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let xml = aws_query(&host, "monitoring", region, creds, &params).await?;

    // CloudWatch returns <PutMetricDataResponse> on success.
    if xml.contains("PutMetricDataResponse") || xml.contains("<RequestId>") {
        info!(
            namespace,
            data_points = n - 1,
            "CloudWatch PutMetricData OK"
        );
        Ok(())
    } else {
        anyhow::bail!("Unexpected CloudWatch PutMetricData response:\n{xml}");
    }
}

// ── Helper ────────────────────────────────────────────────────────────────────

/// Append one `MetricData.member.N` block to `params`.
///
/// `dimensions` accepts any number of `(name, value)` pairs; CloudWatch
/// supports up to 30 dimensions per metric.  Pass `&[("Slot", slot)]` for the
/// common single-dimension case and `&[("Slot", slot), ("Status", status)]`
/// for the two-dimension `StorTransfers` time series.
fn add_point(
    params:     &mut Vec<(String, String)>,
    n:          usize,
    name:       &str,
    value:      f64,
    unit:       &str,
    dimensions: &[(&str, &str)],
    timestamp:  &str,
) {
    let p = format!("MetricData.member.{n}");
    params.push((format!("{p}.MetricName"), name.into()));
    params.push((format!("{p}.Value"),      format!("{value:.4}")));
    params.push((format!("{p}.Unit"),       unit.into()));
    params.push((format!("{p}.Timestamp"),  timestamp.into()));
    for (i, (dim_name, dim_value)) in dimensions.iter().enumerate() {
        let d = i + 1;
        params.push((format!("{p}.Dimensions.member.{d}.Name"),  (*dim_name).into()));
        params.push((format!("{p}.Dimensions.member.{d}.Value"), (*dim_value).into()));
    }
}
