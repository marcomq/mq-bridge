/**
 * What the buttons actually do.
 *
 * `button-sweep.spec.js` proves every button does *something*; it passes just as
 * happily when a button does the wrong thing. This file pins the specific
 * outcome for the controls whose effect nothing else asserts, so a handler wired
 * to the wrong action is a failure rather than a still-green sweep.
 */
const { test, expect } = require("./fixtures");
const { resetConfig, readConfig, gotoView } = require("./helpers");

test.beforeEach(async ({ page }) => {
  await resetConfig(page);
});

test.describe("main tabs", () => {
  test("each tab activates its own panel and records itself in the URL", async ({ page }) => {
    await gotoView(page, "#publishers:0");

    for (const [id, hash, panel] of [
      ["#mtab-consumers", "consumers", "#cons-main-ui"],
      ["#mtab-config", "config", "#form-container"],
      ["#mtab-publishers", "publishers", "#pub-main-ui"],
    ]) {
      await page.locator(id).click();
      await expect(page.locator(id)).toHaveClass(/active/);
      await expect(page).toHaveURL(new RegExp(`#${hash}`));
      await expect(page.locator(panel)).toBeVisible();
      // Exactly one main tab is ever active.
      await expect(page.locator("#mainTabs .main-tab.active")).toHaveCount(1);
    }
  });
});

test.describe("add menus", () => {
  test("adding a publisher appends it to the list and leaves the workspace dirty", async ({ page }) => {
    await gotoView(page, "#publishers:0");
    // The server also synthesizes a publisher per route output, so count what it
    // actually holds rather than what the fixture declares.
    const savedBefore = (await readConfig(page)).publishers.length;
    // The sidebar hydrates a tick after the shell, so wait for it to agree with
    // the server. Counting in between reads as an empty list and throws every
    // later count off by the whole fixture.
    const items = page.locator("#pub-list .pub-item");
    await expect(items).toHaveCount(savedBefore);
    const before = savedBefore;

    await page.locator("#pub-add").click();
    const menu = page.locator(".add-menu");
    await expect(menu).toBeVisible();

    await menu.locator("button", { hasText: "Memory" }).first().click();
    await expect(menu).toBeHidden();
    await expect(items).toHaveCount(before + 1);
    await expect(page.locator("#workspace-save-button")).toHaveAttribute("data-dirty", "true");
    // Unsaved: the server must not have the new publisher yet.
    expect((await readConfig(page)).publishers).toHaveLength(savedBefore);
  });

  test("adding a consumer appends it to the consumer list", async ({ page }) => {
    await gotoView(page, "#consumers:0");
    const before = (await readConfig(page)).consumers.length;
    const items = page.locator("#cons-list .cons-item");
    await expect(items).toHaveCount(before);

    await page.locator("#cons-add").click();
    await page.locator(".add-menu button", { hasText: "Memory" }).first().click();

    await expect(items).toHaveCount(before + 1);
    await expect(page.locator("#workspace-save-button")).toHaveAttribute("data-dirty", "true");
  });

  test("the add menu closes again without adding anything when dismissed", async ({ page }) => {
    await gotoView(page, "#publishers:0");
    const before = (await readConfig(page)).publishers.length;
    const items = page.locator("#pub-list .pub-item");
    await expect(items).toHaveCount(before);

    await page.locator("#pub-add").click();
    await expect(page.locator(".add-menu")).toBeVisible();
    await page.locator("#pub-filter").click();

    await expect(page.locator(".add-menu")).toBeHidden();
    await expect(items).toHaveCount(before);
    await expect(page.locator("#workspace-save-button")).toHaveAttribute("data-dirty", "false");
  });
});

