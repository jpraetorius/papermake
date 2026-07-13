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

Templates can include additional files — images or Typst components you
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

### Bundling fonts

A template can ship its own fonts: any `.ttf`/`.otf`/`.ttc` file in the bundle
(anywhere — e.g. `fonts/Inter.ttf`) is registered for that template's renders,
on top of the server's built-in fonts. Reference the font in the template by its
**family name**, as usual:

```typst
#set text(font: "Inter")
```

This makes a template self-contained — no need to rebuild the server image to
add its typeface. Notes:

- Typst matches fonts by **family name**, not file path, so the bundled file
  just needs to provide the family your template selects.
- Only **TTF/OTF/TTC** are supported (no WOFF/WOFF2).
- Template fonts are **additive**: server fonts (embedded + `FONTS_DIR`) remain
  available; a same-named system font isn't replaced.
- Rendered PDFs embed the subset of each resolved font actually used, so PDF
  readers don't need the fonts installed.

Prefer providing common corporate fonts via a mounted `FONTS_DIR` volume (or
baked into a custom image) — see [Self-hosting → Fonts](self-hosting.md#fonts) —
and bundle only template-specific typefaces.

## Tags and versions

- A tag is just a named pointer: `invoice:latest`, `invoice:v2`.
- Publishing to an existing tag moves it to the new content.
- The underlying content is immutable and addressed by `sha256:…`, so a given
  version never changes even if a tag is later re-pointed.

References accept these forms: `name`, `name:tag`, and (for verification)
`name:tag@sha256:…`.
