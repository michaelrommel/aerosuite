# Aerosuite Monitoring & Load Control — Architecture Plan v2

## Overview

This document supersedes `ARCHITECTURE_PLAN.md`. It incorporates the revised strategy from
`newplan.md` and organises the work into a gated, parallelisable delivery plan.

Three new modules are being added to the `aerosuite` workspace:

| Module | Language | Role |
|---|---|---|
| **aerogym** (extended) | Rust | FTP stress-test agent — executes the load plan and reports results |
| **aerocoach** | Rust | Controller + aggregator — owns the load model, distributes work, collects metrics, feeds the dashboard |
| **aerotrack** | Svelte + TypeScript | Real-time dashboard — visualises live test progress over WebSocket |

The central design principle is unchanged from the original plan: **a shared gRPC contract
(protobuf schema) is the single integration point between aerogym and aerocoach, and must be
finalised before parallel module development begins.**

---

## System Architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                            AEROTRACK (browser)                               │
│                 Svelte 5 + WebGL Canvas  (Three.js / custom shader)          │
│                         WebSocket JSON client                                │
└────────────────────────────────▲─────────────────────────────────────────────┘
                                 │  WebSocket JSON  (port 8080)
                                 ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│                            AEROCOACH  (ECS task)                             │
│              Rust · axum · tonic (gRPC server) · tokio                       │
│   ┌─────────────────────────────────────────────────────────────────────┐    │
│   │  Load Model Store  │  Slice Clock  │  Agent Registry  │  Delta Eng  │    │
│   └─────────────────────────────────────────────────────────────────────┘    │
└────────────────────────────────▲─────────────────────────────────────────────┘
                                 │  gRPC (bidirectional streaming, port 50051)
          ┌──────────────────────┼──────────────────────┐
          ▼                      ▼                      ▼
  ┌───────────────┐    ┌───────────────┐      ┌───────────────┐
  │  aerogym a00  │    │  aerogym a01  │  …   │  aerogym aN   │
  │  (ECS task)   │    │  (ECS task)   │      │  (ECS task)   │
  └───────┬───────┘    └───────┬───────┘      └───────┬───────┘
          │                    │                      │
          └────────────────────┴──────────────────────┘
                               │  FTP (port 21)
                               ▼
                      ┌────────────────────┐
                      │    AEROFTP SERVER  │
                      └────────────────────┘
```

---

## AWS Deployment Strategy

### Container Lifecycle

Agents and aerocoach are both **ECS tasks** (not services), run on-demand:

- A shell script calls `aws ecs run-task` once for aerocoach, waits for it to reach
  `RUNNING`, then queries its ENI private IP (or uses a stable DNS — see below), and
  finally calls `aws ecs run-task` N times for the agents, passing the aerocoach address
  as `AEROCOACH_URL=grpc://<ip>:50051`.
- This avoids any ECS Service with its mandatory minimum-capacity-1 billing.
- The aerogym Dockerfile is unchanged; agents still pull the FTP server address
  (`AEROSTRESS_TARGET`) from an env var as today.

### Agent Discovery Options (in preference order)

| # | Approach | Complexity | Cost |
|---|---|---|---|
| 1 | **Internal NLB with stable DNS** — aerocoach runs behind an internal Network Load Balancer; DNS name is fixed and baked into the launch script | Low — DNS never changes across runs | Small NLB hourly charge |
| 2 | **AWS Cloud Map (ECS Service Connect)** — aerocoach registers itself in a Cloud Map namespace; agents discover via the injected DNS name `aerocoach.local` | Medium — needs Cloud Map namespace in IaC | Negligible |
| 3 | **Env-var IP (current default)** — launch script captures the ECS task private IP and injects it into agent `run-task` calls | Minimal — same pattern as `AEROSTRESS_TARGET` today | Zero |

**Recommendation:** Start with option 3 (env-var IP) to keep the implementation
self-contained. Option 1 (internal NLB) is the production upgrade once the system proves
out — the DNS name is stable across aerocoach restarts, which avoids script fragility.

---

## Phase 0 — Shared gRPC Contract  *(gate: must complete before parallel work)*

Everything in Phases A, B, and C depends on the generated Rust types from this schema.
Phase 0 produces a new `aeroproto` workspace crate that both `aerogym` and `aerocoach`
depend on.  `aerotrack` uses plain JSON over WebSocket so it has no hard dependency on the
proto compilation, but the JSON shape is derived from the same message structures.

### Phase 0 Deliverables

- [x] Create `aeroproto/` as a new workspace member (library crate, no `main.rs`)
- [x] Write `aeroproto/proto/aeromonitor.proto` (full definition below)
- [x] Write `aeroproto/build.rs` using `tonic-build`
- [x] Add `aeroproto` to workspace `Cargo.toml`
- [x] Verify that `cargo build -p aeroproto` succeeds and generates all expected types
- [x] Commit proto file as the stable v1 contract — subsequent changes go through a version bump

### `aeroproto/` Project Structure

```
aeroproto/
├── Cargo.toml
├── build.rs
└── proto/
    └── aeromonitor.proto
```

**`aeroproto/Cargo.toml`**
```toml
[package]
name = "aeroproto"
version = "0.1.0"
edition = "2024"

[dependencies]
tonic  = "0.12"  # requires prost 0.13 internally
prost  = "0.13"  # must match tonic's prost dependency

[build-dependencies]
tonic-build = "0.12"  # uses prost-build 0.13 under the hood
```

**`aeroproto/build.rs`**
```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(
            &["proto/aeromonitor.proto"],
            &["proto/"],
        )?;
    Ok(())
}
```

### Complete Proto Definition

