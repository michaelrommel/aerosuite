# Aerosuite — Session 2 Handoff Summary

## What this session covered

Session 1 completed the workspace restructuring (W0–W9) and laid all the
foundation: `SystemSnapshot`, cleanup (P1–P3), and metrics/CloudWatch (P4).

This session focused exclusively on:
1. Completing and iterating the **P5 autoscaling algorithm** (`aeroscale/src/scaler.rs`)
2. **Cleanup correctness fixes** (lifecycle state handling, orphan grace period)
3. **aeropulse extension** — generating `backends.conf` from IMDS
4. **Workspace finalisation** — `aeroftp` joined, `aeroscrape`/`aerostress.new` removed,
   `libunftp` submodule corrected to `master` branch
5. Various **display and correctness fixes**

---

## Workspace — final state

```
aerosuite/
  Cargo.toml              ← workspace root; all 7 members listed
  aerocore/               ← shared library
  aeroftp/                ← FTP server (joined workspace this session)
  aerogym/                ← stress tester (package name fixed: was "aerostress")
  aeroplug/               ← ENI/IP manager
  aeropulse/              ← keepalived config generator (extended this session)
  aeroscale/              ← autoscaling daemon (main focus this session)
  aeroslot/               ← slot pool daemon
  aerobake/               ← Packer AMI definitions
  libunftp/               ← git submodule, master branch (was wrongly on aeroftp branch)
  unftp-sbe-opendal/      ← git submodule, untouched
```

**Release profile:** all workspace members use `opt-level=3, lto=fat, panic=abort`.
`aeroftp` overrides to `opt-level="s"` via `[profile.release.package.aeroftp]` in the
workspace root — it optimises for binary size rather than speed.

**`aeroscrape/`** has been fully absorbed into `aeroscale/src/metrics/` and can be
deleted from the repository.

---

## aerocore — additions this session

### `slot_network` module (moved from aeroscale)

`SlotNetwork` was living in `aeroscale/src/slot_network.rs`. It has been moved to
`aerocore/src/slot_network.rs` and re-exported at `aerocore::SlotNetwork`.

```rust
// IP = base + offset + slot
// base: from eth1 subnet CIDR via IMDS
// offset: from instance tag aeroftp-slot-offset
SlotNetwork::from_imds().await?
SlotNetwork::new(base, offset, prefix_len)  // for CLI overrides / tests
sn.ip_for_slot(slot)   // u32 -> Ipv4Addr
sn.slot_for_ip(ip)     // Ipv4Addr -> Option<u32>
```

`aeroscale/src/slot_network.rs` is now a one-line re-export shim. Both `aeroscale`
and `aeropulse` use `aerocore::SlotNetwork` directly.

---

## aeropulse — additions this session

### New output: `backends.conf`

aeropulse now generates **two** keepalived include files instead of one:

| File | Content |
|---|---|
| `/etc/keepalived/vrrp.conf` | VRRP instances + sync group (unchanged) |
| `/etc/keepalived/backends.conf` | `track_file` blocks + `virtual_server` block (new) |

The static `/etc/keepalived/keepalived.conf` in aerobake now contains only:
- Full documentation header (preserved)
- `global_defs { router_id aeroscale; enable_script_security; }`
- `include /etc/keepalived/vrrp.conf`
- `include /etc/keepalived/backends.conf`

### New IMDS tags (set in the EC2 launch template, "Resource types: Instances")

| Tag | Example value | Used by |
|---|---|---|
| `aeroftp-vip-outside` | `172.16.29.100` | aeropulse (virtual_server IP), aeroscale |
| `aeroftp-vip-inside` | `172.16.32.10` | aeropulse (VRRP VIP), aeroscale |
| `aeroftp-slot-count` | `20` | aeropulse (number of track_file + real_server entries) |
| `aeroftp-slot-offset` | `20` | aeroscale + aeropulse (via SlotNetwork::from_imds) |

`aeroftp-vip-outside` and `aeroftp-vip-inside` are **required** — aeropulse exits
with a clear error if either is missing. There are no CLI fallbacks for these.

### `--vip-outside` / `--vip-inside` removed from aeropulse

These CLI args have been removed entirely. VIPs come exclusively from IMDS tags.
Having CLI fallbacks was identified as a source of confusion about the source of truth.

### New aeropulse CLI args

```
--backends-out PATH       (default: /etc/keepalived/backends.conf)
--lvs-sched STR           (default: wlc)
--persistence-timeout N   (default: 30)
```

The weights directory (`/etc/keepalived/weights`) and FTP port (21) are hardcoded.

---

## aeroscale — additions this session

### P5: Scale-up / Drain algorithm (`aeroscale/src/scaler.rs`)

