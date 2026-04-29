//! P5 — Autoscaling: scale-up / drain decision engine.
//!
//! ## Scale-up algorithm: three-trigger combined condition
//!
//! Each cycle, the scaler evaluates three independent triggers against the
//! current IPVS session snapshot.  Any one trigger is sufficient to initiate
//! a scale-out; the hard ceiling bypasses hysteresis entirely.
//!
//! ### Trigger 1 — smoothed connection slope (leading indicator)
//! Computes a 6-lag smoothed first derivative of `avg_sessions` expressed in
//! connections per minute.  Fires when the slope exceeds
//! `slope_threshold_conn_per_min` AND `avg_sessions >= slope_low_floor`.
//! Requires `scale_up_hysteresis_cycles` consecutive confirmations before acting.
//!
//! ### Trigger 2 — per-backend bandwidth (saturation guard)
//! Reads the load balancer's `eth0` RX byte counter from `/proc/net/dev`
//! each cycle, computes an EMA-smoothed bytes/s rate, then divides by the
//! number of active backends to estimate per-backend ingress load.  Valid
//! because the LB runs in NAT mode: *all* client traffic — Active and Passive
//! FTP alike — enters through `eth0` and is forwarded to the backend pool.
//! Fires when the per-backend estimate exceeds `bw_threshold_bps_per_backend`
//! AND `avg_sessions >= slope_low_floor`.
//! Shares the hysteresis counter with Trigger 1.
//!
//! ### Trigger 3 — hard connection ceiling (backstop)
//! Fires immediately when `avg_sessions >= hard_conn_threshold` (1 200 by
//! default, 80 % of the 1 500 passive-port limit).  Bypasses the hysteresis
//! counter entirely; only the cooldown still applies.
//!
//! ## TEST vs PRODUCTION timing
//!
//! Test runs use a **10× time compression**: production samples at 300 s
//! intervals are transposed to 30 s and interpolated to 10 s slices.
//! Parameters marked **[time-sensitive]** must be **multiplied by 10** when
//! moving from test to production.
//!
//! | Parameter                      | TEST default     | PRODUCTION (×10)  |
//! |--------------------------------|-----------------|-------------------|
//! | `--snapshot-interval`          | 3 s             | 30 s              |
//! | slope window (6 lags × intv.)  | 18 s            | 180 s             |
//! | `--slope-threshold-per-min`    | 250 conn/min    | 25 conn/min       |
//! | `--scale-up-cooldown-secs`     | 12 s            | 120 s             |
//! | `--drain-cooldown-secs`        | 30 s            | 300 s             |
//! | `hard_conn_threshold`          | 1 200 (same)    | 1 200 (same)      |
//! | `bw_threshold_bps_per_backend` | 200 MB/s (same) | 200 MB/s (same)   |
//!
//! ## Drain algorithm
//!
//! **Gate:** the average sessions across all `Active` backends must fall below
//! `drain_threshold`.  If no backend is already `Draining` and the gate holds
//! for `scale_up_hysteresis_cycles` consecutive cycles, a candidate is selected:
//!
//! 1. **Zero-session backend** (preferred) — removing it costs nothing.
//! 2. **Most-loaded backend** (fallback) — draining it breaks IPVS persistence
//!    so pinned clients are redistributed.
//!
//! Only one backend is drained at a time; the P2 cleanup pass terminates it.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::cleanup::{write_weight, WEIGHT_DRAINING};
use crate::snapshot::{BackendState, BackendStatus, SystemSnapshot};
use aerocore::AwsCredentials;

// ── Bandwidth monitoring constants ────────────────────────────────────────────

/// Network interface on the load balancer whose RX byte counter is used for
/// Trigger 2.  In the production (and test) topology the LB runs in NAT mode:
/// all client FTP traffic — Active and Passive alike — arrives on `eth0`.
/// `eth0 RX bytes/s / active_backends` is therefore an accurate per-backend
/// ingress load estimate.
const BW_IFACE: &str = "eth0";

/// EMA smoothing factor applied to the raw per-cycle bandwidth sample.
/// α = 0.6 gives a half-life of ≈ 1.2 cycles:
///   • TEST  (3 s cycles)  → half-life ≈  3.5 s — damps single-tick jitter
///   • PROD  (30 s cycles) → half-life ≈ 35 s  — smooth but tracks ramps
const BW_EMA_ALPHA: f64 = 0.6;

