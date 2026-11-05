## Runs Nim code on one chosen thread for calls that arrive on library threads.
##
## A library thread fills a `Job` on its own stack, queues it and waits; the
## thread owning the `Executor` runs it. Nothing on the library thread touches
## Nim's heap. While a thread that owns an executor waits in a blocking C call,
## the call runs on a helper thread and the owner keeps running queued jobs, so
## a handler can fire while its own thread sends, polls or joins.

import std/[locks, macros]
import ./raw

type
  JobFn* = proc (job: ptr Job) {.nimcall, gcsafe, raises: [].}

  Job* {.inheritable, pure.} = object
    next: ptr Job
    run*: JobFn
    done: bool
    notify: ptr Executor

  Executor* = object
    lock: Lock
    cond: Cond
    doneCond: Cond
    head, tail: ptr Job
    stopped: bool
    users*: int

  OffloadJob = object of Job
    fn: proc (env: pointer) {.nimcall, gcsafe, raises: [].}
    env: pointer

var
  localExec* {.threadvar.}: ptr Executor
  helpers {.threadvar.}: seq[ptr Executor]
    ## One per nesting level: a handler's blocking call must not queue behind
    ## the call that is waiting for that handler.
  helperThreads {.threadvar.}: seq[ptr Thread[ptr Executor]]
  helperDepth {.threadvar.}: int
  offloadError* {.threadvar.}: string

proc newExecutor*(): ptr Executor =
  result = createShared(Executor)
  initLock(result.lock)
  initCond(result.cond)
  initCond(result.doneCond)

proc freeExecutor*(exec: ptr Executor) =
  deinitCond(exec.doneCond)
  deinitCond(exec.cond)
  deinitLock(exec.lock)
  freeShared(exec)

{.push stackTrace: off, lineTrace: off.}
# Reachable from library threads: with tlsEmulation (the macOS default) a stack
# frame or threadvar access there would crash, so none of these use either.

proc enqueue(exec: ptr Executor, job: ptr Job): bool =
  ## Caller holds `exec.lock`.
  if exec.stopped:
    return false
  job.next = nil
  if exec.tail == nil: exec.head = job
  else: exec.tail.next = job
  exec.tail = job
  signal(exec.cond)
  true

proc pop(exec: ptr Executor): ptr Job =
  result = exec.head
  exec.head = result.next
  if exec.head == nil:
    exec.tail = nil

proc complete(job: ptr Job, exec: ptr Executor) =
  ## The job may live on the waiter's stack: it is not touched after the unlock.
  let target = if job.notify != nil: job.notify else: exec
  acquire(target.lock)
  job.done = true
  if job.notify != nil: broadcast(target.cond)
  else: broadcast(target.doneCond)
  release(target.lock)

proc submitForeign*(exec: ptr Executor, job: ptr Job): bool =
  ## Queues `job` from a library thread and waits until it ran; false if
  ## `exec` stopped. Allocates nothing on Nim's heap.
  acquire(exec.lock)
  result = exec.enqueue(job)
  if result:
    while not job.done:
      wait(exec.doneCond, exec.lock)
  release(exec.lock)

{.pop.}

proc pumpUntil*(exec: ptr Executor, flag: ptr bool) =
  ## Runs `exec`'s jobs on this thread until `flag[]` is set under `exec.lock`.
  acquire(exec.lock)
  while not flag[]:
    if exec.head != nil:
      let job = exec.pop()
      release(exec.lock)
      job.run(job)
      complete(job, exec)
      acquire(exec.lock)
    else:
      wait(exec.cond, exec.lock)
  release(exec.lock)

proc dispatchPending*(exec: ptr Executor): int =
  ## Runs the jobs queued right now and returns how many ran.
  while true:
    acquire(exec.lock)
    if exec.head == nil:
      release(exec.lock)
      return
    let job = exec.pop()
    release(exec.lock)
    job.run(job)
    complete(job, exec)
    inc result

proc stop*(exec: ptr Executor) =
  acquire(exec.lock)
  exec.stopped = true
  broadcast(exec.cond)
  release(exec.lock)

proc runOffload(job: ptr Job) {.nimcall, gcsafe, raises: [].} =
  let job = cast[ptr OffloadJob](job)
  job.fn(job.env)

