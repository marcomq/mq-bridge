"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const test = require("node:test");
const { Caller, Message, Route } = require("..");

test("Caller returns the answer and rejects with the handler error", async () => {
  const id = crypto.randomUUID();
  const route = Route.fromConfig(
    {
      input: { memory: { topic: `node.caller.${id}` } },
      output: { response: {} },
    },
    `caller-${id}`,
  );
  route.withHandler(async (message) => {
    const { n } = message.json();
    if (n < 0) {
      throw new Error("negative input");
    }
    return message.constructor.fromJson({ double: n * 2 });
  });

  const caller = Caller.fromRoute(route);
  try {
    const answers = await Promise.all(
      Array.from({ length: 8 }, (_, n) => caller.callJson({ n })),
    );
    assert.deepEqual(
      answers.map((answer) => answer.json().double),
      Array.from({ length: 8 }, (_, n) => n * 2),
    );

    await assert.rejects(caller.callJson({ n: -1 }), /negative input/);
    assert.deepEqual((await caller.callJson({ n: 4 })).json(), { double: 8 });

    const [first, failed, last] = await caller.callMany(
      [1, -1, 3].map((n) => Message.fromJson({ n })),
    );
    assert.deepEqual(first.value.json(), { double: 2 });
    assert.equal(failed.status, "rejected");
    assert.match(failed.reason.message, /negative input/);
    assert.deepEqual(last.value.json(), { double: 6 });
  } finally {
    assert.equal(await caller.stop(), true);
  }
});
