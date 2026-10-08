//! `mqb status`: a read-only view of the local status registry.

use mq_bridge_app::status_registry::{
    InstanceKind, InstanceStatus, LocalStatusRegistry, StatusSummary, list_off_thread, now_ms,
};
use mq_bridge_app::ui_app::RouteOutcomeSnapshot;
use std::io::IsTerminal;
use std::time::Duration;

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// Redraw every SECS seconds until Ctrl-C (1 when no value is given).
    ///
    /// On a terminal the table already redraws every second; this sets the
    /// interval, or turns redrawing on for `--json` and piped output.
    #[arg(long, value_name = "SECS", num_args = 0..=1, default_missing_value = "1")]
    watch: Option<u64>,

    /// Print once and exit.
    #[arg(long, conflicts_with = "watch")]
    no_watch: bool,

    /// Print the instance records as JSON instead of a table.
    ///
    /// With `--watch`, one compact JSON array per line.
    #[arg(long)]
    json: bool,
}

/// Watching is the default only for a table on a terminal, so a pipe or
/// `--json` in a script still ends.
fn watch_interval(args: &StatusArgs, terminal: bool) -> Option<u64> {
    if args.no_watch {
        return None;
    }
    args.watch.or((terminal && !args.json).then_some(1))
}

pub async fn run(args: StatusArgs, color: crate::ColorChoice) -> anyhow::Result<()> {
    let registry = LocalStatusRegistry::new()?;
    let terminal = std::io::stdout().is_terminal();
    let color = color.enabled(terminal, crate::no_color_requested());

    let Some(secs) = watch_interval(&args, terminal) else {
        let instances = list_off_thread(&registry).await;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&instances)?);
        } else {
            print!("{}", render_table(&instances, now_ms(), color));
        }
        return Ok(());
    };

    let mut interval = tokio::time::interval(Duration::from_secs(secs.max(1)));
    loop {
        tokio::select! {
            _ = crate::shutdown_requested() => return Ok(()),
            _ = interval.tick() => {}
        }
        let instances = list_off_thread(&registry).await;
        if args.json {
            println!("{}", serde_json::to_string(&instances)?);
            continue;
        }
        // Clear and home, so the table redraws in place instead of scrolling.
        if terminal {
            print!("\x1b[2J\x1b[H");
        }
        print!("{}", render_table(&instances, now_ms(), color));
    }
}

const HEADERS: [&str; 9] = [
    "INSTANCE", "NAME", "FLOW", "STATE", "RATE", "AVG", "TOTAL", "PENDING", "UPTIME",
];
const STATE_COLUMN: usize = 3;

fn kind_label(kind: InstanceKind) -> &'static str {
    match kind {
        InstanceKind::Cli => "cli",
        InstanceKind::WebUi => "web-ui",
        InstanceKind::Mcp => "mcp",
        InstanceKind::Tauri => "desktop",
    }
}

fn state_label(summary: &StatusSummary) -> &'static str {
    match summary.outcome {
        Some(RouteOutcomeSnapshot::Completed) if summary.error.is_some() => "completed (error)",
        Some(RouteOutcomeSnapshot::Completed) => "completed",
        Some(RouteOutcomeSnapshot::Stopped) => "stopped",
        Some(RouteOutcomeSnapshot::Failed) => "failed",
        None if !summary.running => "stopped",
        None if !summary.healthy || summary.error.is_some() => "unhealthy",
        None => "running",
    }
}

fn state_color(state: &str) -> &'static str {
    match state {
        "running" | "completed" => "\x1b[32m",
        "failed" | "unhealthy" | "completed (error)" => "\x1b[31m",
        _ => "\x1b[2m",
    }
}

fn rate(value: f64) -> String {
    if value > 0.0 {
        format!("{value:.1}/s")
    } else {
        "-".to_string()
    }
}

fn uptime(started_at_ms: Option<u64>, now_ms: u64) -> String {
    let Some(started) = started_at_ms else {
        return "-".to_string();
    };
    let secs = now_ms.saturating_sub(started) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn summary_cells(summary: &StatusSummary, now_ms: u64) -> [String; 6] {
    let pending = match (summary.pending, summary.capacity) {
        (Some(pending), Some(capacity)) => format!("{pending}/{capacity}"),
        (Some(pending), None) => pending.to_string(),
        _ => "-".to_string(),
    };
    [
        state_label(summary).to_string(),
        rate(summary.throughput),
        rate(summary.average_throughput),
        summary.message_sequence.to_string(),
        pending,
        uptime(summary.started_at_ms, now_ms),
    ]
}

fn row(
    instance: &str,
    name: &str,
    flow: String,
    summary: &StatusSummary,
    now_ms: u64,
) -> Vec<String> {
    let mut cells = vec![instance.to_string(), name.to_string(), flow];
    cells.extend(summary_cells(summary, now_ms));
    cells
}

/// One row per route, plus one per consumer that is not already shown as a
/// route. Publishers are destinations, not running things, so they are left to
/// `--json`.
fn rows(instances: &[InstanceStatus], now_ms: u64) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for instance in instances {
        let label = format!(
            "{} {} [{}]",
            kind_label(instance.kind),
            instance.workspace_label,
            instance.pid
        );
        let before = rows.len();
        for route in &instance.routes {
            let flow = format!("{} → {}", route.input.endpoint, route.output.endpoint);
            rows.push(row(&label, &route.label, flow, &route.summary, now_ms));
        }
        for consumer in &instance.consumers {
            if instance.routes.iter().any(|route| route.id == consumer.id) {
                continue;
            }
            rows.push(row(
                &label,
                &consumer.label,
                consumer.endpoint.clone(),
                &consumer.summary,
                now_ms,
            ));
        }
        if rows.len() == before {
            let mut idle = vec![label, "-".to_string(), "-".to_string(), "idle".to_string()];
            idle.resize(HEADERS.len(), "-".to_string());
            rows.push(idle);
        }
    }
    rows
}

