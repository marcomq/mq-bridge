import { get } from "svelte/store";
import { appShell, getAppState, switchMainTab } from "./app-shell";
import { browserWindow, replaceHash } from "./browser";
import { pickFilePath } from "./desktop-file-dialog";
import { createLocalEntityId, getEntityDisplayLabel } from "./utils";
import { buildPublisherTree } from "./publisher-grouping";
import { PUBLISHER_TYPE_OPTIONS, REQUEST_BAR_LAYOUTS, formatEndpointTypeLabel, type RequestBarFieldDescriptor } from "./endpoint-metadata";
import { createConsumerEndpointFromPublisherEndpoint, createDefaultEndpoint, ensureEndpointDefaults, getEndpointType } from "./endpoint-utils";
import { publishersPanelState } from "./stores";
import { buildPublisherConfigDocument, buildPublisherConfigRouteDocument, extractImportedRequests, type ConfigJsonVariant } from "./import-export";
import { applyEndpointSchemaDefaults } from "./routes";
import { forceRefOnlyEndpoints, hideUnusedDeprecatedProperties, nameMappingRuleBranches, resolveRootArrayItemSchema } from "./schema-utils";
import { ensureWorkspaceCollections, sanitizePublisherHistory, type PublisherHistoryEntry, type PublisherHistoryStore } from "./workspace-config";
import type { ConsumerConfig, PublisherConfig, PublisherResponseState, PublishersAppConfig, PublishersSchemaRoot } from "./panel-types";
import type { PublishRequest } from "./generated/ui-types";

const PUBLISHER_STATE_KEY = "mqb_publisher_state";
const PUBLISHER_HISTORY_KEY = "mqb_publisher_history";
const SCHEMA_CONFIG_ENDPOINT_TYPES: Record<string, string> = {
  HttpConfig: "http",
  WebSocketConfig: "websocket",
  KafkaConfig: "kafka",
  MqttConfig: "mqtt",
  GrpcConfig: "grpc",
  AmqpConfig: "amqp",
  IbmMqConfig: "ibmmq",
  RedisStreamsConfig: "redis_streams",
  NatsConfig: "nats",
  MongoDbConfig: "mongodb",
  ZeroMqConfig: "zeromq",
  SqlxConfig: "sqlx",
  FileConfig: "file",
  MemoryConfig: "memory",
  SledConfig: "sled",
};

let activeConfig: PublishersAppConfig = { publishers: [], routes: {}, consumers: [] };
let activeSchema: PublishersSchemaRoot = {};
let formDrafts = new Map<number, PublisherConfig>();
let localPublisherState: Record<string, { payload: string; headers: Array<{ id: number; key: string; value: string; enabled: boolean }> }> = {};
let historyStore: PublisherHistoryStore = { version: 1, updated_at: 0, publishers: {} };
let responseStateByPublisher: Record<string, PublisherResponseState> = {};
let nextHeaderRowId = 1;
let historySyncTimer: number | null = null;
let renderedPublisherFormSignature: string | null = null;

export { PUBLISHER_TYPE_OPTIONS, createConsumerEndpointFromPublisherEndpoint };

