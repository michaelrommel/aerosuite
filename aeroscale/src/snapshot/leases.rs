//! Read active slot leases from Redis.
//!
//! Queries `slots:leases` (sorted set, score = expiry ms) and resolves the
//! owner instance-id for each leased slot via `slot:owner:<n>`.

use anyhow::{Context, Result};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use tracing::debug;

use aerocore::redis_pool::{key_owner, now_ms, KEY_LEASES};

use super::SlotLease;

/// Return all active leases, sorted by slot number.
///
/// Leases whose TTL has already expired are included in the raw list (Redis
/// only removes them lazily on the next `claim`).  Callers can check
/// `SlotLease::is_expired()` if they need to distinguish.
pub async fn read_all(con: &mut MultiplexedConnection) -> Result<Vec<SlotLease>> {
    let raw: Vec<(String, f64)> = con
        .zrange_withscores(KEY_LEASES, 0isize, -1isize)
        .await
        .context("ZRANGE slots:leases WITHSCORES failed")?;

    debug!("{} lease(s) found in Redis", raw.len());

    let now = now_ms();
    let mut leases = Vec::with_capacity(raw.len());

    for (slot_str, expiry_score) in raw {
        let slot: u32 = slot_str.parse().unwrap_or(0);

        let owner: String = con
            .get(key_owner(&slot_str))
            .await
            .unwrap_or_else(|_| "(unknown)".to_string());

        let expires_ms = expiry_score as u64;
        let remaining_secs = if expires_ms > now {
            (expires_ms - now) as f64 / 1000.0
        } else {
            0.0
        };

        debug!(slot, owner, remaining_secs, "lease");
        leases.push(SlotLease { slot, owner_instance_id: owner, expires_ms });
    }

    leases.sort_by_key(|l| l.slot);
    Ok(leases)
}
