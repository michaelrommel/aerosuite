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
    /// `true` once the agent has sent a `PlanAck` after the last Confirm.
    /// Lets the UI show which agents have the confirmed plan before Start.
    pub plan_acked:         bool,
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
    /// Fraction of transfers that resulted in an error (0.0 - 1.0).
    pub overall_error_rate:      f64,
    /// Instantaneous aggregate bandwidth in bytes/s (computed from delta bytes
    /// divided by the elapsed interval since the previous `compute` call).
    pub current_bandwidth_bps:   u64,
}

// ── DeltaEngine ────────────────────────────────────────────────────────────

/// Smoothing factor for the exponential moving average applied to the raw
/// per-tick bandwidth sample.  α = 0.4 gives a half-life of roughly 2 ticks
/// (~6 s): fast enough to track a ramp-up within one slice, slow enough to
/// damp single-tick jitter caused by heartbeat/ticker phase drift.
const EMA_ALPHA: f64 = 0.6;

/// Computes `DashboardUpdate` payloads for broadcast to WebSocket clients.
pub struct DeltaEngine {
    last_compute_ms:  i64,
    /// Sum of `bytes_transferred` (cumulative completed) + `bytes_in_flight`
    /// (currently in-pipe) across all agents at the previous tick.
    ///
    /// This quantity is monotonically non-decreasing and continuous: when a
    /// transfer finishes, `bytes_transferred` rises by exactly the same amount
    /// that `bytes_in_flight` falls, so the total never jumps.  Its derivative
    /// is the true wire bandwidth regardless of file size or completion rate.
    prev_total_bytes: u64,
    /// Exponentially smoothed bandwidth in bytes/s.
    smoothed_bps:     f64,
}

impl DeltaEngine {
    pub fn new() -> Self {
        Self {
            last_compute_ms:  Utc::now().timestamp_millis(),
            prev_total_bytes: 0,
            smoothed_bps:     0.0,
        }
    }

    /// Build a [`DashboardUpdate`] from a pre-collected snapshot of state.
    ///
    /// # Parameters
    /// - `current_slice` / `total_slices` - from `CoachState` and the plan.
    /// - `registry_snapshot` - from `Registry::status_snapshot()`.
    /// - `totals_fn` - closure returning cumulative `AgentTotals` for one agent.
    /// - `drained` - transfers drained from `MetricsStore` since the last call.
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
                    plan_acked:         s.plan_acked,
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

        // ── Bandwidth: total-bytes-in-system derivative + EMA ─────────────────
        //
        // Track the sum of (cumulative completed bytes) + (current in-flight
        // bytes) across all agents.  When a transfer finishes, bytes_transferred
        // rises by B and bytes_in_flight falls by B, so the total is continuous
        // with no discontinuity at completion.  The derivative of this quantity
        // is the true instantaneous wire rate for any file size or mix:
        //
        //   • Small files (complete within one tick): their full
        //     bytes_transferred shows up in the cumulative total; there is no
        //     inflight residual to subtract, so they are never under-counted.
        //
        //   • Large files (span multiple ticks): their progress is captured by
        //     the growing bytes_in_flight; on completion the bytes_transferred
        //     replaces the inflight contribution seamlessly.
        //
        //   • Mixed workloads: both contributions add up correctly because the
        //     total is a single monotone counter rather than two separate
        //     per-agent deltas that can cancel each other.
        //
        // An EMA (α = 0.4) smooths tick-to-tick jitter from the unsynchronised
        // aerogym heartbeat and aerocoach delta timers without hiding real
        // ramp-up / ramp-down trends.
        let total_now: u64 = agents
            .iter()
            .map(|a| a.bytes_transferred + a.bytes_in_flight)
            .sum();
        let raw_bps = total_now.saturating_sub(self.prev_total_bytes) as f64 / elapsed_secs;
        self.smoothed_bps     = EMA_ALPHA * raw_bps + (1.0 - EMA_ALPHA) * self.smoothed_bps;
        self.prev_total_bytes = total_now;
        let current_bandwidth_bps = self.smoothed_bps as u64;

