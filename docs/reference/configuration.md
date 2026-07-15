# Configuration reference

Papermake is configured with environment variables. The server and worker also
load a local `.env` file when present.

## S3-compatible storage

These variables are used by `papermake-server`, render workers, and maintenance
workers.

| Variable | Required | Default | Meaning |
|---|---|---:|---|
| `S3_ENDPOINT_URL` | yes | - | S3-compatible endpoint, for example `http://object-store:9000`; for AWS S3, use the regional endpoint URL (e.g. `s3.eu-central-1.amazonaws.com`) |
| `S3_BUCKET` | yes | - | Bucket used for templates, renders, jobs, analytics, and expiry indexes |
| `S3_ACCESS_KEY_ID` | yes | - | S3 access key |
| `S3_SECRET_ACCESS_KEY` | yes | - | S3 secret key |
| `S3_OP_TIMEOUT_SECONDS` | no | `20` | Per-attempt timeout for S3 operations |
| `S3_MAX_ATTEMPTS` | no | `3` | Maximum attempts for retryable S3 operations |

S3 operations are treated as idempotent where Papermake retries them. A stalled
or temporarily failing object store should fail a bounded attempt rather than
hang a render or worker loop indefinitely.

## Server

Used by `papermake-server`.

| Variable | Required | Default | Meaning |
|---|---|---:|---|
| `PORT` | no | `3000` | HTTP port |
| `HOST` | no | `0.0.0.0` | HTTP bind address |
| `RUST_LOG` | no | `papermake_server=info,papermake_registry=info,tower_http=info` | Tracing filter |
| `MAX_CONCURRENT_RENDERS` | no | `10` | Maximum synchronous Typst renders running per server |
| `RENDER_TIMEOUT_SECONDS` | no | `300` | Render timeout, including time waiting for a render slot |
| `REQUEST_BODY_LIMIT_BYTES` | no | `52428800` | Maximum accepted HTTP request body size; default is 50 MiB |
| `SHARD_SIZE` | no | `500` | Number of batch inputs per shard when the server enqueues a batch |
| `FONTS_DIR` | no | unset; Docker image uses `/fonts` | One or more font directories, separated by the OS path separator |
| `CACHE_DIRECTORY` | no | system temp directory | Typst cache directory |
| `PAPERMAKE_INSTANCE_ID` | no | random uuid | Stable id used in analytics raw keys written by this server |
| `FLUSH_INTERVAL_SECONDS` | no | `30` | How often the server flushes buffered render records to S3 |
| `FLUSH_MAX_RECORDS` | no | `1000` | Buffer size that triggers an eager analytics flush |
| `RENDER_RETENTION_DAYS` | no | `30` | Global default output retention; `0` keeps outputs forever |

`RENDER_RETENTION_DAYS` is only the global default. A template can set
`metadata.retain_days`, and a render request can set `retain_days`; more
specific values win.

## Worker roles

Used by `papermake-worker`.

| Variable | Required | Default | Meaning |
|---|---|---:|---|
| `WORKER_ROLE` | no | `all` | `render`, `maintenance`, or `all` |
| `WORKER_INTERVAL_SECONDS` | no | role-specific | Poll or maintenance interval: `render` defaults to `5`, `maintenance` to `30`, `all` to `10` |
| `PAPERMAKE_WORKER_ID` | no | hostname, then PID | Unique worker id used as shard owner and analytics instance key |
| `PAPERMAKE_INSTANCE_ID` | no | - | Fallback worker id when `PAPERMAKE_WORKER_ID` is unset |
| `RUST_LOG` | no | `papermake_worker=info` | Tracing filter |

`WORKER_ROLE=all` is convenient for local development. For production-style
deployments, run render workers and one maintenance worker separately.

## Render workers

Used by workers whose role includes `render`.

| Variable | Required | Default | Meaning |
|---|---|---:|---|
| `WORKER_LEASE_SECONDS` | no | `120` | Time before another worker can reclaim a shard whose owner stopped heartbeating |
| `WORKER_MAX_ATTEMPTS` | no | `3` | Number of claims before a repeatedly failing shard is marked failed |
| `RENDER_RETENTION_DAYS` | no | `30` | Retention default for outputs produced by batch renders |
| `FONTS_DIR` | no | unset; Docker image uses `/fonts` | Same font directories as the server |
| `CACHE_DIRECTORY` | no | system temp directory | Typst cache directory |

Use the same font configuration on the server and render workers so synchronous
and batch renders produce the same output.

## Maintenance workers

Used by workers whose role includes `maintenance`.

| Variable | Required | Default | Meaning |
|---|---|---:|---|
| `ANALYTICS_RETENTION_DAYS` | no | `30` | How long to keep raw analytics NDJSON files |
| `JOB_RETENTION_DAYS` | no | `7` | How long to keep batch job documents; `0` keeps them forever |

Maintenance workers also need the S3 variables because they aggregate raw
analytics, prune expired render outputs, and delete stale job documents.

## Local development example

```env
HOST=0.0.0.0
PORT=3000
RUST_LOG=papermake_server=info,papermake_registry=info
MAX_CONCURRENT_RENDERS=10
RENDER_TIMEOUT_SECONDS=300
REQUEST_BODY_LIMIT_BYTES=52428800
SHARD_SIZE=500
FONTS_DIR=/fonts

S3_ENDPOINT_URL=http://localhost:9000
S3_BUCKET=papermake-templates
S3_ACCESS_KEY_ID=papermake
S3_SECRET_ACCESS_KEY=papermake-secret
S3_OP_TIMEOUT_SECONDS=20
S3_MAX_ATTEMPTS=3

PAPERMAKE_INSTANCE_ID=server-1
FLUSH_INTERVAL_SECONDS=30
FLUSH_MAX_RECORDS=1000
RENDER_RETENTION_DAYS=30

WORKER_ROLE=all
WORKER_INTERVAL_SECONDS=10
ANALYTICS_RETENTION_DAYS=30
JOB_RETENTION_DAYS=7
WORKER_LEASE_SECONDS=120
WORKER_MAX_ATTEMPTS=3
PAPERMAKE_WORKER_ID=worker-1
```
