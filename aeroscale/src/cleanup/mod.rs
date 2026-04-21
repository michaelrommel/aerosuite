//! Cleanup actions derived from a `SystemSnapshot`.
//!
//! All three sections are driven purely by snapshot data — no additional I/O
//! happens before the decision is logged.  When `dry_run` is true every
//! action is logged at INFO level but no write is performed.
//!
//! ## Section 2.1 — Active leases
//! For each lease, cross-reference the ASG and backend state, then act.
//! `asg_ids` is built from **InService-only** instances — `Terminating` or
//! `Pending` instances are excluded so their slots are released promptly.
//!
//! ## Section 2.2 — ASG instances without leases
//! Any `InService` instance that holds no lease is an *orphan* — it has either
//! just booted (and hasn't claimed a slot yet) or its registration failed.
//! A per-instance grace period is observed before taking action so that a
//! freshly launched instance has time to run aeroslot and claim a slot.
//! When the grace period expires the instance is terminated **without**
//! decrementing the ASG desired capacity so that a replacement is launched
//! automatically.
//!
//! ## Section 2.3 — Backends without leases
//! Weight files that have no matching lease indicate a crashed backend.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Instant;

use anyhow::{Context, Result};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use tracing::{debug, error, info, warn};

use aerocore::{asg, redis_pool::{key_owner, now_ms, KEY_AVAILABLE, KEY_LEASES}, AwsCredentials};
use crate::snapshot::{BackendState, SlotLease, SystemSnapshot};

// ── Weight file constants ─────────────────────────────────────────────────────

pub const WEIGHT_ACTIVE:   &str = "0";
pub const WEIGHT_DRAINING: &str = "-1";
pub const WEIGHT_DISABLED: &str = "-2147483648";

// ── Persistent cleanup state ──────────────────────────────────────────────────

