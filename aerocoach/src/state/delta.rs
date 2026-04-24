//! Delta engine: computes [`DashboardUpdate`] payloads for WebSocket broadcasts.
//!
//! [`DeltaEngine::compute`] builds a complete JSON-serialisable snapshot every
//! ~3 seconds.  It derives instantaneous bandwidth from the transfers drained
//! from [`super::metrics_store::MetricsStore`] since the previous call, so the
//! caller is responsible for draining under the write lock and passing the
//! collected data in.

use chrono::Utc;
use serde::{Deserialize, Serialize};

use aeroproto::aeromonitor::TransferRecord;

use crate::state::metrics_store::AgentTotals;
use crate::state::registry::AgentStatus;

// ── JSON types (broadcast over WebSocket) ─────────────────────────────────

/// Top-level payload broadcast to aerotrack every ~3 s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardUpdate {
    pub timestamp_ms:        i64,
    pub current_slice:       u32,
    pub total_slices:        u32,
    pub agents:              Vec<AgentSnapshot>,
    /// Delta: only transfers completed since the last broadcast.
    pub completed_transfers: Vec<TransferDelta>,
    pub global_stats:        GlobalStats,
}

/// Per-agent status snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub agent_id:           String,
    pub agent_index:        u32,
    pub connected:          bool,
    pub current_slice:      u32,
    pub active_connections: u32,
    /// Cumulative bytes transferred by completed transfers for this agent.
    pub bytes_transferred:  u64,
    pub success_count:      u32,
    pub error_count:        u32,
    pub private_ip:         String,
    pub instance_id:        String,
    /// Current in-flight bytes across all active transfers.
    /// Reported by the agent on every heartbeat; reset to 0 when idle.
    pub bytes_in_flight:    u64,
}

/// One completed transfer record augmented with its source agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferDelta {
    pub agent_id:          String,
    pub filename:          String,
    pub bucket_id:         String,
    pub bytes_transferred: u64,
    pub file_size_bytes:   u64,
    pub bandwidth_kibps:   u32,
    pub success:           bool,
    pub error_reason:      Option<String>,
    pub start_time_ms:     i64,
    pub end_time_ms:       i64,
    pub time_slice:        u32,
}

/// Aggregate statistics across all agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStats {
    pub total_bytes_transferred: u64,
    pub total_success:           u32,
    pub total_errors:            u32,
    pub active_agents:           u32,
    pub active_connections:      u32,
    /// Fraction of transfers that resulted in an error (0.0 – 1.0).
    pub overall_error_rate:      f64,
    /// Instantaneous aggregate bandwidth in bytes/s (computed from delta bytes
    /// divided by the elapsed interval since the previous `compute` call).
    pub current_bandwidth_bps:   u64,
}

// ── DeltaEngine ────────────────────────────────────────────────────────────

/// Computes `DashboardUpdate` payloads for broadcast to WebSocket clients.
///
/// Tracks the timestamp of the previous call to derive instantaneous bandwidth.
pub struct DeltaEngine {
    last_compute_ms: i64,
    /// Previous `bytes_in_flight` per agent, used to compute the delta
    /// contribution of in-progress transfers between ticks.
    prev_inflight:   std::collections::HashMap<String, u64>,
}

impl DeltaEngine {
    pub fn new() -> Self {
        Self {
            last_compute_ms: Utc::now().timestamp_millis(),
            prev_inflight:   std::collections::HashMap::new(),
        }
    }

