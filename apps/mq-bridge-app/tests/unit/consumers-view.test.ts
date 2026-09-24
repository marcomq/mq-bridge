// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { get } from "svelte/store";
import {
  addConsumerAction,
  addConsumerResponseHeader,
  applyCurrentConsumerRawConfig,
  copyCurrentConsumerAction,
  currentConsumerConfigVariants,
  importAsyncApiToConsumerAction,
  importMqbToConsumerAction,
  initConsumers,
  normalizeConsumerNames,
  normalizeConsumerMessage,
  restoreConsumerStateFromView,
  removeConsumerResponseHeader,
  sanitizeConsumerName,
  saveCurrentConsumerAction,
  selectConsumerSubtab,
  setConsumerMessageCaptureEnabledAction,
  setConsumerMessageCaptureKeepLastAction,
  setConsumerOutputModeAction,
  setConsumerOutputPublisherAction,
  toggleConsumerResponseHeader,
  toggleActiveConsumer,
  updateConsumerResponseHeader,
  updateConsumerResponsePayload,
} from "../../ui/src/lib/consumers-view";
import { consumersPanelState } from "../../ui/src/lib/stores";
import { createHyperscriptNode, installBaseWindowStubs, restoreBaseWindowStubs } from "./test-helpers";

function mountConsumersDom() {
  document.body.innerHTML = `
    <div id="consumers-container">
      <input id="cons-filter" />
      <button id="cons-add"></button>
      <button id="cons-copy"></button>
      <button id="cons-clone"></button>
      <button id="cons-save"></button>
      <button id="cons-delete"></button>
      <div id="cons-list"></div>
      <div id="cons-empty-alert"></div>
      <div id="cons-main-ui"></div>
      <div id="cons-sub-tabs">
        <button class="content-tab" id="ctab-def" data-target="cons-def-panel"></button>
        <button class="content-tab" id="cons-response-tab" data-target="cons-response-panel"></button>
        <button class="content-tab" id="ctab-msg" data-target="cons-msg-panel"></button>
      </div>
      <div id="cons-def-panel"></div>
      <div id="cons-response-panel"></div>
      <div id="cons-msg-panel"></div>
      <div id="cons-config-form"></div>
      <div id="cons-response-editor"></div>
      <div id="cons-live-title"></div>
      <table><tbody id="consumer-log-body"></tbody></table>
      <button id="cons-clear-history"></button>
      <button id="cons-toggle"></button>
      <div id="cons-list-pane"></div>
      <div id="cons-detail-pane"></div>
      <div id="cons-msg-detail-info"></div>
      <div id="cons-msg-details-content"></div>
      <button id="cons-msg-copy-btn"></button>
    </div>
  `;
}

function installConsumerWindowStubs() {
  installBaseWindowStubs();
  window.fetchConfigFromServer = vi.fn().mockResolvedValue({ consumers: [] });

  window.pollRuntimeStatus = vi.fn().mockImplementation(async () => {
    const current = (window._mqb_runtime_status || {
      active_consumers: [],
      active_routes: [],
      route_throughput: {},
      consumers: {},
    }) as any;
    window._mqb_runtime_status = current;
    return current;
  });
  window._mqb_active_tab = "consumers";
  window.location.hash = "#consumers:0";
  window._mqb_runtime_status = {
    active_consumers: [],
    active_routes: [],
    route_throughput: {},
    consumers: {},
  };
  window.appConfig = { consumers: [], routes: {} };
  window.initPublishers = vi.fn();
  window.Split = undefined;
  Object.defineProperty(window, "localStorage", {
    value: {
      getItem: vi.fn().mockReturnValue(null),
      setItem: vi.fn(),
    },
    configurable: true,
  });
  globalThis.fetch = vi.fn().mockResolvedValue({
    ok: true,
    json: async () => ({ running: false, status: { healthy: false } }),
  }) as any;
}

function createDeferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });

  return { promise, resolve };
}

