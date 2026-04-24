//! Agent-side load plan: wraps the proto [`LoadPlan`] with per-agent helpers.
//!
//! The proto message is treated as the source of truth; this wrapper only
//! adds helper methods so the rest of the agent code doesn't need to dig
//! into proto internals.

use aeroproto::aeromonitor::{FileSizeBucket, LoadPlan, LoadPlanUpdate};
use tracing::warn;

use super::rate_limit::RateLimiterConfig;

/// Agent-local view of the load plan received from aerocoach.
#[derive(Debug, Clone)]
pub struct AgentPlan {
    pub proto: LoadPlan,
    pub agent_index: u32,
}

impl AgentPlan {
    pub fn new(proto: LoadPlan, agent_index: u32) -> Self {
        Self { proto, agent_index }
    }

    // ── Fleet accessors ───────────────────────────────────────────────────

    pub fn plan_id(&self) -> &str {
        &self.proto.plan_id
    }

    /// Effective agent count (clamped to at least 1 to avoid division by zero).
    pub fn total_agents(&self) -> u32 {
        self.proto.total_agents.max(1)
    }

    pub fn total_slices(&self) -> u32 {
        self.proto.slices.len() as u32
    }

    #[allow(dead_code)] // used by the slice clock (Phase B)
    pub fn slice_duration_ms(&self) -> u64 {
        self.proto.slice_duration_ms
    }

    // ── Per-agent share ───────────────────────────────────────────────────

    /// Number of concurrent connections *this* agent should be running during
    /// `slice_index`.  Returns 0 if the slice is not found in the plan.
    pub fn my_connections_for_slice(&self, slice_index: u32) -> u32 {
        let total = self
            .proto
            .slices
            .iter()
            .find(|s| s.slice_index == slice_index)
            .map(|s| s.total_connections)
            .unwrap_or(0);
        per_agent_connections(total, self.agent_index, self.total_agents())
    }

    /// Bandwidth ceiling for this agent in bytes per second.
    pub fn my_bandwidth_bps(&self) -> u64 {
        let total = self.proto.total_bandwidth_bps;
        if total == 0 {
            return 0;
        }
        total / self.total_agents() as u64
    }

    /// Build a [`RateLimiterConfig`] for this agent's bandwidth share.
    /// Returns `None` when no bandwidth limit is configured (unlimited).
    pub fn my_rate_config(&self) -> Option<RateLimiterConfig> {
        let bps = self.my_bandwidth_bps();
        if bps == 0 {
            return None;
        }
        RateLimiterConfig::from_bps(bps)
    }

    /// Per-transfer rate when `n` transfers are running concurrently.
    ///
    /// This is the *ideal* rate used as a floor when carry-over transfers
    /// have already consumed most of the agent's bandwidth budget.
    /// Returns `None` when bandwidth is unlimited or `n` is zero.
    pub fn rate_per_transfer(&self, n: u32) -> Option<RateLimiterConfig> {
        let agent_bps = self.my_bandwidth_bps();
        if agent_bps == 0 || n == 0 {
            return None;
        }
        RateLimiterConfig::from_bps(agent_bps / n as u64)
    }

    // ── File distribution ─────────────────────────────────────────────────

    /// Pick a bucket at random, weighted by each bucket's `percentage`.
    ///
    /// Falls back to the last bucket if the distribution doesn't quite sum to
    /// 1.0 (handles floating-point rounding).
    pub fn weighted_random_bucket(&self) -> Option<&FileSizeBucket> {
        let buckets = self
            .proto
            .file_distribution
            .as_ref()
            .map(|d| d.buckets.as_slice())
            .unwrap_or(&[]);

        if buckets.is_empty() {
            warn!("plan has no file-size buckets");
            return None;
        }

        let r: f32 = rand::random();
        let mut cumulative = 0.0_f32;
        for bucket in buckets {
            cumulative += bucket.percentage;
            if r <= cumulative {
                return Some(bucket);
            }
        }
        // Floating-point rounding safety net
        buckets.last()
    }

    /// All buckets in the distribution, in declaration order.
    pub fn buckets(&self) -> &[FileSizeBucket] {
        self.proto
            .file_distribution
            .as_ref()
            .map(|d| d.buckets.as_slice())
            .unwrap_or(&[])
    }

    // ── Dynamic updates ───────────────────────────────────────────────────

