## Generate access-log events with Redpanda Connect, filter and flag them in Nim,
## and write the result as JSON lines. Needs the Connect plugin installed:
## `brew install marcomq/tap/mq-bridge-connect` or `conda install -c marcomq
## mq-bridge-connect`.

import std/[json, options, os, strutils, tables]
import mq_bridge

const here = currentSourcePath().parentDir
var hits: CountTable[string]

proc triage(m: MessageView): Option[Message] =
  let event = m.json
  let path = event["path"].getStr
  if path == "/health":
    return none(Message)                             # acknowledge, write nothing
  hits.inc path
  let status = event["status"].getInt
  let latency = event["latency_ms"].getInt
  event["slow"] = %(latency > 500)
  event["error"] = %(status >= 500)
  some(newMessage(event))

loadInstalledPlugin("connect")                      # load connect plugin

let output = here / "out" / "access_log.jsonl"
createDir(output.parentDir)
removeFile(output)

Route.fromStr("""
exit_on_empty: true
input:
  connect:
    connector: generate
    count: 20
    interval: ""
    mapping: |
      root.path = ["/", "/api/orders", "/api/users", "/health", "/static/app.js"].index(random_int(seed: 1, max: 4))
      root.status = [200, 200, 200, 404, 503].index(random_int(seed: 2, max: 4))
      root.latency_ms = random_int(seed: 3, min: 1, max: 900)
      root.at = now()
output:
  file: { path: "$1", format: raw }
""".replace("$1", output))
  .withHandler(triage)
  .run()

echo readFile(output)
echo "requests per path: ", hits
