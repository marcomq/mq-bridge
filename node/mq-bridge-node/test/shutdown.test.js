"use strict";

// The shutdown latch is process-global and permanent, and one case raises a real
// signal, so each scenario runs in a fresh child process.

const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const path = require("node:path");
const test = require("node:test");

const PRELUDE = `
const mqb = require(${JSON.stringify(path.resolve(__dirname, ".."))});
function makeRoute(tag) {
  return mqb.Route.fromConfig({
    input: { memory: { topic: "shutdown." + tag + ".in", capacity: 8 } },
    output: { memory: { topic: "shutdown." + tag + ".out", capacity: 8 } },
  }, "shutdown-" + tag);
}
`;

function runScript(body) {
  const proc = spawnSync(process.execPath, ["-e", `${PRELUDE}\n(async () => {\n${body}\n})().catch((e) => { console.error(e); process.exit(1); });`], {
    encoding: "utf8",
    timeout: 30_000,
  });
  assert.equal(proc.status, 0, proc.stderr);
  return proc.stdout;
}

test("requestShutdown stops running and later routes", () => {
  const out = runScript(`
    if (mqb.isShutdownRequested()) throw new Error("latched too early");
    const route = makeRoute("a");
    route.start();
    let first;
    setTimeout(() => { first = mqb.requestShutdown(); }, 300);
    await route.wait();
    if (first !== true) throw new Error("first request must return true");
    if (mqb.requestShutdown() !== false) throw new Error("second request must return false");

    const started = Date.now();
    const later = makeRoute("b");
    later.start();
    await later.wait();
    if (Date.now() - started > 2000) throw new Error("later route did not stop promptly");
    console.log("OK");
  `);
  assert.match(out, /OK/);
});

test("a SIGINT handler can stop a route while wait() is pending", { skip: process.platform === "win32" }, () => {
  const out = runScript(`
    process.on("SIGINT", () => mqb.requestShutdown());
    const route = makeRoute("sig");
    route.start();
    setTimeout(() => process.kill(process.pid, "SIGINT"), 300);
    await route.wait();
    if (!mqb.isShutdownRequested()) throw new Error("handler did not run");
    console.log("OK");
  `);
  assert.match(out, /OK/);
});
