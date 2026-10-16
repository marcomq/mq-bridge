//  mq-bridge-app
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use mq_bridge_app::{
    config::{AppConfig, config_file_path, load_config},
    copy_pipeline,
    endpoint_uri::{endpoint_from_uri, make_listen_address},
    mq_bridge,
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

mod catalog_cmd;
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
    let name = match &output.endpoint_type {
        mq_bridge::models::EndpointType::Custom { name, .. } => name,
        named => named.name(),
    };
    mq_bridge::extensions::endpoint_default_batch_size(name).unwrap_or(DEFAULT_BATCH_SIZE)
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

    /// List every endpoint scheme and middleware a URI can name.
    List(catalog_cmd::ListArgs),

    /// Show how to write one endpoint or middleware: URI form, example, params.
    Describe(catalog_cmd::DescribeArgs),
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
    ///
    /// `mqb list` shows every scheme and middleware, `mqb describe <name>` its params.
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
        Some(Command::List(list_args)) => {
            init_copy_logging(args.color, false);
            load_cli_plugins(&args.plugins)?;
            return catalog_cmd::list(list_args);
        }
        Some(Command::Describe(describe_args)) => {
            init_copy_logging(args.color, false);
            load_cli_plugins(&args.plugins)?;
            return catalog_cmd::describe(describe_args);
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

/// As `shutdown_requested`, and also once the reader of stdout is gone.
async fn copy_interrupted() {
    tokio::select! {
        _ = shutdown_requested() => {}
        _ = async {
            while !STDOUT_CLOSED.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(COPY_POLL_INTERVAL).await;
            }
        } => {}
    }
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
    let copy_id = copy_pipeline::checkpoint_identity(&input, &output, args.filter.as_deref())?;
    let resume = if args.resume {
        Some(copy_pipeline::configure_resume(
            &mut input,
            &output,
            args.filter.as_deref(),
        )?)
    } else {
        None
    };
    // After `configure_resume`, which derives its id from the endpoints as given.
    copy_pipeline::name_dedup_tables(&mut input, &copy_id);
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
                _ = copy_interrupted() => info!("Shutdown requested; stopping listen"),
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
                _ = copy_interrupted() => info!("Shutdown requested; stopping copy"),
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
            _ = copy_interrupted() => {
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
                _ = copy_interrupted() => {
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
    if STDOUT_CLOSED.load(std::sync::atomic::Ordering::Relaxed) {
        anyhow::bail!("stdout was closed; stopped after {}", moved.rows_display());
    }
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

/// `RUST_LOG` if set, else `level`. The AMQP client logs each returned message with its
/// whole payload at `warn`; the bridge reports the return itself, so that target is quiet.
fn default_log_filter(level: &str) -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{level},lapin::returned_messages=error")))
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
    let env_filter = default_log_filter(default);
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
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_writer(writer)
        .with_ansi(color.enabled(is_terminal, no_color_requested()))
        .finish()
        .with(ClosedStdoutLayer)
        .try_init();
}

/// Set once a copy to stdout hit a closed pipe (`mqb copy … - | head`).
static STDOUT_CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Drops the engine's lines about a closed stdout; `copy_result` reports it in one.
struct ClosedStdoutLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ClosedStdoutLayer {
    fn event_enabled(
        &self,
        event: &tracing::Event<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        use std::sync::atomic::Ordering::Relaxed;

        struct BrokenPipe(bool);
        impl tracing::field::Visit for BrokenPipe {
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0 |= format!("{value:?}").contains("Broken pipe");
            }
        }

        if STDOUT_CLOSED.load(Relaxed) {
            return false;
        }
        if !SUMMARY_TO_STDERR.load(Relaxed) {
            return true;
        }
        let mut found = BrokenPipe(false);
        event.record(&mut found);
        if found.0 {
            STDOUT_CLOSED.store(true, Relaxed);
        }
        !found.0
    }
}

/// Logging for the `mcp` subcommand. Writes to **stderr** because the `stdio`
/// transport uses stdout as the MCP (JSON-RPC) channel — logging there would
/// corrupt the protocol stream.
fn init_mcp_logging(color: ColorChoice) {
    use std::io::IsTerminal;

    let env_filter = default_log_filter("info");
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

    let env_filter = default_log_filter(&config.log_level);

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
