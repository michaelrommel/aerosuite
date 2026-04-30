//! keepalived-config — generate the VRRP include file and notify scripts for a
//! two-node keepalived HA pair running on EC2.
//!
//! The generated config is designed to be include'd by the operator's existing
//! keepalived.conf, leaving the virtual_server blocks and global_defs untouched:
//!
//!   # append to /etc/keepalived/keepalived.conf:
//!   include "/etc/keepalived/vrrp.conf"
//!
//!
//! Prerequisites
//! ─────────────
//! • "Instance tags in metadata" must be enabled on both instances.
//!   In the launch template: MetadataOptions → InstanceMetadataTags = enabled.
//!
//! • Each instance must carry these tags (all readable via IMDS):
//!
//!     keepalived-role      "master" | "backup"
//!     keepalived-cluster   shared value identifying both LB nodes
//!                          (e.g. "aeroftp-lb") — same on both ASGs
//!
//! Peer IP discovery
//! ─────────────────
//! Because ASGs reject launch templates that specify a fixed primary private
//! IP address, peer IPs cannot be known in advance and stored as tags.
//! Instead, keepalived-config calls DescribeInstances at boot, filtered by the
//! shared keepalived-cluster tag, to find the peer instance and read its
//! network interface IPs directly from the EC2 API response.
//!
//! A configurable retry loop (--peer-discovery-timeout, default 300 s) handles
//! the case where both nodes boot simultaneously and the peer is not yet
//! running when this node starts.
//!
//!
//! Generated files (always written, even when a check is disabled)
//! ───────────────────────────────────────────────────────────────
//!   /etc/keepalived/vrrp.conf            VRRP instance blocks + sync group.
//!                                        Disabled checks appear as commented
//!                                        stubs with instructions.
//!
//!   /etc/keepalived/notify-master.sh     Called on MASTER transition; runs
//!                                        aeroplug ip for both VIPs.
//!
//!   /etc/keepalived/notify-backup.sh     Called on BACKUP transition (no-op;
//!                                        the new master steals the IPs).
//!
//!   /etc/keepalived/chk-backends.sh      Queries IPVS via ipvsadm and fails
//!                                        if fewer than --track-min-backends
//!                                        real servers are active.
//!
//!   /etc/keepalived/chk-forward-path.sh  TCP-probes a real server through
//!                                        eth1 to verify the forwarding path.
//!                                        Source-bound to eth1's IP so a
//!                                        broken eth1 cannot silently pass.
//!
//!
//! Gradual roll-out
//! ────────────────
//! Initially run with no --enable-* flags.  Once the infrastructure is stable,
//! re-run with individual flags to activate each check, then reload keepalived:
//!
//!   keepalived-config --auth-pass "$P" --enable-track-interface
//!   keepalived-config --auth-pass "$P" --enable-track-interface \
//!                                      --enable-track-backends
//!   keepalived-config --auth-pass "$P" --enable-track-interface \
//!                                      --enable-track-backends \
//!                                      --enable-track-forward \
//!                                      --track-probe-host 172.16.32.100
//!
//!   rc-service keepalived reload    # or: kill -HUP $(pgrep keepalived)
//!
//!
//! Typical OpenRC integration
//! ──────────────────────────
//!   depend() { need net aws-config; }
//!
//!   start_pre() {
//!       keepalived-config --auth-pass "${VRRP_PASS}"
//!   }
//!   start() {
//!       keepalived -f /etc/keepalived/keepalived.conf
//!   }

use aerocore::{fetch_imds_path, fetch_imds_token};
use anyhow::{bail, Context, Result};
use clap::Parser;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "aeropulse")]
#[command(
    about = "Generate VRRP keepalived include file and notify/track scripts from EC2 instance tags"
)]
struct Args {
    // ── Security ──────────────────────────────────────────────────────────────
    /// VRRP authentication password.  Must be identical on both nodes.
    /// Prefer passing via the VRRP_PASS environment variable to avoid the
    /// value appearing in process listings.
    #[arg(long, env = "VRRP_PASS")]
    auth_pass: String,

    // ── Output paths ──────────────────────────────────────────────────────────
    /// Path for the generated VRRP include file
    #[arg(long, default_value = "/etc/keepalived/vrrp.conf")]
    out: PathBuf,

    /// Path for the notify-master shell script
    #[arg(long, default_value = "/etc/keepalived/notify-master.sh")]
    notify_master_out: PathBuf,

    /// Path for the notify-backup shell script
    #[arg(long, default_value = "/etc/keepalived/notify-backup.sh")]
    notify_backup_out: PathBuf,

    /// Path for the backend health-check script
    #[arg(long, default_value = "/etc/keepalived/chk-backends.sh")]
    chk_backends_out: PathBuf,

    /// Path for the forward-path probe script
    #[arg(long, default_value = "/etc/keepalived/chk-forward-path.sh")]
    chk_forward_out: PathBuf,

    /// Path for the generated backends include file
    /// (track_file blocks + virtual_server block, included by keepalived.conf)
    #[arg(long, default_value = "/etc/keepalived/backends.conf")]
    backends_out: PathBuf,

    // ── Interface names ───────────────────────────────────────────────────────
    /// OS interface name for the outside ENI (device index 0)
    #[arg(long, default_value = "eth0")]
    iface_outside: String,

