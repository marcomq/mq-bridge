/**
 * App Config toolbar: Export, Import, Reset and the JSON view. None of these had
 * coverage, and every one of them either writes the whole config or leaves the
 * browser sandbox, so a silent failure is invisible in the UI.
 */
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");

const { test, expect } = require("./fixtures");
const { resetConfig, readConfig, gotoView } = require("./helpers");

const ACTIONS = {
  export: "Export app config + env vars",
  import: "Import app config and merge data",
  reset: "Reset publishers and consumers",
};

const action = (page, title) => page.locator(`#form-actions button[title="${title}"]`);
const mqbDialog = (page) => page.locator("wa-dialog.mqb-dialog");

test.beforeEach(async ({ page }) => {
  await resetConfig(page);
});

test("config toolbar actions are visible and enabled", async ({ page }) => {
  await gotoView(page, "#config");
  await expect(page.locator("#form-container")).toBeVisible();

  for (const title of Object.values(ACTIONS)) {
    const button = action(page, title);
    await expect(button, `"${title}" should be reachable`).toBeVisible();
    await expect(button).toBeEnabled();
  }
  await expect(page.locator("#js-show-json")).toBeVisible();
});

test("Export downloads a bundle containing the current publishers", async ({ page }) => {
  await gotoView(page, "#config");

  const downloadPromise = page.waitForEvent("download", { timeout: 10_000 });
  await action(page, ACTIONS.export).click();
  const download = await downloadPromise;

  expect(download.suggestedFilename()).toMatch(/^mqb-export-.*\.json$/);
  const bundle = JSON.parse(fs.readFileSync(await download.path(), "utf8"));
  expect(bundle.type).toBe("mqb-export");
  expect(bundle.config.publishers.map((entry) => entry.name)).toContain("http_publisher");
  expect(bundle.config.consumers.map((entry) => entry.name)).toContain("memory_consumer");
});

test("Import opens a file picker", async ({ page }) => {
  await gotoView(page, "#config");

  const chooserPromise = page.waitForEvent("filechooser", { timeout: 10_000 });
  await action(page, ACTIONS.import).click();
  expect(await chooserPromise).toBeTruthy();
});

test("Import merges publishers from a bundle and persists them", async ({ page }) => {
  await gotoView(page, "#config");

  const bundlePath = path.join(os.tmpdir(), `mqb-import-${Date.now()}.json`);
  fs.writeFileSync(
    bundlePath,
    JSON.stringify({
      type: "mqb-export",
      version: 1,
      exportedAt: new Date().toISOString(),
      config: {
        publishers: [
          { name: "imported_publisher", endpoint: { memory: { topic: "imported-events" } } },
        ],
      },
      envVars: {},
    }),
  );

  try {
    await page.locator("#form-actions input.hidden-file-input").setInputFiles(bundlePath);

    await expect(mqbDialog(page)).toContainText("Imported 1 publishers");
    await mqbDialog(page).locator("wa-button", { hasText: "OK" }).click();

    const config = await readConfig(page);
    const names = config.publishers.map((entry) => entry.name);
    expect(names).toContain("imported_publisher");
    expect(names).toContain("http_publisher");
  } finally {
    fs.rmSync(bundlePath, { force: true });
  }
});

test("Reset asks first and cancelling leaves the config untouched", async ({ page }) => {
  await gotoView(page, "#config");

  await action(page, ACTIONS.reset).click();
  await expect(mqbDialog(page)).toContainText("Reset publishers and consumers?");
  await mqbDialog(page).locator("wa-button", { hasText: "Cancel" }).click();

  // Assert by name, not count: the server also materializes a consumer for the
  // `ingest_http` route in the fixture, so the list is longer than what we sent.
  const config = await readConfig(page);
  expect(config.publishers.map((entry) => entry.name)).toContain("http_publisher");
  expect(config.consumers.map((entry) => entry.name)).toContain("memory_consumer");
});

test("Reset clears publishers and consumers once confirmed", async ({ page }) => {
  await gotoView(page, "#config");

  // resetConfig() ends in window.location.reload(). Arm the wait before the
  // click: otherwise that reload lands mid-`page.goto` below and aborts it.
  const reloaded = page.waitForEvent("load", { timeout: 10_000 });
  await action(page, ACTIONS.reset).click();
  await mqbDialog(page).locator("wa-button", { hasText: "Continue" }).click();
  await reloaded;

  await expect
    .poll(async () => (await readConfig(page)).publishers.length, { timeout: 10_000 })
    .toBe(0);
  const config = await readConfig(page);
  expect(config.consumers).toEqual([]);

  await gotoView(page, "#publishers");
  await expect(page.locator("#pub-list .pub-item")).toHaveCount(0);
});

test("JSON view shows the live config and closes again", async ({ page }) => {
  await gotoView(page, "#config");

  await page.locator("#js-show-json").click();
  // The wa-dialog host has no layout box of its own, so toBeVisible() never
  // holds on it; the editor content is what actually proves the dialog opened.
  const dialog = page.locator('wa-dialog[label="Current Configuration"]');
  await expect(dialog).toHaveAttribute("open", "");
  // CodeMirror only renders the lines in view, so assert on the top of the
  // document; `publishers` sits below the fold and never reaches the DOM.
  await expect(dialog.locator(".cm-content")).toContainText("memory_consumer");

  await dialog.locator("wa-button", { hasText: "Close" }).click();
  await expect(dialog).not.toHaveAttribute("open");
});

test("stored-secret actions stay out of the browser build", async ({ page }) => {
  await gotoView(page, "#config");
  await expect(page.locator("#js-check-desktop-secrets")).toHaveCount(0);
  await expect(page.locator("#js-delete-desktop-secrets")).toHaveCount(0);
});
