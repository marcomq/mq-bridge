## Nim binding of mq-bridge: publishers, consumers and routes over any
## supported endpoint, configured with the same YAML or JSON as the CLI.
##
## Runs on the C library (`libmq_bridge`), loaded at program start. Handlers
## and plugin code always run on Nim threads, one call at a time; see `Route`
## and `registerPlugin`.

import std/json
import mq_bridge/[raw, errors, message, client, route, plugin]

export errors, message, client, route, plugin
export MqbSlice, MqbStatus

proc version*(): string =
  ## Version of the loaded library, e.g. `"1.4.0"`.
  $mqb_version()

proc requestShutdown*(): bool {.discardable.} =
  ## Asks every route to shut down gracefully. True only for the first request.
  mqb_request_shutdown()

proc isShutdownRequested*(): bool = mqb_is_shutdown_requested()

proc configSchema*(): JsonNode =
  ## JSON Schema of the config document; nil if the library has no `schema` feature.
  let text = mqb_config_schema()
  if text == nil: return nil
  try: result = parseJson($text)
  finally: mqb_string_free(text)

type LogCallback* = proc (level, target, message: cstring) {.cdecl, gcsafe, raises: [].}
  ## Called from any library thread, possibly several at once; the strings are
  ## valid for the call only.

proc c_fprintf(f: File, fmt: cstring) {.importc: "fprintf", header: "<stdio.h>", varargs.}

{.push stackTrace: off, lineTrace: off.}

proc logToStderr(level, target, message: cstring, userData: pointer) {.cdecl, gcsafe, raises: [].} =
  c_fprintf(stderr, "[%s %s] %s\n", level, target, message)

proc logToCallback(level, target, message: cstring, userData: pointer) {.cdecl, gcsafe, raises: [].} =
  cast[LogCallback](userData)(level, target, message)

{.pop.}

proc initLogging*(level = "", callback: LogCallback = nil) =
  ## Routes library log events to `callback`, or to stderr. `level`
  ## (`error` ... `trace`, default `warn`) seeds the filter; `MQ_BRIDGE_LOG` and
  ## `RUST_LOG` override it. Fails if logging was already initialized.
  let lvl = if level.len == 0: cstring(nil) else: cstring(level)
  if callback == nil:
    check mqb_init_logging(logToStderr, nil, lvl)
  else:
    check mqb_init_logging(logToCallback, cast[pointer](callback), lvl)

if mqb_api_version() != MQB_API_VERSION:
  raise newMqbError(MQB_ERR_PERMANENT, "libmq_bridge " & $mqb_version() &
    " has API version " & $mqb_api_version() & ", this binding expects " &
    $MQB_API_VERSION & ": use the library from the same release")
