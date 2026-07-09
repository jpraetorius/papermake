# Papermake documentation

User-facing guides for running Papermake and turning Typst templates into a
rendering API.

## Guides

- **[Getting started](getting-started.md)** — bring up the stack and render your first document.
- **[Writing templates](templates.md)** — Typst templates, data injection, schemas, assets, imports.
- **[HTTP API reference](api.md)** — every endpoint, with request/response shapes.
- **[Analytics & retention](analytics-and-retention.md)** — how render analytics and output expiry work, and how to configure them.
- **[Self-hosting](self-hosting.md)** — deployment, environment variables, scaling, and the S3 storage layout.

## Design notes

- **[Buffered-S3 analytics + SSR UI](analytics-storage-and-ssr.md)** — the design/rationale behind the current storage and UI architecture (implementation reference, not a user guide).

New to the project? Start with the [README](../README.md) for the elevator
pitch, then follow [Getting started](getting-started.md).
