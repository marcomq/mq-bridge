"""Use mq-bridge as a bounded, deferred-ack source for a `dlt` pipeline.

This is the split that warehouse loaders (e.g. omniload) need: mq-bridge consumes
and yields raw records, `dlt` owns the write (normalize → stage → merge), and
mq-bridge acks **only after** the load package is committed. If the load fails,
the batch is never acked and the broker redelivers it — so the merge below MUST be
idempotent (note `primary_key` + `write_disposition="merge"`).

Key API: `poll_batch(max, timeout_ms) -> (records, token)` returns a batch without
acking and hands back a token; `ack(token)` commits that batch; `nack(token)`
releases it for redelivery. Contrast with `poll()` + `commit()`, which acks every
outstanding batch at once — fine for strict poll→handle→commit loops, but the token
form is what lets you ack exactly the batch `dlt` just persisted.

Runs brokerless over an in-memory endpoint, so no services are required. Swap the
ENDPOINT dict for a broker (kafka/nats/amqp/…) and the loop is unchanged.

Requires `dlt` (an example-only dependency, not needed to use mq-bridge):
    pip install "dlt[duckdb]"
    python examples/dlt_source.py
"""

from __future__ import annotations

import dlt

from mq_bridge import Consumer, Publisher

ENDPOINT = {"memory": {"topic": "orders.dlt.demo", "capacity": 4096}}

# Bound each run so the generator terminates over an unbounded broker.
MAX_MESSAGES = 1_000
IDLE_TIMEOUT_MS = 2_000  # return early once the broker is idle this long


def record_from_message(message) -> dict:
    """Project an mq-bridge Message into a flat record for dlt to normalize.

    `message.id` is globally unique per source position (Kafka partition/offset,
    NATS stream sequence, AMQP delivery tag), which makes it a natural merge key
    for at-least-once + idempotent-merge. Source cursor fields are also exposed in
    `metadata` (e.g. mqb.src.kafka_topic/mqb.src.kafka_offset,
    mqb.src.nats_subject/mqb.src.nats_stream_sequence) when a broker source config
    sets `source_metadata: true` (for example,
    `{"kafka": {"url": "localhost:9092", "source_metadata": True}}`).
    """
    record = dict(message.json())
    record["_mqb_id"] = message.id
    record["_mqb_metadata"] = dict(message.metadata)
    return record


def batch_resource(records: list[dict]):
    """Wrap one already-polled batch as a dlt resource.

    The batch is fully materialized *before* the run, so a single
    ``pipeline.run(batch_resource(records))`` loads exactly this batch. We can
    then ack only after that run returns — i.e. after the load package has
    committed. (Acking from *inside* a streaming generator would ack while dlt is
    still pulling items, before the load commits — losing the deferred-ack
    guarantee.)
    """

    @dlt.resource(name="orders", write_disposition="merge", primary_key="_mqb_id")
    def orders():
        yield from records

    return orders


def main() -> None:
    # Seed the demo topic with a few records.
    publisher = Publisher.from_config(ENDPOINT)
    for order_id in range(5):
        publisher.send_json({"order_id": order_id, "amount": order_id * 10})

    pipeline = dlt.pipeline(
        pipeline_name="mq_bridge_orders",
        destination="duckdb",
        dataset_name="bridge",
    )

    # Poll → load one batch → ack, in lockstep. The ack happens only after
    # pipeline.run() commits the load; on failure we nack so the broker
    # redelivers (idempotent merge on `_mqb_id` dedups the retry).
    with Consumer.from_config(ENDPOINT) as consumer:
        drained = 0
        while drained < MAX_MESSAGES:
            messages, token = consumer.poll_batch(
                max=min(256, MAX_MESSAGES - drained),
                timeout_ms=IDLE_TIMEOUT_MS,
            )
            if not messages:
                break  # idle timeout or end-of-stream: bounded termination

            records = [record_from_message(m) for m in messages]
            try:
                info = pipeline.run(batch_resource(records))
            except Exception:
                consumer.nack(token)  # release for redelivery; merge dedups
                raise
            consumer.ack(token)  # only now is the load durably committed
            drained += len(messages)
            print(info)


if __name__ == "__main__":
    main()
