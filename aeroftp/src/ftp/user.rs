//! Custom [`UserDetail`] implementation for aeroftp.
//!
//! [`AeroUser`] is populated at login time by [`AeroUserDetailProvider`] and
//! lives in the session for its entire lifetime.  It carries:
//!
//! - The client's IP address (for tracing and audit)
//! - The system metadata fetched from Redis at login (`systems:by-ip:{ip}`)
//!   (empty in Phase 1 — populated in Phase 2 when Redis is wired up)
//!
//! [`AeroUserDetailProvider`]: super::provider::AeroUserDetailProvider

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;

use unftp_core::auth::UserDetail;

/// Session-scoped user context populated at login.
#[derive(Debug, Clone)]
pub struct AeroUser {
    /// Authenticated FTP username.
    pub username: String,

    /// Source IP of the control connection — used as the Redis lookup key in Phase 2.
    pub client_ip: IpAddr,

    /// Key/value pairs from `HGETALL systems:by-ip:{client_ip}`.
    ///
    /// Empty in Phase 1 (no Redis).  Phase 2 populates this with fields such as
    /// `modality`, `product`, `partno`, `serial`, `contracts`, `source_country`.
    pub system_meta: HashMap<String, String>,
}

impl AeroUser {
    /// Returns the value of a metadata field, or an error if it is missing.
    pub fn require(&self, key: &str) -> anyhow::Result<&str> {
        self.system_meta
            .get(key)
            .map(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("system metadata missing required field '{key}'"))
    }
}

impl UserDetail for AeroUser {
    fn account_enabled(&self) -> bool {
        true
    }
}

impl fmt::Display for AeroUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.username, self.client_ip)
    }
}
