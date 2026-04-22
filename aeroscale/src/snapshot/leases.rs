//! Read active slot leases from Redis.
//!
//! Queries `slots:leases` (sorted set, score = expiry ms) and resolves the
//! owner instance-id for each leased slot via `slot:owner:<n>`.
//!
//! ## Owner resolution — three distinct outcomes
//!
//! Each `GET slot:owner:<n>` can produce one of three results, and each is
//! handled differently:
//!
//! | Result              | Meaning                                      | Action                                  |
//! |---------------------|----------------------------------------------|-----------------------------------------|
//! | `Ok(Some(id))`      | Key exists — normal case                    | Use `id`; refresh the cross-cycle cache |
//! | `Ok(None)` (Nil)    | Key is genuinely absent from Redis           | Log specific warning; mark `(missing)`; evict cache entry |
//! | `Err(e)`            | Transient network / IO error                 | Use cached value if available; otherwise mark `(unknown)` |
//!
//! The sentinel strings `"(missing)"` and `"(unknown)"` are intentionally
//! distinct so that downstream cleanup logic can treat them separately and
//! never act destructively on a slot whose owner could not be confirmed.

use std::collections::HashMap;

use anyhow::{Context, Result};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use tracing::{debug, warn};

use aerocore::redis_pool::{key_owner, now_ms, KEY_LEASES};

use super::SlotLease;

/// Sentinel written to `SlotLease::owner_instance_id` when the `slot:owner`
/// key was absent from Redis (genuine Nil response).  The lease exists but the
/// owner record was never written or was deleted externally.
pub const OWNER_MISSING: &str = "(missing)";

/// Sentinel written to `SlotLease::owner_instance_id` when the `GET
/// slot:owner` command failed with a Redis / network error **and** no cached
/// value from a previous successful cycle is available.
pub const OWNER_UNKNOWN: &str = "(unknown)";

/// Return all active leases, sorted by slot number.
///
/// `owner_cache` carries the last successfully read owner per slot across
/// calls.  On a transient Redis error the cached value is used instead of
/// falling back to the `(unknown)` sentinel, preventing cleanup from treating
/// a live, healthy backend as ownerless.  Pass the same `HashMap` on every
/// call; it is updated in-place.
///
/// Leases whose TTL has already expired are included in the raw list (Redis
/// only removes them lazily on the next `claim`).  Callers can check
/// `SlotLease::is_expired()` if they need to distinguish.
pub async fn read_all(
    con:         &mut MultiplexedConnection,
    owner_cache: &mut HashMap<u32, String>,
) -> Result<Vec<SlotLease>> {
    let raw: Vec<(String, f64)> = con
        .zrange_withscores(KEY_LEASES, 0isize, -1isize)
        .await
        .context("ZRANGE slots:leases WITHSCORES failed")?;

    debug!("{} lease(s) found in Redis", raw.len());

    let now = now_ms();
    let mut leases = Vec::with_capacity(raw.len());

    for (slot_str, expiry_score) in raw {
        let slot: u32 = slot_str.parse().unwrap_or(0);

        // Use Option<String> so redis-rs maps a Nil response to Ok(None) rather
        // than Err(...), giving us a clean three-way split between a present
        // value, a genuinely absent key, and a network/IO error.
        let owner_result: redis::RedisResult<Option<String>> =
            con.get(key_owner(&slot_str)).await;

        let owner = match owner_result {
            Ok(Some(id)) => {
                // Normal path — key exists.  Refresh the cache for this slot.
                owner_cache.insert(slot, id.clone());
                id
            }

            Ok(None) => {
                // The key genuinely does not exist in Redis (Nil).  This is a
                // distinct problem from a network error: the lease is present
                // but the owner record is missing.  The cached value (if any)
                // would be stale and misleading, so we evict it and surface a
                // specific sentinel so cleanup can log an accurate message.
                warn!(
                    slot,
                    "slot:owner:{slot} key is absent in Redis (Nil) — \
                     lease exists but owner record was never written or was deleted"
                );
                owner_cache.remove(&slot);
                OWNER_MISSING.to_string()
            }

            Err(e) => {
                // Transient Redis / network error.  Do not touch the cache so
                // that the last known-good value remains available.
                match owner_cache.get(&slot) {
                    Some(cached_id) => {
                        warn!(
                            slot,
                            error = %e,
                            cached_owner = %cached_id,
                            "GET slot:owner:{slot} failed (transient Redis error) \
                             — using cached owner from previous cycle"
                        );
                        cached_id.clone()
                    }
                    None => {
                        // No previous successful read exists (e.g. first cycle
                        // after daemon startup hit an immediate Redis error).
                        warn!(
                            slot,
                            error = %e,
                            "GET slot:owner:{slot} failed and no cached value is \
                             available — owner unresolvable this cycle"
                        );
                        OWNER_UNKNOWN.to_string()
                    }
                }
            }
        };

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
