use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Error};
use chrono::{DateTime, TimeZone, Utc};
use log::{debug, info, trace};
use reqsign::AwsCredential;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::RwLock;

const ECS_METADATA_BASE_URL: &str = "http://169.254.170.2";
const EC2_METADATA_TOKEN_URL: &str = "http://169.254.169.254/latest/api/token";
const EC2_METADATA_IAM_ROLE_PATH: &str =
    "http://169.254.169.254/latest/meta-data/iam/security-credentials/";

/// Type-safe wrapper for AWS access key IDs.
///
/// This newtype ensures that access key IDs are always properly wrapped,
/// preventing accidental misuse or confusion with other string values.
///
/// # Security
/// Unlike `SecretAccessKey`, this type can be safely logged and displayed,
/// as it contains only the public portion of AWS credentials.
#[derive(Clone)]
pub struct AccessKeyId(String);

impl<'de> Deserialize<'de> for AccessKeyId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(AccessKeyId::new(s))
    }
}

impl AccessKeyId {
    /// Creates a new access key ID wrapper from a string.
    ///
    /// # Arguments
    /// * `s` - The raw access key ID string (e.g., "ASIA...")
    ///
    /// # Examples
    /// ```
    /// let key_id = AccessKeyId::new("ASIA1234567890ABCDEF".to_string());
    /// assert_eq!(key_id.as_str(), "ASIA1234567890ABCDEF");
    /// ```
    pub fn new(s: String) -> Self {
        AccessKeyId(s)
    }

    /// Returns the underlying access key ID as a string slice.
    ///
    /// # Examples
    /// ```
    /// let key_id = AccessKeyId::new("ASIA1234567890ABCDEF".to_string());
    /// assert_eq!(key_id.as_str(), "ASIA1234567890ABCDEF");
    /// ```
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccessKeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for AccessKeyId {
    fn from(s: String) -> Self {
        AccessKeyId::new(s)
    }
}

impl From<&str> for AccessKeyId {
    fn from(s: &str) -> Self {
        AccessKeyId::new(s.to_string())
    }
}

/// Type-safe wrapper for AWS secret access keys.
///
/// This newtype ensures that secret access keys are always properly wrapped
/// and prevents accidental logging or display of sensitive credentials.
///
/// # Security
/// The `Debug` implementation returns `[REDACTED]` instead of the actual value,
/// making it safe to use in debug output without risking credential exposure.
#[derive(Clone)]
pub struct SecretAccessKey(String);

impl<'de> Deserialize<'de> for SecretAccessKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(SecretAccessKey::new(s))
    }
}

impl SecretAccessKey {
    /// Creates a new secret access key wrapper from a string.
    ///
    /// # Arguments
    /// * `s` - The raw secret access key string (e.g., "99xxxxxxxxxxxxxx")
    ///
    /// # Security Note
    /// This function accepts the raw secret, but the type guarantees it will never
    /// be displayed or logged directly. Use `as_str()` only when you need to pass
    /// the value to external APIs that require it.
    pub fn new(s: String) -> Self {
        SecretAccessKey(s)
    }

    /// Returns the underlying secret access key as a string slice.
    ///
    /// # Security Note
    /// Use this method only when you need to pass the secret to external APIs.
    /// Never log or display the returned value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretAccessKey {
    /// Does not expose the `SecretAccessKey`, even if printed or logged.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretAccessKey([REDACTED])")
    }
}

impl From<String> for SecretAccessKey {
    fn from(s: String) -> Self {
        SecretAccessKey::new(s)
    }
}

impl From<&str> for SecretAccessKey {
    fn from(s: &str) -> Self {
        SecretAccessKey::new(s.to_string())
    }
}

/// Type-safe wrapper for AWS session tokens.
///
/// This newtype ensures that session tokens are always properly wrapped,
/// preventing accidental misuse or confusion with other string values.
///
/// # Security
/// Unlike `SecretAccessKey`, this type can be safely logged and displayed
/// if needed for debugging purposes, as it represents a temporary credential
/// rather than a permanent secret.
#[derive(Clone)]
pub struct SessionToken(String);

impl<'de> Deserialize<'de> for SessionToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(SessionToken::new(s))
    }
}

