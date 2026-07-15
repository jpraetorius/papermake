# Agent Notes

Papermake is a self-hosted rendering service for Typst documents. It turns
versioned Typst templates into HTTP render APIs, stores templates and render
artifacts in S3-compatible object storage, and can render single documents
synchronously or batch jobs through workers.

Use the product docs as the source of truth:

- `README.md`: project overview and documentation links.
- `docs/README.md`: documentation map.
- `docs/explanation/architecture.md`: system model, storage layout, render
  model, batch model, analytics, and retention.
- `docs/reference/api.md`: HTTP API behavior.
- `docs/reference/configuration.md`: environment variables and defaults.
- `docs/reference/templates.md`: template references, manifests, metadata,
  schemas, bundle paths, and font rules.
- `docs/how-to/operations.md`: production runbooks.
- `docs/explanation/security.md`: trust boundary and deployment risks.

If this file disagrees with the code or those docs, prefer the code first and
the relevant documentation second.

## Repository Layout

- `crates/papermake`: core Typst rendering integration and filesystem support.
- `crates/papermake-registry`: content-addressed template registry, S3 storage,
  manifests, references, render storage, analytics, batch jobs, and retention.
- `crates/papermake-server`: Axum HTTP API and server-rendered web UI.
- `crates/papermake-worker`: background worker binary. Use
  `WORKER_ROLE=render` for batch rendering and `WORKER_ROLE=maintenance` for
  analytics aggregation and pruning.
- `docs`: Diataxis-style documentation.
- `deploy/k8s`: plain Kubernetes starter manifests.
- `docker-compose.yml`: local development stack with S3-compatible storage.

## Development Commands

Run focused checks while working, then the broader checks before handing back
substantial code changes:

```bash
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets
```

Useful local commands:

```bash
podman-compose up -d
curl http://localhost:3000/health
cargo run -r -p papermake-server
cargo run -r -p papermake-worker
kubectl kustomize deploy/k8s
```

The server and worker load `.env` when present. Local S3 defaults are documented
in `.env.example` and `docs/reference/configuration.md`.

## Architecture Reminders

- S3-compatible object storage is the durable source of truth. Papermake does
  not use a separate database.
- Templates are content-addressed. Tags are mutable pointers to immutable
  manifest hashes.
- Synchronous renders run in `papermake-server`; batch renders are enqueued by
  the server and processed by render workers.
- Run one maintenance worker in normal operation. It writes
  `analytics/agg/summary.json`, prunes expired render outputs, deletes old raw
  analytics, and removes stale job documents.
- Render analytics are eventually consistent. Direct PDF lookup by `render_id`
  is immediate after a successful render.
- Render outputs can be kept forever. A retention value of `0` means no expiry
  record is written.

## Deployment Notes

- Containerized servers must bind `HOST=0.0.0.0`.
- `REQUEST_BODY_LIMIT_BYTES` controls the HTTP body limit; the default is
  50 MiB.
- `S3_ENDPOINT_URL` is the complete endpoint URL. For AWS S3, use the regional
  endpoint URL; there is no separate `S3_REGION` configuration.
- Do not add CORS middleware for the web UI. The UI is same-origin SSR, and API
  clients do not need browser CORS.
- Render workers normally do not need `PAPERMAKE_WORKER_ID`; they choose a
  usable id from the hostname, then PID.
- Do not set one shared `PAPERMAKE_INSTANCE_ID` on a scaled server deployment.
  Leave it unset unless each server gets a unique stable id.
- Server and render worker pods should use the same `FONTS_DIR` contents so
  synchronous and batch renders match.

## Documentation Rules

Keep documentation crisp and purpose-specific:

- tutorials teach first success;
- how-to guides solve operational tasks;
- reference pages specify exact behavior and data shapes;
- explanation pages describe how and why the system works.

Do not duplicate large API, storage, or configuration references in README-like
documents. Link to the relevant docs instead. Keep the main README focused on
what Papermake does, a short quick start, and documentation links.

## Change Hygiene

- Prefer existing crate boundaries and local helper APIs over new abstractions.
- Keep changes scoped to the requested behavior.
- Do not overwrite unrelated manual changes in the worktree.
- When changing runtime behavior, update the relevant docs and deployment
  examples in the same chunk.
- When changing public API behavior, update `docs/reference/api.md` and the
  OpenAPI definitions/tests in `crates/papermake-server/src/openapi.rs`.
- Cover changes and new functionality with tests, especially public/exported
  functions where breakage affects downstream users.
- Prefer TDD style where practical: write the test first, then make the
  implementation turn it green, to avoid coupling tests to implementation
  details.
