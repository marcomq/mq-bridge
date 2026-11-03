"use strict";

const assert = require("node:assert/strict");
const net = require("node:net");
const test = require("node:test");
const { Consumer, Message, Publisher, Route } = require("..");

async function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

async function postWithRetry(url, body, headers = {}) {
  let lastError;
  for (let attempt = 0; attempt < 30; attempt += 1) {
    try {
      return await fetch(url, {
        method: "POST",
        headers: {
          accept: "application/json",
          "content-type": "application/json",
          ...headers,
        },
        body: JSON.stringify(body),
      });
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }
  throw lastError;
}

function httpRouteConfig(routeName, port, path, withMetrics = false) {
  const metrics = withMetrics
    ? `      middlewares:
        - metrics: {}
`
    : "";

  return `
routes:
  ${routeName}:
    input:
${metrics}      http:
        url: "127.0.0.1:${port}"
        path: "${path}"
        method: "POST"
    output:
${metrics}      response: {}
`;
}

test("Message round-trips JSON, text, metadata, and ids", () => {
  const json = Message.fromJson({ value: 42 }, { kind: "demo" });
  assert.deepEqual(json.json(), { value: 42 });
  assert.equal(json.metadata.kind, "demo");
  assert.equal(json.id, null);

  const raw = new Message(Buffer.from("hello"), { source: "test" });
  assert.equal(raw.text(), "hello");
  assert.equal(raw.metadata.source, "test");
});

test("Publisher.requestJson echoes through response endpoint", async () => {
  const publisher = Publisher.fromStr(
    `
publishers:
  echo:
    response: {}
`,
    "echo",
  );

  const response = await publisher.requestJson({ orderId: 7 }, { kind: "order.test" });
  assert.deepEqual(response.json(), { orderId: 7 });
  assert.equal(response.metadata.kind, "order.test");

  // Keep the deprecated index.js wrapper alias exercised: fromYamlStr should
  // build an equivalent publisher.
  const aliased = Publisher.fromYamlStr(
    `
publishers:
  echo:
    response: {}
`,
    "echo",
  );
  const aliasResponse = await aliased.requestJson({ orderId: 8 });
  assert.deepEqual(aliasResponse.json(), { orderId: 8 });
});

test("Consumer.poll returns messages and commit acks them", async () => {
  const topic = `node.consumer.${Date.now()}`;
  const endpoint = { memory: { topic, capacity: 4096 } };

  const publisher = Publisher.fromConfig(endpoint);
  const consumer = Consumer.fromConfig(endpoint);

  for (let value = 0; value < 5; value += 1) {
    await publisher.sendJson({ value }, { kind: "node.tick" });
  }

  const received = [];
  while (received.length < 5) {
    const batch = await consumer.poll(10, 5000);
    assert.ok(batch.length > 0, "poll timed out before all messages arrived");
    received.push(...batch);
  }
  await consumer.commit();

  assert.deepEqual(
    received.map((message) => message.json().value),
    [0, 1, 2, 3, 4],
  );
  assert.equal(received[0].metadata.kind, "node.tick");
  assert.equal(consumer.exhausted, false);

  const status = await consumer.status();
  assert.equal(status.healthy, true);

  const empty = await consumer.poll(4, 200);
  assert.deepEqual(empty, []);

  await consumer.close();
  await consumer.close(); // idempotent
  await assert.rejects(() => consumer.poll(1, 50));
  await assert.rejects(() => consumer.status());
});

test("Publisher.sendBatch publishes every message", async () => {
  const topic = `node.sendbatch.${Date.now()}`;
  const endpoint = { memory: { topic, capacity: 4096 } };

  const publisher = Publisher.fromConfig(endpoint);
  const consumer = Consumer.fromConfig(endpoint);

  await publisher.sendBatch([
    Message.fromJson({ value: 0 }, { kind: "node.batch" }),
    Message.fromJson({ value: 1 }, { kind: "node.batch" }),
    Message.fromJson({ value: 2 }, { kind: "node.batch" }),
  ]);

  const received = [];
  while (received.length < 3) {
    const batch = await consumer.poll(10, 5000);
    assert.ok(batch.length > 0, "poll timed out before all messages arrived");
    received.push(...batch);
  }
  await consumer.commit();

  assert.deepEqual(
    received.map((message) => message.json().value),
    [0, 1, 2],
  );
  assert.equal(received[0].metadata.kind, "node.batch");

  await consumer.close();
});

test("Consumer.pollBatch returns a token and ack advances by batch", async () => {
  const topic = `node.consumer.pollbatch.${Date.now()}`;
  const endpoint = { memory: { topic, capacity: 4096 } };

  const publisher = Publisher.fromConfig(endpoint);
  const consumer = Consumer.fromConfig(endpoint);

  for (let value = 0; value < 3; value += 1) {
    await publisher.sendJson({ value });
  }

  const received = [];
  while (received.length < 3) {
    const { messages, token } = await consumer.pollBatch(10, 5000);
    assert.ok(messages.length > 0, "pollBatch timed out before all messages arrived");
    assert.notEqual(token, null);
    received.push(...messages);
    await consumer.ack(token); // ack just this batch by token
  }

  assert.deepEqual(received.map((m) => m.json().value), [0, 1, 2]);

  const empty = await consumer.pollBatch(4, 200);
  assert.deepEqual(empty.messages, []);
  assert.equal(empty.token, null);

  await assert.rejects(() => consumer.ack(9999)); // unknown token
  await consumer.close();
});

test("Consumer.nack redelivers a batch by token", async () => {
  const topic = `node.consumer.nack.${Date.now()}`;
  // enable_nack makes the in-memory endpoint requeue nacked messages.
  const endpoint = { memory: { topic, capacity: 4096, enable_nack: true } };

  const publisher = Publisher.fromConfig(endpoint);
  const consumer = Consumer.fromConfig(endpoint);

  await publisher.sendJson({ value: 1 });

  let { messages, token } = await consumer.pollBatch(10, 5000);
  assert.deepEqual(messages.map((m) => m.json().value), [1]);
  await consumer.nack(token); // release for redelivery instead of acking

  ({ messages, token } = await consumer.pollBatch(10, 5000));
  assert.deepEqual(messages.map((m) => m.json().value), [1], "nacked batch was not redelivered");
  await consumer.ack(token);
  await consumer.close();
});

test("Route.withHandler serves an HTTP response", async () => {
  const port = await freePort();
  const routeName = `node_with_handler_${port}`;
  const route = Route.fromStr(
    httpRouteConfig(routeName, port, "/with-handler"),
    routeName,
  );

  route.withHandler(async (message) => {
    const data = message.json();
    return Message.fromJson(
      { handledBy: "withHandler", value: data.value },
      { "content-type": "application/json" },
    );
  });

  try {
    route.start();
    const response = await postWithRetry(
      `http://127.0.0.1:${port}/with-handler`,
      { value: 11 },
    );
    const text = await response.text();
    assert.equal(response.status, 200, text);
    assert.deepEqual(JSON.parse(text), {
      handledBy: "withHandler",
      value: 11,
    });
  } finally {
    route.stop();
    route.join();
  }
});

test("Route.withBatchHandler serves an HTTP response", async () => {
  const port = await freePort();
  const routeName = `node_with_batch_handler_${port}`;
  const route = Route.fromStr(
    httpRouteConfig(routeName, port, "/with-batch-handler"),
    routeName,
  );

  route.withBatchHandler(async (messages) =>
    messages.map((message) =>
      Message.fromJson(
        { handledBy: "withBatchHandler", value: message.json().value },
        { "content-type": "application/json" },
      ),
    ),
  );

  try {
    route.start();
    const response = await postWithRetry(
      `http://127.0.0.1:${port}/with-batch-handler`,
      { value: 12 },
    );
    const text = await response.text();
    assert.equal(response.status, 200, text);
    assert.deepEqual(JSON.parse(text), {
      handledBy: "withBatchHandler",
      value: 12,
    });
  } finally {
    route.stop();
    route.join();
  }
});

test("Route.addHandler dispatches by kind with middleware config", async () => {
  const port = await freePort();
  const routeName = `node_add_handler_${port}`;
  const route = Route.fromStr(
    httpRouteConfig(routeName, port, "/typed-handler", true),
    routeName,
  );

  route.addHandler("order.created", async (data) => {
    return Message.fromJson(
      { handledBy: "addHandler", orderId: data.orderId },
      { "content-type": "application/json" },
    );
  });

  try {
    route.start();
    const response = await postWithRetry(
      `http://127.0.0.1:${port}/typed-handler`,
      { orderId: 42 },
      { kind: "order.created" },
    );
    const text = await response.text();
    assert.equal(response.status, 200, text);
    assert.deepEqual(JSON.parse(text), {
      handledBy: "addHandler",
      orderId: 42,
    });
  } finally {
    route.stop();
    route.join();
  }
});
