# Operations

This guide covers routine tasks and incident checks for a running Papermake
deployment.

Examples assume:

```bash
BASE=http://localhost:3000
S3_ENDPOINT_URL=http://localhost:9000
S3_BUCKET=papermake-templates
```

Kubernetes examples assume the manifests from `deploy/k8s` and the `papermake`
namespace.

For AWS S3, set `S3_ENDPOINT_URL` to the regional endpoint URL. The S3 commands
below use the AWS CLI syntax; any S3-compatible client is fine.

## Scale render workers

Render workers process batch shards. They do not serve HTTP traffic and do not
run maintenance work.

With Docker Compose:

```bash
docker compose up -d --scale papermake-worker=4
```

Check that workers are running:

```bash
docker compose ps papermake-worker
docker compose logs -f papermake-worker
```

With Kubernetes:

```bash
kubectl -n papermake scale deploy/papermake-render-worker --replicas=4
kubectl -n papermake rollout status deploy/papermake-render-worker
kubectl -n papermake logs -f deploy/papermake-render-worker
```

Each worker needs a unique id, but you normally do not need to set
`PAPERMAKE_WORKER_ID` explicitly. Workers choose a usable id from the container
hostname, falling back to the process id.

Use `SHARD_SIZE` to tune how work is split. Smaller shards spread large jobs
across workers more evenly and are faster to reclaim after a worker dies. Larger
shards create fewer S3 documents and less scheduling overhead.

## Run one maintenance worker

The maintenance worker aggregates analytics and prunes data. In normal
operation, run exactly one process with `WORKER_ROLE=maintenance`.

It does four things each cycle:

| Task | Writes or deletes |
|---|---|
| Aggregate analytics | Writes `analytics/agg/summary.json` from `analytics/raw/...` |
| Prune expired outputs | Deletes due `renders/{id}/meta.json`, `pdf`, and `data` |
| Prune raw analytics | Deletes old `analytics/raw/...` files |
| Prune stale jobs | Deletes old `jobs/{job_id}/...` documents |

Multiple maintenance workers are designed to be repeatable, but they add
redundant S3 load and make incidents harder to reason about.

Check the maintenance worker:

```bash
docker compose ps papermake-maintenance
docker compose logs -f papermake-maintenance
```

With Kubernetes, keep `papermake-maintenance` at one replica:

```bash
kubectl -n papermake get deploy papermake-maintenance
kubectl -n papermake logs -f deploy/papermake-maintenance
```

The sample Kubernetes deployment uses `Recreate` strategy so an update does not
briefly run two maintenance pods.

Avoid `WORKER_ROLE=all` in scaled production deployments. Scaling an `all`
worker also scales maintenance.

## Rotate S3 credentials

Papermake reads S3 credentials at process startup. Rotating credentials requires
restarting the server, render workers, and maintenance worker.

1. Create the new S3 credentials with the same bucket permissions.
2. Update the deployment secrets or environment variables:
   `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY`.
3. Restart Papermake processes while both old and new credentials are accepted.
4. Verify health, worker logs, and S3 access.
5. Revoke the old credentials.

With Docker Compose after editing the environment:

```bash
docker compose up -d --force-recreate papermake-server papermake-worker papermake-maintenance
curl -fsS "$BASE/health"
```

If render workers are scaled, include the current scale in the recreate command,
for example `--scale papermake-worker=4`.

With Kubernetes, update the Secret through your normal secret flow, then restart
the deployments:

```bash
kubectl -n papermake rollout restart \
  deploy/papermake-render-worker \
  deploy/papermake-maintenance \
  deploy/papermake-server

kubectl -n papermake rollout status deploy/papermake-render-worker
kubectl -n papermake rollout status deploy/papermake-maintenance
kubectl -n papermake rollout status deploy/papermake-server
```

For a rolling deployment, restart render workers first, then maintenance, then
servers. A render worker interrupted mid-shard is safe: another worker can
reclaim the shard after its lease expires.

## Inspect failed renders

Recent render history comes from the analytics summary and can lag behind a
new render by one server flush and one maintenance cycle.

List recent failures:

```bash
curl -fsS "$BASE/api/renders?limit=50" \
  | jq '.data[] | select(.success == false) | {render_id, timestamp, template_ref, error}'
```

If you know the `render_id`, inspect the durable files directly:

```bash
RENDER_ID=...

aws s3 cp "s3://$S3_BUCKET/renders/$RENDER_ID/meta.json" - \
  --endpoint-url "$S3_ENDPOINT_URL" | jq .

aws s3 cp "s3://$S3_BUCKET/renders/$RENDER_ID/data" - \
  --endpoint-url "$S3_ENDPOINT_URL" | jq .
```

Failed renders have `meta.json` and `data`, but no `pdf`. A PDF request for a
failed render returns an error; a PDF request for a pruned or unknown render
returns `404`.

With the data file and the other information from `meta.json` you can retry a render from the web UI to see into details more.

## Inspect stuck batch jobs

Start with the API view:

```bash
JOB_ID=...
curl -fsS "$BASE/api/jobs/$JOB_ID" | jq '.data'
```

Use the state to choose the next check:

| Status | Check |
|---|---|
| `queued` | At least one render worker is running and can reach the S3 bucket |
| `running` | Shard leases, worker logs, and S3 latency |
| `completed` with failures | Worker logs and failed item inputs |
| `failed` | Repeated shard failures or a template that workers cannot load |

Inspect shard descriptors in S3:

