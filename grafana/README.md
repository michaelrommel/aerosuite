# aeromon — Grafana Observability Stack on AWS ECS

Single ECS Fargate task running four co-located containers:

| Container     | Image                        | Port(s)                           | Role                                          |
|---------------|------------------------------|-----------------------------------|-----------------------------------------------|
| `config-init` | `busybox:1.36`               | —                                 | Writes all config files to Docker volumes, then exits (`essential: false`) |
| `prometheus`  | `prom/prometheus:v2.51.2`    | `9090`                            | Scrapes aeroftp nodes, remote-writes to Mimir |
| `tempo`       | `grafana/tempo:2.4.2`        | `3200` / `4317` / `4318` / `9411` | Distributed trace storage                     |
| `mimir`       | `grafana/mimir:2.12.0`       | `9009`                            | Metrics storage (Prometheus-compatible)       |
| `grafana`     | `grafana/grafana:10.4.3`     | `3000`                            | Dashboards & datasource UI                    |

Because all containers share the same `awsvpc` network namespace, every
service is reachable from its siblings via **localhost**.

---

## Directory layout

```
grafana/
├── task-definition.json                          # ECS task definition (register with AWS)
│                                                 # configs are embedded inside config-init
├── configs/                                      # reference copies of the config files
│   ├── prometheus/prometheus.yml                 # Static scrape: 172.16.32.20–.39
│   ├── tempo/tempo.yaml                          # Monolithic Tempo, OTLP + Zipkin receivers
│   ├── mimir/mimir.yaml                          # Monolithic Mimir, local filesystem backend
│   └── grafana/provisioning/datasources/
│       └── datasources.yaml                      # Auto-provisioned Prometheus, Mimir & Tempo DSes
└── scripts/
    └── start_task.sh                             # aws ecs run-task wrapper
```

> **Config management** — all configs are embedded directly in the `config-init`
> container's `command` field as shell heredocs. The files under `configs/` are
> kept as human-readable references; edit them there first, then paste the updated
> content back into the corresponding heredoc in `task-definition.json` and
> re-register the task definition.

---

## Prerequisites

### 1. SSM Parameter
Store the Grafana admin password in SSM Parameter Store:

```bash
aws ssm put-parameter \
  --name  /aeromon/grafana/admin-password \
  --value "<your-password>" \
  --type  SecureString
```

### 2. IAM roles
- **`ecsTaskExecutionRole`** — needs `AmazonECSTaskExecutionRolePolicy` plus
  `ssm:GetParameters` on the admin-password parameter.
- **`ecsTaskRole`** — standard ECS task role; no EFS permissions required.

### 3. Placeholders to fill in `task-definition.json`

| Placeholder    | Description                  |
|----------------|------------------------------|
| `<ACCOUNT_ID>` | Your 12-digit AWS account ID |
| `<AWS_REGION>` | e.g. `eu-west-1`             |

And in `scripts/start_task.sh`:

| Placeholder             | Description                                  |
|-------------------------|----------------------------------------------|
| `<SUBNET_ID>`           | e.g. `subnet-0779b66ce8c3a599c`              |
| `<SECURITY_GROUP_ID>`   | e.g. `sg-06d737ea5595c275d`                  |

---

## Register & run

```bash
# Register the task definition
aws ecs register-task-definition \
  --no-cli-pager \
  --cli-input-json file://task-definition.json

# Start the task (defaults to FARGATE; pass FARGATE_SPOT as $1 for spot)
./scripts/start_task.sh
```

---

## Prometheus scrape targets

`configs/prometheus/prometheus.yml` statically scrapes all 20 nodes in the
`172.16.32.20–.39` range on **port 9090**.  
Adjust the port in the `targets` list if your aeroftp/aeroscale binaries
expose metrics on a different port.

Scraped metrics are **remote-written to Mimir** in addition to being stored
locally in Prometheus, giving you long-term retention via Mimir.

---

## Storage

The task is allocated **30 GiB of Fargate ephemeral storage** — more than
enough for a multi-hour demo session. All four config volumes are plain
Docker-managed volumes backed by that same ephemeral layer; no EFS or S3 is
required.

> **Data is lost when the task stops.** This is intentional for a demo
> workload. If you later need persistence, switch Tempo and Mimir to an S3
> backend:
> - **Tempo**: set `storage.trace.backend: s3` and add `s3:` block.
> - **Mimir**: set `common.storage.backend: s3` and fill in
>   `blocks_storage.s3.*`.
