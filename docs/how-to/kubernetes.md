# Kubernetes

Use the manifests in [`deploy/k8s`](../../deploy/k8s/) as a plain Kubernetes
starting point. They run Papermake as a server deployment, a scalable render
worker deployment, and a single maintenance worker deployment.

## Prerequisites

Before applying the manifests, provide:

- a pushed `papermake-server` image;
- a pushed `papermake-worker` image;
- an S3-compatible bucket, endpoint URL, and credentials;
- an ingress, gateway, or port-forwarding plan for the HTTP service;
- a TLS and authentication boundary if the service is reachable outside a
  private network.

## Configure the base

Set image names and tags in `deploy/k8s/kustomization.yaml`.

Set S3, retention, request limit, and worker settings in
`deploy/k8s/configmap.yaml`.

Replace the placeholder credentials in `deploy/k8s/secret.example.yaml`, or
replace that resource with your normal Kubernetes secret management flow. Do not
commit real credentials.

Set the hostname and TLS annotations for your ingress controller in
`deploy/k8s/ingress.yaml`, or remove `ingress.yaml` from `kustomization.yaml`
and expose the service another way.

## Deploy

```bash
kubectl apply -k deploy/k8s

kubectl -n papermake rollout status deploy/papermake-server
kubectl -n papermake rollout status deploy/papermake-render-worker
kubectl -n papermake rollout status deploy/papermake-maintenance
```

Verify the server:

```bash
kubectl -n papermake port-forward svc/papermake-server 3000:80
curl -fsS http://localhost:3000/health
```

## Scale

Scale API traffic by increasing server replicas:

```bash
kubectl -n papermake scale deploy/papermake-server --replicas=2
```

Scale batch throughput by increasing render worker replicas:

```bash
kubectl -n papermake scale deploy/papermake-render-worker --replicas=4
```

Do not scale the maintenance deployment in normal operation. It aggregates
analytics and prunes data, so the sample keeps it at one replica and uses
`Recreate` strategy during updates.

Render workers normally do not need `PAPERMAKE_WORKER_ID`; they choose a usable
id from the pod hostname. Keep `PAPERMAKE_INSTANCE_ID` unset on scaled server
deployments so each server instance uses its own id for analytics flushes.

## Rotate S3 credentials

Papermake reads S3 credentials at process startup. After updating the
Kubernetes Secret, restart all Papermake deployments:

```bash
kubectl -n papermake rollout restart \
  deploy/papermake-render-worker \
  deploy/papermake-maintenance \
  deploy/papermake-server
```

Keep old and new credentials valid until the restarted pods are healthy.

## Fonts

The images use `/fonts` as `FONTS_DIR`. If templates depend on additional
system fonts, mount the same font volume into the server and render worker pods.
The maintenance worker does not render documents.

## Operations

For incident and maintenance tasks, see the
[operations guide](operations.md). It includes Kubernetes variants for scaling
workers, pausing pruning, rotating credentials, checking analytics freshness,
and backing up S3 data.
