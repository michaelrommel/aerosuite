use anyhow::{Context, Error};
use chrono::{DateTime, TimeZone, Utc};
use log::{debug, info, trace, warn};
use reqsign::AwsCredential;
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

const ECS_METADATA_BASE_URL: &str = "http://169.254.170.2";
const EC2_METADATA_TOKEN_URL: &str = "http://169.254.169.254/latest/api/token";
const EC2_METADATA_IAM_ROLE_PATH: &str =
    "http://169.254.169.254/latest/meta-data/iam/security-credentials/";

/// AWS credentials structure matching the AWS Metadata Service response format.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "PascalCase")]
pub struct AwsCreds {
    /// The ARN of the IAM role.
    #[allow(dead_code)]
    pub role_arn: String,
    /// The access key ID.
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: String,
    /// The session token.
    pub token: String,
    /// The expiration time in RFC3339 format.
    pub expiration: String,
}

/// AWS credentials structure from EC2 Metadata Service response.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "PascalCase")]
pub struct Ec2Creds {
    /// The status code of the request.
    #[allow(dead_code)]
    code: String,
    /// The last update timestamp.
    #[allow(dead_code)]
    last_updated: String,
    /// The credential type.
    #[allow(dead_code)]
    #[serde(rename = "Type")]
    r#type: String,
    /// The access key ID.
    access_key_id: String,
    /// The secret access key.
    secret_access_key: String,
    /// The session token.
    token: String,
    /// The expiration time in RFC3339 format.
    expiration: String,
}

impl AwsCreds {
    /// Parses the expiration string and returns the expiry.
    /// Returns `None` if the expiration string is empty.
    pub fn expiry(&self) -> Option<SystemTime> {
        if self.expiration.is_empty() {
            None
        } else {
            DateTime::parse_from_rfc3339(&self.expiration)
                .ok()
                .map(Into::into)
        }
    }
}

/// A credential loader that caches AWS credentials and refreshes them as needed.
pub struct CachingAwsCredentialLoader {
    /// Shared mutable credentials accessible across async tasks.
    pub credentials: Arc<RwLock<AwsCreds>>,
}

impl CachingAwsCredentialLoader {
    /// Creates a new `CachingAwsCredentialLoader` with empty cached credentials.
    pub fn new() -> Self {
        Self {
            credentials: Arc::new(RwLock::new(AwsCreds::default())),
        }
    }

    /// Checks if valid cached credentials exist and are not expiring within 15 minutes.
    /// Returns the cached credentials if valid, otherwise `None`.
    pub async fn check_cache(&self) -> Option<AwsCreds> {
        // the read lock is scoped to this line, we then work on the clone
        let cached_credentials = self.credentials.read().await.clone();

        if let Some(expiry) = cached_credentials.expiry() {
            debug!("Credentials were cached, expiry is {:?}", expiry);
            if let Ok(n) = expiry.duration_since(SystemTime::now()) {
                debug!("Credentials expire in {}", n.as_secs());
                if n >= Duration::from_secs(15 * 60) {
                    return Some(cached_credentials);
                } else {
                    warn!("Credentials are expired");
                }
            }
        }

        None
    }

    /// Fetches AWS credentials from the EC2 metadata service.
    pub async fn get_ec2_credentials(&self, client: Client) -> Result<AwsCreds, Error> {
        let temp_token = client
            .put(EC2_METADATA_TOKEN_URL)
            .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
            .send()
            .await
            .context("Could not fetch temp token")?
            .text()
            .await
            .context("Failed to parse temp token")?;
        trace!("temp_token: {:?}", temp_token);
        let iam_role = client
            .get(EC2_METADATA_IAM_ROLE_PATH)
            .header("X-aws-ec2-metadata-token", &temp_token)
            .send()
            .await
            .context("Could not fetch IAM role")?
            .text()
            .await
            .context("Failed to parse IAM role")?;
        trace!("iam role: {}", iam_role);
        // let response = client
        //     .get(format!(
        //         "http://169.254.169.254/latest/meta-data/iam/security-credentials/{}",
        //         iam_role
        //     ))
        //     .header("X-aws-ec2-metadata-token", &temp_token)
        //     .send()
        //     .await
        //     .context("Could not fetch credentials")?
        //     .text()
        //     .await
        //     .unwrap();
        // trace!("Response: {:?}", response);
        let credentials = client
            .get(format!("{}{}", EC2_METADATA_IAM_ROLE_PATH, iam_role))
            .header("X-aws-ec2-metadata-token", &temp_token)
            .send()
            .await
            .context("Could not fetch credentials")?
            .json::<Ec2Creds>()
            .await
            .context("Failed to parse credentials")?;

        Ok(AwsCreds {
            role_arn: "".to_string(),
            access_key_id: credentials.access_key_id,
            secret_access_key: credentials.secret_access_key,
            token: credentials.token,
            expiration: credentials.expiration,
        })
    }

    /// Fetches AWS credentials from the ECS metadata service at the given URL.
    pub async fn get_ecs_credentials(
        &self,
        client: Client,
        url: String,
    ) -> Result<AwsCreds, Error> {
        trace!("Fetching from {}", url);
        client
            .get(url)
            .send()
            .await
            .context("Could not fetch metadata info")?
            .json::<AwsCreds>()
            .await
            .context("Failed to parse credentials")
    }

    /// Provisions AWS credentials by detecting the environment (ECS or EC2).
    /// If `AWS_CONTAINER_CREDENTIALS_*` is set, fetches from ECS.
    /// Otherwise, attempts to fetch from EC2 metadata service.
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
    async fn load_credential(&self, client: Client) -> anyhow::Result<Option<AwsCredential>> {
        let credentials: AwsCreds;
        match self.check_cache().await {
            Some(c) => credentials = c,
            None => {
                credentials = self.provision_credentials(client).await?;
                let mut credential_cache = self.credentials.write().await;
                *credential_cache = credentials.clone();
                info!(
                    "New credentials fetched and cached, expire at {:?}",
                    credentials.expiry()
                );
                // the write lock is released after this scope ends
            }
        }
        let duration = credentials
            .expiry()
            .context("Credentials have no valid expiration time")?
            .duration_since(UNIX_EPOCH)
            .context("SystemTime is before UNIX_EPOCH")?;
        let expiry = Utc
            .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
            .single();
        // struct AwsCredential is what the reqsign crate expects
        Ok(Some(AwsCredential {
            access_key_id: credentials.access_key_id,
            secret_access_key: credentials.secret_access_key,
            session_token: Some(credentials.token),
            expires_in: expiry,
        }))
    }
}
