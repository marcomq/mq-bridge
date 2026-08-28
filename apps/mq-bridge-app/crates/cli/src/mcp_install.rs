//  mq-bridge-app
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Registers this binary as a stdio MCP server with local MCP clients.
//!
//! Where a client ships its own CLI (Claude Code) we drive that, since it
//! survives config-schema changes; otherwise we merge into the client's JSON
//! config, preserving every other server already registered there.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Key this server is registered under in every client config.
const SERVER_NAME: &str = "mq-bridge";

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    /// Claude Code (via the `claude` CLI when available, else `~/.claude.json`).
    Claude,
    /// Claude Desktop.
    ClaudeDesktop,
    /// Cursor.
    Cursor,
}

impl Client {
    fn label(self) -> &'static str {
        match self {
            Client::Claude => "Claude Code",
            Client::ClaudeDesktop => "Claude Desktop",
            Client::Cursor => "Cursor",
        }
    }

    /// Config file this client reads for the given scope, or `None` when the
    /// client has no such scope.
    fn config_path(self, local: bool) -> Option<PathBuf> {
        match (self, local) {
            // Claude Code reads a project-scoped `.mcp.json` from the cwd.
            (Client::Claude, true) => Some(PathBuf::from(".mcp.json")),
            (Client::Claude, false) => Some(home_dir().ok()?.join(".claude.json")),
            (Client::Cursor, true) => Some(PathBuf::from(".cursor/mcp.json")),
            (Client::Cursor, false) => Some(home_dir().ok()?.join(".cursor/mcp.json")),
            // Claude Desktop is global-only.
            (Client::ClaudeDesktop, true) => None,
            (Client::ClaudeDesktop, false) => Some(claude_desktop_config_path()?),
        }
    }

    /// Whether this client looks present on the machine, used to pick targets
    /// when `--client` was omitted.
    fn detected(self) -> bool {
        match self {
            Client::Claude => which("claude").is_some() || exists(self.config_path(false)),
            _ => exists(self.config_path(false)),
        }
    }
}

fn exists(path: Option<PathBuf>) -> bool {
    path.is_some_and(|p| p.exists())
}

fn home_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("could not determine the home directory (neither HOME nor USERPROFILE is set)")?;
    Ok(PathBuf::from(home))
}

fn claude_desktop_config_path() -> Option<PathBuf> {
    let home = home_dir().ok()?;
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Claude")
    } else if cfg!(target_os = "windows") {
        PathBuf::from(std::env::var_os("APPDATA")?).join("Claude")
    } else {
        home.join(".config/Claude")
    };
    Some(dir.join("claude_desktop_config.json"))
}

/// First match for `name` on PATH. On Windows the same lookup also tries the
/// `PATHEXT` extensions (`.cmd`, `.exe`, …), since an executable there is
/// `name.cmd`/`name.exe` on disk rather than a bare `name` — without this,
/// `which("claude")` never finds the Windows CLI.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        {
            // PATHEXT is a `;`-separated list, e.g. `.COM;.EXE;.BAT;.CMD`.
            let pathext =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            for ext in pathext.split(';').filter(|e| !e.is_empty()) {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    })
}

