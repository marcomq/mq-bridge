import { describe, expect, test } from "vitest";
import { dropAliasBranches, forceRefOnlyEndpoints, hideUnusedDeprecatedProperties, nameMappingRuleBranches } from "../../ui/src/lib/schema-utils";

describe("schema-utils", () => {
  test("forces nested endpoint schemas to ref-only configs", () => {
    const schema: any = {
      $defs: {
        DlqConfig: { properties: { endpoint: { oneOf: [{ type: "string" }] } } },
        FanoutConfig: { items: { anyOf: [{ type: "string" }] } },
        SwitchConfig: {
          properties: {
            default: { properties: { http: {} } },
            cases: { additionalProperties: { enum: ["http"] } },
          },
        },
      },
    };

    forceRefOnlyEndpoints(schema);

    expect(schema.$defs.RefConfig.properties.ref.type).toBe("string");
    expect(schema.$defs.DlqConfig.properties.endpoint).toMatchObject({
      $ref: "#/$defs/RefConfig",
      type: "object",
    });
    expect(schema.$defs.FanoutConfig.items).toMatchObject({
      $ref: "#/$defs/RefConfig",
      type: "object",
    });
    expect(schema.$defs.SwitchConfig.properties.default).toMatchObject({
      $ref: "#/$defs/RefConfig",
      type: "object",
    });
    expect(schema.$defs.SwitchConfig.properties.cases.additionalProperties).toMatchObject({
      $ref: "#/$defs/RefConfig",
      type: "object",
    });
    expect(schema.$defs.DlqConfig.properties.endpoint).not.toHaveProperty("oneOf");
    expect(schema.$defs.FanoutConfig.items).not.toHaveProperty("anyOf");
    expect(schema.$defs.SwitchConfig.properties.default).not.toHaveProperty("properties");
  });

  test("is safe when defs are missing or incomplete and stays idempotent", () => {
    const emptySchema = {};
    expect(() => forceRefOnlyEndpoints(emptySchema as never)).not.toThrow();
    expect(emptySchema).toEqual({
      $defs: {
        RefConfig: {
          type: "object",
          title: "",
          properties: { ref: { type: "string" } },
          required: ["ref"],
          "wa-no-label": true,
        },
        StaticConfig: {
          type: "object",
          properties: { static: { type: "string" } },
          required: ["static"],
        },
      },
    });

    const schema: any = {
      $defs: {
        Existing: { type: "string" },
        DlqConfig: { properties: { endpoint: { oneOf: [{ type: "string" }], properties: { http: {} } } } },
        SwitchConfig: { properties: { default: { anyOf: [{ type: "string" }] } } },
      },
    };

    forceRefOnlyEndpoints(schema);
    const once = JSON.parse(JSON.stringify(schema));
    forceRefOnlyEndpoints(schema);

    expect(schema).toEqual(once);
    expect(schema.$defs.RefConfig.properties.ref.type).toBe("string");
    expect(schema.$defs.Existing).toEqual({ type: "string" });
    expect(schema.$defs.DlqConfig.properties.endpoint).toEqual({
      $ref: "#/$defs/RefConfig",
      type: "object",
    });
    expect(schema.$defs.SwitchConfig.properties.default).toEqual({
      $ref: "#/$defs/RefConfig",
      type: "object",
    });
  });

  test("names the two mapping rule branches", () => {
    const schema: any = {
      $defs: {
        MappingRule: {
          anyOf: [{ type: "string" }, { $ref: "#/$defs/DetailedMappingRule" }],
        },
      },
    };

    nameMappingRuleBranches(schema);

    expect(schema.$defs.MappingRule.anyOf[0].title).toBe("Source path");
    expect(schema.$defs.MappingRule.anyOf[1].title).toBe("Path with default");
    // A non-standard `type` here would fail the form library's AJV validation of the schema.
    expect(schema.$defs.MappingRule.anyOf[0].type).toBe("string");
  });

  test("keeps existing branch titles and tolerates a missing mapping rule", () => {
    const emptySchema: any = {};
    expect(() => nameMappingRuleBranches(emptySchema)).not.toThrow();
    expect(emptySchema).toEqual({});

    const schema: any = {
      $defs: {
        MappingRule: { anyOf: [{ type: "string", title: "Kept" }, { type: "number" }] },
      },
    };

    nameMappingRuleBranches(schema);
    const once = JSON.parse(JSON.stringify(schema));
    nameMappingRuleBranches(schema);

    expect(schema).toEqual(once);
    expect(schema.$defs.MappingRule.anyOf[0].title).toBe("Kept");
    expect(schema.$defs.MappingRule.anyOf[1]).not.toHaveProperty("title");
  });
  test("hides a deprecated field the config does not set", () => {
    const schema: any = {
      $defs: {
        ObjectStoreConfig: {
          properties: {
            name_by: { description: "(Sink only) `auto`, `write_time` or `source_position`." },
            idempotency: { description: "Deprecated: use `name_by`. true = `source_position`." },
          },
        },
      },
    };

    hideUnusedDeprecatedProperties(schema, { output: { object_store: { url: "s3://bucket/out" } } });

    expect(schema.$defs.ObjectStoreConfig.properties.idempotency.hidden).toBe(true);
    expect(schema.$defs.ObjectStoreConfig.properties.name_by).not.toHaveProperty("hidden");
  });

  test("keeps a deprecated field a legacy config still carries", () => {
    const schema: any = {
      $defs: {
        ObjectStoreConfig: {
          properties: {
            idempotency: { description: "Deprecated: use `name_by`. true = `source_position`." },
          },
        },
      },
    };

    hideUnusedDeprecatedProperties(schema, {
      output: { object_store: { url: "s3://bucket/out", idempotency: true } },
    });

    expect(schema.$defs.ObjectStoreConfig.properties.idempotency).not.toHaveProperty("hidden");
  });

  test("keeps a deprecated field scoped to the endpoint that carries it", () => {
    const deprecated = { description: "Deprecated: use `name_by`. true = `source_position`." };
    const schema: any = {
      $defs: {
        Endpoint: {
          oneOf: [
            { properties: { file: { $ref: "#/$defs/FileConfig" } } },
            { properties: { object_store: { $ref: "#/$defs/ObjectStoreConfig" } } },
          ],
        },
        FileConfig: { properties: { idempotency: { ...deprecated } } },
        ObjectStoreConfig: { properties: { idempotency: { ...deprecated } } },
      },
    };

    hideUnusedDeprecatedProperties(schema, {
      input: { file: { path: "in.jsonl", idempotency: true } },
      output: { object_store: { url: "s3://bucket/out" } },
    });

    expect(schema.$defs.FileConfig.properties.idempotency).not.toHaveProperty("hidden");
    expect(schema.$defs.ObjectStoreConfig.properties.idempotency.hidden).toBe(true);
  });

  test("tolerates an empty schema and a null config", () => {
    const emptySchema: any = {};
    expect(() => hideUnusedDeprecatedProperties(emptySchema, null)).not.toThrow();
    expect(emptySchema).toEqual({});
  });

  test("drops alias branches so a format dropdown lists each value once", () => {
    const schema: any = {
      $defs: {
        FileFormat: {
          oneOf: [
            { type: "string", const: "normal" },
            { type: "string", const: "envelope", "x-alias-of": "normal" },
          ],
        },
      },
    };
    dropAliasBranches(schema);
    expect(schema.$defs.FileFormat.oneOf).toEqual([{ type: "string", const: "normal" }]);
  });
});
