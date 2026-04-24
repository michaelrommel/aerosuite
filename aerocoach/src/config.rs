//! Configuration for aerocoach, parsed from environment variables.
//!
//! All variables are optional and fall back to documented defaults so the
//! binary can be started without any environment setup during development.
//!
//! | Variable                | Default          | Description                                              |
//! |-------------------------|------------------|----------------------------------------------------------|
//! | `AEROCOACH_GRPC_PORT`   | `50051`          | gRPC listen port                                         |
//! | `AEROCOACH_HTTP_PORT`   | `8080`           | HTTP + WebSocket listen port                             |
//! | `AEROCOACH_PLAN_DIR`    | *(none)*         | Directory of JSON plan files; last alphabetically loaded |
//! | `AEROCOACH_PLAN_FILE`   | *(none)*         | Path to a single JSON plan file (ignored if DIR is set)  |
//! | `AEROCOACH_RECORD_DIR`  | `/data/records`  | Directory for NDJSON result files                        |

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Runtime configuration for one aerocoach process.
#[derive(Debug, Clone)]
pub struct Config {
    /// gRPC listen port.
    pub grpc_port: u16,

    /// HTTP + WebSocket listen port.
    pub http_port: u16,

    /// Optional directory of JSON load-plan files.
    /// When set, the file with the last name in alphabetical order is loaded
    /// at startup and the UI may switch plans via `POST /plan/select`.
    /// Takes precedence over `plan_file` when both are provided.
    pub plan_dir: Option<PathBuf>,

    /// Optional path to a single JSON load-plan file.
    /// Ignored when `plan_dir` is set.
    pub plan_file: Option<PathBuf>,

    /// Directory where NDJSON result files are written after each test run.
    pub record_dir: PathBuf,
}

impl Config {
    /// Parse configuration from the real process environment.
    ///
    /// # Errors
    /// Returns an error if any numeric environment variable cannot be parsed.
    pub fn from_env() -> Result<Self> {
        Self::from_source(|name| std::env::var(name).ok())
    }

    /// Parse configuration from an arbitrary key-value source.
    ///
    /// Accepts any callable that maps a variable name to an optional string
    /// value.  This makes the config fully testable without touching the
    /// process environment.
    pub(crate) fn from_source<F>(get: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        Ok(Self {
            grpc_port: parse_opt(get("AEROCOACH_GRPC_PORT"), 50051)
                .context("AEROCOACH_GRPC_PORT must be a valid port number (0–65535)")?,
            http_port: parse_opt(get("AEROCOACH_HTTP_PORT"), 8080)
                .context("AEROCOACH_HTTP_PORT must be a valid port number (0–65535)")?,
            plan_dir:  get("AEROCOACH_PLAN_DIR").map(PathBuf::from),
            plan_file: get("AEROCOACH_PLAN_FILE").map(PathBuf::from),
            record_dir: PathBuf::from(
                get("AEROCOACH_RECORD_DIR").unwrap_or_else(|| "/data/records".into()),
            ),
        })
    }
}

/// Parse an optional string value as `T`, returning `default` when absent.
/// Returns an error if the value is present but cannot be parsed.
fn parse_opt<T>(value: Option<String>, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match value {
        Some(s) => s
            .parse::<T>()
            .with_context(|| format!("cannot parse {:?}", s)),
        None => Ok(default),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn src<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults_when_nothing_set() {
        let cfg = Config::from_source(|_| None).unwrap();
        assert_eq!(cfg.grpc_port, 50051);
        assert_eq!(cfg.http_port, 8080);
        assert!(cfg.plan_dir.is_none());
        assert!(cfg.plan_file.is_none());
        assert_eq!(cfg.record_dir, PathBuf::from("/data/records"));
    }

    #[test]
    fn custom_ports() {
        let cfg = Config::from_source(src(&[
            ("AEROCOACH_GRPC_PORT", "9090"),
            ("AEROCOACH_HTTP_PORT", "3000"),
        ]))
        .unwrap();
        assert_eq!(cfg.grpc_port, 9090);
        assert_eq!(cfg.http_port, 3000);
    }

    #[test]
    fn invalid_port_is_error() {
        let result = Config::from_source(src(&[("AEROCOACH_GRPC_PORT", "not-a-number")]));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("AEROCOACH_GRPC_PORT"), "error should name the variable: {msg}");
    }

    #[test]
    fn plan_file_captured() {
        let cfg = Config::from_source(src(&[
            ("AEROCOACH_PLAN_FILE", "/etc/aerocoach/plan.json"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.plan_file,
            Some(PathBuf::from("/etc/aerocoach/plan.json"))
        );
        assert!(cfg.plan_dir.is_none());
    }

    #[test]
    fn plan_dir_captured() {
        let cfg = Config::from_source(src(&[
            ("AEROCOACH_PLAN_DIR", "/etc/aerocoach/plans"),
        ]))
        .unwrap();
        assert_eq!(cfg.plan_dir, Some(PathBuf::from("/etc/aerocoach/plans")));
        assert!(cfg.plan_file.is_none());
    }

    #[test]
    fn custom_record_dir() {
        let cfg = Config::from_source(src(&[("AEROCOACH_RECORD_DIR", "/tmp/results")])).unwrap();
        assert_eq!(cfg.record_dir, PathBuf::from("/tmp/results"));
    }
}
