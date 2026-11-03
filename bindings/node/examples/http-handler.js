"use strict";

const assert = require("node:assert/strict");
const net = require("node:net");
const { Message, Route } = require("..");

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

async function postWithRetry(url, body) {
  let lastError;
  for (let attempt = 0; attempt < 30; attempt += 1) {
    try {
      return await fetch(url, {
        method: "POST",
        headers: {
          accept: "application/json",
          "content-type": "application/json",
          kind: "order.created",
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

async function main() {
  const port = await freePort();
  const config = `
routes:
  node_http_handler:
    input:
      middlewares:
        - metrics: {}
      http:
        url: "127.0.0.1:${port}"
        path: "/orders"
        method: "POST"
    output:
      middlewares:
        - metrics: {}
      response: {}
`;

  const route = Route.fromStr(config, "node_http_handler");
  route.addHandler("order.created", async (data) => {
    return Message.fromJson(
      {
        accepted: true,
        orderId: data.orderId,
      },
      {
        "content-type": "application/json",
      },
    );
  });

  try {
    route.start();
    const response = await postWithRetry(`http://127.0.0.1:${port}/orders`, {
      orderId: 42,
    });
    const text = await response.text();
    assert.equal(response.status, 200, text);
    assert.deepEqual(JSON.parse(text), {
      accepted: true,
      orderId: 42,
    });
  } finally {
    route.stop();
    route.join();
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