    /// OS interface name for the inside ENI (device index 1)
    #[arg(long, default_value = "eth1")]
    iface_inside: String,

    /// OS interface name for the HA sync ENI (device index 2).
    /// Carries both VRRP heartbeats and the IPVS connection sync daemon,
    /// keeping all HA control traffic off the data-plane interfaces.
    #[arg(long, default_value = "eth2")]
    iface_sync: String,

    /// OS interface name for the VXLAN overlay used by the IPVS sync daemon.
    /// A point-to-point VXLAN tunnel over eth2 encapsulates the sync daemon's
    /// multicast traffic as unicast UDP, working around AWS VPC's lack of
    /// multicast support while satisfying keepalived's multicast-only restriction.
    #[arg(long, default_value = "vxlan0")]
    iface_vxlan: String,

    // ── VRRP tuning ───────────────────────────────────────────────────────────
    /// VRRP virtual_router_id for the outside instance (1–255, unique per subnet)
    #[arg(long, default_value_t = 51)]
    vrid_outside: u8,

    /// VRRP virtual_router_id for the inside instance (1–255, unique per subnet)
    #[arg(long, default_value_t = 52)]
    vrid_inside: u8,

    /// VRRP advertisement interval in seconds
    #[arg(long, default_value_t = 1)]
    advert_int: u8,

    /// Sync daemon identifier (1–255).  Must be the same on both nodes.
    /// Allows multiple sync daemon instances on the same subnet if needed.
    #[arg(long, default_value_t = 1)]
    lvs_sync_id: u8,

    /// UDP port used by the IPVS connection sync daemon
    #[arg(long, default_value_t = 8848)]
    lvs_sync_port: u16,

    /// Number of consecutive missed advertisements before declaring the master
    /// absent and transitioning from BACKUP to MASTER.
    /// Dead interval = down_timer_adverts × advert_int.
    ///
    /// The default of 3 (→ 3 s dead interval) serves two purposes:
    ///   1. Startup race: gives enough time for the peer's unicast
    ///      advertisements to arrive after eth1 is initialised, preventing
    ///      both nodes electing themselves MASTER simultaneously.
    ///   2. Failover: the backup waits 3 s after the master goes silent
    ///      before taking over.  Increase for slower but safer failover,
    ///      decrease for faster failover at the cost of more startup risk.
    ///
    /// Compatible with nopreempt (confirmed in keepalived source).
    #[arg(long, default_value_t = 3)]
    down_timer_adverts: u32,

    /// VRRP priority assigned to the master role
    #[arg(long, default_value_t = 150)]
    priority_master: u8,

    /// VRRP priority assigned to the backup role
    #[arg(long, default_value_t = 100)]
    priority_backup: u8,

    // ── Track: interface ──────────────────────────────────────────────────────
    /// Enable track_interface for eth0 and eth1 in each vrrp_instance.
    /// If either interface loses link, priority drops immediately, triggering
    /// failover without waiting for VRRP heartbeat timeout.
    #[arg(long, default_value_t = false)]
    enable_track_interface: bool,

    // ── Track: backend health ─────────────────────────────────────────────────
    /// Enable the chk-backends track_script.
    /// Queries IPVS via ipvsadm; fails when fewer than --track-min-backends
    /// real servers are active.  Requires ipvsadm on the PATH.
    #[arg(long, default_value_t = false)]
    enable_track_backends: bool,

    /// Minimum number of active (weight > 0) IPVS real servers before the
    /// backend check is considered failed
    #[arg(long, default_value_t = 1)]
    track_min_backends: u32,

    // ── Track: forward path ───────────────────────────────────────────────────
    /// Enable the chk-forward-path track_script.
    /// TCP-probes --track-probe-host through eth1's source IP to verify the
    /// actual forwarding path is working.  Required when this flag is set.
    #[arg(long, default_value_t = false)]
    enable_track_forward: bool,

    /// Real server IP to probe for the forward-path check.
    /// Required when --enable-track-forward is set.
    #[arg(long, value_name = "IP")]
    track_probe_host: Option<String>,

    /// TCP port to probe on the real server
    #[arg(long, default_value_t = 80)]
    track_probe_port: u16,

    /// Seconds before the TCP probe times out
    #[arg(long, default_value_t = 2)]
    track_probe_timeout: u8,

    // ── Track: shared tuning ──────────────────────────────────────────────────
    /// Weight subtracted from this node's VRRP priority when a track_script
    /// fails.  Must exceed (priority_master − priority_backup) in absolute
    /// value to actually trigger a failover.  With the defaults (150/100) a
    /// weight of -60 drops the master to 90, below the backup's 100.
    ///
    /// If BOTH nodes fail a script simultaneously, both drop equally and the
    /// master retains its relative lead — no spurious failover occurs.
    #[arg(long, default_value_t = -60)]
    track_weight: i32,

    /// Seconds between track_script executions
    #[arg(long, default_value_t = 5)]
    track_interval: u32,

    /// Consecutive failures required before a script is considered failed
    #[arg(long, default_value_t = 2)]
    track_fall: u32,

    /// Consecutive successes required to recover from failed state
    #[arg(long, default_value_t = 2)]
    track_rise: u32,

