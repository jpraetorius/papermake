# HTTP API reference

Base URL in local dev: `http://localhost:3000`. All JSON API routes are under
`/api`; `/health` and the web UI (`/`, `/templates/{name}`) are at the root.

- Request/response bodies are JSON unless noted.
- Successful single-object responses are wrapped as `{ "data": …, "message"?: … }`.
- List responses are paginated: `{ "data": [...], "pagination": { "limit", "offset", "total", "has_more" } }`.
- Errors return `{ "error": "…", "status": <code> }`.

## Health

### `GET /health`
Returns service status, version, and timestamp.

## Templates

### `GET /api/templates`
List templates. Query: `limit` (default 50), `offset` (default 0).

```json
{
  "data": [
    {
      "name": "invoice",
      "namespace": null,
      "tags": ["latest", "v1"],
      "latest_manifest_hash": "sha256:…",
      "metadata": { "name": "Invoice", "author": "you@example.com" }
    }
  ],
  "pagination": { "limit": 50, "offset": 0, "total": 1, "has_more": false }
}
```

### `POST /api/templates/{name}/publish`
Publish via `multipart/form-data`. Query: `tag` (default `latest`).

| Field | Required | Description |
|---|---|---|
| `main_typ` | yes | The main template file |
| `metadata` | yes | JSON with `name`, `author`, optional `retain_days` |
| `schema` | no | JSON schema file |
| `files[<path>]` | no | Additional files, repeatable (e.g. `files[assets/logo.png]`) |

```bash
curl -X POST "http://localhost:3000/api/templates/invoice/publish?tag=latest" \
  -F "main_typ=@invoice.typ" \
  -F 'metadata={"name":"Invoice","author":"you@example.com"}'
```

### `POST /api/templates/{name}/publish-simple`
Publish via JSON. Query: `tag` (default `latest`).

```json
{
  "main_typ": "#let data = json(bytes(sys.inputs.data))\n= Hi #data.name",
  "schema": { "type": "object" },
  "metadata": { "name": "Greeting", "author": "you@example.com", "retain_days": 7 }
}
```

Both publish endpoints respond with:

```json
{
  "data": { "message": "…", "manifest_hash": "sha256:…", "reference": "invoice:latest" },
  "message": "Template published with reference 'invoice:latest'"
}
```

### `GET /api/templates/{name}/tags`
List a template's tags: `{ "data": ["latest", "v1"] }`.

### `GET /api/templates/{reference}`
Template metadata for a reference (`name`, `name:tag`, `namespace/name[:tag]`).

```json
{
  "data": {
    "name": "invoice",
    "namespace": null,
    "tag": "latest",
    "tags": ["latest"],
    "manifest_hash": "sha256:…",
    "metadata": { "name": "Invoice", "author": "you@example.com" },
    "reference": "invoice:latest"
  }
}
```

### `GET /api/templates/{reference}/source`
The entrypoint (`main.typ`) source as `text/plain`. Used by the editor.

## Rendering

### `POST /api/render/{reference}`
Render a template. The render's PDF and input data are written to S3
immediately (keyed by `render_id`); this endpoint returns metadata, not the PDF.

Request:

```json
{
  "data": { "number": "INV-001", "customer": { "name": "Acme" } },
  "retain_days": 14
}
```

- `data` (required) — injected into the template as `sys.inputs.data`.
- `retain_days` (optional) — overrides the template/global retention for this
  render (`0` = keep forever). See [Analytics & retention](analytics-and-retention.md).

Response:

```json
{ "data": { "render_id": "0192…", "pdf_hash": "sha256:…", "duration_ms": 42 } }
```

Rendering is CPU-bound and runs under a concurrency limit
(`MAX_CONCURRENT_RENDERS`) with a deadline (`RENDER_TIMEOUT_SECONDS`, which
includes time spent waiting for a free render slot). Errors:

- **`422`** — the template failed to compile (a failure record is still logged).
- **`408`** — the render timed out (busy server or a slow/expensive template).

### `POST /api/render/{reference}/batch`
Submit an **async batch**: render one template against many inputs. Returns
`202 Accepted` with a `job_id` immediately; the job is durably enqueued in S3
and the **worker** claims and renders it (one warm Typst world — fonts + layout
memoization stay hot, imports fetched once). Poll the job and fetch each PDF by
`render_id`.

Request:

```json
{
  "inputs": [
    { "data": { "number": "INV-001" }, "key": "cust-a" },
    { "data": { "number": "INV-002" } }
  ],
  "retain_days": 30
}
```

- `inputs[].data` (required) — payload injected as `sys.inputs.data`.
- `inputs[].key` (optional) — caller-chosen label echoed back on the result item.
- `retain_days` (optional) — retention applied to every render in the batch.

Response:

```json
{ "data": { "job_id": "0192…", "total": 2, "status_url": "/api/jobs/0192…" } }
```

### `GET /api/jobs/{job_id}`
Poll a batch job. The job document is persisted in S3, so this returns the full
result whether the job is still running **or already finished**. Map each result
back to its input by `index` (position) or by your `key`; fetch its PDF at
`GET /api/renders/{render_id}/pdf` (you can pull completed items before the whole
job finishes).

```json
{ "data": {
  "job_id": "0192…", "reference": "invoice:latest",
  "status": "running",            // queued | running | completed | failed
  "total": 2, "done": 1, "failed": 0,
  "items": [
    { "index": 0, "key": "cust-a", "render_id": "0192a…", "status": "success" },
    { "index": 1, "render_id": null, "status": "pending" }
  ]
}}
```

> The worker claims a job with an owner + lease and heartbeats it while
> rendering. If that worker dies mid-run, the lease expires and the (single
> active) worker reclaims the job on its next cycle and **resumes** the
> remaining items — already-rendered items keep their `render_id`, so no work is
> repeated and nothing gets stuck in `running`. A job that repeatedly crashes
> the worker is marked `failed` after a few attempts. Because state lives in S3,
> a client that only polls after completion still gets every `render_id`.

## Renders & history

### `GET /api/renders`
Recent renders (newest first), paginated. Query: `limit`, `offset`.
Each item is a render record: `render_id`, `timestamp`, `template_ref`,
`template_name`, `template_tag`, `manifest_hash`, `success`, `duration_ms`,
`pdf_size_bytes`, `error`, `expiry_date`.

> Answered from the S3 aggregate the worker maintains, so a just-completed
> render appears after the next flush + aggregation cycle.

### `GET /api/renders/{render_id}/pdf`
Download the rendered PDF (`application/pdf`).

- **404** — unknown or already-pruned `render_id`.
- **422** — the render exists but failed, so there is no PDF.

## Analytics

Backed by the S3 aggregate (`summary.json`). Responses are the serialized
`AnalyticsResult` enum (externally tagged).

### `GET /api/analytics/volume?days=N`
```json
{ "Volume": [ { "date": "2026-07-09", "renders": 5 } ] }
```

### `GET /api/analytics/templates`
```json
{ "Templates": [ { "template_name": "invoice", "total_renders": 42 } ] }
```

### `GET /api/analytics/performance?days=N`
```json
{ "Duration": [ { "date": "2026-07-09", "avg_duration_ms": 38.5 } ] }
```

## Web UI (server-rendered)

Not JSON — HTML pages and htmx fragments:

| Route | Description |
|---|---|
| `GET /` | Dashboard |
| `GET /templates/{name}` | Template detail (editor, test render, publish) |
| `POST /ui/templates/{name}/render` | htmx test-render fragment (returns HTML) |
| `POST /ui/templates/{name}/publish` | Publish form → redirect |
| `GET /assets/app.css`, `GET /assets/htmx.min.js` | Stylesheet + htmx, embedded in the binary |
