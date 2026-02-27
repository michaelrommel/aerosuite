use anyhow::Result;
use anyhow::{Context, Error, anyhow};
use http::{Method, Request};
use reqsign::Context as ReqsignContext;
use reqsign::ProvideCredential;
use reqsign::aws;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use aws_credential_types::Credentials;
use chrono::{DateTime, TimeZone, Utc};
use metrics::{counter, gauge};
use metrics_cloudwatch_embedded::Builder;
use prometheus_parse::Scrape;
use reqwest::Client;
use std::{
    boxed::Box,
    collections::HashMap,
    env,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

use tracing_subscriber::EnvFilter;

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "PascalCase")]
struct AwsCreds {
    // this is the structure of the AWS Metadata Service response
    #[allow(dead_code)]
    role_arn: String,
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

impl AwsCreds {
    // this allows us to get the expiry as a time from the string
    pub fn expiry(&self) -> Option<SystemTime> {
        if self.expiration.is_empty() {
            None
        } else {
            Some(
                DateTime::parse_from_rfc3339(&self.expiration)
                    .unwrap()
                    .into(),
            )
        }
    }
}

#[derive(Debug)]
struct CachingAwsCredentialLoader {
    // this is a reference that can be shared across async tasks
    credentials: Arc<RwLock<AwsCreds>>,
}

impl CachingAwsCredentialLoader {
    pub fn new() -> Self {
        Self {
            credentials: Arc::new(RwLock::new(AwsCreds::default())),
        }
    }

    async fn check_cache(&self) -> Option<AwsCreds> {
        // this function is the read lock block
        let cached_credentials = self.credentials.read().await;
        if let Some(expiry) = cached_credentials.expiry() {
            // println!("Credentials were cached, expiry is {:?}", expiry);
            match expiry.duration_since(SystemTime::now()) {
                Ok(n) => {
                    // println!("Credentials expire in {}", n.as_secs());
                    if n < Duration::from_secs(15 * 60) {
                        None
                    } else {
                        Some(cached_credentials.clone())
                    }
                }
                Err(_) => {
                    println!("Credentials are expired");
                    None
                }
            }
        } else {
            None
        }
    }

