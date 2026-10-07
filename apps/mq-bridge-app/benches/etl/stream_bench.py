#!/usr/bin/env python3
"""Kafka -> JSONL timing for scenario 9, with one stopwatch for both tools.

Arroyo is a streaming engine: a pipeline never "finishes", so it cannot be timed
by waiting for a process to exit the way the rest of this harness times things.
Rather than time the two sides differently, both are timed the same way here:

    start the job, then poll the sink until it holds the whole dataset.

That symmetry is the point, and two asymmetries it removes are each big enough to
have decided the result on their own:

  * `copy --drain` exits when a poll comes back empty, which costs a fixed
    `drain_idle_timeout` (1 s by default) of idling at the end of every run —
    ~28% of a 3.5 s passthrough — that Arroyo would never have been charged.
    The runs here are continuous bridges, killed once the data has landed.
  * The same 1 s applies to the *first* poll, so a fresh consumer group whose
    rebalance takes longer than that drains immediately: the route logs
    "drained (empty batch)", exits 0, and lands zero rows. It is nondeterministic
    — it hit 1 run in 8 — and without a row-count check it would publish as an
    exceptionally fast sample rather than as a failure.

Completion is detected on total sink bytes, not lines: the dataset is fixed and
both projections are deterministic, so the byte total is identical run to run,
and re-reading ~200 MB every 100 ms to count newlines would perturb the thing
being measured. The row count is verified once, after the clock stops.

Arroyo's pipeline planning and scheduling are measured separately and reported as
`startup_s` rather than folded into throughput — charging a stream processor's
one-off scheduling to a 1M-row batch is the mirror of the mistake above.
"""
from __future__ import annotations

import argparse
import json
import os
import random
import re
import shutil
import signal
import statistics
import string
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

API = os.environ.get("ARROYO_API", "http://localhost:5115/api/v1")
ARROYO_CONTAINER = os.environ.get("ARROYO_CONTAINER", "arroyo-etl-bench")
OUT_ROOT = "/arroyo-out"

SOURCE_COLUMNS = """
    id BIGINT,
    first_name TEXT,
    country TEXT,
    amount DOUBLE,
    created_at TEXT,
    active BOOLEAN,
    attributes TEXT
"""

# The projection both sides perform: 4 of the 7 columns, same 4.
PROJECTION = ["id", "first_name", "country", "amount"]
# The same projection in Bloblang, for Redpanda Connect and the plugin's middleware.
BLOBLANG_PROJECTION = "root = {" + ", ".join(f'"{c}": this.{c}' for c in PROJECTION) + "}"


# --------------------------------------------------------------------------- #
# Arroyo side
# --------------------------------------------------------------------------- #

def arroyo_query(topic: str, group_id: str, out_dir: str, brokers: str) -> str:
    """Stateless JSON -> project -> JSON. No window, join, aggregation or UDF.

    A byte-level passthrough is deliberately absent on this side: Arroyo's
    filesystem sink rejects `raw_string` ("unsupported format raw_string"), so the
    shape `copy --from kafka --to file?format=raw` measures is not expressible
    here at all. Routing it through `format='json'` with a single `value` column
    would re-wrap every record as {"value": "..."}, which is different output and
    different work — so the passthrough figure is reported for mq-bridge-app alone,
    as a ceiling rather than as a comparison.
    """
    sink_cols = "id BIGINT, first_name TEXT, country TEXT, amount DOUBLE"
    return f"""
CREATE TABLE bench_source ({SOURCE_COLUMNS}) WITH (
    connector = 'kafka',
    bootstrap_servers = '{brokers}',
    topic = '{topic}',
    type = 'source',
    format = 'json',
    'source.offset' = 'earliest',
    'source.group_id' = '{group_id}'
);
CREATE TABLE bench_sink ({sink_cols}) WITH (
    connector = 'filesystem',
    type = 'sink',
    path = 'file://{out_dir}',
    format = 'json',
    'rolling_policy.file_size' = '2000000000'
);
INSERT INTO bench_sink SELECT {', '.join(PROJECTION)} FROM bench_source;
"""


def api(method: str, path: str, body: dict | None = None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        f"{API}{path}", data=data, method=method,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req) as r:
            raw = r.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        raise SystemExit(f"{method} {path} -> {e.code}: {e.read().decode()[:500]}")


