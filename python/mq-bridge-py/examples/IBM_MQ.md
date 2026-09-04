# IBM MQ from Python

Companion notes for [`ibm-mq-input.py`](ibm-mq-input.py), which consumes an IBM
MQ queue and hands every message to a Python callable.

IBM MQ is the only endpoint whose native client `mq-bridge` cannot ship or build
for you. IBM does not redistribute it under a licence that allows that, and it
is not on conda-forge, so the client has to be installed separately. Nothing
proprietary is vendored into this repository: no IBM headers, libraries or
archives are checked in, and building the bindings never needs them.

Everything else is ordinary Rust underneath — the wire work is done by the
[`mqi`](https://crates.io/crates/mqi) and
[`libmqm-sys`](https://crates.io/crates/libmqm-sys) crates — so the only two
things you have to get right from Python are **installing the client** and
**letting the interpreter find it**.

## The endpoint is already in your wheel

You do not need a special build. `mq-bridge/full` and `mq-bridge/full-dynamic`
both include the engine's `ibm-mq` feature, which covers:

- the conda-forge `mq-bridge-py` package,
- upstream's `full` PyPI wheel.

The `mq-bridge-py-basic` and reduced wheels do **not** include it — they exist
for targets without a full C toolchain, and IBM MQ is on their excluded list.

That is because `ibm-mq` costs nothing to compile in: with no client installed
the extension still builds and imports fine, and only a route that actually
opens an IBM MQ endpoint fails. There is a second feature, `ibm-mq-static`,
which binds the client at link time and is therefore *not* in any published
wheel; see [Other ways in](#other-ways-in) if you need it.

A note on that name: **`ibm-mq-static` does not statically link anything.**
`libmqm_r` is a shared object either way — IBM ships no static archive. The
difference is *when* it is resolved, at process start versus on first connect.

| | `ibm-mq` (what wheels ship) | `ibm-mq-static` |
| --- | --- | --- |
| IBM client needed to **build** | no | **yes** |
| IBM client needed to **run** | only if a route uses an IBM MQ endpoint | always, or the process will not start |
| How it is found | `dlopen` on first connect | `DT_NEEDED`, resolved by the loader at startup |
| Missing client shows up as | a non-retryable error on that one route | the interpreter failing to load the extension |

## Installing the client

The redistributable client is a self-contained tree and needs no root to use,
only somewhere to unpack it. Pick the archive for your platform from
[IBM's redistributable client directory](https://public.dhe.ibm.com/ibmdl/export/pub/software/websphere/messaging/mqdev/redist/).

`mq-bridge` builds against capability level **9.3.0.0** — 9.2 is end of life,
and 9.3 is what makes the password-protected key repositories described below
work — so install 9.3.0.0 or later.

```sh
# Linux x86-64. This is what .github/workflows/ibm-mq.yml does.
curl -fsSLO https://public.dhe.ibm.com/ibmdl/export/pub/software/websphere/messaging/mqdev/redist/9.3.0.0-IBM-MQC-Redist-LinuxX64.tar.gz
sudo mkdir -p /opt/mqm
sudo tar -xzf 9.3.0.0-IBM-MQC-Redist-LinuxX64.tar.gz -C /opt/mqm
export MQ_INSTALLATION_PATH=/opt/mqm
```

`/opt/mqm` is only a convention. Any readable directory works as long as
`MQ_INSTALLATION_PATH` points at it.

### How the interpreter finds it

The bindings do not resolve the client themselves; they inherit the engine's
loader, which tries these in order and takes the first that loads:

1. `$MQB_IBM_MQ_LIB` — a full path to the library file. Use this when the
   install does not follow the usual layout.
2. `$MQ_INSTALLATION_PATH/lib64/<lib>` then `$MQ_INSTALLATION_PATH/lib/<lib>`.
3. the bare library name, leaving it to the platform's search path
   (`LD_LIBRARY_PATH`, `DYLD_LIBRARY_PATH`, `PATH` on Windows).

`<lib>` is `libmqm_r.so` on Linux, `libmqm_r.dylib` on macOS and `mqm.dll` on
Windows.

These are read from the process environment, so **export them before starting
the interpreter**. Setting `os.environ[...]` at the top of your script also
works, because the load is lazy and does not happen until a route connects —
but a `.env` file does not, since that is an `mq-bridge-app` feature rather than
a bindings one.

**`MQ_HOME` is not consulted.** It is read by the crate's `build.rs`, and only
on the `ibm-mq-static` path. Setting it alone does nothing here.

## Running the example

```sh
export MQ_INSTALLATION_PATH=/opt/mqm
export MQ_PASSWORD=…
python ibm-mq-input.py
```

The route body is a plain dict with the same two keys as the YAML config:

```python
route_config = {
    "input": {
        "ibmmq": {
            "url": "mq1.example.com(1414),mq2.example.com(1414)",
            "queue_manager": "QM1",
            "channel": "DEV.APP.SVRCONN",
            "queue": "DEV.QUEUE.1",
            "username": "app",
            "password": os.environ["MQ_PASSWORD"],
        }
    },
    "output": "null",
}
```

Four things that catch people out:

- The import is **`mq_bridge`**. `mq-bridge-py` is the distribution name, not
  the module name.
- The endpoint key is **`ibmmq`** — one word, no underscore.
- `url` is IBM's `host(port)` form, not a URI scheme. A comma-separated list
  gives client-side failover.
- A handler's **return value is the published message**: bytes, a `str`, a
  `dict` or a `mq_bridge.Message` go to the output endpoint, while `None`
  acknowledges without publishing. Raise `mq_bridge.RetryableError` for
  redelivery, `mq_bridge.NonRetryableError` to fail the message.

Set `topic` instead of `queue` for publish/subscribe; a consumer with `topic`
runs in subscriber mode. `queue` defaults to the route name if both are omitted.
See [`docs/REFERENCE.md`](../../../docs/REFERENCE.md) for every field.

`Route.from_config` accepts a bare route body like the one above, or a whole
config plus a route name. `run()` blocks until `stop()` is called; `start()`
deploys on a background thread and returns immediately, with `join()` to wait.

### Deciding up front whether the client is there

Because the load is lazy, a missing client is only discovered when a route
connects. If you would rather skip a subsystem or fail startup with your own
message, the engine exposes the same probe the endpoint uses — it is available
through the Rust crate rather than the Python API, so from Python the practical
equivalent is to deploy the route and handle the error below.

## What a missing client looks like

The route fails on first connect, and the error is classified non-retryable, so
it fails fast instead of reconnecting forever:

```
failed to load IBM MQ client library (tried ["/opt/mqm/lib64/libmqm_r.so",
"/opt/mqm/lib/libmqm_r.so", "libmqm_r.so"]; last error: …). Install the IBM MQ
redistributable client and ensure it is on the library search path, or set
MQB_IBM_MQ_LIB to the full path of the libmqm_r library.
```

It is raised out of the route, not at import time. Read the list: it is every
path that was tried, in order.

- A short list usually means neither `MQB_IBM_MQ_LIB` nor
  `MQ_INSTALLATION_PATH` reached the process — common when the interpreter is
  started by a supervisor that does not inherit your shell environment.
- A populated list with a "wrong ELF class" or "no suitable image found" error
  means the client is there but the wrong architecture.

Other routes in the same process are unaffected; only the IBM MQ one stops.

## TLS

IBM MQ does not consume PEM files, so the `tls` block keeps the generic field
*names* for config parity but carries MQ-native meaning:

- `tls.cert_file` (alias `key_repository`) is a **CMS key repository stem**, not
  a PEM file: `/path/to/tls` refers to `/path/to/tls.kdb`.
- The repository is either passwordless, backed by a `.sth` stash file beside
  the `.kdb`, or password-protected via `tls.cert_password` (alias
  `key_repository_password`), which needs a client and server at 9.3.0.0+.

`tests/integration/docker-compose/ibm-mq-certs/` holds a throwaway repository
(`client.kdb` + `client.sth`) used by the integration tests. Those are test
fixtures, not redistributed IBM software.

## Other ways in

- **`mq-bridge-app`** — the endpoint is compiled into any build made with
  `full`, `full-dynamic` or `full-static-ibm-mq`, and routes are declared in
  YAML rather than in code. See
  [`IBM_MQ_SETUP.md`](../../../apps/mq-bridge-app/dev/docs/IBM_MQ_SETUP.md) for
  the install, the `/features` check and `mqb copy` one-off drains.
- **The `mq-bridge` crate** — `features = ["ibm-mq"]` for the dlopen build, or
  `features = ["ibm-mq-static"]` to bind the client at link time, which then
  requires the IBM SDK at build. Neither needs a `link-static` /
  `link-dynamic` companion: those two select how librdkafka and SQLite are
  obtained, and IBM MQ is not part of that choice. Enabling `ibm-mq` alongside
  `ibm-mq-static` forces the link-time path.

  On `ibm-mq-static`, `build.rs` adds `$MQ_INSTALLATION_PATH/lib64` (or `lib` on
  32-bit) to the link search path and records it as an rpath, so the binary
  finds `libmqm_r` without `LD_LIBRARY_PATH`. It falls back to `MQ_HOME`, then
  `/opt/mqm`. The dlopen build gets no rpath, because it never consults the link
  search path.

## Verifying an install

```sh
# Does the client load at all? Ignored by default, so name it explicitly.
MQ_INSTALLATION_PATH=/opt/mqm cargo test --no-default-features \
  --features ibm-mq -- --ignored loads_client
```

The full IBM MQ integration suite runs against a containerised queue manager;
see `tests/integration/docker-compose/ibm_mq.yml` and the `ibm-mq` job in
`.github/workflows/ibm-mq.yml`. Those tests skip themselves when no client is
present, which is why a normal `cargo test` passes on a machine with no IBM
software at all.

## Redistribution

IBM's client is redistributable under IBM's own terms, not under this project's
MIT licence. If you ship an application together with the client, include IBM's
licence files from `$MQ_INSTALLATION_PATH/licenses` and follow IBM's
redistribution conditions. Shipping `mq-bridge-py` alone carries no such
obligation: the wheel contains no IBM code.
