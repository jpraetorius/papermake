# Analytics & Data retention

Papermake records render activity and expires old outputs using S3-compatible
object storage.

## Analytics model

Render analytics flow through four steps:

1. A render finishes and its record is buffered in memory.
2. The buffer flushes NDJSON files to `analytics/raw/...` on an interval or when
   it reaches `FLUSH_MAX_RECORDS`.
3. The maintenance worker folds raw records into `analytics/agg/summary.json`.
4. The dashboard, `GET /api/renders`, and `GET /api/analytics/*` read
   `summary.json`.

This means analytics are eventually consistent. A new render appears in charts
and history after the next server flush and maintenance aggregation.

## Data Retention

Every render gets an expiry decision at render time. Papermake writes
renders into an expiry index under `expiry/dt=<date>/...`; the maintenance
worker later scans due date partitions and deletes the matching render
artifacts.

Retention precedence is:

1. `retain_days` on the render request.
2. `retain_days` in the template metadata.
3. `RENDER_RETENTION_DAYS` from the environment.

`retain_days: 0` means keep forever. Changing a template or environment default
does not change the expiry date of outputs that already exist.

When an output expires, Papermake deletes `renders/{id}/meta.json`,
`renders/{id}/pdf`, and `renders/{id}/data`. Analytics rollups remain, so a
historical render can still appear in charts even though its PDF returns `404`.

## Raw analytics retention

`ANALYTICS_RETENTION_DAYS` controls how long the maintenance worker keeps raw
NDJSON records. Deleting raw records does not delete `summary.json`.

## Configuration

| Variable | Process | Meaning |
|---|---|---|
| `FLUSH_INTERVAL_SECONDS` | server | Time between analytics flushes |
| `FLUSH_MAX_RECORDS` | server | Buffer size that triggers a flush |
| `PAPERMAKE_INSTANCE_ID` | server | Stable id used in raw analytics keys |
| `RENDER_RETENTION_DAYS` | server, worker | Default output retention |
| `WORKER_INTERVAL_SECONDS` | worker | Render poll or maintenance cadence |
| `ANALYTICS_RETENTION_DAYS` | maintenance worker | Raw analytics retention |
| `JOB_RETENTION_DAYS` | maintenance worker | Batch job document retention |

See the [configuration reference](../reference/configuration.md) for the full
environment reference.
