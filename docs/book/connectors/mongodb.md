# MongoDB

Reads from or writes to a MongoDB collection. As a source it can copy a collection, follow its
changes (change data capture), or work a collection as a job queue. `database` and
`collection` are parameters of their own; the database is not read from the URL path.

## URL format

```text
mongodb://[user:password@]host[:port]/?database=<db>&collection=<name>
```

| What you want | URL |
| :--- | :--- |
| Write JSON documents to a collection | `mongodb://localhost:27017/?database=shop&collection=orders&format=payload` |
| The same, with the payload's `order_id` as `_id` | `mongodb://localhost:27017/?database=shop&collection=orders&format=payload&id_field=order_id` |
| Copy a collection, then follow its changes | `mongodb://localhost:27017/?database=shop&collection=orders` |
| Follow changes only | `mongodb://localhost:27017/?database=shop&collection=orders&consume=capture_new` |
| Copy a collection once, without a replica set | `mongodb://localhost:27017/?database=shop&collection=orders&consume=snapshot` |
| User and password | `mongodb://app:secret@db.example.com:27017/?database=shop&collection=orders&authSource=admin` |
| Replica set, several hosts | `mongodb://_/?url=mongodb://db1:27017,db2:27017,db3:27017/?replicaSet=rs0&database=shop&collection=orders` |
| MongoDB Atlas (`mongodb+srv`) | `mongodb+srv://<user>:<password>@<cluster>.mongodb.net/?database=shop&collection=orders` |

Rules that the table follows:

- **`database` is required** and is always a parameter. `mongodb://localhost/shop?collection=orders`
  does not start.
- **Driver options stay on the connection string.** A parameter that is not a field of this
  connector, such as `replicaSet`, `authSource` or `tls=true`, is passed to the MongoDB driver.
- **A host list goes in `url=`**, with `_` as a placeholder host, because it is not a URL the
  command line can parse. In a config file the connection string is used as it is.

## Config (YAML / library)

Every URL parameter is a field of the same name under `mongodb:`. `url` takes any MongoDB
connection string.

```yaml
output:
  mongodb:
    url: "mongodb+srv://cluster0.ab12c.mongodb.net"
    username: "app"
    password: "${MONGO_PASSWORD}"
    database: "shop"
    collection: "orders"
    format: payload
```

`username` and `password` take precedence over credentials in the `url`. For TLS with a
private CA or a client certificate, set `tls`:

```yaml
output:
  mongodb:
    url: "mongodb://db.example.com:27017"
    database: "shop"
    collection: "orders"
    tls: { required: true, ca_file: "/etc/ssl/mongo-ca.pem" }
```

## Writing

`format` decides what a stored document looks like. **For a collection that other programs
query, use `format: payload`.** The default keeps the message intact for another mq-bridge route
to read, and stores the payload as binary.

For the message `{"order_id": "A-1", "total": 20}`:

| `format` | Stored document | Use for |
| :--- | :--- | :--- |
| `payload` (also `raw`) | `{ _id: …, order_id: "A-1", total: 20 }` | Documents you query |
| `json` | `{ _id: …, payload: { order_id: "A-1", total: 20 }, metadata: {…}, created_at: … }` | Queryable payload, metadata kept |
| `text` | `{ _id: …, payload: "{\"order_id\": …}", metadata: {…}, created_at: … }` | Text payloads |
| `normal` (default) | `{ _id: …, payload: <binary>, metadata: {…}, created_at: … }` | Queues between mq-bridge routes; any payload |

With `payload`, a payload that is not a JSON object is stored like `normal`.

Load a JSONL file into a collection, with the order number as `_id`:

```yaml
load_orders:
  input:
    file: { path: "/data/orders.jsonl", format: payload }
  output:
    mongodb:
      url: "mongodb://localhost:27017"
      database: "shop"
      collection: "orders"
      format: payload
      id_field: "order_id"
```

