import std/[unittest, json, options, locks, strutils]
import mq_bridge

type
  Gen = ref object of PluginConsumer
    payloads: seq[string]
    sent: bool
  Sink = ref object of PluginPublisher
  DropPing = ref object of PluginMiddleware
  Reuse = ref object of PluginConsumer
    batch: PluginBatch

var
  lock: Lock
  received: seq[string]
  dispositions: seq[Disposition]
  pluginThreads: seq[int]

initLock(lock)

proc record(payloads: seq[string] = @[], ds: seq[Disposition] = @[]) =
  {.cast(gcsafe).}:
    withLock lock:
      received.add payloads
      dispositions.add ds
      pluginThreads.add getThreadId()

method receive(g: Gen, max: int): PluginBatch =
  if g.sent:
    raise newException(EndOfStreamError, "done")
  g.sent = true
  var messages: seq[PluginMessage]
  for p in g.payloads: messages.add toPluginMessage(p, [("source", "gen")])
  newPluginBatch(messages)

method receive(r: Reuse, max: int): PluginBatch =
  if r.batch == nil: r.batch = newPluginBatch(@[toPluginMessage("again")])
  r.batch

method commit(g: Gen, batch: PluginBatch, ds: seq[Disposition]) =
  record(ds = ds)

method send(s: Sink, messages: seq[PluginMessage]) =
  var payloads: seq[string]
  for m in messages:
    if m.payload == "fail": raise newException(RetryableError, "sink refused")
    payloads.add m.payload & "/" & m.metadata("source", "-")
  record(payloads)

method apply(d: DropPing, messages: seq[PluginMessage]): seq[Option[PluginMessage]] =
  for m in messages:
    result.add(if m.payload == "ping": none(PluginMessage) else: some(m))

registerPlugin Plugin(name: "nim_gen", newConsumer: proc (route, config: string): PluginConsumer =
  var payloads: seq[string]
  for p in parseJson(config)["payloads"]: payloads.add p.getStr
  Gen(payloads: payloads))
registerPlugin Plugin(name: "nim_sink", newPublisher: proc (route, config: string): PluginPublisher =
  Sink())
registerPlugin Plugin(name: "nim_reuse", newConsumer: proc (route, config: string): PluginConsumer =
  Reuse())
registerPlugin Plugin(name: "nim_drop_ping",
  newMiddleware: proc (route, config: string, side: MiddlewareSide): PluginMiddleware =
    DropPing())

proc reset() =
  withLock lock:
    received.setLen 0
    dispositions.setLen 0
    pluginThreads.setLen 0

suite "Nim plugins":
  test "consumer, middleware and publisher in one route":
    reset()
    Route.fromStr("""
input:
  custom: { name: nim_gen, config: { payloads: [ping, a, ping, b] } }
output:
  custom: { name: nim_sink, config: {} }
  middlewares:
    - custom: { name: nim_drop_ping, config: {} }
exit_on_empty: true
""").run()
    withLock lock:
      check received == @["a/gen", "b/gen"]
      check dispositions == @[dAck, dAck, dAck, dAck]
      check pluginThreads.len > 0
      for t in pluginThreads: check t != getThreadId()

  test "a plugin publisher used directly reports its errors":
    reset()
    let publisher = Publisher.fromStr("custom: { name: nim_sink, config: {} }")
    publisher.send("x")
    expect RetryableError:
      publisher.send("fail")
    withLock lock:
      check received == @["x/-"]

  test "a batch returned twice is rejected":
    let consumer = Consumer.fromStr("custom: { name: nim_reuse, config: {} }")
    check consumer.poll(max = 10, timeoutMs = 1000).len == 1
    try:
      discard consumer.poll(max = 10, timeoutMs = 1000)
      check false
    except MqbError as e:
      check "returned before" in e.msg

  test "a plugin name registered twice is rejected":
    expect MqbError:
      registerPlugin Plugin(name: "nim_sink", newPublisher: proc (route, config: string): PluginPublisher =
        Sink())