// ── Configuration ─────────────────────────────────────────────────────────────

/// All tunable parameters for the scale-up / drain algorithm.
///
/// Constructed once from CLI args and passed into every [`run`] call.
///
/// ## TEST vs PRODUCTION timing
///
/// Test runs compress the production timeline by **10×**.  Parameters
/// labelled **[time-sensitive]** should be multiplied by 10 when deploying
/// to production (see the module-level table for details).
#[derive(Debug, Clone)]
pub struct ScaleConfig {
    /// Absolute average-sessions ceiling.  When crossed, scale-out fires
    /// **immediately** without waiting for `scale_up_hysteresis_cycles`.  Acts as a
    /// hard backstop regardless of slope or bandwidth state.
    ///
    /// Default: 1 200 (80 % of the 1 500 passive-port ceiling).
    /// TEST and PRODUCTION: same — absolute connection count.
    pub hard_conn_threshold: u32,

    /// Minimum average sessions before slope or bandwidth triggers are armed.
    /// Prevents false positives at idle or very low load.
    ///
    /// Default: 500.  TEST and PRODUCTION: same — absolute connection count.
    pub slope_low_floor: u32,

    /// Smoothed first-derivative threshold in **connections per minute**.
    /// Trigger 1 fires when the 6-lag smoothed slope of `avg_sessions`
    /// exceeds this value AND `avg_sessions >= slope_low_floor`.
    ///
    /// **[time-sensitive]**
    /// TEST default : 250 conn/min  (≈ 37 % of the ~670 conn/min test ramp)
    /// PRODUCTION   : 25 conn/min   (÷ 10 — same fraction at 1× real speed)
    pub slope_threshold_conn_per_min: f64,

    /// Per-backend bandwidth trigger threshold in bytes/s (Trigger 2).
    /// Trigger fires when `eth0_rx_bps / active_backends` exceeds this value
    /// AND `avg_sessions >= slope_low_floor`.
    ///
    /// Backend AWS instances have a burst ceiling of ≈ 5 Gbit/s total
    /// (in + out); sustained throughput is ≈ 2 Gbit/s inbound on eth1.
    /// Default: 200 000 000 bytes/s = 200 MB/s = 1.6 Gbit/s ≈ 80 % of that.
    /// TEST and PRODUCTION: same — physical saturation is unchanged.
    pub bw_threshold_bps_per_backend: u64,

    /// Maximum IPVS active connections on the busiest remaining backend after
    /// a drain candidate is removed.  Recommended: 33 % of per-backend max.
    /// Default: 500 (33 % of 1 500).
    pub drain_threshold: u32,

    /// Number of consecutive snapshot cycles Triggers 1 and 2 must persist
    /// before a scale-up action is taken.
    /// Trigger 3 (hard ceiling) bypasses this counter entirely.
    ///
    /// **[time-sensitive]** — each cycle is `snapshot_interval` seconds.
    /// TEST: 3 cycles × 3 s  = 9 s of confirmed condition.
    /// PRODUCTION: 3 cycles × 30 s = 90 s of confirmed condition.
    pub scale_up_hysteresis_cycles: u32,

    /// Number of consecutive snapshot cycles the drain gate must remain open
    /// before a drain is initiated.  Independent of `scale_up_hysteresis_cycles` so
    /// the drain can be tuned more or less conservatively than the scale-up.
    ///
    /// **[time-sensitive]** — each cycle is `snapshot_interval` seconds.
    /// TEST default : 3 cycles × 3 s  = 9 s
    /// PRODUCTION   : 3 cycles × 30 s = 90 s
    pub drain_hysteresis_cycles: u32,

    /// Minimum seconds between two consecutive scale-up actions.
    ///
    /// **[time-sensitive]**
    /// TEST default : 12 s
    /// PRODUCTION   : 120 s (AWS needs 2–3 min to bring a new backend InService)
    pub scale_up_cooldown_secs: u64,

    /// Minimum seconds between two consecutive drain initiations.
    ///
    /// **[time-sensitive]**
    /// TEST default : 30 s
    /// PRODUCTION   : 300 s
    pub drain_cooldown_secs: u64,

