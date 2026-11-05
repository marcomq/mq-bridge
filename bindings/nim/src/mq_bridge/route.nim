import std/options
import ./[raw, errors, executor, message]

type
  Handler* = proc (message: MessageView): Option[Message]
    ## Handles one message: `none` acknowledges it, `some` publishes that
    ## message instead. Raise `RetryableError` to have it redelivered; any
    ## other exception drops it as failed.

  BatchHandler* = proc (messages: seq[MessageView]): seq[Option[Message]]
    ## Handles a batch; returns one entry per input, with `Handler`'s meaning.

  HandlerBoxObj = object
    exec: ptr Executor
    label: string
    single: Handler
    batch: BatchHandler
  HandlerBox = ref HandlerBoxObj

  RouteObj = object
    raw: ptr mqb_route_t
    boxes: seq[HandlerBox]
    exec: ptr Executor
    worker: ptr Thread[ptr Executor]
    pumping: bool
  Route* = ref RouteObj
    ## One route: an input endpoint, optional handlers, an output endpoint.
    ##
    ## Handlers run on one Nim thread, one call at a time. After `start()` or
    ## `run()` that is the calling thread: they run while it is inside a
    ## mq_bridge call (`join`, `send`, `poll`, ...) or `dispatch`.
    ## `start(worker = true)` runs them on a thread of their own instead.

  HandlerJob = object of Job
    box: ptr HandlerBoxObj
    message: ptr mqb_message_t
    outp: ptr ptr mqb_message_t
    status: MqbStatus

  BatchJob = object of Job
    box: ptr HandlerBoxObj
    messages: ptr UncheckedArray[ptr mqb_message_t]
    count: int
    outp: ptr UncheckedArray[ptr mqb_message_t]
    status: MqbStatus

proc reportFailure(label: string, e: ref Exception) {.raises: [].} =
  try:
    stderr.writeLine("mq_bridge: handler '" & label & "' failed: " & e.msg)
  except IOError:
    discard

proc runHandler(job: ptr Job) {.nimcall, gcsafe, raises: [].} =
  let job = cast[ptr HandlerJob](job)
  {.cast(gcsafe).}:
    try:
      var reply = job.box.single(MessageView(raw: job.message))
      if reply.isSome:
        job.outp[] = reply.get.raw
        reply.get.raw = nil
      job.status = MQB_OK
    except Exception as e:
      reportFailure(job.box.label, e)
      job.status = statusOf(e)

proc runBatchHandler(job: ptr Job) {.nimcall, gcsafe, raises: [].} =
  let job = cast[ptr BatchJob](job)
  {.cast(gcsafe).}:
    try:
      var views = newSeq[MessageView](job.count)
      for i in 0 ..< job.count:
        views[i] = MessageView(raw: job.messages[i])
      var replies = job.box.batch(views)
      if replies.len != job.count:
        raise newException(ValueError, "batch handler returned " & $replies.len &
          " results for " & $job.count & " messages")
      for i in 0 ..< job.count:
        if replies[i].isSome:
          job.outp[i] = replies[i].get.raw
          replies[i].get.raw = nil
      job.status = MQB_OK
    except Exception as e:
      reportFailure(job.box.label, e)
      job.status = statusOf(e)

{.push stackTrace: off, lineTrace: off.}

proc handlerTrampoline(message: ptr mqb_message_t, outp: ptr ptr mqb_message_t,
                       userData: pointer): MqbStatus {.cdecl, gcsafe, raises: [].} =
  let box = cast[ptr HandlerBoxObj](userData)
  var job = HandlerJob(box: box, message: message, outp: outp, status: MQB_ERR_PERMANENT)
  job.run = runHandler
  if box.exec == nil or not submitForeign(box.exec, addr job):
    return MQB_ERR_PERMANENT
  job.status

proc batchTrampoline(messages: ptr UncheckedArray[ptr mqb_message_t], count: csize_t,
                     outp: ptr UncheckedArray[ptr mqb_message_t],
                     userData: pointer): MqbStatus {.cdecl, gcsafe, raises: [].} =
  let box = cast[ptr HandlerBoxObj](userData)
  var job = BatchJob(box: box, messages: messages, count: int(count), outp: outp,
                     status: MQB_ERR_PERMANENT)
  job.run = runBatchHandler
  if box.exec == nil or not submitForeign(box.exec, addr job):
    return MQB_ERR_PERMANENT
  job.status

