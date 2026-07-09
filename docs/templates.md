# Writing templates

A Papermake template is a [Typst](https://typst.app/) document plus metadata,
optionally with a JSON schema and extra files (assets, components). Templates
are stored immutably and addressed by content hash; a **tag** (like `latest` or
`v1.0.0`) is a mutable pointer to a specific version.

## Data injection

At render time the request's JSON is passed to Typst on `sys.inputs.data` as a
byte string. Decode it once at the top of your template:

```typst
#let data = json(bytes(sys.inputs.data))

= Invoice #data.number

*Bill to:* #data.customer.name \
*Date:* #data.date
```

Everything Typst supports is available — expressions, loops, functions, layout:

```typst
#let data = json(bytes(sys.inputs.data))

= Invoice #data.number

#table(
  columns: (auto, 1fr, auto),
  [*Item*], [*Description*], [*Amount*],
  ..data.line_items.map(item => (
    item.sku, item.description, [$#item.amount],
  )).flatten(),
)

*Total:* $#data.total
```

The data you pass to [`POST /api/render/...`](api.md#rendering) must
match what your template reads.

## Metadata

Every template carries metadata:

| Field | Required | Description |
|---|---|---|
| `name` | yes | Human-readable name |
| `author` | yes | Author email or identifier |
| `retain_days` | no | Per-template output retention default in days (`0` = keep forever). See [Analytics & retention](analytics-and-retention.md). |

## Optional: JSON schema

You can attach a JSON schema describing the expected input data. It is stored
with the template (as `schema.json`) for documentation/validation tooling.

## Multiple files, assets, and imports

Templates can include additional files — images, fonts, or Typst components you
`#import`. Reference them by their path within the template bundle:

```typst
#import "components/header.typ": header
#image("assets/logo.png", width: 120pt)

#header(data.title)
```

Publish the extra files alongside `main.typ` using the
[multipart publish endpoint](api.md#templates):

```bash
curl -X POST "http://localhost:3000/api/templates/invoice/publish?tag=latest" \
  -F "main_typ=@invoice.typ" \
  -F "files[components/header.typ]=@header.typ" \
  -F "files[assets/logo.png]=@logo.png" \
  -F 'metadata={"name":"Invoice","author":"you@example.com"}'
```

Files are content-addressed and deduplicated: publishing two templates that
share an identical `logo.png` stores it once.

## Tags and versions

- A tag is just a named pointer: `invoice:latest`, `invoice:v2`.
- Publishing to an existing tag moves it to the new content.
- The underlying content is immutable and addressed by `sha256:…`, so a given
  version never changes even if a tag is later re-pointed.

References accept these forms: `name`, `name:tag`, and (for verification)
`name:tag@sha256:…`.