def docker_inspect(name: str, fmt: str) -> str:
    return subprocess.run(
        ["docker", "inspect", "--format", fmt, name], capture_output=True, text=True,
    ).stdout.strip()


def dexec(cmd: str) -> str:
    return subprocess.run(
        ["docker", "exec", ARROYO_CONTAINER, "sh", "-c", cmd],
        capture_output=True, text=True,
    ).stdout.strip()


class ArroyoJob:
    """One Arroyo pipeline run, as a context manager."""

    def __init__(self, args, tag: str):
        self.args = args
        self.out_dir = f"{OUT_ROOT}/run_{tag}"
        self.group = f"arroyo_{tag}"
        self.pipeline = None
        self.log = f"docker logs {ARROYO_CONTAINER}"
        self.startup = 0.0

    def start(self):
        dexec(f"rm -rf {self.out_dir}")
        created = time.time()
        self.pipeline = api("POST", "/pipelines", {
            "name": f"bench_{self.group}",
            "query": arroyo_query(self.args.topic, self.group, self.out_dir, self.args.brokers),
            "parallelism": self.args.parallelism,
            "checkpointIntervalMicros": int(self.args.checkpoint_interval * 1_000_000),
        })["id"]
        deadline = time.time() + self.args.startup_timeout
        while time.time() < deadline:
            jobs = api("GET", f"/pipelines/{self.pipeline}/jobs")["data"]
            if jobs:
                state = jobs[0]["state"]
                if state == "Running":
                    self.startup = time.time() - created
                    return
                if state in ("Failed", "Stopped"):
                    raise SystemExit(f"job {state}: {jobs[0].get('failure_message')}")
            time.sleep(0.1)
        raise SystemExit(f"pipeline never reached Running within {self.args.startup_timeout}s")

    def sink_bytes(self) -> int:
        out = dexec(
            f"find {self.out_dir} -type f -printf '%s\\n' 2>/dev/null "
            f"| awk '{{s+=$1}} END{{print s+0}}'"
        )
        return int(out or 0)

    def sink_lines(self) -> int:
        return int(dexec(f"find {self.out_dir} -type f -exec cat {{}} + 2>/dev/null | wc -l") or 0)

    def is_alive(self) -> bool:
        if not self.pipeline:
            return False
        jobs = api("GET", f"/pipelines/{self.pipeline}/jobs")["data"]
        return bool(jobs) and jobs[0]["state"] == "Running"

    def stop(self):
        # Idempotent: one_run stops the job before counting rows and again in its
        # finally, and a second PATCH against a torn-down pipeline is a 400 that
        # would mask whatever the run actually did.
        if not self.pipeline:
            return
        pipeline, self.pipeline = self.pipeline, None
        api("PATCH", f"/pipelines/{pipeline}", {"stop": "immediate"})
        deadline = time.time() + 120
        while time.time() < deadline:
            jobs = api("GET", f"/pipelines/{pipeline}/jobs")["data"]
            if not jobs or jobs[0]["state"] in ("Stopped", "Failed", "Finished"):
                break
            time.sleep(0.5)
        try:
            api("DELETE", f"/pipelines/{pipeline}")
        except SystemExit:
            pass  # a pipeline that will not delete is not a measurement error

    def cleanup(self):
        if self.args.keep_output:
            # Named so the caller can compare exactly one run. Every run gets its
            # own directory, so keeping them all and globbing /arroyo-out would
            # concatenate the warmup with the timed runs — 3M rows presented as a
            # parity mismatch against a correct 1M-row file.
            print(f"KEPT_OUTPUT_DIR={self.out_dir}", flush=True)
        else:
            dexec(f"rm -rf {self.out_dir}")

# --------------------------------------------------------------------------- #
# mq-bridge-app side
# --------------------------------------------------------------------------- #

