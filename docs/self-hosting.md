# Self-hosting

Papermake needs two processes and an S3-compatible object store:

- **`papermake-server`** — the HTTP API + web UI. Run one or more.
- **`papermake-worker`** — aggregates analytics and prunes expired outputs. Run
  exactly **one**, regardless of how many servers you run.
- **S3** — RustFS (bundled for local/dev), or any S3-compatible service in
  production.

## Docker Compose (recommended for local/dev)

```bash
docker compose up -d
```

Brings up `papermake-server` (port 3000), `papermake-worker`, and `rustfs`
(S3 API `:9000`, web console `:9001`). The server creates its bucket on
startup. See [Getting started](getting-started.md).

```bash
docker compose logs -f papermake-server papermake-worker
docker compose down        # add -v to also delete stored data
```

The images: server and worker are built from their `Dockerfile`s
(`crates/papermake-server/Dockerfile`, `crates/papermake-worker/Dockerfile`,
multi-stage Rust builds on Rust 1.97, distroless runtime); RustFS is pinned to a
specific release rather than `:latest`. RustFS S3 credentials come from its
`RUSTFS_ACCESS_KEY` / `RUSTFS_SECRET_KEY` env vars and must match the server's
`S3_ACCESS_KEY_ID` / `S3_SECRET_ACCESS_KEY`.

## Run from source

```bash
# 1. Start just the object store
docker compose up -d rustfs       # or: podman-compose up -d rustfs

# 2. Configure
cp .env.example .env              # defaults target local RustFS

# 3. Run the processes
cargo run -r -p papermake-server
cargo run -r -p papermake-worker  # separate shell
```

Without the worker running, renders still work and PDFs are downloadable, but
analytics (`summary.json`) won't be built and expired outputs won't be pruned.

## Environment variables

All configuration is via environment variables (see
[`.env.example`](../.env.example)).

### S3 (server and worker)

| Variable | Description |
|---|---|
| `S3_ENDPOINT_URL` | Endpoint, e.g. `http://rustfs:9000` |
| `S3_REGION` | Region, e.g. `us-east-1` |
| `S3_BUCKET` | Bucket name (created on startup if missing) |
| `S3_ACCESS_KEY_ID` | Access key |
| `S3_SECRET_ACCESS_KEY` | Secret key |

### Server

| Variable | Default | Description |
|---|---|---|
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `3000` | Bind port |
| `PAPERMAKE_INSTANCE_ID` | random uuid | Stable id used in flushed S3 keys |
| `FLUSH_INTERVAL_SECONDS` | `30` | Analytics flush interval |
| `FLUSH_MAX_RECORDS` | `1000` | Buffer size that triggers an eager flush |
| `RENDER_RETENTION_DAYS` | `30` | Global output retention default (`0` = forever) |
| `RUST_LOG` | — | Log filter, e.g. `papermake_server=debug` |

### Worker

| Variable | Default | Description |
|---|---|---|
| `WORKER_INTERVAL_SECONDS` | `60` | Aggregate + prune cadence |
| `ANALYTICS_RETENTION_DAYS` | `30` | How long to keep raw analytics NDJSON |

## Scaling

- **Servers scale horizontally.** Each buffers its own records and flushes to
  S3 under an instance-scoped key prefix, so instances never collide. Give each
  a distinct `PAPERMAKE_INSTANCE_ID`.
- **Run a single worker.** One aggregator produces one consistent `summary.json`
  for all servers. Running several is harmless but redundant.
- Analytics are eventually consistent across servers; artifact retrieval by
  `render_id` is immediate everywhere. See
  [Analytics & retention](analytics-and-retention.md).

## S3 storage layout

```
<bucket>/
├── blobs/sha256/<hash>            # template files & assets (content-addressed)
├── manifests/sha256/<hash>        # template manifests
├── refs/<namespace>/<tag>         # mutable tag → manifest hash pointers
├── renders/<render_id>/
│   ├── meta.json                  # record: success, error, sizes, timestamps
│   ├── pdf                        # rendered PDF (success only)
│   └── data                       # the input data used
├── analytics/
│   ├── raw/dt=<date>/<instance>/*.ndjson   # buffered render records
│   └── agg/summary.json                    # the aggregate the API reads
└── expiry/dt=<expiry-date>/<instance>/*.ndjson   # retention index
```

Templates, manifests, and assets are content-addressed (deduplicated). Rendered
outputs are keyed by `render_id` so by-id lookups are a direct read.

## Migrating from the old scheme

Earlier versions stored PDFs/data at `pdfs/sha256/*` and `data/sha256/*` and
render analytics in ClickHouse. Those are not reachable under the current
`renders/{id}/*` layout, and ClickHouse is no longer used. For a fresh deploy
there's nothing to do; if you have production data under the old scheme, plan a
migration before upgrading.
