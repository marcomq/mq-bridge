"use strict";

/**
 * Native-load HTTP benchmark for the mq-bridge Node binding vs other Node
 * HTTP frameworks.
 *
 * Like the Python `analysis/bench_http_native.py`, this drives each server with
 * an external load generator (`wrk`) rather than an in-process client loop, so
 * the numbers reflect the server and not the load tool. Each server runs in its
 * own child process; `wrk` posts a tiny JSON body and the server echoes it with
 * `value` incremented.
 *
 * Usage:
 *   node analysis/bench_http_native.js --connections 1,8,32 --duration 8
 *   node analysis/bench_http_native.js --targets mqb,node-http,fastify
 *
 * Requires `wrk` on PATH (`brew install wrk`). The `fastify` / `express`
 * targets additionally require those packages to be importable (install with
 * `npm i -D fastify express` in bindings/node). The `mqb` target needs
 * the addon built with the `http` feature (`npm run build` or `npm run
 * build:ci`).
 */

const net = require("node:net");
const path = require("node:path");
const os = require("node:os");
const fs = require("node:fs");
const { spawn, spawnSync } = require("node:child_process");

const HOST = "127.0.0.1";
const PATH = "/bench";
const KIND = "bench.tick";
const BODY = '{"value":0}';

const REQ_RE = /Requests\/sec:\s*([0-9.]+)/;
const LAT_RE = /Latency\s+([0-9.]+\w+)/;

// Absolute path to this package's entry point, so child processes can require
// the binding regardless of their cwd.
const MQB_MAIN = require.resolve("..");

function parseCsvInts(value) {
  const out = value
    .split(",")
    .map((p) => p.trim())
    .filter(Boolean)
    .map((p) => Number.parseInt(p, 10));
  if (!out.length || out.some((v) => !Number.isInteger(v) || v < 1)) {
    throw new Error(`expected positive integers, got '${value}'`);
  }
  return out;
}

function parseArgs(argv) {
  const args = {
    connections: [1, 8, 32],
    duration: 5,
    targets: "mqb,node-http,uws,fastify,express",
    routeConcurrency: 8,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => argv[(i += 1)];
    if (arg === "--connections") args.connections = parseCsvInts(next());
    else if (arg === "--duration") args.duration = Number.parseInt(next(), 10);
    else if (arg === "--targets") args.targets = next();
    else if (arg === "--route-concurrency") args.routeConcurrency = Number.parseInt(next(), 10);
    else throw new Error(`unknown argument '${arg}'`);
  }
  if (!Number.isInteger(args.duration) || args.duration <= 0) {
    throw new Error("--duration must be a positive integer");
  }
  if (!Number.isInteger(args.routeConcurrency) || args.routeConcurrency <= 0) {
    throw new Error("--route-concurrency must be a positive integer");
  }
  return args;
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, HOST, () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

function tryConnect(port) {
  return new Promise((resolve) => {
    const socket = net.createConnection({ host: HOST, port }, () => {
      socket.destroy();
      resolve(true);
    });
    socket.once("error", () => {
      socket.destroy();
      resolve(false);
    });
    socket.setTimeout(200, () => {
      socket.destroy();
      resolve(false);
    });
  });
}

async function waitForPort(port, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await tryConnect(port)) return;
    await new Promise((r) => setTimeout(r, 20));
  }
  throw new Error(`server on ${HOST}:${port} did not become ready`);
}

function writeTemp(prefix, name, contents) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  const file = path.join(dir, name);
  fs.writeFileSync(file, contents, "utf8");
  return file;
}

function wrkLua(sendKind) {
  const lines = [
    'wrk.method = "POST"',
    `wrk.body = '${BODY}'`,
    'wrk.headers["Content-Type"] = "application/json"',
  ];
  if (sendKind) lines.push(`wrk.headers["kind"] = "${KIND}"`);
  return lines.join("\n") + "\n";
}

