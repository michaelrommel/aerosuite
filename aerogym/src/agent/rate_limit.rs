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
    /// Uses a fixed 64 KiB chunk size and computes the interval to reach the
    /// target throughput.  Returns `None` when `bps` is zero (unlimited).
    ///
    /// # Examples
    /// ```
    /// use aerogym::agent::rate_limit::RateLimiterConfig;
    ///
    /// // 10 MiB/s ≈ 10_485_760 bps
    /// let cfg = RateLimiterConfig::from_bps(10_485_760).unwrap();
    /// // One 64 KiB chunk every ~6 ms ≈ 10 MiB/s
    /// assert!(cfg.interval_ms >= 1);
    /// ```
    pub fn from_bps(bps: u64) -> Option<Self> {
        if bps == 0 {
            return None;
        }
        const CHUNK_BYTES: u32 = 64 * 1024; // 64 KiB
        // interval_ms = chunk_bytes * 1000 / bps  (round up, min 1 ms)
        let interval_ms = ((CHUNK_BYTES as u64 * 1_000) / bps).max(1);
        Some(Self {
            chunk_bytes: CHUNK_BYTES,
            interval_ms,
            mss: 0, // system default
        })
    }

    /// Approximate throughput in bytes per second implied by this config.
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

    #[test]
    fn interval_at_least_one_ms() {
        // Very high bandwidth should not produce 0 ms interval
        let cfg = RateLimiterConfig::from_bps(u64::MAX).unwrap();
        assert!(cfg.interval_ms >= 1);
    }

    #[test]
    fn reasonable_throughput_for_10_mibps() {
        let bps = 10 * 1024 * 1024; // 10 MiB/s
        let cfg = RateLimiterConfig::from_bps(bps).unwrap();
        // Should be within 20% of target
        let ratio = cfg.approx_bps() as f64 / bps as f64;
        assert!((0.8..=1.25).contains(&ratio), "ratio was {ratio:.3}");
    }

    #[test]
    fn low_bandwidth_produces_long_interval() {
        // 128 KiB/s → one 64 KiB chunk every 500 ms
        let cfg = RateLimiterConfig::from_bps(128 * 1024).unwrap();
        assert_eq!(cfg.interval_ms, 500);
    }
}
