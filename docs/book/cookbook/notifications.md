# SMS & email notifications

mq-bridge has no SMS or email endpoint of its own. It sends notifications
through the service that delivers them, using one of three outputs:

| Way out | Output | Pick it when |
| :--- | :--- | :--- |
| A notification gateway (Apprise, ntfy) | `http` | You want one route for many channels, and to change provider without changing the route |
| The provider's REST API | `http` or `http_bulk` | You use one provider (SendGrid, Postmark, SES, an API Gateway of your own) |
| An AWS SNS topic | `aws` | A fixed group of phones or addresses gets every message |

Whichever you pick, the message goes to the provider with the retries,
dead-letter queue and back-pressure of an ordinary route.

## `http` or `http_bulk`

| | [`http`](../connectors/http.md) | [`http_bulk`](../connectors/http-bulk.md) |
| :--- | :--- | :--- |
| Requests | One per message | One per batch |
| Body | The payload, shaped by a [`transform`](transform.md) | Payloads in an array, with `envelope` and `document` templates around them |
| Credentials | Fixed headers, `basic_auth` | Fixed headers, OAuth2 client credentials, AWS SigV4 |
| Result | The status of each request | The status, or per recipient with `upsert.result.items` |
| Fits | An API that sends one notification per call | An API that sends many in one call, or needs OAuth2 or SigV4 |

