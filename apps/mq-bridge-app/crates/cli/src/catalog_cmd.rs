//! `mqb list` and `mqb describe`: text and JSON views of
//! [`mq_bridge_app::endpoint_catalog`].

use mq_bridge_app::endpoint_catalog::{Catalog, Param, catalog, matching};

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Print the full catalog, params included, as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
pub struct DescribeArgs {
    /// A scheme (`kafka`), one of its aliases, an endpoint type (`sqlx`) or a
    /// middleware (`retry`).
    name: String,

    /// Print the matches as JSON, in the same shape as `list --json`.
    #[arg(long)]
    json: bool,
}

pub fn list(args: ListArgs) -> anyhow::Result<()> {
    let catalog = catalog()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&catalog)?);
    } else {
        print!("{}", render_list(&catalog));
    }
    Ok(())
}

pub fn describe(args: DescribeArgs) -> anyhow::Result<()> {
    let found = matching(catalog()?, &args.name);
    if found.endpoints.is_empty() && found.middlewares.is_empty() {
        anyhow::bail!(
            "no endpoint or middleware named '{}'. Run `mqb list` to see all of them",
            args.name
        );
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&found)?);
    } else {
        print!("{}", render_describe(&found));
    }
    Ok(())
}

const CATEGORIES: &[(&str, &str)] = &[
    ("broker", "Brokers"),
    ("database", "Databases"),
    ("storage", "Files and object storage"),
    ("http", "HTTP"),
    ("structural", "Structural (no external system)"),
    ("extension", "Extensions and plugins"),
];

fn render_list(catalog: &Catalog) -> String {
    use std::fmt::Write;
    let width = catalog
        .endpoints
        .iter()
        .map(|e| e.scheme.len())
        .chain(catalog.middlewares.iter().map(|m| m.name.len()))
        .max()
        .unwrap_or(0);
    let mut out = String::from("ENDPOINTS — the scheme of a --from/--to URI\n");
    for (category, title) in CATEGORIES {
        let entries: Vec<_> = catalog
            .endpoints
            .iter()
            .filter(|e| e.category == *category)
            .collect();
        if entries.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n  {title}");
        for e in entries {
            let _ = writeln!(out, "    {:width$}  {}", e.scheme, e.summary);
        }
    }
    out.push_str("\nMIDDLEWARES — appended to a URI as |name?param=value\n\n");
    for m in &catalog.middlewares {
        let _ = writeln!(out, "    {:width$}  {}", m.name, m.summary);
    }
    out.push_str(
        "\n`mqb describe <name>` shows the URI form and every param; `--json` prints it all.\n",
    );
    out
}

fn render_describe(catalog: &Catalog) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for e in &catalog.endpoints {
        let _ = writeln!(out, "{} — {}", e.scheme, e.summary);
        let _ = writeln!(out, "  endpoint type   {} ({})", e.endpoint, e.category);
        if !e.aliases.is_empty() {
            let _ = writeln!(out, "  aliases         {}", e.aliases.join(", "));
        }
        let _ = writeln!(out, "  uri             {}", e.uri);
        if let Some(example) = &e.example {
            let _ = writeln!(out, "  example         {example}");
        }
        match e.unknown_params {
            Some("driver_option") => {
                out.push_str("  unknown params  kept on the connection URL as driver options\n")
            }
            Some(_) => out.push_str("  unknown params  rejected\n"),
            None => {}
        }
        render_params(&mut out, &e.params);
        out.push('\n');
    }
    for m in &catalog.middlewares {
        let _ = writeln!(out, "{} — {}", m.name, m.summary);
        let _ = writeln!(out, "  middleware      {}", m.spec);
        render_params(&mut out, &m.params);
        out.push('\n');
    }
    out
}

fn render_params(out: &mut String, params: &[Param]) {
    use std::fmt::Write;
    if params.is_empty() {
        return;
    }
    let width = params.iter().map(|p| p.name.len()).max().unwrap_or(0);
    let ty_width = params.iter().map(|p| p.ty.len()).max().unwrap_or(0);
    out.push_str("  params\n");
    for p in params {
        let mut notes = Vec::new();
        if p.required {
            notes.push("required".to_string());
        }
        if !p.values.is_empty() {
            notes.push(format!("one of: {}", p.values.join(", ")));
        }
        if let Some(default) = p.default.as_ref().filter(|d| !d.is_object()) {
            notes.push(format!("default: {default}"));
        }
        let mut line = p.description.clone().unwrap_or_default();
        if !notes.is_empty() {
            if !line.is_empty() {
                line.push(' ');
            }
            let _ = write!(line, "[{}]", notes.join("; "));
        }
        let _ = writeln!(out, "    {:width$}  {:ty_width$}  {line}", p.name, p.ty);
    }
}
