import ./[raw, executor]

type
  MqbError* = object of CatchableError
    ## A failed library call; `status` is the `MQB_*` code.
    status*: MqbStatus
  RetryableError* = object of MqbError
    ## Transient: the same call may succeed later. Raise it in a handler to
    ## have the message redelivered.
  ConnectionError* = object of MqbError
    ## The connection failed; the library reconnects.
  ConfigError* = object of MqbError
  EndOfStreamError* = object of MqbError
    ## Raise it in a plugin consumer once the source is exhausted.
  UnsupportedError* = object of MqbError

proc newMqbError*(status: MqbStatus, msg: string): ref MqbError =
  case status
  of MQB_ERR_RETRYABLE: result = newException(RetryableError, msg)
  of MQB_ERR_CONNECTION: result = newException(ConnectionError, msg)
  of MQB_ERR_INVALID_CONFIG: result = newException(ConfigError, msg)
  of MQB_END_OF_STREAM: result = newException(EndOfStreamError, msg)
  of MQB_ERR_UNSUPPORTED: result = newException(UnsupportedError, msg)
  else: result = newException(MqbError, msg)
  result.status = status

proc lastError*(): string =
  ## Text of the last failed call on this thread.
  if offloadError.len > 0: move offloadError
  else: $mqb_last_error()

proc check*(status: MqbStatus) =
  if status != MQB_OK:
    raise newMqbError(status, lastError())

proc checkHandle*[T](handle: ptr T): ptr T =
  if handle == nil:
    raise newMqbError(MQB_ERR_PERMANENT, lastError())
  handle

proc checkHandle*(text: cstring): cstring =
  if text == nil:
    raise newMqbError(MQB_ERR_PERMANENT, lastError())
  text

proc statusOf*(e: ref Exception): MqbStatus =
  ## The status that reports `e` to the library.
  if e of RetryableError: MQB_ERR_RETRYABLE
  elif e of ConnectionError: MQB_ERR_CONNECTION
  elif e of ConfigError: MQB_ERR_INVALID_CONFIG
  elif e of EndOfStreamError: MQB_END_OF_STREAM
  elif e of UnsupportedError: MQB_ERR_UNSUPPORTED
  elif e of MqbError and (ref MqbError)(e).status != MQB_OK: (ref MqbError)(e).status
  else: MQB_ERR_PERMANENT
