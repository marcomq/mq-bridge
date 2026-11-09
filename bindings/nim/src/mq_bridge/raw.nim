## Raw declarations of `mq_bridge.h` and `mq_bridge_plugin.h`.
##
## One-to-one with the C headers in `bindings/c/include`; `tests/tdrift.nim`
## fails when a function or table size drifts from them. The library is loaded
## at program start, from the first place that has it: the package's `lib/`
## (filled by `nimble install` or `nimble getlib`), next to the executable
## (macOS, Windows), then the platform's loader search path.
## `-d:mqbLib=/path/to/libmq_bridge.so` replaces that list with one file.

import std/os

when sizeof(int) != 8:
  {.error: "mq-bridge supports 64-bit targets only".}

const
  libFile =
    when defined(windows): "mq_bridge.dll"
    elif defined(macosx): "libmq_bridge.dylib"
    else: "libmq_bridge.so"
  bundledLib = currentSourcePath().parentDir.parentDir / "lib" / libFile
  # Alternatives in `(a|b)` are tried in order; a path with `(`, `|` or `)` cannot be one.
  bundled = if fileExists(bundledLib) and '(' notin bundledLib and '|' notin bundledLib and
                ')' notin bundledLib: bundledLib & "|"
            else: ""
  mqbLib* {.strdefine.} =
    when defined(macosx): "(" & bundled & "@executable_path/" & libFile & "|" & libFile & ")"
    else: "(" & bundled & libFile & ")"

  MQB_API_VERSION* = 1'u32
  MQB_PLUGIN_ABI_MAJOR* = 1'u32
  MQB_PLUGIN_ABI_MINOR* = 5'u32

  MQB_OK* = 0'i32
  MQB_ERR_RETRYABLE* = 1'i32
  MQB_ERR_PERMANENT* = 2'i32
  MQB_ERR_INVALID_CONFIG* = 3'i32
  MQB_END_OF_STREAM* = 4'i32
  MQB_ERR_PANIC* = 5'i32
  MQB_ERR_UNSUPPORTED* = 6'i32
  MQB_ERR_CONNECTION* = 7'i32

  MQB_DISCONNECT_COMPLETED* = 0'u32
  MQB_DISCONNECT_STOPPED* = 1'u32
  MQB_DISCONNECT_FAILED* = 2'u32
  MQB_DISPOSITION_ACK* = 0'u8
  MQB_DISPOSITION_NACK* = 1'u8
  MQB_DISPOSITION_REPLY* = 2'u8
  MQB_OUTCOME_OK* = 0'u8
  MQB_OUTCOME_RETRYABLE* = 1'u8
  MQB_OUTCOME_PERMANENT* = 2'u8
  MQB_CAP_CONSUMER* = 1'u64
  MQB_CAP_PUBLISHER* = 2'u64
  MQB_CAP_MIDDLEWARE* = 4'u64
  MQB_CAP_MIDDLEWARE_WRAP* = 8'u64
  MQB_SCHEMA_ENDPOINT* = 0'u32
  MQB_SCHEMA_MIDDLEWARE* = 1'u32
  MQB_MIDDLEWARE_RECEIVE* = 0'u8
  MQB_MIDDLEWARE_SEND* = 1'u8
  MQB_MESSAGE_DROPPED* = 0'u8
  MQB_MESSAGE_KEPT* = 1'u8
  MQB_DELIVERY_IDEMPOTENT_SINK* = 1'u8
  MQB_DELIVERY_ACKNOWLEDGES* = 2'u8
  MQB_DELIVERY_REQUIRES_FULL_REPLAY* = 4'u8
  MQB_DELIVERY_REPLAYS_FROM_START* = 8'u8
  MQB_LOG_ERROR* = 1'u8
  MQB_LOG_WARN* = 2'u8
  MQB_LOG_INFO* = 3'u8
  MQB_LOG_DEBUG* = 4'u8
  MQB_LOG_TRACE* = 5'u8

  MQB_VTABLE_SIZE_V1_4* = 43 * sizeof(uint)
  MQB_VTABLE_SIZE_V1_5* = 45 * sizeof(uint)
  MQB_HOST_VTABLE_SIZE_V1_2* = 5 * sizeof(uint)

