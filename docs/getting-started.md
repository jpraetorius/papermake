# Getting started

This guide takes you from nothing to a rendered PDF in a few minutes.

## 1. Bring up the stack

```bash
git clone https://github.com/rkstgr/papermake
cd papermake
docker compose up -d      # older Docker: docker-compose up -d
```

Three services start:

| Service | Role | Address |
|---|---|---|
| `papermake-server` | HTTP API + web UI | http://localhost:3000 |
| `papermake-worker` | Rolls up analytics, prunes expired outputs | — |
| `minio` | S3-compatible object storage | API `:9000`, console http://localhost:9001 |

The server creates its S3 bucket on startup. Confirm it's up:

```bash
curl http://localhost:3000/health
```

Then open **http://localhost:3000/** for the dashboard.

> Running from source instead of Docker? See
> [Self-hosting → Run from source](self-hosting.md#run-from-source).

## 2. Publish a template

The quickest path is the JSON "publish-simple" endpoint. Input data is injected
on `sys.inputs.data`; the first line decodes it into `data` (see
[Writing templates](templates.md)).

```bash
curl -X POST "http://localhost:3000/api/templates/hello/publish-simple?tag=latest" \
  -H 'Content-Type: application/json' \
  -d '{
    "main_typ": "#let data = json(bytes(sys.inputs.data))\n= Hello #data.name\nWelcome aboard.",
    "metadata": { "name": "Hello", "author": "you@example.com" }
  }'
```

Response:

```json
{
  "data": {
    "message": "Template 'hello:latest' published successfully",
    "manifest_hash": "sha256:…",
    "reference": "hello:latest"
  },
  "message": "Template published with reference 'hello:latest'"
}
```

## 3. Render it

Rendering is two steps: run the render (returns a `render_id`), then fetch the
PDF by that id. Note the data goes under a `data` key.

```bash
# Run the render
curl -X POST "http://localhost:3000/api/render/hello:latest" \
  -H 'Content-Type: application/json' \
  -d '{"data": {"name": "Ada"}}'
# → { "data": { "render_id": "0192…", "pdf_hash": "sha256:…", "duration_ms": 12 } }

# Download the PDF by render_id
curl "http://localhost:3000/api/renders/0192…/pdf" --output hello.pdf
```

Open `hello.pdf` — you should see "Hello Ada".

## 4. Try the web UI

Open **http://localhost:3000/**:

- The **dashboard** shows 24h totals, a volume sparkline, per-template bars, and
  recent renders. (Analytics refresh on the worker's cycle, so a brand-new
  render appears after the next flush + aggregation — the PDF itself is
  downloadable immediately.)
- Click a template to open its **detail page**: edit the source, hit
  **Test Render** to see the PDF inline (no reload), or publish a new version.

## Next steps

- [Writing templates](templates.md) — schemas, assets, multi-file templates.
- [HTTP API reference](api.md) — all endpoints.
- [Analytics & retention](analytics-and-retention.md) — control how long outputs live.
