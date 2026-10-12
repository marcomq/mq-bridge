# LLM calls in a pipeline

Two endpoints call an [OpenAI-compatible API](../connectors/openai.md):
`openai_chat` sends one chat completion per message, `openai_embeddings` one
embeddings request per batch. Both are used as the `from` of a
[`lookup`](lookup.md), which writes the answer into the message. The message
then goes on to any output, with the retries, dead-letter queue and
back-pressure of an ordinary route.

Chat, embeddings and the Qdrant upsert were run against Ollama (`qwen2.5:0.5b`,
`all-minilm`) and Qdrant 1.19, with files in place of Kafka and Postgres.
OpenAI itself was not called.

## Classify, then route on the answer

With `json: true` the answer is parsed, so a `switch` can read its fields:

```yaml
tickets_triage:
  input: { kafka: { topic: "tickets", url: "localhost:9092" } }
  output:
    middlewares:
      - lookup:
          concurrency: 8
          from:
            openai_chat:
              api_key: "${env:OPENAI_API_KEY}"
              model: gpt-4o-mini
              json: true
              system: 'Answer as JSON: {"urgent": boolean, "topic": string}'
              prompt: "${payload:subject}\n\n${payload:body}"
              options: { temperature: 0 }
          into: triage
      - retry: { max_attempts: 5 }
      - dlq:
          endpoint: { kafka: { topic: "tickets.failed", url: "localhost:9092" } }
    switch:
      when:
        - if: "triage.urgent == true"
          to: { kafka: { topic: "tickets.urgent", url: "localhost:9092" } }
      default: { kafka: { topic: "tickets.normal", url: "localhost:9092" } }
```

`retry` repeats only the messages whose request failed, so answers already
paid for are kept. An answer that is not JSON fails its message for good and
goes to the `dlq`.

## Summarise, and keep only the answer

As the `to` of a [`request`](../engine/reference.md#request) endpoint, the
model's answer replaces the message and is forwarded:

```yaml
articles_summarise:
  input: { file: { path: "articles.jsonl", format: raw } }
  output:
    request:
      to:
        openai_chat:
          url: "http://localhost:11434"
          model: llama3.2
          prompt: "Summarise in one sentence: ${payload:text}"
      forward_to: { file: { path: "summaries.jsonl", format: raw } }
```

On the command line the endpoint is `openai://host?model=…&prompt=…`.
`openai://host` is plain HTTP, for a local server such as Ollama;
`openai+https://host` is HTTPS. Without a `url` the endpoint calls
`https://api.openai.com/v1`.

## Bundle messages into one request

A short classification repeats the same instructions for every message.
`messages_per_request` sends several messages as numbered inputs of one
completion and gives each message its own answer, so the system message is
paid for once per request:

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
          into: sentiment
      - retry: { max_attempts: 5 }
    kafka: { topic: "reviews.scored", url: "localhost:9092" }
```

Each answer carries the number of its input, so the order the model answers
in does not matter. Answers that cannot be matched are asked again in halves,
so a model that loses count costs more, not less. Put the task into `system`
and only the data into `prompt`. What to expect is on the
[connector page](../connectors/openai.md#several-messages-in-one-request).

## Embed a table into a vector store

One embeddings request covers a whole batch. The vector lands in the message,
and a `transform` shapes the point the store expects. Output middlewares run
bottom-up.

```yaml
docs_to_qdrant:
  input:
    sqlx:
      url: "postgres://user:pass@localhost/app"
      table: docs
      cursor_column: id
  output:
    middlewares:
      - transform:
          mapping:
            id: "$.id"
            vector: "$.vector"
            "payload.title": "$.title"
      - lookup:
          from:
            openai_embeddings:
              url: "http://localhost:11434"
              model: all-minilm
              input: "${payload:title}: ${payload:body}"
          into: vector
      - retry: { max_attempts: 5 }
    qdrant: { url: "http://localhost:6333", collection: docs }
```

To follow changes and deletes as well, read the table with `postgres_cdc`; the
[Qdrant](../connectors/qdrant.md) page has that route.

## From Rust

Each endpoint has a typed configuration that converts into an `Endpoint`:

```rust
use mq_bridge::endpoints::http_bulk::OpenAiChatConfig;
use mq_bridge::models::Endpoint;

let llm: Endpoint = OpenAiChatConfig {
    model: "gpt-4o-mini".into(),
    prompt: "Summarise: ${payload:text}".into(),
    api_key: std::env::var("OPENAI_API_KEY").ok(),
    ..Default::default()
}
.into();
```

It needs the `http-bulk` feature and no registration.

## Cost and limits

- **Every message is a request.** For chat, the `lookup`'s `concurrency`
  (default 16) is the number of requests in flight. Lower it, or add a
  [`limiter`](../engine/reference.md#limiter), to stay under a provider's rate
  limit. [`messages_per_request`](#bundle-messages-into-one-request) puts
  several messages into one request instead.
- **A 429 is retryable.** With `retry` the route slows down instead of losing
  messages; the input is not acknowledged until the answer is in.
- **Nothing is cached.** A message that is delivered twice is sent, and billed,
  twice. Put [`deduplication`](deduplication.md) on the input if that matters.
- **Prompts are escaped.** Text taken from a message cannot break the request
  body. It can still contain instructions to the model; treat the answer as
  untrusted input.

## Stream tokens

These endpoints wait for the whole answer. For a token stream, call the API
with the [`http`](../reference/http.md) output and `stream_response_to`, which
publishes each server-sent event as its own message.

## Other providers

| Provider | How |
|---|---|
| OpenAI | default `url`; set `api_key` |
| Ollama, vLLM, LM Studio | `url: "http://host:port"`; `/v1` is added |
| Groq, Mistral, OpenRouter and other compatible APIs | their base URL including the path, and `api_key` |
| Anthropic and other APIs of another shape | [`http_bulk` with `query.per_message`](../connectors/openai.md#what-to-know) |

Of these, only Ollama was called.