type
  MqbStatus* = int32

  MqbSlice* {.bycopy.} = object
    p*: ptr UncheckedArray[uint8]
    len*: csize_t

  MqbBuffer* {.bycopy.} = object
    p*: ptr UncheckedArray[uint8]
    len*: csize_t
    cap*: csize_t

  MqbKeyValue* {.bycopy.} = object
    key*: MqbSlice
    value*: MqbSlice

  MqbMessage* {.bycopy.} = object
    messageId*: array[16, uint8]
    payload*: MqbSlice
    metadata*: ptr UncheckedArray[MqbKeyValue]
    metadataLen*: csize_t

  MqbCompletion* {.bycopy.} = object
    callback*: proc (ctx: pointer, status: MqbStatus) {.cdecl, gcsafe, raises: [].}
    ctx*: pointer

  MqbCrashInfo* {.bycopy.} = object
    structSize*: csize_t
    signal*: int32
    code*: int32
    faultAddress*: pointer
    pc*: pointer
    siginfo*: pointer
    ucontext*: pointer

  MqbCrashHandler* = proc (userData: pointer, info: ptr MqbCrashInfo) {.cdecl, gcsafe, raises: [].}

  MqbHostVTable* {.bycopy.} = object
    structSize*: csize_t
    logEnabled*: proc (level: uint8): uint8 {.cdecl, gcsafe, raises: [].}
    log*: proc (level: uint8, target, message: MqbSlice) {.cdecl, gcsafe, raises: [].}
    metric*: proc (kind: uint8, name: MqbSlice, labels: ptr MqbKeyValue,
                   labelsLen: csize_t, value: cdouble) {.cdecl, gcsafe, raises: [].}
    registerCrashHandler*: proc (handler: MqbCrashHandler,
                                 userData: pointer): MqbStatus {.cdecl, gcsafe, raises: [].}

  Handle* = pointer
  MsgArray* = ptr UncheckedArray[MqbMessage]

  MqbPluginVTable* {.bycopy.} = object
    structSize*: csize_t
    abiMajor*: uint32
    abiMinor*: uint32
    capabilities*: uint64
    name*: MqbSlice
    version*: MqbSlice
    factoryCreate*: proc (outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    factoryFree*: proc (factory: Handle) {.cdecl, gcsafe, raises: [].}
    bufferFree*: proc (buffer: MqbBuffer) {.cdecl, gcsafe, raises: [].}
    consumerCreate*: proc (factory: Handle, routeName, configJson: MqbSlice,
                           outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    consumerReceiveBatch*: proc (consumer: Handle, maxMessages: csize_t, outBatch: ptr Handle,
                                 outMessages: ptr MsgArray, outLen: ptr csize_t,
                                 err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    consumerCommitRequiresOrder*: proc (consumer: Handle): uint8 {.cdecl, gcsafe, raises: [].}
    consumerSetExitOnEmpty*: proc (consumer: Handle, exitOnEmpty: uint8) {.cdecl, gcsafe, raises: [].}
    consumerClose*: proc (consumer: Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    consumerFree*: proc (consumer: Handle) {.cdecl, gcsafe, raises: [].}
    batchCommit*: proc (batch: Handle, dispositions: ptr UncheckedArray[uint8], len: csize_t,
                        err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    batchFree*: proc (batch: Handle) {.cdecl, gcsafe, raises: [].}
    publisherCreate*: proc (factory: Handle, routeName, configJson: MqbSlice,
                            outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherSendBatch*: proc (publisher: Handle, messages: MsgArray, len: csize_t,
                               err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherFlush*: proc (publisher: Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherClose*: proc (publisher: Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherFree*: proc (publisher: Handle) {.cdecl, gcsafe, raises: [].}
    middlewareCreate*: proc (factory: Handle, routeName, configJson: MqbSlice, side: uint8,
                             outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    middlewareApply*: proc (middleware: Handle, messages: MsgArray, len: csize_t,
                            outResult: ptr Handle, outMessages: ptr MsgArray,
                            outKept: ptr ptr UncheckedArray[uint8],
                            err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    middlewareResultFree*: proc (result: Handle) {.cdecl, gcsafe, raises: [].}
    middlewareFree*: proc (middleware: Handle) {.cdecl, gcsafe, raises: [].}
    publisherRequiresOrderedPublish*: proc (publisher: Handle): uint8 {.cdecl, gcsafe, raises: [].}
    publisherSendBatchOutcomes*: proc (publisher: Handle, messages: MsgArray, len: csize_t,
                                       outOutcomes: ptr UncheckedArray[uint8],
                                       err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    factoryConfigSchema*: proc (factory: Handle, kind: uint32, outp: ptr MqbBuffer,
                                err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherSendBatchResponses*: proc (publisher: Handle, messages: MsgArray, len: csize_t,
                                        outOutcomes: ptr UncheckedArray[uint8], outResult: ptr Handle,
                                        outResponses: ptr MsgArray, outResponsesLen: ptr csize_t,
                                        err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    responsesFree*: proc (result: Handle) {.cdecl, gcsafe, raises: [].}
    batchCommitReplies*: proc (batch: Handle, dispositions: ptr UncheckedArray[uint8],
                               replies: MsgArray, len: csize_t,
                               err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    consumerStatus*: proc (consumer: Handle, outp: ptr MqbBuffer,
                           err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherStatus*: proc (publisher: Handle, outp: ptr MqbBuffer,
                            err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    consumerReceiveBatchAsync*: proc (consumer: Handle, maxMessages: csize_t, outBatch: ptr Handle,
                                      outMessages: ptr MsgArray, outLen: ptr csize_t,
                                      err: ptr MqbBuffer,
                                      completion: MqbCompletion): MqbStatus {.cdecl, gcsafe, raises: [].}
    batchCommitAsync*: proc (batch: Handle, dispositions: ptr UncheckedArray[uint8],
                             replies: MsgArray, len: csize_t, err: ptr MqbBuffer,
                             completion: MqbCompletion): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherSendBatchAsync*: proc (publisher: Handle, messages: MsgArray, len: csize_t,
                                    outOutcomes: ptr UncheckedArray[uint8], outResult: ptr Handle,
                                    outResponses: ptr MsgArray, outResponsesLen: ptr csize_t,
                                    err: ptr MqbBuffer,
                                    completion: MqbCompletion): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherFlushAsync*: proc (publisher: Handle, err: ptr MqbBuffer,
                                completion: MqbCompletion): MqbStatus {.cdecl, gcsafe, raises: [].}
    pluginInit*: proc (host: ptr MqbHostVTable) {.cdecl, gcsafe, raises: [].}
    factoryDelivery*: proc (factory: Handle, configJson: MqbSlice, outFlags: ptr uint8,
                            err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherDisconnect*: proc (publisher: Handle, outcome: uint32,
                                err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    publisherDeferredCommitIntervalMs*: proc (publisher: Handle): uint64 {.cdecl, gcsafe, raises: [].}
    middlewareWrapConsumer*: proc (middleware: Handle, innerTable: ptr MqbPluginVTable, inner: Handle,
                                   outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}
    middlewareWrapPublisher*: proc (middleware: Handle, innerTable: ptr MqbPluginVTable, inner: Handle,
                                    outp: ptr Handle, err: ptr MqbBuffer): MqbStatus {.cdecl, gcsafe, raises: [].}

  mqb_batch_t* {.incompleteStruct.} = object
  mqb_caller_t* {.incompleteStruct.} = object
  mqb_consumer_t* {.incompleteStruct.} = object
  mqb_message_t* {.incompleteStruct.} = object
  mqb_publisher_t* {.incompleteStruct.} = object
  mqb_route_t* {.incompleteStruct.} = object

  mqb_log_fn* = proc (level, target, message: cstring, userData: pointer) {.cdecl, gcsafe, raises: [].}
  mqb_handler_fn* = proc (message: ptr mqb_message_t, outp: ptr ptr mqb_message_t,
                          userData: pointer): MqbStatus {.cdecl, gcsafe, raises: [].}
  mqb_batch_handler_fn* = proc (messages: ptr UncheckedArray[ptr mqb_message_t], count: csize_t,
                                outp: ptr UncheckedArray[ptr mqb_message_t],
                                userData: pointer): MqbStatus {.cdecl, gcsafe, raises: [].}

static:
  doAssert sizeof(MqbMessage) == 16 + 4 * sizeof(uint)
  doAssert sizeof(MqbHostVTable) == MQB_HOST_VTABLE_SIZE_V1_2
  doAssert sizeof(MqbPluginVTable) == MQB_VTABLE_SIZE_V1_5

{.push dynlib: mqbLib, cdecl, gcsafe, raises: [].}

proc mqb_version*(): cstring {.importc.}
proc mqb_api_version*(): uint32 {.importc.}
proc mqb_last_error*(): cstring {.importc.}
proc mqb_string_free*(text: cstring) {.importc.}
proc mqb_request_shutdown*(): bool {.importc.}
proc mqb_is_shutdown_requested*(): bool {.importc.}
proc mqb_config_schema*(): cstring {.importc.}
proc mqb_load_plugin*(path: cstring): MqbStatus {.importc.}
proc mqb_load_installed_plugin*(name: cstring): MqbStatus {.importc.}
proc mqb_enable_plugin_discovery*() {.importc.}
proc mqb_register_plugin*(table: ptr MqbPluginVTable): MqbStatus {.importc.}
proc mqb_init_logging*(callback: mqb_log_fn, userData: pointer, level: cstring): MqbStatus {.importc.}

proc mqb_consumer_from_file*(path, name: cstring): ptr mqb_consumer_t {.importc.}
proc mqb_consumer_from_str*(config, name: cstring): ptr mqb_consumer_t {.importc.}
proc mqb_consumer_poll*(consumer: ptr mqb_consumer_t, max: uint32, timeoutMs: int64,
                        batch: ptr ptr mqb_batch_t): MqbStatus {.importc.}
proc mqb_batch_count*(batch: ptr mqb_batch_t): csize_t {.importc.}
proc mqb_batch_at*(batch: ptr mqb_batch_t, index: csize_t): ptr mqb_message_t {.importc.}
proc mqb_batch_token*(batch: ptr mqb_batch_t): uint32 {.importc.}
proc mqb_batch_free*(batch: ptr mqb_batch_t) {.importc.}
proc mqb_consumer_commit*(consumer: ptr mqb_consumer_t): MqbStatus {.importc.}
proc mqb_consumer_ack*(consumer: ptr mqb_consumer_t, token: uint32): MqbStatus {.importc.}
proc mqb_consumer_nack*(consumer: ptr mqb_consumer_t, token: uint32): MqbStatus {.importc.}
proc mqb_consumer_nack_all*(consumer: ptr mqb_consumer_t): MqbStatus {.importc.}
proc mqb_consumer_status_json*(consumer: ptr mqb_consumer_t): cstring {.importc.}
proc mqb_consumer_exhausted*(consumer: ptr mqb_consumer_t): bool {.importc.}
proc mqb_consumer_close*(consumer: ptr mqb_consumer_t): MqbStatus {.importc.}
proc mqb_consumer_free*(consumer: ptr mqb_consumer_t) {.importc.}

proc mqb_message_new*(payload: pointer, len: csize_t): ptr mqb_message_t {.importc.}
proc mqb_message_set_id*(message: ptr mqb_message_t, id: cstring): MqbStatus {.importc.}
proc mqb_message_set_metadata*(message: ptr mqb_message_t, key, value: cstring): MqbStatus {.importc.}
proc mqb_message_payload*(message: ptr mqb_message_t): MqbSlice {.importc.}
proc mqb_message_id*(message: ptr mqb_message_t): MqbSlice {.importc.}
proc mqb_message_metadata*(message: ptr mqb_message_t, key: cstring): MqbSlice {.importc.}
proc mqb_message_metadata_count*(message: ptr mqb_message_t): csize_t {.importc.}
proc mqb_message_metadata_at*(message: ptr mqb_message_t, index: csize_t,
                              key, value: ptr MqbSlice): MqbStatus {.importc.}
proc mqb_message_free*(message: ptr mqb_message_t) {.importc.}

proc mqb_publisher_from_file*(path, name: cstring): ptr mqb_publisher_t {.importc.}
proc mqb_publisher_from_str*(config, name: cstring): ptr mqb_publisher_t {.importc.}
proc mqb_publisher_send*(publisher: ptr mqb_publisher_t, message: ptr mqb_message_t): MqbStatus {.importc.}
proc mqb_publisher_send_batch*(publisher: ptr mqb_publisher_t,
                               messages: ptr UncheckedArray[ptr mqb_message_t],
                               count: csize_t): MqbStatus {.importc.}
proc mqb_publisher_request*(publisher: ptr mqb_publisher_t, message: ptr mqb_message_t,
                            response: ptr ptr mqb_message_t): MqbStatus {.importc.}
proc mqb_publisher_free*(publisher: ptr mqb_publisher_t) {.importc.}

proc mqb_route_from_file*(path, name: cstring): ptr mqb_route_t {.importc.}
proc mqb_route_from_str*(config, name: cstring): ptr mqb_route_t {.importc.}
proc mqb_route_set_handler*(route: ptr mqb_route_t, handler: mqb_handler_fn,
                            userData: pointer): MqbStatus {.importc.}
proc mqb_route_set_batch_handler*(route: ptr mqb_route_t, handler: mqb_batch_handler_fn,
                                  userData: pointer): MqbStatus {.importc.}
proc mqb_route_add_handler*(route: ptr mqb_route_t, kind: cstring, handler: mqb_handler_fn,
                            userData: pointer): MqbStatus {.importc.}
proc mqb_route_start*(route: ptr mqb_route_t): MqbStatus {.importc.}
proc mqb_route_stop*(route: ptr mqb_route_t): MqbStatus {.importc.}
proc mqb_route_join*(route: ptr mqb_route_t): MqbStatus {.importc.}
proc mqb_route_free*(route: ptr mqb_route_t) {.importc.}

proc mqb_caller_from_route*(route: ptr mqb_route_t): ptr mqb_caller_t {.importc.}
proc mqb_caller_call*(caller: ptr mqb_caller_t, message: ptr mqb_message_t,
                      response: ptr ptr mqb_message_t): MqbStatus {.importc.}
proc mqb_caller_send*(caller: ptr mqb_caller_t, message: ptr mqb_message_t,
                      response: ptr ptr mqb_message_t): MqbStatus {.importc.}
proc mqb_caller_call_many*(caller: ptr mqb_caller_t, messages: ptr ptr mqb_message_t,
                           count: csize_t, responses: ptr ptr mqb_message_t,
                           statuses: ptr MqbStatus): MqbStatus {.importc.}
proc mqb_caller_free*(caller: ptr mqb_caller_t) {.importc.}

{.pop.}