```protobuf
syntax = "proto3";
package aeromonitor;

// ─── Service definition ────────────────────────────────────────────────────

// Hosted by aerocoach, called by aerogym agents.
service AgentService {
    // Unary: agent registers on startup, receives its load plan and assigned index.
    rpc Register(RegisterRequest) returns (RegisterResponse);

    // Bidirectional stream: maintained for the lifetime of the test.
    //   Agent → aerocoach : AgentReport  (metrics, slice acknowledgements)
    //   aerocoach → agent : CoachCommand (slice ticks, plan updates, shutdown)
    rpc Session(stream AgentReport) returns (stream CoachCommand);
}

// ─── Registration ─────────────────────────────────────────────────────────

message RegisterRequest {
    // Human-readable tag like "a00"–"a99"; supplied via env var AEROGYM_AGENT_ID.
    string agent_id      = 1;
    string agent_version = 2;  // Binary version string for compatibility checks.

    // Populated by the agent from the ECS task metadata endpoint
    // (http://169.254.170.2/v2/metadata) at startup.
    string private_ip   = 3;  // e.g. "10.0.1.23"
    string instance_id  = 4;  // ECS task ARN short ID or EC2 instance ID
}

message RegisterResponse {
    bool   accepted     = 1;
    string reject_reason = 2;  // Non-empty when accepted == false.

    // Index assigned by aerocoach; used for per-agent load share calculation.
    // Range 0 .. (total_agents - 1).
    uint32 agent_index  = 3;

    // Full load plan for this test run.
    LoadPlan load_plan  = 4;
}

// ─── Load plan ────────────────────────────────────────────────────────────

message LoadPlan {
    string plan_id = 1;

    // Wall-clock start time; 0 = start immediately after all agents register.
    int64  start_time_ms     = 2;

    // Duration of each time slice in milliseconds.
    uint64 slice_duration_ms = 3;

    // Ordered list of slices (index 0 = first slice after start).
    repeated TimeSlice slices = 4;

    // File-size histogram that drives test-file generation and task assignment.
    FileSizeDistribution file_distribution = 5;

    // Total aggregate bandwidth ceiling across ALL agents (bytes per second).
    // Each agent's share = total_bandwidth_bps / total_agents.
    uint64 total_bandwidth_bps = 6;

    // Total number of agents expected; used for per-agent share calculation.
    uint32 total_agents = 7;
}

message TimeSlice {
    uint32 slice_index = 1;

    // Target number of CONCURRENT connections across ALL agents at this slice.
    // Each agent's share = ceil(total_connections / total_agents),
    // with the last agent absorbing the remainder.
    uint32 total_connections = 2;
}

message FileSizeDistribution {
    repeated FileSizeBucket buckets = 1;
}

message FileSizeBucket {
    // Short identifier, e.g. "xs", "sm", "md", "lg", "xl", "xxl", "giant"
    string bucket_id      = 1;
    uint64 size_min_bytes = 2;
    uint64 size_max_bytes = 3;

    // Fraction of connections that should use this bucket, 0.0–1.0.
    float  percentage     = 4;
}

// ─── Bidirectional stream messages ────────────────────────────────────────

// Agent → aerocoach
message AgentReport {
    string agent_id    = 1;
    int64  timestamp_ms = 2;

    oneof payload {
        SliceAck      slice_ack      = 3;  // Confirms agent has advanced to a slice.
        MetricsUpdate metrics_update = 4;  // Periodic status snapshot.
    }
}

message SliceAck {
    uint32 slice_index = 1;
}

message MetricsUpdate {
    uint32 current_slice       = 1;
    uint32 active_connections  = 2;
    uint32 queued_connections  = 3;

    // Transfers completed since the previous MetricsUpdate.
    repeated TransferRecord completed_transfers = 4;
}

message TransferRecord {
    string filename            = 1;  // As sent to the FTP server.
    string bucket_id           = 2;  // Which file-size bucket was used.
    uint64 bytes_transferred   = 3;
    uint64 file_size_bytes     = 4;
    uint32 bandwidth_kibps     = 5;  // Average KiB/s for this transfer.
    bool   success             = 6;
    optional string error_reason = 7;
    int64  start_time_ms       = 8;
    int64  end_time_ms         = 9;
    uint32 time_slice          = 10; // Slice in which this transfer was STARTED.
}

// aerocoach → agent
message CoachCommand {
    oneof payload {
        SliceTick      slice_tick   = 1;  // Advance to the next slice.
        LoadPlanUpdate plan_update  = 2;  // Partial or full plan change.
        ShutdownCmd    shutdown     = 3;  // Graceful test termination.
    }
}

message SliceTick {
    uint32 slice_index   = 1;
    int64  wall_clock_ms = 2;  // Expected wall-clock time; allows drift detection.
}

// Sent when the operator changes parameters mid-test.
message LoadPlanUpdate {
    uint32 effective_from_slice = 1;

    // Replaces slices from effective_from_slice onwards; earlier slices unchanged.
    repeated TimeSlice updated_slices = 2;

    // Optional field-level overrides (absent = no change).
    optional uint64              new_bandwidth_bps      = 3;
    optional FileSizeDistribution new_file_distribution = 4;
}

message ShutdownCmd {
    // If true, agents finish in-flight transfers before exiting.
    // If false, agents abort immediately.
    bool graceful = 1;
    string reason = 2;
}

// ─── Dashboard message (JSON-serialised over WebSocket) ───────────────────

// aerocoach serialises this to JSON and broadcasts over the /ws endpoint.
// Corresponds to the DashboardUpdate sent every ~3 seconds to aerotrack.
message DashboardUpdate {
    int64  timestamp_ms  = 1;
    uint32 current_slice = 2;
    uint32 total_slices  = 3;

    repeated AgentSnapshot agents = 4;

    // Delta: only transfers that completed since the last broadcast.
    repeated TransferRecord completed_transfers = 5;

    // Aggregate totals across all agents since test start.
    GlobalStats global_stats = 6;
}

message AgentSnapshot {
    string agent_id          = 1;
    uint32 agent_index        = 2;
    bool   connected          = 3;
    uint32 current_slice      = 4;
    uint32 active_connections = 5;
    uint64 bytes_transferred  = 6;  // Cumulative for this agent.
    uint32 success_count      = 7;
    uint32 error_count        = 8;
    string private_ip         = 9;  // Forwarded from RegisterRequest.
    string instance_id        = 10; // Forwarded from RegisterRequest.
}

message GlobalStats {
    uint64 total_bytes_transferred = 1;
    uint32 total_success           = 2;
    uint32 total_errors            = 3;
    uint32 active_agents           = 4;
    uint32 active_connections      = 5;
    double overall_error_rate      = 6;  // 0.0–1.0
    uint64 current_bandwidth_bps   = 7;  // Measured across all agents.
}
```

---

## Phase A — aerogym (Agent)  *(parallel, starts after Phase 0)*

### A.0 Legacy Migration

The existing `aerostress` binary is preserved as a second binary target inside the same
`aerogym` crate.  No existing behaviour changes.

- [ ] Add `[[bin]]` section to `aerogym/Cargo.toml`:
  ```toml
  [[bin]]
  name = "aerostress"           # legacy binary — unchanged
  path = "src/bin/legacy.rs"

  [[bin]]
  name = "aerogym"              # new agent binary
  path = "src/main.rs"
  ```
