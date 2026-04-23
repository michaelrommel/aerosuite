# Session Handoff 3 — aerotrack Frontend + Remaining aerocoach B-tasks

**Date:** 2026-04-23  
**Workspace root:** `/home/rommel/software/aerosuite`  
**Next goal:** Build the `aerotrack` Svelte 5 frontend dashboard (Phase C) and complete
the remaining aerocoach backend tasks that feed it (B.7–B.13).

---

## 1 — What was built this session (sessions 1–3)

### Phase 0 ✅ — aeroproto
Shared gRPC contract in `aeroproto/`. Proto file is frozen v1.  
Key file: `aeroproto/proto/aeromonitor.proto`

### Phase A ✅ — aerogym agent
All 13 tasks done. The agent binary at `aerogym/src/main.rs`:

1. Reads env vars (see Section 5)
2. Registers with aerocoach via gRPC `Register` RPC (exponential backoff, 16 retries)
3. Pre-generates one file per bucket into `AEROGYM_WORK_DIR` —
   **reuses existing files** if their size falls within the bucket's `[min, max)` range
   (avoids re-generating large files between runs)
4. Opens bidirectional gRPC `Session` stream
5. Executes slices — ramps connections up/down, uploads via FTP, sends `SliceAck` +
   `MetricsUpdate` messages back
6. On `ShutdownCmd` — graceful drain of in-flight transfers, sends final metrics, returns
7. **Loops back to step 2** — never exits; waits for aerocoach to reset and accept
   another registration

### Phase B (partial) ✅ — aerocoach controller
All core B-tasks done. Remaining stubs: delta engine, WebSocket broadcaster, `/plan` and
`/bandwidth` endpoints, NDJSON result writer.

**What works today:**
- gRPC server (tonic) on port 50051: `Register` and `Session` RPCs fully functional
- HTTP server (axum) on port 8080: `/health`, `/status`, `/start`, `/stop`, `/reset`
- Slice clock with ack-wait loop, ack-timeout, stop signal
- Agent registry with session-generation guards (prevents stale cleanup races)
- Metrics store accumulates `TransferRecord`s from all agents

**Test counts:** 46 aerocoach tests, 33 aerogym tests — all passing.

---

## 2 — What needs to be built next

### 2a — Remaining aerocoach tasks (prerequisite for frontend)

These are needed before aerotrack can show live data:

| Task | File | Description |
|------|------|-------------|
| **B.7** | `state/delta.rs` | `DeltaEngine::compute()` — builds `DashboardUpdate` JSON from `MetricsStore` + `Registry` |
| **B.8** | `ws/broadcaster.rs` | axum WebSocket upgrade handler + `broadcast::Sender<String>` fan-out |
| **B.9** | `main.rs` | Wire `/ws` route; spawn delta-engine ticker (every 3 s) |
| **B.12** | `main.rs` | `PUT /plan`, `PATCH /plan`, `POST /bandwidth` HTTP endpoints |
| **B.13a** | new file | NDJSON record writer — open on start, append each `TransferRecord`, close on DONE |
| **B.13b** | `main.rs` | `GET /results` — stream the record file as a download |

**B.7–B.9 are the critical path** — aerotrack cannot show live data without them.
B.12 and B.13 can be done in parallel or after the frontend is wired up.

### 2b — aerotrack (Phase C, all tasks)

Full task list is in `aerogym/ARCHITECTURE_PLAN_V2.md` section "Phase C".  
Summary: C.0 scaffold through C.19. Start with C.0–C.6 (foundation + stores), then the
three visual panels (C.7–C.11), then the control bar and layout wiring (C.12–C.15).

---

## 3 — Running the system locally

### Prerequisites
```bash
# FTP server (already running on this machine)
python3 -m pyftpdlib -p 2121 -w
# Credentials: test / secret   (set in pyftpdlib config)
# Files land in: /home/rommel/software/aerosuite/tmp_ftp_root/
```