    /// Build a [`DashboardUpdate`] from a pre-collected snapshot of state.
    ///
    /// # Parameters
    /// - `current_slice` / `total_slices` — from `CoachState` and the plan.
    /// - `registry_snapshot` — from `Registry::status_snapshot()`.
    /// - `totals_fn` — closure returning cumulative `AgentTotals` for one agent.
    /// - `drained` — transfers drained from `MetricsStore` since the last call.
    pub fn compute(
        &mut self,
        current_slice:     u32,
        total_slices:      u32,
        registry_snapshot: &[AgentStatus],
        totals_fn:         impl Fn(&str) -> AgentTotals,
        drained:           &[(String, TransferRecord)],
    ) -> DashboardUpdate {
        let now_ms = Utc::now().timestamp_millis();
        // Guard against zero or negative elapsed time (clock jump / first call).
        let elapsed_secs = ((now_ms - self.last_compute_ms) as f64 / 1000.0).max(0.001);
        self.last_compute_ms = now_ms;

        // ── Per-agent snapshots ────────────────────────────────────────
        let agents: Vec<AgentSnapshot> = registry_snapshot
            .iter()
            .map(|s| {
                let t = totals_fn(&s.agent_id);
                AgentSnapshot {
                    agent_id:           s.agent_id.clone(),
                    agent_index:        s.agent_index,
                    connected:          s.connected,
                    current_slice:      s.current_slice,
                    active_connections: s.active_connections,
                    bytes_transferred:  t.bytes_transferred,
                    success_count:      t.success_count,
                    error_count:        t.error_count,
                    private_ip:         s.private_ip.clone(),
                    instance_id:        s.instance_id.clone(),
                    bytes_in_flight:    t.bytes_in_flight,
                }
            })
            .collect();

        // ── Aggregate stats ────────────────────────────────────────────
        let total_bytes:        u64 = agents.iter().map(|a| a.bytes_transferred).sum();
        let total_success:      u32 = agents.iter().map(|a| a.success_count).sum();
        let total_errors:       u32 = agents.iter().map(|a| a.error_count).sum();
        let active_agents:      u32 = agents.iter().filter(|a| a.connected).count() as u32;
        let active_connections: u32 = agents.iter().map(|a| a.active_connections).sum();

        let total_all = (total_success + total_errors) as f64;
        let overall_error_rate = if total_all > 0.0 {
            total_errors as f64 / total_all
        } else {
            0.0
        };

        // ── Instantaneous bandwidth ────────────────────────────────────────────
        //
        // For in-flight transfers: use the positive delta of bytes_in_flight
        // vs. the previous snapshot — this correctly reflects incremental
        // progress within each 3 s window.
        //
        // For completed transfers: bytes_transferred is the *cumulative total*
        // for the whole transfer, not just the last window.  If the agent was
        // sending heartbeats (prev_inflight > 0), those earlier bytes were
        // already counted in previous delta_inflight contributions.  Subtract
        // prev_inflight to keep only the tail that wasn’t yet reported,
        // preventing the final tick from spiking to the full-file rate.
        // Transfers with no prior inflight tracking (fast/unlimited) have
        // prev_inflight == 0, so their full bytes_transferred is used as-is.

        // Sum completed bytes per agent (there may be several concurrent).
        let mut completed_by_agent: std::collections::HashMap<&str, u64> =
            std::collections::HashMap::new();
        for (agent_id, r) in drained.iter().filter(|(_, r)| r.success) {
            *completed_by_agent.entry(agent_id.as_str()).or_insert(0) += r.bytes_transferred;
        }
        let delta_completed: u64 = completed_by_agent
            .iter()
            .map(|(agent_id, &total)| {
                let prev = self.prev_inflight.get(*agent_id).copied().unwrap_or(0);
                total.saturating_sub(prev)
            })
            .sum();

        let delta_inflight: u64 = agents
            .iter()
            .map(|a| {
                let prev = self.prev_inflight.get(&a.agent_id).copied().unwrap_or(0);
                a.bytes_in_flight.saturating_sub(prev)
            })
            .sum();

        // Update stored snapshot for next tick.
        self.prev_inflight = agents
            .iter()
            .map(|a| (a.agent_id.clone(), a.bytes_in_flight))
            .collect();

        let current_bandwidth_bps =
            ((delta_completed + delta_inflight) as f64 / elapsed_secs) as u64;

        tracing::debug!(
            elapsed_ms        = (elapsed_secs * 1000.0) as u64,
            delta_completed_b = delta_completed,
            delta_inflight_b  = delta_inflight,
            bandwidth_bps     = current_bandwidth_bps,
            bandwidth_mbit    = (current_bandwidth_bps * 8) / 1_000_000,
            "bandwidth tick"
        );

        // ── Delta transfer records ─────────────────────────────────────
        let completed_transfers: Vec<TransferDelta> = drained
            .iter()
            .map(|(agent_id, r)| TransferDelta {
                agent_id:          agent_id.clone(),
                filename:          r.filename.clone(),
                bucket_id:         r.bucket_id.clone(),
                bytes_transferred: r.bytes_transferred,
                file_size_bytes:   r.file_size_bytes,
                bandwidth_kibps:   r.bandwidth_kibps,
                success:           r.success,
                error_reason:      r.error_reason.clone(),
                start_time_ms:     r.start_time_ms,
                end_time_ms:       r.end_time_ms,
                time_slice:        r.time_slice,
            })
            .collect();

        DashboardUpdate {
            timestamp_ms: now_ms,
            current_slice,
            total_slices,
            agents,
            completed_transfers,
            global_stats: GlobalStats {
                total_bytes_transferred: total_bytes,
                total_success,
                total_errors,
                active_agents,
                active_connections,
                overall_error_rate,
                current_bandwidth_bps,
            },
        }
    }
}

