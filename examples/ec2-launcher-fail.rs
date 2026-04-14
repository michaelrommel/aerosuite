use anyhow::{Context, Result};
use clap::Parser;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "aeroscaler")]
#[command(about = "Launch EC2 instances with custom parameters")]
struct Args {
    /// The AMI ID for the instance
    #[arg(long)]
    image_id: String,

    /// Instance type (e.g., t3.micro)
    #[arg(long, default_value = "t3.micro")]
    instance_type: String,

    /// AWS region
    #[arg(long, default_value = "${REGION}")]
    region: String,

    /// Key pair name
    #[arg(long, default_value = "ec2-user")]
    key_name: String,

    /// Subnet ID
    #[arg(long)]
    subnet_id: String,

    /// Security group IDs (repeatable: --security-group-ids sg-aaa --security-group-ids sg-bbb)
    #[arg(long)]
    security_group_ids: Vec<String>,

    /// IAM instance profile name
    #[arg(long, default_value = "ecsInstanceRole")]
    iam_instance_profile: String,

    /// Whether to associate a public IP address
    #[arg(long)]
    associate_public_ip_address: bool,
}

// ── EC2 parameters ────────────────────────────────────────────────────────────

fn build_params(args: &Args) -> Vec<(String, String)> {
    let mut p = Vec::new();
    p.push(("Action".into(), "RunInstances".into()));
    p.push(("Version".into(), "2016-11-15".into()));
    p.push(("ImageId".into(), args.image_id.clone()));
    p.push(("InstanceType".into(), args.instance_type.clone()));
    p.push(("KeyName".into(), args.key_name.clone()));
    p.push(("MinCount".into(), "1".into()));
    p.push(("MaxCount".into(), "1".into()));

    if !args.iam_instance_profile.is_empty() {
        p.push(("IamInstanceProfile.Name".into(), args.iam_instance_profile.clone()));
    }

    // SubnetId + SecurityGroupId + AssociatePublicIpAddress must live inside
    // a NetworkInterface block — they are not valid as standalone top-level params.
    p.push(("NetworkInterface.1.DeviceIndex".into(), "0".into()));
    if !args.subnet_id.is_empty() {
        p.push(("NetworkInterface.1.SubnetId".into(), args.subnet_id.clone()));
    }
    for (i, sg) in args.security_group_ids.iter().enumerate() {
        p.push((format!("NetworkInterface.1.SecurityGroupId.{}", i + 1), sg.clone()));
    }
    if args.associate_public_ip_address {
        p.push(("NetworkInterface.1.AssociatePublicIpAddress".into(), "true".into()));
    }

    p
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let creds = fetch_credentials_from_imds().await?;

    let body = serde_urlencoded::to_string(&build_params(&args))
        .context("Failed to URL-encode request parameters")?;

    let host = format!("ec2.{}.amazonaws.com", args.region);
    let endpoint = format!("https://{}/", host);

    println!("\n[debug] POST body (form params):");
    for kv in body.split('&') {
        println!("        {}", kv);
    }

    // Sign the request with a real SHA-256 body hash (UNSIGNED-PAYLOAD causes
    // AuthFailure for EC2 — that is an S3-only shortcut).
    let sig = sigv4_sign(
        "POST",
        &host,
        "/",
        &body,
        "ec2",
        &args.region,
        &creds.access_key_id,
        &creds.secret_access_key,
        creds.session_token.as_deref(),
    );

    println!("\n[debug] signed headers:");
    println!("        x-amz-date:             {}", sig.x_amz_date);
    println!("        x-amz-security-token:   {}...{} ({} chars)",
        &creds.session_token.as_deref().unwrap_or("")[..8],
        &creds.session_token.as_deref().unwrap_or("").chars().rev().take(4).collect::<String>().chars().rev().collect::<String>(),
        creds.session_token.as_deref().unwrap_or("").len(),
    );
    println!("        authorization:          {}", sig.authorization);

    println!("\n[debug] credential:");
    println!("        access_key_id : {}", creds.access_key_id);
    println!("        secret_key    : {}... (first 4 chars)", &creds.secret_access_key[..4]);
    println!("        session_token : {}", if creds.session_token.is_some() { "present" } else { "MISSING" });
    println!("\n[debug] endpoint: POST {}", endpoint);

    // Build and send the request
    let mut request_builder = reqwest::Client::new()
        .post(&endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("x-amz-date", &sig.x_amz_date)
        .header("Authorization", &sig.authorization);

    if let Some(token) = &creds.session_token {
        request_builder = request_builder.header("x-amz-security-token", token);
    }

    println!("\n📡 Sending request to EC2 API...");

    let response = request_builder
        .body(body)
        .send()
        .await
        .context("HTTP request failed")?;

    let status = response.status();
    let body = response.text().await?;

    if status.is_success() {
        println!("\n✅ Instance launch successful!");
        println!("\n📋 Response:");
        print_xml_response(&body);
    } else {
        eprintln!("\n❌ Request failed ({})", status);
        eprintln!("{}", body);
        anyhow::bail!("EC2 API request failed");
    }

    Ok(())
}

// ── SigV4 signing ─────────────────────────────────────────────────────────────

struct SigV4Result {
    authorization: String,
    x_amz_date: String,
}

/// Sign an AWS request with Signature Version 4.
///
/// Returns the `Authorization` and `x-amz-date` header values.
/// The caller is responsible for adding `x-amz-security-token` separately
/// when temporary credentials are in use.
fn sigv4_sign(
    method: &str,
    host: &str,
    path: &str,
    body: &str,
    service: &str,
    region: &str,
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
) -> SigV4Result {
    let now = chrono::Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();

    // Step 1 — canonical headers (must be sorted lexicographically by name)
    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/x-www-form-urlencoded".into()),
        ("host".into(), host.into()),
        ("x-amz-date".into(), amz_date.clone()),
    ];
    if let Some(token) = session_token {
        headers.push(("x-amz-security-token".into(), token.into()));
    }
    headers.sort_by(|(a, _), (b, _)| a.cmp(b));

    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect();

    let signed_headers: String = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    // Step 2 — canonical request
    // Body params go in the body (not the query string), so query string is empty.
    let body_hash = sha256_hex(body.as_bytes());
    let canonical_request = format!(
        "{method}\n{path}\n\n{canonical_headers}\n{signed_headers}\n{body_hash}"
    );

    println!("\n[debug] canonical request:\n---\n{}\n---", canonical_request);
    println!("[debug] body SHA-256: {}", body_hash);

    // Step 3 — string to sign
    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    // Step 4 — signing key
    let k_secret = format!("AWS4{secret_access_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");

    // Step 5 — signature
    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

    // Step 6 — Authorization header
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key_id}/{credential_scope}, \
         SignedHeaders={signed_headers}, Signature={signature}"
    );

    SigV4Result { authorization, x_amz_date: amz_date }
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

