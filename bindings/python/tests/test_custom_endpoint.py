"""Custom endpoints implemented in Python.

Brokerless: the host endpoints below stand in for a real client (Pulsar, a
proprietary broker, ...), so these run without Docker.
"""

import threading
import time
import uuid

import pytest

from mq_bridge import (
    MemoryDrainer,
    Message,
    Publisher,
    Route,
    register_endpoint,
    register_middleware,
    unregister_endpoint,
    unregister_middleware,
)


def _unique(prefix: str) -> str:
    return f"{prefix}.{uuid.uuid4().hex}"


class ListSource:
    """Yields a fixed set of payloads, then reports end of stream."""

    def __init__(self, payloads):
        self.payloads = list(payloads)
        self.commits = []

    def receive_batch(self, max_messages):
        if not self.payloads:
            raise StopIteration
        batch, self.payloads = self.payloads[:max_messages], self.payloads[max_messages:]
        return batch

    def commit(self, dispositions):
        self.commits.extend(dispositions)


class ListSink:
    def __init__(self, fail=False):
        self.received = []
        self.fail = fail
        self.closed = False
        self.done = threading.Event()

    def send_batch(self, messages):
        if self.fail:
            raise ValueError("sink rejected the batch")
        self.received.extend(bytes(m.payload) for m in messages)
        self.done.set()

    def close(self):
        self.closed = True


def _register(prefix, build):
    """Register `build` under a unique name so tests never collide in the
    process-global endpoint registry."""
    name = f"{prefix}_{uuid.uuid4().hex[:8]}"
    register_endpoint(name, build)
    return name


def test_python_source_drains_into_memory() -> None:
    out_topic = _unique("pytest.custom.out")
    source = ListSource([b"one", b"two", b"three"])
    name = _register("pysrc", lambda route_name, config: source)

    route = Route.from_config(
        {
            "routes": {
                "r": {
                    "exit_on_empty": True,
                    "input": {name: {}},
                    "output": {"memory": {"topic": out_topic, "capacity": 4096}},
                }
            }
        },
        "r",
    )
    drainer = MemoryDrainer.from_topic(out_topic, 4096)

    route.start()
    try:
        assert drainer.drain(3, timeout=10.0) == 3
    finally:
        route.stop()
        route.join()

    # One "ack" per message consumed, so a real client can commit its offsets.
    assert source.commits == ["ack", "ack", "ack"]


def test_python_source_receives_config_and_route_name() -> None:
    seen = {}

    def build(route_name, config):
        seen["route_name"] = route_name
        seen["config"] = config
        return ListSource([b"payload"])

    name = _register("pycfg", build)
    out_topic = _unique("pytest.custom.cfg")

    route = Route.from_config(
        {
            "routes": {
                "cfg_route": {
                    "exit_on_empty": True,
                    "input": {name: {"url": "pulsar://localhost:6650", "batch": 7}},
                    "output": {"memory": {"topic": out_topic, "capacity": 64}},
                }
            }
        },
        "cfg_route",
    )
    drainer = MemoryDrainer.from_topic(out_topic, 64)
    route.start()
    try:
        assert drainer.drain(1, timeout=10.0) == 1
    finally:
        route.stop()
        route.join()

    assert seen["route_name"] == "cfg_route"
    assert seen["config"] == {"url": "pulsar://localhost:6650", "batch": 7}


def test_python_sink_receives_published_messages() -> None:
    in_topic = _unique("pytest.custom.in")
    sink = ListSink()
    name = _register("pysink", lambda route_name, config: sink)

    config = {
        "routes": {
            "sink_route": {
                "input": {"memory": {"topic": in_topic, "capacity": 4096}},
                "output": {name: {}},
            }
        },
        "publishers": {"pub": {"memory": {"topic": in_topic, "capacity": 4096}}},
    }
    route = Route.from_config(config, "sink_route")
    publisher = Publisher.from_config(config, "pub")

    route.start()
    try:
        publisher.send(Message(b"hello"))
        assert sink.done.wait(timeout=10.0), "sink never received the message"
    finally:
        route.stop()
        route.join()

    assert sink.received == [b"hello"]


