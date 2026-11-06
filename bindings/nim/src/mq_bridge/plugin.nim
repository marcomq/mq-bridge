## Custom endpoints and middleware written in Nim.
##
## Subclass `PluginConsumer`, `PluginPublisher` or `PluginMiddleware`, override
## its methods and pass constructors to `registerPlugin`. A route then names it
## as `custom: { name: <name>, config: {...} }`. Every call of one plugin runs
## on a Nim thread of its own, one at a time, so a `receive` should return an
## empty batch when nothing is there instead of waiting long.

import std/[options, macros]
import ./[raw, errors, executor]

type
  PluginMessage* = object
    ## A message owned by Nim. An all-zero `id` lets the host generate one.
    id*: array[16, byte]
    payload*: string
    metadata*: seq[(string, string)]

  Disposition* = enum
    ## How the route settled each message of a received batch.
    dAck, dNack, dReply

  MiddlewareSide* = enum
    msReceive ## on an input: sees each batch after the source produced it
    msSend    ## on an output: sees each batch before the sink does

  DeliveryFlag* = enum
    dfIdempotentSink, dfAcknowledges, dfRequiresFullReplay, dfReplaysFromStart

  WireHolder = object
    wire: seq[MqbMessage]
    kvs: seq[seq[MqbKeyValue]]

  PluginBatch* = ref object of RootObj
    ## One received batch. Subclass it to keep what `commit` needs. The host
    ## reads `messages` in place until `commit`: return a new batch from each
    ## `receive` and do not change its messages afterwards.
    messages*: seq[PluginMessage]
    holder: WireHolder

  PluginConsumer* = ref object of RootObj
  PluginPublisher* = ref object of RootObj
  PluginMiddleware* = ref object of RootObj

  InnerConsumer* = ref object of PluginConsumer
    ## The input endpoint a wrapping middleware wraps, lent by the host.
    table: ptr MqbPluginVTable
    handle: Handle
    closed: bool
  InnerPublisher* = ref object of PluginPublisher
    ## The output endpoint a wrapping middleware wraps, lent by the host.
    table: ptr MqbPluginVTable
    handle: Handle
    closed: bool
  InnerBatch = ref object of PluginBatch
    handle: Handle

  Plugin* = object
    name*: string
    version*: string
    newConsumer*: proc (route, configJson: string): PluginConsumer
    newPublisher*: proc (route, configJson: string): PluginPublisher
    newMiddleware*: proc (route, configJson: string, side: MiddlewareSide): PluginMiddleware
    delivery*: proc (configJson: string): set[DeliveryFlag]
      ## Defaults to `{dfAcknowledges}`.
    wraps*: bool
      ## Its middleware overrides `wrapConsumer`/`wrapPublisher` instead of `apply`.
    endpointSchema*: string   ## JSON Schema of the endpoint config, optional
    middlewareSchema*: string ## JSON Schema of the middleware config, optional

method receive*(c: PluginConsumer, max: int): PluginBatch {.base.} =
  ## Up to `max` messages; nil or empty when idle. Raise `EndOfStreamError`
  ## once the source is exhausted.
  raise newMqbError(MQB_ERR_UNSUPPORTED, "receive is not implemented")

method commit*(c: PluginConsumer, batch: PluginBatch,
               dispositions: seq[Disposition]) {.base.} =
  ## One disposition per message of `batch`, in receive order.
  discard

method commitRequiresOrder*(c: PluginConsumer): bool {.base.} = false
method setExitOnEmpty*(c: PluginConsumer, exitOnEmpty: bool) {.base.} = discard
method close*(c: PluginConsumer) {.base.} = discard

method send*(p: PluginPublisher, messages: seq[PluginMessage]) {.base.} =
  ## Publishes the batch; raising fails all of it.
  raise newMqbError(MQB_ERR_UNSUPPORTED, "send is not implemented")

method flush*(p: PluginPublisher) {.base.} = discard
method close*(p: PluginPublisher) {.base.} = discard

method apply*(m: PluginMiddleware,
              messages: seq[PluginMessage]): seq[Option[PluginMessage]] {.base.} =
  ## One entry per input: `none` drops the message, `some` keeps it (possibly
  ## rewritten).
  for message in messages: result.add some(message)

method wrapConsumer*(m: PluginMiddleware, inner: InnerConsumer): PluginConsumer {.base.} =
  ## With `Plugin.wraps`: the consumer the route reads instead of `inner`.
  ## `close` and `setExitOnEmpty` reach `inner` without the wrapper passing them on.
  nil

