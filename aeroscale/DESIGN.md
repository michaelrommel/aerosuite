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
  libunftp/                 ← untouched
  unftp-sbe-opendal/        ← untouched
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

```toml
[workspace]
resolver = "2"
members = [
    "aerocore",
    "aeroftp",
    "aerogym",
    "aeroscale",
    "aeroslot",
    "aeroplug",
    "aeropulse",
]

[workspace.dependencies]
# Pin shared dependency versions here so all crates stay in sync
anyhow      = "1"
tokio       = { version = "1", features = ["full"] }
clap        = { version = "4", features = ["derive", "env"] }
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
redis       = { version = "1.2", features = ["tls-rustls", "tokio-rustls-comp", "tls-rustls-insecure"] }
reqwest     = { version = "0.13", features = ["json"] }
chrono      = "0.4"
tracing     = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

### 0.7 Restructuring Step Checklist

These steps are purely mechanical — no logic changes. Do them in order and
verify `cargo build --workspace` after each.

```
[ ] W0  Add aerosuite/Cargo.toml workspace root with all members listed
[ ] W1  Create aerocore/ crate; move lib.rs content into it; fix imports in all existing bins
[ ] W2  Rename aerostress/ → aerogym/; update package name in Cargo.toml; rename binary
[ ] W3  Create aeroslot/ crate from slot-pool-native.rs; update openrc service name
[ ] W4  Create aeroplug/ crate from assign-secondary-ip.rs + attach-eni.rs + manage-eni.rs;
        consolidate into subcommands
[ ] W5  Create aeropulse/ crate from keepalived-config.rs
[ ] W6  Rename aeroscaler/ → aeroscale/; remove extracted bins; add aeroscale.rs skeleton
[ ] W7  Absorb aeroscrape/ into aeroscale/; remove aeroscrape/ directory
[ ] W8  Move aeroftp/images/ → aerobake/; update .pkr.hcl binary source paths
[ ] W9  Verify full workspace build; update all openrc service files and documentation
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

### asg-change Message Schema (current — partial)

```json
{ "slot": 3, "action": "claim" }
{ "slot": 3, "action": "release" }
```

**TODO (aeroslot):** The `release` message must also carry `"instance_id"` so
`autoscaler` can issue a `scale terminate` for the correct instance without an
extra Redis lookup.

---

### Refactoring Plan (prerequisite to autoscaler, superseded by W-steps above)

These map directly onto the workspace restructuring steps. They are listed here
for cross-reference:

- [ ] **R1** — Extract ASG logic from `scale.rs` into `aerocore::asg`
      (`describe`, `set_desired`, `terminate_instance`). Covered by **W1**.
- [ ] **R2** — Extract `build_redis_client` and key constants from
      `slot-pool-native.rs` into `aerocore::redis`. Covered by **W1** + **W3**.
- [ ] **R3** — Decide slot→IP mapping strategy.
      *Current assumption:* the weight filename encodes the IP, so we can
      reverse-map slot→IP by scanning `/etc/keepalived/weights/`. Confirm or
      add a `slot:ip:<n>` Redis key.

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
| `"release"` | Write `"-2147483648"` to weight file, call `scale terminate <instance_id>` (requires `instance_id` in message — see TODO above) |

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
`ftp_sessions_total` as a sanity check; log discrepancies.

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

Adapt `metrics_cloudwatch_embedded` from the absorbed `aeroscrape` codebase.

**Critical fix vs aeroscrape:** dimensions use slot numbers, never instance IDs.
This prevents unbounded metric registry growth as instances are replaced.

Namespace: `AeroFTP/Autoscaler`

---

### Phase 5 — Autoscaling: Scale Decisions

*(Algorithm to be designed — placeholder)*

Inputs: aggregated metrics snapshot from Phase 4.

Candidate triggers:
- **Scale up** when average `ftp_sessions_total` per active slot exceeds a
  configurable high-water mark AND active slot count < configured max.
- **Drain** when a slot's `ftp_sessions_total` is zero AND active slot count >
  configured low-water mark.

Scale up → `scale --desired <n+1>`.
Drain → write `"-1"` to the backend's weight file; Phase 2 cleanup handles
the rest once IPVS connections reach zero.

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
[ ] W0  Add aerosuite/Cargo.toml workspace root
[ ] W1  Create aerocore/ — move lib.rs (aws/, asg/, redis/ modules)
[ ] W2  Rename aerostress/ → aerogym/; rename package + binary
[ ] W3  Create aeroslot/ from slot-pool-native.rs; rename openrc service
[ ] W4  Create aeroplug/ — consolidate assign-secondary-ip, attach-eni, manage-eni
[ ] W5  Create aeropulse/ from keepalived-config.rs
[ ] W6  Rename aeroscaler/ → aeroscale/; strip extracted bins; add aeroscale.rs skeleton
[ ] W7  Absorb aeroscrape/ into aeroscale/; delete aeroscrape/
[ ] W8  Move aeroftp/images/ → aerobake/; update .pkr.hcl binary source paths

autoscaler implementation
[ ] P1  SystemSnapshot — read weight files, Redis leases, ASG, /proc/net/ip_vs (read-only)
[ ] P1  Print snapshot table on startup (first working aeroscale binary)
[ ] P2  Cleanup actions with --dry-run flag
[ ] P3  asg-change Redis subscriber
[ ] P4  Prometheus scrape + slot-labelled aggregation
[ ] P4  Expose /metrics HTTP endpoint
[ ] P4  Push to CloudWatch (slot-labelled)
[ ] P5  Design and implement scale-up / drain algorithm

Outstanding decisions
[ ] R3  Confirm slot→IP mapping strategy (weight filename scan vs Redis key)
[ ] R4  Confirm slot-pool-native TODO: add instance_id to release message in aeroslot
[ ] R5  Decide whether aws-config binary stays in aeroscale or moves to aerocore
```
