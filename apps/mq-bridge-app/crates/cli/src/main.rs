//  mq-bridge-app
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use mq_bridge_app::{
    config::{AppConfig, config_file_path, load_config},
    copy_pipeline, mq_bridge,
    route_metrics::next_route_metric_sample,
    status_registry::{
        InstanceKind, StatusEntity, StatusLease, StatusRoute, StatusSnapshot, StatusSummary,
        endpoint_type_label, now_ms,
    },
    ui_app::{UiApp, collector_route_name, consumer_runtime_key},
    web_ui,
};

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

use anyhow::Context;

mod checkpoint_cmd;
mod mcp;
mod mcp_install;
mod status_cmd;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// App-level default batch size for headless routes (`copy`, MCP) when the caller
/// does not specify one. The library's `RouteOptions::default()` is 512; this is the
/// bulk-move value the app applies on top.
pub(crate) const DEFAULT_BATCH_SIZE: usize = 1024;

/// The batch size for a run that names none: what the sink's schema asks for
/// (`x-mqb-default-batch-size`), else [`DEFAULT_BATCH_SIZE`].
pub(crate) fn default_batch_size(output: &mq_bridge::models::Endpoint) -> usize {
    match &output.endpoint_type {
        mq_bridge::models::EndpointType::Custom { name, .. } => {
            mq_bridge::extensions::endpoint_default_batch_size(name)
        }
        _ => None,
    }
    .unwrap_or(DEFAULT_BATCH_SIZE)
}

/// App-level default route concurrency for headless routes (`copy`, MCP) when the
/// caller does not specify one. See [`DEFAULT_BATCH_SIZE`].
pub(crate) const DEFAULT_CONCURRENCY: usize = 4;

/// How often `copy --drain` checks whether the route task has ended. The engine
/// exposes completion as a poll, not a notification; see [`run_copy`].
const COPY_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long `copy --wait` pauses between drain attempts. An empty source drains
/// instantly, so without this the wait would spin.
const COPY_WAIT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// How long a stopped route is given to publish what it already read. `stop` only
/// signals; the tally is a source-side count, so reporting before the task ends
/// can claim rows the destination never saw. Matches the budget `Route::stop`
/// gives a route it owns, so the two ways of stopping wait the same.
const COPY_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Address the web UI falls back to when the config names none. Only ever
/// applied after an explicit `--ui` or a `y` from [`ui_prompt`].
const DEFAULT_UI_ADDR: &str = "127.0.0.1:9091";
/// Replaces [`DEFAULT_UI_ADDR`] for `--ui` and the start prompt, not a configured `ui_addr`.
const DEFAULT_UI_ADDR_ENV: &str = "MQB_UI_DEFAULT_ADDR";

/// Address the Prometheus endpoint falls back to when the config names none.
///
/// Loopback, not `0.0.0.0`: metrics are read-only but still describe the routes
/// and endpoints in use, and a bare run on a laptop should not publish that to
/// the local network. Deployments that scrape from another host set the address
/// explicitly — the Docker image does exactly that in its `CMD`.
const DEFAULT_METRICS_ADDR: &str = "127.0.0.1:9090";

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to configuration file for loading and saving.
    #[arg(short, long)]
    config: Option<String>,

    /// Path to a template configuration file to initialize from on first run if the main config file doesn't exist.
    #[arg(short, long)]
    init_config: Option<String>,

    /// A string containing configuration (e.g., YAML or JSON) to initialize from if the main config file doesn't exist.
    #[arg(long)]
    init_config_str: Option<String>,

    /// A string containing configuration (e.g., YAML or JSON) to override the config file.
    #[arg(long)]
    config_str: Option<String>,

    /// Path to a native plugin library to load before starting (repeatable).
    ///
    /// The plugin registers an endpoint — and possibly a middleware — under its
    /// own name, usable in routes like any built-in one. It cannot replace an
    /// endpoint this binary already has (pulsar, meilisearch): install the
    /// plugin and set `MQB_PLUGIN_OVERRIDE=<name>` instead. Also loadable from the
    /// config file's `plugins:` list. Either way the paths are read only at
    /// startup: changing them needs a restart, and the UI rejects a config that
    /// asks for a different set.
    #[arg(long = "plugin", value_name = "PATH", global = true)]
    plugins: Vec<String>,

    /// Generate JSON schema to the specified path
    #[arg(long)]
    schema: Option<String>,

    /// When to colorize log output: `auto` (default), `always` or `never`.
    ///
    /// `auto` colors a terminal but writes plain text to a pipe or file, so a
    /// redirected log does not collect escape sequences. Also honors `NO_COLOR`.
    #[arg(long, value_enum, value_name = "WHEN", default_value_t = ColorChoice::Auto, global = true)]
    color: ColorChoice,

    /// Start the web UI on the default port without asking.
    ///
    /// Only relevant when no config file sets `ui_addr`: that case asks for
    /// confirmation on a terminal and starts nothing anywhere else, so an
    /// unattended run never opens the port by itself.
    #[arg(long)]
    ui: bool,

    /// Never start the web UI, and do not ask.
    #[arg(long, conflicts_with = "ui")]
    no_ui: bool,

    /// Serve the Prometheus endpoint on ADDR (default `127.0.0.1:9090`).
    ///
    /// Overrides `metrics_addr` from the config. Use `0.0.0.0:<port>` to allow
    /// scraping from another host.
    #[arg(long, value_name = "ADDR", conflicts_with = "no_metrics")]
    metrics_addr: Option<String>,

    /// Do not serve the Prometheus endpoint on its own port.
    ///
    /// Metrics are still collected, and still reachable at `/metrics` on the
    /// web UI when that is running.
    #[arg(long)]
    no_metrics: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    /// Whether to emit SGR escapes, given whether the log writer is a terminal
    /// and whether the environment asked for no color.
    ///
    /// `NO_COLOR` applies only under `auto`: an explicit `--color always` is a
    /// direct instruction and outranks the environment.
    fn enabled(self, writer_is_terminal: bool, no_color: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => writer_is_terminal && !no_color,
        }
    }
}

/// `NO_COLOR` (https://no-color.org): set to any non-empty value.
fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Copy data from one endpoint to another as a headless one-route job.
    ///
    /// With `--drain` the job exits once the source is empty; otherwise it runs
    /// as a continuous bridge until Ctrl-C. No web UI is started.
    Copy(CopyArgs),

    /// Run as an MCP (Model Context Protocol) server exposing the bridge as tools.
    ///
    /// A universal, protocol-agnostic message/data bridge driven from natural
    /// language: publish messages to any endpoint and run routes between any two
    /// endpoints, all supplied ad hoc as endpoint JSON. No web UI is started.
    Mcp(McpArgs),

    /// Wait for mail in this machine's agent inbox, write it out, and exit.
    ///
    /// The inbox is `$MQB_AGENTS_DIR/<NAME>` (default `~/.mqb-agents/<NAME>`) —
    /// the same mailbox the MCP server's `agent_listen` and `agent_send` tools
    /// use, so an agent with no MCP client can join in with one command.
    ///
    /// The inbox is held for the whole wait, so a second listener on the same
    /// name is refused rather than quietly splitting the mail.
    ///
    /// Exiting *is* the notification: run it as a background job and its
    /// completion is what tells the agent mail arrived.
    AgentListen(AgentListenArgs),

    /// Print a copyable, self-contained command for the loaded YAML/JSON config.
    /// Credential values are replaced by environment-variable placeholders.
    ToCli,

    /// Show, reset, or set the resume checkpoint of a configured route or of a
    /// `copy --resume` job.
    Checkpoint(checkpoint_cmd::CheckpointArgs),

    /// Show what every mq-bridge process of this user is running on this machine.
    ///
    /// Reads the local status registry that the CLI, MCP server, web UI and
    /// desktop app publish to. Starts nothing and loads no config.
    Status(status_cmd::StatusArgs),
}

#[derive(clap::Args, Debug)]
struct AgentListenArgs {
    /// Inbox to read: this agent's own name.
    #[arg(value_name = "NAME")]
    name: String,

    /// Seconds to wait for the first message before giving up. `0` does not wait
    /// at all: it takes whatever is already queued and exits.
    #[arg(long, value_name = "SECS", default_value_t = 3600)]
    wait: u64,

    /// Where to write what arrives. Defaults to a file named after the inbox
    /// under the system temp directory, whose path is printed on exit.
    #[arg(long, value_name = "TARGET")]
    to: Option<String>,

    /// Log what the listen is doing. Without it only warnings and errors are
    /// logged.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(clap::Args, Debug)]
struct McpArgs {
    /// Transport: `stdio` (default, for local clients like Claude Desktop/Code) or
    /// `http` (streamable HTTP served over hyper).
    #[arg(long, default_value = "stdio")]
    transport: String,

    /// Bind address for `--transport http` (defaults to 127.0.0.1:9092).
    #[arg(long)]
    bind: Option<String>,

    /// Publish this process's sanitized status to the same-user local status
    /// registry (on by default).
    ///
    /// Bare `--report-to-ui` is a no-op kept for MCP client configurations
    /// written by older `mcp install` runs, which baked the flag in while
    /// reporting was still opt-in. Use `--report-to-ui=false` or
    /// `--no-report-to-ui` to turn publication off.
    #[arg(
        long,
        global = true,
        num_args = 0..=1,
        default_value_t = true,
        default_missing_value = "true",
        action = clap::ArgAction::Set,
    )]
    report_to_ui: bool,

    /// Do not publish this process's status to the local status registry.
    #[arg(long, global = true, conflicts_with = "report_to_ui")]
    no_report_to_ui: bool,

    /// Offer the agent bus: the `agent_listen` and `agent_send` tools for
    /// messaging other agents on this machine. Off by default — without it
    /// neither tool is registered.
    ///
    /// Even with the flag, this server's own inbox stays closed until
    /// `agent_listen` is called.
    #[arg(long)]
    agent_bus: bool,

    /// Register/unregister this binary with local MCP clients instead of serving.
    #[command(subcommand)]
    action: Option<McpAction>,
}

#[derive(Subcommand, Debug)]
enum McpAction {
    /// Register this binary as a stdio MCP server with local MCP clients.
    ///
    /// Without `--client`, every client detected on this machine is configured.
    /// The absolute path of the running binary is what gets registered.
    Install {
        /// Client to configure (all detected clients if omitted).
        #[arg(long, value_enum)]
        client: Option<mcp_install::Client>,

        /// Register in the current project's config instead of the user's
        /// global one. Not supported by Claude Desktop.
        #[arg(long)]
        local: bool,

        /// Print the config snippet for a client we don't write directly,
        /// instead of installing anything.
        #[arg(long)]
        print_config: bool,

        /// Bake `--agent-bus` into the registered command, so the client gets
        /// the agent-messaging tools.
        #[arg(long)]
        agent_bus: bool,
    },

    /// Remove this server from local MCP clients.
    Uninstall {
        /// Client to clean up (all detected clients if omitted).
        #[arg(long, value_enum)]
        client: Option<mcp_install::Client>,

        /// Remove the project-scoped registration instead of the global one.
        #[arg(long)]
        local: bool,
    },

    /// Show where this server is registered, and whether it still points here.
    Status {
        /// Inspect project-scoped configs instead of the global ones.
        #[arg(long)]
        local: bool,
    },
}

#[derive(clap::Args, Debug)]
struct CopyArgs {
    /// Source endpoint URI. The scheme selects the endpoint and query params set
    /// its config, e.g. `postgres://user:pass@host/db?table=src&sslmode=disable`,
    /// `nats://host:4222?subject=orders` or `file:///path/to/file?format=json`.
    ///
    /// Append `|`-separated middlewares to wrap the endpoint, applied in order:
    /// `...?table=src|retry?max_attempts=5|metrics`. Middleware params are that
    /// middleware's config fields. A literal `|` inside the URI must be written
    /// as `%7C`.
    #[arg(
        long,
        value_name = "SOURCE",
        conflicts_with = "source",
        allow_hyphen_values = true
    )]
    from: Option<String>,

    /// Destination endpoint URI (same URI and middleware forms as `--from`), e.g.
    /// `postgres://user:pass@host/db?table=dst&insert_query=<url-encoded SQL>`.
    #[arg(
        long,
        value_name = "TARGET",
        conflicts_with = "target",
        allow_hyphen_values = true
    )]
    to: Option<String>,

    /// Source endpoint URI in the positional `copy SOURCE TARGET` form. `-` is
    /// stdin (and stdout as TARGET); the copy ends when the input does.
    #[arg(
        value_name = "SOURCE",
        index = 1,
        conflicts_with = "from",
        allow_hyphen_values = true
    )]
    source: Option<String>,

    /// Destination endpoint URI in the positional `copy SOURCE TARGET` form.
    #[arg(
        value_name = "TARGET",
        index = 2,
        conflicts_with = "to",
        allow_hyphen_values = true
    )]
    target: Option<String>,

    /// Only copy messages for which EXPR evaluates to true.
    ///
    /// Expressions address top-level JSON fields directly, for example
    /// `amount > 100` or `country == "DE" && amount >= 50`.
    #[arg(long, value_name = "EXPR")]
    filter: Option<String>,

    /// Stop after N rows reached the destination; with `--filter`, N matching rows.
    ///
    /// The copy ends there, with or without `--drain`. Meant for a preview: a
    /// queue source may have handed out a few more messages than were copied.
    #[arg(long, value_name = "N", conflicts_with = "resume")]
    limit: Option<u64>,

    /// Resume from the last successfully processed position.
    ///
    /// The source's native cursor, offset, slot, or checkpoint mechanism is used.
    /// Fails before starting when the source cannot resume safely.
    #[arg(long, conflicts_with = "no_resume")]
    resume: bool,

    /// Ignore optional resume/checkpoint configuration.
    ///
    /// This suppresses resume warnings and errors for cursor-based sources. It
    /// is a no-op for native queue and CDC offsets.
    #[arg(long, conflicts_with = "resume")]
    no_resume: bool,

    /// Exit once the source yields an empty batch (drain-then-exit). Without it,
    /// `copy` keeps running like a continuous bridge until Ctrl-C.
    ///
    /// A drain does not wait for an endpoint: it fails on the first error while
    /// nothing has been delivered yet.
    #[arg(long)]
    drain: bool,

    /// Wait up to SECS for the source to produce something (long polling).
    ///
    /// Implies `--drain`: the job still drains and exits, this only governs how
    /// long it looks for the first message. It returns as soon as one arrives,
    /// so this is a ceiling, not a delay. `0` waits not at all, which is plain
    /// `--drain`.
    ///
    /// Use it when another process fills the source: a drain that starts too
    /// early sees an empty source and exits reporting nothing to do.
    #[arg(long, value_name = "SECS")]
    wait: Option<u64>,

    /// Route concurrency (defaults to 4).
    #[arg(long)]
    concurrency: Option<usize>,

    /// Batch size (defaults to 1024, or to what the destination asks for).
    #[arg(long)]
    batch_size: Option<usize>,

    /// Log what the copy is doing: endpoints, connections, shutdown. Without it
    /// only warnings and errors are logged.
    ///
    /// The `copied …` summary prints either way — it goes to stdout directly
    /// rather than through the logger. `RUST_LOG` still wins when set.
    #[arg(short, long)]
    verbose: bool,
}

/// Whether this config has nothing to run, and so exists only to be filled in.
///
/// Deliberately checks both collections: a bridge configured purely with
/// `routes:` has no `consumers`, and treating that as unconfigured would stop a
/// real deployment at the UI prompt.
fn nothing_to_run(config: &AppConfig) -> bool {
    config.consumers.is_empty() && config.routes.is_empty()
}

/// Asks whether to open the web UI on `addr`, defaulting to no.
///
/// Only a terminal is asked. An unattended run — a shell script, a service
/// unit, CI — answers nothing, and the safe reading of silence is to leave the
/// port closed rather than expose a control surface nobody meant to start.
fn ui_prompt(addr: &str) -> bool {
    use std::io::{BufRead, IsTerminal, Write};

    let mut stdin = std::io::stdin().lock();
    if !stdin.is_terminal() {
        println!("      Web UI not started (pass --ui to start it on {addr})");
        return false;
    }
    print!("      Start the web UI on {addr}? [y/N] ");
    // The prompt has no trailing newline, so it sits in the line buffer until flushed.
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if stdin.read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Loads `--plugin` libraries. Called per subcommand rather than once up front
/// so it runs after that command installed its logging — the loader logs what
/// it registered, and before a subscriber exists those lines go nowhere.
fn load_cli_plugins(paths: &[String]) -> anyhow::Result<()> {
    install_signal_handlers();
    mq_bridge_app::plugins::load_trusted_plugins(paths, &std::collections::HashMap::new())?;
    Ok(())
}

/// Registers SIGINT and SIGTERM before any plugin loads (see
/// [`mq_bridge::shutdown::install_signal_handlers`]); a second signal force-exits.
fn install_signal_handlers() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        if let Err(e) =
            mq_bridge::shutdown::install_signal_handlers(|code| std::process::exit(code))
        {
            warn!("Failed to install shutdown signal handlers: {e}");
        }
    });
}

