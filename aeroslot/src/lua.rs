//! aeroslot-lua — Redis slot pool management using Lua scripts.
//!
//! Every command connects to Redis, runs an atomic Lua script, and exits.
//! Intended to be called from app startup/shutdown scripts or a heartbeat loop.
//!
//! Typical lifecycle:
//!
//!   # Once, at deploy time:
//!   slot-pool init --min 20 --max 39
//!
//!   # On each app instance start:
//!   SLOT=$(slot-pool claim --instance-id "$HOSTNAME")
//!
//!   # Heartbeat (every ~10 s, TTL default is 30 s):
//!   slot-pool renew --slot "$SLOT" --instance-id "$HOSTNAME"
//!
//!   # On graceful shutdown:
//!   slot-pool release --slot "$SLOT" --instance-id "$HOSTNAME"
//!
//!   # Crash → TTL expires → slot reclaimed by next caller of `claim`.

use aerocore::redis_pool::{build_redis_client, key_owner, now_ms, KEY_AVAILABLE, KEY_LEASES};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use std::path::PathBuf;

// ── Embedded Lua scripts ──────────────────────────────────────────────────────

/// Initialise the pool.
/// Adds every slot in [min, max] that is neither already free nor currently
/// leased.  Safe to re-run at any time — leased slots are left untouched.
///
/// ARGV[1] = min   ARGV[2] = max
/// Returns: number of slots newly added to the free set
const LUA_INIT: &str = r#"
local min = tonumber(ARGV[1])
local max = tonumber(ARGV[2])
local added = 0
for slot = min, max do
    local s = tostring(slot)
    -- Only add to free set if not currently leased
    if not redis.call('ZSCORE', 'slots:leases', s) then
        -- NX: skip if already free (preserves existing "free since" score)
        -- Score 0: treated as "free since forever", claimed before any recently-released slot
        added = added + redis.call('ZADD', 'slots:available', 'NX', 0, s)
    end
end
return added
"#;

/// Claim one free slot atomically, reclaiming expired leases on the way.
///
/// ARGV[1] = now_ms   ARGV[2] = instance_id   ARGV[3] = ttl_ms
/// Returns: slot number (string), or nil if the pool is exhausted
const LUA_CLAIM: &str = r#"
local now    = tonumber(ARGV[1])
local owner  = ARGV[2]
local ttl_ms = tonumber(ARGV[3])

-- Sweep expired leases back into the free pool (self-healing)
-- Score = now: just freed, so they rank last and cool off before reuse
local expired = redis.call('ZRANGEBYSCORE', 'slots:leases', '-inf', now)
for _, slot in ipairs(expired) do
    redis.call('ZREM', 'slots:leases', slot)
    redis.call('ZADD', 'slots:available', now, slot)
    redis.call('DEL',  'slot:owner:' .. slot)
end

-- Pop the slot that has been free the longest (lowest score = oldest free time)
-- Maximises the cooling-off window before an IP/slot is re-assigned
local result = redis.call('ZPOPMIN', 'slots:available')
if #result == 0 then
    return nil
end
local slot = result[1]

-- Register the lease with an absolute expiry timestamp as the score
redis.call('ZADD', 'slots:leases', now + ttl_ms, slot)
redis.call('SET',  'slot:owner:' .. slot, owner)
return slot
"#;

/// Renew (extend) the TTL on an existing lease.  Use as a heartbeat.
///
/// KEYS[1] = slot   ARGV[1] = instance_id   ARGV[2] = now_ms   ARGV[3] = ttl_ms
/// Returns: 1 = renewed,  0 = not owner or already expired
const LUA_RENEW: &str = r#"
local slot   = KEYS[1]
local owner  = ARGV[1]
local now    = tonumber(ARGV[2])
local ttl_ms = tonumber(ARGV[3])

if redis.call('GET', 'slot:owner:' .. slot) ~= owner then
    return 0
end

redis.call('ZADD', 'slots:leases', now + ttl_ms, slot)
return 1
"#;

/// Release a slot back to the free pool (graceful shutdown).
///
/// KEYS[1] = slot   ARGV[1] = instance_id   ARGV[2] = now_ms
/// Returns: 1 = released,  0 = not owner (slot not touched)
const LUA_RELEASE: &str = r#"
local slot  = KEYS[1]
local owner = ARGV[1]
local now   = tonumber(ARGV[2])

if redis.call('GET', 'slot:owner:' .. slot) ~= owner then
    return 0
end

