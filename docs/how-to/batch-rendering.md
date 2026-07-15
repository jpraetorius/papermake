# Batch rendering

Use batch rendering when one template should produce many PDFs. The server
accepts the batch and writes a durable job to S3; render workers process the
items in shards.

Before submitting a batch, make sure at least one render worker is running. If
no render worker is available, the job stays queued.

## Submit a batch

Send one `inputs` entry per PDF. Use `key` to keep your own identifier attached
to each result.

```json
{
  "inputs": [
    { "key": "cust-a", "data": { "number": "INV-001", "customer": "Acme" } },
    { "key": "cust-b", "data": { "number": "INV-002", "customer": "Globex" } }
  ],
  "retain_days": 30,
  "pdf_standard": "a-3b"
}
```

Submit it:

```bash
BASE=http://localhost:3000
REF=invoice:latest

JOB_ID=$(
  curl -fsS -X POST "$BASE/api/render/$REF/batch" \
    -H 'Content-Type: application/json' \
    --data @batch.json \
    | jq -r '.data.job_id'
)

echo "$JOB_ID"
```

`retain_days` and `pdf_standard` apply to every item in the batch. Omit
`retain_days` to use the template or global retention default. Omit
`pdf_standard` for PDF 1.7.

## Poll the job

```bash
curl -fsS "$BASE/api/jobs/$JOB_ID" | jq '.data'
```

The job view contains:

| Field | Meaning |
|---|---|
| `status` | `queued`, `running`, `completed`, or `failed` |
| `total` | Number of submitted inputs |
| `done` | Items rendered successfully |
| `failed` | Items that failed to render |
| `num_shards` | Number of shards created for the job |
| `shards_terminal` | Shards that are done or failed |

`completed` means all shards are terminal. It can still include failed items, so
check both `done` and `failed`.

## Read item results

Item results are ordered by input index and paginated:

```bash
curl -fsS "$BASE/api/jobs/$JOB_ID/items?offset=0&limit=1000" | jq '.data'
```

Only completed shards have item results. While a job is still running, a page
can be empty or incomplete even though later polling returns results for the
same range.

Each item includes:

| Field | Meaning |
|---|---|
| `index` | Zero-based input index |
| `key` | Your optional caller-supplied key |
| `status` | `success` or `failed` once the shard has completed |
| `render_id` | Present for successful items |

## Download PDFs

Fetch each successful PDF by `render_id`:

```bash
mkdir -p out

curl -fsS "$BASE/api/jobs/$JOB_ID/items?offset=0&limit=1000" \
  | jq -r '.data[] | select(.status == "success" and .render_id) | [(.key // (.index|tostring)), .render_id] | @tsv' \
  | while IFS=$'\t' read -r key render_id; do
      curl -fsS "$BASE/api/renders/$render_id/pdf" --output "out/$key.pdf"
    done
```

For large jobs, page through `/items` until you have covered `total`.

## Scale throughput

Batch throughput comes from render workers, not from the server that accepted
the job.

With Docker Compose:

```bash
docker compose up -d --scale papermake-worker=4
```

`SHARD_SIZE` controls how many inputs are placed in one shard when the server
creates the job. Smaller shards spread work across workers more finely and are
easier to reclaim after a worker dies. Larger shards create fewer S3 documents
and less scheduling overhead.

Use the same `FONTS_DIR` and bundled fonts for the server and render workers so
synchronous and batch renders produce the same PDFs.

## Troubleshooting

| Symptom | Check |
|---|---|
| Job stays `queued` | A render worker is running and can reach the same S3 bucket |
| Job is `running` but slow | Worker logs, worker count, `SHARD_SIZE`, S3 latency, and template render time |
| Job is `completed` with failures | Failed item count and worker logs for render errors |
| PDF returns `404` | The item succeeded, the `render_id` is correct, and output retention has not pruned it |

See the [API reference](../reference/api.md#jobs) for exact response shapes and
the [architecture explanation](../explanation/architecture.md#batch-model) for
the shard model.
