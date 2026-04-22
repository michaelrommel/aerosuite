//! Weight-file state persistence and startup initialisation.
//!
//! ## Why this module exists
//!
//! keepalived has a quirk: backends initialised as *disabled* (`-2147483648`)
//! are not tracked — their weight files are ignored.  Therefore the keepalived
//! OpenRC script always initialises every weight file to *draining* (`-1`) so
//! that keepalived begins tracking them.  On a fresh boot `aeroscale` would
//! then see 20 draining backends with 0 connections and (on the master) try to
//! terminate them all.
//!
//! This module fixes that by rewriting the weight files **once at daemon
//! startup**, before the first cleanup pass runs, using either:
//!
//! 1. **Persisted Redis state** (fresh restart): the master persisted the
//!    canonical weight values on its last pass; restore them verbatim.
//! 2. **Lease-derived state** (first run or stale state): set backends with
//!    a live lease to Active (`0`) and everything else to Disabled.
//!
//! After every cleanup pass the **master** calls `persist()` to write the
//! current weight files back into Redis so the backup (and future restarts)
//! can pick up from where things left off.
//!
//! The **backup** calls `sync_from_redis()` each cycle to keep its local
//! weight files consistent with the master's — ready for an instant, correct
//! failover.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use tracing::{debug, info, warn};

use aerocore::redis_pool::{
    now_ms, KEY_BACKEND_WEIGHT_PREFIX, KEY_BACKEND_WEIGHTS_TS,
};

use crate::cleanup::{WEIGHT_ACTIVE, WEIGHT_DISABLED};
use crate::slot_network::SlotNetwork;
use crate::snapshot::leases;

// ── Public API ────────────────────────────────────────────────────────────────

/// Initialise weight files on daemon startup.
///
/// Reads the Redis state timestamp and decides which strategy to use:
///
/// * **Fresh** (age < `ttl_secs`): restore each file from its Redis key.
///   Any IP with no Redis entry is left as draining (`-1`) — the next
///   cleanup pass will resolve it.
///
/// * **Stale or absent**: compute from current lease state — live leases →
///   Active, everything else → Disabled.
pub async fn init(
    weights_dir:  &str,
    redis_con:    &mut MultiplexedConnection,
    slot_network: &SlotNetwork,
    ttl_secs:     u64,
) -> Result<()> {
    let now       = now_ms();
    let _ttl_ms    = ttl_secs * 1000;

    let ts: Option<String> = redis_con
        .get(KEY_BACKEND_WEIGHTS_TS)
        .await
        .unwrap_or(None);

    let use_redis = match ts.as_deref().and_then(|s| s.parse::<u64>().ok()) {
        Some(stored) => {
            let age_secs = now.saturating_sub(stored) / 1000;
            if age_secs < ttl_secs {
                info!(age_secs, ttl_secs, "Redis weight state is fresh — restoring");
                true
            } else {
                warn!(
                    age_secs, ttl_secs,
                    "Redis weight state is stale — computing from current leases"
                );
                false
            }
        }
        None => {
            info!("No Redis weight state found — computing from current leases");
            false
        }
    };

    if use_redis {
        restore_from_redis(weights_dir, redis_con).await
    } else {
        compute_from_leases(weights_dir, redis_con, slot_network).await
    }
}

/// Persist all current weight file values to Redis.
///
/// Called by the **master** after every cleanup pass.  The backup reads these
/// values via `sync_from_redis()` to stay consistent.
pub async fn persist(weights_dir: &str, redis_con: &mut MultiplexedConnection) -> Result<()> {
    let mut dir = tokio::fs::read_dir(weights_dir)
        .await
        .with_context(|| format!("Cannot read weights dir: {weights_dir}"))?;

    let mut count = 0usize;

    while let Some(entry) = dir.next_entry().await? {
        let fname = entry.file_name();
        let name  = fname.to_string_lossy();
        if let Some(ip_str) = name
            .strip_prefix("backend-")
            .and_then(|s| s.strip_suffix(".weight"))
        {
            if let Ok(content) = tokio::fs::read_to_string(entry.path()).await {
                let key = format!("{KEY_BACKEND_WEIGHT_PREFIX}{ip_str}");
                let _: () = redis_con
                    .set(&key, content.trim())
                    .await
                    .with_context(|| format!("Redis SET {key} failed"))?;
                count += 1;
            }
        }
    }

    let _: () = redis_con
        .set(KEY_BACKEND_WEIGHTS_TS, now_ms().to_string())
        .await
        .context("Redis SET weight timestamp failed")?;

    debug!(count, "weight state persisted to Redis");
    Ok(())
}

