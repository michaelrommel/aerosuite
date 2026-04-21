//! P5 — Autoscaling: scale-up / drain decision engine.
//!
//! ## Primary signal: IPVS active connections
//!
//! All scaling decisions are driven by the IPVS `active_connections` counter
//! read from `/proc/net/ip_vs` — the load balancer's own view of how many
//! live connections each backend is currently serving.  Prometheus scrape data
//! (`ftp_sessions_total`) is used **only** as a cross-check for anomaly
//! detection; see `metrics::scrape_and_push`.
//!
//! ## Scale-up algorithm
//!
//! Each cycle, compute the average active connections across all `Active`
//! backends (those with a live lease and IPVS data).  When that average
//! exceeds `scale_up_threshold` for `hysteresis_cycles` consecutive cycles
//! AND `desired < max` AND the scale-up cooldown has elapsed,
//! `SetDesiredCapacity(desired + 1)` is called.
//!
//! ## Drain algorithm
//!
//! **Gate:** the average sessions across all `Active` backends
//! (`total_sessions / active_count`) must fall below `drain_threshold`.
//! If no backend is already `Draining` and the gate holds for
//! `hysteresis_cycles` consecutive cycles, a candidate is selected:
//!
//! 1. **Zero-session backend** (preferred) -- any `Active` backend carrying
//!    no sessions is drained first; removing it costs nothing.
//!
//! 2. **Most-loaded backend** (fallback) -- when all backends carry sessions,
//!    the *highest*-load backend is drained.  Setting it to `Draining`
//!    breaks IPVS persistence: keepalived stops routing new connections
//!    to it, so clients that were pinned by persistence are redistributed.
//!
//! Only one backend is drained at a time.  If a drain is already in
//! progress the evaluation is skipped entirely for that cycle.
//!
//! The P2 cleanup pass handles termination: once IPVS connections reach
//! zero the backend is disabled and the instance terminated.

use std::net::Ipv4Addr;
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::cleanup::{write_weight, WEIGHT_DRAINING};
use crate::snapshot::{BackendState, BackendStatus, SystemSnapshot};
use aerocore::AwsCredentials;

// ── Configuration ─────────────────────────────────────────────────────────────

/// All tunable parameters for the scale-up / drain algorithm.
///
/// Constructed once from CLI args and passed into every [`run`] call.
#[derive(Debug, Clone)]
pub struct ScaleConfig {
    /// Average active connections per active backend that triggers a scale-up.
    ///
    /// Recommended: 50 % of the per-backend design maximum.
    /// Default: 750 (50 % of 1500).
    pub scale_up_threshold: u32,

    /// Maximum active connections allowed on the busiest remaining backend
    /// after the drain candidate is removed.  If the worst-case redistribution
    /// would exceed this value, no drain is initiated.
    ///
    /// Recommended: 33 % of the per-backend design maximum.
    /// Default: 500 (33 % of 1500).
    pub drain_threshold: u32,

    /// Number of consecutive snapshot cycles a condition must persist before a
    /// scale-up or drain action is taken.  Prevents flapping on transient load
    /// spikes.  Default: 3.
    pub hysteresis_cycles: u32,

    /// Minimum seconds between two consecutive scale-up actions.  AWS
    /// typically takes 2–3 minutes to bring a new backend InService, so
    /// scaling up more frequently than this would queue redundant launches.
    /// Default: 120 s.
    pub scale_up_cooldown_secs: u64,

    /// Minimum seconds between two consecutive drain initiations.  Draining
    /// is slower and more disruptive than scaling up; a longer default keeps
    /// the fleet stable.  Default: 300 s.
    pub drain_cooldown_secs: u64,

    /// Maximum number of backends allowed in `Draining` state simultaneously.
    ///
    /// - When this limit is reached the drain evaluator skips entirely.
    /// - When below the limit but at least one drain is already in progress,
    ///   only a zero-session backend may be added as an additional drain;
    ///   loaded backends are left alone until the in-progress drain completes.
    /// Default: 2.
    pub max_concurrent_draining: u32,
}

// ── Persistent state (survives across cycles) ─────────────────────────────────

