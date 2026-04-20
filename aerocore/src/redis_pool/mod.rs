//! Redis slot-pool helpers shared across aeroslot, aeroscale, and any other
//! crate that needs to read or write the slot pool state.

use anyhow::{Context, Result};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

// ── Key schema ────────────────────────────────────────────────────────────────

pub const KEY_AVAILABLE: &str = "slots:available";
pub const KEY_LEASES:    &str = "slots:leases";

/// Per-backend weight value persisted by the master so backups can sync.
/// Key: `backend:weight:<IPv4>` → value string ("0", "-1", "-2147483648").
pub const KEY_BACKEND_WEIGHT_PREFIX: &str = "backend:weight:";

/// Unix-millisecond timestamp written by the master after every persist pass.
pub const KEY_BACKEND_WEIGHTS_TS:    &str = "backend:weights:ts";

/// Returns the Redis key that stores the owner (instance-id) for a slot.
pub fn key_owner(slot: &str) -> String {
    format!("slot:owner:{slot}")
}

/// Current time as Unix milliseconds.  Panics only if the system clock is
/// set before the Unix epoch (not a real-world concern).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis() as u64
}

// ── Client builder ────────────────────────────────────────────────────────────

/// Build a Redis client, optionally with TLS.
///
/// TLS is activated when any of the following is true:
///   - `tls` flag is set
///   - `tls_insecure` flag is set (also skips certificate verification)
///   - `tls_ca_cert` path is provided (uses a custom CA instead of system roots)
///   - the URL already starts with `rediss://`
pub fn build_redis_client(
    redis_url: &str,
    tls: bool,
    tls_insecure: bool,
    tls_ca_cert: &Option<PathBuf>,
) -> Result<redis::Client> {
    let use_tls =
        tls || tls_insecure || tls_ca_cert.is_some() || redis_url.starts_with("rediss://");

    if !use_tls {
        return redis::Client::open(redis_url).context("Invalid Redis URL");
    }

    // Ensure the URL uses the rediss:// scheme so redis-rs activates TLS.
    let url = if redis_url.starts_with("redis://") {
        redis_url.replacen("redis://", "rediss://", 1)
    } else {
        redis_url.to_string()
    };

    // The #insecure URL fragment tells redis-rs to skip certificate verification.
    let url = if tls_insecure && !url.contains("#insecure") {
        format!("{url}#insecure")
    } else {
        url
    };

    match tls_ca_cert {
        None => redis::Client::open(url.as_str()).context("Invalid Redis URL"),
        Some(ca_path) => {
            let ca_pem = std::fs::read(ca_path)
                .with_context(|| format!("Cannot read CA cert: {}", ca_path.display()))?;

            redis::Client::build_with_tls(
                url.as_str(),
                redis::TlsCertificates {
                    client_tls: None,
                    root_cert: Some(ca_pem),
                },
            )
            .context("Failed to build Redis TLS client")
        }
    }
}
