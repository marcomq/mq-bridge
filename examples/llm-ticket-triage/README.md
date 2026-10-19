# LLM ticket triage: webhook → classify → one queue per team

Support tickets arrive on an HTTP webhook. A local [Ollama](https://ollama.com) model
(`qwen2.5:3b`) names the team for each ticket, and a `switch` puts the ticket into that team's
Redis stream. An answer that names no known team goes to `tickets.triage` for a person to look
at. No API key is needed.

Explained in the book: [Classify and route messages with an LLM](https://marcomq.github.io/mq-bridge/use-cases/llm-ticket-triage.html).

```mermaid
flowchart LR
  H[Helpdesk webhook] -- "POST /tickets" --> B[mq-bridge]
  B -- "chat completion" --> L[Ollama]
  L -- '{"team": "billing"}' --> B
  B --> S1[(tickets.billing)]
  B --> S2[(tickets.technical)]
  B --> S3[(tickets.account)]
  B --> S4[(tickets.triage)]
```

## Run it

Needs Docker, `curl` and `jq`. The first start downloads the model (about 1.9 GB).

```bash
docker compose up -d
curl -sS http://localhost:8080/tickets -H 'Content-Type: application/json' \
  -d '{"id": 7, "subject": "Refund", "body": "Please refund my last order, it never arrived."}'
docker compose exec redis redis-cli XRANGE tickets.billing - +
```

The `curl` returns once the ticket is in its stream, after the model answered: a few seconds on
a laptop CPU.

`./test.sh` posts the six tickets in [`tickets.jsonl`](tickets.jsonl) and checks where each one
lands; it is what CI executes. Ticket 6 tries to choose its own queue (`"admin"`); it still ends
up in one of the four streams. `docker compose down -v` removes everything.

## Files

| File | Purpose |
| :--- | :--- |
| [`routes.yaml`](routes.yaml) | The route: `http` input, `lookup` with `openai_chat`, `switch` to `redis_streams` |
| [`docker-compose.yml`](docker-compose.yml) | Redis 8.8, Ollama 0.12.3, mq-bridge-app 1.0.1 |
| [`tickets.jsonl`](tickets.jsonl) | Six sample tickets |

## Use a hosted model

Replace the `url` of `openai_chat` with the provider's base URL (leave it out for OpenAI), set
`model`, and add `api_key: "${env:OPENAI_API_KEY}"`. The prompt and the `switch` stay as they are.
