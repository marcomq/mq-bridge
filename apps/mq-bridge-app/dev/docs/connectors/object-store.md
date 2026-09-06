# Object storage (local / cloud)

Use `object_store` when a directory or bucket contains immutable files with multiple records.
The same endpoint works with a local directory, S3-compatible storage, Google Cloud Storage,
and Azure Blob Storage.

For a local directory, use `file://` in YAML:

```yaml
load_orders:
  input:
    object_store:
      url: "file:///var/lib/mqb/incoming"
      format: csv
      cursor_id: "orders-import"
      checkpoint_store: "file:///var/lib/mqb/checkpoints/orders.json"
      polling_interval_ms: 1000
  output:
    sqlx:
      url: "postgres://localhost/app"
      table: orders
```

The CLI uses `local-store://` to distinguish this endpoint from the single-file connector:

```bash
mqb copy \
  --from 'local-store:///var/lib/mqb/incoming?format=csv&cursor_id=orders-import&checkpoint_store=file:///var/lib/mqb/checkpoints/orders.json' \
  --to 'nats://localhost:4222/orders'
```

New files are discovered by polling. Files are processed in lexicographic path order, and the
checkpoint advances once every record in a file has been acknowledged. Input files are not deleted.
Name externally produced files with a monotonically sortable prefix, such as a timestamp or
UUIDv7: a file added later with a name before the saved checkpoint is not read.

## Formats and batching

`format` applies to every file under the configured directory or prefix; formats are not detected
from file extensions. Use separate directories and routes for mixed formats.

- `csv` treats the first row as headers and emits each following row as a JSON object.
- `normal` and `json` read mq-bridge message wrappers, one per line, and preserve metadata.
- For ordinary third-party JSONL, use `raw`: each line becomes one message payload without
  validating or transforming its JSON.
- `text` and `raw` emit delimited byte records without JSON or CSV parsing. A JSON array is not
  expanded into records.

One input file can produce many messages, and route batching still applies. The source currently
fetches each file into memory before splitting it, so set `max_object_bytes` for untrusted or large
drop zones. Object-store sinks write one immutable file per flushed batch; CSV is source-only for
this connector.

## Choosing a local connector

| Connector | Source model | Acknowledgement behavior | Best fit |
|---|---|---|---|
| `file` | One named file | Reads or tails that file | A fixed CSV/JSONL file or append-only log |
| `dir_spool` | One opaque message per chunk file | Acknowledged chunks are deleted by default; nacked chunks are redelivered | A durable filesystem queue between producers and consumers |
| local `object_store` | Many immutable, multi-record files | A checkpoint records the last fully acknowledged path; inputs remain in place | ETL drop zones and replayable local archives |

Keep `checkpoint_store` outside the input directory. Otherwise its cursor file would be discovered
and parsed as input data.

For every option, see the [generated object-store reference](../reference/object-store.md).
