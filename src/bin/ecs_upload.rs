use aws_config::BehaviorVersion;
use chrono::{TimeZone, Utc};
use opendal::services::S3;
use opendal::Operator;
use opendal::options;
use reqsign::AwsCredential;
use reqwest::Client;
use std::boxed::Box;
use std::time::{UNIX_EPOCH};
use std::collections::HashMap;

use aws_config::meta::region::RegionProviderChain;
use aws_credential_types::provider::ProvideCredentials;
use aws_types::SdkConfig;

struct AwsCredentialLoad;

#[async_trait::async_trait]
impl reqsign::AwsCredentialLoad for AwsCredentialLoad {
    async fn load_credential(&self, _client: Client) -> anyhow::Result<Option<AwsCredential>> {
        let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");

        let config: SdkConfig = aws_config::defaults(BehaviorVersion::v2026_01_12())
            .region(region_provider)
            .load()
            .await;

        if let Some(creds_provider) = config.credentials_provider() {
            let credentials = creds_provider.provide_credentials().await?;
            let duration = credentials
                .expiry()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .expect("SystemTime is before UNIX_EPOCH");
            let expiry = Utc
                .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
                .single();
            return Ok(Some(AwsCredential {
                access_key_id: credentials.access_key_id().to_string(),
                secret_access_key: credentials.secret_access_key().to_string(),
                session_token: credentials.session_token().map(|s| s.to_string()),
                // OpenDAL will handle the actual signing; we just provide the keys
                expires_in: expiry,
            }));
        }
        Ok(None)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let builder = S3::default()
    .bucket("dev-s3-aeroftp")
    .region("eu-west-2")
    .customized_credential_load(Box::new(AwsCredentialLoad));

    let op: Operator = Operator::new(builder)?.finish();

    op.write_options(
        "fargate.txt",
        "Hello, from a Container!",
        options::WriteOptions {
            user_metadata: Some(HashMap::from([(
                "serial".to_string(),
                "123456".to_string(),
            )])),
            ..Default::default()
        },
    )
    .await?;

    Ok(())
}
