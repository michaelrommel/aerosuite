//! Read keepalived weight files from the weights directory.
//!
//! Each file is named `backend-<IPv4>.weight` and contains a single integer:
//!   "0"            → Active   (keepalived uses this backend at full weight)
//!   "-1"           → Draining (weight reduced; waiting for connections to drain)
//!   "-2147483648"  → Disabled (keepalived ignores this real server entirely)

use std::net::Ipv4Addr;
use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::BackendState;

pub struct WeightEntry {
    pub ip:    Ipv4Addr,
    pub state: BackendState,
}

/// Scan `weights_dir` for files matching `backend-<IP>.weight` and parse
/// each one.  Returns entries sorted by IP address.
///
/// Non-matching filenames and parse failures are skipped with a warning so
/// that a single corrupt file does not abort the whole snapshot.
pub async fn read_all(weights_dir: &str) -> Result<Vec<WeightEntry>> {
    let mut dir = tokio::fs::read_dir(weights_dir)
        .await
        .with_context(|| format!("Cannot read weights directory: {weights_dir}"))?;

    let mut entries = Vec::new();

    while let Some(entry) = dir.next_entry().await? {
        let fname = entry.file_name();
        let name  = fname.to_string_lossy();

        let ip_str = match name
            .strip_prefix("backend-")
            .and_then(|s| s.strip_suffix(".weight"))
        {
            Some(s) => s.to_owned(),
            None    => { debug!("skipping non-weight file: {name}"); continue; }
        };

        let ip: Ipv4Addr = match ip_str.parse() {
            Ok(ip)  => ip,
            Err(_)  => {
                warn!("cannot parse IP from weight filename: {name}");
                continue;
            }
        };

        let raw = tokio::fs::read_to_string(entry.path())
            .await
            .with_context(|| format!("Cannot read weight file: {}", entry.path().display()))?;

        let state = BackendState::from_weight_str(raw.trim());
        debug!("weight file {name}: {state:?}");
        entries.push(WeightEntry { ip, state });
    }

    entries.sort_by_key(|e| e.ip);
    Ok(entries)
}
