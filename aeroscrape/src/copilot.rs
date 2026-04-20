use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use std::env;
use std::time::SystemTime;
use tracing_subscriber::EnvFilter;
use ureq::{
    Agent,
    tls::{RootCerts, TlsConfig},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("trace"))
        .init();

    // 2. AWS Credentials (usually loaded from env in a real app)
    let identity = Credentials::new(
        env::var("AWS_ACCESS_KEY_ID")?,
        env::var("AWS_SECRET_ACCESS_KEY")?,
        Some(env::var("AWS_SESSION_TOKEN")?),
        None,
        "manual",
    )
    .into();

    let region = "${REGION}";
    let service_id = "srv-jotpxhnr7nxsff4c"; // Your Cloud Map Service ID
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

    let signable = SignableRequest::new(
        "POST",
        "/", // EXACT PATH REQUIRED
        headers.into_iter(),
        SignableBody::Bytes(&body),
    )?;

    println!("signable is: {:?}", signable);

    let (inst, _) = sign(signable, &signing_params)?.into_parts();

    let agent = Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .http_status_as_error(false)
        .build()
        .new_agent();
    // --- Send actual request ---
    let mut req = agent.post(&endpoint);

    for (name, value) in headers.into_iter() {
        println!("Adding header: {} -> {}", name, value);
        req = req.header(name, value);
    }
    for (name, value) in inst.headers() {
        println!("Adding header: {} -> {}", name, value);
        req = req.header(name, value);
    }
    println!("DEBUG: {:?}", req);

    let mut response = req.send(&body)?;
    println!("Status: {}", response.status());
    println!("Body: {}", response.body_mut().read_to_string()?);

    Ok(())
}
