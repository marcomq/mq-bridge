# Nim examples

The engine does the I/O: lookups, LLM calls, batching, file formats. The Nim
handler does the logic: merge, validate, compute. Each example is one file and
needs no Docker.

| Example | Shows |
|---|---|
| [customer_lookup.nim](customer_lookup.nim) | Two lookups per order in parallel: one batched SQL query for the whole batch (SQLite) and one HTTP call per order, served by a second route in the same program. Nim merges the answers. |
| [llm_tickets.nim](llm_tickets.nim) | Classifies support tickets with an LLM, ten tickets per request and four requests at a time; Nim checks the labels and escalates by keyword. With a question as argument it streams one answer token by token. |
| [csv_pipeline.nim](csv_pipeline.nim) | Reads a CSV file, types the fields and computes a dew point in Nim, drops invalid rows, and writes JSON lines plus a Parquet dataset. |
| [connect_access_log.nim](connect_access_log.nim) | A Redpanda Connect `generate` input makes access-log events with Bloblang; Nim drops health checks, flags slow and failed requests, and counts hits per path. |

## Run

You need Nim 2.0 or later and the C library `libmq_bridge` with every endpoint
(SQL, HTTP, LLM, Parquet).

1. Build the library, from the repository root (needs a Rust toolchain):

   ```sh
   cargo build --release -p mq-bridge-c
   ```

   Or, for a released version, download it instead: `cd bindings/nim && nimble getlib`
   fetches the release named in `mq_bridge.nimble` into `bindings/nim/src/lib`.

2. Compile and run an example, again from the repository root:

   ```sh
   cd bindings/nim/examples
   nim c -r csv_pipeline.nim
   nim c -r connect_access_log.nim
   nim c -r customer_lookup.nim
   nim c -r llm_tickets.nim
   nim c -r llm_tickets.nim "Explain a message queue in two sentences."
   ```

   `config.nims` here picks up `target/release` (then `target/debug`) on its own;
   `MQB_LIB=/path/to/libmq_bridge.dylib` names another file. Binaries and output
   land in `out/`.

Some examples need something besides the library:

- `customer_lookup` seeds its database with the `sqlite3` command. Against
  Postgres only the `url` of the CRM lookup changes, e.g.
  `postgres://user:pass@localhost/crm`; cast the key there
  (`IN (${payload:customer_id}::bigint)`).
- `llm_tickets` talks to [Ollama](https://ollama.com) on `localhost:11434` with
  the `qwen3.5:2b` model (`ollama pull qwen3.5:2b`, about 2.7 GB). Set
  `LLM_URL`, `LLM_MODEL` and `OPENAI_API_KEY` for any OpenAI-compatible API.
  It sends ten tickets per request (`messages_per_request: 10`); a smaller model
  such as `qwen2.5:0.5b` cannot answer bundles, remove that line for it. Keep
  `reasoning_effort: none` in `options`: without it a thinking model looks hung.
  Remove it for an API that refuses it.
- `connect_access_log` needs the [Connect plugin](../../../docs/book/connectors/connect.md)
  (`brew install marcomq/tap/mq-bridge-connect` or
  `conda install -c marcomq mq-bridge-connect`). `loadInstalledPlugin("connect")`
  loads just that library from the Homebrew or active conda prefix; no other
  installed plugin is touched. For a plugin somewhere else, set `MQB_PLUGIN_DIR`
  to its directory, or load the file with `loadPlugin(path)`. Any Connect input
  or output fits in the same place, e.g. `connector: mqtt` or `connector: gcp_pubsub`.
- `csv_pipeline` writes `out/readings/year=…/month=…/day=…/*.parquet`. Query it
  with DuckDB:
  `SELECT * FROM read_parquet('out/readings/**/*.parquet', union_by_name = true)`.
