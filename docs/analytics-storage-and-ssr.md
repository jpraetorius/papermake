# Drop ClickHouse & the SPA: buffered-S3 analytics + SSR UI

> **Status:** design / not yet implemented.
> **Base:** builds on the dependency-update branch (`chore/update-dependencies`) — assumes the upgraded APIs (S3 client builders, typst 0.15, clickhouse 0.15, thiserror 2.0, redis 1.3). Implement on top of that.

## Context

The render-analytics stack currently requires **two heavy moving parts** that are overkill for what is essentially a render log with a few `GROUP BY`s:

- **ClickHouse** — a full OLAP server the app hard-requires at boot (`ClickHouseStorage::from_env().unwrap()` + `init_schema()` in `main.rs`). The identical analytics already run in pure Rust in `MemoryRenderStorage` (`render_storage/mod.rs`), proving ClickHouse isn't needed for correctness — only operational weight.
- **A Next.js SPA** (`./webui`) that fetches JSON and renders client-side. Its charts are unimplemented placeholders; the only live metrics (24h count, p90 latency) are computed in JS from `/api/renders`.

**Goal.** For a use case that batches up to ~100k renders across **multiple render instances**, replace both with:
1. A **buffered-S3 render store**: records live in memory per instance, flush to S3 on an interval/size threshold; a single **papermake-worker** job aggregates the raw records in S3 into collated analytics. No always-on database; S3 is the shared collation point.
2. A **server-side-rendered UI** in papermake-server: plain HTML styled with **KelpUI** (vendored CSS), a small vendored **htmx** for the editor's no-reload test-render. No SPA, no build step.

## Confirmed decisions
- Aggregator runs as the **papermake-worker** binary (one aggregator regardless of render-instance count).
- **Outputs are keyed by `render_id`, not by content hash.** Store each render's artifacts under `renders/{render_id}/`: a small **`meta.json`** (`{success, error?, template_ref, timestamp, sizes}`) written on **both** success and failure, plus `pdf` (success only) and `data`. By-id lookups are **direct `blob.get`s** keyed by `render_id` — no aggregate consulted, no UUIDv7 date-decoding, no partition scan — so they work **immediately** after a render, before any flush. (Content-addressing is kept only for templates/assets, where dedup actually helps.)
- **`get_render_pdf(id)` distinguishes failed from missing** via `meta.json`: meta absent → **404** (unknown/pruned id); `success == false` → **4xx** (e.g. `410 Gone`, carrying the render error); meta present + success but pdf blob absent → **404**. `meta.json` also makes `RenderStorage::get_render(id)` a real direct read rather than a stub.
- **Per-template recent renders come from the S3 aggregate**, not a raw scan: `summary.json` carries a bounded `recent: [RenderRecord; N]` list **per template** (alongside per-template counts) plus a global `recent` list. `list_template_renders` slices the per-template list.
- **Vendored UI assets are pinned**: specific released versions of `kelp.css` and `htmx.min.js` are committed under `crates/papermake-server/assets/` (exact versions/hashes recorded at vendor time). No CDN at runtime.
- Analytics queries are **always answered from the S3 aggregation**, never from local memory → one consistent view for all instances, refreshed per flush.
- Editor test-render = SSR + **tiny htmx sprinkle** (no full reload; native `<iframe>` PDF, no PDF.js).
- **Delete `./webui`.**

## Working method (applies to every change below)
- **TDD.** For each unit of work write the unit test(s) **first**, watch them fail, then implement until green. Each of the components below (`S3BufferedRenderStorage`, `aggregator`, `retention::prune`, `address` key helpers, `render_and_store` keying, retention resolution, SSR handlers) lands with its tests in the same step — tests are not a trailing phase.
- **Small, green steps.** Prefer many small commits, each with the workspace building and `cargo test --workspace` passing.
- **Gates on every change (non-negotiable):** `cargo fmt --all` and `cargo clippy --workspace --all-targets` must be clean before a change is considered done — not just at the end. Keep the repo `fmt`/`clippy`-clean throughout (as the dependency-update branch already is).
- Offline-first tests use `MemoryStorage` as the `BlobStorage` backend so the whole suite runs without infra; the live S3-compatible backend integration check is the final confirmation.