export async function initPublishers(config: PublishersAppConfig, schema: PublishersSchemaRoot) {
  activeConfig = ensureWorkspaceCollections(config as any) as PublishersAppConfig;
  activeSchema = schema;
  formDrafts = new Map();
  responseStateByPublisher = {};
  nextHeaderRowId = 1;
  renderedPublisherFormSignature = null;
  loadLocalState();
  hydrateHistory();
  activeConfig.publishers = (activeConfig.publishers || []).map(normalizePublisher);
  syncConsumerPublisherReferences();
  const initialPublishersSnapshot = deepClone(activeConfig.publishers);
  getAppState().saved_sections.publishers = deepClone(initialPublishersSnapshot);
  browserWindow()._mqb_saved_sections = {
    ...(browserWindow()._mqb_saved_sections || {}),
    publishers: deepClone(initialPublishersSnapshot),
  };

  browserWindow().registerDirtySection?.("publishers", {
    buttonId: "workspace-save-button",
    getValue: () => activeConfig.publishers,
  });

  const hashMatch = browserWindow().location.hash.match(/^#publishers:(\d+)$/);
  const selectedIndex = hashMatch ? Number(hashMatch[1]) : Math.min((browserWindow()._mqb_last_publisher_idx as number | undefined) ?? 0, Math.max(activeConfig.publishers.length - 1, 0));
  publishersPanelState.update((state) => ({
    ...state,
    hasPublishers: activeConfig.publishers.length > 0,
    items: activeConfig.publishers.map((publisher, index) => ({
      name: String(publisher.name || ""),
      endpointType: formatEndpointTypeLabel(getEndpointType(publisher.endpoint)),
      originalIndex: index,
    })),
    groupedItems: buildPublisherTree(activeConfig.publishers),
    selectedIndex,
    selectedHistoryIndex: -1,
  }));
  await restorePublisherStateFromView(selectedIndex, { tab: (browserWindow()._mqb_last_publisher_tab as string | undefined) || "payload" });
}

export async function restorePublisherStateFromView(idx: number, options?: { tab?: string }) {
  const clamped = Math.min(Math.max(idx, 0), Math.max(activeConfig.publishers.length - 1, 0));
  const tab = (options?.tab as "payload" | "headers" | "history" | "definition" | undefined) || get(publishersPanelState).activeSubtab || "payload";
  getAppState().last_publisher_idx = clamped;
  getAppState().last_publisher_tab = tab;
  browserWindow()._mqb_last_publisher_idx = clamped;
  browserWindow()._mqb_last_publisher_tab = tab;
  publishersPanelState.update((state) => ({ ...state, selectedIndex: clamped, selectedHistoryIndex: -1, activeSubtab: tab }));
  replaceHash(`#publishers:${clamped}`);
  renderSelectedPublisher();
  await renderPublisherForm();
  renderSelectedPublisher();
}

export function selectPublisherSubtab(tab: string) {
  getAppState().last_publisher_tab = tab as any;
  publishersPanelState.update((state) => ({ ...state, activeSubtab: tab as any }));
  browserWindow()._mqb_last_publisher_tab = tab;
}

export async function addPublisherAction(endpointType: string) {
  activeConfig.publishers.push(normalizePublisher({
    id: createLocalEntityId("publisher"),
    name: "",
    endpoint: createDefaultPublisherEndpoint(endpointType),
    comment: "",
    payload: "{}",
    headers: [],
  } as PublisherConfig));
  await restorePublisherStateFromView(activeConfig.publishers.length - 1, { tab: "definition" });
}

export function updatePublisherPayload(value: string) {
  const publisher = currentPublisher();
  if (!publisher) return;
  publisher.payload = value;
  updateLocalPublisherState(publisher, { payload: value });
  publishersPanelState.update((state) => ({ ...state, requestPayload: value }));
  refreshPublisherDirty();
}

export function updatePublisherMethod(value: string) {
  const publisher = currentPublisher();
  if (!publisher) return;
  const endpointType = getEndpointType(publisher.endpoint);
  const endpointConfig = publisher.endpoint[endpointType] as Record<string, unknown>;
  endpointConfig.method = value;
  publishersPanelState.update((state) => ({ ...state, methodValue: value }));
  refreshPublisherDirty();
}

export function updatePublisherRequestField(fieldId: "pub-extra-1" | "pub-extra-2" | "pub-url", value: string) {
  const publisher = currentPublisher();
  if (!publisher) return;
  const endpointType = getEndpointType(publisher.endpoint);
  const endpointConfig = publisher.endpoint[endpointType] as Record<string, unknown>;
  const descriptors = REQUEST_BAR_LAYOUTS[endpointType]?.fields || [];
  const descriptor = descriptors.find((entry) => entry.inputId === fieldId);
  if (!descriptor) return;
  if (endpointType === "http" && descriptor.field === "url") {
    applyHttpUrlToEndpoint(endpointConfig, value);
  } else {
    endpointConfig[descriptor.field] = value;
  }
  renderSelectedPublisher();
  refreshPublisherDirty();
}

export function addPublisherMetadataRow() {
  publishersPanelState.update((state) => ({
    ...state,
    metadataRows: [...state.metadataRows, { id: nextHeaderRowId++, key: "", value: "", enabled: true }],
  }));
  syncMetadataRowsToPublisher();
}

export function updatePublisherMetadataRow(index: number, field: "key" | "value", value: string) {
  publishersPanelState.update((state) => ({
    ...state,
    metadataRows: state.metadataRows.map((row, rowIndex) => rowIndex === index ? { ...row, [field]: value } : row),
  }));
  syncMetadataRowsToPublisher();
}

export function togglePublisherMetadataRow(index: number, enabled: boolean) {
  publishersPanelState.update((state) => ({
    ...state,
    metadataRows: state.metadataRows.map((row, rowIndex) => rowIndex === index ? { ...row, enabled } : row),
  }));
  syncMetadataRowsToPublisher();
}

export function removePublisherMetadataRow(index: number) {
  publishersPanelState.update((state) => ({
    ...state,
    metadataRows: state.metadataRows.filter((_, rowIndex) => rowIndex !== index),
  }));
  syncMetadataRowsToPublisher();
}

export async function saveCurrentPublisherAction(button?: HTMLElement | null) {
  const publisher = currentPublisher();
  if (!publisher) return null;
  await flushPendingFormDraft();
  const idx = get(publishersPanelState).selectedIndex;
  const draft = formDrafts.get(idx);
  const previousPublisherName = String(publisher.name || "");
  const previousPublisherId = String(publisher.id || "");
  if (draft) {
    const next = normalizePublisher({ ...publisher, ...deepClone(draft) });
    next.payload = publisher.payload;
    next.headers = get(publishersPanelState).metadataRows.map(({ id, ...row }) => row);
    activeConfig.publishers[idx] = next;
  }
  syncConsumerPublisherReferences(previousPublisherName, previousPublisherId);
  const saved = await browserWindow().saveConfigSection?.("publishers", activeConfig.publishers, false, button) ?? null;
  if (saved && Array.isArray((saved as any).publishers)) {
    activeConfig.publishers = (saved as any).publishers.map(normalizePublisher);
  }
  await restorePublisherStateFromView(idx, { tab: get(publishersPanelState).activeSubtab });
  return saved;
}

export async function sendPublisherAction() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const requestDetails = buildPublishRequestDetails(publisher);
  setPublisherResponsePending(publisher);
  const startedAt = Date.now();
  const controller = new AbortController();
  const timeoutId = window.setTimeout(() => controller.abort(), 10000);

  try {
    const response = await fetch("/publish", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(requestDetails.request),
      signal: controller.signal,
    });
    const duration = Date.now() - startedAt;
    const responseText = await response.text();
    const responseData = parseJsonIfPossible(responseText);

    applyPublisherResponseState(publisher, response, duration, responseData, requestDetails);

    const entry = buildHistoryEntry(
      publisher,
      response.status,
      response.statusText || "OK",
      duration,
      responseData,
      requestDetails.historyMetadata,
    );
    const historyKey = historyBucketKeyFor(publisher);
    historyStore.publishers[historyKey] ||= [];
    historyStore.publishers[historyKey] = [entry, ...(historyStore.publishers[historyKey] || [])];
    historyStore.updated_at = Date.now();
    persistHistory();
    renderSelectedPublisher();
    scheduleHistorySync();
  } catch (error) {
    const isTimeout = error instanceof DOMException && error.name === "AbortError";
    const nextResponseState: PublisherResponseState = {
      responseVisible: true,
      responseTabLabel: "Response ✓ Error",
      responseStatusLabel: "Error",
      responseStatusText: "",
      responseStatusColor: "var(--accent-kafka)",
      responseDurationLabel: "",
      responseSizeLabel: "",
      requestRows: requestDetails.requestRows,
      requestHeaders: requestDetails.requestHeaders.map(({ k, v }) => [k, v]),
      responseHeaders: [],
      responsePayload: isTimeout
        ? "Error: Publish request timed out after 10s"
        : `Error: ${(error as Error).message}`,
    };
    setPublisherResponseState(publisher, nextResponseState);
    publishersPanelState.update((state) => ({
      ...state,
      ...nextResponseState,
    }));
  } finally {
    window.clearTimeout(timeoutId);
  }
}

export function clearActivePublisherHistory() {
  const publisher = currentPublisher();
  if (!publisher) return;
  delete historyStore.publishers[historyBucketKeyFor(publisher)];
  publishersPanelState.update((state) => ({ ...state, selectedHistoryIndex: -1 }));
  persistHistory();
  renderSelectedPublisher();
  scheduleHistorySync();
}

export async function showPublisherHistoryEntry(historyIndex: number) {
  const publisher = currentPublisher();
  if (!publisher) return;
  const entry = currentHistoryEntries()[historyIndex];
  if (!entry) return;
  publisher.payload = entry.payload;
  applyHistoryRequestFieldsToPublisher(publisher, entry);
  const nextResponseState = buildHistoryResponseState(publisher, entry);
  const headers = entry.metadata.map((row) => ({ id: nextHeaderRowId++, key: row.k, value: row.v, enabled: true }));
  setLocalPublisherState(publisher, { payload: entry.payload, headers });
  setPublisherResponseState(publisher, nextResponseState);
  publishersPanelState.update((state) => ({
    ...state,
    selectedHistoryIndex: historyIndex,
    ...nextResponseState,
  }));
  renderSelectedPublisher();
}

export async function saveCurrentPublisherVariantAction() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const name = await browserWindow().mqbPrompt?.("Choose a name for the saved publisher variant.", "Save Publisher Variant");
  if (!name) return;
  const next = normalizePublisher(deepClone(publisher));
  next.id = createLocalEntityId("publisher");
  next.name = name;
  activeConfig.publishers.push(next);
  renderSelectedPublisher();
}