    // ── Virtual server tuning ──────────────────────────────────────────────────
    /// IPVS scheduler for the virtual_server block
    #[arg(long, default_value = "wlc")]
    lvs_sched: String,

    /// persistence_timeout for the virtual_server block (seconds).
    /// Covers the window from control-connection establishment until
    /// ip_vs_ftp has processed the PASV response and created a data-channel
    /// IPVS entry for that session.
    #[arg(long, default_value_t = 30u32)]
    persistence_timeout: u32,

    // ── Notify script helpers ─────────────────────────────────────────────────
    /// Absolute path to the aeroplug binary (embedded in the
    /// generated notify-master.sh)
    #[arg(long, default_value = "/usr/local/bin/aeroplug")]
    assign_bin: PathBuf,

    // ── Peer discovery ────────────────────────────────────────────────────────

    // ── Common ────────────────────────────────────────────────────────────────
    /// AWS region
    #[arg(long, default_value = "eu-west-2")]
    region: String,
}

// ── Resolved instance data ────────────────────────────────────────────────────

struct IfaceInfo {
    #[allow(dead_code)]
    device_number: u32,
    eni_id: String,
    primary_ip: String,
}

struct InstanceData {
    instance_id: String,
    role: Role,
    outside: IfaceInfo,
    inside: IfaceInfo,
    /// Peer's eth2 IP, read from the keepalived-peer-sync IMDS tag.
    /// Used for both VRRP unicast heartbeats and lvs_sync_daemon.
    peer_sync_ip: String,
    /// HA sync interface (eth2) info.
    sync: IfaceInfo,
    /// Outside VIP — from required IMDS tag `aeroftp-vip-outside`.
    vip_outside: String,
    /// Inside VIP — from required IMDS tag `aeroftp-vip-inside`.
    vip_inside: String,
    /// Slot → IP mapping derived from eth1 subnet CIDR + aeroftp-slot-offset tag.
    slot_network: aerocore::SlotNetwork,
    /// Total number of backend slots, from IMDS tag `aeroftp-slot-count`.
    slot_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Master,
    Backup,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Master => "master",
            Role::Backup => "backup",
        }
    }
    fn vrrp_state(self) -> &'static str {
        // Both instances always start as BACKUP regardless of role.
        //
        // With state MASTER, keepalived immediately claims the role on boot,
        // making nopreempt meaningless (the warning "nopreempt will not work
        // with initial state MASTER" confirms this).  With state BACKUP:
        //
        //   • Initial election: no existing MASTER → dead interval expires →
        //     highest priority (master role, 150) wins naturally.  nopreempt
        //     has no effect here since there is nothing to preempt.
        //
        //   • After failover: backup is acting MASTER.  Replacement instance
        //     boots as BACKUP, sees an existing MASTER, and nopreempt prevents
        //     it from reclaiming the role → no double failover.
        //
        // The role distinction is carried entirely by priority + nopreempt,
        // not by the initial state.
        "BACKUP"
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Validate flag combinations.
    if args.enable_track_forward && args.track_probe_host.is_none() {
        bail!("--track-probe-host <IP> is required when --enable-track-forward is set");
    }

    println!("  Resolving instance data from IMDS and EC2 API …");
    let data = resolve_instance_data().await?;

    println!("  Instance  : {}", data.instance_id);
    println!("  Role      : {}", data.role.as_str());
    println!(
        "  eth0      : {}  ({})",
        data.outside.primary_ip, data.outside.eni_id
    );
    println!(
        "  eth1      : {}  ({})",
        data.inside.primary_ip, data.inside.eni_id
    );
    println!(
        "  eth2      : {}  ({})  [HA sync]",
        data.sync.primary_ip, data.sync.eni_id
    );
    println!(
        "  Peer eth2 : {} (VRRP unicast + lvs_sync_daemon peer)",
        data.peer_sync_ip
    );
    println!("  VIP out   : {} (aeroftp-vip-outside)", data.vip_outside);
    println!("  VIP in    : {} (aeroftp-vip-inside)", data.vip_inside);
    println!(
        "  Slots     : {} × ({} – {})",
        data.slot_count,
        data.slot_network.ip_for_slot(0),
        data.slot_network.ip_for_slot(data.slot_count - 1)
    );

    // Render all content.
    let vrrp_conf = render_vrrp_conf(&args, &data);
    let backends_conf = render_backends_conf(&args, &data);
    let notify_master = render_notify_master(&args, &data);
    let notify_backup = render_notify_backup(&args, &data);
    let chk_backends = render_chk_backends(&args);
    let chk_forward_path = render_chk_forward_path(&args, &data);

    // Write files.
    write_file(&args.out, &vrrp_conf, 0o640)?;
    write_file(&args.backends_out, &backends_conf, 0o640)?;
    write_file(&args.notify_master_out, &notify_master, 0o750)?;
    write_file(&args.notify_backup_out, &notify_backup, 0o750)?;
    write_file(&args.chk_backends_out, &chk_backends, 0o750)?;
    write_file(&args.chk_forward_out, &chk_forward_path, 0o750)?;

    println!("✅ Written:");
    println!("   {}", args.out.display());
    println!("   {}", args.backends_out.display());
    println!("   {}", args.notify_master_out.display());
    println!("   {}", args.notify_backup_out.display());
    println!(
        "   {}  [track: {}]",
        args.chk_backends_out.display(),
        if args.enable_track_backends {
            "ENABLED"
        } else {
            "generated, not yet active"
        }
    );
    println!(
        "   {}  [track: {}]",
        args.chk_forward_out.display(),
        if args.enable_track_forward {
            "ENABLED"
        } else {
            "generated, not yet active"
        }
    );
    println!();
    println!("   keepalived.conf must include:");
    println!("     include \"{}\"", args.out.display());
    println!("     include \"{}\"", args.backends_out.display());

    Ok(())
}