impl SessionToken {
    /// Creates a new session token wrapper from a string.
    ///
    /// # Arguments
    /// * `s` - The raw session token string (e.g., "IQoJb3J...")
    pub fn new(s: String) -> Self {
        SessionToken(s)
    }

    /// Returns the underlying session token as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for SessionToken {
    fn from(s: String) -> Self {
        SessionToken::new(s)
    }
}

impl From<&str> for SessionToken {
    fn from(s: &str) -> Self {
        SessionToken::new(s.to_string())
    }
}

/// Structured representation of AWS credentials from metadata services.
///
/// This type wraps raw credential strings in typed wrappers for improved
/// safety and prevents accidental exposure of sensitive data through logging.
///
/// # Security Features
/// * `SecretAccessKey` is never displayed, even in debug output
/// * All fields are properly typed to prevent mixing with other string values
/// * Deserialization from AWS metadata services is automatic via serde
#[derive(Deserialize, Clone)]
pub(crate) struct AwsCreds {
    /// The ARN of the IAM role.
    #[serde(skip_deserializing, default = "default_role_arn")]
    role_arn: String,

    /// The access key ID.
    #[serde(rename = "AccessKeyId")]
    access_key_id: AccessKeyId,

    /// The secret access key - never displayed in logs or debug output.
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: SecretAccessKey,

    /// The session token.
    #[serde(rename = "Token")]
    session_token: SessionToken,

    /// The expiration time in RFC3339 format.
    #[serde(rename = "Expiration")]
    expiration: String,
}

/// Returns an empty string as default for role ARN.
fn default_role_arn() -> String {
    String::new()
}

impl Default for AwsCreds {
    /// Creates a new `AwsCreds` with all fields set to empty/default values.
    ///
    /// This is useful for creating an initial state before credentials are provisioned.
    fn default() -> Self {
        Self {
            role_arn: String::new(),
            access_key_id: AccessKeyId::new(String::new()),
            secret_access_key: SecretAccessKey::new(String::new()),
            session_token: SessionToken::new(String::new()),
            expiration: String::new(),
        }
    }
}

impl std::fmt::Debug for AwsCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCreds")
            .field("role_arn", &self.role_arn())
            .field("access_key_id", &self.access_key_id().as_str())
            .field("secret_access_key", &"[REDACTED]")
            .field("session_token", &"[REDACTED]")
            .field("expiration", &self.expiration())
            .finish()
    }
}

/// AWS credentials structure from EC2 Metadata Service response.
#[derive(Deserialize, Debug)]
pub(crate) struct Ec2Creds {
    /// The status code of the request.
    #[serde(default)]
    #[allow(dead_code)]
    code: String,

    /// The last update timestamp.
    #[serde(default)]
    #[allow(dead_code)]
    last_updated: String,

    /// The credential type.
    #[serde(default)]
    #[allow(dead_code)]
    #[serde(rename = "Type")]
    r#type: String,

    /// The access key ID.
    #[serde(rename = "AccessKeyId")]
    access_key_id: String,

    /// The secret access key.
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: String,

    /// The session token.
    #[serde(rename = "Token")]
    token: String,

    /// The expiration time in RFC3339 format.
    #[serde(rename = "Expiration")]
    expiration: String,
}

impl Ec2Creds {
    /// Returns the access key ID.
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// Returns the secret access key.
    pub fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }

    /// Returns the session token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Returns the expiration time.
    pub fn expiration(&self) -> &str {
        &self.expiration
    }

    /// Returns the status code.
    #[allow(dead_code)]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the last_updated time.
    #[allow(dead_code)]
    pub fn last_updated(&self) -> &str {
        &self.last_updated
    }

    /// Returns the credential type.
    #[allow(dead_code)]
    pub fn cred_type(&self) -> &str {
        &self.r#type
    }
}

impl AwsCreds {
    /// Creates a new `AwsCreds` instance with the given values.
    ///
    /// This constructor is useful for testing and creating credential objects
    /// without going through deserialization.
    #[cfg(test)]
    pub fn new(
        role_arn: String,
        access_key_id: AccessKeyId,
        secret_access_key: SecretAccessKey,
        session_token: SessionToken,
        expiration: String,
    ) -> Self {
        Self {
            role_arn,
            access_key_id,
            secret_access_key,
            session_token,
            expiration,
        }
    }