/// Resolves once SIGINT or SIGTERM has been received, including before this call.
pub(crate) async fn shutdown_requested() {
    install_signal_handlers();
    mq_bridge::shutdown::shutdown_requested().await;
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // Initialize the default crypto provider for rustls (required for rustls 0.23.0+)
    // This allows mq-bridge to create TLS configurations for secure endpoints.
    #[cfg(feature = "rustls-aws-lc")]
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let args = Args::parse();

    match args.command {
        Some(Command::Copy(copy_args)) => {
            init_copy_logging(args.color, copy_args.verbose);
            load_cli_plugins(&args.plugins)?;
            return run_copy(copy_args, StopWhen::SourceDrained).await;
        }
        Some(Command::AgentListen(listen_args)) => {
            init_copy_logging(args.color, listen_args.verbose);
            load_cli_plugins(&args.plugins)?;
            return run_agent_listen(listen_args).await;
        }
        Some(Command::Mcp(mcp_args)) => {
            // The install actions configure clients and exit; only the bare
            // `mcp` command actually serves.
            match mcp_args.action {
                Some(McpAction::Install {
                    client,
                    local,
                    print_config,
                    agent_bus,
                }) => {
                    return if print_config {
                        mcp_install::print_config(agent_bus)
                    } else {
                        mcp_install::install(client, local, agent_bus)
                    };
                }
                Some(McpAction::Uninstall { client, local }) => {
                    return mcp_install::uninstall(client, local);
                }
                Some(McpAction::Status { local }) => return mcp_install::status(local),
                None => {}
            }

            // stdio transport uses stdout as the MCP channel, so logs must go to stderr.
            init_mcp_logging(args.color);
            load_cli_plugins(&args.plugins)?;
            let workspace_path = config_file_path(args.config.clone());
            return mcp::run(
                mcp_args.transport,
                mcp_args.bind,
                mcp_args.report_to_ui && !mcp_args.no_report_to_ui,
                mcp_args.agent_bus,
                workspace_path,
            )
            .await;
        }
        Some(Command::Checkpoint(checkpoint_args)) => {
            init_copy_logging(args.color, checkpoint_args.verbose());
            load_cli_plugins(&args.plugins)?;
            return checkpoint_cmd::run(checkpoint_args, args.config, args.config_str).await;
        }
        Some(Command::Status(status_args)) => {
            return status_cmd::run(status_args, args.color).await;
        }
        Some(Command::ToCli) => {
            let (config, _) = load_config(
                args.config,
                args.init_config,
                args.init_config_str,
                args.config_str,
            )
            .context("Failed to load configuration")?;
            let export = mq_bridge_app::cli_command::inline_config_command(&config)?;
            println!("{}", export.command);
            if !export.required_env.is_empty() {
                eprintln!(
                    "Required environment variables: {}",
                    export.required_env.join(", ")
                );
            }
            return Ok(());
        }
        None => {}
    }

    if let Some(schema_path) = args.schema {
        let schema = mq_bridge_app::config::app_config_schema();
        let schema_json =
            serde_json::to_string_pretty(&schema).context("Failed to serialize schema")?;

        if schema_path == "-" {
            println!("{}", schema_json);
        } else {
            let path = std::path::Path::new(&schema_path);
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                std::fs::create_dir_all(parent)
                    .context("Failed to create parent directory for schema")?;
            }
            std::fs::write(path, schema_json).context("Failed to write schema file")?;
        }
        return Ok(());
    }

    // The UI saves to this path, and an init source fills it; otherwise a missing file is a typo.
    if let Some(path) = &args.config {
        let has_other_source = args.ui
            || args.init_config.is_some()
            || args.init_config_str.is_some()
            || args.config_str.is_some()
            || ["INIT_CONFIG_FILE", "INIT_CONFIG_STRING", "CONFIG_STRING"]
                .iter()
                .any(|name| std::env::var_os(name).is_some());
        if !has_other_source && !std::path::Path::new(path).exists() {
            anyhow::bail!("configuration file '{path}' does not exist");
        }
    }
    let (mut config, config_file_path): (AppConfig, String) = load_config(
        args.config,
        args.init_config,
        args.init_config_str,
        args.config_str,
    )
    .context("Failed to load configuration")?;
    init_logging(&config, args.color);
    load_cli_plugins(&args.plugins)?;
    mq_bridge_app::plugins::load_trusted_plugins(&config.plugins, &config.env_vars)?;
    println!(
        r#"
      ┌────── mq-bridge-app ──────┐
──────┴───────────────────────────┴──────"#
    );

    // --- Logic for default addresses ---
    // When no persisted config file exists (common in http/no-tauri dev mode), ensure
    // UI + metrics are reachable with sane defaults.
    let has_persisted_config = std::path::Path::new(&config_file_path).exists();
    let unconfigured = !has_persisted_config || config.consumers.is_empty();
    if let Some(addr) = args.metrics_addr {
        config.metrics_addr = addr;
    } else if args.no_metrics {
        config.metrics_addr = String::new();
    } else if unconfigured && config.metrics_addr.is_empty() {
        config.metrics_addr = DEFAULT_METRICS_ADDR.to_string();
    }
    // The UI is a control surface, so its port is the one address never opened
    // implicitly: a `ui_addr` in the config counts as consent, an accidental
    // bare run does not. `--ui` opts in even when a config leaves the address
    // empty; `--no-ui` withdraws that consent even when the config gives one;
    // otherwise the previously automatic default has to be confirmed.
    //
    // The prompt is offered only when there is nothing to run, which is the
    // "start empty and build a config in the UI" case. A config that defines
    // routes or consumers is a deployment: it must never stop at a question.
    if args.no_ui {
        config.ui_addr = String::new();
    } else if config.ui_addr.is_empty() {
        // The container image sets this to every interface; a host stays on loopback.
        let default_addr = std::env::var(DEFAULT_UI_ADDR_ENV)
            .ok()
            .filter(|addr| !addr.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_UI_ADDR.to_string());
        if args.ui || (nothing_to_run(&config) && ui_prompt(&default_addr)) {
            config.ui_addr = default_addr;
        }
    }

    let mut prom_addr = None;
    // --- 2. Initialize Prometheus Metrics Exporter ---
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    let (recorder, metrics_task) =
        if !config.metrics_addr.is_empty() && config.metrics_addr != config.ui_addr {
            let addr: SocketAddr = config.metrics_addr.parse().context(format!(
                "Failed to parse metrics listen address: {}",
                config.metrics_addr
            ))?;
            let (recorder, server_future) = builder.with_http_listener(addr).build()?;
            prom_addr = Some(addr);
            (recorder, Some(tokio::spawn(server_future)))
        } else {
            (builder.build_recorder(), None)
        };
    let prometheus_handle = recorder.handle();
    metrics::set_global_recorder(recorder).context("Failed to install Prometheus recorder")?;
    #[cfg(feature = "otel")]
    let otel_guard = mq_bridge_app::otel_export::init_from_env()
        .context("Failed to install OpenTelemetry OTLP exporter")?;

    // `metrics-exporter-prometheus` only drains its histogram buckets during
    // upkeep. The `build()` (http-listener) branch above spawns its own upkeep
    // task, but `build_recorder()` does not, so without this the per-message
    // `queue_message_processing_duration_seconds` samples recorded by mq-bridge
    // accumulate in an unbounded AtomicBucket and slowly leak memory. Drive
    // upkeep manually whenever we built the recorder without a listener.
    if metrics_task.is_none() {
        let upkeep_handle = prometheus_handle.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                upkeep_handle.run_upkeep();
            }
        });
    }

    metrics::describe_gauge!(
        "mq_bridge_app_info",
        "Information about the mq-bridge-app application"
    );
    // Standard Prometheus pattern: use a fixed value of 1.0 for info metrics,
    // encoding the actual data (version, etc.) in the labels.
    metrics::gauge!("mq_bridge_app_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);

    // Headless only: with a web UI running, its own `UiApp` owns the lease and
    // publishes richer state, so a second lease for the same process would just
    // duplicate every row.
    let mut cli_status = None;
    let workspace_path = config_file_path.clone();

    // Start Web UI
    // Headless, this owns the consumers it started: dropping the app stops their
    // routes, so it has to live until shutdown. With a UI, `start_web_server`
    // owns them for as long as it serves.
    let mut headless_app = None;
    // Headless and all routes drain: how many of them failed to start.
    let mut drain_job = None;
    let web_ui_handle = if !config.ui_addr.is_empty() {
        let addr = &config.ui_addr;
        let socket_addr: SocketAddr = addr
            .parse()
            .with_context(|| format!("Failed to parse UI listen address: {}", addr))?;
        let port = socket_addr.port();
        let host = if socket_addr.ip().is_unspecified() {
            "localhost".to_string()
        } else {
            socket_addr.ip().to_string()
        };
        println!(
            r#"      Web UI: http://{}:{}
"#,
            host, port
        );
        info!(
            "Prometheus metrics enabled on Web UI (http://{}/metrics)",
            config.ui_addr
        );
        if !socket_addr.ip().is_loopback() {
            warn!(
                "The web UI has no login and listens on {}: anyone who can reach that address can read and change the configuration, secrets included",
                config.ui_addr
            );
        }

        let web_ui_server = web_ui::start_web_server(
            addr.into(),
            config.clone(),
            args.plugins.clone(),
            prometheus_handle,
            config_file_path,
        );
        Some(tokio::spawn(web_ui_server))
    } else {
        println!(
            r#"        Starting without UI server
"#
        );
        // No UI means no other owner for what the config describes, so the
        // consumers are started here — otherwise a headless deployment loads a
        // config and then runs nothing.
        let app = UiApp::new_with_startup_plugins(
            config.clone(),
            prometheus_handle,
            config_file_path,
            &args.plugins,
        )?
        .with_instance_kind(InstanceKind::Cli);
        cli_status = cli_status_lease(workspace_path, config.clone(), app.clone());
        let enabled_consumers = config
            .consumers
            .iter()
            .filter(|consumer| consumer.enabled)
            .count();
        let started_consumers = app.start_configured_consumers().await;
        if started_consumers < enabled_consumers {
            warn!("Started {started_consumers} of {enabled_consumers} enabled consumers");
        }
        if enabled_consumers > 0 && started_consumers == 0 {
            anyhow::bail!("none of the {enabled_consumers} enabled consumers could be started");
        }
        // Every enabled route drains, so the run is a batch job that ends with them.
        if enabled_consumers > 0
            && config
                .consumers
                .iter()
                .filter(|consumer| consumer.enabled)
                .all(|consumer| consumer.options.exit_on_empty)
        {
            drain_job = Some(enabled_consumers - started_consumers);
        }
        headless_app = Some(app);
        None
    };
    if let Some(addr) = prom_addr {
        info!("Prometheus exporter listening on http://{}", addr);
    }

    if config.consumers.is_empty() {
        if config.ui_addr.is_empty() {
            warn!("Nothing to run: this config defines no routes or consumers.");
        } else {
            warn!("No consumers configured. Waiting for configuration via Web UI.");
        }
    }

    info!("Bridge running. Waiting for signal.");

    let mut drained = Ok(());
    tokio::select! {
        _ = shutdown_requested() => {},
        result = async {
            match (&headless_app, drain_job) {
                (Some(app), Some(unstarted)) => routes_drained(app, unstarted).await,
                _ => std::future::pending().await,
            }
        } => {
            info!("Every route has drained.");
            drained = result;
        },
    }

    info!("Shutdown signal received. Broadcasting to all tasks...");

    // Dropping the app releases its route handles without stopping the underlying
    // routes; the `stop_route` loop below performs shutdown. The lease holds a
    // clone of the app, so it goes first.
    drop(cli_status);
    drop(headless_app);

    let shutdown_task = async {
        let stopped = mq_bridge::shutdown::stop_all_routes().await;
        if !stopped.is_empty() {
            info!("Stopped {} routes.", stopped.len());
        }
    };

    if tokio::time::timeout(Duration::from_secs(10), shutdown_task)
        .await
        .is_err()
    {
        warn!("Graceful shutdown timed out after 10 seconds. Forcing shutdown.");
    } else {
        info!("All routes stopped gracefully.");
    }

    // Abort the metrics task if it's running. It doesn't support graceful shutdown.
    if let Some(task) = metrics_task {
        task.abort();
    }

    if let Some(handle) = web_ui_handle {
        handle.abort();
    }

    #[cfg(feature = "otel")]
    drop(otel_guard);

    info!("Shutdown complete.");

    drained
}

/// Waits until every started headless route has ended, then judges the run the
/// way `copy --drain` does.
async fn routes_drained(app: &UiApp, unstarted: usize) -> anyhow::Result<()> {
    loop {
        let outcomes = app.consumer_outcomes().await;
        if outcomes.iter().all(|(_, outcome, _)| outcome.is_some()) {
            return drain_result(&outcomes, unstarted);
        }
        tokio::time::sleep(COPY_POLL_INTERVAL).await;
    }
}

/// A drained run failed if a route failed, did not start, or left an error behind.
fn drain_result(
    outcomes: &[(
        String,
        Option<mq_bridge::route::RouteOutcome>,
        Option<String>,
    )],
    unstarted: usize,
) -> anyhow::Result<()> {
    use mq_bridge::route::RouteOutcome;

    let mut problems: Vec<String> = outcomes
        .iter()
        .filter_map(|(name, outcome, error)| match (outcome, error) {
            (_, Some(error)) => Some(format!("route '{name}': {error}")),
            (Some(RouteOutcome::Failed), None) => {
                Some(format!("route '{name}' failed: no error reported"))
            }
            _ => None,
        })
        .collect();
    if unstarted > 0 {
        problems.push(format!("{unstarted} route(s) did not start"));
    }
    if !problems.is_empty() {
        anyhow::bail!("not every route drained cleanly: {}", problems.join("; "));
    }
    Ok(())
}

/// Advertises a headless run: the configured entities, with running state read
/// from the live route registry rather than assumed.
fn cli_status_lease(workspace_path: String, config: AppConfig, app: UiApp) -> Option<StatusLease> {
    let heartbeat_config = config.clone();
    StatusLease::spawn(
        InstanceKind::Cli,
        env!("CARGO_PKG_VERSION"),
        &workspace_path,
        move || {
            let config = heartbeat_config.clone();
            let app = app.clone();
            async move {
                let running = mq_bridge::list_routes();
                let is_running = |name: &str| running.iter().any(|route| route == name);
                let mut tracked = app.tracked_consumer_summaries().await;
                StatusSnapshot {
                    consumers: config
                        .consumers
                        .iter()
                        .map(|consumer| {
                            let id = consumer_runtime_key(consumer);
                            // No handle (not started here): the route registry decides.
                            let summary = tracked.remove(&id).unwrap_or_else(|| {
                                let running = is_running(&collector_route_name(&id));
                                StatusSummary {
                                    running,
                                    healthy: running,
                                    ..Default::default()
                                }
                            });
                            StatusEntity {
                                label: consumer.name.clone(),
                                endpoint: endpoint_type_label(&consumer.endpoint.endpoint_type)
                                    .to_string(),
                                summary,
                                id,
                            }
                        })
                        .collect(),
                    publishers: config
                        .publishers
                        .iter()
                        .map(|publisher| StatusEntity {
                            id: publisher.id.clone(),
                            label: publisher.name.clone(),
                            endpoint: endpoint_type_label(&publisher.endpoint.endpoint_type)
                                .to_string(),
                            summary: StatusSummary::default(),
                        })
                        .collect(),
                    routes: running
                        .iter()
                        // Collector routes are already listed as consumers above.
                        .filter(|name| !name.starts_with("ui_collector_route_"))
                        .map(|name| route_entity(name, &config))
                        .collect(),
                }
            }
        },
    )
}

/// A running route as a linked input/output pair.
fn route_entity(name: &str, config: &AppConfig) -> StatusRoute {
    let summary = StatusSummary {
        running: true,
        healthy: true,
        ..Default::default()
    };
    let (input, output) = config
        .routes
        .get(name)
        .map(|route| {
            (
                endpoint_type_label(&route.route.input.endpoint_type),
                endpoint_type_label(&route.route.output.endpoint_type),
            )
        })
        .unwrap_or(("unknown", "unknown"));
    StatusRoute {
        id: name.to_string(),
        label: name.to_string(),
        input: StatusEntity {
            id: format!("{name}:input"),
            label: name.to_string(),
            endpoint: input.to_string(),
            summary: summary.clone(),
        },
        output: StatusEntity {
            id: format!("{name}:output"),
            label: name.to_string(),
            endpoint: output.to_string(),
            summary: summary.clone(),
        },
        summary,
    }
}
/// When a copy stops.
#[derive(Clone, Copy, PartialEq, Debug)]
enum StopWhen {
    /// What `copy` does: the source running dry, or Ctrl-C without `--drain`.
    SourceDrained,
    /// What `agent-listen` does: the first message to arrive. The route runs as a
    /// continuous bridge, so the source is never let go of while waiting and an
    /// exclusive claim (`dir_spool`) stays held throughout. Anything that lands
    /// after the decision to stop simply waits for the next run.
    FirstMessage,
}

/// Whether this copy is drain-then-exit.
///
/// `--wait` is long polling over a drain: it governs only how long we look for
/// the first message, so it implies `--drain` rather than competing with it.
fn drains(args: &CopyArgs) -> bool {
    args.drain || args.wait.is_some()
}

/// Runs the `agent-listen` subcommand: holds this machine's agent inbox open
/// until mail arrives, writes it out and exits.
///
/// Deliberately a thin wrapper over [`run_copy`] rather than its own consumer:
/// the inbox is an ordinary `dir_spool` queue, so the only thing this adds is
/// knowing where it lives and stopping at the first message.
async fn run_agent_listen(args: AgentListenArgs) -> anyhow::Result<()> {
    let name = mcp::validate_agent_name(&args.name)?;
    let inbox = mcp::agents_root()?.join(name);
    // The inbox has to exist before a consumer can hold it, and an agent that
    // listens before anyone has written to it is the normal first run.
    std::fs::create_dir_all(&inbox)
        .with_context(|| format!("could not create agent inbox {}", inbox.display()))?;

    let to = match args.to {
        Some(to) => to,
        None => {
            let out = std::env::temp_dir().join(format!("mqb-agent-{name}.jsonl"));
            println!("writing mail to {}", out.display());
            // `raw` writes the sender's envelope as-is; the default format would
            // wrap it in a second JSON object with the payload as a string.
            format!("file://{}?format=raw", out.display())
        }
    };

    // With no budget there is no first message to hold out for, so `--wait 0` is a
    // plain drain of what is already queued — the same reading `copy --wait 0` has.
    // Waiting for a delivery that has not happened yet is what `FirstMessage` is for.
    let take_queued_only = args.wait == 0;

    run_copy(
        CopyArgs {
            // `metadata_extension=` (empty) matches what `agent_send` writes: one
            // message is one file, with no sidecar.
            from: Some(format!("spool://{}?metadata_extension=", inbox.display())),
            to: Some(to),
            source: None,
            target: None,
            filter: None,
            limit: None,
            resume: false,
            no_resume: false,
            // Draining is what `--wait 0` means here; any other budget holds the
            // inbox open and stops at the first delivery instead.
            drain: take_queued_only,
            wait: Some(args.wait),
            concurrency: Some(1),
            batch_size: None,
            verbose: args.verbose,
        },
        if take_queued_only {
            StopWhen::SourceDrained
        } else {
            StopWhen::FirstMessage
        },
    )
    .await
}

/// Stops a route and waits for its task to actually end, reporting how it ended.
///
/// `RouteHandle::stop` only signals shutdown: it returns while the route is still
/// publishing what it has already read. Every caller here reports a tally taken
/// on the source side, so settling first is what keeps the summary from claiming
/// rows the destination never received.
async fn stop_and_settle(
    handle: &mq_bridge::route::RouteHandle,
) -> Option<mq_bridge::route::RouteOutcome> {
    handle.stop().await;
    tokio::time::timeout(COPY_STOP_TIMEOUT, async {
        loop {
            if let Some(outcome) = handle.outcome() {
                break outcome;
            }
            tokio::time::sleep(COPY_POLL_INTERVAL).await;
        }
    })
    .await
    .inspect_err(|_| warn!("route did not finish stopping; the summary may overcount"))
    .ok()
}