- **`id_field` makes the write repeatable.** Its value becomes `_id`, with its JSON type kept.
  A document whose `_id` exists is left as it is and counts as written, so running the load
  twice does not duplicate anything. It does not update the stored document. A message
  without the field fails. More in [Upserts & insert-if-absent](../cookbook/upserts.md).
- **Without `id_field`** the `_id` is the payload's own `_id` (`payload`), or one that is generated.
- **`ttl_seconds`** creates a TTL index, so documents are removed that long after they were
  written. **`capped_size_bytes`** creates the collection as a capped collection.

## Reading

`consume` selects one of four ways to read:

| `consume` | What it does | Changes the collection | Needs a replica set |
| :--- | :--- | :--- | :--- |
| `capture_all` (default) | Reads every document, then follows changes | no | yes |
| `capture_new` | Follows changes from now on | no | yes |
| `snapshot` | Reads every document once, then ends | no | no |
| `delete` | Job queue: claims a document, deletes it when done. Also accepted as `consumer` | **yes, deletes** | no |

A single-node replica set is enough for the two `capture` modes; without one they refuse to
start. The comparison in detail is under
[MongoDB consume modes](../reference/endpoints.md#mongodb-consume-modes).

Follow a collection and publish every change to Kafka:

```yaml
orders_cdc:
  input:
    mongodb:
      url: "mongodb://localhost:27017/?replicaSet=rs0"
      database: "shop"
      collection: "orders"
      consume: capture_all
      cursor_id: "orders-to-kafka"
  output:
    kafka: { url: "localhost:9092", topic: "shop.orders" }
```

- **The payload is the document as JSON.** Types that JSON does not have are written the
  MongoDB way: an ObjectId as `{"$oid": "…"}`, a date as `{"$date": …}`.
- **`mongodb.operation` says what happened**: `insert`, `update`, `replace` or `delete`. An
  update carries the whole document as it is after the change. A delete carries only the
  key, `{"_id": …}`. `mongodb.document_id` has the `_id` in every case.
- **`cursor_id` names the saved position**, so a restart continues where it stopped. By
  default the position is kept in the source database, in the collection
  `mqb_cursors_<collection>`; `checkpoint_store` puts it elsewhere, for a source you may not
  write to. See [Checkpoints](../cookbook/checkpoints.md).

The use case [Search index sync: MongoDB → Meilisearch](../use-cases/sync-mongodb-to-meilisearch.md)
follows a collection into a search index, deletes included, with a runnable example.

Copy a collection of a standalone `mongod` into a file, once:

```bash
mqb copy --drain \
  --from 'mongodb://localhost/?database=shop&collection=customers&consume=snapshot' \
  --to 'file:///data/customers.jsonl?format=payload'
```

`snapshot` delivers what exists when it runs. It cannot resume and refuses a `cursor_id`.

### A collection as a job queue

With `consume: delete` several instances share the work: each document goes to one of them
and is deleted when it was processed. The documents must have been written by an mq-bridge
output with the format `normal`, `json` or `text`, which is the shape the queue expects; do
not point this mode at a collection of your own data.

```yaml
enqueue_jobs:
  input:
    http: { url: "0.0.0.0:8080", path: "/jobs" }
  output:
    mongodb: { url: "mongodb://localhost:27017", database: "work", collection: "jobs", format: json }

work_jobs:
  input:
    mongodb: { url: "mongodb://localhost:27017", database: "work", collection: "jobs", consume: delete }
  output:
    http: { url: "http://worker.internal:9000/run" }
```

The queue is read in the order the documents were written. The publisher stores that
position in the field `insert_order`, apart from the `_id`, so a `message_id` from another
system does not change the order. It is a UUIDv7 that always rises within one process;
two writers are ordered by their clocks. A reader that needs an exact order uses
`capture_all` or `capture_new`.

Documents without `insert_order` are read first, in `_id` order: those written with
`format: payload`, by another program, or by an earlier version.

## Examples

**Load a CSV file into a collection, one-shot:**

```bash
mqb copy --drain \
  --from 'file:///data/customers.csv?format=csv' \
  --to 'mongodb://localhost/?database=app&collection=customers&format=payload'
```

**Copy a collection into PostgreSQL (replica set required):**

```bash
mqb copy --drain \
  --from 'mongodb://localhost/?database=app&collection=customers' \
  --to 'postgres://user:pass@localhost/app?table=customers&auto_create_table=true'
```

**Watch for changes only, continuous:**

```bash
mqb copy \
  --from 'mongodb://localhost/?database=app&collection=orders&consume=capture_new' \
  --to 'kafka://kafka.local:9092?topic=orders'
```

### Replacing a collection as one unit

`load_mode: replace` loads into a staging collection of its own,
`<collection>_mqb_load_<id>`, and replaces the collection with it in one `renameCollection`
when the source is drained. Readers see the
old documents until then, and all new ones after.

```bash
mqb copy 'file:///data/orders.jsonl?format=payload' \
  'mongodb://localhost:27017/?database=shop&collection=orders&format=payload&id_field=order_id' --load-mode replace
```

- **The collection keeps its indexes and options.** The staging collection is created with
  the collection's options (validator, collation, capped size), and its indexes are built on
  the loaded documents before the swap. An index the new documents violate, such as a
  unique one, fails the load.
- **A failed or interrupted run leaves the collection unchanged** and drops the staging
  collection; the copy exits non-zero. One document MongoDB rejects fails the whole load.
- **A crash leaves no garbage for long.** A running load holds a lease in the collection
  `mqb_load_leases` and renews it every 15 s; the next load into the same collection drops
  every staging collection whose lease is older than 60 s.
- **Two loads into the same collection do not mix.** Each writes its own staging
  collection; the one that finishes last replaces the collection.
- **An empty source leaves the collection unchanged.**
- **Drain routes only**, with the same rules as SQL's
  [`load_mode`](postgres.md#loading-as-one-unit): `exit_on_empty: true`, the sink is the
  route's output itself, and the source reads the full data set on every run.
- **Only `replace`.** There is no `atomic_append`, and `load_mode` cannot be combined with
  `request_reply`, `report_outcome`, `find` or `aggregate_query`.
- **No sharded collections, no views or time series:** `renameCollection` does not
  replace those. `readWrite` on the database is enough for the swap.
- **Change streams on the collection end at the swap.** MongoDB sends them `rename` and
  `invalidate`, not the documents that were replaced or loaded. A `capture_new` or
  `capture_all` reader with a `cursor_id` stops there with a permanent error, since a copy it
  feeds no longer matches; resync the collection and start it with a new `cursor_id`. Without
  a `cursor_id` the reader logs a warning and reads on from the next change.

## Key options

| Option | Purpose |
|---|---|
| `database` | **Required.** |
| `collection` | Collection name. |
| `format` | Publisher: how a message is stored: `normal` (default), `json`, `text`, `payload` (also `raw`). |
| `id_field` | Publisher-only: payload field that becomes `_id`. |
| `load_mode` | Publisher, drain routes: `replace` swaps in a staging collection when the drain completes. See [Replacing a collection as one unit](#replacing-a-collection-as-one-unit). |
| `consume` | `capture_all` (default — snapshot then change stream, replica set required), `capture_new` (changes only, replica set required), `snapshot` (one-shot read, no replica set), or `delete` (durable work queue — destructive; also `consumer`). |
| `cursor_id` | Names the saved read position of `capture_new`/`capture_all`. |
| `checkpoint_store` | Where to persist the resume cursor for `capture_new`/`capture_all`. |
| `username` / `password` | Take precedence over credentials embedded in `url`. |

Full field list: [reference/mongodb.md](../reference/mongodb.md).