/// Mutable cleanup state that must survive between `run()` calls.
///
/// Initialise once with `CleanupState::default()` before the main loop.
#[derive(Debug, Default)]
pub struct CleanupState {
    /// The first time each `InService` instance was observed without a slot
    /// lease.  Used to implement the orphan grace period (§2.2).
    /// Entries are evicted when the instance acquires a lease or leaves the
    /// `InService` lifecycle state.
    pub orphan_first_seen: HashMap<String, Instant>,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run all three cleanup sections against `snapshot`.
///
/// `state` carries per-cycle observations (orphan grace-period timers) across
/// calls and must not be recreated each cycle.
///
/// `orphan_grace_secs` — how long an `InService` instance is allowed to exist
/// without a slot lease before it is terminated.  The termination always uses
/// `decrement=false` so the ASG replaces the instance automatically.
pub async fn run(
    snapshot:                 &SystemSnapshot,
    weights_dir:              &str,
    region:                   &str,
    creds:                    &AwsCredentials,
    redis_con:                &mut MultiplexedConnection,
    dry_run:                  bool,
    is_master:                bool,
    term_decrements_capacity: bool,
    state:                    &mut CleanupState,
    orphan_grace_secs:        u64,
) -> Result<()> {
    if !is_master {
        debug!("backup mode — skipping cleanup pass");
        return Ok(());
    }
    info!("── cleanup pass ──────────────────────────────────────────────────────────");

    section_21_active_leases(snapshot, weights_dir, region, creds, redis_con, dry_run, term_decrements_capacity).await;
    section_22_orphaned_asg_instances(snapshot, state, region, creds, dry_run, orphan_grace_secs).await;
    section_23_backends_without_leases(snapshot, weights_dir, dry_run).await;

    info!("── cleanup pass done ─────────────────────────────────────────────────────");
    Ok(())
}

// ── 2.1 — Active leases ───────────────────────────────────────────────────────

async fn section_21_active_leases(
    snapshot:                &SystemSnapshot,
    weights_dir:             &str,
    region:                  &str,
    creds:                   &AwsCredentials,
    redis_con:               &mut MultiplexedConnection,
    dry_run:                 bool,
    term_decrements_capacity: bool,
) {
    // Only InService instances are considered valid lease owners.
    // Terminating instances have their slots released (same as if they left
    // the ASG), preventing a leased slot from being held open while AWS
    // slowly completes the termination.
    let asg_ids: std::collections::HashSet<&str> =
        snapshot.asg.iter()
            .filter(|i| i.is_in_service())
            .map(|i| i.instance_id.as_str())
            .collect();

    // ── Sanity guard: trust ASG data only when it looks plausible ─────────────
    //
    // If the ASG query failed (DNS glitch, transient API error, …) the
    // snapshot arrives with asg_group=None and zero instances.  Running
    // cleanup against that empty view would mark every lease owner as
    // "no longer in the ASG", release all 20 slots, and write
    // -2147483648 to every weight file — a total outage from a single
    // dropped DNS packet, as observed in production on 2026-04-21.
    //
    // Two conditions independently trigger the guard:
    //
    //  1. asg_group is None  — the API call returned an error; the instance
    //     list is definitively unreliable.
    //
    //  2. Zero InService instances but active leases exist  — implausible in
    //     normal operation (there is always at least one InService backend).
    //     Catches the case where the query "succeeded" but returned an empty
    //     or all-Terminating list due to a partial API response.
    //
    // In both cases we log at ERROR level (this needs operator attention if
    // it persists) and skip the section entirely.  The next cycle will retry
    // with a fresh ASG query.
    if snapshot.asg_group.is_none() {
        error!(
            leases = snapshot.leases.len(),
            "ASG query failed this cycle (asg_group is None) — \
             skipping section 2.1 to avoid releasing leases against stale data. \
             Will retry next cycle."
        );
        return;
    }
    if asg_ids.is_empty() && !snapshot.leases.is_empty() {
        error!(
            leases = snapshot.leases.len(),
            "ASG returned 0 InService instances but {} active lease(s) exist — \
             implausible result (DNS glitch? transient API error?). \
             Skipping section 2.1. Will retry next cycle.",
            snapshot.leases.len()
        );
        return;
    }

    for lease in &snapshot.leases {
        let result = handle_lease(
            lease, &asg_ids, snapshot, weights_dir, region, creds, redis_con, dry_run, term_decrements_capacity,
        )
        .await;

        if let Err(e) = result {
            error!(
                slot = lease.slot,
                owner = %lease.owner_instance_id,
                "lease cleanup error: {e:#}"
            );
        }
    }
}

async fn handle_lease(
    lease:                   &SlotLease,
    asg_ids:                 &std::collections::HashSet<&str>,
    snapshot:                &SystemSnapshot,
    weights_dir:             &str,
    region:                  &str,
    creds:                   &AwsCredentials,
    redis_con:               &mut MultiplexedConnection,
    dry_run:                 bool,
    term_decrements_capacity: bool,
) -> Result<()> {
    let slot     = lease.slot;
    let owner    = &lease.owner_instance_id;
    let in_asg   = asg_ids.contains(owner.as_str());

    // Find the backend for this lease (may be None if EC2 join failed).
    let backend = snapshot.backends.iter()
        .find(|b| b.lease.as_ref().map(|l| l.slot) == Some(slot));

    // ── Owner no longer in ASG ─────────────────────────────────────────────
    if !in_asg {
        warn!(
            slot, owner = %owner,
            "lease owner is no longer in the ASG — releasing slot and disabling backend"
        );
        release_slot(slot, owner, redis_con, dry_run).await?;
        if let Some(b) = backend {
            write_weight(weights_dir, b.ip, WEIGHT_DISABLED, dry_run).await?;
        } else {
            warn!(slot, "cannot disable backend: IP not resolved (EC2 join failed)");
        }
        return Ok(());
    }

    // ── Owner is alive — inspect weight state ──────────────────────────────
    let Some(b) = backend else {
        // EC2 lookup didn't resolve an IP — nothing to act on yet.
        info!(slot, owner = %owner, "backend IP not yet resolved — skipping weight check");
        return Ok(());
    };

    match &b.weight_state {
        BackendState::Active => {
            // Normal: active backend with a live lease.
        }

        BackendState::Draining => {
            let active_conn = b.ipvs.as_ref().map(|i| i.active_connections).unwrap_or(0);
            if active_conn == 0 {
                // Guard: refuse to terminate if doing so would breach the ASG
                // min-size constraint (AWS rejects the call with a 400 error).
                if let Some(g) = &snapshot.asg_group {
                    if g.would_violate_min() {
                        warn!(
                            slot, owner = %owner, ip = %b.ip,
                            desired = g.desired_capacity, min = g.min_size,
                            "draining backend has 0 connections but desired=min — \
                             cannot terminate without violating the ASG min-size \
                             constraint. Lower the ASG min size or scale up first."
                        );
                        return Ok(());
                    }
                }

                info!(
                    slot, owner = %owner, ip = %b.ip,
                    "draining backend has 0 active connections — disabling and terminating"
                );
                write_weight(weights_dir, b.ip, WEIGHT_DISABLED, dry_run).await?;
                terminate_instance(owner, region, creds, dry_run, term_decrements_capacity).await?;
            } else {
                info!(
                    slot, owner = %owner, ip = %b.ip, active_conn,
                    "draining backend still has active connections — waiting"
                );
            }
        }

        BackendState::Disabled => {
            if lease.is_expired() {
                // Expired lease on a disabled backend — this is the "failed
                // registration" scenario: the instance tried to claim a slot,
                // the init script failed, and the lease TTL has since run out.
                // The same instance may have successfully claimed a different
                // slot on a retry.  Leave the weight file disabled and let
                // aeroslot clean up the stale lease lazily on the next claim.
                info!(
                    slot, owner = %owner, ip = %b.ip,
                    "disabled backend has expired lease — leaving disabled \
                     (stale lease will be reclaimed by aeroslot on next claim)"
                );
            } else {
                // Live lease but disabled weight file — most likely a missed
                // 'claim' message on the asg-change channel.  Re-enable so
                // the backend can receive traffic.
                warn!(
                    slot, owner = %owner, ip = %b.ip,
                    "backend is disabled but lease is active — re-enabling (missed message recovery)"
                );
                write_weight(weights_dir, b.ip, WEIGHT_ACTIVE, dry_run).await?;
            }
        }

        BackendState::Unknown(v) => {
            warn!(
                slot, owner = %owner, ip = %b.ip, value = %v,
                "backend has unknown weight value — leaving unchanged"
            );
        }
    }

    Ok(())
}

// ── 2.2 — Orphaned ASG instances ─────────────────────────────────────────────

async fn section_22_orphaned_asg_instances(
    snapshot:          &SystemSnapshot,
    state:             &mut CleanupState,
    region:            &str,
    creds:             &AwsCredentials,
    dry_run:           bool,
    orphan_grace_secs: u64,
) {
    let leased_owners: std::collections::HashSet<&str> =
        snapshot.leases.iter().map(|l| l.owner_instance_id.as_str()).collect();

    // Track which instance IDs are still orphaned this cycle so we can evict
    // instances that have since been leased or left InService from the map.
    let mut still_orphaned: std::collections::HashSet<String> = std::collections::HashSet::new();

    for inst in snapshot.asg.iter().filter(|i| i.is_in_service()) {
        if leased_owners.contains(inst.instance_id.as_str()) {
            // Instance has a lease this cycle — remove any stale grace entry.
            state.orphan_first_seen.remove(&inst.instance_id);
            continue;
        }

        // InService but no lease.
        let elapsed = match state.orphan_first_seen.get(&inst.instance_id) {
            Some(first_seen) => first_seen.elapsed().as_secs(),
            None => {
                // First observation — start the grace period clock.
                info!(
                    instance_id = %inst.instance_id,
                    grace_secs  = orphan_grace_secs,
                    "InService instance has no slot lease — starting grace period"
                );
                state.orphan_first_seen
                    .insert(inst.instance_id.clone(), Instant::now());
                still_orphaned.insert(inst.instance_id.clone());
                continue;  // give at least one full cycle before acting
            }
        };

        still_orphaned.insert(inst.instance_id.clone());

        if elapsed < orphan_grace_secs {
            info!(
                instance_id  = %inst.instance_id,
                elapsed_secs = elapsed,
                grace_secs   = orphan_grace_secs,
                "InService instance without lease — waiting ({elapsed}/{orphan_grace_secs}s)"
            );
        } else {
            // Grace period expired — this instance failed to register.
            // Terminate WITHOUT decrementing desired capacity so the ASG
            // immediately launches a replacement.
            error!(
                instance_id  = %inst.instance_id,
                elapsed_secs = elapsed,
                grace_secs   = orphan_grace_secs,
                "InService instance has no slot lease after grace period —                  possible registration failure; terminating                  (capacity NOT decremented — ASG will replace automatically)"
            );
            state.orphan_first_seen.remove(&inst.instance_id);
            still_orphaned.remove(&inst.instance_id);
            if let Err(e) = terminate_instance(
                &inst.instance_id, region, creds, dry_run,
                /*decrement=*/ false,
            ).await {
                error!(instance_id = %inst.instance_id, "terminate failed: {e:#}");
            }
        }
    }

    // Evict any instance that was previously orphaned but is no longer
    // InService (e.g. it transitioned to Terminating or Terminated).
    state.orphan_first_seen.retain(|id, _| still_orphaned.contains(id.as_str()));
}

// ── 2.3 — Backends without leases ────────────────────────────────────────────

async fn section_23_backends_without_leases(
    snapshot:    &SystemSnapshot,
    weights_dir: &str,
    dry_run:     bool,
) {
    for b in &snapshot.backends {
        if b.lease.is_some() {
            continue; // has a lease — handled by 2.1
        }

        match &b.weight_state {
            BackendState::Active => {
                warn!(
                    ip = %b.ip,
                    "active backend has no lease — backend or heartbeat likely crashed; disabling"
                );
                if let Err(e) = write_weight(weights_dir, b.ip, WEIGHT_DISABLED, dry_run).await {
                    error!(ip = %b.ip, "write_weight failed: {e:#}");
                }
            }

            BackendState::Draining => {
                info!(
                    ip = %b.ip,
                    "draining backend has no lease — crash during drain; disabling"
                );
                if let Err(e) = write_weight(weights_dir, b.ip, WEIGHT_DISABLED, dry_run).await {
                    error!(ip = %b.ip, "write_weight failed: {e:#}");
                }
            }

            BackendState::Disabled => {
                // Normal idle state — no lease expected.
            }

            BackendState::Unknown(v) => {
                warn!(ip = %b.ip, value = %v, "unknown weight value on unleaseed backend");
            }
        }
    }
}

// ── Action helpers ────────────────────────────────────────────────────────────

/// Write `value` to `<weights_dir>/backend-<ip>.weight`.
pub(crate) async fn write_weight(
    weights_dir: &str,
    ip:          Ipv4Addr,
    value:       &str,
    dry_run:     bool,
) -> Result<()> {
    let path = format!("{weights_dir}/backend-{ip}.weight");
    if dry_run {
        info!("[DRY-RUN] write '{value}' → {path}");
        return Ok(());
    }
    info!("write '{value}' → {path}");
    tokio::fs::write(&path, value)
        .await
        .with_context(|| format!("Cannot write weight file: {path}"))
}

/// Release a slot back to the free pool in Redis.
/// Mirrors the `aeroslot release` logic: DEL owner → ZREM leases → ZADD available.
async fn release_slot(
    slot:      u32,
    owner:     &str,
    con:       &mut MultiplexedConnection,
    dry_run:   bool,
) -> Result<()> {
    let slot_str = slot.to_string();
    if dry_run {
        info!("[DRY-RUN] release slot {slot} (owner: {owner})");
        return Ok(());
    }
    info!("releasing slot {slot} (owner: {owner})");
    let now = now_ms();
    let _: () = con.del(key_owner(&slot_str)).await.context("DEL owner failed")?;
    let _: () = con.zrem(KEY_LEASES, &slot_str).await.context("ZREM leases failed")?;
    let _: () = con.zadd(KEY_AVAILABLE, &slot_str, now).await.context("ZADD available failed")?;
    Ok(())
}

/// Terminate an ASG instance.
///
/// When `decrement` is `true` the ASG desired-capacity counter is decremented
/// by 1 (production behaviour).  Pass `false` in test environments where you
/// want the ASG to launch a replacement automatically.
pub(crate) async fn terminate_instance(
    instance_id: &str,
    region:      &str,
    creds:       &AwsCredentials,
    dry_run:     bool,
    decrement:   bool,
) -> Result<()> {
    if dry_run {
        info!("[DRY-RUN] terminate instance {instance_id} (decrement_capacity={decrement})");
        return Ok(());
    }
    info!("terminating instance {instance_id} (decrement_capacity={decrement})");
    asg::terminate_instance(region, instance_id, creds, decrement).await
}