/// Runs the `copy` subcommand: builds a single in-memory route from the `--from`
/// and `--to` URIs and awaits its completion. With `--drain` the underlying route
/// exits once the source is empty; otherwise it runs until Ctrl-C.
async fn run_copy(args: CopyArgs, stop_when: StopWhen) -> anyhow::Result<()> {
    use mq_bridge::models::{Route, RouteOptions};
    use mq_bridge::route::RouteOutcome;

    let (from, to) = copy_endpoints(&args)?;
    let (mut input, output) = copy_route_endpoints(from, to)?;
    warn_about_surprising_copy(&input, &output);
    let resume = if args.resume {
        Some(copy_pipeline::configure_resume(
            &mut input,
            &output,
            args.filter.as_deref(),
        )?)
    } else {
        None
    };
    // The label names the steady-state source, not the `sequence` wrapper below.
    let input_endpoint_label = endpoint_type_label(&input.endpoint_type);
    // Attached outside every other source middleware, so it sees only what the
    // whole chain let through — past the filter below and past any URI-configured
    // `transform` that rejects rows: what was copied.
    let copied = copy_pipeline::configure_delivered_counter(&mut input, args.limit)?;
    if let Some(expression) = &args.filter {
        copy_pipeline::configure_filter(&mut input, expression);
    }
    // Innermost, so it tallies everything the source produced. The rate is derived
    // from this: a selective filter reduces what lands at the destination without
    // making the copy any slower, and rating the surviving rows against the time
    // spent reading every row reports that as a slowdown.
    let read = copy_pipeline::configure_counter(&mut input)?;
    let output_endpoint_label = endpoint_type_label(&output.endpoint_type);
    // `FirstMessage` deliberately does not drain: a drained route releases the
    // source, and reacquiring it between polls is what opens the window a second
    // listener can slip through.
    let drain = drains(&args) && stop_when == StopWhen::SourceDrained;
    let options = RouteOptions {
        concurrency: args.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        batch_size: args
            .batch_size
            .unwrap_or_else(|| default_batch_size(&output)),
        exit_on_empty: drain,
        ..Default::default()
    };

    let route = Route::new(input, output).with_options(options);
    let run_id = format!("copy-{}", uuid::Uuid::new_v4());
    let started = std::time::Instant::now();
    // An empty source drains instantly, so waiting for one means retrying that
    // drain until an attempt finds something or the budget is spent.
    let wait_until = args.wait.map(|secs| started + Duration::from_secs(secs));
    let _progress = CopyProgress::start(Arc::clone(&copied), args.verbose);

    info!(
        // Redacted: this line is the one that reaches journald, Docker logs and CI.
        from = %copy_pipeline::redact_uri(from),
        to = %copy_pipeline::redact_uri(to),
        filtered = args.filter.is_some(),
        // Names the mechanism, not just the flag: which one the source picked
        // is what tells you where a restart will actually pick up from.
        resume = resume.map_or("off", copy_pipeline::ResumeCapability::as_str),
        drain,
        wait_secs = args.wait,
        "copy route started"
    );

    loop {
        let handle = if args.no_resume {
            route.run_without_resume(&run_id).await
        } else {
            route.run(&run_id).await
        };
        let handle = Arc::new(handle.context("failed to start copy route")?);
        let copy_status = copy_status_lease(
            run_id.clone(),
            input_endpoint_label.to_string(),
            output_endpoint_label.to_string(),
            handle.clone(),
            Arc::clone(&copied),
            started,
        );

        if stop_when == StopWhen::FirstMessage {
            // Stop on whichever comes first: mail, the budget running out, or
            // Ctrl-C. All three end the same way, since the rows already copied
            // are the answer in every case.
            tokio::select! {
                _ = async {
                    loop {
                        if copied.load(std::sync::atomic::Ordering::Relaxed) > 0 {
                            break;
                        }
                        tokio::time::sleep(COPY_POLL_INTERVAL).await;
                    }
                } => {}
                _ = async {
                    match wait_until {
                        Some(deadline) => {
                            tokio::time::sleep(
                                deadline.saturating_duration_since(std::time::Instant::now()),
                            )
                            .await
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => info!("nothing arrived within the wait budget"),
                _ = shutdown_requested() => info!("Shutdown requested; stopping listen"),
            }
            let outcome = stop_and_settle(&handle).await;
            return copy_result(
                outcome.or(Some(RouteOutcome::Stopped)),
                handle.status().error,
                &throughput(&copied, &read, started),
            );
        }

        if !drain {
            // Continuous bridge: run until Ctrl-C, then stop gracefully. A source
            // that ends by itself (stdin at end of input) ends the copy too.
            tokio::select! {
                _ = shutdown_requested() => info!("Shutdown requested; stopping copy"),
                _ = async {
                    while handle.outcome().is_none() {
                        tokio::time::sleep(COPY_POLL_INTERVAL).await;
                    }
                } => {}
            }
            // Through the same reporting as the drained branch: a bridge that dropped
            // rows did not run clean either, and a supervisor restarting it needs to
            // hear that from the exit status. The fallback only guards against a
            // route that outlasts `COPY_STOP_TIMEOUT` without resolving.
            let outcome = stop_and_settle(&handle).await;
            return copy_result(
                outcome.or(Some(RouteOutcome::Stopped)),
                handle.status().error,
                &throughput(&copied, &read, started),
            );
        }

        // One-shot: run until the source is drained, or abort on Ctrl-C.
        //
        // The route task ends on a permanent error just as it does on a real drain, so
        // joining it alone cannot tell a succeeded batch job from a failed one — and
        // `join` consumes the handle, making the outcome unreadable afterwards. Poll
        // `outcome()` instead (the same completion signal `wait_route` uses) so a cron
        // or systemd-timer invocation gets a non-zero exit when nothing was copied.
        let outcome = tokio::select! {
            outcome = async {
                loop {
                    if let Some(outcome) = handle.outcome() {
                        break outcome;
                    }
                    tokio::time::sleep(COPY_POLL_INTERVAL).await;
                }
            } => Some(outcome),
            _ = shutdown_requested() => {
                info!("Shutdown requested; aborting copy");
                None
            }
        };

        // Interrupted: shut the route down the same way the continuous branch does,
        // so the source connection and any checkpoint are released before we exit.
        // A route that had already failed under the Ctrl-C says so through the
        // settled outcome; a healthy one reports the same `Stopped` either way.
        if outcome.is_none() {
            let settled = stop_and_settle(&handle).await;
            return copy_result(
                settled,
                handle.status().error,
                &throughput(&copied, &read, started),
            );
        }

        // Drained with nothing to show and budget left: drop this route and look
        // again. The tally is registered globally, so a later attempt keeps adding
        // to the same counter rather than starting over.
        if copied.load(std::sync::atomic::Ordering::Relaxed) == 0
            && matches!(outcome, Some(RouteOutcome::Completed))
            && handle.status().error.is_none()
            && wait_until.is_some_and(|deadline| std::time::Instant::now() < deadline)
        {
            drop(copy_status);
            tokio::select! {
                _ = tokio::time::sleep(COPY_WAIT_RETRY_INTERVAL) => continue,
                _ = shutdown_requested() => {
                    info!("Shutdown requested; aborting copy");
                    return copy_result(None, None, &throughput(&copied, &read, started));
                }
            }
        }

        return copy_result(
            outcome,
            handle.status().error,
            &throughput(&copied, &read, started),
        );
    }
}

/// Warns about two combinations that run without an error and move less than expected.
fn warn_about_surprising_copy(
    input: &mq_bridge::models::Endpoint,
    output: &mq_bridge::models::Endpoint,
) {
    let source = input.endpoint_type.name();
    if output.endpoint_type.name() == "response"
        && matches!(
            source,
            "file" | "sqlx" | "object_store" | "static" | "clickhouse" | "dir_spool"
        )
    {
        warn!(
            "`response:` replies to the source, and a {source} source takes no replies: every row is read and discarded"
        );
    }
    if source == "sqlx" {
        let config = serde_json::to_value(&input.endpoint_type).unwrap_or_default();
        let sqlx = &config["sqlx"];
        let unset = |field: &str| {
            matches!(
                &sqlx[field],
                serde_json::Value::Null | serde_json::Value::Bool(false)
            )
        };
        if unset("cursor_column") && unset("delete_after_read") && unset("select_query") {
            warn!(
                "the source table is read as a work queue: rows are leased and left in place, so a repeated copy first finds nothing and later copies them again. Set `cursor_column` or `delete_after_read=true`"
            );
        }
    }
}

/// Builds a copy's source and destination. `checkpoint` shares it, so both derive
/// the same resume identity from the same URIs.
fn copy_route_endpoints(
    from: &str,
    to: &str,
) -> anyhow::Result<(mq_bridge::models::Endpoint, mq_bridge::models::Endpoint)> {
    // Expanded here rather than by the shell, so a single-quoted URI can name a
    // credential without it ever appearing in the history or in `argv`.
    let from = copy_pipeline::expand_uri_variables(&dash_as_stream(from, "/dev/stdin"))
        .context("invalid copy source endpoint")?;
    let to = copy_pipeline::expand_uri_variables(&dash_as_stream(to, "/dev/stdout"))
        .context("invalid copy destination endpoint")?;
    if to.starts_with("file:///dev/stdout") {
        SUMMARY_TO_STDERR.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let mut input = endpoint_from_uri(&from).context("invalid copy source endpoint")?;
    make_listen_address(&mut input).context("invalid copy source endpoint")?;
    let output = endpoint_from_uri(&to).context("invalid copy destination endpoint")?;
    Ok((input, output))
}

/// How often a copy on a terminal redraws its running count.
const COPY_PROGRESS_INTERVAL: Duration = Duration::from_secs(2);

/// A running row count on stderr while a copy is in flight. Only on a terminal, so a
/// log file or a pipe never sees it; dropped (and its line cleared) before the summary.
struct CopyProgress(Option<tokio::task::JoinHandle<()>>);

impl CopyProgress {
    fn start(copied: Arc<std::sync::atomic::AtomicU64>, verbose: bool) -> Self {
        use std::io::IsTerminal;
        if verbose || !std::io::stderr().is_terminal() {
            return Self(None);
        }
        Self(Some(tokio::spawn(async move {
            let mut before = 0;
            loop {
                tokio::time::sleep(COPY_PROGRESS_INTERVAL).await;
                let rows = copied.load(std::sync::atomic::Ordering::Relaxed);
                let rate = (rows - before) as f64 / COPY_PROGRESS_INTERVAL.as_secs_f64();
                before = rows;
                PROGRESS_DRAWN.store(true, std::sync::atomic::Ordering::Relaxed);
                eprint!(
                    "\r\x1b[2K{} rows ({} rows/s)",
                    grouped(rows),
                    grouped(rate as u64)
                );
            }
        })))
    }
}

impl Drop for CopyProgress {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            clear_progress();
        }
    }
}

static PROGRESS_DRAWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Wipes the progress line, so the summary or an error starts on a clean line.
fn clear_progress() {
    if PROGRESS_DRAWN.swap(false, std::sync::atomic::Ordering::Relaxed) {
        eprint!("\r\x1b[2K");
    }
}

/// Set when the rows themselves go to stdout, so the summary does not land among them.
static SUMMARY_TO_STDERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `-`, alone or followed by `?params` or `|middleware`, names stdin or stdout.
fn dash_as_stream(uri: &str, device: &str) -> String {
    match uri.strip_prefix('-') {
        Some(rest) if rest.is_empty() || rest.starts_with(['?', '|']) => {
            format!("file://{device}{rest}")
        }
        _ => uri.to_string(),
    }
}

fn copy_endpoints(args: &CopyArgs) -> anyhow::Result<(&str, &str)> {
    match (
        args.from.as_deref(),
        args.to.as_deref(),
        args.source.as_deref(),
        args.target.as_deref(),
    ) {
        (Some(from), Some(to), None, None) => Ok((from, to)),
        (None, None, Some(source), Some(target)) => Ok((source, target)),
        (None, None, None, None) => anyhow::bail!(
            "copy requires SOURCE and TARGET, either positionally or with --from and --to"
        ),
        (Some(_), None, None, None) | (None, Some(_), None, None) => {
            anyhow::bail!("copy requires both --from and --to")
        }
        (None, None, Some(_), None) | (None, None, None, Some(_)) => {
            anyhow::bail!("copy positional syntax requires both SOURCE and TARGET")
        }
        _ => anyhow::bail!("do not mix positional SOURCE/TARGET with --from/--to"),
    }
}

fn copy_status_lease(
    run_id: String,
    input_endpoint: String,
    output_endpoint: String,
    handle: Arc<mq_bridge::route::RouteHandle>,
    copied: Arc<std::sync::atomic::AtomicU64>,
    started: std::time::Instant,
) -> Option<StatusLease> {
    let started_at_ms = now_ms().saturating_sub(started.elapsed().as_millis() as u64);
    let mut sample = None;
    StatusLease::spawn(
        InstanceKind::Cli,
        env!("CARGO_PKG_VERSION"),
        "copy",
        move || {
            let handle = Arc::clone(&handle);
            let run_id = run_id.clone();
            let input_endpoint = input_endpoint.clone();
            let output_endpoint = output_endpoint.clone();
            let messages = copied.load(std::sync::atomic::Ordering::Relaxed);
            let now = std::time::Instant::now();
            let current = next_route_metric_sample(sample, messages as f64, now);
            sample = Some(current);
            // Up to the last growth, so an idle route keeps its achieved rate.
            let elapsed = current
                .last_change_at
                .saturating_duration_since(started)
                .as_secs_f64();
            async move {
                let status = handle.status();
                let summary = StatusSummary {
                    running: handle.outcome().is_none(),
                    healthy: status.healthy,
                    error: status.error.clone(),
                    throughput: current.smoothed_throughput,
                    message_sequence: messages,
                    started_at_ms: Some(started_at_ms),
                    outcome: handle.outcome().map(Into::into),
                    average_throughput: if elapsed > 0.0 {
                        messages as f64 / elapsed
                    } else {
                        0.0
                    },
                    ..Default::default()
                };
                StatusSnapshot {
                    routes: vec![StatusRoute {
                        id: run_id.clone(),
                        label: "copy".to_string(),
                        input: StatusEntity {
                            id: format!("{}:input", run_id),
                            label: "copy".to_string(),
                            endpoint: input_endpoint,
                            summary: summary.clone(),
                        },
                        output: StatusEntity {
                            id: format!("{}:output", run_id),
                            label: "copy".to_string(),
                            endpoint: output_endpoint,
                            summary: summary.clone(),
                        },
                        summary,
                    }],
                    ..Default::default()
                }
            }
        },
    )
}

/// What a finished copy moved, as reported on the last line of the run.
struct Throughput {
    /// Messages that reached the destination — what a filter left behind.
    rows: u64,
    /// Messages taken off the source, filtered or not. The rate is derived from
    /// this, since the elapsed time covers reading all of them.
    read: u64,
    /// Rows the source could not decrypt.
    rejected: u64,
    /// Rows a `dlq` middleware diverted to its dead-letter target.
    dead_lettered: u64,
    elapsed_s: f64,
    rows_per_second: u64,
}

impl Throughput {
    /// `copied 333_495 of 1_000_000 rows` when a filter dropped some, plain
    /// `copied 1_000_000 rows` when nothing was dropped.
    fn rows_display(&self) -> String {
        // Dead-lettered rows left the source like any other but did not reach the target.
        let rows = self.rows.saturating_sub(self.dead_lettered);
        let mut display = if self.read > self.rows {
            format!("{} of {} rows", grouped(rows), grouped(self.read))
        } else {
            format!("{} rows", grouped(rows))
        };
        if self.dead_lettered > 0 {
            display.push_str(&format!(", dead-lettered {}", grouped(self.dead_lettered)));
        }
        display
    }

    /// Sub-second runs read as milliseconds: `0.07s` hides whether a copy took
    /// 65 ms or 5 ms, and `0.00s` looks like a broken measurement.
    fn elapsed_display(&self) -> String {
        if self.elapsed_s < 1.0 {
            format!("{:.0}ms", self.elapsed_s * 1000.0)
        } else {
            format!("{:.2}s", self.elapsed_s)
        }
    }

    fn rate_display(&self) -> String {
        grouped(self.rows_per_second)
    }

    /// The one line every `copy` prints, whatever the log level. Written to stdout
    /// directly rather than through `tracing`, so silencing the bridge's logging
    /// never silences the answer the user ran the command for.
    fn report(&self, verb: &str) {
        let line = format!(
            "{verb} {} in {} ({} rows/s)",
            self.rows_display(),
            self.elapsed_display(),
            self.rate_display()
        );
        if SUMMARY_TO_STDERR.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}

/// Groups digits in threes so a rate stays legible at a glance: `703871` reads as
/// `703_871`.
///
/// Underscore rather than a comma, which half the world reads as a decimal point,
/// or a thin space, which would split the field for anything parsing this line on
/// whitespace.
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('_');
        }
        out.push(digit);
    }
    out
}

/// Rates a finished copy. A run too short to time is reported as no rate rather
/// than a number divided by an elapsed time that rounds to zero.
///
/// The rate is rounded to whole rows: the elapsed time includes connection setup,
/// so it approximates a whole run rather than benchmarking the transfer.
fn throughput(
    copied: &std::sync::atomic::AtomicU64,
    read: &std::sync::atomic::AtomicU64,
    started: std::time::Instant,
) -> Throughput {
    let rows = copied.load(std::sync::atomic::Ordering::Relaxed);
    let read = read.load(std::sync::atomic::Ordering::Relaxed);
    let elapsed_s = started.elapsed().as_secs_f64();
    // `unpack` delivers more rows than it reads, so rate the larger count.
    let rows_per_second = if elapsed_s > 0.0 {
        (read.max(rows) as f64 / elapsed_s).round() as u64
    } else {
        0
    };
    Throughput {
        rows,
        read,
        rejected: mq_bridge::middleware::rejected_input_messages(),
        dead_lettered: mq_bridge::middleware::dead_lettered_messages(),
        elapsed_s,
        rows_per_second,
    }
}

/// Maps a finished `copy` route to the process result: a route killed by a
/// permanent error must not exit 0, or a cron/timer job reports success while having
/// copied nothing. `None` means Ctrl-C interrupted the wait.
///
/// A route that reached its end can *also* have thrown data away, and that case
/// does not arrive as `Failed`: a sink that rejects a message permanently with no
/// `dlq` middleware to catch it drops the message and the route keeps going, by
/// design — one poison message must not wedge a bridge. mq-bridge tallies those
/// drops and publishes them as the route's error when it ends, precisely so the
/// caller can tell a clean run from a lossy one. Rows that were dropped never
/// reached the destination, so that has to reach the exit status too.
fn copy_result(
    outcome: Option<mq_bridge::route::RouteOutcome>,
    error: Option<String>,
    moved: &Throughput,
) -> anyhow::Result<()> {
    use mq_bridge::route::RouteOutcome;

    clear_progress();
    if matches!(outcome, Some(RouteOutcome::Failed)) {
        let cause = error.unwrap_or_else(|| "no error reported".to_string());
        anyhow::bail!("copy failed after {}: {cause}", moved.rows_display());
    }
    // Reported for a run that ended any other way, so the count above it is the
    // rows the copy *read*, not the rows that arrived — the cause says how many
    // did not. A route that merely recovered from a transient fault can leave a
    // cause behind too; erring towards a non-zero exit is the safe direction when
    // the alternative is calling a lossy copy a success.
    if let Some(cause) = error {
        anyhow::bail!(
            "copy did not deliver every row it read ({}): {cause}",
            moved.rows_display()
        );
    }

    if moved.rejected > 0 {
        anyhow::bail!(
            "copy could not decrypt {} of the rows it read ({}): see the errors above",
            grouped(moved.rejected),
            moved.rows_display()
        );
    }

    match outcome {
        Some(RouteOutcome::Stopped) => moved.report("stopped after copying"),
        Some(RouteOutcome::Completed) => moved.report("copied"),
        // Ctrl-C before the drain finished: the rows already copied are the
        // answer, reported the way the continuous branch reports a Ctrl-C.
        None => moved.report("stopped after copying"),
        // Already returned above.
        Some(RouteOutcome::Failed) => {}
    }
    Ok(())
}

/// Maps an endpoint URI to an mq-bridge [`Endpoint`]: the scheme
/// selects the endpoint, and `?param=a&next=b` query parameters set its config.
///
/// Query keys that match a *scalar* field of the target endpoint's config struct
/// become endpoint config (e.g. `table`, `insert_query`, `subject`,
/// `delete_after_read`), and an object-typed field like `tls` takes a JSON
/// literal (`?tls={"required":true,...}`); any other query param stays on the
/// connection URL, so driver options such as `sslmode`, `replicaSet` or
/// `tls=true` pass through unchanged. `file` URIs map the path to the `path` field. For `nats`, the
/// dominant target field `subject` may also be given as the URL path
/// (`nats://host:4222/orders`) as an alternative to `?subject=orders` (the query
/// form wins if both are present); redis is excluded because a redis URL path is
/// the database number, not the stream.
///
/// MongoDB is pinned to a non-destructive source here: when neither `consume` nor
/// the deprecated `change_stream` is given, `consume` is set to `capture_all`
/// (read existing documents, then watch), which needs a replica set. Pass
/// `?consume=snapshot` for a one-shot read of a standalone mongod, or
/// `?consume=consumer` for the destructive queue-drain mode.
///
/// Escaped mode: pass the full connection string percent-encoded as `?url=...`
/// to use it verbatim (e.g. `mongodb://_/?url=<encoded>&collection=orders`); its
/// own `?a=b` options are then never re-interpreted as config, which is the
/// escape hatch for any driver option that collides with a config field name.
/// Parses a `--from`/`--to` value into an endpoint, including any middlewares.
///
/// The value is the endpoint URI optionally followed by `|`-separated middleware
/// specs, applied in the order written:
/// `postgres://host/db?table=src|retry?max_attempts=5|metrics`.
/// A literal `|` inside the URI itself (e.g. in a password) must be written
/// percent-encoded as `%7C`.
///
/// Structural endpoints take their nested endpoints as query params that are
/// themselves endpoint URIs — `fanout:?mirror=<uri>&to=<uri>`,
/// `request:?to=<uri>&forward_to=<uri>`, `switch:?metadata_key=k&case.v=<uri>`
/// or `switch:?when=<expr>&to=<uri>`,
/// plus the argument-free `response:` and `null:`. A nested URI only needs
/// percent-encoding when it carries `&`, `#` or `|` of its own.
fn endpoint_from_uri(uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    let mut parts = uri.split('|');
    let base = parts.next().unwrap_or(uri);
    let mut endpoint = base_endpoint_from_uri(base)?;
    for spec in parts {
        endpoint
            .middlewares
            .push(middleware_from_spec(spec).with_context(|| {
                format!(
                    "invalid middleware '{}' in '{shown}'",
                    copy_pipeline::redact_uri(spec)
                )
            })?);
    }
    Ok(endpoint)
}

fn nested_endpoint(
    key: &str,
    value: &str,
    outer: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    endpoint_from_uri(value).with_context(|| {
        format!(
            "invalid '{key}' endpoint '{}' in '{}'",
            copy_pipeline::redact_uri(value),
            copy_pipeline::redact_uri(outer)
        )
    })
}

/// Wraps a branch so it can neither answer the caller nor fail the message for
/// the other branches: `request` discards the response, and forwards the
/// original rather than erroring when the branch is down.
fn discarding_request(branch: mq_bridge::models::Endpoint) -> mq_bridge::models::Endpoint {
    use mq_bridge::models::{Endpoint, EndpointType, RequestForwardConfig};

    Endpoint::new(EndpointType::Request(RequestForwardConfig {
        to: Box::new(branch),
        forward_to: Box::new(Endpoint::new(EndpointType::Null)),
    }))
}

/// An `http`/`websocket` **source** is a server, so its `url` is a listen
/// address — but a URI needs a scheme to select the endpoint at all, and the
/// driver rejects one as part of an address. `https` asks for a TLS listener;
/// `wss` has nowhere to keep a certificate.
fn make_listen_address(endpoint: &mut mq_bridge::models::Endpoint) -> anyhow::Result<()> {
    use mq_bridge::models::EndpointType;

    let (url, tls) = match &mut endpoint.endpoint_type {
        EndpointType::Http(config) => (&mut config.url, Some(&mut config.tls)),
        EndpointType::WebSocket(config) => (&mut config.url, None),
        _ => return Ok(()),
    };
    if url.starts_with("wss://") {
        anyhow::bail!("a 'wss://' source is not supported: websocket listeners have no TLS config");
    }
    for (prefix, secure) in [("https://", true), ("http://", false), ("ws://", false)] {
        if let Some(rest) = url.strip_prefix(prefix) {
            *url = rest.trim_end_matches('/').to_string();
            if secure && let Some(tls) = tls {
                tls.required = true;
            }
            break;
        }
    }
    Ok(())
}

