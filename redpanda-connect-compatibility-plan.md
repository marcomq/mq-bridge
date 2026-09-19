# Redpanda Connect compatibility plugin for mq-bridge

Decision report and implementation plan — 6 September 2026

## Executive decision

**Verdict: PROTOTYPE ONLY.**

A useful connector-only compatibility layer is technically feasible without running a full Redpanda Connect stream. The current supported API is Benthos `public/service.ResourceBuilder`: register selected Connect component packages, add one labeled input or output as YAML, build the resource manager, and invoke `ResourceInput.ReadBatch` or `ResourceOutput.WriteBatch`. This leaves routing, middleware, transformations, retry policy, and pipeline ownership in mq-bridge.

The recommended proof of concept is a Rust mq-bridge `cdylib` using the existing plugin SDK plus a separately packaged Go `c-shared` library. The Rust layer retains mq-bridge's tested ABI, runtime, handle, and panic boundaries; the Go layer owns Redpanda component registration and lifecycle. A private fixed-width batch ABI connects the two with one FFI call per batch.

This should not be declared production-ready yet. Three release gates remain:

1. Redpanda input acknowledgement is whole-batch, while mq-bridge records per-message dispositions; any nack must safely nack the entire source batch and can duplicate successful siblings.
2. Redpanda `BatchError` can represent partial output success, while mq-bridge plugin ABI v1 reports output batches all-or-nothing; retrying can duplicate writes that already succeeded.
3. The aggregate `public/bundle/free` has a large dependency surface and contradictory license headers in some packages imported through `community`. Legal provenance and redistribution must be resolved at the exact pinned version before distributing a broad bundle.

None is a blocker to a narrow, non-production prototype. The first prototype should import only two or three audited connector categories, prove packaging on all three operating-system families, quantify duplicates and copy costs, and compare against simply running Redpanda Connect as a process.

## Research basis and assumptions

The repository was inspected before designing. Source snapshots were:

- mq-bridge `dev` at `c789935f446b5b0f64a87f0b518955effbc492f0` (2026-09-06).
- Redpanda Connect at `996be69ef10ba51640530a029d54cd606715ac06`, tagged `v4.108.0` (2026-09-04).
- Benthos at `a3bfd1ce941d3136aea407abb44c6286fb9fb07f`, tagged `v4.79.0` (2026-09-03).
- The independently versioned Connect free-bundle module at that Connect commit pins Connect `v4.107.2` and Benthos `v4.78.0`. The recommended component APIs were also checked at the pinned Benthos `v4.78.0` commit `8791a124b6bc86d3a886925c6efb38adb05d1d85` and have the same relevant signatures.

Assumptions:

- Compatibility means connector construction and I/O, not behavioral identity with a complete Connect pipeline.
- At-least-once delivery and possible duplicates are acceptable when explicitly documented; exactly-once behavior is not promised.
- The optional plugin can ship as two sibling dynamic libraries for the prototype.
- Connector configuration may remain raw JSON/YAML, but mq-bridge's route file remains JSON/YAML parsed by mq-bridge.
- License observations are engineering findings, not legal advice.

## 1. Exact source files and APIs inspected

### mq-bridge