    /// Maximum number of backends allowed in `Draining` state simultaneously.
    /// Default: 2.
    pub max_concurrent_draining: u32,
}

// ── Bandwidth snapshot ────────────────────────────────────────────────────────

/// A single point-in-time reading of the [`BW_IFACE`] RX byte counter.
#[derive(Debug, Clone)]
pub struct IfaceSnapshot {
    /// Cumulative RX bytes for [`BW_IFACE`] as reported by `/proc/net/dev`.
    pub rx_bytes: u64,
    /// Wall-clock timestamp of this reading.
    pub at: Instant,
}

// ── Persistent state (survives across cycles) ─────────────────────────────────

/// Mutable scaler state that must live across `run()` calls.
///
/// Initialise once with `ScalerState::default()` before the main loop.
#[derive(Debug, Default)]
pub struct ScalerState {
    /// Consecutive cycles where Trigger 1 (slope) or Trigger 2 (bandwidth)
    /// was satisfied.  Incremented by 1 per cycle regardless of whether one
    /// or both triggers fired simultaneously (shared counter).
    /// Reset to 0 when both conditions clear or when a scale-up action is
    /// successfully initiated.
    /// Trigger 3 (hard ceiling) bypasses this counter entirely.
    pub scale_up_cycles: u32,

    /// Consecutive cycles where the drain condition was satisfied for the
    /// current drain candidate.
    pub drain_cycles: u32,

    /// The drain candidate IP being tracked across hysteresis cycles.
    /// Reset when the cheapest backend changes between cycles.
    pub drain_candidate: Option<Ipv4Addr>,

    /// Wall time of the last successful scale-up call.
    pub last_scale_up: Option<Instant>,

    /// Wall time of the last drain initiation.
    pub last_drain: Option<Instant>,

    /// Rolling window of `(timestamp, avg_sessions)` pairs used to compute
    /// the smoothed connection-rate slope.  Capped at 7 entries (the most
    /// recent 7 snapshot cycles), providing up to 6 usable lag differences.
    ///
    /// Layout: `front` = oldest, `back` = most recent past cycle.
    /// The current cycle's sample is pushed **after** the slope is computed.
    pub slope_samples: VecDeque<(Instant, u32)>,

    /// Previous `/proc/net/dev` RX byte-counter snapshot for [`BW_IFACE`].
    /// `None` on the first cycle; from the second cycle onward it holds the
    /// reading from the immediately preceding cycle.
    pub bw_prev: Option<IfaceSnapshot>,

    /// EMA-smoothed ingress bandwidth in bytes/s for [`BW_IFACE`].
    /// Updated every cycle using [`BW_EMA_ALPHA`].  Starts at 0.0.
    pub bw_smoothed: f64,

    /// Raw (unsmoothed) bytes/s from the most recent `/proc/net/dev` delta.
    /// Zero until the second cycle (first cycle has no previous sample to
    /// diff against) and zero whenever the counter goes backwards.
    /// Exposed as a Prometheus gauge so operators can compare it against
    /// `bw_smoothed` to see the EMA damping effect.
    pub bw_raw_bps: u64,
}

// ── IPVS → session normalisation ─────────────────────────────────────────────

/// Convert an IPVS active-connection count to an approximate FTP session count.
///
/// Each FTP transfer uses **two** TCP connections tracked by IPVS:
///   1. The control channel (port 21) — present for the full session lifetime.
///   2. The passive data channel (ephemeral port) — present during the transfer.
///
/// Note: Active FTP data connections are not visible in IPVS (only the control
/// channel registers); this function only normalises the IPVS count and is not
/// expected to produce exact session counts for Active FTP traffic.
///
/// Dividing by two converts the IPVS metric to the session count that all
/// configured thresholds are expressed in.
#[inline]
fn sessions_from_ipvs(connections: u32) -> u32 {
    connections / 2
}

// ── Slope helper ──────────────────────────────────────────────────────────────

