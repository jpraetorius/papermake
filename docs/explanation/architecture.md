# Architecture

Papermake is a small set of stateless processes around one shared
S3-compatible object store. The object store is the source of truth for
templates, rendered artifacts, batch jobs, analytics, and expiry indexes.

The design goal is to keep document rendering easy to operate: run HTTP
servers for user traffic, run workers for background work, and avoid a separate
application database.

## Building blocks

### Server

`papermake-server` serves the HTTP API and web UI.

It handles:

- template publish, lookup, source, and delete requests
- synchronous render requests
- batch job submission
- PDF download by `render_id`
- dashboard and template editor pages
- buffering render analytics before flushing them to S3

Synchronous renders happen in the server process because the caller is waiting
for the result. Batch renders are only enqueued by the server; render workers do
the batch work.

### Render worker

`papermake-worker` with `WORKER_ROLE=render` processes batch shards.

Render workers:

- poll S3 for claimable shards
- claim one shard at a time with an owner and lease
- render each item in the shard
- write item results back to S3

You can run multiple render workers. A large batch is split into independent
shards, so workers can process different shards in parallel.

### Maintenance worker

`papermake-worker` with `WORKER_ROLE=maintenance` keeps derived data current and
old data bounded.

The maintenance worker:

- aggregates raw render records into `analytics/agg/summary.json`
- prunes expired render outputs from `renders/`
- deletes old raw analytics records
- deletes stale batch job documents

Run one maintenance worker in normal operation. The work is designed to be
repeatable, but multiple maintenance workers add redundant load on the S3 storage.

### S3-compatible storage

Papermake stores durable state in S3-compatible object storage.

The important keyspaces are:

```text
blobs/sha256/<hash>                         template files and assets
manifests/sha256/<hash>                     template manifests
refs/<namespace>/<tag>                      mutable tag pointers
renders/<render_id>/{meta.json,pdf,data}    render artifacts
jobs/<job_id>/...                           batch job state
analytics/raw/...                           raw render records
analytics/agg/summary.json                  aggregated analytics
expiry/dt=<date>/...                        retention index
```

This makes storage portable and simple to inspect. It also means there is no
database transaction boundary across the system; instead, Papermake uses
content addressing, idempotent writes, and background aggregation.

## Template model

Publishing a template stores files and metadata as immutable content.

1. Each file is hashed and stored under `blobs/sha256/<hash>`.
2. A manifest records the entrypoint, file hashes, and metadata.
3. The manifest is hashed and stored under `manifests/sha256/<hash>`.
4. A tag under `refs/...` points to the current manifest hash.

Tags are mutable. Manifests and blobs are immutable.

This gives Papermake two useful properties:

- identical files and manifests are deduplicated
- a manifest hash identifies an exact template version

Stored objects:

| Object | Format | Contains |
|---|---|---|
| `blobs/sha256/<hash>` | bytes | Template source files, schemas, assets, and bundled fonts |
| `manifests/sha256/<hash>` | JSON | `entrypoint`, `files` path-to-hash map, and `metadata` |
| `refs/<namespace>/<tag>` | text | Manifest hash that the tag currently points to |

## Render model

A render combines a template version, input data, and render options.

For a successful render, Papermake writes:

```text
renders/<render_id>/meta.json
renders/<render_id>/pdf
renders/<render_id>/data
```

Those files serve different read paths:

| Object | Format | Contains |
|---|---|---|
| `meta.json` | JSON | Render record: id, timestamps, template reference, hashes, success flag, duration, size, error, and expiry date |
| `pdf` | PDF bytes | The rendered document; present only for successful renders |
| `data` | JSON bytes | The exact input data used for the render |

The `render_id` is content-addressed from the template manifest hash, input data
hash, and PDF export options. Rendering the same data against the same template
version and options returns the same `render_id`.

This makes renders idempotent. If the same render is retried, Papermake writes
the same output keys rather than creating a duplicate output.

## Batch model

A batch job renders one template against many inputs.

When a batch is submitted, the server writes:

```text
jobs/<job_id>/job.json
jobs/<job_id>/shards/<k>/shard.json
jobs/<job_id>/shards/<k>/inputs.json
```

Batch state is split across small documents:

| Object | Format | Contains |
|---|---|---|
| `job.json` | JSON | Immutable job metadata: id, template reference, item count, retention, PDF standards, shard size, shard count, creation time |
| `shards/<k>/inputs.json` | JSON | This shard's input slice: each item has `data` and optional `key` |
| `shards/<k>/shard.json` | JSON | Shard status: owner, lease, attempt count, item counts, and timestamps |
| `shards/<k>/results.json` | JSON | Per-item results: input index, optional key, render id, and status |

Each shard is an independent unit of work. Render workers claim shards by
writing an owner and lease to `shard.json`. When a worker finishes a shard, it
writes `results.json` with each item's status and `render_id`.

If a worker dies, its lease expires. Another worker can reclaim the shard and
resume it. Because render ids are content-addressed, already-written outputs can
be reused safely.

Overall job status is derived from shard state. There is no single mutable
progress document that every worker must update.

## Analytics model

Render analytics are separate data.

Servers buffer render records and flush them to:

```text
analytics/raw/dt=<render-date>/<instance>/*.ndjson
```

The maintenance worker reads raw records and writes:

```text
analytics/agg/summary.json
```

Analytics files:

| Object | Format | Contains |
|---|---|---|
| `analytics/raw/.../*.ndjson` | NDJSON | One render record per line |
| `analytics/agg/summary.json` | JSON | Generated time, daily volume, daily duration, latency histogram, per-template rollups, recent renders, and 24h totals |

The dashboard and analytics API read `summary.json`. This means analytics are
eventually consistent: a render appears in charts after the next flush and
aggregation cycle.


## Retention model

Retention is decided when a render is created.

Papermake resolves retention in this order:

1. render request `retain_days`
2. template metadata `retain_days`
3. environment default `RENDER_RETENTION_DAYS`

If the resolved value is `0`, the render is kept forever and no expiry-index
entry is written.

For expiring renders, Papermake writes the future expiry decision at render time
under:

```text
expiry/dt=<expiry-date>/...
```

Expiry files:

| Object | Format | Contains |
|---|---|---|
| `expiry/dt=<expiry-date>/<instance>/*.ndjson` | line-delimited text | Render ids due for pruning on that date |

The maintenance worker scans due expiry partitions and deletes the matching
render artifacts. Before deleting, it re-reads each render's `meta.json` and
keeps any whose recorded expiry is now unset or in the future: re-rendering the
same request overwrites `meta.json` in place but leaves the original expiry-index
entry behind, so the current `meta.json` — not the index — decides. Analytics
rollups can still mention a historical render after its PDF has expired.

## Consistency

Papermake has two read paths with different freshness guarantees.

Direct artifact reads are immediate:

- `GET /api/renders/{render_id}/pdf`
- render metadata by `render_id`

Aggregated reads are eventually consistent:

- `GET /api/renders`
- `GET /api/analytics/*`
- dashboard charts and recent-render tables

This split is deliberate. Users can download a PDF as soon as the render
returns, while analytics are updated asynchronously.

## Process layout

A typical deployment looks like this:

```text
clients
  |
  v
papermake-server  ---- writes/reads ---->  S3-compatible storage
  |                                           ^
  | enqueue batch jobs                        |
  v                                           |
render workers -------------------------------+
  |
  | write render outputs and shard results
  v
S3-compatible storage

maintenance worker ---- aggregates/prunes ----> S3-compatible storage
```

Scale servers for API and UI traffic. Scale render workers for batch throughput.
Keep one maintenance worker for aggregation and cleanup.