    /// Parses the expiration string and returns the expiry time.
    ///
    /// # Returns
    /// * `Some(SystemTime)` - The credential expiration time if parsing succeeds
    /// * `None` - If the expiration string is empty or cannot be parsed
    pub fn expiry(&self) -> Option<SystemTime> {
        if self.expiration.is_empty() {
            None
        } else {
            DateTime::parse_from_rfc3339(&self.expiration)
                .ok()
                .map(Into::into)
        }
    }

    /// Returns the IAM role ARN.
    pub fn role_arn(&self) -> &str {
        &self.role_arn
    }

    /// Returns the access key ID.
    pub fn access_key_id(&self) -> &AccessKeyId {
        &self.access_key_id
    }

    /// Returns the secret access key.
    pub fn secret_access_key(&self) -> &SecretAccessKey {
        &self.secret_access_key
    }

    /// Returns the session token.
    pub fn session_token(&self) -> &SessionToken {
        &self.session_token
    }

    /// Returns the expiration time in RFC3339 format.
    pub fn expiration(&self) -> &str {
        &self.expiration
    }
}

/// Credential loader with automatic caching and refreshing of AWS credentials.
///
/// This type implements the `reqsign::AwsCredentialLoad` trait, allowing it to be
/// used with opendal and other AWS-compatible services. It automatically:
/// * Caches credentials for up to 15 minutes before expiry
/// * Fetches fresh credentials from EC2 or ECS metadata services when needed
/// * Prevents unnecessary credential refreshes under normal operation
///
/// # Thread Safety
/// This type is fully thread-safe and can be shared across multiple async tasks.
pub struct CachingAwsCredentialLoader {
    /// Shared mutable credentials accessible across async tasks.
    pub credentials: Arc<RwLock<AwsCreds>>,
}

impl Default for CachingAwsCredentialLoader {
    /// Creates a new `CachingAwsCredentialLoader` with empty cached credentials.
    fn default() -> Self {
        Self::new()
    }
}

impl CachingAwsCredentialLoader {
    /// Creates a new credential loader with empty cached credentials.
    ///
    /// The loader will automatically fetch credentials when first requested
    /// and cache them for up to 15 minutes before expiry.
    ///
    /// # Examples
    /// ```no_run
    /// use aeroftp::aws::CachingAwsCredentialLoader;
    ///
    /// let loader = CachingAwsCredentialLoader::new();
    /// // The loader will fetch credentials on first use
    /// ```
    pub fn new() -> Self {
        Self {
            credentials: Arc::new(RwLock::new(AwsCreds::default())),
        }
    }

    /// Checks if valid cached credentials exist and are not expiring within 15 minutes.
    ///
    /// This method implements a proactive refresh strategy by checking if cached
    /// credentials will remain valid for at least 15 more minutes. If they expire sooner,
    /// the cache is considered invalid to allow time for refreshing before actual expiry.
    ///
    /// # Returns
    /// * `Some(AwsCreds)` - Valid cached credentials with expiry > 15 minutes from now
    /// * `None` - No cached credentials exist or they expire within 15 minutes
    ///
    /// # Examples
    /// ```no_run
    /// use aeroftp::aws::{CachingAwsCredentialLoader, AwsCreds};
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let loader = CachingAwsCredentialLoader::new();
    ///     
    ///     match loader.cache_check().await {
    ///         Some(creds) => println!("Using cached credentials: {:?}", creds.access_key_id.as_str()),
    ///         None => println!("Need to fetch fresh credentials"),
    ///     }
    ///     
    ///     Ok(())
    /// }
    /// ```
    #[must_use = "cache_check result indicates whether credentials need refresh"]
    pub async fn cache_check(&self) -> Option<AwsCreds> {
        // the read lock is scoped to this line, we then work on the clone.
        // if we do not clone, the read lock is extended to more lines, which could
        // lead to blocking other accesses. so the clone is the better alternative.
        let cached_credentials = self.credentials.read().await.clone();

        if let Some(expiry) = cached_credentials.expiry() {
            debug!("credentials were cached, expiry is {:?}", expiry);
            if let Ok(n) = expiry.duration_since(SystemTime::now()) {
                debug!("credentials expire in {}", n.as_secs());
                if n >= Duration::from_secs(15 * 60) {
                    return Some(cached_credentials);
                } else {
                    info!("credentials are expired or will be expiring soon, initiating renewal");
                }
            }
        }

        None
    }

