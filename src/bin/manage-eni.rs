//! manage-eni — find an ENI tagged `aeroftp-slot=<n>` and attach or detach it
//! on the currently running EC2 instance.
//!
//! Attach (typical invocation from instance userdata):
//!   eni-attach --slot 3 --attach
//!
//! Detach (called before graceful shutdown / scale-in):
//!   eni-attach --slot 3 --detach
//!
//! The ENI must already exist (pre-created by the operator). The tag
//! `aeroftp-slot=<n>` identifies which fixed IP from the 172.16.32.20-39
//! pool this ENI carries.

use aeroscaler::{
    aws_query, extract_balanced, extract_scalar, fetch_imds_credentials,
    fetch_imds_instance_id, AwsCredentials,
};
use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Parser};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "manage-eni")]
#[command(about = "Attach or detach an ENI from the aeroftp slot pool on this instance")]
// Exactly one of --attach / --detach is required.
#[command(group = ArgGroup::new("action").required(true).args(["attach", "detach"]))]
struct Args {
    /// Slot number (0–19), matched against the ENI tag `aeroftp-slot=<n>`
    #[arg(long)]
    slot: u32,

    /// Attach the ENI for this slot to the current instance
    #[arg(long, group = "action")]
    attach: bool,

    /// Detach the ENI for this slot from the current instance
    #[arg(long, group = "action")]
    detach: bool,

    /// AWS region
    #[arg(long, default_value = "${REGION}")]
    region: String,

    /// Network device index used when attaching.
    /// 0 is always the primary interface; secondary ENIs start at 1.
    #[arg(long, default_value_t = 1)]
    device_index: u32,

    /// Force-detach even if the OS has not yet released the interface.
    /// Use with care — may cause data loss on in-flight connections.
    #[arg(long, default_value_t = false)]
    force: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let instance_id = fetch_imds_instance_id().await?;
    println!("📍 Running on instance: {instance_id}");

    let creds = fetch_imds_credentials().await?;

    if args.attach {
        cmd_attach(&args, &instance_id, &creds).await
    } else {
        cmd_detach(&args, &instance_id, &creds).await
    }
}

// ── Attach ────────────────────────────────────────────────────────────────────

async fn cmd_attach(args: &Args, instance_id: &str, creds: &AwsCredentials) -> Result<()> {
    let (eni_id, _) = find_slot_eni(&args.region, creds, args.slot, "available").await?;
    println!("🔎 Found free ENI for slot {}: {eni_id}", args.slot);

    let attachment_id =
        attach_eni(&args.region, creds, instance_id, &eni_id, args.device_index).await?;

    println!(
        "✅ Attached {eni_id} to {instance_id} as device index {}.",
        args.device_index
    );
    println!("   Attachment ID: {attachment_id}");
    println!("   Bring the interface up with:  ip link set eth1 up");

    Ok(())
}

// ── Detach ────────────────────────────────────────────────────────────────────

async fn cmd_detach(args: &Args, _instance_id: &str, creds: &AwsCredentials) -> Result<()> {
    // For detach we look for an in-use ENI — its attachment block carries the
    // attachment ID we need to pass to DetachNetworkInterface.
    let (eni_id, attachment_id) =
        find_slot_eni(&args.region, creds, args.slot, "in-use").await?;

    let attachment_id = attachment_id.context(format!(
        "ENI {eni_id} (slot {}) has no attachment ID — is it actually attached?",
        args.slot
    ))?;

    println!("🔎 Found attached ENI for slot {}: {eni_id}  (attachment: {attachment_id})", args.slot);

    detach_eni(&args.region, creds, &attachment_id, args.force).await?;

    println!("✅ Detached {eni_id} (slot {}).", args.slot);
    println!("   The ENI is now 'available' and can be claimed by another instance.");
    println!("   Remove the IP from keepalived if you haven't already.");

    Ok(())
}

// ── EC2 API calls ─────────────────────────────────────────────────────────────

