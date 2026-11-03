#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

function option(name, fallback) {
  const index = process.argv.indexOf(name);
  return index === -1 ? fallback : process.argv[index + 1];
}

const root = path.resolve(option("--root", process.cwd()));
const packageDirectory = path.resolve(root, option("--package", "node"));
const manifestPath = path.join(packageDirectory, "mq-bridge-plugin.json");
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
if (typeof manifest.library !== "string") {
  throw new Error(`${manifestPath} must contain a string field 'library'`);
}

const suffix = process.platform === "linux" ? "-gnu" : process.platform === "win32" ? "-msvc" : "";
const target = `${process.platform}-${process.arch}${suffix}`;
const fileName = process.platform === "win32"
  ? `${manifest.library}.dll`
  : process.platform === "darwin"
    ? `lib${manifest.library}.dylib`
    : `lib${manifest.library}.so`;

execFileSync("cargo", ["build", "--release"], { cwd: root, stdio: "inherit" });
const library = path.join(root, "target", "release", fileName);
if (!fs.existsSync(library)) {
  throw new Error(`cargo produced no plugin library at ${library}`);
}

const targetDirectory = path.join(packageDirectory, "prebuilds", target);
fs.mkdirSync(targetDirectory, { recursive: true });
fs.copyFileSync(library, path.join(targetDirectory, fileName));
console.log(`staged ${fileName} for ${target}`);

if (process.argv.includes("--pack")) {
  const output = path.resolve(root, option("--out", "npm"));
  fs.mkdirSync(output, { recursive: true });
  const npm = process.platform === "win32" ? "npm.cmd" : "npm";
  execFileSync(npm, ["pack", packageDirectory, "--pack-destination", output], {
    stdio: "inherit",
  });
}