    /// Fetches fresh AWS credentials directly from the EC2 instance metadata service.
    ///
    /// This method bypasses any caching and always fetches new credentials.
    /// It should only be used when you need to force a credential refresh
    /// or for testing purposes.
    ///
    /// # Arguments
    /// * `client` - HTTP client to use for metadata service requests
    ///
    /// # Errors
    /// Returns an error if:
    /// * The EC2 metadata token cannot be fetched (network issue)
    /// * The IAM role name cannot be determined
    /// * The credentials cannot be parsed from the response
    ///
    /// # Examples
    /// ```no_run
    /// use aeroftp::aws::CachingAwsCredentialLoader;
    /// use reqwest::Client;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let loader = CachingAwsCredentialLoader::new();
    ///     let client = Client::new();
    ///     
    ///     match loader.get_ec2_credentials(client).await {
    ///         Ok(creds) => println!("Fetched credentials: {}", creds.access_key_id().as_str()),
    ///         Err(e) => eprintln!("Failed to fetch credentials: {}", e),
    ///     }
    ///     
    ///     Ok(())
    /// }
    /// ```
    pub async fn get_ec2_credentials(&self, client: Client) -> Result<AwsCreds, Error> {
        let temp_token = client
            .put(EC2_METADATA_TOKEN_URL)
            .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
            .send()
            .await
            .context("could not fetch temp token")?
            .text()
            .await
            .context("failed to parse temp token")?;
        trace!("temp_token: {:?}", temp_token);
        let iam_role = client
            .get(EC2_METADATA_IAM_ROLE_PATH)
            .header("X-aws-ec2-metadata-token", &temp_token)
            .send()
            .await
            .context("could not fetch IAM role")?
            .text()
            .await
            .context("failed to parse IAM role")?;
        trace!("iam role: {}", iam_role);
        let credentials = client
            .get(format!("{}{}", EC2_METADATA_IAM_ROLE_PATH, iam_role))
            .header("X-aws-ec2-metadata-token", &temp_token)
            .send()
            .await
            .context("could not fetch credentials")?
            .json::<Ec2Creds>()
            .await
            .context("failed to parse credentials")?;

        Ok(AwsCreds {
            role_arn: String::new(),
            access_key_id: AccessKeyId::from(credentials.access_key_id()),
            secret_access_key: SecretAccessKey::from(credentials.secret_access_key()),
            session_token: SessionToken::from(credentials.token()),
            expiration: credentials.expiration().to_string(),
        })
    }

    /// Fetches fresh AWS credentials directly from the ECS task metadata service.
    ///
    /// This method bypasses any caching and always fetches new credentials.
    /// It should only be used when you need to force a credential refresh
    /// or for testing purposes.
    ///
    /// # Arguments
    /// * `client` - HTTP client to use for metadata service requests
    /// * `url` - The full URI of the ECS metadata endpoint (from `AWS_CONTAINER_CREDENTIALS_FULL_URI`)
    ///
    /// # Errors
    /// Returns an error if:
    /// * The credentials cannot be fetched from the provided URL
    /// * The response cannot be parsed as valid AWS credentials
    ///
    /// # Examples
    /// ```no_run
    /// use aeroftp::aws::CachingAwsCredentialLoader;
    /// use reqwest::Client;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let loader = CachingAwsCredentialLoader::new();
    ///     let client = Client::new();
    ///     let url = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI")?;
    ///     
    ///     match loader.get_ecs_credentials(client, url).await {
    ///         Ok(creds) => println!("Fetched credentials: {}", creds.access_key_id().as_str()),
    ///         Err(e) => eprintln!("Failed to fetch credentials: {}", e),
    ///     }
    ///     
    ///     Ok(())
    /// }
    /// ```
    pub async fn get_ecs_credentials(
        &self,
        client: Client,
        url: String,
    ) -> Result<AwsCreds, Error> {
        trace!("Fetching from {}", url);
        let ec2_creds: Ec2Creds = client
            .get(url)
            .send()
            .await
            .context("could not fetch metadata info")?
            .json::<Ec2Creds>()
            .await
            .context("failed to parse credentials")?;

        Ok(AwsCreds {
            role_arn: String::new(),
            access_key_id: AccessKeyId::from(ec2_creds.access_key_id()),
            secret_access_key: SecretAccessKey::from(ec2_creds.secret_access_key()),
            session_token: SessionToken::from(ec2_creds.token()),
            expiration: ec2_creds.expiration().to_string(),
        })
    }

