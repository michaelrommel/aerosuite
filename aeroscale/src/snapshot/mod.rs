//! SystemSnapshot — a point-in-time view of all moving parts.
//!
//! Collecting a snapshot is always read-only.  All mutation (cleanup, weight
//! writes, scale commands) happens in later phases and is driven by the data
//! here.

pub mod asg;
pub mod ipvs;
pub mod leases;
pub mod weights;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Instant;

use anyhow::Result;
use tracing::warn;

use crate::slot_network::SlotNetwork;
use aerocore::redis_pool::now_ms;

// ── Types ─────────────────────────────────────────────────────────────────────

/// The keepalived weight value written to each backend's weight file.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendState {
    /// "0"           — active, receives traffic at full weight.
    Active,
    /// "-1"          — draining; weight reduced to zero.
    Draining,
    /// "-2147483648" — fully disabled; keepalived ignores it.
    Disabled,
    /// Any other value (e.g. from manual edits).
    Unknown(String),
}

impl BackendState {
    pub fn from_weight_str(s: &str) -> Self {
        match s.trim() {
            "0" => Self::Active,
            "-1" => Self::Draining,
            "-2147483648" => Self::Disabled,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Disabled => "disabled",
            Self::Unknown(_) => "unknown",
        }
    }

    pub fn colour(&self) -> &str {
        match self {
            Self::Active => "\x1b[32m",     // green
            Self::Draining => "\x1b[33m",   // yellow
            Self::Disabled => "\x1b[90m",   // dark grey
            Self::Unknown(_) => "\x1b[31m", // red
        }
    }
}

/// A currently-held Redis slot lease.
#[derive(Debug, Clone)]
pub struct SlotLease {
    pub slot: u32,
    pub owner_instance_id: String,
    pub expires_ms: u64,
}

impl SlotLease {
    pub fn remaining_secs(&self) -> f64 {
        let now = now_ms();
        if self.expires_ms > now {
            (self.expires_ms - now) as f64 / 1000.0
        } else {
            0.0
        }
    }

    pub fn is_expired(&self) -> bool {
        self.expires_ms <= now_ms()
    }
}

/// Group-level capacity info from the Auto Scaling Group.
#[derive(Debug, Clone)]
pub struct AsgGroupInfo {
    pub name: String,
    pub desired_capacity: i64,
    pub min_size: i64,
    pub max_size: i64,
}

impl AsgGroupInfo {
    /// True when terminating one more instance would violate the min-size
    /// constraint (i.e. AWS would reject the TerminateInstance call).
    pub fn would_violate_min(&self) -> bool {
        self.desired_capacity <= self.min_size
    }
}

/// One instance currently known to the Auto Scaling Group.
#[derive(Debug, Clone)]
pub struct AsgInstance {
    pub instance_id: String,
    pub lifecycle_state: String,
    pub health_status: String,
}

impl AsgInstance {
    pub fn is_in_service(&self) -> bool {
        self.lifecycle_state == "InService"
    }
}

/// Real-server state as reported by IPVS.
#[derive(Debug, Clone)]
pub struct IpvsBackend {
    pub ip: Ipv4Addr,
    pub active_connections: u32,
    pub inactive_connections: u32,
}

/// Per-backend view: weight file + IPVS data + Redis lease, all joined on IP.
#[derive(Debug, Clone)]
pub struct BackendStatus {
    pub ip: Ipv4Addr,
    /// Slot number derived from the IP via `SlotNetwork::slot_for_ip()`.
    /// Always `Some` for IPs within the slot range; `None` for stray files.
    pub slot: Option<u32>,
    pub weight_state: BackendState,
    /// IPVS real-server entry for this IP, if present.
    pub ipvs: Option<IpvsBackend>,
    /// The Redis lease for this slot, if one exists.
    pub lease: Option<SlotLease>,
}

// ── Snapshot ──────────────────────────────────────────────────────────────────

