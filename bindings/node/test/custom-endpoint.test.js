"use strict";

const assert = require("node:assert/strict");
const test = require("node:test");
const { randomUUID } = require("node:crypto");
const {
  Consumer,
  EndOfStream,
  Message,
  Publisher,
  Route,
  registerEndpoint: registerEndpointNative,
  registerMiddleware: registerMiddlewareNative,
  unregisterEndpoint,
  unregisterMiddleware,
} = require("..");

const unique = (prefix) => `${prefix}.${randomUUID().replace(/-/g, "")}`;

/**
 * A live registration holds the JS dispatcher, which keeps the Node event loop
 * alive — a leaked one hangs the whole run instead of failing a test. Register
 * only through these wrappers so cleanup cannot be forgotten.
 */
const cleanups = [];

function registerEndpoint(name, factory) {
  registerEndpointNative(name, factory);
  cleanups.push(() => unregisterEndpoint(name));
}

function registerMiddleware(name, factory) {
  registerMiddlewareNative(name, factory);
  cleanups.push(() => unregisterMiddleware(name));
}

test.after(() => {
  for (const cleanup of cleanups) cleanup();
});

/** Register under a unique name: the endpoint registry is process-global. */
function register(prefix, factory) {
  const name = `${prefix}_${randomUUID().slice(0, 8)}`;
  registerEndpoint(name, factory);
  return name;
}

/**
 * `join()` blocks the JS thread, which starves the dispatch a JS endpoint needs.
 * Yield to the event loop first so the route can finish its teardown.
 */
async function shutdown(route) {
  route.stop();
  await new Promise((resolve) => setTimeout(resolve, 50));
  route.join();
}

