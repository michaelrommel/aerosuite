//! AWS utilities: IMDSv2 credential fetching, SigV4 signing, generic Query API
//! helper, and XML parsing helpers.
//!
//! This is the content that previously lived in `aeroscaler/src/lib.rs`.

use anyhow::{Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

// ── Credentials ───────────────────────────────────────────────────────────────

pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

// ── IMDSv2 ────────────────────────────────────────────────────────────────────

const IMDS_ENDPOINT: &str = "http://169.254.169.254";
const TOKEN_TTL_SECONDS: u32 = 21600; // 6 hours

/// Obtain an IMDSv2 session token.
pub async fn fetch_imds_token() -> Result<String> {
    reqwest::Client::new()
        .put(format!("{IMDS_ENDPOINT}/latest/api/token"))
        .header(
            "X-aws-ec2-metadata-token-ttl-seconds",
            TOKEN_TTL_SECONDS.to_string(),
        )
        .send()
        .await
        .context("Failed to reach IMDS — is this running on an EC2 instance?")?
        .error_for_status()
        .context("IMDS token request failed")?
        .text()
        .await
        .context("Failed to read IMDS token")
}

/// Fetch a single value from the EC2 instance metadata service.
/// `path` is relative to `/latest/meta-data/`, e.g. `"instance-id"`.
pub async fn fetch_imds_path(token: &str, path: &str) -> Result<String> {
    reqwest::Client::new()
        .get(format!("{IMDS_ENDPOINT}/latest/meta-data/{path}"))
        .header("X-aws-ec2-metadata-token", token)
        .send()
        .await
        .with_context(|| format!("Failed to fetch IMDS path '{path}'"))?
        .error_for_status()
        .with_context(|| format!("IMDS returned error for path '{path}'"))?
        .text()
        .await
        .map(|s| s.trim().to_string())
        .with_context(|| format!("Failed to read IMDS response for '{path}'"))
}

/// Fetch temporary IAM credentials from EC2 Instance Metadata Service v2.
pub async fn fetch_imds_credentials() -> Result<AwsCredentials> {
    let token = fetch_imds_token().await?;

    println!("   Fetching credentials from IMDSv2...");

    let role_name = fetch_imds_path(&token, "iam/security-credentials/")
        .await
        .context("Failed to fetch IAM role name")?;

    println!("   IAM role: {role_name}");

    let creds_json = fetch_imds_path(&token, &format!("iam/security-credentials/{role_name}"))
        .await
        .context("Failed to fetch IAM credentials")?;

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

    let c: ImdsCredentials =
        serde_json::from_str(&creds_json).context("Failed to parse IMDS credentials JSON")?;

    println!("   Credentials valid until: {}", c.expiration);

    Ok(AwsCredentials {
        access_key_id: c.access_key_id,
        secret_access_key: c.secret_access_key,
        session_token: c.token,
    })
}

/// Fetch the EC2 instance ID of the currently running instance from IMDS.
pub async fn fetch_imds_instance_id() -> Result<String> {
    let token = fetch_imds_token().await?;
    fetch_imds_path(&token, "instance-id").await
}

// ── SigV4 signing ─────────────────────────────────────────────────────────────

pub struct SigV4Result {
    pub authorization: String,
    pub x_amz_date: String,
}

/// Sign an AWS Query API request with Signature Version 4.
pub fn sigv4_sign(
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

    let mut headers: Vec<(String, String)> = vec![
        (
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
        ("host".into(), host.into()),
        ("x-amz-date".into(), amz_date.clone()),
    ];
    if let Some(token) = session_token {
        headers.push(("x-amz-security-token".into(), token.into()));
    }
    headers.sort_by(|(a, _), (b, _)| a.cmp(b));

    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers: String = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

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

    SigV4Result {
        authorization,
        x_amz_date: amz_date,
    }
}

// ── Generic AWS Query API helper ──────────────────────────────────────────────

/// POST to an AWS Query API endpoint, sign with SigV4, return raw XML.
pub async fn aws_query(
    host: &str,
    service: &str,
    region: &str,
    creds: &AwsCredentials,
    params: &[(&str, &str)],
) -> Result<String> {
    let endpoint = format!("https://{host}/");
    let body =
        serde_urlencoded::to_string(params).context("Failed to URL-encode request parameters")?;

    let sig = sigv4_sign(
        "POST",
        host,
        "/",
        &body,
        service,
        region,
        &creds.access_key_id,
        &creds.secret_access_key,
        creds.session_token.as_deref(),
    );

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

    if !status.is_success() {
        anyhow::bail!("AWS API error ({status}): {text}");
    }

    Ok(text)
}

// ── XML helpers ───────────────────────────────────────────────────────────────

/// Extract content between the first `<tag>` and its correctly balanced `</tag>`.
pub fn extract_balanced<'a>(haystack: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let content_start = haystack.find(&open)? + open.len();
    let mut depth = 1usize;
    let mut pos = content_start;
    loop {
        let next_open = haystack[pos..].find(&open).map(|i| pos + i);
        let next_close = haystack[pos..].find(&close).map(|i| pos + i);
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                pos = o + open.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                if depth == 0 {
                    return Some(&haystack[content_start..c]);
                }
                pos = c + close.len();
            }
            _ => return None,
        }
    }
}

/// Extract the text content of a simple scalar tag (no nesting).
pub fn extract_scalar<'a>(haystack: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = haystack.find(&open)? + open.len();
    let end = haystack[start..].find(&close)? + start;
    Some(haystack[start..end].trim())
}

/// Extract the text content of **every** occurrence of a scalar tag.
/// Useful when the same tag appears multiple times at different nesting depths
/// and you want all values (e.g. all `<privateIpAddress>` inside an instance).
pub fn extract_all_scalars(haystack: &str, tag: &str) -> Vec<String> {
    let open  = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut result = Vec::new();
    let mut pos = 0;
    while let Some(rel_start) = haystack[pos..].find(&open) {
        let content_start = pos + rel_start + open.len();
        if let Some(rel_end) = haystack[content_start..].find(&close) {
            result.push(haystack[content_start..content_start + rel_end].trim().to_string());
            pos = content_start + rel_end + close.len();
        } else {
            break;
        }
    }
    result
}

// ── Crypto helpers ────────────────────────────────────────────────────────────

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}
