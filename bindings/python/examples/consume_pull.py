"""Pull-based consumer: receive batches on your own schedule and commit after
handling them. This is the shape a generator-style sink (e.g. a ``dlt``
resource) wants, instead of the push-based ``Route`` handler.

Runs brokerless over an in-memory endpoint, so no services are required.
"""

from mq_bridge import Consumer, Publisher

# Same topic for both ends; swap this dict for a broker config (nats/amqp/mqtt/…)
# and the rest of the loop is unchanged.
ENDPOINT = {"memory": {"topic": "orders.pull.demo", "capacity": 4096}}


def main() -> None:
    publisher = Publisher.from_config(ENDPOINT)
    consumer = Consumer.from_config(ENDPOINT)

    for value in range(5):
        publisher.send_json({"order_id": value}, {"kind": "order.created"})

    received = 0
    while received < 5:
        batch = consumer.poll(max=10, timeout_ms=2000)  # [] on timeout
        if not batch:
            break
        for message in batch:
            print("got:", message.json(), "kind:", message.metadata.get("kind"))
        received += len(batch)
        # commit() is required: it advances the offset. Skip it and the broker
        # re-delivers everything and eventually stalls. Only commit batches you
        # have durably handled; don't commit a failed one and it is redelivered.
        consumer.commit()

    print(f"handled {received} message(s)")


if __name__ == "__main__":
    main()
