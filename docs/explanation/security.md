# Security model

Papermake renders Typst templates and JSON data supplied through its HTTP API.
Treat it as infrastructure for trusted applications, not as a public document
rendering sandbox.

## Trust boundary

The trusted boundary is the Papermake deployment and the applications allowed
to call it.

Callers that can reach the API can:

| Capability | Impact |
|---|---|
| Publish templates | Store Typst source, assets, schemas, and bundled fonts |
| Render templates | Spend CPU and memory, write PDFs, input data, metadata, analytics, and expiry records |
| Start batch jobs | Create durable job state and queued work for render workers |
| Read APIs and UI pages | Inspect templates, render history, analytics, jobs, and PDFs by id |

For a private internal deployment, this usually means Papermake sits behind a
trusted service, VPN, private network, or platform ingress. If untrusted users
need access, put an application-specific authorization layer in front of it.

## Public exposure

**Papermake is not intended to be exposed directly to the public internet.**

The server currently has no built-in user authentication, authorization,
tenant isolation, or API key checks. Public deployments should add those controls
at a reverse proxy, API gateway, service mesh, or calling application.

At the edge, enforce:

| Control | Purpose |
|---|---|
| Authentication and authorization | Decide who can publish, render, view history, and fetch PDFs |
| TLS | Protect templates, render input data, PDFs, and credentials in transit |
| Request size limits | Keep uploads and render requests within expected bounds |
| Rate limits | Protect render capacity, S3, and worker queues |
| Access logs | Attribute expensive renders, failed requests, and publish activity |

## Typst rendering

Papermake renders with Typst through an in-process `World`.

Template files are loaded from the versioned template bundle into an in-memory
file system before Typst runs. During rendering, Typst can read files from that
bundle path space; it is not given a direct filesystem backend for arbitrary host
paths.

This is an important isolation layer, but it is not a full hostile-code sandbox:

| Assumption | Meaning |
|---|---|
| No arbitrary host file access | Template imports and assets resolve through Papermake's bundle file system |
| In-process rendering | A render runs inside the server or worker process, not inside a separate OS sandbox |
| CPU and memory still matter | Complex templates, large images, large fonts, or huge input data can exhaust resources |
| Typst safety matters | Keep Typst and Papermake dependencies updated as part of normal operations |

For untrusted template authors, run Papermake in containers or another workload
isolation boundary and apply strict CPU, memory, upload, and rate limits outside
the process.

## S3 credentials and data

Papermake stores durable state in one S3-compatible bucket: templates, manifests,
rendered PDFs, render input data, analytics, expiry indexes, and batch jobs.

Every Papermake process that uses S3 needs credentials with read and write access
to that bucket. Keep those credentials in deployment secrets, not in images or
source-controlled files.

Use least-privilege credentials where your object store allows it:

| Process | Needs |
|---|---|
| Server | Read/write templates, renders, analytics raw records, expiry records, jobs |
| Render worker | Read templates and jobs; write render outputs and job results |
| Maintenance worker | Read analytics, expiry, renders, and jobs; write summary data; delete expired data |

Rotate credentials by updating process environment and restarting all Papermake
processes. See [Operations -> Rotate S3 credentials](../how-to/operations.md#rotate-s3-credentials).

## TLS and proxies

Papermake does not terminate TLS itself. Put TLS at the reverse proxy, ingress,
load balancer, or service mesh.

Papermake's web UI is server-rendered and uses same-origin browser requests.
API clients should call the service directly from trusted server-side or
internal network contexts. Browser-based cross-origin API access is not part of
the default deployment model.

Bind `HOST` according to the network boundary:

| Binding | Use |
|---|---|
| `127.0.0.1` | Local development or sidecar-only access |
| Private interface | Internal network access |
| `0.0.0.0` | Container or VM behind an ingress, firewall, or load balancer |

## Resource exhaustion

The main risks are expensive renders, large uploads, oversized input data,
storage growth, and queued batch work.

Papermake has some built-in limits:

| Limit | Setting or behavior |
|---|---|
| HTTP request body size | `REQUEST_BODY_LIMIT_BYTES`, default 50 MiB |
| Concurrent synchronous renders | `MAX_CONCURRENT_RENDERS` per server |
| Render timeout | `RENDER_TIMEOUT_SECONDS`, including queue wait |
| S3 operation timeout and retries | `S3_OP_TIMEOUT_SECONDS`, `S3_MAX_ATTEMPTS` |
| Batch parallelism | Number of render workers and `SHARD_SIZE` |
| Output retention | `RENDER_RETENTION_DAYS`, template `retain_days`, or render override |

For production, also set external limits:

| Control | Why |
|---|---|
| Proxy body-size limits | Reject oversized uploads before they reach Papermake |
| Rate limits by caller | Prevent one client from consuming all render slots |
| Container CPU and memory limits | Bound worst-case Typst and image/font processing |
| S3 lifecycle or backup policy | Control storage cost and recovery behavior |
| Monitoring on latency, failures, queue age, and bucket growth | Detect exhaustion before it becomes an outage |

If an incident involves runaway pruning, stop the maintenance worker. If an
incident involves render load, scale render workers for batch work or reduce
ingress traffic for synchronous renders. See [Operations](../how-to/operations.md)
for the routine checks.
