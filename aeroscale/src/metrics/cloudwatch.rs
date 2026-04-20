//! Push key FTP metrics to AWS CloudWatch via the PutMetricData Query API.
//!
//! Uses our existing `aws_query` / SigV4 infrastructure — no CloudWatch agent
//! or EMF stdout capture needed.
//!
//! Only the business-critical FTP metrics are pushed (not process metrics):
//!   - `ActiveSessions`     ← from `ftp_sessions_total`  (gauge)
//!   - `CumulativeSessions` ← from `ftp_sessions_count`  (counter)
//!
//! Dimensions use **slot number** (not instance ID) to keep the CloudWatch
//! metric registry from growing unboundedly as instances are replaced.
//!
//! Up to 1000 data points per API call; we have at most 20 slots × 2 metrics
//! = 40 data points, well within the limit.

use anyhow::Result;
use tracing::{debug, info};

use aerocore::{aws_query, AwsCredentials};

use super::BackendMetrics;

/// Push active and cumulative session counts for all backends to CloudWatch.
pub async fn push(
    region:    &str,
    creds:     &AwsCredentials,
    namespace: &str,
    backends:  &[BackendMetrics],
) -> Result<()> {
    // Only push backends that scraped successfully and have sessions data.
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

    let mut n = 1usize; // MetricData member index

    for b in &active_backends {
        let slot_str = b.slot.to_string();

        // ── Active sessions (ftp_sessions_total, gauge) ───────────────────────
        let active_sessions = b.samples.iter()
            .find(|s| s.metric == "ftp_sessions_total" && s.labels.is_empty())
            .map(|s| s.value)
            .unwrap_or(0.0);

        add_data_point(&mut owned, n, "ActiveSessions",     active_sessions, "Count", &slot_str, &timestamp);
        n += 1;

        // ── Cumulative sessions (ftp_sessions_count, counter) ─────────────────
        let cumulative = b.samples.iter()
            .find(|s| s.metric == "ftp_sessions_count" && s.labels.is_empty())
            .map(|s| s.value)
            .unwrap_or(0.0);

        add_data_point(&mut owned, n, "CumulativeSessions", cumulative,       "Count", &slot_str, &timestamp);
        n += 1;
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

fn add_data_point(
    params:    &mut Vec<(String, String)>,
    n:         usize,
    name:      &str,
    value:     f64,
    unit:      &str,
    slot:      &str,
    timestamp: &str,
) {
    let p = format!("MetricData.member.{n}");
    params.push((format!("{p}.MetricName"),                         name.into()));
    params.push((format!("{p}.Value"),                              format!("{value:.4}")));
    params.push((format!("{p}.Unit"),                               unit.into()));
    params.push((format!("{p}.Timestamp"),                          timestamp.into()));
    params.push((format!("{p}.Dimensions.member.1.Name"),           "Slot".into()));
    params.push((format!("{p}.Dimensions.member.1.Value"),          slot.into()));
}