export async function copyCurrentPublisherAction() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const consumers = Array.isArray(activeConfig.consumers) ? activeConfig.consumers : (activeConfig.consumers = []);
  const previousConsumers = deepClone(consumers);
  consumers.push({
    id: createLocalEntityId("consumer"),
    name: "",
    endpoint: createConsumerEndpointFromPublisherEndpoint(publisher.endpoint),
    output: { mode: "none" },
    response: null,
  } as ConsumerConfig);
  if (!browserWindow()._mqb_saved_sections || !("consumers" in browserWindow()._mqb_saved_sections)) {
    browserWindow()._mqb_saved_sections = {
      ...(browserWindow()._mqb_saved_sections || {}),
      consumers: previousConsumers,
    };
    getAppState().saved_sections.consumers = deepClone(previousConsumers);
  }
  getAppState().pending_consumer_restore = { idx: consumers.length - 1, tab: "definition" };
  browserWindow().refreshDirtySection?.("consumers");
  await switchMainTab("consumers");
}

export function cloneCurrentPublisherAction() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const cloned = normalizePublisher(deepClone(publisher));
  cloned.id = createLocalEntityId("publisher");
  activeConfig.publishers.push(cloned);
  void restorePublisherStateFromView(activeConfig.publishers.length - 1, { tab: get(publishersPanelState).activeSubtab });
  refreshPublisherDirty();
}

export async function currentPublisherConfigVariants(): Promise<ConfigJsonVariant[] | null> {
  const publisher = currentPublisher();
  if (!publisher) return null;
  await flushPendingFormDraft();
  const idx = get(publishersPanelState).selectedIndex;
  const draft = formDrafts.get(idx);
  const exportPublisher = draft
    ? normalizePublisher({ ...publisher, ...deepClone(draft) })
    : deepClone(publisher);
  const record = exportPublisher as unknown as Record<string, unknown>;
  const { id: _id, ...raw } = record;
  return [
    { id: "raw", label: "App config", value: raw, editable: true },
    {
      id: "publisher",
      label: "Publisher.from_config",
      value: buildPublisherConfigDocument(record),
    },
    {
      id: "route",
      label: "Route.from_config",
      value: buildPublisherConfigRouteDocument(record),
    },
  ];
}

// Replaces the selected publisher with an edited raw config, keeping its id.
export async function applyCurrentPublisherRawConfig(value: Record<string, unknown>) {
  const publisher = currentPublisher();
  if (!publisher) return;
  if (!value.endpoint || typeof value.endpoint !== "object" || Array.isArray(value.endpoint)) {
    throw new Error("The publisher config needs an `endpoint` object.");
  }
  const idx = get(publishersPanelState).selectedIndex;
  const next = normalizePublisher({ ...value, id: publisher.id } as unknown as PublisherConfig);
  activeConfig.publishers[idx] = next;
  // Local request state would otherwise override the applied payload and headers.
  setLocalPublisherState(next, {
    payload: next.payload,
    headers: (next.headers ?? []).map((row) => ({ ...row, id: nextHeaderRowId++ })),
  });
  formDrafts.delete(idx);
  renderedPublisherFormSignature = null;
  refreshPublisherDirty();
  await restorePublisherStateFromView(idx, { tab: get(publishersPanelState).activeSubtab });
}

export async function deleteCurrentPublisherAction() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const confirmed = await browserWindow().mqbConfirm?.(
    `Delete publisher "${publisher.name || "Untitled Publisher"}"?`,
    "Delete Publisher",
  );
  if (!confirmed) return;

  const selectedIndex = get(publishersPanelState).selectedIndex;
  activeConfig.publishers.splice(selectedIndex, 1);
  for (const key of publisherStorageKeysFor(publisher)) {
    delete localPublisherState[key];
    delete historyStore.publishers[key];
    delete responseStateByPublisher[key];
  }
  persistLocalState();
  persistHistory();

  const saved = await browserWindow().saveConfigSection?.("publishers", activeConfig.publishers, false, undefined) ?? null;
  if (saved && Array.isArray((saved as any).publishers)) {
    activeConfig.publishers = (saved as any).publishers.map(normalizePublisher);
  }
  browserWindow().markSectionSaved?.("publishers", activeConfig.publishers);

  if (activeConfig.publishers.length === 0) {
    publishersPanelState.update((state) => ({
      ...state,
      hasPublishers: false,
      items: [],
      groupedItems: [],
      selectedIndex: -1,
      selectedHistoryIndex: -1,
    }));
    replaceHash("#publishers:0");
    return;
  }

  const nextIndex = Math.min(selectedIndex, activeConfig.publishers.length - 1);
  await restorePublisherStateFromView(nextIndex, { tab: "definition" });
}

export async function importAsyncApiToPublisherAction(jsonText: string) {
  importRequestsToPublishers(jsonText);
}

function importRequestsToPublishers(jsonText: string) {
  // Malformed JSON, an unsupported document or an unknown endpoint type all throw
  // from here; the callers are async, so an escaping error is an unhandled rejection.
  try {
    const imported = extractImportedRequests(jsonText);
    for (const request of imported.requests) {
      const endpointType = String(request.endpoint_type || "http");
      const endpoint = createDefaultPublisherEndpoint(endpointType);
      const endpointConfig = endpoint[endpointType] as Record<string, unknown>;
      if (endpointType === "http") {
        if (request.url) applyHttpUrlToEndpoint(endpointConfig, request.url);
        if (request.method) endpointConfig.method = request.method;
      }
      for (const [field, value] of Object.entries(request.request_fields || {})) {
        if (value) endpointConfig[field] = value;
      }
      const headers = Array.isArray(request.headers) ? request.headers : [];
      const publisher = normalizePublisher({
        id: createLocalEntityId("publisher"),
        name: String(request.name || "Imported request"),
        endpoint,
        payload: String(request.payload || ""),
        headers,
        comment: "",
      } as PublisherConfig);
      activeConfig.publishers.push(publisher);
      setLocalPublisherState(publisher, {
        payload: publisher.payload,
        headers: (publisher.headers || []).map((row) => ({ id: nextHeaderRowId++, ...row })),
      });
    }
  } catch (error) {
    console.error("Failed to import requests:", error);
    void browserWindow().mqbAlert?.(`Import failed: ${(error as Error)?.message || error}`);
  }
  renderSelectedPublisher();
}

export async function importMqbToPublisherAction(jsonText: string) {
  const parsed = JSON.parse(jsonText) as any;
  if (parsed?.type !== "mqb-presets") {
    throw new Error("Selected file is not a valid mq-bridge export/presets file.");
  }
  const imported = ensureWorkspaceCollections({
    publishers: activeConfig.publishers,
    consumers: activeConfig.consumers || [],
    routes: activeConfig.routes || {},
    presets: parsed.presets,
  } as any);
  activeConfig.publishers = imported.publishers as any;
  browserWindow().appConfig = { ...(browserWindow().appConfig || {}), publishers: activeConfig.publishers };
  renderSelectedPublisher();
}

