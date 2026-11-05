## Enrich orders from two sources at once and merge the answers in Nim.
## The engine runs both lookups of a batch in parallel: one batched `IN (...)` query
## for the whole batch (CRM) and one HTTP call per order (loyalty service).

import std/[json, options, os, osproc, strutils]
import mq_bridge

const here = currentSourcePath().parentDir
let outDir = here / "out"
let crmDb = outDir / "crm.db"
let results = outDir / "customers.jsonl"

proc loyalty(m: MessageView): Option[Message] =
  ## GET /points/<id>. Customer 2 is not a member.
  let id = m.metadata("http_path").rsplit('/', 1)[^1].parseInt
  var answer = newMessage(%*{"points": id * 137 mod 1000})
  if id == 2:
    answer = newMessage("")
    answer.setMetadata("http_status_code", "404")
  some(answer)

proc merge(m: MessageView): Option[Message] =
  let order = m.json
  var customer = %*{"id": order["customer_id"]}
  for source in ["crm", "loyalty"]:
    if order{source}.kind == JObject:
      for key, value in order[source]: customer[key] = value
  customer["vip"] = %(customer{"tier"}.getStr == "gold" or customer{"points"}.getInt >= 500)
  some(newMessage(%*{"order_id": order["order_id"], "customer": customer}))

createDir(outDir)
removeFile(results)
removeFile(crmDb)
if execCmdEx("sqlite3 " & quoteShell(crmDb), input = readFile(here / "data" / "crm.sql")).exitCode != 0:
  quit "seeding the CRM database needs the sqlite3 command"

let loyaltyService = Route.fromStr("""
input: { http: { url: "127.0.0.1:18080" } }
output: { response: {} }
""")
loyaltyService.withHandler(loyalty)
loyaltyService.start()

Route.fromStr("""
exit_on_empty: true
input:
  file: { path: "$1", format: raw }
  middlewares:
    - lookup:
        entries:
          - into: crm
            from: { sqlx: { url: "sqlite://$2", table: customers, lookup_query:
              "SELECT id, name, tier FROM customers WHERE id IN (${payload:customer_id})" } }
          - into: loyalty
            from: { http: { url: "http://127.0.0.1:18080" } }
            metadata: { http_path: "/points/${payload:customer_id}", http_method: GET }
output:
  file: { path: "$3", format: raw }
""".multiReplace(("$1", here / "data" / "orders.jsonl"), ("$2", crmDb), ("$3", results)))
  .withHandler(merge)
  .run()

loyaltyService.stop()
loyaltyService.join()
echo readFile(results)
