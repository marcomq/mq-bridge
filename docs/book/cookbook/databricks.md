# Load into Databricks / Spark

There is no Databricks or Spark connector. The [`object_store`](../connectors/object-store.md) sink
writes Parquet to S3, GCS or Azure, and Spark reads Parquet from all three.

`format: parquet` needs the `parquet` feature, which the app's default build includes.

## Write Parquet to the lake

```yaml
orders_to_lake:
  batch_size: 10000
  input:
    kafka: { topic: "orders", url: "localhost:9092", group_id: "lake-export" }
  output:
    middlewares:
      - transform:
          schema:
            type: object
            required: [id]
            properties:
              id: { type: integer }
              country: { type: string }
              amount: { type: number }
              note: { type: string, default: "" }
    object_store:
      url: "s3://my-bucket/orders"        # or gs://…, az://container/…
      format: parquet
      compression: zstd
```

- Each payload must be a JSON object; its top-level fields become the columns.
- The sink writes one Parquet file per flushed batch, so `batch_size` sets the file size. Keep
  batches large: many small files slow every scan.
- The [`transform`](transform.md) schema coerces types (`"42"` becomes `42`) and rejects rows that
  do not fit, so a column does not flip between string and number across files. A `number`
  column is still written as a 64-bit integer when a batch holds only whole numbers, so declare
  it as a float column in the warehouse.
- A rejected row is dropped and logged. Add a [`dlq`](dlq.md) after the `transform` to keep it.
- `null` takes the field's `default` if it has one and is rejected otherwise. Mark a column the
  source can leave empty as `nullable: true` to keep the `null`.

## Schema drift between files

The Parquet schema is inferred **per batch**. An optional field that no row of a batch carries is
**absent from that file**. Give such a field a `default` in the schema, as `note` has above, or
ask Spark to merge the file schemas by name with `mergeSchema`. Without it Spark takes the schema
from one file and drops columns that file lacks.

## Load it

> Verify this code against the current Databricks and Spark documentation.

Read the files directly:

```python
df = spark.read.option("mergeSchema", "true").parquet("s3://my-bucket/orders/")
```

Or load them into a Delta table. `COPY INTO` skips files it has already loaded:

```sql
COPY INTO orders
  FROM 's3://my-bucket/orders/'
  FILEFORMAT = PARQUET
  FORMAT_OPTIONS ('mergeSchema' = 'true')
  COPY_OPTIONS ('mergeSchema' = 'true');
```

For continuous ingestion, Databricks Auto Loader (`cloudFiles` with `cloudFiles.format = parquet`)
picks up new files as they arrive.

## Replays and layout

With a replayable input such as Kafka, the default `name_by: auto` names each file after the
source range it holds, and a restart skips ranges already written. For Hive-style date partitions
(`year=…/month=…/day=…`), which Spark discovers as columns, see
[Choose the file layout](duckdb.md#choose-the-file-layout).