- [`docs/PLUGINS.md`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/docs/PLUGINS.md#L1-L125), including ABI v1 limitations at [L189-L228](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/docs/PLUGINS.md#L189-L228) and compatibility rules at [L288-L303](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/docs/PLUGINS.md#L288-L303).
- [`docs/EXTENDING.md`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/docs/EXTENDING.md#L1-L238), including verbatim custom JSON configuration at [L52-L67](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/docs/EXTENDING.md#L52-L67).
- [`src/canonical_message.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/canonical_message.rs#L20-L26): `CanonicalMessage { message_id, payload: Bytes, metadata: HashMap<String,String> }`.
- [`src/traits.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/traits.rs#L22-L28): `MessageDisposition::{Ack, Reply, Nack}`; `MessageConsumer` at [L261-L391](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/traits.rs#L261-L391); `MessagePublisher` at [L393-L513](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/traits.rs#L393-L513); and `CustomEndpointFactory` at [L600-L629](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/traits.rs#L600-L629).
- [`src/support/plugin_abi.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/support/plugin_abi.rs#L67-L69), ABI 1.0; slices, metadata and messages at [L134-L230](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/support/plugin_abi.rs#L134-L230); vtable and batch calls at [L287-L379](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/support/plugin_abi.rs#L287-L379).
- [`src/plugin/message.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/plugin/message.rs), [`src/plugin/endpoint.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/plugin/endpoint.rs#L194-L232), and [`src/plugin/sdk.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/plugin/sdk.rs#L49-L62), covering buffer ownership, blocking ABI calls, batch-handle lifetime, runtime isolation, error conversion, and ABI v1 partial-output collapse.
- [`src/plugin/conformance.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/plugin/conformance.rs), including metadata round-trip, nack redelivery, and uncommitted-batch checks.
- [`tests/fixtures/plugin-fixture/src/lib.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/tests/fixtures/plugin-fixture/src/lib.rs), the existing endpoint plugin example.
- [`src/models.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/models.rs#L327-L332) and [`src/models/secrets.rs`](https://github.com/marcomq/mq-bridge/blob/c789935f446b5b0f64a87f0b518955effbc492f0/src/models/secrets.rs#L170-L262). Custom configuration is an arbitrary JSON value, but it currently falls through the generic secret extractor.

### Benthos public service API

- [`public/service/resource_builder.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/resource_builder.go#L232-L276): `AddInputYAML` and `AddOutputYAML`; `Build` at [L324-L354](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/resource_builder.go#L324-L354).
- [`public/service/resource_builder_test.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/resource_builder_test.go#L21-L106), which directly exercises resource inputs and outputs without a stream.
- [`public/service/resources.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/resources.go#L240-L267): `AccessInput` and `AccessOutput`.
- [`public/service/input.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/input.go#L15-L99): `BatchInput`, `AckFunc`; `ResourceInput.ReadBatch` at [L187-L230](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/input.go#L187-L230).
- [`public/service/output.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/output.go#L50-L75): `BatchOutput`; `ResourceOutput.WriteBatch` at [L148-L196](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/output.go#L148-L196).
- [`public/service/environment.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/environment.go#L281-L336): component enumeration/configuration; output equivalents at [L369-L438](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/environment.go#L369-L438).
- [`public/service/plugins.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/plugins.go#L80-L152): public constructor registration, but no public constructor retrieval.
- [`public/service/config_docs.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/config_docs.go#L17-L115) and [`config.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/config.go#L501-L534): public documentation data and experimental configuration JSON.
- [`public/service/message.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/message.go#L245-L435): bytes, structured messages, metadata, and metadata coercion.
- [`public/service/errors.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/public/service/errors.go#L14-L166): `ErrNotConnected`, `ErrEndOfInput`, `ErrBackOff`, and `BatchError`.
- [`internal/component/output/async_writer.go`](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/internal/component/output/async_writer.go#L124-L203): max-in-flight workers; reconnect handling at [L288-L331](https://github.com/redpanda-data/benthos/blob/a3bfd1ce941d3136aea407abb44c6286fb9fb07f/internal/component/output/async_writer.go#L288-L331). This was inspected to understand behavior, not proposed as an import.

### Representative Redpanda Connect components

- Batch input: [`internal/impl/aws/s3/input.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/aws/s3/input.go#L261-L335), including SQS aggregate acknowledgement at [L618-L695](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/aws/s3/input.go#L618-L695) and batch forwarding at [L914-L968](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/aws/s3/input.go#L914-L968).
- Batch output: [`internal/impl/elasticsearch/v8/output.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/elasticsearch/v8/output.go#L297-L403), which produces per-index `service.BatchError` results.
- Simple input/output: [`internal/impl/beanstalkd/input.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/beanstalkd/input.go#L28-L118) and [`output.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/beanstalkd/output.go#L27-L114).
- Resource-dependent input: [`internal/impl/cockroachdb/input_changefeed.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/cockroachdb/input_changefeed.go#L44-L203), with cache-backed cursor acknowledgement at [L272-L398](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/cockroachdb/input_changefeed.go#L272-L398).
- Context-sensitive example: [`internal/impl/kafka/franz_reader_ordered.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/internal/impl/kafka/franz_reader_ordered.go#L277-L322).
- Bundle composition: [`public/bundle/free/package.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/public/bundle/free/package.go#L15-L23), [`public/components/community/package.go`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/public/components/community/package.go#L15-L85), and [`public/bundle/free/go.mod`](https://github.com/redpanda-data/connect/blob/996be69ef10ba51640530a029d54cd606715ac06/public/bundle/free/go.mod#L1-L5).

## 2. Feasibility and supported construction API

### Answer

Yes: connector-only embedding is feasible. `ResourceBuilder`, not `StreamBuilder`, is the cleanest supported construction API.

The Go bridge should blank-import only the selected public component packages, synthesize a unique label plus the selected connector configuration, and then:

```go
builder := service.NewResourceBuilder()
err := builder.AddInputYAML(inputYAML) // or AddOutputYAML
resources, closeResources, err := builder.Build()

err = resources.AccessInput(ctx, label, func(in *service.ResourceInput) {
    batch, ackFn, readErr = in.ReadBatch(ctx)
})
```

The output path uses `AccessOutput` and `WriteBatch`. Access is callback-scoped; the adapter must not retain a `ResourceInput` or `ResourceOutput` after the callback.

The alternatives rank as follows:

| Path | Finding | Decision |
|---|---|---|
| A. Component-only public API | `ResourceBuilder` constructs and owns labeled connector resources, lifecycle, connection wrappers, batching wrappers, and max-in-flight workers. | **Use this.** |
| B. Minimal `StreamBuilder` | Supported, using a custom producer/consumer at the opposite side, but instantiates stream/pipeline machinery that mq-bridge already owns. | Keep only as a diagnostic fallback; do not ship it unless ResourceBuilder is removed upstream. |
| C. Constructor adapter | Constructor types are public for registration, but the environment does not expose public constructor getters. `ConfigSpec.ParseYAML` is documented as test-oriented. | Do not build around it. |
| D. Internal imports/fork | Go's `internal` rule prevents an external module from importing Connect implementations directly; a fork would create tight version coupling. | Reject. |

If a future Redpanda version removes or destabilizes ResourceBuilder, prefer subprocess Connect over importing internals or silently embedding StreamBuilder. That keeps the framework boundary honest.

### Configuration shape

The existing custom endpoint syntax is sufficient and should be used rather than adding a first-class core endpoint:

```yaml
output:
  custom:
    name: redpanda
    config:
      connector: aws_s3
      config:
        bucket: example
        path: '${! meta("object_key") }'
```

`connector` is clearer than an overloaded `type`. The plugin validates that the name was registered for the requested direction, marshals the nested object to YAML, injects a private unique `label`, and passes the resulting component document to `AddInputYAML` or `AddOutputYAML`. Raw JSON is structurally safe because it is a YAML subset; use a YAML library rather than string concatenation.

Initial scope should reject pipeline keys (`processors`, `buffer`, `input`, `output`, stream definitions, HTTP management) and non-default connector `batching`. A later narrowly scoped `resources` object can be passed to `ResourceBuilder.AddYAML` for connectors such as CockroachDB changefeed that genuinely require named resources. It must not become an escape hatch for a second pipeline runtime.

### Discovery and schemas

`Environment.WalkInputs` and `WalkOutputs` can enumerate registered names. `ConfigView.TemplateData` provides descriptions, fields, defaults, options, status, support level, and version-oriented documentation data. `ConfigView.FormatJSON` gives fuller configuration JSON but is explicitly experimental. Redpanda's authoritative certified/community/enterprise metadata lives under Connect `internal/plugins`, not a public API.

For v1, provide a companion `mq-bridge-redpanda catalog --json` command or generate a pinned catalog at build time. Do not duplicate hundreds of schemas manually. Dynamic UI discovery through mq-bridge would require a general plugin-metadata extension because the current vtable exposes identity/capabilities, not arbitrary endpoint schemas. That extension is optional and should follow the prototype, not block it.

## 3. Hard blockers and material limitations

There is no hard technical blocker to a proof of concept. The following block a production release until resolved:

- **Redistribution provenance:** the broad free/community import graph contains conflicting source headers; exact linked source and notices require automated audit plus legal/upstream confirmation.
- **Platform proof:** Go-in-a-dynamically-loaded-plugin behavior must be exercised on Linux, macOS, and Windows/MSVC. Static archive into a dlopened library is not safe to assume.
- **Semantic disclosure:** whole-batch input ack and all-or-nothing output reporting must be accepted as an at-least-once compatibility contract, with duplicates measured and documented.

Limitations that are acceptable if explicit:

- Generic Redpanda errors do not carry a universal permanent/retryable classification.
- Structured values, typed metadata, and private message contexts are lossy in the minimal bridge.
- `receive_batch(max_messages)` may be smaller than a source batch; the adapter needs a safe chunk/ack aggregator.
- Some connectors require resources, native libraries, platform-specific build tags, credentials, or external runtimes.
- Custom endpoint config is not currently handled by mq-bridge's secret extractor, so the prototype must use environment/file references and must not encourage inline secrets.

## 4. Recommended architecture

```text
mq-bridge
  └─ existing native ABI 1.0
      └─ Rust cdylib: mq-bridge-redpanda
          ├─ mq-bridge plugin SDK, async/runtime and ABI ownership
          ├─ CanonicalMessage/disposition translation
          └─ private batch C ABI, dynamically resolved
              └─ Go c-shared sibling library
                  ├─ Benthos ResourceBuilder/Resources
                  └─ curated Redpanda public component imports
```

The private Rust/Go ABI should use C-owned or Rust-owned fixed-width descriptors and opaque `uintptr_t`/`uint64_t` handles. Go's `runtime/cgo.Handle` is appropriate for Go object handles. Never retain a Go pointer, Go slice, map, string, or interface in C/Rust memory; the cgo pointer rules prohibit that. Every exported Go entry point should recover ordinary panics and return a status plus an allocated error string through explicit matching free functions.

One call crosses Rust→Go for an output batch. One call crosses Go→Rust for an input batch descriptor. Ack is a separate batch-handle call made only when mq-bridge commits or abandons the batch.

### Integration option matrix

| Option | Linux | macOS | Windows/MSVC | Size/runtime | Reliability and maintenance | Verdict |
|---|---|---|---|---|---|---|
| A. Rust cdylib + Go `c-shared` sibling | Intended Go build mode; load by absolute sibling path or `$ORIGIN`. | Supported; use `@loader_path`, sign/notarize both artifacts. | Supported in Go, but cgo uses a GCC/MinGW C toolchain; dynamically resolving a sibling DLL avoids relying on MSVC/MinGW import-library compatibility. Native CI required. | Two artifacts; one embedded Go runtime; Connect dependencies dominate size. | Preserves mq SDK behavior and creates a small private ABI. Same-process fatal Go/runtime/native failures can still kill mq-bridge. | **Recommended POC.** |
| B. Go `c-archive` linked into Rust cdylib | Mechanically possible and Go emits PIC on relevant ELF targets, but Go issue [#48596](https://github.com/golang/go/issues/48596) demonstrates a static-TLS failure when such an archive is linked into a dlopened shared object. Retest current toolchains; do not assume fixed. | Feasible in principle; toolchain, symbol, initialization, and signing tests required. | Weakest path because cgo and Rust MSVC toolchains differ. | One file, but similar total code size and same Go runtime. | More linker/runtime coupling, harder diagnostics, no crash isolation. | Prototype experiment only after A works; not the default. |
| C. Implement mq native ABI directly in Go `c-shared` | Buildable. | Buildable. | Buildable with cgo caveats. | Potentially one artifact, slightly less Rust code; Connect still dominates. | Must reimplement the mq vtable, buffer pairing, status mapping, handles, concurrency, close, dropped-batch behavior, panic containment, and future ABI changes already handled by the Rust SDK. | Reject unless two artifacts are an absolute product blocker. |
| D. Redpanda subprocess + IPC | Straightforward and isolated. | Straightforward and isolated. | Straightforward but service/process packaging differs. | Separate executable plus serialization and IPC overhead. | Best crash isolation and least toolchain mixing; running Connect directly may already provide the same value. | Operational fallback and likely better choice for broad/full compatibility. |

All in-process options initialize a Go runtime that creates threads, participates in signal handling, and runs GC. The plugin should be loaded once and never unloaded; mq-bridge already retains loaded plugins. Initially allow only one Go-runtime compatibility plugin per process because unresolved macOS issue [#65050](https://github.com/golang/go/issues/65050) reports corruption with multiple Go `c-shared` runtimes. `recover` can contain ordinary panics only when invoked by deferred code in the same goroutine; fatal runtime errors, OOM, or native crashes remain process-fatal.

Cross-compilation with cgo is not a dependable release strategy: it is disabled by default for cross builds and needs a target C compiler/sysroot. Use native runners for each target/architecture.

## 5. Message and metadata mapping

| mq-bridge | Redpanda `service.Message` | Loss/constraint |
|---|---|---|
| `payload: Bytes` | `service.NewMessage([]byte)` / `AsBytes()` | Raw bytes map exactly. `AsBytes()` JSON-serializes a structured-only message, so representation/type can be lost. |
| `metadata: HashMap<String,String>` | `MetaSet` / `MetaWalk` | String metadata maps exactly. Redpanda metadata values may be arbitrary `any`; walking into mq coerces values to strings. S3, for example, emits an integer timestamp. |
| `message_id: u128` | No generic native field | Keep the mq ID on the Rust side. Do not pollute connector metadata by default; an opt-in reserved metadata key could be added later. |
| No structured value | Redpanda structured value | Use bytes only in v1. JSON serialization is acceptable for compatibility but not lossless structured fidelity. |
| No arbitrary Go context | `Message.Context()` | Do not try to serialize it. Connector-private callbacks/state can be lost; ordered Kafka uses context to improve dispatch pipelining, so affected connector behavior must be tested or excluded. |

Headers, keys, topics, object paths, and similar connector concepts are commonly represented as metadata and/or interpolated configuration fields, so preserving every UTF-8 key/value is the right minimal rule. This is a broad compatibility inference, not a claim that every connector uses identical metadata conventions.

### Copies and allocations

Safe conservative prototype behavior:

- **Input:** the Go connector produces Go-owned bytes; the private ABI copies bytes/metadata to Rust-owned `CanonicalMessage`; mq's native plugin boundary then copies them into the host. That is two payload copies after the connector, plus string/map allocations.
- **Output:** mq's host→plugin SDK conversion already copies payload and metadata into plugin-owned Rust values. The Go bridge should initially copy them into Go-owned messages before `WriteBatch`. A synchronous borrowed view might eliminate that last payload copy later, but only after cgo lifetime and connector-retention tests prove no reference escapes the call.
- One FFI call per whole batch is practical; never cross once per message.

Copy reduction is an optimization target, not a prerequisite. Metadata allocation and downstream client serialization may dominate before the C-call overhead does.

## 6. Ack/nack semantic mapping

Benthos `AckFunc` acknowledges or rejects the whole batch. mq-bridge can record a disposition per message. The only generally safe reduction is:

| mq outcome for the Redpanda source batch | Call into Redpanda |
|---|---|
| Every message is `Ack` or `Reply` | `ack(ctx, nil)` |
| Filtered or deliberately dropped message | Resolve it as mq `Ack`, therefore contributes success. There is no separate Redpanda drop state. |
| Any message is `Nack` | `ack(ctx, bridgeNackError)` once for the whole original source batch. |
| Route/publish retry eventually succeeds | Do not call AckFunc until the mq commit callback resolves; then apply the aggregate rule. |
| Connection failure downstream | Usually produces mq `Nack`; aggregate to Redpanda error. |
| Shutdown or batch dropped before commit | A Rust RAII guard must invoke the Go ack handle exactly once with a cancellation/nack error during `batch_free`/drop. Never rely on a Go finalizer. |
| AckFunc itself returns an error | Return a retryable commit error to mq. Ack side effects may be ambiguous, so duplicates remain possible. |

`Reply(CanonicalMessage)` maps to source `Ack` because plugin ABI v1 intentionally collapses reply to ack. If a Redpanda source batch is divided to honor `receive_batch(max_messages)`, all chunks must share an aggregator that retains the original AckFunc. It calls AckFunc only after every chunk is resolved; one nack or abandoned chunk rejects the original batch. This obeys mq's maximum batch contract but increases retention and can delay source acknowledgement. For the POC, also cap/configure connector-native batch sizes where available.

Duplicates are unavoidable in partial failure. If messages 1–99 succeed and message 100 nacks, Redpanda receives one batch error and may redeliver all 100. This matches S3/SQS aggregation and `AutoRetryNacksBatched` behavior: successful objects/items can be replayed when the source acknowledgement unit is broader than the mq disposition unit. The plugin must document at-least-once behavior and recommend idempotent destinations/deduplication.

Inputs differ in side effects:

- Beanstalkd deletes a job on successful ack and releases it on nack.
- S3 can delete an object only after success; SQS mode aggregates all object outcomes into the source notification and changes visibility on error.
- Some batch inputs wrap themselves with Benthos auto-retry, holding/replaying a failed batch before a new source read.

Therefore the adapter must treat AckFunc as opaque and exactly-once callable. It must not attempt connector-specific acknowledgement shortcuts.

## 7. Output and error mapping

| Go/Benthos result | mq plugin status/result | Rationale |
|---|---|---|
| `WriteBatch` returns nil | `SentBatch::Ack` | Entire reported batch succeeded. |
| Component config parse/lint/build failure | `InvalidConfig` / factory creation error | Deterministic startup error; do not retry at runtime. |
| `ErrEndOfInput` | consumer end-of-stream | Graceful input termination. |
| `ErrNotConnected` | Normally handled internally by Benthos's resource writer reconnect loop; if it escapes, `Connection`/retryable | Avoid a second connector reconnect loop in Rust. |
| Context cancellation due to orderly close | shutdown/cancelled | Do not schedule retry after shutdown. |
| Deadline/cancellation while route remains active | retryable | The operation was not confirmed. |
| `BatchError` with any failed indexes | whole-batch retryable error under ABI v1 | Safest conservative mapping; successful indexes may duplicate. |
| Other runtime error | retryable by default | Public API lacks a general permanent-error marker. |
| Known connector-specific permanent error | non-retryable only through a small audited allowlist | Never infer permanence from error strings. |
| Panic recovered at Go export boundary | permanent/internal plugin error, with redacted diagnostic | State may be suspect; avoid blind retry loops. |

Elasticsearch illustrates the output gap: one bulk request returns `BatchError` entries only for failed documents, but ABI v1 can report only the whole publish batch. Returning whole-batch retry is safe for loss avoidance but may duplicate already indexed items. An idempotent key/document ID mitigates that.

Retry ownership should be explicit:

- Benthos's resource wrapper owns connector connection establishment, reconnect on `ErrNotConnected`, and any retry behavior deliberately built into the connector.
- mq-bridge owns route-level retry, backoff, DLQ, and middleware after a write error is surfaced.
- Do not expose Redpanda processors/retry pipelines. Do not add an independent generic retry loop in the Go bridge.

## 8. Lifecycle and concurrency

Creation performs component registration lookup, config validation, `ResourceBuilder.Build`, and resource-manager startup. The resource wrappers lazily/start-connect as their normal implementation requires. `ResourceInput.ReadBatch` and `ResourceOutput.WriteBatch` become the endpoint operations. Plugin close cancels outstanding operations, drains or nacks held input batches within a deadline, invokes the `Build` close function, waits for resource shutdown, then releases Go handles.

Close must be idempotent. Handle states should prevent reads/writes after close and prevent AckFunc double invocation. All callbacks into Go run through mq's blocking ABI isolation; the Rust plugin SDK already keeps those calls off the async executor.

No mq capability extension is required for `max_in_flight`. Redpanda's registered batch-output constructor supplies max-in-flight to the resource wrapper, which starts that number of writer workers. Let the wrapper enforce its own limit. The Rust plugin may conservatively report ordered publishing when connector ordering is unknown, but it should not add another semaphore that fights the wrapper.

Connector-level `batching` can aggregate mq transactions inside Redpanda and obscure ack timing. Reject non-default `batching` in v1 (or prove a connector requires it) so mq remains the batching authority. Set `commit_requires_order = true` for inputs by default because the public resource facade does not expose an acknowledgement-order capability. Connector-specific relaxation can follow evidence.

## 9. Packaging strategy

Package one logical plugin distribution with two colocated binaries:

```text
mq-bridge-redpanda/
  libmq_bridge_redpanda.{so,dylib} | mq_bridge_redpanda.dll
  libmq_bridge_redpanda_go.{so,dylib} | mq_bridge_redpanda_go.dll
  THIRD_PARTY_NOTICES
  connectors.json
  checksums/signatures
```

The Rust plugin locates the Go library relative to its own absolute path and resolves a versioned private entry table. Do not depend on the process working directory or unrestricted system DLL search paths. Linux may use `$ORIGIN`; macOS uses `@loader_path`; Windows should use an absolute path or safe `LoadLibraryEx` flags. Sign/notarize both binaries where required.

Start with a single curated plugin, not `core` and `full`. A second `-full` artifact is justified only if prototype measurements show acceptable size, a reproducible license allowlist, and meaningful demand. Runtime connector choice remains one plugin-per-bundle, not one DLL per connector.

Pin exact Go module versions and commit the dependency lock/checksum data. Build on native CI runners for Linux amd64/arm64, macOS amd64/arm64, and Windows amd64/arm64 only after each target's selected connector graph is known to compile. Produce SBOMs and source/license inventories per artifact because Go build tags change the linked set.

## 10. Cross-platform concerns

- **Linux:** Option A is the lowest-risk in-process route. Validate glibc and musl separately; cgo and connector native dependencies can make musl/non-glibc builds distinct products. Never infer Option B safety merely from successful linkage; exercise repeated `dlopen`, message traffic, shutdown, and process exit.
- **macOS:** ship universal binaries only if both Rust and Go sides are built for and combined from matching architectures. Test codesigning, hardened runtime, notarization, `@loader_path`, signal behavior, and the multiple-Go-runtime limitation.
- **Windows/MSVC:** mq-bridge uses the MSVC Rust target while cgo generally uses GCC/MinGW. Option A with runtime symbol loading minimizes link-format coupling, but both DLLs and their dependencies must be discoverable safely. Native Windows CI is mandatory; cross-building from Linux is not the release path.
- **Cross compilation:** Go cgo requires an explicit target C compiler and is disabled by default for cross builds. Connector packages may contain architecture/build-tag branches. Prefer matrix-native builders and test on the produced OS.
- **Crashes:** all in-process options share fate with mq-bridge. Deferred `recover` handles ordinary panics only; fatal Go runtime errors, C crashes, corrupt FFI memory, and OOM remain fatal. Only subprocess mode provides process isolation.
- **ABI:** mq's public plugin ABI remains unchanged. The private Rust/Go ABI is versioned independently, returns a struct size and major/minor version, and uses only C scalar types, byte slices valid for the duration of a call, explicit ownership, and opaque handles.

## 11. Licensing and dependency findings

Benthos is MIT. The free-bundle module carries Apache-2.0 and its documented import path is `github.com/redpanda-data/connect/public/bundle/free/v4`—not the path with `/v4` immediately after `connect`. The free bundle blank-imports `public/components/community`, which currently imports 63 component-category packages and describes them as FOSS/community components. The module declares 453 direct and indirect requirements at the inspected snapshot.

That label is not enough for automatic redistribution approval. The inspected `community` imports include `cohere`, `openai`, and `ollama`, whose public wrapper `package.go` files carry Redpanda Community License/enterprise headers even though inspected underlying implementation files are Apache-2.0. AWS also illustrates how a broad public category can aggregate several implementation packages with different commercial classifications. This may be an upstream header/classification mistake, but engineering should not decide its legal effect.

Release rule:

1. Do not import `public/bundle/free` for the first distributed artifact.
2. Import an explicit allowlist of public component categories.
3. At the pinned module graph, generate an SBOM, enumerate compiled Go packages and source files for every target/build-tag combination, scan SPDX/license headers, retain MIT/Apache notices, and fail CI on unknown/RCL/BSL/proprietary results.
4. Obtain legal review or written Redpanda clarification for contradictory packages before adding them.
5. Do not enable enterprise/all bundles or `internal` packages.

The compatibility plugin can remain separate from the MIT core and carry Apache/MIT notices; permissive code is generally compatible with an MIT project, but the final binary's notices and attribution obligations still apply.

Build tags matter. Connect documents `x_benthos_extra` for components needing external C libraries such as ZeroMQ; leave it off. Other packages can vary under `cgo`, OS, and architecture tags, so the actual artifact—not just `go.mod`—is the audit unit.

### Size estimate

No honest exact plugin size can be stated before building it. The full free graph's 453 requirements, cloud SDKs, database clients, codecs, and one Go runtime strongly suggest tens to well over 100 MB depending on target, debug symbols, compression, and build tags. Official Connect release archives are contextual upper bounds, not measurements of this plugin. The POC must record unstripped, stripped, and compressed sizes for curated and full experimental builds. If full is unreasonably large, keep a curated plugin or make standalone Connect the broad-compatibility answer.

## 12. Expected performance and copying overhead

The bridge keeps batching end-to-end, so FFI call overhead should amortize well. It is plausible—not yet proven—that it becomes small at batches of 100–10,000 messages. Payload copies, Go/Rust allocations, metadata conversion, resource wrappers, downstream serialization, and connector client behavior will matter more than a single C call at large batches.

Benchmark three paths with the same backend, durability, payload, concurrency, and acknowledgement semantics:

1. native mq-bridge endpoint;
2. mq-bridge → compatibility plugin → Redpanda connector;
3. standalone Redpanda Connect → same connector/backend.

Workloads:

- a discard/null output to expose framework and FFI overhead;
- file output on a controlled filesystem, with equivalent flushing;
- NATS or Kafka for realistic network I/O;
- publisher-only and end-to-end input→output runs;
- payloads such as 100 B, 1 KiB, and 100 KiB;
- batch sizes 1, 100, 1,000, and 10,000, plus each connector's practical limit.

Measure messages/s, MB/s, p50/p95/p99 latency, CPU time, peak and steady RSS, allocations/GC using Go pprof and available Rust tooling, artifact size, connect/close time, and redelivery/duplicate counts during injected partial failures. Warm up, use repeated fixed-duration trials, report confidence intervals, pin CPU where practical, and preserve raw configurations/results.

The benchmark must separately answer whether Option A's Go boundary is material and whether the complete plugin has enough benefit over standalone Connect. Do not set a release throughput threshold before measuring the native and standalone baselines.

## 13. Minimal proof-of-concept plan

### Phase 0 — release-gate spike

Deliverables: a new `mq-bridge-redpanda` repository; pinned modules; generated package/license inventory; native CI skeleton; a tiny Go `c-shared` library load/unload/process-exit test; no mq core changes.

Verify:

- ResourceBuilder signatures compile at the pinned free-module Benthos version.
- Linux, macOS, and Windows can load the Rust plugin and its Go sibling repeatedly in test processes.
- Ordinary Go panic is translated; fatal-process behavior is documented.
- Curated connector imports contain no disallowed/unknown license files.

Stop if redistribution cannot be cleared or Windows packaging needs unsupported invasive changes.

### Phase 1 — vertical slice

Implement only:

- Rust plugin SDK endpoint factory named `redpanda`;
- private versioned batch FFI;
- one input and one output using `ResourceBuilder`;
- payload plus UTF-8 string metadata;
- opaque batch ack handle and aggregate nack rule;
- lifecycle, cancellation, and exact-once handle release;
- raw nested connector config; no dynamic GUI schema.

Candidate connectors:

- **Beanstalkd input** for simple local ack/nack/redelivery testing.
- **Elasticsearch v8 output** for local batch output and injected partial `BatchError` behavior.
- A third connector only if its public category and transitive source pass the license audit; S3 is valuable for metadata/SQS ack behavior but should not be distributed until its broad category graph is cleared.

Use local disposable services in integration tests. Also add a synthetic Go-side test connector registered through `public/service` to deterministically produce typed metadata, oversized source batches, ack errors, connection errors, panic, slow close, and partial output errors without depending on cloud services.

Success checks:

- Existing mq plugin conformance suite passes where semantics are representable.
- Metadata and binary payload round-trip exactly; typed metadata/stringification is asserted.
- Nack, filter/drop, abandoned batch, shutdown-before-commit, ack error, source batch chunking, and double-close tests pass.
- Partial output failure loses no failed item; duplicates of successful items are counted and documented.
- Race detector/ASan-equivalent FFI tests show no escaped Go pointer, use-after-free, or double free.

### Phase 2 — comparison and go/no-go

Run the benchmark matrix and compare operational complexity against standalone Connect. Build a full-free experimental artifact solely to measure size/dependency/platform behavior; do not distribute it. Produce a connector eligibility catalog with direction, build targets, native dependency, resource need, context/structured-data caveat, support classification, and license decision.

Move from `PROTOTYPE ONLY` to `BUILD` only if:

- the selected bundle has an approved, reproducible redistribution inventory;
- all supported OS/architecture packages pass load, I/O, shutdown, and failure tests;
- observed copy/FFI overhead is acceptable relative to standalone Connect;
- the duplicate/ack contract is acceptable to intended users;
- the plugin is operationally simpler than deploying standalone Connect for the target use cases.

## 14. Required mq-bridge core/plugin ABI changes

**None for the POC or basic production runtime.** ABI 1.0 already provides:

- batched payload/metadata transfer in one call;
- input batch lifetime plus per-message dispositions;
- publisher batch results;
- endpoint creation from arbitrary JSON;
- lifecycle/error hooks and safe plugin retention.

The plugin reduces per-message input dispositions to Redpanda's one AckFunc and reduces Redpanda partial output results to ABI v1's whole-batch result. Those are fidelity losses, not construction blockers.

Two possible future additive ABI-minor features have general value but should be evidence-driven:

1. **Per-message publisher outcomes.** Preserve `BatchError` granularity and avoid retrying successful siblings. This benefits non-Redpanda publishers too.
2. **Plugin endpoint catalog/schema metadata.** Allow a plugin to return endpoint names/directions and an opaque JSON schema/catalog. This could feed CLI/app discovery. It should be a generic optional callback, not Redpanda-specific.

Separately, consider a core enhancement that allows custom endpoint configurations to participate in secret extraction/redaction. Until then, plugin docs should require indirect secret references and warn that arbitrary custom JSON is not automatically scrubbed.

## 15. Things explicitly not to implement

- Bloblang, processors, mappings, mutation pipelines, or Redpanda routing.
- Buffers, Redpanda batching policy, generic retry pipelines, DLQ, or rate-limit policy.
- Streams, stream manager, HTTP management API, metrics server, or Redpanda CLI emulation.
- One DLL per connector.
- A hand-maintained schema copy for every connector.
- Imports from Connect `internal` packages or an unreviewed enterprise/all/free aggregate.
- Error-string parsing to guess permanence.
- Per-message FFI calls.
- Go pointers retained in Rust/C memory, implicit buffer ownership, or GC finalizers as correctness mechanisms.
- Automatic translation of arbitrary structured values or private message contexts in v1.
- A second generic retry loop around `WriteBatch`.
- Connector-level batching unless a connector demonstrably requires it.
- ABI/core changes before the POC proves a general need.
- Claims of exactly-once delivery, zero-copy operation, full connector compatibility, or safe in-process crash isolation.

## 16. Final recommendation

**PROTOTYPE ONLY.** Build the narrow Option A vertical slice because the current `ResourceBuilder` API creates components without a full stream and therefore offers real architectural value: mq-bridge remains the pipeline engine while Redpanda supplies connector implementations.

Do not yet build or advertise a broad “all free connectors” product. The free-bundle provenance contradiction, binary/platform scope, and inevitable batch-fidelity losses deserve evidence rather than optimism. If the prototype shows that users primarily want the full catalog, need Redpanda processors/resources broadly, or cannot accept two binaries and at-least-once duplicates, then running Redpanda Connect as a subprocess—or deploying it directly with a broker/IPC boundary—provides essentially the same compatibility with better crash isolation and lower maintenance. In that case, stop the in-process plugin rather than growing it into a second Connect runtime.

## Primary source ledger

| Claim area | Primary evidence | Confidence | Remaining gap |
|---|---|---|---|
| Component-only construction | Benthos `ResourceBuilder`, `Resources`, resource builder test | High | Public API stability across future releases; pin and compile-test. |
| Constructor retrieval | Benthos registration/environment APIs | High | None found publicly; upstream could add one later. |
| Ack semantics | `AckFunc`, ResourceInput, S3/SQS, Beanstalkd | High | Exhaustive per-connector behavior not audited. |
| Partial output | `BatchError`, Elasticsearch v8 output, mq ABI v1 | High | Duplicate impact is backend/idempotency-dependent. |
| Message mapping | Both concrete message types and Benthos message API | High | Context-sensitive/structured connectors need eligibility testing. |
| Concurrency/retry | Benthos async writer/resource lifecycle | High | Connector-specific internal retry behavior varies. |
| Go build modes | Official Go build-mode, cgo, runtime/cgo documentation and Go issues #48596/#65050 | Medium-high | Current target/toolchain experiments are still required. |
| Licensing | Exact LICENSE, free/community package lists, module graph, source headers | High on observed files | Legal interpretation/upstream clarification unresolved. |
| Size/performance | Dependency graph and copy-path inspection | Medium | Must be measured on built artifacts/workloads. |

Additional Go primary references: [`go build` modes](https://pkg.go.dev/cmd/go#hdr-Build_modes), [supported ports source](https://go.dev/src/internal/platform/supported.go), [`runtime/cgo.Handle`](https://pkg.go.dev/runtime/cgo#Handle), [cgo pointer/export rules](https://go.dev/src/cmd/cgo/doc.go), [Go called from non-Go programs and signals](https://pkg.go.dev/os/signal#hdr-Non_Go_programs_that_call_Go_code), and [panic/recover specification](https://go.dev/ref/spec#Handling_panics).