test.describe("JSON preview", () => {
  test("shows the selected publisher, switches format, and copies it", async ({ page }) => {
    await gotoView(page, "#publishers:0");
    await page.locator("#ctab-config").click();
    await page.locator("#pub-export-config").click();

    const dialog = page.locator("dialog.json-preview-dialog[open]");
    await expect(dialog).toHaveCount(1);
    // The preview is of the selected publisher, not of some other entity.
    await expect(dialog.locator(".json-preview-container")).toContainText("http_publisher");

    const chips = dialog.locator(".json-preview-variants wa-button");
    if ((await chips.count()) > 1) {
      const beforeText = await dialog.locator(".json-preview-container").innerText();
      await chips.nth(1).click();
      await expect
        .poll(async () => dialog.locator(".json-preview-container").innerText())
        .not.toBe(beforeText);
    }

    await page.evaluate(() => {
      window.__copied = null;
      navigator.clipboard.writeText = async (text) => {
        window.__copied = text;
      };
    });
    await dialog.locator("wa-button", { hasText: "Copy" }).first().click();
    await expect.poll(async () => page.evaluate(() => window.__copied)).toContain("http_publisher");
  });

  test("Close dismisses the dialog and leaves the config untouched", async ({ page }) => {
    const before = await readConfig(page);
    await gotoView(page, "#publishers:0");
    await page.locator("#ctab-config").click();
    await page.locator("#pub-export-config").click();

    const dialog = page.locator("dialog.json-preview-dialog[open]");
    await expect(dialog).toHaveCount(1);
    await dialog.locator("wa-button", { hasText: "Close" }).first().click();

    await expect(page.locator("dialog.json-preview-dialog[open]")).toHaveCount(0);
    expect(await readConfig(page)).toEqual(before);
  });
});

test.describe("theme selector", () => {
  test("each theme choice is applied to the document and remembered", async ({ page }) => {
    await gotoView(page, "#publishers:0");

    for (const choice of ["Light", "Dark", "Auto"]) {
      await page.locator("button.theme-trigger").click();
      await page.locator(".theme-menu button", { hasText: choice }).click();

      const expected = choice.toLowerCase();
      await expect(page.locator("html")).toHaveAttribute("data-theme-preference", expected);
      expect(await page.evaluate(() => localStorage.getItem("theme"))).toBe(expected);
      // "auto" resolves to one of the two real schemes; the other two are literal.
      const resolved = await page.getAttribute("html", "data-theme");
      expect(expected === "auto" ? ["light", "dark"] : [expected]).toContain(resolved);
    }
  });
});

test.describe("sidebar filter", () => {
  test("typing narrows the list and clearing restores it", async ({ page }) => {
    await gotoView(page, "#publishers:0");
    const before = (await readConfig(page)).publishers.length;
    const names = page.locator("#pub-list .pub-item .item-name");
    await expect(names).toHaveCount(before);

    await page.locator("#pub-filter").fill("memory");
    await expect.poll(async () => names.count()).toBeLessThan(before);
    for (const name of await names.allInnerTexts()) {
      expect(name.toLowerCase()).toContain("memory");
    }

    await page.locator("#pub-filter").fill("");
    await expect(names).toHaveCount(before);
  });
});

test.describe("history", () => {
  /**
   * The sweep lists "Clear" as an expected-inert button because it sweeps an
   * empty history, so nothing else checks that it clears anything. Send first,
   * then clear, and the button has real coverage.
   */
  test("Clear empties a publisher's execution history", async ({ page }) => {
    await gotoView(page, "#publishers:1");
    await expect(page.locator("#pub-list .pub-item.active .item-name")).toContainText("memory_publisher");

    await page.locator("#ctab-payload").click();
    await page.locator("#pub-payload .cm-content").fill('{"hello":"history"}');
    const published = page.waitForResponse(
      (response) => response.url().includes("/publish") && response.request().method() === "POST",
    );
    await page.locator("#pub-send").click();
    expect((await published).ok()).toBeTruthy();

    await page.locator('#pub-sub-tabs button[data-target="pub-history-pane"]').click();
    const rows = page.locator("#pub-history-pane tr.history-row");
    await expect(rows).toHaveCount(1);

    await page.locator("#pub-clear-history").click();
    await expect(rows).toHaveCount(0);
    await expect(page.locator("#pub-history-pane")).toContainText("No history for this publisher.");
  });
});
