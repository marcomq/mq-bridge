export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

export type Metadata = Record<string, string>;
export type HandlerResult = Message | null | undefined;
export type MessageHandler = (message: Message) => HandlerResult | Promise<HandlerResult>;
export type JsonHandler = (data: JsonValue) => HandlerResult | Promise<HandlerResult>;

export class Message {
  constructor(payload: Buffer | Uint8Array, metadata?: Metadata | null, id?: string | null);
  static fromJson(data: JsonValue, metadata?: Metadata | null, id?: string | null): Message;
  get payload(): Buffer;
  get metadata(): Metadata;
  get id(): string | null;
  json(): JsonValue;
  text(): string;
}

export class Publisher {
  static fromFile(path: string, name?: string | null): Publisher;
  static fromStr(text: string, name?: string | null): Publisher;
  static fromConfig(config: JsonValue, name?: string | null): Publisher;
  /** @deprecated Use {@link Publisher.fromFile} instead. */
  static fromYaml(path: string, name?: string | null): Publisher;
  /** @deprecated Use {@link Publisher.fromStr} instead. */
  static fromYamlStr(text: string, name?: string | null): Publisher;
  send(message: Message): Promise<void>;
  /** Publish many messages in one call (each carries its own metadata/id). */
  sendBatch(messages: Message[]): Promise<void>;
  request(message: Message): Promise<Message>;
  sendJson(data: JsonValue, metadata?: Metadata | null, id?: string | null): Promise<void>;
  requestJson(data: JsonValue, metadata?: Metadata | null, id?: string | null): Promise<Message>;
}

export interface ConsumerStatus {
  healthy: boolean;
  target: string;
  /** Broker backlog/lag where the transport reports it; absent otherwise. */
  pending?: number;
  capacity?: number;
  error?: string;
  details: JsonValue;
}

export class Consumer {
  static fromFile(path: string, name?: string | null): Consumer;
  static fromStr(text: string, name?: string | null): Consumer;
  static fromConfig(config: JsonValue, name?: string | null): Consumer;
  /**
   * Receive up to `max` messages (default 256) without acking. Resolves to an
   * empty array if `timeoutMs` milliseconds elapse with nothing received, or the
   * source is exhausted. Omit `timeoutMs` to block until a message arrives. The
   * returned messages are acked by the next `commit()` call — which you must
   * call (see {@link Consumer.commit}).
   */
  poll(max?: number | null, timeoutMs?: number | null): Promise<Message[]>;
  /**
   * Like {@link Consumer.poll}, but also returns the batch's `token` so it can be
   * acked or nacked individually with {@link Consumer.ack} / {@link Consumer.nack}
   * — the shape a `dlt` resource wants (`poll → yield → commit load package →
   * ack(token)`). `token` is `null` on timeout or end-of-stream. Tokens stay
   * outstanding until acked/nacked; `commit()` still acks every outstanding batch
   * at once, so don't mix the two styles on one consumer.
   */
  pollBatch(
    max?: number | null,
    timeoutMs?: number | null,
  ): Promise<{ messages: Message[]; token: number | null }>;
  /**
   * Acknowledge every batch returned by `poll()` since the last `commit()`,
   * advancing the consumer offset.
   *
   * Calling this is required, not optional. Without it the offset never advances
   * (messages are re-delivered on the next run), most brokers stall once their
   * unacknowledged/prefetch window fills, and uncommitted batches are held in
   * memory so the process grows unbounded. To retry a failed batch, simply
   * don't commit it — it will be redelivered.
   */
  commit(): Promise<void>;
  /**
   * Acknowledge a single batch by the `token` from {@link Consumer.pollBatch},
   * advancing the consumer offset for just that batch. Rejects if the token is
   * unknown (already acked/nacked, or never polled).
   */
  ack(token: number): Promise<void>;
  /**
   * Negatively acknowledge so the broker can redeliver. With a `token`, nacks
   * just that batch; omit it to nack every outstanding batch (oldest first). On
   * Kafka there is no per-message nack — this leaves the offset unadvanced, so
   * redelivery happens on the next run/rebalance, not at once.
   */
  nack(token?: number | null): Promise<void>;
  /**
   * Status snapshot for the underlying endpoint. `pending === 0` is a precise
   * "caught up" signal on transports that report backlog (Kafka, AMQP, NATS
   * JetStream); `pending` is absent where the broker exposes none.
   */
  status(): Promise<ConsumerStatus>;
  /** Release the broker connection. Idempotent; `poll()`/`status()` reject after. */
  close(): Promise<void>;
  /** `true` once the source has signalled end-of-stream (e.g. a drained file). */
  get exhausted(): boolean;
}

export class Route {
  static fromFile(path: string, name?: string | null): Route;
  static fromStr(text: string, name?: string | null): Route;
  static fromConfig(config: JsonValue, name?: string | null): Route;
  /** @deprecated Use {@link Route.fromFile} instead. */
  static fromYaml(path: string, name?: string | null): Route;
  /** @deprecated Use {@link Route.fromStr} instead. */
  static fromYamlStr(text: string, name?: string | null): Route;
  withHandler(handler: MessageHandler): void;
  addHandler(kind: string, handler: JsonHandler): void;
  start(): void;
  stop(): void;
  /** Blocks the event loop until the route stops; prefer {@link Route.wait}. */
  join(): void;
  /** Resolves once the route stops, without blocking the event loop. */
  wait(): Promise<void>;
}

/**
 * JSON Schema for the route/config mapping, generated from the compiled Rust
 * models. Throws if the addon was built without the `schema` feature.
 */
