//! Custom [`UserDetail`] implementation for aeroftp.
//!
//! [`AeroUser`] is populated at login time by [`AeroUserDetailProvider`] and
//! lives in the session for its entire lifetime.  It carries:
//!
//! - The client's IP address (for tracing and audit)
//! - The system metadata fetched from Redis at login (`systems:by-ip:{ip}`)
//! - A quarantine flag for devices unknown to Redis
//!
//! [`AeroUserDetailProvider`]: super::provider::AeroUserDetailProvider

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;

use unftp_core::auth::UserDetail;

/// Session-scoped user context populated from Redis at login.
#[derive(Debug, Clone)]
pub struct AeroUser {
    /// Authenticated FTP username.
    pub username: String,

    /// Source IP of the control connection — the Redis lookup key.
    pub client_ip: IpAddr,

    /// Raw key/value pairs from `HGETALL systems:by-ip:{client_ip}`.
    ///
    /// Empty when [`quarantined`](AeroUser::quarantined) is `true`.
    ///
    /// Expected fields for registered devices:
    /// `modality`, `product`, `partno`, `serial`, `contracts`, `source_country`
    ///
    /// Additional fields are stored as-is and forwarded as S3 user metadata.
    pub system_meta: HashMap<String, String>,

    /// `true` when the device IP had no entry in `systems:by-ip:{ip}`.
    ///
    /// Quarantined sessions are accepted (no `530` rejection) but every upload
    /// will carry empty S3 metadata. Path remapping (Phase 3) will route them
    /// to a separate prefix once that is wired up.
    pub quarantined: bool,
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
