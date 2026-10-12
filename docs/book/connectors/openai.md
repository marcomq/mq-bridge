# OpenAI-compatible APIs (LLMs)

Two endpoints call an API that speaks the OpenAI wire format: OpenAI itself,
and the servers that copied it, such as Ollama, vLLM, LM Studio, Groq and
Mistral.

| Endpoint | Calls | Requests |
|---|---|---|
| [`openai_chat`](#openai_chat) | `/chat/completions` | one per message, or one per `messages_per_request` |
| [`openai_embeddings`](#openai_embeddings) | `/embeddings` | one per batch |

Both answer; they write nothing. Use them as the `from` of a
[`lookup`](../cookbook/lookup.md), which puts the answer into the message, or
as the `to` of a [`request`](../engine/reference.md#request) endpoint, which
forwards the answer. Recipes are in the [LLM cookbook](../cookbook/llm.md).

mq-bridge is not affiliated with OpenAI; the names say which API the endpoints
speak.

## `openai_chat`

Sends one chat completion per message and answers with the text of the first
choice.

```yaml
output:
  middlewares:
    - lookup:
        from:
          openai_chat:
            url: "http://localhost:11434"      # Ollama; leave out for api.openai.com
            model: llama3.2
            system: "Answer with one word: positive, negative or neutral."
            prompt: "${payload:text}"
        into: sentiment
  kafka: { topic: "reviews.scored", url: "localhost:9092" }
```

With `json: true` the endpoint asks for a JSON object and parses it, so the
answer is an object in the message instead of text:

```yaml
output:
  middlewares:
    - lookup:
        concurrency: 4
        from:
          openai_chat:
            api_key: "${env:OPENAI_API_KEY}"
            model: gpt-4o-mini
            json: true
            prompt: 'Extract {"city": string, "urgent": boolean} as JSON from: ${payload:text}'
            options: { temperature: 0 }
        into: extracted
  kafka: { topic: "tickets.extracted", url: "localhost:9092" }
```

On the command line the scheme is `openai://` or `openai-chat://`, for example
`openai://localhost:11434?model=llama3.2&prompt=…`.

### Several messages in one request

With `messages_per_request` the endpoint sends that many messages as numbered
inputs of one completion and hands each message its own answer. The system
message, and with it most of the prompt tokens, is sent once per request
instead of once per message:

```yaml
reviews_scored:
  batch_size: 100
  input: { kafka: { topic: "reviews", url: "localhost:9092" } }
  output:
    middlewares:
      - lookup:
          from:
            openai_chat:
              api_key: "${env:OPENAI_API_KEY}"
              model: gpt-4o-mini
              system: "Answer with one word: positive, negative or neutral."
              prompt: "${payload:text}"
              messages_per_request: 20
              options: { temperature: 0 }
          into: sentiment
    kafka: { topic: "reviews.scored", url: "localhost:9092" }
```

The request asks for a JSON object `{"answers": [{"n": 1, "answer": …}, …]}`
with one entry per input. Each answer goes to the message whose number it
carries, so the order the model answers in does not matter. With `json: true`
each `answer` is a JSON object, otherwise a string.
On the command line it is one more parameter:
`openai://localhost:11434?model=llama3.2&prompt=…&messages_per_request=20`.

- **Answers that cannot be matched are asked again in halves.** A response
  that is not that object, has more or fewer entries than inputs, or holds a
  number twice or one no input has, is not used. Its messages are sent again
  in two requests, and so on, until what cannot be answered stands alone and
  fails for good. A request the API refuses with 400, 409, 413 or 422, such
  as one over the context length, is halved the same way, so one oversized
  message does not fail its neighbours.
- **Halving costs requests.** One bad message among 20 costs about ten
  requests instead of one; a model that loses count at every size costs up to
  twice as many requests as without bundling. Pick a size your model handles;
  how many inputs a model keeps apart depends on the model and on the length
  of the texts. Each halving logs a warning with the reason.
- **Under a `lookup`, one failure repeats the batch.** The lookup takes a
  batch only whole: when a message fails for good, it asks every message of
  that batch again, one per request. Put a
  [`filter`](../engine/reference.md#filter) before the lookup for what you
  know the model refuses.
- **Leave room for the answers.** A `max_tokens` in `options` bounds the whole
  response, and `request_timeout_ms` the whole request. An answer that was
  cut off cannot be matched; the warning then says the response ended with
  `length`.
- **Size is counted in messages and bytes, not tokens.** `max_request_bytes`
  bounds the request body. A single message over the model's context is
  refused by the API and fails alone.
- **The route's `batch_size` is the upper bound.** A bundle is cut from one
  batch, so a route with `batch_size: 1` still sends one message per request.
  Nothing is held back to fill a bundle.
- **429, 408 and 5xx fail the whole batch as retryable** and are not halved;
  [`retry`](../cookbook/retries.md) sends it again. A `Retry-After` header is
  not read: set the backoff in `retry`.
- **`system` is fixed text here.** It is sent once per request, so it cannot
  take values from a message; put those into `prompt`. Being the same in every
  request, it is also what a provider's prompt cache can reuse.
- **Put the task into `system` and only the data into `prompt`.** The model
  sees all inputs of a request together, and a text that reads like an
  instruction can change the answers of its neighbours. Do not bundle texts
  from sources you do not trust with ones whose answers matter, and leave
  `messages_per_request` out where each message needs a request of its own.
- **The inputs are sent as text parts** of one user message
  (`content: [{"type": "text", …}]`), and the request sets
  `response_format: json_object` unless `options` sets another, such as a
  strict `json_schema`. OpenAI, Ollama and vLLM take both; a server that does
  not needs the one-per-message form.
- **A payload that is not UTF-8:** `${payload}` is sent with the invalid bytes
  replaced. A field such as `${payload:text}` needs a JSON payload, which is
  UTF-8.
- **No metrics per bundle.** The token `usage` of a response is not kept, and
  an answer carries no mark of the bundle it came from.

| Field | Default | Meaning |
|---|---|---|
| `url` | `https://api.openai.com/v1` | Base URL. One without a path gets `/v1`. `openai://host` is plain HTTP, `openai+https://host` is HTTPS |
| `api_key` | none | Sent as `Authorization: Bearer` |
| `model` | required | Model name |
| `prompt` | required | Template of the user message |
| `system` | none | Template of the system message |
| `json` | `false` | Asks for `response_format: json_object` and parses the answer. An answer that is not JSON fails its message for good |
| `options` | none | Further request fields, e.g. `temperature`, `max_tokens`, `response_format`. It cannot set `model`, `messages` or `stream`. JSON text in a URI |
| `concurrency` | 8 | Requests in flight when a batch is sent to the endpoint as an output. Under a `lookup`, the lookup's own `concurrency` (default 16) decides, unless `messages_per_request` is set |
| `messages_per_request` | none | Messages answered by one request, see [above](#several-messages-in-one-request). Unset: one request per message |
| `max_request_bytes` | 10 MiB | With `messages_per_request`: a bundle over this size is cut earlier |
| `request_timeout_ms` | none | Timeout of one request |
| `tls` | none | TLS options as on [`http_bulk`](http-bulk.md). JSON text in a URI |

## `openai_embeddings`

Sends the texts of a whole batch in one request and answers each message with
its vector.

```yaml
output:
  middlewares:
    - lookup:
        from:
          openai_embeddings:
            api_key: "${env:OPENAI_API_KEY}"
            model: text-embedding-3-small
            input: "${payload:title}: ${payload:body}"
        into: vector
  qdrant: { url: "http://localhost:6333", collection: docs }
```

That writes the message with a `vector` field; shape it into a point first, as
on the [Qdrant](./qdrant.md) page. On the command line the scheme is
`openai-embeddings://`.

| Field | Default | Meaning |
|---|---|---|
| `url` | `https://api.openai.com/v1` | Base URL, as above. `openai-embeddings://host` is plain HTTP |
| `api_key` | none | Sent as `Authorization: Bearer` |
| `model` | required | Embedding model |
| `input` | required | Template of the text to embed |
| `dimensions` | none | Length of the vectors, for models that can shorten them |
| `max_request_bytes` | 10 MiB | A larger batch is split into several requests |
| `request_timeout_ms` | none | Timeout of one request |
| `tls` | none | TLS options as on [`http_bulk`](http-bulk.md). JSON text in a URI |

## From Rust

`OpenAiChatConfig` and `OpenAiEmbeddingsConfig` are the typed forms of the
tables above. Either converts into an `Endpoint`; it needs the `http-bulk`
feature and no registration.

```rust
use mq_bridge::endpoints::http_bulk::OpenAiChatConfig;
use mq_bridge::models::Endpoint;

let llm: Endpoint = OpenAiChatConfig {
    model: "gpt-4o-mini".into(),
    prompt: "Summarise: ${payload:text}".into(),
    ..Default::default()
}
.into();
```

## What to know

- **Templates are escaped.** Text taken from a message is JSON-escaped, so a
  quote or a line break in it cannot break the request.
- **A temporary error fails only its message.** 408, 429 and 5xx are retryable.
  List [`retry`](../cookbook/retries.md) after the `lookup`; it repeats only
  the messages that failed, so answers already paid for are kept.
- **One request per batch means one fate per batch for embeddings.** A request
  the API refuses fails every message in it. OpenAI refuses an empty text, so
  drop such messages with a [`filter`](../engine/reference.md#filter) first.
- **Vectors are matched by position.** The API must answer in input order,
  which OpenAI documents. A response with another number of vectors fails the
  batch for good.
- **Batch size is the request size.** OpenAI takes at most 2048 texts per
  embeddings request; keep the route's `batch_size` at or below your
  provider's limit.
- **Nothing is cached.** A replayed message is sent, and billed, again. Use
  [`deduplication`](../cookbook/deduplication.md) before the lookup if that
  matters.
- **No streaming here.** For a token stream, use the `http` output with
  `stream_response_to`; see the [LLM cookbook](../cookbook/llm.md#stream-tokens).

Both endpoints are the generic [`http_bulk`](./http-bulk.md) `query` with the
requests filled in. An API with another shape, such as Anthropic's
`/v1/messages`, is a `query` with `per_message: true`:

```yaml
output:
  http_bulk:
    url: https://api.anthropic.com
    headers:
      x-api-key: <api key>
      anthropic-version: "2023-06-01"
    query:
      path: /v1/messages
      format: json_array
      per_message: true
      request: '{"model":"claude-haiku-4-5","max_tokens":256,"messages":[{"role":"user","content":"${payload:text}"}]}'
      value: /content/0/text
      error: /error/message
```
