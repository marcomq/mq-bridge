#!/usr/bin/env node

// Keep committed package metadata in sync with the root Cargo workspace version.
// Pass a version to bump it, or --check to verify that no copy has drifted.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const checkOnly = args.includes("--check");
const positional = args.filter((arg) => !arg.startsWith("--"));
const unknownFlags = args.filter((arg) => arg.startsWith("--") && arg !== "--check");

if (unknownFlags.length > 0 || positional.length > 1 || (checkOnly && positional.length > 0)) {
  console.error("Usage: node scripts/sync-version.mjs [VERSION | --check]");
  process.exit(2);
}

const paths = {
  rootCargo: resolve(repoRoot, "Cargo.toml"),
  rootLock: resolve(repoRoot, "Cargo.lock"),
  appCargo: resolve(repoRoot, "apps/mq-bridge-app/Cargo.toml"),
  appLock: resolve(repoRoot, "apps/mq-bridge-app/Cargo.lock"),
  server: resolve(repoRoot, "apps/mq-bridge-app/server.json"),
  tauri: resolve(repoRoot, "apps/mq-bridge-app/crates/desktop/tauri.conf.json"),
  nodePackage: resolve(repoRoot, "node/mq-bridge-node/package.json"),
  nodeLock: resolve(repoRoot, "node/mq-bridge-node/package-lock.json"),
};

const originals = new Map(
  Object.values(paths).map((path) => [path, readFileSync(path, "utf8")]),
);
const updates = new Map(originals);

function replaceOnce(path, pattern, replacement, description) {
  const current = updates.get(path);
  const matches = current.match(new RegExp(pattern.source, pattern.flags.includes("g") ? pattern.flags : `${pattern.flags}g`));
  if (matches?.length !== 1) {
    throw new Error(`Expected exactly one ${description} in ${relative(repoRoot, path)}`);
  }
  updates.set(path, current.replace(pattern, replacement));
}

function workspaceVersion(cargo) {
  return cargo.match(
    /\[workspace\.package\][\s\S]*?(?:^|\n)\s*version\s*=\s*"([^"]+)"/,
  )?.[1];
}

const currentVersion = workspaceVersion(originals.get(paths.rootCargo));
if (!currentVersion) {
  throw new Error("Could not read [workspace.package] version from Cargo.toml");
}

const version = positional[0] ?? currentVersion;
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`Invalid semantic version: ${version}`);
}

replaceOnce(
  paths.rootCargo,
  /(\[workspace\.package\][\s\S]*?(?:^|\n)\s*version\s*=\s*")[^"]+("\s*$)/m,
  `$1${version}$2`,
  "root workspace version",
);
replaceOnce(
  paths.appCargo,
  /(\[workspace\.package\][\s\S]*?(?:^|\n)\s*version\s*=\s*")[^"]+("\s*$)/m,
  `$1${version}$2`,
  "app workspace version",
);
replaceOnce(
  paths.appCargo,
  /^(mq-bridge\s*=\s*\{\s*version\s*=\s*")[^"]+(".*)$/m,
  `$1${version}$2`,
  "mq-bridge app dependency version",
);
replaceOnce(
  paths.appCargo,
  /^(mq_bridge_app\s*=\s*\{\s*version\s*=\s*")[^"]+(".*)$/m,
  `$1${version}$2`,
  "mq-bridge-app-core dependency version",
);

function syncCargoLock(path, packageNames) {
  const found = new Map(packageNames.map((name) => [name, 0]));
  const chunks = updates.get(path).split(/(?=^\[\[package\]\]$)/m);
  for (let index = 0; index < chunks.length; index += 1) {
    const name = chunks[index].match(/^name = "([^"]+)"$/m)?.[1];
    if (!found.has(name)) continue;
    found.set(name, found.get(name) + 1);
    chunks[index] = chunks[index].replace(/^version = "[^"]+"$/m, `version = "${version}"`);
  }
  const invalid = [...found].filter(([, count]) => count !== 1);
  if (invalid.length > 0) {
    throw new Error(
      `Expected one lockfile entry for ${invalid.map(([name]) => name).join(", ")} in ${relative(repoRoot, path)}`,
    );
  }
  updates.set(path, chunks.join(""));
}

syncCargoLock(paths.rootLock, [
  "mq-bridge",
  "mq-bridge-bindings-common",
  "mq-bridge-c",
  "mq-bridge-node",
  "mq-bridge-py",
]);
syncCargoLock(paths.appLock, [
  "mq-bridge",
  "mq-bridge-app",
  "mq-bridge-app-core",
  "mq-bridge-app-desktop",
]);

const serverRaw = updates.get(paths.server);
const server = JSON.parse(serverRaw);
if (typeof server.version !== "string") {
  throw new Error("Could not read the root version from server.json");
}
const patchedServer = serverRaw
  .replace(/"version": "[^"]+"/g, `"version": "${version}"`)
  .replace(/("identifier": "[^"]*):[^":]*"/g, `$1:${version}"`);
JSON.parse(patchedServer);
updates.set(paths.server, patchedServer);

const tauriRaw = updates.get(paths.tauri);
const tauri = JSON.parse(tauriRaw);
if (typeof tauri.version !== "string") {
  throw new Error("Could not read the root version from tauri.conf.json");
}
const patchedTauri = tauriRaw.replace(
  /^(\s*"version"\s*:\s*)"[^"]+"/m,
  `$1"${version}"`,
);
if (JSON.parse(patchedTauri).version !== version) {
  throw new Error("Could not update the root version in tauri.conf.json");
}
updates.set(paths.tauri, patchedTauri);

const nodePackage = JSON.parse(updates.get(paths.nodePackage));
nodePackage.version = version;
for (const name of Object.keys(nodePackage.optionalDependencies ?? {})) {
  nodePackage.optionalDependencies[name] = version;
}
updates.set(paths.nodePackage, `${JSON.stringify(nodePackage, null, 2)}\n`);

const nodeLock = JSON.parse(updates.get(paths.nodeLock));
nodeLock.version = version;
const lockRoot = nodeLock.packages?.[""];
if (!lockRoot) {
  throw new Error("Could not find the root package in the Node package lock");
}
lockRoot.version = version;
for (const name of Object.keys(lockRoot.optionalDependencies ?? {})) {
  lockRoot.optionalDependencies[name] = version;
  const entry = nodeLock.packages?.[`node_modules/${name}`];
  if (entry) {
    entry.version = version;
    entry.resolved = `https://registry.npmjs.org/${name}/-/${name}-${version}.tgz`;
    delete entry.integrity;
  }
}
updates.set(paths.nodeLock, `${JSON.stringify(nodeLock, null, 2)}\n`);

const changed = [...updates].filter(([path, contents]) => originals.get(path) !== contents);

if (checkOnly) {
  if (changed.length === 0) {
    console.log(`Version ${version} is in sync.`);
    process.exit(0);
  }
  console.error(
    `Out of sync with version ${version}:\n${changed
      .map(([path]) => `  ${relative(repoRoot, path)}`)
      .join("\n")}\nRun: node scripts/sync-version.mjs`,
  );
  process.exit(1);
}

for (const [path, contents] of changed) writeFileSync(path, contents);

if (changed.length === 0) {
  console.log(`Version ${version} is in sync.`);
} else {
  console.log(
    `Synced version ${version}:\n${changed
      .map(([path]) => `  ${relative(repoRoot, path)}`)
      .join("\n")}`,
  );
}
