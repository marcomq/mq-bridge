from types import TracebackType
from typing import Any, Callable, Dict, Iterable, List, Mapping, Optional, Tuple, Type, Union

__version__: str

def config_schema() -> Dict[str, Any]:
    """Return the route-config JSON Schema, generated from the Rust models."""
    ...

def init_logging(level: Optional[str] = ...) -> None:
    """Route the library's internal ``tracing`` events into the standard
    ``logging`` module. Call once at startup, then configure output with
    ``logging`` as usual (``logging.basicConfig``, handlers, formatters).

    ``level`` seeds the Rust-side filter (default ``"warn"``); the
    ``MQ_BRIDGE_LOG`` / ``RUST_LOG`` environment variables override it.
    Filtering happens in Rust, so suppressed events never cross into Python.
    Raises if logging was already initialized."""
    ...

def request_shutdown() -> bool:
    """Request a graceful shutdown of every route: running ones stop, and ones
    started afterwards stop immediately. Wire it to your own signal handling,
    e.g. ``signal.signal(signal.SIGTERM, lambda *_: request_shutdown())``.
    Returns ``True`` only for the first request. It cannot be undone."""
    ...

def is_shutdown_requested() -> bool:
    """``True`` once :func:`request_shutdown` has been called."""
    ...

def register_endpoint(
    name: str, factory: Callable[[str, Dict[str, Any]], Any]
) -> None:
    """Register a custom endpoint implemented in Python under ``name``, making it
    usable as an endpoint type in route configs — either as ``{"pulsar": {...}}``
    or explicitly as ``{"custom": {"name": "pulsar", "config": {...}}}``.

    ``factory`` is called as ``factory(route_name, config)`` once per route leg.
    It must return an object implementing ``receive_batch(max_messages)`` to be
    usable as an input and/or ``send_batch(messages)`` to be usable as an output,
    plus optional ``commit(dispositions)`` and ``close()``.

    ``receive_batch`` returns an iterable of ``Message``/bytes/str/JSON values,
    or ``None``/``[]`` when nothing is available right now; raise
    ``StopIteration`` to signal end of stream — that meaning applies only to
    ``receive_batch``; from any other method it is an ordinary error. ``commit``
    receives a list of ``"ack"``/``"nack"`` strings, one per message in the batch.

    Register before starting a route that names it; registering the same name
    twice raises and keeps the first factory. All calls into one endpoint object
    are serialized on its own thread, so it need not be thread-safe."""
    ...

def unregister_endpoint(name: str) -> bool:
    """Drop the endpoint factory registered under ``name``, releasing the
    reference it holds on the Python factory object.

    Returns ``True`` when a factory was removed, ``False`` when ``name`` was not
    registered. Call only after every route using the endpoint has stopped;
    routes already holding an instance keep running."""
    ...

def register_middleware(
    name: str, factory: Callable[[str, Dict[str, Any]], Any]
) -> None:
    """Register a custom middleware implemented in Python under ``name``, usable
    in any endpoint's ``middlewares`` list as
    ``{"custom": {"name": name, "config": {...}}}``.

    ``factory`` is called as ``factory(route_name, config)`` once per endpoint
    the middleware is attached to. It must return an object implementing
    ``on_receive(messages)`` (applies on an input endpoint) and/or
    ``on_send(messages)`` (applies on an output endpoint); a side the object does
    not implement passes through untouched.

    Both hooks receive the batch and must return one item per input message: a
    ``Message`` (kept, possibly rewritten) or ``None`` to drop it. Keeping the
    length fixed is what lets acknowledgements stay aligned with the source
    batch."""
    ...

def unregister_middleware(name: str) -> bool:
    """Drop the middleware factory registered under ``name``, releasing the
    reference it holds on the Python factory object.

    Returns ``True`` when a factory was removed, ``False`` when ``name`` was not
    registered. Call only after every route using the middleware has stopped."""
    ...

