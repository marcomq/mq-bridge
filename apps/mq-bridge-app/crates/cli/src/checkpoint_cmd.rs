//  mq-bridge-app
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `mqb checkpoint`: inspect and edit a source's durable resume position.

use anyhow::{Context, anyhow, bail};
use mq_bridge::checkpoint::{CheckpointEntry, CheckpointStore, VersionedCheckpoint};
use mq_bridge::models::{Endpoint, EndpointType};
use mq_bridge_app::{config::load_config, copy_pipeline, mq_bridge};
use std::sync::Arc;

const RUNNING_READER_NOTE: &str =
    "note: a reader that is still running overwrites this with its next batch";

#[derive(clap::Args, Debug)]
pub(crate) struct CheckpointArgs {
    #[command(subcommand)]
    action: CheckpointAction,
}

impl CheckpointArgs {
    pub(crate) fn verbose(&self) -> bool {
        self.target().verbose
    }

    fn target(&self) -> &CheckpointTarget {
        match &self.action {
            CheckpointAction::Show(target) | CheckpointAction::Reset(target) => target,
            CheckpointAction::Set { target, .. } => target,
        }
    }
}

#[derive(clap::Subcommand, Debug)]
enum CheckpointAction {
    /// Print the stored position, which source wrote it, and when.
    Show(CheckpointTarget),

    /// Delete the stored position, so the next run starts from the beginning.
    ///
    /// The previous value is printed first; pass it to `set` to undo.
    Reset(CheckpointTarget),

    /// Overwrite the stored position.
    Set {
        /// The position exactly as `show` prints it, e.g. `int:42`.
        #[arg(long, value_name = "VALUE")]
        value: String,

        #[command(flatten)]
        target: CheckpointTarget,
    },
}

/// Names the checkpoint: a configured route, or the SOURCE/TARGET/--filter of a
/// `copy --resume` job, which together form its checkpoint identity.
#[derive(clap::Args, Debug)]
struct CheckpointTarget {
    /// Route or consumer name in the loaded config.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["source", "target", "filter"])]
    route: Option<String>,

    /// Source endpoint URI, exactly as passed to `copy`.
    #[arg(value_name = "SOURCE", index = 1, requires = "target")]
    source: Option<String>,

    /// Destination endpoint URI, exactly as passed to `copy`.
    #[arg(value_name = "TARGET", index = 2)]
    target: Option<String>,

    /// The `--filter` the copy ran with.
    #[arg(long, value_name = "EXPR")]
    filter: Option<String>,

    /// Log connection details. Without it only warnings and errors are logged.
    #[arg(short, long)]
    verbose: bool,
}

pub(crate) async fn run(
    args: CheckpointArgs,
    config_path: Option<String>,
    config_str: Option<String>,
) -> anyhow::Result<()> {
    let (name, input) = resolve_source(args.target(), config_path, config_str)?;
    let checkpoint = mq_bridge::checkpoint::open_endpoint_checkpoint(&name, &input.endpoint_type)
        .await
        .with_context(|| format!("cannot open the checkpoint of `{name}`"))?
        .ok_or_else(|| anyhow!("`{name}` has no checkpoint: its source sets no `cursor_id` (and `checkpoint_store` where required)"))?;

    match args.action {
        CheckpointAction::Show(_) => print_entry(&checkpoint, checkpoint.entry().await?),
        CheckpointAction::Reset(_) => {
            match checkpoint.entry().await {
                Ok(None) => {
                    print_entry(&checkpoint, None);
                    return Ok(());
                }
                Ok(previous) => print_entry(&checkpoint, previous),
                Err(e) => println!("value:   unreadable ({e:#})"),
            }
            checkpoint.clear().await?;
            println!("reset: the next run starts from the beginning");
            println!("{RUNNING_READER_NOTE}");
        }
        CheckpointAction::Set { value, .. } => {
            check_sql_cursor(&input.endpoint_type, &value)?;
            let previous = match checkpoint.entry().await {
                Ok(previous) => previous.map_or("unset".to_string(), |e| e.value),
                Err(e) => format!("unreadable: {e:#}"),
            };
            checkpoint.save(&value).await?;
            println!("set: {value} (was {previous})");
            println!("{RUNNING_READER_NOTE}");
        }
    }
    Ok(())
}

/// A SQL cursor reader stops on a value in another form, so `set` refuses it.
fn check_sql_cursor(source: &EndpointType, value: &str) -> anyhow::Result<()> {
    let EndpointType::Sqlx(config) = source else {
        return Ok(());
    };
    if config.cursor_column.is_none() || config.publication.is_some() {
        return Ok(());
    }
    let readable = match value.split_once(':') {
        Some(("int", number)) => number.parse::<i64>().is_ok(),
        Some(("str", _)) => true,
        _ => false,
    };
    if !readable {
        bail!(
            "`{value}` is not a SQL cursor: use `int:<number>` or `str:<text>`, as `show` prints it"
        );
    }
    Ok(())
}

/// Returns the source endpoint and the name its consumer runs under.
fn resolve_source(
    target: &CheckpointTarget,
    config_path: Option<String>,
    config_str: Option<String>,
) -> anyhow::Result<(String, Endpoint)> {
    if let Some(route) = &target.route {
        let (config, _) = load_config(config_path, None, None, config_str)
            .context("Failed to load configuration")?;
        if let Some(found) = config.routes.get(route) {
            return Ok((route.clone(), found.route.input.clone()));
        }
        return config
            .consumers
            .into_iter()
            .find(|consumer| &consumer.name == route)
            .map(|consumer| (route.clone(), consumer.endpoint))
            .ok_or_else(|| anyhow!("no route or consumer named `{route}` in the configuration"));
    }
    let (Some(source), Some(destination)) = (&target.source, &target.target) else {
        bail!("name the checkpoint with --route NAME, or with the SOURCE and TARGET of the copy");
    };
    let (mut input, output) = super::copy_route_endpoints(source, destination)?;
    copy_pipeline::configure_resume(&mut input, &output, target.filter.as_deref())?;
    Ok(("copy".to_string(), input))
}

fn print_entry(checkpoint: &Arc<VersionedCheckpoint>, entry: Option<CheckpointEntry>) {
    let Some(entry) = entry else {
        println!("unset ({})", checkpoint.source());
        return;
    };
    println!("value:   {}", entry.value);
    match &entry.source {
        Some(source) if source != checkpoint.source() => println!(
            "source:  {source} (MISMATCH: this source is {}; a run refuses this checkpoint)",
            checkpoint.source()
        ),
        Some(source) => println!("source:  {source}"),
        None => println!("source:  unrecorded (written before checkpoints were versioned)"),
    }
    if let Some(updated) = entry
        .updated_at_ms
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
    {
        println!("updated: {}", updated.to_rfc3339());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_refuses_a_sql_cursor_the_reader_cannot_read() {
        let (_, input) = resolve_source(
            &CheckpointTarget {
                route: None,
                source: Some("sqlite:///tmp/a.db?table=t&cursor_column=id".into()),
                target: Some("null:".into()),
                filter: None,
                verbose: false,
            },
            None,
            None,
        )
        .unwrap();
        for (value, ok) in [
            ("int:42", true),
            ("str:a:b", true),
            ("1000", false),
            ("int:x", false),
        ] {
            assert_eq!(
                check_sql_cursor(&input.endpoint_type, value).is_ok(),
                ok,
                "{value}"
            );
        }
    }
}
