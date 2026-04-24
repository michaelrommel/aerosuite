# Control Theory Ideas for aeroscale Auto-Scaling

## Context

The production backend being replaced has the following load profile:

- **Steepest ramp**: 700 → 1700 connections over 15 minutes (~67 connections/minute)
- **Scale-out reaction time**: 32–40 seconds (from aeroscale trigger to backend ready to accept load)
- **Per-node passive port ceiling**: 1 500 simultaneous FTP data connections (passive port range 41000–42499)
- **Current trigger**: average connection count (lagging indicator)

The goal is to find a trigger algorithm that fires early enough to have a new backend fully registered and accepting load *before* any existing node approaches its 1 500-connection ceiling.

---

## Why Average Connection Count Lags

Average connection count is a *lagging* indicator.  By the time the rolling average crosses a threshold, the ramp is already well underway.

With a 15-minute ramp and a 32–40 second reaction time:

```
ramp rate       ≈ 67 connections / minute
reaction time   ≈ 40 seconds
connections added during reaction ≈ 67 × (40/60) ≈ 45
```

If the scale-out trigger fires when the average reaches, say, 1 000 connections, the new backend is not ready until the cluster is already at ~1 045.  Any threshold must therefore be set conservatively low, which risks unnecessary scale-outs during short-lived spikes.

---

## The Case for Rate-of-Change (First Derivative)

Triggering on the *slope* of the connection count fires early — it detects the ramp starting, not the ramp having already happened.

**Advantage**: on a sustained 15-minute ramp the slope is detectable within the first 1–2 minutes, giving 10+ minutes of lead time.

**Risk**: derivatives amplify noise.  A momentary burst of quick xs-file completions can spike the instantaneous slope without representing a sustained ramp.

**Mitigation**: smooth the derivative with a moving average over a 2–3 minute window.  This still provides ample lead time on a 15-minute ramp while filtering out transient spikes.

```
slope_smoothed(t) = mean( connections(t) - connections(t - Δt)
                          for Δt in [30s, 60s, 90s, 120s, 150s, 180s] )
```

---

## A Combined Trigger (Proposed Starting Point)

Using the derivative alone can fire during a brief ramp that self-resolves.  A compound condition is more robust:

```
trigger scale-out if:

    connections > LOW_FLOOR              # not just noise at idle
    AND slope_180s > SLOPE_THRESHOLD     # ramp is actively and sustainably steepening
    
    OR connections > HIGH_WATERMARK      # hard ceiling backstop regardless of slope
```

| Parameter | Suggested starting value | Rationale |
|---|---|---|
| `LOW_FLOOR` | 500 connections | Avoid reacting to small transient bursts |
| `SLOPE_THRESHOLD` | ~25 conn/min (smoothed) | ~37 % of peak ramp rate; fires ~5 min into the 15-min ramp |
| `HIGH_WATERMARK` | 1 200 connections | ~80 % of the 1 500 passive-port ceiling; absolute backstop |

These are starting values to be calibrated against real test runs — see the Calibration section below.

---

## What aerosuite Now Provides

The tooling built during this project gives a strong foundation for calibration:

### Structured load data
- **NDJSON result files** contain per-transfer start/end timestamps, bytes transferred, bucket, slice index, and success/error status for every connection across all agents.
- **`current_bandwidth_bps`** in `GlobalStats` (WebSocket broadcast every 3 seconds) is already computed from both completed and in-flight transfer bytes, giving a real-time bandwidth signal.
- **`active_connections`** per agent and fleet-wide are broadcast on every tick.

### Proposed additions to support slope-based triggering

1. **`connection_slope_per_min`** field in `GlobalStats` (or a dedicated `/metrics` endpoint) — a 3-minute smoothed first derivative of `active_connections`, computed inside `DeltaEngine` and broadcast with every dashboard update.  aeroscale can poll `/status` or subscribe to the WebSocket to consume this directly.

2. **`slope_history`** in the NDJSON result file or a separate time-series log — records `(timestamp_ms, active_connections, slope)` at every delta tick so post-run analysis can determine where a slope-based trigger *would* have fired and compare it to where an average-based trigger fires.

---

## Calibration Process

Once enough representative test runs are available from aerosuite:

1. **Extract the connection time series** from the NDJSON result files (or from logged dashboard ticks).
2. **Compute the retrospective slope** at each point in time.
3. **Mark the moment the node approached saturation** (when passive-port errors first appeared in the result file, or when the connection count approached 1 200+).
4. **Find the earliest point** where the slope-based trigger *would* have fired, and verify that `reaction_time (40 s) + provisioning_buffer` fits within the remaining lead time.
5. **Adjust `SLOPE_THRESHOLD` and `LOW_FLOOR`** until the trigger fires at least 60–90 seconds before the saturation point across all representative runs.

The goal is a trigger that fires no more than once per genuine ramp event, and never fires on transient spikes that self-resolve within a single time slice.

---

## Open Questions

- **Scale-in hysteresis**: once a second node is live, how long to wait before deregistering it after the load drops?  A mirror of the slope condition (negative slope sustained for N minutes) or a simple low-watermark timer?
- **Multi-node slope aggregation**: when two backends are already running, should the slope be computed per-node or from the fleet-wide connection total?  Per-node is safer (catches uneven distribution); fleet-wide is simpler.
- **Prediction horizon**: a simple first-derivative trigger is reactive (fires when the ramp *is* happening).  A second-order model (acceleration of the connection count) could fire even earlier — worth exploring once the first-derivative baseline is established.
- **Interaction with aeroscale's current cooldown logic**: does aeroscale already have a cooldown period after a scale-out to prevent oscillation?  The slope-based trigger must respect this or it could fire repeatedly during a sustained ramp.

---

## References within the Codebase

| File | Relevance |
|---|---|
| `aerocoach/src/state/delta.rs` | `DeltaEngine::compute` — where `current_bandwidth_bps` and `active_connections` are calculated; the natural home for a slope metric |
| `aerocoach/src/state/mod.rs` | `GlobalStats` struct — add `connection_slope_per_min` here |
| `aeroscale/src/scaler.rs` | Current scale-out trigger logic |
| `aeroscale/src/metrics/` | Metrics collection — slope could be exposed as a Prometheus gauge |
| `aerocoach/scripts/` | Load plan files — use these to generate representative ramp scenarios for calibration |
