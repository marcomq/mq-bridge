"use strict";

const fs = require("node:fs");
const path = require("node:path");
const native = require("./native.js");

class Message {
  constructor(payload, metadata = null, id = null) {
    this._native = native.createMessage(Buffer.from(payload), metadata, id);
  }

  static fromJson(data, metadata = null, id = null) {
    return Message._fromNative(native.messageFromJson(data, metadata, id));
  }

  static _fromNative(raw) {
    const message = Object.create(Message.prototype);
    message._native = raw;
    return message;
  }

  static _toNative(message) {
    if (message instanceof Message) {
      return message._native;
    }
    return message;
  }

  static _toNativeResult(result) {
    if (result == null) {
      return null;
    }
    return Message._toNative(result);
  }

  get payload() {
    return Buffer.from(this._native.payload);
  }

  get metadata() {
    return { ...(this._native.metadata || {}) };
  }

  get id() {
    return this._native.id || null;
  }

  json() {
    return native.messageJson(this._native);
  }

  text() {
    return native.messageText(this._native);
  }
}

class Publisher {
  constructor(nativePublisher) {
    this._native = nativePublisher;
  }

  static fromFile(path, name) {
    return new Publisher(native.Publisher.fromFile(path, name));
  }

  static fromStr(text, name) {
    return new Publisher(native.Publisher.fromStr(text, name));
  }

  static fromConfig(config, name) {
    return new Publisher(native.Publisher.fromConfig(config, name));
  }

  /** @deprecated Use {@link Publisher.fromFile} instead. */
  static fromYaml(path, name) {
    return new Publisher(native.Publisher.fromFile(path, name));
  }

  /** @deprecated Use {@link Publisher.fromStr} instead. */
  static fromYamlStr(text, name) {
    return new Publisher(native.Publisher.fromStr(text, name));
  }

  send(message) {
    return this._native.send(Message._toNative(message));
  }

  sendBatch(messages) {
    return this._native.sendBatch(messages.map((message) => Message._toNative(message)));
  }

  async request(message) {
    return Message._fromNative(await this._native.request(Message._toNative(message)));
  }

  sendJson(data, metadata = null, id = null) {
    return this._native.sendJson(data, metadata, id);
  }

  async requestJson(data, metadata = null, id = null) {
    return Message._fromNative(await this._native.requestJson(data, metadata, id));
  }
}

class Consumer {
  constructor(nativeConsumer) {
    this._native = nativeConsumer;
  }

  static fromFile(path, name) {
    return new Consumer(native.Consumer.fromFile(path, name));
  }

  static fromStr(text, name) {
    return new Consumer(native.Consumer.fromStr(text, name));
  }

  static fromConfig(config, name) {
    return new Consumer(native.Consumer.fromConfig(config, name));
  }

  async poll(max, timeoutMs) {
    const messages = await this._native.poll(max, timeoutMs);
    return messages.map((message) => Message._fromNative(message));
  }

  async pollBatch(max, timeoutMs) {
    const { messages, token } = await this._native.pollBatch(max, timeoutMs);
    return {
      messages: messages.map((message) => Message._fromNative(message)),
      // Normalize the native `undefined` (no batch) to `null` per the typed API.
      token: token ?? null,
    };
  }

  commit() {
    return this._native.commit();
  }

  ack(token) {
    return this._native.ack(token);
  }

  nack(token) {
    return this._native.nack(token);
  }

  status() {
    return this._native.status();
  }

  close() {
    return this._native.close();
  }

  get exhausted() {
    return this._native.exhausted;
  }
}

class Route {
  constructor(nativeRoute) {
    this._native = nativeRoute;
  }

  static fromFile(path, name) {
    return new Route(native.Route.fromFile(path, name));
  }

  static fromStr(text, name) {
    return new Route(native.Route.fromStr(text, name));
  }

  static fromConfig(config, name) {
    return new Route(native.Route.fromConfig(config, name));
  }

  /** @deprecated Use {@link Route.fromFile} instead. */
  static fromYaml(path, name) {
    return new Route(native.Route.fromFile(path, name));
  }

  /** @deprecated Use {@link Route.fromStr} instead. */
  static fromYamlStr(text, name) {
    return new Route(native.Route.fromStr(text, name));
  }

  withHandler(handler) {
    this._native.withHandler(async (error, message) => {
      if (error) {
        throw error;
      }
      const result = await handler(Message._fromNative(message));
      return Message._toNativeResult(result);
    });
  }

  addHandler(kind, handler) {
    this._native.addHandler(kind, async (error, data) => {
      if (error) {
        throw error;
      }
      const result = await handler(data);
      return Message._toNativeResult(result);
    });
  }

