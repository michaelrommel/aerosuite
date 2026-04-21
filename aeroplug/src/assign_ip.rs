//! ip — assign or unassign a secondary private IP address on an ENI.

use aerocore::{
    aws_query, extract_scalar, fetch_imds_credentials, fetch_imds_path, fetch_imds_token,
    AwsCredentials,
};
use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Parser};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Assign or unassign a secondary private IP on an ENI")]
#[command(group = ArgGroup::new("action").required(true).args(["assign", "unassign"]))]
pub struct Args {
    /// The private IP address to assign/unassign (e.g. 172.16.32.50)
    #[arg(long)]
    ip: String,

    /// ENI to operate on. Defaults to the primary ENI of this instance.
    #[arg(long)]
    eni: Option<String>,

    /// Assign the IP as a secondary private address on the ENI
    #[arg(long, group = "action")]
    assign: bool,

    /// Remove the secondary private address from the ENI
    #[arg(long, group = "action")]
    unassign: bool,

    /// Allow the IP to be reassigned even if currently held by another ENI.
    #[arg(long, default_value_t = false)]
    allow_reassignment: bool,

    /// AWS region
    #[arg(long, default_value = "eu-west-2")]
    region: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: Args) -> Result<()> {
    let creds = fetch_imds_credentials().await?;

    let eni_id = match args.eni.clone() {
        Some(id) => {
            println!("  Using ENI (explicit): {id}");
            id
        }
        None => {
            let id = fetch_primary_eni_id().await?;
            println!("  Using ENI (primary, from IMDS): {id}");
            id
        }
    };

    if args.assign {
        cmd_assign(&args, &eni_id, &creds).await
    } else {
        cmd_unassign(&args, &eni_id, &creds).await
    }
}

// ── Assign / Unassign ─────────────────────────────────────────────────────────

async fn cmd_assign(args: &Args, eni_id: &str, creds: &AwsCredentials) -> Result<()> {
    if args.allow_reassignment {
        println!("  ⚡ AllowReassignment=true — will steal IP from current holder if necessary.");
    }
    assign_secondary_ip(&args.region, creds, eni_id, &args.ip, args.allow_reassignment).await?;
    println!("✅ Assigned secondary IP {} to {eni_id}.", args.ip);
    println!("   Add the address in the OS with:");
    println!("     ip addr add {}/32 dev <interface>", args.ip);
    Ok(())
}

async fn cmd_unassign(args: &Args, eni_id: &str, creds: &AwsCredentials) -> Result<()> {
    unassign_secondary_ip(&args.region, creds, eni_id, &args.ip).await?;
    println!("✅ Unassigned secondary IP {} from {eni_id}.", args.ip);
    println!("   Remove the address in the OS with:");
    println!("     ip addr del {}/32 dev <interface>", args.ip);
    Ok(())
}

// ── IMDS helpers ──────────────────────────────────────────────────────────────

async fn fetch_primary_eni_id() -> Result<String> {
    let token = fetch_imds_token().await?;
    let mac = fetch_imds_path(&token, "mac")
        .await
        .context("Failed to fetch primary MAC address from IMDS")?;
    println!("  Primary MAC: {mac}");
    let eni_id = fetch_imds_path(
        &token,
        &format!("network/interfaces/macs/{mac}/interface-id"),
    )
    .await
    .with_context(|| format!("Failed to fetch ENI ID for MAC {mac} from IMDS"))?;
    Ok(eni_id)
}

// ── EC2 API calls ─────────────────────────────────────────────────────────────

async fn assign_secondary_ip(
    region: &str,
    creds: &AwsCredentials,
    eni_id: &str,
    ip: &str,
    allow_reassignment: bool,
) -> Result<()> {
    let host = format!("ec2.{region}.amazonaws.com");
    let reassign_str = allow_reassignment.to_string();
    println!("  Assigning {ip} to {eni_id} …");
    let xml = aws_query(
        &host, "ec2", region, creds,
        &[
            ("Action", "AssignPrivateIpAddresses"),
            ("Version", "2016-11-15"),
            ("NetworkInterfaceId", eni_id),
            ("PrivateIpAddress.1", ip),
            ("AllowReassignment", &reassign_str),
        ],
    )
    .await?;
    match extract_scalar(&xml, "return") {
        Some("true") => Ok(()),
        Some(v) => bail!("AssignPrivateIpAddresses returned unexpected value: {v}"),
        None => bail!("Could not parse AssignPrivateIpAddresses response:\n{xml}"),
    }
}

async fn unassign_secondary_ip(
    region: &str,
    creds: &AwsCredentials,
    eni_id: &str,
    ip: &str,
) -> Result<()> {
    let host = format!("ec2.{region}.amazonaws.com");
    println!("  Unassigning {ip} from {eni_id} …");
    let xml = aws_query(
        &host, "ec2", region, creds,
        &[
            ("Action", "UnassignPrivateIpAddresses"),
            ("Version", "2016-11-15"),
            ("NetworkInterfaceId", eni_id),
            ("PrivateIpAddress.1", ip),
        ],
    )
    .await?;
    match extract_scalar(&xml, "return") {
        Some("true") => Ok(()),
        Some(v) => bail!("UnassignPrivateIpAddresses returned unexpected value: {v}"),
        None => bail!("Could not parse UnassignPrivateIpAddresses response:\n{xml}"),
    }
}
