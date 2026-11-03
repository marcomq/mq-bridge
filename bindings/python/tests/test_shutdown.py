"""Graceful-shutdown tests: ``request_shutdown`` and ``KeyboardInterrupt`` while a
route blocks in ``run()`` / ``join()``.

The shutdown latch is process-global and permanent, and the interrupt tests
raise real signals, so each scenario runs in a fresh subprocess.
"""

import subprocess
import sys
import textwrap

import pytest

_PRELUDE = """
import signal, threading, time
import mq_bridge

def make_route(tag):
    config = {
        "input": {"memory": {"topic": f"shutdown.{tag}.in", "capacity": 8}},
        "output": {"memory": {"topic": f"shutdown.{tag}.out", "capacity": 8}},
    }
    return mq_bridge.Route.from_config(config, f"shutdown-{tag}")

def later(seconds, action):
    threading.Timer(seconds, action).start()
"""


def _run(body: str) -> str:
    script = textwrap.dedent(_PRELUDE) + textwrap.dedent(body)
    proc = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True, timeout=30
    )
    assert proc.returncode == 0, proc.stderr
    return proc.stdout


def test_request_shutdown_stops_running_and_later_routes() -> None:
    out = _run(
        """
        assert not mq_bridge.is_shutdown_requested()
        first = []
        later(0.3, lambda: first.append(mq_bridge.request_shutdown()))
        make_route("a").run()
        assert first == [True]
        assert mq_bridge.request_shutdown() is False
        assert mq_bridge.is_shutdown_requested()

        started = time.monotonic()
        route = make_route("b")
        route.start()
        route.join()
        assert time.monotonic() - started < 2
        print("OK")
        """
    )
    assert "OK" in out


@pytest.mark.skipif(sys.platform == "win32", reason="raise_signal(SIGINT) is POSIX-only here")
@pytest.mark.parametrize("mode", ["run", "join"])
def test_keyboard_interrupt_stops_route_and_reraises(mode: str) -> None:
    out = _run(
        f"""
        route = make_route("kb")
        later(0.3, lambda: signal.raise_signal(signal.SIGINT))
        try:
            if {mode!r} == "run":
                route.run()
            else:
                route.start()
                route.join()
        except KeyboardInterrupt:
            print("INTERRUPTED")
        # The name is free again only once the route really stopped.
        again = make_route("kb")
        again.start()
        again.stop()
        again.join()
        assert not mq_bridge.is_shutdown_requested()
        print("OK")
        """
    )
    assert "INTERRUPTED" in out and "OK" in out
