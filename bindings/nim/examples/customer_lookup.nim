## Read orders from CSV, enrich the whole batch with one batched `IN (...)` SQL query,
## and write each order twice: as a JSON line and to a ZeroMQ PUSH socket.

import std/[json, options, os, osproc, strutils]
import mq_bridge

const here = currentSourcePath().parentDir
let outDir = here / "out"
let crmDb = outDir / "crm.db"
let results = outDir / "customers.jsonl"

proc merge(m: MessageView): Option[Message] =
  let order = m.json
  var customer = %*{"id": order["customer_id"].getStr.parseInt}
  if order{"crm"}.kind == JObject:
    for key, value in order["crm"]: customer[key] = value
  customer["vip"] = %(customer{"tier"}.getStr == "gold")
  some(newMessage(%*{"order_id": order["order_id"], "customer": customer}))

createDir(outDir)
removeFile(results)
removeFile(crmDb)
if execCmdEx("sqlite3 " & quoteShell(crmDb), input = readFile(here / "data" / "crm.sql")).exitCode != 0:
  quit "seeding the CRM database needs the sqlite3 command"

Route.fromStr("""
exit_on_empty: true
input:
  file: { path: "$1", format: csv }
  middlewares:
    - lookup:
        into: crm
        from: { sqlx: { url: "sqlite://$2", table: customers, lookup_query:
          "SELECT id, name, tier FROM customers WHERE id IN (${payload:customer_id})" } }
output:
  fanout:
    - file: { path: "$3", format: raw }
    - zeromq: { url: "tcp://127.0.0.1:5555", socket_type: push }
""".multiReplace(("$1", here / "data" / "orders.csv"), ("$2", crmDb), ("$3", results)))
  .withHandler(merge)
  .run()

echo readFile(results)
