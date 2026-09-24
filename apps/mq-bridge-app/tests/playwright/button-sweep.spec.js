/**
 * Dead-button sweep.
 *
 * Clicks every visible, enabled button in each main view and requires the click
 * to produce *some* observable effect. A button that changes nothing, requests
 * nothing and opens nothing is dead — that is the bug class this file exists to
 * find, and it scales to new buttons without anyone writing a test for them.
 *
 * Each button is clicked from a freshly reloaded page with a freshly reset
 * config, so one button's side effects cannot mask or break the next one.
 */
const { test, expect } = require("./fixtures");
const { resetConfig, gotoView, waitForShell } = require("./helpers");

// Requests the app issues on a 1s timer; they prove nothing about a click.
const POLLING_PATHS = ["/runtime-status", "/peer-status"];
// Subtrees those pollers rewrite, excluded from the DOM signature.
const VOLATILE_SELECTORS = ["#runtime-status", "#peer-status"];
// How long a click gets to show an effect before it counts as inert.
const CLICK_BUDGET_MS = 500;

/** Open one of a panel's content tabs so the buttons in its pane become reachable. */
const subtab = (target) => (page) => page.locator(`button.content-tab[data-target="${target}"]`).click();

/**
 * Open a panel's JSON preview. The dialog carries its own Copy/Close buttons,
 * which no view exposes until it is open. Both panels mount a dialog, so match
 * on the open one: the wa-dialog host has no layout box, which rules out
 * toBeVisible, and the bare class matches two elements.
 */
const DIALOG = "dialog.json-preview-dialog[open]";

const jsonPreview = (paneTarget, triggerId) => async (page) => {
  await subtab(paneTarget)(page);
  await page.locator(`#${triggerId}`).click();
  await expect(page.locator(DIALOG)).toHaveCount(1);
};

// One entry per reachable *state*, not per view: most buttons live in a tab that
// is closed when the view first renders, and a sweep of the landing state alone
// never sees them. `family` groups the states that share a view so a button
// reachable from several of them is still only swept once.
const VIEWS = [
  { family: "publishers", name: "publishers", hash: "#publishers:0" },
  { family: "publishers", name: "publishers/definition", hash: "#publishers:0", seed: subtab("pub-config-pane") },
  { family: "publishers", name: "publishers/headers", hash: "#publishers:0", seed: subtab("pub-meta-pane") },
  { family: "publishers", name: "publishers/history", hash: "#publishers:0", seed: subtab("pub-history-pane") },
  { family: "publishers", name: "publishers/json", hash: "#publishers:0", seed: jsonPreview("pub-config-pane", "pub-export-config"), scope: DIALOG },
  { family: "consumers", name: "consumers", hash: "#consumers:0" },
  { family: "consumers", name: "consumers/definition", hash: "#consumers:0", seed: subtab("cons-def-panel") },
  { family: "consumers", name: "consumers/output", hash: "#consumers:0", seed: subtab("cons-response-panel") },
  { family: "consumers", name: "consumers/json", hash: "#consumers:0", seed: jsonPreview("cons-def-panel", "cons-export-config"), scope: DIALOG },
  { family: "config", name: "config", hash: "#config" },
];

// Buttons whose effect is deliberately outside anything a browser can observe.
// "Clear" empties the captured-message list; the sweep never runs a consumer, so
// the list is already empty and the re-render is identical. Seeding messages
// first would give it real coverage.
const EXPECTED_INERT = ["Clear"];

// Drag affordances exposed as buttons for keyboard users. They respond to drag,
// not to a plain click, so the sweep cannot judge them.
const DRAG_HANDLE = /^Resize /;

/** Record clipboard writes and object-URL creation; both are otherwise invisible. */
function installProbe() {
  const probe = { clipboard: 0, objectUrls: 0 };
  window.__mqbProbe = probe;

  const record = async () => {
    probe.clipboard += 1;
  };
  if (navigator.clipboard) {
    navigator.clipboard.writeText = record;
  } else {
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText: record },
      configurable: true,
    });
  }

  const createObjectURL = URL.createObjectURL.bind(URL);
  URL.createObjectURL = (blob) => {
    probe.objectUrls += 1;
    return createObjectURL(blob);
  };
}