### Start aerocoach
```bash
cd /home/rommel/software/aerosuite
cargo build --bin aerocoach        # always rebuild before running
NO_COLOR=1 RUST_LOG=info \
  AEROCOACH_PLAN_FILE=/tmp/integration_plan.json \
  ./target/debug/aerocoach
```

### Start an agent
```bash
cargo build --bin aerogym          # always rebuild before running
NO_COLOR=1 RUST_LOG=info \
  AEROGYM_AGENT_ID=a00 \
  AEROCOACH_URL=http://127.0.0.1:50051 \
  AEROSTRESS_TARGET=127.0.0.1:2121 \
  AEROGYM_WORK_DIR=/tmp/aerogym_a00 \
  ./target/debug/aerogym
```

### Operator flow
```bash
# Check who has connected
curl http://localhost:8080/status | python3 -m json.tool

# Fire the test
curl -X POST http://localhost:8080/start

# After DONE, reset for another run
curl -X POST http://localhost:8080/reset

# Agents re-register automatically — wait for connected=1 then /start again
```

### Kill processes precisely (important — stale processes caused test confusion)
```bash
pkill -9 -f "target/debug/aerocoach"
pkill -9 -f "target/debug/aerogym"
# Verify:
pgrep -f "target/debug/aerocoach" || echo "clear"
pgrep -f "target/debug/aerogym"   || echo "clear"
```

---

## 4 — Current HTTP API (aerocoach)

| Method | Path | State req. | Response |
|--------|------|-----------|----------|
| GET | `/health` | any | `200 OK` plain text |
| GET | `/status` | any | JSON (see §4a) |
| POST | `/start` | WAITING + plan loaded | `{"status":"started","agents":N}` or 409/412 |
| POST | `/stop` | RUNNING | `{"status":"stopping"}` or 409 |
| POST | `/reset` | DONE | `{"status":"waiting"}` or 409 |
| GET | `/ws` | **not yet implemented** | WebSocket upgrade |
| PUT | `/plan` | **not yet implemented** | — |
| PATCH | `/plan` | **not yet implemented** | — |
| POST | `/bandwidth` | **not yet implemented** | — |
| GET | `/results` | **not yet implemented** | — |

### 4a — GET /status response shape
```json
{
  "state":         "WAITING | RUNNING(slice=N) | DONE",
  "agent_count":   2,
  "connected":     2,
  "plan_id":       "my-test-01",
  "total_slices":  6,
  "current_slice": 3,
  "agents": [
    {
      "agent_id":           "a00",
      "agent_index":        0,
      "private_ip":         "172.16.8.192/20",
      "current_slice":      3,
      "active_connections": 12,
      "connected":          true
    }
  ]
}
```

---

## 5 — Environment variables

### aerocoach
| Variable | Default | Notes |
|----------|---------|-------|
| `AEROCOACH_GRPC_PORT` | `50051` | gRPC listen port |
| `AEROCOACH_HTTP_PORT` | `8080` | HTTP + WS listen port |
| `AEROCOACH_PLAN_FILE` | *(none)* | Path to JSON plan file; required before `/start` |
| `AEROCOACH_RECORD_DIR` | `/data/records` | NDJSON results output directory |
| `NO_COLOR` | *(unset)* | Set to any value to disable ANSI colour in logs |
| `RUST_LOG` | `info` | Tracing filter |

### aerogym
| Variable | Required | Default | Notes |
|----------|----------|---------|-------|
| `AEROGYM_AGENT_ID` | **Yes** | — | e.g. `a00`–`a99`; unique per container |
| `AEROCOACH_URL` | **Yes** | — | Full gRPC URL `http://host:50051` — **not** the HTTP port |
| `AEROSTRESS_TARGET` | **Yes** | — | FTP `host:port` e.g. `10.0.2.10:21` — **port required** |
| `AEROSTRESS_USER` | No | `test` | FTP username |
| `AEROSTRESS_PASS` | No | `secret` | FTP password |
| `AEROGYM_WORK_DIR` | No | `/tmp/aerogym` | Bucket file directory; mount a volume here |
| `AEROGYM_PRIVATE_IP` | No | `""` | Override for ECS metadata private IP |
| `AEROGYM_INSTANCE_ID` | No | `""` | Override for ECS task ARN |
| `NO_COLOR` | No | *(unset)* | Set to disable ANSI log colours (always set in AWS) |
| `RUST_LOG` | No | `info` | Tracing filter |