// ── IMDS resolution ───────────────────────────────────────────────────────────

async fn resolve_instance_data() -> Result<InstanceData> {
    let token = fetch_imds_token().await?;

    let instance_id = fetch_imds_path(&token, "instance-id").await?;

    let role_str = fetch_imds_tag(&token, "keepalived-role").await?;
    let role = match role_str.as_str() {
        "master" => Role::Master,
        "backup" => Role::Backup,
        other => bail!(
            "Tag 'keepalived-role' has unexpected value '{other}'. \
             Expected 'master' or 'backup'."
        ),
    };

    let mut ifaces = fetch_all_interfaces(&token).await?;
    ifaces.sort_by_key(|i| i.device_number);

    if ifaces.len() < 3 {
        bail!(
            "Expected at least 3 network interfaces (eth0 + eth1 + eth2), \
             found {}. Are both the inside and sync ENIs attached?",
            ifaces.len()
        );
    }

    let outside = ifaces.remove(0);
    let inside = ifaces.remove(0);
    let sync = ifaces.remove(0);

    // Peer sync IP is the fixed eth2 IP of the peer, stored as an instance tag.
    // ASG tag name: keepalived-peer-sync
    //   master ASG value: backup's eth2 IP  (e.g. 172.16.32.136)
    //   backup ASG value: master's eth2 IP  (e.g. 172.16.32.135)
    let peer_sync_ip = fetch_imds_tag(&token, "keepalived-peer-sync").await?;

    // ── VIPs: required IMDS instance tags ───────────────────────────────────
    // Set once in the EC2 launch template with "Resource types: Instances".
    // Both master and backup nodes read them from the same source of truth.
    let vip_outside = fetch_imds_tag(&token, "aeroftp-vip-outside")
        .await
        .context(
            "IMDS tag 'aeroftp-vip-outside' not found. \
             Add it to the EC2 launch template (e.g. value: 172.16.29.100).",
        )?;

    let vip_inside = fetch_imds_tag(&token, "aeroftp-vip-inside").await.context(
        "IMDS tag 'aeroftp-vip-inside' not found. \
             Add it to the EC2 launch template (e.g. value: 172.16.32.10).",
    )?;

    // ── Slot network (shared formula with aeroscale) ───────────────────────────
    let slot_network = aerocore::SlotNetwork::from_imds().await.context(
        "Failed to resolve slot network from IMDS. \
             Ensure the aeroftp-slot-offset tag and eth1 subnet CIDR are available.",
    )?;

    let slot_count_str = fetch_imds_tag(&token, "aeroftp-slot-count").await.context(
        "IMDS tag 'aeroftp-slot-count' not found. \
             Add it to the load balancer launch template (e.g. value: 20).",
    )?;
    let slot_count: u32 = slot_count_str
        .trim()
        .parse()
        .with_context(|| format!("Invalid aeroftp-slot-count value: '{slot_count_str}'"))?;

    Ok(InstanceData {
        instance_id,
        role,
        outside,
        inside,
        sync,
        peer_sync_ip,
        vip_outside,
        vip_inside,
        slot_network,
        slot_count,
    })
}
async fn fetch_imds_tag(token: &str, tag_key: &str) -> Result<String> {
    fetch_imds_path(token, &format!("tags/instance/{tag_key}"))
        .await
        .with_context(|| {
            format!(
                "Failed to read tag '{tag_key}' from IMDS. \
                 Is 'Instance tags in metadata' enabled on this instance \
                 (LaunchTemplate → MetadataOptions → InstanceMetadataTags)?"
            )
        })
}

async fn fetch_all_interfaces(token: &str) -> Result<Vec<IfaceInfo>> {
    let macs_raw = fetch_imds_path(token, "network/interfaces/macs/").await?;
    let macs: Vec<&str> = macs_raw
        .lines()
        .map(|s| s.trim_end_matches('/').trim())
        .filter(|s| !s.is_empty())
        .collect();

    let mut interfaces = Vec::with_capacity(macs.len());
    for mac in macs {
        let device_number: u32 = fetch_imds_path(
            token,
            &format!("network/interfaces/macs/{mac}/device-number"),
        )
        .await
        .with_context(|| format!("Failed to read device-number for MAC {mac}"))?
        .parse()
        .with_context(|| format!("device-number for MAC {mac} is not a valid integer"))?;

        let eni_id = fetch_imds_path(
            token,
            &format!("network/interfaces/macs/{mac}/interface-id"),
        )
        .await
        .with_context(|| format!("Failed to read interface-id for MAC {mac}"))?;

        let ips_raw = fetch_imds_path(token, &format!("network/interfaces/macs/{mac}/local-ipv4s"))
            .await
            .with_context(|| format!("Failed to read local-ipv4s for MAC {mac}"))?;

        let primary_ip = ips_raw
            .lines()
            .next()
            .context("local-ipv4s returned an empty response")?
            .trim()
            .to_string();

        interfaces.push(IfaceInfo {
            device_number,
            eni_id,
            primary_ip,
        });
    }
    Ok(interfaces)
}

