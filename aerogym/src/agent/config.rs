//! Agent configuration parsed from environment variables.
//!
//! | Variable                    | Required | Default          | Description                                     |
//! |-----------------------------|----------|------------------|-------------------------------------------------|
//! | `AEROGYM_AGENT_ID`          | **Yes**  | —                | Identifier, e.g. `a00`–`a99`                   |
//! | `AEROCOACH_URL`             | **Yes**  | —                | gRPC endpoint `http://host:port`                |
//! | `AEROSTRESS_TARGET`         | **Yes**  | —                | FTP server `host:port`                          |
//! | `AEROSTRESS_USER`           | No       | `test`           | FTP login username                              |
//! | `AEROSTRESS_PASS`           | No       | `secret`         | FTP login password                              |
//! | `AEROGYM_WORK_DIR`          | No       | `/tmp/aerogym`   | Directory for pre-generated files               |
//! | `AEROGYM_PRIVATE_IP`        | No       | `""`             | Override for private IP metadata                |
//! | `AEROGYM_INSTANCE_ID`       | No       | `""`             | Override for instance-id metadata               |
//! | `AEROGYM_REFILL_THRESHOLD`  | No       | `0.80`           | Replace completed transfers before this slice % |

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

/// All runtime parameters for the aerogym agent.
#[derive(Debug, Clone)]
pub struct Config {
    /// Agent identifier, e.g. `"a03"`.  Must be unique across the fleet.
    pub agent_id: String,

    /// aerocoach gRPC endpoint, e.g. `"http://10.0.1.5:50051"`.
    pub aerocoach_url: String,

    /// FTP server address in `host:port` format, e.g. `"10.0.2.10:21"`.
    pub ftp_target: String,

    /// FTP login username.
    pub ftp_user: String,

    /// FTP login password.
    pub ftp_pass: String,

    /// Working directory for bucket files generated at startup.
    pub work_dir: PathBuf,

    /// Private IP reported to aerocoach at registration.
    /// Populated from ECS task metadata when available; overridable via env var.
    pub private_ip: String,

    /// Instance / task ID reported to aerocoach at registration.
    pub instance_id: String,

    /// Fraction of a time slice (0.0–1.0] within which a completed transfer
    /// is automatically replaced to keep the connection count at the plan
    /// target.  At or beyond this fraction the slot is left empty so the
    /// slice winds down naturally.
    ///
    /// Default: `0.80` (replace completions during the first 80 % of a slice).
    pub refill_threshold: f64,
}

impl Config {
    /// Parse configuration from the real process environment.
    ///
    /// # Errors
    /// Returns an error if any required variable is absent or cannot be parsed.
    pub fn from_env() -> Result<Self> {
        Self::from_source(|name| std::env::var(name).ok())
    }

    /// Parse configuration from an arbitrary key-value source (enables
    /// env-free unit testing).
    pub(crate) fn from_source<F>(get: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let agent_id = get("AEROGYM_AGENT_ID")
            .context("AEROGYM_AGENT_ID is required")?;

        if agent_id.trim().is_empty() {
            bail!("AEROGYM_AGENT_ID must not be empty");
        }

        let aerocoach_url = get("AEROCOACH_URL")
            .context("AEROCOACH_URL is required (e.g. http://10.0.1.5:50051)")?;

        let ftp_target = get("AEROSTRESS_TARGET")
            .context("AEROSTRESS_TARGET is required (e.g. 10.0.2.10:21)")?;

        let refill_threshold = match get("AEROGYM_REFILL_THRESHOLD") {
            Some(s) => {
                let v: f64 = s.parse().context("AEROGYM_REFILL_THRESHOLD must be a number")?;
                if !(0.0..=1.0).contains(&v) {
                    bail!("AEROGYM_REFILL_THRESHOLD must be in the range 0.0–1.0, got {v}");
                }
                v
            }
            None => 0.80,
        };

        Ok(Self {
            agent_id,
            aerocoach_url,
            ftp_target,
            ftp_user: get("AEROSTRESS_USER").unwrap_or_else(|| "test".into()),
            ftp_pass: get("AEROSTRESS_PASS").unwrap_or_else(|| "secret".into()),
            work_dir: PathBuf::from(
                get("AEROGYM_WORK_DIR").unwrap_or_else(|| "/tmp/aerogym".into()),
            ),
            private_ip:  get("AEROGYM_PRIVATE_IP").unwrap_or_default(),
            instance_id: get("AEROGYM_INSTANCE_ID").unwrap_or_default(),
            refill_threshold,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    fn required() -> Vec<(&'static str, &'static str)> {
        vec![
            ("AEROGYM_AGENT_ID",  "a03"),
            ("AEROCOACH_URL",     "http://10.0.1.5:50051"),
            ("AEROSTRESS_TARGET", "10.0.2.10:21"),
        ]
    }

    #[test]
    fn parses_required_fields() {
        let cfg = Config::from_source(src(&required())).unwrap();
        assert_eq!(cfg.agent_id,       "a03");
        assert_eq!(cfg.aerocoach_url,  "http://10.0.1.5:50051");
        assert_eq!(cfg.ftp_target,     "10.0.2.10:21");
        assert_eq!(cfg.ftp_user,       "test");
        assert_eq!(cfg.ftp_pass,       "secret");
        assert_eq!(cfg.work_dir,       PathBuf::from("/tmp/aerogym"));
    }

    #[test]
    fn missing_agent_id_is_error() {
        let pairs = vec![
            ("AEROCOACH_URL",     "http://localhost:50051"),
            ("AEROSTRESS_TARGET", "localhost:21"),
        ];
        assert!(Config::from_source(src(&pairs)).is_err());
    }

    #[test]
    fn missing_coach_url_is_error() {
        let pairs = vec![
            ("AEROGYM_AGENT_ID",  "a00"),
            ("AEROSTRESS_TARGET", "localhost:21"),
        ];
        assert!(Config::from_source(src(&pairs)).is_err());
    }

    #[test]
    fn custom_work_dir() {
        let mut pairs = required();
        pairs.push(("AEROGYM_WORK_DIR", "/data/agent"));
        let cfg = Config::from_source(src(&pairs)).unwrap();
        assert_eq!(cfg.work_dir, PathBuf::from("/data/agent"));
    }

    #[test]
    fn metadata_overrides() {
        let mut pairs = required();
        pairs.push(("AEROGYM_PRIVATE_IP",  "10.0.1.99"));
        pairs.push(("AEROGYM_INSTANCE_ID", "arn:aws:ecs:us-east-1:123456789012:task/abc"));
        let cfg = Config::from_source(src(&pairs)).unwrap();
        assert_eq!(cfg.private_ip,  "10.0.1.99");
        assert_eq!(cfg.instance_id, "arn:aws:ecs:us-east-1:123456789012:task/abc");
    }

    #[test]
    fn refill_threshold_default() {
        let cfg = Config::from_source(src(&required())).unwrap();
        assert!((cfg.refill_threshold - 0.80).abs() < 1e-9);
    }

    #[test]
    fn refill_threshold_custom() {
        let mut pairs = required();
        pairs.push(("AEROGYM_REFILL_THRESHOLD", "0.6"));
        let cfg = Config::from_source(src(&pairs)).unwrap();
        assert!((cfg.refill_threshold - 0.6).abs() < 1e-9);
    }

    #[test]
    fn refill_threshold_out_of_range() {
        let mut pairs = required();
        pairs.push(("AEROGYM_REFILL_THRESHOLD", "1.5"));
        assert!(Config::from_source(src(&pairs)).is_err());
    }
}
