//! aeroslot — Redis slot pool management using plain Redis commands.
//!
//! Functionally identical to aeroslot-lua but all logic runs in Rust.
//! No Lua scripts.

use aerocore::redis_pool::{build_redis_client, key_owner, now_ms, KEY_AVAILABLE, KEY_LEASES};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use redis::{aio::MultiplexedConnection, AsyncCommands, SortedSetAddOptions};
use std::path::PathBuf;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "aeroslot")]
#[command(about = "Manage a Redis-backed pool of numbered slots")]
#[command(long_about = None)]
struct Args {
    /// Redis connection URL (env: REDIS_URL).
    /// Use rediss:// for TLS, or combine redis:// with --tls.
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    redis_url: String,

    /// Enable TLS. Automatically switches redis:// to rediss://.
    #[arg(long)]
    tls: bool,

    /// Skip certificate verification — for self-signed certs (implies --tls).
    #[arg(long)]
    tls_insecure: bool,

    /// PEM-encoded CA certificate file for verifying the Redis server.
    #[arg(long)]
    tls_ca_cert: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialise the slot pool (idempotent; skips leased or already-free slots)
    Init {
        /// First slot number, inclusive
        #[arg(long, default_value_t = 0)]
        min: u32,

        /// Last slot number, inclusive
        #[arg(long, default_value_t = 19)]
        max: u32,
    },

    /// Claim one free slot atomically (also sweeps expired leases)
    Claim {
        /// Unique identifier for this app instance (e.g. hostname or UUID)
        #[arg(long)]
        instance_id: String,

        /// Lease duration in milliseconds  [default: 90 000]
        #[arg(long, default_value_t = 90_000)]
        ttl_ms: u64,
    },

    /// Extend the TTL on an already-held slot (heartbeat)
    Renew {
        /// Slot number to renew
        #[arg(long)]
        slot: u32,

        /// Instance ID that currently owns the slot
        #[arg(long)]
        instance_id: String,

        /// New TTL in milliseconds  [default: 90 000]
        #[arg(long, default_value_t = 90_000)]
        ttl_ms: u64,
    },

    /// Continuously renew a held slot on a fixed interval until SIGTERM
    Heartbeat {
        /// Slot number to renew
        #[arg(long)]
        slot: u32,

        /// Instance ID that currently owns the slot
        #[arg(long)]
        instance_id: String,

        /// Lease TTL to renew to on each tick, in milliseconds  [default: 90 000]
        #[arg(long, default_value_t = 90_000)]
        ttl_ms: u64,

        /// How often to renew, in seconds.  Should be well below ttl_ms / 1000
        /// to avoid the lease expiring between two heartbeats.  [default: 10]
        #[arg(long, default_value_t = 30)]
        interval: u64,

        /// How many times to retry a failed renew before giving up.
        /// Each attempt is preceded by a reconnection attempt and a delay.
        /// Only transient Redis/IO errors are retried; logical errors
        /// (wrong owner, expired lease) cause an immediate exit.  [default: 5]
        #[arg(long, default_value_t = 5)]
        retry_attempts: u32,

        /// Milliseconds to wait between retry attempts.  [default: 6 000]
        #[arg(long, default_value_t = 6_000)]
        retry_delay_ms: u64,
    },

    /// Return a slot to the free pool (graceful shutdown)
    Release {
        /// Slot number to release
        #[arg(long)]
        slot: u32,

        /// Instance ID that currently owns the slot
        #[arg(long)]
        instance_id: String,
    },

    /// Show pool state: free slots, active leases, owners, and remaining TTLs
    Status,
}


// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let client = build_redis_client(
        &args.redis_url,
        args.tls,
        args.tls_insecure,
        &args.tls_ca_cert,
    )?;

    let mut con = client
        .get_multiplexed_async_connection()
        .await
        .context("Failed to connect to Redis")?;

    match args.command {
        Command::Init { min, max } => cmd_init(&mut con, min, max).await,
        Command::Claim {
            instance_id,
            ttl_ms,
        } => cmd_claim(&mut con, &instance_id, ttl_ms).await,
        Command::Renew {
            slot,
            instance_id,
            ttl_ms,
        } => cmd_renew(&mut con, slot, &instance_id, ttl_ms).await,
        Command::Heartbeat {
            slot,
            instance_id,
            ttl_ms,
            interval,
            retry_attempts,
            retry_delay_ms,
        } => {
            cmd_heartbeat(
                &client,
                slot,
                &instance_id,
                ttl_ms,
                interval,
                retry_attempts,
                retry_delay_ms,
            )
            .await
        }
        Command::Release { slot, instance_id } => cmd_release(&mut con, slot, &instance_id).await,
        Command::Status => cmd_status(&mut con).await,
    }
}

