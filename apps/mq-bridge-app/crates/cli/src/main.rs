//  mq-bridge-app
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use mq_bridge_app::{
    config::{AppConfig, config_file_path, load_config},
    copy_pipeline, mq_bridge,
    status_registry::{
        InstanceKind, StatusEntity, StatusLease, StatusRoute, StatusSnapshot, StatusSummary,
        endpoint_type_label,
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

mod mcp;
mod mcp_install;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// App-level default batch size for headless routes (`copy`, MCP) when the caller
/// does not specify one. The library's `RouteOptions::default()` is 512; this is the
/// bulk-move value the app applies on top.
pub(crate) const DEFAULT_BATCH_SIZE: usize = 1024;

/// App-level default route concurrency for headless routes (`copy`, MCP) when the
/// caller does not specify one. See [`DEFAULT_BATCH_SIZE`].
pub(crate) const DEFAULT_CONCURRENCY: usize = 4;

/// How often `copy --drain` checks whether the route task has ended. The engine
/// exposes completion as a poll, not a notification; see [`run_copy`].
const COPY_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Address the web UI falls back to when the config names none. Only ever
/// applied after an explicit `--ui` or a `y` from [`ui_prompt`].
const DEFAULT_UI_ADDR: &str = "0.0.0.0:9091";

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
    /// own name, usable in routes like any built-in one. Also loadable from the
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

    /// Print a copyable, self-contained command for the loaded YAML/JSON config.
    /// Credential values are replaced by environment-variable placeholders.
    ToCli,
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
    #[arg(long, value_name = "SOURCE", conflicts_with = "source")]
    from: Option<String>,

    /// Destination endpoint URI (same URI and middleware forms as `--from`), e.g.
    /// `postgres://user:pass@host/db?table=dst&insert_query=<url-encoded SQL>`.
    #[arg(long, value_name = "TARGET", conflicts_with = "target")]
    to: Option<String>,

    /// Source endpoint URI in the positional `copy SOURCE TARGET` form.
    #[arg(value_name = "SOURCE", index = 1, conflicts_with = "from")]
    source: Option<String>,

    /// Destination endpoint URI in the positional `copy SOURCE TARGET` form.
    #[arg(value_name = "TARGET", index = 2, conflicts_with = "to")]
    target: Option<String>,

    /// Only copy messages for which EXPR evaluates to true.
    ///
    /// Expressions address top-level JSON fields directly, for example
    /// `amount > 100` or `country == "DE" && amount >= 50`.
    #[arg(long, value_name = "EXPR")]
    filter: Option<String>,

    /// Resume from the last successfully processed position.
    ///
    /// The source's native cursor, offset, slot, or checkpoint mechanism is used.
    /// Fails before starting when the source cannot resume safely.
    #[arg(long)]
    resume: bool,

    /// Exit once the source yields an empty batch (drain-then-exit). Without it,
    /// `copy` keeps running like a continuous bridge until Ctrl-C.
    #[arg(long)]
    drain: bool,

    /// Route concurrency (defaults to 4).
    #[arg(long)]
    concurrency: Option<usize>,

    /// Batch size (defaults to 1024).
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
    mq_bridge_app::plugins::load_trusted_plugins(paths, &std::collections::HashMap::new())?;
    Ok(())
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
            return run_copy(copy_args).await;
        }
        Some(Command::Mcp(mcp_args)) => {
            // The install actions configure clients and exit; only the bare
            // `mcp` command actually serves.
            match mcp_args.action {
                Some(McpAction::Install {
                    client,
                    local,
                    print_config,
                }) => {
                    return if print_config {
                        mcp_install::print_config()
                    } else {
                        mcp_install::install(client, local)
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
                workspace_path,
            )
            .await;
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
    } else if config.ui_addr.is_empty()
        && (args.ui || (nothing_to_run(&config) && ui_prompt(DEFAULT_UI_ADDR)))
    {
        config.ui_addr = DEFAULT_UI_ADDR.to_string();
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
    let _cli_status = config
        .ui_addr
        .is_empty()
        .then(|| cli_status_lease(config_file_path.clone(), config.clone()))
        .flatten();

    // Start Web UI
    // Headless, this owns the consumers it started: dropping the app stops their
    // routes, so it has to live until shutdown. With a UI, `start_web_server`
    // owns them for as long as it serves.
    let mut headless_app = None;
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

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Ctrl+C (SIGINT) received.");
        },
        _ = platform_specific_shutdown() => {
                info!("Shutdown signal received.");
        },
    }

    info!("Shutdown signal received. Broadcasting to all tasks...");

    // Dropping the app releases its route handles without stopping the underlying
    // routes; the `stop_route` loop below performs shutdown.
    drop(headless_app);

    let shutdown_task = async {
        let routes = mq_bridge::list_routes();
        if !routes.is_empty() {
            info!("Attempting to gracefully stop {} routes...", routes.len());
            for name in routes {
                mq_bridge::stop_route(&name).await;
            }
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

    info!("Shutdown complete.");

    Ok(())
}

/// Advertises a headless run: the configured entities, with running state read
/// from the live route registry rather than assumed.
fn cli_status_lease(workspace_path: String, config: AppConfig) -> Option<StatusLease> {
    let heartbeat_config = config.clone();
    StatusLease::spawn(
        InstanceKind::Cli,
        env!("CARGO_PKG_VERSION"),
        &workspace_path,
        move || {
            let config = heartbeat_config.clone();
            async move {
                let running = mq_bridge::list_routes();
                let is_running = |name: &str| running.iter().any(|route| route == name);
                StatusSnapshot {
                    consumers: config
                        .consumers
                        .iter()
                        .map(|consumer| {
                            let id = consumer_runtime_key(consumer);
                            let running = is_running(&collector_route_name(&id));
                            StatusEntity {
                                label: consumer.name.clone(),
                                endpoint: endpoint_type_label(&consumer.endpoint.endpoint_type)
                                    .to_string(),
                                summary: StatusSummary {
                                    running,
                                    healthy: running,
                                    ..Default::default()
                                },
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
/// Runs the `copy` subcommand: builds a single in-memory route from the `--from`
/// and `--to` URIs and awaits its completion. With `--drain` the underlying route
/// exits once the source is empty; otherwise it runs until Ctrl-C.
async fn run_copy(args: CopyArgs) -> anyhow::Result<()> {
    use mq_bridge::models::{Route, RouteOptions};
    use mq_bridge::route::RouteOutcome;

    let (from, to) = copy_endpoints(&args)?;
    // Expanded here rather than by the shell, so a single-quoted URI can name a
    // credential without it ever appearing in the history or in `argv`.
    let from = copy_pipeline::expand_uri_variables(from).context("invalid copy source endpoint")?;
    let to =
        copy_pipeline::expand_uri_variables(to).context("invalid copy destination endpoint")?;
    let mut input = endpoint_from_uri(&from).context("invalid copy source endpoint")?;
    make_listen_address(&mut input).context("invalid copy source endpoint")?;
    let output = endpoint_from_uri(&to).context("invalid copy destination endpoint")?;
    let resume = if args.resume {
        Some(copy_pipeline::configure_resume(
            &mut input,
            &output,
            args.filter.as_deref(),
        )?)
    } else {
        None
    };
    // Attached outside every other source middleware, so it sees only what the
    // whole chain let through — past the filter below and past any URI-configured
    // `transform` that rejects rows: what was copied.
    let copied = copy_pipeline::configure_delivered_counter(&mut input)?;
    if let Some(expression) = &args.filter {
        copy_pipeline::configure_filter(&mut input, expression);
    }
    // Innermost, so it tallies everything the source produced. The rate is derived
    // from this: a selective filter reduces what lands at the destination without
    // making the copy any slower, and rating the surviving rows against the time
    // spent reading every row reports that as a slowdown.
    let read = copy_pipeline::configure_counter(&mut input)?;
    let input_endpoint_label = endpoint_type_label(&input.endpoint_type);
    let output_endpoint_label = endpoint_type_label(&output.endpoint_type);
    let options = RouteOptions {
        concurrency: args.concurrency.unwrap_or(DEFAULT_CONCURRENCY),
        batch_size: args.batch_size.unwrap_or(DEFAULT_BATCH_SIZE),
        exit_on_empty: args.drain,
        ..Default::default()
    };

    let route = Route::new(input, output).with_options(options);
    let run_id = format!("copy-{}", uuid::Uuid::new_v4());
    let started = std::time::Instant::now();
    let handle = Arc::new(
        route
            .run(&run_id)
            .await
            .context("failed to start copy route")?,
    );
    let copy_status = copy_status_lease(
        run_id,
        input_endpoint_label.to_string(),
        output_endpoint_label.to_string(),
        handle.clone(),
    );

    info!(
        // Redacted: this line is the one that reaches journald, Docker logs and CI.
        from = %copy_pipeline::redact_uri(&from),
        to = %copy_pipeline::redact_uri(&to),
        filtered = args.filter.is_some(),
        // Names the mechanism, not just the flag: which one the source picked
        // is what tells you where a restart will actually pick up from.
        resume = resume.map_or("off", copy_pipeline::ResumeCapability::as_str),
        drain = args.drain,
        "copy route started"
    );

    let result = if args.drain {
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
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received; aborting copy");
                None
            }
        };

        // Interrupted: shut the route down the same way the continuous branch does,
        // so the source connection and any checkpoint are released before we exit.
        if outcome.is_none() {
            handle.stop().await;
        }

        copy_result(
            outcome,
            handle.status().error,
            &throughput(&copied, &read, started),
        )
    } else {
        // Continuous bridge: run until Ctrl-C, then stop gracefully.
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl+C")?;
        info!("Ctrl+C received; stopping copy");
        handle.stop().await;
        // Through the same reporting as the drained branch: a bridge that dropped
        // rows did not run clean either, and a supervisor restarting it needs to
        // hear that from the exit status. `stop` resolves the outcome, so the
        // fallback only guards against the summary going missing.
        copy_result(
            handle.outcome().or(Some(RouteOutcome::Stopped)),
            handle.status().error,
            &throughput(&copied, &read, started),
        )
    };

    drop(copy_status);

    result
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
) -> Option<StatusLease> {
    StatusLease::spawn(
        InstanceKind::Cli,
        env!("CARGO_PKG_VERSION"),
        "copy",
        move || {
            let handle = Arc::clone(&handle);
            let run_id = run_id.clone();
            let input_endpoint = input_endpoint.clone();
            let output_endpoint = output_endpoint.clone();
            async move {
                let status = handle.status();
                let summary = StatusSummary {
                    running: handle.outcome().is_none(),
                    healthy: status.healthy,
                    error: status.error.clone(),
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
    elapsed_s: f64,
    rows_per_second: u64,
}

impl Throughput {
    /// `copied 333_495 of 1_000_000 rows` when a filter dropped some, plain
    /// `copied 1_000_000 rows` when nothing was dropped.
    fn rows_display(&self) -> String {
        if self.read > self.rows {
            format!("{} of {} rows", grouped(self.rows), grouped(self.read))
        } else {
            format!("{} rows", grouped(self.rows))
        }
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
        println!(
            "{verb} {} in {} ({} rows/s)",
            self.rows_display(),
            self.elapsed_display(),
            self.rate_display()
        );
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
    let rows_per_second = if elapsed_s > 0.0 {
        (read as f64 / elapsed_s).round() as u64
    } else {
        0
    };
    Throughput {
        rows,
        read,
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
    let mut parts = uri.split('|');
    let base = parts.next().unwrap_or(uri);
    let mut endpoint = base_endpoint_from_uri(base)?;
    for spec in parts {
        endpoint.middlewares.push(
            middleware_from_spec(spec)
                .with_context(|| format!("invalid middleware '{spec}' in '{uri}'"))?,
        );
    }
    Ok(endpoint)
}

fn nested_endpoint(
    key: &str,
    value: &str,
    outer: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    endpoint_from_uri(value)
        .with_context(|| format!("invalid '{key}' endpoint '{value}' in '{outer}'"))
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

/// Builds a middleware from a `name` / `name?param=value&...` spec. Params are
/// the middleware config struct's own fields, coerced to the type the field
/// expects; `dlq`'s `endpoint` is itself an endpoint URI, and object/array
/// fields (e.g. `weak_join`'s `required`) take a JSON literal.
fn middleware_from_spec(spec: &str) -> anyhow::Result<mq_bridge::models::Middleware> {
    use anyhow::bail;
    use mq_bridge::models::{
        BufferMiddleware, CompressionMiddleware, CookieJarMiddleware, DeadLetterQueueMiddleware,
        DeduplicationMiddleware, DelayMiddleware, EncryptionConfig, LimiterMiddleware,
        MetricsMiddleware, RandomPanicMiddleware, RetryMiddleware, TransformMiddleware,
        WeakJoinMiddleware,
    };
    use std::collections::HashMap;

    let (name, query) = match spec.split_once('?') {
        Some((name, query)) => (name.trim(), query),
        None => (spec.trim(), ""),
    };
    // An underscore is awkward to type in a shell-quoted URI, so `-` is accepted
    // as well (`weak-join` == `weak_join`).
    let tag = name.replace('-', "_");

    let fields: HashMap<String, FieldType> = match tag.as_str() {
        "deduplication" => schema_fields(schemars::schema_for!(DeduplicationMiddleware)),
        "metrics" => schema_fields(schemars::schema_for!(MetricsMiddleware)),
        "dlq" => schema_fields(schemars::schema_for!(DeadLetterQueueMiddleware)),
        "retry" => schema_fields(schemars::schema_for!(RetryMiddleware)),
        "random_panic" => schema_fields(schemars::schema_for!(RandomPanicMiddleware)),
        "delay" => schema_fields(schemars::schema_for!(DelayMiddleware)),
        "weak_join" => schema_fields(schemars::schema_for!(WeakJoinMiddleware)),
        "limiter" => schema_fields(schemars::schema_for!(LimiterMiddleware)),
        "buffer" => schema_fields(schemars::schema_for!(BufferMiddleware)),
        "cookie_jar" => schema_fields(schemars::schema_for!(CookieJarMiddleware)),
        "transform" => schema_fields(schemars::schema_for!(TransformMiddleware)),
        "encryption" => schema_fields(schemars::schema_for!(EncryptionConfig)),
        "compression" => schema_fields(schemars::schema_for!(CompressionMiddleware)),
        // The escape hatch for a handler-provided middleware: `name` selects it,
        // `config` carries its free-form JSON.
        "custom" => HashMap::from([
            ("name".to_string(), FieldType::StringLike),
            ("config".to_string(), FieldType::Object),
        ]),
        other => bail!(
            "unsupported middleware '{other}'. Supported middlewares: deduplication, metrics, dlq, retry, random_panic, delay, weak_join, limiter, buffer, cookie_jar, transform, encryption, compression, custom"
        ),
    };

    let mut config = serde_json::Map::new();
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        let (k, v) = (k.into_owned(), v.into_owned());
        let value = if tag == "dlq" && k == "endpoint" {
            let endpoint =
                endpoint_from_uri(&v).with_context(|| format!("invalid dlq endpoint '{v}'"))?;
            serde_json::to_value(endpoint)?
        } else {
            match fields.get(&k).copied() {
                // A known object/array field must be a JSON literal; a value that
                // doesn't parse is a user error worth naming, the same way
                // `base_endpoint_from_uri` handles its object fields, rather than
                // a silent fallback to a string that serde rejects later.
                Some(FieldType::Object) => serde_json::from_str(&v).with_context(|| {
                    format!(
                        "query param '{k}' in middleware spec '{spec}' expects a JSON literal, got '{v}'"
                    )
                })?,
                // An unknown field (`None`) is passed through for serde to reject
                // by name.
                None => serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v)),
                Some(ty) => coerce_scalar(v, ty),
            }
        };
        config.insert(k, value);
    }

    let mut tagged = serde_json::Map::new();
    tagged.insert(tag.clone(), serde_json::Value::Object(config));
    serde_json::from_value(serde_json::Value::Object(tagged))
        .with_context(|| format!("could not build a '{tag}' middleware from '{spec}'"))
}

/// Builds a `custom` endpoint for a scheme that names a registered factory.
///
/// The factory owns its config shape and the CLI has no schema for it, so the
/// mapping stays literal: `url` is the URI up to the query (the plugin defined
/// that format, so it is passed through rather than normalised) and every query
/// param becomes a string field. A `?url=` param overrides the derived one.
/// Values stay strings — guessing types here would silently turn an id like
/// `0123` into a number; a factory needing typed fields takes them from config.
fn custom_endpoint_from_uri(
    name: &str,
    parsed: &url::Url,
    uri: &str,
) -> anyhow::Result<mq_bridge::models::Endpoint> {
    use mq_bridge::models::{Endpoint, EndpointType};

    let mut config = serde_json::Map::new();
    let mut url = uri.split('?').next().unwrap_or(uri).to_string();
    for (key, value) in parsed.query_pairs() {
        if key == "url" {
            url = value.into_owned();
        } else {
            config.insert(
                key.into_owned(),
                serde_json::Value::String(value.into_owned()),
            );
        }
    }
    config.insert("url".into(), serde_json::Value::String(url));

    Ok(Endpoint::new(EndpointType::Custom {
        name: name.to_string(),
        config: serde_json::Value::Object(config),
    }))
}

fn base_endpoint_from_uri(uri: &str) -> anyhow::Result<mq_bridge::models::Endpoint> {
    use anyhow::bail;
    use mq_bridge::models::{
        AmqpConfig, AwsConfig, ClickHouseConfig, Endpoint, EndpointType, FileConfig, GrpcConfig,
        HttpConfig, IbmMqConfig, KafkaConfig, MongoDbConfig, MqttConfig, NatsConfig,
        ObjectStoreConfig, PostgresCdcConfig, RedisStreamsConfig, SqlxConfig, WebSocketConfig,
        ZeroMqConfig,
    };
    use std::collections::HashMap;
    use url::Url;

    let parsed = Url::parse(uri).with_context(|| format!("not a valid URI: {uri}"))?;

    // Endpoints without a connection URL are built directly — they don't fit the
    // scalar-field-routing path below (which always attaches a `url`).
    match parsed.scheme() {
        // A sink that discards everything. `null:` (any trailing content ignored).
        "null" => return Ok(Endpoint::new(EndpointType::Null)),
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
                    format!("could not build a 'static' endpoint from URI '{uri}'")
                })?;
            return Ok(Endpoint::new(endpoint_type));
        }
        // `response:` — replies to the caller; needs an input with a reply channel.
        "response" => {
            if let Some((key, _)) = parsed.query_pairs().next() {
                anyhow::bail!(
                    "unsupported query param '{key}' in response URI '{uri}'. Response takes no query parameters"
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
                        "unsupported query param '{other}' in fanout URI '{uri}'. Use 'to=<uri>' for a branch that may answer, 'mirror=<uri>' for one whose response and failures are discarded"
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
                    "fanout URI '{uri}' has no branches. Add at least one 'to=<uri>' or 'mirror=<uri>'"
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
                        "unsupported query param '{other}' in request URI '{uri}'. Supported: to, forward_to"
                    ),
                };
                if slot.is_some() {
                    anyhow::bail!("duplicate query param '{key}' in request URI '{uri}'");
                }
                *slot = Some(Box::new(nested_endpoint(&key, &value, uri)?));
            }
            let Some(to) = to else {
                anyhow::bail!("request URI '{uri}' needs a 'to=<uri>' endpoint to send to");
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
                                "switch URI '{uri}' has 'when={previous}' with no 'to=<uri>' after it"
                            );
                        }
                    }
                    "to" => {
                        let Some(condition) = condition.take() else {
                            anyhow::bail!(
                                "switch URI '{uri}' has a 'to={value}' that no 'when=<expression>' precedes"
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
                        "unsupported query param '{other}' in switch URI '{uri}'. Supported: metadata_key, case.<value>=<uri>, when=<expression> with to=<uri>, default=<uri>"
                    ),
                }
            }
            if let Some(dangling) = condition {
                anyhow::bail!(
                    "switch URI '{uri}' has 'when={dangling}' with no 'to=<uri>' after it"
                );
            }
            // The two modes differ in cost, not just spelling, so mixing them
            // would hide which one a message actually took.
            if !when.is_empty() && (metadata_key.is_some() || !cases.is_empty()) {
                anyhow::bail!(
                    "switch URI '{uri}' mixes both modes. Use either 'metadata_key=<key>' with 'case.<value>=<uri>', or 'when=<expression>' with 'to=<uri>'"
                );
            }
            if when.is_empty() {
                if metadata_key.is_none() {
                    anyhow::bail!(
                        "switch URI '{uri}' needs a 'metadata_key=<key>' to branch on, or 'when=<expression>&to=<uri>' predicates"
                    );
                }
                if cases.is_empty() {
                    anyhow::bail!("switch URI '{uri}' has no cases. Add 'case.<value>=<uri>'");
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
                anyhow::bail!("memory URI '{uri}' must include a topic, e.g. memory://my-topic");
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
                        "unrecognised query param '{other}' in memory URI '{uri}': only 'capacity' and 'subscribe_mode' are supported"
                    ),
                }
            }
            let mut tagged = serde_json::Map::new();
            tagged.insert("memory".into(), serde_json::Value::Object(cfg));
            let endpoint_type: EndpointType =
                serde_json::from_value(serde_json::Value::Object(tagged)).with_context(|| {
                    format!("could not build a 'memory' endpoint from URI '{uri}'")
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
        // Cloud object storage. The bucket scheme both selects the endpoint and is
        // the connection URL the `object_store` crate expects, so it is not
        // rewritten below; credentials come from the environment.
        "s3" | "s3a" | "gs" | "gcs" | "az" | "azure" | "abfs" | "abfss" => (
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
        // (`pulsar`) or loaded with `--plugin` — is that endpoint. This mirrors the
        // config path, where an unknown single key falls back to `custom`.
        other if mq_bridge::extensions::get_endpoint_factory(other).is_some() => {
            return custom_endpoint_from_uri(other, &parsed, uri);
        }
        other => bail!(
            "unsupported endpoint scheme '{other}' in URI '{uri}'. Supported schemes: postgres, postgresql, mysql, mariadb, sqlite, nats, mongodb, redis, file, kafka, mqtt, mqtts, amqp, amqps, rabbitmq, rabbitmqs, http, https, clickhouse, clickhouses, ws, wss, grpc, grpcs, ibmmq, aws, zeromq, zmq, s3, gs, az, abfs, and the structural memory, null, static, fanout, request, switch, response. A scheme may also name an endpoint registered by an extension (pulsar) or loaded with --plugin"
        ),
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
    for (k, v) in parsed.query_pairs() {
        let (k, v) = (k.into_owned(), v.into_owned());
        // A file endpoint takes its path from the URI itself, so a `?path=` would
        // be a second, conflicting source for the same field.
        if k == "path" && tag == "file" {
            continue;
        }
        if k == "url" && tag != "file" {
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
                    format!("query param '{k}' in URI '{uri}' expects a JSON literal, got '{v}'")
                })?;
                config.insert(k, value);
            }
            Some(FieldType::Object) | None if driver_options => driver_params.push((k, v)),
            None => bail!(
                "unrecognised query param '{k}' in URI '{uri}': a '{tag}' endpoint has no connection-URL driver options, so '{k}' would have no effect"
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

    if tag == "file" {
        let path = uri.split('?').next().unwrap_or(uri);
        let path = path.strip_prefix("file://").unwrap_or(path);
        if path.is_empty() {
            bail!("file URI '{uri}' must include a path");
        }
        config.insert("path".into(), serde_json::Value::String(path.to_string()));
    } else if let Some(url) = escaped_url {
        // Escaped mode: the connection string is authoritative and complete, so
        // any leftover non-config param is ambiguous — it belongs inside `url=`.
        if let Some((k, _)) = driver_params.first() {
            bail!(
                "in escaped mode (url=...), put driver options inside the encoded connection string; unexpected query param '{k}' in URI '{uri}'"
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
            // `object_store` only recognizes `gs://` for GCS, not the `gcs://` alias.
            "object_store" => &[("gcs://", "gs://")],
            _ => &[],
        };
        for (prefix, replacement) in rewrites {
            if let Some(rest) = url.strip_prefix(prefix) {
                url = format!("{replacement}{rest}");
                break;
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
        .with_context(|| format!("could not build a '{tag}' endpoint from URI '{uri}'"))?;
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

    let default = if verbose { "info" } else { "warn" };
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_ansi(color.enabled(std::io::stdout().is_terminal(), no_color_requested()))
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

/// Waits for a platform-specific shutdown signal.
/// On Unix, this is SIGTERM. On other platforms, it's a future that never completes.
async fn platform_specific_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                use tracing::info;

                stream.recv().await;
                info!("SIGTERM received.");
            }
            Err(e) => {
                warn!(
                    "Failed to install SIGTERM handler: {}. This signal will be ignored.",
                    e
                );
                // If we can't listen for the signal, pend forever.
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    // On non-unix, ctrl_c is the primary mechanism. This future never completes.
    std::future::pending::<()>().await
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
    // example URIs from README.md, dev/docs/ and benches/etl/.
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

    // An unknown middleware name is rejected with the supported list.
    #[test]
    fn unknown_middleware_is_rejected() {
        let err = endpoint_from_uri("null:|bogus").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unsupported middleware 'bogus'"), "got: {msg}");
    }

    // A scheme naming a registered endpoint builds `custom`, so `copy` can
    // address an extension or `--plugin` endpoint the URI parser knows nothing
    // about. `pulsar` is compiled in, so registering it is enough to exercise it.
    #[test]
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

    #[test]
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

    // Bucket schemes select the `object_store` endpoint and are also the connection
    // URL the crate expects, so they pass through unrewritten; `gcs://` is the one
    // alias normalised (to `gs://`). `cursor_id`/`checkpoint_store` are scalar fields.
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
    }
}
