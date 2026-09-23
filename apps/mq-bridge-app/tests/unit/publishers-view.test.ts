// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { get } from "svelte/store";

const dialogMocks = vi.hoisted(() => ({ open: vi.fn(), save: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => dialogMocks);

import { getAppState } from "../../ui/src/lib/app-shell";
import {
  addPublisherAction,
  addPublisherMetadataRow,
  applyCurrentPublisherRawConfig,
  beautifyPublisherPayloadAction,
  browsePublisherFilePath,
  clearActivePublisherHistory,
  cloneCurrentPublisherAction,
  copyCurrentPublisherAction,
  createConsumerEndpointFromPublisherEndpoint,
  importAsyncApiToPublisherAction,
  importMqbToPublisherAction,
  initPublishers,
  restorePublisherStateFromView,
  removePublisherMetadataRow,
  saveCurrentPublisherAction,
  saveCurrentPublisherVariantAction,
  sendPublisherAction,
  showPublisherHistoryEntry,
  selectPublisherSubtab,
  togglePublisherMetadataRow,
  updatePublisherMetadataRow,
  updatePublisherPayload,
  updatePublisherRequestField,
} from "../../ui/src/lib/publishers-view";
import { publishersPanelState } from "../../ui/src/lib/stores";
import { createHyperscriptNode, installBaseWindowStubs, restoreBaseWindowStubs } from "./test-helpers";

function mountPublishersDom() {
  document.body.innerHTML = `
    <div id="tab-publishers" class="active">
      <div id="publishers-container">
        <input id="pub-filter" />
        <button id="pub-add"></button>
        <button id="pub-copy"></button>
        <button id="pub-clone"></button>
        <button id="pub-save"></button>
        <button id="pub-delete"></button>
        <button id="pub-send"></button>
        <button id="pub-beautify"></button>
        <button id="add-meta"></button>
        <div id="pub-list"></div>
        <div id="pub-empty-alert"></div>
        <div id="pub-main-ui"></div>
        <input id="pub-proto" />
        <div id="pub-method-wrap"></div>
        <select id="pub-method"></select>
        <div id="pub-extra-1-wrap"></div>
        <div id="pub-extra-2-wrap"></div>
        <div id="pub-url-wrap"></div>
        <span id="pub-extra-1-label"></span>
        <span id="pub-extra-2-label"></span>
        <span id="pub-url-label"></span>
        <input id="pub-extra-1" />
        <input id="pub-extra-2" />
        <input id="pub-url" />
        <div id="pub-sub-tabs">
          <button class="content-tab" id="ctab-payload" data-target="pub-payload-pane"></button>
          <button class="content-tab" id="ctab-config" data-target="pub-config-pane"></button>
          <button class="content-tab" data-target="pub-meta-pane"></button>
          <button class="content-tab" data-target="pub-history-pane"></button>
        </div>
        <div id="pub-top-content-wrapper">
          <div class="pane-top" id="pub-payload-pane"></div>
          <div class="pane-top" id="pub-meta-pane"></div>
          <div class="pane-top" id="pub-history-pane"></div>
          <div class="pane-top" id="pub-config-pane"></div>
        </div>
        <textarea id="pub-payload"></textarea>
        <table id="metadata-container"><tbody></tbody></table>
        <div id="pub-response-container"></div>
        <div id="pub-response-status"></div>
        <div id="pub-response"></div>
        <button id="pub-resp-copy"></button>
        <div id="pub-response-tab"></div>
        <div id="pub-config-form"></div>
      </div>
    </div>
  `;
}

function installPublisherWindowStubs() {
  const storage = new Map<string, string>([
    ["mqb_publisher_state", "{}"],
    ["mqb_publisher_history", JSON.stringify({ version: 1, updated_at: 0, publishers: {} })],
  ]);
  installBaseWindowStubs();
  window.fetchConfigFromServer = vi.fn().mockResolvedValue({ publishers: [] });

  let serverConfig: any = { publishers: [], routes: {}, consumers: [], presets: {}, env_vars: {}, history: {} };
  window.appConfig = serverConfig;
  window.initConsumers = vi.fn();
  window.Split = vi.fn().mockReturnValue({});
  Object.defineProperty(window, "localStorage", {
    value: {
      getItem: vi.fn().mockImplementation((key: string) => storage.get(key) ?? null),
      setItem: vi.fn().mockImplementation((key: string, value: string) => {
        storage.set(key, value);
      }),
      removeItem: vi.fn().mockImplementation((key: string) => {
        storage.delete(key);
      }),
    },
    configurable: true,
  });
  globalThis.fetch = vi.fn().mockImplementation(async (input: string, init?: RequestInit) => {
    if (String(input) === "/config" && init?.method === "POST") {
      serverConfig = JSON.parse(String(init.body || "{}"));
      window.appConfig = serverConfig;
      return {
        ok: true,
        status: 200,
        statusText: "OK",
        text: async () => "",
        json: async () => serverConfig,
      };
    }

    if (String(input) === "/config") {
      return {
        ok: true,
        status: 200,
        statusText: "OK",
        json: async () => serverConfig,
      };
    }

    return {
      ok: true,
      status: 200,
      statusText: "OK",
      text: async () => '{"status":"Ack"}',
      json: async () => ({ status: "Ack" }),
    };
  }) as any;
}

describe("initPublishers", () => {
  beforeEach(() => {
    delete (window as any).__mqb_state;
    (window as any)._mqb_last_publisher_idx = undefined;
    (window as any)._mqb_last_publisher_tab = undefined;
    (window as any)._mqb_storage_security = undefined;
    (window as any)._mqb_storage_cache = undefined;
    delete (window as any).__MQB_DESKTOP__;
    dialogMocks.open.mockReset();
    dialogMocks.save.mockReset();
    mountPublishersDom();
    installPublisherWindowStubs();
  });

  afterEach(() => {
    vi.useRealTimers();
    delete (window as any).__MQB_DESKTOP__;
    restoreBaseWindowStubs();
  });

  test("renders publisher names and request bar state", () => {
    initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(get(publishersPanelState).items[0]?.name).toBe("orders_http");
    expect(get(publishersPanelState).selectedIndex).toBe(0);
    expect(get(publishersPanelState).endpointType).toBe("HTTP");
    expect(get(publishersPanelState).urlField.value).toBe("https://example.test/orders");
    expect(get(publishersPanelState).hasPublishers).toBe(true);
  });

  // A file publisher edits its path in the request bar (the definition form hides it), so
  // the desktop picker has to be offered there or it is unreachable.
  test("offers a save-file picker for desktop file publishers", async () => {
    (window as any).__MQB_DESKTOP__ = true;
    dialogMocks.save.mockResolvedValueOnce("/tmp/out.jsonl");
    const config = {
      publishers: [{ name: "dump", endpoint: { file: { path: "/tmp/old.jsonl" } } }],
      routes: {},
      consumers: [],
    };

    await initPublishers(config, { properties: { publishers: { items: {} } } });

    expect(get(publishersPanelState).urlField.browse).toBe(true);

    await browsePublisherFilePath();

    expect(dialogMocks.save).toHaveBeenCalledWith({ defaultPath: "/tmp/old.jsonl" });
    expect(config.publishers[0].endpoint.file.path).toBe("/tmp/out.jsonl");
    expect(get(publishersPanelState).urlField.value).toBe("/tmp/out.jsonl");
  });

  test("defaults an empty save-file path to messages.jsonl", async () => {
    (window as any).__MQB_DESKTOP__ = true;
    dialogMocks.save.mockResolvedValueOnce(null);
    const config = {
      publishers: [{ name: "dump", endpoint: { file: { path: "" } } }],
      routes: {},
      consumers: [],
    };

    await initPublishers(config, { properties: { publishers: { items: {} } } });
    await browsePublisherFilePath();

    expect(dialogMocks.save).toHaveBeenCalledWith({ defaultPath: "messages.jsonl" });
  });

  test("hides the file picker outside the desktop app and for other endpoint types", async () => {
    await initPublishers(
      {
        publishers: [{ name: "dump", endpoint: { file: { path: "/tmp/old.jsonl" } } }],
        routes: {},
        consumers: [],
      },
      { properties: { publishers: { items: {} } } },
    );
    expect(get(publishersPanelState).urlField.browse).toBe(false);

    (window as any).__MQB_DESKTOP__ = true;
    await initPublishers(
      {
        publishers: [{ name: "orders", endpoint: { kafka: { url: "localhost:9092", topic: "events" } } }],
        routes: {},
        consumers: [],
      },
      { properties: { publishers: { items: {} } } },
    );
    expect(get(publishersPanelState).urlField.browse).toBe(false);
  });

  test("shows empty state when there are no publishers", () => {
    initPublishers(
      { publishers: [], routes: {}, consumers: [] },
      { properties: { publishers: { items: {} } } },
    );

    expect(get(publishersPanelState).hasPublishers).toBe(false);
  });

  test("creates new http publishers with usable local defaults", async () => {
    const config = { publishers: [], routes: {}, consumers: [] };

    initPublishers(config, { properties: { publishers: { items: {} } } });

    await addPublisherAction("http");

    expect(config.publishers).toHaveLength(1);
    expect(config.publishers[0]).toMatchObject({
      id: expect.any(String),
      name: "",
      endpoint: {
        http: {
          url: "http://localhost:8080",
          path: "/",
          method: "POST",
          custom_headers: {},
          fire_and_forget: false,
          compression_enabled: false,
        },
      },
      comment: "",
    });
    expect(get(publishersPanelState).endpointType).toBe("HTTP");
    expect(get(publishersPanelState).methodValue).toBe("POST");
    expect(get(publishersPanelState).urlField.value).toBe("http://localhost:8080");
    expect(get(publishersPanelState).activeSubtab).toBe("definition");
    expect(config.publishers[0].endpoint.middlewares).toEqual([]);
  });

  test("applies an edited raw config over the local request state", async () => {
    const config = { publishers: [], routes: {}, consumers: [] } as any;
    initPublishers(config, { properties: { publishers: { items: {} } } });
    await addPublisherAction("http");
    const id = config.publishers[0].id;
    updatePublisherPayload("{\"old\":true}");

    await applyCurrentPublisherRawConfig({
      name: "edited",
      endpoint: { memory: { topic: "out" } },
      payload: "{\"new\":true}",
      headers: [{ key: "x-a", value: "1", enabled: true }],
    });

    expect(config.publishers[0]).toMatchObject({ id, name: "edited", endpoint: { memory: { topic: "out" } } });
    expect(get(publishersPanelState).requestPayload).toBe("{\"new\":true}");
    expect(get(publishersPanelState).metadataRows).toMatchObject([{ key: "x-a", value: "1", enabled: true }]);
  });

  test("creates new static publishers on definition and saves scalar endpoint values", async () => {
    const config = { publishers: [], routes: {}, consumers: [] };
    let formChange: ((updated: unknown) => void) | null = null;
    window.VanillaSchemaForms.init = vi.fn().mockImplementation((_container, _schema, _data, onChange) => {
      formChange = onChange;
      return Promise.resolve({ setData: vi.fn() });
    });
    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, publishers: any[]) => ({ publishers }));

    initPublishers(config, { properties: { publishers: { items: {} } } });

    await addPublisherAction("static");

    expect(config.publishers[0]).toMatchObject({
      id: expect.any(String),
      name: "",
      endpoint: { static: "" },
      comment: "",
    });
    expect(get(publishersPanelState).activeSubtab).toBe("definition");

    formChange?.({
      name: "static",
      endpoint: { static: "hello world" },
      comment: "",
    });

    await saveCurrentPublisherAction(document.getElementById("pub-save"));

    expect(window.saveConfigSection).toHaveBeenCalledWith(
      "publishers",
      [
        {
          id: expect.any(String),
          name: "static",
          endpoint: {
            middlewares: [],
            static: "hello world",
          },
          comment: "",
          payload: '{}',
          headers: [],
        },
      ],
      false,
      document.getElementById("pub-save"),
    );
  });

  test("keeps middlewares added to a static publisher", async () => {
    const config = { publishers: [], routes: {}, consumers: [] };
    let formChange: ((updated: unknown) => void) | null = null;
    window.VanillaSchemaForms.init = vi.fn().mockImplementation((_container, _schema, _data, onChange) => {
      formChange = onChange;
      return Promise.resolve({ setData: vi.fn() });
    });
    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, publishers: any[]) => ({ publishers }));

    initPublishers(config, { properties: { publishers: { items: {} } } });
    await addPublisherAction("static");

    formChange?.({
      name: "static",
      endpoint: { static: "hello world", middlewares: [{ metrics: {} }] },
      comment: "",
    });

    expect((config.publishers[0] as any).endpoint.middlewares).toEqual([{ metrics: {} }]);

    await saveCurrentPublisherAction(document.getElementById("pub-save"));

    expect(window.saveConfigSection).toHaveBeenCalledWith(
      "publishers",
      [
        expect.objectContaining({
          endpoint: {
            middlewares: [{ metrics: {} }],
            static: "hello world",
          },
        }),
      ],
      false,
      document.getElementById("pub-save"),
    );
  });

  test("creates new queue and topic publishers with request bar defaults", async () => {
    const config = { publishers: [], routes: {}, consumers: [] };

    initPublishers(config, { properties: { publishers: { items: {} } } });

    await addPublisherAction("amqp");
    await addPublisherAction("kafka");

    expect(config.publishers[0]).toMatchObject({
      id: expect.any(String),
      name: "",
      endpoint: {
        amqp: {
          url: "amqp://guest:guest@localhost:5672/%2f",
          queue: "jobs",
        },
      },
    });
    expect(config.publishers[1]).toMatchObject({
      id: expect.any(String),
      name: "",
      endpoint: {
        kafka: {
          url: "localhost:9092",
          topic: "events",
        },
      },
    });
    expect(get(publishersPanelState).endpointType).toBe("KAFKA");
    expect(get(publishersPanelState).extraFieldOne.value).toBe("events");
    expect(get(publishersPanelState).urlField.value).toBe("localhost:9092");
  });

  test("falls back safely when persisted publisher localStorage is corrupted", () => {
    window.localStorage.setItem("mqb_publisher_state", "{broken-json");
    window.localStorage.setItem("mqb_publisher_history", "{broken-json");
    window.localStorage.setItem("mqb_publisher_presets", "{broken-json");
    window.localStorage.setItem("mqb_env_vars", "{broken-json");

    expect(() =>
      initPublishers(
        {
          publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
          routes: {},
          consumers: [],
        },
        {
          properties: { publishers: { items: {} } },
          $defs: { HttpConfig: { properties: { custom_headers: {} } } },
        },
      ),
    ).not.toThrow();

    expect(get(publishersPanelState).items[0]?.name).toBe("orders_http");
    expect(get(publishersPanelState).historyRows).toEqual([]);
    expect(get(publishersPanelState).groupedItems).toEqual([
      expect.objectContaining({ kind: "publisher", publisherIndex: 0 }),
    ]);
  });

  test("hydrates publisher history cache from workspace config when local cache is empty", () => {
    const config = {
      publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
      routes: {},
      consumers: [],
      history: {
        version: 1,
        updated_at: 10,
        publishers: {
          orders_http: [{
            name: "orders_http",
            payload: "{\"id\":42}",
            headers: [],
            metadata: [],
            endpoint_type: "http",
            method: "POST",
            url: "https://example.test/orders",
            request_fields: { url: "https://example.test/orders" },
            requestMetadata: { "request_bar.url": "https://example.test/orders" },
            status: 200,
            duration: 5,
            time: 10,
          }],
        },
      },
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(get(publishersPanelState).historyRows).toHaveLength(1);
    const stored = JSON.parse(window.localStorage.getItem("mqb_publisher_history") || "{}");
    expect(stored.publishers.orders_http).toHaveLength(1);
  });

  test("hydrates publisher cache from preloaded temporary encrypted storage", () => {
    (window as any)._mqb_storage_security = {
      encrypted: true,
      persistent: false,
      keySource: "ephemeral-process",
      configEncrypted: false,
      messagesEncrypted: true,
      messagesPersistent: false,
      messageKeyHex: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
      kid: "session-1",
    };
    (window as any)._mqb_storage_cache = {
      publisher_state: {
        orders_http: {
          payload: "{\"cached\":true}",
        },
      },
      publisher_history: {
        version: 1,
        updated_at: 50,
        publishers: {
          orders_http: [{
            name: "orders_http",
            payload: "{\"cached\":true}",
            headers: [],
            metadata: [],
            endpoint_type: "http",
            method: "POST",
            url: "https://example.test/orders",
            request_fields: { url: "https://example.test/orders" },
            requestMetadata: { "request_bar.url": "https://example.test/orders" },
            status: 200,
            duration: 5,
            time: 50,
          }],
        },
      },
    };

    initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
        config_security: { mode: "sensitive" },
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(get(publishersPanelState).requestPayload).toBe("{\"cached\":true}");
    expect(get(publishersPanelState).historyRows).toHaveLength(1);
  });

  test("tracks active subtab and payload in publisher panel state", () => {
    initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    selectPublisherSubtab("definition");
    updatePublisherPayload("{\"ok\":true}");

    expect(get(publishersPanelState).activeSubtab).toBe("definition");
    expect(get(publishersPanelState).requestPayload).toBe("{\"ok\":true}");
  });

  test("keeps definition tab and saves once after renaming an unsaved publisher", async () => {
    const config = {
      publishers: [
        { name: "http", endpoint: { http: { url: "http://localhost:8080", path: "/", method: "POST", custom_headers: {} } } },
      ],
      routes: {},
      consumers: [
        {
          name: "ingest_http",
          endpoint: { http: { url: "http://localhost:8081", path: "/", method: "POST", custom_headers: {} } },
          output: { mode: "publisher", publisher: "http" },
          response: null,
        },
      ],
    };
    let formChange: ((updated: unknown) => void) | null = null;
    window.VanillaSchemaForms.init = vi.fn().mockImplementation((_container, _schema, _data, onChange) => {
      formChange = onChange;
      return Promise.resolve({ setData: vi.fn() });
    });
    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, publishers: any[]) => ({ publishers }));

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );
    selectPublisherSubtab("definition");
    await new Promise((resolve) => setTimeout(resolve, 0));

    formChange?.({
      name: "renamed_http",
      endpoint: { http: { url: "http://localhost:8080", path: "/", method: "POST", custom_headers: {} } },
      comment: "",
    });

    expect(config.publishers[0].name).toBe("renamed_http");
    expect(get(publishersPanelState).activeSubtab).toBe("definition");

    await saveCurrentPublisherAction(document.getElementById("pub-save"));

    expect(window.saveConfigSection).toHaveBeenCalledTimes(1);
    expect(window.saveConfigSection).toHaveBeenCalledWith(
      "publishers",
      [
        expect.objectContaining({
          id: expect.any(String),
          name: "renamed_http",
          endpoint: {
            http: { url: "http://localhost:8080", path: "/", method: "POST", custom_headers: {} },
            middlewares: [],
          },
          comment: "",
          payload: "{}",
          headers: [],
        }),
      ],
      false,
      document.getElementById("pub-save"),
    );
    expect(config.consumers[0].output).toEqual({
      mode: "publisher",
      publisher: "renamed_http",
      publisher_id: expect.any(String),
    });
    expect(window.fetchConfigFromServer).not.toHaveBeenCalled();
    expect(get(publishersPanelState).activeSubtab).toBe("definition");
    expect(get(publishersPanelState).items[0]?.name).toBe("renamed_http");
  });

  test("allows clearing a publisher name and keeps consumer output linked by id", async () => {
    const config = {
      publishers: [
        { name: "http", endpoint: { http: { url: "http://localhost:8080", path: "/", method: "POST", custom_headers: {} } } },
      ],
      routes: {},
      consumers: [
        {
          name: "ingest_http",
          endpoint: { http: { url: "http://localhost:8081", path: "/", method: "POST", custom_headers: {} } },
          output: { mode: "publisher", publisher: "http" },
          response: null,
        },
      ],
    };
    let formChange: ((updated: unknown) => void) | null = null;
    window.VanillaSchemaForms.init = vi.fn().mockImplementation((container, schema, _data, onChange) => {
      expect(schema.required || []).not.toContain("name");
      const input = document.createElement("input");
      input.name = "name";
      input.value = "http";
      container.appendChild(input);
      formChange = onChange;
      return Promise.resolve({ setData: vi.fn() });
    });
    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, publishers: any[]) => ({ publishers }));

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        required: ["name"],
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );
    selectPublisherSubtab("definition");
    await Promise.resolve();

    const publisherId = config.publishers[0].id;
    expect(config.consumers[0].output).toEqual({
      mode: "publisher",
      publisher: "http",
      publisher_id: publisherId,
    });

    formChange?.({
      name: "http",
      endpoint: { http: { url: "http://localhost:8080", path: "/", method: "POST", custom_headers: {} } },
      comment: "",
    });
    const nameInput = document.querySelector<HTMLInputElement>('#pub-config-form input[name="name"]');
    if (nameInput) nameInput.value = "";

    await saveCurrentPublisherAction(document.getElementById("pub-save"));

    const savedPublishers = (window.saveConfigSection as any).mock.calls.at(-1)?.[1];
    expect(savedPublishers[0].name).toBe("");
    expect(config.consumers[0].output).toEqual({
      mode: "publisher",
      publisher: "",
      publisher_id: publisherId,
    });
  });

  test("restoring a publisher updates the remembered index and hash", () => {
    initPublishers(
      {
        publishers: [
          { name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } },
          { name: "events_memory", endpoint: { memory: { topic: "events" } } },
        ],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    restorePublisherStateFromView(1, { tab: "definition" });

    expect(get(publishersPanelState).selectedIndex).toBe(1);
    expect(window.location.hash).toBe("#publishers:1");
    expect((window as any)._mqb_last_publisher_idx).toBe(1);
    expect((window as any)._mqb_last_publisher_tab).toBe("definition");
    expect(getAppState().last_publisher_idx).toBe(1);
    expect(getAppState().last_publisher_tab).toBe("definition");
  });

  test("selecting a publisher subtab updates remembered app state", async () => {
    await initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    selectPublisherSubtab("history");

    expect(get(publishersPanelState).activeSubtab).toBe("history");
    expect((window as any)._mqb_last_publisher_tab).toBe("history");
    expect(getAppState().last_publisher_tab).toBe("history");
  });

  test("save keeps the selected publisher and subtab", async () => {
    let resolveSave: ((value: any) => void) | null = null;
    window.saveConfigSection = vi.fn().mockImplementation(
      async () =>
        await new Promise((resolve) => {
          resolveSave = resolve;
        }),
    );

    initPublishers(
      {
        publishers: [
          { name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } },
          { name: "events_memory", endpoint: { memory: { topic: "events" } } },
        ],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    restorePublisherStateFromView(1, { tab: "definition" });
    const savePromise = saveCurrentPublisherAction(document.getElementById("pub-save"));
    await Promise.resolve();
    resolveSave?.({
      publishers: [
        { name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } },
        { name: "events_memory", endpoint: { memory: { topic: "events" } } },
      ],
    });
    await savePromise;
    await Promise.resolve();

    expect(get(publishersPanelState).selectedIndex).toBe(1);
    expect(get(publishersPanelState).activeSubtab).toBe("definition");
    expect((window as any)._mqb_last_publisher_idx).toBe(1);
    expect((window as any)._mqb_last_publisher_tab).toBe("definition");
    expect(getAppState().last_publisher_idx).toBe(1);
    expect(getAppState().last_publisher_tab).toBe("definition");
  });

  test("init restores the selected publisher from the URL hash on reload", async () => {
    window.location.hash = "#publishers:1";

    await initPublishers(
      {
        publishers: [
          { name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } },
          { name: "events_memory", endpoint: { memory: { topic: "events" } } },
        ],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(get(publishersPanelState).selectedIndex).toBe(1);
    expect((window as any)._mqb_last_publisher_idx).toBe(1);
  });

  test("edits actual custom headers and supports disable/delete", () => {
    const config = {
      publishers: [
        {
          name: "orders_http",
          endpoint: {
            http: {
              url: "https://example.test/orders",
              custom_headers: { authorization: "Bearer token" },
              fire_and_forget: false,
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(get(publishersPanelState).metadataRows).toEqual([
      { id: expect.any(Number), key: "authorization", value: "Bearer token", enabled: true },
    ]);

    addPublisherMetadataRow();
    updatePublisherMetadataRow(1, "key", "x-trace-id");
    updatePublisherMetadataRow(1, "value", "abc-123");
    togglePublisherMetadataRow(1, false);

    expect(config.publishers[0].endpoint.http.custom_headers).toEqual({
      authorization: "Bearer token",
    });

    removePublisherMetadataRow(0);

    expect(get(publishersPanelState).metadataRows).toEqual([
      { id: expect.any(Number), key: "x-trace-id", value: "abc-123", enabled: false },
    ]);
    expect(config.publishers[0].endpoint.http.custom_headers).toEqual({});
  });

  test("keeps header row ids unique across re-init", () => {
    const config = {
      publishers: [
        {
          name: "orders_http",
          endpoint: {
            http: {
              url: "https://example.test/orders",
              custom_headers: {},
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    const schema = {
      properties: { publishers: { items: {} } },
      $defs: { HttpConfig: { properties: { custom_headers: {} } } },
    };

    initPublishers(config, schema);
    addPublisherMetadataRow();
    initPublishers(config, schema);
    addPublisherMetadataRow();

    expect(get(publishersPanelState).metadataRows.map((row) => row.id)).toEqual([1, 2]);
  });

  test("repairs duplicate persisted header row ids from local storage", () => {
    window.localStorage.setItem(
      "mqb_publisher_state",
      JSON.stringify({
        orders_http: {
          payload: "{}",
          headers: [
            { id: 1, key: "authorization", value: "Bearer token", enabled: true },
            { id: 1, key: "x-trace-id", value: "abc-123", enabled: true },
          ],
        },
      }),
    );

    initPublishers(
      {
        publishers: [
          {
            name: "orders_http",
            endpoint: {
              http: {
                url: "https://example.test/orders",
                custom_headers: { authorization: "Bearer token", "x-trace-id": "abc-123" },
              },
            },
          },
        ],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(get(publishersPanelState).metadataRows.map((row) => row.id)).toEqual([1, 2]);
  });

  test("keeps http url+path edits from request bar in endpoint config", () => {
    const config = {
      publishers: [
        {
          name: "orders_http",
          endpoint: {
            http: {
              url: "https://example.test",
              path: "/orders/old",
              custom_headers: {},
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(get(publishersPanelState).urlField.value).toBe("https://example.test/orders/old");

    updatePublisherRequestField("pub-url", "https://api.example.test/new/orders?mode=fast");

    expect(config.publishers[0].endpoint.http.url).toBe("https://api.example.test");
    expect(config.publishers[0].endpoint.http.path).toBe("/new/orders?mode=fast");
    expect(get(publishersPanelState).urlField.value).toBe("https://api.example.test/new/orders?mode=fast");
  });

  test("migrates legacy http presets into saved publisher variants", async () => {
    const config = {
      publishers: [
        {
          name: "orders_http",
          endpoint: {
            http: {
              url: "https://example.test/orders",
              custom_headers: {},
            },
          },
        },
      ],
      routes: {},
      consumers: [],
      presets: {
        orders_http: [
          {
            name: "preset_a",
            method: "PUT",
            url: "https://api.example.test/orders/42",
            payload: "{\"id\":42}",
            headers: [{ key: "x-test", value: "yes", enabled: true }],
          },
        ],
      },
      env_vars: {},
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    expect(config.publishers).toHaveLength(2);
    expect(config.publishers[1]).toMatchObject({
      id: expect.any(String),
      name: "orders_http - preset_a",
      payload: "{\"id\":42}",
      headers: [{ key: "x-test", value: "yes", enabled: true }],
    });
    expect(config.publishers[1].endpoint.http).toMatchObject({
      method: "PUT",
      url: "https://api.example.test",
      path: "/orders/42",
      custom_headers: { "x-test": "yes" },
    });
  });

  test("migrates legacy non-http presets into saved publisher variants", async () => {
    const config = {
      publishers: [
        {
          name: "events_kafka",
          endpoint: {
            kafka: {
              url: "localhost:9092",
              topic: "events",
            },
          },
        },
      ],
      routes: {},
      consumers: [],
      presets: {
        events_kafka: [
          {
            name: "orders_created",
            endpoint_type: "kafka",
            payload: "{\"id\":42}",
            headers: [],
            request_fields: {
              topic: "orders.created",
              url: "kafka-a:9092,kafka-b:9092",
            },
          },
        ],
      },
      env_vars: {},
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { KafkaConfig: { properties: {} } },
      },
    );

    expect(config.publishers).toHaveLength(2);
    expect(config.publishers[1]).toMatchObject({
      id: expect.any(String),
      name: "events_kafka - orders_created",
      payload: "{\"id\":42}",
    });
    expect(config.publishers[1].endpoint.kafka).toMatchObject({
      topic: "orders.created",
      url: "kafka-a:9092,kafka-b:9092",
    });
  });

  test("restores http method and visible url field from history", async () => {
    window.localStorage.setItem(
      "mqb_publisher_history",
      JSON.stringify([
        {
          name: "orders_http",
          payload: "{\"id\":42}",
          metadata: [{ k: "x-test", v: "yes" }],
          requestMetadata: {
            http_method: "PUT",
            http_path: "/orders/42",
            "request_bar.url": "https://api.example.test/orders/42",
            "request_bar.pub-url": "https://api.example.test/orders/42",
          },
          targetLabel: "URL",
          url: "https://api.example.test/orders/42",
          ok: true,
          status: 200,
          statusText: "OK",
          responseData: {
            payload: { accepted: true },
            metadata: { "x-response-id": "abc" },
          },
          duration: 12,
          time: 1,
        },
      ]),
    );
    const config = {
      publishers: [
        {
          name: "orders_http",
          endpoint: {
            http: {
              url: "https://example.test",
              path: "/old",
              method: "POST",
              custom_headers: {},
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    await showPublisherHistoryEntry(0);

    expect(get(publishersPanelState).methodValue).toBe("PUT");
    expect(get(publishersPanelState).urlField.value).toBe("https://api.example.test/orders/42");
    expect(config.publishers[0].endpoint.http.url).toBe("https://api.example.test");
    expect(config.publishers[0].endpoint.http.path).toBe("/orders/42");
    expect(config.publishers[0].endpoint.http.custom_headers).toEqual({ "x-test": "yes" });
    expect(get(publishersPanelState).requestHeaders).toEqual([["x-test", "yes"]]);
    expect(get(publishersPanelState).responseHeaders).toEqual([["x-response-id", "abc"]]);
    expect(get(publishersPanelState).responsePayload).toBe(JSON.stringify({ accepted: true }, null, 2));
  });

  test("restores all visible non-http request bar fields from history", async () => {
    window.localStorage.setItem(
      "mqb_publisher_history",
      JSON.stringify([
        {
          name: "events_kafka",
          payload: "event",
          metadata: [],
          requestMetadata: {
            "request_bar.topic": "orders.created",
            "request_bar.pub-extra-1": "orders.created",
            "request_bar.url": "kafka-a:9092,kafka-b:9092",
            "request_bar.pub-url": "kafka-a:9092,kafka-b:9092",
          },
          targetLabel: "BROKERS",
          url: "kafka-a:9092,kafka-b:9092",
          ok: true,
          status: 200,
          statusText: "OK",
          duration: 12,
          time: 1,
        },
      ]),
    );
    const config = {
      publishers: [
        {
          name: "events_kafka",
          endpoint: {
            kafka: {
              url: "localhost:9092",
              topic: "events",
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { KafkaConfig: { properties: {} } },
      },
    );

    await showPublisherHistoryEntry(0);

    expect(get(publishersPanelState).extraFieldOne.value).toBe("orders.created");
    expect(get(publishersPanelState).urlField.value).toBe("kafka-a:9092,kafka-b:9092");
    expect(config.publishers[0].endpoint.kafka.topic).toBe("orders.created");
    expect(config.publishers[0].endpoint.kafka.url).toBe("kafka-a:9092,kafka-b:9092");
  });

  test("records visible request bar fields in publisher history when sending", async () => {
    const config = {
      publishers: [
        {
          name: "events_kafka",
          endpoint: {
            kafka: {
              url: "localhost:9092",
              topic: "events",
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { KafkaConfig: { properties: {} } },
      },
    );

    updatePublisherRequestField("pub-extra-1", "orders.created");
    updatePublisherRequestField("pub-url", "kafka-a:9092,kafka-b:9092");
    updatePublisherPayload("event");
    await sendPublisherAction();

    const history = JSON.parse(window.localStorage.getItem("mqb_publisher_history") || "{}");
    const historyKey = Object.keys(history.publishers)[0];
    expect(history.publishers[historyKey][0].requestMetadata).toMatchObject({
      "request_bar.topic": "orders.created",
      "request_bar.pub-extra-1": "orders.created",
      "request_bar.url": "kafka-a:9092,kafka-b:9092",
      "request_bar.pub-url": "kafka-a:9092,kafka-b:9092",
    });
    expect(history.publishers[historyKey][0].request_fields).toMatchObject({
      topic: "orders.created",
      url: "kafka-a:9092,kafka-b:9092",
    });
  });

  test("syncs publisher history to config after the debounce window", async () => {
    vi.useFakeTimers();
    const config = {
      publishers: [
        {
          name: "events_kafka",
          endpoint: {
            kafka: {
              url: "localhost:9092",
              topic: "events",
            },
          },
        },
      ],
      routes: {},
      consumers: [],
      history: {},
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { KafkaConfig: { properties: {} } },
      },
    );

    updatePublisherRequestField("pub-extra-1", "orders.created");
    updatePublisherRequestField("pub-url", "kafka-a:9092,kafka-b:9092");
    updatePublisherPayload("event");
    await sendPublisherAction();

    await vi.advanceTimersByTimeAsync(2100);

    const historyCall = (window.saveConfigSection as any).mock.calls.find((call: any[]) => call[0] === "history");
    expect(historyCall).toBeTruthy();
    expect(historyCall[1]).toMatchObject({ version: 1 });
    expect(Object.keys(historyCall[1].publishers || {})).toHaveLength(1);
    const firstHistoryEntries = Object.values(historyCall[1].publishers)[0] as any[];
    expect(firstHistoryEntries[0]).toEqual(expect.objectContaining({
      endpoint_type: "kafka",
      payload: "event",
      request_fields: expect.objectContaining({
        topic: "orders.created",
        url: "kafka-a:9092,kafka-b:9092",
      }),
    }));
    expect(historyCall[2]).toBe(true);
  });

  test("syncs cleared publisher history back to config", async () => {
    vi.useFakeTimers();
    window.localStorage.setItem(
      "mqb_publisher_history",
      JSON.stringify({
        version: 1,
        updated_at: 1,
        publishers: {
          orders_http: [
            {
              name: "orders_http",
              payload: "{\"id\":42}",
              headers: [],
              metadata: [],
              endpoint_type: "http",
              method: "POST",
              url: "https://api.example.test/orders/42",
              request_fields: { url: "https://api.example.test/orders/42" },
              requestMetadata: { "request_bar.url": "https://api.example.test/orders/42" },
              status: 200,
              duration: 12,
              time: 1,
            },
          ],
        },
      }),
    );

    initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
        history: {},
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    clearActivePublisherHistory();
    await vi.advanceTimersByTimeAsync(2100);

    const historyCall = (window.saveConfigSection as any).mock.calls.findLast((call: any[]) => call[0] === "history");
    expect(historyCall).toBeTruthy();
    expect(historyCall[1]).toMatchObject({
      version: 1,
      publishers: {},
    });
    expect(historyCall[2]).toBe(true);
  });

  test("restores request bar fields from input-id fallback history metadata", async () => {
    window.localStorage.setItem(
      "mqb_publisher_history",
      JSON.stringify([
        {
          name: "jobs_amqp",
          payload: "job",
          metadata: [],
          requestMetadata: {
            "request_bar.pub-extra-1": "critical_jobs",
            "request_bar.pub-url": "amqp://guest:guest@mq.local:5672/%2f",
          },
          targetLabel: "URL",
          url: "amqp://guest:guest@mq.local:5672/%2f",
          ok: true,
          status: 200,
          statusText: "OK",
          duration: 12,
          time: 1,
        },
      ]),
    );
    const config = {
      publishers: [
        {
          name: "jobs_amqp",
          endpoint: {
            amqp: {
              url: "amqp://guest:guest@localhost:5672/%2f",
              queue: "jobs",
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { AmqpConfig: { properties: {} } },
      },
    );

    await showPublisherHistoryEntry(0);

    expect(get(publishersPanelState).extraFieldOne.value).toBe("critical_jobs");
    expect(get(publishersPanelState).urlField.value).toBe("amqp://guest:guest@mq.local:5672/%2f");
    expect(config.publishers[0].endpoint.amqp.queue).toBe("critical_jobs");
    expect(config.publishers[0].endpoint.amqp.url).toBe("amqp://guest:guest@mq.local:5672/%2f");
  });

  test("restores multi-field request bar publishers from history", async () => {
    window.localStorage.setItem(
      "mqb_publisher_history",
      JSON.stringify([
        {
          name: "messages_mongodb",
          payload: "{\"ok\":true}",
          metadata: [],
          requestMetadata: {
            "request_bar.database": "orders",
            "request_bar.collection": "outbox",
            "request_bar.url": "mongodb://mongo.local:27017",
          },
          targetLabel: "URL",
          url: "mongodb://mongo.local:27017",
          ok: true,
          status: 200,
          statusText: "OK",
          duration: 12,
          time: 1,
        },
      ]),
    );
    const config = {
      publishers: [
        {
          name: "messages_mongodb",
          endpoint: {
            mongodb: {
              url: "mongodb://localhost:27017",
              database: "app",
              collection: "messages",
            },
          },
        },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { MongoDbConfig: { properties: {} } },
      },
    );

    await showPublisherHistoryEntry(0);

    expect(get(publishersPanelState).extraFieldOne.value).toBe("orders");
    expect(get(publishersPanelState).extraFieldTwo.value).toBe("outbox");
    expect(get(publishersPanelState).urlField.value).toBe("mongodb://mongo.local:27017");
    expect(config.publishers[0].endpoint.mongodb).toMatchObject({
      database: "orders",
      collection: "outbox",
      url: "mongodb://mongo.local:27017",
    });
  });

  test("rejects non-mq-bridge import payloads", async () => {
    initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    await expect(importMqbToPublisherAction(JSON.stringify({ type: "unknown" }))).rejects.toThrow(
      "Selected file is not a valid mq-bridge export/presets file.",
    );
  });

  test("imports mq-bridge presets as publisher variants", async () => {
    initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    await importMqbToPublisherAction(
      JSON.stringify({
        type: "mqb-presets",
        presets: {
          orders_http: [
            {
              name: "imported_preset",
              method: "POST",
              url: "https://example.test/orders",
              payload: "{\"source\":\"mqb\"}",
              headers: [{ key: "x-source", value: "mqb", enabled: true }],
            },
          ],
        },
      }),
    );

    expect(window.appConfig.publishers.map((row) => row.name)).toContain("orders_http - imported_preset");
  });

  test("imports AsyncAPI requests as new publishers", async () => {
    const config = {
      publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
      routes: {},
      consumers: [],
    };
    const schema = {
      properties: { publishers: { items: {} } },
      $defs: { HttpConfig: { properties: { custom_headers: {} } } },
    };

    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, publishers: any[]) => ({ publishers }));
    window.fetchConfigFromServer = vi.fn().mockImplementation(async () => ({ publishers: config.publishers }));

    initPublishers(config, schema);

    await importAsyncApiToPublisherAction(
      JSON.stringify({
        asyncapi: "3.0.0",
        channels: {
          orderEvents: { address: "/events/orders" },
        },
        operations: {
          publishOrderEvents: {
            action: "send",
            channel: { $ref: "#/channels/orderEvents" },
            messages: [],
            title: "Order events",
          },
        },
        servers: { local: { host: "localhost:3000", protocol: "http" } },
      }),
    );

    expect(config.publishers.length).toBe(2);
    expect(config.publishers[1].name).toContain("Order events");
  });

  test("saves current publisher request as a new publisher variant", async () => {
    const config = {
      publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
      routes: {},
      consumers: [],
    };
    window.mqbPrompt = vi.fn().mockResolvedValue("saved_variant");

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    updatePublisherPayload("{\"hello\":true}");
    await saveCurrentPublisherVariantAction();

    expect(config.publishers[1]).toMatchObject({
      name: "saved_variant",
      payload: "{\"hello\":true}",
    });
  });

  test("saves current non-http publisher request as a new publisher variant", async () => {
    const config = {
      publishers: [{ name: "messages_mongodb", endpoint: { mongodb: { url: "mongodb://localhost:27017", database: "app", collection: "messages" } } }],
      routes: {},
      consumers: [],
    };
    window.mqbPrompt = vi.fn().mockResolvedValue("saved_mongo_variant");

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { MongoDbConfig: { properties: {} } },
      },
    );

    updatePublisherRequestField("pub-extra-1", "orders");
    updatePublisherRequestField("pub-extra-2", "outbox");
    updatePublisherRequestField("pub-url", "mongodb://mongo.local:27017");
    updatePublisherPayload("{\"hello\":true}");
    await saveCurrentPublisherVariantAction();

    expect(config.publishers[1]).toMatchObject({
      name: "saved_mongo_variant",
      payload: "{\"hello\":true}",
    });
    expect(config.publishers[1].endpoint.mongodb).toMatchObject({
        database: "orders",
        collection: "outbox",
        url: "mongodb://mongo.local:27017",
    });
  });

  test("beautify payload shows alert on invalid JSON", () => {
    initPublishers(
      {
        publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
        routes: {},
        consumers: [],
      },
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );
    updatePublisherPayload("{invalid");
    beautifyPublisherPayloadAction();
    expect(window.mqbAlert).toHaveBeenCalled();
  });

  test("copy publisher creates a consumer", async () => {
    const config = {
      publishers: [{ name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } }],
      routes: {},
      consumers: [],
    };
    window.mqbPrompt = vi.fn();

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    await copyCurrentPublisherAction();
    expect(window.mqbPrompt).not.toHaveBeenCalled();
    expect(window._mqb_saved_sections?.consumers).toEqual([]);
    expect(config.consumers).toEqual([
      expect.objectContaining({
        name: "",
        endpoint: expect.objectContaining({ http: expect.any(Object) }),
      }),
    ]);
  });

  test("clone publisher keeps the current name and creates a new id", async () => {
    const config = {
      publishers: [
        { name: "orders_http", endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } } },
        { name: "orders_http_copy", endpoint: { http: { url: "https://example.test/orders2", custom_headers: {} } } },
      ],
      routes: {},
      consumers: [],
    };

    initPublishers(
      config,
      {
        properties: { publishers: { items: {} } },
        $defs: { HttpConfig: { properties: { custom_headers: {} } } },
      },
    );

    cloneCurrentPublisherAction();
    await Promise.resolve();

    expect(window.mqbAlert).not.toHaveBeenCalled();
    expect(config.publishers).toHaveLength(3);
    expect(config.publishers[2]).toMatchObject({
      name: "orders_http",
      endpoint: { http: { url: "https://example.test/orders", custom_headers: {} } },
    });
    expect(config.publishers[2].id).toEqual(expect.any(String));
    expect(config.publishers[2].id).not.toBe(config.publishers[0].id);
  });

});

describe("publisher to consumer endpoint conversion", () => {
  test("converts http publisher URLs into consumer listen addresses", () => {
    const endpoint = createConsumerEndpointFromPublisherEndpoint({
      http: {
        url: "http://localhost:1234/api/orders/updated?mode=test",
        method: "POST",
        custom_headers: { authorization: "Bearer token" },
      },
    });

    expect(endpoint).toEqual({
      http: {
        url: "127.0.0.1:1234",
        path: "/api/orders/updated?mode=test",
        method: "POST",
        custom_headers: { authorization: "Bearer token" },
      },
    });
  });

  test("converts websocket publisher URLs into consumer listen addresses", () => {
    const endpoint = createConsumerEndpointFromPublisherEndpoint({
      websocket: {
        url: "ws://localhost:8081/socket/events",
      },
    });

    expect(endpoint).toEqual({
      websocket: {
        url: "127.0.0.1:8081",
        path: "/socket/events",
      },
    });
  });

  test("converts grpc publisher URLs into consumer listen addresses", () => {
    const endpoint = createConsumerEndpointFromPublisherEndpoint({
      grpc: {
        url: "grpc://localhost:50051/bridge.Messages",
      },
    });

    expect(endpoint).toEqual({
      grpc: {
        url: "127.0.0.1:50051",
      },
    });
  });

  test("leaves non-http endpoints unchanged", () => {
    const endpoint = createConsumerEndpointFromPublisherEndpoint({
      memory: { topic: "events" },
    });

    expect(endpoint).toEqual({
      memory: { topic: "events" },
    });
  });
});
