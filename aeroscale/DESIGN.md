# aeroscale — Design Document

## Part 0 — Workspace Restructuring & Naming Conventions

### 0.1 Motivation

The suite has grown beyond a single crate. Moving to a Cargo **workspace** under
`aerosuite/` gives each logical component its own crate with its own name,
version, and dependency set, while sharing a single `Cargo.lock` and `target/`
directory.

### 0.2 Name Map

| New name | Old location / name | Role |
|---|---|---|
| `aeroftp` | `aerosuite/aeroftp/` (unchanged) | Scalable classic FTP service in the cloud |
| `aerogym` | `aerosuite/aerostress/` | Stress tester — let your FTP service flex its muscle |
| `aeroslot` | `src/bin/slot-pool-native.rs` + openrc `slotmanager` | Slot manager — acquiring a place to perform |
| `aeroscale` | `aerosuite/aeroscaler/` (this crate) + absorbs `aeroscrape/` | Elasticity guru — scaling your service out or in |
| `aeroplug` | `src/bin/assign-secondary-ip.rs`, `attach-eni.rs`, `manage-eni.rs` | Network connectivity facilitator — managing ENIs and IPs |
| `aeropulse` | `src/bin/keepalived-config.rs` | Config assistant for your keepalived balancer |

The directories `libunftp/`, `unftp-sbe-opendal/`, and the patch file
`libunftp_metadata.patch` are **left untouched** — they are compile-time
dependencies of `aeroftp` only.

### 0.3 Target Directory Layout

```
aerosuite/
  Cargo.toml                ← workspace root (new)
  target/                   ← single shared build output directory for all crates
  aerocore/                 ← new shared library crate (see §0.4)
  aeroftp/                  ← unchanged
  aerogym/                  ← renamed from aerostress/
  aeroscale/                ← renamed from aeroscaler/ — main daemon + scale CLI
  aeroslot/                 ← extracted from aeroscaler/src/bin/slot-pool-native.rs
  aeroplug/                 ← extracted from aeroscaler/src/bin/{assign-secondary-ip,attach-eni,manage-eni}.rs
  aeropulse/                ← extracted from aeroscaler/src/bin/keepalived-config.rs
  aerobake/                 ← moved from aeroftp/images/ — Packer AMI image definitions
    backend/                   AMI for FTP backend instances
    loadbalancer/              AMI for keepalived load balancer instances
  libunftp/                 ← git submodule (untouched) — aeroftp path dependency
  unftp-sbe-opendal/        ← git submodule (untouched) — aeroftp path dependency
  libunftp_metadata.patch   ← untouched
```

### 0.4 Shared Library: `aerocore`

Several crates need the same AWS/SigV4/IMDSv2/Redis plumbing that currently
lives in `aeroscaler/src/lib.rs`. Rather than duplicating it or forcing every
crate to depend on `aeroscale`, these helpers are extracted into a dedicated
internal library crate:

```
aerosuite/
  aerocore/                 ← new shared library (no binary)
    src/
      lib.rs
      aws/          ← AwsCredentials, fetch_imds_*, sigv4_sign, aws_query, XML helpers
      redis/        ← build_redis_client, key constants (slots:available etc.)
      asg/          ← describe_asg, set_desired, terminate_instance  (from scale.rs)
```

Dependency graph (simplified):

```
aeroftp    (no aerocore dependency)
aerogym    (no aerocore dependency)
aeroslot   → aerocore (redis/)
aeroplug   → aerocore (aws/)
aeropulse  → aerocore (aws/)
aeroscale  → aerocore (aws/ + redis/ + asg/)
```

> **Decision point:** `aerocore` is a private, path-only dependency — it is
> never published to crates.io. All workspace members reference it as
> `aerocore = { path = "../aerocore" }`.

### 0.5a AMI Image Definitions: `aerobake/`

The Packer build definitions currently live in `aeroftp/images/backend/` and
`aeroftp/images/loadbalancer/`. They are **not** part of `aeroftp` — they are
cross-cutting assembly manifests that pull in compiled binaries from
`aeroscale`, `aeroslot`, `aeroplug`, `aeropulse`, and `aeroftp`. Keeping them
inside `aeroftp/` is misleading and makes relative paths fragile.