- [ ] Move existing `src/main.rs` → `src/bin/legacy.rs` (update module path for `config`)
- [ ] Existing `src/config.rs` stays in place; `legacy.rs` references it with `mod config`
- [ ] Verify `cargo build --bin aerostress` still produces the same binary

### A.1 New Source File Layout

```
aerogym/
├── Cargo.toml                      (updated — adds aeroproto dep, tonic, prost, uuid)
├── src/
│   ├── main.rs                     (new agent entry point)
│   ├── config.rs                   (unchanged — used by legacy)
│   ├── bin/
│   │   └── legacy.rs               (old main.rs, moved here)
│   ├── agent/
│   │   ├── mod.rs
│   │   ├── registration.rs         (Register call → aerocoach, receive LoadPlan)
│   │   ├── session.rs              (bidirectional gRPC stream handler)
│   │   ├── load_plan.rs            (LoadPlan → per-agent task schedule)
│   │   ├── file_manager.rs         (pre-generate bucket files on startup)
│   │   ├── transfer.rs             (FTP upload logic, wraps existing write_async)
│   │   ├── metrics.rs              (in-memory transfer result tracking)
│   │   └── rate_limit.rs           (per-agent bandwidth quota, wraps existing governor logic)
```

### A.2 Startup Sequence

```
1. Read env vars (AEROGYM_AGENT_ID, AEROCOACH_URL, AEROSTRESS_TARGET, ...)
2. Connect gRPC channel to aerocoach
3. Call Register() → receive LoadPlan + agent_index
4. Pre-generate one file per FileSizeBucket (random size within bucket range)
   - filename format: bucket_<agent_id>_<bucket_id>.dat
5. Open bidirectional Session() stream
6. Enter wait loop: block until SliceTick(slice=0) arrives from aerocoach
7. Execute slice loop (see A.3)
8. On ShutdownCmd: finish in-flight transfers, send final MetricsUpdate, exit
```

### A.3 Per-Slice Execution Logic

```rust
// Pseudo-code for the slice execution loop

let mut active_tasks: JoinSet<TransferResult> = JoinSet::new();
let mut current_slice = 0u32;

loop {
    // Wait for SliceTick from aerocoach
    let tick = coach_rx.recv().await?;
    current_slice = tick.slice_index;

    // How many connections should this agent be running right now?
    let slice_spec = plan.slice_for(current_slice);
    let my_target   = per_agent_connections(slice_spec.total_connections,
                                            agent_index,
                                            total_agents);

    // Count already-running tasks (carry-overs from previous slice)
    let running = active_tasks.len() as u32;

    if my_target > running {
        // Ramp UP: start additional transfers
        let to_start = my_target - running;
        for _ in 0..to_start {
            let bucket = weighted_random_bucket(&plan.file_distribution);
            let filename = make_filename(agent_id, current_slice, next_conn_id());
            let rate_params = rate_params_for_agent(plan.total_bandwidth_bps, total_agents);
            active_tasks.spawn(run_transfer(filename, bucket_file(&bucket), rate_params));
        }
    }
    // Ramp DOWN: do NOT abort running tasks — let them finish naturally.
    // The deficit resolves organically as in-progress transfers complete.

    // Acknowledge the slice to aerocoach
    coach_tx.send(AgentReport { payload: SliceAck { slice_index: current_slice } }).await?;

    // Drain completed tasks and build MetricsUpdate
    let mut completed = vec![];
    while let Some(result) = active_tasks.try_join_next() {
        completed.push(result?.into_transfer_record(current_slice));
    }
    if !completed.is_empty() {
        coach_tx.send(AgentReport {
            payload: MetricsUpdate { current_slice, active_connections: active_tasks.len() as u32, completed_transfers: completed, .. }
        }).await?;
    }
}
```

### A.4 File Naming Convention

```
Bucket files  (on-disk, reused):
  bucket_<agent_id>_<bucket_id>.dat
  e.g.  bucket_a03_sm.dat

Transfer remote names (unique per transfer, avoids FTP conflicts):
  <agent_id>_s<slice_idx>_c<conn_id>_<seq>.dat
  e.g.  a03_s007_c0042_1.dat
```

### A.5 Rate Limiting per Agent

Each agent receives `total_bandwidth_bps / total_agents` as its bandwidth ceiling.
The existing `governor`-based rate limiter in `write_async` is reused.
The `RateLimiterConfig` is rebuilt at each slice if a `LoadPlanUpdate` changes the
bandwidth parameter.

### A.6 Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `AEROGYM_AGENT_ID` | **Yes** | — | Agent identifier, e.g. `a00`–`a99` |
| `AEROCOACH_URL` | **Yes** | — | gRPC endpoint, e.g. `grpc://10.0.1.5:50051` |
| `AEROSTRESS_TARGET` | **Yes** | — | FTP server `host:port` |
| `AEROGYM_WORK_DIR` | No | `/tmp/aerogym` | Directory for bucket files |

### A.7 New Dependencies for `aerogym/Cargo.toml`

```toml
aeroproto = { path = "../aeroproto" }
tonic     = "0.12"
prost     = "0.13"
uuid      = { version = "1", features = ["v4"] }
```

### A.8 Task Checklist — aerogym

- [x] **A.0** Migrate legacy binary (`aerostress`) to `src/bin/legacy.rs`; verify CI passes
- [x] **A.1** Scaffold `src/agent/` module tree and `main.rs` entry point
- [x] **A.2** Implement `registration.rs`: connect to aerocoach, call `Register`, deserialise `LoadPlan`
- [x] **A.3** Implement `file_manager.rs`: create one file per bucket on startup
- [x] **A.4** Implement `load_plan.rs`: calculate per-agent connection counts and file assignments per slice
- [x] **A.5** Implement `session.rs`: open `Session` stream; handle incoming `CoachCommand` variants
- [x] **A.6** Implement slice execution loop: ramp up / carry-over logic with `JoinSet`
- [x] **A.7** Implement `transfer.rs`: wrap existing `write_async`, capture timing + bandwidth for `TransferRecord`
- [x] **A.8** Implement `rate_limit.rs`: derive per-agent bandwidth quota from load plan
- [x] **A.9** Implement `metrics.rs`: accumulate `TransferRecord`s; send `MetricsUpdate` after each completed batch
- [x] **A.10** Handle `LoadPlanUpdate`: rebuild rate limiter and slice schedule from next slice onward
- [x] **A.11** Handle `ShutdownCmd`: graceful drain of `JoinSet` before exit
- [x] **A.12** Add structured logging (tracing crate) for all slice transitions and transfer outcomes
- [x] **A.13** Integration test: single agent against local aeroftp, mock aerocoach (tonic test server)

