<!-- description: Enrich each event with a database record (lookup) and keep per-key history in Postgres (aggregate with a store): order count, spend and a moving average per customer, then route outliers to review. Survives restarts, shared by several instances. Runnable docker-compose example. -->

# How to enrich events and keep per-key history (lookup + aggregate in Postgres)

One mq-bridge route takes orders, adds the customer record from Postgres, and adds what is known
about that customer so far: number of orders, total spend, the usual order amount and the spend of
the last week. The history is kept in a Postgres table, so a restart continues where it stopped
and several instances work on the same numbers. An order far above the customer's usual amount
goes to a review table.

Runnable example: [`examples/order-features`](https://github.com/marcomq/mq-bridge/tree/main/examples/order-features).

## The problem: every event needs context it does not carry

An order event says who ordered and how much. Whatever acts on it, a fraud score, a loyalty rule,
a dashboard, needs more: the customer's tier from the CRM, and how this order compares to the
customer's earlier ones. The usual ways to get there each have a cost:

- **Querying the history per event** (`SELECT count(*), avg(amount) FROM orders WHERE customer_id
  = …`) gets slower as the table grows and runs once for every event.
- **A counter in the application's memory** is lost on restart and splits into separate counts
  as soon as a second instance runs.
- **A stream processor** (Flink, Kafka Streams) keeps state properly, but is a cluster and a
  programming model of its own for what may be four numbers per customer.

## The solution: a lookup for the record, an aggregate for the history

```mermaid
flowchart LR
  A[Shop] -- "POST /orders" --> B[mq-bridge route]
  B -- "1. customer by id<br/>(one query per batch)" --> C[(customers)]
  B <-- "2. history per customer<br/>(load, fold, write back)" --> S[(customer_stats)]
  B -- "usual order" --> E[(orders_enriched)]
  B -- "far above usual" --> R[(orders_review)]
```

```yaml
orders_features:
  input:
    http: { url: "0.0.0.0:8080", path: "/orders", method: "POST" }
  output:
    # Output middlewares run bottom-up: lookup, then aggregate, then retry around the write.
    middlewares:
      - retry: { max_attempts: 5 }
      # Per-customer history, kept in Postgres: it survives restarts and is shared by instances.
      - aggregate:
          store: "postgres://app:app@postgres:5432/app/customer_stats"
          key: "${payload:customer_id}"
          into: history
          emit: previous          # the history before this order; null for a first order
          time: created_at
          fields:
            orders: count
            spend: sum(amount)
            typical_amount: ema(amount, 30d)
            spend_7d: decayed_sum(amount, 7d)
      # One query per batch; an unknown customer gets null.
      - lookup:
          from:
            sqlx:
              url: "postgres://app:app@postgres:5432/app"
              table: customers
              lookup_query: "SELECT id, name, tier FROM customers WHERE id IN (${payload:customer_id}::bigint)"
          into: customer
    switch:
      when:
        - if: "history != null and amount > 3 * history.typical_amount"
          to:
            sqlx: { url: "postgres://app:app@postgres:5432/app", table: orders_review, columns: auto, key: id }
      default:
        sqlx: { url: "postgres://app:app@postgres:5432/app", table: orders_enriched, columns: auto, key: id }
```

An order `{"id": 4, "customer_id": 1, "amount": 900, "created_at": "2026-10-03T09:00:00Z"}`
from a customer with two earlier orders, 40 on October 1 and 60 on October 2, leaves as
(numbers rounded here):

```json
{"id": 4, "customer_id": 1, "amount": 900, "created_at": "2026-10-03T09:00:00Z",
 "customer": {"id": 1, "name": "Ada", "tier": "gold"},
 "history": {"orders": 2, "spend": 100.0, "spend_7d": 87.29, "typical_amount": 50.11}}
```

and goes to `orders_review`, since 900 is more than three times 50.11. `spend_7d` is below 100
because both earlier orders have faded towards the time of this one.

How it fits together:

- The [`lookup`](../cookbook/lookup.md) puts the customer row under `customer`. With the key
  inside `IN (…)` one query answers the whole batch. A customer that does not exist gives
  `null`; the order is still written.
- The [`aggregate`](../cookbook/aggregate.md) keeps one state per `customer_id`. `fields` are
  built-in aggregates that need no expression: `count`, `sum`, a moving average `ema` that
  halves the weight of an order every 30 days, and `decayed_sum`, a recent total that fades
  instead of dropping out of a window.
- `emit: previous` writes the state from *before* the order, so the order is compared with the
  customer's past and not with itself. Leave it out to include the order in its own numbers.
- `time: created_at` makes the averages follow the order time instead of the clock, so
  replaying old orders gives the same numbers. An order that arrives late counts with the
  weight it would have had at its time.
- `store` keeps the states in `customer_stats`, one row per customer, created on start. With the
  default `consistency: shared`, every batch loads its customers' states, folds and writes them
  back, guarded by a version, so instances running at the same time do not overwrite each
  other. With one instance only, `consistency: single_writer` keeps the states in memory as
  well and is several times faster; see the
  [aggregate reference](../engine/reference.md#aggregate).
- The [`switch`](../cookbook/switch.md) reads the enriched payload. For a first order `history` is
  `null`, which the condition checks first; such an order logs one warning that the field is
  absent, and goes to `default`.
- Both outputs write by [column name](../connectors/postgres.md#writing-by-column-name);
  `customer` and `history` land in `jsonb` columns. `key: id` makes a repeated order overwrite
  its row.

## Runnable example

```bash
git clone https://github.com/marcomq/mq-bridge && cd mq-bridge/examples/order-features
docker compose up -d
curl -sS http://localhost:8080/orders -H 'Content-Type: application/json' \
  -d '{"id": 1, "customer_id": 1, "amount": 40, "created_at": "2026-10-01T10:00:00Z"}'
docker compose exec postgres psql -U app -d app -c 'SELECT id, amount, customer, history FROM orders_enriched'
```

The stack is Postgres 16 and mq-bridge-app 1.0.1. `./test.sh` posts five orders with a restart
of the bridge in between and checks that the history continues, that the outlier is in
`orders_review`, and that an unknown customer is stored without customer data; it runs in CI.

## Failure semantics

| Event | What happens |
| :--- | :--- |
| **mq-bridge restarts** | The states are in `customer_stats`; the next order of a customer continues from there. Nothing is kept only in memory. |
| **Several instances** | With `consistency: shared` they can run side by side. A write that lost the race to another instance is refused, and the batch is folded again from the reloaded states. |
| **Duplicates** | Delivery is at-least-once, and an order delivered twice is counted twice: the state write and the acknowledgement are two steps. The row in `orders_enriched` is overwritten by its `id`, the history is not. To count each order once, let a [`deduplication`](../cookbook/deduplication.md) keyed on the order id see it first, on the input. |
| **Postgres unavailable** | The lookup or the state write fails as retryable and the caller gets an error. The `lookup` and `aggregate` run before the `retry`, so a failed lookup counts nothing and the caller can send again; `retry` repeats only the table write. |
| **A failed table write** | When the write still fails after `retry`, the order has already been counted; sent again, it is counted twice. |
| **Order of events** | Within a batch orders are folded in order. With route `concurrency` above 1 batches can overtake each other; with `time` set, the averages and decayed sums do not depend on that, `count` and `sum` never did. |

Limits of this recipe:

- **No windows.** "Spend in the last 7 days" is a decayed sum, not an exact window. For exact
  calendar windows put the window into the key (`${payload:customer_id}:${payload:day}`); old
  rows then stay until you delete them.
- **States are never removed.** A customer who never orders again keeps their row.
- **Changing `fields`** changes the layout of the stored states: start with a new table or a new
  `into`.
- **The customer record is read at the time of the order.** A later change in `customers` does
  not update orders already written.
- **The table name.** In `mq-bridge-app` a route runs under an internal name, so a store without
  a table in its URL gets a table named after that, not after the route. Name the table in the
  URL, as above.

## When not to use mq-bridge for this

- **The history is a query you already have.** If the orders are in Postgres and a view or a
  materialized view answers fast enough, there is nothing to maintain.
- **You need exactly-once counts or exact windows.** A stream processor commits state and input
  position together; here a redelivered event counts twice.
- **The state is more than numbers per key**: joins between two streams over time, sessions,
  pattern detection. That is what Flink, Kafka Streams or RisingWave are built for.

## Alternatives

| | mq-bridge | A stream processor (Flink, Kafka Streams) | A streaming database (Materialize, RisingWave) |
| :--- | :--- | :--- | :--- |
| Lookup of a record by key | `lookup` against Postgres, MySQL, SQLite, MongoDB, ClickHouse or HTTP | a lookup join or your own code | a join with a table |
| Per-key state | `aggregate`: built-in sums, counts, moving averages and decayed sums, or an expression | any state, in code | any SQL aggregate |
| Where state lives | Postgres, SQLite or MongoDB, or memory | the processor's state backend | the database |
| Delivery | at-least-once; a replay counts twice | exactly-once available | exactly-once |
| Windows | none; decayed sums or a window in the key | tumbling, sliding, session | SQL windows |
| Runs as | library in a Rust, Python or Node.js process, or one binary | a cluster | a database server |

mq-bridge fits when the per-key numbers are a step of a data flow that runs anyway, and the
state should sit in a database you already operate. The other two fit when the state is the
main work.
