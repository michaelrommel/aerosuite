//! aeroscale — Backend Management and Autoscaling daemon.
//!
//! Runs on both keepalived nodes.  Only the current VRRP master performs
//! destructive actions (cleanup, termination, CloudWatch push, Redis persist).
//! The backup observes, scrapes metrics, and syncs weight files from Redis.
//!
//! See DESIGN.md for the full specification.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use aeroscale::{
    cleanup::{self, CleanupState},
    listener,
    metrics::{self, MetricsStore},
    scaler::{ScaleConfig, ScalerState},
    slot_network::SlotNetwork,
    snapshot::SystemSnapshot,
    vrrp,
    weight_sync,
};
use aerocore::{fetch_imds_credentials, fetch_imds_path, fetch_imds_token, redis_pool::build_redis_client};

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

    // ── Autoscaling ───────────────────────────────────────────────────────────

    /// Average IPVS active connections per active backend above which a
    /// scale-up is triggered.  Recommended: 50% of the per-backend design
    /// maximum.  Default: 750 (50% of 1500).
    #[arg(long, default_value_t = 750)]
    scale_up_threshold: u32,

    /// Maximum IPVS active connections on the busiest remaining backend after
    /// a drain candidate is removed.  If the worst-case redistribution would
    /// exceed this, no drain is initiated.  Recommended: 33% of per-backend
    /// design maximum.  Default: 500 (33% of 1500).
    #[arg(long, default_value_t = 500)]
    drain_threshold: u32,

    /// Number of consecutive snapshot cycles a scale-up or drain condition
    /// must persist before the action is taken (flap prevention).
    #[arg(long, default_value_t = 3)]
    hysteresis_cycles: u32,

    /// Minimum seconds between two consecutive scale-up actions.
    /// AWS typically needs 2–3 minutes to bring a new backend InService.
    #[arg(long, default_value_t = 120)]
    scale_up_cooldown_secs: u64,

    /// Minimum seconds between two consecutive drain initiations.
    #[arg(long, default_value_t = 300)]
    drain_cooldown_secs: u64,

    /// Percentage difference between IPVS active connections and the backend's
    /// scraped ftp_sessions_total above which a warning is emitted.  Set to
    /// 0.0 to warn on any non-zero difference.
    #[arg(long, default_value_t = 5.0)]
    scrape_mismatch_pct: f64,

    /// Whether TerminateInstanceInAutoScalingGroup should also decrement the
    /// ASG desired-capacity counter.  Set to false for testing to let the ASG
    /// launch a replacement automatically.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    term_decrements_capacity: bool,

    /// Seconds an `InService` instance is allowed to exist without a slot
    /// lease before it is terminated.  The termination never decrements
    /// desired capacity (the ASG replaces the instance automatically).
    /// Increase this value if instances need longer to boot and register.
    #[arg(long, default_value_t = 120)]
    orphan_grace_secs: u64,

    /// Maximum number of backends allowed in `Draining` state at the same
    /// time.  When the limit is reached the drain evaluator skips entirely.
    /// When below the limit with a drain in progress, only a zero-session
    /// backend may be added as an additional drain.
    #[arg(long, default_value_t = 2)]
    max_concurrent_draining: u32,
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
        scale_up_threshold   = args.scale_up_threshold,
        drain_threshold      = args.drain_threshold,
        hysteresis_cycles    = args.hysteresis_cycles,
        scale_up_cooldown    = args.scale_up_cooldown_secs,
        drain_cooldown       = args.drain_cooldown_secs,
        scrape_mismatch_pct  = args.scrape_mismatch_pct,
        term_decrements_capacity = args.term_decrements_capacity,
        "aeroscale starting"
    );

    if args.dry_run {
        info!("DRY-RUN mode — no writes will be performed");
    }

    // ── Resolve vip_inside ─────────────────────────────────────────────────────────
    // Priority: CLI flag → IMDS tag aeroftp-vip-inside → None (assume master).
    let vip_inside: Option<Ipv4Addr> = match args.vip_inside {
        Some(vip) => {
            info!(%vip, "vip-inside: using CLI value");
            Some(vip)
        }
        None => {
            let resolved = async {
                let token = fetch_imds_token().await?;
                let s = fetch_imds_path(&token, "tags/instance/aeroftp-vip-inside").await?;
                s.trim().parse::<Ipv4Addr>().map_err(|e| anyhow::anyhow!("invalid IP in aeroftp-vip-inside tag: {e}"))
            }.await;
            match resolved {
                Ok(vip) => {
                    info!(%vip, "vip-inside: resolved from IMDS tag aeroftp-vip-inside");
                    Some(vip)
                }
                Err(e) => {
                    warn!(
                        "--vip-inside not set and aeroftp-vip-inside IMDS tag not found \
                         ({e:#}); always assuming MASTER role. \
                         Add the tag to the load balancer launch template for correct \
                         two-node behaviour."
                    );
                    None
                }
            }
        }
    };

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
        args.term_decrements_capacity,
    ));

    let metrics_store: MetricsStore = metrics::new_store();
    tokio::spawn(metrics::server::serve(
        Arc::clone(&metrics_store),
        args.metrics_port,
    ));

    // ── Build scale config (immutable for the lifetime of the daemon) ─────────
    let scale_config = ScaleConfig {
        scale_up_threshold:      args.scale_up_threshold,
        drain_threshold:         args.drain_threshold,
        hysteresis_cycles:       args.hysteresis_cycles,
        scale_up_cooldown_secs:  args.scale_up_cooldown_secs,
        drain_cooldown_secs:     args.drain_cooldown_secs,
        max_concurrent_draining: args.max_concurrent_draining,
    };

    // Mutable scaler state: hysteresis counters and last-action timestamps.
    // Lives outside the loop so state is preserved between cycles.
    let mut scaler_state  = ScalerState::default();
    let mut cleanup_state = CleanupState::default();
    // Last successfully read owner instance-id per slot.
    // Survives across snapshot cycles so that a transient Redis GET failure
    // does not erase the known owner and trigger a spurious slot release.
    let mut owner_cache: HashMap<u32, String> = HashMap::new();

    // ── Refresh loop ──────────────────────────────────────────────────────────

    let interval = Duration::from_secs(args.snapshot_interval);

    loop {
        // Determine role each cycle — handles failover transparently.
        let is_master = match vip_inside {
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
            &mut owner_cache,
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
                    args.term_decrements_capacity,
                    &mut cleanup_state,
                    args.orphan_grace_secs,
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
                    args.scrape_mismatch_pct,
                )
                .await;

                // P5: Scale-up / drain algorithm (master only)
                if is_master {
                    if let Err(e) = aeroscale::scaler::run(
                        &snapshot,
                        &scale_config,
                        &mut scaler_state,
                        &args.asg_name,
                        &args.region,
                        &creds,
                        &args.weights_dir,
                        args.dry_run,
                    )
                    .await
                    {
                        error!("scaler pass failed: {e:#}");
                    }
                }
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