---

## Phase B — aerocoach (Controller + Aggregator)  *(parallel, starts after Phase 0)*

### B.1 Project Structure

```
aerocoach/
├── Cargo.toml
├── proto -> ../aeroproto/proto    (symlink or copy — build.rs points to aeroproto)
├── src/
│   ├── main.rs                    (entry point, server bootstrap)
│   ├── config.rs                  (env var config)
│   ├── grpc/
│   │   ├── mod.rs
│   │   └── agent_service.rs       (tonic AgentService implementation)
│   ├── model/
│   │   ├── mod.rs
│   │   ├── load_plan.rs           (LoadPlan construction and validation)
│   │   ├── distributor.rs         (per-agent share calculation)
│   │   └── clock.rs               (slice tick scheduler)
│   ├── state/
│   │   ├── mod.rs
│   │   ├── registry.rs            (connected agent tracking)
│   │   ├── metrics_store.rs       (accumulates TransferRecords from all agents)
│   │   └── delta.rs               (computes DashboardUpdate deltas for WS broadcast)
│   └── ws/
│       ├── mod.rs
│       └── broadcaster.rs         (axum WebSocket + broadcast channel)
```

### B.2 Startup Sequence

```
1. Parse config (env vars, optional JSON load-plan file via AEROCOACH_PLAN_FILE)
2. Start gRPC server on 0.0.0.0:50051  (tonic)
3. Start WebSocket + HTTP server on 0.0.0.0:8080  (axum)
4. Enter WAITING state: accept agent registrations, wait for operator to press Start
   - Agents can connect and register while in WAITING state
   - The load plan can still be replaced via PUT /plan in this state
   - Test does NOT auto-start; operator must explicitly POST /start
5. Enter RUNNING state: start slice clock, broadcast SliceTick to all agents every slice_duration_ms
6. Collect metrics, compute deltas, broadcast DashboardUpdate to WebSocket clients every 3s
7. On all slices complete OR /stop command: send ShutdownCmd, drain final metrics, enter DONE state
8. Write final NDJSON record file to AEROCOACH_RECORD_DIR (default /data/records/)
```

### B.3 Load Plan Input Format

The load plan is loaded from a JSON file (path from `AEROCOACH_PLAN_FILE`).  It is
designed to be hand-editable with any text editor (`vi`, `nano`, etc.) and can be
replaced at runtime via `HTTP PUT /plan` while aerocoach is in WAITING state.  aerotrack
reads the current plan from `GET /plan` and renders it as a step graph so the operator
can visually verify the connection profile before pressing Start.

```json
{
  "plan_id": "test-2026-04-22",
  "slice_duration_ms": 60000,
  "total_bandwidth_bps": 104857600,
  "file_distribution": {
    "buckets": [
      { "bucket_id": "xs",    "size_min_bytes":        0, "size_max_bytes":  10485760, "percentage": 0.580 },
      { "bucket_id": "sm",    "size_min_bytes": 10485760, "size_max_bytes":  52428800, "percentage": 0.129 },
      { "bucket_id": "md",    "size_min_bytes": 52428800, "size_max_bytes": 104857600, "percentage": 0.087 },
      { "bucket_id": "lg",    "size_min_bytes": 104857600,"size_max_bytes": 209715200, "percentage": 0.063 },
      { "bucket_id": "xl",    "size_min_bytes": 209715200,"size_max_bytes": 524288000, "percentage": 0.052 },
      { "bucket_id": "xxl",   "size_min_bytes": 524288000,"size_max_bytes":1073741824, "percentage": 0.040 },
      { "bucket_id": "giant", "size_min_bytes":1073741824,"size_max_bytes":2147483648, "percentage": 0.049 }
    ]
  },
  "slices": [
    { "slice_index": 0, "total_connections": 50  },
    { "slice_index": 1, "total_connections": 120 },
    { "slice_index": 2, "total_connections": 280 },
    { "slice_index": 3, "total_connections": 280 },
    { "slice_index": 4, "total_connections": 150 },
    { "slice_index": 5, "total_connections": 50  }
  ]
}
```

### B.4 Slice Clock & Synchronisation

The slice clock is the master timekeeper.  It fires every `slice_duration_ms`.

```rust
// Pseudo-code

let mut slice_index = 0u32;
let mut clock_interval = tokio::time::interval(Duration::from_millis(plan.slice_duration_ms));

loop {
    clock_interval.tick().await;

    // Broadcast SliceTick to all connected agents
    let tick = CoachCommand { payload: SliceTick { slice_index, wall_clock_ms: now_ms() } };
    registry.broadcast(tick).await;

    // Wait for SliceAck from every registered agent (with 5-second timeout)
    registry.wait_for_acks(slice_index, Duration::from_secs(5)).await;

    // Log any agents that missed the deadline
    slice_index += 1;

    if slice_index >= plan.total_slices() {
        registry.broadcast(CoachCommand::shutdown(graceful: true)).await;
        break;
    }
}
```

**Ack timeout strategy:** Agents that miss the ack deadline within 5 s are flagged as
lagging in the `AgentSnapshot` but are not disconnected.  The clock advances regardless —
this prevents one slow agent from halting the entire test.

### B.5 Per-Agent Share Calculation

```rust
/// Returns the target concurrent connection count for a specific agent.
fn per_agent_connections(total: u32, agent_index: u32, total_agents: u32) -> u32 {
    let base = total / total_agents;
    let remainder = total % total_agents;
    // Distribute remainder to the first `remainder` agents (by index).
    base + if agent_index < remainder { 1 } else { 0 }
}

/// Returns the bandwidth ceiling in bytes/sec for one agent.
fn per_agent_bandwidth(total_bps: u64, total_agents: u32) -> u64 {
    total_bps / total_agents as u64
}
```

### B.6 Delta Engine for Dashboard

The delta engine keeps a snapshot of the last broadcast state and only includes changes
in each `DashboardUpdate`, minimising WebSocket payload size.

```rust
struct DeltaEngine {
    last_global: GlobalStats,
    last_agents: HashMap<String, AgentSnapshot>,
}

impl DeltaEngine {
    fn compute(&mut self, current: &MetricsStore) -> DashboardUpdate {
        // Build fresh AgentSnapshot list from MetricsStore
        // Build GlobalStats
        // Collect transfers completed since last compute()
        // Update last_* for next call
        DashboardUpdate { ... }
    }
}
```