// ── backends.conf rendering ───────────────────────────────────────────────────

/// Generate `/etc/keepalived/backends.conf` — included by the static
/// `keepalived.conf`.  Contains:
///
/// 1. One `track_file` block per slot — keepalived reads the weight file
///    directly to determine whether the backend is active, draining or removed.
///
/// 2. One `virtual_server` block with one `real_server` per slot referencing
///    the corresponding `track_file`.
///
/// The slot IPs are computed deterministically via `SlotNetwork`:
///   `IP = base + offset + slot`
///
/// This file is regenerated at every boot so changes to `aeroftp-slot-count`
/// or the slot subnet are picked up without manual edits.
fn render_backends_conf(args: &Args, data: &InstanceData) -> String {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let slot_0 = data.slot_network.ip_for_slot(0);
    let slot_last = data.slot_network.ip_for_slot(data.slot_count - 1);

    // ── track_file blocks ─────────────────────────────────────────────────────
    let mut track_files = String::new();
    for slot in 0..data.slot_count {
        let ip = data.slot_network.ip_for_slot(slot);
        track_files.push_str(&format!(
            "track_file backend_{ip} {{\n    file \"/etc/keepalived/weights/backend-{ip}.weight\"\n}}\n"
        ));
    }

    // ── real_server blocks ────────────────────────────────────────────────────
    let mut real_servers = String::new();
    for slot in 0..data.slot_count {
        let ip = data.slot_network.ip_for_slot(slot);
        real_servers.push_str(&format!(
            "    real_server {ip} 21 {{\n        weight 1\n        FILE_CHECK {{\n            track_file backend_{ip}\n            weight 1\n        }}\n    }}\n"
        ));
    }

    format!(
        r#"# backends.conf — generated by aeropulse for {instance_id}
# Generated : {ts}
# DO NOT EDIT MANUALLY — re-run aeropulse to regenerate.
#
# Slot network:
#   base      : {base}
#   offset    : {offset}
#   count     : {count}
#   range     : {slot_0} – {slot_last}
#
# Weight file values:
#    0           → active   (effective weight 1, receives new connections)
#   -1           → draining (effective weight 0, no new connections)
#   -2147483648  → removed  (FAULT state, fully absent from IPVS scheduler)
#
# These files are managed by aeroscale; do not edit manually.

# ---------------------------------------------------------------------------
# One track_file per backend slot.
# ---------------------------------------------------------------------------

{track_files}
# ---------------------------------------------------------------------------
# Virtual server  (outside VIP → backend pool on FTP port 21)
#
# FTP requires NAT mode so that ip_vs_ftp can rewrite PASV responses and
# automatically track data-channel connections.  DR mode is not supported.
#
# persistence_timeout only needs to cover the window from control-connection
# establishment to ip_vs_ftp processing the PASV response.  Once ip_vs_ftp
# has created the data-channel IPVS entry the session no longer depends on
# persistence.  Since each control connection carries exactly one file upload
# there is no need to cover multiple transfers per control connection.
# ---------------------------------------------------------------------------

virtual_server {vip_outside} 21 {{
    lvs_sched    {sched}
    lvs_method   NAT
    protocol     TCP
    persistence_timeout {timeout}
    quorum       1

{real_servers}}}
"#,
        instance_id = data.instance_id,
        ts = ts,
        base = data.slot_network.base,
        offset = data.slot_network.offset,
        count = data.slot_count,
        slot_0 = slot_0,
        slot_last = slot_last,
        track_files = track_files,
        vip_outside = data.vip_outside,
        sched = args.lvs_sched,
        timeout = args.persistence_timeout,
        real_servers = real_servers,
    )
}

// ── vrrp.conf rendering ───────────────────────────────────────────────────────

