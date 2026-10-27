# Running aggregates

The [`aggregate`](../engine/reference.md#aggregate) middleware keeps a running value per key
and updates it with every message, such as a counter or a sum. The message goes on with
the value written into its payload. Use it for values that depend on earlier messages, such
as "how many orders did this customer place so far" or "what is this sensor's usual reading".

By default the values live in a database you name with `store`. Every batch loads the values
of its keys, computes, and writes them back before its messages go on. Nothing is kept in the
process between batches, so a restart, or a second instance of the route, continues from the
stored values. A route that runs as one instance can keep them in memory instead
([`single_writer`](#faster-keep-the-states-in-memory)).

## Where it fits

mq-bridge moves messages, and a few middlewares keep something about them between
messages: [`deduplication`](deduplication.md) the keys it has seen, `aggregate` a value
computed from them. It is meant for a value the pipeline needs while the message is on its
way: to filter, route or alert on it, to hand it to the receiver, or to thin out what reaches
an expensive sink. It is kept small on purpose: one state per key, updated message by
message, in the order the messages arrive.

| You need | Use |
|---|---|
| a running value per key that a `filter`, a `switch` or the receiver decides on | `aggregate` with a `store` |
| the same, faster, for a route that runs as one instance | `aggregate` with `consistency: single_writer` |
| a counter that other applications update too | a writing [`lookup`](lookup.md) |
| totals for reports only, in a database that aggregates on its own | write the raw rows and let the sink aggregate |
| time windows, event time and late data, joins between streams, exactly-once state | a stream processor, with mq-bridge in front of it or behind it |

Letting the database compute the counter, with a writing `lookup`, costs a read and a write
per message and key. `aggregate` computes in the process and asks the store twice per batch,
however large the batch is.

Windows, event time and joins are out of scope. They need the state and the input position
committed together, which a bridge between arbitrary endpoints cannot promise; see
[How this compares to stream processors](#how-this-compares-to-stream-processors).

## A counter and a sum per key

```yaml
sensor_stats:
  batch_size: 4096
  input:
    kafka: { topic: "readings", url: "localhost:9092" }
    middlewares:
      - aggregate:
          store: "postgres://localhost/telemetry/sensor_states"
          key: "${payload:sensor_id}"
          into: sensor
          fields: { total: sum(reading), count: count }
  output:
    kafka: { topic: "readings.enriched", url: "localhost:9092" }
```

`{"sensor_id": 7, "reading": 20}` leaves as
`{"sensor_id": 7, "reading": 20, "sensor": {"count": 1, "total": 20.0}}`. The next message
for sensor 7 continues from that state, in this process or after a restart.

A CSV sink writes that nested result as the columns `sensor.count` and `sensor.total`, not as
JSON text in one cell; see [CSV](../connectors/file.md#csv).

## Where the states live

PostgreSQL, SQLite (`sqlite:///var/lib/mqb/agg.db?mode=rwc`) and MongoDB
(`mongodb://localhost/telemetry`) work as a `store`. The table or collection is created on
start. **Name it in the URL**, as above: without a name it is `mqb_aggregate_<route>`, after
the name the route runs under. mq-bridge-app runs routes under an internal name
(`ui_collector_route_<hex>`), so there the default is not the name in your YAML, and renaming
a route would start from empty states.

Per batch, under the default `consistency: shared`:

1. The distinct keys of the batch are loaded with one query.
2. The batch is computed in memory, message by message.
3. The changed states are written back with one statement.
4. Only then do the messages go on, and only then is the batch acked.

That is why a restart changes nothing: a batch is acked only after its states are stored,
so the route continues exactly where it stopped, and after weeks of stops and starts it
holds what a route that never stopped would hold. Several instances can run against the same
store; a state that another instance changed in between is computed again. Two caveats:

- **Decaying values need `time`.** With `time` they fade by the times in the messages.
  Without it the clock of the process is used, and a replay computes different values. See
  [A moving average by time](#a-moving-average-by-time).
- **A replayed message counts twice.** Storing the states and acking the batch are two
  steps. A crash between them, or a nack after the states were stored, delivers the batch
  again. See [What happens on a crash or a retry](#what-happens-on-a-crash-or-a-retry).

The cost is one load and one store per batch (each split into requests of 1000 keys), so use
a large `batch_size` (4096 to 8192). If the
route runs as one instance, [`single_writer`](#faster-keep-the-states-in-memory) is several
times faster.

## Logic of your own: `expression`

Where the built-in aggregates do not fit, `expression` returns the new state:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    key: "${payload:sensor_id}"
    into: sensor
    expression: "{ total: (state.total ?? 0) + reading, count: (state.count ?? 0) + 1 }"
```

It reads payload fields by name, metadata as `meta.<key>`, and `state`, the value it returned
for the previous message with the same key. For a new key `state` is `null`, so
`state.total ?? 0` gives the starting value. The language is the one
[`transform`](transform.md) and `filter` use.

The usual form `avg * 0.99 + reading * 0.01` starts at the first reading and needs hundreds
of messages to forget it. Keep the weighted sum and the weight, and divide when writing:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    key: "${payload:sensor_id}"
    into: sensor.avg_reading
    expression: "{ s: (state.s ?? 0) * 0.99 + reading, w: (state.w ?? 0) * 0.99 + 1 }"
    output: "state.s / state.w"
```

`output` shapes what the message carries; the stored state stays `{s, w}`. After a first
reading of 500 followed by readings of 50, this reads 52.6 after 100 messages. The usual form
still reads 216.

## Compare a message with what came before

With `emit: previous` the message carries the state from before its own update. That is what
a rule such as "this reading is five times the sensor's average" needs:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    key: "${payload:sensor_id}"
    into: sensor.avg_before
    emit: previous
    expression: "{ s: (state.s ?? 0) * 0.99 + reading, w: (state.w ?? 0) * 0.99 + 1 }"
    output: "state.s / state.w"
```

For the first message of a sensor `sensor.avg_before` is `null`.

A fixed factor is a rough rule. With the built-in aggregates (see below) the message can
carry the average and the standard deviation from before its own update, and a `filter` or
`switch` behind it can ask how many deviations the reading is away:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    key: "${payload:sensor_id}"
    into: before
    emit: previous
    fields:
      avg: ema(reading, 0.01)
      sd: ema_stddev(reading, 0.01)
```

`stddev` and `variance` cover all readings of the key; the `ema_` forms weight them like
`ema`, so old readings fade and the result follows a sensor that drifts. Both are sample
values (divided by n − 1) and `null` for the first message of a key, which has no spread yet.

## Several keys from one message

`entries` updates one state per key, each with its own expression:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    entries:
      - key: "${payload:sensor_id}"
        into: stats.sensor
        expression: "{ sum: (state.sum ?? 0) + reading, n: (state.n ?? 0) + 1 }"
      - key: "${payload:site_id}"
        into: stats.site
        expression: "{ max: max([state.max ?? reading, reading]) }"
```

## Built-in aggregates

For counters, sums and averages you do not need an expression. `fields` names the results and
what each one computes:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    entries:
      - key: "${payload:sensor_id}"
        into: sensor
        fields:
          n: count
          total: sum(reading)
          avg: ema(reading, 0.01)
      - key: "${payload:site_id}"
        into: site
        fields: { high: max(reading), usual: mean(reading) }
```

Available are `count`, `sum`, `min`, `max`, `last`, `mean`, `stddev`, `variance`, and
`ema(path, alpha)`, `ema_stddev(path, alpha)`, `ema_variance(path, alpha)`, and the
time-based `decayed_count(half_life)` and `decayed_sum(path, half_life)`. `ema` is the
moving average from above: it has no start bias, so the first message reads its own value. The value may be a number or a numeric string, as a CSV source delivers it. `fields`
is several times faster than an expression; use an expression where you need logic that the
built-ins do not cover.

### A moving average by time

`ema(reading, 0.01)` counts messages: a sensor that reports every second forgets a value a
thousand times faster than one that reports every quarter of an hour. Give a half-life
instead of `alpha` and the weight follows the time between two messages of a key:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    key: "${payload:sensor_id}"
    into: sensor
    time: measured_at
    fields:
      avg: ema(reading, 5m)
      spread: ema_stddev(reading, 1h)
```

A reading counts half after five minutes, a quarter after ten. `time` names the payload
field with the message's time: epoch seconds or milliseconds, or an RFC 3339 text. Without
`time` the clock of the process is used, and a replay then computes different values.

A message that is older than the newest one its state has seen **counts with the weight it
would have had by then**: half per half-life it is late. The averages and decayed values
therefore come out the same in any arrival order. `last` keeps the newest value. The message
carries the metadata `mqb.aggregate.late`, which names the entries concerned. Filter or route
on it where late data must not count at all:

```yaml
- filter: 'meta["mqb.aggregate.late"] == null'
```

A wrong `time` far in the future makes every later message late. Send one message with the
metadata `mqb.aggregate.overwrite: true` to set the state's clock back to its time.

### Rising or falling activity

`decayed_count(half_life)` counts messages that fade: one counts half after a half-life. At
a steady rate it settles at about the messages of the last 1.44 half-lives, without a
window. `decayed_sum(path, half_life)` does the same for a value. A short and a long
half-life side by side compare the recent activity of a key with its own usual level:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    key: "${payload:customer_id}"
    into: orders
    time: created_at
    fields:
      last_hour: decayed_count(1h)
      last_week: decayed_count(7d)
      amount_hour: decayed_sum(amount, 1h)
      amount_week: decayed_sum(amount, 7d)
```

`last_hour` × 168 well above `last_week` means far more orders than this customer usually
places.

## Messages with missing fields

A message that lacks a field an entry reads, or a key, cannot be folded. `on_error` says
what happens then:

| `on_error` | Effect |
|---|---|
| `drop` (default) | On an input the message is logged, acked and dropped; an HTTP caller still gets `202`. On an output it fails. |
| `fail` | On an input the message is nacked: an HTTP caller gets `500`, a broker delivers it again. |
| `skip` | Only the entries that cannot be computed are left out. The message goes on and carries `mqb.aggregate.skipped` with their `into` paths. |

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    on_error: skip
    entries:
      - { key: "${payload:sensor_id}", into: sensor, fields: { avg: "ema(reading, 0.01)" } }
      - { key: "${payload:site_id}", into: site, fields: { high: max(reading) } }
```

A message without `site_id` leaves with `sensor` filled, without `site`, and with the
metadata `mqb.aggregate.skipped: site`.

## Try a configuration without changing the states

`read_only: true` reads the states from the `store`, computes and writes the result into the
message, and stores nothing. Use it for a dry run against production states, or for a second
route that only reads what another one maintains. With `emit: previous` the message carries
the stored state as it is; with `emit: updated` what it would become.

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    read_only: true
    key: "${payload:sensor_id}"
    into: sensor
    fields: { n: count, avg: "ema(reading, 0.01)" }
```

## Faster: keep the states in memory

If exactly one instance runs the route, `consistency: single_writer` keeps the states in
memory and writes the changed ones to the store behind the batches:

```yaml
- aggregate:
    store: "postgres://localhost/telemetry/sensor_states"
    consistency: single_writer
    key: "${payload:sensor_id}"
    into: sensor
    fields: { n: count, avg: "ema(reading, 0.01)" }
```

A key is loaded from the store the first time it is seen, and a key that changed a hundred
times between two writes is written once. A batch is still acked only once its states are
stored, so a restart continues from the same values as under `shared`. In our measurements
it reached 240k msg/s on PostgreSQL where `shared` reached 17k (see
[Performance](#performance-and-tuning)). Two instances of such a route are wrong: run one.

Without a `store`, `single_writer` keeps the states in this process only. They are gone
after a restart and are not shared between instances. Leaving out both `store` and
`consistency: single_writer` is a startup error, so that states in memory are never the
result of a forgotten `store`.

```yaml
- aggregate:
    consistency: single_writer
    key: "${payload:sensor_id}"
    into: sensor
    fields: { n: count, avg: "ema(reading, 0.01)" }
```

## How it works internally

### Folding a batch

The middleware works on whole batches. For each message, in order, and for each entry:

1. The `key` template is rendered from the message.
2. The state of that key is looked up in a hash map; every entry has its own map.
3. The new state is computed, by the expression or by the built-in aggregates.
4. The state (or the previous one, or what `output` returns) is written into the payload at
   `into`. Fields the middleware does not write are copied byte for byte.

A message that fails in any step changes no state. Without a `store` this is everything:
the maps live in the process and the message goes on at once.

### What a store holds

One row per key, in one table for all entries of the middleware:

| Column | Content |
|---|---|
| `agg_key` | `<into>:<key>`, for example `sensor:7`; at most 512 characters |
| `state` | the state as JSON text |
| `version` | a counter, raised by one with every write |

MongoDB holds the same as a document `{_id, state, v}`. Every write names the version the
state was computed from and only takes effect while the row still has that version. This is
optimistic concurrency: nobody locks a state while computing, and a writer that was overtaken
notices it when it writes.

### `shared`: the store owns the states

Per batch:

1. The distinct keys of the batch are collected and loaded with one query (`WHERE agg_key
   IN (…)`, 1000 keys per statement), each with its version.
2. The batch is folded in memory, as above.
3. Every changed state is written back with the version it was loaded with. PostgreSQL and
   SQLite use one transaction of upserts, so the states of a batch land completely or not at
   all. MongoDB uses one bulk command; each state is atomic, the batch is not.
4. If a write is refused, another instance changed that key in between. The states are
   loaded again and the affected messages are folded again from them. MongoDB repeats only
   the refused keys; PostgreSQL and SQLite roll back and repeat the whole batch. After 32
   attempts the batch fails and is redelivered.
5. Only then do the messages go on. Nothing stays in memory for the next batch.

One instance runs its batches against the store one after the other, so it does not collide
with itself. The cost is two round trips per batch whatever its size, which is why large
batches pay off and why `fields` versus `expression` hardly matters here.

Between instances there is no order: of two batches that change the same key, the one that
writes first wins and the other computes again on top of it. Counters and sums come out
right either way; a moving average depends on which batch was first.

### `single_writer`: memory owns the states

1. A key is loaded from the store the first time a batch needs it. After that its state
   stays in memory, until `max_keys` pushes it out.
2. The batch is folded in memory, the changed keys are marked, and the batch gets a number.
3. The messages go on at once; they do not wait for the store.
4. A background task writes the marked states. As soon as one write returns it takes
   everything changed in the meantime, so under load one write covers many batches, and a
   key that changed a hundred times is written once. This is where the speed comes from.
5. The ack of a batch (on an output: the return of `send`) waits until a write has covered
   its number.

The writes carry versions here too. A refused write can only mean that somebody else writes
the same states, and is treated like a failed write: the states in memory are dropped, every
batch not yet stored is nacked, and the redelivered batches load their states again from the
store. So a second instance is noticed, but not prevented; nothing fences it off.

### What happens on a crash or a retry

Storing a state and acking a message are two steps, not one transaction. A message is never
acked before its state is stored, so no update is lost. The other way round is possible: the
state is stored, then the process dies or a later step fails, and the message is delivered
again. It then counts twice. A [`deduplication`](deduplication.md) in front narrows that
window, but it has a store of its own and does not close it completely.

### How this compares to stream processors

The building blocks are common ones: keyed state folded per message, as in Kafka Streams or
Flink; versioned conditional writes for `shared`; write-behind with one write for many
batches and an ack that waits for it for `single_writer`. Three things those systems do are
not done here, and they decide whether the middleware fits:

- **No exactly-once.** Kafka Streams and Flink commit the state and the input position
  together. Here a redelivered message counts twice, as described above.
- **No key ownership.** They assign each key to exactly one worker and move the state when
  workers come and go. Here you choose: `shared` lets every instance write every key and
  pays a database round trip per batch, `single_writer` is fast and leaves it to you to run
  one instance.
- **No windows and no expiry by time.** There is no "sum of the last hour" and no state
  that ends with a window or a time to live. A moving average stands in for a window, by
  message count or by time. States
  leave memory only when `max_keys` is reached, and rows in a store are kept for good. For fixed windows you can put the window into the key
  (`key: "${payload:sensor_id}:${payload:hour}"`) and use a `store` with `shared`, which
  keeps nothing in memory; the rows of past windows stay until you delete them.

If you need one of these, compute the aggregate in the database with a
[`lookup`](lookup.md), or use a stream processor for that step.

## What to know before relying on it

- **A restart continues with the same values**, with a `store` and with `time` for decaying
  values; see [Where the states live](#where-the-states-live).
- **Without `store`, state lives in memory only.** A restart starts from empty states and
  two instances do not share them. That is why it has to be asked for with
  `consistency: single_writer`.
- **Memory is bounded.** An entry keeps at most `max_keys` states in memory, one million
  unless you set it. See [Memory](#memory) for what happens beyond that.
- **`single_writer` with two instances is wrong.** The second writer is noticed as a failed
  write and its batches are redelivered, but do not rely on that: run one instance.
- **A replayed message counts twice.** Where that matters, let
  [`deduplication`](deduplication.md) see the message first: on an input, list it *after*
  `aggregate`, because a message passes the last middleware of an input first.
- **Name the table.** The default is named after the route, which under mq-bridge-app is an
  internal name, and renaming a route starts with empty states. Name the table in the URL
  (`postgres://host/db/sensor_states`).
- **On an output, list `retry` before `aggregate`.** The last middleware of an output runs
  first, so the retry then repeats only the send, not the update of the states.
- **Order matters for a moving average.** Messages of a batch are folded in order. With a
  route `concurrency` above 1 the order across batches is not guaranteed. With `time`, a
  message older than its state is weighted by its lateness and marked
  `mqb.aggregate.late`, not reordered; `ema(path, alpha)` and expressions still depend on
  the order.
- **A message that cannot be folded fails alone** and changes no state: its payload is not a
  JSON object, its key has no value, a field is missing, or an expression fails. On an input
  it is logged and dropped, and the source is told it succeeded; on an output only that
  message fails, so a following [`dlq`](dlq.md) can keep it. `on_error: fail` or `skip`
  changes that, see [Messages with missing fields](#messages-with-missing-fields).
- **Field order.** The fields of a written state object are not in the order the expression
  lists them.

## Performance and tuning

Measured on one Apple M1 core, one route worker, states in memory unless a store is
named. Read the numbers as proportions; absolute values depend on the machine and payload.

| Setup | msg/s |
|---|---|
| `fields`, one key, three aggregates | 1.4M to 1.8M |
| `fields`, five keys, three moving averages each | 700k |
| `expression`, one key | 330k to 370k |
| `expression`, five keys | 100k |
| PostgreSQL store, `single_writer`, five keys, `fields` | 240k |
| PostgreSQL store, `single_writer`, five keys, `expression` | 68k |
| PostgreSQL store, `shared`, five keys | 17k |

The update itself costs about 0.3 µs per message with `fields` and about 2 µs with an
expression. The same counter, sum and moving average built from Redpanda Connect's `branch`,
`cache` and `mapping` processors cost about 8 µs per message on the same machine.

What to change, in the order it pays off:

1. **Use `fields` where the built-ins cover it.** It is four to seven times faster than an
   expression, because no expression runs and the payload is not parsed into a tree. Keep
   expressions for the entries that need logic.
2. **With a store, pick the consistency first.** `single_writer` is about 14 times faster
   than `shared`, but only correct with one instance. Under `shared` the database sets the
   pace, and `fields` versus `expression` hardly matters.
3. **With a store, raise `batch_size`** to 4096 or 8192. A batch costs about two round trips
   under `shared` whatever its size. Without a store the batch size has little effect.
4. **Update fewer keys per message.** The cost grows with the number of `entries`, not with
   the number of aggregates in one entry: five keys cost about three times as much as one.
5. **Do not raise `concurrency` for speed** if the result depends on order, as a moving
   average does. Counters and sums stay correct; under `shared`, workers that update the
   same keys collide and compute those states again.
6. **Mind the neighbours.** A `deduplication` keyed on a payload field adds about 0.5 µs per
   message, on `message_id` about 0.2 µs. A `transform` before or after parses the payload
   again; let `aggregate` write the fields you need instead of reshaping afterwards.

## Memory

An entry keeps at most `max_keys` states in memory, one million unless you set it. When it is
full, the half that was used least recently is dropped, so a key that keeps coming stays and
a key that went quiet goes. What "dropped" means depends on where the states live:

| Setup | A dropped state |
|---|---|
| no `store` (`single_writer`) | is forgotten; its key starts again from an empty state |
| `store`, `single_writer` | is only dropped from memory and loaded again when its key returns |
| `store`, `shared` | does not occur; nothing is kept in memory between batches |

Without a store a warning is logged the first time states are forgotten. If the number of
keys is bounded, such as sensors or customers, and below the limit, nothing is ever dropped.
If it is unbounded, such as a session id, the limit is what keeps the process alive for
weeks: set `max_keys` to what you can afford, or use a `store` so that no state is lost.

```yaml
- aggregate:
    consistency: single_writer
    key: "${payload:session_id}"
    into: session
    max_keys: 200000
    fields: { n: count }
```

A `fields` state takes roughly 100 to 300 bytes, so the default limit stays within a few
hundred megabytes per entry; an expression state with a large object takes more. `max_keys:
0` removes the limit. Rows in a store are never removed; delete them yourself if the key is
unbounded.

A complete pipeline with a Postgres store, a `lookup` and routing on the result, with a runnable
example: [Enrich events and add per-key totals](../use-cases/enrich-and-aggregate-orders.md).