### B.7 HTTP Control Endpoints

Served by axum alongside the WebSocket endpoint:

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Liveness probe |
| `GET` | `/status` | JSON: current state (WAITING / RUNNING / DONE), agent count, current slice |
| `GET` | `/plan` | Return the active load plan as JSON (for aerotrack step-graph render) |
| `PUT` | `/plan` | Replace the full load plan — only accepted while in WAITING state |
| `PATCH` | `/plan` | Partial update from `effective_from_slice` onwards — accepted in WAITING or RUNNING state; broadcasts `LoadPlanUpdate` to all agents |
| `POST` | `/start` | **Manually trigger test start** — only accepted while in WAITING state |
| `POST` | `/stop` | Send graceful ShutdownCmd to all agents |
| `POST` | `/bandwidth` | Hot-update total bandwidth; issues `LoadPlanUpdate` to all agents |
| `GET` | `/results` | Stream the NDJSON record file as a download (available in DONE state) |
| `GET` | `/ws` | WebSocket upgrade for aerotrack |

**Record file format** — after a test completes, aerocoach writes one JSON object per
line (NDJSON) to `<AEROCOACH_RECORD_DIR>/<plan_id>_<timestamp>.ndjson`.  Each line is a
`TransferRecord` augmented with the `agent_id` field:

```jsonc
// One line per completed transfer
{"agent_id":"a00","filename":"a00_s002_c0017_1.dat","bucket_id":"sm","bytes_transferred":31457280,"file_size_bytes":31457280,"bandwidth_kibps":4096,"success":true,"start_time_ms":1714000200000,"end_time_ms":1714000207680,"time_slice":2}
{"agent_id":"a01","filename":"a01_s002_c0005_1.dat","bucket_id":"xs","bytes_transferred":5242880,"file_size_bytes":5242880,"bandwidth_kibps":4096,"success":false,"error_reason":"550 Permission denied","start_time_ms":1714000201100,"end_time_ms":1714000201900,"time_slice":2}
```

The `GET /results` endpoint serves this file with `Content-Disposition: attachment` so
aerotrack's Download button triggers a browser file-save.

### B.8 Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `AEROCOACH_GRPC_PORT` | No | `50051` | gRPC listen port |
| `AEROCOACH_HTTP_PORT` | No | `8080` | WebSocket + HTTP listen port |
| `AEROCOACH_PLAN_FILE` | No | — | Path to JSON load plan; can also be replaced at runtime via PUT /plan |
| `AEROCOACH_RECORD_DIR` | No | `/data/records` | Directory for NDJSON result files |
| `RUST_LOG` | No | `info` | Log filter |

### B.9 Task Checklist — aerocoach

- [x] **B.0** Initialise `aerocoach/` as a new workspace member; add to root `Cargo.toml`
- [x] **B.1** Implement `config.rs`: env vars, optional load-plan JSON file loading
- [x] **B.2** Implement `model/load_plan.rs`: deserialise JSON plan, validate bucket percentages sum to 1.0
- [x] **B.3** Implement `model/distributor.rs`: per-agent share calculation helpers
- [x] **B.4** Implement `state/registry.rs`: agent registration, session tracking, broadcast helpers
- [x] **B.5** Implement `grpc/agent_service.rs`: `Register` + `Session` tonic handlers
- [x] **B.6** Implement `model/clock.rs`: slice tick scheduler; ack wait + timeout logic
- [ ] **B.7** Implement `state/metrics_store.rs`: accumulate `TransferRecord`s; derive `AgentSnapshot`s
- [ ] **B.8** Implement `state/delta.rs`: delta engine; produce `DashboardUpdate`
- [ ] **B.9** Implement `ws/broadcaster.rs`: axum WebSocket handler + broadcast channel
- [x] **B.10** Wire up `main.rs`: gRPC server + axum server running concurrently via `tokio::select!`
- [x] **B.11** Implement HTTP control endpoints (`/health`, `/status`, `/start`, `/stop`) — `/plan` GET+PUT and `/bandwidth` deferred to next session
- [ ] **B.12** Implement hot-reload: `POST /bandwidth` and `PATCH /plan` issue `LoadPlanUpdate` to all agents; `PUT /plan` rejected if state is not WAITING; `PATCH /plan` accepted in WAITING or RUNNING
- [ ] **B.13a** Implement NDJSON record writer: open file on test start, append each `TransferRecord` + `agent_id` as it arrives, flush and close on DONE
- [ ] **B.13b** Implement `GET /results`: stream the record file with `Content-Disposition: attachment; filename=<plan_id>_<ts>.ndjson`
- [ ] **B.14** Add structured tracing + optional JSON log output for production deployments
- [ ] **B.15** Unit tests: per-agent share math, delta engine idempotency, bucket validation
- [ ] **B.16** Integration test: two mock agents, verify slice synchronisation and metrics aggregation

---

## Phase C — aerotrack (Frontend Dashboard)  *(parallel, starts after Phase 0)*

### C.1 Project Structure

```
aerotrack/
├── package.json
├── svelte.config.js
├── vite.config.js
├── src/
│   ├── app.html
│   ├── routes/
│   │   └── +page.svelte             (single page — all states: WAITING / RUNNING / DONE)
│   ├── lib/
│   │   ├── components/
│   │   │   ├── DashboardLayout.svelte   (top/bottom split with divider line)
│   │   │   ├── PlanPanel.svelte          (top-left: step graph, live/edit mode toggle)
│   │   │   ├── GlobalStats.svelte        (top-right: 4-card stat panel)
│   │   │   ├── AgentGrid.svelte          (bottom: 10 × 10 slot grid)
│   │   │   ├── AgentBox.svelte           (single agent cell)
│   │   │   ├── ControlBar.svelte         (Start / Stop / Download; edit-mode trigger)
│   │   │   ├── Legend.svelte             (error-rate colour key)
│   │   │   └── TransferTooltip.svelte    (future: agent drill-down)
│   │   ├── stores/
│   │   │   ├── dashboard.svelte.ts       (Svelte 5 runes-based reactive state)
│   │   │   ├── plan.svelte.ts            (load plan fetch, draft editing state)
│   │   │   └── websocket.ts              (WebSocket client, reconnect logic)
│   │   └── utils/
│   │       ├── colors.ts                 (error-rate colour palette)
│   │       ├── format.ts                 (bytes/bandwidth auto-scale helpers)
│   │       └── layout.ts                 (agent_index → grid row/col)
│   └── styles/
│       └── global.css
```

### C.2 WebSocket Message Shape (JSON)

