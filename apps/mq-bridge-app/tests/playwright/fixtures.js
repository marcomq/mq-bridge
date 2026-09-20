/**
 * The shared test base for every UI spec.
 *
 * A spec that only asserts what it came to assert will happily pass while the
 * page throws on every render, logs an error per poll, or 500s on a request it
 * does not await. This base fails a test on any of those, so each spec hardens
 * every other spec's ground.
 *
 * Import `test`/`expect` from here rather than from `@playwright/test`.
 */
const base = require("@playwright/test");
const { startApp } = require("./app-server");

/**
 * Noise that is not the app's fault and cannot be fixed from here. Keep this
 * list short and specific: every entry is a class of real failure going unseen.
 */
const ALWAYS_IGNORED = [
  // The dev shell has no favicon; the packaged binary serves one.
  /\/favicon\.ico\b/,
  // Navigating away cancels whatever the pollers had in flight. That is the
  // browser doing as it was told, not a request the server refused — and the
  // specs navigate constantly, the sweep twice per button. One spelling per engine.
  /net::ERR_ABORTED|NS_BINDING_ABORTED|Load request cancelled/,
];

function isIgnored(message, extra) {
  // A bare RegExp, because a two-element array collides with Playwright's own
  // `[value, options]` tuple shape and gets destructured out from under us.
  const allowed = extra instanceof RegExp ? [extra] : Array.isArray(extra) ? extra : [];
  return [...ALWAYS_IGNORED, ...allowed].some((pattern) => pattern.test(message));
}

const test = base.test.extend({
  /**
   * This worker's own app process. Worker-scoped, so it is started once and
   * reused by every test the worker runs — and torn down with it.
   */
  appServer: [
    async ({}, use) => {
      const app = await startApp();
      await use(app);
      await app.stop();
    },
    { scope: "worker" },
  ],

  /** Point every relative goto and `page.request` call at this worker's app. */
  baseURL: async ({ appServer }, use) => {
    await use(appServer.baseURL);
  },

  /**
   * Per-spec escape hatch for a page problem a test provokes on purpose:
   * `test.use({ allowedPageProblems: /403 \(Forbidden\)/ })`. Pass one RegExp,
   * using alternation for several. Prefer fixing the page.
   */
  allowedPageProblems: [null, { option: true }],

  /** Set false only for a test that deliberately drives the page into failure. */
  failOnPageProblems: [true, { option: true }],

  problems: [
    async ({ page, allowedPageProblems, failOnPageProblems }, use) => {
      const problems = [];
      const record = (kind, message) => {
        const text = String(message);
        if (!isIgnored(text, allowedPageProblems)) problems.push(`${kind}: ${text}`);
      };

      page.on("pageerror", (error) => record("uncaught exception", error.stack || error));
      page.on("console", (message) => {
        if (message.type() !== "error") return;
        // A failed subresource is reported as a console error whose text names
        // only the status, never the URL. The location does, and without it the
        // URL-keyed ignores above can never match a network message.
        const url = message.location()?.url;
        record("console.error", url ? `${message.text()} — ${url}` : message.text());
      });
      // A request the app never awaits still means the server rejected it. 4xx is
      // left to the specs: some flows legitimately probe an endpoint that 404s.
      page.on("response", (response) => {
        if (response.status() >= 500) record("server error", `${response.status()} ${response.url()}`);
      });
      page.on("requestfailed", (request) => {
        const failure = request.failure();
        record("request failed", `${request.url()} — ${failure ? failure.errorText : "unknown"}`);
      });

      await use(problems);

      if (failOnPageProblems && problems.length > 0) {
        throw new Error(`the page reported ${problems.length} problem(s):\n  ${problems.join("\n  ")}`);
      }
    },
    { auto: true },
  ],
});

module.exports = { test, expect: base.expect };
