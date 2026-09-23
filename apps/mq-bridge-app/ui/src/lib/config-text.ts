import { parse as parseYaml, stringify as stringifyYaml } from "yaml";

export type ConfigTextFormat = "json" | "yaml";

export function formatConfigText(value: unknown, format: ConfigTextFormat): string {
  return format === "yaml" ? stringifyYaml(value) : JSON.stringify(value, null, 2);
}

// Parses edited config text; only a top-level mapping is accepted.
export function parseConfigText(text: string, format: ConfigTextFormat): Record<string, unknown> {
  const value: unknown = format === "yaml" ? parseYaml(text) : JSON.parse(text);
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("The configuration must be an object.");
  }
  return value as Record<string, unknown>;
}
