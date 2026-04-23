//! aerogym — FTP load-test agent controlled by aerocoach.
//!
//! # Startup sequence
//! 1. Parse configuration from environment variables.
//! 2. Connect to aerocoach and call `Register` (with retry).
//! 3. Pre-generate one file per file-size bucket.
//! 4. Open the bidirectional `Session` stream and enter the slice loop.
//! 5. Execute transfers as directed by `SliceTick` commands.
//! 6. On `ShutdownCmd`: drain in-flight transfers, send final metrics, exit.

mod agent;

use anyhow::Result;
use tracing::info;

use agent::{config::Config, file_manager, registration, session};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Tracing ───────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // ── Config ────────────────────────────────────────────────────────────
    let config = Config::from_env()?;
    info!(
        agent_id    = %config.agent_id,
        coach_url   = %config.aerocoach_url,
        ftp_target  = %config.ftp_target,
        work_dir    = %config.work_dir.display(),
        "aerogym agent starting"
    );

    // ── Register with aerocoach ───────────────────────────────────────────
    let reg = registration::register(&config).await?;

    let mut plan = agent::load_plan::AgentPlan::new(reg.load_plan, reg.agent_index);

    info!(
        plan_id      = %plan.plan_id(),
        agent_index  = reg.agent_index,
        total_agents = plan.total_agents(),
        total_slices = plan.total_slices(),
        my_bw_bps    = plan.my_bandwidth_bps(),
        "plan received"
    );

    // ── Pre-generate bucket files ─────────────────────────────────────────
    let bucket_files =
        file_manager::generate(&config.work_dir, &config.agent_id, plan.buckets()).await?;

    info!(
        files = bucket_files.len(),
        dir   = %config.work_dir.display(),
        "bucket files ready"
    );

    // ── Run session ───────────────────────────────────────────────────────
    session::run(reg.channel, &config, &mut plan, &bucket_files).await?;

    info!("aerogym agent exiting cleanly");
    Ok(())
}