---

## 6 — Load plan JSON format

```json
{
  "plan_id": "my-test-01",
  "slice_duration_ms": 10000,
  "total_bandwidth_bps": 52428800,
  "file_distribution": {
    "buckets": [
      { "bucket_id": "xs", "size_min_bytes":        0, "size_max_bytes":  10485760, "percentage": 0.60 },
      { "bucket_id": "sm", "size_min_bytes": 10485760, "size_max_bytes":  52428800, "percentage": 0.25 },
      { "bucket_id": "md", "size_min_bytes": 52428800, "size_max_bytes": 104857600, "percentage": 0.15 }
    ]
  },
  "slices": [
    { "slice_index": 0, "total_connections": 10 },
    { "slice_index": 1, "total_connections": 20 },
    { "slice_index": 2, "total_connections": 10 }
  ]
}
```

**Validation rules (enforced at load time):**
- `plan_id` non-empty
- `slice_duration_ms` > 0
- `total_bandwidth_bps` > 0
- At least one slice; indices must be 0-based and gapless (no holes)
- At least one bucket; `bucket_id` unique; `size_min_bytes` < `size_max_bytes`
- All `percentage` values must sum to exactly 1.0

---

## 7 — WebSocket / DashboardUpdate shape (for aerotrack)

This is the JSON that aerocoach will broadcast to `/ws` clients every ~3 seconds once
B.7–B.9 are implemented. The shape is defined in the proto and documented in the
architecture plan (section C.2). Key points for the frontend:

```typescript
interface DashboardUpdate {
  timestamp_ms:          number;
  current_slice:         number;
  total_slices:          number;
  agents:                AgentSnapshot[];
  completed_transfers:   TransferRecord[];   // delta only — since last broadcast
  global_stats:          GlobalStats;
}

interface AgentSnapshot {
  agent_id:           string;
  agent_index:        number;    // 0–99; maps to grid position (row=index÷10, col=index%10)
  connected:          boolean;
  current_slice:      number;
  active_connections: number;
  bytes_transferred:  number;    // cumulative
  success_count:      number;
  error_count:        number;
  private_ip:         string;
  instance_id:        string;
}

interface GlobalStats {
  total_bytes_transferred: number;
  total_success:           number;
  total_errors:            number;
  active_agents:           number;
  active_connections:      number;
  overall_error_rate:      number;   // 0.0–1.0
  current_bandwidth_bps:   number;
}
```

**Until `/ws` is implemented**, the frontend can poll `GET /status` (every 2–3 s) for
agent state and use that to build a degraded live view. The `/status` shape is in §4a.

---

## 8 — Architecture decisions made this session

1. **Agent never exits** — after `ShutdownCmd` + graceful drain, the agent loops back
   and re-registers. ECS tasks stay running across multiple test runs. The operator calls
   `POST /reset` on aerocoach to re-open the WAITING state.

2. **Session-generation guard** — `Registry::set_session_channel()` returns a `u64`
   generation counter. The session receive task captures it; `close_session(id, gen)`
   only clears the channel if the generation still matches, preventing stale cleanup
   tasks from closing a newer session. (`gen` is a reserved keyword in edition 2024 —
   use `session_gen` instead.)

3. **Bucket file reuse** — `file_manager::generate()` checks existing file size against
   the bucket's `[size_min_bytes, size_max_bytes)` range before regenerating. Large
   files (xxl = 1 GB, giant = 2 GB) are preserved across test runs. Zero-byte files
   are always regenerated.

4. **gRPC bidi session pre-buffering** — the agent sends its identification report into
   the mpsc channel *before* calling `client.session().await`. This breaks the deadlock
   where the server reads the first message before returning response headers, but the
   client only sends after receiving response headers.

