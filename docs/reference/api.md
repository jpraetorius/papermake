# HTTP API reference

Local base URL: `http://localhost:3000`.

JSON API routes live under `/api`. Health and the server-rendered UI live at the
root.

- JSON responses wrap successful single objects as `{ "data": ... }`.
- List responses add `pagination`.
- Errors use `{ "error": "...", "status": <code> }`.
- The generated OpenAPI 3.1 spec is available at `GET /api/openapi.json`.

## Health

### `GET /health`

Returns service status, version, and timestamp.

## Templates

For the reference string, manifest, metadata, schema, and bundle model, see the
[template reference](templates.md).

### `GET /api/templates`

Lists templates.

| Query | Default | Meaning |
|---|---:|---|
| `limit` | `50` | Page size |
| `offset` | `0` | Number of items to skip |

```json
{
  "data": [
    {
      "name": "invoice",
      "namespace": null,
      "tags": ["latest"],
      "latest_manifest_hash": "sha256:...",
      "metadata": { "name": "Invoice", "author": "you@example.com" }
    }
  ],
  "pagination": { "limit": 50, "offset": 0, "total": 1, "has_more": false }
}
```

### `POST /api/templates/{name}/publish`

Publishes a template from `multipart/form-data`.

| Query | Default | Meaning |
|---|---:|---|
| `tag` | `latest` | Tag to create or move |

| Field | Required | Meaning |
|---|---|---|
| `main_typ` | yes | Main Typst file |
| `metadata` | yes | JSON metadata: `name`, `author`, optional `retain_days` |
| `schema` | no | JSON schema file |
| `files[<path>]` | no | Additional bundled file; repeatable |

```bash
curl -X POST "http://localhost:3000/api/templates/invoice/publish?tag=latest" \
  -F "main_typ=@invoice.typ" \
  -F "files[assets/logo.png]=@logo.png" \
  -F 'metadata={"name":"Invoice","author":"you@example.com"}'
```

The publish endpoint responds with:

```json
{
  "data": {
    "message": "...",
    "manifest_hash": "sha256:...",
    "reference": "invoice:latest"
  },
  "message": "Template published with reference 'invoice:latest'"
}
```

### `GET /api/templates/{name}/tags`

Lists tags for one template.

```json
{ "data": ["latest", "v1"] }
```

### `GET /api/templates/{reference}`

Returns metadata for `name`, `name:tag`, or `namespace/name[:tag]`.

```json
{
  "data": {
    "name": "invoice",
    "namespace": null,
    "tag": "latest",
    "tags": ["latest"],
    "manifest_hash": "sha256:...",
    "metadata": { "name": "Invoice", "author": "you@example.com" },
    "reference": "invoice:latest"
  }
}
```

### `GET /api/templates/{reference}/source`

Returns the template entrypoint source as `text/plain`.

## Rendering

### `POST /api/render/{reference}`

Renders a template and returns metadata. Fetch the PDF separately by
`render_id`.

```json
{
  "data": { "number": "INV-001", "customer": { "name": "Acme" } },
  "retain_days": 14,
  "pdf_standard": "a-3b"
}
```

| Field | Required | Meaning |
|---|---|---|
| `data` | yes | JSON made available to Typst templates as `data` |
| `retain_days` | no | Output retention override; `0` keeps forever |
| `pdf_standard` | no | `1.7`, `2.0`, `a-2a`, `a-2b`, `a-3a`, `a-3b`, `a-4`, or `ua-1` |

```json
{ "data": { "render_id": "0192...", "pdf_hash": "sha256:...", "duration_ms": 42 } }
```

The `render_id` is derived from the template version and input data. Re-rendering
the same input against the same version returns the same id.

Common errors:

| Status | Meaning |
|---:|---|
| `404` | Template reference not found |
| `408` | Render timed out |
| `422` | Template compiled or exported unsuccessfully |

### `POST /api/render/{reference}/batch`

Submits an async batch job for one template and many inputs. Workers render the
job in shards.

```json
{
  "inputs": [
    { "data": { "number": "INV-001" }, "key": "cust-a" },
    { "data": { "number": "INV-002" } }
  ],
  "retain_days": 30,
  "pdf_standard": "1.7"
}
```

| Field | Required | Meaning |
|---|---|---|
| `inputs[].data` | yes | JSON passed to Typst |
| `inputs[].key` | no | Caller label echoed on the item result |
| `retain_days` | no | Retention applied to every item |
| `pdf_standard` | no | PDF standard applied to every item |

Response status: `202 Accepted`.

```json
{ "data": { "job_id": "0192...", "total": 2, "status_url": "/api/jobs/0192..." } }
```

## Jobs

### `GET /api/jobs/{job_id}`

Returns batch status and counts.

```json
{
  "data": {
    "job_id": "0192...",
    "reference": "invoice:latest",
    "status": "running",
    "total": 100000,
    "done": 42500,
    "failed": 3,
    "num_shards": 200,
    "shards_terminal": 85
  }
}
```

`status` is `queued`, `running`, `completed`, or `failed`.

### `GET /api/jobs/{job_id}/items`

Returns a page of item results ordered by input index.

| Query | Default | Meaning |
|---|---:|---|
| `limit` | `1000` | Page size |
| `offset` | `0` | Number of items to skip |

```json
{
  "data": [
    { "index": 0, "key": "cust-a", "render_id": "0192a...", "status": "success" },
    { "index": 1, "render_id": "0192b...", "status": "failed" }
  ]
}
```

Fetch successful PDFs at `GET /api/renders/{render_id}/pdf`.

## Renders

### `GET /api/renders`

Lists recent render records from the analytics aggregate.

| Query | Default | Meaning |
|---|---:|---|
| `limit` | `50` | Page size |
| `offset` | `0` | Number of items to skip |

Each record includes `render_id`, timestamp, template reference, manifest hash,
success flag, duration, PDF size, error, and expiry date.

### `GET /api/renders/{render_id}/pdf`

Downloads the rendered PDF as `application/pdf`.

| Status | Meaning |
|---:|---|
| `200` | PDF returned |
| `404` | Unknown or pruned render id |
| `422` | Render exists but failed |

## Analytics

Analytics endpoints read `analytics/agg/summary.json`.

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

## Web UI

These routes return HTML:

| Route | Meaning |
|---|---|
| `GET /` | Dashboard |
| `GET /templates` | Template list |
| `GET /templates/new` | New template form |
| `GET /templates/{reference}` | Template detail |
| `POST /ui/templates` | Create from UI form |
| `POST /ui/templates/{name}/render` | htmx test-render fragment |
| `POST /ui/templates/{name}/publish` | Publish from UI form |
| `POST /ui/templates/{name}/delete` | Delete from UI form |
| `GET /assets/app.css` | Embedded stylesheet |
| `GET /assets/htmx.min.js` | Embedded htmx |
| `GET /assets/logo.svg` | Embedded logo |