    async fn get_ecs_credentials(&self) -> Result<AwsCreds, Error> {
        match env::var("AWS_SESSION_TOKEN") {
            Ok(token) => Ok(AwsCreds {
                role_arn: "unused".to_string(),
                access_key_id: env::var("AWS_ACCESS_KEY_ID")?,
                secret_access_key: env::var("AWS_SECRET_ACCESS_KEY")?,
                token: token.to_string(),
                expiration: env::var("AWS_TOKEN_EXPIRATION")?,
            }),
            Err(_) => {
                let url = if let Ok(full_uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI")
                {
                    Some(full_uri)
                } else if let Ok(rel_uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
                {
                    Some(format!("http://169.254.170.2{}", rel_uri))
                } else {
                    None
                };
                match url {
                    Some(url) => {
                        // println!("Fetching from {}", url);
                        let client = reqwest::Client::new();
                        client
                            .get(url)
                            .send()
                            .await
                            .context("Could not fetch metadata info")?
                            .json::<AwsCreds>()
                            .await
                            .context("Failed to parse credentials")
                    }
                    None => Err(anyhow!("No ECS URI set")),
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl ProvideCredential for CachingAwsCredentialLoader {
    type Credential = reqsign::aws::Credential;

    async fn provide_credential<'a, 'b>(
        &'a self,
        _ctx: &'b ReqsignContext,
    ) -> Result<Option<Self::Credential>, reqsign::Error>
    where
        Self: 'a,
    {
        let credentials: AwsCreds;
        match self.check_cache().await {
            Some(c) => credentials = c,
            None => {
                credentials = self.get_ecs_credentials().await?;
                // This creates a write lock block
                {
                    let mut credential_cache = self.credentials.write().await;
                    *credential_cache = credentials.clone();
                    println!(
                        "New credentials fetched and cached, expire at {:?}",
                        credentials.expiry()
                    );
                }
            }
        }
        let duration = credentials
            .expiry()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime is before UNIX_EPOCH");
        let expiry = Utc
            .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
            .single();
        // struct AwsCredential is what the reqsign crate expects
        Ok(Some(Self::Credential {
            access_key_id: credentials.access_key_id.clone(),
            secret_access_key: credentials.secret_access_key.clone(),
            session_token: Some(credentials.token.clone()),
            expires_in: expiry,
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DiscoverInstancesResponse {
    pub instances: Vec<Instance>,
    pub instances_revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Instance {
    pub instance_id: String,
    pub namespace_name: String,
    pub service_name: String,
    pub health_status: String,
    pub attributes: HashMap<String, String>,
}

async fn get_endpoints(
    credential_loader: CachingAwsCredentialLoader,
) -> Result<Vec<String>, Error> {
    // tracing_subscriber::fmt()
    //     .with_env_filter(EnvFilter::new("trace"))
    //     .init();

    let signer = aws::default_signer("servicediscovery", "${REGION}")
        .with_credential_provider(credential_loader);

    let payload = serde_json::json!({
        "NamespaceName": "aeroftp",
        "ServiceName": "aeroftp-service"
    });
    let body_bytes = serde_json::to_vec(&payload)?;

    let mut hasher = Sha256::new();
    hasher.update(&body_bytes);
    let body_hash = hex::encode(hasher.finalize());

    let req = Request::builder()
        .method(Method::POST)
        .uri("https://data-servicediscovery.${REGION}.amazonaws.com")
        .header("content-type", "application/x-amz-json-1.1")
        .header(
            "x-amz-target",
            "Route53AutoNaming_v20170314.DiscoverInstances",
        )
        .header("x-amz-content-sha256", body_hash)
        .body(body_bytes)?;

    let (mut parts, body) = req.into_parts();
    signer.sign(&mut parts, None).await?;
    let signed_req = Request::from_parts(parts, body);

    let client = reqwest::Client::new();
    let req = reqwest::Request::try_from(signed_req)?;
    // println!("Request: {:?}", req);

    let resp = client.execute(req).await?;

    // println!("Response: {:?}", resp);

    let response = resp.json::<DiscoverInstancesResponse>().await?;

    let mut endpoints: Vec<String> = vec![];
    for i in response.instances {
        if let Some(ip) = i.attributes.get("AWS_INSTANCE_IPV4") {
            endpoints.push(format!("http://{}:9090/metrics", ip));
        }
    }
    // println!("Endpoints: {:?}", endpoints);

    Ok(endpoints)
}

async fn scrape_and_record(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let body = reqwest::get(url).await?.text().await?;
    // println!("Body: {}", body);
    let scrape = Scrape::parse(body.lines().map(|s| Ok(s.to_owned())))?;
    // println!("Scrape: {:?}", scrape);

    for sample in scrape.samples {
        // Map labels to metrics labels
        let labels: Vec<(String, String)> = sample
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // Extract the f64 from the Value enum
        let val = match sample.value {
            prometheus_parse::Value::Counter(v) => v,
            prometheus_parse::Value::Gauge(v) => v,
            prometheus_parse::Value::Untyped(v) => v,
            prometheus_parse::Value::Histogram(ref v) => {
                // Histograms are complex; usually you'd iterate the buckets,
                // but for a minimal scraper, you might just grab the sum.
                v.iter().map(|b| b.count).sum::<f64>()
            }
            _ => 0.0, // Handle Summaries or unknown types as needed
        };

        // println!("Metric: {}, Value: {}", sample.metric, val);
        // Push to the metrics facade
        // Note: EMF typically handles counters and gauges as absolute values
        // depending on how you define them in the recorder configuration.
        gauge!(sample.metric.clone(), &labels).set(val);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let credential_loader = CachingAwsCredentialLoader::new();

    // 1. Initialize the EMF Recorder
    // This will periodically flush metrics to stdout in EMF format
    let _recorder = Builder::new()
        .cloudwatch_namespace("MyScrapedMetrics")
        .with_auto_flush_interval(Duration::from_secs(30)) // Flush every minute
        .init()
        .unwrap();

    // 2. Your discovery logic would provide these URLs
    let endpoints = get_endpoints(credential_loader).await?;

    loop {
        for url in &endpoints {
            if let Err(e) = scrape_and_record(url).await {
                eprintln!("Failed to scrape {}: {}", url, e);
            }
        }

        // Explicitly flush if you aren't using the auto_flush background task
        // recorder.flush(std::io::stdout());

        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}