/// Sync weight files from Redis (**backup mode**, called each cycle).
///
/// Only writes a file when the Redis value differs from what is on disk,
/// minimising filesystem churn.
pub async fn sync_from_redis(
    weights_dir: &str,
    redis_con:   &mut MultiplexedConnection,
) -> Result<()> {
    let mut dir = tokio::fs::read_dir(weights_dir)
        .await
        .with_context(|| format!("Cannot read weights dir: {weights_dir}"))?;

    let mut synced = 0usize;

    while let Some(entry) = dir.next_entry().await? {
        let fname = entry.file_name();
        let name  = fname.to_string_lossy();
        if let Some(ip_str) = name
            .strip_prefix("backend-")
            .and_then(|s| s.strip_suffix(".weight"))
        {
            let key = format!("{KEY_BACKEND_WEIGHT_PREFIX}{ip_str}");
            if let Ok(Some(redis_val)) =
                redis_con.get::<_, Option<String>>(&key).await
            {
                let disk = tokio::fs::read_to_string(entry.path())
                    .await
                    .unwrap_or_default();
                if disk.trim() != redis_val.trim() {
                    tokio::fs::write(entry.path(), redis_val.trim())
                        .await
                        .with_context(|| {
                            format!("Cannot write {}", entry.path().display())
                        })?;
                    debug!(ip = ip_str, "synced weight file from Redis");
                    synced += 1;
                }
            }
        }
    }

    if synced > 0 {
        info!(synced, "weight files synced from Redis (backup mode)");
    }
    Ok(())
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// Restore weight files from per-IP Redis keys.
/// IPs with no stored value are left as-is (keepalived already set them to
/// draining; the first cleanup pass will correct them).
async fn restore_from_redis(
    weights_dir: &str,
    redis_con:   &mut MultiplexedConnection,
) -> Result<()> {
    let mut dir = tokio::fs::read_dir(weights_dir).await?;
    let mut restored = 0usize;

    while let Some(entry) = dir.next_entry().await? {
        let fname = entry.file_name();
        let name  = fname.to_string_lossy();
        if let Some(ip_str) = name
            .strip_prefix("backend-")
            .and_then(|s| s.strip_suffix(".weight"))
        {
            let key = format!("{KEY_BACKEND_WEIGHT_PREFIX}{ip_str}");
            if let Ok(Some(val)) = redis_con.get::<_, Option<String>>(&key).await {
                tokio::fs::write(entry.path(), val.trim())
                    .await
                    .with_context(|| format!("Cannot write {}", entry.path().display()))?;
                restored += 1;
            }
            // No stored value → leave as "-1"; cleanup will fix it next pass.
        }
    }

    info!(restored, "weight files restored from Redis");
    Ok(())
}

/// Compute weight files from current Redis lease state.
async fn compute_from_leases(
    weights_dir:  &str,
    redis_con:    &mut MultiplexedConnection,
    slot_network: &SlotNetwork,
) -> Result<()> {
    let lease_list = leases::read_all(redis_con, &mut HashMap::new()).await.unwrap_or_else(|e| {
        warn!("Could not read leases for weight init: {e:#}");
        Vec::new()
    });

    let live_ips: HashSet<Ipv4Addr> = lease_list
        .iter()
        .filter(|l| !l.is_expired())
        .map(|l| slot_network.ip_for_slot(l.slot))
        .collect();

    let mut dir = tokio::fs::read_dir(weights_dir).await?;
    let (mut active, mut disabled) = (0usize, 0usize);

    while let Some(entry) = dir.next_entry().await? {
        let fname = entry.file_name();
        let name  = fname.to_string_lossy();
        if let Some(ip_str) = name
            .strip_prefix("backend-")
            .and_then(|s| s.strip_suffix(".weight"))
        {
            if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
                let value = if live_ips.contains(&ip) {
                    active += 1;
                    WEIGHT_ACTIVE
                } else {
                    disabled += 1;
                    WEIGHT_DISABLED
                };
                tokio::fs::write(entry.path(), value)
                    .await
                    .with_context(|| format!("Cannot write {}", entry.path().display()))?;
            }
        }
    }

    info!(active, disabled, "weight files initialised from lease state");
    Ok(())
}
