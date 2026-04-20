//! Deterministic slot → IP address mapping.
//!
//! Every backend instance occupies a numbered slot.  Its IP address is
//! computed from three values:
//!
//!   IP = base_addr + offset + slot
//!
//! For example, with base = 172.16.32.0 and offset = 20:
//!
//!   slot  0 → 172.16.32.20
//!   slot 11 → 172.16.32.31
//!   slot 12 → 172.16.32.32
//!   slot 19 → 172.16.32.39
//!
//! This is the same formula used by the backend's `aeroftp-routing` OpenRC
//! service (`BASE_IP`, `OFFSET` in `/etc/conf.d/aeroftp-routing`).
//!
//! ## IMDS discovery (default)
//!
//! - **base** and **prefix_len**: read from the load balancer's eth1
//!   `subnet-ipv4-cidr-block` IMDS entry.
//! - **offset**: read from the instance tag `aeroftp-slot-offset`.
//!
//! ## CLI overrides
//!
//! Pass `--slot-base <IP>` and `--slot-offset <N>` to bypass IMDS — useful
//! for local development or when the tag is not yet present.

use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use tracing::info;

use aerocore::{fetch_imds_path, fetch_imds_token};

// ── Struct ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SlotNetwork {
    /// Network base address of the backend slot subnet (e.g. `172.16.32.0`).
    /// Obtained from the eth1 subnet CIDR block reported by IMDS.
    pub base: Ipv4Addr,

    /// First slot's offset from `base` (e.g. `20` → slot 0 = `.20`).
    /// Read from the `aeroftp-slot-offset` instance tag.
    pub offset: u32,

    /// Subnet prefix length (e.g. `26`).  Not needed for the formula but
    /// recorded for logging and future validation.
    pub prefix_len: u8,
}

impl SlotNetwork {
    /// Construct from explicit values — CLI override or unit tests.
    pub fn new(base: Ipv4Addr, offset: u32, prefix_len: u8) -> Self {
        Self { base, offset, prefix_len }
    }

    /// Discover the slot network from the load balancer instance's IMDS.
    ///
    /// Steps:
    /// 1. Walk all MACs and find the one with `device-number == 1` (eth1).
    /// 2. Read `subnet-ipv4-cidr-block` for that MAC → base IP + prefix len.
    /// 3. Read instance tag `aeroftp-slot-offset` → offset.
    pub async fn from_imds() -> Result<Self> {
        let token = fetch_imds_token().await?;

        // ── Find eth1 (device index 1) ────────────────────────────────────────
        let macs_raw = fetch_imds_path(&token, "network/interfaces/macs/").await?;

        let mut eth1_mac: Option<String> = None;
        for mac in macs_raw.lines().map(|s| s.trim_end_matches('/').trim()) {
            if mac.is_empty() { continue; }
            let dev = fetch_imds_path(
                &token,
                &format!("network/interfaces/macs/{mac}/device-number"),
            )
            .await
            .unwrap_or_default();
            if dev.trim() == "1" {
                eth1_mac = Some(mac.to_string());
                break;
            }
        }

        let mac = eth1_mac.context(
            "No eth1 (device index 1) found in IMDS — is the inside ENI attached?"
        )?;

        // ── Read subnet CIDR for eth1 (e.g. "172.16.32.0/26") ────────────────
        let cidr = fetch_imds_path(
            &token,
            &format!("network/interfaces/macs/{mac}/subnet-ipv4-cidr-block"),
        )
        .await
        .context("Failed to read eth1 subnet CIDR from IMDS")?;

        let cidr = cidr.trim();
        let (base_str, prefix_str) = cidr
            .split_once('/')
            .with_context(|| format!("IMDS returned invalid CIDR: '{cidr}'"))?;

        let base: Ipv4Addr = base_str
            .parse()
            .with_context(|| format!("Invalid subnet base IP: '{base_str}'"))?;
        let prefix_len: u8 = prefix_str
            .parse()
            .with_context(|| format!("Invalid prefix length: '{prefix_str}'"))?;

        // ── Read slot offset from instance tag ────────────────────────────────
        let offset_raw = fetch_imds_path(&token, "tags/instance/aeroftp-slot-offset")
            .await
            .context(
                "Instance tag 'aeroftp-slot-offset' not found — \
                 add it to the ASG launch template",
            )?;
        let offset: u32 = offset_raw
            .trim()
            .parse()
            .with_context(|| format!("Invalid aeroftp-slot-offset value: '{offset_raw}'"))?;

        info!(
            %base,
            prefix_len,
            offset,
            subnet_cidr = cidr,
            "slot network resolved from IMDS"
        );

        Ok(Self { base, offset, prefix_len })
    }

    // ── Mapping helpers ───────────────────────────────────────────────────────

    /// Compute the backend IP for a given slot number.
    ///
    /// This cannot overflow in practice: slots are 0–19, offset is ~20,
    /// and base is a /26 or larger subnet.
    #[inline]
    pub fn ip_for_slot(&self, slot: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.base) + self.offset + slot)
    }

    /// Reverse-map an IP address to a slot number.
    /// Returns `None` if the IP is before the slot range starts.
    #[inline]
    pub fn slot_for_ip(&self, ip: Ipv4Addr) -> Option<u32> {
        let ip_u32   = u32::from(ip);
        let base_u32 = u32::from(self.base) + self.offset;
        ip_u32.checked_sub(base_u32)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn net() -> SlotNetwork {
        SlotNetwork::new("172.16.32.0".parse().unwrap(), 20, 26)
    }

    #[test]
    fn slot_to_ip() {
        assert_eq!(net().ip_for_slot(0),  "172.16.32.20".parse::<Ipv4Addr>().unwrap());
        assert_eq!(net().ip_for_slot(11), "172.16.32.31".parse::<Ipv4Addr>().unwrap());
        assert_eq!(net().ip_for_slot(12), "172.16.32.32".parse::<Ipv4Addr>().unwrap());
        assert_eq!(net().ip_for_slot(19), "172.16.32.39".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn ip_to_slot() {
        assert_eq!(net().slot_for_ip("172.16.32.20".parse().unwrap()), Some(0));
        assert_eq!(net().slot_for_ip("172.16.32.31".parse().unwrap()), Some(11));
        assert_eq!(net().slot_for_ip("172.16.32.32".parse().unwrap()), Some(12));
        // IP before slot range starts
        assert_eq!(net().slot_for_ip("172.16.32.19".parse().unwrap()), None);
        // Completely different subnet
        assert_eq!(net().slot_for_ip("172.16.29.100".parse().unwrap()), None);
    }

    #[test]
    fn roundtrip() {
        let n = net();
        for slot in 0..20 {
            let ip = n.ip_for_slot(slot);
            assert_eq!(n.slot_for_ip(ip), Some(slot), "roundtrip failed for slot {slot}");
        }
    }
}