---

## Part A — Render storage: buffered S3 + worker aggregation

**Two independent concerns, fully decoupled:**

1. **Artifacts (PDF + input data)** — written to S3 at render time, **keyed by `render_id`**: `renders/{render_id}/pdf`, `renders/{render_id}/data`. Retrieval is a direct `blob.get` by render_id — no record consulted, no hashing needed to locate them. (Templates/assets/manifests keep content-addressing via `ContentAddress` in `address.rs` — dedup helps there.)
2. **Analytics records** — `RenderRecord` (timing, success, sizes, refs). These are what move off ClickHouse: staged in memory, flushed to S3 as NDJSON, aggregated by the worker.

### A0. Registry changes for render_id-keyed outputs (`crates/papermake-registry/src/registry.rs`, `address.rs`)
- `render_and_store`: generate `render_id` up front; on **both** success and failure `put` a `renders/{render_id}/meta.json` (`RenderMeta { success, error, template_ref, timestamp, pdf_size_bytes, data_size_bytes }`); on success also `put` the PDF to `renders/{render_id}/pdf` and input data to `renders/{render_id}/data` (replaces the `ContentAddress::pdf_key`/`data_key` content-hash writes). These artifact writes do **not** depend on `RenderStorage` — the metadata *record* staging (A1) is separate.
- `get_render_pdf(id)`: read `renders/{id}/meta.json` first — S3 NotFound → `RenderNotFound` (404); parse and if `!success` → `RenderFailed` (a **4xx**, e.g. `410 Gone`, carrying `error`); else `blob.get("renders/{id}/pdf")`, NotFound → `RenderNotFound`. `get_render_data(id)`: **direct `blob.get("renders/{id}/data")`**, NotFound → `RenderNotFound`. Neither touches the analytics buffer/aggregate.
- `address.rs`: drop `pdf_key`/`data_key` (content-hash) helpers; add `render_meta_key(id)`/`render_pdf_key(id)`/`render_data_key(id)`. Keep `blob_key`/`manifest_key`/`ref_key`.
- `RenderRecord.pdf_hash`/`data_hash` become optional integrity metadata (keep computing them if cheap, or drop from the record); `manifest_hash` stays. `render_id` is the durable handle to the artifacts.
- Add a new `RenderStorageError::RenderFailed { error: String }` (→ server 4xx) distinct from `RenderNotFound` (→ 404); wire the mapping in `papermake-server/src/error.rs`.

