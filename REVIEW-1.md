# Papermake — Whole-Repo Review (REVIEW-1)

- **Date:** 2026-07-15
- **HEAD:** `1f57a88` — "Document PDF standards and the PDF/UA-1 title requirement"
- **Scope:** entire workspace (`papermake`, `papermake-registry`, `papermake-server`, `papermake-worker`), five axes: correctness, readability, architecture, security, performance.

**Verdict: Request changes.** The foundations are genuinely good — the content-addressing model is coherent, the batch shard/lease design is honest about its consistency trade-offs, the Typst virtual-filesystem sandbox is sound, and the registry crate's behavioral test suite is strong. But there are five findings to block on: two data-loss paths in the retention pipeline, an un-killable render path that lets anyone take the service down, a broken pagination endpoint, and a cluster of error-mapping bugs that violate the documented API contract.

**Verification:** `cargo clippy --workspace --all-targets` clean; all workspace tests pass (registry ~60 behavioral tests, 20 server tests, doctests).

**Confidence note:** two findings were discovered independently by multiple reviewers — the un-killable render and the unvalidated publish path. Treat those as high-confidence.

---

## Critical

### C1. Renders can't be cancelled, and the semaphore permit lives inside the un-killable task — permanent slot exhaustion

`crates/papermake-registry/src/registry.rs:805-864` (and `:868-975`). The timeout at `registry.rs:844` only drops the `JoinHandle`; a `spawn_blocking` task can't be cancelled, and the permit is moved into the closure (`let _permit = permit;`), so a non-terminating template holds a render slot forever. Typst is a full language, publishing is unauthenticated, so ten requests with `#let f(n) = if n>0 { f(n) }` permanently consume all `MAX_CONCURRENT_RENDERS=10` slots; every later render 408s forever. There is also no memory ceiling, so a memory-bomb template OOMs the whole process.

**Fix:** run compilation in a disposable child process you can hard-kill on timeout, under an OS memory limit. Interim mitigations: much shorter default timeout, a cap on total detached in-flight renders, container memory limits.

### C2. Prune deletes artifacts whose retention was later extended

`crates/papermake-registry/src/render_storage/retention.rs:99-110` interacting with `registry.rs:1904-1913`. Render ids are content-addressed, so re-rendering the same request overwrites the same keys — but the expiry index written at the *first* render is never revisited. Render with `retain_days=1`, re-render identical data with `retain_days=0` ("keep forever"): on day D+1, prune reads the stale index and deletes the pinned PDF, data, and meta.

**Fix:** before deleting, read the render's current `meta.json` and skip ids whose `expiry_date` is `None` or in the future — promote `RenderRecord.expiry_date` from "audit/visibility" to the source of truth at prune time.

### C3. A failed analytics flush silently converts a batch of renders to "keep forever"

`crates/papermake-registry/src/render_storage/s3_buffered.rs:73-119`. `flush()` drains the buffer with `mem::take` *before* any S3 put. If a put fails, the drained records are gone — and the expiry-index entries in them are the *only* mechanism that ever deletes those PDFs and input data. For a system storing rendered invoices with customer data, that's a retention/compliance failure, not just an analytics undercount.

**Fix:** on error, re-stage the drained records into the buffer and return the error; the keys embed `instance_id/millis/seq` and the aggregator dedupes by `render_id`, so retrying is safe.

### C4. `GET /api/renders` pagination is arithmetically wrong for any `offset > 0`

`crates/papermake-server/src/routes/renders.rs:42-59`. The handler fetches `limit + 1` records and then skips `offset` from that same slice — `?limit=50&offset=50` returns at most 1 record and `has_more` is computed on the mangled remainder. Also, `pagination.limit + 1` on an unclamped user `u32` overflows in debug builds at `limit=u32::MAX`.

**Fix:** fetch `offset + limit + 1` (or push offset into `list_recent_renders`), then skip/truncate; clamp `limit` to a sane max.

### C5. One malformed NDJSON line permanently kills all analytics

`crates/papermake-registry/src/render_storage/aggregator.rs:41`. `serde_json::from_str(line)?` inside `load_raw_records` means a single corrupt raw file makes every future aggregation cycle fail — `summary.json` never refreshes again and the worker logs errors forever.

**Fix:** skip-and-warn on malformed lines. Add the test first; it fails today.

---

## Important

### Correctness — batch pipeline

