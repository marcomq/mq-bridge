import std/[unittest, options, strutils, locks, os]
import mq_bridge

let mainThread = getThreadId()

proc upper(m: MessageView): Option[Message] =
  doAssert getThreadId() == mainThread
  some newMessage(m.payload.toUpperAscii)

suite "route":
  test "handler runs on the calling thread while it polls":
    let route = Route.fromStr("""
input: { memory: { topic: nim_route_in } }
output: { memory: { topic: nim_route_out } }
""").withHandler(upper)
    route.start()
    let publisher = Publisher.fromStr("memory: { topic: nim_route_in }")
    let consumer = Consumer.fromStr("memory: { topic: nim_route_out }")
    publisher.send("shout")
    let batch = consumer.poll(max = 1, timeoutMs = 5000)
    check batch.len == 1
    check batch[0].payload == "SHOUT"
    consumer.commit()
    route.stop()
    route.join()

  test "closure handler and batch handler":
    var calls = 0
    let route = Route.fromStr("""
input: { memory: { topic: nim_batch_in } }
output: { memory: { topic: nim_batch_out } }
""")
    route.withBatchHandler(proc (ms: seq[MessageView]): seq[Option[Message]] =
      for m in ms:
        inc calls
        result.add some newMessage(m.payload & "!"))
    route.start()
    let publisher = Publisher.fromStr("memory: { topic: nim_batch_in }")
    let consumer = Consumer.fromStr("memory: { topic: nim_batch_out }")
    publisher.send("a")
    publisher.send("b")
    var seen: seq[string]
    while seen.len < 2:
      for m in consumer.poll(max = 2, timeoutMs = 5000): seen.add m.payload
    check seen == @["a!", "b!"]
    check calls == 2

  test "request-reply from the thread that runs the handler":
    let route = Route.fromStr("""
input: { memory: { topic: nim_rpc, request_reply: true } }
output: { response: {} }
""").withHandler(upper)
    route.start()
    let publisher = Publisher.fromStr("memory: { topic: nim_rpc, request_reply: true }")
    check publisher.request("ping").payload == "PING"

  test "worker thread runs a closure handler":
    var lock: Lock
    initLock(lock)
    var threads: seq[int]
    let route = Route.fromStr("""
input: { memory: { topic: nim_worker_in } }
output: { memory: { topic: nim_worker_out } }
""").withHandler(proc (m: MessageView): Option[Message] =
      withLock lock: threads.add getThreadId()
      none(Message))
    route.start(worker = true)
    let publisher = Publisher.fromStr("memory: { topic: nim_worker_in }")
    publisher.send("x")
    publisher.send("y")
    var calls = 0
    for _ in 0 ..< 500:
      withLock lock: calls = threads.len
      if calls == 2: break
      sleep(10)
    route.stop()
    route.join()
    withLock lock:
      check threads.len == 2
      check threads[0] != mainThread
      check threads[0] == threads[1]

  test "drained file route runs to the end":
    let dir = getTempDir()
    writeFile(dir & "nim_in.jsonl", "a\nb\nc\n")
    var count = 0
    let route = Route.fromStr("input: { file: { path: \"" & dir & "nim_in.jsonl\" } }\n" &
      "output: null\nexit_on_empty: true\n")
    route.withHandler(proc (m: MessageView): Option[Message] =
      inc count
      none(Message))
    route.run()
    check count == 3

  test "a retryable error redelivers":
    var attempts = 0
    let route = Route.fromStr("""
input: { memory: { topic: nim_retry_in, enable_nack: true } }
output: { memory: { topic: nim_retry_out } }
""").withHandler(proc (m: MessageView): Option[Message] =
      inc attempts
      if attempts == 1: raise newException(RetryableError, "try again")
      some m.toMessage)
    route.start()
    Publisher.fromStr("memory: { topic: nim_retry_in }").send("once")
    let batch = Consumer.fromStr("memory: { topic: nim_retry_out }").poll(1, 10000)
    check batch.len == 1
    check attempts >= 2