def mqb_source_uri(args, tag: str) -> str:
    """kafka:// source URI for one run.

    Two params are not optional and both fail silently if omitted:
    `auto.offset.reset=earliest`, because librdkafka defaults a fresh group to
    `latest` and every run after the first would read nothing; and a unique
    `group_id`, because a reused group resumes at its committed offset and does
    the same.
    """
    extra = json.loads(os.environ.get("MQB_CONSUMER_OPTIONS", "[]"))
    opts = urllib.parse.quote(json.dumps([["auto.offset.reset", "earliest"], *extra]))
    brokers = args.brokers if args.tool == "mqb-docker" else args.brokers_host
    uri = (f"kafka://{brokers}?topic={args.topic}"
           f"&group_id=mqb_{tag}&consumer_options={opts}")
    if args.variant in ("projection", "projection-dedup"):
        mapping = urllib.parse.quote(json.dumps({c: f"$.{c}" for c in PROJECTION}))
        uri += f"|transform?mapping={mapping}"
    if args.variant == "projection-bloblang":
        uri += f"|connect_mapping?mapping={urllib.parse.quote(BLOBLANG_PROJECTION, safe='')}"
    if args.variant == "projection-dedup":
        uri += (f"|deduplication?store=sled://{args.dedup_store}"
                f"&key={urllib.parse.quote('${payload:id}', safe='')}&ttl_seconds=3600")
    return uri


class KafkaSink:
    """A destination topic per run, measured in records instead of file bytes."""

    def __init__(self, args, tag: str):
        # Lazy: only `--sink kafka` needs the client (`uv run --with kafka-python`).
        from kafka import KafkaConsumer, TopicPartition
        from kafka.admin import KafkaAdminClient, NewTopic
        self.topic = f"bench_dst_{tag}"
        self.admin = KafkaAdminClient(bootstrap_servers=args.brokers_host)
        self.admin.create_topics([NewTopic(self.topic, args.partitions, 1)])
        self.consumer = KafkaConsumer(bootstrap_servers=args.brokers_host)
        self.parts = [TopicPartition(self.topic, p) for p in range(args.partitions)]

    def records(self) -> int:
        return sum(self.consumer.end_offsets(self.parts).values())

    def close(self):
        self.admin.delete_topics([self.topic])
        self.consumer.close()
        self.admin.close()


class MqbJob:
    """One `copy` process, run as a continuous bridge and killed once complete."""

    def __init__(self, args, tag: str):
        self.args = args
        self.out = args.out_file
        self.proc = None
        self.log = f"{self.out}.{tag}.log"
        self.log_file = None
        self.startup = 0.0
        self.tag = tag
        self.kafka_sink = KafkaSink(args, tag) if args.sink == "kafka" else None

    def start(self):
        for p in (self.out, self.args.dedup_store):
            if os.path.isdir(p):
                shutil.rmtree(p, ignore_errors=True)
            elif os.path.exists(p):
                os.remove(p)
        cmd = [
            self.args.bin, "copy",
            "--from", mqb_source_uri(self.args, self.tag),
            "--to", (f"kafka://{self.args.brokers_host}?topic={self.kafka_sink.topic}"
                     if self.kafka_sink
                     else f"file://{self.out}?format={self.args.mqb_file_format}"),
            "--batch-size", str(self.args.batch_size),
            "--concurrency", str(self.args.parallelism),
        ]
        if self.args.variant == "projection-bloblang":
            cmd[2:2] = ["--plugin", self.args.connect_plugin]
        self.log_file = open(self.log, "w")
        self.proc = subprocess.Popen(cmd, stdout=self.log_file, stderr=subprocess.STDOUT, start_new_session=True)

    def is_alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def sink_bytes(self) -> int:
        if self.kafka_sink:
            return self.kafka_sink.records()
        try:
            return os.path.getsize(self.out)
        except OSError:
            return 0

    def sink_lines(self) -> int:
        if self.kafka_sink:
            return self.kafka_sink.records()
        with open(self.out, "rb") as f:
            return sum(1 for _ in f)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
            self.proc.wait(timeout=30)

    def cleanup(self):
        if self.log_file:
            self.log_file.close()
        if self.kafka_sink:
            self.kafka_sink.close()


