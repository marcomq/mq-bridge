use std::env;
use std::process::Command;

fn main() {
    emit_build_metadata();

    if env::var("CARGO_FEATURE_IBM_MQ").is_err() {
        return;
    }

    println!("cargo:rerun-if-env-changed=MQ_INSTALLATION_PATH");
    println!("cargo:rerun-if-env-changed=MQ_HOME");

    let mq_home = env::var("MQ_INSTALLATION_PATH")
        .or_else(|_| env::var("MQ_HOME"))
        .unwrap_or_else(|_| "/opt/mqm".to_string());

    let target_pointer_width =
        env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_else(|_| "64".to_string());
    let lib_dir = if target_pointer_width == "64" {
        "lib64"
    } else {
        "lib"
    };
    let lib_path = format!("{}/{}", mq_home, lib_dir);

    println!("cargo:rustc-link-search=native={}", lib_path);

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
    }

    // On macOS no rpath is embedded by default, so the app can't find the IBM MQ
    // client dylib at runtime unless the user sets DYLD_LIBRARY_PATH manually.
    // Embed the installation lib dir as an rpath so it loads out of the box.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
    }
}

/// Capture build identity (git commit, profile, timestamp) into compile-time
/// env vars so the binary can report exactly which build is running. All
/// best-effort: a source tarball without git still builds, reporting "unknown".
fn emit_build_metadata() {
    let git_output = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };

    // Rerun the script when the checked-out commit or branch moves, so the git
    // hash below doesn't go stale: `logs/HEAD` advances on every commit, checkout
    // and reset (covering both new commits and branch switches), and `index` on
    // staging. Emitting any rerun-if directive opts out of Cargo's default
    // "rerun on any package change" — fine here, since nothing below reads crate
    // sources. (An unstaged edit still won't refresh the dirty flag: it touches
    // no git-metadata file, so that part stays best-effort.)
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    if let Some(git_dir) = git_output(&["rev-parse", "--absolute-git-dir"]) {
        for p in ["HEAD", "logs/HEAD", "index"] {
            let path = std::path::Path::new(&git_dir).join(p);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }

    let hash =
        git_output(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git_output(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let git = if dirty { format!("{hash}-dirty") } else { hash };
    println!("cargo:rustc-env=MQB_GIT_HASH={git}");

    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=MQB_BUILD_PROFILE={profile}");

    // No `date` subprocess: it isn't an executable on Windows (it's a cmd
    // builtin), so `Command::new("date")` failed there and the build time was
    // always "unknown". Compute it from the clock instead, honouring
    // SOURCE_DATE_EPOCH for reproducible builds.
    println!("cargo:rustc-env=MQB_BUILD_TIME={}", build_time());
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`, or `SOURCE_DATE_EPOCH` when set
/// (reproducible builds). "unknown" only if the clock predates the Unix epoch.
fn build_time() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = match env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        Some(epoch) => epoch,
        None => match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(_) => return "unknown".to_string(),
        },
    };

    // Civil date from Unix seconds (UTC), Howard Hinnant's algorithm.
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