        tracing::debug!(
            elapsed_ms        = (elapsed_secs * 1000.0) as u64,
            total_bytes_now   = total_now,
            raw_bps,
            smoothed_bps      = current_bandwidth_bps,
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
            plan_acked:         false,
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
    fn bandwidth_correct_for_small_files() {
        // Simulate 100 small files (1 MB each) completing entirely within one
        // tick window.  bytes_in_flight stays near zero; all throughput is in
        // bytes_transferred.  The old algorithm would zero this out when a
        // large prev_inflight value was present; the new one must not.
        let mut engine = DeltaEngine::new();
        let snapshot = [make_status("a00", 0, true, 50)];

        // Tick 1: large file partially transferred (60 MB in-flight),
        // no completions yet.
        let totals_t1 = AgentTotals {
            bytes_transferred: 0,
            bytes_in_flight:   60 * 1024 * 1024,
            success_count: 0, error_count: 0,
        };
        let u1 = engine.compute(0, 1, &snapshot, |_| totals_t1.clone(), &[]);
        // First tick: raw_bps = (60 MB - 0) / ~3 s = ~20 MB/s, smoothed.
        assert!(u1.global_stats.current_bandwidth_bps > 0);

        // Tick 2: large file still running (80 MB in-flight = 20 MB progress),
        // AND 100 small files completed (100 MB total).
        // Old algorithm: delta_completed = 100 MB - 60 MB = 40 MB (prev_inflight
        // of the large file cancels the small files).
        // New algorithm: total = 80 + 100 = 180 MB, prev = 60 MB, delta = 120 MB.
        let totals_t2 = AgentTotals {
            bytes_transferred: 100 * 1024 * 1024,  // 100 small files done
            bytes_in_flight:   80  * 1024 * 1024,  // large file still running
            success_count: 100, error_count: 0,
        };
        let u2 = engine.compute(0, 1, &snapshot, |_| totals_t2.clone(), &[]);
        // 120 MB delta / 3 s = 40 MB/s raw; EMA-smoothed value must be
        // substantially above zero and above the tick-1 value.
        assert!(
            u2.global_stats.current_bandwidth_bps > u1.global_stats.current_bandwidth_bps,
            "bandwidth should rise as both large and small file bytes flow"
        );
    }

    #[test]
    fn bandwidth_continuous_across_file_completion() {
        // When a large file completes, bytes_transferred rises and
        // bytes_in_flight falls by the same amount.  The total-in-system stays
        // flat: delta = 0, so raw_bps = 0 and the EMA decays — no spike.
        let mut engine = DeltaEngine::new();
        let snapshot = [make_status("a00", 0, true, 1)];

        // Tick 1: 100 MB in-flight, nothing completed yet.
        let t1 = AgentTotals { bytes_transferred: 0, bytes_in_flight: 100 * 1024 * 1024,
            success_count: 0, error_count: 0 };
        let u1 = engine.compute(0, 1, &snapshot, |_| t1.clone(), &[]);
        let bw1 = u1.global_stats.current_bandwidth_bps;
        assert!(bw1 > 0, "first tick should show positive bandwidth from in-flight data");

        // Tick 2: file completes (bytes_transferred = 100 MB, in_flight drops
        // to 0).  total_now == total_prev, so raw_bps = 0.
        let t2 = AgentTotals { bytes_transferred: 100 * 1024 * 1024, bytes_in_flight: 0,
            success_count: 1, error_count: 0 };
        let u2 = engine.compute(0, 1, &snapshot, |_| t2.clone(), &[]);
        let bw2 = u2.global_stats.current_bandwidth_bps;

        // EMA decays: bw2 = 0.4*0 + 0.6*bw1 = 0.6*bw1 < bw1.
        assert!(
            bw2 < bw1,
            "bandwidth should decay (not spike) when a file completes with no new data"
        );
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