// ── ASG-change notification ─────────────────────────────────────────────────

const ASG_CHANGE_CHANNEL: &str = "asg-change";

/// Publish a slot event on the `asg-change` Redis channel so that the
/// autoscaler on the keepalived MASTER can react in real time.
///
/// `instance_id` is included in `release` messages so the autoscaler can
/// terminate the correct instance without an extra Redis lookup.
async fn publish_asg_change(
    con:         &mut MultiplexedConnection,
    slot:        &str,
    action:      &str,
    instance_id: Option<&str>,
) -> Result<()> {
    let slot_num: u64 = slot.parse().unwrap_or(0);
    let mut payload = serde_json::json!({
        "slot":   slot_num,
        "action": action,
    });
    if let Some(id) = instance_id {
        payload["instance_id"] = serde_json::Value::String(id.to_string());
    }
    let msg = payload.to_string();

    let _: () = redis::cmd("PUBLISH")
        .arg(ASG_CHANGE_CHANNEL)
        .arg(&msg)
        .query_async(con)
        .await
        .with_context(|| format!("PUBLISH {ASG_CHANGE_CHANNEL} failed"))?;

    Ok(())
}

// ── Subcommand implementations ────────────────────────────────────────────────

async fn cmd_init(con: &mut MultiplexedConnection, min: u32, max: u32) -> Result<()> {
    if min > max {
        bail!("--min ({min}) must be ≤ --max ({max})");
    }

    let mut added = 0usize;
    for slot in min..=max {
        let s = slot.to_string();
        // Only add to the free set if not currently leased
        let leased: bool = con
            .zscore::<_, _, Option<f64>>(KEY_LEASES, &s)
            .await
            .context("ZSCORE failed")?
            .is_some();

        if !leased {
            // NX: skip if already free (preserves existing "free since" score)
            // Score 0: treated as "free since forever", claimed before any recently-released slot
            added += con
                .zadd_options::<_, _, _, usize>(
                    KEY_AVAILABLE,
                    &s,
                    0u64,
                    &SortedSetAddOptions::add_only(),
                )
                .await
                .context("ZADD failed")?;
        }
    }

    let total = max - min + 1;
    println!("✅ Pool ready: slots {min}–{max} ({total} total, {added} newly added to free set).");
    Ok(())
}

async fn cmd_claim(con: &mut MultiplexedConnection, instance_id: &str, ttl_ms: u64) -> Result<()> {
    // Sweep expired leases back into the free set.
    // Score = now: just freed, so they rank last and cool off before reuse.
    // Concurrent sweeps are harmless: ZREM is idempotent, ZADD on an existing
    // member just updates the score (same value), ZPOPMIN below is atomic.
    let now = now_ms();
    let expired: Vec<String> = con
        .zrangebyscore(KEY_LEASES, "-inf", now)
        .await
        .context("ZRANGEBYSCORE failed")?;

    for slot in &expired {
        let _: () = con.zrem(KEY_LEASES, slot).await.context("ZREM failed")?;
        let _: () = con
            .zadd(KEY_AVAILABLE, slot, now)
            .await
            .context("ZADD failed")?;
        let _: () = con.del(key_owner(slot)).await.context("DEL failed")?;
    }

    // Pop the slot free the longest (lowest score = oldest free time).
    // Maximises the cooling-off window before an IP/slot is re-assigned.
    let result: Vec<(String, f64)> = con
        .zpopmin(KEY_AVAILABLE, 1isize)
        .await
        .context("ZPOPMIN failed")?;

    let slot = result.into_iter().next().map(|(s, _)| s);

    match slot {
        None => {
            eprintln!("❌ No free slots available.");
            std::process::exit(1);
        }
        Some(s) => {
            // 3. Register the lease.
            let expiry = now + ttl_ms;
            let _: () = con
                .zadd(KEY_LEASES, &s, expiry)
                .await
                .context("ZADD failed")?;
            let _: () = con
                .set(key_owner(&s), instance_id)
                .await
                .context("SET failed")?;

            println!(
                "✅ Claimed slot {s} for '{instance_id}' (TTL {:.0} s).",
                ttl_ms as f64 / 1000.0
            );
            // Bare slot number on its own line so shell callers can capture it:
            //   SLOT=$(slot-pool-native claim --instance-id "$HOSTNAME" | tail -1)
            println!("{s}");
            publish_asg_change(con, &s, "claim", None).await?;
        }
    }
    Ok(())
}

