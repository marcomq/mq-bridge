# Compression

There are **two kinds of compression**, both behind the `compression` feature:

- the **batch `compression` field** on the `file` / `object_store` endpoints — compresses whole
  write batches, so the output stays readable with `zcat` / `lz4 -d`;
- the [**`compression` middleware**](#the-compression-middleware) — compresses each message
  *payload*, so it works over any transport (Kafka, NATS, HTTP, …), not just files.

## Endpoint batch compression

The `file` and `object_store` endpoints can compress each batch on write with the
`compression` field:

```yaml
output:
  file:
    path: "data.jsonl"
    format: json
    compression: lz4        # none | gzip | lz4 | zstd
```

| Codec | Notes |
|---|---|
| `none` | default — no compression |
| `gzip` | widest compatibility; standard `.gz` stream |
| `lz4` | fastest; standard `.lz4` stream |
| `zstd` | best ratio for the CPU cost |

`object_store` derives its default object extension from the codec (e.g. `.jsonl.gz` /
`.jsonl.lz4`).

## Reading it back

A file **source** must declare the **same** `compression` the data was written with:

```yaml
input:
  file:
    path: "data.jsonl"
    format: json
    compression: lz4
```

A mismatch — wrong codec, or a missing field — is a permanent decode failure: the route
ends `failed` with the error in its status rather than silently completing as if the
file were empty. Reading a compressed file with no `compression` set is likewise
rejected up front by sniffing the leading magic bytes, so raw compressed bytes are never
emitted as messages.

File compression supports only the default `consume` mode. `csv` works too: the header
row is written into the first member, so the decoded stream is a normal CSV file.

## The `compression` middleware

To compress payloads **over the wire** — a Kafka topic, a NATS subject, an HTTP body — attach
the [`compression`](../engine/reference.md#compression) middleware instead. It compresses each
message payload on the output side and decompresses it on the input side; metadata and routing
keys are untouched. It works on **input and output**:

```yaml
orders_bridge:
  input:
    middlewares:
      - compression: { algorithm: zstd }
    kafka: { topic: "orders", url: "localhost:9092" }
  output:
    middlewares:
      - compression: { algorithm: zstd }
    nats: { subject: "orders.out", url: "nats://localhost:4222" }
```

| Field | Default | Notes |
|---|---|---|
| `algorithm` | `zstd` | `none` \| `gzip` \| `lz4` \| `zstd`; `none` is a passthrough |
| `max_decompressed_bytes` | unset | consumer-side bomb guard; exceeding it is a permanent error |

Put the **same** `algorithm` on both sides of a route. Each payload is framed independently, so
unlike the endpoint field this is only readable through a matching consumer — a truncated or
corrupt frame is a permanent consumer error rather than an endlessly re-read poison message.

From the `copy` CLI it is an inline middleware like any other:

```bash
mqb copy \
  --from 'nats://localhost:4222?stream=orders&subject=orders.in|compression?algorithm=zstd' \
  --to   'file:///tmp/orders.jsonl'
```

A compressed payload is binary, so a `file`/`object_store` sink that stores it must use
`format=normal`. With `format=json` or `text` the payload is rendered as a JSON value and cannot
be read back. Verified round trip:

```bash
mqb copy --drain \
  --from 'file:///tmp/in.jsonl?format=raw' \
  --to   'file:///tmp/packed.bin?format=normal|compression?algorithm=zstd'
mqb copy --drain \
  --from 'file:///tmp/packed.bin?format=normal|compression?algorithm=zstd' \
  --to   'file:///tmp/out.jsonl?format=raw'
```

## Compression *and* encryption

Do **not** stack the [`encryption` middleware](encryption.md) on top of a sink's batch
`compression` on the same route — ciphertext does not compress. For compressed *and*
encrypted data at rest, use the endpoints' own fields, which apply
**compress-then-encrypt** per batch:

```yaml
output:
  file:
    path: "data.enc"
    format: raw
    compression: lz4
    encryption: { key: "${env:MQB_ENC_KEY}" }
```

An encrypted file is written as length-prefixed sealed frames (one per batch) and is
only readable through a matching consumer; `object_store` adds a trailing `.enc` since
the object is ciphertext, not a directly decompressible `.gz`.

## See also

- [Encryption at rest](encryption.md) — the encryption side and key handling.
- [Middleware & structural endpoints](../engine/reference.md) — the authoritative `compression`/`encryption` field docs.
- [Performance tuning → Compression & encryption cost](../operations/tuning.md).