class MqbDockerJob:
    """The published mq-bridge-app image, as a container next to Arroyo's.

    Used when the other side is itself a container, so both tools sit in the same
    Docker VM, reach the broker over the compose network, and write to the same
    named volume. The sink is read through the Arroyo container, exactly as
    Arroyo's own is, because the mq-bridge-app image has no shell.
    """

    def __init__(self, args, tag: str):
        self.args = args
        self.tag = tag
        self.container = f"mqb-etl-bench-{tag}"
        self.out_dir = f"{OUT_ROOT}/mqb_{tag}"
        self.log = f"docker logs {self.container}"
        self.startup = 0.0

    def start(self):
        # The image runs as an unprivileged user; the volume root is root-owned.
        dexec(f"rm -rf {self.out_dir} && mkdir -p {self.out_dir} && chmod 777 {self.out_dir}")
        fmt = "{{range .Mounts}}{{if eq .Destination \"%s\"}}{{.Name}}{{end}}{{end}}" % OUT_ROOT
        volume = docker_inspect(ARROYO_CONTAINER, fmt)
        network = docker_inspect(
            ARROYO_CONTAINER, "{{range $k, $v := .NetworkSettings.Networks}}{{$k}}{{end}}")
        if not volume or not network:
            raise SystemExit(f"cannot resolve the volume/network of {ARROYO_CONTAINER}")
        created = time.time()
        subprocess.run([
            "docker", "run", "-d", "--name", self.container,
            "--network", network, "-v", f"{volume}:{OUT_ROOT}",
            self.args.mqb_image, "copy",
            "--from", mqb_source_uri(self.args, self.tag),
            "--to", f"file://{self.out_dir}/out.jsonl?format={self.args.mqb_file_format}",
            "--batch-size", str(self.args.batch_size),
            "--concurrency", str(self.args.parallelism),
        ], check=True, capture_output=True)
        self.startup = time.time() - created

    def is_alive(self) -> bool:
        return docker_inspect(self.container, "{{.State.Running}}") == "true"

    def sink_bytes(self) -> int:
        out = dexec(
            f"find {self.out_dir} -type f -printf '%s\\n' 2>/dev/null "
            f"| awk '{{s+=$1}} END{{print s+0}}'"
        )
        return int(out or 0)

    def sink_lines(self) -> int:
        return int(dexec(f"find {self.out_dir} -type f -exec cat {{}} + 2>/dev/null | wc -l") or 0)

    def stop(self):
        subprocess.run(["docker", "kill", self.container], capture_output=True)

    def cleanup(self):
        subprocess.run(["docker", "rm", "-f", self.container], capture_output=True)
        if self.args.keep_output:
            print(f"KEPT_OUTPUT_DIR={self.out_dir}", flush=True)
        else:
            dexec(f"rm -rf {self.out_dir}")


class RedpandaConnectJob(MqbJob):
    """Redpanda Connect standalone: `redpanda` input, optional mapping, `file` output."""

    # Its fetches can pause for seconds mid-backlog, which the default 2 s
    # calibration window would take for the end of the data.
    stable_window = 8.0

    def start(self):
        if os.path.exists(self.out):
            os.remove(self.out)
        pipeline = ""
        if self.args.variant == "projection":
            pipeline = f"pipeline:\n  processors:\n    - mapping: '{BLOBLANG_PROJECTION}'\n"
        elif self.args.variant != "passthrough":
            raise SystemExit(f"redpanda-connect has no {self.args.variant} variant")
        config = (
            "input:\n  redpanda:\n"
            f"    seed_brokers: [\"{self.args.brokers_host}\"]\n"
            f"    topics: [\"{self.args.topic}\"]\n"
            f"    consumer_group: rpc_{self.tag}\n"
            "    start_offset: earliest\n"
            # The ordered default stalls for seconds on this backlog; unordered
            # with the same batch size as mq-bridge-app is the fair setting.
            "    unordered_processing:\n      enabled: true\n"
            f"      batching:\n        count: {self.args.batch_size}\n        period: 100ms\n"
            f"{pipeline}"
            f"output:\n  file:\n    path: {self.out}\n    codec: lines\n"
            "logger:\n  level: ERROR\n"
        )
        self.config = f"{self.out}.{self.tag}.yaml"
        with open(self.config, "w") as f:
            f.write(config)
        self.log_file = open(self.log, "w")
        self.proc = subprocess.Popen([self.args.rpc_bin, "run", self.config],
                                     stdout=self.log_file, stderr=subprocess.STDOUT,
                                     start_new_session=True)

    def cleanup(self):
        super().cleanup()
        if os.path.exists(self.config):
            os.remove(self.config)


