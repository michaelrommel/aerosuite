//! Per-agent bandwidth quota: converts a bytes-per-second ceiling into the
//! chunk-size + interval parameters consumed by the `governor` rate limiter
//! in the FTP transfer layer.

/// Rate limiter parameters for one FTP transfer stream.
#[derive(Debug, Copy, Clone)]
pub struct RateLimiterConfig {
    /// Chunk size in bytes streamed per governor tick.
    pub chunk_bytes: u32,
    /// Interval between governor ticks in milliseconds.
    pub interval_ms: u64,
    /// TCP Maximum Segment Size (0 = system default).
    pub mss: u32,
}

impl RateLimiterConfig {
    /// Derive a [`RateLimiterConfig`] from a bandwidth ceiling in bytes/sec.
    ///
    /// The algorithm keeps `chunk_bytes` proportional to the target rate so
    /// that even small files are broken into many chunks and are properly
    /// throttled.  With a fixed large chunk (the previous 64 KiB design) the
    /// governor grants the first token immediately; any file smaller than one
    /// chunk transfers at full line speed regardless of the configured rate.
    ///
    /// ## Normal path (rate ≥ ~10 KB/s)
    ///
    /// A fixed **50 ms** tick is used and `chunk_bytes` is derived:
    ///
    /// ```text
    /// chunk_bytes = bps × 50 ms / 1000   (clamped to [512 B, 4 MiB])
    /// interval_ms = 50
    /// ```
    ///
    /// At 10 592 B/s (e.g. 1 MiB/s ÷ 3 agents ÷ 33 connections) this gives
    /// 529-byte chunks every 50 ms — a 36 KB xs-file becomes ~70 chunks and
    /// the "first-token-free" burst is only ~1.4 % of the transfer time.
    ///
    /// ## Low-rate path (rate < 10 KB/s)
    ///
    /// When `bps × 50 ms < 512 B` the chunk is fixed at **512 B** and the
    /// interval is extended to match the target:
    ///
    /// ```text
    /// chunk_bytes = 512
    /// interval_ms = 512 × 1000 / bps   (≥ 1 ms)
    /// ```
    ///
    /// This keeps `approx_bps` accurate even for very low per-connection
    /// rates that arise with many concurrent connections on a modest total
    /// bandwidth budget (e.g. 800 connections at 1 MiB/s ÷ 3 agents).
    ///
    /// Returns `None` when `bps` is zero (unlimited).
    pub fn from_bps(bps: u64) -> Option<Self> {
        if bps == 0 {
            return None;
        }
        const TICK_MS:   u64 = 50;
        const MIN_CHUNK: u64 = 512;
        const MAX_CHUNK: u64 = 4 * 1024 * 1024;

        let ideal_chunk = bps.saturating_mul(TICK_MS).saturating_div(1_000);

        let (chunk_bytes, interval_ms) = if ideal_chunk >= MIN_CHUNK {
            // Normal path: 50 ms tick, chunk scales with rate.
            (ideal_chunk.min(MAX_CHUNK) as u32, TICK_MS)
        } else {
            // Low-rate path: fix chunk at MIN_CHUNK, extend the interval.
            let interval = MIN_CHUNK
                .saturating_mul(1_000)
                .saturating_div(bps)
                .max(1);
            (MIN_CHUNK as u32, interval)
        };

        Some(Self { chunk_bytes, interval_ms, mss: 0 })
    }

    /// Approximate throughput in bytes per second implied by this config.
    #[allow(dead_code)]
    pub fn approx_bps(&self) -> u64 {
        if self.interval_ms == 0 {
            return u64::MAX;
        }
        (self.chunk_bytes as u64 * 1_000) / self.interval_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_bps_returns_none() {
        assert!(RateLimiterConfig::from_bps(0).is_none());
    }

    /// Normal path: 50 ms fixed tick, chunk proportional to rate.
    #[test]
    fn normal_path_uses_50ms_tick() {
        for bps in [10_592_u64, 50_000, 500_000, 5_000_000, 10 * 1024 * 1024] {
            let cfg = RateLimiterConfig::from_bps(bps).unwrap();
            assert_eq!(cfg.interval_ms, 50, "expected 50 ms tick for bps={bps}");
        }
    }

    /// Low-rate path: interval extends beyond 50 ms to keep chunk at 512 B.
    #[test]
    fn low_rate_path_extends_interval() {
        // 1 500 B/s → chunk=512, interval=341 ms
        let cfg = RateLimiterConfig::from_bps(1_500).unwrap();
        assert_eq!(cfg.chunk_bytes, 512);
        assert!(cfg.interval_ms > 50, "interval should exceed 50 ms at low rate");
    }

    /// The key regression: xs files (~36 KB) must be broken into many small
    /// chunks so they are properly throttled, not sent in one burst.
    #[test]
    fn small_file_gets_many_chunks() {
        // 1 MiB/s ÷ 3 agents ÷ 33 connections ≈ 10 591 B/s
        let cfg = RateLimiterConfig::from_bps(10_591).unwrap();
        let xs_file_bytes: u64 = 36_948;
        let chunks = xs_file_bytes.div_ceil(cfg.chunk_bytes as u64);
        // Must be well above 1 so the first-token-free burst is negligible.
        assert!(chunks > 50, "got only {chunks} chunks — small files will not be throttled");
    }

    /// approx_bps must be within 2 % of the target across realistic rates.
    #[test]
    fn approx_bps_accurate_across_rate_range() {
        for bps in [1_500_u64, 5_000, 10_592, 50_000, 500_000, 5_000_000, 10 * 1024 * 1024] {
            let cfg = RateLimiterConfig::from_bps(bps).unwrap();
            let ratio = cfg.approx_bps() as f64 / bps as f64;
            assert!(
                (0.98..=1.02).contains(&ratio),
                "bps={bps}: approx={} ratio={ratio:.4}",
                cfg.approx_bps()
            );
        }
    }

    #[test]
    fn chunk_clamped_at_upper_bound_for_extreme_rates() {
        let cfg = RateLimiterConfig::from_bps(u64::MAX).unwrap();
        assert_eq!(cfg.chunk_bytes, 4 * 1024 * 1024);
    }
}
