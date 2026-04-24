//! Redis `asg-change` channel subscriber.
//!
//! Runs as a background tokio task.  Reacts immediately to slot claim and
//! release events published by `aeroslot`, rather than waiting for the next
//! periodic snapshot refresh.
//!
//! ## Message format
//!
//! ```json
//! // Backend claimed a slot — enable it immediately
//! { "slot": 3, "action": "claim" }
//!
//! // Backend released a slot gracefully — disable and terminate
//! { "slot": 3, "action": "release", "instance_id": "i-0abc1234567890def" }
//! ```
//!
//! ## Behaviour
//!
//! | `action`    | Steps taken                                                     |
//! |-------------|-----------------------------------------------------------------|
//! | `"claim"`   | Write `"0"` to the backend weight file (enable immediately)     |
//! | `"release"` | Write `"-2147483648"` (disable); terminate the instance if      |
//! |             | `instance_id` is present in the message                         |
//!
//! After handling each message the main loop is notified via `Arc<Notify>` so
//! a snapshot refresh runs immediately rather than after the next timer tick.
//!
//! ## Resilience
//!
//! If the pub/sub connection drops, the task reconnects automatically with a
//! short delay so a Redis restart or network blip does not kill the daemon.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::sync::Notify;
use tokio_stream::StreamExt as _;
use tracing::{debug, error, info, warn};

use tokio::sync::RwLock;
use aerocore::AwsCredentials;

use crate::cleanup::{terminate_instance, write_weight, WEIGHT_ACTIVE, WEIGHT_DISABLED};
use crate::slot_network::SlotNetwork;

// ── Message type ──────────────────────────────────────────────────────────────

const ASG_CHANGE_CHANNEL: &str = "asg-change";

#[derive(Debug, Deserialize)]
struct AsgChangeMsg {
    slot:        u32,
    action:      String,
    /// Present only on `"release"` messages (added by aeroslot R4).
    instance_id: Option<String>,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Spawn and run the listener forever.  Reconnects on failure.
///
/// This is a plain `async fn` — call it inside `tokio::spawn`.
pub async fn run(
    redis_client:             redis::Client,
    slot_network:             Arc<SlotNetwork>,
    weights_dir:              String,
    region:                   String,
    creds:                    Arc<RwLock<AwsCredentials>>,
    notify:                   Arc<Notify>,
    dry_run:                  bool,
    term_decrements_capacity: bool,
) {
    loop {
        match subscribe_once(
            &redis_client,
            &slot_network,
            &weights_dir,
            &region,
            &creds,
            &notify,
            dry_run,
            term_decrements_capacity,
        )
        .await
        {
            Ok(()) => warn!("pub/sub stream ended unexpectedly — reconnecting in 5 s"),
            Err(e) => error!("pub/sub error: {e:#} — reconnecting in 5 s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ── Inner loop (one connection lifetime) ─────────────────────────────────────

async fn subscribe_once(
    redis_client:             &redis::Client,
    slot_network:             &SlotNetwork,
    weights_dir:              &str,
    region:                   &str,
    creds:                    &RwLock<AwsCredentials>,
    notify:                   &Notify,
    dry_run:                  bool,
    term_decrements_capacity: bool,
) -> Result<()> {
    let mut pubsub = redis_client
        .get_async_pubsub()
        .await
        .context("Failed to open Redis pub/sub connection")?;

    pubsub
        .subscribe(ASG_CHANGE_CHANNEL)
        .await
        .with_context(|| format!("Failed to subscribe to '{ASG_CHANGE_CHANNEL}'"))?;

    info!("subscribed to Redis channel '{ASG_CHANGE_CHANNEL}'");

    let mut stream = pubsub.on_message();

    while let Some(msg) = stream.next().await {
        let raw: String = match msg.get_payload() {
            Ok(s)  => s,
            Err(e) => { warn!("asg-change: cannot read payload: {e}"); continue; }
        };

        debug!("asg-change raw message: {raw}");

        let parsed: AsgChangeMsg = match serde_json::from_str(&raw) {
            Ok(m)  => m,
            Err(e) => { warn!("asg-change: cannot parse JSON '{raw}': {e}"); continue; }
        };

        handle_message(parsed, slot_network, weights_dir, region, creds, notify, dry_run, term_decrements_capacity).await;
    }

    Ok(())
}

// ── Message handler ───────────────────────────────────────────────────────────

async fn handle_message(
    msg:                      AsgChangeMsg,
    slot_network:             &SlotNetwork,
    weights_dir:              &str,
    region:                   &str,
    creds:                    &RwLock<AwsCredentials>,
    notify:                   &Notify,
    dry_run:                  bool,
    term_decrements_capacity: bool,
) {
    let ip = slot_network.ip_for_slot(msg.slot);

    info!(
        slot        = msg.slot,
        action      = %msg.action,
        %ip,
        instance_id = msg.instance_id.as_deref().unwrap_or("-"),
        "asg-change"
    );

    match msg.action.as_str() {
        "claim" => {
            // A backend just claimed this slot and is starting up.
            // Enable the backend immediately so keepalived starts sending traffic.
            if let Err(e) = write_weight(weights_dir, ip, WEIGHT_ACTIVE, dry_run).await {
                error!(slot = msg.slot, %ip, "claim: write_weight failed: {e:#}");
            }
        }

        "release" => {
            // A backend is shutting down gracefully.
            // Disable it first, then terminate the ASG instance.
            if let Err(e) = write_weight(weights_dir, ip, WEIGHT_DISABLED, dry_run).await {
                error!(slot = msg.slot, %ip, "release: write_weight failed: {e:#}");
            }

            match msg.instance_id.as_deref() {
                Some(instance_id) => {
                    let creds_r = creds.read().await;
                    if let Err(e) = terminate_instance(instance_id, region, &*creds_r, dry_run, term_decrements_capacity).await {
                        error!(
                            slot = msg.slot,
                            instance_id,
                            "release: terminate_instance failed: {e:#}"
                        );
                    }
                }
                None => {
                    // Older aeroslot versions (pre-R4) don't include instance_id.
                    // The P2 cleanup loop will catch the expired lease on the next refresh.
                    warn!(
                        slot = msg.slot,
                        %ip,
                        "release message has no instance_id — \
                         termination deferred to cleanup loop \
                         (upgrade aeroslot to include instance_id in release messages)"
                    );
                }
            }
        }

        other => {
            warn!(slot = msg.slot, action = other, "asg-change: unknown action — ignoring");
        }
    }

    // Signal the main loop to refresh the snapshot immediately.
    notify.notify_one();
}