export function configSchema(): JsonValue;

/** One library log event delivered to the {@link initLogging} callback. */
export interface LogRecord {
  /** `error` | `warn` | `info` | `debug` | `trace`. */
  level: string;
  /** Emitting module, e.g. `mq_bridge::route`. */
  target: string;
  message: string;
}

/**
 * Route the library's internal `tracing` events into `callback` so your host
 * logger (console, pino, winston, …) owns output. Call once at startup.
 *
 * `level` seeds the Rust-side filter (default `"warn"`); the `MQ_BRIDGE_LOG` /
 * `RUST_LOG` environment variables override it. Filtering happens in Rust, so
 * suppressed events never reach JS. The callback is held weakly and will not
 * keep the process alive. Throws if logging was already initialized.
 */
export function initLogging(
  callback: (record: LogRecord) => void,
  level?: string | null,
): void;

/** Throw from {@link CustomEndpoint.receiveBatch} to end the route cleanly. */
/**
 * Request a graceful shutdown of every route: running ones stop, and ones
 * started afterwards stop immediately. Wire it to your signal handling, e.g.
 * `process.on("SIGTERM", () => requestShutdown())`. Returns `true` only for the
 * first request. It cannot be undone.
 */
export function requestShutdown(): boolean;

/** `true` once {@link requestShutdown} has been called. */
export function isShutdownRequested(): boolean;

export class EndOfStream extends Error {
  constructor(message?: string);
}

/** One message disposition reported to {@link CustomEndpoint.commit}. */
export type Disposition = "ack" | "nack";

/** What a {@link registerEndpoint} factory returns. */
export interface CustomEndpoint {
  /**
   * Read up to `maxMessages`. Return `null` or `[]` when nothing is available
   * right now (the route backs off and retries; under `exit_on_empty` this is
   * the drain signal), or throw {@link EndOfStream} when the source is done.
   */
  receiveBatch?(
    maxMessages: number,
  ):
    | Promise<Iterable<Message | Buffer | Uint8Array | string | JsonValue> | null>
    | Iterable<Message | Buffer | Uint8Array | string | JsonValue>
    | null;
  /** Called once per received batch, with one disposition per message. */
  commit?(dispositions: Disposition[]): Promise<void> | void;
  /** Publish a batch. Throw to fail it; set `err.retryable` to have it retried. */
  sendBatch?(messages: Message[]): Promise<void> | void;
  /** Called when the route releases the endpoint. */
  close?(): Promise<void> | void;
}

/**
 * Register a custom endpoint implemented in JavaScript under `name`, making it
 * usable as an endpoint type in route configs — either as `{ pulsar: {...} }`
 * or explicitly as `{ custom: { name: "pulsar", config: {...} } }`.
 *
 * `factory` is called once per route leg. The returned object must implement
 * `receiveBatch` to be usable as an input and/or `sendBatch` to be usable as an
 * output. Register before starting a route that names it; registering the same
 * name twice throws and preserves the first factory.
 */
export function registerEndpoint(
  name: string,
  factory: (
    routeName: string,
    config: Record<string, JsonValue>,
  ) => CustomEndpoint | Promise<CustomEndpoint>,
): void;

/** Unregister a custom endpoint after all routes using it have stopped. */
export function unregisterEndpoint(name: string): boolean;

/**
 * Load a native endpoint plugin and register the endpoint it provides.
 *
 * `path` is the compiled plugin library shipped by an endpoint package (for
 * example `mq-bridge-pulsar`), which normally exposes its own `register()`
 * helper that resolves the bundled file and calls this. Returns the registered
 * endpoint name, usable as a route's endpoint type.
 *
 * Call once, before starting routes; loading the same file again is a no-op. A
 * plugin is native code with the same privileges as the Node process.
 */
export function loadEndpointPlugin(path: string): string;

/** Resolves the native library in an mq-bridge plugin package directory. */
export function pluginLibraryPath(packageDirectory: string): string;

/** Resolves and loads the native library in an mq-bridge plugin package. */
export function loadPluginPackage(packageDirectory: string): string;

/** Standard exports for a thin JavaScript plugin package entry point. */
export function definePluginPackage(packageDirectory: string): {
  ENDPOINT_NAME: string;
  libraryPath(): string;
  register(): string;
};

/** What a {@link registerMiddleware} factory returns. */
export interface CustomMiddleware {
  /** Applies on an input endpoint, after the source produced the batch. */
  onReceive?(messages: Message[]): Promise<(Message | null)[]> | (Message | null)[];
  /** Applies on an output endpoint, before the sink receives the batch. */
  onSend?(messages: Message[]): Promise<(Message | null)[]> | (Message | null)[];
}

/**
 * Register a custom middleware implemented in JavaScript under `name`, usable in
 * any endpoint's `middlewares` list as `{ custom: { name, config: {...} } }`.
 *
 * `factory` is called once per endpoint the middleware is attached to. A side
 * the returned object does not implement passes through untouched. Both hooks
 * must return one item per input message — a `Message` to keep it (possibly
 * rewritten) or `null` to drop it — which is what keeps acknowledgements aligned
 * with the source batch.
 */
export function registerMiddleware(
  name: string,
  factory: (
    routeName: string,
    config: Record<string, JsonValue>,
  ) => CustomMiddleware | Promise<CustomMiddleware>,
): void;

/** Unregister custom middleware after all routes using it have stopped. */
export function unregisterMiddleware(name: string): boolean;

export const version: string;
