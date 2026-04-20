//! aeroscale — Backend Management and Autoscaling daemon.
//!
//! Runs on both keepalived nodes.  Only the current VRRP master performs
//! destructive actions (cleanup, termination, CloudWatch push, Redis persist).
//! The backup observes, scrapes metrics, and syncs weight files from Redis.
//!
//! See DESIGN.md for the full specification.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use aeroscale::{
    cleanup,
    listener,
    metrics::{self, MetricsStore},
    slot_network::SlotNetwork,
    snapshot::SystemSnapshot,
    vrrp,
    weight_sync,
};
use aerocore::{fetch_imds_credentials, redis_pool::build_redis_client};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "aeroscale")]
#[command(about = "Backend management and autoscaling daemon for aeroftp")]
struct Args {
    /// AWS region
    #[arg(long, default_value = "eu-west-2")]
    region: String,

    /// Auto Scaling Group name for FTP backends
    #[arg(long, default_value = "aeroftp-backend")]
    asg_name: String,

    /// Redis connection URL
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    redis_url: String,

    /// Directory containing backend-<IP>.weight files
    #[arg(long, default_value = "/etc/keepalived/weights")]
    weights_dir: String,

    /// Port to expose the aggregated Prometheus /metrics endpoint
    #[arg(long, default_value_t = 9090)]
    metrics_port: u16,

    /// Port on which each backend exposes its own Prometheus metrics
    #[arg(long, default_value_t = 9090)]
    scrape_port: u16,

    /// AWS CloudWatch namespace for pushed metrics
    #[arg(long, default_value = "AeroFTP/Autoscaler")]
    cloudwatch_namespace: String,

    /// Seconds between full state refreshes
    #[arg(long, default_value_t = 30)]
    snapshot_interval: u64,

    /// Log actions but do not write anything (safe for testing)
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Enable TLS for Redis
    #[arg(long, default_value_t = false)]
    tls: bool,

    /// Skip Redis TLS certificate verification (implies --tls)
    #[arg(long, default_value_t = false)]
    tls_insecure: bool,

    // ── Slot network ──────────────────────────────────────────────────────────

    /// Override the backend slot subnet base IP.
    /// Default: read from the load balancer's eth1 subnet CIDR via IMDS.
    #[arg(long, value_name = "IP")]
    slot_base: Option<Ipv4Addr>,

    /// Override the slot offset within the subnet.
    /// Default: read from the 'aeroftp-slot-offset' instance tag via IMDS.
    #[arg(long, value_name = "N")]
    slot_offset: Option<u32>,

    // ── VRRP role detection ───────────────────────────────────────────────────

    /// Inside VIP address used to detect whether this node is the VRRP master.
    /// If this IP appears in `ip addr show`, this node is the master.
    /// If unset, master mode is always assumed (fine for single-node or dev).
    #[arg(long, value_name = "IP")]
    vip_inside: Option<Ipv4Addr>,

    // ── Weight state TTL ──────────────────────────────────────────────────────

    /// Maximum age (seconds) of Redis weight state before it is considered
    /// stale and recomputed from current lease state instead.
    /// Set to 0 to always recompute from leases (never restore from Redis).
    #[arg(long, default_value_t = 3600)]
    weight_state_ttl: u64,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!(
        region               = %args.region,
        asg_name             = %args.asg_name,
        redis_url            = %args.redis_url,
        weights_dir          = %args.weights_dir,
        metrics_port         = args.metrics_port,
        scrape_port          = args.scrape_port,
        cloudwatch_namespace = %args.cloudwatch_namespace,
        snapshot_interval    = args.snapshot_interval,
        dry_run              = args.dry_run,
        vip_inside           = %args.vip_inside.map(|v| v.to_string()).unwrap_or_else(|| "unset (assuming master)".into()),
        weight_state_ttl     = args.weight_state_ttl,
        "aeroscale starting"
    );

    if args.dry_run {
        info!("DRY-RUN mode — no writes will be performed");
    }
    if args.vip_inside.is_none() {
        warn!("--vip-inside not set; always assuming MASTER role. \
               Set it for correct two-node behaviour.");
    }

    // ── One-time setup ────────────────────────────────────────────────────────

