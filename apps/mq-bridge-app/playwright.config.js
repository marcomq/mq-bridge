const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "./tests/playwright",
  timeout: 30_000,
  // No maxFailures: when the UI is broken the point is to see every failure in
  // one run, not to stop at the first one.
  workers: 1, // Required because tests modify shared global /config state
  expect: {
    timeout: 5_000,
  },
  reporter: [["line"]],
  use: {
    baseURL: "http://127.0.0.1:39091",
    headless: process.env.SHOWCASE === "true" ? false : true,
  },
  webServer: {
    command:
      'echo \'ui_addr: "127.0.0.1:39091"\nlog_level: "info"\npublishers: []\nconsumers: []\nroutes: {}\' > /tmp/mqb-playwright-minimal.yml && cargo run -p mq-bridge-app -- --config /tmp/mqb-playwright-minimal.yml',
    url: "http://127.0.0.1:39091/health",
    reuseExistingServer: true,
    // CI pre-builds the binary so this is a no-op link check; the generous
    // budget is for cold local runs, where a full build takes ~10 min.
    timeout: 900_000,
  },
});