// ── IMDSv2 credential fetching ────────────────────────────────────────────────

const IMDS_ENDPOINT: &str = "http://169.254.169.254";
const TOKEN_TTL_SECONDS: u32 = 21600; // 6 hours

struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

/// Fetch temporary IAM credentials via IMDSv2.
///
///   1. PUT  /latest/api/token                             → IMDS session token
///   2. GET  /latest/meta-data/iam/security-credentials/  → IAM role name
///   3. GET  /latest/meta-data/iam/security-credentials/{role} → credentials
async fn fetch_credentials_from_imds() -> Result<AwsCredentials> {
    let client = reqwest::Client::new();

    println!("🔑 Fetching credentials from IMDSv2...");

    // Step 1 — IMDSv2 session token
    let imds_token = client
        .put(format!("{IMDS_ENDPOINT}/latest/api/token"))
        .header("X-aws-ec2-metadata-token-ttl-seconds", TOKEN_TTL_SECONDS.to_string())
        .send()
        .await
        .context("Failed to reach IMDS — is this running on an EC2 instance?")?
        .error_for_status()
        .context("IMDS token request returned an error")?
        .text()
        .await
        .context("Failed to read IMDS token")?;

    // Step 2 — IAM role name attached to this instance
    let role_name = client
        .get(format!("{IMDS_ENDPOINT}/latest/meta-data/iam/security-credentials/"))
        .header("X-aws-ec2-metadata-token", &imds_token)
        .send()
        .await
        .context("Failed to fetch IAM role name from IMDS")?
        .error_for_status()
        .context("IMDS role-name request returned an error")?
        .text()
        .await
        .context("Failed to read IAM role name")?;

    println!("   IAM role: {}", role_name.trim());

    // Step 3 — Temporary credentials for that role
    let creds_json = client
        .get(format!(
            "{IMDS_ENDPOINT}/latest/meta-data/iam/security-credentials/{}",
            role_name.trim()
        ))
        .header("X-aws-ec2-metadata-token", &imds_token)
        .send()
        .await
        .context("Failed to fetch IAM credentials from IMDS")?
        .error_for_status()
        .context("IMDS credentials request returned an error")?
        .text()
        .await
        .context("Failed to read IAM credentials")?;

    // IMDS returns PascalCase keys:
    // { "AccessKeyId": "…", "SecretAccessKey": "…", "Token": "…", "Expiration": "…" }
    #[derive(Deserialize)]
    struct ImdsCredentials {
        #[serde(rename = "AccessKeyId")]
        access_key_id: String,
        #[serde(rename = "SecretAccessKey")]
        secret_access_key: String,
        #[serde(rename = "Token")]
        token: Option<String>,
        #[serde(rename = "Expiration")]
        expiration: String,
    }

    let creds: ImdsCredentials = serde_json::from_str(&creds_json)
        .context("Failed to parse IMDS credentials JSON")?;

    println!("   Credentials valid until: {}", creds.expiration);

    Ok(AwsCredentials {
        access_key_id: creds.access_key_id,
        secret_access_key: creds.secret_access_key,
        session_token: creds.token,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn print_xml_response(xml: &str) {
    for line in xml.lines() {
        if !line.trim().is_empty() {
            println!("  {}", line);
        }
    }
}