/// Compute a smoothed first derivative of the session count in
/// **connections per minute**.
///
/// The slope is the mean of up to six per-lag finite differences.  Lag `k`
/// compares the current sessions against the sample `k` positions back in
/// `samples` (i.e. `k` snapshot cycles ago):
///
/// ```text
/// slope_k = (now_sessions − samples[len−k].sessions) / Δt_k_secs × 60
/// ```
///
/// Averaging over multiple lags smooths tick-to-tick jitter from staggered
/// FTP completions without requiring a separate EMA pass on top.
///
/// ## Timing (TEST vs PRODUCTION)
///
/// | Environment | Snapshot interval | Maximum window covered |
/// |-------------|-------------------|------------------------|
/// | TEST        | 3 s               | 6 × 3 s  = 18 s        |
/// | PRODUCTION  | 30 s              | 6 × 30 s = 180 s       |
///
/// Returns `None` when `samples` is empty (insufficient history).
fn compute_smoothed_slope(
    samples:      &VecDeque<(Instant, u32)>,
    now_sessions: u32,
    now:          Instant,
) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }

    let max_lags = samples.len().min(6);
    let mut total = 0.0_f64;
    let mut count = 0_usize;

    for lag in 1..=max_lags {
        // lag=1 → samples[len-1] (most recent past cycle)
        // lag=k → samples[len-k]
        let (t_past, sessions_past) = samples[samples.len() - lag];

        let dt_secs = match now.checked_duration_since(t_past) {
            Some(d) => d.as_secs_f64(),
            None    => continue, // clock went backwards — skip this lag
        };
        if dt_secs < 0.1 {
            continue; // guard against identical timestamps on the first cycle
        }

        let slope_conn_per_min =
            (now_sessions as f64 - sessions_past as f64) / dt_secs * 60.0;
        total += slope_conn_per_min;
        count += 1;
    }

    if count == 0 { None } else { Some(total / count as f64) }
}

// ── Bandwidth helpers ─────────────────────────────────────────────────────────

