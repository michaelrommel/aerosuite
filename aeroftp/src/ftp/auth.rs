//! Authenticator wrapper that enriches the [`Principal`] with the client's
//! source IP address.
//!
//! [`AeroAuthenticator`] delegates all password / certificate validation to the
//! inner [`JsonFileAuthenticator`] and then copies `Credentials::source_ip`
//! onto the returned [`Principal`].  This is the only change needed in the auth
//! layer: the IP then flows to [`AeroUserDetailProvider`], which uses it as the
//! Redis lookup key (Phase 2) or simply stores it on the session (Phase 1).
//!
//! [`AeroUserDetailProvider`]: super::provider::AeroUserDetailProvider

use std::fmt;

use async_trait::async_trait;
use tracing::instrument;
use unftp_auth_jsonfile::JsonFileAuthenticator;
use unftp_core::auth::{AuthenticationError, Authenticator, Credentials, Principal};

/// Wraps [`JsonFileAuthenticator`] and stamps `source_ip` onto the
/// [`Principal`] so downstream providers can use it.
#[derive(Debug)]
pub struct AeroAuthenticator {
    inner: JsonFileAuthenticator,
}

impl AeroAuthenticator {
    /// Create a new `AeroAuthenticator` backed by the given credentials file.
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let inner = JsonFileAuthenticator::from_file(path)
            .map_err(|e| anyhow::anyhow!("could not load credentials file: {e}"))?;
        Ok(Self { inner })
    }
}

impl fmt::Display for AeroAuthenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AeroAuthenticator")
    }
}

#[async_trait]
impl Authenticator for AeroAuthenticator {
    #[instrument(skip(self, creds), fields(username, ip = %creds.source_ip))]
    async fn authenticate(
        &self,
        username: &str,
        creds: &Credentials,
    ) -> Result<Principal, AuthenticationError> {
        // Delegate all credential validation to the inner authenticator.
        let mut principal = self.inner.authenticate(username, creds).await?;

        // Stamp the client IP so AeroUserDetailProvider can use it as the
        // session key without needing access to the raw Credentials.
        principal.source_ip = Some(creds.source_ip);

        Ok(principal)
    }
}