class VectorJob(MqbJob):
    """Vector: `kafka` source, optional remap, `file` or `kafka` sink."""

    def start(self):
        if os.path.exists(self.out):
            os.remove(self.out)
        self.work = f"{self.out}.{self.tag}.vector"
        os.makedirs(self.work, exist_ok=True)
        # The same librdkafka option the mq-bridge-app rows set, when they set it.
        extra = json.loads(os.environ.get("MQB_CONSUMER_OPTIONS", "[]"))
        opts = "".join(f'      "{k}": "{v}"\n' for k, v in extra)
        source = (
            "sources:\n  in:\n    type: kafka\n"
            f"    bootstrap_servers: \"{self.args.brokers_host}\"\n"
            f"    group_id: vector_{self.tag}\n"
            f"    topics: [\"{self.args.topic}\"]\n"
            "    auto_offset_reset: earliest\n"
            + (f"    librdkafka_options:\n{opts}" if opts else "")
        )
        if self.args.variant == "passthrough":
            transform, sink_input, codec = "", "in", "text"
        elif self.args.variant == "projection":
            fields = ", ".join(f'"{c}": row.{c}' for c in PROJECTION)
            transform = (
                "transforms:\n  proj:\n    type: remap\n    inputs: [in]\n    source: |\n"
                "      row = parse_json!(string!(.message))\n"
                f"      . = {{{fields}}}\n"
            )
            sink_input, codec = "proj", "json"
        else:
            raise SystemExit(f"vector has no {self.args.variant} variant")
        if self.kafka_sink:
            target = (f"    type: kafka\n    bootstrap_servers: \"{self.args.brokers_host}\"\n"
                      f"    topic: {self.kafka_sink.topic}\n")
        else:
            target = f"    type: file\n    path: {self.out}\n"
        config = (
            f"data_dir: {self.work}\n{source}{transform}"
            f"sinks:\n  out:\n    inputs: [{sink_input}]\n{target}"
            f"    encoding:\n      codec: {codec}\n"
        )
        with open(f"{self.work}/vector.yaml", "w") as f:
            f.write(config)
        self.log_file = open(self.log, "w")
        self.proc = subprocess.Popen([self.args.vector_bin, "-q", "-c", f"{self.work}/vector.yaml"],
                                     stdout=self.log_file, stderr=subprocess.STDOUT,
                                     start_new_session=True)

    def cleanup(self):
        super().cleanup()
        shutil.rmtree(self.work, ignore_errors=True)


class SeaStreamerJob:
    """Official Sea Streamer relay, writing the native `.ss` file format."""

    def __init__(self, args, tag: str):
        self.args = args
        self.out = args.sea_out_file
        self.proc = None
        self.log = f"{self.out}.{tag}.log"
        self.log_file = None
        self.startup = 0.0
        self.tag = tag

    def start(self):
        if os.path.isdir(self.out):
            shutil.rmtree(self.out, ignore_errors=True)
        elif os.path.exists(self.out):
            os.remove(self.out)
        with open(self.out, "a"):
            pass
        cmd = [
            self.args.sea_relay,
            "--input", f"kafka://{self.args.brokers_host}/{self.args.topic}",
            "--output", f"file://{self.out}/bench",
            "--offset", "start",
        ]
        self.log_file = open(self.log, "w")
        self.proc = subprocess.Popen(cmd, stdout=self.log_file, stderr=subprocess.STDOUT, start_new_session=True)

    def is_alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def sink_bytes(self) -> int:
        try:
            return os.path.getsize(self.out)
        except OSError:
            return 0

    def sink_lines(self) -> int:
        result = subprocess.run(
            [self.args.sea_count, "--file", self.out],
            capture_output=True, text=True, check=True,
        )
        return int(result.stdout.strip())

    def stop(self):
        if self.proc and self.proc.poll() is None:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
            self.proc.wait(timeout=30)

    def cleanup(self):
        if self.log_file:
            self.log_file.close()