/// Builds a middleware from a `name` / `name?param=value&...` spec.
///
/// Built-ins are read off the `Middleware` enum's own schema, so a new variant
/// needs no change here. Params are the variant's config fields, coerced to
/// their type; an endpoint-typed field (`dlq`'s `endpoint`, `lookup`'s `from`)
/// takes an endpoint URI, and an object/array field a JSON literal. A string
/// variant (`id`, `filter`) takes the whole percent-decoded query as its value.
/// Any other name is a registered or installed custom middleware.
fn middleware_from_spec(spec: &str) -> anyhow::Result<mq_bridge::models::Middleware> {
    let (name, query) = match spec.split_once('?') {
        Some((name, query)) => (name.trim(), query),
        None => (spec.trim(), ""),
    };
    // An underscore is awkward to type in a shell-quoted URI, so `-` is accepted
    // as well (`weak-join` == `weak_join`).
    let tag = name.replace('-', "_");
    let root = serde_json::to_value(schemars::schema_for!(mq_bridge::models::Middleware))?;

    if let Some(variant) = middleware_variant(&root, &tag) {
        let value = if accepts_bare_string(&root, variant)
            && !query_sets_fields(&root, variant, query)
        {
            let raw = percent_encoding::percent_decode_str(query)
                .decode_utf8()
                .with_context(|| {
                    format!(
                        "middleware spec '{}' is not valid UTF-8",
                        copy_pipeline::redact_uri(spec)
                    )
                })?;
            serde_json::Value::String(raw.into_owned())
        } else {
            serde_json::Value::Object(middleware_params(spec, query, Some((&root, variant)))?)
        };
        return serde_json::from_value(serde_json::json!({ tag.as_str(): value })).with_context(
            || {
                format!(
                    "could not build a '{tag}' middleware from '{}'",
                    copy_pipeline::redact_uri(spec)
                )
            },
        );
    }

    let Some((name, declared)) = custom_middleware(name, &tag)? else {
        let mut known = middleware_tags(&root);
        known.extend(mq_bridge::extensions::middleware_config_schemas().into_keys());
        anyhow::bail!(
            "unsupported middleware '{tag}'. Supported middlewares: {}. A name may also be a \
             middleware loaded with --plugin or installed on the plugin search path ({})",
            known.join(", "),
            mq_bridge::plugin::search_path_hint(name),
        );
    };
    let config = match declared {
        Some(schema) => middleware_params(spec, query, Some((&schema, &schema)))?,
        None => middleware_params(spec, query, None)?,
    };
    Ok(mq_bridge::models::Middleware::Custom {
        name,
        config: serde_json::Value::Object(config),
    })
}

/// Whether a variant can be written as a bare string: a string variant (`id`)
/// or a string-or-map one (`filter`).
fn accepts_bare_string(root: &serde_json::Value, variant: &serde_json::Value) -> bool {
    let variant = variant
        .get("$ref")
        .and_then(|r| r.as_str())
        .and_then(|r| resolve_ref(root, r))
        .unwrap_or(variant);
    if matches!(field_type(root, variant), FieldType::StringLike) {
        return true;
    }
    ["anyOf", "oneOf"].iter().any(|key| {
        variant
            .get(key)
            .and_then(|a| a.as_array())
            .is_some_and(|members| {
                members
                    .iter()
                    .any(|m| m.get("type").and_then(|t| t.as_str()) == Some("string"))
            })
    })
}

/// Whether every `&`-separated part of `query` sets a config field of `variant`,
/// which selects the map form of a string-or-map variant.
fn query_sets_fields(root: &serde_json::Value, variant: &serde_json::Value, query: &str) -> bool {
    let mut fields = std::collections::HashMap::new();
    collect_props(
        root,
        variant,
        &mut fields,
        &mut std::collections::HashSet::new(),
    );
    !query.is_empty()
        && query.split('&').all(|part| {
            part.split_once('=')
                .is_some_and(|(key, _)| fields.contains_key(key))
        })
}

/// The schema of the `Middleware` variant tagged `tag`, if the enum has one.
fn middleware_variant<'a>(root: &'a serde_json::Value, tag: &str) -> Option<&'a serde_json::Value> {
    root["oneOf"]
        .as_array()?
        .iter()
        .find_map(|variant| variant.get("properties")?.get(tag))
}

/// Every tag the `Middleware` enum accepts, for the unsupported-middleware error.
fn middleware_tags(root: &serde_json::Value) -> Vec<String> {
    root["oneOf"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|variant| variant.get("properties")?.as_object())
        .flat_map(|properties| properties.keys().cloned())
        .collect()
}

/// A middleware registered under `name` or its `_` spelling, loading an
/// installed plugin when none is. Yields the name it answers to and its schema.
fn custom_middleware(
    name: &str,
    tag: &str,
) -> anyhow::Result<Option<(String, Option<serde_json::Value>)>> {
    let registered = |candidate: &str| {
        mq_bridge::extensions::get_middleware_factory(candidate).map(|factory| {
            let schema = factory.config_schema();
            let flat = schema
                .as_ref()
                .map(mq_bridge::support::config_schema::flatten);
            (candidate.to_string(), flat)
        })
    };
    if let Some(found) = registered(name).or_else(|| registered(tag)) {
        return Ok(Some(found));
    }
    for candidate in [name, tag] {
        if mq_bridge::plugin::discover_middleware_plugin(candidate)
            .with_context(|| format!("middleware '{candidate}'"))?
            .is_some()
        {
            return Ok(registered(candidate));
        }
    }
    Ok(None)
}

/// Reads a middleware spec's query params into its config object, typed by
/// `schema` (`(root, node)`). Without one, nothing says a param is a number,
/// so each stays a string rather than turning an id like `0123` into one.
fn middleware_params(
    spec: &str,
    query: &str,
    schema: Option<(&serde_json::Value, &serde_json::Value)>,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let mut fields = std::collections::HashMap::new();
    let mut endpoints = std::collections::HashSet::new();
    if let Some((root, node)) = schema {
        collect_props(
            root,
            node,
            &mut fields,
            &mut std::collections::HashSet::new(),
        );
        endpoints = endpoint_fields(root, node);
    }

    let mut config = serde_json::Map::new();
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        let (k, v) = (k.into_owned(), v.into_owned());
        let value = if endpoints.contains(&k) {
            let endpoint = endpoint_from_uri(&v).with_context(|| {
                format!(
                    "invalid '{k}' endpoint '{}' in '{}'",
                    copy_pipeline::redact_uri(&v),
                    copy_pipeline::redact_uri(spec)
                )
            })?;
            serde_json::to_value(endpoint)?
        } else {
            match fields.get(&k).copied() {
                // A known object/array field must be a JSON literal; a value that
                // doesn't parse is a user error worth naming, the same way
                // `base_endpoint_from_uri` handles its object fields, rather than
                // a silent fallback to a string that serde rejects later.
                Some(FieldType::Object) => serde_json::from_str(&v).with_context(|| {
                    format!(
                        "query param '{k}' in middleware spec '{}' expects a JSON literal, got '{}'",
                        copy_pipeline::redact_uri(spec),
                        copy_pipeline::redact_param(&k, &v, None)
                    )
                })?,
                None if schema.is_none() => serde_json::Value::String(v),
                // An unknown field (`None`) is passed through for serde to reject
                // by name.
                None => serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v)),
                Some(ty) => coerce_scalar(v, ty),
            }
        };
        config.insert(k, value);
    }
    Ok(config)
}

/// The fields of `node` that hold an `Endpoint` (possibly optional or boxed),
/// which a spec writes as an endpoint URI.
fn endpoint_fields(
    root: &serde_json::Value,
    node: &serde_json::Value,
) -> std::collections::HashSet<String> {
    let is_endpoint = |schema: &serde_json::Value| {
        let names = |s: &serde_json::Value| {
            s.get("$ref").and_then(serde_json::Value::as_str) == Some("#/$defs/Endpoint")
        };
        names(schema)
            || ["anyOf", "oneOf"].iter().any(|key| {
                schema
                    .get(key)
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|members| members.iter().any(names))
            })
    };
    let node = node
        .get("$ref")
        .and_then(serde_json::Value::as_str)
        .and_then(|reference| resolve_ref(root, reference))
        .unwrap_or(node);
    node.get("properties")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, schema)| is_endpoint(schema))
        .map(|(name, _)| name.clone())
        .collect()
}

/// The extension endpoints this build compiled in, for the unsupported-scheme
/// message, so it never names one this binary does not have.
fn extension_schemes() -> String {
    let mut names = Vec::new();
    if cfg!(feature = "pulsar") {
        names.push("pulsar");
    }
    if cfg!(feature = "meilisearch") {
        names.push("meilisearch");
    }
    #[cfg(feature = "http-bulk")]
    for name in mq_bridge::endpoints::http_bulk::preset_names() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return String::new();
    }
    format!(" ({})", names.join(", "))
}

/// Builds a `custom` endpoint for a scheme that names a registered factory.
///
/// The factory owns its config shape, so the mapping comes from the JSON Schema
/// it declares: which field takes the address, which takes the path, and what
/// type each query param is read as. A factory that declares nothing keeps the
/// mapping that predates schemas — `url` is the URI up to the query and every
/// query param is a string, since guessing a type there would silently turn an
/// id like `0123` into a number.
fn custom_endpoint_from_uri(name: &str, uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use mq_bridge::models::{Endpoint, EndpointType};

    let config = mq_bridge::plugin::endpoint_uri_schema(name)
        .config_from_uri(uri)
        .with_context(|| format!("endpoint '{name}' in URI '{shown}'"))?;

    Ok(Endpoint::new(EndpointType::Custom {
        name: name.to_string(),
        config: serde_json::Value::Object(config),
    }))
}

/// The plugin a `plugin+component` scheme names. Built-in schemes are matched
/// before this is consulted, so none of them can be shadowed by it.
fn plugin_of(scheme: &str) -> &str {
    scheme.split_once('+').map_or(scheme, |(plugin, _)| plugin)
}

fn http_bulk_endpoint(parsed: &url::Url, uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use serde_json::Value;

    let mut text = None;
    let mut url = None;
    for (key, value) in parsed.query_pairs() {
        let slot = match key.as_ref() {
            "config" | "config_file" => &mut text,
            "url" => &mut url,
            other => anyhow::bail!(
                "unsupported query param '{other}' in http-bulk URI '{shown}'. Supported: config_file, config, url"
            ),
        };
        if slot.is_some() {
            anyhow::bail!("http-bulk URI '{shown}' gives '{key}' or its alternative twice");
        }
        *slot = Some(match key.as_ref() {
            "config_file" => std::fs::read_to_string(value.as_ref())
                .with_context(|| format!("failed to read http-bulk config_file '{value}'"))?,
            _ => value.into_owned(),
        });
    }
    let Some(text) = text else {
        anyhow::bail!(
            "http-bulk URI '{shown}' needs 'config_file=<path>' or 'config=<YAML or JSON>' holding the http_bulk fields"
        );
    };
    let mut config: Value = serde_yaml_ng::from_str(&text)
        .with_context(|| format!("http-bulk config in URI '{shown}' is not valid YAML or JSON"))?;
    // A recipe copied with its `http_bulk:` key is taken as well.
    if let Some(inner) = config.as_object_mut().filter(|map| map.len() == 1)
        && let Some(inner) = inner
            .remove("http_bulk")
            .or_else(|| inner.remove("http-bulk"))
    {
        config = inner;
    }
    if let (Some(url), Some(fields)) = (url, config.as_object_mut()) {
        fields.insert("url".into(), Value::String(url));
    }
    let endpoint_type = serde_json::from_value(serde_json::json!({ "http_bulk": config }))
        .with_context(|| format!("could not build an 'http_bulk' endpoint from URI '{shown}'"))?;
    Ok(mq_bridge::models::Endpoint::new(endpoint_type))
}

