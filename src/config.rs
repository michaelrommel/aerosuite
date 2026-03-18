use anyhow::{Context, Result};
use std::env;

/// FTP load test configuration parsed from environment variables.
#[derive(Debug)]
pub struct Config {
    pub target: String,
    pub batches: i32,
    pub tasks: i32,
    pub delay: u64,
    pub limiter: bool,
    pub file_size_mb: u32,
    pub chunk_kb: u32,
    pub interval: u64,
    pub mss: u32,
}

/// Builder for constructing Config with validation.
#[derive(Debug)]
pub struct ConfigBuilder {
    target: Option<String>,
    batches: Option<i32>,
    tasks: Option<i32>,
    delay: Option<u64>,
    limiter: Option<bool>,
    file_size_mb: Option<u32>,
    chunk_kb: Option<u32>,
    interval: Option<u64>,
    mss: Option<u32>,
}

impl ConfigBuilder {
    /// Creates a new ConfigBuilder with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the FTP server target address.
    pub fn target(mut self, value: impl Into<String>) -> Self {
        self.target = Some(value.into());
        self
    }

    /// Sets the number of batches to send.
    pub fn batches(mut self, value: i32) -> Self {
        self.batches = Some(value);
        self
    }

    /// Sets the number of parallel tasks per batch.
    pub fn tasks(mut self, value: i32) -> Self {
        self.tasks = Some(value);
        self
    }

    /// Sets the delay in seconds between batches.
    pub fn delay(mut self, value: u64) -> Self {
        self.delay = Some(value);
        self
    }

    /// Enables or disables rate limiting.
    pub fn limiter(mut self, value: bool) -> Self {
        self.limiter = Some(value);
        self
    }

    /// Sets the file size in megabytes for test files.
    pub fn file_size_mb(mut self, value: u32) -> Self {
        self.file_size_mb = Some(value);
        self
    }

    /// Sets the chunk size for streaming in KB.
    pub fn chunk_kb(mut self, value: u32) -> Self {
        self.chunk_kb = Some(value);
        self
    }

    /// Sets the rate limit interval in milliseconds.
    pub fn interval(mut self, value: u64) -> Self {
        self.interval = Some(value);
        self
    }

    /// Sets the TCP MSS (Maximum Segment Size).
    pub fn mss(mut self, value: u32) -> Self {
        self.mss = Some(value);
        self
    }

    /// Builds and validates the Config.
    ///
    /// # Errors
    /// Returns an error if any required field is missing.
    pub fn build(self) -> Result<Config> {
        Ok(Config {
            target: self.target.context("target is required")?,
            batches: self.batches.context("batches is required")?,
            tasks: self.tasks.context("tasks is required")?,
            delay: self.delay.context("delay is required")?,
            limiter: self.limiter.context("limiter is required")?,
            file_size_mb: self.file_size_mb.context("file_size_mb is required")?,
            chunk_kb: self.chunk_kb.context("chunk_kb is required")?,
            interval: self.interval.context("interval is required")?,
            mss: self.mss.context("mss is required")?,
        })
    }
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self {
            target: Some("127.0.0.1".to_string()),
            batches: Some(8),
            tasks: Some(20),
            delay: Some(10),
            limiter: Some(false),
            file_size_mb: Some(10),
            chunk_kb: Some(4),
            interval: Some(0),
            mss: Some(1460),
        }
    }
}

/// Parses configuration from environment variables using the builder pattern.
///
/// # Errors
/// Returns an error if any required environment variable cannot be parsed as its expected type.
pub fn parse_config() -> Result<Config> {
    ConfigBuilder::new()
        .target(env::var("AEROSTRESS_TARGET").unwrap_or_else(|_| "127.0.0.1".to_string()))
        .batches(
            env::var("AEROSTRESS_BATCHES")
                .unwrap_or_else(|_| "8".to_string())
                .parse()
                .context("AEROSTRESS_BATCHES must be a number")?,
        )
        .tasks(
            env::var("AEROSTRESS_TASKS")
                .unwrap_or_else(|_| "20".to_string())
                .parse()
                .context("AEROSTRESS_TASKS must be a number")?,
        )
        .delay(
            env::var("AEROSTRESS_DELAY")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("AEROSTRESS_DELAY must be a number")?,
        )
        .limiter(
            env::var("AEROSTRESS_LIMITER")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .context("AEROSTRESS_LIMITER must be a boolean")?,
        )
        .file_size_mb(
            env::var("AEROSTRESS_SIZE")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("AEROSTRESS_SIZE must be a number")?,
        )
        .chunk_kb(
            env::var("AEROSTRESS_CHUNK")
                .unwrap_or_else(|_| "4".to_string())
                .parse()
                .context("AEROSTRESS_CHUNK must be a number")?,
        )
        .interval(
            env::var("AEROSTRESS_INTERVAL")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .context("AEROSTRESS_INTERVAL must be a number")?,
        )
        .mss(
            env::var("AEROSTRESS_MSS")
                .unwrap_or_else(|_| "1460".to_string())
                .parse()
                .context("AEROSTRESS_MSS must be a number")?,
        )
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_chaining() {
        let config = ConfigBuilder::new()
            .target("ftp.example.com".to_string())
            .batches(10)
            .tasks(50)
            .delay(5)
            .limiter(true)
            .file_size_mb(12)
            .chunk_kb(8)
            .interval(100)
            .mss(1200)
            .build()
            .unwrap();

        assert_eq!(config.target, "ftp.example.com");
        assert_eq!(config.batches, 10);
        assert_eq!(config.tasks, 50);
        assert_eq!(config.delay, 5);
        assert!(config.limiter);
        assert_eq!(config.file_size_mb, 12);
        assert_eq!(config.chunk_kb, 8);
        assert_eq!(config.interval, 100);
        assert_eq!(config.mss, 1200);
    }

    #[test]
    fn test_builder_defaults() {
        let config = ConfigBuilder::new().build().unwrap();

        assert_eq!(config.target, "127.0.0.1");
        assert_eq!(config.batches, 8);
        assert_eq!(config.tasks, 20);
        assert_eq!(config.delay, 10);
        assert!(!config.limiter);
        assert_eq!(config.file_size_mb, 10);
        assert_eq!(config.chunk_kb, 4);
        assert_eq!(config.interval, 0);
        assert_eq!(config.mss, 1460);
    }

    #[test]
    fn test_missing_required_field() {
        // Override default with None to test validation
        let result = ConfigBuilder {
            target: Some("ftp.example.com".to_string()),
            batches: Some(10),
            tasks: Some(20),
            delay: Some(5),
            limiter: Some(false),
            file_size_mb: Some(10),
            chunk_kb: Some(4),
            interval: None,
            mss: Some(1460),
        }
        .build();

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("is required"));
    }
}