def test_python_sink_failure_reaches_the_dlq() -> None:
    in_topic = _unique("pytest.custom.dlq.in")
    dlq_topic = _unique("pytest.custom.dlq.out")
    name = _register("pyfail", lambda route_name, config: ListSink(fail=True))

    config = {
        "routes": {
            "dlq_route": {
                "input": {"memory": {"topic": in_topic, "capacity": 4096}},
                "output": {
                    name: {},
                    "middlewares": [
                        {"dlq": {"endpoint": {"memory": {"topic": dlq_topic, "capacity": 4096}}}}
                    ],
                },
            }
        },
        "publishers": {"pub": {"memory": {"topic": in_topic, "capacity": 4096}}},
    }
    route = Route.from_config(config, "dlq_route")
    publisher = Publisher.from_config(config, "pub")
    drainer = MemoryDrainer.from_topic(dlq_topic, 4096)

    route.start()
    try:
        publisher.send(Message(b"poison"))
        assert drainer.drain(1, timeout=10.0) == 1
    finally:
        route.stop()
        route.join()


def test_endpoint_without_receive_batch_cannot_be_an_input() -> None:
    """A missing method is a config error, so the route must not retry it.

    No `reconnect_interval_ms` override here on purpose: at the 5s default, a
    retried factory error would take ~50s to surface. Failing fast is the
    assertion.
    """
    built = []

    def build(route_name, config):
        built.append(route_name)
        return ListSink()

    name = _register("pysinkonly", build)
    out_topic = _unique("pytest.custom.bad")

    route = Route.from_config(
        {
            "routes": {
                "bad_route": {
                    "exit_on_empty": True,
                    "input": {name: {}},
                    "output": {"memory": {"topic": out_topic, "capacity": 64}},
                }
            }
        },
        "bad_route",
    )
    started = time.monotonic()
    with pytest.raises(RuntimeError, match="receive_batch"):
        route.run()
    assert time.monotonic() - started < 10.0, "the route retried a permanent config error"
    assert built == ["bad_route"], "the factory was called more than once"


def test_factory_must_be_callable() -> None:
    with pytest.raises(TypeError):
        register_endpoint("not_callable", object())

    with pytest.raises(ValueError):
        register_endpoint("  ", lambda route_name, config: ListSink())


class Tagger:
    """Rewrites every message and drops the ones the config names."""

    def __init__(self, config):
        self.drop = set(config.get("drop", []))
        self.seen = []

    def _apply(self, messages):
        out = []
        for message in messages:
            payload = bytes(message.payload)
            self.seen.append(payload)
            if payload in (p.encode() for p in self.drop):
                out.append(None)
            else:
                out.append(Message(payload + b"!", message.metadata))
        return out

    def on_receive(self, messages):
        return self._apply(messages)

    def on_send(self, messages):
        return self._apply(messages)


def _register_mw(prefix, build):
    name = f"{prefix}_{uuid.uuid4().hex[:8]}"
    register_middleware(name, build)
    return name


def test_middleware_rewrites_and_drops_on_the_input_side() -> None:
    out_topic = _unique("pytest.mw.out")
    source = ListSource([b"keep", b"drop-me", b"also-keep"])
    endpoint = _register("mwsrc", lambda route_name, config: source)
    taggers = []

    def build(route_name, config):
        tagger = Tagger(config)
        taggers.append(tagger)
        return tagger

    mw = _register_mw("tagger", build)

    route = Route.from_config(
        {
            "routes": {
                "mw_route": {
                    "exit_on_empty": True,
                    "input": {
                        endpoint: {},
                        "middlewares": [{"custom": {"name": mw, "config": {"drop": ["drop-me"]}}}],
                    },
                    "output": {"memory": {"topic": out_topic, "capacity": 4096}},
                }
            }
        },
        "mw_route",
    )
    drainer = MemoryDrainer.from_topic(out_topic, 4096)

    route.start()
    try:
        assert drainer.drain(2, timeout=10.0) == 2
    finally:
        route.stop()
        route.join()

    assert taggers[0].seen == [b"keep", b"drop-me", b"also-keep"]
    # The dropped message is still acked at the source, or it would come back.
    assert source.commits == ["ack", "ack", "ack"]


