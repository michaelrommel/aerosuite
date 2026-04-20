use anyhow::Result;
use anyhow::{Context, Error, anyhow};
use http::{Method, Request};
use reqsign::Context as ReqsignContext;
use reqsign::ProvideCredential;
use reqsign::aws;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use chrono::{DateTime, TimeZone, Utc};
use metrics::{counter, gauge};
use metrics_cloudwatch_embedded::Builder;
use prometheus_parse::Scrape;
use std::{
    boxed::Box,
    collections::HashMap,
    env,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{RwLock, mpsc},
    task::JoinSet,
    time::{Duration, sleep},
};

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

#[derive(Debug, Clone)]
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
) -> Result<Vec<(String, String)>, Error> {
    // tracing_subscriber::fmt()
    //     .with_env_filter(EnvFilter::new("trace"))
    //     .init();

    let signer = aws::default_signer("servicediscovery", "eu-west-2")
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
        .uri("https://data-servicediscovery.eu-west-2.amazonaws.com")
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

    let mut endpoints: Vec<(String, String)> = vec![];
    for i in response.instances {
        if let Some(ip) = i.attributes.get("AWS_INSTANCE_IPV4") {
            endpoints.push((i.instance_id, format!("http://{}:9090/metrics", ip)));
        }
    }
    // println!("Endpoints: {:?}", endpoints);

    Ok(endpoints)
}

async fn scrape_and_record(id: &str, url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let body = reqwest::get(url).await?.text().await?;
    // println!("Body: {}", body);
    let scrape = Scrape::parse(body.lines().map(|s| Ok(s.to_owned())))?;
    // println!("Scrape: {:?}", scrape);

    // Scrape: Scrape { docs: {"ftp_command_total": "Total number of commands received.", "ftp_sessions_count": "Total number of FTP sessions.", "ftp_reply_total": "Total number of reply codes server sent to clients.", "process_virtual_memory_bytes": "Virtual memory size in bytes.", "ftp_sessions_total": "Total number of FTP sessions.", "process_cpu_seconds_total": "Total user and system CPU time spent in seconds.", "process_start_time_seconds": "Start time of the process since unix epoch in seconds.", "process_threads": "Number of OS threads in the process.", "process_max_fds": "Maximum number of open file descriptors.", "process_resident_memory_bytes": "Resident memory size in bytes.", "process_open_fds": "Number of open file descriptors."}, samples: [Sample { metric: "ftp_command_total", value: Counter(1.0), labels: Labels({"command": "quit"}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "ftp_reply_total", value: Counter(1.0), labels: Labels({"event": "quit", "range": "2xx", "event_type": "command"}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "ftp_sessions_count", value: Counter(1.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "ftp_sessions_total", value: Gauge(0.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "process_cpu_seconds_total", value: Counter(0.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "process_max_fds", value: Gauge(65535.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "process_open_fds", value: Gauge(22.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "process_resident_memory_bytes", value: Gauge(27709440.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "process_start_time_seconds", value: Gauge(1772217439.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "process_threads", value: Gauge(6.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }, Sample { metric: "process_virtual_memory_bytes", value: Gauge(45023232.0), labels: Labels({}), timestamp: 2026-02-27T20:10:33.861081004Z }] }

    for sample in scrape.samples {
        // Map labels to metrics labels
        let mut labels: Vec<(String, String)> = sample
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        labels.push(("TaskId".to_string(), id.to_string()));

        match sample.value {
            prometheus_parse::Value::Counter(v) => {
                counter!(sample.metric.clone(), &labels).absolute(v as u64);
            }
            prometheus_parse::Value::Gauge(v) => {
                gauge!(sample.metric.clone(), &labels).set(v);
            }
            prometheus_parse::Value::Untyped(v) => {
                gauge!(sample.metric.clone(), &labels).set(v);
            }
            prometheus_parse::Value::Histogram(ref v) => {
                // Histograms are complex; usually you'd iterate the buckets,
                // but for a minimal scraper, you might just grab the sum.
                let val = v.iter().map(|b| b.count).sum::<f64>();
                gauge!(sample.metric.clone(), &labels).set(val);
            }
            _ => {
                gauge!(sample.metric.clone(), &labels).set(0.0);
            }
        };
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let credential_loader = CachingAwsCredentialLoader::new();

    // Initialize the EMF Recorder
    // This will periodically flush metrics to stdout in EMF format, it runs
    // on a separate background tokio task
    let _recorder = Builder::new()
        .cloudwatch_namespace("AeroFTP")
        .with_auto_flush_interval(Duration::from_secs(30)) // Flush every minute
        .init()
        .unwrap();

    // This is the set of tasks, we spawn ourselves
    let mut set: JoinSet<Result<()>> = JoinSet::new();
    // Bounded channel with capacity of 32 messages
    // It is needed to inform the scraping task about the existing
    // endpoints
    let (tx, mut rx) = mpsc::channel(32);

    set.spawn(async move {
        // hold discovered endpoints
        let mut endpoints: Vec<(String, String)> = [].to_vec();
        loop {
            // discover all endpoints
            let updated_endpoints = get_endpoints(credential_loader.clone()).await?;
            if updated_endpoints != endpoints {
                println!("Discovered new endpoints: {:?}", updated_endpoints);
                endpoints = updated_endpoints;
                // send those over to the scraper
                tx.send(endpoints.clone()).await.unwrap();
            }
            // wait inside the task and update the endpoints again
            sleep(Duration::from_secs(60)).await;
        }
    });

    set.spawn(async move {
        let mut endpoints: Vec<(String, String)> = [].to_vec();
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    println!("Received new endpoints: {:?}", msg);
                    endpoints = msg;
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    // No message yet, totally fine!
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    println!("Sender hung up, exiting loop.");
                    break;
                }
            }

            for (id, url) in &endpoints {
                if let Err(e) = scrape_and_record(id, url).await {
                    eprintln!("Failed to scrape {}: {}", url, e);
                }
            }
            // Explicitly flush if you aren't using the auto_flush background task
            // recorder.flush(std::io::stdout());
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
        Ok(())
    });

    while let Some(res) = set.join_next().await {
        match res {
            Ok(taskresult) => match taskresult {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("A task failed: {:?}", e);
                }
            },
            Err(e) => eprintln!("A JoinHandle failed: {:?}", e),
        }
    }

    println!("All tasks joined.");

    Ok(())
}
