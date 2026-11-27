# Custom endpoints

When a protocol isn't built in, you can plug your own endpoint into the engine. A
custom endpoint is selected from config by name and delegates to a factory you register
programmatically before starting routes.

## Config

Name the registered factory as the endpoint key; everything under it is its config:

```yaml
output:
  my_sink: { target: "internal://thing" }
```

The explicit form is the same endpoint. Use it if you validate configs against
`mq-bridge.schema.json`, which does not know your key:

```yaml
output:
  custom:
    name: "my_sink"
    config: { target: "internal://thing" }
```

| Field | Type | Required |
|---|---|---|
| `name` | string | yes — matches the registered factory |
| `config` | any JSON | yes — passed through to your factory |

Either form works as a route `input` or `output`.

## Implementing it

Implement the `CustomEndpointFactory` trait and register your type before starting
routes. The factory receives the `config` JSON and builds an endpoint that speaks the
engine's `MessageConsumer` / `MessagePublisher` traits (in `mq_bridge::traits`), so
your endpoint participates in batching, middleware, and ack/nack exactly like a
built-in one.

Registration is process-global and keyed by name: it must happen before any route that
names it starts, and registering a name twice is an error rather than a silent
replacement, so each factory needs its own name. Python and Node.js can register a
factory directly, with the same semantics — no Rust required.

## Testing a plugin against the delivery contract

A plugin endpoint runs the same delivery checks as the built-in brokers. They cover nack,
crash recovery, mixed acks, metadata, backpressure, route stop, order, a refusing sink,
parallel sends with rejects, reconnects, batch sizes, writes before a consumer exists,
repeated open and close, payload fidelity and a large backlog.

With the `test-utils` feature, call the catalog from a test in the plugin's own repository:

```rust,ignore
use mq_bridge::test_utils::contract::Contract;
use serde_json::json;

#[tokio::test]
#[ignore = "requires the broker"]
async fn my_endpoint_contract() {
    let endpoint = json!({ "custom": { "name": "my", "config": { "queue": "$ID" } } });
    Contract::new("my-plugin", endpoint.clone(), endpoint)
        .run()
        .await
        .assert_passed();
}
```

`$ID` gives each check its own queue, so the checks run in parallel. Without it they run one
after another on the shared queue. The report lists every check as `ok`, `FAILED` or `skipped`.

A shared queue must be a dedicated test queue. With `disposable_input()`, or
`"disposable_input": true` in `MQB_TEST_CONTRACT`, the sink-refusal check first reads and acks
everything the queue holds, so that a leftover cannot hide its message. Without it, nothing is
drained, and a leftover from an earlier run can fail that check.

Declare only what the transport does not promise: `unordered()`, `unstable_id()`,
`id_not_kept()`, `no_metadata()`, `json_payloads()`, or `skip(check, reason)`. Use
`setup` to create a queue and `expire_locks` to end a broker's lease early. A target without
a consumer uses `Contract::sink_only(name, output, read_back)`.

From this repository, the same run needs no code. `MQB_TEST_PLUGIN` lists the libraries to
load, separated like `PATH`, and `MQB_TEST_CONTRACT` describes the endpoint:

```sh
MQB_TEST_PLUGIN="$PWD/target/debug/libmy_plugin.dylib" \
MQB_TEST_CONTRACT='{
  "name": "my-plugin",
  "compose": "nats",
  "input":  { "custom": { "name": "my_source", "config": { "url": "nats://localhost:4222", "queue": "$ID" } } },
  "output": { "custom": { "name": "my_sink", "config": { "url": "nats://localhost:4222", "queue": "$ID" } } }
}' \
cargo test --test integration_test --features full,test-utils -- \
  --ignored --nocapture --exact integration::contract::test_plugin_contract
```

`compose` names a file in `tests/integration/docker-compose/` without its extension. Leave
it empty if the endpoint needs no service. If the endpoint does not promise a property, set
`stable_id`, `hop_keeps_id`, `keeps_metadata` or `ordered` to `false`. Set `json` to `true`
for an output that accepts only JSON.

### Process scenarios

SIGTERM, `kill -9` and a broker restart need a real process. The CLI suite runs them against
any endpoint the binary can load. `MQB_TEST_BIN` selects another `mqb` build, such as an older
release:

```sh
cd apps/mq-bridge-app
MQB_TEST_PLUGIN="/path/to/libmy_plugin.dylib" MQB_TEST_BACKEND=external \
MQB_TEST_TARGET='{
  "compose": "/abs/path/compose.yml",
  "service": "broker",
  "input":  { "custom": { "name": "my", "config": { "queue": "$ID" } } },
  "output": { "custom": { "name": "my", "config": { "queue": "$ID" } } },
  "input_uri": "my://localhost?queue=$ID",
  "output_uri": "my://localhost?queue=$ID"
}' \
cargo test -p mq-bridge-app --test cli_scenario_docker_test --features full -- \
  --ignored --nocapture --test-threads=1 external
```

Start the compose service first. Without `input_uri` and `output_uri` the `kill -9` scenario
is skipped, because `mqb copy` takes endpoints as URIs.

## See also

- [`custom` (endpoint) reference](../engine/reference.md#custom-endpoint) — the authoritative field list.
- [Writing endpoints & middleware](../engine/extending.md) — the trait, registration, and the Python/Node equivalents.
- [Native plugins](plugins.md) — the same endpoint shipped as a loadable library instead.
- [Custom middleware](custom-middleware.md) — the middleware equivalent.
