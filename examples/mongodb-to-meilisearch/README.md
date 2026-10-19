# MongoDB change stream → Meilisearch

Keeps a Meilisearch index in sync with a MongoDB collection. Existing documents are copied once,
then every insert, update and delete follows through the change stream.

Explained in the book: [Keep a search index in sync with MongoDB](https://marcomq.github.io/mq-bridge/use-cases/sync-mongodb-to-meilisearch.html).

```mermaid
flowchart LR
  M[(MongoDB<br/>shop.products)] -- change stream --> B[mq-bridge]
  B -- "upsert or delete document" --> S[(Meilisearch<br/>products index)]
```

## Run it

Needs Docker, `curl` and `jq`.

```bash
docker compose up -d
./search.sh "hedphones"
```

Then change the collection and search again:

```bash
docker compose exec mongo mongosh shop --eval 'db.products.updateOne({name: "Wireless headphones"}, {$set: {price: 69.0}})'
docker compose exec mongo mongosh shop --eval 'db.products.deleteOne({name: "USB-C charger"})'
./search.sh "charger"
```

`./test.sh` runs the whole sequence (copy, insert, update, delete, restart, search) and is what
CI executes. `docker compose down -v` removes everything.

## Files

| File | Purpose |
| :--- | :--- |
| [`routes.yaml`](routes.yaml) | The route: `mongodb` input with `capture_all`, a `transform` to the document shape, `meilisearch` output |
| [`docker-compose.yml`](docker-compose.yml) | MongoDB 8.2 (single-member replica set), Meilisearch 1.53, mq-bridge-app 1.0.1 |
| [`init.js`](init.js) | Initiates the replica set and inserts three products |
| [`search.sh`](search.sh) | Asks Meilisearch for the three best matches |

Meilisearch runs without a master key here. In production set `MEILI_MASTER_KEY` and add
`api_key` to the `meilisearch` output.