Both send to a fixed URL, so the recipient goes in the body, not in the path.
`transform` builds objects but not arrays: an API that wants
`"to": [{"email": …}]` from `http` needs the payload in that shape already, or an
`http_bulk` `document` template. Neither output sends form-encoded bodies, so an
API that takes only those (Twilio's own) is reached through a gateway.

## Through a notification gateway

[Apprise API](https://github.com/caronc/apprise-api) takes one JSON request
and passes it to email (SMTP, SES, SendGrid, …), SMS (Twilio, Vonage, SNS, …)
or chat. Which services and recipients a message reaches is configured in
Apprise, so the route stays the same when the provider changes.

With the recipients stored in Apprise under the key `alerts`, each with a tag
such as `ops` or `billing`, the message picks its audience by tag:

```yaml
alerts_notify:
  input: { kafka: { topic: "alerts", url: "localhost:9092" } }
  output:
    middlewares:
      - transform:
          mapping:
            tag: "$.team"
            title: "$.subject"
            body: "$.text"
      - retry: { max_attempts: 5, initial_interval_ms: 500 }
      - dlq:
          endpoint: { file: { path: "undelivered-alerts.jsonl" } }
    http:
      url: "http://apprise:8000/notify/alerts"
      custom_headers:
        Content-Type: "application/json"
```

A message `{"team": "ops", "subject": "Disk full", "text": "db-1 at 97%"}`
becomes `{"tag": "ops", "title": "Disk full", "body": "db-1 at 97%"}`.
Set `Content-Type` yourself: the `http` output sends none of its own.

[ntfy](https://ntfy.sh) is the lighter choice for push notifications to phones:
the payload is the message text, and the topic is the path of `url`.

## Through the provider's API

### One notification per request: `http`

An API Gateway of your own in front of SES, SNS or a Lambda, or a mail API
such as Postmark's `/email`, takes one notification per request. Put the key in
a header and shape the body with a `transform`:

```yaml
receipts_mail:
  input: { kafka: { topic: "receipts", url: "localhost:9092" } }
  output:
    middlewares:
      - transform:
          mapping:
            To: "$.email"
            Subject: "$.subject"
            TextBody: "$.text"
            From: { path: "$.from", default: "shop@example.com" }
      - retry: { max_attempts: 5, initial_interval_ms: 500 }
      - dlq:
          endpoint: { file: { path: "unsent-receipts.jsonl" } }
    http:
      url: "https://api.postmarkapp.com/email"
      custom_headers:
        Content-Type: "application/json"
        Accept: "application/json"
        X-Postmark-Server-Token: "${env:POSTMARK_TOKEN}"
```

`From` falls back to the `default` when the message has none. For an API
Gateway the same shape applies, with its `x-api-key` header in place of the
token. A gateway that checks IAM signatures cannot be called this way: `http`
does not sign requests; use `http_bulk` or put an API key or a Lambda authorizer
on the gateway.

### Many notifications per request: `http_bulk`

SendGrid sends one template to up to 1,000 recipients per request, each with
data of its own. With `http_bulk` each message becomes one recipient, and the
message itself is the template data:

```yaml
welcome_mails:
  batch_size: 1000
  input: { kafka: { topic: "signups", url: "localhost:9092" } }
  output:
    middlewares:
      - retry: { max_attempts: 5, initial_interval_ms: 1000 }
      - dlq:
          endpoint: { file: { path: "unsent-welcome.jsonl" } }
    http_bulk:
      url: https://api.sendgrid.com
      headers:
        Authorization: "Bearer ${env:SENDGRID_API_KEY}"
      upsert:
        path: /v3/mail/send
        format: json_array
        envelope: '{"from": {"email": "hello@example.com"}, "template_id": "d-0123456789abcdef", "personalizations": {documents}}'
        document: '{"to": [{"email": "${payload:email}"}], "dynamic_template_data": {document}}'
```

A batch of signups `{"email": "ada@example.com", "name": "Ada"}` is one request
whose `personalizations` hold one entry per signup. SendGrid answers the
request as a whole, so a refused request fails every message in it; the route's
`batch_size` must stay within the API's limit, since `http_bulk` has no limit of
its own on documents per request.

Amazon SES is called the same way, with requests signed by the AWS credentials
of the environment instead of a header. `SendBulkEmail` takes up to 50
recipients and reports each one, so `result.items` fails only the recipients
SES refused:

```yaml
    http_bulk:
      url: https://email.eu-central-1.amazonaws.com
      auth:
        aws_sigv4: { region: eu-central-1, service: ses }
      upsert:
        path: /v2/email/outbound-bulk-emails
        format: json_array
        envelope: '{"FromEmailAddress": "hello@example.com", "DefaultContent": {"Template": {"TemplateName": "welcome", "TemplateData": "{}"}}, "BulkEmailEntries": {documents}}'
        result:
          items: { path: /BulkEmailEntryResults, error: /Error }
```

Here every payload must already be one SES entry, such as
`{"Destination": {"ToAddresses": ["ada@example.com"]}, "ReplacementEmailContent": {"ReplacementTemplate": {"ReplacementTemplateData": "{\"name\":\"Ada\"}"}}}`:
SES wants the template data as a JSON string, which a `document` template
cannot produce from the payload. Set the route's `batch_size` to 50 at most.

The request bodies of the Apprise and SendGrid routes were checked against a
local HTTP server; the SES example for its configuration only. None of the
services themselves was called.

## Through an AWS SNS topic

An [`aws`](../connectors/aws.md) output with a `topic_arn` publishes every
message to an SNS topic. Phone numbers (SMS) and email addresses subscribed to
the topic in AWS each receive it:

```yaml
alerts_sns:
  input: { kafka: { topic: "alerts", url: "localhost:9092" } }
  output:
    aws:
      topic_arn: "arn:aws:sns:eu-central-1:123456789012:ops-alerts"
      region: "eu-central-1"
```

On the command line: `--to 'aws://?topic_arn=arn:aws:sns:eu-central-1:123456789012:ops-alerts&region=eu-central-1'`.

- Every subscriber gets every message. To reach different people, use one topic
  per audience and a [`switch`](switch.md) to choose between them.
- The payload is the text that is sent. Emails arrive as plain text with the
  subject "AWS Notification", and each address must confirm its subscription
  before it receives anything.
- Message metadata is sent as SNS message attributes, which subscription filter
  policies can match.

This example was checked for its configuration only.

## Replies and delivery reports

Notifications have answers too: delivery reports, bounces, SMS replies.

- Providers that call a webhook (SendGrid, Postmark, Twilio, ntfy) reach an
  [`http`](../connectors/http.md) input, which writes each call on to any output.
- SES reports bounces and complaints to an SNS topic. Subscribe an SQS queue to
  it and read the queue with an [`aws`](../connectors/aws.md) input with
  `queue_url`.
