import os
import re
import subprocess
import sys
from pathlib import Path

import pytest


BENCHMARK_RE = re.compile(
    r"^benchmark: (?P<count>\d+) messages in (?P<elapsed>[0-9.]+)s "
    r"\((?P<throughput>[0-9,]+) msgs/sec\)$"
)


pytestmark = pytest.mark.performance


def test_memory_benchmark_smoke() -> None:
    if os.environ.get("MQ_BRIDGE_RUN_PERF_TESTS") != "1":
        pytest.skip("set MQ_BRIDGE_RUN_PERF_TESTS=1 to run performance smoke tests")

    package_dir = Path(__file__).resolve().parents[1]
    script = package_dir / "examples" / "bench_memory.py"
    completed = subprocess.run(
        [
            sys.executable,
            str(script),
            "--messages",
            "100",
            "--warmup",
            "10",
            "--senders",
            "1",
            "--timeout",
            "10",
        ],
        cwd=package_dir,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=30,
    )

    match = next(
        (
            BENCHMARK_RE.match(line)
            for line in completed.stdout.splitlines()
            if line.startswith("benchmark:")
        ),
        None,
    )
    assert match is not None, completed.stdout
    assert int(match.group("count")) == 100
    assert float(match.group("elapsed")) > 0
    assert int(match.group("throughput").replace(",", "")) > 0