export async function importPostmanToPublisherAction(jsonText: string) {
  importRequestsToPublishers(jsonText);
}

export async function importOpenApiToPublisherAction(jsonText: string) {
  importRequestsToPublishers(jsonText);
}

export function copyPublisherResponse() {
  const publisher = currentPublisher();
  if (!publisher) return;
  void writeClipboardText(String(getPublisherResponseState(publisher)?.responsePayload || ""));
}

export function copyPublisherResponseJson() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const payload = String(getPublisherResponseState(publisher)?.responsePayload || "");
  const parsed = parseJsonIfPossible(payload);
  void writeClipboardText(typeof parsed === "string" ? payload : JSON.stringify(parsed, null, 2));
}

export function copyPublisherAsCurl() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const { endpointType, endpointConfig, values } = currentRequestBarValues(publisher);
  if (endpointType !== "http") {
    void browserWindow().mqbAlert?.("Copy as curl is only available for HTTP publishers.");
    return;
  }
  const method = String(endpointConfig.method || "POST").trim() || "POST";
  const url = String(values["pub-url"] || "").trim();
  const parts = [`curl -X ${method} ${shellQuote(url)}`];
  for (const row of get(publishersPanelState).metadataRows) {
    if (!row.enabled || !row.key.trim()) continue;
    parts.push(`-H ${shellQuote(`${row.key.trim()}: ${row.value}`)}`);
  }
  const payload = String(publisher.payload || "");
  if (payload) parts.push(`-d ${shellQuote(payload)}`);
  void writeClipboardText(parts.join(" \\\n  "));
}

export async function savePublisherHistoryAsPublisherAction(historyIndex: number) {
  const publisher = currentPublisher();
  if (!publisher) return;
  const entry = currentHistoryEntries()[historyIndex];
  if (!entry) return;
  const name = await browserWindow().mqbPrompt?.("Choose a name for the new publisher.", "Save History Entry As Publisher");
  if (!name) return;
  const next = normalizePublisher(deepClone(publisher));
  next.id = createLocalEntityId("publisher");
  next.name = name;
  next.payload = entry.payload;
  next.headers = entry.metadata.map((row) => ({ key: row.k, value: row.v, enabled: true }));
  applyHistoryRequestFieldsToPublisher(next, entry);
  activeConfig.publishers.push(next);
  setLocalPublisherState(next, {
    payload: entry.payload,
    headers: next.headers.map((row) => ({ id: nextHeaderRowId++, ...row })),
  });
  await restorePublisherStateFromView(activeConfig.publishers.length - 1, { tab: "payload" });
}
export async function resendPublisherHistoryAction(historyIndex: number) {
  if (!currentHistoryEntries()[historyIndex]) return;
  await showPublisherHistoryEntry(historyIndex);
  await sendPublisherAction();
}
export async function editEnvironmentVarsAction() {}

export function beautifyPublisherPayloadAction() {
  const publisher = currentPublisher();
  if (!publisher) return;
  try {
    publisher.payload = JSON.stringify(JSON.parse(String(publisher.payload || "")), null, 2);
    renderSelectedPublisher();
  } catch {
    void browserWindow().mqbAlert?.("Invalid JSON");
  }
}

async function writeClipboardText(text: string) {
  try {
    await navigator.clipboard.writeText(text);
  } catch (error) {
    console.error("Failed to copy to clipboard:", error);
    void browserWindow().mqbAlert?.("Failed to copy to clipboard");
  }
}

function shellQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

function deepClone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function normalizeEndpoint(endpoint: unknown) {
  return ensureEndpointDefaults(endpoint, normalizeEndpoint);
}

function createDefaultPublisherEndpoint(endpointType: string): Record<string, unknown> {
  const endpoint = createDefaultEndpoint(endpointType);
  const root = endpoint[endpointType] as Record<string, unknown>;
  if (endpointType === "http") {
    root.url = "http://localhost:8080";
    root.path = "/";
    root.method = "POST";
    root.custom_headers = {};
    root.fire_and_forget = false;
    root.compression_enabled = false;
  }
  if (endpointType === "amqp") {
    root.url = "amqp://guest:guest@localhost:5672/%2f";
    root.queue = "jobs";
  }
  if (endpointType === "kafka") {
    root.url = "localhost:9092";
    root.topic = "events";
  }
  if (endpointType === "mongodb") {
    root.url = "mongodb://localhost:27017";
    root.database = "app";
    root.collection = "messages";
  }
  if (endpointType === "ibmmq") {
    root.url = "localhost(1414)";
    root.queue_manager = "QM1";
    root.channel = "DEV.APP.SVRCONN";
    root.queue = "DEV.QUEUE.1";
  }
  if (endpointType === "static") {
    endpoint.static = "";
  }
  endpoint.middlewares = [];
  return endpoint;
}

function normalizePublisher(input: PublisherConfig): PublisherConfig {
  const next = deepClone(input);
  next.id = String(next.id || "").trim() || createLocalEntityId("publisher");
  next.name = String(next.name || "");
  next.comment = String(next.comment || "");
  next.endpoint = normalizeEndpoint(next.endpoint);
  next.payload = String((next as any).payload || "{}");
  next.headers = Array.isArray(next.headers) ? next.headers.map((row) => ({
    key: String(row.key || ""),
    value: String(row.value || ""),
    enabled: row.enabled !== false,
  })) : [];
  return next;
}

function currentPublisher(): PublisherConfig | null {
  const idx = get(publishersPanelState).selectedIndex;
  if (activeConfig.publishers.length === 0) return null;
  if (idx < 0 || idx >= activeConfig.publishers.length || !Number.isInteger(idx)) {
    return activeConfig.publishers[0] || null;
  }
  return activeConfig.publishers[idx];
}

function loadLocalState() {
  const preloaded = browserWindow()._mqb_storage_cache?.publisher_state;
  if (preloaded && typeof preloaded === "object") {
    localPublisherState = preloaded;
    return;
  }
  try {
    const raw = browserWindow().localStorage?.getItem(PUBLISHER_STATE_KEY);
    localPublisherState = raw ? JSON.parse(raw) : {};
  } catch {
    localPublisherState = {};
  }
}

function hydrateHistory() {
  const preloaded = browserWindow()._mqb_storage_cache?.publisher_history;
  if (preloaded) {
    const sanitizedPreloaded = sanitizePublisherHistory(preloaded);
    if (hasPublisherHistoryEntries(sanitizedPreloaded)) {
      historyStore = sanitizedPreloaded;
      return;
    }
  }
  const localRaw = browserWindow().localStorage?.getItem(PUBLISHER_HISTORY_KEY);
  if (localRaw) {
    try {
      const sanitizedLocal = sanitizePublisherHistory(JSON.parse(localRaw));
      if (hasPublisherHistoryEntries(sanitizedLocal)) {
        historyStore = sanitizedLocal;
        return;
      }
    } catch {}
  }
  historyStore = sanitizePublisherHistory(activeConfig.history);
  persistHistory();
}

