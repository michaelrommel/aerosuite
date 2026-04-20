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
//! The backend with the **fewest** active connections is the drain *candidate*.
//! We estimate the worst-case load on the busiest remaining backend if the
//! candidate were removed:
//!
//! ```text
//! extra_per  = candidate_connections / (active_count − 1)
//! worst_case = max_connections + extra_per
//! ```
//!
//! If `worst_case < drain_threshold` for `hysteresis_cycles` consecutive
//! cycles AND `desired > min` AND the drain cooldown has elapsed, the
//! candidate's weight file is written to `-1` (Draining).
//!
//! The existing P2 cleanup pass handles the rest: once IPVS connections reach
//! zero the backend is disabled and the instance is terminated.

use std::net::Ipv4Addr;
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info, warn};

use aerocore::AwsCredentials;
use crate::cleanup::{write_weight, WEIGHT_DRAINING};
use crate::snapshot::{BackendState, BackendStatus, SystemSnapshot};

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

// ── Entry point ───────────────────────────────────────────────────────────────

/// Evaluate scale-up and drain conditions against the current snapshot.
///
/// Must only be called on the VRRP master (the caller checks `is_master`).
/// `state` is updated in-place and carries hysteresis across cycles.
pub async fn run(
    snapshot:    &SystemSnapshot,
    config:      &ScaleConfig,
    state:       &mut ScalerState,
    asg_name:    &str,
    region:      &str,
    creds:       &AwsCredentials,
    weights_dir: &str,
    dry_run:     bool,
) -> Result<()> {
    info!("── scaler pass ───────────────────────────────────────────────────────────");

    // ── Collect active backends ───────────────────────────────────────────────
    //
    // Only backends that are Active, hold a live (non-expired) lease, *and*
    // have IPVS data participate in scale decisions.  Backends that are
    // Draining or Disabled are intentionally excluded — they are either
    // winding down or idle and would skew the averages.
    let active: Vec<&BackendStatus> = snapshot.backends.iter()
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
        state.drain_cycles    = 0;
        state.drain_candidate = None;
        return Ok(());
    }

    let total_connections: u32 = active.iter()
        .map(|b| b.ipvs.as_ref().unwrap().active_connections)
        .sum();
    // Use ceiling division so a single heavily-loaded backend triggers
    // scale-up even when others are idle.
    let avg_connections = (total_connections + active_count - 1) / active_count;

    info!(
        active_backends      = active_count,
        total_connections,
        avg_connections,
        scale_up_threshold   = config.scale_up_threshold,
        drain_threshold      = config.drain_threshold,
        "scaler: IPVS snapshot"
    );

    // Run both evaluations independently so a simultaneous "already maxed out
    // but also should drain" edge-case is handled gracefully.
    evaluate_scale_up(snapshot, config, state, avg_connections, asg_name, region, creds, dry_run).await;
    evaluate_drain(snapshot, &active, config, state, weights_dir, dry_run).await;

    info!("── scaler pass done ──────────────────────────────────────────────────────");
    Ok(())
}

// ── Scale-up evaluation ───────────────────────────────────────────────────────