/// Find a single ENI tagged `aeroftp-slot=<slot>` with the given `status`
/// (`"available"` for attach, `"in-use"` for detach).
///
/// Returns `(eni_id, Option<attachment_id>)`.
/// The attachment ID is `Some(...)` only when the ENI is currently attached.
///
/// EC2 API: `DescribeNetworkInterfaces`
/// Response uses `<item>` (not `<member>`) for its lists.
///
/// ```xml
/// <networkInterfaceSet>
///   <item>
///     <networkInterfaceId>eni-…</networkInterfaceId>
///     <status>in-use</status>
///     <privateIpAddress>172.16.32.23</privateIpAddress>
///     <attachment>
///       <attachmentId>eni-attach-…</attachmentId>
///       <instanceId>i-…</instanceId>
///       <status>attached</status>
///     </attachment>
///   </item>
/// </networkInterfaceSet>
/// ```
async fn find_slot_eni(
    region: &str,
    creds: &AwsCredentials,
    slot: u32,
    status: &str,
) -> Result<(String, Option<String>)> {
    let host = format!("ec2.{region}.amazonaws.com");
    let slot_str = slot.to_string();

    let xml = aws_query(
        &host,
        "ec2",
        region,
        creds,
        &[
            ("Action", "DescribeNetworkInterfaces"),
            ("Version", "2016-11-15"),
            ("Filter.1.Name", "tag:aeroftp-slot"),
            ("Filter.1.Value.1", &slot_str),
            ("Filter.2.Name", "status"),
            ("Filter.2.Value.1", status),
        ],
    )
    .await?;

    let item_xml = extract_balanced(&xml, "item").context(format!(
        "No {status} ENI found for aeroftp-slot={slot}. \
         Check the ENI exists, is tagged correctly, and has status '{status}'."
    ))?;

    let eni_id = extract_scalar(item_xml, "networkInterfaceId")
        .context("Could not extract networkInterfaceId")?
        .to_string();

    if let Some(ip) = extract_scalar(item_xml, "privateIpAddress") {
        println!("   Private IP: {ip}");
    }

    // Extract the attachment ID if present (only set when status = "in-use")
    let attachment_id = extract_balanced(item_xml, "attachment")
        .and_then(|a| extract_scalar(a, "attachmentId"))
        .map(|s| s.to_string());

    Ok((eni_id, attachment_id))
}

/// Attach `eni_id` to `instance_id` at `device_index`.
/// Returns the new attachment ID.
///
/// EC2 API: `AttachNetworkInterface`
async fn attach_eni(
    region: &str,
    creds: &AwsCredentials,
    instance_id: &str,
    eni_id: &str,
    device_index: u32,
) -> Result<String> {
    let host = format!("ec2.{region}.amazonaws.com");
    let device_str = device_index.to_string();

    let xml = aws_query(
        &host,
        "ec2",
        region,
        creds,
        &[
            ("Action", "AttachNetworkInterface"),
            ("Version", "2016-11-15"),
            ("NetworkInterfaceId", eni_id),
            ("InstanceId", instance_id),
            ("DeviceIndex", &device_str),
        ],
    )
    .await?;

    extract_scalar(&xml, "attachmentId")
        .context("Could not extract attachmentId from AttachNetworkInterface response")
        .map(|s| s.to_string())
}

/// Detach an ENI by its attachment ID.
///
/// EC2 API: `DetachNetworkInterface`
async fn detach_eni(
    region: &str,
    creds: &AwsCredentials,
    attachment_id: &str,
    force: bool,
) -> Result<()> {
    let host = format!("ec2.{region}.amazonaws.com");
    let force_str = force.to_string();

    let xml = aws_query(
        &host,
        "ec2",
        region,
        creds,
        &[
            ("Action", "DetachNetworkInterface"),
            ("Version", "2016-11-15"),
            ("AttachmentId", attachment_id),
            ("Force", &force_str),
        ],
    )
    .await?;

    // Response: <DetachNetworkInterfaceResponse>
    //             <return>true</return>
    //           </DetachNetworkInterfaceResponse>
    match extract_scalar(&xml, "return") {
        Some("true") => Ok(()),
        Some(v) => bail!("DetachNetworkInterface returned unexpected value: {v}"),
        None => bail!("Could not parse DetachNetworkInterface response:\n{xml}"),
    }
}