    /// Provisions fresh AWS credentials by automatically detecting the runtime environment.
    ///
    /// This method intelligently determines whether to fetch from ECS or EC2 metadata
    /// based on environment variables:
    /// * If `AWS_CONTAINER_CREDENTIALS_FULL_URI` is set → Uses ECS
    /// * If `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` is set → Uses ECS with base URL
    /// * Otherwise → Falls back to EC2 instance metadata service
    ///
    /// # Arguments
    /// * `client` - HTTP client to use for metadata service requests
    ///
    /// # Errors
    /// Returns an error if:
    /// * No valid credential source is detected (neither ECS nor EC2 available)
    /// * The chosen metadata service cannot be reached
    /// * Credentials cannot be parsed from the response
    ///
    /// # Examples
    /// ```no_run
    /// use aeroftp::aws::CachingAwsCredentialLoader;
    /// use reqwest::Client;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     let loader = CachingAwsCredentialLoader::new();
    ///     let client = Client::new();
    ///     
    ///     match loader.provision_credentials(client).await {
    ///         Ok(creds) => println!("Provisioned credentials: {}", creds.access_key_id().as_str()),
    ///         Err(e) => eprintln!("Failed to provision credentials: {}", e),
    ///     }
    ///     
    ///     Ok(())
    /// }
    /// ```
    pub async fn provision_credentials(&self, client: Client) -> Result<AwsCreds, Error> {
        let url = if let Ok(full_uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
            Some(full_uri)
        } else if let Ok(rel_uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
            Some(format!("{}{}", ECS_METADATA_BASE_URL, rel_uri))
        } else {
            None
        };

        match url {
            Some(url) => self.get_ecs_credentials(client, url).await,
            None => self.get_ec2_credentials(client).await,
        }
    }
}

#[async_trait::async_trait]
impl reqsign::AwsCredentialLoad for CachingAwsCredentialLoader {
    /// Loads AWS credentials, using cache if valid or provisioning new ones.
    ///
    /// # Returns
    /// * `Ok(Some(AwsCredential))` - Valid AWS credentials loaded from cache or fresh
    /// * `Ok(None)` - No credentials available (should not occur in normal operation)
    /// # Errors
    /// Returns an error if credential provisioning fails due to network issues,
    /// invalid environment configuration, or metadata service unavailability.
    async fn load_credential(&self, client: Client) -> anyhow::Result<Option<AwsCredential>> {
        let credentials: AwsCreds;
        match self.cache_check().await {
            Some(c) => credentials = c,
            None => {
                credentials = self.provision_credentials(client).await?;
                let mut credential_cache = self.credentials.write().await;
                *credential_cache = credentials.clone();
                // although at info level, the debug formatting is acceptable here, since
                // the message will occur only roughly every six hours
                info!(
                    "new credentials fetched and cached, expire at {:?}",
                    credentials.expiry()
                );
                // the write lock is released after this scope ends
            }
        }
        let duration = credentials
            .expiry()
            .context("credentials have no valid expiration time")?
            .duration_since(UNIX_EPOCH)
            .context("systemTime is before UNIX_EPOCH")?;
        let expiry = Utc
            .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
            .single();
        // struct AwsCredential is what the reqsign crate expects
        Ok(Some(AwsCredential {
            access_key_id: credentials.access_key_id().as_str().to_string(),
            secret_access_key: credentials.secret_access_key().as_str().to_string(),
            session_token: Some(credentials.session_token().as_str().to_string()),
            expires_in: expiry,
        }))
    }
}