fn render_vrrp_conf(args: &Args, data: &InstanceData) -> String {
    let priority = match data.role {
        Role::Master => args.priority_master,
        Role::Backup => args.priority_backup,
    };
    let state = data.role.vrrp_state();
    // nopreempt on the master prevents it from automatically reclaiming VIPs
    // after recovering from a failure, avoiding a double-failover event.
    let nopreempt = if data.role == Role::Master {
        "    nopreempt\n"
    } else {
        ""
    };
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");

    let vrrp_scripts_block = render_vrrp_scripts_definitions(args);
    let lvs_sync_block = render_lvs_sync_daemon(args);
    let track_iface_block = render_track_interface_block(args);
    let track_scripts_block = render_track_scripts_references(args);
    format!(
        r#"# ── VRRP — generated by keepalived-config ────────────────────────────────────
# Instance  : {instance_id}
# Role      : {role}
# Generated : {ts}
#
# DO NOT EDIT MANUALLY — re-run keepalived-config to regenerate.
#
# Add to /etc/keepalived/keepalived.conf:
#   include "{out}"
#
# Interface roles:
#   eth0  ({iface_outside})  outside / public-facing data plane
#   eth1  ({iface_inside})   inside  / backend data plane + inside VIP
#   eth2  ({iface_sync})     HA sync / VRRP heartbeats + IPVS session sync
{vrrp_scripts_block}
{lvs_sync_block}
vrrp_sync_group VG_LB {{
    group {{
        VI_OUTSIDE
        VI_INSIDE
    }}
    # The sync group ensures both VIPs always move together — you will never
    # end up with the outside VIP on one node and the inside VIP on the other.
    # notify_master fires once per group transition, not once per instance.
    notify_master "{notify_master}"
    notify_backup "{notify_backup}"
}}

# ── Outside (public-facing) VIP: {vip_outside} ───────────────────────────────

vrrp_instance VI_OUTSIDE {{
    state             {state}
    # Both VRRP instances bind their heartbeat socket to eth2 (the dedicated
    # HA sync interface).  This completely isolates VRRP traffic from the data
    # plane: a burst of FTP traffic on eth1 can no longer starve the VRRP
    # heartbeat and cause a spurious failover.
    # The outside VIP is still placed on eth0 via 'dev' in virtual_ipaddress.
    interface         {iface_sync}
    virtual_router_id {vrid_outside}
    priority          {priority}
    advert_int        {advert_int}
    down_timer_adverts {down_timer_adverts}
{nopreempt}
    # Unicast over eth2 — AWS VPC drops VRRP multicast (224.0.0.18).
    unicast_src_ip  {src_sync}
    unicast_peer {{
        {peer_sync}
    }}

    authentication {{
        auth_type PASS
        auth_pass {auth_pass}
    }}
{track_iface_block}{track_scripts_block}
    # /32 avoids injecting an unwanted subnet route into the local routing table.
    virtual_ipaddress {{
        {vip_outside}/32 dev {iface_outside}
    }}
}}

# ── Inside (server-facing) VIP: {vip_inside} ─────────────────────────────────

vrrp_instance VI_INSIDE {{
    state             {state}
    interface         {iface_sync}
    virtual_router_id {vrid_inside}
    priority          {priority}
    advert_int        {advert_int}
    down_timer_adverts {down_timer_adverts}
{nopreempt}
    unicast_src_ip  {src_sync}
    unicast_peer {{
        {peer_sync}
    }}

    authentication {{
        auth_type PASS
        auth_pass {auth_pass}
    }}
{track_iface_block}{track_scripts_block}
    virtual_ipaddress {{
        {vip_inside}/32 dev {iface_inside}
    }}
}}
"#,
        instance_id = data.instance_id,
        role = data.role.as_str(),
        ts = ts,
        out = args.out.display(),
        notify_master = args.notify_master_out.display(),
        notify_backup = args.notify_backup_out.display(),
        iface_outside = args.iface_outside,
        iface_inside = args.iface_inside,
        iface_sync = args.iface_sync,
        vip_outside = data.vip_outside,
        vip_inside = data.vip_inside,
        vrid_outside = args.vrid_outside,
        vrid_inside = args.vrid_inside,
        priority = priority,
        advert_int = args.advert_int,
        down_timer_adverts = args.down_timer_adverts,
        nopreempt = nopreempt,
        src_sync = data.sync.primary_ip,
        peer_sync = data.peer_sync_ip,
        auth_pass = args.auth_pass,
        vrrp_scripts_block = vrrp_scripts_block,
        track_iface_block = track_iface_block,
        track_scripts_block = track_scripts_block,
    )
}

/// Render the `vrrp_script { … }` definition blocks that appear at the top
/// level of the config (outside vrrp_instance).  Only emitted for enabled
/// checks — keepalived validates scripts at startup even if unreferenced.
fn render_vrrp_scripts_definitions(args: &Args) -> String {
    let mut out = String::new();
    let mut any = false;

    if args.enable_track_backends {
        any = true;
        out.push_str(&format!(
            r#"
vrrp_script chk_backends {{
    script   "{script}"
    interval {interval}
    weight   {weight}
    fall     {fall}
    rise     {rise}
    # Queries IPVS via ipvsadm.  Fails if fewer than {min} real server(s) are
    # active.  Both nodes failing simultaneously does not cause a failover
    # because both priorities drop equally and the master retains its lead.
}}
"#,
            script = args.chk_backends_out.display(),
            interval = args.track_interval,
            weight = args.track_weight,
            fall = args.track_fall,
            rise = args.track_rise,
            min = args.track_min_backends,
        ));
    }

    if args.enable_track_forward {
        any = true;
        out.push_str(&format!(
            r#"
vrrp_script chk_forward_path {{
    script   "{script}"
    interval {interval}
    weight   {weight}
    fall     {fall}
    rise     {rise}
    # TCP-probes a real server through eth1.  Source-bound to eth1's own IP
    # so a broken eth1 cannot silently pass the check via eth0.
}}
"#,
            script = args.chk_forward_out.display(),
            interval = args.track_interval,
            weight = args.track_weight,
            fall = args.track_fall,
            rise = args.track_rise,
        ));
    }

    if !any {
        out.push_str(
            r#"
# ── track_script definitions (currently disabled) ────────────────────────────
# Re-run keepalived-config with one or more of the flags below to activate:
#
#   --enable-track-backends          query IPVS; fail if < N real servers active
#   --enable-track-forward           TCP-probe a real server through eth1
#
# Both script files have been written and can be tested manually:
#   /etc/keepalived/chk-backends.sh
#   /etc/keepalived/chk-forward-path.sh
"#,
        );
    }

    out
}