async fn cmd_renew(
    con: &mut MultiplexedConnection,
    slot: u32,
    instance_id: &str,
    ttl_ms: u64,
) -> Result<()> {
    let slot_str = slot.to_string();

    // Check ownership first.  If this fails the application is in a severe
    // state (someone else holds our IP slot) — propagate as a hard error so
    // the caller can decide to panic / alert.
    let owner: Option<String> = con
        .get(key_owner(&slot_str))
        .await
        .context("GET owner failed")?;

    match owner.as_deref() {
        Some(o) if o == instance_id => {
            let expiry = now_ms() + ttl_ms;
            let _: () = con
                .zadd(KEY_LEASES, &slot_str, expiry)
                .await
                .context("ZADD failed")?;
            println!(
                "✅ Lease on slot {slot} renewed for '{instance_id}' (+{:.0} s).",
                ttl_ms as f64 / 1000.0
            );
        }
        Some(other) => {
            // Another instance holds this slot — fatal for IP-based use cases.
            bail!(
                "Slot {slot} is owned by '{other}', not '{instance_id}'. \
                 IP conflict likely — caller should panic."
            );
        }
        None => {
            bail!(
                "Slot {slot} has no owner (lease expired or slot was never claimed). \
                 Caller should panic."
            );
        }
    }
    Ok(())
}

/// Returns `true` when the error chain contains a transient Redis network or
/// IO error — i.e. the kind of error that a reconnect might fix.
///
/// Logical errors produced by `bail!()` (wrong owner, expired lease) do NOT
/// contain a `redis::RedisError` in their chain, so they return `false` here
/// and are never retried.
fn is_redis_io_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<redis::RedisError>()
            .is_some_and(|re| re.is_io_error() || re.is_connection_dropped())
    })
}

