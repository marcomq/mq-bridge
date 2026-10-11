# AWS SQS / SNS

Consumes from an **SQS** queue or publishes to an SQS queue / **SNS** topic.
Credentials come from the standard AWS provider chain (environment, profile,
instance role) unless supplied explicitly as query params.

## URL format

```text
aws://?queue_url=<sqs-url>&region=<region>
```

`aws://` and `aws-sqs://` are both accepted. The host is unused — the queue is
selected by `queue_url` (required for consumers, and for publishers unless
`topic_arn` is set for SNS). Point `endpoint_url` at LocalStack for local
testing.

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `aws:`.

```yaml
input:
  aws: { queue_url: "https://sqs.us-east-1.amazonaws.com/000000000000/orders", region: "us-east-1" }
output:
  aws: { topic_arn: "arn:aws:sns:us-east-1:000000000000:events", region: "us-east-1" }
```

## Examples

**Drain an SQS queue into a file, one-shot:**

```bash
mqb copy --drain \
  --from 'aws://?queue_url=https://sqs.us-east-1.amazonaws.com/1234/orders&region=us-east-1' \
  --to file:///data/orders.jsonl?format=json
```

**Publish a Postgres table to an SNS topic, one-shot:**

```bash
mqb copy --drain \
  --from postgres://user:pass@localhost/app?table=orders \
  --to 'aws://?topic_arn=arn:aws:sns:us-east-1:1234:orders&region=us-east-1'
```

**LocalStack, continuous:**

```bash
mqb copy \
  --from 'aws://?queue_url=http://localhost:4566/000000000000/orders&region=us-east-1&endpoint_url=http://localhost:4566&access_key=test&secret_key=test' \
  --to null:
```

> [!WARNING]
> `access_key` / `secret_key` in the URL are shown here only because LocalStack's
> `test`/`test` are throwaway values. Never put real credentials in a connector URL —
> they leak into shell history and process listings. Against real AWS, omit both and let
> the standard provider chain (env vars, `~/.aws/credentials`, IAM role) supply them; see
> [Secrets](../cookbook/secrets.md).

## Key options

| Option | Purpose |
|---|---|
| `queue_url` | SQS queue URL. Required for consumers; optional for publishers if `topic_arn` is set. |
| `topic_arn` | Publisher-only: SNS topic ARN to publish to. |
| `region` | AWS region (e.g. `us-east-1`). |
| `endpoint_url` | Custom endpoint (e.g. LocalStack). |
| `access_key` / `secret_key` / `session_token` | Explicit credentials (otherwise the AWS provider chain is used). |
| `max_messages` | Consumer-only: batch size per receive (1–10). |
| `wait_time_seconds` | Consumer-only: long-poll wait (0–20). |
| `binary_payload_mode` | Publisher-only: send the payload as base64 with the attribute `aws_binary_encoded`; a consumer decodes it. Needed for payloads that are not UTF-8 text. |

Metadata travels as message attributes. SQS carries at most 10 per message; an entry with
an empty value or a key with characters other than letters, digits, `_`, `-`, `.` is left out.

Full field list: [reference/aws.md](../reference/aws.md).