`DashboardUpdate` (from the proto definition) is serialised to JSON by aerocoach:

```typescript
interface TransferRecord {
  filename: string;
  bucket_id: string;
  bytes_transferred: number;
  file_size_bytes: number;
  bandwidth_kibps: number;
  success: boolean;
  error_reason?: string;
  start_time_ms: number;
  end_time_ms: number;
  time_slice: number;
}

interface AgentSnapshot {
  agent_id: string;
  agent_index: number;
  connected: boolean;
  current_slice: number;
  active_connections: number;
  bytes_transferred: number;
  success_count: number;
  error_count: number;
}

interface GlobalStats {
  total_bytes_transferred: number;
  total_success: number;
  total_errors: number;
  active_agents: number;
  active_connections: number;
  overall_error_rate: number;
  current_bandwidth_bps: number;
}

interface DashboardUpdate {
  timestamp_ms: number;
  current_slice: number;
  total_slices: number;
  agents: AgentSnapshot[];
  completed_transfers: TransferRecord[];
  global_stats: GlobalStats;
}
```

### C.3 Reactive State Store (Svelte 5 Runes)

```typescript
// src/lib/stores/dashboard.svelte.ts

export class DashboardStore {
  // Reactive state (Svelte 5 runes)
  agents = $state<Map<string, AgentSnapshot>>(new Map());
  globalStats = $state<GlobalStats | null>(null);
  currentSlice = $state(0);
  totalSlices = $state(0);

  // Rolling window of recent transfers for canvas rendering
  // Keyed by filename; each entry holds the latest known state.
  transfers = $state<Map<string, TransferRecord>>(new Map());

  applyUpdate(update: DashboardUpdate) {
    this.currentSlice = update.current_slice;
    this.totalSlices = update.total_slices;
    this.globalStats = update.global_stats;

    for (const agent of update.agents) {
      this.agents.set(agent.agent_id, agent);
    }
    // Trigger Svelte reactivity
    this.agents = new Map(this.agents);

    for (const t of update.completed_transfers) {
      this.transfers.set(t.filename, t);
    }
    this.transfers = new Map(this.transfers);
  }
}

export const dashboard = new DashboardStore();
```

### C.4 Dashboard Layout & Visualisation Design

The live dashboard (`routes/+page.svelte`) is split into two horizontal bands separated
by a divider line:

```
┌─────────────────────────────────────────────────────────────────────┐  ─┐
│  LOAD PLAN TIMELINE          │  GLOBAL STATS                        │   │
│                              │                                       │  ~33%
│  step graph (slices × conns) │  files · bytes · errors · bandwidth  │   │
│  current slice highlighted   │                                       │   │
├──────────────────────────────┴───────────────────────────────────────┤  ─┤
│                        AGENT GRID  (10 × 10)                         │   │
│                                                                      │  ~67%
│  [ a00 ]  [ a01 ]  [ a02 ]  …  [ a09 ]                              │   │
│  [ a10 ]  [ a11 ]  …                                                 │   │
│  …                                                                   │   │
│  [ dimmed inactive slots ]                                           │   │
└──────────────────────────────────────────────────────────────────────┘  ─┘
```

---

#### Top-left — Plan Panel (`PlanPanel.svelte`)

An SVG step graph that mirrors the load plan:

- **X-axis**: slice indices, labelled as wall-clock minutes from test start
  (e.g. `0 min`, `1 min`, `2 min` … for 60 s slices)
- **Y-axis**: total concurrent connections, auto-scaled to plan maximum
- **Step line**: each slice is a horizontal segment at its connection count;
  vertical risers connect adjacent steps
- **Completed slices**: line segment drawn in a muted / filled colour
- **Current slice indicator**: a filled circle on the step line at the current
  slice x-position; the active step segment is drawn thicker and in the accent colour
- **Future slices**: normal weight, lighter colour
- The graph is read-only in the live view (editing happens in the setup view)

#### Top-right — Global Stats (`GlobalStats.svelte`)

Four stat cards arranged in a 2 × 2 grid, fed from `DashboardUpdate.global_stats`:

| Card | Value | Unit |
|---|---|---|
| Files transferred | `total_success` | count |
| Bytes transferred | `total_bytes_transferred` | auto-scaled (MB / GB) |
| Error rate | `overall_error_rate × 100` | % |
| Current bandwidth | `current_bandwidth_bps` | auto-scaled (Mbit/s) |

All four values update on every `DashboardUpdate` (every ~3 s).

---

#### Bottom — Agent Grid (`AgentGrid.svelte` + `AgentBox.svelte`)

A fixed 10 × 10 grid of agent slots.  Slot position is determined solely by
`agent_index` (0–99): row = `agent_index ÷ 10`, column = `agent_index mod 10`.

**Inactive slot** (no agent registered at that index):
- Dimmed background, no text, no icon
- Renders as an empty grey rectangle to preserve grid geometry

**Active slot** (`AgentBox`):

```
┌──────────────────────────────────┐
│ 🐇  a03           slice 4 / 6   │  ← agent id + pace icon + slice badge
│ 10.0.1.23  i-0abc1234def56789   │  ← private IP + ECS task / instance id
├──────────────────────────────────┤
│  ██ 47 transfers   ▲ 2.1 GB     │  ← totals (normal size)
│                                  │
│   32 running    1.4 % err        │  ← prominent stats (large / bold)
└──────────────────────────────────┘
```

**Fields displayed in each active `AgentBox`:**

| Field | Source | Style |
|---|---|---|
| Agent ID (`a00`–`a99`) | `agent_id` | header, medium |
| Current slice / total | `current_slice` / `total_slices` | header, right-aligned |
| Private IP | `AgentSnapshot.private_ip` | subheader, small |
| ECS task / instance ID | from `AgentSnapshot` | subheader, small |
| Total files transferred | `success_count` | normal |
| Total bytes transferred | `bytes_transferred` (auto-scaled) | normal |
| **Currently running transfers** | `active_connections` | **large, bold** |
| **Cumulative error rate** | `error_count / (success_count + error_count)` | **large, bold** |
| Pace icon | derived (see below) | top-left corner |

**Pace icon logic:**

```typescript
// An agent is "lagging" if its acknowledged slice is behind the master clock
// by more than one slice.
function paceIcon(agentSlice: number, masterSlice: number): '🐇' | '🐢' {
  return agentSlice >= masterSlice - 1 ? '🐇' : '🐢';
}
```

**Active agent box colour:**  a single accent colour for all active agents (e.g. a
dark-blue card with white text); the error rate value changes colour independently
using the same error-rate palette from the colour table below.

