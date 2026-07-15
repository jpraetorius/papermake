# Writing templates

A Papermake template is a Typst entrypoint plus metadata. It can also include a
JSON schema, assets, components, and fonts. Templates are immutable once
published; tags such as `latest` move between versions.

## Read input data

Papermake makes the render request's `data` value available as `data` in Typst:

```typst
= Invoice #data.number

*Bill to:* #data.customer.name \
*Date:* #data.date
```

Then use normal Typst for layout, loops, tables, and functions:

```typst
#table(
  columns: (1fr, auto),
  [*Item*], [*Amount*],
  ..data.items.map(item => (
    item.name,
    [$#item.amount],
  )).flatten(),
)
```

The JSON you send to [`POST /api/render/...`](../reference/api.md#rendering)
must match the fields your template reads.

## Set metadata

Every template publish request includes metadata:

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Human-readable template name |
| `author` | yes | Author email or identifier |
| `retain_days` | no | Default output retention for this template; `0` keeps outputs forever |

`retain_days` can still be overridden per render. See
[Analytics & retention](../explanation/analytics-and-retention.md).

## Add a schema

Attach a JSON schema when you want the expected input shape stored with the
template:

```json
{
  "type": "object",
  "required": ["number", "customer"],
  "properties": {
    "number": { "type": "string" },
    "customer": { "type": "object" }
  }
}
```

Papermake stores the schema as template metadata for tooling and documentation.

## Add files and assets

Use relative paths inside the template bundle:

```typst
#import "components/header.typ": header
#image("assets/logo.png", width: 120pt)

#header(data.title)
```

Publish those files with the multipart endpoint:

```bash
curl -X POST "http://localhost:3000/api/templates/invoice/publish?tag=latest" \
  -F "main_typ=@invoice.typ" \
  -F "schema=@schema.json" \
  -F "files[components/header.typ]=@header.typ" \
  -F "files[assets/logo.png]=@logo.png" \
  -F 'metadata={"name":"Invoice","author":"you@example.com"}'
```

Shared files are content-addressed and deduplicated.

## Bundle fonts

Put `.ttf`, `.otf`, or `.ttc` files anywhere in the template bundle, then select
the font by family name:

```typst
#set text(font: "Inter")
```

Keep common organization fonts in `FONTS_DIR`; bundle fonts that are specific to
one template. See [Self-hosting -> Fonts](self-hosting.md#fonts).

## Request PDF standards

Renders produce PDF 1.7 by default. Set `pdf_standard` in the render request to
ask for another output standard.

| Value | Standard | Notes |
|---|---|---|
| `1.7` | PDF 1.7 | Default |
| `2.0` | PDF 2.0 | Newer base version |
| `a-2b`, `a-3b` | PDF/A-2b, PDF/A-3b | Archival |
| `a-2a`, `a-3a` | PDF/A-2a, PDF/A-3a | Archival and tagged |
| `a-4` | PDF/A-4 | Archival, based on PDF 2.0 |
| `ua-1` | PDF/UA-1 | Accessibility |

For `ua-1`, set a document title or the render fails:

```typst
#set document(title: [Invoice #data.number])
```

Use a title for tagged archival profiles (`a-2a`, `a-3a`) as well.

## Use tags

- `invoice:latest` points to a tag.
- Publishing to an existing tag moves it to the new version.
- The version behind a manifest hash is immutable.

See the [template reference](../reference/templates.md) for the full reference
format, tag semantics, manifest shape, metadata rules, and bundle path rules.