JsonValue = Any
HandlerResult = Optional[Union["Message", bytes, str, Dict[str, JsonValue], List[JsonValue], int, float, bool]]


class RetryableError(Exception): ...


class NonRetryableError(Exception): ...


class Message:
    """A message payload with optional metadata and id.

    ``id`` accepts a UUID string, a ``0x``-prefixed hex literal, or a decimal
    integer. Any other string is hashed to a stable id rather than rejected, so
    an arbitrary string no longer raises ``ValueError``; equal strings always
    yield the same id.
    """

    def __init__(
        self,
        payload: bytes,
        metadata: Optional[Mapping[str, str]] = ...,
        id: Optional[str] = ...,
    ) -> None: ...

    @classmethod
    def from_json(
        cls,
        data: JsonValue,
        metadata: Optional[Mapping[str, str]] = ...,
        id: Optional[str] = ...,
    ) -> "Message": ...

    @property
    def payload(self) -> bytes: ...

    @property
    def metadata(self) -> Dict[str, str]: ...

    @property
    def id(self) -> Optional[str]: ...

    def json(self) -> JsonValue: ...

    def text(self) -> str: ...

    def with_json(self, data: JsonValue) -> "Message": ...

    def with_payload(self, payload: bytes) -> "Message": ...


class Route:
    @classmethod
    def from_file(cls, path: str, name: Optional[str] = ...) -> "Route": ...

    @classmethod
    def from_str(cls, text: str, name: Optional[str] = ...) -> "Route": ...

    @classmethod
    def from_config(
        cls, config: Mapping[str, Any], name: Optional[str] = ...
    ) -> "Route": ...

    # Deprecated: use from_file (emits DeprecationWarning at runtime).
    @classmethod
    def from_yaml(cls, path: str, name: Optional[str] = ...) -> "Route": ...

    # Deprecated: use from_str (emits DeprecationWarning at runtime).
    @classmethod
    def from_yaml_str(cls, text: str, name: Optional[str] = ...) -> "Route": ...

    def with_handler(self, handler: Callable[[Message], HandlerResult]) -> "Route": ...

    def add_handler(
        self,
        kind: str,
        handler: Callable[[JsonValue], HandlerResult],
    ) -> "Route": ...

    def run(self) -> None:
        """Deploy and block the calling thread until ``stop()`` is called. A
        ``KeyboardInterrupt`` stops the route gracefully and is re-raised."""
        ...

    def start(self) -> None:
        """Deploy on a background thread and return immediately."""
        ...

    def join(self) -> None:
        """Block until a route started with ``start()`` has stopped. A
        ``KeyboardInterrupt`` stops the route gracefully and is re-raised."""
        ...

    def stop(self) -> None: ...

    def __enter__(self) -> "Route": ...

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[TracebackType],
    ) -> bool: ...