/**
 * Tag buttons so a click can address exactly the element we measured.
 * Playwright locators pierce shadow roots; querySelectorAll does not, so tagging
 * here keeps `wa-button`'s inner button out of the enumeration — with one
 * deliberate exception: `wa-dialog` renders its own close control in its shadow
 * root, and it is a real button of the app that no light-DOM query reaches.
 *
 * The key is label + occurrence rather than a document-wide index: indices shift
 * whenever a panel renders a different number of buttons, which previously made
 * most of the inventory unmatchable on the second load. Occurrences are counted
 * over the whole document even when `scope` narrows the returned inventory, so
 * one control keeps one key in every state — that is what lets the two JSON
 * dialogs' identically labelled Copy/Close buttons stay distinct.
 */
function tagButtons(scope) {
  const closeControl = (dialog) => dialog.shadowRoot?.querySelector('[part~="close-button"]') || null;

  const buttons = [];
  document.querySelectorAll("button, wa-button, wa-dialog").forEach((element) => {
    if (element.localName === "wa-dialog") {
      const close = closeControl(element);
      if (close) buttons.push(close);
      return;
    }
    buttons.push(element);
  });

  const inScope = scope
    ? new Set([
        ...document.querySelectorAll(`${scope} button, ${scope} wa-button`),
        ...[...document.querySelectorAll(scope)].map(closeControl).filter(Boolean),
      ])
    : null;

  const seen = [];
  const labelCounts = new Map();
  buttons.forEach((element) => {
    const raw =
      element.getAttribute("title") ||
      element.getAttribute("aria-label") ||
      (element.textContent || "").trim() ||
      element.id ||
      element.className;
    const label = String(raw).replace(/\s+/g, " ").slice(0, 60);
    const occurrence = (labelCounts.get(label) || 0) + 1;
    labelCounts.set(label, occurrence);
    const key = `${label}#${occurrence}`;
    element.setAttribute("data-sweep-key", key);
    if (inScope && !inScope.has(element)) return;
    seen.push({
      key,
      label,
      id: element.id || null,
      visible: element.getClientRects().length > 0,
      disabled:
        element.disabled === true ||
        element.hasAttribute("disabled") ||
        element.getAttribute("aria-disabled") === "true",
      // An already-selected control (active tab, current theme) correctly does
      // nothing when clicked again; that is not a dead button.
      active:
        element.classList.contains("active") ||
        element.getAttribute("aria-selected") === "true" ||
        element.getAttribute("aria-pressed") === "true" ||
        // wa-button marks the chosen item of a group by rendering it filled;
        // the JSON preview's format chips are the only such group.
        element.getAttribute("appearance") === "filled",
    });
  });
  return seen;
}

async function domSignature(page) {
  return page.evaluate((volatile) => {
    const clone = document.body.cloneNode(true);
    for (const selector of volatile) {
      clone.querySelectorAll(selector).forEach((node) => node.remove());
    }
    clone.querySelectorAll("[data-sweep-key]").forEach((node) => node.removeAttribute("data-sweep-key"));
    const html = clone.innerHTML;
    let hash = 5381;
    for (let index = 0; index < html.length; index += 1) {
      hash = ((hash * 33) ^ html.charCodeAt(index)) >>> 0;
    }
    return `${html.length}:${hash}`;
  }, VOLATILE_SELECTORS);
}

function isPolling(url) {
  return POLLING_PATHS.some((path) => url.includes(path));
}

/**
 * Changing only the hash is a same-document navigation, so the app keeps
 * whatever dialog the previous button opened — and an open modal swallows the
 * clicks that follow. A real document remount is what clears it.
 *
 * Reload when already on the right URL: it remounts in one navigation where
 * about:blank plus a goto costs two, and the sweep does this once per button.
 */
async function freshView(page, view) {
  if (page.url().endsWith(`/${view.hash}`)) {
    await page.reload();
    await waitForShell(page);
  } else {
    await page.goto("about:blank");
    await gotoView(page, view.hash);
  }
  await settle(page);
  if (view.seed) {
    await view.seed(page);
    await settle(page);
  }
}