    let slot_network = Arc::new(match (args.slot_base, args.slot_offset) {
        (Some(base), Some(offset)) => {
            info!(%base, offset, "slot network: using CLI overrides");
            SlotNetwork::new(base, offset, 0)
        }
        _ => {
            info!("slot network: resolving from IMDS …");
            SlotNetwork::from_imds()
                .await
                .context(
                    "Failed to resolve slot network from IMDS. \
                     Provide --slot-base and --slot-offset if not running on EC2.",
                )?
        }
    });

    info!(
        base   = %slot_network.base,
        offset = slot_network.offset,
        prefix = slot_network.prefix_len,
        "slot network ready  (slot 0 = {})",
        slot_network.ip_for_slot(0),
    );

    info!("fetching AWS credentials from IMDSv2 …");
    let creds = Arc::new(
        fetch_imds_credentials()
            .await
            .context("Failed to obtain AWS credentials from IMDS")?,
    );

    info!("connecting to Redis …");
    let redis_client = build_redis_client(&args.redis_url, args.tls, args.tls_insecure, &None)
        .context("Failed to build Redis client")?;
    let mut redis_con = redis_client
        .get_multiplexed_async_connection()
        .await
        .context("Failed to connect to Redis")?;
    info!("Redis connection established");

    // ── Startup weight-file initialisation ────────────────────────────────────
    // keepalived has already set all weight files to "-1" (draining).
    // Overwrite with the correct values from Redis (if fresh) or from the
    // current lease state (if this is a first run or a full restart).
    info!("initialising weight files …");
    if let Err(e) = weight_sync::init(
        &args.weights_dir,
        &mut redis_con,
        &slot_network,
        args.weight_state_ttl,
    )
    .await
    {
        warn!("weight file init failed: {e:#} — weight files remain as draining; \
               cleanup will fix them on the first pass");
    }

    // ── Spawn background tasks ────────────────────────────────────────────────

    let notify = Arc::new(Notify::new());
    tokio::spawn(listener::run(
        redis_client.clone(),
        Arc::clone(&slot_network),
        args.weights_dir.clone(),
        args.region.clone(),
        Arc::clone(&creds),
        Arc::clone(&notify),
        args.dry_run,
    ));

    let metrics_store: MetricsStore = metrics::new_store();
    tokio::spawn(metrics::server::serve(
        Arc::clone(&metrics_store),
        args.metrics_port,
    ));

    // ── Refresh loop ──────────────────────────────────────────────────────────

    let interval = Duration::from_secs(args.snapshot_interval);

    loop {
        // Determine role each cycle — handles failover transparently.
        let is_master = match args.vip_inside {
            Some(vip) => vrrp::is_master(vip).await,
            None      => true,
        };

        info!(is_master, "── cycle ─────────────────────────────────────────────────────────────");

        // Backup: sync weight files from Redis before collecting the snapshot
        // so the local view is up-to-date with what the master decided.
        if !is_master {
            if let Err(e) = weight_sync::sync_from_redis(&args.weights_dir, &mut redis_con).await {
                warn!("weight sync from Redis failed: {e:#}");
            }
        }

        // Collect snapshot (always — both master and backup observe state)
        match SystemSnapshot::collect(
            &args.weights_dir,
            &args.region,
            &args.asg_name,
            &creds,
            &mut redis_con,
            &slot_network,
        )
        .await
        {
            Ok(snapshot) => {
                snapshot.print();

                // Cleanup (master only — backup skips internally)
                if let Err(e) = cleanup::run(
                    &snapshot,
                    &args.weights_dir,
                    &args.region,
                    &creds,
                    &mut redis_con,
                    args.dry_run,
                    is_master,
                )
                .await
                {
                    error!("cleanup pass failed: {e:#}");
                }

                // Master: persist weight state so backup can sync
                if is_master {
                    if let Err(e) =
                        weight_sync::persist(&args.weights_dir, &mut redis_con).await
                    {
                        warn!("weight state persist failed: {e:#}");
                    }
                }

                // Scrape metrics (always) + CloudWatch push (master only)
                metrics::scrape_and_push(
                    &snapshot,
                    &metrics_store,
                    args.scrape_port,
                    &args.region,
                    &creds,
                    &args.cloudwatch_namespace,
                    is_master,
                )
                .await;

                // TODO P5: scale-up / drain algorithm
            }
            Err(e) => {
                error!("snapshot collection failed: {e:#}");
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                info!("periodic refresh ({}s interval)", args.snapshot_interval);
            }
            _ = notify.notified() => {
                info!("asg-change received — refreshing snapshot immediately");
            }
        }
    }
}