function hasPublisherHistoryEntries(store: PublisherHistoryStore) {
  return Object.values(store.publishers || {}).some((entries) => Array.isArray(entries) && entries.length > 0);
}

function persistLocalState() {
  browserWindow().localStorage?.setItem(PUBLISHER_STATE_KEY, JSON.stringify(localPublisherState));
}

function persistHistory() {
  browserWindow().localStorage?.setItem(PUBLISHER_HISTORY_KEY, JSON.stringify(historyStore));
}

function historyBucketKeyFor(publisher: PublisherConfig): string {
  return String(publisher.id || publisher.name || "");
}

function publisherStorageKeysFor(publisher: PublisherConfig): string[] {
  const keys = [String(publisher.id || "").trim(), String(publisher.name || "").trim()].filter(Boolean);
  return Array.from(new Set(keys));
}

function readLocalPublisherState(publisher: PublisherConfig) {
  for (const key of publisherStorageKeysFor(publisher)) {
    const entry = localPublisherState[key];
    if (entry && typeof entry === "object") {
      return entry;
    }
  }
  return undefined;
}

function currentHistoryEntries(): PublisherHistoryEntry[] {
  const publisher = currentPublisher();
  if (!publisher) return [];
  for (const key of publisherStorageKeysFor(publisher)) {
    const entries = historyStore.publishers[key];
    if (Array.isArray(entries)) return entries;
  }
  return [];
}

function buildMetadataRows(publisher: PublisherConfig) {
  const localRows = readLocalPublisherState(publisher)?.headers;
  if (Array.isArray(localRows) && localRows.length > 0) {
    return normalizeHeaderRows(localRows.map((row) => ({ ...row })));
  }
  const endpointType = getEndpointType(publisher.endpoint);
  const endpointConfig = publisher.endpoint[endpointType] as Record<string, unknown>;
  const headers = endpointType === "http" && endpointConfig?.custom_headers && typeof endpointConfig.custom_headers === "object"
    ? Object.entries(endpointConfig.custom_headers as Record<string, unknown>).map(([key, value]) => ({
      id: nextHeaderRowId++,
      key,
      value: String(value),
      enabled: true,
    }))
    : [];
  return normalizeHeaderRows(headers);
}

function normalizeHeaderRows(rows: Array<{ id: number; key: string; value: string; enabled: boolean }>) {
  const seen = new Set<number>();
  let maxAssignedId = 0;
  const normalizedRows = rows.map((row) => {
    let nextId = Number.isInteger(row.id) && row.id > 0 ? row.id : 0;
    if (!nextId || seen.has(nextId)) {
      nextId = 1;
      while (seen.has(nextId)) {
        nextId += 1;
      }
    }
    seen.add(nextId);
    maxAssignedId = Math.max(maxAssignedId, nextId);
    return { ...row, id: nextId };
  });
  nextHeaderRowId = Math.max(nextHeaderRowId, maxAssignedId + 1);
  return normalizedRows;
}

function currentRequestBarValues(publisher: PublisherConfig) {
  const endpointType = getEndpointType(publisher.endpoint);
  const endpointConfig = publisher.endpoint[endpointType] as Record<string, unknown>;
  const requestBar = REQUEST_BAR_LAYOUTS[endpointType] || { fields: [] };
  const initial: Record<string, string> = { "pub-extra-1": "", "pub-extra-2": "", "pub-url": "" };
  for (const field of requestBar.fields || []) {
    if (endpointType === "http" && field.field === "url") {
      initial[field.inputId] = composeHttpRequestUrl(endpointConfig);
    } else {
      initial[field.inputId] = String(endpointConfig[field.field] || "");
    }
  }
  return { endpointType, endpointConfig, requestBar, values: initial };
}

function renderSelectedPublisher() {
  const publisher = currentPublisher();
  if (!publisher) {
    publishersPanelState.update((state) => ({ ...state, hasPublishers: false, items: [], groupedItems: [], historyRows: [] }));
    return;
  }
  const { endpointType, requestBar, values } = currentRequestBarValues(publisher);
  const urlDescriptor = requestBar.fields?.find((field) => field.inputId === "pub-url");
  const responseState = getPublisherResponseState(publisher);
  const metadataRows = buildMetadataRows(publisher);
  const historyRows = currentHistoryEntries().map((entry, index) => ({
    historyIndex: index,
    timeLabel: new Date(entry.time || 0).toLocaleTimeString(),
    statusLabel: String(entry.status || ""),
    statusClass: entry.ok === false ? "danger" : "success",
    payloadPreview: entry.payload,
    pinned: entry.pinned === true,
  }));

  publishersPanelState.update((state) => ({
    ...state,
    hasPublishers: activeConfig.publishers.length > 0,
    items: activeConfig.publishers.map((row, index) => ({
      name: String(row.name || ""),
      endpointType: formatEndpointTypeLabel(getEndpointType(row.endpoint)),
      originalIndex: index,
    })),
    groupedItems: buildPublisherTree(activeConfig.publishers),
    selectedIndex: activeConfig.publishers.indexOf(publisher),
    selectedHistoryIndex: state.selectedHistoryIndex,
    endpointType: formatEndpointTypeLabel(endpointType),
    isNew: false,
    deleteLabel: "Delete",
    methodVisible: Boolean(requestBar.showMethod),
    methodValue: String((publisher.endpoint[endpointType] as Record<string, unknown>)?.method || "POST"),
    extraFieldOne: fieldStateFor(requestBar.fields?.find((field) => field.inputId === "pub-extra-1"), values["pub-extra-1"]),
    extraFieldTwo: fieldStateFor(requestBar.fields?.find((field) => field.inputId === "pub-extra-2"), values["pub-extra-2"]),
    urlField: {
      ...fieldStateFor(urlDescriptor, values["pub-url"], "URL"),
      browse: browsableFilePath(endpointType, urlDescriptor),
    },
    requestPayload: readLocalPublisherState(publisher)?.payload || publisher.payload || "{}",
    metadataRows,
    responseVisible: responseState?.responseVisible ?? true,
    responseTabLabel: responseState?.responseTabLabel ?? "Response",
    responseStatusLabel: responseState?.responseStatusLabel ?? "Ready",
    responseStatusText: responseState?.responseStatusText ?? "",
    responseStatusColor: responseState?.responseStatusColor ?? "var(--text-primary)",
    responseDurationLabel: responseState?.responseDurationLabel ?? "",
    responseSizeLabel: responseState?.responseSizeLabel ?? "",
    requestRows: responseState?.requestRows ?? [],
    requestHeaders: responseState?.requestHeaders ?? [],
    responseHeaders: responseState?.responseHeaders ?? [],
    responsePayload: responseState?.responsePayload ?? "",
    historyRows,
  }));
}

function fieldStateFor(descriptor: RequestBarFieldDescriptor | undefined, value: string, fallbackLabel = "Target") {
  return {
    label: descriptor?.label || fallbackLabel,
    placeholder: descriptor?.placeholder || "",
    value,
    visible: Boolean(descriptor),
  };
}

