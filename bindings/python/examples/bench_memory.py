import argparse
import os
import threading
import time
from pathlib import Path

from mq_bridge import MemoryDrainer, Publisher, Route


KIND = "bench.tick"
CONFIG_PATH = Path(__file__).with_name("bench_memory.yaml")
DEFAULT_SENDERS = max(1, min(8, os.cpu_count() or 1))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark Python JSON handlers over mq-bridge memory endpoints."
    )
    parser.add_argument(
        "--messages",
        type=int,
        default=100_000,
        help="Number of measured messages to publish.",
    )
    parser.add_argument(
        "--warmup",
        type=int,
        default=10_000,
        help="Number of warmup messages to publish before measurement.",
    )
    parser.add_argument(
        "--senders",
        type=int,
        default=DEFAULT_SENDERS,
        help="Number of concurrent publisher threads to use.",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=30.0,
        help="Seconds to wait for a benchmark phase to complete.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.messages < 1:
        raise ValueError("--messages must be at least 1")
    if args.warmup < 0:
        raise ValueError("--warmup must be zero or greater")
    if args.senders < 1:
        raise ValueError("--senders must be at least 1")

    transform_route = Route.from_file(str(CONFIG_PATH), "transform_route")
    publishers = [
        Publisher.from_file(str(CONFIG_PATH), "bench_publisher")
        for _ in range(args.senders)
    ]
    drainer = MemoryDrainer.from_topic("bench.out", 65_536)

    route_failures = []
    route_failed = threading.Event()

    def transform(data):
        data["value"] += 1
        return data

    transform_route.add_handler(KIND, transform)

    def run_route(route: Route, name: str) -> None:
        try:
            route.run()
        except Exception as exc:  # pragma: no cover - benchmark control path
            route_failures.append((name, exc))
            route_failed.set()

    threads = [
        threading.Thread(
            target=run_route,
            args=(transform_route, "transform_route"),
            daemon=True,
        ),
    ]

    for thread in threads:
        thread.start()

    time.sleep(0.2)

    def send_range(
        publisher: Publisher,
        start: int,
        stop: int,
        failures: list,
    ) -> None:
        try:
            for value in range(start, stop):
                publisher.send_json({"value": value}, {"kind": KIND})
        except Exception as exc:  # pragma: no cover - benchmark control path
            failures.append(exc)

    def run_phase(message_count: int, label: str):
        regular_messages = message_count
        send_failures = []

        started_at = time.perf_counter()
        drain_result = {}
        drain_failure = {}

        def drain_output() -> None:
            try:
                drain_result["count"] = drainer.drain(
                    message_count,
                    timeout=args.timeout,
                )
            except Exception as exc:  # pragma: no cover - benchmark control path
                drain_failure["error"] = exc

        drain_thread = threading.Thread(target=drain_output, daemon=True)
        drain_thread.start()

        sender_threads = []
        for sender_idx, publisher in enumerate(publishers):
            start = sender_idx * regular_messages // len(publishers)
            stop = (sender_idx + 1) * regular_messages // len(publishers)
            if start >= stop:
                continue
            thread = threading.Thread(
                target=send_range,
                args=(publisher, start, stop, send_failures),
                daemon=True,
            )
            sender_threads.append(thread)

        for thread in sender_threads:
            thread.start()

        for thread in sender_threads:
            thread.join(args.timeout)
            if thread.is_alive():
                raise TimeoutError(
                    f"{label} phase timed out while publishing "
                    f"after {args.timeout:.1f}s"
                )

        if route_failed.is_set():
            name, exc = route_failures[0]
            raise RuntimeError(f"{name} failed: {exc}") from exc
        if send_failures:
            raise RuntimeError(f"{label} sender failed: {send_failures[0]}")

        if route_failed.is_set():
            name, exc = route_failures[0]
            raise RuntimeError(f"{name} failed: {exc}") from exc
        drain_thread.join(args.timeout + 1.0)
        if drain_thread.is_alive():
            raise TimeoutError(
                f"{label} phase timed out waiting for output drain "
                f"after {args.timeout:.1f}s"
            )
        if "error" in drain_failure:
            raise RuntimeError(
                f"{label} output drain failed: {drain_failure['error']}"
            ) from drain_failure["error"]

        elapsed = time.perf_counter() - started_at
        if drain_result.get("count") != message_count:
            raise AssertionError(
                f"expected to drain {message_count} messages, "
                f"got {drain_result!r}"
            )

        throughput = message_count / elapsed if elapsed > 0 else float("inf")
        return elapsed, throughput

    try:
        print(f"senders: {len(publishers)}")
        if args.warmup > 0:
            elapsed, throughput = run_phase(args.warmup, "warmup")
            print(
                f"warmup: {args.warmup} messages in {elapsed:.3f}s "
                f"({throughput:,.0f} msgs/sec)"
            )

        elapsed, throughput = run_phase(args.messages, "benchmark")
        print(
            f"benchmark: {args.messages} messages in {elapsed:.3f}s "
            f"({throughput:,.0f} msgs/sec)"
        )
    finally:
        transform_route.stop()
        for thread in threads:
            thread.join(timeout=2.0)


if __name__ == "__main__":
    main()
