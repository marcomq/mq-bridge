<!-- description: Classify incoming messages with an LLM (OpenAI or a local Ollama model) and route each one by the answer: support tickets from a webhook into one Redis stream per team. Runnable docker-compose example. -->

# How to classify and route messages with an LLM (webhook → LLM → one queue per team)

One mq-bridge route takes support tickets from a webhook, asks a language model which team a
ticket belongs to, and puts the ticket into that team's queue. The model's answer is checked
against the list of teams; anything else goes to a queue that people read.

Runnable example: [`examples/llm-ticket-triage`](https://github.com/marcomq/mq-bridge/tree/main/examples/llm-ticket-triage).

## The problem: sorting messages that have no field to sort by

Tickets, emails, reviews and chat messages arrive as free text. Routing them by keyword rules
breaks as soon as a customer words it differently ("charged twice", "double payment", "two
debits"). A model reads the text the way a person would, but calling it adds what a rule never
needed: a slow and sometimes failing API, a bill per request, and an answer that cannot be
trusted to stay inside the options it was given.

## The solution: a lookup to the model, a switch on the answer

```mermaid
flowchart LR
  H[Helpdesk webhook] -- "POST /tickets" --> B[mq-bridge route]
  B -- "chat completion" --> L[LLM]
  L -- '{"team": "billing"}' --> B
  B --> S1[(tickets.billing)]
  B --> S2[(tickets.technical)]
  B --> S3[(tickets.account)]
  B -- "anything else" --> S4[(tickets.triage)]
```

```yaml
tickets_triage:
  input:
    http: { url: "0.0.0.0:8080", path: "/tickets", method: "POST" }
  output:
    middlewares:
      # Output middlewares run bottom-up: retry, then the model call.
      - lookup:
          from:
            openai_chat:
              url: "http://ollama:11434"
              model: "qwen2.5:3b"
              json: true
              system: >-
                You route customer support tickets to a team. Answer with a JSON
                object {"team": string}. team is exactly one of:
                "billing" for payments, charges, refunds and invoices;
                "technical" for bugs, crashes, errors and outages;
                "account" for login, password, email and profile changes;
                "other" for everything else.
              prompt: "Subject: ${payload:subject}\n\n${payload:body}"
              options: { temperature: 0 }
          into: triage
      - retry: { max_attempts: 5 }
    # Only known teams get a queue; any other answer goes to people.
    switch:
      when:
        - if: 'triage.team == "billing"'
          to: { redis_streams: { url: "redis://redis:6379", stream: "tickets.billing" } }
        - if: 'triage.team == "technical"'
          to: { redis_streams: { url: "redis://redis:6379", stream: "tickets.technical" } }
        - if: 'triage.team == "account"'
          to: { redis_streams: { url: "redis://redis:6379", stream: "tickets.account" } }
      default: { redis_streams: { url: "redis://redis:6379", stream: "tickets.triage" } }
```

How it fits together:

- The [`http`](../connectors/http.md) input answers the webhook once the ticket is in its stream.
  The request body is the payload.
- [`openai_chat`](../connectors/openai.md#openai_chat) sends one chat completion per ticket to any
  API that speaks the OpenAI wire format; here a local Ollama. With `json: true` the answer is
  parsed, and the [`lookup`](../cookbook/lookup.md) writes it into the ticket as `triage`. The
  ticket keeps all its fields.
- The task is in `system` and only the ticket in `prompt`, so the ticket text is data, not
  instructions. `temperature: 0` makes the same ticket get the same answer.
- [`switch`](../cookbook/switch.md) routes on `triage.team`. Because every queue is named in the
  config, an answer outside the list (a misspelled team, an invented one, or what a ticket
  talked the model into) matches nothing and goes to `default`.
- [`redis_streams`](../connectors/redis.md) is the queue here. Any output works per branch:
  Kafka topics, NATS subjects, a database table, an HTTP call to the helpdesk API.

The prompt was tried on the six tickets of the example: `qwen2.5:1.5b` sent the double charge
to `technical`, `qwen2.5:3b` sorted all of them as intended, the same way in repeated runs.

### A hosted model

Leave out `url` for OpenAI, or set the provider's base URL, set `model`, and add
`api_key: "${env:OPENAI_API_KEY}"`. The [LLM cookbook](../cookbook/llm.md#other-providers) lists
the providers and the form for APIs of another shape.

### Fewer requests

Each ticket is one request. With `messages_per_request: 20` on `openai_chat`, up to 20 messages
of a batch go into one request and the system prompt is paid for once; see
[Several messages in one request](../connectors/openai.md#several-messages-in-one-request). It
needs a batch to bundle from, which a webhook delivering one ticket per request does not give;
it pays off with a broker input. Do not bundle texts from sources that might try to steer the
model with ones whose answers matter.

## Runnable example

```bash
git clone https://github.com/marcomq/mq-bridge && cd mq-bridge/examples/llm-ticket-triage
docker compose up -d
curl -sS http://localhost:8080/tickets -H 'Content-Type: application/json' \
  -d '{"id": 7, "subject": "Refund", "body": "Please refund my last order, it never arrived."}'
docker compose exec redis redis-cli XRANGE tickets.billing - +
```

The stack is Redis 8.8, Ollama 0.12.3 with `qwen2.5:3b` and mq-bridge-app 1.0.1. `./test.sh`
posts six tickets and checks the stream each one lands in, including one that tries to name its
own queue; it runs in CI.

## Failure semantics

| Event | What happens |
| :--- | :--- |
| **The model API is slow** | The webhook caller waits for the answer. A helpdesk that gives up after a few seconds sends the ticket again; set `fire_and_forget: true` on the `http` input to answer `202` at once, at the price of losing a ticket whose processing fails afterwards. A broker in front of the route avoids both. |
| **The model API fails or rate-limits** | 408, 429 and 5xx are retryable and `retry` repeats the request; the caller gets an error only when the attempts are used up and can send again. Other 4xx fail the ticket. |
| **The answer is not JSON** | The ticket fails for good. Add a [`dlq`](../cookbook/dlq.md) after `retry` to keep it. |
| **The answer is JSON but wrong** | It goes to the stream the answer names, or to `tickets.triage` when it names no known team. A model that answers wrongly but within the list is not detected. |
| **Duplicates** | A ticket the caller sends twice is classified, and billed, twice, and appears twice in its stream. Put [`deduplication`](../cookbook/deduplication.md) keyed on the ticket id on the input if that matters. |
| **Redis unavailable** | The send fails and the caller gets an error. A batch split across several streams is sent to all of them again when one fails, so the others can see a ticket twice. |

## When not to use mq-bridge for this

- **The helpdesk has routing built in** and its rules or its own AI routing are good enough.
- **Routing needs a conversation**: asking the customer a question, looking up their account in
  several systems, deciding on a refund. That is an agent with tools; this route makes one
  call and acts on one answer.
- **Keywords are enough.** A [`switch`](../cookbook/switch.md) on a field, or a
  [`filter`](../engine/reference.md#filter) expression, costs nothing per message.

## Alternatives

| | mq-bridge | Redpanda Connect | n8n and similar workflow tools |
| :--- | :--- | :--- | :--- |
| Model call | `openai_chat` (any OpenAI-compatible API) as a `lookup` | [`openai_chat_completion`](https://docs.redpanda.com/redpanda-connect/components/processors/openai_chat_completion/) and [`ollama_chat`](https://docs.redpanda.com/connect/components/processors/ollama_chat/) processors | an AI node per workflow |
| Routing on the answer | `switch` output | `switch` output | branch nodes |
| Runs as | library in a Rust, Python or Node.js process, or one binary | one binary | a server with a visual editor |

A workflow tool fits when the steps change often and are edited by people who do not write
config files. mq-bridge fits when the classification is one step of a data flow that also has
to be reliable: retries, back-pressure from the queue and at-least-once delivery come with the
route.