function deferred() {
  let resolve;
  const promise = new Promise((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

test("a JS source drains into a memory sink", async () => {
  const outTopic = unique("node.custom.out");
  const payloads = ["one", "two", "three"];
  const commits = [];

  const name = register("jssrc", () => ({
    async receiveBatch(maxMessages) {
      if (payloads.length === 0) {
        throw new EndOfStream();
      }
      return payloads.splice(0, maxMessages);
    },
    async commit(dispositions) {
      commits.push(...dispositions);
    },
  }));

  const route = Route.fromConfig(
    {
      routes: {
        r: {
          exit_on_empty: true,
          input: { [name]: {} },
          output: { memory: { topic: outTopic, capacity: 4096 } },
        },
      },
    },
    "r",
  );
  const consumer = Consumer.fromConfig(
    { memory: { topic: outTopic, capacity: 4096 } },
    "",
  );

  route.start();
  try {
    const received = [];
    while (received.length < 3) {
      const messages = await consumer.poll(10, 5000);
      received.push(...messages.map((m) => m.payload.toString()));
    }
    assert.deepEqual(received.sort(), ["one", "three", "two"]);
  } finally {
    await shutdown(route);
  }

  assert.deepEqual(commits, ["ack", "ack", "ack"]);
});

test("a JS factory receives the route name and its config block", async () => {
  const outTopic = unique("node.custom.cfg");
  let seen = null;

  const name = register("jscfg", (routeName, config) => {
    seen = { routeName, config };
    let sent = false;
    return {
      async receiveBatch() {
        if (sent) {
          throw new EndOfStream();
        }
        sent = true;
        return [Buffer.from("payload")];
      },
    };
  });

  const route = Route.fromConfig(
    {
      routes: {
        cfg_route: {
          exit_on_empty: true,
          input: { [name]: { url: "pulsar://localhost:6650", batch: 7 } },
          output: { memory: { topic: outTopic, capacity: 64 } },
        },
      },
    },
    "cfg_route",
  );
  const consumer = Consumer.fromConfig({ memory: { topic: outTopic, capacity: 64 } }, "");

  route.start();
  try {
    const messages = await consumer.poll(1, 5000);
    assert.equal(messages.length, 1);
  } finally {
    await shutdown(route);
  }

  assert.equal(seen.routeName, "cfg_route");
  assert.deepEqual(seen.config, { url: "pulsar://localhost:6650", batch: 7 });
});

test("a JS sink receives published messages", async () => {
  const inTopic = unique("node.custom.in");
  const received = [];
  const arrived = deferred();

  const name = register("jssink", () => ({
    async sendBatch(messages) {
      received.push(...messages.map((m) => m.payload.toString()));
      arrived.resolve();
    },
  }));

  const config = {
    routes: {
      sink_route: {
        input: { memory: { topic: inTopic, capacity: 4096 } },
        output: { [name]: {} },
      },
    },
    publishers: { pub: { memory: { topic: inTopic, capacity: 4096 } } },
  };
  const route = Route.fromConfig(config, "sink_route");
  const publisher = Publisher.fromConfig(config, "pub");

  route.start();
  try {
    await publisher.send(new Message(Buffer.from("hello")));
    await arrived.promise;
  } finally {
    await shutdown(route);
  }

  assert.deepEqual(received, ["hello"]);
});

test("a throwing JS sink dead-letters the message", async () => {
  const inTopic = unique("node.custom.dlq.in");
  const dlqTopic = unique("node.custom.dlq.out");

  const name = register("jsfail", () => ({
    async sendBatch() {
      throw new Error("sink rejected the batch");
    },
  }));

  const config = {
    routes: {
      dlq_route: {
        input: { memory: { topic: inTopic, capacity: 4096 } },
        output: {
          [name]: {},
          middlewares: [
            { dlq: { endpoint: { memory: { topic: dlqTopic, capacity: 4096 } } } },
          ],
        },
      },
    },
    publishers: { pub: { memory: { topic: inTopic, capacity: 4096 } } },
  };
  const route = Route.fromConfig(config, "dlq_route");
  const publisher = Publisher.fromConfig(config, "pub");
  const dlq = Consumer.fromConfig({ memory: { topic: dlqTopic, capacity: 4096 } }, "");

  route.start();
  try {
    await publisher.send(new Message(Buffer.from("poison")));
    const messages = await dlq.poll(1, 5000);
    assert.equal(messages.length, 1);
    assert.equal(messages[0].payload.toString(), "poison");
  } finally {
    await shutdown(route);
  }
});

test("an endpoint without receiveBatch cannot be an input", async () => {
  const outTopic = unique("node.custom.bad");
  let closed = false;
  const name = register("jssinkonly", () => ({
    async sendBatch() {},
    close() {
      closed = true;
    },
  }));

  const route = Route.fromConfig(
    {
      routes: {
        bad_route: {
          exit_on_empty: true,
          // A factory error looks like a failed connection to the route, so it
          // is retried; drop the backoff to keep the test quick.
          reconnect_interval_ms: 1,
          input: { [name]: {} },
          output: { memory: { topic: outTopic, capacity: 64 } },
        },
      },
    },
    "bad_route",
  );

  route.start();
  // Keep the event loop free until capability rejection has cleaned up the
  // instance. This bounds the wait without assuming how quickly CI starts it.
  for (let attempt = 0; attempt < 50 && !closed; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.equal(closed, true, "endpoint was not closed within 5 seconds");
  assert.throws(() => route.join(), /receiveBatch/);
});

test("a capability getter failure cleans up the endpoint instance", async () => {
  let closed = false;
  const name = register("jsthrowinggetter", () => ({
    get receiveBatch() {
      const error = new Error("capability inspection failed");
      error.retryable = false;
      throw error;
    },
    close() {
      closed = true;
    },
  }));
  const route = Route.fromConfig(
    {
      routes: {
        getter_route: {
          input: { [name]: {} },
          output: { memory: { topic: unique("node.custom.getter"), capacity: 64 } },
        },
      },
    },
    "getter_route",
  );

  route.start();
  for (let attempt = 0; attempt < 50 && !closed; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.equal(closed, true, "endpoint was not closed within 5 seconds");
  assert.throws(() => route.join(), /capability inspection failed/);
});

test("registerEndpoint rejects a non-callable factory", () => {
  assert.throws(() => registerEndpoint("not_callable", {}), TypeError);
});

test("unregistering reports whether a registration existed and frees the name", () => {
  const factory = () => ({ async sendBatch() {} });
  const name = register("jsunreg", factory);

  assert.equal(unregisterEndpoint(name), true);
  assert.equal(unregisterEndpoint(name), false, "a second call finds nothing");

  assert.equal(unregisterMiddleware(registerMw("mwunreg", () => ({}))), true);
  assert.equal(unregisterMiddleware("mw_never_registered"), false);

  // The registry rejects duplicates, so re-registering proves the name is free.
  registerEndpoint(name, factory);
});

/** Rewrites every message and drops the ones the config names. */
function tagger(config) {
  const drop = new Set(config.drop ?? []);
  const apply = (messages) =>
    messages.map((m) => {
      const payload = m.payload.toString();
      return drop.has(payload) ? null : new Message(Buffer.from(payload + "!"), m.metadata);
    });
  return { onReceive: apply, onSend: apply };
}

function registerMw(prefix, factory) {
  const name = `${prefix}_${randomUUID().slice(0, 8)}`;
  registerMiddleware(name, factory);
  return name;
}

test("middleware rewrites and drops on the input side", async () => {
  const outTopic = unique("node.mw.out");
  const payloads = ["keep", "drop-me", "also-keep"];
  const commits = [];

  const endpoint = register("mwsrc", () => ({
    async receiveBatch(maxMessages) {
      if (payloads.length === 0) {
        throw new EndOfStream();
      }
      return payloads.splice(0, maxMessages);
    },
    async commit(dispositions) {
      commits.push(...dispositions);
    },
  }));
  const mw = registerMw("tagger", (routeName, config) => tagger(config));

  const route = Route.fromConfig(
    {
      routes: {
        mw_route: {
          exit_on_empty: true,
          input: {
            [endpoint]: {},
            middlewares: [{ custom: { name: mw, config: { drop: ["drop-me"] } } }],
          },
          output: { memory: { topic: outTopic, capacity: 4096 } },
        },
      },
    },
    "mw_route",
  );
  const consumer = Consumer.fromConfig({ memory: { topic: outTopic, capacity: 4096 } }, "");

  route.start();
  try {
    const received = [];
    while (received.length < 2) {
      const messages = await consumer.poll(10, 5000);
      received.push(...messages.map((m) => m.payload.toString()));
    }
    assert.deepEqual(received.sort(), ["also-keep!", "keep!"]);
  } finally {
    await shutdown(route);
  }

  // The dropped message is still acked at the source, or it would come back.
  assert.deepEqual(commits, ["ack", "ack", "ack"]);
});

test("middleware dropping a whole batch does not end the route", async () => {
  // The source hands over one message per call, so the middleware drops an
  // entire batch before the surviving one arrives. If that empty result were
  // passed up, `exit_on_empty` would end the route and "keep" would be lost.
  const outTopic = unique("node.mw.dropall");
  const payloads = ["drop-me", "keep"];
  const commits = [];

  const endpoint = register("mwdropall", () => ({
    async receiveBatch() {
      if (payloads.length === 0) {
        throw new EndOfStream();
      }
      return payloads.splice(0, 1);
    },
    async commit(dispositions) {
      commits.push(...dispositions);
    },
  }));
  const mw = registerMw("dropall", (routeName, config) => tagger(config));

  const route = Route.fromConfig(
    {
      routes: {
        drop_route: {
          exit_on_empty: true,
          input: {
            [endpoint]: {},
            middlewares: [{ custom: { name: mw, config: { drop: ["drop-me"] } } }],
          },
          output: { memory: { topic: outTopic, capacity: 4096 } },
        },
      },
    },
    "drop_route",
  );
  const consumer = Consumer.fromConfig({ memory: { topic: outTopic, capacity: 4096 } }, "");

  route.start();
  try {
    const received = [];
    while (received.length < 1) {
      const messages = await consumer.poll(10, 5000);
      received.push(...messages.map((m) => m.payload.toString()));
    }
    assert.deepEqual(received, ["keep!"]);
  } finally {
    await shutdown(route);
  }

  // Both acked: the dropped one by the middleware, the kept one by the route.
  assert.deepEqual(commits, ["ack", "ack"]);
});

test("middleware rewrites on the output side", async () => {
  const inTopic = unique("node.mw.in");
  const received = [];
  const arrived = deferred();

  const endpoint = register("mwsink", () => ({
    async sendBatch(messages) {
      received.push(...messages.map((m) => m.payload.toString()));
      arrived.resolve();
    },
  }));
  const mw = registerMw("tagger_out", (routeName, config) => tagger(config));

  const config = {
    routes: {
      mw_out_route: {
        input: { memory: { topic: inTopic, capacity: 4096 } },
        output: {
          [endpoint]: {},
          middlewares: [{ custom: { name: mw, config: {} } }],
        },
      },
    },
    publishers: { pub: { memory: { topic: inTopic, capacity: 4096 } } },
  };
  const route = Route.fromConfig(config, "mw_out_route");
  const publisher = Publisher.fromConfig(config, "pub");

  route.start();
  try {
    await publisher.send(new Message(Buffer.from("payload")));
    await arrived.promise;
  } finally {
    await shutdown(route);
  }

  assert.deepEqual(received, ["payload!"]);
});

test("a middleware without either hook passes through", async () => {
  const inTopic = unique("node.mw.noop.in");
  const received = [];
  const arrived = deferred();

  const endpoint = register("mwnoop", () => ({
    async sendBatch(messages) {
      received.push(...messages.map((m) => m.payload.toString()));
      arrived.resolve();
    },
  }));
  const mw = registerMw("noop", () => ({}));

  const config = {
    routes: {
      noop_route: {
        input: { memory: { topic: inTopic, capacity: 4096 } },
        output: {
          [endpoint]: {},
          middlewares: [{ custom: { name: mw, config: {} } }],
        },
      },
    },
    publishers: { pub: { memory: { topic: inTopic, capacity: 4096 } } },
  };
  const route = Route.fromConfig(config, "noop_route");
  const publisher = Publisher.fromConfig(config, "pub");

  route.start();
  try {
    await publisher.send(new Message(Buffer.from("untouched")));
    await arrived.promise;
  } finally {
    await shutdown(route);
  }

  assert.deepEqual(received, ["untouched"]);
});
