//! Cleanup actions derived from a `SystemSnapshot`.
//!
//! All three sections are driven purely by snapshot data — no additional I/O
//! happens before the decision is logged.  When `dry_run` is true every
//! action is logged at INFO level but no write is performed.
//!
//! ## Section 2.1 — Active leases
//! For each lease, cross-reference the ASG and backend state, then act.
//!
//! ## Section 2.2 — ASG instances without leases
//! Any InService instance that holds no lease is an orphan; terminate it.
//!
//! ## Section 2.3 — Backends without leases
//! Weight files that have no matching lease indicate a crashed backend.

use std::net::Ipv4Addr;
use anyhow::{Context, Result};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use tracing::{debug, error, info, warn};

use aerocore::{asg, redis_pool::{key_owner, now_ms, KEY_AVAILABLE, KEY_LEASES}, AwsCredentials};
use crate::snapshot::{BackendState, SlotLease, SystemSnapshot};

// ── Weight file constants ─────────────────────────────────────────────────────

pub const WEIGHT_ACTIVE:   &str = "0";
pub const WEIGHT_DRAINING: &str = "-1";
pub const WEIGHT_DISABLED: &str = "-2147483648";

// ── Public entry point ────────────────────────────────────────────────────────

/// Run all three cleanup sections against `snapshot`.
///
/// On the VRRP backup this is a no-op: the backup maintains its weight files
/// by syncing from Redis rather than running independent cleanup logic.
///
/// `term_decrements_capacity` controls whether `TerminateInstance` asks AWS
/// to also decrement the ASG desired-capacity counter.  Set to `false` during
/// testing so the ASG immediately launches a replacement; `true` in production.
pub async fn run(
    snapshot:                &SystemSnapshot,
    weights_dir:             &str,
    region:                  &str,
    creds:                   &AwsCredentials,
    redis_con:               &mut MultiplexedConnection,
    dry_run:                 bool,
    is_master:               bool,
    term_decrements_capacity: bool,
) -> Result<()> {
    if !is_master {
        debug!("backup mode — skipping cleanup pass");
        return Ok(());
    }
    info!("── cleanup pass ──────────────────────────────────────────────────────────");

    section_21_active_leases(snapshot, weights_dir, region, creds, redis_con, dry_run, term_decrements_capacity).await;
    section_22_orphaned_asg_instances(snapshot, region, creds, dry_run, term_decrements_capacity).await;
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
    let asg_ids: std::collections::HashSet<&str> =
        snapshot.asg.iter().map(|i| i.instance_id.as_str()).collect();

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
    snapshot:                &SystemSnapshot,
    region:                  &str,
    creds:                   &AwsCredentials,
    dry_run:                 bool,
    term_decrements_capacity: bool,
) {
    let leased_owners: std::collections::HashSet<&str> =
        snapshot.leases.iter().map(|l| l.owner_instance_id.as_str()).collect();

    for inst in snapshot.asg.iter().filter(|i| i.is_in_service()) {
        if !leased_owners.contains(inst.instance_id.as_str()) {
            error!(
                instance_id = %inst.instance_id,
                "InService ASG instance has no slot lease — possible ENI leak; terminating"
            );
            if let Err(e) = terminate_instance(&inst.instance_id, region, creds, dry_run, term_decrements_capacity).await {
                error!(instance_id = %inst.instance_id, "terminate failed: {e:#}");
            }
        }
    }
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