def test_middleware_dropping_a_whole_batch_does_not_end_the_route() -> None:
    """A fully-filtered batch must not look like a drained source.

    The source hands over one message per call, so the middleware drops an
    entire batch before the surviving one arrives. If that empty result were
    passed up, `exit_on_empty` would end the route and `keep` would be lost.
    """

    class DripSource:
        def __init__(self, payloads):
            self.payloads = list(payloads)
            self.commits = []

        def receive_batch(self, max_messages):
            if not self.payloads:
                raise StopIteration
            return [self.payloads.pop(0)]

        def commit(self, dispositions):
            self.commits.extend(dispositions)

    out_topic = _unique("pytest.mw.dropall")
    source = DripSource([b"drop-me", b"keep"])
    endpoint = _register("mwdropall", lambda route_name, config: source)
    mw = _register_mw("dropall", lambda route_name, config: Tagger(config))

    route = Route.from_config(
        {
            "routes": {
                "drop_route": {
                    "exit_on_empty": True,
                    "input": {
                        endpoint: {},
                        "middlewares": [{"custom": {"name": mw, "config": {"drop": ["drop-me"]}}}],
                    },
                    "output": {"memory": {"topic": out_topic, "capacity": 4096}},
                }
            }
        },
        "drop_route",
    )
    drainer = MemoryDrainer.from_topic(out_topic, 4096)

    route.start()
    try:
        assert drainer.drain(1, timeout=10.0) == 1
    finally:
        route.stop()
        route.join()

    # Both messages acked: the dropped one by the middleware, the kept one by
    # the route.
    assert source.commits == ["ack", "ack"]


def test_middleware_rewrites_on_the_output_side() -> None:
    in_topic = _unique("pytest.mw.in")
    sink = ListSink()
    endpoint = _register("mwsink", lambda route_name, config: sink)
    mw = _register_mw("tagger_out", lambda route_name, config: Tagger(config))

    config = {
        "routes": {
            "mw_out_route": {
                "input": {"memory": {"topic": in_topic, "capacity": 4096}},
                "output": {
                    endpoint: {},
                    "middlewares": [{"custom": {"name": mw, "config": {}}}],
                },
            }
        },
        "publishers": {"pub": {"memory": {"topic": in_topic, "capacity": 4096}}},
    }
    route = Route.from_config(config, "mw_out_route")
    publisher = Publisher.from_config(config, "pub")

    route.start()
    try:
        publisher.send(Message(b"payload"))
        assert sink.done.wait(timeout=10.0), "sink never received the message"
    finally:
        route.stop()
        route.join()

    assert sink.received == [b"payload!"]


def test_unregister_reports_whether_a_registration_existed_and_frees_the_name() -> None:
    def build(route_name, config):
        return ListSink()

    name = _register("pyunreg", build)

    assert unregister_endpoint(name) is True
    assert unregister_endpoint(name) is False
    assert unregister_middleware(_register_mw("mwunreg", build)) is True
    assert unregister_middleware("mw_never_registered") is False

    # The registry rejects duplicates, so re-registering proves the name is free.
    register_endpoint(name, build)
    assert unregister_endpoint(name) is True


def test_middleware_without_either_hook_passes_through() -> None:
    in_topic = _unique("pytest.mw.noop.in")
    sink = ListSink()
    endpoint = _register("mwnoop", lambda route_name, config: sink)
    mw = _register_mw("noop", lambda route_name, config: object())

    config = {
        "routes": {
            "noop_route": {
                "input": {"memory": {"topic": in_topic, "capacity": 4096}},
                "output": {
                    endpoint: {},
                    "middlewares": [{"custom": {"name": mw, "config": {}}}],
                },
            }
        },
        "publishers": {"pub": {"memory": {"topic": in_topic, "capacity": 4096}}},
    }
    route = Route.from_config(config, "noop_route")
    publisher = Publisher.from_config(config, "pub")

    route.start()
    try:
        publisher.send(Message(b"untouched"))
        assert sink.done.wait(timeout=10.0), "sink never received the message"
    finally:
        route.stop()
        route.join()

    assert sink.received == [b"untouched"]