method wrapPublisher*(m: PluginMiddleware, inner: InnerPublisher): PluginPublisher {.base.} =
  ## With `Plugin.wraps`: the publisher the route sends to instead of `inner`.
  ## `inner` is flushed after the wrapper's `flush` and closed after its `close`.
  nil

proc newPluginBatch*(messages: sink seq[PluginMessage]): PluginBatch =
  PluginBatch(messages: messages)

proc toPluginMessage*(payload: string, metadata: openArray[(string, string)] = []): PluginMessage =
  PluginMessage(payload: payload, metadata: @metadata)

proc metadata*(m: PluginMessage, key: string, default = ""): string =
  for (k, v) in m.metadata:
    if k == key: return v
  default

# --- wire conversion -------------------------------------------------------

proc slice(s: string): MqbSlice =
  if s.len == 0: MqbSlice()
  else: MqbSlice(p: cast[ptr UncheckedArray[uint8]](unsafeAddr s[0]), len: csize_t(s.len))

proc toStr(s: MqbSlice): string =
  result = newString(s.len)
  if s.len > 0: copyMem(addr result[0], s.p, s.len)

proc fromWire(messages: MsgArray, len: csize_t): seq[PluginMessage] =
  result = newSeq[PluginMessage](len)
  for i in 0 ..< int(len):
    let m = messages[i]
    result[i].id = m.messageId
    result[i].payload = toStr(m.payload)
    for j in 0 ..< int(m.metadataLen):
      result[i].metadata.add (toStr(m.metadata[j].key), toStr(m.metadata[j].value))

proc fill(holder: var WireHolder, messages: seq[PluginMessage]) =
  ## Points `holder.wire` into `messages`, which must not change afterwards.
  holder.wire.setLen(messages.len)
  holder.kvs.setLen(messages.len)
  # Index loops: `for i, m in` copies each message, and the slices would dangle.
  for i in 0 ..< messages.len:
    let n = messages[i].metadata.len
    holder.kvs[i].setLen(n)
    for j in 0 ..< n:
      holder.kvs[i][j] = MqbKeyValue(key: slice(messages[i].metadata[j][0]),
                                     value: slice(messages[i].metadata[j][1]))
    holder.wire[i] = MqbMessage(messageId: messages[i].id, payload: slice(messages[i].payload),
      metadata: if n == 0: nil
                else: cast[ptr UncheckedArray[MqbKeyValue]](addr holder.kvs[i][0]),
      metadataLen: csize_t(n))

proc wirePtr(holder: var WireHolder): MsgArray =
  if holder.wire.len == 0: nil
  else: cast[MsgArray](addr holder.wire[0])

# --- lent endpoints, driven through the host's table ------------------------

