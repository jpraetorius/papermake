# Papermake on Kubernetes

This directory contains a plain Kubernetes base for running Papermake with:

- one HTTP server deployment;
- one scalable render worker deployment;
- one single-replica maintenance worker deployment;
- one ClusterIP service;
- one ConfigMap and one Secret for runtime configuration.

It assumes you provide:

- a Kubernetes cluster;
- pushed `papermake-server` and `papermake-worker` images;
- an S3-compatible bucket, endpoint URL, and credentials.

## Configure

Set the images in `kustomization.yaml`:

```yaml
images:
  - name: papermake-server
    newName: registry.example.com/papermake-server
    newTag: v0.1.0
  - name: papermake-worker
    newName: registry.example.com/papermake-worker
    newTag: v0.1.0
```

Edit `configmap.yaml` for the S3 endpoint, bucket, retention settings, worker
intervals, and request limits.

Edit or replace `secret.example.yaml` before applying it:

```yaml
stringData:
  S3_ACCESS_KEY_ID: "..."
  S3_SECRET_ACCESS_KEY: "..."
```

Do not commit real credentials. In a real environment, replace the example
Secret with your normal secret management flow.

The base does not expose an Ingress by default because Papermake has no built-in
authentication. If you expose it beyond a private network, configure TLS,
authentication, authorization, and rate limits at your edge first, then add
`ingress.yaml` to `kustomization.yaml` or expose the service through your normal
gateway.

## Apply

```bash
kubectl apply -k deploy/k8s

kubectl -n papermake rollout status deploy/papermake-server
kubectl -n papermake rollout status deploy/papermake-render-worker
kubectl -n papermake rollout status deploy/papermake-maintenance
```

Check the server without an Ingress:

```bash
kubectl -n papermake port-forward svc/papermake-server 3000:80
curl -fsS http://localhost:3000/health
```

## Operate

Scale render workers independently:

```bash
kubectl -n papermake scale deploy/papermake-render-worker --replicas=4
```

Keep `papermake-maintenance` at one replica in normal operation. The sample uses
`Recreate` strategy so an update does not briefly run two maintenance pods.

Render workers normally do not need `PAPERMAKE_WORKER_ID`; they choose a usable
id from the pod hostname. The sample sets a fixed id only for the single
maintenance worker.

If you mount extra fonts, mount the same font set at `/fonts` in the server and
render worker pods so synchronous and batch renders match.

See the docs for the full runbooks:

- [Kubernetes deployment](../../docs/how-to/kubernetes.md)
- [Operations](../../docs/how-to/operations.md)
- [Security model](../../docs/explanation/security.md)