Moving them to `aerosuite/aerobake/` makes the relationship explicit: this
directory assembles the final deployable AMI from the outputs of all other
workspace members.

**Path simplification from workspace `target/`:**

With a Cargo workspace, all crates share a single `aerosuite/target/` directory.
The Packer files can then reference every binary through one consistent relative
path instead of navigating into each crate individually:

| Before (from `aeroftp/images/backend/`) | After (from `aerobake/backend/`) |
|---|---|
| `../../../aeroscaler/target/release/slot-pool-native` | `../../target/release/aeroslot` |
| `../../../aeroscaler/target/release/manage-eni` | `../../target/release/aeroplug` |
| `../../target/release/aeroftp` | `../../target/release/aeroftp` |

| Before (from `aeroftp/images/loadbalancer/`) | After (from `aerobake/loadbalancer/`) |
|---|---|
| `../../../aeroscaler/target/release/aeroscaler` | `../../target/release/autoscaler` |
| `../../../aeroscaler/target/release/keepalived-config` | `../../target/release/aeropulse` |
| `../../../aeroscaler/target/release/assign-secondary-ip` | `../../target/release/aeroplug` |
| `../../../aeroscaler/target/release/slot-pool-native` | `../../target/release/aeroslot` |

The openrc service files (`_etc_init.d_slotmanager`, `_etc_conf.d_slotmanager`)
and any references to `slot-pool-native` or `aeroscaler` inside the packer
scripts need to be updated to the new binary names as part of step **W8**.

### 0.5 Binary Names Within Each Crate

Binary names (the compiled executable names) do not have to match crate names.
The following table shows what each crate produces:

| Crate | Binary / service name | Notes |
|---|---|---|
| `aeroscale` | `aeroscale` (daemon), `scale` (CLI) | `scale` kept for operator convenience |
| `aeroslot` | `aeroslot` | Replaces `slot-pool-native`; openrc service renamed `aeroslot` |
| `aeroplug` | `aeroplug` | Single consolidated binary replacing three separate ones; subcommands: `assign-ip`, `attach-eni`, `detach-eni` |
| `aeropulse` | `aeropulse` | Replaces `keepalived-config` |
| `aerogym` | `aerogym` | Replaces `aerostress` |
| `aws-config` | `aws-config` | Standalone helper, stays in `aeroscale` or moves to `aerocore` as a thin binary |

### 0.6 Workspace `Cargo.toml` Skeleton

The final workspace root after all restructuring is complete:

```toml
[workspace]
resolver = "2"
members = [
    "aerocore",
    "aeroftp",
    "aerogym",
    "aeroplug",
    "aeropulse",
    "aeroscale",
    "aeroslot",
]
exclude = [
    "libunftp",           # git submodule — aeroftp path dependency
    "unftp-sbe-opendal",  # git submodule — aeroftp path dependency
]
```

The `[workspace.dependencies]` table pins shared dependency versions so all
crates stay in sync.  Individual crate `Cargo.toml` files opt in with
`dep = { workspace = true }`.  See the workspace root `Cargo.toml` for the
current pinned versions.

### 0.7 Restructuring Step Checklist

These steps are purely mechanical — no logic changes. Do them in order and
verify `cargo build --workspace` after each.

```
[x] W0  Add aerosuite/Cargo.toml workspace root with all members listed
[x] W1  Create aerocore/ crate; move lib.rs content into it; fix imports in all existing bins
[x] W2  Rename aerostress/ → aerogym/; update package name in Cargo.toml; rename binary
[x] W3  Create aeroslot/ crate from slot-pool-native.rs; update openrc service name
[x] W4  Create aeroplug/ crate from assign-secondary-ip.rs + attach-eni.rs + manage-eni.rs;
        consolidate into subcommands
[x] W5  Create aeropulse/ crate from keepalived-config.rs
[x] W6  Rename aeroscaler/ → aeroscale/; remove extracted bins; add aeroscale.rs skeleton
[x] W7  Absorb aeroscrape/ into aeroscale/; remove aeroscrape/ directory
[x] W8  Move aeroftp/images/ → aerobake/; update .pkr.hcl binary source paths
[x] W9  Verify full workspace build; update all openrc service files and documentation
```

---

## Part 1 — autoscaler Daemon Design

### Overview