/// Complete point-in-time view of all subsystems.
pub struct SystemSnapshot {
    /// One entry per weight file, with slot, IPVS and lease merged in.
    pub backends: Vec<BackendStatus>,
    /// All active Redis slot leases (full list, for cross-checks).
    pub leases: Vec<SlotLease>,
    /// All instances currently in the ASG (any lifecycle state).
    pub asg: Vec<AsgInstance>,
    /// Group-level capacity constraints (desired / min / max).
    pub asg_group: Option<AsgGroupInfo>,
    /// Raw IPVS real-server list.
    pub ipvs: Vec<IpvsBackend>,
    /// When this snapshot was taken.
    pub taken_at: Instant,
    /// Human-readable UTC timestamp.
    pub taken_at_utc: String,
}

impl SystemSnapshot {
    /// Collect data from all sources.
    ///
    /// Weight files and Redis leases are mandatory — errors propagate.
    /// ASG and IPVS failures degrade gracefully (empty list + warning).
    /// Slot→IP mapping is computed directly via `slot_network` — no EC2 call.
    pub async fn collect(
        weights_dir: &str,
        region: &str,
        asg_name: &str,
        creds: &aerocore::AwsCredentials,
        redis_con: &mut redis::aio::MultiplexedConnection,
        slot_network: &SlotNetwork,
    ) -> Result<Self> {
        // ── Mandatory ─────────────────────────────────────────────────────────
        let weight_entries = weights::read_all(weights_dir).await?;
        let lease_list = leases::read_all(redis_con).await?;

        // ── Best-effort ───────────────────────────────────────────────────────
        let (asg_group, asg_instances) = match asg::read_all(region, asg_name, creds).await {
            Ok(v) => v,
            Err(e) => {
                warn!("ASG query failed: {e:#}");
                (None, Vec::new())
            }
        };

        let ipvs_backends = match ipvs::read_all().await {
            Ok(v) => v,
            Err(e) => {
                warn!("IPVS read failed: {e:#}");
                Vec::new()
            }
        };

        // ── Build slot→lease map using the deterministic formula ───────────────
        // ip_for_slot() is a pure arithmetic operation — no network call needed,
        // and works even after the owner instance has been terminated.
        let ip_to_lease: HashMap<Ipv4Addr, SlotLease> = lease_list
            .iter()
            .map(|l| (slot_network.ip_for_slot(l.slot), l.clone()))
            .collect();

        // ── Assemble BackendStatus ────────────────────────────────────────────
        let backends = weight_entries
            .into_iter()
            .map(|w| {
                let slot = slot_network.slot_for_ip(w.ip);
                let ipvs = ipvs_backends.iter().find(|i| i.ip == w.ip).cloned();
                let lease = ip_to_lease.get(&w.ip).cloned();
                BackendStatus {
                    ip: w.ip,
                    slot,
                    weight_state: w.state,
                    ipvs,
                    lease,
                }
            })
            .collect();

        Ok(SystemSnapshot {
            backends,
            leases: lease_list,
            asg: asg_instances,
            asg_group,
            ipvs: ipvs_backends,
            taken_at: Instant::now(),
            taken_at_utc: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        })
    }

    // ── Display ───────────────────────────────────────────────────────────────