fn base_endpoint_from_uri(uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    let shown = copy_pipeline::redact_uri(uri);
    use anyhow::bail;
    use mq_bridge::models::{
        AmqpConfig, AwsConfig, ClickHouseConfig, DirSpoolConfig, Endpoint, EndpointType,
        FileConfig, GrpcConfig, HttpConfig, IbmMqConfig, KafkaConfig, MongoDbConfig, MqttConfig,
        NatsConfig, ObjectStoreConfig, PostgresCdcConfig, RedisStreamsConfig, SqlxConfig,
        WebSocketConfig, ZeroMqConfig,
    };
    use std::collections::HashMap;
    use url::Url;

    let parsed = Url::parse(uri).with_context(|| format!("not a valid URI: {shown}"))?;

    // Endpoints without a connection URL are built directly — they don't fit the
    // scalar-field-routing path below (which always attaches a `url`).
    match parsed.scheme() {
        // A sink that discards everything. `null:` (any trailing content ignored).
        "null" => return Ok(Endpoint::new(EndpointType::Null)),
        // `http-bulk:?config_file=<path>` or `http-bulk:?config=<YAML or JSON>`.
        // The value is what a config file has under `http_bulk:`, which is too
        // nested for query params. `url=` replaces the target of the config.
        "http-bulk" => return http_bulk_endpoint(&parsed, uri),
        // A source that endlessly produces a fixed message (config-only load
        // generator) or a sink. Body from `?body=`, or read a file with
        // `?body_file=`. `raw=true` sends the body verbatim (no JSON re-encode) —
        // use it so a generated JSON row is the payload as-is. Any other query
        // param becomes message metadata.
        "static" => {
            let mut body: Option<String> = None;
            let mut raw = false;
            let mut metadata: HashMap<String, String> = HashMap::new();
            for (k, v) in parsed.query_pairs() {
                match k.as_ref() {
                    "body" => body = Some(v.into_owned()),
                    "body_file" => {
                        body =
                            Some(std::fs::read_to_string(v.as_ref()).with_context(|| {
                                format!("failed to read static body_file '{}'", v)
                            })?);
                    }
                    "raw" => raw = v == "true",
                    _ => {
                        metadata.insert(k.into_owned(), v.into_owned());
                    }
                }
            }
            let mut cfg = serde_json::Map::new();
            cfg.insert(
                "body".into(),
                serde_json::Value::String(body.unwrap_or_default()),
            );
            cfg.insert("raw".into(), serde_json::Value::Bool(raw));
            cfg.insert("metadata".into(), serde_json::to_value(metadata)?);
            let mut tagged = serde_json::Map::new();
            tagged.insert("static".into(), serde_json::Value::Object(cfg));
            let endpoint_type: EndpointType =
                serde_json::from_value(serde_json::Value::Object(tagged)).with_context(|| {
                    format!("could not build a 'static' endpoint from URI '{shown}'")
                })?;
            return Ok(Endpoint::new(endpoint_type));
        }
        // `response:` — replies to the caller; needs an input with a reply channel.
        "response" => {
            if let Some((key, _)) = parsed.query_pairs().next() {
                anyhow::bail!(
                    "unsupported query param '{key}' in response URI '{shown}'. Response takes no query parameters"
                );
            }
            return Ok(Endpoint::new(EndpointType::Response(Default::default())));
        }
        // `fanout:?mirror=<uri>&to=<uri>`, in the order written. Only a `to`
        // branch can answer the caller.
        "fanout" => {
            let mut branches = Vec::new();
            for (key, value) in parsed.query_pairs() {
                let mirrored = match key.as_ref() {
                    "to" => false,
                    "mirror" => true,
                    other => anyhow::bail!(
                        "unsupported query param '{other}' in fanout URI '{shown}'. Use 'to=<uri>' for a branch that may answer, 'mirror=<uri>' for one whose response and failures are discarded"
                    ),
                };
                let branch = nested_endpoint(&key, &value, uri)?;
                branches.push(if mirrored {
                    discarding_request(branch)
                } else {
                    branch
                });
            }
            if branches.is_empty() {
                anyhow::bail!(
                    "fanout URI '{shown}' has no branches. Add at least one 'to=<uri>' or 'mirror=<uri>'"
                );
            }
            return Ok(Endpoint::new(EndpointType::Fanout(branches)));
        }
        // `request:?to=<uri>&forward_to=<uri>`; without a `forward_to` the
        // response is discarded.
        "request" => {
            let mut to = None;
            let mut forward_to = None;
            for (key, value) in parsed.query_pairs() {
                let slot = match key.as_ref() {
                    "to" => &mut to,
                    "forward_to" => &mut forward_to,
                    other => anyhow::bail!(
                        "unsupported query param '{other}' in request URI '{shown}'. Supported: to, forward_to"
                    ),
                };
                if slot.is_some() {
                    anyhow::bail!("duplicate query param '{key}' in request URI '{shown}'");
                }
                *slot = Some(Box::new(nested_endpoint(&key, &value, uri)?));
            }
            let Some(to) = to else {
                anyhow::bail!("request URI '{shown}' needs a 'to=<uri>' endpoint to send to");
            };
            return Ok(Endpoint::new(EndpointType::Request(
                mq_bridge::models::RequestForwardConfig {
                    to,
                    forward_to: forward_to
                        .unwrap_or_else(|| Box::new(Endpoint::new(EndpointType::Null))),
                },
            )));
        }
        // Value lookup: `switch:?metadata_key=<key>&case.<value>=<uri>&default=<uri>`.
        // Predicates:   `switch:?when=<expression>&to=<uri>&…&default=<uri>`, first
        // match wins, so `when`/`to` pairs keep the order they were written in. An
        // expression goes in the *value*, where an `=` needs no escaping — but a
        // literal `&` still splits the query, so write `and`/`or` rather than
        // `&&`/`||` (the engine accepts both spellings anyway).
        "switch" => {
            let mut metadata_key = None;
            let mut cases = std::collections::BTreeMap::new();
            let mut when: Vec<mq_bridge::models::SwitchCase> = Vec::new();
            let mut condition: Option<String> = None;
            let mut default = None;
            for (key, value) in parsed.query_pairs() {
                match key.as_ref() {
                    "metadata_key" => metadata_key = Some(value.into_owned()),
                    "when" => {
                        if let Some(previous) = condition.replace(value.into_owned()) {
                            anyhow::bail!(
                                "switch URI '{shown}' has 'when={previous}' with no 'to=<uri>' after it"
                            );
                        }
                    }
                    "to" => {
                        let Some(condition) = condition.take() else {
                            anyhow::bail!(
                                "switch URI '{shown}' has a 'to={}' that no 'when=<expression>' precedes",
                                copy_pipeline::redact_uri(&value)
                            );
                        };
                        when.push(mq_bridge::models::SwitchCase {
                            condition,
                            to: nested_endpoint(&key, &value, uri)?,
                        });
                    }
                    "default" => default = Some(nested_endpoint(&key, &value, uri)?),
                    case if case.starts_with("case.") => {
                        cases.insert(
                            case["case.".len()..].to_string(),
                            nested_endpoint(&key, &value, uri)?,
                        );
                    }
                    other => anyhow::bail!(
                        "unsupported query param '{other}' in switch URI '{shown}'. Supported: metadata_key, case.<value>=<uri>, when=<expression> with to=<uri>, default=<uri>"
                    ),
                }
            }
            if let Some(dangling) = condition {
                anyhow::bail!(
                    "switch URI '{shown}' has 'when={dangling}' with no 'to=<uri>' after it"
                );
            }
            // The two modes differ in cost, not just spelling, so mixing them
            // would hide which one a message actually took.
            if !when.is_empty() && (metadata_key.is_some() || !cases.is_empty()) {
                anyhow::bail!(
                    "switch URI '{shown}' mixes both modes. Use either 'metadata_key=<key>' with 'case.<value>=<uri>', or 'when=<expression>' with 'to=<uri>'"
                );
            }
            if when.is_empty() {
                if metadata_key.is_none() {
                    anyhow::bail!(
                        "switch URI '{shown}' needs a 'metadata_key=<key>' to branch on, or 'when=<expression>&to=<uri>' predicates"
                    );
                }
                if cases.is_empty() {
                    anyhow::bail!("switch URI '{shown}' has no cases. Add 'case.<value>=<uri>'");
                }
            }
            return Ok(Endpoint::new(EndpointType::Switch(
                mq_bridge::models::SwitchConfig {
                    metadata_key: metadata_key.unwrap_or_default(),
                    cases: cases.into_iter().collect(),
                    when,
                    default: default.map(Box::new),
                },
            )));
        }
        // In-process channel. Topic is the host (+path): `memory://my-topic`.
        // `?capacity=`, `?subscribe_mode=` are recognised; other params ignored.
        "memory" => {
            let host = parsed.host_str().unwrap_or("");
            let path = parsed.path().trim_matches('/');
            let topic = if path.is_empty() {
                host.to_string()
            } else if host.is_empty() {
                path.to_string()
            } else {
                format!("{host}/{path}")
            };
            if topic.is_empty() {
                anyhow::bail!("memory URI '{shown}' must include a topic, e.g. memory://my-topic");
            }
            let mut cfg = serde_json::Map::new();
            cfg.insert("topic".into(), serde_json::Value::String(topic));
            for (k, v) in parsed.query_pairs() {
                match k.as_ref() {
                    "capacity" => {
                        if let Ok(n) = v.parse::<u64>() {
                            cfg.insert("capacity".into(), serde_json::Value::from(n));
                        }
                    }
                    "subscribe_mode" => {
                        cfg.insert(
                            "subscribe_mode".into(),
                            serde_json::Value::Bool(v == "true"),
                        );
                    }
                    // An in-process channel has no connection URL to carry driver
                    // options, so anything else would just be discarded.
                    other => anyhow::bail!(
                        "unrecognised query param '{other}' in memory URI '{shown}': only 'capacity' and 'subscribe_mode' are supported"
                    ),
                }
            }
            let mut tagged = serde_json::Map::new();
            tagged.insert("memory".into(), serde_json::Value::Object(cfg));
            let endpoint_type: EndpointType =
                serde_json::from_value(serde_json::Value::Object(tagged)).with_context(|| {
                    format!("could not build a 'memory' endpoint from URI '{shown}'")
                })?;
            return Ok(Endpoint::new(endpoint_type));
        }
        _ => {}
    }

    // scheme -> (EndpointType tag, recognised config fields with their types).
    let (tag, fields): (&str, HashMap<String, FieldType>) = match parsed.scheme() {
        "postgres" | "postgresql" | "mysql" | "mariadb" | "sqlite" => {
            ("sqlx", schema_fields(schemars::schema_for!(SqlxConfig)))
        }
        // Logical-replication CDC source. The connection URL is rebuilt with a
        // plain `postgres` scheme; `publication`, `slot_name`, etc. are scalar
        // config fields set via query params. (An underscore is not a legal URI
        // scheme character, so the scheme is spelled `postgres-cdc`/`pgcdc`.)
        "postgres-cdc" | "pgcdc" => (
            "postgres_cdc",
            schema_fields(schemars::schema_for!(PostgresCdcConfig)),
        ),
        "nats" => ("nats", schema_fields(schemars::schema_for!(NatsConfig))),
        "mongodb" => (
            "mongodb",
            schema_fields(schemars::schema_for!(MongoDbConfig)),
        ),
        "redis" | "rediss" | "redis_streams" => (
            "redis",
            schema_fields(schemars::schema_for!(RedisStreamsConfig)),
        ),
        "file" => ("file", schema_fields(schemars::schema_for!(FileConfig))),
        // Directory-backed FIFO queue, targeted by path like `file`. An underscore
        // is not a legal URI scheme character, hence `spool`/`dir-spool`.
        "spool" | "dir-spool" | "dirspool" => (
            "dir_spool",
            schema_fields(schemars::schema_for!(DirSpoolConfig)),
        ),
        // Local and cloud object storage. Cloud schemes pass through; the explicit
        // local alias is rewritten to `file://` below so plain `file://` keeps
        // selecting the single-file connector.
        "local-store" | "s3" | "s3a" | "gs" | "gcs" | "az" | "azure" | "abfs" | "abfss" => (
            "object_store",
            schema_fields(schemars::schema_for!(ObjectStoreConfig)),
        ),
        "kafka" => ("kafka", schema_fields(schemars::schema_for!(KafkaConfig))),
        "mqtt" | "mqtts" => ("mqtt", schema_fields(schemars::schema_for!(MqttConfig))),
        // AMQP is RabbitMQ's wire protocol; both scheme spellings are accepted.
        "amqp" | "amqps" | "rabbitmq" | "rabbitmqs" => {
            ("amqp", schema_fields(schemars::schema_for!(AmqpConfig)))
        }
        "http" | "https" => ("http", schema_fields(schemars::schema_for!(HttpConfig))),
        // ClickHouse is accessed over its HTTP interface; the `clickhouse(s)`
        // scheme here just picks the endpoint kind and is rewritten to
        // `http(s)://` below.
        "clickhouse" | "clickhouses" => (
            "clickhouse",
            schema_fields(schemars::schema_for!(ClickHouseConfig)),
        ),
        "ws" | "wss" => (
            "websocket",
            schema_fields(schemars::schema_for!(WebSocketConfig)),
        ),
        // `grpc(s)` only selects the endpoint kind and is rewritten to
        // `http(s)://` below, matching the client-mode URL GrpcConfig expects.
        "grpc" | "grpcs" => ("grpc", schema_fields(schemars::schema_for!(GrpcConfig))),
        "ibmmq" | "ibm-mq" => ("ibmmq", schema_fields(schemars::schema_for!(IbmMqConfig))),
        // AWS SQS/SNS has no single connection URL: `queue_url`/`topic_arn` are
        // scalar config fields set via query params, so the URI's own
        // authority is just a placeholder (e.g. `aws://_/?queue_url=...`).
        "aws" | "aws-sqs" => ("aws", schema_fields(schemars::schema_for!(AwsConfig))),
        "zeromq" | "zmq" => ("zeromq", schema_fields(schemars::schema_for!(ZeroMqConfig))),
        // A scheme naming a registered endpoint — one compiled in as an extension
        // or loaded with `--plugin` — is that endpoint. This mirrors the config
        // path, where an unknown single key falls back to `custom`. In
        // `plugin+component://` the plugin is the part before the `+`; the full
        // URI still goes to the factory, whose schema reads the component.
        other if mq_bridge::extensions::get_endpoint_factory(plugin_of(other)).is_some() => {
            return custom_endpoint_from_uri(plugin_of(other), uri);
        }
        other => {
            // A URI names the endpoint before any route is built, so an
            // installed plugin is searched for here as well as in the engine.
            let plugin = plugin_of(other);
            if mq_bridge::plugin::discover_endpoint_plugin(plugin)
                .with_context(|| format!("endpoint scheme '{other}' in URI '{shown}'"))?
                .is_some()
            {
                return custom_endpoint_from_uri(plugin, uri);
            }
            bail!(
                "unsupported endpoint scheme '{other}' in URI '{shown}'. Supported schemes: postgres, postgresql, mysql, mariadb, sqlite, nats, mongodb, redis, file, spool, kafka, mqtt, mqtts, amqp, amqps, rabbitmq, rabbitmqs, http, https, clickhouse, clickhouses, http-bulk, ws, wss, grpc, grpcs, ibmmq, aws, zeromq, zmq, s3, gs, az, abfs, and the structural memory, null, static, fanout, request, switch, response. A scheme may also name an endpoint registered by an extension{}, loaded with --plugin, or installed on the plugin search path ({})",
                extension_schemes(),
                mq_bridge::plugin::search_path_hint(plugin),
            )
        }
    };

    // Split query params: recognised scalar config fields become endpoint config,
    // everything else is kept on the connection URL (driver params) — but only
    // where such a param can actually take effect, see `driver_options`.
    let mut config = serde_json::Map::new();
    let mut driver_params: Vec<(String, String)> = Vec::new();
    // Escaped mode: `?url=<percent-encoded connection string>` supplies the exact
    // connection URL verbatim, so its own `?a=b` options are never re-interpreted
    // as config fields. Use it when a driver option would otherwise collide.
    let mut escaped_url: Option<String> = None;
    // Whether an unrecognised param can ride along on the connection URL as a
    // driver option. True only for endpoints whose URL really is a query-bearing
    // connection string. The rest either have no URL at all (`file` has a path,
    // `aws` has ARNs) or a connection string that is not a URI — kafka's bare
    // `host:port` list, ibmmq's `host(port)`, the mqtt/nats/zeromq/grpc endpoint
    // addresses — where appending `?k=v` corrupts it rather than configuring
    // anything. There, an unrecognised param is a user error, not an option.
    let driver_options = matches!(
        tag,
        "sqlx"
            | "postgres_cdc"
            | "mongodb"
            | "redis"
            | "amqp"
            | "http"
            | "clickhouse"
            | "websocket"
    );
    // Target is a filesystem path carried by the URI itself, so `?path=`/`?url=`
    // would be a second source for the same field.
    let path_target = matches!(tag, "file" | "dir_spool");
    for (k, v) in parsed.query_pairs() {
        let (k, v) = (k.into_owned(), v.into_owned());
        if k == "path" && path_target {
            continue;
        }
        if k == "url" && !path_target {
            escaped_url = Some(v);
            continue;
        }
        match fields.get(&k).copied() {
            // An object/array field (e.g. `encryption`, `tls`, kafka's
            // `producer_options`) can't be populated from a scalar, so its value is
            // read as a JSON literal, as the middleware spec syntax already does.
            // Where driver options exist the same name is also a plausible option
            // (`?tls=true`), so only an actual `{`/`[` literal is taken as config.
            Some(FieldType::Object) if !driver_options || is_json_literal(&v) => {
                let value: serde_json::Value = serde_json::from_str(&v).with_context(|| {
                    format!(
                        "query param '{k}' in URI '{shown}' expects a JSON literal, got '{}'",
                        copy_pipeline::redact_param(&k, &v, None)
                    )
                })?;
                config.insert(k, value);
            }
            Some(FieldType::Object) | None if driver_options => driver_params.push((k, v)),
            None => bail!(
                "unrecognised query param '{k}' in URI '{shown}': a '{tag}' endpoint has no connection-URL driver options, so '{k}' would have no effect"
            ),
            Some(ty) => {
                config.insert(k, coerce_scalar(v, ty));
            }
        }
    }

    // Non-destructive default for MongoDB sources: `capture_all` (read existing
    // docs, then watch) so pointing at an existing collection never mutates it.
    // This matches the library default since 0.4.0 and is pinned here so a future
    // library change cannot make a CLI/UI source destructive. Only applied when the
    // user gave neither `consume` nor the deprecated `change_stream`, so any
    // explicit choice still wins.
    if tag == "mongodb" && !config.contains_key("consume") && !config.contains_key("change_stream")
    {
        config.insert(
            "consume".into(),
            serde_json::Value::String("capture_all".into()),
        );
    }

    if path_target {
        let path = uri.split('?').next().unwrap_or(uri);
        // `scheme://x` and the authority-less `scheme:/x` both name a path, so the
        // slashes come off separately from the scheme.
        let prefix = format!("{}:", parsed.scheme());
        let path = path.strip_prefix(prefix.as_str()).unwrap_or(path);
        let path = path.strip_prefix("//").unwrap_or(path);
        if path.is_empty() {
            bail!("{tag} URI '{shown}' must include a path");
        }
        // `x.jsonl.gz` says how it is compressed; `compression=none` overrides.
        if tag == "file" && !config.contains_key("compression") {
            let codec = [(".gz", "gzip"), (".zst", "zstd"), (".lz4", "lz4")]
                .into_iter()
                .find(|(extension, _)| path.ends_with(extension));
            if let Some((_, codec)) = codec {
                config.insert("compression".into(), codec.into());
            }
        }
        config.insert("path".into(), serde_json::Value::String(path.to_string()));
    } else if let Some(url) = escaped_url {
        // Escaped mode: the connection string is authoritative and complete, so
        // any leftover non-config param is ambiguous — it belongs inside `url=`.
        if let Some((k, _)) = driver_params.first() {
            bail!(
                "in escaped mode (url=...), put driver options inside the encoded connection string; unexpected query param '{k}' in URI '{shown}'"
            );
        }
        config.insert("url".into(), serde_json::Value::String(url));
    } else {
        // For endpoints with a single dominant "target" field, also accept it as
        // the URL path (e.g. `nats://host:4222/orders`), matching the UI's short
        // display convention, alongside the query-param form (`?subject=orders`).
        // Only for `nats`: a redis URL path is the database number, not the stream.
        let path_field = match tag {
            "nats" => Some("subject"),
            _ => None,
        };
        let mut base = parsed.clone();
        if let Some(field) = path_field {
            let path = base.path().trim_matches('/');
            if !path.is_empty() && !config.contains_key(field) {
                config.insert(field.into(), serde_json::Value::String(path.to_string()));
            }
            base.set_path("");
        }
        // Connection URL = base URI plus any leftover (driver) params.
        base.set_fragment(None);
        base.set_query(None);
        if !driver_params.is_empty() {
            let mut qs = base.query_pairs_mut();
            for (k, v) in &driver_params {
                qs.append_pair(k, v);
            }
        }
        // The postgres_cdc endpoint takes a plain `postgres://` connection URL;
        // the `postgres_cdc`/`pgcdc` scheme only selects the endpoint kind.
        let mut url = base.to_string();
        if tag == "postgres_cdc" {
            for prefix in ["postgres-cdc://", "pgcdc://"] {
                if let Some(rest) = url.strip_prefix(prefix) {
                    url = format!("postgres://{rest}");
                    break;
                }
            }
        }
        // A few schemes exist only to pick the endpoint kind from the CLI and
        // are rewritten to the connection scheme the underlying driver expects.
        let rewrites: &[(&str, &str)] = match tag {
            // rdkafka's bootstrap.servers is a bare host:port list, no scheme.
            "kafka" => &[("kafka://", "")],
            "mqtt" => &[("mqtts://", "ssl://"), ("mqtt://", "tcp://")],
            "amqp" => &[("rabbitmqs://", "amqps://"), ("rabbitmq://", "amqp://")],
            "clickhouse" => &[("clickhouses://", "https://"), ("clickhouse://", "http://")],
            "grpc" => &[("grpcs://", "https://"), ("grpc://", "http://")],
            "zeromq" => &[("zeromq://", "tcp://"), ("zmq://", "tcp://")],
            // Normalize CLI-only aliases to schemes recognized by `object_store`.
            "object_store" => &[("gcs://", "gs://"), ("local-store://", "file://")],
            _ => &[],
        };
        for (prefix, replacement) in rewrites {
            if let Some(rest) = url.strip_prefix(prefix) {
                url = format!("{replacement}{rest}");
                break;
            }
        }
        // `zeromq://tcp://host:port` names the transport itself; it is taken as written.
        if tag == "zeromq" {
            let address = uri.split(['?', '#']).next().unwrap_or(uri);
            let transport = ["zeromq://", "zmq://"]
                .iter()
                .find_map(|prefix| address.strip_prefix(prefix));
            if let Some(transport) = transport.filter(|t| t.contains("://")) {
                url = transport.to_string();
            }
        }
        if tag == "kafka" {
            url = url.trim_end_matches('/').to_string();
        }
        // IBM MQ's driver expects `host(port)` (with comma-separated hosts for
        // failover), not a URI authority, so `host:port` is reformatted here.
        if tag == "ibmmq"
            && let Some(rest) = url.strip_prefix("ibmmq://")
        {
            let rest = rest.trim_end_matches('/');
            url = match rest.rsplit_once(':') {
                Some((host, port)) => format!("{host}({port})"),
                None => rest.to_string(),
            };
        }
        // AwsConfig has no `url` field (`queue_url`/`topic_arn` carry the
        // connection info as scalar config fields), so the placeholder
        // authority is discarded rather than attached as an unknown field.
        if tag != "aws" {
            config.insert("url".into(), serde_json::Value::String(url));
        }
    }

    let mut tagged = serde_json::Map::new();
    tagged.insert(tag.to_string(), serde_json::Value::Object(config));
    let endpoint_type: EndpointType = serde_json::from_value(serde_json::Value::Object(tagged))
        .with_context(|| format!("could not build a '{tag}' endpoint from URI '{shown}'"))?;
    Ok(Endpoint::new(endpoint_type))
}

/// The JSON scalar type a config field expects, used to coerce string query
/// params into the right type without guessing from the value's shape.
#[derive(Clone, Copy)]
enum FieldType {
    Bool,
    Integer,
    Number,
    /// An object or array field (e.g. a nested config struct): it cannot be set
    /// from a scalar query param, so such params are routed to driver options.
    Object,
    /// Strings, enums, and anything else scalar — kept as a JSON string.
    StringLike,
}

/// Maps a config struct's serde field names to their expected scalar type, so
/// query params can be routed (recognised field vs driver param) and coerced
/// correctly. Walks the JSON schema following `$ref`, `allOf`, `anyOf` and
/// `oneOf`, so fields introduced via `#[serde(flatten)]` (e.g. an internally
/// tagged enum) are recognised too, not just top-level `properties`.
fn schema_fields(schema: schemars::Schema) -> std::collections::HashMap<String, FieldType> {
    let mut out = std::collections::HashMap::new();
    if let Ok(root) = serde_json::to_value(&schema) {
        let mut visited = std::collections::HashSet::new();
        collect_props(&root, &root, &mut out, &mut visited);
    }
    out
}

/// Recursively collects `(field name, type)` pairs from `node` into `out`,
/// resolving local `$ref`s against `root` and descending schema combinators.
fn collect_props(
    root: &serde_json::Value,
    node: &serde_json::Value,
    out: &mut std::collections::HashMap<String, FieldType>,
    visited: &mut std::collections::HashSet<String>,
) {
    let Some(obj) = node.as_object() else { return };

    if let Some(reference) = obj.get("$ref").and_then(|r| r.as_str()) {
        if visited.insert(reference.to_string())
            && let Some(target) = resolve_ref(root, reference)
        {
            collect_props(root, target, out, visited);
        }
        return;
    }

    if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
        for (name, sub) in props {
            out.entry(name.clone())
                .or_insert_with(|| field_type(root, sub));
        }
    }

    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(arr) = obj.get(key).and_then(|a| a.as_array()) {
            for sub in arr {
                collect_props(root, sub, out, visited);
            }
        }
    }
}

/// Resolves a local JSON-schema `$ref` (`#/$defs/Name` or `#/definitions/Name`)
/// to its target subschema within `root`.
fn resolve_ref<'a>(root: &'a serde_json::Value, reference: &str) -> Option<&'a serde_json::Value> {
    let name = reference.rsplit('/').next()?;
    ["$defs", "definitions"]
        .iter()
        .find_map(|defs| root.get(defs).and_then(|d| d.get(name)))
}

/// Determines the scalar [`FieldType`] of a property subschema. Handles a direct
/// `type`, an `Option<T>` (`{"type":["integer","null"]}` or an `anyOf`/`oneOf`
/// with a null member), and a `$ref` to a scalar def; anything else is treated
/// as string-like (enums deserialize from a string, so no coercion is needed).
fn field_type(root: &serde_json::Value, sub: &serde_json::Value) -> FieldType {
    if let Some(reference) = sub.get("$ref").and_then(|r| r.as_str())
        && let Some(target) = resolve_ref(root, reference)
    {
        return field_type(root, target);
    }
    // A `serde_json::Value` field (e.g. `transform`'s `schema`) constrains
    // nothing, so schemars renders it as the always-true schema. It takes whole
    // JSON rather than a scalar, which is exactly the `Object` handling.
    if is_unconstrained_schema(sub) {
        return FieldType::Object;
    }
    let has = |t: &str| match sub.get("type") {
        Some(serde_json::Value::String(s)) => s == t,
        Some(serde_json::Value::Array(a)) => a.iter().any(|x| x.as_str() == Some(t)),
        _ => false,
    };
    if has("boolean") {
        return FieldType::Bool;
    }
    if has("integer") {
        return FieldType::Integer;
    }
    if has("number") {
        return FieldType::Number;
    }
    if has("object") || has("array") {
        return FieldType::Object;
    }
    // Option<scalar> is often modelled as anyOf/oneOf of the scalar and null.
    for key in ["anyOf", "oneOf"] {
        if let Some(arr) = sub.get(key).and_then(|a| a.as_array()) {
            for member in arr {
                match field_type(root, member) {
                    FieldType::StringLike => {}
                    ty => return ty,
                }
            }
        }
    }
    FieldType::StringLike
}

/// Whether a subschema accepts any JSON at all, i.e. states no `type`, `$ref`,
/// combinator or enumeration. Only annotations such as `description`/`default`
/// may remain.
fn is_unconstrained_schema(sub: &serde_json::Value) -> bool {
    match sub {
        serde_json::Value::Bool(accepts_anything) => *accepts_anything,
        serde_json::Value::Object(obj) => !obj.keys().any(|key| {
            matches!(
                key.as_str(),
                "type"
                    | "$ref"
                    | "allOf"
                    | "anyOf"
                    | "oneOf"
                    | "enum"
                    | "const"
                    | "properties"
                    | "items"
            )
        }),
        _ => false,
    }
}

