"""Brokerless end-to-end tests for the route/publisher runtime surface.

These exercise the actual compiled extension (not just internal Rust units)
over in-memory endpoints, so no Docker services are required.
"""

import threading
import time
import uuid

import pytest

from mq_bridge import Consumer, MemoryDrainer, Publisher, Route


def _unique(prefix: str) -> str:
    return f"{prefix}.{uuid.uuid4().hex}"


def _transform_config(in_topic: str, out_topic: str) -> dict:
    return {
        "routes": {
            "transform_route": {
                "batch_size": 64,
                "input": {"memory": {"topic": in_topic, "capacity": 4096}},
                "output": {"memory": {"topic": out_topic, "capacity": 4096}},
            }
        },
        "publishers": {
            "pub": {"memory": {"topic": in_topic, "capacity": 4096}}
        },
    }


@pytest.mark.parametrize("executor", ["worker", "direct"])
def test_from_config_round_trip(monkeypatch: pytest.MonkeyPatch, executor: str) -> None:
    monkeypatch.setenv("MQ_BRIDGE_PY_HANDLER_EXECUTOR", executor)

    in_topic = _unique("pytest.in")
    out_topic = _unique("pytest.out")
    config = _transform_config(in_topic, out_topic)

    route = Route.from_config(config, "transform_route")
    publisher = Publisher.from_config(config, "pub")
    drainer = MemoryDrainer.from_topic(out_topic, 4096)

    def transform(data):
        data["value"] += 1
        return data

    route.add_handler("bench.tick", transform)
    route.start()
    try:
        for value in range(50):
            publisher.send_json({"value": value}, {"kind": "bench.tick"})
        drained = drainer.drain(50, timeout=10.0)
    finally:
        route.stop()
        route.join()

    assert drained == 50


def test_context_manager_starts_and_stops() -> None:
    in_topic = _unique("pytest.cm.in")
    out_topic = _unique("pytest.cm.out")
    config = _transform_config(in_topic, out_topic)

    route = Route.from_config(config, "transform_route").add_handler(
        "bench.tick", lambda data: data
    )
    publisher = Publisher.from_config(config, "pub")
    drainer = MemoryDrainer.from_topic(out_topic, 4096)

    with route:
        for value in range(10):
            publisher.send_json({"value": value}, {"kind": "bench.tick"})
        assert drainer.drain(10, timeout=10.0) == 10


def test_start_rejects_double_start() -> None:
    in_topic = _unique("pytest.dup.in")
    out_topic = _unique("pytest.dup.out")
    route = Route.from_config(_transform_config(in_topic, out_topic), "transform_route")
    route.add_handler("bench.tick", lambda data: data)

    route.start()
    try:
        with pytest.raises(RuntimeError):
            route.start()
    finally:
        route.stop()
        route.join()


def test_run_blocks_until_stop() -> None:
    """run() must not return until another thread calls stop() — this is the
    blocking contract that start() exists to avoid."""
    in_topic = _unique("pytest.run.in")
    out_topic = _unique("pytest.run.out")
    route = Route.from_config(_transform_config(in_topic, out_topic), "transform_route")
    route.add_handler("bench.tick", lambda data: data)

    returned = threading.Event()

    def run() -> None:
        route.run()
        returned.set()

    thread = threading.Thread(target=run, daemon=True)
    thread.start()

    # Give run() a moment; it should still be blocking.
    time.sleep(0.3)
    assert not returned.is_set()

    route.stop()
    assert returned.wait(timeout=5.0)
    thread.join(timeout=5.0)


def test_consumer_poll_and_commit_round_trip() -> None:
    topic = _unique("pytest.consumer")
    endpoint = {"memory": {"topic": topic, "capacity": 4096}}

    publisher = Publisher.from_config(endpoint)
    consumer = Consumer.from_config(endpoint)

    for value in range(5):
        publisher.send_json({"value": value}, {"kind": "bench.tick"})

    received = []
    while len(received) < 5:
        batch = consumer.poll(max=10, timeout_ms=5000)
        assert batch, "poll timed out before all messages arrived"
        received.extend(batch)
    consumer.commit()

    assert [m.json()["value"] for m in received] == list(range(5))
    assert received[0].metadata["kind"] == "bench.tick"


def test_consumer_poll_batch_ack_round_trip() -> None:
    topic = _unique("pytest.consumer.pollbatch")
    endpoint = {"memory": {"topic": topic, "capacity": 4096}}

    publisher = Publisher.from_config(endpoint)
    consumer = Consumer.from_config(endpoint)

    for value in range(3):
        publisher.send_json({"value": value})

    received = []
    while len(received) < 3:
        messages, token = consumer.poll_batch(max=10, timeout_ms=5000)
        assert messages, "poll_batch timed out before all messages arrived"
        assert token is not None
        received.extend(messages)
        consumer.ack(token)  # ack just this batch by token

    assert [m.json()["value"] for m in received] == list(range(3))


def test_consumer_poll_batch_timeout_returns_none_token() -> None:
    topic = _unique("pytest.consumer.pollbatch.empty")
    consumer = Consumer.from_config({"memory": {"topic": topic, "capacity": 16}})

    messages, token = consumer.poll_batch(max=4, timeout_ms=200)
    assert messages == []
    assert token is None


