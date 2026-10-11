# File (CSV / JSON / JSONL)

Reads from or writes to a local file. Useful as a one-shot source/sink for
migrating data in or out of the other connectors.

## URL format

```text
file:///absolute/path/to/file?format=<normal|json|text|raw|csv>
```

The path comes from the URI path itself (`file:///...`), not a query param.
`format` defaults to `normal` (the full message serialized as JSON).

The names are historical, so read them by what ends up in the file. Each has a second,
descriptive name that means exactly the same:

| `format` | Also | One line of the file |
|---|---|---|
| `raw` | `payload` | the payload and nothing else. For JSON payloads this is plain JSON lines |
| `json` | `envelope_json` | the whole message (id, metadata, payload) as JSON, the payload as a JSON value |
| `text` | `envelope_text` | the whole message as JSON, the payload as a string |
| `normal` | `envelope` | the whole message as JSON, the payload as text or base64 |

For a file other tools read, `raw` is almost always the one you want; `json` is for a file
mq-bridge reads back with ids and metadata intact.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `file:`.

```yaml
input:
  file: { path: "/data/in.csv", format: csv }
output:
  file: { path: "/data/out.jsonl", format: raw }
```

The URL path becomes the `path` field.

## Examples

**Load a CSV file into MongoDB, one-shot (first row = header):**

```bash
mqb copy --drain \
  --from file:///data/customers.csv?format=csv \
  --to 'mongodb://localhost?database=app&collection=customers'
```

**Export a table to JSONL, one-shot:**

```bash
mqb copy --drain \
  --from postgres://user:pass@localhost/app?table=orders \
  --to file:///data/orders.jsonl?format=json
```

**Tail a file as it grows (broadcast/subscribe mode), continuous:**

```bash
mqb copy \
  --from file:///var/log/app/events.log?mode=subscribe \
  --to kafka://kafka.local:9092?topic=app-events
```

## Key options

| Option | Purpose |
|---|---|
| `format` | `normal`, `json`, `text`, `raw`, or `csv`. |
| `delimiter` | Message delimiter. Defaults to newline. |
| `mode` | Consumer only: `consume` (from start), `subscribe` (tail from end), or persistent offset-tracked modes. |
| `compression` | Compress/decompress each batch: `none` (default), `gzip`, `lz4`, `zstd` (needs the `compression` build feature). A source must declare the same codec the file was written with. See [Compression](../cookbook/compression.md). On the command line a path ending in `.gz`, `.zst` or `.lz4` sets it; `compression=none` overrides. |

Full field list: [reference/file.md](../reference/file.md).

## CSV

- **Reading:** the first record is the header, and each later record becomes a JSON object
  keyed by it. Every value is read as a string; type them with a
  [`transform`](../cookbook/transform.md) schema. Quoted fields may hold commas, doubled
  quotes (`""`) and line breaks. CRLF files, a leading UTF-8 byte-order mark (Excel's
  "CSV UTF-8") and blank lines are handled: the BOM is stripped and blank lines are skipped.
  A repeated header name gets a suffix (`a,a` → keys `a`, `a_2`) so no column is lost.
- **Writing:** the payload must be a JSON object. A new file takes its columns from the
  first message's keys, sorted; appending to a non-empty file keeps that file's header, so
  rows stay aligned with it. Fields containing the separator, the quote character, a line
  break or the `delimiter` are quoted. A nested object becomes one `parent.child` column per
  leaf (`stats.avg`), which is what an [`aggregate`](../cookbook/aggregate.md) result needs;
  `csv.nested: json` writes its JSON text into one cell instead. Arrays are written as their
  JSON text, and `null` as `null`. A payload that is not an object, or a string with no UTF-8
  spelling (a lone `\ud800` escape), fails that message instead of writing a broken row.
  A header that exists but cannot be read fails the write as retryable; nothing is appended
  under guessed columns.
- `delimiter` separates records (rows), not fields. It must not contain the field separator
  or the quote character.

### Dialects

Comma-separated with `"` quotes and a header record is the default. Anything else goes in the
`csv` block:

```yaml
input:
  file:
    path: export.csv
    format: csv
    csv:
      separator: auto
```

| Field | Default | Meaning |
| --- | --- | --- |
| `separator` | `,` | One character, `tab`, `space`, hex (`0x1f`), or `auto`. `auto` is for sources: it takes whichever of `,` `;` tab `\|` occurs most often outside quotes in the first record. |
| `quote` | `"` | One character, or `none` when the file quotes nothing. |
| `header` | `true` | `false` when the first record is already data; a source then needs `columns`, and a sink writes rows only. |
| `columns` | — | Source: the keys to use instead of the header's. Sink: the columns to write, in this order. |
| `nested` | `flatten` | Sink: `flatten` or `json`, see above. |
| `on_mismatch` | `warn` | Sink: `warn` writes a record whose keys differ from the columns, leaving missing columns empty and dropping extra keys. `fail` rejects it, so a `dlq` takes it. |

| Export | Setting |
| --- | --- |
| Excel "CSV UTF-8", semicolon locales | `separator: ";"` (or `auto`); the byte-order mark and CRLF need nothing |
| Excel "Text (Tab delimited)", saved as UTF-8 | `separator: tab` |
| `mongoexport --type=csv` | default |
| `mongoexport --type=tsv` | `separator: tab` |
| `psql --csv`, `COPY … WITH (FORMAT csv)` | default; add `separator` for `DELIMITER ';'` |
| `COPY … WITH (FORMAT csv, HEADER false)`, `mongoexport --noHeaderLine` | `header: false` plus `columns` |

On the command line the block is one JSON parameter:
`mqb copy 'file:///data/export.csv?format=csv&csv={"separator":"auto"}' …`.

Not read: UTF-16 files (Excel's "Unicode Text"; re-save as UTF-8) and PostgreSQL's `COPY`
*text* format, which escapes with backslashes and writes `\N` for null.

## JSON lines (`normal`, `json`, `text`)

With `format: json`, a pretty-printed payload is written on one line (its line breaks are
insignificant JSON whitespace), so each message stays one line of the file. With a custom
`delimiter`, any occurrence of it inside a JSON string is written as a `\uXXXX` escape, which
decodes to the same value. A delimiter that would appear in JSON syntax itself, such as `,`,
fails the message; `0x1e` (record separator) never occurs in JSON and is a safe choice.
`raw` writes the payload untouched, so its delimiter must not occur in the payloads.
