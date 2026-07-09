# 📄 Papermake

**Content-addressable template registry with server-side rendering for [Typst](https://typst.app/) documents.**

Turn your Typst templates into APIs. Publish once, render anywhere — no local Typst install, no database to operate.

```bash
# 1. Bring up the stack
docker compose up -d

# 2. Publish a template
curl -X POST "http://localhost:3000/api/templates/invoice/publish-simple?tag=latest" \
  -H 'Content-Type: application/json' \
  -d '{
        "main_typ": "#let data = json(bytes(sys.inputs.data))\n= Invoice #data.number\nBill to: #data.customer",
        "metadata": { "name": "Invoice", "author": "you@company.com" }
      }'

# 3. Render it with data (returns a render_id)
curl -X POST "http://localhost:3000/api/render/invoice:latest" \
  -H 'Content-Type: application/json' \
  -d '{"data": {"number": "INV-001", "customer": "Acme Corp"}}'

# 4. Download the PDF by render_id
curl "http://localhost:3000/api/renders/<render_id>/pdf" --output invoice.pdf
```

Or just open the web UI at **http://localhost:3000/** and publish / test-render in the browser.

## 🚀 Why Papermake?

- **🏗️ Templates as code** — version document templates like software; immutable, deduplicated storage (Git-style content addressing).
- **⚡ Server-side rendering** — the Typst engine runs in the server; clients only send data.
- **🗄️ S3 is the only dependency** — templates, rendered PDFs, input data, and analytics all live in S3. No always-on database.
- **📊 Built-in analytics** — every render is logged; a background worker rolls it up into per-template volume, success rate, and p90 latency.
- **🧹 Retention built in** — outputs expire on a schedule (per-render, per-template, or global) and are pruned automatically.
- **🖥️ Server-rendered UI** — a dependency-light dashboard + template editor (KelpUI + a touch of htmx), no SPA build step.
- **🐳 Self-hostable** — `docker compose up` and you're running.

## 🏃 Quick Start

### Bring up the stack (Docker Compose)

```bash
git clone https://github.com/rkstgr/papermake
cd papermake
docker compose up -d      # older Docker: docker-compose up -d
```

This starts three services on the `papermake` network:

| Service | What it is | Where |
|---|---|---|
| **papermake-server** | HTTP API + server-rendered UI | http://localhost:3000 |
| **papermake-worker** | Aggregates analytics → `summary.json`, prunes expired outputs | (no exposed port) |
| **seaweedfs** | S3-compatible object storage (Apache-2.0) | S3 API `:8333`, master console http://localhost:9333 |

The server creates its bucket on startup. Check health with `curl http://localhost:3000/health`, then open **http://localhost:3000/** for the dashboard.

To follow logs or tear down:

```bash
docker compose logs -f papermake-server papermake-worker
docker compose down          # add -v to also wipe stored data
```

### Run from source

```bash
# 1. Start just the object store (Docker or Podman)
docker compose up -d seaweedfs     # or: podman-compose up -d seaweedfs

# 2. Configure the environment
cp .env.example .env               # defaults already point at local SeaweedFS

# 3. Run the server and (optionally) the worker
cargo run -r -p papermake-server
cargo run -r -p papermake-worker   # in a second shell, for analytics rollups
```

## 📖 Documentation

Full guides live in [`docs/`](docs/README.md):

- [Getting started](docs/getting-started.md) — from zero to a rendered PDF.
- [Writing templates](docs/templates.md) — data injection, schemas, assets, imports.
- [HTTP API reference](docs/api.md) — every endpoint with request/response shapes.
- [Analytics & retention](docs/analytics-and-retention.md) — how they work and how to configure them.
- [Self-hosting](docs/self-hosting.md) — deployment, env vars, scaling, storage layout.

## 📝 Writing a template

A template is a Typst file plus metadata (and optionally a JSON schema and extra asset files). Input data is injected as JSON on `sys.inputs.data`; the idiomatic first line decodes it into `data`:

```typst
// invoice.typ
#let data = json(bytes(sys.inputs.data))

= Invoice #data.number

*Bill to:* #data.customer.name \
*Amount:* $#data.amount
```

## 📚 Usage

All API routes are under `/api`. Health (`/health`) and the UI (`/`, `/templates/{name}`) are served at the root.

### Publish a template

Simple JSON publish (inline source):

```bash
curl -X POST "http://localhost:3000/api/templates/invoice/publish-simple?tag=latest" \
  -H 'Content-Type: application/json' \
  -d '{
    "main_typ": "#let data = json(bytes(sys.inputs.data))\n= Invoice #data.number",
    "metadata": { "name": "Customer Invoice", "author": "dev@company.com" }
  }'
```

Multipart publish (files from disk, optional schema and extra assets):

```bash
curl -X POST "http://localhost:3000/api/templates/invoice/publish?tag=latest" \
  -F "main_typ=@invoice.typ" \
  -F "schema=@schema.json" \
  -F "files[assets/logo.png]=@logo.png" \
  -F 'metadata={"name":"Professional Invoice","author":"finance@company.com"}'
```

Both return the manifest hash and reference:

```json
{
  "data": {
    "message": "Template 'invoice:latest' published successfully",
    "manifest_hash": "sha256:8e0e5843…",
    "reference": "invoice:latest"
  },
  "message": "Template published with reference 'invoice:latest'"
}
```

> **Tip:** set a per-template retention default by adding `"retain_days": 7` to `metadata` (`0` = keep forever).

### Render a document

Rendering is a two-step flow: `POST /api/render/...` runs the render and returns a `render_id`; you then fetch the PDF by id. (The PDF is written to S3 at render time, so it's fetchable immediately.)

```bash
# Render — note the data goes under a "data" key
curl -X POST "http://localhost:3000/api/render/invoice:latest" \
  -H 'Content-Type: application/json' \
  -d '{
        "data": { "number": "INV-001", "customer": {"name": "Acme Corp"}, "amount": 1500 },
        "retain_days": 14
      }'
# → { "data": { "render_id": "0192…", "pdf_hash": "sha256:…", "duration_ms": 42 } }

# Download the rendered PDF
curl "http://localhost:3000/api/renders/0192…/pdf" --output invoice.pdf
```

`retain_days` is optional and overrides the template/global default for this render. A PDF request for a render that **failed** returns `422`; an unknown or already-pruned `render_id` returns `404`.

### Analytics & history

Analytics are answered from the S3 aggregate (`summary.json`) that the worker refreshes each cycle, so they're eventually consistent across server instances.

```bash
curl "http://localhost:3000/api/renders?limit=10"                 # recent renders
curl "http://localhost:3000/api/analytics/templates"              # total renders per template
curl "http://localhost:3000/api/analytics/volume?days=30"         # render volume over time
curl "http://localhost:3000/api/analytics/performance?days=30"    # avg duration over time
```

### Web UI

- **`/`** — dashboard: 24h totals (count, success rate, p90 latency), volume sparkline, per-template bars, recent renders, template list.
- **`/templates/{name}`** — template detail: metadata/tags, an editor prefilled with the source, a **Test Render** button (htmx-powered, shows the PDF inline in an `<iframe>` with no page reload), and a publish form.

## 🗄️ How storage works

Two decoupled concerns, both on S3:

- **Artifacts** are keyed by `render_id`: `renders/{id}/meta.json`, `renders/{id}/pdf`, `renders/{id}/data`. By-id lookups are direct blob reads — immediate, no database.
- **Analytics** flow: each server buffers `RenderRecord`s in memory → flushes NDJSON to `analytics/raw/` on an interval/size threshold → the worker aggregates all raw into `analytics/agg/summary.json` and writes an `expiry/` index → the worker prunes expired outputs and old raw.

Templates, assets, and manifests keep **content addressing** (SHA-256) for dedup.

## ⚙️ Configuration

All configuration is via environment variables (see [`.env.example`](.env.example)).

**Server & worker (S3):** `S3_ENDPOINT_URL`, `S3_REGION`, `S3_BUCKET`, `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY`

**Server:** `HOST`, `PORT`, `PAPERMAKE_INSTANCE_ID`, `FLUSH_INTERVAL_SECONDS`, `FLUSH_MAX_RECORDS`, `RENDER_RETENTION_DAYS`

**Worker:** `WORKER_INTERVAL_SECONDS`, `ANALYTICS_RETENTION_DAYS`

## 🏗️ Architecture

```
                        ┌──────────────────────────────┐
  data ───▶  POST /api/render                          │
                        │  papermake-server            │
  browser ─▶  GET /  ───┤   • Typst engine (render)    │
                        │   • SSR UI (maud + htmx)      │
                        │   • buffers RenderRecords     │
                        └───────────────┬──────────────┘
                                        │ put artifacts + flush NDJSON
                                        ▼
                        ┌──────────────────────────────┐
                        │  S3 / SeaweedFS               │
                        │   renders/{id}/{meta,pdf,data}│
                        │   analytics/raw · agg · expiry│
                        │   blobs · manifests · refs    │
                        └───────────────▲──────────────┘
                                        │ aggregate + prune
                        ┌───────────────┴──────────────┐
                        │  papermake-worker             │
                        │   summary.json + retention    │
                        └──────────────────────────────┘
```

Crates:
- **`papermake`** — Typst compilation engine with a virtual filesystem.
- **`papermake-registry`** — content-addressable storage, rendering, buffered-S3 analytics, aggregator, retention.
- **`papermake-server`** — HTTP API + server-rendered UI.
- **`papermake-worker`** — analytics aggregator + output pruner.

## 🛠️ API reference

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET`  | `/health` | Health check |
| `GET`  | `/` | Dashboard UI |
| `GET`  | `/templates/{name}` | Template detail UI (editor, test render, publish) |
| `GET`  | `/api/templates` | List templates (`?limit=&offset=`) |
| `POST` | `/api/templates/{name}/publish?tag={tag}` | Publish (multipart) |
| `POST` | `/api/templates/{name}/publish-simple?tag={tag}` | Publish (JSON) |
| `GET`  | `/api/templates/{name}/tags` | List a template's tags |
| `GET`  | `/api/templates/{reference}` | Template metadata |
| `GET`  | `/api/templates/{reference}/source` | Entrypoint source (`text/plain`) |
| `POST` | `/api/render/{reference}` | Render → `{ render_id, pdf_hash, duration_ms }` |
| `GET`  | `/api/renders?limit=N&offset=M` | Recent render history |
| `GET`  | `/api/renders/{render_id}/pdf` | Download rendered PDF |
| `GET`  | `/api/analytics/volume?days=N` | Render volume over time |
| `GET`  | `/api/analytics/templates` | Total renders per template |
| `GET`  | `/api/analytics/performance?days=N` | Average render duration over time |

## 🎯 Use cases

- **Document generation APIs** — invoices, contracts, reports
- **Transactional documents** — receipts, tickets, certificates, labels
- **Report automation** — scheduled financial / analytics reports

## 🤝 Development

```bash
# Unit + doc tests (no infra required — uses in-memory storage)
cargo test --workspace

# Formatting and lints
cargo fmt --all
cargo clippy --workspace --all-targets

# Integration tests that need live S3 run against SeaweedFS:
#   docker compose up -d seaweedfs      # or: podman-compose up -d seaweedfs
cargo test --workspace -- --ignored
```

Built with Rust 🦀 • Powered by [Typst](https://typst.app/) • Inspired by Docker registry & Git's content addressing
