# Encryption at rest

Two ways to encrypt data, depending on whether you want it encrypted **in transit between
endpoints** or **at rest in a file / object**.

## The `encryption` middleware — payloads in transit

The [`encryption`](../engine/reference.md#encryption) middleware seals each message **payload**
into a self-describing AEAD envelope on the output side and decrypts it on the input side.
Metadata and routing keys stay in the clear. Requires the `encryption` feature.

```yaml
output:
  middlewares:
    - encryption: { key: "${env:MQB_ENC_KEY}" }
  nats: { subject: "secure.orders", url: "nats://localhost:4222" }
```

The key is a base64-encoded 32-byte key; `${env:VAR}` reads it from the environment (see
[Secrets & interpolation](secrets.md)). Cipher defaults to `xchacha20poly1305` (`aes256gcm`
also available).

A sealed payload is binary, so a `file`/`object_store` sink that stores it must use
`format=normal` — `format=json`/`text` render the payload as a JSON value and it cannot be read
back (the reader reports "unsupported encryption envelope version 91"). Stacking it with
`compression` works, but the reading route must list the two middlewares in the **reverse**
order, since middlewares apply in list order on both ends.

From the `copy` CLI it is an inline middleware; keep the key in `${env:VAR}` so it never reaches
the process list or shell history (`{`/`}` percent-encode to `%7B`/`%7D`):

```bash
MQB_ENC_KEY=… mqb copy \
  --from 'file:///tmp/orders.jsonl' \
  --to   'nats://localhost:4222?stream=secure&subject=secure.orders|encryption?key=$%7Benv:MQB_ENC_KEY%7D'
```

**Key rotation:** seal with a new `key_id`/`key` while listing the old key under `decrypt_keys`
on the consuming side. Each payload is authenticated independently — tampering, a torn frame, or
a wrong/missing key fails decryption. By default (`on_error: drop`) such a message is logged,
acknowledged and skipped, so a reader with the wrong key drains its input; set `on_error: fail`
to stop the route instead.

> The AEAD binds only the payload (empty associated data), so a sealed payload can be replayed
> under different metadata. Use [`deduplication`](deduplication.md) or a sink uniqueness
> constraint if that matters.

## File / object encryption at rest

To store data **compressed and encrypted at rest**, use the `file` / `object_store` endpoints'
own `compression` + `encryption` fields — they apply **compress-then-encrypt per batch**, which
is what you want (ciphertext does not compress):

```yaml
output:
  file:
    path: "data.enc"
    format: raw
    compression: lz4          # none | gzip | lz4 | zstd  (`compression` feature)
    encryption: { key: "${env:MQB_ENC_KEY}" }
```

> **Do not** stack the `encryption` *middleware* on top of a sink's batch `compression` on the
> same route — ciphertext won't compress. Use the endpoint fields above instead.

An encrypted file is written as length-prefixed sealed frames (one per batch) and is only
readable through a matching consumer. `object_store` derives its object extension from these
fields (e.g. `.jsonl.gz` / `.jsonl.lz4`, plus a trailing `.enc` when encryption is on).

## Reading it back

A file **source** must declare the **same** `compression`/`encryption` the data was written
with. A mismatch (wrong key, wrong codec, or a missing field) is a permanent decode failure — the
route ends `failed` with the error in its status, rather than completing as if the file were
empty. See [Troubleshooting](../operations/troubleshooting.md#encrypted--compressed-file-source-ends-immediately-or-failed)
and the [Compression](compression.md) recipe.
