# Self-hosting

Papermake needs two processes and an S3-compatible object store:

- **`papermake-server`** — the HTTP API + web UI. Run one or more.
- **`papermake-worker`** — the same binary in one of two roles (`WORKER_ROLE`):
  - **render** — polls for batch shards and renders them; **scale these** to
    split large batches (see [Scaling](#scaling)).
  - **maintenance** — aggregates analytics into `summary.json` and prunes
    expired outputs/jobs; run **one**.
  - **all** (default) — both in one process, for simple single-node/dev.
- **S3** — RustFS (bundled for local/dev), or any S3-compatible service in
  production.

## Docker Compose (recommended for local/dev)

```bash
docker compose up -d
```

Brings up `papermake-server` (port 3000), `papermake-worker`, and `rustfs`
(S3 API `:9000`, web console `:9001`). The server creates its bucket on
startup. See [Getting started](getting-started.md).

Fonts are supplied by the mounted `./fonts` directory (gitignored, so empty on a
fresh clone). Drop the TTF/OTF/TTC files your templates need in there before
starting — Typst's embedded fonts are always available regardless.

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

Without the worker running, single renders still work and PDFs are downloadable,
but **batch jobs stay `queued`**, analytics (`summary.json`) won't be built, and
expired outputs won't be pruned.

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
| `MAX_CONCURRENT_RENDERS` | `10` | Maximum CPU-bound Typst renders running at once per server |
| `RENDER_TIMEOUT_SECONDS` | `300` | Render timeout, including queue wait for a render slot |
| `SHARD_SIZE` | `500` | Items per batch shard (unit of work a worker claims) |
| `PAPERMAKE_INSTANCE_ID` | random uuid | Stable id used in flushed S3 keys |
| `FLUSH_INTERVAL_SECONDS` | `30` | Analytics flush interval |
| `FLUSH_MAX_RECORDS` | `1000` | Buffer size that triggers an eager flush |
| `RENDER_RETENTION_DAYS` | `30` | Global output retention default (`0` = forever) |
| `FONTS_DIR` | `/fonts` in Docker; unset otherwise | One or more directories (`:`-separated) of TTF/OTF/TTC files scanned at startup; missing dirs are skipped. Compose mounts the repo's `./fonts` here; add more without rebuilding. |
| `RUST_LOG` | — | Log filter, e.g. `papermake_server=debug` |

### Worker

| Variable | Default | Description |
|---|---|---|
| `WORKER_ROLE` | `all` | `render` (poll + render shards; scale these), `maintenance` (aggregate + prune; run one), or `all` (both) |
| `WORKER_INTERVAL_SECONDS` | `60` | `render`: idle shard-poll cadence. `maintenance`: aggregate + prune cadence |
| `ANALYTICS_RETENTION_DAYS` | `30` | [maintenance] How long to keep raw analytics NDJSON |
| `JOB_RETENTION_DAYS` | `7` | [maintenance] How long to keep batch-job docs (`0` = keep forever) |
| `WORKER_LEASE_SECONDS` | `120` | [render] Shard lease; a dead worker's shard is reclaimable after this |
| `WORKER_MAX_ATTEMPTS` | `3` | [render] Give up on a shard (mark `failed`) after this many claims |
| `PAPERMAKE_WORKER_ID` | hostname, then PID | Unique per process (shard-claim owner + analytics instance); scaled replicas auto-differ |
| `RENDER_RETENTION_DAYS` | `30` | [render] Retention for batch-rendered outputs |
| `FONTS_DIR` | `/fonts` in Docker; unset otherwise | Same as the server — the worker renders batch jobs, so it needs the same fonts |

## Fonts

Renders use three font sources, all merged:
1. **Typst's embedded fonts** — always available.
2. **`FONTS_DIR`** — one or more directories scanned at startup. Nothing is
   baked into the image beyond Typst's embedded set; instead the image sets
   `FONTS_DIR=/fonts` and Compose mounts the repo's `./fonts` there, so you add
   fonts by dropping files in the mount without rebuilding. Set `FONTS_DIR` to a
   `:`-separated list of directories.
3. **Template-bundled fonts** — any `.ttf`/`.otf`/`.ttc` file in a template's
   bundle is registered for that template's renders (see
   [Writing templates](templates.md)), so templates can be self-contained.

Both the server (single renders) and the worker (batch renders) need the same
fonts — they mount the same `/fonts` volume and set the same `FONTS_DIR`.
Templates reference fonts by **family name** (`#set text(font: "…")`); Typst
supports TTF/OTF/TTC only (no WOFF/WOFF2).

## Scaling

- **Servers scale horizontally.** Each buffers its own records and flushes to
  S3 under an instance-scoped key prefix, so instances never collide. Give each
  a distinct `PAPERMAKE_INSTANCE_ID`.
- **Render workers scale horizontally.** A batch is split into `SHARD_SIZE`-item
  shards; each render worker claims shards independently (owner + lease), so more
  workers finish a big batch faster
  (`podman compose up -d --scale papermake-worker=N`). No atomic claim is needed:
  render output is content-addressed, so if two workers ever briefly claim the
  same shard the duplicate work is harmless (identical outputs, deduped
  analytics). A dead worker's shard is reclaimed after its lease expires and
  resumes only the items whose output doesn't yet exist. Each replica needs a
  distinct `PAPERMAKE_WORKER_ID` — leave it unset and it defaults to the
  container hostname, so scaled replicas differ automatically.
- **Run one maintenance worker.** Aggregation (`summary.json`) and pruning run on
  their own cadence there, independent of render load — so stats stay fresh even
  while render workers are saturated with a large batch. It's idempotent, so a
  brief second instance during a rolling restart is harmless, but you normally
  want exactly one.
- Analytics are eventually consistent across servers/workers; artifact retrieval
  by `render_id` is immediate everywhere. See
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
├── expiry/dt=<expiry-date>/<instance>/*.ndjson   # retention index
└── jobs/<job_id>/
    ├── job.json                    # immutable batch metadata (total, shards)
    └── shards/<k>/
        ├── shard.json              # shard status/owner/lease/counts
        ├── inputs.json             # this shard's slice of the inputs
        └── results.json            # per-item render_ids (written when done)
```

Templates, manifests, and assets are content-addressed (deduplicated). Rendered
outputs are keyed by `render_id` so by-id lookups are a direct read.

The worker keeps time-based prefixes bounded each cycle: `renders/` are pruned
once their per-render retention expires (via the `expiry/` index), `analytics/raw`
rolls off after `ANALYTICS_RETENTION_DAYS`, and `jobs/` status docs after
`JOB_RETENTION_DAYS`. `blobs/`/`manifests/` persist as template content (orphans
are collected when a version is deleted); `refs/` are small mutable pointers.

## Migrating from the old scheme

Earlier versions stored PDFs/data at `pdfs/sha256/*` and `data/sha256/*` and
render analytics in ClickHouse. Those are not reachable under the current
`renders/{id}/*` layout, and ClickHouse is no longer used. For a fresh deploy
there's nothing to do; if you have production data under the old scheme, plan a
migration before upgrading.
