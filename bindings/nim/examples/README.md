# Nim examples

The engine does the I/O: lookups, LLM calls, batching, file formats. The Nim
handler does the logic: merge, validate, compute. Each example is one file and
needs no Docker.

| Example | Shows |
|---|---|
| [customer_lookup.nim](customer_lookup.nim) | Two lookups per order in parallel: one batched SQL query for the whole batch (SQLite) and one HTTP call per order, served by a second route in the same program. Nim merges the answers. |
| [llm_tickets.nim](llm_tickets.nim) | Classifies support tickets with an LLM, four requests at a time; Nim checks the labels and escalates by keyword. With a question as argument it streams one answer token by token. |
| [csv_pipeline.nim](csv_pipeline.nim) | Reads a CSV file, types the fields and computes a dew point in Nim, drops invalid rows, and writes JSON lines plus a Parquet dataset. |

## Run

They need the full C library (SQL, HTTP, LLM and Parquet endpoints). From the
repository root:

```sh
cargo build -p mq-bridge-c
cd bindings/nim/examples
nim c -r customer_lookup.nim
nim c -r csv_pipeline.nim
nim c -r llm_tickets.nim
nim c -r llm_tickets.nim "Explain a message queue in two sentences."
```

The library is found as for the tests (`target/release`, `target/debug`, or
`MQB_LIB`). Binaries and output land in `out/`.

- `customer_lookup` seeds its database with the `sqlite3` command. Against
  Postgres only the `url` of the CRM lookup changes, e.g.
  `postgres://user:pass@localhost/crm`; cast the key there
  (`IN (${payload:customer_id}::bigint)`).
- `llm_tickets` talks to [Ollama](https://ollama.com) on `localhost:11434` with
  the small `qwen2.5:0.5b` model (`ollama pull qwen2.5:0.5b`, about 400 MB). Set
  `LLM_URL`, `LLM_MODEL` and `OPENAI_API_KEY` for any OpenAI-compatible API. A
  larger model can answer several tickets per request: add
  `messages_per_request: 10` to the `openai_chat` block.
- `csv_pipeline` writes `out/readings/year=…/month=…/day=…/*.parquet`. Query it
  with DuckDB:
  `SELECT * FROM read_parquet('out/readings/**/*.parquet', union_by_name = true)`.