```bash
aws s3 ls "s3://$S3_BUCKET/jobs/$JOB_ID/shards/" \
  --recursive \
  --endpoint-url "$S3_ENDPOINT_URL"

aws s3 cp "s3://$S3_BUCKET/jobs/$JOB_ID/shards/0/shard.json" - \
  --endpoint-url "$S3_ENDPOINT_URL" | jq .
```

Important shard fields:

| Field | Meaning |
|---|---|
| `status` | `pending`, `running`, `done`, or `failed` |
| `owner` | Worker id that claimed the shard |
| `lease_expires_at` | Time after which another worker can reclaim it |
| `attempts` | Number of claims; capped by `WORKER_MAX_ATTEMPTS` |
| `done`, `failed` | Item counts inside the shard |

Do not edit shard files by hand during normal recovery. Fix the worker, S3, or
template issue and let leases expire. Content-addressed render ids make retrying
safe.

## Verify analytics freshness

Analytics endpoints and the dashboard read `analytics/agg/summary.json`.
Freshness depends on:

| Setting | Role |
|---|---|
| `FLUSH_INTERVAL_SECONDS` | How often servers flush raw render records |
| `FLUSH_MAX_RECORDS` | Buffer size that triggers an eager raw flush |
| `WORKER_INTERVAL_SECONDS` | How often the maintenance worker aggregates |

Inspect the summary timestamp:

```bash
aws s3 cp "s3://$S3_BUCKET/analytics/agg/summary.json" - \
  --endpoint-url "$S3_ENDPOINT_URL" \
  | jq '{generated_at, totals}'
```

If `generated_at` is stale, check maintenance logs and raw records:

```bash
docker compose logs --tail=100 papermake-maintenance

aws s3 ls "s3://$S3_BUCKET/analytics/raw/" \
  --recursive \
  --endpoint-url "$S3_ENDPOINT_URL" \
  | tail
```

With Kubernetes, check maintenance logs with:

```bash
kubectl -n papermake logs --tail=100 deploy/papermake-maintenance
```

Direct PDF downloads by `render_id` do not depend on `summary.json`.

## Recover from stale `summary.json`

The maintenance worker rebuilds `summary.json` by scanning
`analytics/raw/...`. If raw files still exist, recovery is usually just a
maintenance restart:

```bash
docker compose restart papermake-maintenance
docker compose logs -f papermake-maintenance
```

With Kubernetes:

```bash
kubectl -n papermake rollout restart deploy/papermake-maintenance
kubectl -n papermake logs -f deploy/papermake-maintenance
```

If `summary.json` is malformed or you want to force a clean rewrite, move it
aside and let maintenance recreate it:

```bash
STAMP=$(date +%Y%m%d%H%M%S)

aws s3 mv \
  "s3://$S3_BUCKET/analytics/agg/summary.json" \
  "s3://$S3_BUCKET/analytics/agg/summary.json.bak-$STAMP" \
  --endpoint-url "$S3_ENDPOINT_URL"

docker compose restart papermake-maintenance
```

Or with Kubernetes:

```bash
kubectl -n papermake rollout restart deploy/papermake-maintenance
```

Until the next aggregation cycle, analytics APIs return an empty summary. If
the raw files were already pruned, old detail cannot be reconstructed from
Papermake data; keep the moved summary backup for inspection.

## Stop pruning during an incident

There is no separate pruning switch. The maintenance worker both aggregates
analytics and prunes data.

To pause pruning, stop the maintenance worker:

```bash
docker compose stop papermake-maintenance
```

With Kubernetes:

```bash
kubectl -n papermake scale deploy/papermake-maintenance --replicas=0
```

This also pauses analytics aggregation, raw analytics pruning, and stale job
pruning. Synchronous renders, PDF downloads, and render workers can continue.

When the incident is over:

```bash
docker compose start papermake-maintenance
```

With Kubernetes:

```bash
kubectl -n papermake scale deploy/papermake-maintenance --replicas=1
```

Changing `RENDER_RETENTION_DAYS` affects future render expiry decisions only. It
does not stop pruning for expiry records that were already written.

## Back up or migrate S3 data

Papermake's durable state is the S3 bucket. Back up the whole bucket, not only
rendered PDFs.

Important prefixes:

```text
blobs/
manifests/
refs/
renders/
jobs/
analytics/
expiry/
```

For a consistent backup, pause writers first:

```bash
docker compose stop papermake-server papermake-worker papermake-maintenance

aws s3 sync "s3://$S3_BUCKET" ./papermake-backup \
  --endpoint-url "$S3_ENDPOINT_URL"

docker compose start papermake-server papermake-worker papermake-maintenance
```

With Kubernetes, record the intended replica counts, scale writers down, run the
same S3 sync, then restore the replicas:

```bash
kubectl -n papermake scale \
  deploy/papermake-server \
  deploy/papermake-render-worker \
  deploy/papermake-maintenance \
  --replicas=0

aws s3 sync "s3://$S3_BUCKET" ./papermake-backup \
  --endpoint-url "$S3_ENDPOINT_URL"

kubectl -n papermake scale deploy/papermake-server --replicas=<servers>
kubectl -n papermake scale deploy/papermake-render-worker --replicas=<workers>
kubectl -n papermake scale deploy/papermake-maintenance --replicas=1
```

To migrate to another bucket or object store:

```bash
aws s3 sync ./papermake-backup "s3://$NEW_S3_BUCKET" \
  --endpoint-url "$NEW_S3_ENDPOINT_URL"
```

Then update `S3_ENDPOINT_URL`, `S3_BUCKET`, `S3_ACCESS_KEY_ID`, and
`S3_SECRET_ACCESS_KEY` for every Papermake process and restart them.

For a shorter downtime window, run one live sync first, stop writers, run a
second sync, then restart against the new bucket.