def container_rss_mib(name: str) -> float:
    out = subprocess.run(
        ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", name],
        capture_output=True, text=True,
    ).stdout.strip()
    if "/" not in out:
        return 0.0
    used = out.split("/")[0].strip()
    m = re.match(r"([\d.]+)\s*([A-Za-z]+)", used)
    if not m:
        return 0.0
    n, unit = float(m.group(1)), m.group(2).upper()
    return n * {"GIB": 1024, "MIB": 1, "KIB": 1 / 1024, "B": 1 / 1048576}.get(unit, 1)


def process_rss_mib(pid: int) -> float:
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    return int(out) / 1024 if out.isdigit() else 0.0


class RssSampler(threading.Thread):
    """Peak RSS while a run is in flight."""

    def __init__(self, job, interval: float = 0.25):
        super().__init__(daemon=True)
        self.job, self.interval = job, interval
        self.peak, self._stop_event = 0.0, threading.Event()

    def run(self):
        while not self._stop_event.is_set():
            if isinstance(self.job, (MqbJob, SeaStreamerJob)):
                if self.job.proc and self.job.proc.poll() is None:
                    self.peak = max(self.peak, process_rss_mib(self.job.proc.pid))
            else:
                name = getattr(self.job, "container", ARROYO_CONTAINER)
                self.peak = max(self.peak, container_rss_mib(name))
            self._stop_event.wait(self.interval)

    def finish(self) -> float:
        self._stop_event.set()
        self.join(timeout=10)
        return self.peak


# --------------------------------------------------------------------------- #
# The shared stopwatch
# --------------------------------------------------------------------------- #