/// Whether a query-param value is written as a JSON object or array literal.
/// Used to tell a nested-config value (`?tls={"ca_file":"/x"}`) from a driver
/// option that happens to share the field's name (`?tls=true`) on endpoints
/// whose connection URL carries both.
fn is_json_literal(v: &str) -> bool {
    let v = v.trim_start();
    v.starts_with('{') || v.starts_with('[')
}

/// Coerces a query-param string into the JSON scalar its target field expects.
/// Only bool/number *fields* trigger bool/number coercion, so a string field
/// keeps values like `2024` or `true` verbatim; a value that fails to parse for
/// a numeric field falls back to a string so serde reports a clear type error.
fn coerce_scalar(s: String, ty: FieldType) -> serde_json::Value {
    match ty {
        FieldType::Bool => match s.as_str() {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            _ => serde_json::Value::String(s),
        },
        FieldType::Integer => match s.parse::<i64>() {
            Ok(i) => serde_json::Value::from(i),
            Err(_) => serde_json::Value::String(s),
        },
        FieldType::Number => match s.parse::<f64>() {
            Ok(f) => serde_json::Value::from(f),
            Err(_) => serde_json::Value::String(s),
        },
        // Object/array fields are routed to driver params before reaching here;
        // keep the raw string as a defensive fallback.
        FieldType::StringLike | FieldType::Object => serde_json::Value::String(s),
    }
}

/// Minimal logging setup for the headless `copy` subcommand (no AppConfig).
///
/// A one-shot `copy` answers with its summary line, not with a log, so the
/// default is `warn` — only warnings and errors get through, because anything
/// below that is chatter about a command the user is watching run. `--verbose`
/// restores the detail.
fn init_copy_logging(color: ColorChoice, verbose: bool) {
    use std::io::IsTerminal;
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    let default = if verbose { "info" } else { "warn" };
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    // stderr, so stdout carries only the summary line; `MQB_LOG_STDOUT` restores the old stream.
    let to_stdout = std::env::var_os("MQB_LOG_STDOUT").is_some_and(|v| !v.is_empty() && v != "0");
    let (writer, is_terminal) = if to_stdout {
        (
            BoxMakeWriter::new(std::io::stdout),
            std::io::stdout().is_terminal(),
        )
    } else {
        (
            BoxMakeWriter::new(std::io::stderr),
            std::io::stderr().is_terminal(),
        )
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_writer(writer)
        .with_ansi(color.enabled(is_terminal, no_color_requested()))
        .try_init();
}

/// Logging for the `mcp` subcommand. Writes to **stderr** because the `stdio`
/// transport uses stdout as the MCP (JSON-RPC) channel — logging there would
/// corrupt the protocol stream.
fn init_mcp_logging(color: ColorChoice) {
    use std::io::IsTerminal;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(color.enabled(std::io::stderr().is_terminal(), no_color_requested()))
        .try_init();
}

fn init_logging(config: &AppConfig, color: ColorChoice) {
    use std::io::IsTerminal;

    // --- 1. Initialize Logging ---
    // If the TOKIO_CONSOLE env var is set, initialize the console subscriber.
    // This is an exclusive choice, as the console subscriber is a logging layer.
    if std::env::var("TOKIO_CONSOLE").is_ok() {
        // console_subscriber::init();
        warn!("Tokio console subscriber not initialized. Cannot run `tokio-console` to connect.");
        return;
    }

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log_level.clone()));

    let logger = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_span_events(FmtSpan::CLOSE) // Log entry and exit of spans
        .with_target(true)
        .with_ansi(color.enabled(std::io::stdout().is_terminal(), no_color_requested()));
    match config.logger.as_str() {
        "json" => {
            logger.json().init();
        }
        "plain" => {
            logger.init();
        }
        _ => {
            logger.init();
        }
    }
    tracing::debug!(
        "Logging initialized with level {} and logger {}",
        config.log_level,
        config.logger
    );
}

#[cfg(test)]
mod ui_flag_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn the_ui_is_opt_in() {
        let bare = Args::try_parse_from(["mqb"]).expect("arguments should parse");
        assert!(!bare.ui && !bare.no_ui);
        assert!(Args::try_parse_from(["mqb", "--ui"]).unwrap().ui);
        assert!(Args::try_parse_from(["mqb", "--no-ui"]).unwrap().no_ui);
    }

    // Opting in and out at once has no sensible reading, so it is rejected
    // rather than silently resolved one way.
    #[test]
    fn ui_and_no_ui_cannot_be_combined() {
        assert!(Args::try_parse_from(["mqb", "--ui", "--no-ui"]).is_err());
    }

    // A routes-only bridge has no consumers, and must not be mistaken for an
    // empty config — that would stop a deployment at an interactive prompt.
    #[test]
    fn a_routes_only_config_counts_as_configured() {
        assert!(nothing_to_run(&AppConfig::default()));

        let config: AppConfig = serde_json::from_value(serde_json::json!({
            "routes": {
                "file_to_file": {
                    "input": { "file": { "path": "input.log" } },
                    "output": { "file": { "path": "output.log" } }
                }
            }
        }))
        .expect("a minimal routes-only config should deserialize");
        assert!(config.consumers.is_empty());
        assert!(!nothing_to_run(&config));
    }
}

#[cfg(test)]
mod report_to_ui_flag_tests {
    use super::*;
    use clap::Parser;

    fn reporting_enabled(argv: &[&str]) -> bool {
        let args = Args::try_parse_from(argv).expect("arguments should parse");
        let Some(Command::Mcp(mcp)) = args.command else {
            panic!("expected the mcp subcommand")
        };
        mcp.report_to_ui && !mcp.no_report_to_ui
    }

    // Older `mcp install` runs baked `--report-to-ui` into client configs while
    // reporting was still opt-in. Those configs must keep working, so the bare
    // flag asks for what is now the default rather than inverting it.
    #[test]
    fn the_bare_legacy_flag_is_a_no_op() {
        assert!(reporting_enabled(&["mqb", "mcp"]));
        assert!(reporting_enabled(&["mqb", "mcp", "--report-to-ui"]));
        assert!(reporting_enabled(&["mqb", "mcp", "--report-to-ui=true"]));
    }

    #[test]
    fn both_disable_spellings_turn_reporting_off() {
        assert!(!reporting_enabled(&["mqb", "mcp", "--report-to-ui=false"]));
        assert!(!reporting_enabled(&["mqb", "mcp", "--no-report-to-ui"]));
    }
}

#[cfg(test)]
mod copy_result_tests {
    use super::mq_bridge::route::RouteOutcome;
    use super::{ColorChoice, Throughput, copy_result};

    // Piping a run into a log file must not fill it with escape sequences, but a
    // terminal keeps its color, and an explicit `always` overrides both signals.
    #[test]
    fn color_follows_the_writer_unless_told_otherwise() {
        assert!(ColorChoice::Auto.enabled(true, false));
        assert!(!ColorChoice::Auto.enabled(false, false));

        // NO_COLOR only speaks for `auto`.
        assert!(!ColorChoice::Auto.enabled(true, true));
        assert!(ColorChoice::Always.enabled(false, true));
        assert!(!ColorChoice::Never.enabled(true, false));
    }

    fn moved(rows: u64) -> Throughput {
        Throughput {
            rows,
            read: rows,
            rejected: 0,
            dead_lettered: 0,
            elapsed_s: 1.0,
            rows_per_second: rows,
        }
    }

    /// A bare `703871` is the number a reader has to count digits on; the summary
    /// line is the one piece of `copy` output that gets read by a human every run.
    #[test]
    fn large_counts_are_grouped_and_short_runs_read_in_milliseconds() {
        assert_eq!(moved(703_871).rate_display(), "703_871");
        assert_eq!(moved(1_167_428).rows_display(), "1_167_428 rows");
        assert_eq!(moved(0).rows_display(), "0 rows");
        assert_eq!(moved(999).rows_display(), "999 rows");

        let mut brief = moved(2);
        brief.elapsed_s = 0.0654;
        assert_eq!(brief.elapsed_display(), "65ms");
        assert_eq!(moved(2).elapsed_display(), "1.00s");
    }

    /// A selective filter must not read as a slowdown: it reduces what lands at the
    /// destination while the copy still reads — and is timed against — every row.
    #[test]
    fn a_filtered_run_reports_both_counts_and_rates_the_rows_it_read() {
        let filtered = Throughput {
            rows: 333_495,
            read: 1_000_000,
            rejected: 0,
            dead_lettered: 0,
            elapsed_s: 1.05,
            rows_per_second: 952_380,
        };
        assert_eq!(filtered.rows_display(), "333_495 of 1_000_000 rows");
        assert_eq!(filtered.rate_display(), "952_380");
        // Nothing dropped: no second number to explain.
        assert_eq!(moved(1_000).rows_display(), "1_000 rows");
    }

    /// A route killed by a permanent error ends its task exactly like a real drain
    /// does, so `copy --drain` used to exit 0 and log "source drained" after copying
    /// nothing — silent success for a cron job. The cause must reach the exit status.
    #[test]
    fn failed_outcome_is_an_error_carrying_the_cause() {
        let err = copy_result(
            Some(RouteOutcome::Failed),
            Some("Any driver does not support MySql type Timestamp".to_string()),
            &moved(0),
        )
        .expect_err("a failed route must not report success");
        assert!(
            err.to_string()
                .contains("Any driver does not support MySql type Timestamp"),
            "the permanent error must be surfaced, got: {err}"
        );
    }

    #[test]
    fn failed_outcome_without_a_recorded_error_still_fails() {
        assert!(copy_result(Some(RouteOutcome::Failed), None, &moved(0)).is_err());
    }

    #[test]
    fn drained_and_stopped_and_interrupted_succeed() {
        assert!(copy_result(Some(RouteOutcome::Completed), None, &moved(7)).is_ok());
        assert!(copy_result(Some(RouteOutcome::Stopped), None, &moved(7)).is_ok());
        // Ctrl-C before the route finished.
        assert!(copy_result(None, None, &moved(0)).is_ok());
    }

    /// A sink that permanently rejects a message drops it and the route runs on to
    /// a normal end — by design, so one poison message cannot wedge a bridge. The
    /// rows are gone, so the copy is not a success: pointing a `sql` sink at a
    /// misspelled table used to log a `Dropping message` line per row and then
    /// print `copied 3 rows` and exit 0, which no scheduled job would ever notice.
    #[test]
    fn a_route_that_dropped_rows_is_not_a_successful_copy() {
        let dropped = Some(
            "dropped 3 message(s): sink rejected them permanently and no dlq middleware \
             is configured: no such table: orders"
                .to_string(),
        );

        for outcome in [RouteOutcome::Completed, RouteOutcome::Stopped] {
            let err = copy_result(Some(outcome), dropped.clone(), &moved(3))
                .expect_err("a lossy copy must not report success");
            assert!(
                err.to_string().contains("no such table: orders"),
                "the drop cause must reach the exit status, got: {err}"
            );
        }
    }

    // A headless drain exits non-zero on the same grounds as `copy --drain`.
    #[test]
    fn a_headless_drain_fails_on_any_failed_unstarted_or_lossy_route() {
        use super::drain_result;
        let done = |name: &str, outcome, error: Option<&str>| {
            (name.to_string(), Some(outcome), error.map(str::to_string))
        };

        let clean = [
            done("a", RouteOutcome::Completed, None),
            done("b", RouteOutcome::Completed, None),
        ];
        assert!(drain_result(&clean, 0).is_ok());

        let failed = [
            done("a", RouteOutcome::Completed, None),
            done("b", RouteOutcome::Failed, Some("boom")),
        ];
        let err = drain_result(&failed, 0).unwrap_err().to_string();
        assert!(err.contains("route 'b': boom"), "got: {err}");

        let lossy = [done("a", RouteOutcome::Completed, Some("dropped 2 rows"))];
        assert!(drain_result(&lossy, 0).is_err());

        let err = drain_result(&clean, 1).unwrap_err().to_string();
        assert!(err.contains("1 route(s) did not start"), "got: {err}");
    }
}

#[cfg(test)]
mod uri_tests {
    use super::mq_bridge::models::{EndpointType, MongoConsume};
    use super::{endpoint_from_uri, make_listen_address};

    fn config(uri: &str, tag: &str) -> serde_json::Value {
        let ep = endpoint_from_uri(uri).expect("uri should parse");
        serde_json::to_value(&ep).unwrap()[tag].clone()
    }

    // A driver option whose name matches an object-typed config field (`tls` is a
    // TlsConfig struct) must stay on the connection URL, not be hijacked as config.
    #[test]
    fn http_bulk_uri_takes_its_config_inline_or_from_a_file() {
        use super::mq_bridge::models::EndpointType;

        let file = std::env::temp_dir().join(format!("mqb-http-bulk-{}.yaml", std::process::id()));
        std::fs::write(
            &file,
            "http_bulk:\n  url: http://from-file\n  upsert:\n    path: /docs\n    result: &r\n      job: {id: /taskUid, poll: '/tasks/{id}', status: /status, succeeded: [ok], failed: [bad]}\n  delete:\n    path: /delete\n    result: *r\n",
        )
        .unwrap();
        let from_file = endpoint_from_uri(&format!(
            "http-bulk:?config_file={}&url=http://other:7700",
            file.display()
        ));
        std::fs::remove_file(&file).unwrap();
        let EndpointType::HttpBulk(config) = from_file.unwrap().endpoint_type else {
            panic!("not an http_bulk endpoint");
        };
        assert_eq!(config.url, "http://other:7700");
        assert!(config.delete.unwrap().result.job.is_some());

        let inline =
            endpoint_from_uri(r#"http-bulk:?config={"url":"http://h","upsert":{"path":"/d"}}"#);
        let EndpointType::HttpBulk(config) = inline.unwrap().endpoint_type else {
            panic!("not an http_bulk endpoint");
        };
        assert_eq!(
            (config.url.as_str(), config.upsert.unwrap().path.as_str()),
            ("http://h", "/d")
        );

        // The same scheme names a source: the config then carries `read`.
        let source = endpoint_from_uri(
            r#"http-bulk:?config={"url":"http://h","read":{"path":"/d?o={cursor}"}}"#,
        );
        let EndpointType::HttpBulk(config) = source.unwrap().endpoint_type else {
            panic!("not an http_bulk endpoint");
        };
        assert_eq!(config.read.unwrap().path, "/d?o={cursor}");
    }

    #[test]
    fn mongodb_tls_option_stays_on_url() {
        let cfg = config("mongodb://host:27017/?tls=true&database=appdb", "mongodb");
        assert_eq!(cfg["url"], "mongodb://host:27017/?tls=true");
        assert_eq!(cfg["database"], "appdb");
    }

    // A recognised scalar field becomes config; an unrecognised param is a driver
    // option and passes through on the URL unchanged.
    #[test]
    fn mongodb_scalar_field_vs_driver_param() {
        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&replicaSet=rs0",
            "mongodb",
        );
        assert_eq!(cfg["collection"], "orders");
        assert_eq!(cfg["url"], "mongodb://host/?replicaSet=rs0");
    }