**Error rate colour for the rate value itself:**

| Error Rate | Colour | Hex |
|---|---|---|
| 0 % | `#38CC60` | green |
| >0 % to <1 % | `#FDCB4D` | yellow |
| ≥1 % to ≤3 % | `#FC8D59` | orange |
| >3 % | `#F4726A` | red |

> `private_ip` and `instance_id` are included in both `RegisterRequest` (fields 3–4)
> and `AgentSnapshot` (fields 9–10) in the Phase 0 proto definition above.
> Agents populate them from the ECS task metadata endpoint
> (`http://169.254.170.2/v2/metadata`) at startup.

---

#### Transfer-level canvas (deferred)

The original per-transfer coloured-rectangle WebGL canvas from the v1 plan is **not**
part of the v1 live view.  It may be added later as a drill-down panel that opens when
the operator clicks an individual `AgentBox`.  The colour palette and progress-fill
shader design are preserved in the `colors.ts` utility for that future use.

### C.5 `PlanPanel.svelte` — Unified Plan View

`PlanPanel.svelte` occupies the top-left quadrant permanently.  It renders the same SVG
step graph in all states, switching between two internal modes:

#### Live mode (default while RUNNING / DONE, and initial view while WAITING)

- SVG step graph: x-axis = slice indices labelled as wall-clock minutes, y-axis =
  total connections, y-axis auto-scales to plan maximum
- Each slice is a horizontal segment at its connection count; vertical risers connect
  adjacent steps
- **Completed segments**: muted, filled colour
- **Current slice indicator**: filled circle on the step line at the active x-position;
  active segment drawn thicker in the accent colour
- **Future segments**: normal weight, reduced opacity
- Hover tooltip per step: slice index, connection count, time window
- **Edit Plan** button (pencil icon, top-right corner of the panel) — switches to edit
  mode; visible at all times so the operator can adjust mid-test without navigating away
- **Reload** button — calls `GET /plan` to pull the latest file-backed plan from
  aerocoach (useful after editing the JSON file directly)

#### Edit mode (toggled by Edit Plan button)

- Same SVG step graph, but rendered as a **draft** of the in-progress edits:
  - Steps that have been modified are highlighted (e.g. dashed border or different fill)
  - Past slices (index < `currentSlice`) are locked and shown greyed out; only present
    and future slices can be modified
- **Per-slice controls**: each step bar has a small `▲` / `▼` button pair (or
  click-and-drag on the bar) to increment / decrement the connection count for that slice
- **Bandwidth field**: a number input (Mbit/s) at the top of the panel, pre-filled with
  the current plan bandwidth; editing it updates the draft
- The graph preview updates in real time as the operator adjusts values — typos are
  immediately visible as disproportionate bars
- **Apply** button:
  1. Validates the draft (no negative values, percentages still valid)
  2. Calls `PATCH /plan` with `{ effective_from_slice, updated_slices, new_bandwidth_bps }`
  3. On success: updates the local plan store, switches back to live mode
  4. On failure: shows inline error, stays in edit mode
- **Cancel** button: discards all draft changes, switches back to live mode with no
  network request

#### Draft state in `plan.svelte.ts`

```typescript
class PlanStore {
  // Committed plan (last successfully applied or fetched)
  committed = $state<LoadPlan | null>(null);

  // Draft is a deep copy of committed, mutated by the editor.
  // null when not in edit mode.
  draft = $state<LoadPlan | null>(null);

  enterEditMode() {
    this.draft = structuredClone(this.committed);
  }

  cancelEdit() {
    this.draft = null;
  }

  async applyEdit(currentSlice: number): Promise<void> {
    const updatedSlices = this.draft!.slices.filter(
      s => s.slice_index >= currentSlice
    );
    await fetch('/api/plan', {
      method: 'PATCH',
      body: JSON.stringify({
        effective_from_slice: currentSlice,
        updated_slices: updatedSlices,
        new_bandwidth_bps: this.draft!.total_bandwidth_bps,
      }),
    });
    this.committed = this.draft;
    this.draft = null;
  }
}
```

### C.6 — Note on superseded components

`LoadPlanGraph.svelte` and `LoadPlanTimeline.svelte` from earlier drafts are replaced
by the single `PlanPanel.svelte` component.  The separate `/setup` route is no longer
needed; the whole application is a single page.

### C.7 Environment Variables

| Variable | Required | Default | Description |
|---|---|---|---|
| `PUBLIC_WS_URL` | No | `ws://localhost:8080/ws` | aerocoach WebSocket endpoint |

### C.8 Task Checklist — aerotrack

**Foundation**
- [ ] **C.0** Scaffold SvelteKit project (`npm create svelte@latest aerotrack`); configure Vite
- [ ] **C.1** Implement `websocket.ts`: connect to aerocoach `/ws`, auto-reconnect with exponential backoff, parse `DashboardUpdate` JSON
- [ ] **C.2** Implement `dashboard.svelte.ts`: reactive store with `applyUpdate`; derive `masterSlice` and per-agent pace flags
- [ ] **C.3** Implement `plan.svelte.ts`: `committed` + `draft` state, `enterEditMode` / `cancelEdit` / `applyEdit(currentSlice)` helpers; `reload()` fetches `GET /plan`
- [ ] **C.4** Implement `colors.ts`: error-rate → RGB colour function (in-progress and completed variants)
- [ ] **C.5** Implement `format.ts`: bytes auto-scale (B / KB / MB / GB), bandwidth auto-scale (bit/s → Mbit/s)
- [ ] **C.6** Implement `layout.ts`: `agent_index` → `{ row, col }` (row = index ÷ 10, col = index mod 10)

**PlanPanel (top-left)**
- [ ] **C.7** Implement `PlanPanel.svelte` live mode: SVG step graph, completed/active/future segment styles, filled-circle current-slice indicator, hover tooltips, Edit Plan + Reload buttons
- [ ] **C.8** Implement `PlanPanel.svelte` edit mode: past-slice locking, per-slice ▲/▼ controls with real-time graph preview, bandwidth input field, Apply (calls `PATCH /plan` then reverts to live mode) and Cancel buttons; inline error display on Apply failure

