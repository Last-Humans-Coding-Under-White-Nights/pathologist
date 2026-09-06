//! Inspect-mode integration tests: call graph + dataflow graph construction
//! from an exported database, plus end-to-end binary runs.

use std::path::PathBuf;
mod common;

use common::TempDb;
use std::process::Command;
use trace_analysis::analyze;
use trace_db::{
    call_graph, dataflow_graph, export_to_sqlite, find_functions_at, open_db, require_function_at,
    require_symbols_at, Direction, ExportOptions, QueryGraph,
};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

fn build_and_export(name: &str) -> TempDb {
    let root = fixture(name);
    let opts = PreprocessOptions::new()
        .with_include(root.clone())
        .with_include(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/include"),
        );
    let program = build_program(&root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    let out = TempDb::new(&format!("{name}.db"));
    export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &ExportOptions {
            output: out.to_path_buf(),
            trace_version: env!("CARGO_PKG_VERSION").to_owned(),
            include_points_to: false,
            full_detail: false,
            model_files: Vec::new(),
        },
    )
    .expect("export");
    out
}

/// Visited node names in BFS order (var nodes show their variable's name).
/// For value-flow graphs.
fn visited_names(conn: &rusqlite::Connection, g: &QueryGraph) -> Vec<String> {
    g.order
        .iter()
        .filter_map(|&(id, _)| {
            g.nodes.get(&id).map(|n| {
                let var_name: Option<String> = conn
                    .query_row(
                        "SELECT v.name FROM variables v JOIN flow_nodes n ON n.var_id = v.id WHERE n.id = ?1",
                        [id],
                        |r| r.get(0),
                    )
                    .ok();
                var_name.unwrap_or_else(|| n.label.clone())
            })
        })
        .collect()
}

/// Visited function names in BFS order. For call graphs.
fn fn_names(g: &QueryGraph) -> Vec<String> {
    g.order
        .iter()
        .filter_map(|&(id, _)| g.nodes.get(&id).map(|n| n.label.clone()))
        .collect()
}

#[test]
fn function_line_ranges_exported() {
    let db = build_and_export("static_direct_call");
    let conn = open_db(&db).unwrap();
    let rows: Vec<(String, i64, i64)> = conn
        .prepare("SELECT f.name, f.line_start, f.line_end FROM functions f ORDER BY f.line_start")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec![("helper".into(), 1, 3), ("caller".into(), 5, 7)]);
}