    // MongoDB sources are non-destructive by default in the CLI: `consume` is set
    // to `capture_all` when the user gives neither `consume` nor `change_stream`,
    // so pointing at an existing collection never claims/deletes its documents.
    #[test]
    fn mongodb_defaults_to_non_destructive_capture_all() {
        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "capture_all");
    }

    // An explicit `consume` (or the deprecated `change_stream`) always wins over
    // the non-destructive default.
    #[test]
    fn mongodb_explicit_consume_wins_over_default() {
        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&consume=consumer",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "consumer");

        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&change_stream=true",
            "mongodb",
        );
        assert!(cfg["consume"].is_null());
        assert_eq!(cfg["change_stream"], true);
        let endpoint = endpoint_from_uri(
            "mongodb://host/?collection=orders&database=appdb&change_stream=true",
        )
        .unwrap();
        let EndpointType::MongoDb(mongo) = endpoint.endpoint_type else {
            panic!("expected MongoDB endpoint");
        };
        assert_eq!(mongo.resolved_consume(), MongoConsume::CaptureNew);

        let cfg = config(
            "mongodb://host/?collection=orders&database=appdb&consume=snapshot&change_stream=true",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "snapshot");
        assert_eq!(cfg["change_stream"], true);
        let endpoint = endpoint_from_uri(
            "mongodb://host/?collection=orders&database=appdb&consume=snapshot&change_stream=true",
        )
        .unwrap();
        let EndpointType::MongoDb(mongo) = endpoint.endpoint_type else {
            panic!("expected MongoDB endpoint");
        };
        assert_eq!(mongo.resolved_consume(), MongoConsume::Snapshot);

        let error = endpoint_from_uri(
            "mongodb://host/?collection=orders&database=appdb&consume=subscriber",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("subscriber"), "{error:#}");
    }

    // A redis URL path is the database number, not the stream, so it must remain on
    // the connection URL.
    #[test]
    fn redis_path_is_database_not_stream() {
        let cfg = config("redis://host:6379/0", "redis_streams");
        assert_eq!(cfg["url"], "redis://host:6379/0");
        assert!(cfg["stream"].is_null());
    }

    // Escaped mode: `?url=<encoded>` is used verbatim (its own options are never
    // re-interpreted), while sibling params still set config fields.
    #[test]
    fn escaped_url_is_verbatim() {
        let inner = "mongodb://u:p@host/db?tls=true&replicaSet=rs0";
        let mut outer = url::Url::parse("mongodb://_/").unwrap();
        outer
            .query_pairs_mut()
            .append_pair("url", inner)
            .append_pair("collection", "orders")
            .append_pair("database", "appdb");
        let cfg = config(outer.as_str(), "mongodb");
        assert_eq!(cfg["url"], inner);
        assert_eq!(cfg["collection"], "orders");
    }

    // `null:` builds the discard sink regardless of trailing content. A flattened
    // unit `Null` variant serializes as a `null` key with a null value.
    #[test]
    fn null_scheme_builds_null_endpoint() {
        let ep = endpoint_from_uri("null:").expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert!(v.get("null").is_some(), "expected a null endpoint, got {v}");
    }

    // `static:` carries its payload in `?body=`; `raw=true` sends it verbatim.
    #[test]
    fn static_scheme_body_and_raw() {
        let cfg = config("static:?body=hello&raw=true", "static");
        assert_eq!(cfg["body"], "hello");
        assert_eq!(cfg["raw"], true);
    }

    // `memory://topic` maps the host to the channel topic.
    #[test]
    fn memory_scheme_topic_from_host() {
        let cfg = config("memory://my-topic?capacity=1000", "memory");
        assert_eq!(cfg["topic"], "my-topic");
        assert_eq!(cfg["capacity"], 1000);
    }

    // `postgres_cdc://` selects the CDC endpoint; the connection URL is rebuilt
    // with a plain `postgres` scheme, and `publication` is a scalar config field.
    #[test]
    fn postgres_cdc_scheme_rewrites_url_and_takes_publication() {
        let cfg = config(
            "postgres-cdc://u:p@host:5432/db?publication=mqb_pub&slot_name=mqb_slot",
            "postgres_cdc",
        );
        assert_eq!(cfg["url"], "postgres://u:p@host:5432/db");
        assert_eq!(cfg["publication"], "mqb_pub");
        assert_eq!(cfg["slot_name"], "mqb_slot");
    }

    // In escaped mode the connection string is complete, so a stray driver-style
    // param is rejected rather than silently dropped.
    #[test]
    fn escaped_url_rejects_stray_param() {
        let mut outer = url::Url::parse("mongodb://_/").unwrap();
        outer
            .query_pairs_mut()
            .append_pair("url", "mongodb://host/db")
            .append_pair("database", "appdb")
            .append_pair("bogus", "x");
        let err = endpoint_from_uri(outer.as_str()).unwrap_err();
        assert!(err.to_string().contains("escaped mode"), "got: {err}");
    }

    // A file endpoint has no driver options, so an object-typed field such as
    // `encryption` is read as a JSON literal instead of being dropped.
    #[test]
    fn file_object_field_is_read_as_json_literal() {
        let mut uri = url::Url::parse("file:///tmp/out.jsonl").unwrap();
        uri.query_pairs_mut()
            .append_pair("format", "raw")
            .append_pair("compression", "gzip")
            .append_pair("encryption", r#"{"key_id":"k1","key":"${env:MQB_KEY}"}"#);
        let cfg = config(uri.as_str(), "file");
        assert_eq!(cfg["path"], "/tmp/out.jsonl");
        assert_eq!(cfg["format"], "raw");
        assert_eq!(cfg["compression"], "gzip");
        assert_eq!(cfg["encryption"]["key_id"], "k1");
        assert_eq!(cfg["encryption"]["key"], "${env:MQB_KEY}");
    }

    // The consumer mode is a `#[serde(flatten)]`ed tagged enum, so `mode` and its
    // variant fields are only recognised by walking the schema — they must not
    // trip the unrecognised-param error.
    #[test]
    fn file_flattened_mode_fields_are_recognised() {
        let cfg = config("file:///var/log/app.log?mode=subscribe&delete=true", "file");
        assert_eq!(cfg["mode"], "subscribe");
        assert_eq!(cfg["delete"], true);
    }

    /// `dir_spool` is targeted by path like `file`, under three scheme spellings
    /// (an underscore is not legal in a URI scheme).
    #[test]
    fn spool_scheme_takes_the_path_from_the_uri() {
        for uri in [
            "spool:///tmp/video?payload_extension=.h264",
            "dir-spool:///tmp/video?payload_extension=.h264",
            "dirspool:///tmp/video?payload_extension=.h264",
        ] {
            let cfg = config(uri, "dir_spool");
            assert_eq!(cfg["path"], "/tmp/video", "for {uri}");
            assert_eq!(cfg["payload_extension"], ".h264", "for {uri}");
            assert!(cfg.get("url").is_none(), "{uri} must not carry a url field");
        }
    }

    /// The CSV dialect is a nested struct, so it arrives as one JSON parameter.
    #[test]
    fn file_csv_dialect_is_a_json_param() {
        let mut uri = url::Url::parse("file:///tmp/export.csv?format=csv").unwrap();
        uri.query_pairs_mut().append_pair(
            "csv",
            r#"{"separator":"auto","header":false,"columns":["id"]}"#,
        );
        let cfg = config(uri.as_str(), "file");
        assert_eq!(cfg["format"], "csv");
        assert_eq!(cfg["csv"]["separator"], "auto");
        assert_eq!(cfg["csv"]["columns"][0], "id");
    }

    /// The authority-less form has no `//` to strip, so the scheme has to come off on
    /// its own or the path keeps it and names a relative directory.
    #[test]
    fn a_path_uri_without_an_authority_still_yields_an_absolute_path() {
        for (uri, tag) in [
            ("spool:/tmp/video", "dir_spool"),
            ("dir-spool:/tmp/video", "dir_spool"),
            ("file:/tmp/out.jsonl", "file"),
        ] {
            assert_eq!(config(uri, tag)["path"], uri.split(':').nth(1).unwrap());
        }
    }

    /// The producer/consumer settings from the documented example are all scalar
    /// config fields, so none of them trips the unrecognised-param error.
    #[test]
    fn spool_producer_and_consumer_settings_are_config_fields() {
        let cfg = config(
            "spool:///tmp/q?naming_pattern={seq:06d}&emit_done=success&shard_depth=2",
            "dir_spool",
        );
        assert_eq!(cfg["naming_pattern"], "{seq:06d}");
        assert_eq!(cfg["emit_done"], "success");
        assert_eq!(cfg["shard_depth"], 2);

        let cfg = config(
            "spool:///tmp/q?drain_on_read=true&stop_on_done=true",
            "dir_spool",
        );
        assert_eq!(cfg["drain_on_read"], true);
        assert_eq!(cfg["stop_on_done"], true);
    }

    /// A spool has no connection URL, so an unrecognised param is a user error
    /// rather than a driver option riding along.
    #[test]
    fn spool_rejects_an_unrecognised_param() {
        let err = endpoint_from_uri("spool:///tmp/q?nonsense=1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonsense"), "got: {err}");
    }

    #[test]
    fn file_name_by_is_a_scalar_flag() {
        let cfg = config("file:///var/lib/mqb/parts?name_by=source_position", "file");
        assert_eq!(cfg["name_by"], "source_position");
    }

    /// `idempotency` is the deprecated spelling of `name_by`; v0.4.x URLs must keep working.
    #[test]
    fn file_idempotency_is_still_accepted() {
        let cfg = config("file:///var/lib/mqb/parts?idempotency=true", "file");
        assert_eq!(cfg["idempotency"], true);
    }

    // Rejecting unrecognised params on the endpoints that have no driver options
    // only works if every documented param really is a config field. These are the
    // example URIs from README.md, docs/book/ and benches/etl/.
    #[test]
    fn documented_example_uris_parse() {
        for uri in [
            // Endpoints with no connection-URL driver options: newly strict.
            "kafka://kafka.local:9092?topic=orders&group_id=mqb-orders-sync",
            "kafka://kafka.local:9093?topic=orders&username=svc&password=secret",
            "mqtt://broker.local:1883?topic=alerts&client_id=mqb-alerts-01&qos=2",
            "mqtts://user:pass@broker.local:8883?topic=events",
            "nats://localhost:4222?subject=orders",
            "zeromq://127.0.0.1:5555?socket_type=push",
            "grpc://localhost:50051?topic=orders",
            "ibmmq://qmhost:1414?queue_manager=QM1&channel=DEV.APP.SVRCONN&queue=orders",
            "aws://_/?queue_url=https://sqs.us-east-1.amazonaws.com/123/orders&region=us-east-1",
            "memory://my-topic?capacity=1000",
            "file:///data/customers.csv?format=csv",
            "file:///var/log/app/events.log?mode=subscribe",
            // Endpoints that do carry driver options: `sslmode`/`async_insert` are
            // driver options, not config fields, and must still pass through.
            "postgres://u:p@localhost:5432/db?table=bench&cursor_column=id&sslmode=disable",
            "clickhouse://user:pass@ch.local:8123?table=events&database=analytics&async_insert=true",
            "amqp://guest:guest@localhost:5672/%2f?exchange=events&queue=events",
            "mongodb://localhost?database=app&collection=orders&consume=capture_new",
            "postgres-cdc://user:pass@localhost/app?publication=mqb_pub&slot_name=mqb_slot",
            "https://api.example.com/ingest?method=POST&request_timeout_ms=5000",
        ] {
            if let Err(e) = endpoint_from_uri(uri) {
                panic!("documented URI should parse: {uri}\n  {e:#}");
            }
        }
    }

    // On an endpoint whose URL does carry driver options, a JSON literal picks the
    // nested config field while a scalar of the same name stays a driver option
    // (see `mongodb_tls_option_stays_on_url` for the scalar half).
    #[test]
    fn mongodb_tls_json_literal_is_config() {
        let mut uri = url::Url::parse("mongodb://host:27017/").unwrap();
        uri.query_pairs_mut()
            .append_pair("database", "appdb")
            .append_pair("tls", r#"{"required":true,"ca_file":"/etc/ca.pem"}"#);
        let cfg = config(uri.as_str(), "mongodb");
        assert_eq!(cfg["tls"]["required"], true);
        assert_eq!(cfg["tls"]["ca_file"], "/etc/ca.pem");
        assert_eq!(cfg["url"], "mongodb://host:27017/");
    }

    // Kafka's connection string is a bare `host:port` list, not a URI, so a param
    // appended to it could never be read as a driver option — an object field is
    // config, and an unrecognised name is an error.
    #[test]
    fn kafka_object_fields_are_config_not_url_junk() {
        let mut uri = url::Url::parse("kafka://broker:9092").unwrap();
        uri.query_pairs_mut()
            .append_pair("topic", "orders")
            .append_pair("tls", r#"{"required":true}"#)
            .append_pair("producer_options", r#"[["linger.ms","5"]]"#);
        let cfg = config(uri.as_str(), "kafka");
        assert_eq!(cfg["url"], "broker:9092");
        assert_eq!(cfg["tls"]["required"], true);
        assert_eq!(cfg["producer_options"][0][0], "linger.ms");
    }

    #[test]
    fn kafka_rejects_unrecognised_param() {
        let err = endpoint_from_uri("kafka://broker:9092?topic=t&bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // IBM MQ's `host(port)` connection string is likewise not a URI; `tls` is a
    // nested struct that was previously unreachable from a URI.
    #[test]
    fn ibmmq_tls_is_config_and_url_stays_host_port() {
        let mut uri = url::Url::parse("ibmmq://qmhost:1414").unwrap();
        uri.query_pairs_mut()
            .append_pair("queue_manager", "QM1")
            .append_pair("channel", "DEV.APP.SVRCONN")
            .append_pair("queue", "orders")
            .append_pair("tls", r#"{"required":true,"cipher_spec":"ANY_TLS12"}"#);
        let cfg = config(uri.as_str(), "ibmmq");
        assert_eq!(cfg["url"], "qmhost(1414)");
        assert_eq!(cfg["tls"]["cipher_spec"], "ANY_TLS12");
    }

    // AwsConfig has no `url` field, so a leftover param had nothing to ride on and
    // was dropped on the floor.
    #[test]
    fn aws_rejects_unrecognised_param() {
        let err = endpoint_from_uri("aws://_/?region=us-east-1&bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // An in-process channel has no connection URL at all.
    #[test]
    fn memory_rejects_unrecognised_param() {
        let err = endpoint_from_uri("memory://my-topic?bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // `path` is a real WebSocketConfig field; it used to be skipped for every
    // scheme because a file endpoint derives its path from the URI.
    #[test]
    fn websocket_path_param_reaches_config() {
        let cfg = config("ws://host:8080?path=/stream", "websocket");
        assert_eq!(cfg["path"], "/stream");
    }

    // A param that is not a FileConfig field can never take effect, so it is
    // rejected rather than silently ignored.
    #[test]
    fn file_rejects_unrecognised_param() {
        let err = endpoint_from_uri("file:///tmp/out.jsonl?bogus=x").unwrap_err();
        assert!(
            err.to_string().contains("unrecognised query param"),
            "got: {err}"
        );
    }

    // An object-typed field given something that is not JSON at all is reported as
    // such, rather than reaching serde as a bare string. (A value that *is* valid
    // JSON but the wrong shape still gets serde's own type error.)
    #[test]
    fn file_object_field_rejects_non_json() {
        let err = endpoint_from_uri("file:///tmp/out.jsonl?encryption=yes-please").unwrap_err();
        assert!(
            err.to_string().contains("expects a JSON literal"),
            "got: {err}"
        );
    }

    // `|`-separated middlewares wrap the endpoint in the order written, and their
    // params are coerced to the middleware config field's own type.
    #[test]
    fn middlewares_are_appended_in_order() {
        let ep = endpoint_from_uri("kafka://broker:9092?topic=orders|retry?max_attempts=5|metrics")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["kafka"]["topic"], "orders");
        let mw = v["middlewares"].as_array().expect("middlewares array");
        assert_eq!(mw.len(), 2);
        assert_eq!(mw[0]["retry"]["max_attempts"], 5);
        assert!(mw[1].get("metrics").is_some(), "got: {}", mw[1]);
    }

    // A middleware's own object-typed field takes a JSON literal, and `-` in the
    // name is accepted for the snake_case tag.
    #[test]
    fn middleware_dash_alias_and_json_field() {
        let ep = endpoint_from_uri(
            "null:|weak-join?group_by=cid&expected_count=2&timeout_ms=1000&required=[\"a\",\"b\"]",
        )
        .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        let wj = &v["middlewares"][0]["weak_join"];
        assert_eq!(wj["group_by"], "cid");
        assert_eq!(wj["required"], serde_json::json!(["a", "b"]));
    }

    // `transform`'s `schema` is an untyped `serde_json::Value`, so it must still
    // be read as a JSON literal rather than handed through as a string (which
    // `transform` rejects with "schema must be a JSON object").
    #[test]
    fn transform_schema_param_is_a_json_literal() {
        let schema = r#"{"type":"object","properties":{"qty":{"type":"number"}}}"#;
        let mut spec = String::from("null:|transform?");
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("schema", schema);
        spec.push_str(&q.finish());

        let ep = endpoint_from_uri(&spec).expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(
            v["middlewares"][0]["transform"]["schema"],
            serde_json::from_str::<serde_json::Value>(schema).unwrap()
        );
    }

    // `dlq`'s `endpoint` param is itself an endpoint URI, parsed recursively.
    #[test]
    fn dlq_endpoint_param_is_a_nested_uri() {
        let mut spec = String::from("kafka://broker:9092?topic=orders|dlq?");
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("endpoint", "file:///tmp/failed.jsonl");
        spec.push_str(&q.finish());
        let ep = endpoint_from_uri(&spec).expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(
            v["middlewares"][0]["dlq"]["endpoint"]["file"]["path"],
            "/tmp/failed.jsonl"
        );
    }

    // A `--from` http endpoint is a listener, and its driver takes a bare
    // `host:port` — the scheme the URI needed to select the endpoint would
    // otherwise reach it as part of the address.
    #[test]
    fn an_http_source_url_becomes_a_listen_address() {
        let mut ep =
            endpoint_from_uri("http://0.0.0.0:8080?method=POST").expect("uri should parse");
        make_listen_address(&mut ep).unwrap();
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["http"]["url"], "0.0.0.0:8080");
        assert_eq!(v["http"]["method"], "POST");
        assert_eq!(v["http"]["tls"]["required"], false);

        // `https` asks for a TLS listener; the certificate still comes from `tls`.
        let mut ep = endpoint_from_uri("https://0.0.0.0:8443").expect("uri should parse");
        make_listen_address(&mut ep).unwrap();
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["http"]["url"], "0.0.0.0:8443");
        assert_eq!(v["http"]["tls"]["required"], true);

        // Endpoints that are never servers keep their connection URL.
        let mut ep = endpoint_from_uri("kafka://broker:9092?topic=orders").expect("parses");
        let before = serde_json::to_value(&ep).unwrap();
        make_listen_address(&mut ep).unwrap();
        assert_eq!(serde_json::to_value(&ep).unwrap(), before);

        let err =
            make_listen_address(&mut endpoint_from_uri("wss://0.0.0.0:9000").unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("wss"), "got: {err:#}");
    }

    // The mirror-proxy shape: every branch gets the message, and only the `to`
    // branch is left able to answer the caller.
    #[test]
    fn fanout_mirrors_and_keeps_one_answering_branch() {
        let ep =
            endpoint_from_uri("fanout:?mirror=http://staging.internal/&to=http://prod.internal/")
                .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        let branches = v["fanout"].as_array().expect("branches keep their order");
        assert_eq!(branches.len(), 2);

        // The mirror is wrapped so its response and its failures go nowhere.
        assert_eq!(
            branches[0]["request"]["to"]["http"]["url"],
            "http://staging.internal/"
        );
        assert!(branches[0]["request"]["forward_to"].get("null").is_some());
        assert_eq!(branches[1]["http"]["url"], "http://prod.internal/");
    }

    #[test]
    fn structural_uris_nest_endpoints_and_name_their_mistakes() {
        // A nested URI with its own query params is percent-encoded.
        let ep = endpoint_from_uri("request:?to=http%3A%2F%2Fapi.internal%2F%3Fmethod%3DPUT")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["request"]["to"]["http"]["method"], "PUT");
        // Without a `forward_to` the response is discarded.
        assert!(v["request"]["forward_to"].get("null").is_some());

        let ep = endpoint_from_uri("switch:?metadata_key=http_status_code&case.200=null:&default=file%3A%2F%2F%2Ftmp%2Fother.jsonl")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["switch"]["metadata_key"], "http_status_code");
        assert!(v["switch"]["cases"]["200"].get("null").is_some());
        assert_eq!(v["switch"]["default"]["file"]["path"], "/tmp/other.jsonl");

        // Predicate mode: `when`/`to` pairs keep their order, and an expression
        // needs no escaping for its `=` because it travels as a query *value*.
        let ep = endpoint_from_uri(
            "switch:?when=amount > 100&to=null:&when=status == 'paid'&to=file%3A%2F%2F%2Ftmp%2Fpaid.jsonl&default=file%3A%2F%2F%2Ftmp%2Frest.jsonl",
        )
        .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["switch"]["when"][0]["if"], "amount > 100");
        assert!(v["switch"]["when"][0]["to"].get("null").is_some());
        assert_eq!(v["switch"]["when"][1]["if"], "status == 'paid'");
        assert_eq!(
            v["switch"]["when"][1]["to"]["file"]["path"],
            "/tmp/paid.jsonl"
        );
        assert_eq!(v["switch"]["default"]["file"]["path"], "/tmp/rest.jsonl");
        assert_eq!(v["switch"]["metadata_key"], "");

        let ep = endpoint_from_uri("response:").expect("uri should parse");
        assert!(serde_json::to_value(&ep).unwrap().get("response").is_some());

        for (uri, expected) in [
            ("fanout:", "no branches"),
            // Named by the key, not by whatever its value fails to parse as.
            (
                "fanout:?towards=bogus://x",
                "unsupported query param 'towards'",
            ),
            ("response:?to=null:", "unsupported query param 'to'"),
            ("http-bulk:", "needs 'config_file=<path>'"),
            ("http-bulk:?config=a&config_file=b", "twice"),
            ("http-bulk:?config=url: [", "not valid YAML or JSON"),
            (
                "http-bulk:?config={url: 'http://h', upsert: {path: /d, foo: 1}}",
                "unknown field `foo`",
            ),
            ("request:?forward_to=null:", "needs a 'to=<uri>'"),
            ("request:?to=null:&to=null:", "duplicate query param 'to'"),
            (
                "request:?to=null:&forward_to=null:&forward_to=null:",
                "duplicate query param 'forward_to'",
            ),
            ("switch:?case.200=null:", "needs a 'metadata_key=<key>'"),
            ("switch:?when=amount > 100", "with no 'to=<uri>' after it"),
            (
                "switch:?when=a&when=b&to=null:",
                "with no 'to=<uri>' after it",
            ),
            ("switch:?to=null:", "that no 'when=<expression>' precedes"),
            (
                "switch:?metadata_key=k&case.1=null:&when=amount > 100&to=null:",
                "mixes both modes",
            ),
            (
                "fanout:?to=bogus://x",
                "unsupported endpoint scheme 'bogus'",
            ),
        ] {
            let err = format!("{:#}", endpoint_from_uri(uri).unwrap_err());
            assert!(err.contains(expected), "{uri}: got {err}");
        }
    }

    // A URI that fails to parse is quoted in the error without its credentials.
    #[test]
    fn uri_errors_do_not_quote_credentials() {
        for uri in [
            "mongodb://app:hunter2@db/shop?bogus=1",
            "s3://bucket/prefix?secret_access_key=hunter2&bogus=1",
            "http://host/x?basic_auth=%5B%22app%22%2C%22hunter2%22%5D&request_timeout_ms=soon",
            "fanout:?to=bogus://app:hunter2@host/x",
            "null:|encryption?key=hunter2&bogus=1",
            "null:|dlq?endpoint=bogus://app:hunter2@host/x",
            "http://host/x?basic_auth=[hunter2",
            "switch:?to=bogus://app:hunter2@host/x",
        ] {
            let err = format!("{:#}", endpoint_from_uri(uri).unwrap_err());
            assert!(!err.contains("hunter2"), "{uri}: got {err}");
        }
    }

    // An unknown middleware name is rejected with the supported list.
    #[test]
    fn unknown_middleware_is_rejected() {
        let err = endpoint_from_uri("null:|bogus").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported middleware 'bogus'"), "got: {msg}");
    }

    // A registered middleware builds `custom`, its params typed by its declared
    // schema, and is named in the unknown-middleware error.
    #[test]
    fn registered_middleware_builds_a_custom_middleware() {
        #[derive(Debug)]
        struct Declaring;

        impl super::mq_bridge::traits::CustomMiddlewareFactory for Declaring {
            fn config_schema(&self) -> Option<serde_json::Value> {
                Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": { "type": "integer" },
                        "id": { "type": "string" }
                    }
                }))
            }
        }

        let name = "cli-test-declaring-mw";
        super::mq_bridge::extensions::register_middleware_factory(
            name,
            std::sync::Arc::new(Declaring),
        )
        .unwrap();
        let built = super::middleware_from_spec(&format!("{name}?limit=5&id=0123"));
        let err = format!("{:#}", super::middleware_from_spec("bogus").unwrap_err());
        super::mq_bridge::extensions::unregister_middleware_factory(name);

        let super::mq_bridge::models::Middleware::Custom {
            name: built_name,
            config,
        } = built.unwrap()
        else {
            panic!("expected a custom middleware");
        };
        assert_eq!(built_name, name);
        assert_eq!(config, serde_json::json!({ "limit": 5, "id": "0123" }));
        assert!(err.contains(name), "got: {err}");
    }

    // Every `Middleware` variant is reachable from a spec, so a new one can't
    // go missing here. Some reject an empty config, but never as unsupported.
    #[test]
    fn every_middleware_variant_is_accepted_by_name() {
        let root =
            serde_json::to_value(schemars::schema_for!(super::mq_bridge::models::Middleware))
                .unwrap();
        let tags = super::middleware_tags(&root);
        assert!(tags.contains(&"otel".to_string()), "got: {tags:?}");
        for tag in tags {
            if let Err(err) = super::middleware_from_spec(&tag) {
                let msg = format!("{err:#}");
                assert!(!msg.contains("unsupported middleware"), "{tag}: {msg}");
            }
        }
    }

    #[test]
    fn string_middlewares_take_the_raw_query() {
        let ep = endpoint_from_uri("null:|id?${payload:order_id}|filter?amount%20%3E%20100|otel")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["middlewares"][0]["id"], "${payload:order_id}");
        assert_eq!(v["middlewares"][1]["filter"], "amount > 100");
        assert!(v["middlewares"][2].get("otel").is_some(), "got: {v}");
    }

    #[test]
    fn filter_takes_its_map_form_from_named_fields() {
        let ep = endpoint_from_uri("null:|filter?expression=amount%20%3E%20100&on_error=drop")
            .expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["middlewares"][0]["filter"]["expression"], "amount > 100");
        assert_eq!(v["middlewares"][0]["filter"]["on_error"], "drop");

        // `==` is not a field assignment, so this stays a bare expression.
        let ep = endpoint_from_uri("null:|filter?on_error%20==%201").expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        assert_eq!(v["middlewares"][0]["filter"], "on_error == 1");
    }

    // An endpoint-typed field anywhere takes a URI, not only `dlq`'s.
    #[test]
    fn lookup_from_takes_an_endpoint_uri() {
        let ep =
            endpoint_from_uri("null:|lookup?from=null%3A&into=extra").expect("uri should parse");
        let v = serde_json::to_value(&ep).unwrap();
        let lookup = &v["middlewares"][0]["lookup"];
        assert_eq!(lookup["into"], "extra");
        assert!(lookup["from"].get("null").is_some(), "got: {lookup}");
    }

    // Without a declared schema every param stays a string, and a registered
    // `_` name resolves from its `-` spelling.
    #[test]
    fn undeclared_registered_middleware_keeps_params_as_strings() {
        #[derive(Debug)]
        struct Silent;

        impl super::mq_bridge::traits::CustomMiddlewareFactory for Silent {}

        let name = "cli_test_silent_mw";
        super::mq_bridge::extensions::register_middleware_factory(
            name,
            std::sync::Arc::new(Silent),
        )
        .unwrap();
        let built = super::middleware_from_spec("cli-test-silent-mw?id=0123");
        super::mq_bridge::extensions::unregister_middleware_factory(name);

        let super::mq_bridge::models::Middleware::Custom {
            name: built_name,
            config,
        } = built.unwrap()
        else {
            panic!("expected a custom middleware");
        };
        assert_eq!(built_name, name);
        assert_eq!(config, serde_json::json!({ "id": "0123" }));
    }

    // A scheme naming a registered endpoint builds `custom`, so `copy` can
    // address an extension or `--plugin` endpoint the URI parser knows nothing
    // about. `pulsar` is compiled in, so registering it is enough to exercise it.
    #[test]
    #[cfg(feature = "pulsar")]
    fn registered_endpoint_scheme_builds_a_custom_endpoint() {
        mq_bridge_app::plugins::register_builtin_endpoints().unwrap();

        let endpoint = endpoint_from_uri(
            "pulsar://localhost:6650?topic=persistent://public/default/orders&subscription=workers",
        )
        .unwrap();

        let EndpointType::Custom { name, config } = endpoint.endpoint_type else {
            panic!(
                "expected a custom endpoint, got {:?}",
                endpoint.endpoint_type
            );
        };
        assert_eq!(name, "pulsar");
        assert_eq!(config["url"], "pulsar://localhost:6650");
        assert_eq!(config["topic"], "persistent://public/default/orders");
        assert_eq!(config["subscription"], "workers");
    }

    // Meilisearch reaches `copy` the same way, as the `http_bulk` preset or as
    // the plugin crate. Either normalizes the `meilisearch://` url to the HTTP
    // one the server speaks.
    #[test]
    #[cfg(any(feature = "meilisearch", feature = "http-bulk"))]
    fn meilisearch_scheme_builds_a_custom_endpoint() {
        mq_bridge_app::plugins::register_builtin_endpoints().unwrap();

        let endpoint =
            endpoint_from_uri("meilisearch://localhost:7700?index=orders&primary_key=id").unwrap();

        let EndpointType::Custom { name, config } = endpoint.endpoint_type else {
            panic!(
                "expected a custom endpoint, got {:?}",
                endpoint.endpoint_type
            );
        };
        assert_eq!(name, "meilisearch");
        assert_eq!(config["url"], "meilisearch://localhost:7700");
        assert_eq!(config["index"], "orders");
        assert_eq!(config["primary_key"], "id");
    }

    // A named `http_bulk` endpoint takes its address and its collection or index
    // from the URI, so a load needs no config file.
    #[test]
    #[cfg(feature = "http-bulk")]
    fn http_bulk_preset_schemes_build_custom_endpoints() {
        mq_bridge_app::plugins::register_builtin_endpoints().unwrap();

        for (uri, url, field) in [
            (
                "typesense://localhost:8108/books?api_key=k",
                "typesense://localhost:8108",
                "collection",
            ),
            (
                "elasticsearch+https://es.example.com/books?api_key=k",
                "https://es.example.com",
                "index",
            ),
        ] {
            let endpoint = endpoint_from_uri(uri).unwrap();
            let EndpointType::Custom { config, .. } = endpoint.endpoint_type else {
                panic!("expected a custom endpoint for {uri}");
            };
            assert_eq!(config["url"], url);
            assert_eq!(config[field], "books");
            assert_eq!(config["api_key"], "k");
        }
        let error = endpoint_from_uri("typesense://h/books?request_timeout_ms=soon").unwrap_err();
        assert!(
            format!("{error:#}").contains("request_timeout_ms"),
            "{error:#}"
        );
    }

    // Deliberately ungated: the features that name an extension here are this
    // crate's, while the one that compiles it in is the core crate's. A build
    // enabling only the latter registers the endpoint but omits it from the
    // error, and every gated test would still pass.
    #[test]
    fn every_registered_extension_is_named_in_the_scheme_error() {
        mq_bridge_app::plugins::register_builtin_endpoints().unwrap();
        let schemes = super::extension_schemes();

        for name in ["pulsar", "meilisearch", "typesense", "elasticsearch"] {
            if super::mq_bridge::extensions::get_endpoint_factory(name).is_some() {
                assert!(
                    schemes.contains(name),
                    "`{name}` is registered but missing from `{schemes}`: enable the \
                     `{name}` feature of this crate alongside the core one"
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "pulsar")]
    fn registered_endpoint_url_preserves_a_trailing_slash() {
        mq_bridge_app::plugins::register_builtin_endpoints().unwrap();

        let endpoint = endpoint_from_uri("pulsar://localhost:6650/?topic=orders").unwrap();

        let EndpointType::Custom { config, .. } = endpoint.endpoint_type else {
            panic!("expected a custom endpoint");
        };
        assert_eq!(config["url"], "pulsar://localhost:6650/");
    }

    // An unregistered scheme still fails fast rather than becoming a `custom`
    // endpoint that would only break later with "no factory named ...".
    #[test]
    fn unregistered_scheme_still_fails_fast() {
        let err = endpoint_from_uri("kafkaa://broker:9092").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported endpoint scheme 'kafkaa'"),
            "got: {msg}"
        );
    }

    #[derive(Debug)]
    struct SubschemeFactory;

    impl super::mq_bridge::traits::CustomEndpointFactory for SubschemeFactory {
        fn config_schema(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "component": { "type": "string", "x-mqb-uri": "subscheme" },
                    "url": { "type": "string", "x-mqb-uri": "url" },
                    "subject": { "type": "string", "x-mqb-uri": "path" }
                }
            }))
        }
    }

    // `plugin+component://` names the plugin before the `+`; the component
    // reaches the factory's config through its schema.
    #[test]
    fn a_plugin_scheme_with_a_component_resolves_the_plugin() {
        let _ = super::mq_bridge::extensions::register_endpoint_factory(
            "subschemetest",
            std::sync::Arc::new(SubschemeFactory),
        );

        let endpoint =
            endpoint_from_uri("subschemetest+nats-jetstream://127.0.0.1:4222/subj.data").unwrap();

        let EndpointType::Custom { name, config } = endpoint.endpoint_type else {
            panic!(
                "expected a custom endpoint, got {:?}",
                endpoint.endpoint_type
            );
        };
        assert_eq!(name, "subschemetest");
        assert_eq!(config["component"], "nats-jetstream");
        assert_eq!(config["url"], "nats-jetstream://127.0.0.1:4222/subj.data");
        assert_eq!(config["subject"], "subj.data");
    }

    // The search-path hint names the plugin's library, not one for the whole scheme.
    #[test]
    fn an_unknown_plugin_with_a_component_names_the_plugin_library() {
        let err = endpoint_from_uri("nosuchplugin+nats-jetstream://127.0.0.1:4222").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported endpoint scheme 'nosuchplugin+nats-jetstream'"),
            "got: {msg}"
        );
        assert!(
            !msg.contains("jetstream."),
            "hint names the wrong library: {msg}"
        );
    }

    // `kafka://` selects the Kafka endpoint; the scheme is stripped so `url`
    // (rdkafka's bootstrap.servers) is a bare host:port, and `topic` is scalar.
    #[test]
    fn kafka_scheme_strips_prefix_and_takes_topic() {
        let cfg = config("kafka://broker:9092?topic=orders", "kafka");
        assert_eq!(cfg["url"], "broker:9092");
        assert_eq!(cfg["topic"], "orders");
    }

    #[test]
    fn kafka_source_metadata_is_a_scalar_flag() {
        let cfg = config(
            "kafka://broker:9092?topic=orders&source_metadata=true",
            "kafka",
        );
        assert_eq!(cfg["source_metadata"], true);
    }

    // `mqtt://` is rewritten to `tcp://` (what rumqtt expects); `mqtts://`
    // becomes `ssl://`.
    #[test]
    fn mqtt_scheme_rewrites_to_tcp_and_ssl() {
        let cfg = config("mqtt://broker:1883?topic=sensors", "mqtt");
        assert_eq!(cfg["url"], "tcp://broker:1883");
        assert_eq!(cfg["topic"], "sensors");

        let cfg = config("mqtts://broker:8883?topic=sensors", "mqtt");
        assert_eq!(cfg["url"], "ssl://broker:8883");
    }

    // `rabbitmq://` is accepted as an alias for `amqp://`, rewritten to the
    // native scheme; `queue` is a scalar config field.
    #[test]
    fn rabbitmq_scheme_rewrites_to_amqp() {
        let cfg = config(
            "rabbitmq://guest:guest@host:5672/vhost?queue=orders",
            "amqp",
        );
        assert_eq!(cfg["url"], "amqp://guest:guest@host:5672/vhost");
        assert_eq!(cfg["queue"], "orders");
    }

    // `http://`/`https://` pass through unchanged, with the target path already
    // part of the URL; `method` is a scalar config field.
    #[test]
    fn http_scheme_passthrough_and_scalar_fields() {
        let cfg = config("http://api.example.com/ingest?method=POST", "http");
        assert_eq!(cfg["url"], "http://api.example.com/ingest");
        assert_eq!(cfg["method"], "POST");
    }

    // `clickhouse://` is rewritten to `http://` (ClickHouse's HTTP interface);
    // `table` and `database` are scalar config fields.
    #[test]
    fn clickhouse_scheme_rewrites_to_http() {
        let cfg = config(
            "clickhouse://host:8123?table=events&database=analytics",
            "clickhouse",
        );
        assert_eq!(cfg["url"], "http://host:8123");
        assert_eq!(cfg["table"], "events");
        assert_eq!(cfg["database"], "analytics");
    }

    // Storage schemes select the `object_store` endpoint. Cloud URLs pass through,
    // while CLI aliases are normalized to schemes understood by the backing crate.
    // `cursor_id`/`checkpoint_store` are scalar fields.
    #[test]
    fn object_store_bucket_schemes() {
        let cfg = config(
            "s3://my-bucket/events?cursor_id=replayer&checkpoint_store=file:///tmp/c.json",
            "object_store",
        );
        assert_eq!(cfg["url"], "s3://my-bucket/events");
        assert_eq!(cfg["cursor_id"], "replayer");
        assert_eq!(cfg["checkpoint_store"], "file:///tmp/c.json");

        assert_eq!(config("gs://b/p", "object_store")["url"], "gs://b/p");
        assert_eq!(config("az://b/p", "object_store")["url"], "az://b/p");
        assert_eq!(config("gcs://b/p", "object_store")["url"], "gs://b/p");
        assert_eq!(
            config("local-store:///var/lib/mqb/incoming", "object_store")["url"],
            "file:///var/lib/mqb/incoming"
        );
    }

    #[test]
    fn object_store_name_by_is_a_scalar_flag() {
        let cfg = config(
            "s3://my-bucket/events?name_by=source_position",
            "object_store",
        );
        assert_eq!(cfg["name_by"], "source_position");
    }

    /// `idempotency` is the deprecated spelling of `name_by`; v0.4.x URLs must keep working.
    #[test]
    fn object_store_idempotency_is_still_accepted() {
        let cfg = config("s3://my-bucket/events?idempotency=true", "object_store");
        assert_eq!(cfg["idempotency"], true);
    }

    // `ws://`/`wss://` pass through unchanged.
    #[test]
    fn websocket_scheme_passthrough() {
        let cfg = config("ws://0.0.0.0:9000", "websocket");
        assert_eq!(cfg["url"], "ws://0.0.0.0:9000/");
    }

    // `grpc://` is rewritten to `http://` (the client-mode URL GrpcConfig
    // expects); `topic` is a scalar config field.
    #[test]
    fn grpc_scheme_rewrites_to_http() {
        let cfg = config("grpc://localhost:50051?topic=orders", "grpc");
        assert_eq!(cfg["url"], "http://localhost:50051");
        assert_eq!(cfg["topic"], "orders");
    }

    // The documented lake-export target. `format` decides whether rows are written
    // as themselves or wrapped in a stringified envelope, so it has to survive the
    // URI rather than falling back to the sink's `normal` default; `extension` and
    // `compression` ride alongside it as scalar config fields.
    #[test]
    fn object_store_target_carries_format_extension_and_compression() {
        let cfg = config(
            "s3://lake/orders?format=raw&extension=jsonl.zst&compression=zstd",
            "object_store",
        );
        assert_eq!(cfg["url"], "s3://lake/orders");
        assert_eq!(cfg["format"], "raw");
        assert_eq!(cfg["compression"], "zstd");
        // Spelled out including the compression suffix on purpose: overriding
        // `extension` replaces the derived name wholesale, so `extension=jsonl`
        // beside `compression=zstd` would name a zstd object `.jsonl`.
        assert_eq!(cfg["extension"], "jsonl.zst");
    }

    // `ibmmq://` is reformatted to the driver's `host(port)` connection string;
    // `queue_manager` and `channel` are required scalar config fields.
    #[test]
    fn ibmmq_scheme_reformats_host_port() {
        let cfg = config(
            "ibmmq://qmhost:1414?queue_manager=QM1&channel=DEV.APP.SVRCONN&queue=orders",
            "ibmmq",
        );
        assert_eq!(cfg["url"], "qmhost(1414)");
        assert_eq!(cfg["queue_manager"], "QM1");
        assert_eq!(cfg["channel"], "DEV.APP.SVRCONN");
        assert_eq!(cfg["queue"], "orders");
    }

    // AWS SQS/SNS has no connection URL: the placeholder authority is dropped,
    // and `queue_url`/`region` are scalar config fields set via query params.
    #[test]
    fn aws_scheme_has_no_url_field() {
        let cfg = config(
            "aws://_/?queue_url=https://sqs.us-east-1.amazonaws.com/123/orders&region=us-east-1",
            "aws",
        );
        assert!(
            cfg.get("url").is_none(),
            "aws config should have no url field, got {cfg}"
        );
        assert_eq!(
            cfg["queue_url"],
            "https://sqs.us-east-1.amazonaws.com/123/orders"
        );
        assert_eq!(cfg["region"], "us-east-1");
    }

    // `zeromq://`/`zmq://` are rewritten to `tcp://`, the transport ZeroMQ expects.
    #[test]
    fn zeromq_scheme_rewrites_to_tcp() {
        let cfg = config("zeromq://127.0.0.1:5555?socket_type=push", "zeromq");
        assert_eq!(cfg["url"], "tcp://127.0.0.1:5555");
        assert_eq!(cfg["socket_type"], "push");

        let cfg = config("zmq://127.0.0.1:5555", "zeromq");
        assert_eq!(cfg["url"], "tcp://127.0.0.1:5555");

        // The form the book documents: the transport address after the scheme.
        let cfg = config(
            "zeromq://tcp://127.0.0.1:5555?socket_type=pull&bind=true",
            "zeromq",
        );
        assert_eq!(cfg["url"], "tcp://127.0.0.1:5555");
        assert_eq!(cfg["bind"], true);
        let cfg = config("zmq://ipc:///tmp/mqb.sock", "zeromq");
        assert_eq!(cfg["url"], "ipc:///tmp/mqb.sock");
    }

    // `consume: capture_all` is the documented way to back fill a CDC source, so it has to
    // be reachable from a URI on both CDC endpoints.
    #[test]
    fn both_cdc_endpoints_take_consume_capture_all_from_a_uri() {
        let cfg = config(
            "postgres-cdc://u:p@host:5432/db?publication=mqb_pub&consume=capture_all",
            "postgres_cdc",
        );
        assert_eq!(cfg["consume"], "capture_all");

        // Mongo takes the database as a query param; the path stays part of the connection URL.
        let cfg = config(
            "mongodb://host:27017/?database=shop&collection=orders&consume=capture_all",
            "mongodb",
        );
        assert_eq!(cfg["consume"], "capture_all");
    }

    /// An unknown scheme is often a plugin that is not installed yet, so the error
    /// has to name the file to install and where it looked — listing the built-in
    /// schemes alone sends the reader looking for a typo that is not there.
    #[test]
    fn an_unknown_scheme_names_the_plugin_file_it_looked_for() {
        let error = endpoint_from_uri("nosuchthing://host/orders")
            .map(|_| ())
            .expect_err("an unknown scheme cannot resolve");
        let message = format!("{error:#}");

        assert!(message.contains("nosuchthing"), "{message}");
        assert!(
            message.contains(&super::mq_bridge::plugin::library_file_name("nosuchthing")),
            "{message}"
        );
    }
}