**Dashboard components**
- [ ] **C.9** Implement `GlobalStats.svelte`: 2×2 card grid — files, bytes, error rate, bandwidth; updates on each `DashboardUpdate`
- [ ] **C.10** Implement `AgentBox.svelte`: active state (IP, instance-id, slice badge, total files, total bytes, **running transfers bold-large**, **error rate bold-large with colour**, rabbit/turtle icon); inactive state (dimmed placeholder)
- [ ] **C.11** Implement `AgentGrid.svelte`: 10×10 CSS grid; map `agents` store to slot positions via `layout.ts`; render `AgentBox` for each slot
- [ ] **C.12** Implement `ControlBar.svelte`: state badge (WAITING / RUNNING / DONE), connected agent count, Start / Stop / Download Results buttons; **Edit Plan** button that calls `plan.enterEditMode()` and is visible at all times
- [ ] **C.13** Implement `DashboardLayout.svelte`: top band (~33 vh) split left/right with `PlanPanel` and `GlobalStats`; horizontal divider line; bottom band (~67 vh) with `AgentGrid`
- [ ] **C.14** Implement `Legend.svelte`: error-rate colour key panel
- [ ] **C.15** Implement `+page.svelte`: compose `DashboardLayout` + `ControlBar` + `Legend`
- [ ] **C.16** Add ECS metadata fetch in agent registration: agents query `http://169.254.170.2/v2/metadata` for private IP and task ARN short ID; populate `RegisterRequest.private_ip` and `RegisterRequest.instance_id`

**Testing**
- [ ] **C.17** Test with mock WebSocket server replaying a recorded `DashboardUpdate` stream; verify pace icons switch correctly
- [ ] **C.18** Test `PlanPanel` edit mode: verify past slices are locked, draft preview updates in real time, Apply sends correct `PATCH /plan` payload, Cancel restores committed state
- [ ] **C.19** Test Download Results: verify `GET /results` triggers browser file-save with correct NDJSON content
- [ ] **C.20** *(Future)* Agent drill-down: clicking an `AgentBox` opens a panel with per-transfer coloured rectangles (`TransferTooltip.svelte` + canvas renderer)

---

## Implementation Sequence & Parallel Execution Guide

```
Timeline ─────────────────────────────────────────────────────────────────────►

 PHASE 0  │ aeroproto: proto definition, tonic-build, workspace wiring
  (gate)  │ ─────────────────────────── ✓ MERGE ──────────────────────────────
          │
          ├── PHASE A ────────────────────────────────────────────────────────►
          │   Agent session A: aerogym
          │   (A.0 legacy migration → A.1–A.9 new agent binary → A.13 test)
          │
          ├── PHASE B ────────────────────────────────────────────────────────►
          │   Agent session B: aerocoach
          │   (B.0 scaffold → B.1–B.11 full controller → B.15 integration test)
          │
          └── PHASE C ────────────────────────────────────────────────────────►
              Agent session C: aerotrack
              (C.0 scaffold → C.1–C.11 dashboard → C.13 mock test)

 INTEGRATION PHASE (after A, B, C all merge):
   - Deploy local Docker Compose stack (aerocoach + 2× aerogym + aeroftp)
   - Run end-to-end test with a 6-slice load plan
   - Verify slice lock-step across agents
   - Verify aerotrack renders live updates
   - AWS ECS smoke test with launch script
```

### Contracts between parallel phases

Each parallel session must not change the proto definition.  If a change is needed:

1. Open a PR targeting Phase 0 with the proto change.
2. All three sessions rebase on that PR before continuing.

The JSON shape of `DashboardUpdate` (Phase B output, Phase C input) is documented in C.2
above and must be treated as a frozen API between B and C.

---

## Integration & Testing Phase

- [ ] Write `docker-compose.yml` with services: `aeroftp`, `aerocoach`, `aerogym-a00`, `aerogym-a01`, `aerotrack`
- [ ] Run 6-slice test plan; confirm slice sync within ±500 ms across agents
- [ ] Confirm bandwidth ceiling is respected within ±10 % margin across the fleet
- [ ] Confirm error-rate colour changes in aerotrack when FTP errors are injected
- [ ] Test mid-run bandwidth change via `POST /bandwidth`; confirm agents adapt
- [ ] Test graceful shutdown via `POST /stop`; confirm no partial transfer records lost
- [ ] Simulate agent disconnect mid-test; confirm aerocoach continues, aerotrack marks agent as offline
- [ ] AWS ECS launch script: start aerocoach, capture private IP, start 5 agents, observe dashboard
- [ ] Load test: 20 agents × 50 peak connections = 1 000 concurrent FTP transfers; measure aerocoach CPU

---

## Performance Considerations

### gRPC

- Use `tonic` with HTTP/2 keepalive to detect dead agent connections quickly
- `MetricsUpdate` is only sent when transfers complete (not polled) — low traffic at stable load
- `LoadPlanUpdate` is rare (operator-triggered); no need for compression optimisation there

### WebSocket to aerotrack

- Broadcast every 3 seconds; `DashboardUpdate` only includes transfers completed since last broadcast
- At 1 000 transfers/3 s, each `TransferRecord` ≈ 150 bytes JSON → ~150 KB/broadcast — acceptable
- If payloads exceed 500 KB, enable gzip compression on the axum WebSocket handler

### Browser rendering

- Canvas 2D batch-by-colour-group is efficient up to ~10 000 rects at 60 fps
- Three.js `InstancedMesh` upgrade (task C.12) handles 100 000+ instances

---

## Open Questions / Decisions for Review

*Resolved items are marked ✅.*

1. ✅ **Test start trigger** — manual `POST /start` only; no auto-start.  The Start button
   will be wired into aerotrack's `ControlBar` in Phase C.

2. ✅ **Historical storage** — aerocoach writes an NDJSON record file on test completion;
   aerotrack exposes a Download Results button backed by `GET /results`.

3. ✅ **Load plan editor** — vi-editable JSON file is the primary authoring tool;
   aerotrack provides a read-only step-graph visualisation (`LoadPlanGraph`) plus a
   Reload Plan button for the verify-before-start workflow.

4. ✅ **Agent discovery** — env-var IP approach kept (`AEROCOACH_URL`), same pattern as
   `AEROSTRESS_TARGET` today.

5. **Authentication**: Both the gRPC endpoint and the WebSocket/HTTP endpoints are
   unauthenticated.  For a private VPC this is acceptable; flag for review if the
   dashboard is ever exposed outside the VPC.

6. ✅ **Maximum agent count** — 100 agents (`a00`–`a99`) is sufficient.  The ID scheme
   is fixed at two decimal digits.

7. ✅ **Slice duration granularity** — all slices share the same duration, defined once
   as `slice_duration_ms` at the top level of the plan.  Per-slice duration overrides
   are explicitly out of scope.