`autoscaler` is a long-running daemon that runs on the keepalived **MASTER**
node. It has two distinct responsibilities:

1. **Backend Management** — monitors the state of all FTP backend slots and
   keeps keepalived, Redis, and the AWS ASG consistent with each other.
2. **Autoscaling** — scrapes Prometheus metrics from all active backends,
   aggregates them by slot number, exposes them as a local Prometheus endpoint,
   pushes them to AWS CloudWatch, and drives scale-up / drain decisions.

---

### Existing Code Inventory

| Location | What it provides | Reuse plan |
|---|---|---|
| `aeroscaler/src/lib.rs` | `AwsCredentials`, IMDSv2, SigV4, `aws_query`, XML helpers | Move to `aerocore::aws` (W1) |
| `src/bin/scale.rs` | ASG DescribeAutoScalingGroups, SetDesiredCapacity, TerminateInstanceInAutoScalingGroup | Extract to `aerocore::asg` (R1 / W1) |
| `src/bin/slot-pool-native.rs` | Redis key schema, `build_redis_client` | Move to `aerocore::redis` and new `aeroslot` crate (R2 / W3) |
| `aeroscrape/src/main.rs` | Prometheus scraping (`prometheus_parse`), CloudWatch push via `metrics_cloudwatch_embedded` | Absorb into `aeroscale` (W7); use **slot numbers** as labels |

### Redis Key Schema (canonical reference)

```
slots:available          sorted-set   score = freed-at-ms (0 = since init)
slots:leases             sorted-set   score = expiry-ms
slot:owner:<n>           string       value = instance-id
asg-change               pub/sub channel
```

### Weight File Schema

```
/etc/keepalived/weights/backend-<IP>.weight
  "0"           → active / healthy
  "-1"          → draining (graceful shutdown in progress)
  "-2147483648" → disabled (keepalived ignores this real server)
```

### asg-change Message Schema

```json
{ "slot": 3, "action": "claim" }
{ "slot": 3, "action": "release", "instance_id": "i-0abc1234567890def" }
```

The `instance_id` field on `release` messages was added in aeroslot (R4) so
the listener can terminate the instance immediately without an extra Redis
lookup.  The listener degrades gracefully if `instance_id` is absent (older
aerolslot versions): a warning is logged and termination is deferred to the
next P2 cleanup pass.

---

### Refactoring Plan (prerequisite to autoscaler, superseded by W-steps above)

These map directly onto the workspace restructuring steps. They are listed here
for cross-reference:

- [x] **R1** — Extract ASG logic from `scale.rs` into `aerocore::asg`
      (`describe`, `set_desired`, `terminate_instance`). Covered by **W1**.
- [x] **R2** — Extract `build_redis_client` and key constants from
      `slot-pool-native.rs` into `aerocore::redis`. Covered by **W1** + **W3**.
- [x] **R3** — Slot→IP mapping resolved: `SlotNetwork::ip_for_slot(slot)` and
      `SlotNetwork::slot_for_ip(ip)` use a deterministic arithmetic formula
      (`base + offset + slot`) derived from the load balancer's eth1 subnet CIDR
      (IMDS) and the `aeroftp-slot-offset` instance tag.  No weight-filename
      scan and no `slot:ip:<n>` Redis key required.

---

### Phase 1 — Backend Management: Read / Observe State

**Goal:** Collect a unified snapshot of the system. No writes, no side-effects.

#### 1.1 Read weight files

Parse all files matching `/etc/keepalived/weights/backend-<IP>.weight`:

```rust
struct BackendWeight {
    ip: Ipv4Addr,
    state: BackendState,
}

enum BackendState { Active, Draining, Disabled }
// Active = "0", Draining = "-1", Disabled = "-2147483648"
```

#### 1.2 Read Redis lease state

- `ZRANGE slots:leases 0 -1 WITHSCORES` → all leased slots + expiry timestamps
- For each leased slot `n`: `GET slot:owner:<n>` → instance-id

```rust
struct SlotLease {
    slot: u32,
    owner_instance_id: String,
    expires_ms: u64,
}
```

#### 1.3 Read AWS ASG instance list

Call `DescribeAutoScalingGroups` for `aeroftp-backend`.
Collect all instances with `LifecycleState == "InService"`.