#[cfg(test)]
mod wait_flag_tests {
    use super::*;
    use clap::Parser;

    fn copy_args(argv: &[&str]) -> CopyArgs {
        let args = Args::try_parse_from(argv).expect("arguments should parse");
        match args.command {
            Some(Command::Copy(copy)) => copy,
            _ => panic!("expected the copy subcommand"),
        }
    }

    // A source another process is still filling drains empty on the first look,
    // so `--wait` has to keep the drain alive rather than start a bridge.
    #[test]
    fn wait_implies_drain() {
        assert!(!drains(&copy_args(&["mqb", "copy", "a://b", "c://d"])));
        assert!(drains(&copy_args(&[
            "mqb", "copy", "a://b", "c://d", "--drain"
        ])));
        assert!(drains(&copy_args(&[
            "mqb", "copy", "a://b", "c://d", "--wait", "30"
        ])));
    }

    // `--wait 0` is the SQS reading: a ceiling of zero waits not at all. It still
    // drains, so it stays equivalent to a bare `--drain` rather than becoming a
    // continuous bridge.
    #[test]
    fn a_zero_wait_still_drains() {
        let args = copy_args(&["mqb", "copy", "a://b", "c://d", "--wait", "0"]);
        assert_eq!(args.wait, Some(0));
        assert!(drains(&args));
    }

    #[test]
    fn agent_listen_defaults_to_an_hour() {
        let args =
            Args::try_parse_from(["mqb", "agent-listen", "bob"]).expect("arguments should parse");
        let Some(Command::AgentListen(listen)) = args.command else {
            panic!("expected the agent-listen subcommand")
        };
        assert_eq!(listen.name, "bob");
        assert_eq!(listen.wait, 3600);
        assert!(listen.to.is_none());
    }
}
