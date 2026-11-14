#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const checkOnly = process.argv.includes("--check");
const appRoot = resolve(fileURLToPath(new URL("../..", import.meta.url)));
const outputPath = resolve(appRoot, "ui/src/lib/generated/ui-types.ts");

const rawSchemas = execFileSync(
  "cargo",
  ["run", "-p", "mq-bridge-app-core", "--bin", "export-ui-type-schemas", "--quiet"],
  // The app is its own cargo workspace and the repo root excludes `apps/`, so
  // running this from the root cannot see `mq-bridge-app-core` at all.
  { cwd: appRoot, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
);

const schemas = JSON.parse(rawSchemas);

const definitions = new Map();
for (const schema of Object.values(schemas)) {
  for (const [name, definition] of Object.entries(schema.$defs || {})) {
    definitions.set(name, definition);
  }
}

function refName(ref) {
  const prefix = "#/$defs/";
  return typeof ref === "string" && ref.startsWith(prefix)
    ? ref.slice(prefix.length)
    : null;
}

function typeFromSchema(schema) {
  if (schema === true) return "unknown";
  if (!schema || schema === false) return "never";
  if (Object.prototype.hasOwnProperty.call(schema, "const")) return JSON.stringify(schema.const);
  const referencedName = refName(schema.$ref);
  if (referencedName) return referencedName;

  if (Array.isArray(schema.anyOf) || Array.isArray(schema.oneOf)) {
    return (schema.anyOf || schema.oneOf)
      .map((entry) => typeFromSchema(entry))
      .filter((entry, index, list) => list.indexOf(entry) === index)
      .join(" | ");
  }

  if (Array.isArray(schema.enum)) {
    return schema.enum.map((entry) => JSON.stringify(entry)).join(" | ");
  }

  const type = schema?.type;
  if (Array.isArray(type)) {
    return type
      .map((entry) => entry === "null" ? "null" : typeFromSchema({ ...schema, type: entry }))
      .filter((entry, index, list) => list.indexOf(entry) === index)
      .join(" | ");
  }

  if (type === "string") return "string";
  if (type === "boolean") return "boolean";
  if (type === "null") return "null";
  if (type === "number" || type === "integer") return "number";
  if (type === "array") return `${typeFromSchema(schema.items || {})}[]`;
  if (type === "object") {
    if (schema.additionalProperties !== undefined) {
      return `Record<string, ${typeFromSchema(schema.additionalProperties)}>`;
    }
    if (schema.properties) {
      return objectTypeFromSchema(schema);
    }
    return "Record<string, unknown>";
  }

  return "unknown";
}

function tsPropName(name) {
  return /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name) ? name : JSON.stringify(name);
}

function objectTypeFromSchema(schema) {
  const required = new Set(schema.required || []);
  const properties = Object.entries(schema.properties || {}).map(([propertyName, propertySchema]) => {
    const optional = required.has(propertyName) ? "" : "?";
    return `${tsPropName(propertyName)}${optional}: ${typeFromSchema(propertySchema)}`;
  });
  return `{ ${properties.join("; ")} }`;
}

function interfaceFromSchema(name, schema) {
  if (schema.enum || schema.type !== "object" || !schema.properties) {
    return `export type ${name} = ${typeFromSchema(schema)};`;
  }

  const required = new Set(schema.required || []);
  const lines = [`export interface ${name} {`];
  for (const [propertyName, propertySchema] of Object.entries(schema.properties || {})) {
    const optional = required.has(propertyName) ? "" : "?";
    lines.push(
      `  ${tsPropName(propertyName)}${optional}: ${typeFromSchema(propertySchema)};`,
    );
  }
  lines.push("}");
  return lines.join("\n");
}

const output = [
  "/* This file is generated from Rust schemars schemas. Do not edit by hand. */",
  "",
  ...[
    ...Object.entries(schemas),
    ...[...definitions.entries()].filter(([name]) => !(name in schemas)),
  ].map(([name, schema]) => interfaceFromSchema(name, schema)),
  "",
].join("\n\n");

if (checkOnly) {
  const existing = readFileSync(outputPath, "utf8");
  if (existing !== output) {
    console.error(`${outputPath} is out of date. Run npm run generate:ui-types.`);
    process.exit(1);
  }
} else {
  writeFileSync(outputPath, output);
}
