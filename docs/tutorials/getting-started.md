# Getting started

This tutorial takes a fresh checkout to a rendered PDF.

## 1. Start Papermake

```bash
git clone https://github.com/jpraetorius/papermake
cd papermake
docker compose up -d
```

This starts the HTTP server, render worker, maintenance worker, and local
S3-compatible object store.

Check the server:

```bash
curl http://localhost:3000/health
```

Open **http://localhost:3000/** for the web UI.

## 2. Publish a template

Use the simple JSON publish endpoint:

```bash
curl -X POST "http://localhost:3000/api/templates/hello/publish-simple?tag=latest" \
  -H 'Content-Type: application/json' \
  -d '{
    "main_typ": "= Hello #data.name\nWelcome aboard.",
    "metadata": { "name": "Hello", "author": "you@example.com" }
  }'
```

The response includes the template reference:

```json
{
  "data": {
    "message": "Template 'hello:latest' published successfully",
    "manifest_hash": "sha256:...",
    "reference": "hello:latest"
  },
  "message": "Template published with reference 'hello:latest'"
}
```

## 3. Render the template

Put render input under the `data` key:

```bash
curl -X POST "http://localhost:3000/api/render/hello:latest" \
  -H 'Content-Type: application/json' \
  -d '{"data": {"name": "Ada"}}'
```

The response returns a `render_id`:

```json
{ "data": { "render_id": "0192...", "pdf_hash": "sha256:...", "duration_ms": 12 } }
```

Download the PDF:

```bash
curl "http://localhost:3000/api/renders/<render_id>/pdf" --output hello.pdf
```

Open `hello.pdf`. It should say "Hello Ada".

## 4. Try the UI

In **http://localhost:3000/**, open the template detail page. You can edit the
source, test-render inline, and publish a new version.

## Next steps

- [Writing templates](../how-to/templates.md)
- [HTTP API reference](../reference/api.md)
- [Self-hosting](../how-to/self-hosting.md)