```rust
struct AsgInstance {
    instance_id: String,
}
```

#### 1.4 Read active IPVS connections

Read `/proc/net/ip_vs` directly (no `ipvsadm` binary needed).
Addresses are big-endian hex (`AC102014` → `172.16.32.20`). Per real server (by IP):

```rust
struct IpvsBackend {
    ip: Ipv4Addr,
    active_connections: u32,
}
```

#### 1.5 Assemble SystemSnapshot

```rust
struct SystemSnapshot {
    backends: Vec<BackendStatus>,   // one entry per weight file
    leases:   Vec<SlotLease>,       // from Redis
    asg:      Vec<AsgInstance>,     // from AWS
    ipvs:     Vec<IpvsBackend>,     // from /proc/net/ip_vs
    taken_at: Instant,
}

struct BackendStatus {
    ip:           Ipv4Addr,
    weight_state: BackendState,
    lease:        Option<SlotLease>,
    ipvs:         Option<IpvsBackend>,
}
```

Refreshed on a configurable interval (default: 30 s) and on every `asg-change`
event.

**Milestone P1:** `autoscaler` starts, prints a clean state table, exits
cleanly. No writes yet.

---

### Phase 2 — Backend Management: Cleanup Actions

All actions are derived from the snapshot. Each action is logged before
execution. A `--dry-run` flag suppresses all writes and prints what *would*
happen.

#### 2.1 Cleanup: Active Leases

For each `SlotLease`:

| Condition | Action |
|---|---|
| `owner_instance_id` NOT in ASG | Release slot (Redis), disable backend (`-2147483648`) |
| Weight = Active (`"0"`) | No-op |
| Weight = Draining (`"-1"`) AND `active_connections == 0` | Disable backend, call `scale terminate <instance_id>` |
| Weight = Draining AND `active_connections > 0` | No-op (wait) |
| Weight = Disabled | Enable backend (`"0"`) — missed Redis message recovery |

#### 2.2 Cleanup: ASG Instances Without Leases

For each `AsgInstance` with **no** matching `SlotLease`:

| Condition | Action |
|---|---|
| No lease found | Log ERROR ("orphaned instance — possible ENI leak"), call `scale terminate <instance_id>` |

#### 2.3 Cleanup: Backends Without Leases

For each `BackendStatus` where `lease == None`:

| Weight state | Action |
|---|---|
| Active (`"0"`) | Log WARN ("active backend has no lease — crash?"), disable backend (`-2147483648`) |
| Draining (`"-1"`) | Log INFO ("draining backend has no lease — crash"), disable backend (`-2147483648`) |
| Disabled | No-op |

---

### Phase 3 — Redis Channel Listener

Subscribe to `asg-change` on startup (separate tokio task).

| `action` field | Behaviour |
|---|---|
| `"claim"` | Write `"0"` to the weight file for the backend IP of the claimed slot |
| `"release"` | Write `"-2147483648"` to weight file; terminate instance via `instance_id` in message (degrades gracefully if absent — defers to next P2 cleanup pass) |

After each message: trigger a full snapshot refresh.

---

### Phase 4 — Autoscaling: Metrics Collection

#### 4.1 Prometheus scrape

For every backend with an active lease, scrape `http://<backend-IP>:9090/metrics`
using `prometheus-parse`.

Key metrics:
- `ftp_sessions_total` — active FTP sessions (gauge)
- `ftp_sessions_count` — cumulative sessions
- `ftp_command_total` — command throughput

#### 4.2 IPVS cross-check

Active connection counts are already in the snapshot (§1.4). Compare against
`ftp_sessions_total` as a sanity check; warn only when the relative difference
exceeds a configurable threshold (`--scrape-mismatch-pct`, default **5%**).
This prevents noise from normal sampling jitter on lightly-loaded backends
while surfacing real routing or session-counting anomalies.  The warning log
includes the exact percentage so anomalies are quantified.

#### 4.3 Aggregated metrics structure

Label all metrics with **slot number** — never instance-id — to keep the
label-set stable across instance rotation.

```
ftp_sessions_total{slot="3"} 12
ftp_sessions_total{slot="5"} 7
```

#### 4.4 Expose aggregated Prometheus endpoint

Serve `/metrics` on a configurable port (default: `9091`) via `axum`.

