use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "aeroscaler")]
#[command(about = "Manage the FTP backend Auto Scaling Group")]
struct Args {
    /// AWS region
    #[arg(long, global = true, default_value = "${REGION}")]
    region: String,

    /// Auto Scaling Group name
    #[arg(long, global = true, default_value = "ftp-asg")]
    asg_name: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show current ASG state: desired/min/max capacity and all running instances
    List,

    /// Set the desired number of running FTP backends
    Scale {
        /// Target number of instances (0–20)
        #[arg(long)]
        desired: u32,
    },

    /// Gracefully terminate one specific instance and decrement desired capacity.
    /// Use after draining the backend in keepalived.
    Terminate {
        /// EC2 instance ID to terminate (e.g. i-0abc1234567890def)
        #[arg(long)]
        instance_id: String,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let creds = fetch_credentials_from_imds().await?;

    match args.command {
        Command::List => cmd_list(&args.region, &args.asg_name, &creds).await,
        Command::Scale { desired } => {
            cmd_scale(&args.region, &args.asg_name, desired, &creds).await
        }
        Command::Terminate { instance_id } => {
            cmd_terminate(&args.region, &args.asg_name, &instance_id, &creds).await
        }
    }
}

// ── Subcommand implementations ────────────────────────────────────────────────

async fn cmd_list(region: &str, asg_name: &str, creds: &AwsCredentials) -> Result<()> {
    let xml = asg_api(
        region,
        creds,
        &[
            ("Action", "DescribeAutoScalingGroups"),
            ("Version", "2011-01-01"),
            ("AutoScalingGroupNames.member.1", asg_name),
        ],
    )
    .await?;

    let group = parse_asg_describe(&xml)?;

    println!("Auto Scaling Group: {}", group.name);
    println!(
        "  Capacity — desired: {}  min: {}  max: {}",
        group.desired_capacity, group.min_size, group.max_size
    );
    println!("  Instances ({}):", group.instances.len());

    if group.instances.is_empty() {
        println!("    (none)");
    } else {
        println!("    {:<25} {:<15} {:<12} {}", "Instance ID", "Health", "State", "AZ");
        println!("    {}", "-".repeat(70));
        for inst in &group.instances {
            println!(
                "    {:<25} {:<15} {:<12} {}",
                inst.instance_id, inst.health_status, inst.lifecycle_state, inst.availability_zone
            );
        }
    }

    Ok(())
}

async fn cmd_scale(
    region: &str,
    asg_name: &str,
    desired: u32,
    creds: &AwsCredentials,
) -> Result<()> {
    println!(
        "📐 Setting desired capacity of '{}' to {} ...",
        asg_name, desired
    );

    let desired_str = desired.to_string();
    let xml = asg_api(
        region,
        creds,
        &[
            ("Action", "SetDesiredCapacity"),
            ("Version", "2011-01-01"),
            ("AutoScalingGroupName", asg_name),
            ("DesiredCapacity", &desired_str),
            // Do not wait for cooldown — caller controls timing
            ("HonorCooldown", "false"),
        ],
    )
    .await?;

    // SetDesiredCapacity returns an empty success response
    if xml.contains("SetDesiredCapacityResponse") {
        println!("✅ Desired capacity set to {desired}.");
        println!("   New instances will claim a free ENI from the pool on startup.");
    } else {
        eprintln!("Unexpected response:\n{xml}");
        anyhow::bail!("SetDesiredCapacity failed");
    }

    Ok(())
}

async fn cmd_terminate(
    region: &str,
    asg_name: &str,
    instance_id: &str,
    creds: &AwsCredentials,
) -> Result<()> {
    println!(
        "🛑 Terminating instance '{}' in ASG '{}' ...",
        instance_id, asg_name
    );
    println!("   DesiredCapacity will be decremented by 1 automatically.");

    let xml = asg_api(
        region,
        creds,
        &[
            ("Action", "TerminateInstanceInAutoScalingGroup"),
            ("Version", "2011-01-01"),
            ("InstanceId", instance_id),
            // Decrement desired so the ASG does not immediately replace the instance
            ("ShouldDecrementDesiredCapacity", "true"),
        ],
    )
    .await?;

    if xml.contains("TerminateInstanceInAutoScalingGroupResponse") {
        println!("✅ Termination request accepted.");
        println!("   Run 'aeroscaler list' to watch the instance leave the group.");
    } else {
        eprintln!("Unexpected response:\n{xml}");
        anyhow::bail!("TerminateInstanceInAutoScalingGroup failed");
    }

    Ok(())
}

// ── ASG API helper ────────────────────────────────────────────────────────────

/// POST to the AutoScaling query API, sign with SigV4, return the raw XML body.
async fn asg_api(region: &str, creds: &AwsCredentials, params: &[(&str, &str)]) -> Result<String> {
    let host = format!("autoscaling.{region}.amazonaws.com");
    let endpoint = format!("https://{host}/");

    let body = serde_urlencoded::to_string(params)
        .context("Failed to URL-encode ASG request parameters")?;

    println!("\n[debug] POST body: {body}");

    let sig = sigv4_sign(
        "POST",
        &host,
        "/",
        &body,
        "autoscaling",
        region,
        &creds.access_key_id,
        &creds.secret_access_key,
        creds.session_token.as_deref(),
    );

    println!("[debug] x-amz-date:    {}", sig.x_amz_date);
    println!("[debug] authorization: {}", sig.authorization);

    let mut req = reqwest::Client::new()
        .post(&endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("x-amz-date", &sig.x_amz_date)
        .header("Authorization", &sig.authorization);

    if let Some(token) = &creds.session_token {
        req = req.header("x-amz-security-token", token);
    }

    let response = req.body(body).send().await.context("HTTP request failed")?;
    let status = response.status();
    let text = response.text().await?;

    println!("[debug] HTTP {status}");

    if !status.is_success() {
        eprintln!("\n❌ ASG API error ({status}):\n{text}");
        anyhow::bail!("ASG API request failed");
    }

    Ok(text)
}

// ── XML response parsers ──────────────────────────────────────────────────────

struct AsgGroup {
    name: String,
    desired_capacity: i64,
    min_size: i64,
    max_size: i64,
    instances: Vec<AsgInstance>,
}

struct AsgInstance {
    instance_id: String,
    availability_zone: String,
    lifecycle_state: String,
    health_status: String,
}

/// Minimal hand-rolled parser for the DescribeAutoScalingGroups XML response.
/// The response structure we care about:
///
/// <DescribeAutoScalingGroupsResponse>
///   <DescribeAutoScalingGroupsResult>
///     <AutoScalingGroups>
///       <member>
///         <AutoScalingGroupName>…</AutoScalingGroupName>
///         <DesiredCapacity>…</DesiredCapacity>
///         <MinSize>…</MinSize>
///         <MaxSize>…</MaxSize>
///         <Instances>
///           <member>
///             <InstanceId>…</InstanceId>
///             <AvailabilityZone>…</AvailabilityZone>
///             <LifecycleState>…</LifecycleState>
///             <HealthStatus>…</HealthStatus>
///           </member>
///         </Instances>
///       </member>
///     </AutoScalingGroups>
///   </DescribeAutoScalingGroupsResult>
/// </DescribeAutoScalingGroupsResponse>
fn parse_asg_describe(xml: &str) -> Result<AsgGroup> {
    fn extract_first<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let start = xml.find(&open)? + open.len();
        let end = xml[start..].find(&close)? + start;
        Some(&xml[start..end])
    }

    fn parse_i64(xml: &str, tag: &str) -> i64 {
        extract_first(xml, tag)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    let group_xml = extract_first(xml, "member")
        .context("No AutoScalingGroup member found in response")?;

    let name = extract_first(group_xml, "AutoScalingGroupName")
        .unwrap_or("(unknown)")
        .to_string();

    let desired_capacity = parse_i64(group_xml, "DesiredCapacity");
    let min_size = parse_i64(group_xml, "MinSize");
    let max_size = parse_i64(group_xml, "MaxSize");

    // Parse instances — each is a <member> inside <Instances>
    let mut instances = Vec::new();
    if let Some(instances_block) = extract_first(group_xml, "Instances") {
        let mut remaining = instances_block;
        while let Some(start) = remaining.find("<member>") {
            let after_open = start + "<member>".len();
            let end = remaining[after_open..]
                .find("</member>")
                .map(|i| i + after_open)
                .unwrap_or(remaining.len());

            let member = &remaining[after_open..end];
            instances.push(AsgInstance {
                instance_id: extract_first(member, "InstanceId")
                    .unwrap_or("(unknown)")
                    .to_string(),
                availability_zone: extract_first(member, "AvailabilityZone")
                    .unwrap_or("")
                    .to_string(),
                lifecycle_state: extract_first(member, "LifecycleState")
                    .unwrap_or("")
                    .to_string(),
                health_status: extract_first(member, "HealthStatus")
                    .unwrap_or("")
                    .to_string(),
            });

            remaining = &remaining[(end + "</member>".len())..];
        }
    }

    Ok(AsgGroup {
        name,
        desired_capacity,
        min_size,
        max_size,
        instances,
    })
}

// ── SigV4 signing ─────────────────────────────────────────────────────────────

struct SigV4Result {
    authorization: String,
    x_amz_date: String,
}

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

    // Canonical headers — sorted lexicographically
    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), "application/x-www-form-urlencoded".into()),
        ("host".into(), host.into()),
        ("x-amz-date".into(), amz_date.clone()),
    ];
    if let Some(token) = session_token {
        headers.push(("x-amz-security-token".into(), token.into()));
    }
    headers.sort_by(|(a, _), (b, _)| a.cmp(b));

    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers: String = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let body_hash = sha256_hex(body.as_bytes());
    let canonical_request =
        format!("{method}\n{path}\n\n{canonical_headers}\n{signed_headers}\n{body_hash}");

    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_secret = format!("AWS4{secret_access_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");

    let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));
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
const TOKEN_TTL_SECONDS: u32 = 21600;

struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

async fn fetch_credentials_from_imds() -> Result<AwsCredentials> {
    let client = reqwest::Client::new();

    println!("🔑 Fetching credentials from IMDSv2...");

    let imds_token = client
        .put(format!("{IMDS_ENDPOINT}/latest/api/token"))
        .header("X-aws-ec2-metadata-token-ttl-seconds", TOKEN_TTL_SECONDS.to_string())
        .send()
        .await
        .context("Failed to reach IMDS — is this running on an EC2 instance?")?
        .error_for_status()
        .context("IMDS token request failed")?
        .text()
        .await
        .context("Failed to read IMDS token")?;

    let role_name = client
        .get(format!("{IMDS_ENDPOINT}/latest/meta-data/iam/security-credentials/"))
        .header("X-aws-ec2-metadata-token", &imds_token)
        .send()
        .await
        .context("Failed to fetch IAM role name")?
        .error_for_status()
        .context("IMDS role-name request failed")?
        .text()
        .await
        .context("Failed to read IAM role name")?;

    println!("   IAM role: {}", role_name.trim());

    let creds_json = client
        .get(format!(
            "{IMDS_ENDPOINT}/latest/meta-data/iam/security-credentials/{}",
            role_name.trim()
        ))
        .header("X-aws-ec2-metadata-token", &imds_token)
        .send()
        .await
        .context("Failed to fetch IAM credentials")?
        .error_for_status()
        .context("IMDS credentials request failed")?
        .text()
        .await
        .context("Failed to read IAM credentials")?;

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

    let creds: ImdsCredentials =
        serde_json::from_str(&creds_json).context("Failed to parse IMDS credentials JSON")?;

    println!("   Credentials valid until: {}", creds.expiration);

    Ok(AwsCredentials {
        access_key_id: creds.access_key_id,
        secret_access_key: creds.secret_access_key,
        session_token: creds.token,
    })
}
