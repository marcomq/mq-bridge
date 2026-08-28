const { defineConfig } = require("vite");
const { svelte } = require("@sveltejs/vite-plugin-svelte");
const { resolve } = require("path");

const backendTarget = "http://127.0.0.1:39091";
const backendUrl = new URL(backendTarget);
const proxiedPaths = [
  "/health",
  "/schema.json",
  "/config",
  "/config-recovery",
  "/storage-security",
  "/desktop-secrets",
  "/consumer-status",
  "/consumer-start",
  "/consumer-stop",
  "/messages",
  "/publish",
  "/runtime-status",
  "/metrics",
];

module.exports = defineConfig({
  root: resolve(__dirname, "ui"),
  publicDir: resolve(__dirname, "crates/core/static"),
  plugins: [svelte({ configFile: resolve(__dirname, "svelte.config.js") })],
  resolve: {
    alias: {
      // Map absolute paths in index.html to the physical static folder.
      // It lives inside the core crate so it is embedded into the published
      // binary (see crates/core/src/ui_app.rs) and bundled by Tauri.
      "/style.css": resolve(__dirname, "crates/core/static/style.css"),
    },
  },
  server: {
    host: "127.0.0.1",
    port: 39092,
    proxy: Object.fromEntries(
      proxiedPaths.map((path) => [
        path,
        {
          target: backendTarget,
          changeOrigin: true,
          configure: (proxy) => {
            proxy.on("proxyReq", (proxyReq) => {
              proxyReq.setHeader("origin", backendTarget);
              proxyReq.setHeader("referer", `${backendTarget}/`);
              proxyReq.setHeader("host", backendUrl.host);
            });
          },
        },
      ]),
    ),
  },
  build: {
    emptyOutDir: false,
    chunkSizeWarningLimit: 2000,
    outDir: resolve(__dirname, "crates/core/static"),
    copyPublicDir: false,
    rollupOptions: {
      input: resolve(__dirname, "ui/index.html"),
      output: {
        entryFileNames: "[name].js",
        assetFileNames: "assets/[name][extname]",
      },
    },
  },
  test: {
    environment: "node",
    include: ["../tests/unit/**/*.test.ts"],
  },
});
