import { tick } from "svelte";
import { get } from "svelte/store";
import { appShell, getAppState, switchMainTab, workspaceRuntime } from "./app-shell";
import { initSchemaForm } from "./forms/form-cache";
import { browserWindow, replaceHash } from "./browser";
import { createLocalEntityId, getEntityDisplayLabel, normalizeConsumerNames, normalizeConsumerResponse, sanitizeConsumerName } from "./utils";
import { CONSUMER_TYPE_OPTIONS, RESPONSE_CAPABLE_CONSUMER_TYPES, formatEndpointTypeLabel } from "./endpoint-metadata";
import { createDefaultEndpoint, createPublisherEndpointFromConsumerEndpoint, ensureEndpointDefaults, getEndpointType, normalizeScalarEndpointValue } from "./endpoint-utils";
import { buildConsumerTree } from "./consumer-grouping";
import { consumersPanelState } from "./stores";
import { buildConsumerConfigDocument, buildConsumerPublisherDocument, extractImportedRequests, type ConfigJsonVariant } from "./import-export";
import { forceRefOnlyEndpoints, hideUnusedDeprecatedProperties, nameMappingRuleBranches, resolveRootArrayItemSchema } from "./schema-utils";
import { applyEndpointSchemaDefaults } from "./routes";
import { getStoredJson, setStoredJson } from "./encrypted-json-storage";
import { EMPTY_STORAGE_SECURITY } from "./storage-security";
import type {
  ConsumerConfig,
  ConsumerMessage,
  ConsumerMessageCaptureConfig,
  ConsumerOutputConfig,
  ConsumerResponseConfig,
  ConsumerResponseHeaderRow,
  ConsumerStatus,
  ConsumersAppConfig,
  ConsumersSchemaRoot,
  PublisherConfig,
} from "./panel-types";

const MESSAGE_STORAGE_KEY = "mqb_consumer_messages";
const DEFAULT_CAPTURE: Required<ConsumerMessageCaptureConfig> = { enabled: true, keep_last: 100 };
const EMPTY_STATUS: ConsumerStatus = { running: false, status: { healthy: false } } as ConsumerStatus;
const CONSUMER_ENDPOINT_DEFAULTS: Record<string, Record<string, unknown> | string> = {
  http: { url: "0.0.0.0:8080", path: "/api/messages", method: "POST" },
  websocket: { url: "0.0.0.0:8080", path: "/socket" },
  grpc: { url: "0.0.0.0:50051" },
  nats: { url: "nats://localhost:4222", subject: "events.created" },
  memory: { topic: "events" },
  amqp: { url: "amqp://guest:guest@localhost:5672/%2f", queue: "jobs" },
  kafka: { url: "kafka:9092", topic: "events" },
  pulsar: { url: "pulsar://localhost:6650", topic: "persistent://public/default/events", subscription: "mq-bridge" },
  mqtt: { url: "tcp://localhost:1883", topic: "events/updates" },
  mongodb: { url: "mongodb://localhost:27017", database: "app", collection: "messages" },
  sqlx: { url: "postgres://user:pass@localhost/db", table: "events" },
  zeromq: { url: "tcp://127.0.0.1:5555", topic: "events" },
  redis_streams: { url: "redis://localhost:6379", stream: "events", group: "mq-bridge" },
  ibmmq: { url: "localhost(1414)", queue_manager: "QM1", channel: "DEV.APP.SVRCONN", queue: "DEV.QUEUE.1" },
  file: { path: "/tmp/messages.jsonl" },
  static: "",
};

let activeConfig: ConsumersAppConfig = { consumers: [], routes: {}, publishers: [] };
let activeSchema: ConsumersSchemaRoot = {};
let formDrafts = new Map<number, ConsumerConfig>();
let messagesByConsumer: Record<string, ConsumerMessage[]> = {};
let nextResponseHeaderId = 1;
let pollTimer: number | null = null;
let lastMessageSequenceByConsumer: Record<string, number> = {};
// For a finished (terminal) consumer: the `outcome:sequence` we last captured,
// so it is polled exactly once per terminal state instead of every cycle.
let finalPollByConsumer: Record<string, string> = {};
let consumerErrorByKey: Record<string, string> = {};
let renderedConsumerFormSignature: string | null = null;
const DETAIL_METADATA_ORDER = ["content-length", "host", "http_method", "http_path", "http_query", "http_version", "content-type"];

