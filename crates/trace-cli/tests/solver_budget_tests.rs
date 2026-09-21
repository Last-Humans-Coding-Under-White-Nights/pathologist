//! Solver work-budget behavior: a budget-truncated solve must still export a
//! consistent (partial) database and *record* that it stopped early, both in
//! `analysis_run.options_json` and as an `analyze`-stage diagnostic, and
//! `trace inspect` must surface the warning.

use std::path::PathBuf;
use std::process::Command;

mod common;
use common::TempDb;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

fn analyze_with_pop_budget(tmp: &TempDb, budget: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_trace"))
        .args([
            "analyze",
            fixture("indirect_call").to_str().unwrap(),
            "-o",
            tmp.to_str().unwrap(),
            "--solve-budget-pops",
            budget,
        ])
        .output()
        .expect("analyze runs")
}

#[test]
fn truncated_solve_is_recorded_in_db_and_warns_on_stderr() {
    let tmp = TempDb::new("partial.db");
    let out = analyze_with_pop_budget(&tmp, "1");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("solver stopped before convergence"),
        "{stderr}"
    );
    assert!(stderr.contains("partial"), "{stderr}");

    let conn = trace_db::open_db(&tmp).unwrap();
    let info = trace_db::solver_outcome(&conn).unwrap();
    assert!(info.partial);
    assert!(info.pops >= 1);
    assert_eq!(info.budget_pops, Some(1));
    assert_eq!(info.budget_secs, None);

    // options_json carries the same facts for consumers that read it raw.
    let json: String = conn
        .query_row("SELECT options_json FROM analysis_run", [], |r| r.get(0))
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["solver_partial"], serde_json::json!(true));
    assert_eq!(value["solve_budget_pops"], serde_json::json!(1));
    assert_eq!(value["solve_budget_secs"], serde_json::Value::Null);
    assert!(value["solver_pops"].as_u64().unwrap() >= 1);

    // An `analyze`-stage warning diagnostic, unattached to any file.
    let (severity, file_id, line, stage, message): (String, Option<i64>, i64, String, String) = conn
        .query_row(
            "SELECT severity, file_id, line, stage, message FROM diagnostics WHERE stage = 'analyze'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(severity, "warning");
    assert_eq!(file_id, None);
    assert_eq!(line, 0);
    assert_eq!(stage, "analyze");
    assert!(message.contains("stopped early"), "{message}");
}

#[test]
fn inspect_flags_a_partial_database() {
    let tmp = TempDb::new("partial_inspect.db");
    let out = analyze_with_pop_budget(&tmp, "1");
    assert!(out.status.success());

    let out = Command::new(env!("CARGO_BIN_EXE_trace"))
        .args(["inspect", tmp.to_str().unwrap(), "calls"])
        .output()
        .expect("inspect runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("partial solve"), "{stderr}");
    // The query itself still answers against the partial database; a
    // truncated fixpoint legitimately may hold fewer (or no) edges.
    assert!(
        out.status.success(),
        "partial DB should still answer `calls`"
    );
}

#[test]
fn explicit_unlimited_pops_does_not_mark_partial() {
    let tmp = TempDb::new("full.db");
    let out = analyze_with_pop_budget(&tmp, "0");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("stopped before convergence"), "{stderr}");

    let conn = trace_db::open_db(&tmp).unwrap();
    let info = trace_db::solver_outcome(&conn).unwrap();
    assert!(!info.partial);
    assert_eq!(info.budget_pops, None);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM diagnostics WHERE stage = 'analyze'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "a converged run records no analyze diagnostic");
}
