import { saveWholeConfig } from "./config-api";
import { appShell, getAppState } from "./app-shell";
import {
  ensureWorkspaceCollections,
  isSensitiveConfig,
  sanitizeEnvVars,
  sanitizePublisherHistory,
  sanitizePresets,
  type EnvVars,
  type HeaderRow,
  type PublisherHistoryStore,
  type PresetsByPublisher,
  type PublisherPreset,
} from "./workspace-config";
import { removeStoredJson, setStoredJson } from "./encrypted-json-storage";
import { hasEncryptedMessages, resolveStorageSecurity } from "./storage-security";

export type ImportedRequest = PublisherPreset;

// One selectable view in the config preview dialog; `editable` views can be applied back.
export type ConfigJsonVariant = { id: string; label: string; value: unknown; editable?: boolean };

type ExportBundle = {
  type: "mqb-export";
  version: 1;
  exportedAt: string;
  config: Record<string, unknown>;
  envVars: EnvVars;
};

const PUBLISHER_HISTORY_KEY = "mqb_publisher_history";

function getWorkspaceConfig() {
  return ensureWorkspaceCollections(appShell.config<Record<string, unknown>>());
}

function triggerJsonDownload(filename: string, value: unknown) {
  const blob = new Blob([JSON.stringify(value, null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  URL.revokeObjectURL(url);
}

function isoDateCompact() {
  return new Date().toISOString().replace(/[:.]/g, "-");
}

export function exportFullBundle() {
  const config = structuredClone(getWorkspaceConfig());
  const bundle: ExportBundle = {
    type: "mqb-export",
    version: 1,
    exportedAt: new Date().toISOString(),
    config,
    envVars: sanitizeEnvVars(config.env_vars),
  };
  triggerJsonDownload(`mqb-export-${isoDateCompact()}.json`, bundle);
}

export function exportConfigOnly() {
  const payload = {
    type: "mqb-config",
    version: 1,
    exportedAt: new Date().toISOString(),
    config: structuredClone(appShell.config()),
  };
  triggerJsonDownload(`mqb-config-${isoDateCompact()}.json`, payload);
}

function buildSingleConfig(
  section: "consumers" | "publishers",
  entry: Record<string, unknown>,
  extraConfig: Record<string, unknown> = {},
) {
  return {
    type: "mqb-config",
    version: 1,
    exportedAt: new Date().toISOString(),
    config: {
      ...extraConfig,
      [section]: [structuredClone(entry)],
    },
  };
}

export function buildConsumerConfigExport(consumer: Record<string, unknown>) {
  const config = appShell.config<Record<string, unknown>>();
  return buildSingleConfig("consumers", consumer, {
    env_vars: sanitizeEnvVars(config.env_vars),
  });
}

export function buildPublisherConfigExport(publisher: Record<string, unknown>) {
  const config = appShell.config<Record<string, unknown>>();
  return buildSingleConfig("publishers", publisher, {
    env_vars: sanitizeEnvVars(config.env_vars),
  });
}

// Route-level option fields shared between an mq-bridge `ConsumerConfig` and a
// native `RouteConfig`. App-only fields (id, name, comment, message_capture,
// response, output) are intentionally excluded.
const ROUTE_OPTION_FIELDS = [
  "enabled",
  "description",
  "concurrency",
  "batch_size",
  "commit_concurrency_limit",
  "startup_timeout_ms",
  "reconnect_interval_ms",
  "empty_batch_delay_ms",
  "allow_fault_injection",
] as const;

function resolveConsumerOutputEndpoint(
  output: unknown,
  publishers: Array<Record<string, unknown>>,
): Record<string, unknown> | null {
  const cfg = asObject(output);
  const mode = String(cfg.mode || "none");
  if (mode === "response") {
    // Mirrors the backend: a response output becomes a `static` endpoint that
    // emits the configured payload with metadata as response headers.
    const response = asObject(cfg.response);
    return {
      static: {
        body: typeof response.payload === "string" ? response.payload : "",
        raw: true,
        metadata: asObject(response.headers),
      },
    };
  }
  if (mode === "publisher") {
    const publisherId = cfg.publisher_id ? String(cfg.publisher_id) : "";
    const publisherName = cfg.publisher ? String(cfg.publisher) : "";
    const match = publishers.find(
      (entry) =>
        (publisherId && String(entry.id || "") === publisherId) ||
        (publisherName && String(entry.name || "") === publisherName),
    );
    const endpoint = match ? asObject(match.endpoint) : {};
    return Object.keys(endpoint).length > 0 ? structuredClone(endpoint) : null;
  }
  return null;
}

// A bare mq-bridge `Endpoint` dict, usable directly with
// `Publisher.from_config(endpoint, name)` in mq-bridge-py.
export function buildPublisherEndpointDoc(publisher: Record<string, unknown>) {
  return structuredClone(asObject(publisher.endpoint));
}

// A single mq-bridge `Route` dict (`input`/`output` + options), usable directly
// with `Route.from_config(route, name)` in mq-bridge-py.
export function buildConsumerRouteDoc(
  consumer: Record<string, unknown>,
  publishers: Array<Record<string, unknown>> = [],
) {
  const route: Record<string, unknown> = {
    input: structuredClone(asObject(consumer.endpoint)),
  };
  const output = resolveConsumerOutputEndpoint(consumer.output, publishers);
  if (output) {
    route.output = output;
  }
  for (const field of ROUTE_OPTION_FIELDS) {
    const value = consumer[field];
    if (value !== undefined && value !== null) {
      route[field] = structuredClone(value);
    }
  }
  return route;
}

// `from_config` mappings. `from_config(mapping, name)` selects a named entry
// from a config document, so the mappings are wrapped under `publishers`/`routes`
// keyed by the entry name — matching mq-bridge-py's ConfigDocument.

// Usable with `Publisher.from_config(doc, name)` in mq-bridge-py.
export function buildPublisherConfigDocument(publisher: Record<string, unknown>) {
  const name = String(publisher.name || "publisher");
  return { publishers: { [name]: buildPublisherEndpointDoc(publisher) } };
}

// A consumer's source endpoint as a publisher document, usable with
// `Publisher.from_config(doc, name)` (the endpoint-centric entry point).
export function buildConsumerPublisherDocument(consumer: Record<string, unknown>) {
  const name = String(consumer.name || "consumer");
  return { publishers: { [name]: buildPublisherEndpointDoc(consumer) } };
}

// A publisher's endpoint as a route's `output`, fed by a memory `input` channel,
// usable with `Route.from_config(doc, name)`. Send into the memory topic and the
// route forwards to the publisher endpoint.
export function buildPublisherConfigRouteDocument(publisher: Record<string, unknown>) {
  const name = String(publisher.name || "publisher");
  return {
    routes: {
      [name]: {
        input: { memory: { topic: `${name}.in` } },
        output: buildPublisherEndpointDoc(publisher),
      },
    },
  };
}

// Usable with `Route.from_config(doc, name)` in mq-bridge-py.
export function buildConsumerConfigDocument(
  consumer: Record<string, unknown>,
  publishers: Array<Record<string, unknown>> = [],
) {
  const name = String(consumer.name || "consumer");
  return { routes: { [name]: buildConsumerRouteDoc(consumer, publishers) } };
}

function nextUniqueName(base: string, existing: Set<string>) {
  let index = 1;
  let candidate = base;
  while (existing.has(candidate)) {
    candidate = `${base}_${index++}`;
  }
  existing.add(candidate);
  return candidate;
}

function asObject(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function normalizeNamedArray<T extends { name?: unknown }>(value: unknown): Array<T & { name: string }> {
  if (!Array.isArray(value)) return [];
  return value
    .filter((row) => row && typeof row === "object")
    .map((row) => {
      const obj = row as Record<string, unknown>;
      return { ...(obj as T), name: String(obj.name || "") };
    })
    .filter((row) => row.name.trim().length > 0);
}

function mergeNamedEntries(
  currentRows: Array<Record<string, unknown>>,
  incomingRows: Array<Record<string, unknown>>,
) {
  const existingNames = new Set(currentRows.map((row) => String(row?.name || "")));
  const merged = [...currentRows];
  for (const raw of incomingRows) {
    const row = asObject(raw);
    const baseName = String(row.name || "imported");
    const name = existingNames.has(baseName)
      ? nextUniqueName(baseName, existingNames)
      : (existingNames.add(baseName), baseName);
    merged.push({ ...row, name });
  }
  return merged;
}

export async function importAppConfigFromJsonText(text: string) {
  const parsed = JSON.parse(text) as unknown;
  const currentConfig = asObject(structuredClone(getWorkspaceConfig()));
  const currentPublishers = normalizeNamedArray<Record<string, unknown>>(currentConfig.publishers);
  const currentConsumers = normalizeNamedArray<Record<string, unknown>>(currentConfig.consumers);
  const currentPresets = sanitizePresets(currentConfig.presets);
  const currentEnvVars = sanitizeEnvVars(currentConfig.env_vars);
  const currentHistory = sanitizePublisherHistory(currentConfig.history);

  const obj = asObject(parsed);
  const type = String(obj.type || "");
  const incomingConfig =
    type === "mqb-export" || type === "mqb-config"
      ? asObject(obj.config)
      : asObject(obj);

  const mergedPublishers = mergeNamedEntries(
    currentPublishers as Array<Record<string, unknown>>,
    normalizeNamedArray<Record<string, unknown>>(incomingConfig.publishers) as Array<Record<string, unknown>>,
  );
  const mergedConsumers = mergeNamedEntries(
    currentConsumers as Array<Record<string, unknown>>,
    normalizeNamedArray<Record<string, unknown>>(incomingConfig.consumers) as Array<Record<string, unknown>>,
  );

  const nextConfig: Record<string, unknown> = {
    ...currentConfig,
    ...incomingConfig,
    publishers: mergedPublishers,
    consumers: mergedConsumers,
    presets: currentPresets,
    env_vars: currentEnvVars,
    history: currentHistory,
  };

  const importedPresets = {
    ...sanitizePresets(incomingConfig.presets),
  };
  for (const [publisherName, rows] of Object.entries(sanitizePresets(obj.presets))) {
    importedPresets[publisherName] = [
      ...(importedPresets[publisherName] || []),
      ...rows,
    ];
  }
  let mergedPresets = sanitizePresets(nextConfig.presets);
  for (const [publisherName, rows] of Object.entries(importedPresets)) {
    mergedPresets = mergePresets(mergedPresets, publisherName, rows);
  }
  nextConfig.presets = mergedPresets;
  nextConfig.env_vars = {
    ...sanitizeEnvVars(nextConfig.env_vars),
    ...sanitizeEnvVars(incomingConfig.env_vars),
    ...sanitizeEnvVars(obj.envVars),
  };
  nextConfig.history = sanitizePublisherHistory(nextConfig.history);

  await saveImportedConfig(nextConfig);
  return {
    importedPublishers: normalizeNamedArray<Record<string, unknown>>(incomingConfig.publishers).length,
    importedConsumers: normalizeNamedArray<Record<string, unknown>>(incomingConfig.consumers).length,
    importedRoutes: Object.keys(asObject(incomingConfig.routes)).length,
  };
}

export async function resetAppConfigToDefaults() {
  const currentConfig = asObject(structuredClone(appShell.config<Record<string, unknown>>()));
  const normalizedCurrentConfig = ensureWorkspaceCollections(currentConfig);
  const nextConfig: Record<string, unknown> = {
    ...currentConfig,
    consumers: [],
    publishers: [],
    default_tab: "publishers",
    config_security: normalizedCurrentConfig.config_security,
    history: sanitizePublisherHistory({}),
  };
  delete nextConfig.routes;
  await saveImportedConfig(nextConfig);
}

export function exportPresetsOnly() {
  const config = getWorkspaceConfig();
  const payload = {
    type: "mqb-presets",
    version: 1,
    exportedAt: new Date().toISOString(),
    presets: sanitizePresets(config.presets),
    envVars: sanitizeEnvVars(config.env_vars),
  };
  triggerJsonDownload(`mqb-presets-${isoDateCompact()}.json`, payload);
}

export function exportPresetsForPublisher(publisherName: string) {
  const config = getWorkspaceConfig();
  const allPresets = sanitizePresets(config.presets);
  const payload = {
    type: "mqb-presets",
    version: 1,
    exportedAt: new Date().toISOString(),
    presets: {
      [publisherName]: Array.isArray(allPresets[publisherName]) ? allPresets[publisherName] : [],
    },
    envVars: sanitizeEnvVars(config.env_vars),
  };
  triggerJsonDownload(`mqb-presets-${publisherName}-${isoDateCompact()}.json`, payload);
}

function flattenPostmanItems(
  items: unknown[],
  out: Array<Record<string, unknown>>,
  folderPath: string[] = [],
) {
  for (const item of items) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    if (row.request && typeof row.request === "object") {
      out.push({
        ...row,
        __folderPath: folderPath.slice(),
      });
    }
    if (Array.isArray(row.item)) {
      const nextPath = row.name ? [...folderPath, String(row.name)] : folderPath;
      flattenPostmanItems(row.item, out, nextPath);
    }
  }
}

function parsePostmanCollection(raw: unknown): PublisherPreset[] {
  if (!raw || typeof raw !== "object") return [];
  const obj = raw as Record<string, unknown>;
  const info = obj.info as Record<string, unknown> | undefined;
  const schema = String(info?.schema ?? "");
  if (!schema.includes("postman.com")) return [];
  const list = Array.isArray(obj.item) ? obj.item : [];
  const entries: Array<Record<string, unknown>> = [];
  flattenPostmanItems(list, entries);
  return entries.map((entry, index) => {
    const request = (entry.request || {}) as Record<string, unknown>;
    const rawUrl = request.url;
    const method = String(request.method || "GET").toUpperCase();
    let url = "";
    if (typeof rawUrl === "string") {
      url = rawUrl;
    } else if (rawUrl && typeof rawUrl === "object") {
      const urlObj = rawUrl as Record<string, unknown>;
      url = String(urlObj.raw || "");
    }

    const headersValue = Array.isArray(request.header) ? request.header : [];
    const headers: HeaderRow[] = headersValue.map((headerRow) => {
      const header = headerRow && typeof headerRow === "object"
        ? (headerRow as Record<string, unknown>)
        : {};
      return {
        key: String(header.key || ""),
        value: String(header.value || ""),
        enabled: header.disabled !== true,
      };
    });

    const body = request.body && typeof request.body === "object"
      ? (request.body as Record<string, unknown>)
      : {};
    const payload = String(body.raw || "");

    const folderPath = Array.isArray(entry.__folderPath) ? entry.__folderPath : [];
    const group = folderPath.length > 0 ? folderPath.join(" / ") : "";

    return {
      name: String(entry.name || `Postman request ${index + 1}`),
      payload,
      headers,
      group,
      endpoint_type: "http",
      method,
      url,
      request_fields: { url },
    };
  });
}

function parseOpenApiDocument(raw: unknown): { presets: PublisherPreset[]; envVars: EnvVars } {
  if (!raw || typeof raw !== "object") return { presets: [], envVars: {} };
  const obj = raw as Record<string, unknown>;
  if (!obj.openapi) return { presets: [], envVars: {} };
  const paths = obj.paths && typeof obj.paths === "object" ? (obj.paths as Record<string, unknown>) : {};
  const methods = ["get", "post", "put", "delete", "patch", "head", "options"];
  const presets: PublisherPreset[] = [];

  for (const [path, operations] of Object.entries(paths)) {
    if (!operations || typeof operations !== "object") continue;
    const operationMap = operations as Record<string, unknown>;
    for (const method of methods) {
      const operation = operationMap[method];
      if (!operation || typeof operation !== "object") continue;
      const op = operation as Record<string, unknown>;
      const name = String(op.summary || op.operationId || `${method.toUpperCase()} ${path}`);
      let payload = "";
      const requestBody = op.requestBody && typeof op.requestBody === "object"
        ? (op.requestBody as Record<string, unknown>)
        : {};
      const content = requestBody.content && typeof requestBody.content === "object"
        ? (requestBody.content as Record<string, unknown>)
        : {};
      const jsonContent = content["application/json"];
      if (jsonContent && typeof jsonContent === "object") {
        const jc = jsonContent as Record<string, unknown>;
        if (jc.example !== undefined) {
          payload = JSON.stringify(jc.example, null, 2);
        }
      }
      presets.push({
        name,
        payload,
        headers: [],
        endpoint_type: "http",
        method: method.toUpperCase(),
        url: `\${baseUrl}${path}`,
        request_fields: { url: `\${baseUrl}${path}` },
      });
    }
  }

  const servers = Array.isArray(obj.servers) ? obj.servers : [];
  const firstServer = servers[0] && typeof servers[0] === "object"
    ? String((servers[0] as Record<string, unknown>).url || "")
    : "";
  const envVars: EnvVars = firstServer ? { baseUrl: firstServer } : {};
  return { presets, envVars };
}

function parseAsyncApiDocument(raw: unknown): { presets: PublisherPreset[]; envVars: EnvVars } {
  if (!raw || typeof raw !== "object") return { presets: [], envVars: {} };
  const obj = raw as Record<string, unknown>;
  if (!obj.asyncapi) return { presets: [], envVars: {} };
  const asyncApiVersion = String(obj.asyncapi);

  const resolveRef = (root: Record<string, unknown>, ref: unknown) => {
    const refValue = String(ref || "");
    if (!refValue.startsWith("#/")) return null;
    const parts = refValue.replace(/^#\//, "").split("/");
    let current: any = root;
    for (const part of parts) {
      if (!current || typeof current !== "object") return null;
      current = current[part];
    }
    return current ?? null;
  };

  const pickPayloadExample = (messageNodeRaw: unknown) => {
    const messageNode = messageNodeRaw && typeof messageNodeRaw === "object"
      ? (messageNodeRaw as Record<string, unknown>)
      : {};
    const messagePayload = messageNode.payload && typeof messageNode.payload === "object"
      ? (messageNode.payload as Record<string, unknown>)
      : {};
    if (messageNode.example !== undefined) {
      return JSON.stringify(messageNode.example, null, 2);
    }
    if (messagePayload.example !== undefined) {
      return JSON.stringify(messagePayload.example, null, 2);
    }
    if (Array.isArray(messagePayload.examples) && messagePayload.examples[0] !== undefined) {
      return JSON.stringify(messagePayload.examples[0], null, 2);
    }
    return "";
  };

  const channels = obj.channels && typeof obj.channels === "object"
    ? (obj.channels as Record<string, unknown>)
    : {};
  const presets: PublisherPreset[] = [];

  if (asyncApiVersion.startsWith("3")) {
    const operations = obj.operations && typeof obj.operations === "object"
      ? (obj.operations as Record<string, unknown>)
      : {};

    for (const [operationName, operationRaw] of Object.entries(operations)) {
      if (!operationRaw || typeof operationRaw !== "object") continue;
      const operation = operationRaw as Record<string, unknown>;
      const channelRef = operation.channel && typeof operation.channel === "object"
        ? String((operation.channel as Record<string, unknown>).$ref || "")
        : "";
      const channelNode = resolveRef(obj, channelRef);
      const channelAddress = channelNode && typeof channelNode === "object"
        ? String((channelNode as Record<string, unknown>).address || "")
        : "";

      const messagesRaw = Array.isArray(operation.messages) ? operation.messages : [];
      let payload = "";
      for (const messageRefRaw of messagesRaw) {
        const messageRef = messageRefRaw && typeof messageRefRaw === "object"
          ? String((messageRefRaw as Record<string, unknown>).$ref || "")
          : "";
        const messageNode = resolveRef(obj, messageRef);
        payload = pickPayloadExample(messageNode);
        if (payload) break;
      }

      const label = String(operation.summary || operation.title || operationName || channelAddress || "AsyncAPI operation");
      const normalizedAddress = channelAddress.replace(/^\/+/, "");
      if (!normalizedAddress) continue;
      presets.push({
        name: label,
        payload,
        headers: [],
        endpoint_type: "http",
        method: "POST",
        url: `\${baseUrl}/${normalizedAddress}`,
        request_fields: { url: `\${baseUrl}/${normalizedAddress}` },
      });
    }
  } else {
  for (const [channelName, channelDef] of Object.entries(channels)) {
    if (!channelDef || typeof channelDef !== "object") continue;
    const channel = channelDef as Record<string, unknown>;
    const opCandidates: Array<[string, Record<string, unknown>]> = [];
    if (channel.publish && typeof channel.publish === "object") {
      opCandidates.push(["PUBLISH", channel.publish as Record<string, unknown>]);
    }
    if (channel.subscribe && typeof channel.subscribe === "object") {
      opCandidates.push(["SUBSCRIBE", channel.subscribe as Record<string, unknown>]);
    }

    for (const [direction, operation] of opCandidates) {
      const payload = pickPayloadExample(operation.message);

      const summary = String(operation.summary || operation.operationId || `${direction} ${channelName}`);
      presets.push({
        name: summary,
        payload,
        headers: [],
        endpoint_type: "http",
        method: "POST",
        url: `\${baseUrl}/${channelName.replace(/^\/+/, "")}`,
        request_fields: { url: `\${baseUrl}/${channelName.replace(/^\/+/, "")}` },
      });
    }
  }
  }

  const servers = obj.servers && typeof obj.servers === "object"
    ? (obj.servers as Record<string, unknown>)
    : {};
  const firstServer = Object.values(servers)[0];
  const firstServerUrl = firstServer && typeof firstServer === "object"
    ? String((firstServer as Record<string, unknown>).url || "")
    : "";

  const envVars: EnvVars = firstServerUrl ? { baseUrl: firstServerUrl } : {};
  return { presets, envVars };
}

function mergePresets(
  existing: PresetsByPublisher,
  publisherName: string,
  incoming: PublisherPreset[],
): PresetsByPublisher {
  const next = { ...existing };
  const currentRows = Array.isArray(next[publisherName]) ? next[publisherName] : [];
  next[publisherName] = [...currentRows, ...incoming];
  return next;
}

type ParsedRequestImport =
  | { kind: "postman"; requests: ImportedRequest[]; envVars: EnvVars }
  | { kind: "openapi"; requests: ImportedRequest[]; envVars: EnvVars }
  | { kind: "asyncapi"; requests: ImportedRequest[]; envVars: EnvVars }
  | { kind: "unknown"; requests: ImportedRequest[]; envVars: EnvVars };

function parseRequestImport(raw: unknown): ParsedRequestImport {
  const postmanRequests = parsePostmanCollection(raw);
  if (postmanRequests.length > 0) {
    return { kind: "postman", requests: postmanRequests, envVars: {} };
  }

  const openApiImport = parseOpenApiDocument(raw);
  if (openApiImport.presets.length > 0) {
    return { kind: "openapi", requests: openApiImport.presets, envVars: openApiImport.envVars };
  }

  const asyncApiImport = parseAsyncApiDocument(raw);
  if (asyncApiImport.presets.length > 0) {
    return { kind: "asyncapi", requests: asyncApiImport.presets, envVars: asyncApiImport.envVars };
  }

  return { kind: "unknown", requests: [], envVars: {} };
}

export function extractImportedRequests(text: string): {
  kind: "postman" | "openapi" | "asyncapi";
  requests: ImportedRequest[];
  envVars: EnvVars;
} {
  const parsed = JSON.parse(text) as unknown;
  const result = parseRequestImport(parsed);
  if (result.kind === "unknown") {
    throw new Error("Unsupported file. Use Postman collection JSON, OpenAPI JSON, or AsyncAPI JSON.");
  }
  return result;
}

export async function replaceAppConfig(config: Record<string, unknown>) {
  await saveImportedConfig(config);
}

async function saveImportedConfig(config: Record<string, unknown>) {
  const normalizedConfig = ensureWorkspaceCollections(config);
  const refreshed = await saveWholeConfig(fetch, normalizedConfig);
  const nextConfig = { ...(refreshed as Record<string, unknown>) };
  delete nextConfig.routes;
  const history = sanitizePublisherHistory(nextConfig.history);
  nextConfig.history = history;
  const storageSecurity = resolveStorageSecurity(getAppState().storage_security, nextConfig);
  if (hasEncryptedMessages(storageSecurity)) {
    await setStoredJson(PUBLISHER_HISTORY_KEY, history, storageSecurity);
  } else if (storageSecurity.messagesEncrypted) {
    removeStoredJson(PUBLISHER_HISTORY_KEY);
  } else if (!isSensitiveConfig(nextConfig)) {
    await setStoredJson(PUBLISHER_HISTORY_KEY, history, storageSecurity);
  } else {
    removeStoredJson(PUBLISHER_HISTORY_KEY);
  }
  appShell.setConfig(nextConfig);
}

export async function importFromJsonText(
  text: string,
  options: {
    includeConfig: boolean;
    includePresets: boolean;
    targetPublisherName: string;
  },
) {
  const parsed = JSON.parse(text) as unknown;
  const currentConfig = structuredClone(getWorkspaceConfig());
  const nextConfig = ensureWorkspaceCollections(currentConfig);
  const mergedEnvVars = { ...sanitizeEnvVars(nextConfig.env_vars) };
  let mergedPresets = sanitizePresets(nextConfig.presets);
  let mergedHistory: PublisherHistoryStore = sanitizePublisherHistory(nextConfig.history);
  let importedPresetCount = 0;
  let importedKind = "unknown";

  if (parsed && typeof parsed === "object") {
    const obj = parsed as Record<string, unknown>;
    if (obj.type === "mqb-export" || obj.type === "mqb-presets") {
      importedKind = String(obj.type);
      if (options.includeConfig && obj.config && typeof obj.config === "object") {
        const importedConfig = ensureWorkspaceCollections(structuredClone(obj.config as Record<string, unknown>));
        nextConfig.publishers = importedConfig.publishers;
        nextConfig.consumers = importedConfig.consumers;
        nextConfig.routes = importedConfig.routes;
        nextConfig.default_tab = importedConfig.default_tab;
        nextConfig.log_level = importedConfig.log_level;
        nextConfig.logger = importedConfig.logger;
        nextConfig.ui_addr = importedConfig.ui_addr;
        nextConfig.metrics_addr = importedConfig.metrics_addr;
        nextConfig.config_security = importedConfig.config_security;
        mergedHistory = sanitizePublisherHistory(importedConfig.history);
        mergedPresets = sanitizePresets(importedConfig.presets);
        Object.assign(mergedEnvVars, sanitizeEnvVars(importedConfig.env_vars));
      }
      if (options.includePresets) {
        const importedPresets = sanitizePresets(obj.presets);
        for (const [publisherName, rows] of Object.entries(importedPresets)) {
          mergedPresets = mergePresets(mergedPresets, publisherName, rows);
          importedPresetCount += rows.length;
        }
        Object.assign(mergedEnvVars, sanitizeEnvVars(obj.envVars));
      }
    } else {
      const requestImport = parseRequestImport(obj);
      if (requestImport.kind === "unknown") {
        throw new Error(
          "Unsupported file. Use mqb export/presets JSON, Postman collection JSON, OpenAPI JSON, or AsyncAPI JSON.",
        );
      }
      importedKind = requestImport.kind;
      if (options.includePresets) {
        mergedPresets = mergePresets(mergedPresets, options.targetPublisherName, requestImport.requests);
        importedPresetCount += requestImport.requests.length;
        Object.assign(mergedEnvVars, requestImport.envVars);
      }
    }
  }

  if (options.includePresets) {
    nextConfig.presets = mergedPresets;
    nextConfig.env_vars = mergedEnvVars;
  }
  if (options.includeConfig) {
    nextConfig.history = mergedHistory;
  }

  if (options.includeConfig || options.includePresets) {
    await saveImportedConfig(nextConfig);
  }

  return { importedPresetCount, importedKind, configName: currentConfig?.name };
}

export async function importPostmanFromJsonText(text: string, targetPublisherName: string) {
  const result = await importFromJsonText(text, {
    includeConfig: false,
    includePresets: true,
    targetPublisherName,
  });
  if (result.importedKind !== "postman") {
    throw new Error("Selected file is not a valid Postman collection JSON.");
  }
  return result;
}

export async function importOpenApiFromJsonText(text: string, targetPublisherName: string) {
  const result = await importFromJsonText(text, {
    includeConfig: false,
    includePresets: true,
    targetPublisherName,
  });
  if (result.importedKind !== "openapi") {
    throw new Error("Selected file is not a valid OpenAPI JSON document.");
  }
  return result;
}

export async function importAsyncApiFromJsonText(text: string, targetPublisherName: string) {
  const result = await importFromJsonText(text, {
    includeConfig: false,
    includePresets: true,
    targetPublisherName,
  });
  if (result.importedKind !== "asyncapi") {
    throw new Error("Selected file is not a valid AsyncAPI JSON document.");
  }
  return result;
}
