use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use serde::Deserialize;
use std::env;
use std::time::SystemTime;
// use tracing_subscriber::EnvFilter;
use metrics::{counter, gauge};
use metrics_cloudwatch_embedded::Builder;
use prometheus_parse::Scrape;
use std::time::Duration;
use ureq::{
    Agent,
    tls::{RootCerts, TlsConfig},
};

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ResponseBody {
    pub instances: Vec<Instance>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Instance {
    pub _id: String,
    pub attributes: Attributes,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Attributes {
    #[serde(rename = "AWS_INSTANCE_IPV4")]
    pub aws_instance_ipv4: String,

    #[serde(rename = "AWS_INSTANCE_PORT")]
    pub _aws_instance_port: String,

    pub _availability_zone: String,
    pub _deployment_id: String,
}

fn get_endpoints() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // tracing_subscriber::fmt()
    //     .with_env_filter(EnvFilter::new("trace"))
    //     .init();

    let identity = Credentials::new(
        env::var("AWS_ACCESS_KEY_ID")?,
        env::var("AWS_SECRET_ACCESS_KEY")?,
        Some(env::var("AWS_SESSION_TOKEN")?),
        None,
        "manual",
    )
    .into();

    let region = "eu-west-2";
    let service_id = "srv-jotpxhnr7nxsff4c";
    let host = format!("servicediscovery.{}.amazonaws.com", region);
    let endpoint = format!("https://{}", host);

    let payload = serde_json::json!({
        "ServiceId": service_id
    });
    let body = serde_json::to_vec(&payload)?;
    let content_length = body.len().to_string();

    let headers = [
        ("host", host.as_str()),
        ("content-length", content_length.as_str()),
        ("content-type", "application/x-amz-json-1.1"),
        ("x-amz-target", "Route53AutoNaming_v20170314.ListInstances"),
    ];

    let mut signing_settings = SigningSettings::default();
    signing_settings.payload_checksum_kind =
        aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256;

    let signing_params = v4::SigningParams::builder()
        .region(region)
        .name("servicediscovery")
        .identity(&identity)
        .settings(signing_settings)
        .time(SystemTime::now())
        .build()
        .expect("signing prams")
        .into();

    let signable =
        SignableRequest::new("POST", "/", headers.into_iter(), SignableBody::Bytes(&body))?;

    let (instruction, _) = sign(signable, &signing_params)?.into_parts();

    let agent = Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .http_status_as_error(false)
        .build()
        .new_agent();

    let mut req = agent.post(&endpoint);
    for (name, value) in headers.into_iter() {
        // println!("Adding header: {} -> {}", name, value);
        req = req.header(name, value);
    }
    for (name, value) in instruction.headers() {
        // println!("Adding header: {} -> {}", name, value);
        req = req.header(name, value);
    }

    let mut response = req.send(&body)?;

    // "Instances": [
    //     {
    //         "Id": "97e7125e4dd341cb8809ebbcac1fd402",
    //         "Attributes": {
    //             "AWS_INSTANCE_IPV4": "172.16.31.167",
    //             "AWS_INSTANCE_PORT": "21",
    //             "AvailabilityZone": "eu-west-2b",
    //             "DeploymentId": "arn:aws:ecs:eu-west-2:295934382486:task-set/aeroftp-cluster/aeroftp-service/ecs-svc/0596460527630332029"
    //         }
    //     }
    // ]

    let response = response.body_mut().read_json::<ResponseBody>()?;

    let mut endpoints: Vec<String> = vec![];

    for i in response.instances.clone() {
        endpoints.push(format!("http://{}:9090", i.attributes.aws_instance_ipv4));
    }

    Ok(endpoints)
}

async fn scrape_and_record(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let body = reqwest::get(url).await?.text().await?;
    let scrape = Scrape::parse(body.lines().map(|s| Ok(s.to_owned())))?;

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

        // Push to the metrics facade
        // Note: EMF typically handles counters and gauges as absolute values
        // depending on how you define them in the recorder configuration.
        gauge!(sample.metric.clone(), &labels).set(val);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize the EMF Recorder
    // This will periodically flush metrics to stdout in EMF format
    let _recorder = Builder::new()
        .cloudwatch_namespace("MyScrapedMetrics")
        .with_auto_flush_interval(Duration::from_secs(30)) // Flush every minute
        .init()
        .unwrap();

    // 2. Your discovery logic would provide these URLs
    let endpoints = get_endpoints()?;

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