def one_run(args, expect_bytes: int | None):
    """Returns (elapsed_s, startup_s, sink_bytes, peak_rss_mib)."""
    tag = "".join(random.choices(string.ascii_lowercase + string.digits, k=8))
    if args.tool == "arroyo":
        job = ArroyoJob(args, tag)
    elif args.tool == "sea-streamer":
        job = SeaStreamerJob(args, tag)
    elif args.tool == "mqb-docker":
        job = MqbDockerJob(args, tag)
    elif args.tool == "redpanda-connect":
        job = RedpandaConnectJob(args, tag)
    elif args.tool == "vector":
        job = VectorJob(args, tag)
    else:
        job = MqbJob(args, tag)
    sampler = RssSampler(job)
    try:
        job.start()
        sampler.start()
        t0 = time.time()
        deadline = t0 + args.run_timeout

        if expect_bytes is None:
            # Calibration run: no target size yet, so settle on "no growth for 2s"
            # and subtract that idle window rather than billing it as work.
            last, stable_since = -1, None
            while time.time() < deadline:
                if hasattr(job, "is_alive") and not job.is_alive():
                    raise SystemExit(f"{args.label} exited early; see {job.log}")
                b = job.sink_bytes()
                if b == last and b > 0:
                    stable_since = stable_since or time.time()
                    if time.time() - stable_since >= getattr(job, "stable_window", 2.0):
                        break
                else:
                    last, stable_since = b, None
                time.sleep(0.05)
            else:
                raise SystemExit(
                    f"{args.label} exceeded {args.run_timeout}s before output stabilized"
                )
            elapsed = (stable_since or time.time()) - t0
        else:
            while time.time() < deadline:
                if hasattr(job, "is_alive") and not job.is_alive():
                    raise SystemExit(f"{args.label} exited early; see {job.log}")
                if job.sink_bytes() >= expect_bytes:
                    break
                time.sleep(0.05)
            else:
                raise SystemExit(f"{args.label} exceeded {args.run_timeout}s — no usable timing")
            elapsed = time.time() - t0

        peak = sampler.finish()
        job.stop()
        lines = job.sink_lines()
        if lines != args.rows:
            raise SystemExit(f"{args.label} landed {lines} rows != expected {args.rows}")
        return elapsed, job.startup, job.sink_bytes(), peak
    finally:
        sampler.finish()
        job.stop()
        job.cleanup()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--tool", choices=["mqb", "mqb-docker", "arroyo", "sea-streamer", "redpanda-connect", "vector"], required=True)
    p.add_argument("--variant", choices=["passthrough", "projection", "projection-bloblang", "projection-dedup"],
                   default="projection")
    p.add_argument("--label", required=True)
    p.add_argument("--topic", default="bench_src")
    p.add_argument("--brokers", default="kafka:29092", help="brokers as seen from Arroyo")
    p.add_argument("--brokers-host", default="localhost:9092", help="brokers as seen from the host")
    p.add_argument("--bin", default=os.environ.get("BIN", "target/release/mq-bridge-app"))
    p.add_argument("--mqb-image", default=os.environ.get("MQB_IMAGE", "ghcr.io/marcomq/mq-bridge-app:latest"),
                   help="image for --tool mqb-docker")
    p.add_argument("--mqb-file-format", choices=["normal", "raw"], default="raw")
    p.add_argument("--sink", choices=["file", "kafka"], default="file",
                   help="kafka: a per-run topic; sink_bytes then counts records (mqb, vector)")
    p.add_argument("--partitions", type=int, default=4, help="of the --sink kafka topic")
    p.add_argument("--rpc-bin", default=os.environ.get("RPC_BIN", "benches/etl/bin/redpanda-connect"))
    p.add_argument("--vector-bin", default=os.environ.get("VECTOR_BIN", "benches/etl/bin/vector"))
    p.add_argument("--connect-plugin", default=os.environ.get(
        "CONNECT_PLUGIN", "/opt/homebrew/lib/mq-bridge/libmq_bridge_connect.dylib"))
    p.add_argument("--sea-relay", default=os.environ.get("SEA_STREAMER_RELAY", "target/release/sea-streamer-relay"))
    p.add_argument("--sea-count", default=os.environ.get("SEA_STREAMER_COUNT", "target/release/sea-streamer-count"))
    p.add_argument("--sea-out-file", default="/tmp/sea_streamer_kafka_out.ss")
    p.add_argument("--out-file", default="/tmp/mqb_kafka_out.jsonl")
    p.add_argument("--dedup-store", default="/tmp/mqb_kafka_dedup")
    p.add_argument("--batch-size", type=int, default=1024)
    p.add_argument("--rows", type=int, default=1_000_000)
    p.add_argument("--repeats", type=int, default=3)
    p.add_argument("--parallelism", type=int, default=4)
    p.add_argument("--checkpoint-interval", type=float, default=10.0)
    p.add_argument("--startup-timeout", type=float, default=180.0)
    p.add_argument("--run-timeout", type=float, default=900.0)
    p.add_argument("--results", required=True)
    p.add_argument("--keep-output", action="store_true")
    args = p.parse_args()
    if args.sink == "kafka" and args.tool not in ("mqb", "vector"):
        p.error("--sink kafka is implemented for --tool mqb and vector only")

    print(f"-- {args.label}: warmup (also calibrates the expected sink size)", flush=True)
    _, _, expect_bytes, _ = one_run(args, None)
    print(f"   expected sink bytes = {expect_bytes}", flush=True)

    elapsed, startups, peaks = [], [], []
    for i in range(1, args.repeats + 1):
        e, s, _, r = one_run(args, expect_bytes)
        print(f"  {args.label} run {i}: {e:.3f}s "
              f"({int(args.rows / e)} rows/s, startup {s:.2f}s, peak {r:.0f} MiB)", flush=True)
        elapsed.append(e)
        startups.append(s)
        peaks.append(r)

    median = statistics.median(elapsed)
    stddev = statistics.pstdev(elapsed) if len(elapsed) > 1 else 0.0
    row = (f"{args.label},{args.rows},{args.repeats},{median:.3f},{stddev:.3f},"
           f"{int(args.rows / median)},{statistics.median(startups):.3f},"
           f"{expect_bytes},{max(peaks):.0f}")
    print(row)

    header = ("tool,rows,repeats,median_elapsed_s,stddev_elapsed_s,median_rows_per_s,"
              "startup_s,sink_bytes,peak_rss_mib")
    if results_dir := os.path.dirname(args.results):
        os.makedirs(results_dir, exist_ok=True)
    lines = []
    if os.path.exists(args.results):
        with open(args.results) as f:
            lines = [ln for ln in f.read().splitlines()
                     if ln and not ln.startswith(args.label + ",")]
    if not lines or not lines[0].startswith("tool,"):
        lines = [header] + lines
    lines.append(row)
    with open(args.results, "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    sys.exit(main())