// A file publisher edits its path in the request bar, not in the definition form, so the
// desktop picker has to be offered here rather than through the schema field renderer.
function browsableFilePath(endpointType: string, descriptor: RequestBarFieldDescriptor | undefined) {
  return appShell.isDesktop() && endpointType === "file" && descriptor?.field === "path";
}

export async function browsePublisherFilePath() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const { values } = currentRequestBarValues(publisher);
  const selected = await pickFilePath("save", values["pub-url"]);
  if (selected === null) return;
  updatePublisherRequestField("pub-url", selected);
}

function refreshPublisherDirty() {
  browserWindow().refreshDirtySection?.("publishers");
}

function syncMetadataRowsToPublisher() {
  const publisher = currentPublisher();
  if (!publisher) return;
  const state = get(publishersPanelState);
  const rows = state.metadataRows;
  publisher.headers = rows.map(({ id, ...row }) => row);
  const endpointType = getEndpointType(publisher.endpoint);
  const endpointConfig = publisher.endpoint[endpointType] as Record<string, unknown>;
  if (endpointType === "http") {
    endpointConfig.custom_headers = Object.fromEntries(
      rows.filter((row) => row.enabled && row.key.trim()).map((row) => [row.key.trim(), row.value]),
    );
  }
  setLocalPublisherState(publisher, { headers: rows });
  refreshPublisherDirty();
}

function setLocalPublisherState(publisher: PublisherConfig, partial: Partial<{ payload: string; headers: Array<{ id: number; key: string; value: string; enabled: boolean }> }>) {
  const existing = readLocalPublisherState(publisher);
  const nextValue = {
    payload: partial.payload ?? existing?.payload ?? publisher.payload ?? "{}",
    headers: partial.headers ?? existing?.headers ?? [],
  };
  for (const key of publisherStorageKeysFor(publisher)) {
    localPublisherState[key] = nextValue;
  }
  persistLocalState();
}

function updateLocalPublisherState(publisher: PublisherConfig, partial: Partial<{ payload: string; headers: Array<{ id: number; key: string; value: string; enabled: boolean }> }>) {
  setLocalPublisherState(publisher, partial);
}

async function renderPublisherForm() {
  const publisher = currentPublisher();
  const container = document.getElementById("pub-config-form");
  if (!publisher || !container) return;
  // Rebuilding the schema form dominates the cost of a tab switch, and the panels stay mounted,
  // so the already-rendered form is reused whenever it still matches the selected publisher.
  const signature = JSON.stringify(publisher);
  if (container.childElementCount > 0 && renderedPublisherFormSignature === signature) return;
  renderedPublisherFormSignature = signature;
  const forms = appShell.forms() as any;
  (window as any)._mqb_form_mode = "publisher";
  const schema = resolveRootArrayItemSchema(activeSchema as Record<string, any>, "publishers");
  applyEndpointSchemaDefaults(schema as any);
  forceRefOnlyEndpoints(schema as any);
  nameMappingRuleBranches(schema as any);
  hideUnusedDeprecatedProperties(schema as any, publisher);
  Object.entries(SCHEMA_CONFIG_ENDPOINT_TYPES).forEach(([defName, endpointType]) => {
    const endpointSchema = (schema as any).$defs?.[defName];
    if (!endpointSchema?.properties) return;
    (REQUEST_BAR_LAYOUTS[endpointType]?.fields || []).forEach(({ field: fieldName }) => {
      if (endpointSchema.properties[fieldName]) {
        endpointSchema.properties[fieldName].hidden = true;
      }
      if (Array.isArray(endpointSchema.required)) {
        endpointSchema.required = endpointSchema.required.filter((key: string) => key !== fieldName);
      }
    });
  });
  const httpConfigSchema = (schema as any).$defs?.HttpConfig;
  if (httpConfigSchema?.properties?.custom_headers) {
    httpConfigSchema.properties.custom_headers.hidden = true;
  }
  if ((schema as any).properties?.id) {
    (schema as any).properties.id.hidden = true;
  }
  if (Array.isArray((schema as any).required)) {
    (schema as any).required = (schema as any).required.filter((key: string) => key !== "name");
  }
  await forms.init(container, schema, deepClone(publisher), (updated: PublisherConfig) => {
    formDrafts.set(get(publishersPanelState).selectedIndex, updated);
    const current = currentPublisher();
    if (!current) return;
    Object.assign(current, normalizePublisher({ ...current, ...updated }));
    renderedPublisherFormSignature = JSON.stringify(current);
    refreshPublisherDirty();
    renderSelectedPublisher();
  });
}

function applyHttpUrlToEndpoint(endpointConfig: Record<string, unknown>, value: string) {
  try {
    const parsed = new URL(value);
    endpointConfig.url = `${parsed.protocol}//${parsed.host}`;
    endpointConfig.path = `${parsed.pathname || "/"}${parsed.search || ""}`;
  } catch {
    endpointConfig.url = value;
    endpointConfig.path = "/";
  }
}

function applyHistoryRequestFieldsToPublisher(publisher: PublisherConfig, entry: PublisherHistoryEntry) {
  const endpointType = getEndpointType(publisher.endpoint);
  const endpointConfig = publisher.endpoint[endpointType] as Record<string, unknown>;
  if (endpointType === "http") {
    const historyMethod = entry.method || entry.requestMetadata.http_method || "";
    if (historyMethod) endpointConfig.method = historyMethod;
    const value = entry.requestMetadata["request_bar.url"]
      || entry.requestMetadata["request_bar.pub-url"]
      || entry.request_fields.url
      || entry.url
      || "";
    if (value) applyHttpUrlToEndpoint(endpointConfig, value);
    if (!value && entry.requestMetadata.http_path) {
      endpointConfig.path = entry.requestMetadata.http_path;
    }
    if (entry.metadata.length > 0) {
      endpointConfig.custom_headers = Object.fromEntries(entry.metadata.map((row) => [row.k, row.v]));
    }
    return;
  }
  const layout = REQUEST_BAR_LAYOUTS[endpointType] || { fields: [] };
  for (const field of layout.fields || []) {
    const nextValue = entry.requestMetadata[`request_bar.${field.field}`]
      || entry.requestMetadata[`request_bar.${field.inputId}`]
      || entry.request_fields[field.field]
      || "";
    if (nextValue) {
      endpointConfig[field.field] = nextValue;
    }
  }
}