/// Absolute path of the binary currently executing, so the registered command
/// points at whatever the user just ran rather than a stale build output.
fn current_exe() -> Result<String> {
    let exe = std::env::current_exe().context("could not determine the path of this binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    exe.into_os_string()
        .into_string()
        .map_err(|p| anyhow::anyhow!("binary path is not valid UTF-8: {}", p.to_string_lossy()))
}

/// Arguments the client should launch this binary with.
///
/// Status publication is on by default, so nothing is added here for it. Older
/// installs carry a `--report-to-ui` this binary still accepts as a no-op.
fn server_args() -> Vec<String> {
    vec![
        "mcp".to_string(),
        "--transport".to_string(),
        "stdio".to_string(),
    ]
}

fn server_entry(exe: &str) -> Value {
    json!({ "command": exe, "args": server_args() })
}

/// Clients to act on: the explicit one, or every client detected on this machine.
fn targets(client: Option<Client>) -> Result<Vec<Client>> {
    if let Some(client) = client {
        return Ok(vec![client]);
    }
    let detected: Vec<Client> = [Client::Claude, Client::ClaudeDesktop, Client::Cursor]
        .into_iter()
        .filter(|c| c.detected())
        .collect();
    if detected.is_empty() {
        bail!(
            "no MCP client detected; pass --client <claude|claude-desktop|cursor>, \
             or use --print-config to configure one by hand"
        );
    }
    Ok(detected)
}

pub fn install(client: Option<Client>, local: bool) -> Result<()> {
    let exe = current_exe()?;
    for client in targets(client)? {
        // Claude Code owns its config format; let its CLI write it when present.
        if client == Client::Claude
            && which("claude").is_some()
            && install_via_claude_cli(&exe, local)?
        {
            println!(
                "{}: registered '{SERVER_NAME}' via the claude CLI",
                client.label()
            );
            continue;
        }
        let Some(path) = client.config_path(local) else {
            bail!(
                "{} has no project-scoped config; drop --local",
                client.label()
            );
        };
        merge_server(&path, server_entry(&exe))?;
        println!(
            "{}: registered '{SERVER_NAME}' in {}",
            client.label(),
            path.display()
        );
    }
    println!("Restart the client fully (not just a new tab) for it to pick the server up.");
    Ok(())
}

/// Returns `false` when the CLI ran but refused, so we can fall back to JSON.
fn install_via_claude_cli(exe: &str, local: bool) -> Result<bool> {
    // Re-adding an existing name is an error, so clear it first; a missing
    // server makes this a no-op and either way we ignore the outcome.
    let _ = Command::new("claude")
        .args(["mcp", "remove", "--scope", claude_scope(local), SERVER_NAME])
        .output();

    let mut cmd = Command::new("claude");
    cmd.args([
        "mcp",
        "add",
        "--transport",
        "stdio",
        "--scope",
        claude_scope(local),
        SERVER_NAME,
        "--",
        exe,
    ])
    .args(server_args());

    let out = cmd.output().context("failed to run the claude CLI")?;
    if out.status.success() {
        return Ok(true);
    }
    eprintln!(
        "claude CLI failed ({}), falling back to editing the config directly: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(false)
}

fn claude_scope(local: bool) -> &'static str {
    if local { "project" } else { "user" }
}

pub fn uninstall(client: Option<Client>, local: bool) -> Result<()> {
    for client in targets(client)? {
        if client == Client::Claude && which("claude").is_some() {
            let out = Command::new("claude")
                .args(["mcp", "remove", "--scope", claude_scope(local), SERVER_NAME])
                .output()
                .context("failed to run the claude CLI")?;
            if out.status.success() {
                println!(
                    "{}: removed '{SERVER_NAME}' via the claude CLI",
                    client.label()
                );
                continue;
            }
        }
        let Some(path) = client.config_path(local) else {
            continue;
        };
        match remove_server(&path)? {
            true => println!(
                "{}: removed '{SERVER_NAME}' from {}",
                client.label(),
                path.display()
            ),
            false => println!("{}: '{SERVER_NAME}' was not registered", client.label()),
        }
    }
    Ok(())
}

pub fn status(local: bool) -> Result<()> {
    let exe = current_exe()?;
    println!("this binary: {exe}\n");
    for client in [Client::Claude, Client::ClaudeDesktop, Client::Cursor] {
        let Some(path) = client.config_path(local) else {
            println!("{}: no {} scope", client.label(), scope_label(local));
            continue;
        };
        let registered = read_config(&path)?
            .pointer(&format!("/mcpServers/{SERVER_NAME}"))
            .cloned();
        match registered {
            Some(entry) => {
                let command = entry.get("command").and_then(Value::as_str).unwrap_or("?");
                let stale = if command == exe {
                    ""
                } else {
                    "  (points at a different binary)"
                };
                println!("{}: registered -> {command}{stale}", client.label());
            }
            None => println!("{}: not registered ({})", client.label(), path.display()),
        }
    }
    Ok(())
}

fn scope_label(local: bool) -> &'static str {
    if local { "project" } else { "user" }
}

/// The config snippet, for any client we don't write directly.
pub fn print_config() -> Result<()> {
    let exe = current_exe()?;
    let config = json!({ "mcpServers": { SERVER_NAME: server_entry(&exe) } });
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

/// Parsed contents of `path`, or an empty object when it doesn't exist.
fn read_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))
}

fn write_config(path: &Path, config: &Value) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(path, text + "\n").with_context(|| format!("failed to write {}", path.display()))
}

/// Sets our entry under `mcpServers`, leaving every other key untouched.
fn merge_server(path: &Path, entry: Value) -> Result<()> {
    let mut config = read_config(path)?;
    let Some(obj) = config.as_object_mut() else {
        bail!("{} does not contain a JSON object", path.display());
    };
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("'mcpServers' in {} is not an object", path.display()))?;
    servers.insert(SERVER_NAME.to_string(), entry);
    write_config(path, &config)
}

/// Returns whether an entry was actually removed.
fn remove_server(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut config = read_config(path)?;
    let removed = config
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .is_some_and(|servers| servers.remove(SERVER_NAME).is_some());
    if removed {
        write_config(path, &config)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mq-bridge-mcp-install-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.json")
    }

    #[test]
    fn merge_preserves_other_servers_and_keys() {
        let path = temp_path("preserve");
        write_config(
            &path,
            &json!({ "theme": "dark", "mcpServers": { "other": { "command": "x" } } }),
        )
        .unwrap();

        merge_server(&path, server_entry("/bin/mq-bridge-app")).unwrap();

        let config = read_config(&path).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            config["mcpServers"][SERVER_NAME]["command"],
            "/bin/mq-bridge-app"
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let path = temp_path("idempotent");
        let entry = server_entry("/bin/mq-bridge-app");
        merge_server(&path, entry.clone()).unwrap();
        let first = read_config(&path).unwrap();
        merge_server(&path, entry).unwrap();
        assert_eq!(first, read_config(&path).unwrap());
    }

    #[test]
    fn remove_only_touches_our_entry() {
        let path = temp_path("remove");
        write_config(
            &path,
            &json!({ "mcpServers": { "other": { "command": "x" } } }),
        )
        .unwrap();
        merge_server(&path, server_entry("/bin/mq-bridge-app")).unwrap();

        assert!(remove_server(&path).unwrap());
        let config = read_config(&path).unwrap();
        assert!(config["mcpServers"].get(SERVER_NAME).is_none());
        assert_eq!(config["mcpServers"]["other"]["command"], "x");
        // Removing again is a no-op rather than an error.
        assert!(!remove_server(&path).unwrap());
    }

    // Newly written client configs must not bake in the deprecated flag.
    #[test]
    fn installed_args_carry_no_report_to_ui_flag() {
        assert_eq!(server_args(), ["mcp", "--transport", "stdio"]);
    }
}