- **Resume treats persisted transient failures as terminal** (`registry.rs:1262-1277`): resume-skip fires on any existing `meta.json`, including `success=false` metas written for timeouts and S3 hiccups. A shard reclaimed after a worker died under load permanently counts those items as failed. Fix: only skip when `success=true`; the content-addressed id makes re-rendering harmless.
- **A transient `prepare_batch` error permanently fails an entire shard** (`registry.rs:1229-1245`), bypassing the `max_attempts` poison guard. Fix: release the shard back to claimable and let the existing guard handle repeated failures.
- **After one item errors, remaining items render with a world missing the template's bundled fonts** (`registry.rs:1524-1547`): the placeholder world swapped in on error is built without `fonts`, so subsequent items silently render with fallback fonts while reporting `success=true`. Fix: hold the world in an `Option`/`take()` (no placeholder at all — this also removes a per-item `CACHED_FONTS` clone), and rebuild via the fonts-aware constructor on error.

### Correctness — HTTP error contract

Three handlers bypass the good centralized mapping in `error.rs` and break the documented 404/408/422 contract:

- `routes/render.rs:111-114` — every non-timeout error becomes 422, including unknown template (documented 404) and storage outages (documented 500). It also stringifies raw `RegistryError` into the response, leaking S3/internal key details to anonymous clients.
- `routes/renders.rs:95-100` — the documented 422 for failed renders is unreachable; everything flattens to 404, and an S3 outage masquerades as "render not found".
- `routes/jobs.rs:24-29` — same blanket-404 for jobs.

**Fix for all three:** return `ApiError::Registry(e)` and let `error.rs` do its job; return compile diagnostics (the product) but generic messages for storage/internal errors.

### Security / input validation

- **Publish/delete paths skip the validation the read path enforces** (`routes/templates.rs:271,336`, `routes/ui.rs:1138-1147`, `registry.rs:223-286`). Axum decodes `%2F`, so raw `name`/`tag` with `/`, `..`, uppercase, or unbounded length go straight into S3 key construction — creating refs that `resolve()` can never find, phantom namespaces in listings, and genuine path traversal the day a filesystem backend lands. Three reviewers found this independently. Fix: apply `Reference::validate_name`/`validate_tag` inside `Registry::publish`/`delete_version` so the library boundary is safe regardless of caller.
- **Batch input count is unbounded** (`routes/render.rs:178-224`): a 50 MB body packs millions of `{"data":{}}` items → tens of thousands of synchronous S3 puts in the request handler plus unbounded worker/storage amplification. Fix: cap `inputs.len()` (400 above it), reject empty batches, bound per-item data size.

### Performance (Critical at production scale)

- **No manifest/blob/ref caching — every render pays F+8 sequential S3 round trips for immutable content** (`registry.rs:1883-2107`, `:511-654`, `:661-756`): the ref is resolved twice, the manifest fetched twice, the entrypoint three times, and every asset (fonts, images) re-downloaded per render. CLAUDE.md's Phase 4 cache was never built. Fix: LRU keyed by hash for blobs/manifests (immutable → cache forever), short-TTL ref cache, and thread the loaded manifest through the call chain.
- **`claim_next_shard` lists the entire `jobs/` subtree and sequentially GETs every shard descriptor on every 5s poll, per worker** (`registry.rs:1097-1164`) — cost grows with 7 days of job history, not with pending work. Fix: a `jobs/pending/` marker keyspace listed by prefix.
- **The aggregator re-downloads the entire raw analytics set into memory every 10–30s cycle** (`aggregator.rs:22-61`) — ~GBs of heap and thousands of sequential GETs at moderate volume. Fix: stream/fold per file, fetch concurrently, keep daily rollup objects so only today's partitions are re-read.
- Secondary: `list_templates` is a sequential 2N+1 scan and single-template endpoints (`get_template_metadata`, `list_template_tags`) call it anyway (`templates.rs:364-433`); `S3Storage` never overrides `delete_many` so pruning 10k renders issues 30k+ sequential DELETEs (`s3_storage.rs`, trait default at `blob_storage.rs:48-56`); `run_claimed_shard` HEADs every item even on first attempt (`registry.rs:1262-1267`); the PDF is cloned per put and re-cloned per retry (`registry.rs:1973-1976`, `s3_storage.rs:329`); `hydrate_file_system` fetches files sequentially and stores every asset's bytes twice (`registry.rs:731-736`).

