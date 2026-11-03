# rust-mqbridge-marcomq

Rust on [mq-bridge](https://github.com/marcomq/mq-bridge), a message and data bridge library.
The whole API is one mq-bridge route: an `http` input, a batch handler on SQLite, and a
`response` output. The handler (`src/main.rs`, ~350 lines) is the only code; HTTP, connection
handling and batching come from the library.

| | |
|---|---|
| Language | Rust 1.99.1 |
| Framework | mq-bridge 1.3.0 (hyper 1 + tokio underneath) |
| SQLite driver | sqlx 0.9 `sqlite`, SQLite bundled (`libsqlite3-sys`) |
| Allocator | mimalloc |
| Nginx | **No, direct.** Set `HOST=0.0.0.0 PORT=80` |

## How it works

- **Group commit.** The handler implements `handle_many` and `prefers_batches`. Requests that
  arrive while a batch is running reach the handler together as the next batch, and the whole
  batch runs in one transaction (`BEGIN IMMEDIATE` when it holds a write). Every response is
  sent after `COMMIT`, so a 201 means the row is durable (rule 6). At most 512 requests are in
  flight at a time (`concurrency_limit`, mq-bridge's default is 100), so a batch has at most
  512 requests.
- **One SQLite connection**, WAL, `synchronous=NORMAL`, `mmap_size` 1 GiB. With a single writer
  there is no `database is locked`, and batching makes up for the single connection.
- **SQLite builds the JSON.** Each query returns the finished response body via `json_object`
  (key order as in the spec), so no row is decoded into Rust values.
- **Feed:** the reference query, with `json_group_array` over a subquery that is already sorted,
  so SQLite doesn't sort a second time.
- **Like:** one `INSERT … SELECT … WHERE EXISTS (post) ON CONFLICT DO NOTHING`. The post is
  looked up separately only when nothing was inserted, to tell 200 from 404.
- **JWT:** HS256 is checked on every write request with `hmac`/`sha2`, before the id is parsed
  (auth first, then id). Nothing is cached.

Every request reads from SQLite while it is served; prepared statements are reused by sqlx's
statement cache.

## Local numbers (Apple M1, not the benchmark box)

CPU time of the server process per request, 64 connections with keep-alive: feed 41 µs,
post 20 µs, like 35 µs, create 30 µs. These are not k6 numbers and were not measured on the
challenge box.