  start() {
    this._native.start();
  }

  stop() {
    this._native.stop();
  }

  join() {
    this._native.join();
  }

  wait() {
    return this._native.wait();
  }
}

/** Throw this from `receiveBatch` to tell the route the source is finished. */
class EndOfStream extends Error {
  constructor(message = "end of stream") {
    super(message);
    this.name = "EndOfStream";
  }
}

/**
 * Register a custom endpoint implemented in JavaScript under `name`, making it
 * usable as an endpoint type in route configs — either as `{ pulsar: {...} }`
 * or explicitly as `{ custom: { name: "pulsar", config: {...} } }`.
 *
 * `factory(routeName, config)` is called once per route leg and returns an
 * object implementing `receiveBatch(maxMessages)` to be usable as an input
 * and/or `sendBatch(messages)` to be usable as an output, plus optional
 * `commit(dispositions)` and `close()`. All of them may be async.
 *
 * `receiveBatch` returns an array of `Message`/Buffer/string values, or
 * `null`/`[]` when nothing is available right now; throw `EndOfStream` to end
 * the route. `commit` receives one `"ack"`/`"nack"` per message in the batch.
 * Set `err.retryable = true` on a thrown error to have it retried.
 */
function registerEndpoint(name, factory) {
  if (typeof factory !== "function") {
    throw new TypeError("factory must be callable as factory(routeName, config)");
  }
  // The instance table lives here because JS objects cannot cross to Rust;
  // native code refers to each instance by the id it allocated.
  const instances = new Map();

  native.registerEndpointDispatch(name, async (error, call) => {
    if (error) {
      throw error;
    }
    try {
      return await dispatchEndpointCall(instances, factory, call);
    } catch (err) {
      if (err instanceof EndOfStream) {
        return { endOfStream: true };
      }
      return errorReply(err);
    }
  });
}

/** Unregister a custom endpoint after all routes using it have stopped. */
function unregisterEndpoint(name) {
  return native.unregisterEndpointDispatch(name);
}

/**
 * Report a thrown error back to Rust. `retryable` stays `undefined` unless the
 * host set it explicitly — collapsing it to `false` would classify every
 * ordinary error as permanent and stop the route instead of reconnecting.
 */
function errorReply(err) {
  return {
    error: String((err && err.stack) || err),
    retryable: typeof err?.retryable === "boolean" ? err.retryable : undefined,
  };
}

/**
 * Register a custom middleware implemented in JavaScript under `name`, usable in
 * any endpoint's `middlewares` list as
 * `{ custom: { name, config: {...} } }`.
 *
 * `factory(routeName, config)` is called once per endpoint the middleware is
 * attached to and returns an object implementing `onReceive(messages)` (applies
 * on an input endpoint) and/or `onSend(messages)` (applies on an output
 * endpoint). A side the object does not implement passes through untouched.
 *
 * Both hooks take the batch and must return one item per input message: a
 * `Message` (kept, possibly rewritten) or `null` to drop it. Keeping the length
 * fixed is what lets acknowledgements stay aligned with the source batch.
 */
function registerMiddleware(name, factory) {
  if (typeof factory !== "function") {
    throw new TypeError("factory must be callable as factory(routeName, config)");
  }
  const instances = new Map();

  native.registerMiddlewareDispatch(name, async (error, call) => {
    if (error) {
      throw error;
    }
    try {
      return await dispatchEndpointCall(instances, factory, call);
    } catch (err) {
      return errorReply(err);
    }
  });
}

/** Unregister a custom middleware after all routes using it have stopped. */
function unregisterMiddleware(name) {
  return native.unregisterMiddlewareDispatch(name);
}