/// Render a second `global_defs { }` block containing the `lvs_sync_daemon`
/// directive.  keepalived merges multiple global_defs blocks across included
/// files, so this is safe alongside the existing block in keepalived.conf.
///
/// The sync daemon is bound to the VXLAN overlay interface (vxlan0), which
/// encapsulates the multicast sync traffic as unicast UDP over eth2.  This
/// satisfies keepalived's multicast-only restriction while working within
/// AWS VPC's no-multicast constraint.  keepalived automatically switches the
/// daemon between master (send) and backup (receive) roles on every VRRP
/// state transition — no manual ipvsadm calls are needed in the notify scripts.
fn render_lvs_sync_daemon(args: &Args) -> String {
    format!(
        r#"# ── IPVS connection sync daemon ──────────────────────────────────────────────
# Bound to vxlan0: a point-to-point VXLAN tunnel over eth2.
# keepalived manages master/backup role transitions automatically.

global_defs {{
    lvs_sync_daemon {iface_vxlan} VI_INSIDE id {sync_id}
}}
"#,
        iface_vxlan = args.iface_vxlan,
        sync_id = args.lvs_sync_id,
    )
}

/// Render the `track_interface { … }` block inside a vrrp_instance.
/// Tracks all three interfaces — if any link drops the sync group fails over
/// as a unit immediately, without waiting for the VRRP heartbeat timeout.
fn render_track_interface_block(args: &Args) -> String {
    if args.enable_track_interface {
        format!(
            r#"
    track_interface {{
        {iface_outside}
        {iface_inside}
        {iface_sync}
    }}
"#,
            iface_outside = args.iface_outside,
            iface_inside = args.iface_inside,
            iface_sync = args.iface_sync,
        )
    } else {
        format!(
            r#"
    # track_interface disabled — enable with --enable-track-interface and regenerate.
    # track_interface {{
    #     {iface_outside}
    #     {iface_inside}
    #     {iface_sync}
    # }}
"#,
            iface_outside = args.iface_outside,
            iface_inside = args.iface_inside,
            iface_sync = args.iface_sync,
        )
    }
}

/// Render the `track_script { … }` reference block inside a vrrp_instance.
fn render_track_scripts_references(args: &Args) -> String {
    let backends_line = if args.enable_track_backends {
        "        chk_backends\n".to_string()
    } else {
        "        # chk_backends    (enable with --enable-track-backends)\n".to_string()
    };

    let forward_line = if args.enable_track_forward {
        "        chk_forward_path\n".to_string()
    } else {
        "        # chk_forward_path    (enable with --enable-track-forward --track-probe-host <IP>)\n".to_string()
    };

    format!(
        r#"
    track_script {{
{backends_line}{forward_line}    }}
"#,
    )
}

// ── notify-master.sh rendering ────────────────────────────────────────────────

fn render_notify_master(args: &Args, data: &InstanceData) -> String {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    format!(
        r#"#!/bin/sh
# notify-master.sh \u2014 generated by aeropulse for {instance_id}
# Generated : {ts}
# DO NOT EDIT MANUALLY \u2014 re-run aeropulse to regenerate.
#
# Called by keepalived when this node's sync group (VG_LB) transitions to
# MASTER.  Claims both virtual IPs at the AWS EC2 control plane using
# AllowReassignment=true so they are stolen from the previous holder
# atomically, without requiring a prior unassign step.
#
# The IPVS connection sync daemon is managed entirely by keepalived via the
# lvs_sync_daemon directive in global_defs \u2014 no ipvsadm calls needed here.

set -eu

log() {{ logger -p local3.info -t keepalived-notify "notify-master[${{TYPE:-?}}/${{NAME:-?}}]: $*"; }}

TYPE="${{1:-}}"
NAME="${{2:-}}"
STATE="${{3:-}}"

log "Transitioning to MASTER on {instance_id} \u2014 claiming VIPs ..."

# Outside VIP \u2014 defaults to primary ENI (eth0), no --eni flag needed.
{assign} ip \
    --ip {vip_outside} \
    --region {region} \
    --allow-reassignment \
    --assign \
    && log "Outside VIP {vip_outside} assigned." \
    || {{ log "ERROR: failed to assign outside VIP {vip_outside}"; exit 1; }}

# Inside VIP \u2014 must specify eth1's ENI ID explicitly.
{assign} ip \
    --ip {vip_inside} \
    --eni {eni_inside} \
    --region {region} \
    --allow-reassignment \
    --assign \
    && log "Inside VIP {vip_inside} assigned." \
    || {{ log "ERROR: failed to assign inside VIP {vip_inside}"; exit 1; }}

log "Both VIPs claimed successfully."
"#,
        instance_id = data.instance_id,
        ts = ts,
        assign = args.assign_bin.display(),
        vip_outside = data.vip_outside,
        vip_inside = data.vip_inside,
        eni_inside = data.inside.eni_id,
        region = args.region,
    )
}

// \u2500\u2500 notify-backup.sh rendering \u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500