#[test]
fn callgraph_down_from_containing_line() {
    let db = build_and_export("static_direct_call");
    let conn = open_db(&db).unwrap();
    // Line 5 is inside caller's body.
    let start = require_function_at(&conn, "static_direct_call", 5).unwrap();
    assert_eq!(start.name, "caller");
    assert_eq!((start.line_start, start.line_end), (5, 7));

    // A file-static callee must resolve through scope-aware edges.
    let helper_id: i64 = conn
        .query_row("SELECT id FROM functions WHERE name = 'helper'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let g = call_graph(&conn, start.id, Direction::Down, 3).unwrap();
    assert!(
        g.edges
            .iter()
            .any(|e| e.to == helper_id && e.label == "direct"),
        "{:?}",
        g.edges
    );

    // Up from helper finds caller.
    let up = call_graph(&conn, helper_id, Direction::Up, 3).unwrap();
    assert_eq!(up.order.len(), 2);
    assert!(up.order.iter().any(|&(id, _)| id == start.id));
}

#[test]
fn indirect_call_up_edges_are_labeled_indirect() {
    let db = build_and_export("indirect_call");
    let conn = open_db(&db).unwrap();
    let run_id: i64 = conn
        .query_row("SELECT id FROM functions WHERE name = 'run'", [], |r| {
            r.get(0)
        })
        .unwrap();
    // Down from run reaches target through the fn-pointer call.
    let down = call_graph(&conn, run_id, Direction::Down, 5).unwrap();
    assert!(
        down.edges.iter().any(|e| e.label == "indirect"),
        "{:?}",
        down.edges
    );

    // Up from defined target shows the caller with the same annotation.
    let target_id: i64 = conn
        .query_row(
            "SELECT id FROM functions WHERE name = 'target' AND is_defined != 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let up = call_graph(&conn, target_id, Direction::Up, 5).unwrap();
    assert_eq!(fn_names(&up), vec!["target", "run"]);
    assert!(up.edges.iter().any(|e| e.label == "indirect"));
}

#[test]
fn dataflow_param_flows_to_callee_formal() {
    let db = build_and_export("static_direct_call");
    let conn = open_db(&db).unwrap();

    fn param_pos(conn: &rusqlite::Connection, name: &str) -> (i64, i64) {
        conn.query_row(
            "SELECT line, col FROM variables WHERE name = ?1 AND kind = 'param'",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    let (vline, vcol) = param_pos(&conn, "v");
    let syms = require_symbols_at(&conn, "static_direct_call", vline, vcol).unwrap();
    assert_eq!(syms[0].name, "v");

    // Down: the actual reaches helper's formal p.
    let down = dataflow_graph(&conn, &syms[..1], Direction::Down, 6).unwrap();
    let reached = visited_names(&conn, &down);
    assert!(
        reached.contains(&"p".to_string()),
        "formal p must be reachable from actual v; got {reached:?}"
    );

    // Up from formal p must contain actual v.
    let (pline, pcol) = param_pos(&conn, "p");
    let p_syms = require_symbols_at(&conn, "static_direct_call", pline, pcol).unwrap();
    assert_eq!(p_syms[0].name, "p");
    let up = dataflow_graph(&conn, &p_syms[..1], Direction::Up, 6).unwrap();
    let reached_up = visited_names(&conn, &up);
    assert!(
        reached_up.contains(&"v".to_string()),
        "actual v must be reachable backwards from p; got {reached_up:?}"
    );
}

#[test]
fn dataflow_indirect_call_param_and_fn_value() {
    // indirect_call/fn_ptr.c run(): `void (*fp)(int *) = &target; fp(&x);`
    let db = build_and_export("indirect_call");
    let conn = open_db(&db).unwrap();

    let (xline, xcol): (i64, i64) = conn
        .query_row(
            "SELECT line, col FROM variables WHERE name = 'x'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let syms = require_symbols_at(&conn, "fn_ptr.c", xline, xcol).unwrap();
    assert_eq!(syms[0].name, "x");

    // Down: x reaches target's formal p through the indirect call wiring.
    let down = dataflow_graph(&conn, &syms[..1], Direction::Down, 8).unwrap();
    assert!(
        visited_names(&conn, &down).contains(&"p".to_string()),
        "target's formal p must be reachable from actual x; got {:?}",
        visited_names(&conn, &down)
    );

    // Up from the local fn pointer fp finds the function value target.
    let (fpline, fpcol): (i64, i64) = conn
        .query_row(
            "SELECT line, col FROM variables WHERE name = 'fp' AND kind = 'local'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let fp_syms = require_symbols_at(&conn, "fn_ptr.c", fpline, fpcol).unwrap();
    assert_eq!(fp_syms[0].name, "fp");
    let up = dataflow_graph(&conn, &fp_syms[..1], Direction::Up, 8).unwrap();
    assert!(
        visited_names(&conn, &up)
            .iter()
            .any(|n| n.contains("fn:target")),
        "function value target must flow into fp; got {:?}",
        visited_names(&conn, &up)
    );
}

#[test]
fn end_to_end_binary_inspect_commands() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let tmp = TempDb::new("trace_e2e.db");

    // Analyze the fixture with the real binary.
    let out = Command::new(bin)
        .args([
            "analyze",
            fixture("static_direct_call").to_str().unwrap(),
            "-o",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("analyze runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // callgraph down from inside caller.
    let out = Command::new(bin)
        .args([
            "inspect",
            tmp.to_str().unwrap(),
            "callgraph",
            "--file",
            "static_direct_call",
            "--line",
            "5",
            "--depth",
            "2",
            "--direction",
            "down",
        ])
        .output()
        .expect("callgraph runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("callgraph from caller"), "{stdout}");
    assert!(stdout.contains("-direct-> helper"), "{stdout}");
    assert!(stdout.contains("(main.c:5)"), "{stdout}");

    // callgraph up from inside helper.
    let out = Command::new(bin)
        .args([
            "inspect",
            tmp.to_str().unwrap(),
            "callgraph",
            "--file",
            "static_direct_call",
            "--line",
            "2",
            "--direction",
            "up",
        ])
        .output()
        .expect("callgraph up runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("callers"), "{stdout}");
    assert!(stdout.contains("* helper"), "{stdout}");
    assert!(stdout.contains("caller"), "{stdout}");

    // dataflow from caller's param v (declared on line 4).
    let out = Command::new(bin)
        .args([
            "inspect",
            tmp.to_str().unwrap(),
            "dataflow",
            "--file",
            "static_direct_call",
            "--line",
            "4",
            "--col",
            "18",
        ])
        .output()
        .expect("dataflow runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dataflow for v"), "{stdout}");
    assert!(
        stdout.contains("-call_arg->") || stdout.contains("-copy->") || stdout.contains("-store->"),
        "{stdout}"
    );

    // Bad position errors cleanly.
    let out = Command::new(bin)
        .args([
            "inspect",
            tmp.to_str().unwrap(),
            "callgraph",
            "--file",
            "static_direct_call",
            "--line",
            "999",
        ])
        .output()
        .expect("bad position handled");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no function contains"), "{stderr}");
}

#[test]
fn direct_fixture_still_resolves() {
    let db = build_and_export("direct_call");
    let conn = open_db(&db).unwrap();
    let hits = find_functions_at(&conn, "main.c", 7).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].name, "main");
    let g = call_graph(&conn, hits[0].id, Direction::Down, 3).unwrap();
    assert_eq!(g.order.len(), 2, "main -> helper");
}

#[test]
fn inspect_calls_matches_cpp_qualified_suffix() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let tmp = TempDb::new("trace_inspect_suffix.db");
    let out = Command::new(bin)
        .args([
            "analyze",
            fixture("cpp_implicit_this").to_str().unwrap(),
            "-o",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("analyze runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(bin)
        .args([
            "inspect",
            tmp.to_str().unwrap(),
            "calls",
            "--to",
            "OnEventProxy",
        ])
        .output()
        .expect("inspect --to runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Plugin::OnEventProxy"),
        "--to OnEventProxy should match Plugin::OnEventProxy, got:\n{stdout}"
    );

    let out = Command::new(bin)
        .args([
            "inspect",
            tmp.to_str().unwrap(),
            "calls",
            "--from",
            "OnEventProxy",
        ])
        .output()
        .expect("inspect --from runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Plugin::OnEventProxy") && stdout.contains("Plugin::OnEvent"),
        "--from OnEventProxy should list implicit this->OnEvent, got:\n{stdout}"
    );
}

#[test]
fn inspect_calls_file_filter_preserves_call_site_semantics() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let db = build_and_export("static_direct_call");
    {
        let conn = open_db(&db).unwrap();
        conn.execute(
            "INSERT INTO files (id, path, sha256) VALUES (999, '/defs/caller_only.c', 'caller')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE functions SET file_id = 999 WHERE name = 'caller'",
            [],
        )
        .unwrap();
    }

    let caller_only = Command::new(bin)
        .args([
            "inspect",
            db.to_str().unwrap(),
            "calls",
            "--file",
            "caller_only.c",
        ])
        .output()
        .expect("inspect caller definition filter");
    assert!(caller_only.status.success());
    assert!(
        caller_only.stdout.is_empty(),
        "ordinary edges must not match only their caller definition file: {}",
        String::from_utf8_lossy(&caller_only.stdout)
    );

    let call_site = Command::new(bin)
        .args(["inspect", db.to_str().unwrap(), "calls", "--file", "main.c"])
        .output()
        .expect("inspect call-site filter");
    assert!(call_site.status.success());
    assert!(
        String::from_utf8_lossy(&call_site.stdout).contains("caller"),
        "ordinary edge should still match its call-site file"
    );
}

#[test]
fn inspect_calls_file_filter_uses_synthetic_caller_file() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let db = build_and_export("ipc_basic");
    {
        let conn = open_db(&db).unwrap();
        conn.execute(
            "INSERT INTO files (id, path, sha256) VALUES (999, '/defs/proxy_only.cpp', 'proxy')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE functions SET file_id = 999 WHERE name LIKE 'IFooProxy::%'",
            [],
        )
        .unwrap();
    }

    let output = Command::new(bin)
        .args([
            "inspect",
            db.to_str().unwrap(),
            "calls",
            "--file",
            "proxy_only.cpp",
        ])
        .output()
        .expect("inspect synthetic caller filter");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("IFooProxy::GetInfo") && stdout.contains("(ipc)"),
        "synthetic edge should match its caller definition file: {stdout}"
    );
}

#[test]
fn inspect_calls_like_wildcards_are_literal() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let tmp = TempDb::new("trace_inspect_like.db");
    let out = Command::new(bin)
        .args([
            "analyze",
            fixture("cpp_implicit_this").to_str().unwrap(),
            "-o",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("analyze runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(bin)
        .args(["inspect", tmp.to_str().unwrap(), "calls", "--from", "f_o"])
        .output()
        .expect("inspect --from f_o");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("ns::foo"),
        "--from f_o must not match ns::foo via LIKE '_', got:\n{stdout}"
    );

    let out = Command::new(bin)
        .args([
            "inspect",
            tmp.to_str().unwrap(),
            "calls",
            "--from",
            "foo_bar",
        ])
        .output()
        .expect("inspect --from foo_bar");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ns::foo_bar") && stdout.contains("-> ns::foo ["),
        "--from foo_bar should match ns::foo_bar -> ns::foo, got:\n{stdout}"
    );
}