function runWrk(port, connections, duration, luaPath) {
  const threads = Math.min(connections, os.cpus().length || 8);
  const result = spawnSync(
    "wrk",
    [
      `-t${threads}`,
      `-c${connections}`,
      `-d${duration}s`,
      "-s",
      luaPath,
      `http://${HOST}:${port}${PATH}`,
    ],
    { encoding: "utf8" },
  );
  if (result.error) {
    throw new Error(`failed to run wrk: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(
      `wrk exited with status ${result.status}: ${(result.stderr || "").trim()}`,
    );
  }
  const out = result.stdout || "";
  const req = REQ_RE.exec(out);
  const lat = LAT_RE.exec(out);
  return { rps: req ? Number.parseFloat(req[1]) : 0, lat: lat ? lat[1] : "?" };
}

// --- server launchers --------------------------------------------------------
// Each returns { port, sendKind, proc }. The caller awaits readiness and kills
// `proc` when done.

async function spawnNode(source, prefix, port) {
  const script = writeTemp(prefix, "server.js", source);
  const proc = spawn(process.execPath, [script], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  proc.stderr.on("data", (chunk) => {
    stderr += chunk.toString();
  });
  let ready = false;
  const exitPromise = new Promise((_, reject) => {
    proc.once("exit", (code, signal) => {
      // Once ready, exit is the expected teardown; only treat it as a crash
      // while we are still waiting for the port.
      if (ready) return;
      reject(
        new Error(
          `server (${prefix}) exited before becoming ready ` +
            `(code=${code}, signal=${signal}):\n${stderr.trim()}`,
        ),
      );
    });
  });
  await Promise.race([waitForPort(port), exitPromise]);
  ready = true;
  return proc;
}

async function mqbServer(routeConcurrency, variant = "kind") {
  const port = await freePort();
  const config = `
routes:
  http_bench:
    concurrency: ${routeConcurrency}
    batch_size: 128
    input:
      http:
        url: "${HOST}:${port}"
        path: "${PATH}"
        method: "POST"
        internal_buffer_size: 8192
        request_timeout_ms: 30000
        concurrency_limit: 512
        inline_response_fast_path: true
    output:
      response: {}
`;
  // "kind": addHandler delivers a JS object decoded by Rust/napi and returns
  //   Message.fromJson (napi marshals the object back to serde_json). Two JSON
  //   round-trips cross the boundary as structured values.
  // "raw": withHandler hands the body Buffer straight to JS; V8 does the
  //   JSON.parse/stringify and we return raw bytes — no napi value marshaling.
  const handler =
    variant === "raw"
      ? `route.withHandler((m) => {
           const d = JSON.parse(m.payload);
           d.value += 1;
           return new Message(Buffer.from(JSON.stringify(d)));
         });`
      : `route.addHandler(${JSON.stringify(KIND)}, (data) => Message.fromJson({ value: data.value + 1 }));`;
  // route.start() deploys on a background runtime; the handler runs on THIS
  // process's event loop via the threadsafe function, so we must keep the loop
  // alive without blocking it (no join()) or the callbacks would never run.
  const source = `
"use strict";
const { Route, Message } = require(${JSON.stringify(MQB_MAIN)});
const route = Route.fromStr(${JSON.stringify(config)}, "http_bench");
${handler}
route.start();
const keepAlive = setInterval(() => {}, 1 << 30);
process.on("SIGTERM", () => { try { route.stop(); } catch (_) {} clearInterval(keepAlive); process.exit(0); });
`;
  const proc = await spawnNode(source, "mqb-node-", port);
  return { port, sendKind: variant === "kind", proc };
}

async function nodeHttpServer() {
  const port = await freePort();
  const source = `
"use strict";
const http = require("node:http");
const server = http.createServer((req, res) => {
  if (req.method !== "POST") { res.writeHead(404); res.end(); return; }
  let body = "";
  req.on("data", (c) => { body += c; });
  req.on("end", () => {
    const data = JSON.parse(body);
    data.value += 1;
    const rb = JSON.stringify(data);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(rb);
  });
});
server.listen(${port}, ${JSON.stringify(HOST)});
`;
  const proc = await spawnNode(source, "node-http-", port);
  return { port, sendKind: false, proc };
}

async function fastifyServer() {
  const port = await freePort();
  const source = `
"use strict";
const Fastify = require(${JSON.stringify(resolvePeer("fastify"))});
const app = Fastify({ logger: false });
app.post(${JSON.stringify(PATH)}, async (req) => { const d = req.body; d.value += 1; return d; });
app.listen({ port: ${port}, host: ${JSON.stringify(HOST)} });
`;
  const proc = await spawnNode(source, "fastify-", port);
  return { port, sendKind: false, proc };
}

async function uwsServer() {
  const port = await freePort();
  // uWebSockets.js is the Node HTTP leader on public benchmarks (http-arena,
  // TechEmpower): a C++ socket layer that bypasses Node's `http` stack.
  const source = `
"use strict";
const uWS = require(${JSON.stringify(resolvePeer("uWebSockets.js"))});
uWS.App()
  .post(${JSON.stringify(PATH)}, (res, req) => {
    res.onAborted(() => {});
    const chunks = [];
    res.onData((ab, isLast) => {
      chunks.push(Buffer.from(ab));
      if (isLast) {
        const data = JSON.parse(Buffer.concat(chunks));
        data.value += 1;
        const rb = JSON.stringify(data);
        res.cork(() => {
          res.writeHeader("Content-Type", "application/json").end(rb);
        });
      }
    });
  })
  .listen(${JSON.stringify(HOST)}, ${port}, (token) => { if (!token) process.exit(1); });
`;
  const proc = await spawnNode(source, "uws-", port);
  return { port, sendKind: false, proc };
}

async function expressServer() {
  const port = await freePort();
  const source = `
"use strict";
const express = require(${JSON.stringify(resolvePeer("express"))});
const app = express();
app.use(express.json());
app.post(${JSON.stringify(PATH)}, (req, res) => { const d = req.body; d.value += 1; res.json(d); });
app.listen(${port}, ${JSON.stringify(HOST)});
`;
  const proc = await spawnNode(source, "express-", port);
  return { port, sendKind: false, proc };
}

function resolvePeer(name) {
  // Resolve relative to this package so children find the dep no matter their cwd.
  return require.resolve(name, { paths: [path.join(__dirname, "..")] });
}

function peerInstalled(name) {
  try {
    resolvePeer(name);
    return true;
  } catch (_) {
    return false;
  }
}

const TARGETS = {
  mqb: { run: (a) => mqbServer(a.routeConcurrency, "kind"), needs: null },
  "mqb-raw": { run: (a) => mqbServer(a.routeConcurrency, "raw"), needs: null },
  "node-http": { run: () => nodeHttpServer(), needs: null },
  uws: { run: () => uwsServer(), needs: "uWebSockets.js" },
  fastify: { run: () => fastifyServer(), needs: "fastify" },
  express: { run: () => expressServer(), needs: "express" },
};

async function killProc(proc) {
  if (!proc || proc.exitCode !== null) return;
  proc.kill("SIGTERM");
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      proc.kill("SIGKILL");
      resolve();
    }, 5000);
    proc.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

async function main() {
  if (spawnSync("wrk", ["--version"], { encoding: "utf8" }).error) {
    throw new Error("wrk not found on PATH (brew install wrk)");
  }
  const args = parseArgs(process.argv.slice(2));
  const requested = args.targets
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);

  const targets = [];
  for (const t of requested) {
    const target = TARGETS[t];
    if (!target) throw new Error(`unknown target '${t}'; choices: ${Object.keys(TARGETS).join(", ")}`);
    if (target.needs && !peerInstalled(target.needs)) {
      console.log(`skipping ${t}: '${target.needs}' not installed (npm i -D ${target.needs})`);
      continue;
    }
    targets.push(t);
  }
  if (!targets.length) throw new Error("no runnable targets");

  const results = {};
  for (const target of targets) {
    results[target] = {};
    let server;
    try {
      server = await TARGETS[target].run(args);
    } catch (error) {
      console.log(`skipping ${target}: ${error.message}`);
      continue;
    }
    try {
      const lua = writeTemp("wrk-", "post.lua", wrkLua(server.sendKind));
      runWrk(server.port, Math.max(...args.connections), 2, lua); // warmup
      for (const conn of args.connections) {
        const { rps, lat } = runWrk(server.port, conn, args.duration, lua);
        results[target][conn] = rps;
        console.log(
          `${target.padEnd(12)} c=${String(conn).padEnd(4)} ${rps
            .toLocaleString("en-US", { maximumFractionDigits: 0 })
            .padStart(12)} req/s  (lat ${lat})`,
        );
      }
    } finally {
      await killProc(server.proc);
    }
  }

  console.log("\nreq/s by connections (native wrk load):");
  const header = "target".padEnd(12) + args.connections.map((c) => String(c).padStart(14)).join("");
  console.log(header);
  for (const target of targets) {
    const row =
      target.padEnd(12) +
      args.connections
        .map((c) =>
          (results[target][c] || 0)
            .toLocaleString("en-US", { maximumFractionDigits: 0 })
            .padStart(14),
        )
        .join("");
    console.log(row);
  }
}

main().catch((error) => {
  console.error(error.message || error);
  process.exitCode = 1;
});
