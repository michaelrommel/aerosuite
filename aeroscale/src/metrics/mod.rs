//! Metrics collection, aggregation, and distribution for all active backends.
//!
//! ## Flow (runs after every snapshot refresh)
//!
//! 1. `scrape_all()` — HTTP-scrapes each backend that has a live lease
//! 2. Results stored in `MetricsStore` (Arc<RwLock>), shared with the HTTP server
//! 3. IPVS cross-check — compares `ftp_sessions_total` with IPVS active
//!    connections and logs discrepancies
//! 4. `cloudwatch::push()` — sends key metrics to CloudWatch (slot-labelled)
//!
//! The `/metrics` HTTP endpoint (served by `server::serve`) reads from the
//! same `MetricsStore` on every request, so it always reflects the last scrape.

pub mod cloudwatch;
pub mod exposition;
pub mod scrape;
pub mod server;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::snapshot::SystemSnapshot;

// ── Types ─────────────────────────────────────────────────────────────────────

/// All scraped data for one backend at one point in time.
#[derive(Debug, Clone)]
pub struct BackendMetrics {
    pub slot:    u32,
    pub ip:      Ipv4Addr,
    /// Parsed metric samples (excluding histograms and summaries).
    pub samples: Vec<scrape::RawSample>,
    /// `# HELP` text from the scraped endpoint, keyed by metric name.
    pub docs:    HashMap<String, String>,
    /// `None` = scrape succeeded; `Some(msg)` = scrape failed.
    pub error:   Option<String>,
}

/// Shared state between the scrape task and the HTTP server task.
#[derive(Debug, Default, Clone)]
pub struct MetricsState {
    pub backends: Vec<BackendMetrics>,
}

pub type MetricsStore = Arc<RwLock<MetricsState>>;

/// Create a new, empty metrics store.
pub fn new_store() -> MetricsStore {
    Arc::new(RwLock::new(MetricsState::default()))
}

// ── Scrape ────────────────────────────────────────────────────────────────────

/// Scrape all backends that have a live (non-expired) lease, update the store,
/// cross-check against IPVS, and push to CloudWatch.
pub async fn scrape_and_push(
    snapshot:    &SystemSnapshot,
    store:       &MetricsStore,
    scrape_port: u16,
    region:      &str,
    creds:       &aerocore::AwsCredentials,
    namespace:   &str,
    is_master:   bool,
) {
    // ── Scrape ─────────────────────────────────────────────────────────────────
    let mut results: Vec<BackendMetrics> = Vec::new();

    for b in &snapshot.backends {
        // Only scrape backends with a live, non-expired lease.
        let live_lease = match &b.lease {
            Some(l) if !l.is_expired() => l,
            _                          => continue,
        };
        let slot = match b.slot {
            Some(s) => s,
            None    => continue,
        };

        debug!(slot, ip = %b.ip, "scraping backend");

        let backend_metrics = match scrape::scrape_one(b.ip, scrape_port).await {
            Ok((samples, docs)) => {
                info!(slot, ip = %b.ip, samples = samples.len(), "scraped OK");
                BackendMetrics { slot, ip: b.ip, samples, docs, error: None }
            }
            Err(e) => {
                warn!(slot, ip = %b.ip, "scrape failed: {e:#}");
                BackendMetrics {
                    slot,
                    ip:      b.ip,
                    samples: Vec::new(),
                    docs:    HashMap::new(),
                    error:   Some(e.to_string()),
                }
            }
        };

        // ── IPVS cross-check ──────────────────────────────────────────────────
        // Compare ftp_sessions_total (from the backend itself) with the
        // active-connection count reported by IPVS (from the load balancer).
        // Discrepancies can indicate routing issues or stale IPVS entries.
        if backend_metrics.error.is_none() {
            let ftp_sessions = backend_metrics.samples.iter()
                .find(|s| s.metric == "ftp_sessions_total" && s.labels.is_empty())
                .map(|s| s.value as u32)
                .unwrap_or(0);

            let ipvs_active = b.ipvs.as_ref()
                .map(|i| i.active_connections)
                .unwrap_or(0);

            // Only warn when both sides report non-zero and differ meaningfully.
            if ftp_sessions != ipvs_active && (ftp_sessions > 0 || ipvs_active > 0) {
                warn!(
                    slot,
                    ip       = %b.ip,
                    instance = live_lease.owner_instance_id.as_str(),
                    ftp_sessions,
                    ipvs_active,
                    "ftp_sessions_total vs IPVS active-connections mismatch"
                );
            } else {
                debug!(slot, ftp_sessions, ipvs_active, "IPVS cross-check OK");
            }
        }

        results.push(backend_metrics);
    }

    // Sort by slot for stable output.
    results.sort_by_key(|m| m.slot);

    // ── Update the shared store (the HTTP server reads from here) ─────────────
    {
        let mut state = store.write().await;
        state.backends = results.clone();
    }

    // ── CloudWatch push (master only) ──────────────────────────────────────────
    if is_master {
        if let Err(e) = cloudwatch::push(region, creds, namespace, &results).await {
            warn!("CloudWatch push failed: {e:#}");
        }
    } else {
        debug!("backup mode — skipping CloudWatch push");
    }
}
