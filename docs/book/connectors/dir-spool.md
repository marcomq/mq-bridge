# Directory spool

Stores each message as a payload file in a directory, with an optional JSON
metadata sidecar. Use it as a durable FIFO hand-off between processes when you
want a queue that can be inspected with ordinary filesystem tools and do not
want to operate a broker.

Unlike the [file connector](./file.md) and local [object storage](./object-store.md),
which can frame many records in a file, the directory spool writes one opaque
payload per file. A producer may exit while a consumer continues draining the backlog.

## URL format

```text
dir-spool:///absolute/path/to/spool?<option>=<value>
```

The aliases `spool://` and `dirspool://` are also accepted. The directory path
comes from the URI path, not a `path` query parameter.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `dir_spool:`.

```yaml
output:
  dir_spool: { path: "/var/spool/orders" }
```

The URL path becomes the `path` field.

## Source, target, and on-disk layout

As a **target** (sink), the connector writes each incoming message as one chunk:

```text
/var/spool/orders/
├── 000000000.bin   # raw payload bytes for message 0
├── 000000000.json  # message id and string metadata for message 0
├── 000000001.bin   # raw payload bytes for message 1
├── 000000001.json  # message id and string metadata for message 1
├── PRODUCER        # producer claim file
├── CONSUMER        # draining-consumer claim file, when one is running
└── DONE            # optional completion sentinel
```

The default `{seq:09}` naming pattern produces the zero-padded names above.
`payload_extension` and `metadata_extension` change only the suffixes. A sidecar
has this shape:

```json
{
  "message_id": "0195f4a0c7d77801a2b3c4d5e6f78901",
  "metadata": {
    "content_type": "application/json",
    "source": "orders"
  }
}
```

The payload file contains exactly the message payload, byte for byte. It may be
UTF-8 text, JSON, an image, compressed data, or any other binary content. The
connector does not add a delimiter, envelope, or encoding. Set
`metadata_extension` to an empty string when sidecars are not needed.

With the default `atomic=true`, files are first written to sibling `.tmp` names.
The sidecar is finalized before the payload; the payload's final rename makes
the chunk visible to readers without exposing incomplete data.

As a **source**, the connector scans for files with `payload_extension`, reads
each whole file as one message, and restores its message id and metadata from
the matching sidecar when present. A payload without a sidecar is valid and is
delivered with empty metadata, so another program can produce chunks without
knowing the sidecar format. Such a producer should likewise finalize the
payload last.

With sharding enabled, the leading sequence digits become directories. For
example, `{seq:09}`, `shard_depth=2`, and `shard_width=3` store message 1 as:

```text
/var/spool/orders/000/000/001.bin
/var/spool/orders/000/000/001.json
```

Both source and target must use the same sharding depths, widths, and file
extensions.

## Payload formats and batching

`dir_spool` does not parse or generate JSONL, CSV, or other record-oriented
formats. A `.jsonl` or `.csv` payload file is still one opaque message, even if
it contains many lines. Changing `payload_extension` changes file selection and
naming, not parsing behavior.

Use a record-oriented connector when records must be framed:

- The [file connector](./file.md) reads or writes one named file, for example
  `file:///data/orders.jsonl?format=raw` or `file:///data/orders.csv?format=csv`.
- Local [object storage](./object-store.md) watches a directory of immutable,
  multi-record files, making it a better fit for CSV/JSONL ETL drop zones.
- Sending either source to a directory spool creates one chunk per emitted
  message, rather than one chunk containing the whole input file.

Route-level batching is supported in both directions. A target can receive a
batch of messages and writes one payload/sidecar pair for each message. A source
can return up to the route's batch-size limit per read. Batching therefore
reduces routing and filesystem-sync overhead but does not change the one-file,
one-message chunk format.

## Examples

**Write a finite input to a spool and mark it complete:**

```bash
mqb copy --drain \
  --from file:///data/orders.jsonl?format=raw \
  --to 'dir-spool:///var/spool/orders?emit_done=success'
```

**Drain that spool into PostgreSQL, then exit:**

```bash
mqb copy --drain \
  --from 'dir-spool:///var/spool/orders?stop_on_done=true' \
  --to 'postgres://user:pass@localhost/app?table=orders'
```

This one-shot example starts after the producer command has completed, so
`--drain` exits when the backlog is empty. In a continuously running route,
`stop_on_done=true` ends the source only when both the queue is empty and the
producer's `DONE` sentinel exists. Set `emit_done` only on the last producer;
a producer opening the spool removes a stale sentinel before writing again.

**Shard a high-volume spool:**

```bash
mqb copy \
  --from mqtt://broker.local:1883?topic=telemetry \
  --to 'dir-spool:///var/spool/telemetry?naming_pattern={seq:012}&shard_depth=2&shard_width=3'
```

Configure the consumer with the same `shard_depth`, `shard_width`, payload
extension, and metadata extension. Sharding uses leading sequence digits as
subdirectories and avoids placing an unbounded number of files in one
directory.

## Delivery and concurrency

Chunks are delivered in lexical filename order. Keep a zero-padded sequence at
the start of `naming_pattern`; the default `{seq:09}` is safe. The producer
writes temporary files and renames them into place, so the consumer does not
observe a partial chunk. With the default `fsync=chunk`, acknowledged writes
are also flushed to durable storage.

A draining consumer deletes a chunk only after its message is acknowledged. A
nack leaves the chunk on disk for redelivery. Set `drain_on_read=false` for a
non-destructive pass that leaves files in place.

By default, one producer and one draining consumer may use a spool at the same
time. Separate `PRODUCER` and `CONSUMER` lock files enforce those roles without
preventing the normal producer-consumer pair. Avoid disabling these claims:
multiple producers can collide, and multiple draining consumers can deliver
duplicates because claiming is in memory rather than an on-disk rename.

## Key options

| Option | Purpose |
|---|---|
| `naming_pattern` | Sink only: chunk name template. It must begin with a sequence; default `{seq:09}`. |
| `payload_extension` / `metadata_extension` | File suffixes. Set `metadata_extension` to an empty string to omit sidecars. |
| `atomic` | Sink only: write through a temporary file and rename; default `true`. |
| `fsync` | `chunk` (default) for durable writes, or `off` for higher throughput with weaker crash guarantees. |
| `emit_done` | Sink only: write the completion sentinel on `success`, on any `end`, or `never` (default). |
| `stop_on_done` | Source only: exit after the sentinel is present and the backlog is empty. |
| `drain_on_read` | Source only: delete acknowledged chunks; default `true`. |
| `shard_depth` / `shard_width` | Spread chunks across sequence-derived subdirectories. Both ends must agree. |
| `claim` | `exclusive` (default), `warn`, or `off` for same-role locking. |

Full field list: [reference/dir-spool.md](../reference/dir-spool.md).

For detailed durability, sharding, lock, and completion-sentinel behavior, see
the [configuration guide](../engine/configuration.md#directory-spool-dir_spool).