**Read model — analytics queries answered from S3, never from memory.** The in-memory buffer is *write-only staging*; never a read source. `list_recent_renders` and the rollups read the S3 aggregation → one globally-consistent view for every instance, refreshed each flush+aggregate cycle (a record becomes queryable after the next flush → plain global eventual consistency, no per-instance divergence). Serving artifacts by id does **not** depend on this (it's a direct blob read), so a just-finished render's PDF is fetchable immediately even before its analytics record is flushed.

### A1. New `S3BufferedRenderStorage` (`crates/papermake-registry/src/render_storage/s3_buffered.rs`)
Implements the existing `RenderStorage` trait (`render_storage/mod.rs`) over the existing `BlobStorage` (`storage/blob_storage.rs`: `put`/`get`/`list_keys`/`exists`/`delete`) — reuse `S3Storage`, no new S3 code.
- Fields: `blob: Arc<dyn BlobStorage>`, in-memory `buffer: RwLock<Vec<RenderRecord>>` (staging only), `instance_id` (env `PAPERMAKE_INSTANCE_ID`, else a generated uuid), flush config.
- `store_render` → push to buffer (fast, no network). PDF/data artifacts are already persisted to S3 by `render_and_store` (A0) — this only stages the metadata record.
- `flush()` → drain buffer → NDJSON (records already `Serialize` with rfc3339 time, `types.rs`) → `put` to `analytics/raw/dt=YYYY-MM-DD/{instance_id}/{unix_millis}-{seq}.ndjson`. Called by a background task on **interval OR when buffer ≥ N** (bounds memory during 100k batches). **Also flush on graceful shutdown** (SIGTERM handler in the server) so at most zero records are lost, not up-to-one-window — the buffer is the only unpersisted state (artifacts are already on S3 from render time).
- `list_recent_renders`, `list_template_renders`, `render_volume_over_time`, `total_renders_per_template`, `average_duration_over_time` → read the aggregate `analytics/agg/summary.json` and slice it (`list_template_renders` slices the per-template `recent` list). **No buffer merge** — same answer everywhere.
- `get_render(id)` → **direct read of `renders/{id}/meta.json`** (the same blob `get_render_pdf` consults), mapped into a `RenderRecord`. Immediate and by-id — not a stub, not dependent on the aggregate.

### A2. Aggregator (`crates/papermake-registry/src/render_storage/aggregator.rs`)
Pure function over `BlobStorage` (testable offline with `MemoryStorage`):
- `list_keys("analytics/raw/")` → `get` each → parse NDJSON → `Vec<RenderRecord>`.
- Compute a `Summary { generated_at, volume_by_day, duration_by_day, templates: [{ name, total_renders, recent: [RenderRecord; N] }], recent (global top-N), totals { renders_24h, success_rate_24h, p90_latency_ms_24h } }`. The per-template `recent` list backs `list_template_renders` (and the template detail page) entirely from the aggregate — no raw scan. Reuse the aggregation logic already in `MemoryRenderStorage` (extract the HashMap-counting into shared helpers).
- Write `analytics/agg/summary.json`. Re-scan a rolling window (e.g. last 90 days) each run — idempotent; incremental watermarking is a later optimization (note in code).

### A3. `papermake-worker` becomes the aggregator + housekeeper (`crates/papermake-worker/`)
- `Cargo.toml`: **drop `redis`**, add `papermake-registry` (+ tokio, tracing, dotenv).
- `main.rs`: `S3Storage::from_env()` → loop { `aggregator::run(&blob)` ; `retention::prune(&blob)` (Part C) ; log ; `sleep(WORKER_INTERVAL_SECONDS)` }.

### A4. Remove ClickHouse
- Delete `render_storage/clickhouse.rs`; drop the `clickhouse` dep + `clickhouse` feature + `default = ["s3","clickhouse"]` in `crates/papermake-registry/Cargo.toml` (new default `["s3"]`).
- Remove `clickhouse` re-exports from `lib.rs`.
- `docker-compose.yml`: remove the `clickhouse` service + its env on the server; add a `papermake-worker` service (shares S3-compatible object storage). Remove `./clickhouse-init` usage.

---

## Part B — SSR UI in papermake-server (replaces `./webui`)

### B1. Templating + assets
- Add **`maud`** (with the `axum` feature → `Markup: IntoResponse`) to `crates/papermake-server/Cargo.toml`. Single dep, compile-time HTML, XSS-safe, no template files/build.rs. (Alternative: askama with `.html` files — not chosen, to avoid build wiring.)
- Vendor **`kelp.css`** and **`htmx.min.js`** (specific pinned released versions, committed; record the exact versions/hashes at vendor time) into `crates/papermake-server/assets/`; serve via `tower-http` `ServeDir` at `/assets` (the `fs` feature is already enabled). Keeps everything self-contained (no CDN).
- Shared `layout(title, body)` maud fn (Kelp classes, `<link>` to `/assets/kelp.css`, `<script>` htmx). Small inline-SVG helpers for bar/sparkline charts (Kelp has no charts).

### B2. Pages (`crates/papermake-server/src/routes/ui.rs`, mounted at `/` in `routes/mod.rs`)
- `GET /` — dashboard: totals (24h renders, success rate, p90) + volume/day sparkline + per-template bars from `summary.json`; recent-renders table (id, template_ref, status, duration, relative time, download link); template list. Reuses `registry.list_recent_renders`, `list_templates`, `get_render_analytics`.
- `GET /templates/{reference}` — detail: metadata/tags, recent renders for it, editor `<textarea>` prefilled with source, **htmx** "Test Render" button, publish `<form>`.
- `POST /ui/templates/{name}/render` (htmx) → `registry.render_and_store` → returns an HTML **fragment** with `<iframe src="/api/renders/{id}/pdf">`.
- `POST /ui/templates/{name}/publish` → `registry.publish` (reuse publish-simple path) → redirect back.

### B3. Small backend additions
- `GET /api/templates/{reference}/source` (text/plain): fetch the entrypoint `.typ` for the editor. The registry already resolves manifest→entrypoint blob (see `registry.rs` render/resolve path + `RegistryFileSystem`); add a `Registry::get_template_source(reference)` helper.
- Fill the empty `routes/analytics.rs`: `GET /api/analytics/{volume?days,templates,performance?days}` backed by `summary.json` (cheap now that the trait methods work; keeps JSON API parity).

### B4. Server wiring (`crates/papermake-server/src/main.rs`)
- `AppState.registry: Arc<Registry<S3Storage, S3BufferedRenderStorage>>`.
- Startup: build `S3BufferedRenderStorage`, **`tokio::spawn` the flush loop**; drop all ClickHouse init. On shutdown signal (SIGTERM/ctrl-c via axum `with_graceful_shutdown`), call `flush()` once more so no staged records are dropped.
- Keep existing `/api/*` JSON routes; add `/` UI routes + `/assets`.

### B5. Delete `./webui` and update `CLAUDE.md`
Remove ClickHouse/SPA sections; document the buffered-S3 + worker + SSR design and new env vars: `PAPERMAKE_INSTANCE_ID`, `FLUSH_INTERVAL_SECONDS`, `FLUSH_MAX_RECORDS`, `WORKER_INTERVAL_SECONDS`, `RENDER_RETENTION_DAYS`, `ANALYTICS_RETENTION_DAYS`.

---

## Part C — Retention & housekeeping (worker prunes expired outputs)

**Core idea — partition by expiry date, don't scan.** Each output's expiry lives in the *key space*, so pruning lists only the day-partitions that are due and deletes them. Cost is O(items expiring today), not O(all outputs) — no per-record scan.

### C1. Effective retention (resolved at render time, in `render_and_store`)
Precedence: **per-render override → per-template default → global default.**
- Global default: env `RENDER_RETENTION_DAYS` (e.g. 30).
- Per-template: add `retain_days: Option<u32>` to `TemplateMetadata` (`bundle.rs`/manifest), set at publish.
- Per-render: add `retain_days: Option<u32>` to the render request (`routes/render.rs` `RenderRequest`).
- `retain_days == 0` (or an explicit "never" sentinel) → **keep forever**: no expiry-index entry written.
- Compute `expiry_date = render_date + effective_retain_days`; also record it on `RenderRecord` (new `expiry_date` field) for audit/visibility.

### C2. Expiry index (the "clever" structure)
The flush task already buckets buffered records; have it **also** write an expiry index grouped by expiry date, mirroring the analytics NDJSON flush:
- `expiry/dt=YYYY-MM-DD/{instance_id}/{ulid}.ndjson`, each line = the artifact keys (or just `render_id`) expiring that day.
- Grouping many render_ids per file (vs one marker object each) keeps object count sane at 100k/day and is concurrency-safe across instances (instance-scoped filenames).
- This is a *second* partitioning of the same records (analytics raw is partitioned by **render** date for aggregation; the expiry index by **expiry** date for pruning) — both are cheap append-only NDJSON produced in the same flush.

### C3. `retention::prune(&blob)` (`crates/papermake-registry/src/render_storage/retention.rs`, run by the worker)
- `list_keys("expiry/")` → select partitions where `dt <= today`.
- Read those NDJSON files → the set of due `render_id`s → delete `renders/{id}/pdf` + `renders/{id}/data`, then delete the consumed `expiry/dt=<due>/...` files.
- Prefer a batched delete: add `BlobStorage::delete_many(keys)` (S3-compatible `delete_objects`, up to 1000/call) for throughput; fall back to `delete` in a loop otherwise.
- **Also prune analytics raw**: delete `analytics/raw/dt=<old>/` older than `ANALYTICS_RETENTION_DAYS` (independent, usually short — the persisted `summary.json` keeps the rollups, so history survives even after raw is dropped).

### C4. Semantics / notes
- Expiry is **fixed at render time**; later changing a template's default does not retroactively re-date existing outputs (predictable, no re-scan). Retroactive re-evaluation is a non-goal.
- After an artifact is pruned, its analytics record may still exist → a by-id PDF fetch simply 404s; analytics/rollups are unaffected. (Whether to also drop the record is an analytics-retention choice, handled by the analytics-raw prune above.)

## Verification
- **Unit (offline, `MemoryStorage` as blob backend):** `S3BufferedRenderStorage` store→flush→ raw-blob roundtrip; aggregator over synthetic raw NDJSON → assert `summary.json` totals/rollups (mirror existing `render_storage` tests); artifact keying (`render_and_store` writes `renders/{id}/meta.json|pdf|data`; `get_render_pdf/data` round-trip by id). **Failed-vs-missing:** a failed render writes `meta.json` (no pdf) → `get_render_pdf` yields `RenderFailed` (4xx); an unknown id → `RenderNotFound` (404); per-template `recent` list in `summary.json` backs `list_template_renders`. **Retention:** effective-retain precedence (render > template > global; `0` = keep forever → no index entry); expiry-index written under the right `expiry/dt=<expiry>/…`; `retention::prune` deletes only due-partition artifacts + index files and leaves not-yet-due ones intact.
- **SSR handler tests:** pages return 200 and contain expected text (template names, metric labels).
- **Integration (live S3-compatible backend via `podman-compose up -d object-store`; there is no `docker` on this machine — see `docker-compose.yml`):** run server + worker; POST a render; confirm the PDF lands at `renders/{id}/pdf` and is fetchable by id immediately; confirm raw NDJSON under `analytics/raw/...` + an `expiry/dt=.../` entry; worker writes `analytics/agg/summary.json` and `GET /` shows the render. Retention: render with a past/short `retain_days`, run the worker prune, confirm `renders/{id}/*` is deleted while a longer-retained render survives.
- **Full gates:** `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets` (clean), `cargo fmt --all --check`.
- Manual: open `/` and `/templates/{ref}` in a browser; edit + Test Render (htmx swaps in the PDF iframe, no reload); publish.

## Out of scope / notes
- **Templates/assets/manifests keep content-addressing** (dedup helps). Only **rendered outputs** switch to `render_id` keys, and analytics **records** move off ClickHouse. Rendering itself (Typst engine, `render_and_store` flow) is otherwise unchanged.
- **Migration:** existing `pdfs/sha256/*` / `data/sha256/*` blobs from the old scheme won't be reachable by the new `renders/{id}/*` paths. Fine for a fresh deploy; note it if there's production data to migrate.
- Analytics are **globally eventually consistent**: every instance reads the same S3 aggregate; a render shows up after the next flush+aggregate. Reads never touch local memory, so there is no per-instance skew. **Artifact retrieval is immediate** (direct blob read), independent of the analytics flush.
- Incremental aggregation (watermark) deferred; rolling-window re-scan is fine at this volume.
- Keeping the JSON `/api` surface means external clients/tests keep working.
