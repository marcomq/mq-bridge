# Meilisearch

Meilisearch is an output (a document sink for a search index) and an input (a scan of an index's
documents). From `mqb` 0.4.18 the built-in `meilisearch` endpoint is the generic
[`http_bulk`](./http-bulk.md) endpoint with the Meilisearch requests filled in. It writes, deletes,
waits for the indexing task and reads an index back.

Some features are only in the separate
[mq-bridge-meilisearch](https://github.com/marcomq/mq-bridge-meilisearch) plugin, which can
replace the built-in endpoint; see [Built in or plugin](#built-in-or-plugin). This page marks them
with "plugin only".

On this page:

- [Keep an index in sync with a Postgres table](#keep-an-index-in-sync-with-a-postgres-table)
- [Make a bulk load fast](#make-a-bulk-load-fast)
- [Documents with embeddings](#documents-with-embeddings) (pgvector, Supabase)
- [One document from several tables](#one-document-from-several-tables)
- [One-off copy or rebuild](#one-off-copy-or-rebuild), including a rebuild without downtime
- [Built in or plugin](#built-in-or-plugin)
- [Options](#options) and [Limitations](#limitations)

## Keep an index in sync with a Postgres table

This is the pipeline a CDC-to-search tool such as Sequin runs: read the rows that already exist,
then follow every insert, update and delete. Here it is one route.

```yaml
movies_to_search:
  batch_size: 10000                        # see "Make a bulk load fast" below
  input:
    postgres_cdc:
      url: "postgres://user:pass@localhost/app"
      publication: "movies_pub"
      slot_name: "mqb_meili"
      consume: capture_all                 # backfill first, then stream changes
      cursor_id: "movies_backfill"
      checkpoint_store: "file:///var/lib/mq-bridge/movies-phase.json"
  output:
    middlewares:
      - retry: { max_attempts: 5 }
    custom:
      name: meilisearch
      config:
        url: "http://localhost:7700"
        api_key: "${MEILI_MASTER_KEY}"
        index: "movies"
        primary_key: "id"
        operation: "${metadata:postgres.operation}"
```

Apply the index settings once before the first run; see
[Apply index settings before the first document](#1-apply-index-settings-before-the-first-document).

| A Sequin Meilisearch sink has | Here |
|---|---|
| Endpoint URL, API key | `url`, `api_key` |
| Index name | `index`; a template such as `${metadata:postgres.table}` is plugin only |
| Primary key (default `id`) | `primary_key`; no default, so set it |
| Index action (create or update a document) | Any change that is not a delete replaces the document; `method` with `update` or `update_existing` is plugin only |
| Delete action | `operation` mapped to `postgres.operation`; `delete_values` lists what counts as a delete |
| Function action (Meilisearch function-based edits) | Not supported. Compute the value in a [`transform`](../cookbook/transform.md) before the sink |
| Transform function | [`transform`](../cookbook/transform.md) middleware; the primary key must stay a top-level field |
| Routing function | A [`switch`](../cookbook/switch.md) output, or a templated `index` (plugin only) |
| Backfill | `consume: capture_all` on [`postgres_cdc`](../tutorials/postgres-cdc.md) |

> The left column is taken from Sequin's sink reference. Verify it against the current Sequin
> documentation before relying on a one-to-one migration.

What to know before running it:

- **Give each table its own publication and route.** Backfilled rows carry neither
  `postgres.operation` nor `postgres.table`. A missing operation is an upsert, which is right,
  but a templated `index` (plugin only) cannot resolve and the row is dead-lettered.
- **Delivery is at-least-once.** A row changed during the backfill is read twice. Both writes
  carry the same `primary_key`, so the later one wins.
- **`cursor_id` and `checkpoint_store` make the backfill resumable.** Write the store as
  `file:///…`; a plain path is read as a table name in the source database.
- **`capture_all` needs a single-column primary key.** For a composite key, build one id field
  with a `transform` expression and backfill through a
  [`sequence`](../engine/reference.md#sequence) input instead.
- **Writes are confirmed.** Meilisearch answers `202 Accepted` and indexes later, so the sink
  waits for the task before the replication slot advances.
- **Order is kept at any `concurrency`.** Two writes to the same document reach the index in
  source order.
- **A `truncate` is dead-lettered**, since it carries no row.
- **The slot is permanent.** It retains WAL on the server until the route consumes it. When you
  retire the route, drop it: `SELECT pg_drop_replication_slot('mqb_meili')`.

## Make a bulk load fast

Almost all the time of a bulk load is spent inside Meilisearch, not in the bridge: reading
200,000 rows from Postgres and sending them takes under 2 seconds, and indexing them takes the
rest. So a load gets faster by giving Meilisearch less work, in three ways. In order of effect:

### 1. Apply index settings before the first document

By default Meilisearch makes every field searchable. Its
[indexing guide](https://www.meilisearch.com/docs/capabilities/indexing/advanced/indexing_best_practices)
says to list only the fields that are searched in `searchableAttributes`, to keep numbers and
booleans in `filterableAttributes` or `sortableAttributes` only, and to do this before loading,
because changing any of these later reindexes every document. The same holds for `embedders`.

The built-in endpoint does not manage settings, so apply them once before the first run:

```bash
curl -X POST "$MEILI/indexes" -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"uid": "movies", "primaryKey": "id"}'
curl -X PATCH "$MEILI/indexes/movies/settings" -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"searchableAttributes": ["title", "overview"], "filterableAttributes": ["genre", "year"]}'
```

Plugin only: `settings` on the endpoint holds the same object and is applied when the index is
created, so a route in a config file needs no `curl`.

### 2. Use a large batch

Meilisearch indexes in batches, and each indexing batch has a fixed cost that grows with the
index: around 2 seconds per batch once an index holds 100,000 documents, whether the batch
carries 1,000 documents or 30,000. The sink waits for each batch's task before it sends the next
one, which is what keeps writes in source order and makes an acknowledgement mean "searchable".
So the number of documents per batch, the route's `batch_size`, sets the throughput.

Copying 200,000 rows (six short columns) from Postgres 18 into Meilisearch 1.53 on one laptop:

| `batch_size` | Time until searchable |
|---|---|
| 1,024 (the `mqb copy` default) | 162 s |
| 10,000 | 34 s |
| 30,000 | 17 s |
| 50,000 | 13 s |

For comparison, Filament 0.13.1 (which sends 10,000 documents per request and waits once at the
end of the run) took 13 s on the same table. Meilisearch's own guide recommends 50,000 to
100,000 documents per request for a large dataset, and fewer when documents exceed 10 KB.

The cost of a larger batch is memory, because a whole batch is held until Meilisearch confirms
it. Rows with a 1,024-dimension vector need about 46 KB each in flight, so a 10,000-row batch of
them takes about 500 MB. Size the batch for your rows:

- **Short text rows:** 30,000 to 50,000.
- **Rows with embeddings or long text:** 5,000 to 10,000.

From `mqb` 0.4.18, `mqb copy` into Meilisearch uses 50,000 when no `--batch-size` is given, so short rows need no flag. Pass a smaller `--batch-size` for rows with
embeddings or long text. A route in a config file keeps its own `batch_size`.

A request larger than `max_request_bytes` is split automatically, so a large batch does not fail
with `payload_too_large`; only a single document over the limit does. The built-in endpoint waits
for each part's task before it sends the next. The plugin queues all parts first, so Meilisearch
indexes them together.

A live CDC stream sends whatever has changed, usually far less than a batch, so `batch_size`
matters there for the backfill only.

### 3. Send embeddings as vectors, not as a field

An embedding left in an ordinary field is indexed as a thousand separate numbers per document.
That is slow and cannot be searched by similarity. Put it under `_vectors` instead, as the next
section shows.

### What does not help

- **`concurrency`.** The sink sends one batch at a time to keep source order, at any setting.
- **`compression: gzip`.** It shrinks the request, which helps on a slow link to a remote
  instance. Indexing time is unchanged.
- **`wait_for_task: false`** (plugin only). It returns sooner but acknowledges documents that may
  still fail to index, and the source position advances past them.
- **Server tuning** (`--max-indexing-memory`, `--max-indexing-threads`) helps any client the
  same way; see Meilisearch's guide.

## Documents with embeddings

Meilisearch takes ready-made embeddings under the reserved `_vectors` field, for an embedder
declared as `userProvided` in the index settings. A pgvector column arrives from Postgres as the
text `[0.1,0.2,…]`, in the backfill and in the change stream alike, so the route has two things to
do: move the column to `_vectors.<embedder>` and decode the text into an array. One `transform`
does both.

```yaml
articles_to_search:
  batch_size: 5000                         # embeddings are large; see the sizes above
  input:
    postgres_cdc:
      url: "postgres://user:pass@localhost/app"
      publication: "articles_pub"
      slot_name: "mqb_articles"
      consume: capture_all
      cursor_id: "articles_backfill"
      checkpoint_store: "file:///var/lib/mq-bridge/articles-phase.json"
  output:
    middlewares:
      - transform:
          mapping:                         # only the fields listed here reach the index
            id: "$.id"
            title: "$.title"
            body: "$.body"
            category: "$.category"
            "_vectors.default": "$.embedding"
          schema:
            type: object
            properties:
              _vectors:
                type: object
                properties:
                  default:
                    type: ["string", "null"]            # null: see below
                    contentMediaType: application/json  # decode "[0.1,…]" into an array
      - retry: { max_attempts: 5 }
      - dlq:
          endpoint: { file: { path: "articles-rejected.jsonl" } }
    custom:
      name: meilisearch
      config:
        url: "http://localhost:7700"
        api_key: "${MEILI_MASTER_KEY}"
        index: "articles"
        primary_key: "id"
        operation: "${metadata:postgres.operation}"
```

Declare the embedder before the first run (with the plugin, the same object goes under
`settings`):

```bash
curl -X PATCH "$MEILI/indexes/articles/settings" -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"searchableAttributes": ["title", "body"], "filterableAttributes": ["category"],
       "embedders": {"default": {"source": "userProvided", "dimensions": 1024}}}'
```

- **`dimensions` must equal the column's size** (`vector(1024)` here), and the embedder's name,
  `default`, must be the key under `_vectors`.
- **Keep `"null"` in the type.** A delete carries only the key, with every other column null, and
  a row may have no embedding yet. Without `"null"` the transform rejects both, so the delete
  never reaches the index. A null under `_vectors` tells Meilisearch the document has no
  embedding.
- **Keep the `dlq`.** Without one, a message the transform rejects is logged and dropped. See
  [Dead-letter queues](../cookbook/dlq.md).
- **Leave the embedding out of `searchableAttributes`.** Listing the searched fields, as above,
  already does that.

Check the result: `numberOfEmbeddings` should equal `numberOfDocuments`, and a vector search
should return hits.

```bash
curl "$MEILI/indexes/articles/stats" -H "Authorization: Bearer $KEY"
curl "$MEILI/indexes/articles/search" -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"vector": [0.1, 0.2, …], "hybrid": {"embedder": "default", "semanticRatio": 1}, "limit": 3}'
```

This recipe was run with the plugin and its `settings` against Postgres 18 with pgvector 0.8 and
Meilisearch 1.53: backfill, then live inserts, updates, deletes and rows with a null embedding.

### On Supabase

Supabase is Postgres with pgvector, so the route above applies unchanged. Three things differ
from a local Postgres and otherwise look like bugs:

- **Use the direct connection** (`db.<ref>.supabase.co:5432`), not the pooler. A pooled
  connection cannot start replication.
- **The direct connection is IPv6-only** unless the project has the IPv4 add-on. Without an IPv6
  route the connection times out; check with `ping6` first.
- **Create your own publication** (`CREATE PUBLICATION articles_pub FOR TABLE articles`). Do not
  stream from `supabase_realtime`. `wal_level` is already `logical`.

These three points come from Supabase's documentation and were not run against a hosted project.

## One document from several tables

Meilisearch has no joins, so a search document that shows a book with its author's name has to be
assembled before it is indexed. Two tools do that:

- **A foreign key is joined with [`lookup`](../cookbook/lookup.md).** The middleware reads the
  referenced row and writes it into the document, one query per batch:

  ```yaml
  output:
    middlewares:
      - lookup:
          from:
            sqlx:
              url: "postgres://user:pass@localhost/app"
              table: "authors"
              lookup_query: "SELECT id, name FROM authors WHERE id IN (${payload:author_id}::int)"
          into: author
    custom:
      name: meilisearch
      config: { url: "http://localhost:7700", index: "books", primary_key: "id" }
  ```

- **A table sharing the document's key is merged with `method: update`** (plugin only), from a second route
  writing the same index. Use `method: update_existing` on that route so a late change cannot
  recreate a deleted document as a stub.

The [plugin README](https://github.com/marcomq/mq-bridge-meilisearch#merging-rows-from-different-tables)
has a complete example with both, and the rules for deletes across routes.

### Keeping joined fields current

A joined field is copied at the time the book is written. Renaming the author afterwards leaves
the old name in every one of their books, because no book row changed. To close this gap, make
Postgres report those books as changed. An update that sets a column to its own value is enough:
it is written to the change stream like any other, and the route looks the author up again.

```sql
CREATE FUNCTION touch_books_of_author() RETURNS trigger AS $$
BEGIN
  UPDATE books SET author_id = author_id WHERE author_id = NEW.id;
  RETURN NEW;
END $$ LANGUAGE plpgsql;

CREATE TRIGGER authors_reindex_books
  AFTER UPDATE OF name ON authors
  FOR EACH ROW EXECUTE FUNCTION touch_books_of_author();
```

Plugin only, from 0.1.3: the sink can do this without touching Postgres: a second route on `authors`
with `update_where: "author.id = ${payload:id}"` and `update_into: author` writes each changed
author into every book that embeds it. It needs `author.id` in `filterableAttributes` and
Meilisearch 1.14 or newer; the
[plugin README](https://github.com/marcomq/mq-bridge-meilisearch#merging-rows-from-different-tables)
has the route and its limits. It was not run for this page.

The trigger costs one book write per affected book, in the same transaction as the author
change. That a same-value update travels through the route into the index was checked on the
setup above; the trigger itself is standard Postgres and was not benchmarked.

## Options

| Field | Applies to | Default | Meaning |
|---|---|---|---|
| `url` | both | required | Base URL. `meilisearch://` and `meilisearchs://` become `http(s)://`. |
| `api_key` | both | none | Sent as `Authorization: Bearer`. |
| `index` | both | route name | Index UID. |
| `primary_key` | both | none | The field Meilisearch keys documents by; a delete takes its id from it. |
| `operation` | output | none | Each message's change operation. |
| `delete_values` | output | `["delete", "d"]` | Operation values that remove the document. |
| `max_request_bytes` | output | `90000000` | Split a batch rather than exceed this body size. |
| `compression` | output | `none` | `gzip` request bodies. `zstd` and `lz4` are accepted here, but Meilisearch 1.53 answers them with a "payload is malformed" 400. |
| `connect_timeout_ms` | both | `10000` | How long to wait for a connection. |
| `request_timeout_ms` | both | none | Upper bound for one HTTP request. |
| `fields` | input | all | Comma-separated document fields to read. |
| `cursor_id`, `checkpoint_store` | input | none | Persist the read position (a `file://` store). |
| `polling_interval_ms` | input | `1000` | Wait after an empty page. |
| `max_polling_interval_ms` | input | none | The wait doubles after each empty page up to this. |

Left unset, `primary_key` is inferred by Meilisearch from the first batch, possibly wrongly and
permanently.

The wait for a task has no upper bound. A slow task logs a warning every minute and is never
sent again, and a failed status request is repeated. The batch fails only when Meilisearch
reports the task as failed or canceled, so a route stalls with warnings while Meilisearch is
down.

Reading pages through the index by offset, which is only stable while nothing is deleted during
the read.

## Built in or plugin

The built-in endpoint refuses the plugin's options at startup and names the plugin, instead of
ignoring them. These are plugin only:

| Option | What it does |
|---|---|
| `settings` | Applies index settings when the index is created. |
| `method` | `update` merges top-level fields, `update_existing` merges only into an existing document. The built-in endpoint always replaces. |
| A template in `index` | Routes each message to the index its metadata names. |
| `update_where`, `update_into` | Writes a message into every document it matches. |
| `swap_into` | Swaps a rebuilt index into the live one after a complete copy. Not released yet. |
| `create_index` | Creates the index at startup. The built-in endpoint lets the first write create it. |
| `wait_for_task`, `task_timeout_ms` | Acknowledge on enqueue, and when a slow task is reported. |

To use the plugin in `mqb`, install it and prefer it over the built-in endpoint:

```bash
brew install marcomq/tap/mq-bridge-meilisearch     # or: conda install -c marcomq mq-bridge-meilisearch
MQB_PLUGIN_OVERRIDE=meilisearch mqb copy … 'meilisearch://localhost:7700?index=movies'
```

`--plugin` does not work for this, see [Native plugins](../extending/plugins.md). A build of
`mqb` with the cargo feature `meilisearch` has the plugin compiled in instead of the built-in
endpoint. `mqb` up to 0.4.17 had plugin 0.1.2 compiled in.

## One-off copy or rebuild

```bash
mqb copy --drain --batch-size 30000 \
  'postgres://user:pass@localhost/app?table=movies&cursor_column=id' \
  'meilisearch://localhost:7700?index=movies&primary_key=id&api_key=KEY'
```

Pick `--batch-size` as described in [Use a large batch](#2-use-a-large-batch), and apply the index
settings first as shown in
[Apply index settings before the first document](#1-apply-index-settings-before-the-first-document).
Without `operation` every message is an upsert, which is all a copy needs. A copy sends rows as
they are, so a table with embeddings needs the route from
[Documents with embeddings](#documents-with-embeddings) instead.

### Rebuild without downtime

After changing a `transform` or the index settings, build the new version under another name and
swap it in. Searches keep hitting the old index until the swap, which is atomic.

1. Stop the streaming route. Its replication slot keeps the changes that arrive meanwhile.
2. Create `movies_v2` with its settings, using the two `curl` calls from
   [above](#1-apply-index-settings-before-the-first-document).
3. Load it with the copy command above and `index=movies_v2`.
4. Swap, then delete the leftover, which now holds the old documents:

   ```bash
   curl -X POST "$MEILI/swap-indexes" -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
     -d '[{"indexes": ["movies", "movies_v2"]}]'
   curl -X DELETE "$MEILI/indexes/movies_v2" -H "Authorization: Bearer $KEY"
   ```

5. Start the streaming route again. It replays the changes from step 1 into the rebuilt index.

Plugin only, in a version not released yet: the sink does step 4 itself. Add
`swap_into=movies` to the copy in step 3. After a complete copy it swaps `movies_v2` into
`movies` and deletes the leftover. It leaves `movies` untouched, with a warning in the log, when
the copy was stopped or failed, when any document was refused, or when nothing was written. A
swap that fails is logged as an error but does not change the exit code of `mqb copy`, so check
the log for "is now live". `swap_into` cannot be combined with `wait_for_task: false` or
`update_where`. It was not run for this page.

The [plugin README](https://github.com/marcomq/mq-bridge-meilisearch#readme) also covers
composite primary keys and reading an index back out.

## Outside `mqb`

From Rust, the built-in endpoint is part of the `http-bulk` feature: call
`mq_bridge::endpoints::http_bulk::register_preset("meilisearch")?` before any route starts and
address it as a `custom` endpoint named `meilisearch`.

The plugin is a separate package for the library and the bindings:

```bash
pip install mq-bridge mq-bridge-meilisearch    # then: mq_bridge_meilisearch.register()
npm install mq-bridge mq-bridge-meilisearch    # then: import { register } from "mq-bridge-meilisearch"
```

From Rust, add the `mq-bridge-meilisearch` crate and call `mq_bridge_meilisearch::register()?`.
The plugin needs mq-bridge 0.4.13 or newer. Register one of the two, not both: the name
`meilisearch` can be taken once.

## Limitations

- The built-in endpoint leaves out what [Built in or plugin](#built-in-or-plugin) lists.
- `update` (plugin only) merges top-level fields only; a list replaces the whole field.
- A field joined with `lookup` goes stale when the referenced row changes later, unless a
  trigger reports the change; see [Keeping joined fields current](#keeping-joined-fields-current).
- With plugin 0.1.2, built into `mqb` 0.4.17, a task that outlives `task_timeout_ms` is sent
  again. Set it well above the slowest task in `GET /tasks`, or upgrade.
- There is no replay log: recovering from a bad transform means reading the source again.