{.pop.}

proc workerLoop(exec: ptr Executor) {.thread.} =
  runUntilStopped(exec)

proc `=copy`(dst: var RouteObj, src: RouteObj) {.error.}
proc `=dup`(src: RouteObj): RouteObj {.error.}

proc unpump(r: var RouteObj) =
  if r.pumping:
    dec r.exec.users
    r.pumping = false

proc `=destroy`(r: RouteObj) =
  if r.raw != nil:
    blocking mqb_route_free(r.raw)
  if r.pumping:
    dec r.exec.users
  if r.worker != nil:
    r.exec.stop()
    joinThread(r.worker[])
    freeShared(r.worker)
    freeExecutor(r.exec)
  `=destroy`(r.boxes)

proc fromStr*(T: typedesc[Route], config: string, name = ""): Route =
  ## A route from YAML or JSON text: a `routes:` document, a `{name: route}`
  ## map, or a single route body (`name` empty).
  let raw = checkHandle blocking mqb_route_from_str(cstring(config), cstring(name))
  Route(raw: raw)

proc fromFile*(T: typedesc[Route], path: string, name = ""): Route =
  let raw = checkHandle blocking mqb_route_from_file(cstring(path), cstring(name))
  Route(raw: raw)

proc addBox(r: Route, label: string, single: Handler = nil,
            batch: BatchHandler = nil): HandlerBox =
  result = HandlerBox(label: label, single: single, batch: batch)
  r.boxes.add result

proc withHandler*(r: Route, handler: Handler): Route {.discardable.} =
  ## Runs every message through `handler` before the output.
  let box = r.addBox("handler", single = handler)
  check mqb_route_set_handler(r.raw, handlerTrampoline, cast[pointer](box))
  r

proc withBatchHandler*(r: Route, handler: BatchHandler): Route {.discardable.} =
  ## Runs messages through `handler` a batch at a time before the output.
  let box = r.addBox("batch handler", batch = handler)
  check mqb_route_set_batch_handler(r.raw, batchTrampoline, cast[pointer](box))
  r

proc addHandler*(r: Route, kind: string, handler: Handler): Route {.discardable.} =
  ## Runs messages whose `kind` metadata equals `kind` through `handler`.
  let box = r.addBox("handler for kind '" & kind & "'", single = handler)
  check mqb_route_add_handler(r.raw, cstring(kind), handlerTrampoline, cast[pointer](box))
  r

proc start*(r: Route, worker = false) =
  ## Connects both endpoints and starts moving messages in the background.
  if r.exec == nil:
    if worker:
      r.exec = newExecutor()
      r.worker = createShared(Thread[ptr Executor])
      createThread(r.worker[], workerLoop, r.exec)
    else:
      r.exec = threadExecutor()
  for box in r.boxes:
    box.exec = r.exec
  if r.worker == nil and r.boxes.len > 0 and not r.pumping:
    inc r.exec.users
    r.pumping = true
  let status = blocking mqb_route_start(r.raw)
  if status != MQB_OK:
    let text = lastError()
    r[].unpump()
    raise newMqbError(status, text)

proc stop*(r: Route) =
  ## Asks a running route to stop; `join` waits for it.
  check mqb_route_stop(r.raw)

proc join*(r: Route) =
  ## Waits until the route stopped, by `stop` or on its own (a drained source
  ## under `exit_on_empty`). Raises if it ended on a permanent error.
  let status = blocking mqb_route_join(r.raw)
  let text = if status != MQB_OK: lastError() else: ""
  r[].unpump()
  if status != MQB_OK:
    raise newMqbError(status, text)

proc run*(r: Route) =
  ## `start` followed by `join`: handlers run on this thread until the route ends.
  r.start()
  r.join()

proc dispatch*(): int {.discardable.} =
  ## Runs the handler calls waiting for this thread and returns how many ran,
  ## for a thread that is busy outside mq_bridge calls.
  if localExec == nil: 0 else: dispatchPending(localExec)