function buildHistoryEntry(
  publisher: PublisherConfig,
  status: number,
  statusText: string,
  duration: number,
  responseData: unknown,
  requestMetadataOverride?: Record<string, string>,
): PublisherHistoryEntry {
  const endpointType = getEndpointType(publisher.endpoint);
  const { requestBar, values } = currentRequestBarValues(publisher);
  const requestMetadata: Record<string, string> = {};
  const requestFields: Record<string, string> = {};
  for (const field of requestBar.fields || []) {
    const value = values[field.inputId];
    if (!value) continue;
    requestMetadata[`request_bar.${field.field}`] = value;
    requestMetadata[`request_bar.${field.inputId}`] = value;
    requestFields[field.field] = value;
  }
  const mergedRequestMetadata = requestMetadataOverride
    ? { ...requestMetadata, ...requestMetadataOverride }
    : requestMetadata;
  return {
    publisher_id: publisher.id,
    name: String(publisher.name || ""),
    payload: String(publisher.payload || ""),
    headers: (publisher.headers || []).map((row) => ({ key: String(row.key || ""), value: String(row.value || ""), enabled: row.enabled !== false })),
    metadata: (publisher.headers || []).filter((row) => row.enabled !== false && row.key).map((row) => ({ k: row.key, v: row.value })),
    endpoint_type: endpointType,
    method: String((publisher.endpoint[endpointType] as Record<string, unknown>)?.method || ""),
    url: requestFields.url || "",
    request_fields: requestFields,
    requestMetadata: mergedRequestMetadata,
    status,
    statusText,
    duration,
    time: Date.now(),
    ok: status >= 200 && status < 300,
    responseData,
  };
}

function buildHistoryResponseState(publisher: PublisherConfig, entry: PublisherHistoryEntry): PublisherResponseState {
  const responsePayload = extractPublisherResponsePayload(entry.responseData);
  const responseHeaders = extractPublisherResponseHeaders(entry.responseData);
  const payloadString = typeof responsePayload === "string"
    ? responsePayload
    : JSON.stringify(responsePayload ?? "", null, 2);
  const payloadSize = new TextEncoder().encode(payloadString).length;
  const endpointType = getEndpointType(publisher.endpoint);
  const endpointConfig = publisher.endpoint[endpointType] as Record<string, unknown>;
  const requestBar = REQUEST_BAR_LAYOUTS[endpointType] || { fields: [] };
  const values = requestValuesFromHistoryEntry(entry, requestBar.fields || []);
  const statusOk = entry.ok ?? (entry.status >= 200 && entry.status < 300);
  const statusLabel = entry.displayStatus || (statusOk ? String(entry.status || "OK") : "Error");
  const statusText = entry.displayStatusText || entry.statusText || "";

  return {
    responseVisible: true,
    responseTabLabel: `Response ✓ ${statusLabel}`,
    responseStatusLabel: statusLabel,
    responseStatusText: statusText,
    responseStatusColor: statusOk ? "var(--accent-http)" : "var(--accent-kafka)",
    responseDurationLabel: Number.isFinite(entry.duration) ? `${entry.duration}ms` : "",
    responseSizeLabel: payloadSize > 1024 ? `${(payloadSize / 1024).toFixed(2)} KB` : `${payloadSize} B`,
    requestRows: buildPublisherRequestRows(requestBar, endpointType, endpointConfig, values),
    requestHeaders: entry.metadata.map((row) => [row.k, row.v]),
    responseHeaders,
    responsePayload: payloadString,
  };
}

function requestValuesFromHistoryEntry(entry: PublisherHistoryEntry, fields: readonly RequestBarFieldDescriptor[]) {
  const values: Record<string, string> = {};
  for (const field of fields) {
    values[field.inputId] = entry.requestMetadata[`request_bar.${field.field}`]
      || entry.requestMetadata[`request_bar.${field.inputId}`]
      || entry.request_fields[field.field]
      || "";
  }
  if (!values["pub-url"]) {
    values["pub-url"] = entry.requestMetadata["request_bar.url"]
      || entry.requestMetadata["request_bar.pub-url"]
      || entry.request_fields.url
      || entry.url
      || "";
  }
  return values;
}

function buildPublishRequestDetails(publisher: PublisherConfig) {
  const { endpointType, endpointConfig, requestBar, values } = currentRequestBarValues(publisher);
  const metadataRows = get(publishersPanelState).metadataRows
    .filter((row) => row.enabled && row.key.trim())
    .map((row) => ({ k: row.key.trim(), v: row.value }));
  const requestMetadata: Record<string, string> = {};
  const requestFields: Record<string, string> = {};

  for (const field of requestBar.fields || []) {
    const value = String(values[field.inputId] || "").trim();
    if (!value) continue;
    requestMetadata[`request_bar.${field.field}`] = value;
    requestMetadata[`request_bar.${field.inputId}`] = value;
    requestFields[field.field] = value;
  }

  if (endpointType === "http") {
    const method = String(endpointConfig.method || "POST").trim();
    if (method) {
      requestMetadata.http_method = method;
    }
    const requestUrl = String(values["pub-url"] || "").trim();
    if (requestUrl) {
      const pathAndQuery = parsePathAndQuery(requestUrl);
      if (pathAndQuery.path) {
        requestMetadata.http_path = pathAndQuery.path;
      }
      if (pathAndQuery.query) {
        requestMetadata.http_query = pathAndQuery.query;
      }
    }
  }

  const metadata = Object.fromEntries(metadataRows.map(({ k, v }) => [k, v]));
  Object.assign(metadata, Object.fromEntries(
    Object.entries(requestMetadata).filter(([key]) =>
      key === "http_method" || key === "http_path" || key === "http_query"),
  ));

  const requestRows = buildPublisherRequestRows(requestBar, endpointType, endpointConfig, values);
  const request: PublishRequest = {
    name: String(publisher.name || ""),
    publisher_id: publisher.id || null,
    payload: String(publisher.payload || ""),
    metadata,
    endpoint: deepClone(publisher.endpoint) as any,
  };

  return {
    request,
    requestRows,
    requestHeaders: metadataRows,
    historyMetadata: requestMetadata,
  };
}

function buildPublisherRequestRows(
  requestBar: { fields?: readonly RequestBarFieldDescriptor[]; showMethod?: boolean },
  endpointType: string,
  endpointConfig: Record<string, unknown>,
  values: Record<string, string>,
) {
  const requestRows: Array<[string, string]> = [];
  if (requestBar.showMethod) {
    const method = String(endpointConfig.method || "POST").trim();
    if (method) requestRows.push(["Method", method]);
  }
  for (const field of requestBar.fields || []) {
    const value = String(values[field.inputId] || "").trim();
    if (!value) continue;
    requestRows.push([field.label || field.field, value]);
  }
  if (endpointType === "http" && requestRows.every(([label]) => label !== "URL")) {
    const url = String(values["pub-url"] || "").trim();
    if (url) requestRows.push(["URL", url]);
  }
  return requestRows;
}

function setPublisherResponsePending(publisher: PublisherConfig) {
  const nextResponseState: PublisherResponseState = {
    responseVisible: true,
    responseTabLabel: "Response",
    responseStatusLabel: "Sending",
    responseStatusText: "",
    responseStatusColor: "var(--text-primary)",
    responseDurationLabel: "",
    responseSizeLabel: "",
    requestRows: [],
    requestHeaders: [],
    responseHeaders: [],
    responsePayload: "Sending...",
  };
  setPublisherResponseState(publisher, nextResponseState);
  publishersPanelState.update((state) => ({
    ...state,
    ...nextResponseState,
  }));
}