proc innerReceive(t: ptr MqbPluginVTable, h: Handle, max: csize_t, b: ptr Handle,
                  m: ptr MsgArray, l: ptr csize_t, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  t.consumerReceiveBatch(h, max, b, m, l, err)

proc innerCommit(t: ptr MqbPluginVTable, b: Handle, d: ptr UncheckedArray[uint8], len: csize_t,
                 err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  t.batchCommit(b, d, len, err)

proc innerConsumerClose(t: ptr MqbPluginVTable, h: Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  t.consumerClose(h, err)

proc innerSend(t: ptr MqbPluginVTable, h: Handle, m: MsgArray, len: csize_t,
               err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  t.publisherSendBatch(h, m, len, err)

proc innerFlush(t: ptr MqbPluginVTable, h: Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  t.publisherFlush(h, err)

proc innerPublisherClose(t: ptr MqbPluginVTable, h: Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  t.publisherClose(h, err)

proc checkInner(t: ptr MqbPluginVTable, status: MqbStatus, err: MqbBuffer) =
  offloadError = ""
  if status == MQB_OK: return
  var text = "lent endpoint failed"
  if err.p != nil:
    text = newString(err.len)
    if err.len > 0: copyMem(addr text[0], err.p, err.len)
    t.bufferFree(err)
  raise newMqbError(status, text)

method receive*(c: InnerConsumer, max: int): PluginBatch =
  var batch: Handle
  var messages: MsgArray
  var len: csize_t
  var err: MqbBuffer
  let status = blocking(innerReceive(c.table, c.handle, csize_t(max), addr batch,
                                     addr messages, addr len, addr err))
  checkInner(c.table, status, err)
  InnerBatch(messages: fromWire(messages, len), handle: batch)

method commit*(c: InnerConsumer, batch: PluginBatch, dispositions: seq[Disposition]) =
  ## `batch` must be one this consumer returned; each is committed once.
  let b = InnerBatch(batch)
  var codes = newSeq[uint8](dispositions.len)
  for i, d in dispositions:
    codes[i] = case d
      of dNack: MQB_DISPOSITION_NACK
      of dReply: MQB_DISPOSITION_REPLY
      of dAck: MQB_DISPOSITION_ACK
  let handle = b.handle
  b.handle = nil
  var err: MqbBuffer
  let status = blocking(innerCommit(c.table, handle,
    if codes.len == 0: nil else: cast[ptr UncheckedArray[uint8]](addr codes[0]),
    csize_t(codes.len), addr err))
  checkInner(c.table, status, err)

method commitRequiresOrder*(c: InnerConsumer): bool =
  c.table.consumerCommitRequiresOrder(c.handle) != 0

method setExitOnEmpty*(c: InnerConsumer, exitOnEmpty: bool) =
  c.table.consumerSetExitOnEmpty(c.handle, uint8(exitOnEmpty))

method close*(c: InnerConsumer) =
  if c.closed: return
  c.closed = true
  var err: MqbBuffer
  let status = blocking(innerConsumerClose(c.table, c.handle, addr err))
  checkInner(c.table, status, err)

method send*(p: InnerPublisher, messages: seq[PluginMessage]) =
  var holder: WireHolder
  holder.fill(messages)
  var err: MqbBuffer
  let status = blocking(innerSend(p.table, p.handle, holder.wirePtr,
                                  csize_t(messages.len), addr err))
  checkInner(p.table, status, err)

method flush*(p: InnerPublisher) =
  var err: MqbBuffer
  let status = blocking(innerFlush(p.table, p.handle, addr err))
  checkInner(p.table, status, err)

method close*(p: InnerPublisher) =
  if p.closed: return
  p.closed = true
  var err: MqbBuffer
  let status = blocking(innerPublisherClose(p.table, p.handle, addr err))
  checkInner(p.table, status, err)

proc release(lent: RootRef) {.gcsafe, raises: [].} =
  ## Frees a lent endpoint's host handle; a publisher is closed first.
  if lent of InnerConsumer:
    let c = InnerConsumer(lent)
    c.table.consumerFree(c.handle)
  elif lent of InnerPublisher:
    let p = InnerPublisher(lent)
    if not p.closed:
      {.cast(gcsafe).}:
        try: p.close()
        except Exception: discard
    p.table.publisherFree(p.handle)

# --- handles ---------------------------------------------------------------

type
  PluginStateObj = object
    plugin: Plugin
    exec: ptr Executor
    worker: ptr Thread[ptr Executor]
    table: MqbPluginVTable
  PluginState = ref PluginStateObj

  HandleObj = object
    exec: ptr Executor
    target: RootRef
    lent: RootRef ## the host endpoint a wrapping middleware's `target` wraps
  HandleRef = ref HandleObj

  BatchHandleObj = object
    exec: ptr Executor
    consumer: PluginConsumer
    batch: PluginBatch
  BatchHandleRef = ref BatchHandleObj

  ResultObj = object
    exec: ptr Executor
    messages: seq[PluginMessage]
    holder: WireHolder
    kept: seq[uint8]
  ResultRef = ref ResultObj

  CallJob = object of Job
    fn: proc (env: pointer) {.nimcall, gcsafe, raises: [].}
    env: pointer

const maxPlugins = 16

var
  slots: array[maxPlugins, ptr PluginStateObj]
  slotCount: int
  states: seq[PluginState]

proc share[T](r: ref T): pointer =
  GC_ref(r)
  cast[pointer](r)

proc unshare[T](p: pointer, _: typedesc[T]) =
  GC_unref(cast[ref T](p))

proc c_malloc(size: csize_t): pointer {.importc: "malloc", header: "<stdlib.h>".}
proc c_free(p: pointer) {.importc: "free", header: "<stdlib.h>".}

proc ownedBuffer(text: string): MqbBuffer =
  if text.len == 0: return
  let p = c_malloc(csize_t(text.len))
  if p == nil: return
  copyMem(p, unsafeAddr text[0], text.len)
  MqbBuffer(p: cast[ptr UncheckedArray[uint8]](p), len: csize_t(text.len), cap: csize_t(text.len))

proc fail(err: ptr MqbBuffer, e: ref Exception): MqbStatus =
  if err != nil: err[] = ownedBuffer(e.msg)
  statusOf(e)

template guarded(err: ptr MqbBuffer, body: untyped): MqbStatus =
  {.cast(gcsafe).}:
    try:
      body
      MQB_OK
    except Exception as e:
      fail(err, e)

# --- implementations, run on the plugin's thread ---------------------------

proc wrap(exec: ptr Executor, target: RootRef): Handle =
  share(HandleRef(exec: exec, target: target))

proc targetOf[T](h: Handle, _: typedesc[T]): T = T(cast[ptr HandleObj](h).target)

proc consumerCreateImpl(factory: Handle, route, config: MqbSlice, outp: ptr Handle,
                        err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  guarded(err):
    let state = cast[ptr PluginStateObj](factory)
    if state.plugin.newConsumer == nil:
      raise newMqbError(MQB_ERR_UNSUPPORTED, state.plugin.name & " has no consumer")
    let c = state.plugin.newConsumer(toStr(route), toStr(config))
    outp[] = wrap(state.exec, c)

proc consumerReceiveImpl(consumer: Handle, max: csize_t, outBatch: ptr Handle,
                         outMessages: ptr MsgArray, outLen: ptr csize_t,
                         err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  guarded(err):
    let h = cast[ptr HandleObj](consumer)
    let c = PluginConsumer(h.target)
    var batch = c.receive(int(max))
    if batch == nil: batch = PluginBatch()
    if batch.holder.wire.len > 0:
      raise newMqbError(MQB_ERR_PERMANENT, "receive returned a batch it returned before")
    batch.holder.fill(batch.messages)
    let bh = BatchHandleRef(exec: h.exec, consumer: c, batch: batch)
    outMessages[] = batch.holder.wirePtr
    outLen[] = csize_t(batch.messages.len)
    outBatch[] = share(bh)

proc consumerRequiresOrderImpl(consumer: Handle): uint8 {.gcsafe, raises: [].} =
  let h = cast[ptr HandleObj](consumer)
  {.cast(gcsafe).}:
    try:
      uint8(PluginConsumer(h.target).commitRequiresOrder() or
            (h.lent != nil and PluginConsumer(h.lent).commitRequiresOrder()))
    except Exception: 0'u8

proc consumerSetExitImpl(consumer: Handle, exitOnEmpty: uint8) {.gcsafe, raises: [].} =
  let h = cast[ptr HandleObj](consumer)
  {.cast(gcsafe).}:
    try:
      PluginConsumer(h.target).setExitOnEmpty(exitOnEmpty != 0)
      if h.lent != nil: PluginConsumer(h.lent).setExitOnEmpty(exitOnEmpty != 0)
    except Exception: discard

proc consumerCloseImpl(consumer: Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  let h = cast[ptr HandleObj](consumer)
  guarded(err):
    PluginConsumer(h.target).close()
    if h.lent != nil: PluginConsumer(h.lent).close()

proc handleFreeImpl(h: Handle) {.gcsafe, raises: [].} =
  if h != nil:
    let lent = cast[ptr HandleObj](h).lent
    unshare(h, HandleObj)
    if lent != nil: release(lent)

proc batchCommitImpl(batch: Handle, dispositions: ptr UncheckedArray[uint8], len: csize_t,
                     err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  let bh = cast[ptr BatchHandleObj](batch)
  result = guarded(err):
    var ds = newSeq[Disposition](len)
    for i in 0 ..< int(len):
      ds[i] = case dispositions[i]
        of MQB_DISPOSITION_NACK: dNack
        of MQB_DISPOSITION_REPLY: dReply
        else: dAck
    bh.consumer.commit(bh.batch, ds)
  unshare(batch, BatchHandleObj)

proc batchFreeImpl(batch: Handle) {.gcsafe, raises: [].} =
  if batch != nil: unshare(batch, BatchHandleObj)

proc publisherCreateImpl(factory: Handle, route, config: MqbSlice, outp: ptr Handle,
                         err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  guarded(err):
    let state = cast[ptr PluginStateObj](factory)
    if state.plugin.newPublisher == nil:
      raise newMqbError(MQB_ERR_UNSUPPORTED, state.plugin.name & " has no publisher")
    let p = state.plugin.newPublisher(toStr(route), toStr(config))
    outp[] = wrap(state.exec, p)

proc publisherSendImpl(publisher: Handle, messages: MsgArray, len: csize_t,
                       err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  guarded(err): targetOf(publisher, PluginPublisher).send(fromWire(messages, len))

proc publisherFlushImpl(publisher: Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  let h = cast[ptr HandleObj](publisher)
  guarded(err):
    PluginPublisher(h.target).flush()
    if h.lent != nil: PluginPublisher(h.lent).flush()

proc publisherCloseImpl(publisher: Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  let h = cast[ptr HandleObj](publisher)
  guarded(err):
    PluginPublisher(h.target).close()
    if h.lent != nil: PluginPublisher(h.lent).close()

proc middlewareCreateImpl(factory: Handle, route, config: MqbSlice, side: uint8,
                          outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  guarded(err):
    let state = cast[ptr PluginStateObj](factory)
    if state.plugin.newMiddleware == nil:
      raise newMqbError(MQB_ERR_UNSUPPORTED, state.plugin.name & " has no middleware")
    let s = if side == MQB_MIDDLEWARE_SEND: msSend else: msReceive
    let m = state.plugin.newMiddleware(toStr(route), toStr(config), s)
    outp[] = wrap(state.exec, m)

proc middlewareApplyImpl(middleware: Handle, messages: MsgArray, len: csize_t,
                         outResult: ptr Handle, outMessages: ptr MsgArray,
                         outKept: ptr ptr UncheckedArray[uint8],
                         err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  guarded(err):
    let h = cast[ptr HandleObj](middleware)
    let input = fromWire(messages, len)
    let output = PluginMiddleware(h.target).apply(input)
    if output.len != input.len:
      raise newMqbError(MQB_ERR_PERMANENT, "middleware returned " & $output.len &
        " results for " & $input.len & " messages")
    let r = ResultRef(exec: h.exec)
    r.messages.setLen(output.len)
    r.kept.setLen(output.len)
    for i, entry in output:
      if entry.isSome:
        r.messages[i] = entry.get
        r.kept[i] = MQB_MESSAGE_KEPT
      else:
        r.kept[i] = MQB_MESSAGE_DROPPED
    r.holder.fill(r.messages)
    outMessages[] = r.holder.wirePtr
    outKept[] = if r.kept.len == 0: nil else: cast[ptr UncheckedArray[uint8]](addr r.kept[0])
    outResult[] = share(r)

proc wrapConsumerImpl(middleware: Handle, table: ptr MqbPluginVTable, inner: Handle,
                      outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  let h = cast[ptr HandleObj](middleware)
  let lent = InnerConsumer(table: table, handle: inner)
  result = guarded(err):
    let c = PluginMiddleware(h.target).wrapConsumer(lent)
    if c == nil: raise newMqbError(MQB_ERR_UNSUPPORTED, "this middleware does not wrap an input")
    outp[] = share(HandleRef(exec: h.exec, target: c, lent: lent))
  if result != MQB_OK: release(lent)

proc wrapPublisherImpl(middleware: Handle, table: ptr MqbPluginVTable, inner: Handle,
                       outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  let h = cast[ptr HandleObj](middleware)
  let lent = InnerPublisher(table: table, handle: inner)
  result = guarded(err):
    let p = PluginMiddleware(h.target).wrapPublisher(lent)
    if p == nil: raise newMqbError(MQB_ERR_UNSUPPORTED, "this middleware does not wrap an output")
    outp[] = share(HandleRef(exec: h.exec, target: p, lent: lent))
  if result != MQB_OK: release(lent)

proc resultFreeImpl(r: Handle) {.gcsafe, raises: [].} =
  if r != nil: unshare(r, ResultObj)

proc configSchemaImpl(factory: Handle, kind: uint32, outp: ptr MqbBuffer,
                      err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  let state = cast[ptr PluginStateObj](factory)
  outp[] = ownedBuffer(if kind == MQB_SCHEMA_MIDDLEWARE: state.plugin.middlewareSchema
                       else: state.plugin.endpointSchema)
  MQB_OK

proc deliveryImpl(factory: Handle, config: MqbSlice, outFlags: ptr uint8,
                  err: ptr MqbBuffer): MqbStatus {.gcsafe, raises: [].} =
  guarded(err):
    let state = cast[ptr PluginStateObj](factory)
    let flags = if state.plugin.delivery == nil: {dfAcknowledges}
                else: state.plugin.delivery(toStr(config))
    var bits = 0'u8
    for f in flags: bits = bits or (1'u8 shl ord(f))
    outFlags[] = bits

# --- trampolines, run on library threads -----------------------------------

macro runOn(exec: ptr Executor, call: untyped, fallback: untyped): untyped =
  ## Runs `call` on `exec`'s thread and returns its result, or `fallback`
  ## when the executor stopped. Packs the arguments on this stack only.
  expectKind call, {nnkCall}
  var args = nnkTupleConstr.newTree()
  for i in 1 ..< call.len: args.add call[i]
  let env = genSym(nskVar, "env")
  let envT = genSym(nskType, "EnvT")
  let fn = genSym(nskProc, "fn")
  let p = genSym(nskParam, "p")
  let e = genSym(nskLet, "e")
  let job = genSym(nskVar, "job")
  var inner = newCall(call[0])
  for i in 0 ..< args.len:
    inner.add nnkBracketExpr.newTree(newDotExpr(e, ident"args"), newLit(i))
  let isVoid = genSym(nskConst, "isVoid")
  result = quote do:
    block:
      const `isVoid` = typeof(`call`) is void
      when `isVoid`:
        var `env` = (args: `args`, r: 0)
      else:
        var `env` = (args: `args`, r: default(typeof(`call`)))
      type `envT` = typeof(`env`)
      proc `fn`(`p`: pointer) {.nimcall, gcsafe, raises: [].} =
        let `e` = cast[ptr `envT`](`p`)
        when `isVoid`: `inner`
        else: `e`.r = `inner`
      var `job` = CallJob(fn: `fn`, env: addr `env`)
      `job`.run = proc (j: ptr Job) {.nimcall, gcsafe, raises: [].} =
        let c = cast[ptr CallJob](j)
        c.fn(c.env)
      when `isVoid`:
        discard submitForeign(`exec`, addr `job`)
      else:
        if submitForeign(`exec`, addr `job`): `env`.r else: `fallback`

{.push stackTrace: off, lineTrace: off, gcsafe, raises: [].}

proc execOf(h: Handle): ptr Executor {.inline.} = cast[ptr HandleObj](h).exec
proc factoryExec(f: Handle): ptr Executor {.inline.} = cast[ptr PluginStateObj](f).exec

proc factoryCreate[N: static int](outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  outp[] = slots[N]
  MQB_OK

proc factoryFree(factory: Handle) {.cdecl.} = discard
proc bufferFree(buffer: MqbBuffer) {.cdecl.} = c_free(buffer.p)
proc pluginInit(host: ptr MqbHostVTable) {.cdecl.} = discard
proc stubFalse(h: Handle): uint8 {.cdecl.} = 0
proc stubFree(h: Handle) {.cdecl.} = discard
proc stubNoDefer(h: Handle): uint64 {.cdecl.} = 0
proc stubDisconnect(h: Handle, outcome: uint32, err: ptr MqbBuffer): MqbStatus {.cdecl.} = MQB_OK
proc stubStatus(h: Handle, outp, err: ptr MqbBuffer): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED

proc stubOutcomes(p: Handle, m: MsgArray, len: csize_t, o: ptr UncheckedArray[uint8],
                  err: ptr MqbBuffer): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED
proc stubResponses(p: Handle, m: MsgArray, len: csize_t, o: ptr UncheckedArray[uint8],
                   r: ptr Handle, rs: ptr MsgArray, rl: ptr csize_t,
                   err: ptr MqbBuffer): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED
proc stubCommitReplies(b: Handle, d: ptr UncheckedArray[uint8], r: MsgArray, len: csize_t,
                       err: ptr MqbBuffer): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED
proc stubReceiveAsync(c: Handle, max: csize_t, b: ptr Handle, m: ptr MsgArray,
                      l: ptr csize_t, err: ptr MqbBuffer,
                      done: MqbCompletion): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED
proc stubCommitAsync(b: Handle, d: ptr UncheckedArray[uint8], r: MsgArray, len: csize_t,
                     err: ptr MqbBuffer, done: MqbCompletion): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED
proc stubSendAsync(p: Handle, m: MsgArray, len: csize_t, o: ptr UncheckedArray[uint8],
                   r: ptr Handle, rs: ptr MsgArray, rl: ptr csize_t, err: ptr MqbBuffer,
                   done: MqbCompletion): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED
proc stubFlushAsync(p: Handle, err: ptr MqbBuffer,
                    done: MqbCompletion): MqbStatus {.cdecl.} = MQB_ERR_UNSUPPORTED

proc consumerCreate(factory: Handle, route, config: MqbSlice, outp: ptr Handle,
                    err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(factoryExec(factory), consumerCreateImpl(factory, route, config, outp, err),
        MQB_ERR_PERMANENT)

proc consumerReceive(consumer: Handle, max: csize_t, outBatch: ptr Handle,
                     outMessages: ptr MsgArray, outLen: ptr csize_t,
                     err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(consumer),
        consumerReceiveImpl(consumer, max, outBatch, outMessages, outLen, err),
        MQB_ERR_PERMANENT)

proc consumerRequiresOrder(consumer: Handle): uint8 {.cdecl.} =
  runOn(execOf(consumer), consumerRequiresOrderImpl(consumer), 0'u8)

proc consumerSetExit(consumer: Handle, exitOnEmpty: uint8) {.cdecl.} =
  runOn(execOf(consumer), consumerSetExitImpl(consumer, exitOnEmpty), ())

proc consumerClose(consumer: Handle, err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(consumer), consumerCloseImpl(consumer, err), MQB_ERR_PERMANENT)

proc handleFree(h: Handle) {.cdecl.} =
  if h != nil:
    runOn(execOf(h), handleFreeImpl(h), ())

proc batchCommit(batch: Handle, dispositions: ptr UncheckedArray[uint8], len: csize_t,
                 err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(cast[ptr BatchHandleObj](batch).exec,
        batchCommitImpl(batch, dispositions, len, err), MQB_ERR_PERMANENT)

proc batchFree(batch: Handle) {.cdecl.} =
  if batch != nil:
    runOn(cast[ptr BatchHandleObj](batch).exec, batchFreeImpl(batch), ())

proc publisherCreate(factory: Handle, route, config: MqbSlice, outp: ptr Handle,
                     err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(factoryExec(factory), publisherCreateImpl(factory, route, config, outp, err),
        MQB_ERR_PERMANENT)

proc publisherSend(publisher: Handle, messages: MsgArray, len: csize_t,
                   err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(publisher), publisherSendImpl(publisher, messages, len, err),
        MQB_ERR_PERMANENT)

proc publisherFlush(publisher: Handle, err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(publisher), publisherFlushImpl(publisher, err), MQB_ERR_PERMANENT)

proc publisherClose(publisher: Handle, err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(publisher), publisherCloseImpl(publisher, err), MQB_ERR_PERMANENT)

proc middlewareCreate(factory: Handle, route, config: MqbSlice, side: uint8,
                      outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(factoryExec(factory),
        middlewareCreateImpl(factory, route, config, side, outp, err), MQB_ERR_PERMANENT)

proc middlewareApply(middleware: Handle, messages: MsgArray, len: csize_t,
                     outResult: ptr Handle, outMessages: ptr MsgArray,
                     outKept: ptr ptr UncheckedArray[uint8], err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(middleware),
        middlewareApplyImpl(middleware, messages, len, outResult, outMessages, outKept, err),
        MQB_ERR_PERMANENT)

proc middlewareWrapConsumer(middleware: Handle, table: ptr MqbPluginVTable, inner: Handle,
                            outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(middleware), wrapConsumerImpl(middleware, table, inner, outp, err),
        MQB_ERR_PERMANENT)

proc middlewareWrapPublisher(middleware: Handle, table: ptr MqbPluginVTable, inner: Handle,
                             outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(execOf(middleware), wrapPublisherImpl(middleware, table, inner, outp, err),
        MQB_ERR_PERMANENT)

proc resultFree(r: Handle) {.cdecl.} =
  if r != nil:
    runOn(cast[ptr ResultObj](r).exec, resultFreeImpl(r), ())

proc configSchema(factory: Handle, kind: uint32, outp: ptr MqbBuffer,
                  err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(factoryExec(factory), configSchemaImpl(factory, kind, outp, err), MQB_ERR_PERMANENT)

proc delivery(factory: Handle, config: MqbSlice, outFlags: ptr uint8,
              err: ptr MqbBuffer): MqbStatus {.cdecl.} =
  runOn(factoryExec(factory), deliveryImpl(factory, config, outFlags, err), MQB_ERR_PERMANENT)

{.pop.}

macro factoryCreates(): untyped =
  result = nnkBracket.newTree()
  for i in 0 ..< maxPlugins:
    result.add nnkBracketExpr.newTree(bindSym"factoryCreate", newLit(i))

proc pluginLoop(exec: ptr Executor) {.thread.} =
  runUntilStopped(exec)

proc registerPlugin*(plugin: Plugin) =
  ## Registers `plugin` in this process. Call it before building a route,
  ## publisher or consumer that names it; at most 16 per process.
  if plugin.name.len == 0:
    raise newMqbError(MQB_ERR_INVALID_CONFIG, "a plugin needs a name")
  if slotCount >= maxPlugins:
    raise newMqbError(MQB_ERR_PERMANENT, "at most " & $maxPlugins & " Nim plugins per process")
  var caps = 0'u64
  if plugin.newConsumer != nil: caps = caps or MQB_CAP_CONSUMER
  if plugin.newPublisher != nil: caps = caps or MQB_CAP_PUBLISHER
  if plugin.newMiddleware != nil: caps = caps or MQB_CAP_MIDDLEWARE
  if plugin.newMiddleware != nil and plugin.wraps: caps = caps or MQB_CAP_MIDDLEWARE_WRAP
  let state = PluginState(plugin: plugin, exec: newExecutor())
  state.plugin.version = if plugin.version.len == 0: "0.0.0" else: plugin.version
  const creates = factoryCreates()
  state.table = MqbPluginVTable(
    structSize: csize_t(sizeof(MqbPluginVTable)),
    abiMajor: MQB_PLUGIN_ABI_MAJOR, abiMinor: MQB_PLUGIN_ABI_MINOR,
    capabilities: caps,
    name: slice(state.plugin.name), version: slice(state.plugin.version),
    factoryCreate: creates[slotCount], factoryFree: factoryFree, bufferFree: bufferFree,
    consumerCreate: consumerCreate, consumerReceiveBatch: consumerReceive,
    consumerCommitRequiresOrder: consumerRequiresOrder,
    consumerSetExitOnEmpty: consumerSetExit, consumerClose: consumerClose,
    consumerFree: handleFree, batchCommit: batchCommit, batchFree: batchFree,
    publisherCreate: publisherCreate, publisherSendBatch: publisherSend,
    publisherFlush: publisherFlush, publisherClose: publisherClose, publisherFree: handleFree,
    middlewareCreate: middlewareCreate, middlewareApply: middlewareApply,
    middlewareResultFree: resultFree, middlewareFree: handleFree,
    publisherRequiresOrderedPublish: stubFalse, publisherSendBatchOutcomes: stubOutcomes,
    factoryConfigSchema: configSchema, publisherSendBatchResponses: stubResponses,
    responsesFree: stubFree, batchCommitReplies: stubCommitReplies,
    consumerStatus: stubStatus, publisherStatus: stubStatus,
    consumerReceiveBatchAsync: stubReceiveAsync, batchCommitAsync: stubCommitAsync,
    publisherSendBatchAsync: stubSendAsync, publisherFlushAsync: stubFlushAsync,
    pluginInit: pluginInit, factoryDelivery: delivery,
    publisherDisconnect: stubDisconnect, publisherDeferredCommitIntervalMs: stubNoDefer,
    middlewareWrapConsumer: middlewareWrapConsumer,
    middlewareWrapPublisher: middlewareWrapPublisher)
  state.worker = createShared(Thread[ptr Executor])
  createThread(state.worker[], pluginLoop, state.exec)
  slots[slotCount] = cast[ptr PluginStateObj](state)
  let status = mqb_register_plugin(addr state.table)
  if status != MQB_OK:
    let text = lastError()
    slots[slotCount] = nil
    state.exec.stop()
    joinThread(state.worker[])
    freeShared(state.worker)
    freeExecutor(state.exec)
    raise newMqbError(status, text)
  states.add state
  inc slotCount

proc loadPlugin*(path: string) =
  ## Loads a native plugin library and registers what it exports.
  check mqb_load_plugin(cstring(path))

proc loadInstalledPlugin*(name: string) =
  ## Loads the one installed plugin library named for `name`
  ## (`libmq_bridge_<name>`) from the plugin search path, and nothing else.
  check mqb_load_installed_plugin(cstring(name))

proc enablePluginDiscovery*() =
  ## Lets routes load any installed plugin whose endpoint or middleware they
  ## name. Off by default; `MQB_PLUGIN_DISCOVERY=1` does the same.
  mqb_enable_plugin_discovery()