/// Extract the RX byte counter for `iface` from the raw text of
/// `/proc/net/dev`.  Returns `None` when the interface is not present.
///
/// `/proc/net/dev` line format (after the interface name and colon):
/// ```text
/// rx_bytes rx_pkts rx_errs rx_drop rx_fifo rx_frame rx_compressed rx_mcast \
/// tx_bytes tx_pkts …
/// ```
fn parse_iface_rx_bytes(content: &str, iface: &str) -> Option<u64> {
    for line in content.lines() {
        let trimmed = line.trim_start();
        let rest = match trimmed
            .strip_prefix(iface)
            .and_then(|s| s.strip_prefix(':'))
        {
            Some(r) => r,
            None    => continue,
        };
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

/// Read the current RX byte counter for [`BW_IFACE`] from `/proc/net/dev`.
async fn read_iface_snapshot() -> Result<IfaceSnapshot> {
    let content = tokio::fs::read_to_string("/proc/net/dev")
        .await
        .context("failed to read /proc/net/dev")?;
    // Snapshot the time immediately after the file read so Δt is as accurate
    // as possible regardless of parse time.
    let at = Instant::now();
    let rx_bytes = parse_iface_rx_bytes(&content, BW_IFACE)
        .ok_or_else(|| anyhow::anyhow!(
            "interface '{BW_IFACE}' not found in /proc/net/dev"
        ))?;
    Ok(IfaceSnapshot { rx_bytes, at })
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
    // have IPVS data participate in scale decisions.
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
        active_backends     = active_count,
        total_sessions,
        avg_sessions,
        hard_conn_threshold = config.hard_conn_threshold,
        drain_threshold     = config.drain_threshold,
        "scaler: session snapshot (IPVS connections / 2)"
    );

    // Run both evaluations independently.
    evaluate_scale_up(
        snapshot,
        config,
        state,
        avg_sessions,
        active_count,
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
    snapshot:     &SystemSnapshot,
    config:       &ScaleConfig,
    state:        &mut ScalerState,
    avg_sessions: u32,
    active_count: u32,
    asg_name:     &str,
    region:       &str,
    creds:        &AwsCredentials,
    dry_run:      bool,
) {
    let Some(group) = &snapshot.asg_group else {
        debug!("ASG group info not available — skipping scale-up check");
        return;
    };

    let now = Instant::now();

    // ── Trigger 2: per-backend bandwidth from eth0 RX ─────────────────────────
    //
    // Read /proc/net/dev RX bytes for eth0 (client-facing interface).  In NAT
    // mode eth0 RX captures ALL client upload traffic — both Active and Passive
    // FTP — regardless of how many IPVS connections are registered.  Dividing
    // by active_count gives an estimate of the ingress load on each backend.
    //
    // On the first cycle bw_prev is None, so no delta is computed and
    // bw_smoothed stays at 0.0.  The EMA warms up from the second cycle.
    match read_iface_snapshot().await {
        Ok(current) => {
            if let Some(ref prev) = state.bw_prev {
                if let Some(dt) = current.at.checked_duration_since(prev.at) {
                    let dt_secs = dt.as_secs_f64();
                    if dt_secs > 0.1 {
                        if current.rx_bytes >= prev.rx_bytes {
                            let raw_bps =
                                (current.rx_bytes - prev.rx_bytes) as f64 / dt_secs;
                            state.bw_raw_bps = raw_bps as u64;
                            state.bw_smoothed = BW_EMA_ALPHA * raw_bps
                                + (1.0 - BW_EMA_ALPHA) * state.bw_smoothed;
                        } else {
                            // Counter decreased: the network interface was reset or the
                            // system rebooted.  On 64-bit Linux /proc/net/dev uses u64
                            // counters; genuine overflow would require ~18 exabytes of
                            // traffic (~2 335 years at 2 Gbit/s), so a backwards jump
                            // virtually always means an interface or kernel reset.
                            //
                            // Skip the EMA update entirely to avoid injecting a false
                            // 0 bps spike that would take several cycles to decay.
                            // bw_prev is still advanced to `current` so the next cycle
                            // produces a clean Δ from the new counter baseline.
                            warn!(
                                prev_rx = prev.rx_bytes,
                                curr_rx = current.rx_bytes,
                                "eth0 RX counter decreased — interface reset or reboot; \
                                 skipping bandwidth EMA update this cycle"
                            );
                        }
                    }
                }
            }
            state.bw_prev = Some(current);
        }
        Err(e) => warn!("bandwidth read failed (Trigger 2 disarmed this cycle): {e:#}"),
    }

    // ── Trigger 1: smoothed connection slope ──────────────────────────────────
    //
    // Compute from historical samples BEFORE pushing the current reading so
    // that lag 1 correctly refers to the previous cycle.
    let slope = compute_smoothed_slope(&state.slope_samples, avg_sessions, now);

    state.slope_samples.push_back((now, avg_sessions));
    if state.slope_samples.len() > 7 {
        state.slope_samples.pop_front();
    }

    // ── Evaluate the three triggers ───────────────────────────────────────────

    // Trigger 3: hard connection ceiling — immediate, bypasses hysteresis.
    let hard_trigger = avg_sessions >= config.hard_conn_threshold;

    // Trigger 1: slope-based.
    let slope_trigger = avg_sessions >= config.slope_low_floor
        && slope.map_or(false, |s| s >= config.slope_threshold_conn_per_min);

    // Trigger 2: per-backend bandwidth.
    let bw_per_backend: Option<u64> = if active_count > 0 && state.bw_smoothed > 0.0 {
        Some(state.bw_smoothed as u64 / active_count as u64)
    } else {
        None
    };
    let bw_trigger = bw_per_backend.map_or(false, |bw| {
        bw >= config.bw_threshold_bps_per_backend && avg_sessions >= config.slope_low_floor
    });

    let slope_display = slope
        .map(|s| format!("{s:.1}"))
        .unwrap_or_else(|| "n/a".into());

    info!(
        avg_sessions,
        hard_threshold      = config.hard_conn_threshold,
        slope_conn_per_min  = %slope_display,
        slope_threshold     = config.slope_threshold_conn_per_min,
        bw_per_backend_mbit = bw_per_backend.map(|b| b * 8 / 1_000_000).unwrap_or(0),
        bw_threshold_mbit   = config.bw_threshold_bps_per_backend * 8 / 1_000_000,
        trig_hard           = hard_trigger,
        trig_slope          = slope_trigger,
        trig_bw             = bw_trigger,
        "scaler: trigger evaluation"
    );

    // ── Apply triggers to the hysteresis counter ──────────────────────────────

    if hard_trigger {
        // Bypass hysteresis: force the counter to the threshold so the check
        // below passes immediately and the cooldown/capacity guards run.
        info!(
            avg_sessions,
            threshold = config.hard_conn_threshold,
            "hard connection ceiling reached — bypassing hysteresis counter"
        );
        state.scale_up_cycles = config.scale_up_hysteresis_cycles;
    } else if slope_trigger || bw_trigger {
        state.scale_up_cycles += 1;
        info!(
            avg_sessions,
            cycles      = state.scale_up_cycles,
            required    = config.scale_up_hysteresis_cycles,
            slope_armed = slope_trigger,
            bw_armed    = bw_trigger,
            "scale-up condition met ({}/{})",
            state.scale_up_cycles,
            config.scale_up_hysteresis_cycles,
        );
    } else {
        if state.scale_up_cycles > 0 {
            debug!(avg_sessions, "scale-up condition cleared — resetting hysteresis counter");
        }
        state.scale_up_cycles = 0;
        return;
    }

    if state.scale_up_cycles < config.scale_up_hysteresis_cycles {
        return; // condition holds but hysteresis window not yet satisfied
    }

    // ── Guards ────────────────────────────────────────────────────────────────
    if group.desired_capacity >= group.max_size {
        warn!(
            desired = group.desired_capacity,
            max     = group.max_size,
            avg_sessions,
            "scale-up condition met but ASG is at maximum capacity — cannot scale up further"
        );
        // Do NOT reset: if max_size is raised the next cycle acts immediately.
        return;
    }

    if let Some(last) = state.last_scale_up {
        let elapsed = last.elapsed().as_secs();
        if elapsed < config.scale_up_cooldown_secs {
            info!(
                elapsed_secs  = elapsed,
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

    let trigger_reason = match (hard_trigger, slope_trigger, bw_trigger) {
        (true,  _,    _)    => "hard-ceiling",
        (false, true, true) => "slope+bandwidth",
        (false, true, _)    => "slope",
        (false, _, true)    => "bandwidth",
        _                   => "unknown",
    };

    if dry_run {
        info!(
            "[DRY-RUN] scale-up: SetDesiredCapacity({asg_name}, {new_desired})  \
             (trigger={trigger_reason}  avg={avg_sessions} sessions)"
        );
    } else {
        info!(
            asg_name,
            new_desired,
            avg_sessions,
            trigger = trigger_reason,
            "scaling up: SetDesiredCapacity → {new_desired}"
        );
        if let Err(e) = aerocore::asg::set_desired(region, asg_name, new_desired, creds).await {
            warn!("scale-up SetDesiredCapacity failed: {e:#}");
            return;
        }
    }

    state.scale_up_cycles = 0;
    state.last_scale_up = Some(Instant::now());
}

// ── Drain evaluation ─────────────────────────────────────────────────────────

async fn evaluate_drain(
    snapshot:    &SystemSnapshot,
    active:      &[&BackendStatus],
    config:      &ScaleConfig,
    state:       &mut ScalerState,
    weights_dir: &str,
    dry_run:     bool,
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

    let draining_count = snapshot
        .backends
        .iter()
        .filter(|b| b.weight_state == BackendState::Draining)
        .count() as u32;

    if draining_count >= config.max_concurrent_draining {
        debug!(
            draining_count,
            max = config.max_concurrent_draining,
            "at max concurrent draining backends -- skipping"
        );
        return;
    }

    let mut sorted: Vec<&BackendStatus> = active.to_vec();
    sorted.sort_by_key(|b| sessions_from_ipvs(b.ipvs.as_ref().unwrap().active_connections));

    // ── Gate: average sessions must be below drain_threshold ─────────────────

    let total_sessions: u32 = active
        .iter()
        .map(|b| sessions_from_ipvs(b.ipvs.as_ref().unwrap().active_connections))
        .sum();
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

    // ── Hysteresis ────────────────────────────────────────────────────────────

    state.drain_cycles += 1;
    info!(
        avg_sessions,
        threshold    = config.drain_threshold,
        active_count,
        cycles   = state.drain_cycles,
        required = config.drain_hysteresis_cycles,
        "drain gate open -- average below threshold ({}/{})",
        state.drain_cycles,
        config.drain_hysteresis_cycles,
    );

    if state.drain_cycles < config.drain_hysteresis_cycles {
        return;
    }

    // ── Cooldown ──────────────────────────────────────────────────────────────

    if let Some(last) = state.last_drain {
        let elapsed = last.elapsed().as_secs();
        if elapsed < config.drain_cooldown_secs {
            info!(
                elapsed_secs  = elapsed,
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
        (least, "0-session backend -- free drain")
    } else if draining_count == 0 {
        (
            *sorted.last().unwrap(),
            "all backends loaded -- draining most loaded to break persistence",
        )
    } else {
        debug!(draining_count, "drain in progress, no 0-session backend available -- waiting");
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_iface_rx_bytes ──────────────────────────────────────────────────

    const PROC_NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 5889822   51972    0    0    0     0          0         0  5889822   51972    0    0    0     0       0          0
  eth0: 490569799018 344109988    0  186    0     0          0         0 3104194205 44613177    0    0    0     0       0          0
  eth1: 2390126013 44064985    0    0    0     0          0         0 495291134072 343572253    0    0    0     0       0          0
  eth2:  129662    4090    0    0    0     0          0         0 4874086362 6131997    0    0    0     0       0          0
vxlan0:    2024      35    0    0    0     0          0         0 4483060340 5993428    0    0    0     0       0          0
";

    #[test]
    fn parses_eth0_rx_bytes() {
        assert_eq!(
            parse_iface_rx_bytes(PROC_NET_DEV, "eth0"),
            Some(490_569_799_018)
        );
    }

    #[test]
    fn parses_eth1_rx_bytes() {
        assert_eq!(
            parse_iface_rx_bytes(PROC_NET_DEV, "eth1"),
            Some(2_390_126_013)
        );
    }

    #[test]
    fn parses_lo_rx_bytes() {
        assert_eq!(
            parse_iface_rx_bytes(PROC_NET_DEV, "lo"),
            Some(5_889_822)
        );
    }

    #[test]
    fn parses_vxlan0_rx_bytes() {
        assert_eq!(
            parse_iface_rx_bytes(PROC_NET_DEV, "vxlan0"),
            Some(2_024)
        );
    }

    #[test]
    fn returns_none_for_missing_iface() {
        assert_eq!(parse_iface_rx_bytes(PROC_NET_DEV, "eth9"), None);
    }

    // ── compute_smoothed_slope ────────────────────────────────────────────────

    #[test]
    fn slope_none_with_empty_buffer() {
        let buf = VecDeque::new();
        assert!(compute_smoothed_slope(&buf, 500, Instant::now()).is_none());
    }

    #[test]
    fn slope_single_sample_positive_ramp() {
        let mut buf = VecDeque::new();
        let t0 = Instant::now();
        // Simulate a 3-second-old sample at 400 sessions
        let past = t0.checked_sub(std::time::Duration::from_secs(3)).unwrap();
        buf.push_back((past, 400_u32));

        // Now at 415 sessions → slope ≈ (15 / 3) × 60 = 300 conn/min
        let slope = compute_smoothed_slope(&buf, 415, t0).unwrap();
        assert!((slope - 300.0).abs() < 5.0, "slope={slope}");
    }

    #[test]
    fn slope_six_lags_averaged() {
        // All lags report the same rate: 10 conn / 3 s = 200 conn/min.
        // The mean of six identical values is still 200 conn/min.
        let t0 = Instant::now();
        let mut buf = VecDeque::new();
        for k in (1_u64..=6).rev() {
            let t = t0.checked_sub(std::time::Duration::from_secs(k * 3)).unwrap();
            // sessions grow at 10/3s: sessions_at_t = 600 - k*10
            let sessions = (600 - k * 10) as u32;
            buf.push_back((t, sessions));
        }
        let slope = compute_smoothed_slope(&buf, 600, t0).unwrap();
        assert!((slope - 200.0).abs() < 2.0, "slope={slope}");
    }

    #[test]
    fn slope_flat_load_is_zero() {
        let t0 = Instant::now();
        let mut buf = VecDeque::new();
        for k in 1_u64..=6 {
            let t = t0.checked_sub(std::time::Duration::from_secs(k * 3)).unwrap();
            buf.push_back((t, 500_u32)); // constant sessions
        }
        let slope = compute_smoothed_slope(&buf, 500, t0).unwrap();
        assert!(slope.abs() < 0.01, "slope={slope}");
    }
}
