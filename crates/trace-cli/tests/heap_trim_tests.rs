//! On Windows `analyze` reports whether it trims the process heaps at
//! phase boundaries (`TRACE_HEAP_TRIM=1`, the #179 measurement
//! configuration) as a `heap:` line on stderr. Runs in the Windows memory
//! measurement workflow (`.github/workflows/memory-windows.yml`).

#![cfg(windows)]

mod common;

use std::process::Command;

/// The `heap:` status line of `analyze` on the `direct_call` fixture, run
/// with `TRACE_HEAP_TRIM` unset or set to `value`.
fn heap_line(value: Option<&str>) -> String {
    let db = common::TempDb::new("out.db");
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace"));
    command
        .arg("analyze")
        .arg(common::fixture("direct_call"))
        .args(["--jobs", "1", "-o"])
        .arg(db.path())
        .env_remove("TRACE_HEAP_TRIM");
    if let Some(value) = value {
        command.env("TRACE_HEAP_TRIM", value);
    }
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(db.is_file(), "analysis completed and exported");
    let lines: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("heap:"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one heap: status line, no failures: {stderr}"
    );
    lines[0].to_owned()
}

#[test]
fn analyze_keeps_the_default_heap_by_default() {
    assert_eq!(heap_line(None), "heap: default");
}

#[test]
fn an_explicit_one_trims_at_phase_boundaries() {
    assert_eq!(heap_line(Some("1")), "heap: trim");
}

#[test]
fn an_explicit_zero_keeps_the_default_heap() {
    assert_eq!(heap_line(Some("0")), "heap: default");
}
