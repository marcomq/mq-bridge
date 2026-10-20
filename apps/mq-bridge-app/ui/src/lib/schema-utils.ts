type MutableSchema = {
  $defs?: Record<string, MutableSchema>;
  $ref?: string;
  type?: string;
  title?: string;
  properties?: Record<string, MutableSchema>;
  items?: MutableSchema;
  additionalProperties?: MutableSchema;
  required?: string[];
  oneOf?: unknown;
  anyOf?: unknown;
  allOf?: unknown;
  enum?: unknown;
  const?: unknown;
  default?: unknown;
  description?: string;
  hidden?: boolean;
  ["wa-no-label"]?: boolean;
};

type RootSchema = {
  properties?: Record<string, MutableSchema>;
  $defs?: Record<string, MutableSchema>;
};

function cloneSchema<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

// Untitled branches leave the mapping rule picker labelled by derived type names; say what the
// two forms of a rule actually are. Transform's free-form `schema` gets its editor from a custom
// renderer instead: it has no `type` here, and a non-standard one would fail the form library's
// AJV validation of the whole schema.
export function nameMappingRuleBranches(itemSchema: MutableSchema): void {
  const branches = itemSchema.$defs?.MappingRule?.anyOf;
  if (!Array.isArray(branches)) return;

  for (const branch of branches as MutableSchema[]) {
    if (!branch || typeof branch !== "object" || branch.title) continue;
    if (branch.type === "string") branch.title = "Source path";
    else if (branch.$ref?.endsWith("/DetailedMappingRule")) branch.title = "Path with default";
  }
}

// The schema lists serde aliases (`envelope` for `normal`) as extra branches so editors accept
// them; the form shows only the canonical name.
export function dropAliasBranches(itemSchema: MutableSchema): void {
  for (const def of Object.values(itemSchema.$defs || {})) {
    if (!Array.isArray(def?.oneOf)) continue;
    def.oneOf = def.oneOf.filter((branch) => !(branch && typeof branch === "object" && "x-alias-of" in branch));
  }
}

// Schemars has no `deprecated` keyword, so the backend marks a superseded field by opening its
// doc comment with "Deprecated:". Such a field is dropped from the form unless the config still
// carries it — a legacy value stays visible and clearable, a new config only sees the replacement.
//
// Presence is matched against the property that owns the def, not against the bare field name:
// `file` and `object_store` both declare a deprecated `idempotency`, and a legacy value on one
// must not keep the other's visible. A def nothing refs by name falls back to the name alone.
export function hideUnusedDeprecatedProperties(itemSchema: MutableSchema, data: unknown): void {
  const owners = new Map<string, Set<string>>();
  const collectOwners = (node: unknown) => {
    if (Array.isArray(node)) return node.forEach(collectOwners);
    if (!node || typeof node !== "object") return;
    for (const [key, child] of Object.entries((node as MutableSchema).properties || {})) {
      const def = child?.$ref?.startsWith("#/$defs/") ? child.$ref.slice("#/$defs/".length) : null;
      if (!def) continue;
      if (!owners.has(def)) owners.set(def, new Set());
      owners.get(def)!.add(key);
    }
    Object.values(node as Record<string, unknown>).forEach(collectOwners);
  };
  collectOwners(itemSchema);

  const present = new Set<string>();
  const collect = (value: unknown, owner: string) => {
    if (Array.isArray(value)) return value.forEach((item) => collect(item, owner));
    if (!value || typeof value !== "object") return;
    for (const [key, child] of Object.entries(value)) {
      present.add(`${owner}.${key}`);
      collect(child, key);
    }
  };
  collect(data, "");

  for (const [defName, def] of Object.entries(itemSchema.$defs || {})) {
    for (const [name, property] of Object.entries(def?.properties || {})) {
      if (!property?.description?.startsWith("Deprecated:")) continue;
      const ownerKeys = owners.get(defName);
      const used = ownerKeys
        ? [...ownerKeys].some((owner) => present.has(`${owner}.${name}`))
        : [...present].some((entry) => entry.endsWith(`.${name}`));
      if (!used) property.hidden = true;
    }
  }
}

export function resolveRootArrayItemSchema(rootSchema: RootSchema, propertyName: string): MutableSchema {
  const itemSchema = cloneSchema(rootSchema.properties?.[propertyName]?.items || {});
  const rootDefs = cloneSchema(rootSchema.$defs || {});
  if (itemSchema.$ref && itemSchema.$ref.startsWith("#/$defs/")) {
    const defName = itemSchema.$ref.slice("#/$defs/".length);
    const resolved = rootDefs[defName] ? cloneSchema(rootDefs[defName]) : null;
    return {
      ...(resolved || itemSchema),
      $defs: {
        ...rootDefs,
        ...(itemSchema.$defs || {}),
        ...(resolved?.$defs || {}),
      },
    };
  }
  return {
    ...itemSchema,
    $defs: {
      ...rootDefs,
      ...(itemSchema.$defs || {}),
    },
  };
}

function ensureStaticConfigDef(itemSchema: MutableSchema) {
  // Keep this shared def available for downstream schema consumers that still expect it.
  if (!itemSchema.$defs?.StaticConfig) {
    itemSchema.$defs ||= {};
    itemSchema.$defs.StaticConfig = {
      type: "object",
      properties: { static: { type: "string" } },
      required: ["static"],
    };
  }
}

export function forceRefOnlyEndpoints(itemSchema: MutableSchema): void {
  if (!itemSchema.$defs) itemSchema.$defs = {};
  if (!itemSchema.$defs.RefConfig) {
    itemSchema.$defs.RefConfig = {
      type: "object",
      title: "",
      properties: { ref: { type: "string" } },
      required: ["ref"],
      "wa-no-label": true,
    };
  }
  ensureStaticConfigDef(itemSchema);

  const forceRef = (obj: MutableSchema | undefined) => {
    if (!obj || typeof obj !== "object") return;
    obj.$ref = "#/$defs/RefConfig";
    delete obj.oneOf;
    delete obj.anyOf;
    delete obj.allOf;
    delete obj.properties;
    delete obj.enum;
    delete obj.const;
    delete obj.default;
    obj.type = "object";
  };

  const dlq = itemSchema.$defs.DlqConfig;
  if (dlq?.properties?.endpoint) {
    forceRef(dlq.properties.endpoint);
  }

  const fanout = itemSchema.$defs.FanoutConfig || itemSchema.$defs.FanOutConfig;
  if (fanout) {
    const endpoints = fanout.items || fanout.properties?.endpoints?.items;
    if (endpoints) {
      forceRef(endpoints);
    }
  }

  const sw = itemSchema.$defs.SwitchConfig;
  if (sw?.properties) {
    if (sw.properties.default) {
      forceRef(sw.properties.default);
    }
    if (sw.properties.cases?.additionalProperties) {
      forceRef(sw.properties.cases.additionalProperties);
    }
  }
}