5. **`NO_COLOR` env var** — both binaries call `.with_ansi(std::env::var_os("NO_COLOR").is_none())`.
   Set `NO_COLOR=1` in all ECS task definitions / Dockerfiles.

6. **Registration retry** — 16 attempts, exponential backoff 2 s → 4 s → 8 s → 16 s →
   capped at 30 s. Total tolerance before first cycle ends: ~4 minutes. After exhausting
   16 attempts the outer loop sleeps 10 s and starts a new cycle — the agent never
   permanently gives up.

7. **`cargo build` vs `cargo test`** — `cargo test` updates only the test artifact in
   `target/debug/deps/`, NOT `target/debug/aerogym`. Always run
   `cargo build --bin <name>` explicitly before running the binary.

---

## 9 — Known issues / deferred items

| Item | Notes |
|------|-------|
| Double registration on startup | Harmless — the second registration wins and the first is overwritten; both succeed as re-registrations. Root cause: tonic channel lazy-connect can trigger two connect attempts in quick succession before the first completes. Investigate if it causes observable problems in AWS. |
| `active_connections` in `/status` after DONE | Shows the last reported value (not 0) because the final MetricsUpdate arrives after the session stream closes. Cosmetic only. |
| `/plan` GET+PUT | Not yet implemented; plan is loaded from file at startup only. Needed for the aerotrack Edit mode (C.8). |
| `POST /bandwidth` and `PATCH /plan` | Not yet implemented. Needed for mid-run plan adjustments from aerotrack. |
| NDJSON result writer | Not yet implemented (B.13). `GET /results` will 404 until done. |
| `GET /ws` | Not yet implemented (B.8–B.9). aerotrack must poll `/status` until this is ready. |
| ECS metadata fetch in aerogym | `AEROGYM_PRIVATE_IP` and `AEROGYM_INSTANCE_ID` are injected as env vars today. Auto-fetching from `http://169.254.170.2/v2/metadata` (task C.16 in the plan) is deferred. |

---

## 10 — File map (key source files)

```
aerosuite/
├── Cargo.toml                          workspace root
├── session-handoff-3.md                THIS FILE
├── aeroproto/
│   ├── Cargo.toml
│   ├── build.rs
│   └── proto/aeromonitor.proto         frozen v1 proto contract
├── aerocoach/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                     HTTP server + routes + shutdown
│       ├── config.rs                   env var config (closure-based testing)
│       ├── grpc/agent_service.rs       Register + Session tonic handlers
│       ├── model/
│       │   ├── clock.rs                slice tick scheduler (tokio interval)
│       │   ├── distributor.rs          per-agent share math
│       │   └── load_plan.rs            JSON plan parsing + validation + to_proto()
│       ├── state/
│       │   ├── mod.rs                  AppState, CoachState, SharedState, reset()
│       │   ├── registry.rs             AgentEntry, session_gen guard, broadcast()
│       │   ├── metrics_store.rs        accumulates TransferRecords
│       │   └── delta.rs                STUB — DeltaEngine not yet implemented
│       └── ws/
│           ├── mod.rs
│           └── broadcaster.rs          STUB — WebSocket broadcaster not yet implemented
└── aerogym/
    ├── Cargo.toml
    ├── ARCHITECTURE_PLAN_V2.md         full architecture spec (all phases)
    └── src/
        ├── main.rs                     outer loop: register → generate → session → loop
        ├── config.rs                   legacy aerostress config
        ├── bin/legacy.rs               original aerostress binary
        └── agent/
            ├── config.rs               aerogym agent config (env vars)
            ├── file_manager.rs         bucket file generate/reuse logic
            ├── load_plan.rs            AgentPlan: per-agent connections + bandwidth
            ├── metrics.rs              MetricsAccumulator
            ├── rate_limit.rs           governor-based bandwidth throttle
            ├── registration.rs         Register RPC + retry loop
            ├── session.rs              Session RPC + slice execution loop
            └── transfer.rs             FTP upload + TransferOutcome
```
