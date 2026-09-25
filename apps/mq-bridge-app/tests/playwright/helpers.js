const { expect } = require("@playwright/test");

/**
 * The address the *config* claims, which is no longer the address the app is
 * actually on: each worker's server binds a port of its own (app-server.js).
 * It survives because the settings view renders it and a screenshot baseline
 * pins it — treat it as display data, not as somewhere to connect.
 */
const DISPLAY_UI_ADDR = "127.0.0.1:39091";

/**
 * The port used by HTTP endpoints in test configs. Only the delivery test in
 * ui.spec.js actually binds it; everywhere else it is text on screen.
 */
const DATA_ADDR = "127.0.0.1:39081";

/**
 * The shell every spec's config needs, so a spec declares only the publishers,
 * consumers and routes its own assertions care about.
 */
function makeConfig(overrides = {}) {
  return {
    log_level: "info",
    ui_addr: DISPLAY_UI_ADDR,
    metrics_addr: "",
    default_tab: "publishers",
    routes: {},
    consumers: [],
    publishers: [],
    ...overrides,
  };
}

/**
 * A populated workspace: one disabled route, one consumer, two publishers.
 *
 * The ids are pinned rather than left out. `id` defaults to a fresh UUID on
 * every deserialization (config.rs), so a config posted without one gives the
 * consumer a different id on each reset — and a request already in flight
 * against the previous id then 404s. The sweep hits this: it advances as soon
 * as a click shows an effect, so the next button's reset can land between a
 * Start handler's save and the `/consumer-start` that follows it.
 */
const BASE_CONFIG = makeConfig({
  routes: {
    ingest_http: {
      enabled: false,
      input: {
        middlewares: [{ metrics: {} }],
        http: { url: DATA_ADDR },
      },
      output: { memory: { topic: "route-output" } },
    },
  },
  consumers: [
    {
      id: "00000000-0000-7000-8000-00000000c001",
      name: "memory_consumer",
      comment: "Demo consumer comment",
      endpoint: {
        middlewares: [{ metrics: {} }],
        memory: { topic: "consumer-events" },
      },
      response: {
        headers: { "x-initial": "test" },
        payload: "ok",
      },
    },
  ],
  publishers: [
    {
      id: "00000000-0000-7000-8000-00000000b001",
      name: "http_publisher",
      comment: "Demo publisher comment",
      endpoint: {
        middlewares: [{ metrics: {} }],
        http: { url: "http://localhost:8080/api/orders" },
      },
    },
    {
      id: "00000000-0000-7000-8000-00000000b002",
      name: "memory_publisher",
      comment: "Queue publisher comment",
      endpoint: {
        middlewares: [{ metrics: {} }],
        memory: { topic: "publisher-events" },
      },
    },
  ],
});

async function resetConfig(page, config = BASE_CONFIG) {
  const response = await page.request.post("/config", { data: config });
  expect(response.ok()).toBeTruthy();
}

async function readConfig(page) {
  const response = await page.request.get("/config");
  expect(response.ok()).toBeTruthy();
  return response.json();
}

/**
 * Navigate to a hash view and wait for the shell to finish booting. Cannot use
 * networkidle: the runtime poller keeps a request in flight every second.
 */
async function gotoView(page, hash) {
  await page.goto(`/${hash}`);
  await waitForShell(page);
}

/**
 * The tab strip mounts before bootstrap has loaded /config, so also wait for
 * the ready marker; otherwise actions run against an empty config.
 */
async function waitForShell(page) {
  await expect(page.locator("html[data-mqb-ready]")).toBeAttached();
  await expect(page.locator("#mainTabs")).toBeVisible();
  await expect(page.locator(".tab-content-panel.active")).toBeVisible();
}

module.exports = {
  BASE_CONFIG,
  DATA_ADDR,
  DISPLAY_UI_ADDR,
  makeConfig,
  resetConfig,
  readConfig,
  gotoView,
  waitForShell,
};
