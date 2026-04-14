//! slot-pool-native — Redis slot pool management using plain Redis commands.
//!
//! Functionally identical to slot-pool-lua but all logic runs in Rust.
//! No Lua scripts.  Intended for side-by-side comparison.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use std::time::{SystemTime, UNIX_EPOCH};

// ── Redis key constants ───────────────────────────────────────────────────────

const KEY_AVAILABLE: &str = "slots:available";
const KEY_LEASES: &str = "slots:leases";

fn key_owner(slot: &str) -> String {
    format!("slot:owner:{slot}")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis() as u64
}

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "slot-pool-native")]
#[command(about = "Manage a Redis-backed pool of numbered slots (native commands, no Lua)")]
#[command(long_about = None)]
struct Args {
    /// Redis connection URL (env: REDIS_URL)
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    redis_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialise the slot pool (idempotent; skips leased or already-free slots)
    Init {
        /// First slot number, inclusive
        #[arg(long, default_value_t = 20)]
        min: u32,

        /// Last slot number, inclusive
        #[arg(long, default_value_t = 39)]
        max: u32,
    },

    /// Claim one free slot atomically (also sweeps expired leases)
    Claim {
        /// Unique identifier for this app instance (e.g. hostname or UUID)
        #[arg(long)]
        instance_id: String,

        /// Lease duration in milliseconds  [default: 30 000]
        #[arg(long, default_value_t = 30_000)]
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

        /// New TTL in milliseconds  [default: 30 000]
        #[arg(long, default_value_t = 30_000)]
        ttl_ms: u64,
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

    let client = redis::Client::open(args.redis_url.as_str()).context("Invalid Redis URL")?;

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
        Command::Release { slot, instance_id } => cmd_release(&mut con, slot, &instance_id).await,
        Command::Status => cmd_status(&mut con).await,
    }
}

// ── Subcommand implementations ────────────────────────────────────────────────

async fn cmd_init(con: &mut MultiplexedConnection, min: u32, max: u32) -> Result<()> {
    if min > max {
        bail!("--min ({min}) must be ≤ --max ({max})");
    }

    let mut added = 0u32;
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
            added += redis::cmd("ZADD")
                .arg(KEY_AVAILABLE)
                .arg("NX")
                .arg(0u64)
                .arg(&s)
                .query_async::<u32>(con)
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
    let expired: Vec<String> = redis::cmd("ZRANGEBYSCORE")
        .arg(KEY_LEASES)
        .arg("-inf")
        .arg(now)
        .query_async(con)
        .await
        .context("ZRANGEBYSCORE failed")?;

    for slot in &expired {
        let _: () = con.zrem(KEY_LEASES, slot).await.context("ZREM failed")?;
        let _: () = con
            .zadd(KEY_AVAILABLE, now, slot)
            .await
            .context("ZADD failed")?;
        let _: () = con.del(key_owner(slot)).await.context("DEL failed")?;
    }

    // Pop the slot free the longest (lowest score = oldest free time).
    // Maximises the cooling-off window before an IP/slot is re-assigned.
    let result: Vec<(String, f64)> = redis::cmd("ZPOPMIN")
        .arg(KEY_AVAILABLE)
        .arg(1u32)
        .query_async(con)
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
                .zadd(KEY_LEASES, expiry, &s)
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
                .zadd(KEY_LEASES, expiry, &slot_str)
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
        .zadd(KEY_AVAILABLE, now, &slot_str)
        .await
        .context("ZADD failed")?;

    println!("✅ Slot {slot} released back to the free pool.");
    Ok(())
}

async fn cmd_status(con: &mut MultiplexedConnection) -> Result<()> {
    let free_count: u64 = con.zcard(KEY_AVAILABLE).await.context("ZCARD failed")?;
    // ZRANGE returns slots ordered by score (ascending = oldest free first)
    let free_slots_raw: Vec<(String, f64)> = redis::cmd("ZRANGE")
        .arg(KEY_AVAILABLE)
        .arg(0i64)
        .arg(-1i64)
        .arg("WITHSCORES")
        .query_async(con)
        .await
        .context("ZRANGE available WITHSCORES failed")?;

    let leases: Vec<(String, f64)> = redis::cmd("ZRANGE")
        .arg(KEY_LEASES)
        .arg(0i64)
        .arg(-1i64)
        .arg("WITHSCORES")
        .query_async(con)
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