### Architecture & dead code

- **`registry.rs` (3,844 lines) needs decomposition**: the template-load sequence is duplicated three times and `RenderRecord` construction twice. Named restructuring: split into a `registry/` module dir (`publish.rs`, `templates.rs`, `render.rs`, `batch.rs`, `history.rs`), extract `load_template()` and `build_render_record()` helpers. Similarly `routes/ui.rs` (1,594 lines) splits cleanly along its existing pure-function seams (`pages`, `charts`, `infer`).
- **Dead scaffolding to delete** (confirm before removal): `papermake-server/src/worker.rs` (not even declared as a module; wouldn't compile), `AppState.job_sender` whose receiver is dropped on the next line (`main.rs:126`), empty `models/template.rs` + `models/analytics.rs`, `papermake-registry/src/storage/filesystem.rs` (`RegistryFileSystem` — unused and embodies the block-on anti-pattern the codebase engineered around), `Reference::has_hash_verification`.
- **OpenAPI spec drift**: `search`/`sort_by`/`sort_order` are documented but ignored (`templates.rs:127,140-150`) — the filter is written and commented out. Reinstate or remove from the spec; the "stays in sync" claim currently doesn't hold. CLAUDE.md also documents `GET /renders/{id}/data`, which has no route.
- **`FLUSH_INTERVAL_SECONDS=0` / `WORKER_INTERVAL_SECONDS=0` produce busy-loops** hammering S3 (`config.rs:73-78`, worker `main.rs:118,248`). Clamp to ≥1.

### Test gaps (highest-leverage)

- **Zero request-level tests in `papermake-server`** — no handler, router, or status-code test exists, which is exactly where C4 and the error-mapping bugs live. A single `oneshot` test of `POST /api/render/unknown:latest` fails today. Blocker: `AppState` is hardwired to concrete S3 types and the crate is binary-only; extract a lib target with a generic `AppState` first.
- **`papermake-registry/tests/integration_tests.rs` is an empty file** — no live-S3 coverage despite CLAUDE.md claiming it. Recommend an env-gated BlobStorage contract suite (put/get/list with >1000 keys, `delete_many`) run against both `MemoryStorage` and MinIO.
- Untested: render timeout/semaphore paths, the C1/C2/C5 scenarios above, failed-render 404-vs-422 variant pinning, multipart publish handler, worker `Role` parsing (a typo'd role string silently becomes `All`). Also `crates/papermake/tests/render_tests.rs:16-17` requires system Arial — will fail on Linux CI.

---

## Suggestions

- Feature-gate `utoipa` in papermake-registry (`optional = true` + `cfg_attr`) — the dependency direction is fine, but it's an unconditional liability for non-server consumers.
- Replace the `Option<Arc<MemoryRenderStorage>>` "no render storage" modeling with a `NullRenderStorage` and collapse the four near-identical constructors (`registry.rs:119-209`).
- Security headers on the SSR UI (CSP, `nosniff`, `frame-ancestors`); validate `render_id` before building `Content-Disposition` (`renders.rs:102-114` — a `%0A` in the path panics the handler via `.unwrap()`).
- UI: don't render raw registry error strings into pages (`ui.rs:1084`); don't swallow S3 outages into an all-zeros dashboard (`ui.rs:995-1004`).
- i18n `negotiate` ignores q-values (`i18n.rs:66-82`) — fine for two languages, comment it or sort by q before a third locale.
- Worker: check shutdown inside `run_claimed_shard` (a 500-item shard blows past Docker's 10s grace and the final flush dies with SIGKILL); warn on malformed env values instead of silently defaulting.
- Batch: write `job.json` before shard descriptors (a fast worker can claim a shard whose job meta doesn't exist yet); time-based lease heartbeat instead of every-20-items; surface `completed_with_failures` — `JobStatus::Completed` currently can mean "every item failed".
- The injected `#let data = …` prelude (`typst.rs:138-141`) shifts `RenderError` byte offsets relative to the user's source and silently double-binds templates that define `data` themselves.
- Stale docs: `papermake-registry/src/lib.rs:1-16` advertises scopes/forks/marketplace that don't exist; `retention.rs:59` says jobs prune by `updated_at`, code uses `created_at` (and would delete a still-running week-old job).
- docker-compose ships guessable credentials over plaintext HTTP — fine for dev, but document that production must inject secrets and use TLS.