    pub fn print(&self) {
        let reset = "\x1b[0m";
        let bold = "\x1b[1m";
        let dim = "\x1b[2m";
        let bar = "=".repeat(88);
        let sep = "-".repeat(85);

        println!("\n{bold}{bar}{reset}");
        println!(" System Snapshot  {dim}{}{reset}", self.taken_at_utc);
        println!("{bold}{bar}{reset}");

        // ── Backends ──────────────────────────────────────────────────────────
        println!(
            "\n {bold}Backends{reset}  ({} weight file(s))\n",
            self.backends.len()
        );
        if self.backends.is_empty() {
            println!("   (none — check --weights-dir)");
        } else {
            println!(
                "   {bold}{:<18} {:<12} {:<6} {:<26} {:>8}  {:>8}{reset}",
                "IP", "State", "Slot", "Owner Instance", "ActConn", "InactConn"
            );
            println!("   {dim}{sep}{reset}");
            for b in &self.backends {
                let colour = b.weight_state.colour();
                let slot_s = b.slot.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
                let owner = b
                    .lease
                    .as_ref()
                    .map(|l| l.owner_instance_id.as_str())
                    .unwrap_or("-");
                let (active, inactive) = b
                    .ipvs
                    .as_ref()
                    .map(|i| {
                        (
                            i.active_connections.to_string(),
                            i.inactive_connections.to_string(),
                        )
                    })
                    .unwrap_or_else(|| ("-".into(), "-".into()));

                println!(
                    "   {:<18} {colour}{:<12}{reset} {:<6} {:<26} {:>8}  {:>8}",
                    b.ip.to_string(),
                    b.weight_state.label(),
                    slot_s,
                    owner,
                    active,
                    inactive,
                );
            }
        }

        // ── Redis leases ───────────────────────────────────────────────────────
        println!(
            "\n {bold}Redis Leases{reset}  ({} slot(s) leased)\n",
            self.leases.len()
        );
        if self.leases.is_empty() {
            println!("   (none)");
        } else {
            println!(
                "   {bold}{:<6} {:<26} {:<12} {}{reset}",
                "Slot", "Owner Instance", "Expires in", "Status"
            );
            println!("   {dim}{sep}{reset}");
            for l in &self.leases {
                let status = if l.is_expired() {
                    "\x1b[31mEXPIRED\x1b[0m"
                } else {
                    ""
                };
                println!(
                    "   {:<6} {:<26} {:>9.1} s  {}",
                    l.slot,
                    l.owner_instance_id,
                    l.remaining_secs(),
                    status,
                );
            }
        }

        // ── ASG ────────────────────────────────────────────────────────────────
        let in_service = self.asg.iter().filter(|i| i.is_in_service()).count();
        let capacity_line = self
            .asg_group
            .as_ref()
            .map(|g| {
                let warn = if g.would_violate_min() {
                    " \x1b[33m[!] desired=min\x1b[0m"
                } else {
                    ""
                };
                format!(
                    "  desired={}  min={}  max={}{}",
                    g.desired_capacity, g.min_size, g.max_size, warn
                )
            })
            .unwrap_or_default();
        println!(
            "\n {bold}ASG Instances{reset}  ({} total, {} InService){capacity_line}\n",
            self.asg.len(),
            in_service
        );
        if self.asg.is_empty() {
            println!("   (none — ASG may be empty or query failed)");
        } else {
            println!(
                "   {bold}{:<26} {:<14} {}{reset}",
                "Instance ID", "Health", "Lifecycle"
            );
            println!("   {dim}{sep}{reset}");
            for inst in &self.asg {
                let colour = if inst.is_in_service() {
                    "\x1b[32m"
                } else {
                    "\x1b[33m"
                };
                println!(
                    "   {:<26} {:<14} {colour}{}{reset}",
                    inst.instance_id, inst.health_status, inst.lifecycle_state,
                );
            }
        }

        // ── Summary ────────────────────────────────────────────────────────────
        let active_count = self
            .backends
            .iter()
            .filter(|b| b.weight_state == BackendState::Active)
            .count();
        let draining_count = self
            .backends
            .iter()
            .filter(|b| b.weight_state == BackendState::Draining)
            .count();
        let disabled_count = self
            .backends
            .len()
            .saturating_sub(active_count + draining_count);
        let total_conn: u32 = self.ipvs.iter().map(|i| i.active_connections).sum();

        println!("\n{bold}{bar}{reset}");
        println!(
            " {bold}Summary:{reset}  \
             \x1b[32m{active_count} active\x1b[0m  \
             \x1b[33m{draining_count} drains\x1b[0m  \
             \x1b[90m{disabled_count} disabled\x1b[0m  \
             |  {leases} leases  |  {in_service} inservice  |  {total_conn} conns",
            leases = self.leases.len(),
        );
        println!("{bold}{bar}{reset}\n");
    }
}