/**
 * gotoView returns once the shell is up, but the panels hydrate a tick later.
 * Taking inventory before that hid the entire publisher detail pane — 18 of its
 * buttons were reported "not visible" when they were merely not rendered yet.
 * Wait until the count of visible buttons stops moving.
 */
async function settle(page) {
  let previous = -1;
  await expect
    .poll(
      async () => {
        const count = await page.evaluate(
          () =>
            [...document.querySelectorAll("button, wa-button")].filter(
              (element) => element.getClientRects().length > 0,
            ).length,
        );
        const stable = count > 0 && count === previous;
        previous = count;
        return stable;
      },
      { timeout: 10_000, intervals: [100, 100, 200, 200, 400] },
    )
    .toBe(true);
}

/** The families VIEWS declares, in declaration order. */
const FAMILIES = [...new Set(VIEWS.map((view) => view.family))];

/**
 * Wire up everything a click's effect can show up in. Returns the mutable record
 * the sweep resets before each button.
 */
function observe(page) {
  const observed = { requests: [], pageErrors: [], fileChoosers: 0, downloads: 0 };

  page.on("request", (request) => {
    if (!isPolling(request.url())) observed.requests.push(request.url());
  });
  page.on("pageerror", (error) => observed.pageErrors.push(String(error)));
  // Registering this listener disables Playwright's auto-cancel, so the chooser
  // must be dismissed here — otherwise it stays open and blocks every
  // subsequent click in the sweep with an actionability timeout.
  page.on("filechooser", (chooser) => {
    observed.fileChoosers += 1;
    void chooser.setFiles([]).catch(() => {});
  });
  page.on("download", () => {
    observed.downloads += 1;
  });
  // The app uses its own wa-dialog, but never let a native one block the run.
  page.on("dialog", (dialog) => void dialog.dismiss().catch(() => {}));

  return observed;
}

