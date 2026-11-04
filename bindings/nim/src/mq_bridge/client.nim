import std/json
import ./[raw, errors, executor, message]

type
  PublisherObj = object
    raw: ptr mqb_publisher_t
  Publisher* = ref PublisherObj
    ## Publishes to one output endpoint.

  ConsumerObj = object
    raw: ptr mqb_consumer_t
  Consumer* = ref ConsumerObj
    ## A pull-based consumer over one input endpoint. A polled batch stays
    ## outstanding until it is acked, nacked or committed.

  BatchObj = object
    raw: ptr mqb_batch_t
  Batch* = ref BatchObj
    ## The messages of one `poll`. Freeing it does not acknowledge them.

proc `=destroy`(p: PublisherObj) =
  if p.raw != nil:
    blocking mqb_publisher_free(p.raw)

proc `=destroy`(c: ConsumerObj) =
  if c.raw != nil:
    blocking mqb_consumer_free(c.raw)

proc `=destroy`(b: BatchObj) =
  if b.raw != nil:
    mqb_batch_free(b.raw)

proc `=copy`(dst: var PublisherObj, src: PublisherObj) {.error.}
proc `=copy`(dst: var ConsumerObj, src: ConsumerObj) {.error.}
proc `=copy`(dst: var BatchObj, src: BatchObj) {.error.}
proc `=dup`(src: PublisherObj): PublisherObj {.error.}
proc `=dup`(src: ConsumerObj): ConsumerObj {.error.}
proc `=dup`(src: BatchObj): BatchObj {.error.}

proc fromStr*(T: typedesc[Publisher], config: string, name = ""): Publisher =
  ## A publisher from YAML or JSON text. `name` selects an entry of a
  ## `publishers:` document.
  Publisher(raw: checkHandle blocking mqb_publisher_from_str(cstring(config), cstring(name)))

proc fromFile*(T: typedesc[Publisher], path: string, name = ""): Publisher =
  Publisher(raw: checkHandle blocking mqb_publisher_from_file(cstring(path), cstring(name)))

proc send*(p: Publisher, message: Message) =
  ## Publishes one message and waits for the endpoint to accept it.
  check blocking mqb_publisher_send(p.raw, message.raw)

proc send*(p: Publisher, payload: string | JsonNode) =
  p.send(newMessage(payload))

proc sendBatch*(p: Publisher, messages: openArray[Message]) =
  ## Publishes the messages in one call; fails if any was rejected.
  if messages.len == 0:
    return
  var pointers = newSeq[ptr mqb_message_t](messages.len)
  for i in 0 ..< messages.len:
    pointers[i] = messages[i].raw
  let arr = cast[ptr UncheckedArray[ptr mqb_message_t]](addr pointers[0])
  check blocking mqb_publisher_send_batch(p.raw, arr, csize_t(pointers.len))

proc request*(p: Publisher, message: Message): Message =
  ## Sends a request and returns the reply. Needs an endpoint with request-reply.
  var response: ptr mqb_message_t
  check blocking mqb_publisher_request(p.raw, message.raw, addr response)
  result.raw = response

proc request*(p: Publisher, payload: string | JsonNode): Message =
  p.request(newMessage(payload))

proc fromStr*(T: typedesc[Consumer], config: string, name = ""): Consumer =
  ## A consumer from YAML or JSON text. `name` selects an entry of a
  ## `consumers:` document.
  Consumer(raw: checkHandle blocking mqb_consumer_from_str(cstring(config), cstring(name)))

proc fromFile*(T: typedesc[Consumer], path: string, name = ""): Consumer =
  Consumer(raw: checkHandle blocking mqb_consumer_from_file(cstring(path), cstring(name)))

proc poll*(c: Consumer, max = 100, timeoutMs = -1): Batch =
  ## Receives up to `max` messages. `timeoutMs < 0` waits until something
  ## arrives. The batch is empty after a timeout or once the source is exhausted.
  var batch: ptr mqb_batch_t
  check blocking mqb_consumer_poll(c.raw, uint32(max), int64(timeoutMs), addr batch)
  Batch(raw: batch)

proc len*(b: Batch): int =
  if b.raw == nil: 0 else: int(mqb_batch_count(b.raw))

proc `[]`*(b: Batch, i: int): MessageView =
  if i < 0 or i >= b.len:
    raise newException(IndexDefect, "batch index " & $i & " out of range")
  MessageView(raw: mqb_batch_at(b.raw, csize_t(i)))

iterator items*(b: Batch): MessageView =
  for i in 0 ..< b.len:
    yield MessageView(raw: mqb_batch_at(b.raw, csize_t(i)))

proc token*(b: Batch): uint32 =
  if b.raw == nil: 0'u32 else: mqb_batch_token(b.raw)

proc ack*(c: Consumer, b: Batch) =
  ## Acknowledges one batch. An empty batch is a no-op.
  if b.len > 0:
    check blocking mqb_consumer_ack(c.raw, b.token)

proc nack*(c: Consumer, b: Batch) =
  ## Negatively acknowledges one batch so the broker can redeliver it.
  if b.len > 0:
    check blocking mqb_consumer_nack(c.raw, b.token)

proc nackAll*(c: Consumer) =
  check blocking mqb_consumer_nack_all(c.raw)

proc commit*(c: Consumer) =
  ## Acknowledges every outstanding batch, oldest first.
  check blocking mqb_consumer_commit(c.raw)

proc status*(c: Consumer): JsonNode =
  ## Status snapshot of the endpoint (`healthy`, `target`, ...).
  let text = checkHandle blocking mqb_consumer_status_json(c.raw)
  try:
    result = parseJson($text)
  finally:
    mqb_string_free(text)

proc exhausted*(c: Consumer): bool =
  ## True once the source signalled end-of-stream.
  mqb_consumer_exhausted(c.raw)

proc close*(c: Consumer) =
  ## Releases the endpoint connection; polling fails afterwards.
  check blocking mqb_consumer_close(c.raw)
