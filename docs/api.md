# HTTP API reference

Base URL in local dev: `http://localhost:3000`. All JSON API routes are under
`/api`; `/health` and the web UI (`/`, `/templates/{name}`) are at the root.

- Request/response bodies are JSON unless noted.
- Successful single-object responses are wrapped as `{ "data": …, "message"?: … }`.
- List responses are paginated: `{ "data": [...], "pagination": { "limit", "offset", "total", "has_more" } }`.
- Errors return `{ "error": "…", "status": <code> }`.

## OpenAPI spec

A generated OpenAPI 3.1 document is served at **`GET /api/openapi.json`** (kept
in sync with the implementation). The server does not bundle a docs UI — point
your own OpenAPI client at that URL (Scalar, Swagger UI, Redoc, or a code
generator such as `openapi-generator`).

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
- `pdf_standard` (optional) — output PDF standard: `"1.7"` (default), `"2.0"`,
  `"a-2a"`, `"a-2b"`, `"a-3a"`, `"a-3b"`, `"a-4"` (archival) or `"ua-1"`
  (accessibility). Note: `ua-1` requires the template to set a document title —
  see [Writing templates → PDF standards](templates.md#pdf-standards-archival--accessibility).

Response:

```json
{ "data": { "render_id": "0192…", "pdf_hash": "sha256:…", "duration_ms": 42 } }
```

The `render_id` is **content-addressed** — derived from the template version and
the input data — so rendering the same data against the same template version
again returns the **same** `render_id` and reuses the existing output. Identical
renders are idempotent: one stored output, one history/analytics entry (not one
per call).

Rendering is CPU-bound and runs under a concurrency limit
(`MAX_CONCURRENT_RENDERS`) with a deadline (`RENDER_TIMEOUT_SECONDS`, which
includes time spent waiting for a free render slot). Errors:

- **`422`** — the template failed to compile (a failure record is still logged).
- **`408`** — the render timed out (busy server or a slow/expensive template).

### `POST /api/render/{reference}/batch`
Submit an **async batch**: render one template against many inputs. Returns
`202 Accepted` with a `job_id` immediately; the job is durably enqueued in S3,
split into `SHARD_SIZE`-item **shards**, and **workers** claim shards to render
them (each with a warm Typst world — fonts + layout memoization hot, imports
fetched once). Run multiple workers to split a large batch across them. Poll the
job for progress and fetch each PDF by `render_id`.

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
- `pdf_standard` (optional) — output PDF standard applied to every render in the
  batch; same values as the single render above.

Response:

```json
{ "data": { "job_id": "0192…", "total": 2, "status_url": "/api/jobs/0192…" } }
```

### `GET /api/jobs/{job_id}`
Poll a batch job's **aggregated** status and counts, derived from its shard
descriptors — cheap regardless of batch size.

```json
{ "data": {
  "job_id": "0192…", "reference": "invoice:latest",
  "status": "running",            // queued | running | completed | failed
  "total": 100000, "done": 42500, "failed": 3,
  "num_shards": 200, "shards_terminal": 85
}}
```

### `GET /api/jobs/{job_id}/items?offset=0&limit=1000`
A page of the item→`render_id` mapping, ordered by input `index`. Map each item
back by `index` (position) or by your `key`; fetch its PDF at
`GET /api/renders/{render_id}/pdf`. Only items in **completed shards** appear,
so poll until `status` is `completed` for the full set. Paginated so a 100k-item
batch never returns one giant document.

```json
{ "data": [
  { "index": 0, "key": "cust-a", "render_id": "0192a…", "status": "success" },
  { "index": 1, "render_id": "0192b…", "status": "failed" }
] }
```

> Workers claim shards with an owner + lease and heartbeat while rendering. If a
> worker dies mid-shard, the lease expires and another worker reclaims it,
> **resuming** only items whose (content-addressed) output doesn't yet exist —
> so no work is repeated and nothing gets stuck. A shard that repeatedly crashes
> a worker is marked failed after a few attempts. No compare-and-set is needed:
> identical renders are idempotent, so a rare double-claim just wastes CPU.

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