async function dispatchEndpointCall(instances, factory, call) {
  if (call.op === "create") {
    const instance = await factory(call.routeName, call.config ?? {});
    instances.set(call.instance, instance);
    return {
      consumer: typeof instance?.receiveBatch === "function",
      publisher: typeof instance?.sendBatch === "function",
      onReceive: typeof instance?.onReceive === "function",
      onSend: typeof instance?.onSend === "function",
    };
  }

  const instance = instances.get(call.instance);
  if (!instance) {
    // `close` is also sent when the native side drops the endpoint, which can
    // race a close that already happened; that is not an error.
    if (call.op === "close") {
      return {};
    }
    throw new Error(`unknown endpoint instance ${call.instance}`);
  }

  switch (call.op) {
    case "receive": {
      const batch = await instance.receiveBatch(call.maxMessages);
      if (batch == null) {
        return { messages: [] };
      }
      return { messages: Array.from(batch, toNativeEndpointMessage) };
    }
    case "commit": {
      if (typeof instance.commit === "function") {
        await instance.commit(call.dispositions ?? []);
      }
      return {};
    }
    case "send": {
      const messages = (call.messages ?? []).map(Message._fromNative);
      await instance.sendBatch(messages);
      return {};
    }
    case "onReceive":
    case "onSend": {
      const hook = call.op === "onReceive" ? instance.onReceive : instance.onSend;
      const messages = (call.messages ?? []).map(Message._fromNative);
      const result = await hook.call(instance, messages);
      return {
        filtered: Array.from(result ?? [], (item) =>
          item == null ? null : toNativeEndpointMessage(item),
        ),
      };
    }
    case "close": {
      instances.delete(call.instance);
      if (typeof instance.close === "function") {
        await instance.close();
      }
      return {};
    }
    default:
      throw new Error(`unknown endpoint op '${call.op}'`);
  }
}

/** Accepts what an endpoint may yield: a Message, a Buffer, or a string. */
function toNativeEndpointMessage(item) {
  if (item instanceof Message) {
    return item._native;
  }
  if (typeof item === "string") {
    return native.createMessage(Buffer.from(item), null, null);
  }
  if (Buffer.isBuffer(item) || item instanceof Uint8Array) {
    return native.createMessage(Buffer.from(item), null, null);
  }
  // Anything else: treat as JSON, matching the Python binding.
  return native.messageFromJson(item, null, null);
}

function configSchema() {
  if (typeof native.configSchema !== "function") {
    throw new Error(
      "configSchema() is unavailable: this build was compiled without the 'schema' feature",
    );
  }
  return native.configSchema();
}

function initLogging(callback, level = null) {
  return native.initLogging(callback, level);
}

function requestShutdown() {
  return native.requestShutdown();
}

function isShutdownRequested() {
  return native.isShutdownRequested();
}

function loadEndpointPlugin(pluginPath) {
  return native.loadEndpointPlugin(pluginPath);
}

function pluginPlatformTag(platform = process.platform, arch = process.arch) {
  const normalizedArch = arch === "x86_64" ? "x64" : arch === "aarch64" ? "arm64" : arch;
  const suffix = platform === "linux" ? "-gnu" : platform === "win32" ? "-msvc" : "";
  return `${platform}-${normalizedArch}${suffix}`;
}

function readPluginManifest(packageDirectory) {
  const root = path.resolve(packageDirectory);
  const manifestPath = path.join(root, "mq-bridge-plugin.json");
  if (!fs.existsSync(manifestPath)) {
    throw new Error(`mq-bridge plugin manifest not found: ${manifestPath}`);
  }
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  if (typeof manifest.name !== "string" || typeof manifest.library !== "string") {
    throw new Error(`${manifestPath} must contain string fields 'name' and 'library'`);
  }
  return { root, manifest };
}

function pluginLibraryPath(packageDirectory) {
  const { root, manifest } = readPluginManifest(packageDirectory);
  const fileName = process.platform === "win32"
    ? `${manifest.library}.dll`
    : process.platform === "darwin"
      ? `lib${manifest.library}.dylib`
      : `lib${manifest.library}.so`;
  const candidates = [
    path.join(root, "prebuilds", pluginPlatformTag(), fileName),
    path.join(root, fileName),
  ];
  const library = candidates.find((candidate) => fs.existsSync(candidate));
  if (library) {
    return library;
  }
  throw new Error(
    `no native library for ${pluginPlatformTag()} in mq-bridge plugin package ${root}; ` +
      `looked in ${candidates.join(", ")}`,
  );
}

function loadPluginPackage(packageDirectory) {
  return loadEndpointPlugin(pluginLibraryPath(packageDirectory));
}

function definePluginPackage(packageDirectory) {
  const { manifest } = readPluginManifest(packageDirectory);
  return {
    ENDPOINT_NAME: manifest.name,
    libraryPath: () => pluginLibraryPath(packageDirectory),
    register: () => loadPluginPackage(packageDirectory),
  };
}

module.exports = {
  Message,
  Publisher,
  Consumer,
  Route,
  EndOfStream,
  configSchema,
  initLogging,
  isShutdownRequested,
  requestShutdown,
  definePluginPackage,
  loadEndpointPlugin,
  loadPluginPackage,
  pluginLibraryPath,
  registerEndpoint,
  registerMiddleware,
  unregisterEndpoint,
  unregisterMiddleware,
  version: native.VERSION,
};