proc helperLoop(exec: ptr Executor) {.thread.} =
  pumpUntil(exec, addr exec.stopped)

proc stopHelpers() =
  for i in 0 ..< helpers.len:
    helpers[i].stop()
    joinThread(helperThreads[i][])
    freeShared(helperThreads[i])
    freeExecutor(helpers[i])
  helpers.setLen(0)
  helperThreads.setLen(0)

proc runUntilStopped*(exec: ptr Executor) =
  ## Runs `exec`'s jobs on this thread until `stop`, with blocking calls made
  ## by them pumping it too.
  localExec = exec
  inc exec.users
  pumpUntil(exec, addr exec.stopped)
  stopHelpers()

proc submit*(exec: ptr Executor, job: ptr Job): bool =
  ## Like `submitForeign`, from a Nim thread: its own executor keeps running
  ## jobs meanwhile.
  let own = localExec
  if own == exec:
    job.run(job)
    return true
  if own == nil:
    return submitForeign(exec, job)
  job.notify = own
  acquire(exec.lock)
  let queued = exec.enqueue(job)
  release(exec.lock)
  if queued:
    pumpUntil(own, addr job.done)
  queued

proc offload*(fn: proc (env: pointer) {.nimcall, gcsafe, raises: [].}, env: pointer) =
  ## Calls `fn(env)`; on a thread whose executor has users, on a helper thread
  ## while this one keeps running its jobs.
  let own = localExec
  if own == nil or own.users == 0:
    fn(env)
    return
  if helperDepth == helpers.len:
    let exec = newExecutor()
    let thread = createShared(Thread[ptr Executor])
    try:
      createThread(thread[], helperLoop, exec)
    except ResourceExhaustedError:
      freeShared(thread)
      freeExecutor(exec)
      fn(env)
      return
    helpers.add exec
    helperThreads.add thread
  var job = OffloadJob(fn: fn, env: env)
  job.run = runOffload
  inc helperDepth
  discard submit(helpers[helperDepth - 1], addr job)
  dec helperDepth

proc threadExecutor*(): ptr Executor =
  ## This thread's executor, created on first use and kept for the thread's life.
  if localExec == nil:
    localExec = newExecutor()
  localExec

proc c_strdup(s: cstring): cstring {.importc: "strdup", header: "<string.h>".}
proc c_free(p: pointer) {.importc: "free", header: "<stdlib.h>".}

proc failed*[T](value: T): bool {.inline.} =
  when T is MqbStatus: value != MQB_OK
  elif T is ptr or T is cstring: value == nil
  else: false

proc keepError*(text: cstring) =
  offloadError = if text == nil: "" else: $text
  if text != nil: c_free(text)

proc copyError*(): cstring {.gcsafe, raises: [].} = c_strdup(mqb_last_error())

macro blocking*(call: untyped): untyped =
  ## Runs a blocking `mqb_*` call through `offload` and returns its result. On
  ## failure the library's error text lands in `offloadError`, whichever thread
  ## made the call.
  expectKind call, {nnkCall}
  var args = nnkTupleConstr.newTree()
  for i in 1 ..< call.len: args.add call[i]
  let env = genSym(nskVar, "env")
  let envT = genSym(nskType, "EnvT")
  let fn = genSym(nskProc, "fn")
  let p = genSym(nskParam, "envPtr")
  let e = genSym(nskLet, "envRef")
  var inner = newCall(call[0])
  for i in 0 ..< args.len:
    inner.add nnkBracketExpr.newTree(newDotExpr(e, ident"args"), newLit(i))
  let isVoid = genSym(nskConst, "isVoid")
  result = quote do:
    block:
      const `isVoid` = typeof(`call`) is void
      when `isVoid`:
        var `env` = (args: `args`, err: cstring(nil))
      else:
        var `env` = (args: `args`, r: default(typeof(`call`)), err: cstring(nil))
      type `envT` = typeof(`env`)
      proc `fn`(`p`: pointer) {.nimcall, gcsafe, raises: [].} =
        let `e` = cast[ptr `envT`](`p`)
        when `isVoid`:
          `inner`
        else:
          `e`.r = `inner`
          if failed(`e`.r): `e`.err = copyError()
      offloadError = ""
      offload(`fn`, addr `env`)
      when not `isVoid`:
        keepError(`env`.err)
        `env`.r
