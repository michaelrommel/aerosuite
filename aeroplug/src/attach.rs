//! attach-eni — attach or detach a pre-existing ENI on the currently running
//! EC2 instance, looking the ENI up by any one of:
//!
//!   --eni-id      direct ENI ID         (eni-0abc1234def56789)
//!   --name        Name tag              (tag:Name filter)
//!   --description Description field     (description filter)
//!   --tag         arbitrary tag k=v     (repeatable; all must match)
//!
//! Unlike `manage-eni` (which resolves ENIs via the `aeroftp-slot` tag), this
//! tool is region-portable: operators can deploy a new region with the same
//! naming/tagging convention and use identical call parameters.
//!
//! Normal attach (ENI must already be available):
//!   attach-eni --name "aeroftp-mgmt" --attach
//!
//! Normal detach:
//!   attach-eni --name "aeroftp-mgmt" --detach
//!
//! Failover takeover (ENI is in-use on an unresponsive primary — steal it):
//!   attach-eni --name "aeroftp-mgmt" --takeover
//!
//! The --takeover action force-detaches from the current holder, polls until
//! the ENI reaches 'available', then attaches to this instance.  It is the
//! correct action when the primary is unresponsive and cannot release the ENI
//! gracefully.  --force is implied; --takeover-timeout controls the maximum
//! time spent waiting for the ENI to become available (default: 30 s).

use aerocore::{
    aws_query, extract_balanced, extract_scalar, fetch_imds_credentials, fetch_imds_instance_id,
    AwsCredentials,
};
use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Parser};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "attach-eni")]
#[command(
    about = "Attach or detach a pre-existing ENI (looked up by ID, name, description, or tag)"
)]
// Exactly one of --attach / --detach / --takeover is required.
#[command(group = ArgGroup::new("action").required(true).args(["attach", "detach", "takeover"]))]
// Exactly one ENI selector is required.
#[command(group = ArgGroup::new("selector").required(true).args(["eni_id", "name", "description", "tag"]))]
pub struct Args {
    // ── ENI selectors (pick exactly one) ─────────────────────────────────────
    /// Locate the ENI by its ID directly (e.g. eni-0abc1234def56789)
    #[arg(long = "eni-id", value_name = "ENI_ID", group = "selector")]
    eni_id: Option<String>,

    /// Locate the ENI by its Name tag (tag:Name filter)
    #[arg(long, value_name = "NAME", group = "selector")]
    name: Option<String>,

    /// Locate the ENI by its Description field
    #[arg(long, value_name = "DESC", group = "selector")]
    description: Option<String>,

    /// Locate the ENI by a tag in key=value format.
    /// Repeat to require multiple tags (AND logic).
    /// Example: --tag role=management --tag env=prod
    #[arg(long, value_name = "KEY=VALUE", group = "selector")]
    tag: Vec<String>,

    // ── Actions ───────────────────────────────────────────────────────────────
    /// Attach the ENI to this instance (ENI must be in 'available' state)
    #[arg(long, group = "action")]
    attach: bool,

    /// Detach the ENI from this instance
    #[arg(long, group = "action")]
    detach: bool,

    /// Failover takeover: force-detach from the current holder (even if
    /// unresponsive), wait until the ENI is available, then attach here.
    /// Implies --force; use when the primary is unreachable and cannot
    /// release the ENI gracefully.
    #[arg(long, group = "action")]
    takeover: bool,

    /// Maximum seconds to wait for the ENI to become 'available' after a
    /// force-detach during --takeover (default: 30)
    #[arg(long, default_value_t = 30)]
    takeover_timeout: u64,

    // ── Attach options ────────────────────────────────────────────────────────
    /// Network device index when attaching (0 = primary; secondary ENIs start at 1)
    #[arg(long, default_value_t = 1)]
    device_index: u32,

    // ── Detach options ────────────────────────────────────────────────────────
    /// Force-detach even if the OS has not yet released the interface.
    /// Use with care — may cause data loss on in-flight connections.
    #[arg(long, default_value_t = false)]
    force: bool,

    // ── Common ────────────────────────────────────────────────────────────────
    /// AWS region
    #[arg(long, default_value = "eu-west-2")]
    region: String,
}

// ── ENI selector ──────────────────────────────────────────────────────────────