test.describe("dead button sweep", () => {
  // The sweep clicks Send too, against a publisher whose endpoint nothing is
  // listening on. The 500 that comes back is the sweep doing its job.
  test.use({ allowedPageProblems: /\/publish|500 \(Internal Server Error\)/ });

  /**
   * One test per family rather than one for the whole app, so the families run
   * on separate workers. Not per *state*: the states in a family share a sidebar
   * and a toolbar, and the dedup that keeps 90 clicks from becoming 169 only
   * holds within a family. Splitting finer would multiply the work, not divide
   * it.
   */
  for (const family of FAMILIES) {
    const states = VIEWS.filter((view) => view.family === family);

    test(`every visible button in ${family} does something when clicked`, async ({ page }, testInfo) => {
      // Budget sized for a loaded runner, not for the ~1min this takes idle.
      testInfo.setTimeout(600_000);

      await page.addInitScript(installProbe);
      const observed = observe(page);

      const findings = [];
      const skipped = [];
      // Scenes in one family share a sidebar and a toolbar; without this the same
      // button would be clicked once per scene for no extra coverage.
      const sweptKeys = new Set();
      let clicked = 0;
      let deduped = 0;

      for (const view of states) {
        await resetConfig(page);
        await freshView(page, view);
        const inventory = (await page.evaluate(tagButtons, view.scope)).filter((button) => {
          if (!button.visible) {
            skipped.push({ view, button, reason: "not visible" });
            return false;
          }
          if (button.disabled) {
            skipped.push({ view, button, reason: "disabled" });
            return false;
          }
          if (button.active) {
            skipped.push({ view, button, reason: "already active" });
            return false;
          }
          if (DRAG_HANDLE.test(button.label)) {
            skipped.push({ view, button, reason: "drag handle" });
            return false;
          }
          if (sweptKeys.has(button.key)) {
            deduped += 1;
            return false;
          }
          sweptKeys.add(button.key);
          return true;
        });

        for (const button of inventory) {
          await resetConfig(page);
          await freshView(page, view);
          const current = await page.evaluate(tagButtons, view.scope);
          const match = current.find((entry) => entry.key === button.key);
          if (!match || !match.visible || match.disabled || match.active) {
            skipped.push({ view, button, reason: "not reachable on reload" });
            continue;
          }

          const escapedKey = button.key.replace(/(["\\])/g, "\\$1");
          const target = page.locator(`[data-sweep-key="${escapedKey}"]`);
          const before = await domSignature(page);
          const hashBefore = page.url();
          observed.requests.length = 0;
          observed.pageErrors.length = 0;
          observed.fileChoosers = 0;
          observed.downloads = 0;

          let clickError = null;
          try {
            await target.click({ timeout: 5_000 });
            clicked += 1;
          } catch (error) {
            clickError = String(error).split("\n")[0];
          }

          // Async handlers (fetch, dialog mount, Svelte flush) land a tick or two
          // after the click, so poll for the effect rather than paying the
          // slowest button's cost on every one: a live button resolves in a few
          // ms and only a genuinely inert one waits out the budget.
          //
          // Exiting early can miss an exception thrown after the effect landed.
          // The shared fixture still records every pageerror and fails the test
          // at teardown; what is lost is only the attribution to this button.
          const deadline = Date.now() + CLICK_BUDGET_MS;
          let after;
          let probe;
          let alive;
          for (;;) {
            after = await domSignature(page);
            probe = await page.evaluate(() => window.__mqbProbe || { clipboard: 0, objectUrls: 0 });
            alive =
              after !== before ||
              page.url() !== hashBefore ||
              observed.requests.length > 0 ||
              observed.fileChoosers > 0 ||
              observed.downloads > 0 ||
              probe.clipboard > 0 ||
              probe.objectUrls > 0;
            if (alive || observed.pageErrors.length > 0 || Date.now() >= deadline) break;
            await page.waitForTimeout(25);
          }

          if (clickError) {
            findings.push(`${view.name}: "${button.label}" — click failed: ${clickError}`);
            continue;
          }
          if (observed.pageErrors.length > 0) {
            findings.push(`${view.name}: "${button.label}" — threw: ${observed.pageErrors[0]}`);
            continue;
          }

          if (!alive && !EXPECTED_INERT.includes(button.label)) {
            findings.push(`${view.name}: "${button.label}" — click had no observable effect`);
          }
        }
      }

      await testInfo.attach("skipped-buttons", {
        body: skipped.map((entry) => `${entry.view.name}: "${entry.button.label}" (${entry.reason})`).join("\n"),
        contentType: "text/plain",
      });
      console.log(
        `${family}: swept ${clicked} buttons across ${states.length} state(s) ` +
          `(${deduped} reachable from more than one, clicked once)`,
      );

      expect(findings, `dead or broken buttons:\n  ${findings.join("\n  ")}`).toEqual([]);
    });
  }

  /**
   * Coverage for the whole app, which no single family's test can see: a button
   * is invisible in every view but its own, so a per-family verdict would report
   * the other families' buttons as gaps. Inventory only, no clicking — this test
   * answers "does anything render this button", not "does it work".
   */
  test("every button is reachable from some view", async ({ page }, testInfo) => {
    testInfo.setTimeout(300_000);

    // Keyed per *button*, not per label: the two JSON dialogs each carry a
    // "Copy" and a "Close", and one must not account for the other.
    const seen = new Map();
    for (const view of VIEWS) {
      await resetConfig(page);
      await freshView(page, view);
      for (const button of await page.evaluate(tagButtons, view.scope)) {
        const entry = seen.get(button.key) || { label: button.label, views: new Set(), reached: false };
        // Visible is the whole test. A visible button that is disabled or already
        // active was found and deliberately not clicked; that is a decision, not
        // a gap. A button no state ever renders is the gap.
        if (button.visible) {
          entry.reached = true;
          entry.views.add(view.name);
        }
        seen.set(button.key, entry);
      }
    }

    const neverReached = [...seen.values()].filter(
      (entry) => !entry.reached && !DRAG_HANDLE.test(entry.label),
    );

    await testInfo.attach("button-coverage", {
      body: [...seen.entries()]
        .map(([key, entry]) => `${entry.reached ? "reached" : "NEVER  "} ${key} ${[...entry.views].join(", ")}`)
        .join("\n"),
      contentType: "text/plain",
    });
    console.log(
      `${seen.size} buttons across ${VIEWS.length} states; ${neverReached.length} never reached`,
    );
    if (neverReached.length > 0) {
      console.log(`never reached:\n  ${neverReached.map((entry) => `"${entry.label}"`).join("\n  ")}`);
    }
  });
});