function parseJsonIfPossible(value: string) {
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

function parsePathAndQuery(value: string) {
  try {
    const parsed = new URL(value);
    return {
      path: parsed.pathname || "/",
      query: parsed.search.length > 1 ? parsed.search.slice(1) : "",
    };
  } catch {
    const slashIndex = value.indexOf("/", value.indexOf("//") + 2);
    if (slashIndex >= 0) {
      const pathWithQuery = value.slice(slashIndex);
      const [path, query] = pathWithQuery.split("?");
      return { path: path || "/", query: query || "" };
    }
    return { path: "/", query: "" };
  }
}

function applyPublisherResponseState(
  publisher: PublisherConfig,
  response: Response,
  duration: number,
  responseData: unknown,
  requestDetails: {
    requestRows: Array<[string, string]>;
    requestHeaders: Array<{ k: string; v: string }>;
  },
) {
  const statusOk = response.ok;
  const statusLabel = statusOk ? String(response.status || "OK") : "Error";
  const statusText = response.statusText || "";
  const responsePayload = extractPublisherResponsePayload(responseData);
  const responseHeaders = extractPublisherResponseHeaders(responseData);
  const payloadString = typeof responsePayload === "string"
    ? responsePayload
    : JSON.stringify(responsePayload ?? "", null, 2);
  const payloadSize = new TextEncoder().encode(payloadString).length;

  const nextResponseState: PublisherResponseState = {
    responseVisible: true,
    responseTabLabel: `Response ✓ ${statusLabel}`,
    responseStatusLabel: statusLabel,
    responseStatusText: statusText,
    responseStatusColor: statusOk ? "var(--accent-http)" : "var(--accent-kafka)",
    responseDurationLabel: `${duration}ms`,
    responseSizeLabel: payloadSize > 1024 ? `${(payloadSize / 1024).toFixed(2)} KB` : `${payloadSize} B`,
    requestRows: requestDetails.requestRows,
    requestHeaders: requestDetails.requestHeaders.map(({ k, v }) => [k, v]),
    responseHeaders,
    responsePayload: payloadString,
  };

  setPublisherResponseState(publisher, nextResponseState);
  publishersPanelState.update((state) => ({
    ...state,
    ...nextResponseState,
  }));
}

function getPublisherResponseState(publisher: PublisherConfig): PublisherResponseState | undefined {
  for (const key of publisherStorageKeysFor(publisher)) {
    const responseState = responseStateByPublisher[key];
    if (responseState) return responseState;
  }
  return undefined;
}

function setPublisherResponseState(publisher: PublisherConfig, responseState: PublisherResponseState) {
  for (const key of publisherStorageKeysFor(publisher)) {
    responseStateByPublisher[key] = responseState;
  }
}

function extractPublisherResponseHeaders(responseData: unknown): Array<[string, string]> {
  if (!responseData || typeof responseData !== "object" || Array.isArray(responseData)) return [];
  const metadata = (responseData as Record<string, unknown>).metadata;
  if (!metadata || typeof metadata !== "object" || Array.isArray(metadata)) return [];
  // The backend serialises metadata from a hash map, so the order differs per response; sorting
  // here keeps the response and history header tables stable between clicks.
  return Object.entries(metadata)
    .map(([key, value]) => [key, String(value ?? "")] as [string, string])
    .sort(([leftKey], [rightKey]) => leftKey.localeCompare(rightKey));
}

function extractPublisherResponsePayload(responseData: unknown) {
  if (!responseData || typeof responseData !== "object" || Array.isArray(responseData)) {
    return responseData;
  }
  const status = String((responseData as Record<string, unknown>).status || "");
  if (status === "Ack") {
    return "// ACKNOWLEDGED: The backend processed the message successfully.";
  }
  if ("payload" in (responseData as Record<string, unknown>)) {
    return (responseData as Record<string, unknown>).payload ?? "";
  }
  return responseData;
}

function syncConsumerPublisherReferences(previousPublisherName?: string, previousPublisherId?: string) {
  const publishersById = new Map(
    activeConfig.publishers
      .map((publisher) => [String(publisher.id || "").trim(), publisher] as const)
      .filter(([id]) => id.length > 0),
  );
  const publishersByName = new Map<string, PublisherConfig>();
  for (const publisher of activeConfig.publishers) {
    const name = String(publisher.name || "").trim();
    if (!name) continue;
    if (publishersByName.has(name)) {
      publishersByName.delete(name);
      continue;
    }
    publishersByName.set(name, publisher);
  }
  for (const consumer of activeConfig.consumers || []) {
    if (consumer.output?.mode !== "publisher") continue;
    const output = consumer.output as any;
    const outputPublisherId = String(output.publisher_id || "").trim();
    let publisher = publishersById.get(outputPublisherId);
    if (!publisher) {
      publisher = publishersByName.get(String(output.publisher || "").trim());
    }
    if (!publisher && previousPublisherId && outputPublisherId === previousPublisherId) {
      publisher = publishersById.get(previousPublisherId);
    }
    if (!publisher && previousPublisherName && String(output.publisher || "").trim() === previousPublisherName.trim()) {
      publisher = publishersById.get(String(previousPublisherId || "").trim()) || publishersByName.get(previousPublisherName.trim());
    }
    if (!publisher) continue;
    output.publisher = String(publisher.name || "");
    output.publisher_id = publisher.id;
  }
}

function scheduleHistorySync() {
  if (historySyncTimer != null) window.clearTimeout(historySyncTimer);
  historySyncTimer = window.setTimeout(() => {
    void browserWindow().saveConfigSection?.("history", historyStore, true, undefined);
  }, 2000) as unknown as number;
}

async function flushPendingFormDraft() {
  const activeElement = document.activeElement as { blur?: () => void } | null;
  if (activeElement?.blur && activeElement !== document.body) {
    activeElement.blur();
    await Promise.resolve();
    await Promise.resolve();
  }
  const idx = get(publishersPanelState).selectedIndex;
  const nameInput = document.querySelector<HTMLInputElement | HTMLTextAreaElement>(
    '#pub-config-form input[id$=".name"], #pub-config-form input[name$="[name]"], #pub-config-form input[name="name"], #pub-config-form textarea[id$=".name"], #pub-config-form textarea[name$="[name]"], #pub-config-form textarea[name="name"]',
  );
  if (nameInput) {
    const existingDraft = formDrafts.get(idx) || currentPublisher() || {};
    formDrafts.set(idx, { ...deepClone(existingDraft as PublisherConfig), name: nameInput.value } as PublisherConfig);
  }
}

function composeHttpRequestUrl(endpointConfig: Record<string, unknown>) {
  const url = String(endpointConfig.url || "");
  const rawPath = String(endpointConfig.path || "");
  if (!rawPath || rawPath === "/") {
    return url;
  }
  if (url.endsWith(rawPath)) {
    return url;
  }
  const normalizedPath = rawPath.startsWith("/") ? rawPath : `/${rawPath}`;
  return `${url.replace(/\/+$/, "")}${normalizedPath}`;
}
