# How the app is built

`mq-bridge-app` is a demonstration of the [`mq-bridge`](https://github.com/marcomq/mq-bridge)
engine used on itself: the application serves its own management UI *through* the engine rather
than through a separate web framework. If you want to understand the engine's request-reply model
in practice, this is a worked example. For the engine internals themselves, see
[Learn the architecture](../engine/architecture.md).

## Backend: `mq-bridge` as a web server

Instead of using a traditional web framework like Actix or Axum directly for the management API,
the application routes HTTP through the engine:

1. **HTTP input** — an `http` input endpoint listens on the configured UI port and converts
   incoming HTTP requests into `CanonicalMessage`s.
2. **WebUiHandler** — a custom `Handler` processes these messages as a router: serving static
   files (HTML, JS) or handling API requests (e.g. `/config`, `/schema.json`).
3. **Response output** — the handler returns a response message, sent to a `response` output
   endpoint, completing the HTTP request-response cycle.

This showcases the library's ability to handle [request-reply](../tutorials/request-reply.md)
patterns and act as a lightweight web server.

## Frontend: `vanilla-schema-forms`

The Web UI is generated dynamically from the Rust configuration structures — no hand-written form
code:

1. **Schema generation** — the backend uses `schemars` to generate a JSON Schema for the
   `AppConfig` struct at runtime, exposed via `/schema.json` (also available on the CLI:
   `mqb --schema schema.json` writes it to a file, `mqb --schema -` prints it).
2. **Dynamic form** — the frontend uses
   [vanilla-schema-forms](https://github.com/marcomq/vanilla-schema-forms) to render a complete
   configuration form from that schema alone.
3. **No UI code changes** — when a new feature or config option is added to the Rust code (e.g. a
   new middleware), the schema updates automatically and the UI reflects it without any frontend
   changes.

## Project status

This project is in **active development**. It originally served as the primary reference
implementation and testbed for the [`mq-bridge`](https://github.com/marcomq/mq-bridge) engine.

The UI/Tauri layer was largely prototyped quickly and does not mirror the `mq-bridge` /
`mq-bridge-app` core/CLI standards — treat it as a working demo, not a reference implementation,
and test before relying on it in production.