class Consumer:
    @classmethod
    def from_file(cls, path: str, name: Optional[str] = ...) -> "Consumer": ...

    @classmethod
    def from_str(cls, text: str, name: Optional[str] = ...) -> "Consumer": ...

    @classmethod
    def from_config(
        cls, config: Mapping[str, Any], name: Optional[str] = ...
    ) -> "Consumer": ...

    def poll(
        self,
        max: int = ...,
        timeout_ms: Optional[int] = ...,
    ) -> List["Message"]:
        """Receive up to ``max`` messages without acking. Empty list once
        ``timeout_ms`` milliseconds elapse with nothing received, or when the
        source is exhausted; omit ``timeout_ms`` to block until a message
        arrives. The returned messages are acked by the next ``commit()`` call —
        you must call it (see ``commit``)."""
        ...

    def poll_batch(
        self,
        max: int = ...,
        timeout_ms: Optional[int] = ...,
    ) -> Tuple[List["Message"], Optional[int]]:
        """Like ``poll()``, but also return the batch's token so it can be acked
        or nacked individually with ``ack(token)`` / ``nack(token)`` — the shape a
        ``dlt`` resource wants (``poll → yield → commit load package →
        ack(token)``). Returns ``(messages, token)``, or ``([], None)`` on timeout
        or end-of-stream. Tokens stay outstanding until acked/nacked; ``commit()``
        still acks every outstanding batch at once, so don't mix the two styles on
        one consumer."""
        ...

    def ack(self, token: int) -> None:
        """Ack a single batch by the token from ``poll_batch()``, advancing the
        consumer offset for just that batch. Raises if the token is unknown
        (already acked/nacked, or never polled)."""
        ...

    def nack(self, token: Optional[int] = ...) -> None:
        """Negatively acknowledge so the broker can redeliver. With a ``token``,
        nacks just that batch; without one, nacks every outstanding batch (oldest
        first). On Kafka there is no per-message nack — this leaves the offset
        unadvanced, so redelivery happens on the next run/rebalance, not at
        once."""
        ...

    def commit(self) -> None:
        """Ack every batch returned by ``poll()`` since the last ``commit()``,
        advancing the consumer offset.

        Calling this is required, not optional. Without it the offset never
        advances (messages are re-delivered on the next run), most brokers stall
        once their unacknowledged/prefetch window fills, and uncommitted batches
        are held in memory so the process grows unbounded. To retry a failed
        batch, simply don't commit it — it will be redelivered."""
        ...

    def status(self) -> Dict[str, Any]:
        """Status snapshot for the underlying endpoint: ``healthy``, ``target``,
        optional ``pending`` (broker backlog/lag where reported — Kafka offset
        lag, AMQP queue depth, NATS JetStream ``num_pending``), optional
        ``capacity``/``error``, and ``details``. ``pending == 0`` is a precise
        "caught up" signal; ``None`` where the broker exposes no backlog."""
        ...

    def close(self) -> None:
        """Release the broker connection. Idempotent; ``poll()``/``status()``
        raise afterwards. The context-manager form calls this on exit."""
        ...

    @property
    def exhausted(self) -> bool: ...

    def __enter__(self) -> "Consumer": ...

    def __exit__(
        self,
        exc_type: Optional[Type[BaseException]],
        exc_value: Optional[BaseException],
        traceback: Optional[TracebackType],
    ) -> bool: ...


class MemoryDrainer:
    @classmethod
    def from_topic(
        cls,
        topic: str,
        capacity: int = ...,
    ) -> "MemoryDrainer": ...

    def drain(
        self,
        count: int,
        timeout: Optional[float] = ...,
        batch_size: int = ...,
    ) -> int: ...


class Publisher:
    @classmethod
    def from_file(cls, path: str, name: Optional[str] = ...) -> "Publisher": ...

    @classmethod
    def from_str(cls, text: str, name: Optional[str] = ...) -> "Publisher": ...

    @classmethod
    def from_config(
        cls, config: Mapping[str, Any], name: Optional[str] = ...
    ) -> "Publisher": ...

    # Deprecated: use from_file (emits DeprecationWarning at runtime).
    @classmethod
    def from_yaml(cls, path: str, name: Optional[str] = ...) -> "Publisher": ...

    # Deprecated: use from_str (emits DeprecationWarning at runtime).
    @classmethod
    def from_yaml_str(cls, text: str, name: Optional[str] = ...) -> "Publisher": ...

    def send(
        self,
        message: Union[Message, bytes],
        metadata: Optional[Mapping[str, str]] = ...,
    ) -> None: ...

    def send_batch(
        self,
        messages: Iterable[Union[Message, bytes]],
    ) -> None: ...

    def request(
        self,
        message: Union[Message, bytes],
        metadata: Optional[Mapping[str, str]] = ...,
    ) -> Message: ...

    def send_json(
        self,
        data: JsonValue,
        metadata: Optional[Mapping[str, str]] = ...,
        id: Optional[str] = ...,
    ) -> None: ...

    def request_json(
        self,
        data: JsonValue,
        metadata: Optional[Mapping[str, str]] = ...,
        id: Optional[str] = ...,
    ) -> Message: ...
