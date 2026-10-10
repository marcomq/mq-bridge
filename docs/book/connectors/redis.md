# Redis Streams

Publishes to or consumes from a Redis **stream** (`XADD` / `XREADGROUP`).
Consumers use a consumer group by default for durable, acked delivery;
`subscriber_mode=true` reads ephemerally via `XREAD`.

## URL format

```text
redis://[user:pass@]host[:port]?stream=<key>
```

`redis://` (plain) and `rediss://` (TLS) are both accepted; `redis_streams://`
is an explicit alias. If `stream` is omitted it defaults to the route name.

## TLS

A `rediss://` URL, or `tls.required: true`, connects with TLS and checks the server certificate
and host name.

| Setting | Effect |
|---|---|
| none | The certificate is checked against the public roots, as managed Redis services need. |
| `tls.ca_file` | Trusts that CA instead. Needed for a server with a private or self-signed CA. |
| `tls.cert_file` and `tls.key_file` | Presents a client certificate (PEM), for a server with `tls-auth-clients yes`. |

`tls.accept_invalid_certs` is ignored: the certificate is always checked.

```yaml
output:
  redis_streams:
    url: "rediss://redis.internal:6380"
    stream: "orders"
    tls: { ca_file: /etc/mqb/ca.pem, cert_file: /etc/mqb/client.pem, key_file: /etc/mqb/client.key }
```

```bash
mqb copy --drain \
  --from file:///data/events.csv?format=csv \
  --to 'rediss://redis.internal:6380?stream=events&tls={"ca_file":"/etc/mqb/ca.pem"}'
```

## Config (YAML / library)

The same settings as a route endpoint in a config file, or in `Route.from_config` /
`fromConfig`. Every URL query parameter is a field of the same name under `redis_streams:`.

```yaml
input:
  redis_streams: { url: "redis://localhost:6379", stream: "orders", group: "mqb" }
```

## Examples

**Load a CSV file into a stream, one-shot:**

```bash
mqb copy --drain \
  --from file:///data/events.csv?format=csv \
  --to redis://localhost:6379?stream=events
```

**Consume with a durable group into ClickHouse, continuous:**

```bash
mqb copy \
  --from 'redis://localhost:6379?stream=events&group=analytics&consumer_name=w1' \
  --to 'clickhouse://localhost:8123?table=events&database=analytics'
```

Unclaimed entries pending longer than `redelivery_timeout_ms` (default 60s)
are re-delivered via `XAUTOCLAIM`.

**Ephemeral tail of new messages (no group, no acks), continuous:**

```bash
mqb copy \
  --from 'redis://localhost:6379?stream=events&subscriber_mode=true' \
  --to kafka://kafka.local:9092?topic=events
```

## Key options

| Option | Purpose |
|---|---|
| `stream` | Stream key to publish to / read from. Defaults to the route name. |
| `group` | Consumer-only: group name. Defaults to `{APP_NAME}-{stream}`. |
| `consumer_name` | Consumer-only: consumer within the group. |
| `subscriber_mode` | Consumer-only: ephemeral `XREAD` from new messages (no group/acks). |
| `read_from_start` | Consumer-only: on group creation, start from the beginning (`0`) not `$`. |
| `redelivery_timeout_ms` | Consumer-only: re-claim entries pending ≥ this long; `0` disables. |
| `reader_connections` | Consumer-only: parallel `XREADGROUP` readers across the group. |
| `maxlen` + `approx_trim` | Publisher-only: cap stream length with `XADD MAXLEN`. |
| `username` / `password` | Authentication (Redis ACL). |

Full field list: [reference/redis.md](../reference/redis.md).