fn render_notify_backup(args: &Args, data: &InstanceData) -> String {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    // Suppress unused variable warnings \u2014 args and data are kept in the
    // signature for consistency and future use.
    let _ = args;
    let _ = &data.instance_id;
    format!(
        r#"#!/bin/sh
# notify-backup.sh \u2014 generated by aeropulse for {instance_id}
# Generated : {ts}
# DO NOT EDIT MANUALLY \u2014 re-run aeropulse to regenerate.
#
# Called by keepalived when this node's sync group (VG_LB) transitions to
# BACKUP.  No AWS API action is needed: the new MASTER will claim the VIPs.
# keepalived removes the virtual_ipaddress entries from the OS interfaces
# automatically as part of the BACKUP transition.
#
# The IPVS connection sync daemon role is also switched automatically by
# keepalived via lvs_sync_daemon \u2014 no ipvsadm calls needed here.

set -eu

logger -p local3.info -t keepalived-notify \
    "notify-backup[${{1:-?}}/${{2:-?}}]: transitioned to BACKUP \u2014 no action needed."

exit 0
"#,
        instance_id = data.instance_id,
        ts = ts,
    )
}
// ── chk-backends.sh rendering ─────────────────────────────────────────────────

fn render_chk_backends(args: &Args) -> String {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let enabled_note = if args.enable_track_backends {
        "# Status: ACTIVE — referenced in vrrp.conf track_script block."
    } else {
        "# Status: NOT YET ACTIVE — re-run keepalived-config with --enable-track-backends."
    };
    format!(
        r#"#!/bin/sh
# chk-backends.sh — generated by keepalived-config
# Generated : {ts}
# {enabled_note}
#
# Checks that at least MIN_ACTIVE real servers are currently active in IPVS.
# "Active" means weight > 0, i.e. keepalived's own health checks consider the
# server reachable.
#
# Returns 0 (healthy) or 1 (failed — triggers weight penalty in VRRP election).
#
# Requires: ipvsadm
# Test:     {script} && echo OK || echo FAIL

MIN_ACTIVE={min_active}

# ipvsadm -l -n lists real servers as lines starting with "  ->".
# Column 4 is the weight; 0 means the server has been marked down.
ACTIVE=$(ipvsadm -l -n 2>/dev/null \
    | awk '/^[[:space:]]*->/ {{ if ($4 > 0) count++ }} END {{ print count+0 }}')

if [ "${{ACTIVE}}" -ge "${{MIN_ACTIVE}}" ]; then
    exit 0
else
    logger -p local3.info -t keepalived-track \
        "chk-backends: ${{ACTIVE}} active backend(s), minimum is ${{MIN_ACTIVE}}"
    exit 1
fi
"#,
        ts = ts,
        enabled_note = enabled_note,
        script = args.chk_backends_out.display(),
        min_active = args.track_min_backends,
    )
}

// ── chk-forward-path.sh rendering ─────────────────────────────────────────────

fn render_chk_forward_path(args: &Args, data: &InstanceData) -> String {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let enabled_note = if args.enable_track_forward {
        "# Status: ACTIVE — referenced in vrrp.conf track_script block."
    } else {
        "# Status: NOT YET ACTIVE — re-run keepalived-config with --enable-track-forward --track-probe-host <IP>."
    };
    // Use a placeholder when the probe host is not yet configured.
    let probe_host = args
        .track_probe_host
        .as_deref()
        .unwrap_or("<not configured — set --track-probe-host>");

    format!(
        r#"#!/bin/sh
# chk-forward-path.sh — generated by keepalived-config
# Generated : {ts}
# {enabled_note}
#
# Verifies the actual forwarding path through eth1 to a real server by
# attempting a TCP connection.  The probe is source-bound to eth1's own
# private IP ({src_ip}) so a broken or mis-routed eth1 cannot silently pass
# via eth0, which would give a false positive.
#
# Returns 0 (healthy) or 1 (failed — triggers weight penalty in VRRP election).
#
# Requires: nc (netcat — available via busybox on Alpine)
# Test:     {script} && echo OK || echo FAIL

PROBE_HOST="{probe_host}"
PROBE_PORT={probe_port}
PROBE_TIMEOUT={probe_timeout}
SRC_IP="{src_ip}"

nc -z -s "${{SRC_IP}}" -w "${{PROBE_TIMEOUT}}" "${{PROBE_HOST}}" "${{PROBE_PORT}}" \
    >/dev/null 2>&1

if [ $? -ne 0 ]; then
    logger -p local3.info -t keepalived-track \
        "chk-forward-path: cannot reach ${{PROBE_HOST}}:${{PROBE_PORT}} via ${{SRC_IP}} (eth1)"
    exit 1
fi

exit 0
"#,
        ts = ts,
        enabled_note = enabled_note,
        script = args.chk_forward_out.display(),
        probe_host = probe_host,
        probe_port = args.track_probe_port,
        probe_timeout = args.track_probe_timeout,
        src_ip = data.inside.primary_ip,
    )
}

// ── File writing ──────────────────────────────────────────────────────────────

fn write_file(path: &PathBuf, content: &str, mode: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("Cannot create directory {}", dir.display()))?;
    }
    fs::write(path, content).with_context(|| format!("Cannot write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("Cannot set permissions on {}", path.display()))?;
    Ok(())
}
