import { describe, expect, test } from "vitest";
import { formatConfigText, parseConfigText } from "../../ui/src/lib/config-text";

const sample = {
  name: "orders",
  endpoint: { kafka: { url: "localhost:9092", topic: "orders" } },
  middlewares: [{ retry: { max_attempts: 3 } }],
};

describe("config text", () => {
  test.each(["json", "yaml"] as const)("round-trips %s", (format) => {
    expect(parseConfigText(formatConfigText(sample, format), format)).toEqual(sample);
  });

  test("converts edited yaml to the same object as json", () => {
    const yaml = "name: orders\nendpoint:\n  memory:\n    topic: in\n";
    expect(parseConfigText(yaml, "yaml")).toEqual({ name: "orders", endpoint: { memory: { topic: "in" } } });
  });

  test.each([
    ["json", "[1, 2]"],
    ["json", "null"],
    ["yaml", "- a\n- b\n"],
    ["yaml", "plain"],
  ] as const)("rejects a non-object %s document", (format, text) => {
    expect(() => parseConfigText(text, format)).toThrow("must be an object");
  });

  test("surfaces syntax errors", () => {
    expect(() => parseConfigText("{ \"name\": ", "json")).toThrow();
    expect(() => parseConfigText("a: [1, 2", "yaml")).toThrow();
  });
});
