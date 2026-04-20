use anyhow::Result;
use http::{Method, Request};
use reqsign::aws;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

// {"Instances":[{"Attributes":{"AWS_INSTANCE_IPV4":"172.16.24.156","AWS_INSTANCE_PORT":"21","AvailabilityZone":"eu-west-2b","DeploymentId":"arn:aws:ecs:eu-west-2:295934382486:task-set/aeroftp-cluster/aeroftp-service/ecs-svc/9893778955264594543"},"HealthStatus":"UNKNOWN","InstanceId":"ce6d6b2eca2443ee9c2f02b661b8aa96","NamespaceName":"aeroftp","ServiceName":"aeroftp-service"},{"Attributes":{"AWS_INSTANCE_IPV4":"172.16.12.216","AWS_INSTANCE_PORT":"21","AvailabilityZone":"eu-west-2a","DeploymentId":"arn:aws:ecs:eu-west-2:295934382486:task-set/aeroftp-cluster/aeroftp-service/ecs-svc/9893778955264594543"},"HealthStatus":"UNKNOWN","InstanceId":"c6bdc0ae4fd04b98a6d6cf06d1da2df7","NamespaceName":"aeroftp","ServiceName":"aeroftp-service"}],"InstancesRevision":141862889065566542}

use std::collections::HashMap;

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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("debug"))
        .init();

    let signer = aws::default_signer("servicediscovery", "eu-west-2");

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
    println!("Request: {:?}", req);

    let resp = client.execute(req).await?;

    let response = resp.json::<DiscoverInstancesResponse>().await?;

    let mut endpoints: Vec<String> = vec![];

    for i in response.instances {
        if let Some(ip) = i.attributes.get("AWS_INSTANCE_IPV4") {
            endpoints.push(format!("http://{}:9090", ip));
        }
    }
    println!("Endpoints: {:?}", endpoints);

    Ok(())
}
