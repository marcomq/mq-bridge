# Object storage (S3 / GCS / Azure / local)

The `object_store` endpoint reads and writes files in a bucket or a directory: Amazon S3 and
S3-compatible stores (MinIO, Cloudflare R2), Google Cloud Storage, Azure Blob Storage and a
local directory. The `url` picks the store; everything else on this page is the same for all
of them.

| Store | `url` | Environment variables for the usual setup |
|---|---|---|
| [Amazon S3](#amazon-s3) | `s3://bucket/prefix` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` |
| [MinIO, Cloudflare R2, other S3-compatible](#s3-compatible-stores-minio-cloudflare-r2) | `s3://bucket/prefix` | the same, plus `AWS_ENDPOINT_URL` |
| [Google Cloud Storage](#google-cloud-storage) | `gs://bucket/prefix` | `GOOGLE_SERVICE_ACCOUNT` |
| [Azure Blob Storage / ADLS Gen2](#azure-blob-storage) | `az://container/prefix` | `AZURE_STORAGE_ACCOUNT_NAME`, `AZURE_STORAGE_ACCOUNT_KEY` |
| [Local directory](#local-directory) | `file:///absolute/path` (CLI: `local-store:///absolute/path`) | none |

Use it for files that hold many records each and are written once: exports, archives, data-lake
tables, drop zones. As a sink it writes one file per batch; as a source it reads the files under
the prefix in name order and remembers the last one it finished.

```yaml
orders_to_s3:
  input:
    sqlx: { url: "postgres://user:pass@localhost/app", table: "orders", cursor_column: "id" }
  output:
    object_store:
      url: "s3://my-bucket/orders"
      format: parquet
```

```bash
export AWS_ACCESS_KEY_ID=AKIA... AWS_SECRET_ACCESS_KEY=... AWS_REGION=eu-central-1
mqb copy --drain \
  --from 'postgres://user:pass@localhost/app?table=orders&cursor_column=id' \
  --to 's3://my-bucket/orders?format=parquet'
```

## Credentials

Credentials and store options are read from the **environment of the mq-bridge process**. The
endpoint has no fields for them, and they cannot be put into the `url`. This is the way the
cloud SDKs and command lines work, so a machine, container or CI job that is set up for
`aws`, `gcloud` or `az` usually needs nothing more.

Every option of the store can be set this way, not only the credentials. The rule for the
variable name:

- take the option's name, for example `region`, `endpoint` or `timeout`;
- write it in upper or lower case, it does not matter;
- put the store's prefix in front: `AWS_`, `GOOGLE_` or `AZURE_`. The name without the prefix
  is read as well.

So `AWS_REGION`, `aws_region` and `REGION` all set the region of an S3 store. Besides the
credentials in the sections below, these are the options most often needed:

| Variable | Effect | Example |
| :--- | :--- | :--- |
| `AWS_ENDPOINT_URL` | Another S3 endpoint (MinIO, R2, LocalStack) | `http://localhost:9000` |
| `AWS_ALLOW_HTTP` / `AZURE_ALLOW_HTTP` | Allow an endpoint without TLS | `true` |
| `AWS_VIRTUAL_HOSTED_STYLE_REQUEST` | `bucket.host` instead of `host/bucket` | `true` |
| `AWS_SERVER_SIDE_ENCRYPTION`, `AWS_SSE_KMS_KEY_ID` | Encrypt new objects with SSE-S3 or a KMS key | `aws:kms` |
| `AWS_REQUEST_PAYER` | Read a requester-pays bucket | `true` |
| `AWS_SKIP_SIGNATURE` | Read a public bucket without credentials | `true` |
| `TIMEOUT`, `CONNECT_TIMEOUT` | Longest request, longest connect | `60s` |
| `PROXY_URL` | Send requests through an HTTP proxy | `http://proxy.internal:3128` |
| `ALLOW_INVALID_CERTIFICATES` | Do not verify the TLS certificate. For tests only | `true` |

What to know about it:

- **The bucket or container always comes from the `url`**, never from a variable.
- **All stores of one kind share the variables.** Every `s3://` endpoint and every `s3://`
  `checkpoint_store` of a process uses the same account, region and endpoint. Two S3
  accounts, or AWS S3 and MinIO, in one process are not possible; run two processes.
  Different clouds (S3 and GCS, say) work side by side.
- **A short name can already be in use.** `REGION`, `ENDPOINT`, `TOKEN`, `TIMEOUT` or
  `PROXY_URL` set for another program are read as store options too. When the short and the prefixed name are both set, to different
  values, it is not defined which one is used. If a store behaves oddly, look for such a
  variable.
- **A variable that is not an option is ignored**, and so is an option of another store.
- **The bucket or container must exist.** mq-bridge does not create it.

## Amazon S3

```text
s3://<bucket>/<prefix>
```

`s3a://` is accepted as well.

| Variable | Purpose |
|---|---|
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | Access key. |
| `AWS_SESSION_TOKEN` | Needed with temporary credentials (SSO, an assumed role). |
| `AWS_REGION` | Region of the bucket. Defaults to `us-east-1`, so set it for every other region. |
| `AWS_WEB_IDENTITY_TOKEN_FILE`, `AWS_ROLE_ARN` | IAM role for a Kubernetes service account (IRSA). EKS sets both. |

Without an access key the endpoint uses, in this order: the web identity token, the ECS task
role, EKS Pod Identity, the EC2 instance profile. On EC2, ECS and EKS nothing has to be set
besides `AWS_REGION`.

`~/.aws/credentials`, `~/.aws/config` and `AWS_PROFILE` are **not** read. To use a profile or an
SSO login, export it into the environment first:

```bash
eval "$(aws configure export-credentials --profile my-profile --format env)"
export AWS_REGION=eu-central-1
```

The IAM permissions needed are `s3:PutObject` for a sink, and `s3:ListBucket` and `s3:GetObject`
for a source. A sink that names objects by source position (see [Writing](#writing-sink)) lists
the prefix as well.

Server-side encryption with a KMS key is set with `AWS_SERVER_SIDE_ENCRYPTION=aws:kms` and
`AWS_SSE_KMS_KEY_ID`.

## S3-compatible stores (MinIO, Cloudflare R2)

The URL stays `s3://bucket/prefix`; `AWS_ENDPOINT_URL` points it at the other server.

**MinIO** (or LocalStack) on plain HTTP:

```bash
export AWS_ENDPOINT_URL=http://localhost:9000
export AWS_ALLOW_HTTP=true
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1
mqb copy --drain --from 'file://orders.csv?format=csv' --to 's3://lake/orders?format=parquet'
```

`AWS_ALLOW_HTTP=true` is required for an `http://` endpoint. Requests are path-style
(`http://host/bucket/key`), which is what MinIO expects; set
`AWS_VIRTUAL_HOSTED_STYLE_REQUEST=true` for a server that wants the bucket in the host name.

**Cloudflare R2:**

```bash
export AWS_ENDPOINT_URL=https://<account-id>.r2.cloudflarestorage.com
export AWS_REGION=auto
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
```

## Google Cloud Storage

```text
gs://<bucket>/<prefix>
```

The CLI also accepts `gcs://` and rewrites it to `gs://`.

| Variable | Purpose |
|---|---|
| `GOOGLE_SERVICE_ACCOUNT` | Path to a service-account key file (JSON). |
| `GOOGLE_SERVICE_ACCOUNT_KEY` | The key file's JSON content itself, for when there is no file to mount. |
| `GOOGLE_APPLICATION_CREDENTIALS` | Path to an application-default-credentials file. |

Without any of them the endpoint uses the file that `gcloud auth application-default login`
writes, then the metadata server. On GCE, GKE (with Workload Identity) and Cloud Run nothing
has to be set.

```bash
export GOOGLE_SERVICE_ACCOUNT=/secrets/gcs-writer.json
mqb copy --drain \
  --from 'postgres://user:pass@localhost/app?table=orders&cursor_column=id' \
  --to 'gs://my-bucket/orders?format=parquet'
```

The service account needs `roles/storage.objectCreator` on the bucket for a sink and
`roles/storage.objectViewer` for a source. A sink that names objects by source position lists
the prefix and needs both.

## Azure Blob Storage

```text
az://<container>/<prefix>
```

The first part of the URL is the **container**. The storage account is not part of this URL; it
comes from `AZURE_STORAGE_ACCOUNT_NAME`. `azure://`, `abfs://` and `abfss://` work the same way,
and all of them use the Blob endpoint, also on an account with hierarchical namespace
(ADLS Gen2).

| Variable | Purpose |
|---|---|
| `AZURE_STORAGE_ACCOUNT_NAME` | Storage account. Required with the `az://container/…` form. |
| `AZURE_STORAGE_ACCOUNT_KEY` | Account access key. |
| `AZURE_STORAGE_SAS_TOKEN` | A SAS token instead of the key. |
| `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_TENANT_ID` | Service principal. |
| `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE` | Workload identity on AKS, which sets all three. |
| `AZURE_USE_AZURE_CLI=true` | Use the login of `az login`. For a developer machine. |

Without any credential the endpoint uses the managed identity of the VM, App Service or
container it runs on. A service principal or identity needs the role *Storage Blob Data
Contributor* (sink) or *Storage Blob Data Reader* (source) on the container.

```bash
export AZURE_STORAGE_ACCOUNT_NAME=mystorageaccount
export AZURE_STORAGE_ACCOUNT_KEY=...
mqb copy --drain \
  --from 'postgres://user:pass@localhost/app?table=orders&cursor_column=id' \
  --to 'az://lake/orders?format=parquet'
```

In a config file the account can be named in the URL instead, in the form Spark and Databricks
use:

```yaml
output:
  object_store:
    url: "abfss://lake@mystorageaccount.dfs.core.windows.net/orders"
    format: parquet
```

For the Azurite emulator set `AZURE_STORAGE_USE_EMULATOR=true`.

## Local directory

In a config file, use `file://`:

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

| Connector | Source model | Acknowledgement behavior | Best fit |
|---|---|---|---|
| `file` | One named file | Reads or tails that file | A fixed CSV/JSONL file or append-only log |
| `dir_spool` | One opaque message per chunk file | Acknowledged chunks are deleted by default; nacked chunks are redelivered | A durable filesystem queue between producers and consumers |
| local `object_store` | Many immutable, multi-record files | A checkpoint records the last fully acknowledged path; inputs remain in place | ETL drop zones and replayable local archives |

## Writing (sink)

Every batch the route delivers becomes **one object**. An object is written once and never
appended to or replaced, so the route's `batch_size` decides how large the files get: raise it
for an analytics table, where many small files are slow to query.

The object key depends on `name_by`:

- `write_time`: `<prefix>/2026/10/12/<uuidv7>.<ext>`. The date is the write time in UTC.
  `date_partition: false` leaves the date folders out, and `date_partition_style: hive`
  writes them as `year=2026/month=10/day=12/`, which Athena, Spark, BigQuery and DuckDB read
  as partition columns.
- `source_position`: the name carries the range of source records the object holds, and the
  objects lie flat under the prefix. When a run is repeated or resumed after a crash, the
  sink finds the ranges that are already there and does not write them a second time. This
  needs an input with a replay position: Kafka, Postgres CDC, an SQL cursor, a MongoDB change
  stream.
- `auto` (default): `source_position` when the input has a replay position, otherwise
  `write_time`. With a middleware that drops records (`filter`, `deduplication`) it is
  `write_time` as well; see [object naming under a filter](../reference/cli.md#object-naming-under-a-filter).

The extension follows `format`, `compression` and `encryption`: `jsonl`, `csv`, `parquet`,
`jsonl.gz`, `jsonl.zst.enc`. Set `extension` to override it.

A `csv` sink gives every file its own header, taken from that batch's first row, so set
`csv.columns` when all files must share the same columns.

## Reading (source)

New objects are discovered by polling (`polling_interval_ms`, default 1000). Objects are
processed in lexicographic key order, and the checkpoint advances once every record of an
object has been acknowledged. Objects are not deleted or changed.

- Set `cursor_id` and `checkpoint_store` to resume after a restart. Without them every start
  reads all objects again.
- Keep `checkpoint_store` outside the prefix that is read, otherwise the cursor object would
  be read as data. For a bucket this means another bucket or a prefix next to it, such as
  `s3://my-bucket/cursors` for a source on `s3://my-bucket/orders`.
- Name externally produced files with a sortable prefix, such as a timestamp or UUIDv7: an
  object added later with a key before the saved checkpoint is not read. Objects written by an
  `object_store` sink sort correctly.
- With `exit_on_empty` (`mqb copy --drain`) the run ends when the listing has nothing left
  after the last object.

An object without records is passed over.

An object that cannot be decoded (damaged, or written with another `compression` or
`encryption` than configured) is tried five times. Then:

- If the source has not decoded any object since it started, the route stops with an error
  that names the settings to check. The checkpoint does not move. If that object really is
  damaged, remove it.
- Otherwise the object is skipped with a warning and the checkpoint moves past it.
- With `on_decode_error: fail` the route stops at every such object and nothing is skipped.

## Formats and batching

`format` applies to every object under the configured prefix; formats are not detected
from file extensions. Use separate prefixes and routes for mixed formats.

- `csv` treats the first row as headers and emits each following row as a JSON object. The
  `csv` block (`separator`, `quote`, `header`, `columns`) reads other dialects, exactly as for
  the [file connector](file.md#dialects); `separator: auto` is settled on the first object read.
- `parquet` reads every row of a Parquet file as one JSON object, with the file's column types
  kept (numbers stay numbers). It reads what the `object_store` sink writes and files from
  other tools, on S3, GCS, Azure and local directories alike, and needs the `parquet` build
  feature, which `mqb` includes. The file is fetched whole, but its rows are decoded batch
  by batch as the route consumes them, so memory stays near the file's size.
- `normal` and `json` read mq-bridge message wrappers, one per line, and preserve metadata.
- For ordinary third-party JSONL, use `raw`: each line becomes one message payload without
  validating or transforming its JSON.
- `text` and `raw` emit delimited byte records without JSON or CSV parsing. A JSON array is not
  expanded into records.

One input object can produce many messages, and route batching still applies. The source
fetches each object into memory before splitting it, so set `max_object_bytes` for untrusted or
large drop zones.

`compression` (`gzip`, `lz4`, `zstd`) and `encryption` apply to the whole object and must be
the same on the sink and on the source that reads it back; see
[Compression](../cookbook/compression.md) and [Encryption at rest](../cookbook/encryption.md).

## Troubleshooting

| Symptom | Cause |
|---|---|
| The route waits for some seconds, then fails with a metadata or `169.254.169.254` error | No credentials were found in the environment, so the instance metadata service was tried. Check the variable names; on AWS, remember that profiles are not read. |
| S3 rejects the request although the key is valid, or answers with a redirect | `AWS_REGION` is not the bucket's region. |
| `Account must be specified` (Azure) | `AZURE_STORAGE_ACCOUNT_NAME` is not set. |
| Azure writes to a container named like the storage account | The URL was written as `az://account/container/…`. The first part is the container. |
| An `http://` endpoint is refused | Set `AWS_ALLOW_HTTP=true`. |
| A source finds nothing new | The new objects sort before the saved checkpoint, or the prefix differs from the one written to. |

## See also

- Every option: the [generated object-store reference](../reference/object-store.md).
- Warehouses that read these files: [DuckDB](../cookbook/duckdb.md),
  [Snowflake](../cookbook/snowflake.md), [BigQuery](../cookbook/bigquery.md),
  [Databricks / Spark](../cookbook/databricks.md), [Athena / Trino](../cookbook/athena.md),
  [ClickHouse](../cookbook/clickhouse.md).
- [Checkpoints & resumable copies](../cookbook/checkpoints.md) and
  [Deduplication](../cookbook/deduplication.md) for repeatable runs.
