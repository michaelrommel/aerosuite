
 ### 1 — The load plan file (aerocoach only)

 This is the one file you write by hand. Create it anywhere on the host and point aerocoach at it via AEROCOACH_PLAN_FILE.

 ```json
   {
     "plan_id": "my-test-01",
     "slice_duration_ms": 10000,
     "total_bandwidth_bps": 52428800,
     "file_distribution": {
       "buckets": [
         { "bucket_id": "xs", "size_min_bytes":       0, "size_max_bytes":  10485760, "percentage": 0.60 },
         { "bucket_id": "sm", "size_min_bytes": 10485760, "size_max_bytes":  52428800, "percentage": 0.25 },
         { "bucket_id": "md", "size_min_bytes": 52428800, "size_max_bytes": 104857600, "percentage": 0.15 }
       ]
     },
     "slices": [
       { "slice_index": 0, "total_connections": 10 },
       { "slice_index": 1, "total_connections": 20 },
       { "slice_index": 2, "total_connections": 30 },
       { "slice_index": 3, "total_connections": 20 },
       { "slice_index": 4, "total_connections": 10 }
     ]
   }
 ```

 Validation rules (startup will refuse the plan if violated):
 - plan_id must not be empty
 - slice_duration_ms > 0
 - total_bandwidth_bps > 0
 - At least one slice; slice_index must be 0-based and gapless
 - At least one bucket; bucket_id unique; size_min_bytes < size_max_bytes; all percentage values sum to exactly 1.0
 - total_bandwidth_bps ÷ number-of-agents = per-agent rate limit in bytes/s

 ────────────────────────────────────────────────────────────────────────────────

 ### 2 — aerocoach environment variables

 ┌──────────────────────┬──────────┬───────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
 │ Variable             │ Required │ Default       │ Notes                                                                                                                     │
 ├──────────────────────┼──────────┼───────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROCOACH_PLAN_FILE  │ No       │ (none)        │ Path to the JSON plan file. If omitted, you must supply it via PUT /plan before starting (not yet implemented — set this) │
 ├──────────────────────┼──────────┼───────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROCOACH_GRPC_PORT  │ No       │ 50051         │ Port agents call Register/Session on                                                                                      │
 ├──────────────────────┼──────────┼───────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROCOACH_HTTP_PORT  │ No       │ 8080          │ Port for operator HTTP API                                                                                                │
 ├──────────────────────┼──────────┼───────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROCOACH_RECORD_DIR │ No       │ /data/records │ Where NDJSON result files will be written (directory must exist or be mounted)                                            │
 └──────────────────────┴──────────┴───────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘

 Minimal start:

 ```bash
   AEROCOACH_PLAN_FILE=/etc/aerocoach/plan.json \
     aerocoach
 ```

 ────────────────────────────────────────────────────────────────────────────────

 ### 3 — aerogym environment variables

 ┌─────────────────────┬──────────┬──────────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
 │ Variable            │ Required │ Default      │ Notes                                                                                                                 │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROGYM_AGENT_ID    │ Yes      │ —            │ Unique ID, e.g. a00–a99. Must be distinct across all containers                                                       │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROCOACH_URL       │ Yes      │ —            │ Full gRPC URL: http://<host>:50051                                                                                    │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROSTRESS_TARGET   │ Yes      │ —            │ FTP server as host:port, e.g. ftpserver:21                                                                            │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROSTRESS_USER     │ No       │ test         │ FTP username                                                                                                          │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROSTRESS_PASS     │ No       │ secret       │ FTP password                                                                                                          │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROGYM_WORK_DIR    │ No       │ /tmp/aerogym │ Where bucket files are pre-generated. Needs ~2× the size of your largest bucket. Mount a volume here if space matters │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROGYM_PRIVATE_IP  │ No       │ ""           │ Reported to aerocoach for dashboards; set to container IP or ECS private IP                                           │
 ├─────────────────────┼──────────┼──────────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
 │ AEROGYM_INSTANCE_ID │ No       │ ""           │ ECS task ARN or other identifier for dashboards                                                                       │
 └─────────────────────┴──────────┴──────────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘

 Minimal start (one agent):

 ```bash
   AEROGYM_AGENT_ID=a00 \
   AEROCOACH_URL=http://10.0.1.10:50051 \
   AEROSTRESS_TARGET=10.0.2.10:21 \
     aerogym
 ```

 ────────────────────────────────────────────────────────────────────────────────

 ### 4 — Manual test sequence

 ```
   # Terminal 1 — start the controller
   AEROCOACH_PLAN_FILE=/tmp/plan.json aerocoach

   # Terminal 2..N — start one agent per terminal (each needs a unique AEROGYM_AGENT_ID)
   AEROGYM_AGENT_ID=a00 AEROCOACH_URL=http://127.0.0.1:50051 AEROSTRESS_TARGET=127.0.0.1:21 aerogym
   AEROGYM_AGENT_ID=a01 AEROCOACH_URL=http://127.0.0.1:50051 AEROSTRESS_TARGET=127.0.0.1:21 aerogym

   # Wait until all agents show connected=true, then kick off the test
   curl http://localhost:8080/status          # check agents registered + connected

   curl -X POST http://localhost:8080/start   # fire

   curl http://localhost:8080/status          # watch state: WAITING → RUNNING → DONE
 ```

 aerocoach waits indefinitely in WAITING — agents can register at any time before you call /start. There is no auto-start and no timeout.

 ────────────────────────────────────────────────────────────────────────────────

 ### 5 — Dockerfile sketches

 aerocoach — single static binary + plan file mount:

 ```dockerfile
   FROM debian:bookworm-slim
   RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
   COPY target/release/aerocoach /usr/local/bin/aerocoach
   RUN mkdir -p /data/records /etc/aerocoach
   EXPOSE 50051 8080
   ENV AEROCOACH_GRPC_PORT=50051 \
       AEROCOACH_HTTP_PORT=8080 \
       AEROCOACH_RECORD_DIR=/data/records
   # Mount your plan.json at /etc/aerocoach/plan.json and set AEROCOACH_PLAN_FILE
   CMD ["aerocoach"]
 ```

 Mount the plan at runtime:

 ```bash
   docker run \
     -v /host/path/plan.json:/etc/aerocoach/plan.json \
     -e AEROCOACH_PLAN_FILE=/etc/aerocoach/plan.json \
     -v aerocoach-results:/data/records \
     -p 50051:50051 -p 8080:8080 \
     aerocoach
 ```

 aerogym — binary + ephemeral work volume:

 ```dockerfile
   FROM debian:bookworm-slim
   RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
   COPY target/release/aerogym /usr/local/bin/aerogym
   RUN mkdir -p /data/aerogym
   ENV AEROGYM_WORK_DIR=/data/aerogym \
       AEROSTRESS_USER=test \
       AEROSTRESS_PASS=secret
   # AEROGYM_AGENT_ID, AEROCOACH_URL, AEROSTRESS_TARGET must be injected at runtime
   CMD ["aerogym"]
 ```

 Run one container per agent, injecting the unique ID:

 ```bash
   docker run \
     -e AEROGYM_AGENT_ID=a00 \
     -e AEROCOACH_URL=http://10.0.1.10:50051 \
     -e AEROSTRESS_TARGET=10.0.2.10:21 \
     -e AEROGYM_PRIVATE_IP=10.0.1.20 \
     aerogym
 ```

 ────────────────────────────────────────────────────────────────────────────────

 ### 6 — Key things to keep in mind for containers

 Work directory size — each agent pre-generates one file per bucket before connecting. With your current three-bucket plan that is roughly max(xs) + max(sm) + max(md) = 10 + 52 + 104 = ~166 MB
 per container. Mount a volume or tmpfs at AEROGYM_WORK_DIR so it doesn't bloat the container layer.

 Agent IDs must be unique — if you run 10 containers, inject a00–a09. A duplicate ID is rejected by aerocoach with already_exists.

 Start order matters — aerocoach must be reachable before aerogym starts. aerogym retries registration up to 8 times with 3 s back-off (~24 s tolerance), so a brief delay is fine, but aerocoach
 must be healthy before that window expires.

 /start is manual — nothing auto-fires. Once all your agents show "connected": true in GET /status, you call POST /start from outside (operator laptop, CI step, deploy hook, etc.).

 AEROCOACH_URL points to the gRPC port (50051), not the HTTP port (8080) — a common gotcha. The HTTP API is for the operator only; agents use gRPC exclusively.