function formatLocalTimeFromIso(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function formatTimeAgoLabel(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "Select a message to view details";
  const deltaSeconds = Math.max(0, Math.round((Date.now() - date.getTime()) / 1000));
  if (deltaSeconds < 60) return `${deltaSeconds}s ago`;
  const deltaMinutes = Math.round(deltaSeconds / 60);
  if (deltaMinutes < 60) return `${deltaMinutes}m ago`;
  const deltaHours = Math.round(deltaMinutes / 60);
  return `${deltaHours}h ago`;
}

const normalizeEndpointRecord = (endpoint: unknown) => ensureEndpointDefaults(endpoint, normalizeEndpointRecord);

export { CONSUMER_TYPE_OPTIONS, normalizeConsumerNames, sanitizeConsumerName };

function deepClone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

function getRuntimeKey(consumer: ConsumerConfig): string {
  return String(consumer.id || sanitizeConsumerName(consumer.name || "") || "consumer");
}

function getStorageKey(consumer: ConsumerConfig): string {
  return String(consumer.id || consumer.name || "");
}

function getConsumerLookupKeys(consumer: ConsumerConfig): string[] {
  const keys = [getRuntimeKey(consumer), getStorageKey(consumer), String(consumer.name || "")]
    .map((value) => value.trim())
    .filter(Boolean);
  return Array.from(new Set(keys));
}

function getLivePublishers(): PublisherConfig[] {
  const localPublishers = Array.isArray(activeConfig.publishers) ? activeConfig.publishers : [];
  if (localPublishers.length > 0) return localPublishers;
  const livePublishers = (appShell.config<Record<string, unknown>>()?.publishers || []) as PublisherConfig[];
  return Array.isArray(livePublishers) ? livePublishers : [];
}

function createDefaultConsumerEndpoint(endpointType: string): Record<string, unknown> {
  const endpoint = createDefaultEndpoint(endpointType);
  const defaults = CONSUMER_ENDPOINT_DEFAULTS[endpointType];
  if (endpointType === "static") {
    endpoint.static = typeof defaults === "string" ? defaults : "";
    return endpoint;
  }
  Object.assign(endpoint[endpointType] as Record<string, unknown>, defaults ?? {});
  return endpoint;
}

function normalizeMessageCapture(input: ConsumerMessageCaptureConfig | null | undefined): Required<ConsumerMessageCaptureConfig> {
  return {
    enabled: input?.enabled ?? DEFAULT_CAPTURE.enabled,
    keep_last: Number.isFinite(input?.keep_last) ? Math.max(1, Number(input?.keep_last)) : DEFAULT_CAPTURE.keep_last,
  };
}

function normalizeOutput(input: ConsumerOutputConfig | null | undefined): ConsumerOutputConfig {
  if (!input || typeof input !== "object") return { mode: "none" };
  const mode = input.mode;
  if (mode === "publisher") {
    const output: ConsumerOutputConfig = { mode, publisher: String((input as any).publisher || "") };
    const publisherId = String((input as any).publisher_id || "").trim();
    if (publisherId) {
      (output as any).publisher_id = publisherId;
    }
    return output;
  }
  if (mode === "response") return { mode, response: (input as any).response ?? { payload: "", headers: {} } };
  return { mode: "none" };
}

function normalizeConsumerConfig(input: ConsumerConfig): ConsumerConfig {
  const next = deepClone(input);
  next.id = String(next.id || "").trim() || createLocalEntityId("consumer");
  next.name = String(next.name ?? "");
  next.endpoint = normalizeEndpointRecord(next.endpoint);
  next.message_capture = normalizeMessageCapture(next.message_capture);
  next.output = normalizeOutput(next.output);
  next.response = normalizeConsumerResponse(next.response) ?? null;
  return next;
}

function parseStructuredValue(raw: unknown): unknown {
  if (typeof raw !== "string") return raw;
  const trimmed = raw.trim();
  if (!trimmed) return raw;
  if (!trimmed.startsWith("{") && !trimmed.startsWith("[") && !trimmed.startsWith("\"")) {
    return raw;
  }
  try {
    return JSON.parse(trimmed);
  } catch {
    return raw;
  }
}

// Render a (possibly structured) payload for display without mangling raw strings.
// Strings are returned verbatim so invalid JSON like `"test":a` is shown as-is
// instead of being re-escaped by JSON.stringify (`"\"test\":a"`). Only objects
// and arrays are pretty-printed.
function payloadToDisplayString(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function normalizeStringMap(raw: unknown): Record<string, string> | undefined {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return undefined;
  const entries = Object.entries(raw as Record<string, unknown>)
    .map(([key, value]) => [key, String(value ?? "")] as const);
  return entries.length > 0 ? Object.fromEntries(entries) : undefined;
}

function sortDetailMetadataEntries(entries: Array<[string, string]>) {
  return entries
    .sort(([leftKey], [rightKey]) => {
      const leftIndex = DETAIL_METADATA_ORDER.indexOf(leftKey);
      const rightIndex = DETAIL_METADATA_ORDER.indexOf(rightKey);
      if (leftIndex !== -1 || rightIndex !== -1) {
        if (leftIndex === -1) return 1;
        if (rightIndex === -1) return -1;
        return leftIndex - rightIndex;
      }
      return leftKey.localeCompare(rightKey);
    });
}

export function normalizeConsumerMessage(raw: unknown, fallbackTime = new Date().toISOString()): ConsumerMessage {
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    const record = raw as Record<string, unknown>;
    const metadata = normalizeStringMap(record.metadata);
    const responseMetadata = normalizeStringMap(record.response_metadata);
    if ("payload" in record || "id" in record || "time" in record || "response" in record) {
      return {
        id: record.id == null ? undefined : String(record.id),
        payload: "payload" in record ? parseStructuredValue(record.payload) : raw,
        metadata,
        time: typeof record.time === "string" ? record.time : fallbackTime,
        response: parseStructuredValue(record.response),
        response_metadata: responseMetadata,
      };
    }
  }
  return { payload: raw, time: fallbackTime };
}

async function readStoredMessages(): Promise<Record<string, ConsumerMessage[]>> {
  const preloaded = getAppState().storage_cache?.consumer_messages;
  if (preloaded && typeof preloaded === "object" && !Array.isArray(preloaded)) {
    return preloaded as Record<string, ConsumerMessage[]>;
  }
  return await getStoredJson(
    MESSAGE_STORAGE_KEY,
    {},
    getAppState().storage_security || { ...EMPTY_STORAGE_SECURITY },
  );
}

function persistMessages() {
  const state = getAppState();
  state.storage_cache = {
    ...(state.storage_cache || {}),
    consumer_messages: deepClone(messagesByConsumer),
  };
  void setStoredJson(
    MESSAGE_STORAGE_KEY,
    messagesByConsumer,
    state.storage_security || { ...EMPTY_STORAGE_SECURITY },
  );
}

function consumerMessagesFor(consumer: ConsumerConfig): ConsumerMessage[] {
  const byId = messagesByConsumer[getStorageKey(consumer)];
  if (Array.isArray(byId)) return byId;
  const byName = messagesByConsumer[String(consumer.name || "")];
  if (Array.isArray(byName)) return byName;
  return [];
}

function setConsumerMessages(consumer: ConsumerConfig, rows: ConsumerMessage[]) {
  const keepLast = normalizeMessageCapture(consumer.message_capture).keep_last;
  const storageKey = getStorageKey(consumer);
  const legacyNameKey = String(consumer.name || "");
  messagesByConsumer[storageKey] = rows.slice(0, keepLast);
  if (legacyNameKey && legacyNameKey !== storageKey) {
    delete messagesByConsumer[legacyNameKey];
  }
  persistMessages();
}

function getConsumerStatus(consumer: ConsumerConfig): ConsumerStatus {
  const runtime = (browserWindow()._mqb_runtime_status?.consumers || {}) as Record<string, ConsumerStatus>;
  return runtime[getRuntimeKey(consumer)] || EMPTY_STATUS;
}

function formatThroughput(consumer: ConsumerConfig): string {
  const status = getConsumerStatus(consumer) as any;
  const throughput = Number(status?.throughput || 0);
  if (!status?.running || throughput <= 0) return "";
  return `${throughput.toFixed(1)} msg/s`;
}

function getStatusClass(consumer: ConsumerConfig): string {
  if (consumerErrorByKey[getRuntimeKey(consumer)]) return "status-error";
  const status = getConsumerStatus(consumer);
  // A route that ended by itself is not running; only a failure is an error,
  // so a clean drain does not raise a red dot.
  if (status.outcome === "failed") return "status-error";
  if (!status.running) return "status-off";
  if (status.status?.healthy === false) return "status-error";
  return "status-ok";
}

// Label for a route that has ended, or "" while it is still running.
function finishedStatusText(status: ConsumerStatus): string {
  switch (status.outcome) {
    case "completed":
      return "Completed";
    case "failed":
      return status.status?.error || "Consumer Failed";
    case "stopped":
      return "Consumer Stopped";
    default:
      return "";
  }
}

function buildSidebarItems() {
  return activeConfig.consumers.map((consumer, index) => ({
    name: String(consumer.name || ""),
    displayName: getEntityDisplayLabel(consumer.name, consumer.endpoint, getEndpointType(consumer.endpoint)),
    inputProto: formatEndpointTypeLabel(getEndpointType(consumer.endpoint)),
    statusClass: getStatusClass(consumer),
    messageCount: consumerMessagesFor(consumer).length,
    throughputLabel: formatThroughput(consumer),
    originalIndex: index,
    id: consumer.id,
  }));
}

function buildGroupedSidebarItems(items = buildSidebarItems()) {
  return buildConsumerTree(activeConfig.consumers, items);
}

function currentConsumer(): ConsumerConfig | null {
  const idx = get(consumersPanelState).selectedIndex;
  if (idx < 0 || idx >= activeConfig.consumers.length) return null;
  return activeConfig.consumers[idx];
}

function nextPublisherOptions() {
  const publishers = getLivePublishers();
  const nameCounts = publishers.reduce((counts, publisher) => {
    const name = String(publisher.name || "").trim();
    if (name) counts.set(name, (counts.get(name) || 0) + 1);
    return counts;
  }, new Map<string, number>());

  return publishers.map((publisher, index) => {
    const name = String(publisher.name || "").trim();
    const id = String(publisher.id || "").trim() || createLocalEntityId("publisher");
    publisher.id = id;
    const value = id;
    const label = name && nameCounts.get(name) === 1
      ? name
      : getEntityDisplayLabel("", publisher.endpoint, getEndpointType(publisher.endpoint));
    return { value, label };
  });
}

function findPublisherByOutputValue(value: string): PublisherConfig | undefined {
  if (!value.trim()) return undefined;
  const publishers = getLivePublishers();
  return publishers.find((publisher) => String(publisher.id || "").trim() === value)
    || publishers.find((publisher) => String(publisher.name || "").trim() === value);
}

function getSelectedPublisherValue(output: ConsumerOutputConfig | null | undefined): string {
  if (output?.mode !== "publisher") return "";
  const publisherId = String((output as any).publisher_id || "").trim();
  if (publisherId && findPublisherByOutputValue(publisherId)) return publisherId;
  return String((output as any).publisher || "").trim();
}

function createConsumerPublisherOutput(value: string): ConsumerOutputConfig {
  const publisher = findPublisherByOutputValue(value);
  const publisherName = String(publisher?.name || (publisher ? "" : value)).trim();
  const output: ConsumerOutputConfig = { mode: "publisher", publisher: publisherName };
  const publisherId = String(publisher?.id || "").trim();
  if (publisherId) {
    (output as any).publisher_id = publisherId;
  }
  return output;
}

function responseRowsFromConsumer(consumer: ConsumerConfig): ConsumerResponseHeaderRow[] {
  const response = normalizeConsumerResponse(consumer.response) ?? { headers: {}, payload: "" };
  return Object.entries(response.headers || {}).map(([key, value]) => ({
    id: nextResponseHeaderId++,
    key,
    value,
    enabled: true,
  }));
}

function headersFromResponseRows(rows: ConsumerResponseHeaderRow[]): Record<string, string> {
  return Object.fromEntries(
    rows.filter((row) => row.enabled && row.key.trim()).map((row) => [row.key.trim(), row.value]),
  );
}

// The rows are the live editing surface, so rebuilding them costs an edit in
// progress: fresh ids re-create the keyed inputs (dropping focus) and a row that
// is still blank or disabled is not in the consumer at all. Only rebuild when the
// stored headers no longer match what the rows already express. The comparison
// runs against the raw headers the rows were written to, not the normalized view,
// which drops a header whose value has not been typed yet.
function responseRowsMatchConsumer(rows: ConsumerResponseHeaderRow[], consumer: ConsumerConfig): boolean {
  const stored = (consumer.response as { headers?: Record<string, string> } | undefined)?.headers;
  if (!stored || typeof stored !== "object") return rows.length === 0;
  const fromRows = headersFromResponseRows(rows);
  const keys = Object.keys(stored);
  return keys.length === Object.keys(fromRows).length && keys.every((key) => fromRows[key] === stored[key]);
}

function applyResponseStateToConsumer(consumer: ConsumerConfig) {
  const state = get(consumersPanelState);
  const enabledHeaders = headersFromResponseRows(state.responseHeaders);
  const nextResponse = { payload: state.responsePayload, headers: enabledHeaders };
  consumer.response = nextResponse;
  consumer.output = { mode: "response", response: nextResponse };
  refreshConsumerDirty();
}

function refreshConsumerDirty() {
  browserWindow().refreshDirtySection?.("consumers");
}

function renderMessageDetails() {
  const consumer = currentConsumer();
  if (!consumer) return;
  const rows = consumerMessagesFor(consumer);
  const selected = rows[get(consumersPanelState).selectedMessageIndex] || null;
  consumersPanelState.update((state) => ({
    ...state,
    detailInfo: selected ? formatTimeAgoLabel(selected.time || new Date().toISOString()) : "Select a message to view details",
    detailRequestPayload: selected ? payloadToDisplayString(selected.payload) : "",
    detailRequestHeaders: selected ? sortDetailMetadataEntries(Object.entries(selected.metadata || {})) : [],
    detailResponsePayload: selected && selected.response !== undefined ? payloadToDisplayString(selected.response) : "",
    detailResponseHeaders: selected ? sortDetailMetadataEntries(Object.entries(selected.response_metadata || {})) : [],
    detailRequestContentType: selected?.metadata?.["content-type"] || "",
    detailResponseContentType: selected?.response_metadata?.["content-type"] || "",
    hasResponse: Boolean(selected?.response !== undefined),
  }));
}

function renderSelectedConsumer() {
  const consumer = currentConsumer();
  const items = buildSidebarItems();
  if (!consumer) {
    consumersPanelState.update((state) => ({
      ...state,
      hasConsumers: activeConfig.consumers.length > 0,
      items,
      groupedItems: buildGroupedSidebarItems(items),
      currentConsumerKey: null,
    }));
    return;
  }

  const capture = normalizeMessageCapture(consumer.message_capture);
  const output = normalizeOutput(consumer.output);
  const rows = consumerMessagesFor(consumer);
  const status = getConsumerStatus(consumer);
  const runtimeKey = getRuntimeKey(consumer);
  const runtimeError = consumerErrorByKey[runtimeKey];
  const responseCapable = RESPONSE_CAPABLE_CONSUMER_TYPES.has(getEndpointType(consumer.endpoint));
  const publisherOptions = nextPublisherOptions();
  const responseValue = normalizeConsumerResponse(consumer.response) ?? { headers: {}, payload: "" };

  consumersPanelState.update((state) => ({
    ...state,
    hasConsumers: activeConfig.consumers.length > 0,
    items,
    groupedItems: buildGroupedSidebarItems(items),
    currentConsumerKey: getRuntimeKey(consumer),
    selectedIndex: activeConfig.consumers.indexOf(consumer),
    isNew: !isSavedConsumer(consumer),
    deleteLabel: "Delete",
    messageCaptureEnabled: capture.enabled,
    messageCaptureKeepLast: capture.keep_last,
    outputMode: (output.mode as any) || "none",
    publisherOptions,
    selectedPublisher: getSelectedPublisherValue(output),
    responseSupported: responseCapable,
    responseHeaders: state.currentConsumerKey === runtimeKey && responseRowsMatchConsumer(state.responseHeaders, consumer)
      ? state.responseHeaders
      : responseRowsFromConsumer(consumer),
    responsePayload: responseValue.payload || "",
    liveStatusText: runtimeError || finishedStatusText(status) || (!status.running ? "Consumer Stopped" : status.status?.healthy === false ? (status.status?.error || "Consumer Error") : "Connected"),
    liveStatusVariant: runtimeError || status.outcome === "failed" ? "danger" : !status.running ? "neutral" : status.status?.healthy === false ? "danger" : "success",
    toggleLabel: !status.running ? "Start" : "Stop",
    toggleVariant: !status.running ? "success" : "danger",
    toggleBusy: false,
      messages: rows.map((message, index) => ({
        timeLabel: formatLocalTimeFromIso(message.time || new Date().toISOString()),
        payloadPreview: typeof message.payload === "string" ? message.payload : JSON.stringify(message.payload),
        messageIndex: index,
        selected: index === state.selectedMessageIndex,
      })),
  }));
  renderMessageDetails();
}

function markRememberedSelection(idx: number, tab: "definition" | "response" | "messages") {
  const appState = getAppState();
  appState.last_consumer_idx = idx;
  appState.last_consumer_tab = tab;
  browserWindow()._mqb_last_consumer_idx = idx;
  browserWindow()._mqb_last_consumer_tab = tab;
}

function isSavedConsumer(consumer: ConsumerConfig): boolean {
  const saved = (browserWindow()._mqb_saved_sections?.consumers || []) as ConsumerConfig[];
  return saved.some((item) => String(item.id || "") === String(consumer.id || ""));
}

function hasUnsavedConsumers(): boolean {
  if (!browserWindow()._mqb_saved_sections || !("consumers" in browserWindow()._mqb_saved_sections)) {
    return false;
  }
  const saved = JSON.stringify((browserWindow()._mqb_saved_sections?.consumers || []) as ConsumerConfig[]);
  const current = JSON.stringify(activeConfig.consumers);
  return saved !== current;
}

async function saveConsumersSection(sectionValue = activeConfig.consumers) {
  const saved = browserWindow().saveConfigSection
    ? await browserWindow().saveConfigSection("consumers", sectionValue, false, undefined)
    : await workspaceRuntime.saveConfigSection("consumers", sectionValue, false, undefined);
  if (saved && Array.isArray((saved as any).consumers)) {
    activeConfig.consumers = (saved as any).consumers.map(normalizeConsumerConfig);
    browserWindow()._mqb_saved_sections = {
      ...(browserWindow()._mqb_saved_sections || {}),
      consumers: deepClone(activeConfig.consumers),
    };
  }
  return saved;
}

async function flushPendingFormDraft() {
  const activeElement = document.activeElement as { blur?: () => void } | null;
  if (activeElement?.blur && activeElement !== document.body) {
    activeElement.blur();
    await Promise.resolve();
    await Promise.resolve();
  }
}

function createConsumerFormSchema(consumer: ConsumerConfig): Record<string, unknown> {
  const itemSchema = resolveRootArrayItemSchema(activeSchema as Record<string, any>, "consumers");
  applyEndpointSchemaDefaults(itemSchema as any);
  forceRefOnlyEndpoints(itemSchema as any);
  nameMappingRuleBranches(itemSchema as any);
  hideUnusedDeprecatedProperties(itemSchema as any, consumer);
  return itemSchema;
}

async function renderConsumerForm() {
  const consumer = currentConsumer();
  const container = document.getElementById("cons-config-form");
  if (!consumer || !container) return;
  // Rebuilding the schema form dominates the cost of a tab switch, and the panels stay mounted,
  // so the already-rendered form is reused whenever it still matches the selected consumer.
  const signature = JSON.stringify(consumer);
  if (container.childElementCount > 0 && renderedConsumerFormSignature === signature) return;
  renderedConsumerFormSignature = signature;
  const forms = appShell.forms() as any;
  getAppState().form_mode = "consumer";
  (window as any)._mqb_form_mode = "consumer";
  await initSchemaForm(forms, container, createConsumerFormSchema(consumer), deepClone(consumer), (updated: ConsumerConfig) => {
    formDrafts.set(get(consumersPanelState).selectedIndex, updated);
    const current = currentConsumer();
    if (!current) return;
    const normalized = normalizeConsumerConfig({ ...current, ...updated });
    normalized.output = current.output;
    normalized.message_capture = current.message_capture;
    normalized.response = current.response;
    Object.assign(current, normalized);
    renderedConsumerFormSignature = JSON.stringify(current);
    refreshConsumerDirty();
    renderSelectedConsumer();
  });
}

async function fetchNewMessagesForRunningConsumers() {
  const runtimeConsumers = (browserWindow()._mqb_runtime_status?.consumers || {}) as Record<string, any>;
  for (const consumer of activeConfig.consumers) {
    const runtimeKey = getRuntimeKey(consumer);
    const runtime = runtimeConsumers[runtimeKey];
    // A route that just finished is no longer running but may still hold the
    // tail of its capture, so it gets one more poll: its sequence has stopped
    // advancing, so the guard below settles it after that fetch.
    if (!runtime?.running && !runtime?.outcome) continue;
    const capture = normalizeMessageCapture(consumer.message_capture);
    if (!capture.enabled) continue;
    const messageSequence = Number(runtime.message_sequence || 0);
    // A terminal route (finished, not running) is polled once per
    // `outcome:sequence`. Its sequence has stopped advancing, so without this a
    // zero-message capture — where `lastSeen` and stored rows both stay 0 — would
    // re-fetch /messages every cycle forever, never settling via the guard below.
    // A new outcome or a higher sequence makes the key differ and polls again.
    const terminal = !runtime.running && runtime.outcome;
    const terminalKey = `${runtime.outcome}:${messageSequence}`;
    if (terminal && finalPollByConsumer[runtimeKey] === terminalKey) continue;
    const lastSeen = Number(lastMessageSequenceByConsumer[runtimeKey] || 0);
    if (!terminal && messageSequence <= lastSeen && consumerMessagesFor(consumer).length > 0) continue;
    const response = await fetch(`/messages?consumer_id=${encodeURIComponent(runtimeKey)}`);
    if (!response.ok) continue;
    const payload = await response.json();
    const rawRows = getConsumerLookupKeys(consumer)
      .map((key) => payload?.[key])
      .find((value) => Array.isArray(value)) || [];
    const existing = consumerMessagesFor(consumer);
    const merged = rawRows.map((row: unknown) => normalizeConsumerMessage(row)).concat(existing);
    messagesByConsumer[getStorageKey(consumer)] = merged.slice(0, capture.keep_last);
    lastMessageSequenceByConsumer[runtimeKey] = messageSequence;
    // Record the terminal capture only after it lands, so a failed fetch retries.
    if (terminal) finalPollByConsumer[runtimeKey] = terminalKey;
    persistMessages();
  }
}

function scheduleMessagePolling() {
  if (pollTimer != null) {
    window.clearTimeout(pollTimer);
  }
  pollTimer = window.setTimeout(async () => {
    try {
      await fetchNewMessagesForRunningConsumers();
    } catch (error) {
      console.error("Failed to refresh consumer messages", error);
    } finally {
      renderSelectedConsumer();
      scheduleMessagePolling();
    }
  }, 2000) as unknown as number;
}

export async function restoreConsumerStateFromView(idx: number, options?: { tab?: string }) {
  const clamped = Math.min(Math.max(idx, 0), Math.max(activeConfig.consumers.length - 1, 0));
  const nextTab = (options?.tab as "definition" | "response" | "messages" | undefined) || get(consumersPanelState).activeSubtab || "messages";
  markRememberedSelection(clamped, nextTab);
  consumersPanelState.update((state) => ({ ...state, selectedIndex: clamped, selectedMessageIndex: -1, activeSubtab: nextTab }));
  replaceHash(`#consumers:${clamped}`);
  renderSelectedConsumer();
  await renderConsumerForm();
  renderSelectedConsumer();
}

export function selectConsumerSubtab(tab: "definition" | "response" | "messages") {
  consumersPanelState.update((state) => ({ ...state, activeSubtab: tab }));
  markRememberedSelection(get(consumersPanelState).selectedIndex, tab);
  renderSelectedConsumer();
}

export async function addConsumerAction(endpointType: string) {
  activeConfig.consumers.push(normalizeConsumerConfig({
    id: createLocalEntityId("consumer"),
    name: "",
    endpoint: createDefaultConsumerEndpoint(endpointType),
    response: null,
    output: { mode: "none" },
    message_capture: { ...DEFAULT_CAPTURE },
  } as ConsumerConfig));
  await restoreConsumerStateFromView(activeConfig.consumers.length - 1, { tab: "definition" });
}

export function setConsumerOutputModeAction(mode: "none" | "publisher" | "response") {
  const consumer = currentConsumer();
  if (!consumer) return;
  if (mode === "publisher") {
    const options = nextPublisherOptions();
    if (options.length === 0) {
      void browserWindow().mqbAlert?.("Create or select a publisher first.");
      consumer.output = { mode: "none" };
      renderSelectedConsumer();
      return;
    }
    const selectedPublisher = options.length === 1 ? options[0].value : getSelectedPublisherValue(consumer.output);
    consumer.output = createConsumerPublisherOutput(selectedPublisher);
    consumersPanelState.update((state) => ({ ...state, outputMode: "publisher", publisherOptions: options, selectedPublisher }));
    refreshConsumerDirty();
    return;
  }
  if (mode === "response") {
    applyResponseStateToConsumer(consumer);
    consumersPanelState.update((state) => ({ ...state, outputMode: "response" }));
    return;
  }
  consumer.output = { mode: "none" };
  consumersPanelState.update((state) => ({ ...state, outputMode: "none", selectedPublisher: "" }));
  refreshConsumerDirty();
}

export function setConsumerOutputPublisherAction(publisher: string) {
  const consumer = currentConsumer();
  if (!consumer) return;
  consumer.output = createConsumerPublisherOutput(publisher);
  consumersPanelState.update((state) => ({ ...state, selectedPublisher: publisher }));
  refreshConsumerDirty();
}

export function setConsumerMessageCaptureEnabledAction(enabled: boolean) {
  const consumer = currentConsumer();
  if (!consumer) return;
  consumer.message_capture = { ...normalizeMessageCapture(consumer.message_capture), enabled };
  consumersPanelState.update((state) => ({ ...state, messageCaptureEnabled: enabled }));
  refreshConsumerDirty();
}

export function setConsumerMessageCaptureKeepLastAction(keepLast: number) {
  const consumer = currentConsumer();
  if (!consumer) return;
  const numericKeepLast = Number.isFinite(keepLast) ? keepLast : 1;
  const nextKeepLast = Math.max(1, numericKeepLast);
  consumer.message_capture = { ...normalizeMessageCapture(consumer.message_capture), keep_last: nextKeepLast };
  const key = getStorageKey(consumer);
  const legacyNameKey = String(consumer.name || "");
  messagesByConsumer[key] = consumerMessagesFor(consumer).slice(0, nextKeepLast);
  if (legacyNameKey && legacyNameKey !== key) {
    delete messagesByConsumer[legacyNameKey];
  }
  persistMessages();
  consumersPanelState.update((state) => ({ ...state, messageCaptureKeepLast: nextKeepLast }));
  refreshConsumerDirty();
}

export function addConsumerResponseHeader() {
  consumersPanelState.update((state) => ({
    ...state,
    responseHeaders: [...state.responseHeaders, { id: nextResponseHeaderId++, key: "", value: "", enabled: true }],
  }));
  const consumer = currentConsumer();
  if (consumer) applyResponseStateToConsumer(consumer);
}

export function updateConsumerResponseHeader(index: number, field: "key" | "value", value: string) {
  consumersPanelState.update((state) => {
    const responseHeaders = state.responseHeaders.map((row, rowIndex) => rowIndex === index ? { ...row, [field]: value } : row);
    return { ...state, responseHeaders };
  });
  const consumer = currentConsumer();
  if (consumer) applyResponseStateToConsumer(consumer);
}

export function toggleConsumerResponseHeader(index: number, enabled: boolean) {
  consumersPanelState.update((state) => ({
    ...state,
    responseHeaders: state.responseHeaders.map((row, rowIndex) => rowIndex === index ? { ...row, enabled } : row),
  }));
  const consumer = currentConsumer();
  if (consumer) applyResponseStateToConsumer(consumer);
}

export function removeConsumerResponseHeader(index: number) {
  consumersPanelState.update((state) => ({
    ...state,
    responseHeaders: state.responseHeaders.filter((_, rowIndex) => rowIndex !== index),
  }));
  const consumer = currentConsumer();
  if (consumer) applyResponseStateToConsumer(consumer);
}

export function updateConsumerResponsePayload(value: string) {
  consumersPanelState.update((state) => ({ ...state, responsePayload: value }));
  const consumer = currentConsumer();
  if (consumer) applyResponseStateToConsumer(consumer);
}

export function showConsumerMessageDetails(_name: string, msgIdx: number) {
  consumersPanelState.update((state) => ({ ...state, selectedMessageIndex: msgIdx }));
  renderSelectedConsumer();
}

export function clearActiveConsumerHistory() {
  const consumer = currentConsumer();
  if (!consumer) return;
  messagesByConsumer[getStorageKey(consumer)] = [];
  consumersPanelState.update((state) => ({ ...state, selectedMessageIndex: -1 }));
  persistMessages();
  renderSelectedConsumer();
}

export async function copyCurrentConsumerAction() {
  const consumer = currentConsumer();
  if (!consumer) return;
  const publisherName = await browserWindow().mqbPrompt?.("Choose a name for the publisher.", "Copy Consumer to Publisher");
  if (!publisherName) return;
  const publishers = Array.isArray(activeConfig.publishers) ? activeConfig.publishers : (activeConfig.publishers = []);
  publishers.push({
    id: createLocalEntityId("publisher"),
    name: publisherName,
    endpoint: createPublisherEndpointFromConsumerEndpoint(consumer.endpoint),
    headers: [],
  } as PublisherConfig);
  browserWindow().refreshDirtySection?.("publishers");
  getAppState().pending_publisher_restore = { idx: publishers.length - 1, tab: "definition" };
  await switchMainTab("publishers");
}

export async function cloneCurrentConsumerAction() {
  const consumer = currentConsumer();
  if (!consumer) return;
  const existingNames = new Set(activeConfig.consumers.map((item) => String(item.name || "")));
  let nextName = `${sanitizeConsumerName(String(consumer.name || "consumer"))}_copy`;
  let suffix = 1;
  while (existingNames.has(nextName)) {
    nextName = `${sanitizeConsumerName(String(consumer.name || "consumer"))}_copy_${suffix++}`;
  }
  const cloned = normalizeConsumerConfig(deepClone(consumer));
  cloned.id = createLocalEntityId("consumer");
  cloned.name = nextName;
  activeConfig.consumers.push(cloned);
  refreshConsumerDirty();
  await restoreConsumerStateFromView(activeConfig.consumers.length - 1, { tab: get(consumersPanelState).activeSubtab });
}

export async function currentConsumerConfigVariants(): Promise<ConfigJsonVariant[] | null> {
  const consumer = currentConsumer();
  if (!consumer) return null;
  await flushPendingFormDraft();
  const draft = formDrafts.get(get(consumersPanelState).selectedIndex);
  const exportConsumer = draft
    ? normalizeConsumerConfig({ ...consumer, ...deepClone(draft) })
    : deepClone(consumer);
  exportConsumer.output = consumer.output;
  const endpointType = getEndpointType(exportConsumer.endpoint);
  if (endpointType === "static" || endpointType === "ref") {
    exportConsumer.endpoint[endpointType] = normalizeScalarEndpointValue(endpointType, exportConsumer.endpoint[endpointType]);
  }
  const record = exportConsumer as unknown as Record<string, unknown>;
  const publishers = (activeConfig.publishers ?? []) as unknown as Array<Record<string, unknown>>;
  const { id: _id, ...raw } = record;
  return [
    { id: "raw", label: "App config", value: raw, editable: true },
    {
      id: "publisher",
      label: "Publisher.from_config",
      value: buildConsumerPublisherDocument(record),
    },
    {
      id: "route",
      label: "Route.from_config",
      value: buildConsumerConfigDocument(record, publishers),
    },
  ];
}

// Replaces the selected consumer with an edited raw config, keeping its id.
export async function applyCurrentConsumerRawConfig(value: Record<string, unknown>) {
  const consumer = currentConsumer();
  if (!consumer) return;
  if (!value.endpoint || typeof value.endpoint !== "object" || Array.isArray(value.endpoint)) {
    throw new Error("The consumer config needs an `endpoint` object.");
  }
  const idx = get(consumersPanelState).selectedIndex;
  activeConfig.consumers[idx] = normalizeConsumerConfig({ ...value, id: consumer.id } as unknown as ConsumerConfig);
  formDrafts.delete(idx);
  renderedConsumerFormSignature = null;
  refreshConsumerDirty();
  await restoreConsumerStateFromView(idx, { tab: get(consumersPanelState).activeSubtab });
}

export async function deleteCurrentConsumerAction() {
  const consumer = currentConsumer();
  if (!consumer) return;
  const confirmed = await browserWindow().mqbConfirm?.(
    `Delete consumer "${consumer.name || "Untitled Consumer"}"?`,
    "Delete Consumer",
  );
  if (!confirmed) return;
  const selectedIndex = get(consumersPanelState).selectedIndex;
  activeConfig.consumers.splice(selectedIndex, 1);
  delete messagesByConsumer[getStorageKey(consumer)];
  persistMessages();
  refreshConsumerDirty();
  if (activeConfig.consumers.length === 0) {
    consumersPanelState.update((state) => ({
      ...state,
      hasConsumers: false,
      items: [],
      groupedItems: [],
      currentConsumerKey: null,
      selectedIndex: -1,
      selectedMessageIndex: -1,
      messages: [],
    }));
    replaceHash("#consumers:0");
    return;
  }
  const nextIndex = Math.min(selectedIndex, activeConfig.consumers.length - 1);
  await restoreConsumerStateFromView(nextIndex, { tab: get(consumersPanelState).activeSubtab });
}

export async function saveCurrentConsumerAction() {
  const consumer = currentConsumer();
  if (!consumer) return;
  await flushPendingFormDraft();
  const draft = formDrafts.get(get(consumersPanelState).selectedIndex);
  if (draft) {
    const normalizedDraft = normalizeConsumerConfig({ ...consumer, ...deepClone(draft) });
    normalizedDraft.output = consumer.output;
    normalizedDraft.message_capture = consumer.message_capture;
    normalizedDraft.response = consumer.response;
    const endpointType = getEndpointType(normalizedDraft.endpoint);
    if (endpointType === "static" || endpointType === "ref") {
      normalizedDraft.endpoint[endpointType] = normalizeScalarEndpointValue(endpointType, normalizedDraft.endpoint[endpointType]);
    }
    activeConfig.consumers[get(consumersPanelState).selectedIndex] = normalizedDraft;
  }
  const saved = await saveConsumersSection(activeConfig.consumers);
  if (saved) {
    await restoreConsumerStateFromView(get(consumersPanelState).selectedIndex, { tab: get(consumersPanelState).activeSubtab });
  }
  return saved;
}

export async function toggleActiveConsumer() {
  let consumer = currentConsumer();
  if (!consumer) return;
  const status = getConsumerStatus(consumer);
  const currentlyRunning = Boolean(status.running);
  consumersPanelState.update((state) => ({
    ...state,
    toggleBusy: true,
    toggleLabel: currentlyRunning ? "Stopping..." : "Starting...",
  }));
  try {
    let runtimeKey = getRuntimeKey(consumer);
    // Auto-save before starting so the backend knows about the consumer. Besides
    // the general dirty check, explicitly cover newly-created consumers that have
    // never been persisted (e.g. the first consumer in a fresh workspace, where
    // `_mqb_saved_sections.consumers` doesn't exist yet and hasUnsavedConsumers()
    // would report false).
    if (!currentlyRunning && (hasUnsavedConsumers() || !isSavedConsumer(consumer))) {
      // Flush any focused form field so its draft is merged into the consumer
      // config before we persist and hand it to the runtime.
      await flushPendingFormDraft();
      const saved = await saveConsumersSection(activeConfig.consumers);
      if (saved && Array.isArray((saved as any).consumers)) {
        const rawSavedConsumer = (saved as any).consumers[get(consumersPanelState).selectedIndex];
        runtimeKey = rawSavedConsumer?.id ? String(rawSavedConsumer.id) : String(rawSavedConsumer?.name || runtimeKey);
        consumer = currentConsumer();
        if (!consumer) return;
        renderSelectedConsumer();
      }
    }
    const response = await fetch(`/${currentlyRunning ? "consumer-stop" : "consumer-start"}?consumer_id=${encodeURIComponent(runtimeKey)}`, { method: "POST" });
    if (!response.ok) {
      const message = await response.text();
      consumerErrorByKey[runtimeKey] = message;
      void browserWindow().mqbAlert?.(message);
      consumersPanelState.update((state) => ({
        ...state,
        liveStatusText: message,
        liveStatusVariant: "danger",
        toggleBusy: false,
        toggleLabel: currentlyRunning ? "Stop" : "Start",
      }));
      return;
    }
    delete consumerErrorByKey[runtimeKey];
    await browserWindow().pollRuntimeStatus?.();
    browserWindow().renderConsumersRuntimeStatus?.();
  } finally {
    consumersPanelState.update((state) => ({ ...state, toggleBusy: false }));
    renderSelectedConsumer();
  }
}

export async function importAsyncApiToConsumerAction(jsonText: string) {
  const imported = extractImportedRequests(jsonText);
  const existingNames = new Set(activeConfig.consumers.map((consumer) => consumer.name));
  for (const request of imported.requests) {
    const parsed = (() => {
      try {
        return new URL(String((request as any).url || "http://localhost/"));
      } catch {
        return new URL(`http://localhost${String((request as any).url || "/")}`);
      }
    })();
    const baseName = sanitizeConsumerName(String((request as any).name || "imported_consumer")).toLowerCase();
    let uniqueName = baseName;
    let index = 1;
    while (existingNames.has(uniqueName)) {
      uniqueName = `${baseName}_${index++}`;
    }
    existingNames.add(uniqueName);
    activeConfig.consumers.push(normalizeConsumerConfig({
      id: createLocalEntityId("consumer"),
      name: uniqueName,
      endpoint: {
        middlewares: [],
        http: { url: "0.0.0.0:8080", path: parsed.pathname || "/" },
      },
      response: null,
      output: { mode: "none" },
      message_capture: { ...DEFAULT_CAPTURE },
    } as ConsumerConfig));
  }
  renderSelectedConsumer();
}

export async function importMqbToConsumerAction(jsonText: string) {
  const parsed = JSON.parse(jsonText) as any;
  const importedConsumers = Array.isArray(parsed?.config?.consumers) ? parsed.config.consumers : [];
  const existingNames = new Set(activeConfig.consumers.map((consumer) => consumer.name));
  for (const raw of importedConsumers) {
    const next = normalizeConsumerConfig(raw as ConsumerConfig);
    const baseName = String(next.name || "consumer");
    let candidate = baseName;
    let index = 1;
    while (existingNames.has(candidate)) {
      candidate = `${baseName}_${index++}`;
    }
    next.name = candidate;
    existingNames.add(candidate);
    activeConfig.consumers.push(next);
  }
  renderSelectedConsumer();
}

export async function initConsumers(config: ConsumersAppConfig, schema: ConsumersSchemaRoot) {
  activeConfig = config;
  activeSchema = schema;
  messagesByConsumer = await readStoredMessages();
  consumerErrorByKey = {};
  formDrafts = new Map();
  renderedConsumerFormSignature = null;
  nextResponseHeaderId = 1;
  lastMessageSequenceByConsumer = {};
  finalPollByConsumer = {};
  activeConfig.consumers = (activeConfig.consumers || []).map(normalizeConsumerConfig);
  activeConfig.publishers = Array.isArray(activeConfig.publishers) ? activeConfig.publishers : [];
  const initialConsumersSnapshot = deepClone(activeConfig.consumers);
  if (!browserWindow()._mqb_saved_sections?.consumers) {
    await tick();
    browserWindow().markSectionSaved?.("consumers", initialConsumersSnapshot);
  }
  browserWindow().registerDirtySection?.("consumers", {
    buttonId: "workspace-save-button",
    getValue: () => activeConfig.consumers,
  });

  browserWindow().renderConsumersRuntimeStatus = () => {
    renderSelectedConsumer();
    void fetchNewMessagesForRunningConsumers().then(() => {
      renderSelectedConsumer();
    });
    scheduleMessagePolling();
  };
  browserWindow().restoreConsumerState = restoreConsumerStateFromView;

  const hashMatch = browserWindow().location.hash.match(/^#consumers:(\d+)$/);
  const selectedIndex = hashMatch ? Number(hashMatch[1]) : Math.min(getAppState().last_consumer_idx ?? 0, Math.max(activeConfig.consumers.length - 1, 0));
  const items = buildSidebarItems();
  consumersPanelState.update((state) => ({
    ...state,
    hasConsumers: activeConfig.consumers.length > 0,
    items,
    groupedItems: buildGroupedSidebarItems(items),
    selectedIndex,
    selectedMessageIndex: -1,
    activeSubtab: "messages",
    messages: [],
  }));
  await restoreConsumerStateFromView(selectedIndex, { tab: getAppState().last_consumer_tab || "messages" });
  renderSelectedConsumer();
}
