# Template reference

This reference describes the template reference string, manifest, metadata, and
bundle rules used by Papermake.

For authoring examples, see [Writing templates](../how-to/templates.md).

## Template references

A template reference identifies a template tag, with optional manifest-hash
verification.

```text
[namespace/]name[:tag][@sha256:<64 hex chars>]
```

Accepted forms:

| Form | Resolves as |
|---|---|
| `invoice` | `invoice:latest` |
| `invoice:v1` | `invoice:v1` |
| `team/invoice` | `team/invoice:latest` |
| `team/invoice:v1` | `team/invoice:v1` |
| `invoice@sha256:...` | `invoice:latest`, verified against the hash |
| `team/invoice:v1@sha256:...` | `team/invoice:v1`, verified against the hash |

References are case-insensitive on input and are normalized to lowercase before
validation.

Reference components:

| Component | Rules |
|---|---|
| `namespace` | Optional; one segment before `/`; 1-255 chars |
| `name` | Required; 1-255 chars |
| `tag` | Optional; defaults to `latest`; 1-128 chars |
| `hash` | Optional; `sha256:` plus 64 hexadecimal characters |

`namespace` and `name` may contain lowercase ASCII letters, digits, `.`, `-`,
and `_`. They cannot start or end with `.`, `-`, or `_`.

`tag` may contain lowercase ASCII letters, digits, `.`, `-`, and `_`.

A hash-qualified reference still resolves through the tag. The hash is a guard:
Papermake returns an error if the tag no longer points at that manifest hash.
Hash-only references are not accepted.

## Tags

Tags are mutable pointers under `refs/`.

```text
refs/<name>/<tag>
refs/<namespace>/<name>/<tag>
```

Publishing to a tag creates or moves that pointer to the new manifest hash.
Existing manifests and blobs are content-addressed and immutable.

`latest` has no special storage behavior. It is only the default tag when a
reference omits `:tag`.

## Manifest hashes

A manifest hash identifies an exact template version.

Publishing stores:

| Object | Key | Meaning |
|---|---|---|
| File blobs | `blobs/sha256/<hash>` | Content-addressed bytes for `main.typ`, assets, schema, components, and fonts |
| Manifest | `manifests/sha256/<hash>` | JSON document mapping bundle paths to file hashes and metadata |
| Tag ref | `refs/.../<tag>` | Text containing the manifest hash |

File hashes are SHA-256 hashes of file bytes. The manifest hash is the SHA-256
hash of the serialized manifest JSON, so metadata changes can produce a new
manifest hash even when file bytes are unchanged.

Two publishes with identical manifest JSON produce the same manifest hash.

## Manifest shape

Manifests are JSON:

```json
{
  "entrypoint": "main.typ",
  "files": {
    "main.typ": "sha256:...",
    "schema.json": "sha256:...",
    "assets/logo.png": "sha256:...",
    "fonts/Inter.ttf": "sha256:..."
  },
  "metadata": {
    "name": "Invoice",
    "author": "you@example.com",
    "retain_days": 30
  }
}
```

Manifest fields:

| Field | Meaning |
|---|---|
| `entrypoint` | Always `main.typ` |
| `files` | Map of bundle-relative paths to `sha256:<64 hex>` file hashes |
| `metadata` | Template metadata stored with the version |

The entrypoint must be present in `files`.

## Metadata

Template metadata is part of the manifest.

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Human-readable template name; must not be empty |
| `author` | yes | Author email or identifier; must not be empty |
| `retain_days` | no | Per-template output retention default; `0` keeps outputs forever |

If `retain_days` is absent, renders use the global default unless the render
request provides its own `retain_days`.

Unknown metadata fields are not stored.

## Schema

A template can include a JSON schema. Papermake stores it as `schema.json` in
the bundle and references it from the manifest like any other file.

Schema behavior:

| Behavior | Detail |
|---|---|
| Publish validation | If present, the schema must be valid JSON |
| Render validation | Render input is not validated against the schema by Papermake |
| Storage | The schema is content-addressed and versioned with the template |
| Use | Tooling and humans can use it to understand the expected input shape |

In `publish`, the `schema` form field becomes `schema.json`.

## Bundle file paths

The publish API builds a bundle from:

| Source | Bundle path |
|---|---|
| `main_typ` | `main.typ` |
| `schema` | `schema.json` |
| `files[<path>]` | `<path>` |

Bundle paths are stored without a leading slash.

Path rules:

| Rule | Reason |
|---|---|
| Path must not be empty | Every file needs a manifest key |
| Path must not start with `/` | Bundle paths are relative |
| Path must not contain `..` | Prevents traversal outside the bundle |

Use forward-slash paths such as `components/header.typ` and `assets/logo.png`.
Do not publish `main.typ` through `files[...]`; use the required `main_typ`
field for the entrypoint.

During rendering, files are available to Typst by their bundle path. For
example, `files[assets/logo.png]` can be used as:

```typst
#image("assets/logo.png")
```

## Font files

Template bundles may contain fonts. Files with these extensions are parsed and
registered in addition to the process fonts:

| Extension | Meaning |
|---|---|
| `.ttf` | TrueType font |
| `.otf` | OpenType font |
| `.ttc` | TrueType collection |

Extension matching is case-insensitive. Font files are still normal bundle
files: they are content-addressed, listed in the manifest, and can live at any
valid bundle path.