#### 4.5 Push to AWS CloudWatch

Adapted from the absorbed `aeroscrape` codebase via our own `aws_query` /
SigV4 infrastructure — no CloudWatch agent or EMF capture needed.

**Critical fix vs aeroscrape:** dimensions use slot numbers, never instance IDs.
This prevents unbounded metric registry growth as instances are replaced.

Namespace: `AeroFTP/Autoscaler`

Metrics pushed per slot.  Any metric absent from the scrape (e.g. a freshly
started backend with no transfers yet) is silently pushed as `0.0`.

| CloudWatch name        | Dimensions    | Source Prometheus metric                                    | Unit  |
|------------------------|---------------|-------------------------------------------------------------|-------|
| `ActiveSessions`       | Slot          | `ftp_sessions_total`                         (gauge)        | Count |
| `CumulativeSessions`   | Slot          | `ftp_sessions_count`                         (counter)      | Count |
| `BackendWriteBytes`    | Slot          | `ftp_backend_write_bytes`                    (counter)      | Bytes |
| `BackendWriteFiles`    | Slot          | `ftp_backend_write_files`                    (counter)      | Count |
| `ReceivedBytes`        | Slot          | `ftp_received_bytes{command="stor"}`         (counter)      | Bytes |
| `StorTransfers`        | Slot + Status | `ftp_transferred_total{command="stor"}` — one row per status value | Count |
| `PassiveModeCommands`  | Slot          | sum of `ftp_command_total` for `epsv` + `pasv` (counter)   | Count |
| `StorCommands`         | Slot          | `ftp_command_total{command="stor"}`          (counter)      | Count |
| `ResidentMemoryBytes`  | Slot          | `process_resident_memory_bytes`              (gauge)        | Bytes |
| `OpenFileDescriptors`  | Slot          | `process_open_fds`                           (gauge)        | Count |
| `MaxFileDescriptors`   | Slot          | `process_max_fds`                            (gauge)        | Count |
| `Threads`              | Slot          | `process_threads`                            (gauge)        | Count |

`StorTransfers` uses a `{Slot, Status}` dimension pair so each status value
(`success`, `failure`, etc.) becomes a separate CloudWatch time series.
CloudWatch metric math can then express the failure rate as
`failure / (success + failure)` without server-side aggregation.  If no
transfer data is present yet nothing is pushed for this metric; CloudWatch
handles sparse time series gracefully.

Worst-case data points: 20 slots × (11 single-dim + ~2 status values) ≈ 260,
well within CloudWatch's 1000-point-per-call limit.

---

### Phase 5 — Autoscaling: Scale Decisions

**Primary signal: IPVS `active_connections`** (the load balancer's real-time
view), not per-backend Prometheus scrapes.  Scrape data is used only for the
cross-check warning in Phase 4.2.  All scale decisions are master-only.

#### 5.1 Scale-up algorithm

Each cycle, compute the **ceiling-average** active connections across all
`Active` backends (live lease + IPVS data present):

```
avg = ceil(total_connections / active_count)
```

When `avg > scale_up_threshold` for `hysteresis_cycles` consecutive cycles
AND `desired < max` AND the scale-up cooldown has elapsed:

```
SetDesiredCapacity(desired + 1)
```

The hysteresis counter resets on any cycle where the average drops back below
the threshold, or after a successful scale-up action.  If the AWS call fails
the counter is preserved so the next cycle retries.

**CLI flags:** `--scale-up-threshold` (default: **750**), `--hysteresis-cycles`
(default: **3**), `--scale-up-cooldown-secs` (default: **120 s**).

If `desired == max` the condition is logged as a warning and the counter is
preserved; the action fires immediately if `max_size` is later raised.

#### 5.2 Drain algorithm

The backend with the **fewest** active connections is the drain *candidate*.
Worst-case load on the busiest remaining backend after removal:

```
extra_per  = ceil(candidate_connections / (active_count − 1))
worst_case = max_connections + extra_per
```

When `worst_case < drain_threshold` for `hysteresis_cycles` consecutive cycles
AND `desired > min` AND the drain cooldown has elapsed:

- Write `"-1"` (DRAINING) to the candidate's weight file.
- keepalived stops routing new connections to that backend.
- The **P2 cleanup pass** handles the rest: on a future cycle when IPVS
  active connections reach zero it disables the backend (`-2147483648`) and
  calls `TerminateInstanceInAutoScalingGroup`.

**Candidate stability:** if the cheapest backend changes between cycles the
hysteresis counter resets.  This prevents draining a backend that only briefly
had the lowest count due to a transient spike on another.

**CLI flags:** `--drain-threshold` (default: **500**), `--hysteresis-cycles`
(default: **3**), `--drain-cooldown-secs` (default: **300 s**).

#### 5.3 Desired-capacity decrement

`TerminateInstanceInAutoScalingGroup` passes `ShouldDecrementDesiredCapacity`
as a configurable flag (`--term-decrements-capacity`, default: **true**).
Set to `false` during testing to let the ASG launch a replacement automatically.

This flag is respected consistently everywhere an instance is terminated:
the P2 cleanup pass, the `asg-change` listener, and the `scale terminate` CLI
command (the CLI always uses `true` as it represents explicit operator intent).

#### 5.4 Safety floors

- Scale-up: never exceeds ASG `max_size`.
- Drain: never initiates when `desired <= min_size`.
- P2 cleanup: never terminates when `desired == min_size` (the ASG min-size
  guard added in P2 prevents the 400 error from AWS).

No separate "minimum active backends" floor is configured — the ASG `min_size`
is the single authoritative constraint.

---

### aeroscale Binary Structure (post-restructuring)

```
aeroscale/
  Cargo.toml
  src/
    bin/
      aeroscale.rs   ← daemon (this design)
      scale.rs        ← CLI tool (kept for operator convenience)
      aws-config.rs   ← or move to aerocore as thin binary
```

### autoscaler CLI sketch

```
aeroscale [OPTIONS]

Options:
  --region <REGION>        AWS region [default: eu-west-2]
  --asg-name <NAME>        ASG name for FTP backends [default: aeroftp-backend]
  --redis-url <URL>        Redis connection URL [env: REDIS_URL]
  --weights-dir <DIR>      Weight files directory [default: /etc/keepalived/weights]
  --metrics-port <PORT>    Port to expose aggregated /metrics [default: 9091]
  --snapshot-interval <S>  Seconds between full state refreshes [default: 30]
  --dry-run                Log actions but do not write anything
  --tls / --tls-insecure   Redis TLS options (same as aeroslot)
```

---

## Master Implementation Checklist

Work through these in order. Each item must compile and be spot-checked before
moving to the next.

```
Workspace restructuring
[x] W0  Add aerosuite/Cargo.toml workspace root
[x] W1  Create aerocore/ — move lib.rs (aws/, asg/, redis/ modules)
[x] W2  Rename aerostress/ → aerogym/; rename package + binary
[x] W3  Create aeroslot/ from slot-pool-native.rs; rename openrc service
[x] W4  Create aeroplug/ — consolidate assign-secondary-ip, attach-eni, manage-eni
[x] W5  Create aeropulse/ from keepalived-config.rs
[x] W6  Rename aeroscaler/ → aeroscale/; strip extracted bins; add aeroscale.rs skeleton
[x] W7  Absorb aeroscrape/ into aeroscale/; delete aeroscrape/
[x] W8  Move aeroftp/images/ → aerobake/; update .pkr.hcl binary source paths

autoscaler implementation
[x] P1  SystemSnapshot — read weight files, Redis leases, ASG, /proc/net/ip_vs (read-only)
[x] P1  Print snapshot table on startup (first working aeroscale binary)
[x] P2  Cleanup actions with --dry-run flag
[x] P3  asg-change Redis subscriber
[x] P4  Prometheus scrape + slot-labelled aggregation
[x] P4  Expose /metrics HTTP endpoint
[x] P4  Push to CloudWatch (slot-labelled)
[x] P5  Design and implement scale-up / drain algorithm

Outstanding decisions
[x] R3  Slot→IP mapping: resolved with SlotNetwork deterministic formula
        (subnet CIDR via IMDS + aeroftp-slot-offset tag).  No Redis key needed.
[x] R4  instance_id in release message: implemented in aeroslot; listener and
        cleanup both consume it for immediate termination.
[x] R5  aws-config binary: stays in aeroscale/src/bin/aws-config.rs.
```