describe("initConsumers", () => {
  beforeEach(() => {
    delete (window as any).__mqb_state;
    mountConsumersDom();
    installConsumerWindowStubs();
  });

  afterEach(() => {
    vi.useRealTimers();
    restoreBaseWindowStubs();
  });

  test("renders consumer names and waiting log state", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
        },
      ],
      routes: {},
      publishers: [],
    };
    const schema = {
      properties: {
        consumers: {
          items: {
            properties: {
              response: {},
            },
          },
        },
      },
      $defs: {
        HttpConfig: {
          properties: {
            custom_headers: {},
          },
        },
      },
    };

    await initConsumers(config, schema);

    const state = get(consumersPanelState);
    expect(state.items[0]?.name).toBe("orders_http");
    expect(state.groupedItems[0]).toMatchObject({ kind: "consumer", consumerIndex: 0, label: "orders_http" });
    expect(state.hasConsumers).toBe(true);
    expect(state.messages).toEqual([]);
  });

  test("shows empty state when there are no consumers", async () => {
    await initConsumers(
      {
        consumers: [],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {},
          },
        },
      },
    );

    const state = get(consumersPanelState);
    expect(state.hasConsumers).toBe(false);
    expect(state.items).toEqual([]);
  });

  test("creates new consumers with local defaults", async () => {
    const config = { consumers: [], routes: {}, publishers: [] };
    const schema = {
      properties: { consumers: { items: {} } },
      $defs: { HttpConfig: { properties: { custom_headers: {} } } },
    };

    await initConsumers(config, schema);

    await addConsumerAction("nats");

    expect(config.consumers).toHaveLength(1);
    expect(config.consumers[0].name).toBe("");
    expect(config.consumers[0].id).toEqual(expect.any(String));
    expect(config.consumers[0].endpoint).toMatchObject({
      nats: { url: "nats://localhost:4222", subject: "events.created" },
    });
  });

  test("applies an edited raw config and keeps the consumer id", async () => {
    const config = { consumers: [], routes: {}, publishers: [] } as any;
    const schema = { properties: { consumers: { items: {} } } };
    await initConsumers(config, schema);
    await addConsumerAction("nats");
    const id = config.consumers[0].id;

    const raw = (await currentConsumerConfigVariants())!.find((variant) => variant.id === "raw")!;
    expect(raw.editable).toBe(true);
    expect(raw.value).not.toHaveProperty("id");

    await applyCurrentConsumerRawConfig({ name: "edited", endpoint: { memory: { topic: "in" } } });

    expect(config.consumers[0]).toMatchObject({ id, name: "edited", endpoint: { memory: { topic: "in" } } });
    await expect(applyCurrentConsumerRawConfig({ name: "broken" })).rejects.toThrow("endpoint");
    expect(config.consumers[0].name).toBe("edited");
  });

  test("includes fallback endpoint metadata for unnamed consumers in the sidebar state", async () => {
    const config = {
      consumers: [
        {
          name: "",
          endpoint: { http: { url: "https://api.test", path: "/events", method: "POST" } },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [],
    };
    const schema = {
      properties: { consumers: { items: {} } },
      $defs: { HttpConfig: { properties: { custom_headers: {} } } },
    };

    await initConsumers(config, schema);

    const state = get(consumersPanelState);
    expect(state.items[0]).toMatchObject({
      name: "",
      displayName: "POST api.test /events",
    });
  });

  test("tracks response editor state for response-capable consumers", async () => {
    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: { headers: { "content-type": "application/json" }, payload: "{\"ok\":true}" },
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    selectConsumerSubtab("response");

    expect(get(consumersPanelState).activeSubtab).toBe("response");
    expect(get(consumersPanelState).responseHeaders).toEqual([
      { id: expect.any(Number), key: "content-type", value: "application/json", enabled: true },
    ]);
    expect(get(consumersPanelState).responsePayload).toBe("{\"ok\":true}");
  });

  test("switches consumer output modes and persists publisher selection", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [{ name: "orders_pub", endpoint: { http: { url: "https://example.com" } } }],
    };

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerOutputModeAction("publisher");
    expect(get(consumersPanelState).outputMode).toBe("publisher");
    expect(get(consumersPanelState).selectedPublisher).toEqual(expect.any(String));
    expect(config.consumers[0].output).toEqual({
      mode: "publisher",
      publisher: "orders_pub",
      publisher_id: expect.any(String),
    });

    setConsumerOutputModeAction("none");
    expect(get(consumersPanelState).outputMode).toBe("none");
    expect(config.consumers[0].output).toEqual({ mode: "none" });
  });

  test("saves static consumers with scalar endpoint values", async () => {
    const config = {
      consumers: [
        {
          name: "static",
          endpoint: { middlewares: [{ metrics: {} }], static: "hello" },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [],
    };
    let formChange: ((updated: unknown) => void) | null = null;
    window.VanillaSchemaForms.init = vi.fn().mockImplementation((_container, _schema, _data, onChange) => {
      formChange = onChange;
      return Promise.resolve({ setData: vi.fn() });
    });
    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    formChange?.({
      name: "static",
      endpoint: { middlewares: [{ metrics: {} }], static: {} },
      response: null,
      output: { mode: "none" },
    });

    await saveCurrentConsumerAction();

    const savedConsumers = (window.saveConfigSection as any).mock.calls.at(-1)?.[1];
    expect(savedConsumers[0].endpoint.static).toBe("");
    expect(savedConsumers[0].output).toEqual({ mode: "none" });
  });

  test("shows output controls for static consumers", async () => {
    const config = {
      consumers: [
        {
          name: "static",
          endpoint: { middlewares: [{ metrics: {} }], static: "hello" },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [{ name: "audit_pub", endpoint: { memory: { topic: "audit" } } }],
    };

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
      },
    );

    selectConsumerSubtab("response");

    expect(get(consumersPanelState).responseSupported).toBe(false);
    expect(get(consumersPanelState).activeSubtab).toBe("response");

    setConsumerOutputModeAction("publisher");

    expect(get(consumersPanelState).outputMode).toBe("publisher");
    expect(config.consumers[0].output).toEqual({
      mode: "publisher",
      publisher: "audit_pub",
      publisher_id: expect.any(String),
    });
  });

  test("captures blur-driven consumer edits on the first save click", async () => {
    const config = {
      consumers: [
        {
          name: "static",
          endpoint: { middlewares: [{ metrics: {} }], static: "initial" },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [],
    };
    let formChange: ((updated: unknown) => void) | null = null;
    window.VanillaSchemaForms.init = vi.fn().mockImplementation((_container, _schema, _data, onChange) => {
      formChange = onChange;
      return Promise.resolve({ setData: vi.fn() });
    });
    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    const activeInput = document.createElement("input");
    document.body.appendChild(activeInput);
    activeInput.focus();
    activeInput.blur = () => {
      queueMicrotask(() => {
        formChange?.({
          name: "static_renamed",
          endpoint: { middlewares: [{ metrics: {} }], static: "changed" },
          response: null,
          output: { mode: "none" },
        });
      });
    };

    await saveCurrentConsumerAction();

    const savedConsumers = (window.saveConfigSection as any).mock.calls.at(-1)?.[1];
    expect(savedConsumers[0].name).toBe("static_renamed");
    expect(savedConsumers[0].endpoint.static).toBe("changed");
  });

  test("keeps publisher mode visible until a publisher is selected", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [
        { name: "orders_pub", endpoint: { memory: { topic: "orders" } } },
        { name: "audit_pub", endpoint: { memory: { topic: "audit" } } },
      ],
    };

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerOutputModeAction("publisher");

    expect(get(consumersPanelState).outputMode).toBe("publisher");
    expect(get(consumersPanelState).publisherOptions).toEqual([
      { value: expect.any(String), label: "orders_pub" },
      { value: expect.any(String), label: "audit_pub" },
    ]);
    expect(get(consumersPanelState).selectedPublisher).toBe("");
    expect(config.consumers[0].output).toEqual({ mode: "publisher", publisher: "" });
  });

  test("reads publisher options from the live app config when local consumer config is stale", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [],
    };
    window.appConfig = {
      consumers: config.consumers,
      routes: {},
      publishers: [{ name: "orders_pub", endpoint: { memory: { topic: "orders" } } }],
    } as any;

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerOutputModeAction("publisher");

    expect(get(consumersPanelState).publisherOptions).toEqual([
      { value: expect.any(String), label: "orders_pub" },
    ]);
    expect(get(consumersPanelState).selectedPublisher).toEqual(expect.any(String));
    expect(config.consumers[0].output).toEqual({
      mode: "publisher",
      publisher: "orders_pub",
      publisher_id: expect.any(String),
    });
  });

  test("uses endpoint details for unnamed and duplicate publisher output options", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [
        { id: "pub-empty", name: "", endpoint: { memory: { topic: "audit" } } },
        { id: "pub-http-a", name: "duplicate", endpoint: { http: { method: "POST", url: "https://api.test", path: "/orders" } } },
        { id: "pub-http-b", name: "duplicate", endpoint: { http: { method: "GET", url: "https://api.test/status?ready=true" } } },
        { id: "pub-amqp", name: "", endpoint: { amqp: { url: "amqp://localhost:5672/%2f", queue: "jobs" } } },
        { id: "pub-mqtt", name: "", endpoint: { mqtt: { url: "tcp://localhost:1883", topic: "events/updates" } } },
      ],
    };

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
      },
    );

    setConsumerOutputModeAction("publisher");

    expect(get(consumersPanelState).publisherOptions).toEqual([
      { value: "pub-empty", label: "audit" },
      { value: "pub-http-a", label: "POST api.test /orders" },
      { value: "pub-http-b", label: "GET api.test/status?ready=true" },
      { value: "pub-amqp", label: "localhost:5672/%2f jobs" },
      { value: "pub-mqtt", label: "localhost:1883 events/updates" },
    ]);

    setConsumerOutputPublisherAction("pub-http-b");

    expect(get(consumersPanelState).selectedPublisher).toBe("pub-http-b");
    expect(config.consumers[0].output).toEqual({
      mode: "publisher",
      publisher: "duplicate",
      publisher_id: "pub-http-b",
    });
  });

  test("keeps consumer output unchanged when publisher mode is selected without publishers", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [],
    };

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerOutputModeAction("publisher");

    expect(window.mqbAlert).toHaveBeenCalledWith("Create or select a publisher first.");
    expect(get(consumersPanelState).outputMode).toBe("none");
    expect(config.consumers[0].output).toEqual({ mode: "none" });
  });

  test("saves consumer output configuration instead of dropping it", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [
        { name: "orders_pub", endpoint: { memory: { topic: "orders" } } },
        { name: "audit_pub", endpoint: { memory: { topic: "audit" } } },
      ],
    };
    (window.saveConfigSection as any).mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerOutputModeAction("publisher");
    const auditOption = get(consumersPanelState).publisherOptions.find((option) => option.label === "audit_pub");
    setConsumerOutputPublisherAction(auditOption?.value || "");
    await saveCurrentConsumerAction();

    const savedConsumers = (window.saveConfigSection as any).mock.calls.at(-1)?.[1];
    expect(savedConsumers[0].output).toEqual({
      mode: "publisher",
      publisher: "audit_pub",
      publisher_id: expect.any(String),
    });
    expect(savedConsumers[0].response).toBeNull();
  });

  test("keeps output mode none on save after form edits", async () => {
    const config = {
      consumers: [
        {
          name: "static",
          endpoint: { middlewares: [{ metrics: {} }], static: "initial" },
          response: null,
          output: { mode: "publisher", publisher: "audit_pub" },
        },
      ],
      routes: {},
      publishers: [{ name: "audit_pub", endpoint: { memory: { topic: "audit" } } }],
    };
    let formChange: ((updated: unknown) => void) | null = null;
    window.VanillaSchemaForms.init = vi.fn().mockImplementation((_container, _schema, _data, onChange) => {
      formChange = onChange;
      return Promise.resolve({ setData: vi.fn() });
    });
    window.saveConfigSection = vi.fn().mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerOutputModeAction("none");
    formChange?.({
      name: "static",
      endpoint: { middlewares: [{ metrics: {} }], static: "changed" },
      response: null,
      output: { mode: "publisher", publisher: "" },
    });

    await saveCurrentConsumerAction();

    const savedConsumers = (window.saveConfigSection as any).mock.calls.at(-1)?.[1];
    expect(savedConsumers[0].output).toEqual({ mode: "none" });
    expect(savedConsumers[0].response).toBeNull();
    expect(savedConsumers[0].endpoint.static).toBe("changed");
  });

  test("persists message capture settings and trims the stored buffer", async () => {
    (window.localStorage.getItem as any).mockReturnValue(JSON.stringify({
      orders_http: Array.from({ length: 20 }, (_, index) => ({ payload: `message-${index}` })),
    }));
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [],
    };
    (window.saveConfigSection as any).mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
                message_capture: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerMessageCaptureEnabledAction(false);
    setConsumerMessageCaptureKeepLastAction(10);
    await saveCurrentConsumerAction();

    const savedConsumers = (window.saveConfigSection as any).mock.calls.at(-1)?.[1];
    expect(savedConsumers[0].message_capture).toEqual({ enabled: false, keep_last: 10 });
    expect(window.localStorage.setItem).toHaveBeenCalledWith(
      "mqb_consumer_messages",
      expect.stringContaining("message-9"),
    );
    const persistedPayload = (window.localStorage.setItem as any).mock.calls.at(-1)?.[1] as string;
    const persistedMessages = JSON.parse(persistedPayload);
    expect(Object.values(persistedMessages)[0]).toHaveLength(10);
  });

  test("falls back to one message when capture keep-last input is invalid", async () => {
    (window.localStorage.getItem as any).mockReturnValue(JSON.stringify({
      orders_http: [{ payload: "message-0" }, { payload: "message-1" }],
    }));
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          output: { mode: "none" },
        },
      ],
      routes: {},
      publishers: [],
    };
    (window.saveConfigSection as any).mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                output: {},
                message_capture: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    setConsumerMessageCaptureKeepLastAction(Number.NaN);
    await saveCurrentConsumerAction();

    const savedConsumers = (window.saveConfigSection as any).mock.calls.at(-1)?.[1];
    expect(savedConsumers[0].message_capture.keep_last).toBe(1);
    expect(get(consumersPanelState).messageCaptureKeepLast).toBe(1);
  });

  // A route that drained and one that died both report `running: false`; only the
  // outcome separates a finished batch job from a failure.
  test.each([
    ["completed", "Completed", "neutral", "status-off"],
    ["failed", "connection refused", "danger", "status-error"],
  ])("renders a %s route distinctly from a stopped one", async (outcome, text, variant, statusClass) => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
          message_capture: { enabled: false, keep_last: 10 },
        },
      ],
      routes: {},
      publishers: [],
    };

    await initConsumers(config as any, {
      properties: {
        consumers: {
          items: {
            properties: {
              response: {},
              message_capture: {},
            },
          },
        },
      },
    } as any);

    const runtimeKey = String((config.consumers[0] as any)?.id || "orders_http");
    window._mqb_runtime_status = {
      active_consumers: [],
      active_routes: [],
      route_throughput: {},
      consumers: {
        [runtimeKey]: {
          running: false,
          outcome,
          status: { healthy: true, error: outcome === "failed" ? "connection refused" : null },
          message_sequence: 3,
          capture_enabled: false,
          capture_keep_last: 0,
        },
      },
    } as any;
    window.renderConsumersRuntimeStatus?.();

    expect(get(consumersPanelState).liveStatusText).toBe(text);
    expect(get(consumersPanelState).liveStatusVariant).toBe(variant);
    expect(get(consumersPanelState).items[0]?.statusClass).toBe(statusClass);
    // Ended is not running: the button offers a restart either way.
    expect(get(consumersPanelState).toggleLabel).toBe("Start");
  });

  test("skips message fetches when capture is disabled", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ orders_http: [{ payload: "ignored" }] }),
    });
    globalThis.fetch = fetchMock as any;
    window._mqb_runtime_status = {
      active_consumers: ["orders_http"],
      active_routes: [],
      route_throughput: {},
      consumers: {
        orders_http: {
          running: true,
          status: { healthy: true },
          message_sequence: 3,
          capture_enabled: false,
          capture_keep_last: 10,
        },
      },
    };

    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
            message_capture: { enabled: false, keep_last: 10 },
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
                message_capture: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    await Promise.resolve();
    await Promise.resolve();

    expect(fetchMock).not.toHaveBeenCalledWith("/messages?consumer_id=orders_http");
    expect(get(consumersPanelState).messageCaptureEnabled).toBe(false);
    expect(get(consumersPanelState).messageCaptureKeepLast).toBe(10);
  });

  test("polling refreshes consumer status and reschedules message polling", async () => {
    const scheduled: Array<() => void | Promise<void>> = [];
    const setTimeoutSpy = vi.spyOn(window, "setTimeout").mockImplementation(((handler: TimerHandler) => {
      if (typeof handler === "function") {
        scheduled.push(handler as () => void);
      }
      return 1 as any;
    }) as any);
    globalThis.fetch = vi.fn().mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.startsWith("/messages")) {
        return {
          ok: true,
          json: async () => ({
            orders_http: [{ ok: true, source: "memory" }],
          }),
        } as any;
      }
      return {
        ok: true,
        json: async () => ({}),
      } as any;
    }) as any;

    try {
      const config = {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      };
      await initConsumers(
        config,
        {
          properties: {
            consumers: {
              items: {
                properties: {
                  response: {},
                },
              },
            },
          },
          $defs: {
            HttpConfig: {
              properties: {
                custom_headers: {},
              },
            },
          },
        },
      );
      const runtimeKey = String(config.consumers[0]?.id || "orders_http");
      window._mqb_runtime_status = {
        active_consumers: [runtimeKey],
        active_routes: [],
        route_throughput: {},
        consumers: {
          [runtimeKey]: {
            running: true,
            status: { healthy: true },
            message_sequence: 1,
          },
        },
      };
      window.renderConsumersRuntimeStatus?.();

      expect(scheduled.length).toBeGreaterThan(0);
      for (let i = 0; i < 10 && scheduled.length > 0; i += 1) {
        const callback = scheduled.shift();
        if (callback) {
          await callback();
        }
      }
      await Promise.resolve();
      await Promise.resolve();

      expect(get(consumersPanelState).items[0]?.statusClass).toBe("status-ok");
      expect(globalThis.fetch).not.toHaveBeenCalledWith(`/consumer-status?consumer_id=${runtimeKey}`);
    } finally {
      setTimeoutSpy.mockRestore();
    }
  });

  test("normalizes raw backend message payloads into displayable consumer messages", () => {
    expect(normalizeConsumerMessage({ ok: true, source: "memory" }, "2026-04-30T12:00:00.000Z")).toEqual({
      payload: { ok: true, source: "memory" },
      time: "2026-04-30T12:00:00.000Z",
    });
  });

  test("copy consumer creates a publisher", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: { url: "127.0.0.1:8080" } },
          response: null,
        },
      ],
      routes: {},
      publishers: [],
    };
    window.mqbPrompt = vi.fn().mockResolvedValue("orders_publisher");

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    await copyCurrentConsumerAction();
    expect(config.publishers).toEqual([
      expect.objectContaining({
        name: "orders_publisher",
        endpoint: expect.objectContaining({
          http: expect.objectContaining({ url: "http://127.0.0.1:8080" }),
        }),
      }),
    ]);
  });

  test("copy consumer creates publisher client URLs with scheme and path", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: { url: "127.0.0.1:8080", path: "/orders" } },
          response: null,
        },
        {
          name: "orders_ws",
          endpoint: { middlewares: [{ metrics: {} }], websocket: { url: "127.0.0.1:8081", path: "/socket" } },
          response: null,
        },
        {
          name: "orders_grpc",
          endpoint: { middlewares: [{ metrics: {} }], grpc: { url: "127.0.0.1:50051" } },
          response: null,
        },
      ],
      routes: {},
      publishers: [],
    };
    window.mqbPrompt = vi.fn()
      .mockResolvedValueOnce("orders_http_publisher")
      .mockResolvedValueOnce("orders_ws_publisher")
      .mockResolvedValueOnce("orders_grpc_publisher");

    await initConsumers(config, {
      properties: { consumers: { items: { properties: { response: {} } } } },
      $defs: { HttpConfig: { properties: { custom_headers: {} } } },
    });

    await copyCurrentConsumerAction();
    await restoreConsumerStateFromView(1, { tab: "definition" });
    await copyCurrentConsumerAction();
    await restoreConsumerStateFromView(2, { tab: "definition" });
    await copyCurrentConsumerAction();

    expect(config.publishers.map((publisher) => publisher.endpoint)).toEqual([
      expect.objectContaining({ http: expect.objectContaining({ url: "http://127.0.0.1:8080/orders" }) }),
      expect.objectContaining({ websocket: expect.objectContaining({ url: "ws://127.0.0.1:8081/socket" }) }),
      expect.objectContaining({ grpc: expect.objectContaining({ url: "grpc://127.0.0.1:50051" }) }),
    ]);
  });

  test("restoring a consumer updates the remembered index and hash", async () => {
    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
          {
            name: "events_memory",
            endpoint: { middlewares: [{ metrics: {} }], memory: { topic: "events" } },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    await restoreConsumerStateFromView(1, { tab: "definition" });

    expect(get(consumersPanelState).selectedIndex).toBe(1);
    expect(window.location.hash).toBe("#consumers:1");
    expect((window as any)._mqb_last_consumer_idx).toBe(1);
    expect((window as any)._mqb_last_consumer_tab).toBe("definition");
  });

  test("init restores the selected consumer from the URL hash on reload", async () => {
    window.location.hash = "#consumers:1";

    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
          {
            name: "events_memory",
            endpoint: { middlewares: [{ metrics: {} }], memory: { topic: "events" } },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    expect(get(consumersPanelState).selectedIndex).toBe(1);
    expect((window as any)._mqb_last_consumer_idx).toBe(1);
  });

  test("save keeps the selected consumer and subtab", async () => {
    let resolveSave: ((value: any) => void) | null = null;
    window.saveConfigSection = vi.fn().mockImplementation(
      async () =>
        await new Promise((resolve) => {
          resolveSave = resolve;
        }),
    );

    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
          {
            name: "events_memory",
            endpoint: { middlewares: [{ metrics: {} }], memory: { topic: "events" } },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    await restoreConsumerStateFromView(1, { tab: "definition" });
    const savePromise = saveCurrentConsumerAction();
    await Promise.resolve();
    resolveSave?.({
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          output: { mode: "none" },
          response: null,
        },
        {
          name: "events_memory",
          endpoint: { middlewares: [{ metrics: {} }], memory: { topic: "events" } },
          output: { mode: "none" },
          response: null,
        },
      ],
    });
    await savePromise;
    await Promise.resolve();

    expect(get(consumersPanelState).selectedIndex).toBe(1);
    expect(get(consumersPanelState).activeSubtab).toBe("definition");
    expect((window as any)._mqb_last_consumer_idx).toBe(1);
    expect((window as any)._mqb_last_consumer_tab).toBe("definition");
  });

  test("throughput label is only shown while consumer is running", async () => {
    globalThis.fetch = vi.fn().mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.startsWith("/consumer-status")) {
        return {
          ok: true,
          json: async () => ({ running: false, status: { healthy: false } }),
        } as any;
      }
      if (url.startsWith("/messages")) {
        return {
          ok: true,
          json: async () => ({ orders_http: [] }),
        } as any;
      }
      return { ok: true, json: async () => ({}) } as any;
    }) as any;

    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    const state = get(consumersPanelState);
    expect(state.items[0]?.throughputLabel).toBe("");
  });

  test("shows throughput from runtime status while new messages are fetched", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2025-01-01T00:00:00Z"));
    let runtimeKey = "orders_http";

    globalThis.fetch = vi.fn().mockImplementation(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.startsWith("/messages")) {
        return {
          ok: true,
          json: async () => ({ [runtimeKey]: [{ payload: "first" }] }),
        } as any;
      }
      return { ok: true, json: async () => ({}) } as any;
    }) as any;

    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
        },
      ],
      routes: {},
      publishers: [],
    };

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );
    runtimeKey = String(config.consumers[0]?.id || "orders_http");
    window._mqb_runtime_status = {
      active_consumers: [runtimeKey],
      active_routes: [],
      route_throughput: {},
      consumers: {
        [runtimeKey]: {
          running: true,
          status: { healthy: true },
          message_sequence: 1,
        },
      },
    };
    window.renderConsumersRuntimeStatus?.();

    await Promise.resolve();
    window._mqb_runtime_status.consumers[runtimeKey].message_sequence = 2;
    window._mqb_runtime_status.consumers[runtimeKey].throughput = 30;
    vi.setSystemTime(new Date("2025-01-01T00:00:01Z"));
    await vi.advanceTimersByTimeAsync(2000);
    await Promise.resolve();

    expect(get(consumersPanelState).items[0]?.throughputLabel).toBe("30.0 msg/s");
  });

  test("keeps editable response header rows in local state and persists filtered headers", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
        },
      ],
      routes: {},
      publishers: [],
    };

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    addConsumerResponseHeader();
    expect(get(consumersPanelState).responseHeaders).toEqual([
      { id: expect.any(Number), key: "", value: "", enabled: true },
    ]);

    updateConsumerResponseHeader(0, "key", "x-test");
    updateConsumerResponseHeader(0, "value", "ready");
    updateConsumerResponsePayload("ready");

    expect(get(consumersPanelState).responseHeaders).toEqual([
      { id: expect.any(Number), key: "x-test", value: "ready", enabled: true },
    ]);
    expect(get(consumersPanelState).responsePayload).toBe("ready");
    expect(config.consumers[0].response).toEqual({ payload: "ready", headers: { "x-test": "ready" } });
    expect(config.consumers[0].output).toEqual({
      mode: "response",
      response: { payload: "ready", headers: { "x-test": "ready" } },
    });

    toggleConsumerResponseHeader(0, false);

    expect(get(consumersPanelState).responseHeaders).toEqual([
      { id: expect.any(Number), key: "x-test", value: "ready", enabled: false },
    ]);
    expect(config.consumers[0].response).toEqual({ payload: "ready", headers: {} });
    expect(config.consumers[0].output).toEqual({
      mode: "response",
      response: { payload: "ready", headers: {} },
    });

    removeConsumerResponseHeader(0);

    expect(get(consumersPanelState).responseHeaders).toEqual([]);
    expect(config.consumers[0].response).toEqual({ payload: "ready", headers: {} });
  });

  test("imports AsyncAPI requests as http consumers", async () => {
    const config = {
      consumers: [],
      routes: {},
      publishers: [],
    };
    (window.saveConfigSection as any).mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: { items: {} },
        },
        $defs: {
          HttpConfig: { properties: { custom_headers: {} } },
        },
      },
    );

    const asyncApi = JSON.stringify({
      asyncapi: "3.0.0",
      channels: {
        userSignedUp: { address: "/events/user/signed-up" },
      },
      operations: {
        onUserSignedUp: {
          action: "receive",
          channel: { $ref: "#/channels/userSignedUp" },
          messages: [],
          title: "User signup event",
        },
      },
      servers: {
        local: { host: "localhost:3000", protocol: "http" },
      },
    });

    await importAsyncApiToConsumerAction(asyncApi);

    expect(config.consumers.length).toBe(1);
    expect(config.consumers[0].name).toContain("user_signup_event");
    expect(config.consumers[0].endpoint.http.url).toBe("0.0.0.0:8080");
    expect(config.consumers[0].endpoint.http.path).toBe("/events/user/signed-up");
  });

  test("imports mq-bridge config consumers and keeps names unique", async () => {
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: { url: "0.0.0.0:3000", path: "/orders" } },
          response: null,
        },
      ],
      routes: {},
      publishers: [],
    };
    (window.saveConfigSection as any).mockImplementation(async (_section: string, consumers: any[]) => ({ consumers }));

    await initConsumers(
      config,
      {
        properties: {
          consumers: { items: {} },
        },
        $defs: {
          HttpConfig: { properties: { custom_headers: {} } },
        },
      },
    );

    const mqbImport = JSON.stringify({
      type: "mqb-export",
      config: {
        consumers: [
          {
            name: "orders_http",
            endpoint: { http: { url: "0.0.0.0:4000", path: "/orders/new" } },
          },
        ],
      },
    });

    await importMqbToConsumerAction(mqbImport);

    expect(config.consumers.length).toBe(2);
    expect(config.consumers[1].name).toMatch(/^orders_http/);
    expect(config.consumers[1].name).not.toBe("orders_http");
    expect(config.consumers[1].endpoint.http.url).toBe("0.0.0.0:4000");
    expect(config.consumers[1].endpoint.http.path).toBe("/orders/new");
  });

  test("shows starting state while toggling the active consumer", async () => {
    const startRequest = createDeferred<{ ok: boolean }>();
    let statusCallCount = 0;
    const fetchMock = vi.fn().mockImplementation((input: string) => {
      if (input.startsWith("/consumer-start")) {
        return startRequest.promise;
      }

      return Promise.resolve({
        ok: true,
        json: async () => ({ running: false, status: { healthy: false } }),
      });
    });
    globalThis.fetch = fetchMock as any;
    window._mqb_active_tab = "publishers";
    const config = {
      consumers: [
        {
          name: "orders_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          response: null,
        },
      ],
      routes: {},
      publishers: [],
    };
    window.pollRuntimeStatus = vi.fn().mockImplementation(async () => {
      statusCallCount += 1;
      const runtimeKey = String(config.consumers[0]?.id || "orders_http");
      window._mqb_runtime_status = {
        active_consumers: [runtimeKey],
        active_routes: [],
        route_throughput: {},
        consumers: {
          [runtimeKey]: {
            running: statusCallCount > 0,
            status: { healthy: statusCallCount > 0 },
            message_sequence: 0,
          },
        },
      };
      return window._mqb_runtime_status;
    });

    await initConsumers(
      config,
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    toggleActiveConsumer();

    expect(get(consumersPanelState).toggleBusy).toBe(true);
    expect(get(consumersPanelState).toggleLabel).toBe("Starting...");

    startRequest.resolve({ ok: true });
    await Promise.resolve();
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(get(consumersPanelState).toggleBusy).toBe(false);
    expect(get(consumersPanelState).toggleLabel).toBe("Stop");
  });

  test("saves unsaved consumers before attempting to start them", async () => {
    const startRequest = createDeferred<{ ok: boolean }>();
    const savedConsumers = [
      {
        name: "orders_http",
        endpoint: { middlewares: [{ metrics: {} }], http: {} },
        response: null,
      },
    ];

    window.saveConfigSection = vi.fn().mockResolvedValue({ consumers: savedConsumers });
    globalThis.fetch = vi.fn().mockImplementation((input: string) => {
      if (input.startsWith("/consumer-start")) {
        return startRequest.promise;
      }

      if (input.startsWith("/consumer-status")) {
        return Promise.resolve({
          ok: true,
          json: async () => ({ running: true, status: { healthy: true } }),
        });
      }

      return Promise.resolve({
        ok: true,
        json: async () => ({ running: false, status: { healthy: false } }),
      });
    }) as any;
    window._mqb_saved_sections = { consumers: [] } as any;
    window._mqb_active_tab = "publishers";

    await initConsumers(
      {
        consumers: [...savedConsumers],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    toggleActiveConsumer();

    await Promise.resolve();
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(window.saveConfigSection).toHaveBeenCalledWith(
      "consumers",
      expect.any(Array),
      false,
      undefined,
    );
    expect(globalThis.fetch).toHaveBeenCalledWith("/consumer-start?consumer_id=orders_http", { method: "POST" });

    startRequest.resolve({ ok: true });
    await Promise.resolve();
    await Promise.resolve();
  });

  test("sanitizes unsafe consumer names before saving and starting", async () => {
    const startRequest = createDeferred<{ ok: boolean }>();
    const fetchMock = vi.fn().mockImplementation((input: string) => {
      if (input.startsWith("/consumer-start")) {
        return startRequest.promise;
      }

      if (input.startsWith("/consumer-status")) {
        return Promise.resolve({
          ok: true,
          json: async () => ({ running: true, status: { healthy: true } }),
        });
      }

      return Promise.resolve({
        ok: true,
        json: async () => ({ running: false, status: { healthy: false } }),
      });
    });

    globalThis.fetch = fetchMock as any;
    window._mqb_active_tab = "publishers";
    window._mqb_saved_sections = { consumers: [] } as any;
    window.saveConfigSection = vi.fn().mockImplementation(async (_sectionName, sectionValue) => ({
      consumers: structuredClone(sectionValue),
    }));

    await initConsumers(
      {
        consumers: [
          {
            name: "Mémory Consumer 2",
            endpoint: { middlewares: [{ metrics: {} }], memory: {} },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    toggleActiveConsumer();

    await Promise.resolve();
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(window.saveConfigSection).toHaveBeenCalledWith(
      "consumers",
      [
        expect.objectContaining({
          id: expect.any(String),
          name: "Mémory Consumer 2",
          endpoint: { middlewares: [{ metrics: {} }], memory: {} },
          message_capture: { enabled: true, keep_last: 100 },
          output: { mode: "none" },
          response: null,
        }),
      ],
      false,
      undefined,
    );
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringMatching(/^\/consumer-start\?consumer_id=consumer_/),
      { method: "POST" },
    );

    startRequest.resolve({ ok: true });
    await Promise.resolve();
    await Promise.resolve();
  });

  test("saves through shared consumer action", async () => {
    window.saveConfigSection = vi.fn().mockResolvedValue({
      consumers: [{ name: "orders_http", endpoint: { middlewares: [{ metrics: {} }], http: {} }, response: null }],
    });
    window.fetchConfigFromServer = vi.fn();

    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    await saveCurrentConsumerAction();

    expect(window.saveConfigSection).toHaveBeenCalledWith(
      "consumers",
      expect.any(Array),
      false,
      undefined,
    );
    expect(window.fetchConfigFromServer).not.toHaveBeenCalled();
  });

  test("surfaces backend start errors in the UI", async () => {
    globalThis.fetch = vi.fn().mockImplementation((input: string) => {
      if (input.startsWith("/consumer-start")) {
        return Promise.resolve({
          ok: false,
          text: async () =>
            "Internal Server Error: route 'ui_collector_route_http' failed: Invalid listen address: http://localhost:1234/api/orders/updated. Reconnecting in 5 seconds...",
        });
      }

      if (input.startsWith("/consumer-status")) {
        return Promise.resolve({
          ok: true,
          json: async () => ({ running: false, status: { healthy: false } }),
        });
      }

      return Promise.resolve({
        ok: true,
        json: async () => ({ running: false, status: { healthy: false } }),
      });
    }) as any;
    window._mqb_active_tab = "publishers";

    await initConsumers(
      {
        consumers: [
          {
            name: "orders_http",
            endpoint: { middlewares: [{ metrics: {} }], http: { url: "0.0.0.0:1234" } },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    toggleActiveConsumer();
    await Promise.resolve();
    await Promise.resolve();

    expect(window.mqbAlert).toHaveBeenCalledWith(
      "Internal Server Error: route 'ui_collector_route_http' failed: Invalid listen address: http://localhost:1234/api/orders/updated. Reconnecting in 5 seconds...",
    );
    expect(get(consumersPanelState).liveStatusVariant).toBe("danger");
    expect(get(consumersPanelState).liveStatusText).toContain("Invalid listen address");
  });

  test("does not poll status for unsaved consumers", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ running: false, status: { healthy: false } }),
    });
    globalThis.fetch = fetchMock as any;
    window._mqb_saved_sections = {
      consumers: [{
        name: "saved_http",
        endpoint: { middlewares: [{ metrics: {} }], http: {} },
        output: { mode: "none" },
        response: null,
      }],
    } as any;

    await initConsumers(
      {
        consumers: [
          {
            name: "saved_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
          {
            name: "memory_consumer 2",
            endpoint: { middlewares: [{ metrics: {} }], memory: {} },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      },
      {
        properties: {
          consumers: {
            items: {
              properties: {
                response: {},
              },
            },
          },
        },
        $defs: {
          HttpConfig: {
            properties: {
              custom_headers: {},
            },
          },
        },
      },
    );

    expect(fetchMock).not.toHaveBeenCalledWith("/messages?consumer_id=memory_consumer%202");
  });

  test("freezes the initial saved baseline before later local edits", async () => {
    vi.useFakeTimers();
    try {
      window._mqb_active_tab = "publishers";
      window.location.hash = "#publishers";
      const config = {
        consumers: [
          {
            name: "saved_http",
            endpoint: { middlewares: [{ metrics: {} }], http: {} },
            response: null,
          },
        ],
        routes: {},
        publishers: [],
      };

      const initPromise = initConsumers(
        config,
        {
          properties: {
            consumers: {
              items: {
                properties: {
                  response: {},
                },
              },
            },
          },
          $defs: {
            HttpConfig: {
              properties: {
                custom_headers: {},
              },
            },
          },
        },
      );

      config.consumers.push({
        name: "memory_consumer 2",
        endpoint: { middlewares: [{ metrics: {} }], memory: {} },
        response: null,
      });

      await initPromise;
      await vi.runOnlyPendingTimersAsync();

      expect(window.markSectionSaved).toHaveBeenCalledWith("consumers", expect.arrayContaining([
        expect.objectContaining({
          id: expect.any(String),
          name: "saved_http",
          endpoint: { middlewares: [{ metrics: {} }], http: {} },
          message_capture: { enabled: true, keep_last: 100 },
          output: { mode: "none" },
          response: null,
        }),
      ]));
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("consumer name normalization", () => {
  test("sanitizes whitespace and non-ascii characters", () => {
    expect(sanitizeConsumerName(" Mémory Consumer 2 ")).toBe("Memory_Consumer_2");
  });

  test("deduplicates normalized consumer names", () => {
    const consumers = [
      { name: "Mémory Consumer", endpoint: {}, response: null },
      { name: "Memory   Consumer", endpoint: {}, response: null },
    ];

    const result = normalizeConsumerNames(consumers as any, 1);

    expect(result.changed).toBe(true);
    expect(consumers.map((consumer) => consumer.name)).toEqual(["Memory_Consumer", "Memory_Consumer_1"]);
    expect(result.selectedName).toBe("Memory_Consumer_1");
  });
});
