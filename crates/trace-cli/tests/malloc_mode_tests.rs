//! On macOS `analyze` re-executes under libmalloc's space-efficient mode
//! and reports the mode in effect as a `malloc:` line on stderr.

#![cfg(all(target_os = "macos", not(feature = "mimalloc")))]

mod common;

use std::process::Command;

/// The `malloc:` status line of `analyze` on the `direct_call` fixture, run
/// with `MallocSpaceEfficient` unset or set to `mode`.
fn malloc_line(mode: Option<&str>) -> String {
    let db = common::TempDb::new("out.db");
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace"));
    command
        .arg("analyze")
        .arg(common::fixture("direct_call"))
        .args(["--jobs", "1", "-o"])
        .arg(db.path())
        .env_remove("MallocSpaceEfficient");
    if let Some(mode) = mode {
        command.env("MallocSpaceEfficient", mode);
    }
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(db.is_file(), "analysis completed and exported");
    stderr
        .lines()
        .find(|line| line.starts_with("malloc:"))
        .expect("a malloc: status line")
        .to_owned()
}

#[test]
fn analyze_runs_under_space_efficient_malloc_by_default() {
    assert_eq!(malloc_line(None), "malloc: space-efficient");
}

#[test]
fn an_explicit_zero_keeps_the_default_allocator() {
    assert_eq!(malloc_line(Some("0")), "malloc: default");
}

#[test]
fn an_empty_value_is_replaced() {
    assert_eq!(malloc_line(Some("")), "malloc: space-efficient");
}