/// Parsed form of whichever ENI lookup method the caller chose.
enum EniSelector<'a> {
    Id(&'a str),
    Name(&'a str),
    Description(&'a str),
    /// One or more `(key, value)` pairs; all must match (AND).
    Tags(Vec<(&'a str, &'a str)>),
}

impl Args {
    fn eni_selector(&self) -> Result<EniSelector<'_>> {
        if let Some(id) = &self.eni_id {
            return Ok(EniSelector::Id(id));
        }
        if let Some(name) = &self.name {
            return Ok(EniSelector::Name(name));
        }
        if let Some(desc) = &self.description {
            return Ok(EniSelector::Description(desc));
        }
        if !self.tag.is_empty() {
            let pairs = self
                .tag
                .iter()
                .map(|kv| {
                    kv.split_once('=')
                        .with_context(|| format!("--tag value '{kv}' is not in key=value format"))
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(EniSelector::Tags(pairs));
        }
        // Clap's ArgGroup guarantees this is unreachable, but we need to satisfy the compiler.
        bail!("No ENI selector provided (use --eni-id, --name, --description, or --tag)");
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: Args) -> Result<()> {
    let instance_id = fetch_imds_instance_id().await?;
    println!("   Running on instance: {instance_id}");

    let creds = fetch_imds_credentials().await?;
    let selector = args.eni_selector()?;

    if args.attach {
        cmd_attach(&args, &selector, &instance_id, &creds).await
    } else if args.detach {
        cmd_detach(&args, &selector, &instance_id, &creds).await
    } else {
        cmd_takeover(&args, &selector, &instance_id, &creds).await
    }
}

// ── Attach ────────────────────────────────────────────────────────────────────

async fn cmd_attach(
    args: &Args,
    selector: &EniSelector<'_>,
    instance_id: &str,
    creds: &AwsCredentials,
) -> Result<()> {
    // Look for an available ENI matching the selector.
    let (eni_id, private_ip) =
        resolve_eni(&args.region, creds, selector, Some("available")).await?;

    if let Some(ref ip) = private_ip {
        println!("   ENI primary private IP: {ip}");
    }

    let attachment_id =
        do_attach_eni(&args.region, creds, instance_id, &eni_id, args.device_index).await?;

    println!(
        "✅ Attached {eni_id} to {instance_id} as device index {}.",
        args.device_index
    );
    println!("   Attachment ID: {attachment_id}");

    Ok(())
}

// ── Detach ────────────────────────────────────────────────────────────────────

async fn cmd_detach(
    args: &Args,
    selector: &EniSelector<'_>,
    _instance_id: &str,
    creds: &AwsCredentials,
) -> Result<()> {
    // Look for an in-use ENI matching the selector; we need its attachment ID.
    let (eni_id, attachment_id) = resolve_eni_for_detach(&args.region, creds, selector).await?;

    println!("   Detaching {eni_id} (attachment: {attachment_id}) …");

    do_detach_eni(&args.region, creds, &attachment_id, args.force).await?;

    println!("✅ Detached {eni_id}.");
    println!("   The ENI is now 'available' and can be claimed by another instance.");

    Ok(())
}

// ── Takeover (failover) ──────────────────────────────────────────────────────

async fn cmd_takeover(
    args: &Args,
    selector: &EniSelector<'_>,
    instance_id: &str,
    creds: &AwsCredentials,
) -> Result<()> {
    // Step 1 — find the ENI regardless of current status.
    // We drop the status filter so we can handle both in-use and (already)
    // available ENIs gracefully.
    let xml = describe_eni_xml(&args.region, creds, selector, None).await?;

    let item_xml = extract_balanced(&xml, "item")
        .context("No ENI found matching the given selector. Check the selector and region.")?;

    if count_items(&xml) > 1 {
        eprintln!(
            "⚠️ Warning: selector matched multiple ENIs; using the first one. \
             Consider using --eni-id for an unambiguous match."
        );
    }

    let eni_id = extract_scalar(item_xml, "networkInterfaceId")
        .context("Could not extract networkInterfaceId")?
        .to_string();
    let current_status = extract_scalar(item_xml, "status")
        .context("Could not extract ENI status")?
        .to_string();

    println!("   Resolved ENI: {eni_id}  (current status: {current_status})");

    // Step 2 — if in-use, force-detach from the current holder.
    if current_status == "in-use" {
        let attachment_id = extract_balanced(item_xml, "attachment")
            .and_then(|a| extract_scalar(a, "attachmentId"))
            .map(str::to_string)
            .context("ENI is in-use but has no attachment block — cannot detach")?;

        println!("   ⚡Force-detaching from current holder (attachment: {attachment_id}) …");
        do_detach_eni(&args.region, creds, &attachment_id, true).await?;
        println!("   Detach request accepted. Waiting for ENI to become available …");

        // Step 3 — poll until available or timeout.
        poll_until_available(&args.region, creds, &eni_id, args.takeover_timeout).await?;
    } else if current_status == "available" {
        println!("   ENI is already available — skipping detach.");
    } else {
        anyhow::bail!(
            "ENI {eni_id} has unexpected status '{current_status}'; \
             expected 'in-use' or 'available'."
        );
    }

    // Step 4 — attach to this instance.
    let attachment_id =
        do_attach_eni(&args.region, creds, instance_id, &eni_id, args.device_index).await?;

    println!(
        "✅ Takeover complete. Attached {eni_id} to {instance_id} as device index {}.",
        args.device_index
    );
    println!("   Attachment ID: {attachment_id}");
    println!(
        "   Bring the interface up with:  ip link set eth{} up",
        args.device_index
    );

    Ok(())
}

// ── EC2 API calls ─────────────────────────────────────────────────────────────

/// Build the `DescribeNetworkInterfaces` parameter list for a given selector
/// and optional status filter, then call the API and return the raw XML of the
/// first matching `<item>` block.
async fn describe_eni_xml(
    region: &str,
    creds: &AwsCredentials,
    selector: &EniSelector<'_>,
    status: Option<&str>,
) -> Result<String> {
    let host = format!("ec2.{region}.amazonaws.com");

    // We build params as owned Strings first, then borrow them.
    let mut owned: Vec<(String, String)> = vec![
        ("Action".into(), "DescribeNetworkInterfaces".into()),
        ("Version".into(), "2016-11-15".into()),
    ];

    let mut filter_n: u32 = 1;

    match selector {
        EniSelector::Id(id) => {
            owned.push(("NetworkInterfaceId.1".into(), id.to_string()));
        }
        EniSelector::Name(name) => {
            owned.push((format!("Filter.{filter_n}.Name"), "tag:Name".into()));
            owned.push((format!("Filter.{filter_n}.Value.1"), name.to_string()));
            filter_n += 1;
        }
        EniSelector::Description(desc) => {
            owned.push((format!("Filter.{filter_n}.Name"), "description".into()));
            owned.push((format!("Filter.{filter_n}.Value.1"), desc.to_string()));
            filter_n += 1;
        }
        EniSelector::Tags(pairs) => {
            for (key, value) in pairs {
                owned.push((format!("Filter.{filter_n}.Name"), format!("tag:{key}")));
                owned.push((format!("Filter.{filter_n}.Value.1"), value.to_string()));
                filter_n += 1;
            }
        }
    }

    if let Some(s) = status {
        owned.push((format!("Filter.{filter_n}.Name"), "status".into()));
        owned.push((format!("Filter.{filter_n}.Value.1"), s.into()));
    }

    // Convert to &str pairs for aws_query.
    let params: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let xml = aws_query(&host, "ec2", region, creds, &params).await?;

    Ok(xml)
}

/// Resolve an ENI matching `selector` with optional `status` filter.
/// Returns `(eni_id, Option<primary_private_ip>)`.
/// Fails clearly if zero or more than one ENI matches.
async fn resolve_eni(
    region: &str,
    creds: &AwsCredentials,
    selector: &EniSelector<'_>,
    status: Option<&str>,
) -> Result<(String, Option<String>)> {
    let xml = describe_eni_xml(region, creds, selector, status).await?;

    let item_xml = extract_balanced(&xml, "item").with_context(|| {
        let status_hint = status
            .map(|s| format!(" with status '{s}'"))
            .unwrap_or_default();
        format!(
            "No ENI found matching the given selector{status_hint}. \
                 Check the ENI exists and the selector / region are correct."
        )
    })?;

    // Warn if multiple ENIs matched — we use only the first.
    if count_items(&xml) > 1 {
        eprintln!(
            "⚠️ Warning: selector matched multiple ENIs; using the first one. \
             Consider using --eni-id for an unambiguous match."
        );
    }

    let eni_id = extract_scalar(item_xml, "networkInterfaceId")
        .context("Could not extract networkInterfaceId")?
        .to_string();

    println!("   Resolved ENI: {eni_id}");

    let private_ip = extract_scalar(item_xml, "privateIpAddress").map(str::to_string);

    Ok((eni_id, private_ip))
}

/// Resolve an in-use ENI and return its `(eni_id, attachment_id)`.
async fn resolve_eni_for_detach(
    region: &str,
    creds: &AwsCredentials,
    selector: &EniSelector<'_>,
) -> Result<(String, String)> {
    let xml = describe_eni_xml(region, creds, selector, Some("in-use")).await?;

    let item_xml = extract_balanced(&xml, "item").with_context(|| {
        "No in-use ENI found matching the given selector. \
         Is it actually attached? Check the selector and region."
            .to_string()
    })?;

    if count_items(&xml) > 1 {
        eprintln!(
            "⚠️ Warning: selector matched multiple ENIs; using the first one. \
             Consider using --eni-id for an unambiguous match."
        );
    }

    let eni_id = extract_scalar(item_xml, "networkInterfaceId")
        .context("Could not extract networkInterfaceId")?
        .to_string();

    println!("   Resolved ENI: {eni_id}");

    let attachment_id = extract_balanced(item_xml, "attachment")
        .and_then(|a| extract_scalar(a, "attachmentId"))
        .map(str::to_string)
        .with_context(|| {
            format!("ENI {eni_id} has no attachment block — is it actually attached?")
        })?;

    Ok((eni_id, attachment_id))
}

/// Attach `eni_id` to `instance_id` at `device_index`.
/// Returns the new attachment ID.
///
/// EC2 API: `AttachNetworkInterface`
async fn do_attach_eni(
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
        .map(str::to_string)
}

/// Detach an ENI by its attachment ID.
///
/// EC2 API: `DetachNetworkInterface`
async fn do_detach_eni(
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

    match extract_scalar(&xml, "return") {
        Some("true") => Ok(()),
        Some(v) => bail!("DetachNetworkInterface returned unexpected value: {v}"),
        None => bail!("Could not parse DetachNetworkInterface response:\n{xml}"),
    }
}

// ── Poll helper ──────────────────────────────────────────────────────────────

/// Poll `DescribeNetworkInterfaces` every 2 s until the ENI reaches
/// `available` status or `timeout_secs` elapses.
///
/// After a `DetachNetworkInterface` call (even with `Force=true`) the ENI
/// transitions through `detaching` before it becomes `available`.  This
/// typically takes 2–5 s but can be longer under load.
async fn poll_until_available(
    region: &str,
    creds: &AwsCredentials,
    eni_id: &str,
    timeout_secs: u64,
) -> Result<()> {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_secs(2);
    let host = format!("ec2.{region}.amazonaws.com");

    loop {
        let xml = aws_query(
            &host,
            "ec2",
            region,
            creds,
            &[
                ("Action", "DescribeNetworkInterfaces"),
                ("Version", "2016-11-15"),
                ("NetworkInterfaceId.1", eni_id),
            ],
        )
        .await?;

        if let Some(item_xml) = extract_balanced(&xml, "item") {
            if let Some(status) = extract_scalar(item_xml, "status") {
                println!("  … ENI status: {status}");
                if status == "available" {
                    return Ok(());
                }
            }
        }

        if Instant::now() >= deadline {
            anyhow::bail!(
                "Timed out after {timeout_secs} s waiting for {eni_id} to become available. \
                 Check the AWS console and retry."
            );
        }

        tokio::time::sleep(poll_interval).await;
    }
}

// ── XML helpers ───────────────────────────────────────────────────────────────

/// Count how many top-level `<item>` elements are inside `<networkInterfaceSet>`.
/// Used to warn when a selector matches more than one ENI.
///
/// A naive full-string scan is wrong: the EC2 response contains many nested
/// `<item>` elements (security groups, tags, private IPs, …).  We must scope
/// the count to the `<networkInterfaceSet>` level and walk each item as a
/// balanced block so that its inner `<item>` children are skipped.
fn count_items(xml: &str) -> usize {
    use aerocore::extract_balanced;

    // Scope to networkInterfaceSet so nested <item> blocks are not counted.
    let set_xml = match extract_balanced(xml, "networkInterfaceSet") {
        Some(s) => s,
        None => return 0,
    };

    let open = "<item>";
    let close = "</item>";
    let mut count = 0;
    let mut remaining = set_xml;

    // Walk balanced item blocks: extract_balanced skips over all nested
    // <item> elements inside each ENI item, so only top-level items are
    // counted.
    while let Some(inner) = extract_balanced(remaining, "item") {
        count += 1;
        let start = remaining.find(open).unwrap() + open.len();
        remaining = &remaining[start + inner.len() + close.len()..];
    }
    count
}
