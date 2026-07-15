# Self-hosting

Papermake runs as:

- `papermake-server`: HTTP API and web UI.
- `papermake-worker` with `WORKER_ROLE=render`: renders batch shards. Run one or
  more.
- `papermake-worker` with `WORKER_ROLE=maintenance`: aggregates analytics and
  prunes expired data. Run one.
- S3-compatible object storage.

## Run with Docker Compose

```bash
docker compose up -d
curl http://localhost:3000/health
```

Compose starts:

| Service | Role | Address |
|---|---|---|
| `papermake-server` | API and web UI | http://localhost:3000 |
| `papermake-worker` | Batch render worker | no public port |
| `papermake-maintenance` | Analytics and pruning | no public port |
| `object-store` | Local S3-compatible storage | S3 `:9000`, console http://localhost:9001 |

Useful commands:

```bash
docker compose logs -f papermake-server papermake-worker papermake-maintenance
docker compose down
docker compose down -v   # also delete local object-store data
```

For production, replace the Compose credentials, use TLS in front of public
endpoints, and point the S3 variables at your storage service. See the
[security model](../explanation/security.md) before exposing Papermake outside a
private network.

## Run from source

Start only the local object store:

```bash
docker compose up -d object-store
```

Configure the environment:

```bash
cp .env.example .env
```

Run the server and worker in separate shells:

```bash
cargo run -r -p papermake-server
cargo run -r -p papermake-worker
```

Without a worker, synchronous renders still return PDFs, but batch jobs remain
queued, analytics are not aggregated, and retention pruning does not run.

## Environment variables

All configuration comes from environment variables. `.env.example` contains a
local development configuration. See the
[configuration reference](../reference/configuration.md) for every variable,
default, and owning process.

## Fonts

Papermake uses three font sources:

1. Typst's embedded fonts.
2. Directories listed in `FONTS_DIR`.
3. Fonts bundled with a template.

Compose mounts `./fonts` at `/fonts`. Put `.ttf`, `.otf`, or `.ttc` files there
before starting the containers. The server and render workers should use the
same font set so synchronous and batch renders match.

Templates select fonts by family name:

```typst
#set text(font: "Inter")
```

## Scaling

Scale API traffic by running more `papermake-server` instances.

Scale batch throughput by running more render workers:

```bash
docker compose up -d --scale papermake-worker=4
```

Run exactly one maintenance worker in normal operation. It writes
`summary.json`, prunes expired render outputs, deletes old raw analytics, and
removes stale batch job documents.

Analytics are eventually consistent. PDF download by `render_id` is immediate
after a successful render.

## S3 storage layout

```text
<bucket>/
|-- blobs/sha256/<hash>
|-- manifests/sha256/<hash>
|-- refs/<namespace>/<tag>
|-- renders/<render_id>/
|   |-- meta.json
|   |-- pdf
|   `-- data
|-- analytics/
|   |-- raw/dt=<date>/<instance>/*.ndjson
|   `-- agg/summary.json
|-- expiry/dt=<expiry-date>/<instance>/*.ndjson
`-- jobs/<job_id>/
    |-- job.json
    `-- shards/<k>/
        |-- shard.json
        |-- inputs.json
        `-- results.json
```

Templates and assets are content-addressed. Render outputs are keyed by
`render_id` for direct lookup.
