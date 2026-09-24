//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Finding an installed plugin from the endpoint name a route asked for.
//!
//! A `custom` endpoint whose name no factory is registered under is the trigger:
//! the searched directories are consulted for one file, `libmq_bridge_<name>`.
//! Directories are never listed, so a library no route names is never opened.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use super::{load_endpoint_plugin, PluginInfo};

/// Set to `0`, `false`, `off` or `no` to resolve `custom` endpoints only from
/// factories the host registered or a config listed by path.
pub const DISCOVERY_VAR: &str = "MQB_PLUGIN_DISCOVERY";

/// Platform-separated list of directories searched ahead of the default ones.
pub const SEARCH_PATH_VAR: &str = "MQB_PLUGIN_DIR";

/// Whether a name may be resolved against the search path.
pub fn discovery_enabled() -> bool {
    discovery_enabled_from(std::env::var(DISCOVERY_VAR).ok().as_deref())
}

fn discovery_enabled_from(value: Option<&str>) -> bool {
    match value {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

/// The file name a plugin providing `name` is expected to have.
pub fn library_file_name(name: &str) -> String {
    let stem = name.replace('-', "_");
    if cfg!(target_os = "windows") {
        format!("mq_bridge_{stem}.dll")
    } else if cfg!(target_os = "macos") {
        format!("libmq_bridge_{stem}.dylib")
    } else {
        format!("libmq_bridge_{stem}.so")
    }
}

/// Install prefixes searched when the environment does not name one, so a
/// Homebrew plugin is found by a binary installed somewhere else.
///
/// Homebrew's own prefix differs by architecture on macOS — `/opt/homebrew` on
/// Apple Silicon, `/usr/local` on Intel — and both are listed rather than gated
/// on `target_arch`, because an x86_64 build under Rosetta reports x86_64 while
/// the host's brew is in `/opt/homebrew`. Each costs one `stat`.
#[cfg(target_os = "macos")]
const DEFAULT_PREFIXES: &[&str] = &["/opt/homebrew", "/usr/local"];
#[cfg(target_os = "windows")]
const DEFAULT_PREFIXES: &[&str] = &[];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const DEFAULT_PREFIXES: &[&str] = &["/home/linuxbrew/.linuxbrew", "/usr/local"];

/// Directories searched for a plugin library, nearest first.
pub fn plugin_search_path() -> Vec<PathBuf> {
    search_path_from(
        std::env::var_os(SEARCH_PATH_VAR).as_deref(),
        std::env::current_exe().ok().as_deref(),
        &environment_prefixes(),
        user_data_dir().as_deref(),
    )
}

/// Install prefixes to search besides the running binary's own: an activated
/// conda environment, Homebrew, and Homebrew's usual locations when its
/// `brew shellenv` is not in the environment (a service, a container).
fn environment_prefixes() -> Vec<PathBuf> {
    let mut prefixes: Vec<PathBuf> = ["CONDA_PREFIX", "HOMEBREW_PREFIX"]
        .iter()
        .filter_map(|var| non_empty_var(var))
        .map(PathBuf::from)
        .collect();
    prefixes.extend(DEFAULT_PREFIXES.iter().map(PathBuf::from));
    prefixes
}

/// Where an install prefix keeps a plugin library. `lib/mq-bridge` is the
/// convention; plain `lib` is where a brew formula or conda package puts a
/// library by default, and one exact file name is cheap to probe either way.
fn prefix_dirs(prefix: &Path) -> Vec<PathBuf> {
    let lib = prefix.join("lib");
    let mut dirs = vec![lib.join("mq-bridge"), lib];
    if cfg!(target_os = "windows") {
        dirs.push(prefix.join("Library").join("bin"));
    }
    dirs
}

fn search_path_from(
    configured: Option<&OsStr>,
    current_exe: Option<&Path>,
    prefixes: &[PathBuf],
    data_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(configured) = configured {
        dirs.extend(std::env::split_paths(configured).filter(|dir| !dir.as_os_str().is_empty()));
    }
    if let Some(bin) = current_exe.and_then(Path::parent) {
        dirs.push(bin.to_path_buf());
        // An installed layout splits `<prefix>/bin` from `<prefix>/lib`;
        // a build tree keeps the binary and the cdylib in one directory.
        if let Some(prefix) = bin.parent() {
            dirs.extend(prefix_dirs(prefix));
        }
    }
    dirs.extend(prefixes.iter().flat_map(|prefix| prefix_dirs(prefix)));
    if let Some(data) = data_dir {
        dirs.push(data.join("mq-bridge").join("plugins"));
    }
    let mut seen = HashSet::new();
    dirs.retain(|dir| seen.insert(dir.clone()));
    dirs
}

fn user_data_dir() -> Option<PathBuf> {
    if let Some(xdg) = non_empty_var("XDG_DATA_HOME") {
        return Some(PathBuf::from(xdg));
    }
    if cfg!(target_os = "windows") {
        return non_empty_var("APPDATA").map(PathBuf::from);
    }
    non_empty_var("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
}

fn non_empty_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// Loads the plugin providing `name` from the search path.
///
/// `Ok(None)` means no candidate file exists, which is not an error on its own —
/// the caller reports the unresolved name, with [`search_path_hint`].
pub fn discover_endpoint_plugin(name: &str) -> anyhow::Result<Option<PluginInfo>> {
    if !discovery_enabled() {
        return Ok(None);
    }
    discover_endpoint_plugin_in(&plugin_search_path(), name)
}

/// [`discover_endpoint_plugin`] against an explicit list of directories.
///
/// An explicit call is not subject to [`DISCOVERY_VAR`], which switches off only
/// the search a route triggers by itself. Useful to a host that keeps plugins
/// somewhere it already knows, and to tests, which then need no process env.
pub fn discover_endpoint_plugin_in(
    dirs: &[PathBuf],
    name: &str,
) -> anyhow::Result<Option<PluginInfo>> {
    let file_name = library_file_name(name);
    for dir in dirs {
        let candidate = dir.join(&file_name);
        if !candidate.is_file() {
            continue;
        }
        let info = load_endpoint_plugin(&candidate)
            .with_context(|| format!("endpoint `{name}` resolved to {}", candidate.display()))?;
        // The file name is a convention the library itself never sees, so a
        // mismatch is possible. It stays loaded, because unloading is not safe.
        if info.name != name {
            return Err(anyhow!(
                "{} is named for endpoint `{name}` but provides `{}`; it stays loaded for \
                 the life of the process. Rename the file to {} or ask for `{}`.",
                candidate.display(),
                info.name,
                library_file_name(&info.name),
                info.name,
            ));
        }
        if !(info.supports_consumer || info.supports_publisher) {
            return Err(anyhow!(
                "{} provides the `{name}` middleware but no endpoint",
                candidate.display(),
            ));
        }
        return Ok(Some(info));
    }
    Ok(None)
}

/// Where an unresolved endpoint name was looked for, to append to that error.
pub fn search_path_hint(name: &str) -> String {
    if !discovery_enabled() {
        return format!("plugin discovery is off ({DISCOVERY_VAR})");
    }
    let file_name = library_file_name(name);
    // The path spans several install prefixes, most of which exist on no one
    // machine; a directory that is not there tells the reader nothing.
    let dirs = plugin_search_path()
        .iter()
        .filter(|dir| dir.is_dir())
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if dirs.is_empty() {
        return format!("no {file_name} on the plugin search path ({SEARCH_PATH_VAR} adds to it)");
    }
    format!("no {file_name} in {dirs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_library_name_is_the_endpoint_name_with_the_platform_decoration() {
        let name = library_file_name("connect");
        assert!(name.contains("mq_bridge_connect"), "{name}");
        if cfg!(target_os = "windows") {
            assert_eq!(name, "mq_bridge_connect.dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(name, "libmq_bridge_connect.dylib");
        } else {
            assert_eq!(name, "libmq_bridge_connect.so");
        }
    }

    /// An endpoint name is free to contain `-`; a library name is not.
    #[test]
    fn a_hyphenated_endpoint_name_becomes_an_underscored_library_name() {
        assert!(library_file_name("ibm-mq").contains("mq_bridge_ibm_mq"));
    }

    #[test]
    fn the_configured_directory_comes_before_the_default_ones() {
        let dirs = search_path_from(
            Some(OsStr::new("/opt/first")),
            Some(Path::new("/usr/local/bin/mqb")),
            &[],
            Some(Path::new("/home/u/.local/share")),
        );

        assert_eq!(dirs.first(), Some(&PathBuf::from("/opt/first")));
        assert_eq!(
            dirs.last(),
            Some(&PathBuf::from("/home/u/.local/share/mq-bridge/plugins"))
        );
    }

    /// The installed layout is the reason the binary's own directory is not enough.
    #[test]
    fn the_search_path_covers_the_sibling_lib_directory_of_an_installed_binary() {
        let dirs = search_path_from(None, Some(Path::new("/opt/homebrew/bin/mqb")), &[], None);

        assert!(
            dirs.contains(&PathBuf::from("/opt/homebrew/lib/mq-bridge")),
            "{dirs:?}"
        );
        assert!(
            dirs.contains(&PathBuf::from("/opt/homebrew/lib")),
            "{dirs:?}"
        );
    }

    /// A conda environment or a Homebrew prefix reaches a plugin the running
    /// binary is not installed beside — `python` in a venv, `cargo run`, a container.
    #[test]
    fn a_prefix_from_the_environment_is_searched_besides_the_binary_s_own() {
        let dirs = search_path_from(
            None,
            Some(Path::new("/home/u/.venv/bin/python")),
            &[PathBuf::from("/opt/conda/envs/etl")],
            None,
        );

        assert!(
            dirs.contains(&PathBuf::from("/opt/conda/envs/etl/lib/mq-bridge")),
            "{dirs:?}"
        );
        assert!(
            dirs.contains(&PathBuf::from("/opt/conda/envs/etl/lib")),
            "{dirs:?}"
        );
        let binary_lib = dirs
            .iter()
            .position(|d| d == Path::new("/home/u/.venv/lib"));
        let prefix_lib = dirs
            .iter()
            .position(|d| d == Path::new("/opt/conda/envs/etl/lib"));
        assert!(binary_lib < prefix_lib, "{dirs:?}");
    }

    /// Homebrew exports `HOMEBREW_PREFIX` from a shell profile, so a service or a
    /// container has to fall back to where it actually installs.
    #[test]
    fn homebrew_is_searched_without_its_shellenv() {
        let prefixes = environment_prefixes();

        // Both macOS prefixes, whatever this build's architecture: the host's
        // brew is not necessarily the one a `target_arch` cfg would pick.
        if cfg!(target_os = "macos") {
            assert!(
                prefixes.contains(&PathBuf::from("/opt/homebrew")),
                "{prefixes:?}"
            );
        }
        if !cfg!(target_os = "windows") {
            assert!(
                prefixes.contains(&PathBuf::from("/usr/local")),
                "{prefixes:?}"
            );
        }
    }

    #[test]
    fn a_directory_named_twice_is_searched_once() {
        let dirs = search_path_from(
            Some(OsStr::new("/usr/local/bin")),
            Some(Path::new("/usr/local/bin/mqb")),
            &[PathBuf::from("/usr/local")],
            None,
        );

        for once in [
            "/usr/local/bin",
            "/usr/local/lib",
            "/usr/local/lib/mq-bridge",
        ] {
            // `Path`, not the raw string: a joined directory spells its
            // separator the platform's way.
            let once = Path::new(once);
            assert_eq!(
                dirs.iter().filter(|d| d.as_path() == once).count(),
                1,
                "{} in {dirs:?}",
                once.display()
            );
        }
    }

    #[test]
    fn an_empty_search_path_entry_is_not_the_current_directory() {
        let dirs = search_path_from(Some(OsStr::new("")), None, &[], None);

        assert!(dirs.is_empty(), "{dirs:?}");
    }

    #[test]
    fn discovery_is_on_unless_it_is_switched_off() {
        assert!(discovery_enabled_from(None));
        assert!(discovery_enabled_from(Some("1")));
        for off in ["0", "false", "off", "no", " OFF "] {
            assert!(!discovery_enabled_from(Some(off)), "{off}");
        }
    }

    #[test]
    fn nothing_is_discovered_for_a_name_with_no_library() {
        let found = discover_endpoint_plugin("a-name-no-library-is-installed-for").unwrap();

        assert!(found.is_none());
    }

    /// The path spans install prefixes that exist on no one machine, so the hint
    /// names only the directories the reader can actually go and look in.
    #[test]
    fn the_hint_lists_only_directories_that_exist() {
        let name = "a-name-no-library-is-installed-for";
        let hint = search_path_hint(name);

        assert!(hint.contains(&library_file_name(name)), "{hint}");
        for dir in plugin_search_path() {
            let listed = hint.contains(&dir.display().to_string());
            assert_eq!(listed, dir.is_dir(), "{} in: {hint}", dir.display());
        }
    }
}
