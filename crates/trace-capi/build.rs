// Reuse trace-cli's build-identity logic so databases produced through
// `trace_index` carry the same provenance the CLI stamps (`0.1.0 (sha date)`):
// see crates/trace-cli/build.rs.
#[path = "../trace-cli/build_support.rs"]
mod build_support;

use build_support::{civil_from_days, parse_dirty, BuildMetadata};
use std::env;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    for name in [
        "TRACE_BUILD_GIT_SHA",
        "TRACE_BUILD_GIT_DIRTY",
        "TRACE_BUILD_DATE",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../trace-cli/build_support.rs");

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if let Some(git_dir) = build_support::git_dir(&workspace_root) {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
    }

    let metadata = BuildMetadata {
        commit: env::var("TRACE_BUILD_GIT_SHA")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| git_output(&["rev-parse", "--short=7", "HEAD"]))
            .unwrap_or_else(|| "unknown".to_owned()),
        dirty: env::var("TRACE_BUILD_GIT_DIRTY")
            .ok()
            .map(|value| parse_dirty(&value))
            .unwrap_or_else(|| {
                git_output(&["status", "--porcelain"]).is_some_and(|output| !output.is_empty())
            }),
        date: env::var("TRACE_BUILD_DATE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(current_utc_date),
    };
    println!(
        "cargo:rustc-env=TRACE_BUILD_VERSION={}",
        build_support::format_version(env!("CARGO_PKG_VERSION"), &metadata)
    );
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn current_utc_date() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}
