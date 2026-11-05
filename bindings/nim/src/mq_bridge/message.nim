import std/json
import ./[raw, errors]

type
  Message* = object
    ## An owned message: payload bytes, string metadata and an id. Copying it
    ## copies the message.
    raw*: ptr mqb_message_t

  MessageView* = object
    ## A message borrowed from the library, valid for the handler call or
    ## until its batch is freed. `toMessage` keeps a copy.
    raw*: ptr mqb_message_t

  AnyMessage* = Message | MessageView

proc toStr*(s: MqbSlice): string =
  result = newString(s.len)
  if s.len > 0:
    copyMem(addr result[0], s.p, s.len)

proc `=destroy`*(m: Message) =
  if m.raw != nil:
    mqb_message_free(m.raw)

proc `=wasMoved`*(m: var Message) =
  m.raw = nil

proc copyRaw(src: ptr mqb_message_t): ptr mqb_message_t =
  let payload = mqb_message_payload(src)
  result = checkHandle mqb_message_new(payload.p, payload.len)
  try:
    let id = toStr(mqb_message_id(src))
    if id.len > 0:
      check mqb_message_set_id(result, cstring(id))
    for i in 0 ..< mqb_message_metadata_count(src):
      var key, value: MqbSlice
      check mqb_message_metadata_at(src, i, addr key, addr value)
      check mqb_message_set_metadata(result, cstring(toStr(key)), cstring(toStr(value)))
  except MqbError:
    mqb_message_free(result)
    raise

proc `=copy`*(dst: var Message, src: Message) =
  if dst.raw == src.raw:
    return
  let copied = if src.raw == nil: nil else: copyRaw(src.raw)
  `=destroy`(dst)
  dst.raw = copied

proc `=dup`*(src: Message): Message =
  result.raw = if src.raw == nil: nil else: copyRaw(src.raw)

proc newMessage*(payload: openArray[byte] | string, id = "",
                 metadata: openArray[(string, string)] = []): Message =
  ## A message with a copy of `payload`. Without `id` the library generates one.
  var data: pointer = nil
  if payload.len > 0:
    data = unsafeAddr payload[0]
  result.raw = checkHandle mqb_message_new(data, csize_t(payload.len))
  if id.len > 0:
    check mqb_message_set_id(result.raw, cstring(id))
  for (key, value) in metadata:
    check mqb_message_set_metadata(result.raw, cstring(key), cstring(value))

proc newMessage*(data: JsonNode, id = "",
                 metadata: openArray[(string, string)] = []): Message =
  newMessage($data, id, metadata)

proc toMessage*(m: MessageView): Message =
  result.raw = copyRaw(m.raw)

proc view*(m: Message): MessageView =
  MessageView(raw: m.raw)

proc isNil*(m: AnyMessage): bool = m.raw == nil

proc payload*(m: AnyMessage): string =
  toStr(mqb_message_payload(m.raw))

proc payloadBytes*(m: AnyMessage): seq[byte] =
  let s = mqb_message_payload(m.raw)
  result = newSeq[byte](s.len)
  if s.len > 0:
    copyMem(addr result[0], s.p, s.len)

proc payloadView*(m: AnyMessage): MqbSlice =
  ## The payload without a copy; valid until the message is changed or freed.
  mqb_message_payload(m.raw)

proc json*(m: AnyMessage): JsonNode =
  parseJson(m.payload)

proc id*(m: AnyMessage): string =
  toStr(mqb_message_id(m.raw))

proc metadata*(m: AnyMessage, key: string, default = ""): string =
  let s = mqb_message_metadata(m.raw, cstring(key))
  if s.p == nil: default else: toStr(s)

proc hasMetadata*(m: AnyMessage, key: string): bool =
  mqb_message_metadata(m.raw, cstring(key)).p != nil

iterator metadataPairs*(m: AnyMessage): (string, string) =
  for i in 0 ..< mqb_message_metadata_count(m.raw):
    var key, value: MqbSlice
    check mqb_message_metadata_at(m.raw, i, addr key, addr value)
    yield (toStr(key), toStr(value))

proc metadata*(m: AnyMessage): seq[(string, string)] =
  for pair in m.metadataPairs: result.add pair

proc setId*(m: var Message, id: string) =
  check mqb_message_set_id(m.raw, cstring(id))

proc setMetadata*(m: var Message, key, value: string) =
  check mqb_message_set_metadata(m.raw, cstring(key), cstring(value))

proc `$`*(m: AnyMessage): string =
  if m.raw == nil: "Message(nil)"
  else: "Message(id: " & m.id & ", payload: " & m.payload.escapeJson & ")"