/// Mutable scaler state that must live across `run()` calls.
///
/// Initialise once with `ScalerState::default()` before the main loop.
#[derive(Debug, Default)]
pub struct ScalerState {
    /// Consecutive cycles where the average connections exceeded the scale-up
    /// threshold.  Reset to 0 when the average drops below threshold or when
    /// a scale-up action is successfully initiated.
    pub scale_up_cycles: u32,

    /// Consecutive cycles where the drain condition was satisfied for the
    /// current drain candidate.  Reset when the condition clears, when the
    /// candidate IP changes, or when a drain is initiated.
    pub drain_cycles: u32,

    /// The drain candidate IP being tracked across hysteresis cycles.
    /// When the cheapest backend changes between cycles the counter resets.
    pub drain_candidate: Option<Ipv4Addr>,

    /// Wall time of the last successful scale-up call.
    pub last_scale_up: Option<Instant>,

    /// Wall time of the last drain initiation.
    pub last_drain: Option<Instant>,
}

// ── IPVS → session normalisation ─────────────────────────────────────────────

/// Convert an IPVS active-connection count to an approximate FTP session count.
///
/// Each FTP transfer uses **two** TCP connections tracked by IPVS:
///   1. The control channel (port 21) — present for the full session lifetime.
///   2. The passive data channel (ephemeral port) — present during the transfer.
///
/// Dividing by two converts the IPVS metric to the session count that all
/// configured thresholds (`scale_up_threshold`, `drain_threshold`) are
/// expressed in.
#[inline]
fn sessions_from_ipvs(connections: u32) -> u32 {
    connections / 2
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Evaluate scale-up and drain conditions against the current snapshot.
///
/// Must only be called on the VRRP master (the caller checks `is_master`).
/// `state` is updated in-place and carries hysteresis across cycles.
pub async fn run(
    snapshot: &SystemSnapshot,
    config: &ScaleConfig,
    state: &mut ScalerState,
    asg_name: &str,
    region: &str,
    creds: &AwsCredentials,
    weights_dir: &str,
    dry_run: bool,
) -> Result<()> {
    info!("── scaler pass ───────────────────────────────────────────────────────────");

    // ── Collect active backends ───────────────────────────────────────────────
    //
    // Only backends that are Active, hold a live (non-expired) lease, *and*
    // have IPVS data participate in scale decisions.  Backends that are
    // Draining or Disabled are intentionally excluded — they are either
    // winding down or idle and would skew the averages.
    let active: Vec<&BackendStatus> = snapshot
        .backends
        .iter()
        .filter(|b| {
            b.weight_state == BackendState::Active
                && b.lease.as_ref().map(|l| !l.is_expired()).unwrap_or(false)
                && b.ipvs.is_some()
        })
        .collect();

    let active_count = active.len() as u32;

    if active_count == 0 {
        debug!("no active backends with IPVS data — skipping scaler");
        state.scale_up_cycles = 0;
        state.drain_cycles = 0;
        state.drain_candidate = None;
        return Ok(());
    }

    let total_sessions: u32 = active
        .iter()
        .map(|b| sessions_from_ipvs(b.ipvs.as_ref().unwrap().active_connections))
        .sum();
    // Ceiling division: a single heavily-loaded backend triggers scale-up
    // even when others are idle.
    let avg_sessions = (total_sessions + active_count - 1) / active_count;

    info!(
        active_backends = active_count,
        total_sessions,
        avg_sessions,
        scale_up_threshold = config.scale_up_threshold,
        drain_threshold = config.drain_threshold,
        "scaler: session snapshot (IPVS connections / 2)"
    );

    // Run both evaluations independently so a simultaneous "already maxed out
    // but also should drain" edge-case is handled gracefully.
    evaluate_scale_up(
        snapshot,
        config,
        state,
        avg_sessions,
        asg_name,
        region,
        creds,
        dry_run,
    )
    .await;
    evaluate_drain(snapshot, &active, config, state, weights_dir, dry_run).await;

    info!("── scaler pass done ──────────────────────────────────────────────────────");
    Ok(())
}

// ── Scale-up evaluation ───────────────────────────────────────────────────────

async fn evaluate_scale_up(
    snapshot: &SystemSnapshot,
    config: &ScaleConfig,
    state: &mut ScalerState,
    avg_sessions: u32,
    asg_name: &str,
    region: &str,
    creds: &AwsCredentials,
    dry_run: bool,
) {
    let Some(group) = &snapshot.asg_group else {
        debug!("ASG group info not available — skipping scale-up check");
        return;
    };

    // ── Evaluate condition ────────────────────────────────────────────────────
    if avg_sessions > config.scale_up_threshold {
        state.scale_up_cycles += 1;
        info!(
            avg_sessions,
            threshold = config.scale_up_threshold,
            cycles = state.scale_up_cycles,
            required = config.hysteresis_cycles,
            "scale-up condition met ({}/{})",
            state.scale_up_cycles,
            config.hysteresis_cycles,
        );
    } else {
        if state.scale_up_cycles > 0 {
            debug!(
                avg_sessions,
                threshold = config.scale_up_threshold,
                "scale-up condition cleared — resetting hysteresis counter"
            );
        }
        state.scale_up_cycles = 0;
        return;
    }

    if state.scale_up_cycles < config.hysteresis_cycles {
        return; // condition holds but hysteresis window not yet satisfied
    }

    // ── Guards ────────────────────────────────────────────────────────────────
    if group.desired_capacity >= group.max_size {
        warn!(
            desired = group.desired_capacity,
            max = group.max_size,
            avg_sessions,
            "scale-up condition met but ASG is at maximum capacity — cannot scale up further"
        );
        // Do NOT reset the counter: the condition is still true.  If max_size
        // is raised by the operator the next cycle will act immediately.
        return;
    }

    if let Some(last) = state.last_scale_up {
        let elapsed = last.elapsed().as_secs();
        if elapsed < config.scale_up_cooldown_secs {
            info!(
                elapsed_secs = elapsed,
                cooldown_secs = config.scale_up_cooldown_secs,
                "scale-up cooldown active — waiting ({}/{}s)",
                elapsed,
                config.scale_up_cooldown_secs,
            );
            return;
        }
    }

    // ── Act ───────────────────────────────────────────────────────────────────
    let new_desired = (group.desired_capacity + 1) as u32;

    if dry_run {
        info!(
            "[DRY-RUN] scale-up: SetDesiredCapacity({asg_name}, {new_desired})  \
             (avg={avg_sessions} sessions > threshold={})",
            config.scale_up_threshold,
        );
    } else {
        info!(
            asg_name,
            new_desired,
            avg_sessions,
            threshold = config.scale_up_threshold,
            "scaling up: SetDesiredCapacity → {new_desired}"
        );
        if let Err(e) = aerocore::asg::set_desired(region, asg_name, new_desired, creds).await {
            warn!("scale-up SetDesiredCapacity failed: {e:#}");
            // Do NOT reset state — attempt again next cycle.
            return;
        }
    }

    state.scale_up_cycles = 0;
    state.last_scale_up = Some(Instant::now());
}

// ── Drain evaluation ─────────────────────────────────────────────────────────

async fn evaluate_drain(
    snapshot: &SystemSnapshot,
    active: &[&BackendStatus],
    config: &ScaleConfig,
    state: &mut ScalerState,
    weights_dir: &str,
    dry_run: bool,
) {
    let Some(group) = &snapshot.asg_group else {
        debug!("ASG group info not available -- skipping drain check");
        return;
    };

    let active_count = active.len() as u32;

    // ── Guards ────────────────────────────────────────────────────────────────

    if active_count <= 1 {
        if state.drain_cycles > 0 {
            debug!("only 1 active backend -- cannot drain; resetting counter");
        }
        state.drain_cycles = 0;
        state.drain_candidate = None;
        return;
    }

    if group.desired_capacity <= group.min_size {
        if state.drain_cycles > 0 {
            debug!(
                desired = group.desired_capacity,
                min = group.min_size,
                "desired == min -- skipping drain (ASG constraint); resetting counter"
            );
        }
        state.drain_cycles = 0;
        state.drain_candidate = None;
        return;
    }

    // Count backends currently in Draining state.
    // At max_concurrent_draining: skip entirely (do not reset drain_cycles).
    // Below max but non-zero: only allow an additional zero-session drain.
    let draining_count = snapshot.backends.iter()
        .filter(|b| b.weight_state == BackendState::Draining)
        .count() as u32;

    if draining_count >= config.max_concurrent_draining {
        debug!(
            draining_count,
            max = config.max_concurrent_draining,
            "at max concurrent draining backends -- skipping"
        );
        // Do not reset drain_cycles: the gate may still be open.
        return;
    }

    // Sort active backends ascending by sessions (needed for candidate selection).
    let mut sorted: Vec<&BackendStatus> = active.to_vec();
    sorted.sort_by_key(|b| sessions_from_ipvs(b.ipvs.as_ref().unwrap().active_connections));

    // ── Gate: average sessions must be below drain_threshold ─────────────────

    let total_sessions: u32 = active
        .iter()
        .map(|b| sessions_from_ipvs(b.ipvs.as_ref().unwrap().active_connections))
        .sum();
    // Floor division is intentionally conservative: only drain when the
    // average is comfortably below the limit.
    let avg_sessions = total_sessions / active_count;

    if avg_sessions >= config.drain_threshold {
        if state.drain_cycles > 0 {
            debug!(
                avg_sessions,
                threshold = config.drain_threshold,
                "drain gate closed -- average above threshold; resetting counter"
            );
        }
        state.drain_cycles = 0;
        return;
    }

    // ── Hysteresis: gate must hold for N consecutive cycles ───────────────────

    state.drain_cycles += 1;
    info!(
        avg_sessions,
        threshold = config.drain_threshold,
        active_count,
        cycles = state.drain_cycles,
        required = config.hysteresis_cycles,
        "drain gate open -- average below threshold ({}/{})",
        state.drain_cycles,
        config.hysteresis_cycles,
    );

    if state.drain_cycles < config.hysteresis_cycles {
        return;
    }

    // ── Cooldown check ────────────────────────────────────────────────────────

    if let Some(last) = state.last_drain {
        let elapsed = last.elapsed().as_secs();
        if elapsed < config.drain_cooldown_secs {
            info!(
                elapsed_secs = elapsed,
                cooldown_secs = config.drain_cooldown_secs,
                "drain cooldown active -- waiting ({}/{}s)",
                elapsed,
                config.drain_cooldown_secs,
            );
            return;
        }
    }

    // ── Candidate selection ───────────────────────────────────────────────────

    let least          = *sorted.first().unwrap();
    let least_sessions = sessions_from_ipvs(least.ipvs.as_ref().unwrap().active_connections);

    let (candidate, reason) = if least_sessions == 0 {
        // Zero-session backend: free drain regardless of other draining state.
        (least, "0-session backend -- free drain")
    } else if draining_count == 0 {
        // No drain in progress: drain the most-loaded backend to break
        // IPVS persistence and redistribute its pinned clients.
        (
            *sorted.last().unwrap(),
            "all backends loaded -- draining most loaded to break persistence",
        )
    } else {
        // A drain is in progress and no zero-session backend is available.
        // Wait for it to complete rather than piling a loaded backend on top.
        debug!(
            draining_count,
            "drain in progress, no 0-session backend available -- waiting"
        );
        return;
    };

    let candidate_sessions =
        sessions_from_ipvs(candidate.ipvs.as_ref().unwrap().active_connections);
    let slot_label = candidate
        .slot
        .map(|s| s.to_string())
        .unwrap_or_else(|| "?".into());

    // ── Initiate drain ────────────────────────────────────────────────────────

    if dry_run {
        info!(
            "[DRY-RUN] drain: write DRAINING for backend {} (slot {}, {} sessions) -- {}",
            candidate.ip, slot_label, candidate_sessions, reason,
        );
    } else {
        info!(
            candidate_ip       = %candidate.ip,
            candidate_slot     = %slot_label,
            candidate_sessions,
            avg_sessions,
            reason,
            "initiating drain: setting backend weight to DRAINING"
        );
        if let Err(e) = write_weight(weights_dir, candidate.ip, WEIGHT_DRAINING, dry_run).await {
            warn!(ip = %candidate.ip, "failed to write DRAINING weight: {e:#}");
            return;
        }
    }

    state.drain_cycles = 0;
    state.drain_candidate = None;
    state.last_drain = Some(Instant::now());
}
