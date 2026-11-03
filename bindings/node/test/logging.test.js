"use strict";

const assert = require("node:assert/strict");
const path = require("node:path");
const { execFileSync } = require("node:child_process");
const test = require("node:test");
const { Route, initLogging } = require("..");

// The subscriber installs once per process; `node --test` runs each test file in
// its own process, so this file owns that single install. The two in-process
// tests below therefore run in order: the first initialises, the second asserts
// re-initialising is rejected.

const MEMORY_ROUTE = `
routes:
  logtest:
    input:
      memory: { topic: "log.in", capacity: 8 }
    output:
      memory: { topic: "log.out", capacity: 8 }
`;

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

test("initLogging forwards core tracing events to the JS callback", async () => {
  const records = [];
  // "debug" lowers the Rust-side filter so the core's INFO channel events cross.
  initLogging((record) => records.push(record), "debug");

  const route = Route.fromStr(MEMORY_ROUTE, "logtest");
  route.start();
  await delay(300);
  route.stop();
  route.join();
  await delay(100); // let queued threadsafe-function calls drain into the loop

  const core = records.filter((r) => r.target.startsWith("mq_bridge"));
  assert.ok(core.length > 0, "no mq_bridge events reached the callback");

  const record = core[0];
  assert.equal(typeof record.level, "string");
  assert.equal(typeof record.target, "string");
  assert.equal(typeof record.message, "string");
  assert.ok(
    ["error", "warn", "info", "debug", "trace"].includes(record.level),
    `unexpected level: ${record.level}`,
  );
  assert.ok(
    core.some((r) => r.level === "info"),
    "expected at least one INFO event from core",
  );
});

test("initLogging rejects a second initialisation", () => {
  assert.throws(
    () => initLogging(() => {}, "debug"),
    /already initialized/,
  );
});

test("filtering happens in Rust below the requested level", () => {
  // Fresh process: at "error" the core's INFO channel events must be dropped
  // before they ever reach JS, proving the filter runs Rust-side.
  const pkg = path.resolve(__dirname, "..");
  const script = `
    const { Route, initLogging } = require(${JSON.stringify(pkg)});
    const records = [];
    initLogging((r) => records.push(r), "error");
    const route = Route.fromStr(${JSON.stringify(MEMORY_ROUTE)}, "logtest");
    route.start();
    setTimeout(() => {
      route.stop();
      route.join();
      setTimeout(() => {
        const info = records.filter((r) => r.target.startsWith("mq_bridge") && r.level === "info");
        console.log("INFO_COUNT", info.length);
      }, 100);
    }, 300);
  `;
  const out = execFileSync(process.execPath, ["-e", script], {
    encoding: "utf8",
    // Deterministic filter: don't let an ambient env var override "error".
    env: { ...process.env, MQ_BRIDGE_LOG: "", RUST_LOG: "" },
  });
  assert.match(out, /INFO_COUNT 0/, `expected no INFO events, got:\n${out}`);
});