    /// Apply a partial [`LoadPlanUpdate`] received via the `Session` stream.
    ///
    /// Slices before `effective_from_slice` are preserved unchanged; slices
    /// from that index onwards are replaced with the incoming list.
    pub fn apply_update(&mut self, update: LoadPlanUpdate) {
        // Replace future slices
        self.proto
            .slices
            .retain(|s| s.slice_index < update.effective_from_slice);
        self.proto.slices.extend(update.updated_slices);
        self.proto.slices.sort_by_key(|s| s.slice_index);

        if let Some(bw) = update.new_bandwidth_bps {
            self.proto.total_bandwidth_bps = bw;
        }
        if let Some(dist) = update.new_file_distribution {
            self.proto.file_distribution = Some(dist);
        }
    }
}

// ── Helper (mirrors aerocoach/model/distributor.rs) ───────────────────────

/// Calculate the connection share for one agent.
///
/// The remainder is distributed one-per-agent to the lowest-indexed agents so
/// that the sum across all agents always equals `total`.
pub fn per_agent_connections(total: u32, agent_index: u32, total_agents: u32) -> u32 {
    let agents = total_agents.max(1);
    let base = total / agents;
    let remainder = total % agents;
    base + u32::from(agent_index < remainder)
}

// ── Generate a unique remote filename for one transfer ─────────────────────

/// Build the remote FTP filename for a single transfer.
///
/// Format: `<agent_id>_s<slice:03>_c<conn_id:06>.dat`
/// Example: `a03_s007_c000042.dat`
pub fn make_transfer_filename(agent_id: &str, slice_index: u32, conn_id: u64) -> String {
    format!("{agent_id}_s{slice_index:03}_c{conn_id:06}.dat")
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aeroproto::aeromonitor::{FileSizeDistribution, FileSizeBucket, LoadPlan, TimeSlice};

    fn make_plan(total_connections: u32, total_agents: u32) -> AgentPlan {
        AgentPlan::new(
            LoadPlan {
                plan_id: "test".into(),
                slice_duration_ms: 60_000,
                total_bandwidth_bps: 10_000_000,
                total_agents,
                slices: vec![
                    TimeSlice { slice_index: 0, total_connections },
                    TimeSlice { slice_index: 1, total_connections: total_connections * 2 },
                ],
                file_distribution: Some(FileSizeDistribution {
                    buckets: vec![
                        FileSizeBucket { bucket_id: "xs".into(), size_min_bytes: 0, size_max_bytes: 10_485_760, percentage: 0.6 },
                        FileSizeBucket { bucket_id: "lg".into(), size_min_bytes: 10_485_760, size_max_bytes: 104_857_600, percentage: 0.4 },
                    ],
                }),
                start_time_ms: 0,
            },
            0, // agent_index
        )
    }

    #[test]
    fn connections_split_evenly() {
        // 10 connections, 2 agents → 5 each
        for i in 0..2 {
            let mut p = make_plan(10, 2);
            p.agent_index = i;
            assert_eq!(p.my_connections_for_slice(0), 5);
        }
    }

    #[test]
    fn connections_remainder_goes_to_lower_indices() {
        // 10 connections, 3 agents → 4, 3, 3
        let counts: Vec<u32> = (0..3).map(|i| {
            let mut p = make_plan(10, 3);
            p.agent_index = i;
            p.my_connections_for_slice(0)
        }).collect();
        assert_eq!(counts, [4, 3, 3]);
        assert_eq!(counts.iter().sum::<u32>(), 10);
    }

    #[test]
    fn unknown_slice_returns_zero() {
        let p = make_plan(10, 1);
        assert_eq!(p.my_connections_for_slice(99), 0);
    }

    #[test]
    fn bandwidth_divided_by_agent_count() {
        // plan has 10 MB/s, 4 agents → 2.5 MB/s each
        let mut p = make_plan(0, 4);
        p.proto.total_bandwidth_bps = 10_000_000;
        assert_eq!(p.my_bandwidth_bps(), 2_500_000);
    }

    #[test]
    fn apply_update_replaces_future_slices() {
        let mut p = make_plan(10, 1);
        p.apply_update(LoadPlanUpdate {
            effective_from_slice: 1,
            updated_slices: vec![TimeSlice { slice_index: 1, total_connections: 999 }],
            new_bandwidth_bps: None,
            new_file_distribution: None,
        });
        assert_eq!(p.my_connections_for_slice(0), 10); // unchanged
        assert_eq!(p.my_connections_for_slice(1), 999); // replaced
    }

    #[test]
    fn apply_update_changes_bandwidth() {
        let mut p = make_plan(0, 1);
        p.apply_update(LoadPlanUpdate {
            effective_from_slice: 0,
            updated_slices: vec![],
            new_bandwidth_bps: Some(50_000_000),
            new_file_distribution: None,
        });
        assert_eq!(p.my_bandwidth_bps(), 50_000_000);
    }

    #[test]
    fn filename_format() {
        assert_eq!(make_transfer_filename("a03", 7, 42), "a03_s007_c000042.dat");
    }
}