async fn cmd_heartbeat(
    client: &redis::Client,
    slot: u32,
    instance_id: &str,
    ttl_ms: u64,
    interval: u64,
    retry_attempts: u32,
    retry_delay_ms: u64,
) -> Result<()> {
    use std::time::Duration;
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm =
        signal(SignalKind::terminate()).context("Failed to register SIGTERM handler")?;

    let mut con = client
        .get_multiplexed_async_connection()
        .await
        .context("Failed to connect to Redis")?;

    println!(
        "💓 Heartbeat started — slot {slot}, owner '{instance_id}', \
         TTL {:.0} s, interval {interval} s, retry {retry_attempts}×/{retry_delay_ms} ms. \
         Send SIGTERM to stop.",
        ttl_ms as f64 / 1000.0
    );

    loop {
        // ── Renew with retry on transient IO/network errors ───────────────────
        // Logical errors (wrong owner, expired lease) propagate immediately
        // since retrying them would not help and indicates a serious problem.
        let mut attempt = 0u32;
        loop {
            match cmd_renew(&mut con, slot, instance_id, ttl_ms).await {
                Ok(()) => break,
                Err(e) if is_redis_io_error(&e) && attempt < retry_attempts => {
                    attempt += 1;
                    eprintln!("⚠️  Renew failed (attempt {attempt}/{retry_attempts}): {e:#}");
                    eprintln!("    Waiting {retry_delay_ms} ms before reconnecting…");
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;

                    // Try to establish a fresh connection.  If this also fails,
                    // the old (broken) `con` is kept and cmd_renew will fail
                    // again on the next iteration, consuming another attempt.
                    match client.get_multiplexed_async_connection().await {
                        Ok(new_con) => {
                            con = new_con;
                            eprintln!("🔌 Reconnected to Redis.");
                        }
                        Err(conn_err) => {
                            eprintln!("⚠️  Reconnect failed: {conn_err:#} — will retry.");
                        }
                    }
                }
                Err(e) => {
                    // Either a logical error or all retry attempts exhausted.
                    return Err(e).with_context(|| {
                        if attempt > 0 {
                            format!("Renew failed after {attempt} reconnect attempt(s)")
                        } else {
                            "Renew failed with a non-retryable error".to_string()
                        }
                    });
                }
            }
        }

        // ── Wait for the next tick or SIGTERM ─────────────────────────────────
        tokio::select! {
            _ = sigterm.recv() => {
                println!("🛑 SIGTERM received — stopping heartbeat for slot {slot}.");
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {
                // tick — loop back and renew again
            }
        }
    }
}

async fn cmd_release(con: &mut MultiplexedConnection, slot: u32, instance_id: &str) -> Result<()> {
    let slot_str = slot.to_string();

    // Verify ownership before touching anything.
    let owner: Option<String> = con
        .get(key_owner(&slot_str))
        .await
        .context("GET owner failed")?;

    if owner.as_deref() != Some(instance_id) {
        bail!("Slot {slot} is not owned by '{instance_id}' — refusing to release.");
    }

    // Release in this order so a concurrent claim never observes a half-released
    // slot with a stale owner:
    //
    // 1. DEL owner   — slot is ownerless; concurrent claim sweep won't touch it
    //                  (TTL not expired), ZPOPMIN can't reach it (still in leases).
    // 2. ZREM lease  — slot gone from leased set; sweep skips it, still not claimable.
    // 3. ZADD available (score = now) — slot becomes claimable, ranked last so it
    //                  cools off before being reused.  De-allocate the IP *before*
    //                  this step in the calling application.
    let now = now_ms();
    let _: () = con
        .del(key_owner(&slot_str))
        .await
        .context("DEL owner failed")?;
    let _: () = con
        .zrem(KEY_LEASES, &slot_str)
        .await
        .context("ZREM failed")?;
    let _: () = con
        .zadd(KEY_AVAILABLE, &slot_str, now)
        .await
        .context("ZADD failed")?;

    println!("✅ Slot {slot} released back to the free pool.");
    publish_asg_change(con, &slot.to_string(), "release", Some(instance_id)).await?;
    Ok(())
}

async fn cmd_status(con: &mut MultiplexedConnection) -> Result<()> {
    let free_count: u64 = con.zcard(KEY_AVAILABLE).await.context("ZCARD failed")?;
    // ZRANGE returns slots ordered by score (ascending = oldest free first)
    let free_slots_raw: Vec<(String, f64)> = con
        .zrange_withscores(KEY_AVAILABLE, 0isize, -1isize)
        .await
        .context("ZRANGE available WITHSCORES failed")?;

    let leases: Vec<(String, f64)> = con
        .zrange_withscores(KEY_LEASES, 0isize, -1isize)
        .await
        .context("ZRANGE WITHSCORES failed")?;

    let now = now_ms();

    let width = 68;
    let bar = "━".repeat(width);

    println!("{bar}");
    println!(" Slot Pool Status  (native)");
    println!("{bar}");

    println!(" Free  ({free_count:>3}):");
    if free_slots_raw.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:<8} Free for", "Slot");
        println!("  {}", "─".repeat(30));
        for (slot_str, score) in &free_slots_raw {
            let free_for = if *score == 0.0 {
                "∞  (since init)".to_string()
            } else {
                let freed_ms = *score as u64;
                format!("{:.0} s", now.saturating_sub(freed_ms) as f64 / 1000.0)
            };
            println!("  {:<8} {}", slot_str, free_for);
        }
    }
    println!();

    if leases.is_empty() {
        println!(" Leases: (none)");
    } else {
        let mut sorted = leases;
        sorted.sort_by_key(|(s, _)| s.parse::<u32>().unwrap_or(0));

        println!(" Leases ({}):", sorted.len());
        println!("  {:<8} {:<36} Expires in", "Slot", "Owner");
        println!("  {}", "─".repeat(width - 2));

        for (slot_str, expiry_score) in &sorted {
            let owner: String = con
                .get(key_owner(slot_str))
                .await
                .unwrap_or_else(|_| "(unknown)".to_string());

            let expiry_ms = *expiry_score as u64;
            let remaining = if expiry_ms <= now {
                "⚠  EXPIRED (reclaimed on next claim)".to_string()
            } else {
                format!("{:.1} s", (expiry_ms - now) as f64 / 1000.0)
            };

            println!("  {:<8} {:<36} {}", slot_str, owner, remaining);
        }
    }

    println!("{bar}");
    Ok(())
}