**Primary signal:** IPVS `active_connections` (keepalived's view), not Prometheus
scrape data. Scrape data is used only for cross-checking anomalies.

**IPVS connection normalisation:** FTP uses two TCP connections per session (control
channel port 21 + passive data channel). All session counts are normalised via:

```rust
fn sessions_from_ipvs(connections: u32) -> u32 { connections / 2 }
```

All thresholds (`scale_up_threshold`, `drain_threshold`) are expressed in **FTP
sessions**, not raw TCP connections.

#### Scale-up algorithm

```
avg_sessions = ceil(total_sessions / active_backends)
if avg_sessions > scale_up_threshold  →  increment scale_up_cycles
if scale_up_cycles >= hysteresis_cycles AND desired < max AND cooldown elapsed:
    SetDesiredCapacity(desired + 1)
```

#### Drain algorithm

```
Gate:   avg_sessions = total_sessions / active_backends  (floor)
        if avg_sessions >= drain_threshold  →  reset drain_cycles, return

        if draining_count >= max_concurrent_draining  →  return (do not reset)
        if any_draining AND no 0-session backend  →  wait

Hysteresis: drain_cycles >= hysteresis_cycles AND cooldown elapsed

Candidate selection:
  1. Any Active backend with 0 sessions   →  free drain (preferred)
  2. No drain in progress, all loaded     →  drain most-loaded backend
                                             (breaks IPVS persistence,
                                              redistributes pinned clients)
  3. Drain in progress, all loaded        →  wait
```

Only `Draining` state is written by the scaler. The P2 cleanup pass terminates
the instance once IPVS connections reach zero.

#### ScaleConfig fields and defaults (conf.d values take precedence over code defaults)

| Field | CLI flag | Code default | conf.d default |
|---|---|---|---|
| `scale_up_threshold` | `--scale-up-threshold` | 750 | 400 |
| `drain_threshold` | `--drain-threshold` | 500 | 200 |
| `hysteresis_cycles` | `--hysteresis-cycles` | 3 | 2 |
| `scale_up_cooldown_secs` | `--scale-up-cooldown-secs` | 120 | 20 |
| `drain_cooldown_secs` | `--drain-cooldown-secs` | 300 | 20 |
| `max_concurrent_draining` | `--max-concurrent-draining` | 2 | 2 |

#### ScalerState (lives outside the main loop, persists between cycles)

```rust
pub struct ScalerState {
    pub scale_up_cycles:  u32,
    pub drain_cycles:     u32,
    pub drain_candidate:  Option<Ipv4Addr>,  // kept for logging; not used for hysteresis
    pub last_scale_up:    Option<Instant>,
    pub last_drain:       Option<Instant>,
}
```

### `--vip-inside` IMDS fallback in aeroscale

aeroscale now resolves `vip_inside` with this priority:
1. `--vip-inside` CLI flag
2. IMDS tag `aeroftp-vip-inside`
3. `None` → always assumes MASTER (logs a warning)

The conf.d `VIP_INSIDE` is now empty by default (`""`). The init.d only passes
`--vip-inside` when the variable is non-empty (`${VIP_INSIDE:+--vip-inside ...}`).

### Cleanup fixes (`aeroscale/src/cleanup/`)

#### Section 2.1 — `asg_ids` InService-only filter

Previously `asg_ids` was built from **all** ASG instances regardless of lifecycle
state. A `Terminating` instance holding a lease was incorrectly seen as "valid" and
its slot was never released.

**Fix:** `asg_ids` now only includes `InService` instances (same definition as §2.2).
A `Terminating` instance is treated the same as a missing instance: its slot is
released and weight file disabled on the next cleanup cycle.

#### Section 2.2 — Orphan grace period

Previously: first sighting of an InService-without-lease → immediate termination.

**Fix:** a configurable grace period (`ORPHAN_GRACE_SECS`, default 180 s in conf.d)
is observed. State persists in `CleanupState.orphan_first_seen: HashMap<String, Instant>`.

Termination after grace period uses **`decrement=false`** unconditionally — the ASG
must replace the failed instance automatically. The global `TERM_DECREMENTS_CAPACITY`
flag does not apply here.

Log progression:
```
INFO  starting grace period                     (first sighting)
INFO  waiting (45/180s)                         (subsequent cycles)
ERROR terminating ... (capacity NOT decremented) (grace expired)
```

#### `CleanupState` struct (new, lives outside the main loop)

```rust
pub struct CleanupState {
    pub orphan_first_seen: HashMap<String, Instant>,
}
```

#### `terminate_instance` — `decrement: bool` parameter

`aerocore::asg::terminate_instance` now takes `decrement: bool` which maps to
`ShouldDecrementDesiredCapacity`. Callers:
- P2 cleanup (normal drain completion): uses `TERM_DECREMENTS_CAPACITY` config
- §2.2 orphan termination: always `false`
- §2.2 listener release messages: uses `TERM_DECREMENTS_CAPACITY` config
- `scale terminate` CLI command: always `true` (explicit operator intent)

### CloudWatch metrics (`aeroscale/src/metrics/cloudwatch.rs`)

All metrics are pushed per-slot with a `Slot` dimension. `StorTransfers` additionally
uses a `Status` dimension so success vs failure breakdown is preserved as separate
CloudWatch time series.

| CloudWatch name | Dimensions | Source | Unit |
|---|---|---|---|
| `ActiveSessions` | Slot | `ftp_sessions_total` (gauge) | Count |
| `CumulativeSessions` | Slot | `ftp_sessions_count` (counter) | Count |
| `BackendWriteBytes` | Slot | `ftp_backend_write_bytes` | Bytes |
| `BackendWriteFiles` | Slot | `ftp_backend_write_files` | Count |
| `ReceivedBytes` | Slot | `ftp_received_bytes{command="stor"}` | Bytes |
| `StorTransfers` | Slot + Status | `ftp_transferred_total{command="stor"}` per status | Count |
| `PassiveModeCommands` | Slot | sum of `ftp_command_total` for epsv+pasv | Count |
| `StorCommands` | Slot | `ftp_command_total{command="stor"}` | Count |
| `ResidentMemoryBytes` | Slot | `process_resident_memory_bytes` | Bytes |
| `OpenFileDescriptors` | Slot | `process_open_fds` | Count |
| `MaxFileDescriptors` | Slot | `process_max_fds` | Count |
| `Threads` | Slot | `process_threads` | Count |

Absent metrics (freshly started backend) silently push `0.0`. Max ~260 data points
per call (well within CloudWatch's 1000-point limit).

### IPVS cross-check (`aeroscale/src/metrics/mod.rs`)

Cross-check compares `ftp_sessions_total * 2` against `ipvs_active` (two TCP
connections per FTP session). Warns only when the difference exceeds
`SCRAPE_MISMATCH_PCT` (default 10% in conf.d). Only runs on the master — the
backup's IPVS table is always empty.

### Display (`aeroscale/src/snapshot/mod.rs`)

All box-drawing Unicode characters replaced with ASCII:
- `━` → `=` (outer bars, 85 chars)
- `─` → `-` (table separators, 85 chars)
- `⚠` → `[!]`
- `│` → `|`

Summary line: `2 act  0 drain  18 dis  |  2 leases  |  2 active  |  360 conns`

---

## All aeroscale CLI flags (current full set)

```
--region                   eu-west-2
--asg-name                 aeroftp-backend
--redis-url                $REDIS_URL
--weights-dir              /etc/keepalived/weights
--metrics-port             9090
--scrape-port              9090
--cloudwatch-namespace     AeroFTP/Autoscaler
--snapshot-interval        30
--vip-inside               <optional: falls back to IMDS tag aeroftp-vip-inside>
--weight-state-ttl         3600
--scale-up-threshold       750   (sessions/slot avg)
--drain-threshold          500   (sessions/slot avg)
--hysteresis-cycles        3
--scale-up-cooldown-secs   120
--drain-cooldown-secs      300
--scrape-mismatch-pct      5.0
--term-decrements-capacity true
--orphan-grace-secs        120
--max-concurrent-draining  2
--dry-run
--tls / --tls-insecure
--slot-base / --slot-offset   (override IMDS for dev)
```

---

## conf.d parameter reference (aerobake/aeroscale/_etc_conf.d_aeroscale)

The conf.d contains the live/testing-tuned values which differ from the code
defaults in several places. Always check conf.d for the values actually deployed.

---

## Dual-node behaviour summary (unchanged from session 1, confirmed working)

| Action | Master | Backup |
|---|---|---|
| Collect SystemSnapshot | ✅ | ✅ |
| Run cleanup (P2) | ✅ | ❌ |
| Run scaler (P5) | ✅ | ❌ |
| Persist weight state to Redis | ✅ | ❌ |
| Sync weight files from Redis | ❌ | ✅ |
| Scrape backend metrics / serve `/metrics` | ✅ | ✅ |
| IPVS cross-check | ✅ | ❌ (table always empty) |
| Push to CloudWatch | ✅ | ❌ |

---

## Open items / future work

- **Drain algorithm iteration**: the current algorithm works well in production
  scenarios. For testing with limited IP addresses and aggressive IPVS persistence,
  further tuning of `HYSTERESIS_CYCLES` and `DRAIN_COOLDOWN` in conf.d is the
  primary lever. The algorithm design itself may evolve further once production
  traffic patterns are observed.

- **`aeroscrape/` deletion**: safe to `git rm -r aeroscrape/` — fully absorbed.

- **`aeroftp` version field**: `aeroftp/Cargo.toml` has no `version` field (it
  predates the workspace). Works fine with `publish = false` but worth adding
  `version = "0.1.0"` for consistency.

- **`scale.rs` CLI `--asg-name` default**: currently defaults to `"ftp-asg"` which
  is the old name. Should be `"aeroftp-backend"` for consistency with the daemon.

- **Scale-up rate limiting**: currently a single cooldown between scale-up actions.
  If rapid multi-step scale-up is needed (e.g. from 1 → 3 backends quickly), the
  cooldown would need to be reduced or a "scale by N" strategy considered.

Good luck with production! 🚀