pub fn render_table(instances: &[InstanceStatus], now_ms: u64, color: bool) -> String {
    if instances.is_empty() {
        return "No running mq-bridge instances.\n".to_string();
    }
    let rows = rows(instances, now_ms);
    let mut widths: Vec<usize> = HEADERS.iter().map(|header| header.len()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let mut out = String::new();
    let mut push_line = |cells: &[String], colored: bool| {
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let padding = " ".repeat(widths[column] - cell.chars().count());
                if colored && column == STATE_COLUMN {
                    format!("{}{cell}\x1b[0m{padding}", state_color(cell))
                } else {
                    format!("{cell}{padding}")
                }
            })
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    };
    push_line(&HEADERS.map(String::from), false);
    for row in &rows {
        push_line(row, color);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mq_bridge_app::status_registry::{StatusEntity, StatusRoute};

    fn entity(id: &str, endpoint: &str, summary: StatusSummary) -> StatusEntity {
        StatusEntity {
            id: id.to_string(),
            label: id.to_string(),
            endpoint: endpoint.to_string(),
            summary,
        }
    }

    #[test]
    fn watching_is_the_default_only_for_a_table_on_a_terminal() {
        let args = |watch, no_watch, json| StatusArgs {
            watch,
            no_watch,
            json,
        };
        assert_eq!(watch_interval(&args(None, false, false), true), Some(1));
        assert_eq!(watch_interval(&args(None, true, false), true), None);
        assert_eq!(watch_interval(&args(None, false, false), false), None);
        assert_eq!(watch_interval(&args(None, false, true), true), None);
        assert_eq!(watch_interval(&args(Some(5), false, true), false), Some(5));
    }

    #[test]
    fn an_empty_registry_says_so() {
        assert_eq!(
            render_table(&[], 0, false),
            "No running mq-bridge instances.\n"
        );
    }

    #[test]
    fn a_route_is_one_row_and_its_consumer_twin_is_not_repeated() {
        let summary = StatusSummary {
            running: true,
            healthy: true,
            throughput: 1234.56,
            average_throughput: 1000.0,
            message_sequence: 5000,
            pending: Some(3),
            started_at_ms: Some(10_000),
            ..Default::default()
        };
        let mut instance = InstanceStatus::new(InstanceKind::Mcp, "1", "/tmp/work.yml");
        instance.pid = 42;
        instance
            .consumers
            .push(entity("orders", "kafka", summary.clone()));
        instance.routes.push(StatusRoute {
            id: "orders".to_string(),
            label: "orders".to_string(),
            input: entity("orders:input", "kafka", summary.clone()),
            output: entity("orders:output", "postgres", summary.clone()),
            summary,
        });

        let table = render_table(&[instance], 75_000, false);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 2, "{table}");
        assert!(lines[0].starts_with("INSTANCE"));
        for expected in [
            "mcp work [42]",
            "kafka → postgres",
            "running",
            "1234.6/s",
            "1000.0/s",
            "5000",
            "1m05s",
        ] {
            assert!(lines[1].contains(expected), "missing {expected}: {table}");
        }
    }

    #[test]
    fn states_follow_outcome_then_health() {
        let state = |summary: StatusSummary| state_label(&summary);
        assert_eq!(
            state(StatusSummary {
                outcome: Some(RouteOutcomeSnapshot::Failed),
                ..Default::default()
            }),
            "failed"
        );
        assert_eq!(
            state(StatusSummary {
                outcome: Some(RouteOutcomeSnapshot::Completed),
                error: Some("error".to_string()),
                ..Default::default()
            }),
            "completed (error)"
        );
        assert_eq!(
            state(StatusSummary {
                running: true,
                healthy: false,
                ..Default::default()
            }),
            "unhealthy"
        );
        assert_eq!(state(StatusSummary::default()), "stopped");
    }

    #[test]
    fn an_instance_running_nothing_still_gets_a_row() {
        let instance = InstanceStatus::new(InstanceKind::WebUi, "1", "/tmp/work.yml");
        let table = render_table(&[instance], 0, true);
        assert!(table.lines().nth(1).unwrap().contains("idle"), "{table}");
    }
}
