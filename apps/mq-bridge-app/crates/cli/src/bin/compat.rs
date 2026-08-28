//  mq-bridge-app
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `mqb` — the short command, forwarding to the `mq-bridge-app` binary.
//!
//! The real binary keeps the crate name because three things invoke it that way
//! and cannot be told otherwise: `cargo install`, the MCP registry's `cargo`
//! package type (whose spec says clients "invoke it directly by name"), and MCP
//! client configs, which store an absolute path. Wrapping the short name instead
//! leaves all three on the direct path.
//!
//! This is a launcher, not a second copy of the app: it depends only on `std`,
//! so it costs a link rather than a build. A symlink would do the same job, but
//! `cargo install` only installs binaries a package actually builds — hence a
//! real (tiny) binary, so Rust users get `mqb` too.
//!
//! Note that `mcp install` registers `std::env::current_exe()`, which after the
//! hand-off below is the real binary — so client configs never point here.

use std::path::PathBuf;
use std::process::Command;

/// The real binary, resolved next to this one. They are always installed
/// together, so this never consults PATH, where another install could shadow it.
fn sibling_target() -> PathBuf {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("mqb: could not determine this binary's path: {err}");
            std::process::exit(127);
        }
    };
    let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
    dir.join(if cfg!(windows) {
        "mq-bridge-app.exe"
    } else {
        "mq-bridge-app"
    })
}

fn main() {
    let target = sibling_target();
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` replaces this process rather than wrapping it, so the real
        // binary keeps the same pid, stdio and signal handling — nothing extra
        // sits in the middle of an MCP stdio pipeline. It only returns on error.
        //
        // `arg0` only renames argv[0], which is where clap reads the program
        // name for usage and error messages — so `mqb --help` says `mqb`. The
        // kernel still execs the real path, so `current_exe()` (and therefore
        // what `mcp install` registers) is unaffected.
        let err = Command::new(&target).arg0("mqb").args(&args).exec();
        eprintln!("mqb: could not run {}: {err}", target.display());
        std::process::exit(127);
    }

    #[cfg(not(unix))]
    {
        match Command::new(&target).args(&args).status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(err) => {
                eprintln!("mqb: could not run {}: {err}", target.display());
                std::process::exit(127);
            }
        }
    }
}
