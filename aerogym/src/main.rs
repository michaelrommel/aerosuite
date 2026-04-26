//! aerogym — FTP load-test agent controlled by aerocoach.
//!
//! # Startup sequence
//! 1. Parse configuration from environment variables.
//! 2. Connect to aerocoach and call `Register` (with retry).
//! 3. Pre-generate one file per file-size bucket.
//! 4. Open the bidirectional `Session` stream and enter the slice loop.
//! 5. Execute transfers as directed by `SliceTick` commands.
//! 6. On `ShutdownCmd`: drain in-flight transfers, send final metrics.
//! 7. Loop back to step 2 — wait for aerocoach to reset and accept a new run.

mod agent;

use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

use agent::{config::Config, file_manager, registration, session};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Tracing ───────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::env::var_os("NO_COLOR").is_none())
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

    // ── Main loop: register → run → wait for reset → repeat ──────────────
    //
    // After each completed test run aerocoach transitions to DONE.  The
    // operator calls POST /reset to put it back to WAITING, at which point
    // the agent re-registers here and is ready for the next run.  This lets
    // long-running ECS tasks participate in multiple test runs without being
    // stopped and restarted.
    loop {
        // ── Register with aerocoach (retries with exp. back-off) ──────────
        let reg = match registration::register(&config).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "registration permanently failed — retrying loop");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        let mut plan = agent::load_plan::AgentPlan::new(reg.load_plan, reg.agent_index);

        info!(
            plan_id      = %plan.plan_id(),
            agent_index  = reg.agent_index,
            total_agents = plan.total_agents(),
            total_slices = plan.total_slices(),
            my_bw_bps    = plan.my_bandwidth_bps(0),
            "plan received"
        );

        // ── Pre-generate bucket files ─────────────────────────────────────
        let bucket_files = match file_manager::generate(
            &config.work_dir,
            &config.agent_id,
            plan.buckets(),
        ).await {
            Ok(files) => files,
            Err(e) => {
                warn!(error = %e, "bucket file generation failed — retrying loop");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        info!(
            files = bucket_files.len(),
            dir   = %config.work_dir.display(),
            "bucket files ready"
        );

        // ── Run session ───────────────────────────────────────────────────
        match session::run(reg.channel, &config, &mut plan, &bucket_files).await {
            Ok(()) => {
                info!(
                    "test run complete — waiting for aerocoach to reset \
                     before re-registering for the next run"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "session ended with error — will re-register after brief delay"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        // Loop immediately: registration::register() has its own exponential
        // back-off and will block until aerocoach is back in WAITING state.
    }
}
