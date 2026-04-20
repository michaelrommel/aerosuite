Here is the full handoff summary for the new session:

---

## Aerosuite — Project Handoff Summary

### Repository layout

```
aerosuite/                          ← Cargo workspace root
  Cargo.toml                        ← workspace + shared deps + release profile
  aerocore/                         ← shared library (no binary)
  aeroftp/                          ← scalable FTP service (libunftp-based)
  aerogym/                          ← stress tester (formerly aerostress)
  aeroscale/                        ← autoscaling daemon + scale CLI  ← MAIN FOCUS
  aeroslot/                         ← slot pool manager (formerly slot-pool-native)
  aeroplug/                         ← ENI/IP manager (consolidates 3 old binaries)
  aeropulse/                        ← keepalived config generator (formerly keepalived-config)
  aerobake/
    aeroftp/                        ← Packer AMI for FTP backends
    aeroscale/                      ← Packer AMI for load balancers
  libunftp/                         ← git submodule (untouched)
  unftp-sbe-opendal/                ← git submodule (untouched)
```

---

### Crate responsibilities

**`aerocore`** — shared library used by all other crates:
- `aws/` — IMDSv2, SigV4, `aws_query()`, XML helpers (`extract_balanced`, `extract_scalar`, `extract_all_scalars`)
- `asg/` — `describe()`, `set_desired()`, `terminate_instance()`, `AsgGroup`/`AsgInstance` structs
- `redis_pool/` — `build_redis_client()`, key constants (`slots:available`, `slots:leases`, `slot:owner:<n>`, `backend:weight:<ip>`, `backend:weights:ts`), `now_ms()`

**`aeroslot`** — Redis slot pool daemon. Binaries: `aeroslot` (main), `aeroslot-lua` (alternative). OpenRC service on backend AMI. On `release`, publishes `{"slot": N, "action": "release", "instance_id": "i-xxx"}` to the `asg-change` Redis channel (R4 — instance_id added).

**`aeroplug`** — single binary with three subcommands:
- `assign-ip` — assign/unassign secondary private IPs on ENIs
- `attach-eni` — attach/detach/takeover ENIs by selector (ID, name, tag, description)
- `manage-eni` — slot-based ENI attach/detach via `aeroftp-slot` tag

**`aeropulse`** — generates keepalived VRRP config, notify scripts, and track scripts from EC2 IMDS tags. The generated `notify-master.sh` now calls `aeroplug assign-ip` (updated from old `assign-secondary-ip`).

**`aeroscale`** — the main daemon. Binaries: `aeroscale` (daemon), `scale` (CLI tool), `aws-config`.

---

### aeroscale internal structure

```
aeroscale/src/
  lib.rs               ← re-exports aerocore::* + declares all modules
  slot_network.rs      ← SlotNetwork: ip_for_slot(slot), slot_for_ip(ip), from_imds()
  vrrp.rs              ← is_master(vip_inside): checks `ip addr show` for the VIP
  weight_sync.rs       ← init(), persist(), sync_from_redis()
  snapshot/
    mod.rs             ← SystemSnapshot, BackendStatus, AsgGroupInfo, collect(), print()
    weights.rs         ← reads /etc/keepalived/weights/backend-<IP>.weight files
    leases.rs          ← reads slots:leases + slot:owner:<n> from Redis
    asg.rs             ← reads ASG instances + group capacity (desired/min/max)
    ipvs.rs            ← reads /proc/net/ip_vs (hex format), parses real servers
  cleanup/
    mod.rs             ← 3-section cleanup (active leases, orphaned ASG, no-lease backends)
  listener.rs          ← subscribes to asg-change Redis channel, reacts to claim/release
  metrics/
    mod.rs             ← scrape_and_push(): scrape + IPVS cross-check + CloudWatch
    scrape.rs          ← HTTP scrape of backend :9090/metrics via prometheus-parse
    exposition.rs      ← Prometheus text format with slot labels
    cloudwatch.rs      ← PutMetricData via aws_query (slot dimensions, not instance IDs)
    server.rs          ← axum HTTP server serving GET /metrics on :9090
  bin/
    aeroscale.rs       ← main daemon
    scale.rs           ← CLI: list / scale --desired N / terminate --instance-id
    aws-config.rs      ← writes IMDS metadata to a VAR=VALUE file
```

---

### Slot → IP mapping

The formula `IP = base + offset + slot` is implemented in `SlotNetwork`:
- **base**: read from the load balancer's eth1 subnet CIDR via IMDS (`network/interfaces/macs/<mac>/subnet-ipv4-cidr-block`) e.g. `172.16.32.0`
- **offset**: read from instance tag `aeroftp-slot-offset` e.g. `20`
- slot 0 → `172.16.32.20`, slot 11 → `172.16.32.31`, slot 19 → `172.16.32.39`
- CLI overrides: `--slot-base <IP>` and `--slot-offset <N>`

---

### Dual-node (master/backup) behaviour