impl Default for DeltaEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::metrics_store::AgentTotals;
    use crate::state::registry::AgentStatus;

    fn make_status(id: &str, idx: u32, connected: bool, conns: u32) -> AgentStatus {
        AgentStatus {
            agent_id:           id.into(),
            agent_index:        idx,
            private_ip:         "10.0.0.1".into(),
            instance_id:        "i-abc".into(),
            current_slice:      0,
            active_connections: conns,
            connected,
        }
    }

    fn make_record(agent_id: &str, success: bool, bytes: u64) -> (String, TransferRecord) {
        (
            agent_id.into(),
            TransferRecord {
                filename:          "a00_s001.dat".into(),
                bucket_id:         "xs".into(),
                bytes_transferred: bytes,
                file_size_bytes:   bytes,
                bandwidth_kibps:   1024,
                success,
                error_reason:      None,
                start_time_ms:     0,
                end_time_ms:       1000,
                time_slice:        0,
            },
        )
    }

    #[test]
    fn compute_returns_correct_slice_info() {
        let mut engine = DeltaEngine::new();
        let snapshot = [make_status("a00", 0, true, 5)];
        let totals = AgentTotals { success_count: 2, error_count: 0, bytes_transferred: 2048, bytes_in_flight: 0 };
        let update = engine.compute(2, 6, &snapshot, |_| totals.clone(), &[]);
        assert_eq!(update.current_slice, 2);
        assert_eq!(update.total_slices, 6);
    }

    #[test]
    fn global_stats_aggregated_correctly() {
        let mut engine = DeltaEngine::new();
        let snapshot = [
            make_status("a00", 0, true, 5),
            make_status("a01", 1, true, 3),
        ];
        let totals_a00 = AgentTotals { success_count: 10, error_count: 2, bytes_transferred: 8192, bytes_in_flight: 0 };
        let totals_a01 = AgentTotals { success_count: 5,  error_count: 1, bytes_transferred: 4096, bytes_in_flight: 0 };
        let update = engine.compute(1, 3, &snapshot, |id| {
            if id == "a00" { totals_a00.clone() } else { totals_a01.clone() }
        }, &[]);
        assert_eq!(update.global_stats.total_success,           15);
        assert_eq!(update.global_stats.total_errors,             3);
        assert_eq!(update.global_stats.total_bytes_transferred, 12288);
        assert_eq!(update.global_stats.active_agents,           2);
        assert_eq!(update.global_stats.active_connections,      8);
    }

    #[test]
    fn error_rate_computed_correctly() {
        let mut engine = DeltaEngine::new();
        let snapshot = [make_status("a00", 0, true, 0)];
        let totals = AgentTotals { success_count: 9, error_count: 1, bytes_transferred: 0, bytes_in_flight: 0 };
        let update = engine.compute(0, 1, &snapshot, |_| totals.clone(), &[]);
        assert!((update.global_stats.overall_error_rate - 0.1).abs() < 1e-9);
    }

    #[test]
    fn no_agents_produces_zero_stats() {
        let mut engine = DeltaEngine::new();
        let update = engine.compute(0, 1, &[], |_| AgentTotals::default(), &[]);
        assert_eq!(update.global_stats.total_success, 0);
        assert_eq!(update.global_stats.overall_error_rate, 0.0);
        assert!(update.agents.is_empty());
    }

    #[test]
    fn drained_transfers_appear_in_completed() {
        let mut engine = DeltaEngine::new();
        let drained = [
            make_record("a00", true,  1024),
            make_record("a00", false, 0),
        ];
        let update = engine.compute(0, 1, &[], |_| AgentTotals::default(), &drained);
        assert_eq!(update.completed_transfers.len(), 2);
        assert_eq!(update.completed_transfers[0].agent_id, "a00");
        assert!(update.completed_transfers[0].success);
        assert!(!update.completed_transfers[1].success);
    }

    #[test]
    fn inactive_agents_excluded_from_active_count() {
        let mut engine = DeltaEngine::new();
        let snapshot = [
            make_status("a00", 0, true,  4),
            make_status("a01", 1, false, 0), // disconnected
        ];
        let totals = AgentTotals::default();
        let update = engine.compute(0, 1, &snapshot, |_| totals.clone(), &[]);
        assert_eq!(update.global_stats.active_agents, 1);
    }
}
