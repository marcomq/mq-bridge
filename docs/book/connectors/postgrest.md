# PostgREST and Supabase

PostgREST turns a Postgres table into a REST resource, and Supabase's data API
is PostgREST. The generic [`http_bulk`](./http-bulk.md) output writes to it:
an upsert is a POST of a JSON array, a delete is a filter on the primary key.
Tested against PostgREST 16.4. A hosted Supabase project has not been tried.

Use this when only the REST API is reachable. With a database connection, the
[Postgres](./postgres.md) output writes the same rows directly and faster.

```yaml
output:
  http_bulk:
    url: http://localhost:3000
    headers:
      Prefer: resolution=merge-duplicates
    operation: "${metadata:postgres.operation}"
    upsert:
      path: /books
      format: json_array
      split_on_reject: true
    delete:
      method: DELETE
      path: /books?id=in.({ids})
      line: "{id}"
```

For Supabase, the documented form is the project URL with `/rest/v1` before the
table, and the key in two headers:

```yaml
output:
  http_bulk:
    url: https://your-project.supabase.co
    headers:
      apikey: <key>
      Authorization: Bearer <key>
      Prefer: resolution=merge-duplicates
    upsert:
      path: /rest/v1/books
      format: json_array
      split_on_reject: true
    delete:
      method: DELETE
      path: /rest/v1/books?id=in.({ids})
      line: "{id}"
```

## What to know

- **The table needs a primary key.** `resolution=merge-duplicates` updates the
  row with the same key; without the header a second write of a row fails.
- **A request is one transaction.** One row the table refuses, for example a
  text in an integer column, makes PostgREST refuse the whole request with the
  database's message. With `split_on_reject: true`, as above, the request is then
  sent again in halves until the refused row stands alone, and only that row fails.
  Without it every row of the request fails.
- **Every row of a request needs the same columns**, which is what a change
  stream of one table gives.
- **Text ids are quoted.** `line: "{id}"` writes each id of the `in` list as JSON, so
  `"a,b"` stays one value, as PostgREST's URL grammar asks for. Without `line` an id
  holding a comma cannot be deleted.
- **Row level security applies** to the role behind the key.

All fields and the behaviour on errors are on the [`http_bulk`](./http-bulk.md) page.
