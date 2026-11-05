## Classify support tickets with an LLM, or stream one answer token by token.
##   llm_tickets             classify data/tickets.jsonl, all fifteen tickets in one request
##   llm_tickets "question"  stream the answer to one question
## Talks to Ollama on localhost by default; LLM_URL, LLM_MODEL and OPENAI_API_KEY
## point it at any OpenAI-compatible API.

import std/[json, options, os, sequtils, strutils, tables]
import mq_bridge

const here = currentSourcePath().parentDir
let llm = "url: \"" & getEnv("LLM_URL", "http://localhost:11434") & "\"\n" &
          "model: \"" & getEnv("LLM_MODEL", "qwen3.5:2b") & "\"\n" &
          (if existsEnv("OPENAI_API_KEY"): "api_key: \"${env:OPENAI_API_KEY}\"\n" else: "")
var counts: CountTable[string]

proc triage(m: MessageView): Option[Message] =
  ## Never trust the model blindly: unknown labels fall back, keywords escalate.
  let ticket = m.json
  let answer = ticket{"llm"}
  var category = answer{"category"}.getStr.toLowerAscii
  if category notin ["billing", "bug", "account", "feature"]: category = "other"
  var priority = answer{"priority"}.getStr.toLowerAscii
  if priority notin ["low", "medium", "high"]: priority = "medium"
  let text = toLowerAscii(ticket["subject"].getStr & " " & ticket["body"].getStr)
  if ["outage", "down", "urgent", "gone"].anyIt(it in text): priority = "high"
  counts.inc(category & "/" & priority)
  some(newMessage(%*{"id": ticket["id"], "category": category, "priority": priority,
                     "summary": answer{"summary"}.getStr(ticket["subject"].getStr)}))

proc classify() =
  removeFile(here / "out" / "tickets.jsonl")
  Route.fromStr("""
exit_on_empty: true
batch_size: 100
input:
  file: { path: "$1", format: raw }
  middlewares:
    - lookup:
        into: llm
        from:
          openai_chat:
            $2
            messages_per_request: 15
            concurrency: 4
            json: true
            options: { temperature: 0, reasoning_effort: none }
            system: $4
            prompt: "${payload:subject}: ${payload:body}"
output:
  file: { path: "$3", format: raw }
""".multiReplace(("$1", here / "data" / "tickets.jsonl"),
                 ("$2", llm.replace("\n", "\n            ")),
                 ("$3", here / "out" / "tickets.jsonl"),
                 ("$4", $(%readFile(here / "data" / "ticket_policy.txt").strip))))
    .withHandler(triage)
    .run()
  for label, n in counts: echo label, ": ", n

proc stream(question: string) =
  let tokens = Consumer.fromStr("memory: { topic: llm_tokens }")
  let chat = Route.fromStr("input: { memory: { topic: llm_questions } }\noutput:\n" &
    "  openai_chat:\n    prompt: \"${message:payload}\"\n" &
    "    stream_to: { memory: { topic: llm_tokens } }\n" & llm.indent(4))
  chat.start()
  Publisher.fromStr("memory: { topic: llm_questions }").send(question)
  var done = false
  while not done:
    let batch = tokens.poll(max = 100, timeoutMs = 60_000)
    if batch.len == 0: break
    for token in batch:
      stdout.write token.payload
      done = done or token.metadata("mqb.stream.end") == "true"
    stdout.flushFile
    tokens.ack(batch)
  echo ""
  chat.stop()
  chat.join()

createDir(here / "out")
if paramCount() > 0: stream(commandLineParams().join(" ")) else: classify()
