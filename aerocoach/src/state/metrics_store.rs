//! Accumulated transfer metrics from all agents.
//!
//! Stores per-agent [`TransferRecord`]s received via `MetricsUpdate` messages.
//! The delta engine drains completed transfers each broadcast cycle to build
//! the [`DashboardUpdate`] payload for aerotrack.

use aeroproto::aeromonitor::{MetricsUpdate, TransferRecord};
use tracing::debug;

/// Accumulated transfer metrics for the current test run.
#[derive(Debug, Default)]
pub struct MetricsStore {
    /// All completed transfers received from agents since the test started.
    /// The delta engine drains this periodically.
    completed: Vec<(String, TransferRecord)>, // (agent_id, record)

    /// Per-agent running totals (success count, error count, bytes).
    totals: std::collections::HashMap<String, AgentTotals>,
}

/// Running totals for one agent.
#[derive(Debug, Default, Clone)]
pub struct AgentTotals {
    pub success_count: u32,
    pub error_count: u32,
    pub bytes_transferred: u64,
}

impl MetricsStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a [`MetricsUpdate`] received from an agent.
    ///
    /// Appends completed transfers and updates running totals.
    pub fn record_update(&mut self, agent_id: String, update: &MetricsUpdate) {
        let totals = self.totals.entry(agent_id.clone()).or_default();

        for record in &update.completed_transfers {
            if record.success {
                totals.success_count += 1;
                totals.bytes_transferred += record.bytes_transferred;
            } else {
                totals.error_count += 1;
            }
            debug!(
                agent_id  = %agent_id,
                filename  = %record.filename,
                bytes     = record.bytes_transferred,
                success   = record.success,
                "transfer recorded"
            );
            self.completed.push((agent_id.clone(), record.clone()));
        }
    }

    /// Drain all completed transfers since the last call.
    ///
    /// Used by the delta engine to build `DashboardUpdate.completed_transfers`.
    pub fn drain_completed(&mut self) -> Vec<(String, TransferRecord)> {
        std::mem::take(&mut self.completed)
    }

    /// Running totals for one agent, or zeroed defaults if unknown.
    pub fn agent_totals(&self, agent_id: &str) -> AgentTotals {
        self.totals.get(agent_id).cloned().unwrap_or_default()
    }

    /// Total transfers recorded across all agents.
    pub fn total_count(&self) -> u64 {
        self.totals
            .values()
            .map(|t| (t.success_count + t.error_count) as u64)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(filename: &str, success: bool, bytes: u64) -> TransferRecord {
        TransferRecord {
            filename: filename.into(),
            bucket_id: "xs".into(),
            bytes_transferred: bytes,
            file_size_bytes: bytes,
            bandwidth_kibps: 1024,
            success,
            error_reason: if success { None } else { Some("550 error".into()) },
            start_time_ms: 0,
            end_time_ms: 1000,
            time_slice: 0,
        }
    }

    #[test]
    fn record_update_accumulates_totals() {
        let mut store = MetricsStore::new();
        let update = MetricsUpdate {
            current_slice: 1,
            active_connections: 5,
            queued_connections: 0,
            completed_transfers: vec![
                make_record("a00_s001_c001.dat", true, 1024),
                make_record("a00_s001_c002.dat", false, 0),
            ],
        };
        store.record_update("a00".into(), &update);

        let totals = store.agent_totals("a00");
        assert_eq!(totals.success_count, 1);
        assert_eq!(totals.error_count, 1);
        assert_eq!(totals.bytes_transferred, 1024);
        assert_eq!(store.total_count(), 2);
    }

    #[test]
    fn drain_completed_clears_buffer() {
        let mut store = MetricsStore::new();
        let update = MetricsUpdate {
            current_slice: 0,
            active_connections: 1,
            queued_connections: 0,
            completed_transfers: vec![make_record("f.dat", true, 512)],
        };
        store.record_update("a01".into(), &update);

        let drained = store.drain_completed();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, "a01");

        // Second drain returns empty
        assert!(store.drain_completed().is_empty());
    }
}