`aeroscale` runs on **both** load balancer nodes. Role is determined each cycle by checking whether the inside VIP (`--vip-inside`, default `172.16.32.10`) is assigned to a local interface.

| Action | Master | Backup |
|---|---|---|
| Collect SystemSnapshot | ✅ | ✅ |
| Run cleanup (terminate, release slots, write weights) | ✅ | ❌ |
| Persist weight state to Redis (`backend:weight:<ip>`) | ✅ | ❌ |
| Sync weight files from Redis | ❌ | ✅ |
| Scrape backend metrics / serve `/metrics` | ✅ | ✅ |
| Push to CloudWatch | ✅ | ❌ |

---

### Weight file startup initialisation

keepalived **must** initialise weight files to `-1` (draining) — a keepalived bug means disabled backends are not tracked at all. `aeroscale` fixes this immediately at startup before the first cleanup pass:

1. Read `backend:weights:ts` from Redis
2. If age < `--weight-state-ttl` (default **3600s**): restore each `backend:weight:<ip>` key → weight file
3. If stale/absent: read current leases → live lease → `0` (active), no lease → `-2147483648` (disabled)

After each master cleanup pass, all weight files are written back to Redis (`backend:weight:<ip>` + `backend:weights:ts`).

---

### Key Redis keys

| Key | Type | Written by | Read by |
|---|---|---|---|
| `slots:available` | sorted set | aeroslot | aeroslot |
| `slots:leases` | sorted set (score=expiry ms) | aeroslot | aeroslot, aeroscale |
| `slot:owner:<n>` | string | aeroslot | aeroscale |
| `asg-change` | pub/sub channel | aeroslot | aeroscale listener |
| `backend:weight:<ip>` | string | aeroscale master | aeroscale backup, aeroscale init |
| `backend:weights:ts` | string (unix ms) | aeroscale master | aeroscale init |

### asg-change message format

```json
{ "slot": 3, "action": "claim" }
{ "slot": 3, "action": "release", "instance_id": "i-0abc1234567890def" }
```

---

### Cleanup logic (P2)

**Section 2.1 — Active leases:**
- Owner no longer InService in ASG → release slot (Redis) + disable weight file
- Weight=active, lease alive → no-op
- Weight=draining, 0 IPVS connections, `desired > min` → disable + terminate instance
- Weight=draining, 0 IPVS connections, `desired == min` → **WARN, skip termination** (ASG constraint guard)
- Weight=draining, connections > 0 → wait
- Weight=disabled, lease alive and **not expired** → re-enable (missed claim message recovery)
- Weight=disabled, lease **expired** → leave disabled (stale lease from failed init)

**Section 2.2 — Orphaned ASG instances:** InService instance with no lease → ERROR + terminate

**Section 2.3 — Backends without leases:** active/draining weight with no lease → disable (crashed backend)

---

### aeroscale CLI flags (key ones)

```
--region             eu-west-2
--asg-name           aeroftp-backend
--redis-url          $REDIS_URL
--weights-dir        /etc/keepalived/weights
--metrics-port       9090
--scrape-port        9090
--cloudwatch-namespace  AeroFTP/Autoscaler
--snapshot-interval  30
--vip-inside         172.16.32.10   ← VRRP master detection
--weight-state-ttl   3600           ← Redis state freshness threshold
--slot-base          <override>
--slot-offset        <override>
--dry-run
```

---

### What remains: P5 — Scale-up / Drain algorithm

This is the only major piece not yet implemented. The algorithm needs to be **designed first** before writing code. The inputs available are:

- `SystemSnapshot` — weight states, leases, ASG capacity (desired/min/max), IPVS connections
- `MetricsStore` — per-slot Prometheus metrics from the last scrape cycle:
  - `ftp_sessions_total` (gauge) — current active FTP sessions per slot
  - `ftp_sessions_count` (counter) — cumulative sessions per slot
  - `ftp_command_total` (counter, labelled by command) — throughput per slot
  - `ipvs_active_connections` — from IPVS (cross-check against `ftp_sessions_total`)
- `AsgGroupInfo.desired_capacity`, `.min_size`, `.max_size`

**Scale-up trigger (candidate):** average `ftp_sessions_total` per active slot exceeds a high-water mark AND `desired < max`
→ call `asg::set_desired(desired + 1)`

**Drain trigger (candidate):** a slot's `ftp_sessions_total` is 0 AND total active slots > low-water mark
→ write `-1` to that backend's weight file; P2 cleanup handles the rest once IPVS drains

**Open design questions for the new session:**
1. What are the right high/low watermarks? Are they fixed or per-slot?
2. Hysteresis: how long must a condition persist before acting (avoid flapping)?
3. Which slot to drain first? Least loaded? Highest slot number? Oldest?
4. Should scale-up and drain decisions be rate-limited independently?
5. The `DESIGN.md` in `aeroscale/` has a placeholder for P5 — fill it in before implementing

Good luck with the testing, and see you in the next session!
