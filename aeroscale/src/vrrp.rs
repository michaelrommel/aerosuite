//! VRRP master detection.
//!
//! The active VRRP master holds the inside VIP on one of its interfaces.
//! Checking for that IP in `ip addr show` output is the simplest and most
//! reliable indicator — it changes immediately on failover without any
//! additional coordination.

use std::net::Ipv4Addr;
use tracing::{debug, warn};

/// Returns `true` if this node currently holds `vip_inside`, meaning it is
/// the active VRRP master and should perform destructive actions (cleanup,
/// termination, CloudWatch push).
///
/// Falls back to `false` (backup mode) on any error so the daemon never
/// accidentally takes master actions when its own state is uncertain.
pub async fn is_master(vip_inside: Ipv4Addr) -> bool {
    let result = tokio::process::Command::new("ip")
        .args(["addr", "show"])
        .output()
        .await;

    match result {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let master = stdout.contains(&vip_inside.to_string());
            debug!(%vip_inside, master, "VRRP role check");
            master
        }
        Ok(o) => {
            warn!("'ip addr show' exited {}: assuming BACKUP", o.status);
            false
        }
        Err(e) => {
            warn!("'ip addr show' failed: {e} — assuming BACKUP");
            false
        }
    }
}
