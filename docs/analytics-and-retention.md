# Analytics & retention

Papermake records every render and expires old outputs on a schedule. Both are
backed by S3 alone — there is no analytics database to operate.

## How analytics work

1. **Each server buffers records.** When a render finishes, a `RenderRecord`
   (timing, success, sizes, template ref) is staged in memory.
2. **The buffer flushes to S3.** On an interval (`FLUSH_INTERVAL_SECONDS`) or
   once it reaches `FLUSH_MAX_RECORDS`, records are written as NDJSON under
   `analytics/raw/dt=<date>/<instance>/…`. The buffer is also flushed on
   graceful shutdown.
3. **The worker aggregates.** `papermake-worker` periodically reads all raw
   NDJSON and writes `analytics/agg/summary.json` — per-day volume, per-day
   average duration, per-template counts + recent renders, a global recent
   list, and 24h totals (count, success rate, p90 latency).
4. **Queries read the aggregate.** `GET /api/renders` and all
   `GET /api/analytics/*` endpoints (and the dashboard) read `summary.json`.

### Consistency

Analytics are **globally eventually consistent**: every server reads the same
S3 aggregate, so there is no per-instance skew, but a brand-new render only
shows up in analytics after the next flush + aggregation cycle.

**Artifact retrieval is immediate** and independent of this: a render's PDF and
input data are keyed by `render_id` and written at render time, so
`GET /api/renders/{id}/pdf` works the moment the render returns.

## How retention works

Every render's outputs get an **expiry date**, fixed at render time. When a
render flushes, its id is also written to an expiry index
(`expiry/dt=<expiry-date>/…`). The worker prunes by listing only the
day-partitions that are due — cost scales with what's expiring, not with total
output count.

Pruning deletes `renders/{id}/{meta.json,pdf,data}` for due renders. After an
output is pruned, `GET /api/renders/{id}/pdf` returns `404`; the analytics
rollups in `summary.json` are unaffected.

### Choosing retention

Effective retention resolves by precedence — **most specific wins**:

1. **Per-render** — `retain_days` in the [render request](api.md#rendering).
2. **Per-template** — `retain_days` in the template's
   [metadata](templates.md#metadata).
3. **Global default** — `RENDER_RETENTION_DAYS` on the server.

`retain_days == 0` means **keep forever** (no expiry-index entry is written).
Retention is fixed at render time; changing a template's default later does not
re-date already-rendered outputs.

### Analytics raw retention

Separately, the worker deletes raw NDJSON older than
`ANALYTICS_RETENTION_DAYS`. This only trims the raw event log — the rolled-up
history in `summary.json` survives, so dashboards keep their long-range trends
even after raw is dropped.

## Configuration summary

| Variable | Applies to | Meaning |
|---|---|---|
| `FLUSH_INTERVAL_SECONDS` | server | How often to flush the buffer to S3 |
| `FLUSH_MAX_RECORDS` | server | Buffer size that triggers an eager flush |
| `RENDER_RETENTION_DAYS` | server | Global output retention default (`0` = forever) |
| `PAPERMAKE_INSTANCE_ID` | server | Stable id used in flushed object keys |
| `WORKER_INTERVAL_SECONDS` | worker | Aggregate + prune cadence |
| `ANALYTICS_RETENTION_DAYS` | worker | How long to keep raw analytics NDJSON |

See [Self-hosting](self-hosting.md) for the full environment reference.
