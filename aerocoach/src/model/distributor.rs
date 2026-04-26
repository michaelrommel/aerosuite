//! Per-agent workload distribution helpers.
//!
//! aerocoach holds a single fleet-level load plan (total connections, total
//! bandwidth).  These functions derive the individual share for one agent so
//! the same calculation is performed consistently in aerocoach (when building
//! `RegisterResponse`) and can be validated in tests independently of the
//! gRPC layer.

/// Returns the target number of concurrent connections for one agent.
///
/// The fleet-level `total` is divided as evenly as possible.  Any remainder
/// is spread one-per-agent across the lowest-indexed agents, so the total
/// always equals the requested fleet target.
///
/// # Panics
/// Panics in debug builds if `agent_index >= total_agents` or if
/// `total_agents` is zero.
///
/// # Examples
/// ```
/// use aerocoach::model::distributor::per_agent_connections;
///
/// // 10 connections across 3 agents → 4, 3, 3
/// assert_eq!(per_agent_connections(10, 0, 3), 4);
/// assert_eq!(per_agent_connections(10, 1, 3), 3);
/// assert_eq!(per_agent_connections(10, 2, 3), 3);
///
/// // Verify the sum always equals the fleet total.
/// let total: u32 = (0..3).map(|i| per_agent_connections(10, i, 3)).sum();
/// assert_eq!(total, 10);
/// ```
#[allow(dead_code)]
pub fn per_agent_connections(total: u32, agent_index: u32, total_agents: u32) -> u32 {
    debug_assert!(total_agents > 0, "total_agents must be > 0");
    debug_assert!(
        agent_index < total_agents,
        "agent_index {agent_index} out of range (total_agents={total_agents})"
    );
    let base = total / total_agents;
    let remainder = total % total_agents;
    base + u32::from(agent_index < remainder)
}

/// Returns the bandwidth ceiling for one agent in bytes per second.
///
/// The fleet-level `total_bps` is divided evenly; any sub-byte-per-second
/// remainder is truncated (immaterial at practical bandwidths).
///
/// # Panics
/// Panics in debug builds if `total_agents` is zero.
///
/// # Examples
/// ```
/// use aerocoach::model::distributor::per_agent_bandwidth;
///
/// // 100 Mbit/s across 4 agents → 25 Mbit/s each
/// assert_eq!(per_agent_bandwidth(104_857_600, 4), 26_214_400);
/// ```
#[allow(dead_code)]
pub fn per_agent_bandwidth(total_bps: u64, total_agents: u32) -> u64 {
    debug_assert!(total_agents > 0, "total_agents must be > 0");
    total_bps / total_agents as u64
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── per_agent_connections ──────────────────────────────────────────────

    #[test]
    fn connections_divides_evenly() {
        // 12 connections, 4 agents → 3 each
        for i in 0..4 {
            assert_eq!(per_agent_connections(12, i, 4), 3);
        }
    }

    #[test]
    fn connections_remainder_distributed_to_front() {
        // 10 connections, 3 agents → 4, 3, 3
        assert_eq!(per_agent_connections(10, 0, 3), 4);
        assert_eq!(per_agent_connections(10, 1, 3), 3);
        assert_eq!(per_agent_connections(10, 2, 3), 3);
    }

    #[test]
    fn connections_sum_always_matches_fleet_total() {
        for total in [0u32, 1, 7, 10, 99, 100, 101] {
            for n_agents in 1u32..=10 {
                let sum: u32 = (0..n_agents)
                    .map(|i| per_agent_connections(total, i, n_agents))
                    .sum();
                assert_eq!(
                    sum, total,
                    "sum mismatch for total={total} n_agents={n_agents}"
                );
            }
        }
    }

    #[test]
    fn connections_zero_total() {
        for i in 0..5 {
            assert_eq!(per_agent_connections(0, i, 5), 0);
        }
    }

    #[test]
    fn connections_single_agent_gets_all() {
        assert_eq!(per_agent_connections(42, 0, 1), 42);
    }

    #[test]
    fn connections_more_agents_than_connections() {
        // 3 connections, 10 agents → first 3 get 1 each, rest get 0
        for i in 0..3 {
            assert_eq!(per_agent_connections(3, i, 10), 1);
        }
        for i in 3..10 {
            assert_eq!(per_agent_connections(3, i, 10), 0);
        }
    }

    // ── per_agent_bandwidth ────────────────────────────────────────────────

    #[test]
    fn bandwidth_divides_evenly() {
        // 100 MiB/s across 4 agents
        assert_eq!(per_agent_bandwidth(104_857_600, 4), 26_214_400);
    }

    #[test]
    fn bandwidth_truncates_remainder() {
        // 10 bytes/s across 3 agents → 3 each (remainder 1 is dropped)
        assert_eq!(per_agent_bandwidth(10, 3), 3);
    }

    #[test]
    fn bandwidth_single_agent_gets_all() {
        assert_eq!(per_agent_bandwidth(999_999, 1), 999_999);
    }
}