def test_consumer_nack_token_redelivers_batch() -> None:
    topic = _unique("pytest.consumer.nack")
    # enable_nack makes the in-memory endpoint requeue nacked messages (brokers
    # redeliver natively); without it nack is a no-op on memory.
    endpoint = {"memory": {"topic": topic, "capacity": 4096, "enable_nack": True}}

    publisher = Publisher.from_config(endpoint)
    consumer = Consumer.from_config(endpoint)

    publisher.send_json({"value": 1})

    messages, token = consumer.poll_batch(max=10, timeout_ms=5000)
    assert [m.json()["value"] for m in messages] == [1]
    consumer.nack(token)  # release for redelivery instead of acking

    messages, token = consumer.poll_batch(max=10, timeout_ms=5000)
    assert [m.json()["value"] for m in messages] == [1], "nacked batch was not redelivered"
    consumer.ack(token)


def test_consumer_nack_all_redelivers() -> None:
    topic = _unique("pytest.consumer.nackall")
    endpoint = {"memory": {"topic": topic, "capacity": 4096, "enable_nack": True}}

    publisher = Publisher.from_config(endpoint)
    consumer = Consumer.from_config(endpoint)

    publisher.send_json({"value": 1})
    publisher.send_json({"value": 2})

    # Two separate outstanding batches, neither acked.
    first, _ = consumer.poll_batch(max=1, timeout_ms=5000)
    second, _ = consumer.poll_batch(max=1, timeout_ms=5000)
    assert {m.json()["value"] for m in first + second} == {1, 2}

    consumer.nack()  # nack every outstanding batch

    redelivered = []
    while len(redelivered) < 2:
        messages, token = consumer.poll_batch(max=10, timeout_ms=5000)
        assert messages, "nack-all batch was not redelivered"
        redelivered.extend(messages)
        consumer.ack(token)
    assert {m.json()["value"] for m in redelivered} == {1, 2}


def test_consumer_ack_unknown_token_raises() -> None:
    topic = _unique("pytest.consumer.badtoken")
    consumer = Consumer.from_config({"memory": {"topic": topic, "capacity": 16}})

    with pytest.raises(ValueError):
        consumer.ack(123)
    with pytest.raises(ValueError):
        consumer.nack(123)


def test_consumer_poll_timeout_returns_empty() -> None:
    topic = _unique("pytest.consumer.empty")
    consumer = Consumer.from_config({"memory": {"topic": topic, "capacity": 16}})

    assert consumer.poll(max=4, timeout_ms=200) == []
    assert consumer.exhausted is False


def test_consumer_status_reports_endpoint() -> None:
    topic = _unique("pytest.consumer.status")
    consumer = Consumer.from_config({"memory": {"topic": topic, "capacity": 16}})

    status = consumer.status()
    assert isinstance(status, dict)
    assert status["healthy"] is True
    # `pending` is present for the memory backend (channel depth); on transports
    # without a backlog concept it may be absent/None.
    assert "pending" in status


def test_consumer_close_is_idempotent_and_blocks_use() -> None:
    topic = _unique("pytest.consumer.close")
    consumer = Consumer.from_config({"memory": {"topic": topic, "capacity": 16}})

    consumer.close()
    consumer.close()  # idempotent

    with pytest.raises(RuntimeError):
        consumer.poll(max=1, timeout_ms=50)
    with pytest.raises(RuntimeError):
        consumer.status()


def test_consumer_context_manager_closes() -> None:
    topic = _unique("pytest.consumer.cm")
    with Consumer.from_config({"memory": {"topic": topic, "capacity": 16}}) as consumer:
        assert consumer.poll(max=1, timeout_ms=50) == []

    # Closed on exit.
    with pytest.raises(RuntimeError):
        consumer.poll(max=1, timeout_ms=50)


def test_publisher_request_json_echoes_via_config() -> None:
    publisher = Publisher.from_config({"response": {}}, "echo")

    reply = publisher.request_json({"order_id": 7, "status": "ok"})

    assert reply.json() == {"order_id": 7, "status": "ok"}
    assert reply.id is not None


def test_publisher_from_config_without_name_uses_bare_endpoint() -> None:
    # No name => the mapping is a single bare endpoint body.
    publisher = Publisher.from_config({"response": {}})

    reply = publisher.request(b"ping")

    assert reply.payload == b"ping"


def test_route_from_config_without_name_uses_bare_route() -> None:
    in_topic = _unique("pytest.in")
    out_topic = _unique("pytest.out")

    # No name => the mapping is a single bare route body.
    route = Route.from_config(
        {
            "input": {"memory": {"topic": in_topic, "capacity": 4096}},
            "output": {"memory": {"topic": out_topic, "capacity": 4096}},
        }
    ).with_handler(lambda msg: msg)
    publisher = Publisher.from_config({"memory": {"topic": in_topic, "capacity": 4096}})
    drainer = MemoryDrainer.from_topic(out_topic, 4096)

    route.start()
    try:
        publisher.send(b"hello")
        assert drainer.drain(1, timeout=5.0) == 1
    finally:
        route.stop()
        route.join()


def test_publisher_from_str_echoes() -> None:
    publisher = Publisher.from_str(
        """
        publishers:
          echo:
            response: {}
        """,
        "echo",
    )

    reply = publisher.request(b"ping")

    assert reply.payload == b"ping"


def test_from_yaml_str_alias_is_deprecated_but_works() -> None:
    with pytest.warns(DeprecationWarning):
        publisher = Publisher.from_yaml_str("response: {}")

    reply = publisher.request(b"ping")

    assert reply.payload == b"ping"
