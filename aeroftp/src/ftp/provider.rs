//! [`UserDetailProvider`] that performs the login-time Redis lookup.
//!
//! After a successful authentication, libunftp calls
//! [`provide_user_detail`](AeroUserDetailProvider::provide_user_detail) with
//! the [`Principal`] (which now carries `source_ip` thanks to
//! [`AeroAuthenticator`]).
//!
//! This provider does one Redis call:
//!
//! ```text
//! HGETALL systems:by-ip:{client_ip}
//! ```
//!
//! If the key does not exist the session is accepted but flagged as
//! quarantined (`AeroUser::quarantined = true`). Path remapping (Phase 3)
//! will route quarantined uploads to a separate prefix.
//!
//! On success, all hash fields are stored in [`AeroUser::system_meta`] for the
//! lifetime of the session, and attached as S3 object metadata on every STOR.
//!
//! [`AeroAuthenticator`]: super::auth::AeroAuthenticator

use std::fmt;

use async_trait::async_trait;
use redis::AsyncCommands;
use tracing::{info, instrument, warn};
use unftp_core::auth::{Principal, UserDetailError, UserDetailProvider};

use super::user::AeroUser;

/// Performs `HGETALL systems:by-ip:{ip}` at login and builds [`AeroUser`].
///
/// [`ConnectionManager`] is internally `Arc`-based and multiplexes concurrent
/// commands safely without an external `Mutex`.  Cloning it per call is cheap
/// (just an Arc ref-count bump) and lets all concurrent logins proceed in
/// parallel rather than serialising through a lock.
pub struct AeroUserDetailProvider {
    redis: redis::aio::ConnectionManager,
}

impl fmt::Debug for AeroUserDetailProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AeroUserDetailProvider").finish_non_exhaustive()
    }
}

impl AeroUserDetailProvider {
    pub fn new(redis: redis::aio::ConnectionManager) -> Self {
        Self { redis }
    }
}

#[async_trait]
impl UserDetailProvider for AeroUserDetailProvider {
    type User = AeroUser;

    #[instrument(skip(self), fields(username = %principal.username, ip = ?principal.source_ip))]
    async fn provide_user_detail(
        &self,
        principal: &Principal,
    ) -> Result<AeroUser, UserDetailError> {
        let client_ip = principal.source_ip.ok_or_else(|| {
            UserDetailError::Generic(
                "Principal is missing source_ip — AeroAuthenticator must be used".to_string(),
            )
        })?;

        let key = format!("systems:by-ip:{client_ip}");

        let system_meta: std::collections::HashMap<String, String> = {
            // Clone the manager — cheap (Arc bump) and gives this call its
            // own send slot on the multiplexed connection without locking.
            let mut conn = self.redis.clone();
            conn.hgetall(&key).await.map_err(|e| {
                UserDetailError::Generic(format!("Redis error looking up '{key}': {e}"))
            })?
        };

        if system_meta.is_empty() {
            warn!(
                ip  = %client_ip,
                key,
                "Device not found in Redis — accepting as quarantined"
            );
            return Ok(AeroUser {
                username:    principal.username.clone(),
                client_ip,
                system_meta: std::collections::HashMap::new(),
                quarantined: true,
            });
        }

        info!(
            ip       = %client_ip,
            modality = system_meta.get("modality").map(|s| s.as_str()).unwrap_or("?"),
            product  = system_meta.get("product").map(|s| s.as_str()).unwrap_or("?"),
            partno   = system_meta.get("partno").map(|s| s.as_str()).unwrap_or("?"),
            serial   = system_meta.get("serial").map(|s| s.as_str()).unwrap_or("?"),
            "Device identified from Redis"
        );

        Ok(AeroUser {
            username: principal.username.clone(),
            client_ip,
            system_meta,
            quarantined: false,
        })
    }
}
