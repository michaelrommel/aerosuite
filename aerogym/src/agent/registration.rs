//! Agent registration: calls aerocoach `Register` RPC, receives the
//! [`LoadPlan`] and assigned agent index.
//!
//! Retries up to [`MAX_RETRIES`] times with linear back-off so that agents
//! started in a container can tolerate aerocoach not being ready yet.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use aeroproto::aeromonitor::{
    agent_service_client::AgentServiceClient, LoadPlan, RegisterRequest,
};
use tonic::transport::Channel;

use super::config::Config;

const MAX_RETRIES: u32 = 8;
const RETRY_DELAY: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of a successful registration.
pub struct Registration {
    /// gRPC channel kept open for the subsequent `Session` call.
    pub channel: Channel,
    /// Index assigned by aerocoach (0-based; used for load-share calculations).
    pub agent_index: u32,
    /// Full load plan as provided by aerocoach.
    pub load_plan: LoadPlan,
}

/// Connect to aerocoach and call `Register`, retrying on transient failures.
///
/// # Errors
/// Returns an error after all retries are exhausted, or if aerocoach rejects
/// the registration (e.g. state is not WAITING, or no plan is loaded).
pub async fn register(config: &Config) -> Result<Registration> {
    let mut last_err = None;

    for attempt in 1..=MAX_RETRIES {
        match try_register(config).await {
            Ok(reg) => return Ok(reg),
            Err(e) => {
                warn!(
                    attempt,
                    max = MAX_RETRIES,
                    error = %e,
                    "registration failed, retrying in {}s",
                    RETRY_DELAY.as_secs()
                );
                last_err = Some(e);
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }

    Err(last_err
        .unwrap()
        .context(format!("registration failed after {MAX_RETRIES} attempts")))
}

/// Single registration attempt (no retry).
async fn try_register(config: &Config) -> Result<Registration> {
    // ── Connect ────────────────────────────────────────────────────────────
    let endpoint = tonic::transport::Channel::builder(
        config
            .aerocoach_url
            .parse()
            .with_context(|| format!("invalid AEROCOACH_URL: {:?}", config.aerocoach_url))?,
    )
    .connect_timeout(CONNECT_TIMEOUT);

    let channel = endpoint
        .connect()
        .await
        .with_context(|| format!("could not connect to aerocoach at {}", config.aerocoach_url))?;

    // ── Register ───────────────────────────────────────────────────────────
    let mut client = AgentServiceClient::new(channel.clone());

    let response = client
        .register(RegisterRequest {
            agent_id:      config.agent_id.clone(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            private_ip:    config.private_ip.clone(),
            instance_id:   config.instance_id.clone(),
        })
        .await
        .context("Register RPC failed")?
        .into_inner();

    if !response.accepted {
        bail!("aerocoach rejected registration: {}", response.reject_reason);
    }

    let load_plan = response
        .load_plan
        .context("aerocoach accepted registration but sent no load plan")?;

    info!(
        agent_id    = %config.agent_id,
        agent_index = response.agent_index,
        plan_id     = %load_plan.plan_id,
        slices      = load_plan.slices.len(),
        total_agents = load_plan.total_agents,
        "registered successfully"
    );

    Ok(Registration {
        channel,
        agent_index: response.agent_index,
        load_plan,
    })
}
