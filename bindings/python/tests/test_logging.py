"""End-to-end tests for the ``tracing`` -> ``logging`` bridge (``init_logging``).

The subscriber is a process-global, install-once resource, so each scenario runs
in a fresh subprocess. That keeps the cases independent *and* asserts the process
exits cleanly (returncode 0) — a regression guard for the shutdown crash that
happens if the bridge touches a torn-down interpreter from a transport thread.
"""

import subprocess
import sys
import textwrap

# A brokerless memory route emits INFO events from the `mq_bridge` core as it
# creates its channels; that is what we assert crosses the bridge.
_ROUTE = """
import json, logging, time
import mq_bridge

captured = []

class Capture(logging.Handler):
    def emit(self, record):
        captured.append((record.name, record.levelname, record.getMessage()))

logging.getLogger().addHandler(Capture())
logging.getLogger().setLevel(logging.DEBUG)

mq_bridge.init_logging({level!r})

config = {{
    "input": {{"memory": {{"topic": "log.in", "capacity": 8}}}},
    "output": {{"memory": {{"topic": "log.out", "capacity": 8}}}},
}}
route = mq_bridge.Route.from_config(config, "logtest")
route.start()
time.sleep(0.3)
route.stop()
route.join()

mqb = [c for c in captured if c[0].startswith("mq_bridge")]
print("RESULT", json.dumps(mqb))
"""


def _run(level, env=None, extra=""):
    script = textwrap.dedent(_ROUTE).format(level=level) + textwrap.dedent(extra)
    proc = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        env=env,
    )
    # returncode 0 also proves the bridge tears down without a segfault.
    assert proc.returncode == 0, f"stdout={proc.stdout!r} stderr={proc.stderr!r}"
    records = []
    for line in proc.stdout.splitlines():
        if line.startswith("RESULT "):
            import json

            records = json.loads(line[len("RESULT ") :])
    return records, proc


def _clean_env():
    import os

    env = dict(os.environ)
    env.pop("MQ_BRIDGE_LOG", None)
    env.pop("RUST_LOG", None)
    return env


def test_init_logging_bridges_core_events_into_stdlib_logging():
    records, _ = _run("debug", env=_clean_env())
    assert records, "no mq_bridge log records reached Python logging"
    # Hierarchical logger name (`::` -> `.`) and a real INFO event from core.
    assert any(name.startswith("mq_bridge.") for name, _, _ in records)
    assert any(level == "INFO" for _, level, _ in records)


def test_filtering_happens_in_rust_below_the_threshold():
    # At `error`, the core's INFO channel events must be dropped before they ever
    # cross into Python — proving the filter runs Rust-side, not in `logging`.
    records, _ = _run("error", env=_clean_env())
    assert not any(level == "INFO" for _, level, _ in records)


def test_env_var_overrides_the_requested_level():
    env = _clean_env()
    env["MQ_BRIDGE_LOG"] = "mq_bridge=debug"
    # Ask for `error`, but the env var must win and let INFO through.
    records, _ = _run("error", env=env)
    assert any(level == "INFO" for _, level, _ in records)


def test_empty_env_var_is_treated_as_unset():
    env = _clean_env()
    env["MQ_BRIDGE_LOG"] = ""  # set-but-empty must fall through to the arg
    records, _ = _run("debug", env=env)
    assert any(level == "INFO" for _, level, _ in records)


def test_second_init_raises_and_process_still_exits_cleanly():
    extra = """
try:
    mq_bridge.init_logging()
    print("SECOND_OK")
except RuntimeError:
    print("SECOND_RAISED")
"""
    _, proc = _run("warn", env=_clean_env(), extra=extra)
    assert "SECOND_RAISED" in proc.stdout
    assert "SECOND_OK" not in proc.stdout
