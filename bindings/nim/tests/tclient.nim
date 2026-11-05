import std/unittest
import mq_bridge

suite "publisher and consumer":
  test "publish then poll and commit":
    let publisher = Publisher.fromStr("memory: { topic: nim_poll }")
    let consumer = Consumer.fromStr("memory: { topic: nim_poll }")
    publisher.send("one")
    publisher.sendBatch([newMessage("two"), newMessage("three")])
    var seen: seq[string]
    while seen.len < 3:
      let batch = consumer.poll(max = 10, timeoutMs = 5000)
      check batch.len > 0
      for m in batch: seen.add m.payload
    check seen == @["one", "two", "three"]
    consumer.commit()

  test "poll times out with an empty batch":
    let consumer = Consumer.fromStr("memory: { topic: nim_empty }")
    let batch = consumer.poll(max = 1, timeoutMs = 50)
    check batch.len == 0
    consumer.ack(batch)

  test "bad config raises with the library's text":
    expect MqbError:
      discard Publisher.fromStr("not_an_endpoint: {}")
    try:
      discard Publisher.fromStr("not_an_endpoint: {}")
    except MqbError as e:
      check e.msg.len > 0
