# Papermake documentation

Read the [README](../README.md) for the project overview. Use
[Getting started](tutorials/getting-started.md) to run Papermake and render a
first PDF.

## Guides

- [Getting started](tutorials/getting-started.md): bring up the local stack and
  render a document.
- [Writing templates](how-to/templates.md): author Typst templates for
  Papermake.
- [Batch rendering](how-to/batch-rendering.md): submit many inputs, monitor the
  job, and collect the resulting PDFs.
- [Self-hosting](how-to/self-hosting.md): run Papermake with Docker Compose,
  from source, or against your own S3-compatible storage.
- [Operations](how-to/operations.md): scale workers, inspect incidents, rotate
  credentials, and back up S3 data.

## Reference

- [HTTP API reference](reference/api.md): endpoints, request bodies, responses,
  and error behavior.
- [Configuration reference](reference/configuration.md): environment variables
  by process.
- [Template reference](reference/templates.md): reference strings, tag and
  manifest semantics, metadata, schemas, bundle paths, and fonts.

## Explanation

- [Architecture](explanation/architecture.md): the system components, storage
  model, and main data flows.
- [Analytics & retention](explanation/analytics-and-retention.md): how render
  history, rollups, and output expiry work.
- [Security model](explanation/security.md): deployment trust boundary,
  authentication status, Typst sandbox assumptions, and operational safeguards.
