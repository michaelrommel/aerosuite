//! [`UserDetailProvider`] that builds [`AeroUser`] from a [`Principal`].
//!
//! Phase 1 (this file): constructs [`AeroUser`] directly from the
//! authenticated [`Principal`], carrying only the client IP.
//! `system_meta` is left empty — no Redis call is made.
//!
//! Phase 2 will replace the body of [`provide_user_detail`] with a Redis
//! `HGETALL systems:by-ip:{client_ip}` call, populating `system_meta` and
//! rejecting unknown devices before any data channel is opened.
//!
//! [`provide_user_detail`]: AeroUserDetailProvider::provide_user_detail

use std::fmt;

use async_trait::async_trait;
use tracing::instrument;
use unftp_core::auth::{Principal, UserDetailError, UserDetailProvider};

use super::user::AeroUser;

/// Builds an [`AeroUser`] from the authenticated [`Principal`].
///
/// Phase 1: IP-only, empty `system_meta`. No external I/O.
#[derive(Debug)]
pub struct AeroUserDetailProvider;

impl AeroUserDetailProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AeroUserDetailProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AeroUserDetailProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AeroUserDetailProvider")
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

        // Phase 1: no Redis lookup; system_meta is empty.
        // Phase 2: replace this with HGETALL systems:by-ip:{client_ip}.
        Ok(AeroUser {
            username: principal.username.clone(),
            client_ip,
            system_meta: std::collections::HashMap::new(),
        })
    }
}
