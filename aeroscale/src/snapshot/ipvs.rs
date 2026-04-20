//! Read IPVS real-server state directly from `/proc/net/ip_vs`.
//!
//! Avoids the `ipvsadm` binary dependency entirely — the kernel proc file is
//! always available when the `ip_vs` module is loaded and is faster to read.
//!
//! ## `/proc/net/ip_vs` format
//!
//! ```text
//! IP Virtual Server version 1.2.1 (size=4096)
//! Prot LocalAddress:Port Scheduler Flags
//!   -> RemoteAddress:Port Forward Weight ActiveConn InActConn
//! TCP  AC101D64:0015 wlc  persistent 30000 FFFFFFFF
//!   -> AC102014:0015      Masq    1      5          2
//!   -> AC102015:0015      Masq    1      0          0
//!   -> AC102016:0015      Masq    0      0          0
//! ```
//!
//! Addresses are 8 hex digits (32-bit big-endian / network-order IPv4),
//! ports are 4 hex digits.  After `split_whitespace()` on a real-server line:
//!
//! ```text
//! [0] "->"
//! [1] "AC102014:0015"   ← HEX_IP:HEX_PORT
//! [2] "Masq"            ← Forward method
//! [3] "1"               ← Weight
//! [4] "5"               ← ActiveConn
//! [5] "2"               ← InActConn
//! ```
//!
//! This module is resilient: if the file is absent (IPVS not loaded, or
//! running on a dev machine) it logs a warning and returns an empty list so
//! the rest of the snapshot continues normally.

use std::net::Ipv4Addr;
use anyhow::Result;
use tracing::{debug, warn};

use super::IpvsBackend;

const PROC_FILE: &str = "/proc/net/ip_vs";

/// Read `/proc/net/ip_vs` and return one entry per real server.
/// Returns an empty `Vec` (with a warning) if the file is unavailable.
pub async fn read_all() -> Result<Vec<IpvsBackend>> {
    let content = match tokio::fs::read_to_string(PROC_FILE).await {
        Ok(c)  => c,
        Err(e) => {
            warn!(
                "{PROC_FILE} not readable: {e} \
                 — is the ip_vs kernel module loaded? IPVS data will be missing."
            );
            return Ok(Vec::new());
        }
    };

    let backends = parse(&content);
    debug!("{} IPVS real server(s) found", backends.len());
    Ok(backends)
}

// ── Parser ────────────────────────────────────────────────────────────────────

fn parse(content: &str) -> Vec<IpvsBackend> {
    let mut result = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("->") {
            continue;
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        // Minimum: -> ADDR:PORT FORWARD WEIGHT ACTIVE INACTIVE
        if parts.len() < 6 {
            warn!("ip_vs: unexpected line format: {trimmed}");
            continue;
        }

        // parts[1] is "HEX_IP:HEX_PORT" — split on ':'
        let addr_port = parts[1];
        let (hex_ip, _hex_port) = match addr_port.split_once(':') {
            Some(pair) => pair,
            None => {
                warn!("ip_vs: cannot split address:port in: {addr_port}");
                continue;
            }
        };

        let ip = match parse_hex_ip(hex_ip) {
            Some(ip) => ip,
            None => {
                // The column-header line ("-> RemoteAddress:Port ...") also
                // starts with "->" — silently skip it and any other non-hex entry.
                debug!("ip_vs: skipping non-hex address (likely header): {hex_ip}");
                continue;
            }
        };

        let active:   u32 = parts[4].parse().unwrap_or(0);
        let inactive: u32 = parts[5].parse().unwrap_or(0);

        result.push(IpvsBackend { ip, active_connections: active, inactive_connections: inactive });
    }

    result.sort_by_key(|b| b.ip);
    result
}

/// Parse an 8-character big-endian hex string into an `Ipv4Addr`.
///
/// Example: `"AC102014"` → `172.16.32.20`
fn parse_hex_ip(hex: &str) -> Option<Ipv4Addr> {
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(Ipv4Addr::from(n))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Real /proc/net/ip_vs sample from the production load balancer.
    const SAMPLE: &str = "\
IP Virtual Server version 1.2.1 (size=4096)
Prot LocalAddress:Port Scheduler Flags
  -> RemoteAddress:Port Forward Weight ActiveConn InActConn
TCP  AC101D64:0015 wlc  persistent 30000 FFFFFFFF
  -> AC102027:0015      Masq    0      0          0
  -> AC102015:0015      Masq    1      3          1
  -> AC102014:0015      Masq    1      5          2
";

    #[test]
    fn parse_hex_ips() {
        assert_eq!(parse_hex_ip("AC102014"), Some(Ipv4Addr::new(172, 16, 32, 20)));
        assert_eq!(parse_hex_ip("AC102015"), Some(Ipv4Addr::new(172, 16, 32, 21)));
        assert_eq!(parse_hex_ip("AC102027"), Some(Ipv4Addr::new(172, 16, 32, 39)));
        assert_eq!(parse_hex_ip("AC101D64"), Some(Ipv4Addr::new(172, 16, 29, 100)));
        assert_eq!(parse_hex_ip("ZZZZZZZZ"), None);
    }

    #[test]
    fn parse_real_servers() {
        let backends = parse(SAMPLE);
        // Sorted by IP: .20, .21, .39
        assert_eq!(backends.len(), 3);

        assert_eq!(backends[0].ip,                  Ipv4Addr::new(172, 16, 32, 20));
        assert_eq!(backends[0].active_connections,   5);
        assert_eq!(backends[0].inactive_connections, 2);

        assert_eq!(backends[1].ip,                  Ipv4Addr::new(172, 16, 32, 21));
        assert_eq!(backends[1].active_connections,   3);
        assert_eq!(backends[1].inactive_connections, 1);

        assert_eq!(backends[2].ip,                  Ipv4Addr::new(172, 16, 32, 39));
        assert_eq!(backends[2].active_connections,   0);
    }

    #[test]
    fn parse_empty() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn skips_virtual_service_lines() {
        // VIP lines must not appear in the result
        let backends = parse(SAMPLE);
        let vip: Ipv4Addr = Ipv4Addr::new(172, 16, 29, 100);
        assert!(!backends.iter().any(|b| b.ip == vip));
    }
}