async fn evaluate_scale_up(
    snapshot:        &SystemSnapshot,
    config:          &ScaleConfig,
    state:           &mut ScalerState,
    avg_connections: u32,
    asg_name:        &str,
    region:          &str,
    creds:           &AwsCredentials,
    dry_run:         bool,
) {
    let Some(group) = &snapshot.asg_group else {
        debug!("ASG group info not available — skipping scale-up check");
        return;
    };

    // ── Evaluate condition ────────────────────────────────────────────────────
    if avg_connections > config.scale_up_threshold {
        state.scale_up_cycles += 1;
        info!(
            avg_connections,
            threshold = config.scale_up_threshold,
            cycles    = state.scale_up_cycles,
            required  = config.hysteresis_cycles,
            "scale-up condition met ({}/{})",
            state.scale_up_cycles, config.hysteresis_cycles,
        );
    } else {
        if state.scale_up_cycles > 0 {
            debug!(
                avg_connections,
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
            desired          = group.desired_capacity,
            max              = group.max_size,
            avg_connections,
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
                elapsed_secs  = elapsed,
                cooldown_secs = config.scale_up_cooldown_secs,
                "scale-up cooldown active — waiting ({}/{}s)",
                elapsed, config.scale_up_cooldown_secs,
            );
            return;
        }
    }

    // ── Act ───────────────────────────────────────────────────────────────────
    let new_desired = (group.desired_capacity + 1) as u32;

    if dry_run {
        info!(
            "[DRY-RUN] scale-up: SetDesiredCapacity({asg_name}, {new_desired})  \
             (avg={avg_connections} > threshold={})",
            config.scale_up_threshold,
        );
    } else {
        info!(
            asg_name,
            new_desired,
            avg_connections,
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
    state.last_scale_up   = Some(Instant::now());
}

// ── Drain evaluation ──────────────────────────────────────────────────────────

async fn evaluate_drain(
    snapshot:    &SystemSnapshot,
    active:      &[&BackendStatus],
    config:      &ScaleConfig,
    state:       &mut ScalerState,
    weights_dir: &str,
    dry_run:     bool,
) {
    let Some(group) = &snapshot.asg_group else {
        debug!("ASG group info not available — skipping drain check");
        return;
    };

    let active_count = active.len() as u32;

    // Need at least 2 active backends to drain one.
    if active_count <= 1 {
        if state.drain_cycles > 0 {
            debug!("only 1 active backend — cannot drain; resetting counter");
        }
        state.drain_cycles    = 0;
        state.drain_candidate = None;
        return;
    }

    // Respect the ASG minimum: cleanup already guards individual terminations,
    // but we should not even start draining if it would leave desired == min.
    if group.desired_capacity <= group.min_size {
        if state.drain_cycles > 0 {
            debug!(
                desired = group.desired_capacity,
                min     = group.min_size,
                "desired == min — skipping drain (ASG constraint); resetting counter"
            );
        }
        state.drain_cycles    = 0;
        state.drain_candidate = None;
        return;
    }

    // ── Find candidate (fewest connections) and heaviest remaining backend ────
    let mut sorted: Vec<&BackendStatus> = active.to_vec();
    sorted.sort_by_key(|b| b.ipvs.as_ref().unwrap().active_connections);

    // SAFETY: active_count >= 2, so both unwraps are fine.
    let candidate   = *sorted.first().unwrap();
    let max_backend = *sorted.last().unwrap();

    let candidate_conn = candidate.ipvs.as_ref().unwrap().active_connections;
    let max_conn       = max_backend.ipvs.as_ref().unwrap().active_connections;

    // Conservative worst-case redistribution: candidate's connections are
    // spread evenly (ceiling) across the remaining (active_count - 1) backends.
    let extra_per  = (candidate_conn + (active_count - 2)) / (active_count - 1);
    let worst_case = max_conn + extra_per;

    debug!(
        candidate_ip   = %candidate.ip,
        candidate_conn,
        max_backend_ip = %max_backend.ip,
        max_conn,
        extra_per,
        worst_case,
        drain_threshold = config.drain_threshold,
        "drain evaluation"
    );

    // ── Candidate stability: reset hysteresis if the cheapest backend changed ─
    if state.drain_candidate != Some(candidate.ip) {
        if state.drain_cycles > 0 {
            info!(
                old = ?state.drain_candidate,
                new = %candidate.ip,
                "drain candidate changed — resetting hysteresis counter"
            );
        }
        state.drain_cycles    = 0;
        state.drain_candidate = Some(candidate.ip);
    }

    // ── Evaluate condition ────────────────────────────────────────────────────
    if worst_case < config.drain_threshold {
        state.drain_cycles += 1;
        info!(
            candidate_ip   = %candidate.ip,
            candidate_conn,
            worst_case,
            threshold = config.drain_threshold,
            cycles    = state.drain_cycles,
            required  = config.hysteresis_cycles,
            "drain condition met ({}/{})",
            state.drain_cycles, config.hysteresis_cycles,
        );
    } else {
        if state.drain_cycles > 0 {
            debug!(
                worst_case,
                threshold = config.drain_threshold,
                "drain condition cleared — resetting hysteresis counter"
            );
        }
        state.drain_cycles = 0;
        return;
    }

    if state.drain_cycles < config.hysteresis_cycles {
        return; // condition holds but hysteresis window not yet satisfied
    }

    // ── Cooldown check ────────────────────────────────────────────────────────
    if let Some(last) = state.last_drain {
        let elapsed = last.elapsed().as_secs();
        if elapsed < config.drain_cooldown_secs {
            info!(
                elapsed_secs  = elapsed,
                cooldown_secs = config.drain_cooldown_secs,
                "drain cooldown active — waiting ({}/{}s)",
                elapsed, config.drain_cooldown_secs,
            );
            return;
        }
    }

    // ── Initiate drain ────────────────────────────────────────────────────────
    //
    // Writing WEIGHT_DRAINING ("-1") to the weight file tells keepalived to
    // stop sending new connections to this backend.  The P2 cleanup pass will
    // detect Draining + 0 IPVS connections on a future cycle and issue the
    // TerminateInstance call (with the desired-capacity decrement flag).
    let slot_label = candidate.slot.map(|s| s.to_string()).unwrap_or_else(|| "?".into());

    if dry_run {
        info!(
            "[DRY-RUN] drain: write DRAINING weight for backend {} (slot {}, {} connections)",
            candidate.ip, slot_label, candidate_conn,
        );
    } else {
        info!(
            candidate_ip   = %candidate.ip,
            candidate_slot = %slot_label,
            candidate_conn,
            worst_case,
            drain_threshold = config.drain_threshold,
            "initiating drain: setting backend weight to DRAINING"
        );
        if let Err(e) = write_weight(weights_dir, candidate.ip, WEIGHT_DRAINING, dry_run).await {
            warn!(ip = %candidate.ip, "failed to write DRAINING weight: {e:#}");
            // Do NOT reset state — attempt again next cycle.
            return;
        }
    }

    state.drain_cycles    = 0;
    state.drain_candidate = None;
    state.last_drain      = Some(Instant::now());
}