-- Score = now: recently freed slots rank last, maximising cooling-off time
redis.call('DEL',  'slot:owner:' .. slot)
redis.call('ZREM', 'slots:leases', slot)
redis.call('ZADD', 'slots:available', now, slot)
return 1
"#;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "aeroslot-lua")]
#[command(about = "Manage a Redis-backed pool of numbered slots with TTL-based leases")]
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
        Command::Claim { instance_id, ttl_ms } => cmd_claim(&mut con, &instance_id, ttl_ms).await,
        Command::Renew { slot, instance_id, ttl_ms } => {
            cmd_renew(&mut con, slot, &instance_id, ttl_ms).await
        }
        Command::Release { slot, instance_id } => cmd_release(&mut con, slot, &instance_id).await,
        Command::Status => cmd_status(&mut con).await,
    }
}

// ── ASG-change notification ─────────────────────────────────────────────────

const ASG_CHANGE_CHANNEL: &str = "asg-change";

/// Publish a slot event on the `asg-change` Redis channel so that load-balancer
/// components (e.g. keepalived weight scripts) can react in real time.
///
/// Only called on `claim` and `release` — not on `renew`, `init`, or `status`.
async fn publish_asg_change(
    con: &mut MultiplexedConnection,
    slot: &str,
    action: &str,
) -> Result<()> {
    let slot_num: u64 = slot.parse().unwrap_or(0);
    let msg = serde_json::json!({
        "slot":   slot_num,
        "action": action,
    })
    .to_string();

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

    let added: i64 = redis::Script::new(LUA_INIT)
        .arg(min)
        .arg(max)
        .invoke_async(con)
        .await
        .context("INIT script failed")?;

    let total = max - min + 1;
    println!("✅ Pool ready: slots {min}–{max} ({total} total, {added} newly added to free set).");
    Ok(())
}

async fn cmd_claim(
    con: &mut MultiplexedConnection,
    instance_id: &str,
    ttl_ms: u64,
) -> Result<()> {
    let slot: Option<String> = redis::Script::new(LUA_CLAIM)
        .arg(now_ms())
        .arg(instance_id)
        .arg(ttl_ms)
        .invoke_async(con)
        .await
        .context("CLAIM script failed")?;

    match slot {
        Some(n) => {
            println!(
                "✅ Claimed slot {n} for '{instance_id}' (TTL {:.0} s).",
                ttl_ms as f64 / 1000.0
            );
            // Print just the slot number on its own line so callers can capture it:
            //   SLOT=$(slot-pool claim --instance-id "$HOSTNAME" | tail -1)
            println!("{n}");
            publish_asg_change(con, &n, "claim").await?;
        }
        None => {
            eprintln!("❌ No free slots available.");
            std::process::exit(1);
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
    let ok: i64 = redis::Script::new(LUA_RENEW)
        .key(slot.to_string())
        .arg(instance_id)
        .arg(now_ms())
        .arg(ttl_ms)
        .invoke_async(con)
        .await
        .context("RENEW script failed")?;

    if ok == 1 {
        println!(
            "✅ Lease on slot {slot} renewed for '{instance_id}' (+{:.0} s).",
            ttl_ms as f64 / 1000.0
        );
    } else {
        eprintln!("❌ Slot {slot} is not owned by '{instance_id}' or has already expired.");
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_release(
    con: &mut MultiplexedConnection,
    slot: u32,
    instance_id: &str,
) -> Result<()> {
    let ok: i64 = redis::Script::new(LUA_RELEASE)
        .key(slot.to_string())
        .arg(instance_id)
        .arg(now_ms())
        .invoke_async(con)
        .await
        .context("RELEASE script failed")?;

    if ok == 1 {
        println!("✅ Slot {slot} released back to the free pool.");
        publish_asg_change(con, &slot.to_string(), "release").await?;
    } else {
        eprintln!("❌ Slot {slot} is not owned by '{instance_id}'.");
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_status(con: &mut MultiplexedConnection) -> Result<()> {
    // ── Free slots ────────────────────────────────────────────────────────────
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

    // ── Active leases: flat [member, score, member, score …] ─────────────────
    // Use a raw command so we don't depend on a specific AsyncCommands method
    // signature for zrange_withscores across redis-rs versions.
    let leases: Vec<(String, f64)> = redis::cmd("ZRANGE")
        .arg(KEY_LEASES)
        .arg(0i64)
        .arg(-1i64)
        .arg("WITHSCORES")
        .query_async(con)
        .await
        .context("ZRANGE WITHSCORES failed")?;

    let now = now_ms();

    // ── Render ────────────────────────────────────────────────────────────────
    let width = 68;
    let bar = "━".repeat(width);

    println!("{bar}");
    println!(" Slot Pool Status");
    println!("{bar}");

    println!(" Free  ({free_count:>3}):");
    if free_slots_raw.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:<8} {}", "Slot", "Free for");
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
        // Sort by slot number for stable output
        let mut sorted = leases;
        sorted.sort_by_key(|(s, _)| s.parse::<u32>().unwrap_or(0));

        println!(" Leases ({}):", sorted.len());
        println!("  {:<8} {:<36} {}", "Slot", "Owner", "Expires in");
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
