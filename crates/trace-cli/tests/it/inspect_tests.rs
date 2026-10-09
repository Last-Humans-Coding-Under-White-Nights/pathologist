//! Inspect-mode integration tests: call graph + dataflow graph construction
//! from an exported database, plus end-to-end binary runs.

use crate::common;
use std::path::PathBuf;

use common::TempDb;
use std::process::Command;
use trace_analysis::analyze;
use trace_db::{
    call_edges, call_graph, dataflow_graph, export_to_sqlite, find_functions_at,
    find_functions_by_name, open_db, require_function_at, require_symbols_at, CallEdgeFilter,
    Direction, ExportOptions, QueryGraph, SymbolRef,
};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

fn build_and_export(name: &str) -> TempDb {
    export_tree(&fixture(name), name)
}

/// Build the tree at `root` and export it minimally to a scratch database.
fn export_tree(root: &std::path::Path, name: &str) -> TempDb {
    export_tree_with(root, name, false)
}

/// Build the tree at `root` and export it, with full detail or minimally.
fn export_tree_with(root: &std::path::Path, name: &str, full_detail: bool) -> TempDb {
    let root = root.to_path_buf();
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
            full_detail,
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

/// Synthesized externals (never declared in the tree) must not present an
/// arbitrary call site as their own source location: `functions` exports
/// line 0 for them, call edges report no callee path, and the call-graph
/// node renders as a bare `[external]`. Prototype-only externals keep their
/// real declaration location.
#[test]
fn external_callees_carry_no_fabricated_location() {
    let db = build_and_export("extern_call");
    let conn = open_db(&db).unwrap();

    let no_location = |name: &str| -> Result<(i64, i64), _> {
        conn.query_row(
            "SELECT f.line_start, f.line_end FROM functions f WHERE f.name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    };
    // `undeclared_stub` is synthesized (no tree declaration); `ext_helper`
    // is a real prototype whose location must survive.
    assert_eq!(no_location("undeclared_stub").unwrap(), (0, 0));
    assert!(no_location("ext_helper").unwrap().0 > 0);

    let edges = call_edges(
        &conn,
        &CallEdgeFilter {
            from: None,
            to: None,
            file: None,
            exclude_deps: false,
        },
    )
    .unwrap();
    let by_callee = |name: &str| edges.iter().find(|e| e.callee_name == name).unwrap();
    assert_eq!(by_callee("undeclared_stub").callee_path, None);
    assert!(by_callee("ext_helper").callee_path.is_some());
    assert_eq!(by_callee("undeclared_stub").resolution, "external");

    // Browsing the call graph down from `local_wrap` (main.c:12) must show
    // the synthesized callee without a made-up file:line, both in the node
    // detail and in the text the real binary prints (the label closure lives
    // in the binary crate, so only running it pins the rendered form).
    let start = require_function_at(&conn, "main.c", 12).unwrap();
    let g = call_graph(&conn, start.id, Direction::Down, 3).unwrap();
    let external_node = g
        .nodes
        .values()
        .find(|n| n.label == "undeclared_stub")
        .unwrap();
    assert_eq!(external_node.detail, "[external]", "{:?}", external_node);

    let out = Command::new(env!("CARGO_BIN_EXE_trace"))
        .args([
            "inspect",
            db.to_str().unwrap(),
            "callgraph",
            "--file",
            "main.c",
            "--line",
            "12",
        ])
        .output()
        .expect("callgraph runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("-external-> undeclared_stub ([external]) (main.c:12)"),
        "rendered call graph must mark the external without a location:\n{text}"
    );
    assert!(!text.contains("main.c:0"), "{text}");
}

/// The 0-sentinel must not become a queryable location: `--line 0` is not a
/// source line, so it resolves to nothing (matching master), and a `--file`
/// filter must not match a synthesized external through its arbitrary stored
/// call-site file.
#[test]
fn locationless_externals_are_not_matched_by_file_or_line_filters() {
    let db = build_and_export("extern_call");
    let conn = open_db(&db).unwrap();

    assert!(
        find_functions_at(&conn, "main.c", 0).unwrap().is_empty(),
        "line 0 must not match a location-less external"
    );
    assert!(
        require_function_at(&conn, "main.c", 0).is_err(),
        "`callgraph --line 0` must fail like master"
    );

    // Findable by name, but not by an unrelated file.
    assert_eq!(
        find_functions_by_name(&conn, "undeclared_stub", None)
            .unwrap()
            .len(),
        1
    );
    assert!(
        find_functions_by_name(&conn, "undeclared_stub", Some("main.c"))
            .unwrap()
            .is_empty(),
        "a synthesized external is in no file, so --file must not match it"
    );
    // A prototype-only external keeps matching its real declaration file.
    assert_eq!(
        find_functions_by_name(&conn, "ext_helper", Some("main.c"))
            .unwrap()
            .len(),
        1
    );
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
    assert!(stdout.contains("→ helper::p [var#"), "{stdout}");
    assert!(stdout.contains("pass argument [fn#"), "{stdout}");
    assert!(!stdout.contains("call#"), "{stdout}");
    assert!(stdout.contains("reached p [var#"), "{stdout}");

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

#[test]
fn inspect_chains_direct_and_indirect() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let db_chain = build_and_export("header_chain");

    // Depth 1 cannot reach ChainTarget
    let out = Command::new(bin)
        .args([
            "inspect",
            db_chain.to_str().unwrap(),
            "callchain",
            "--from",
            "user",
            "--to",
            "ChainTarget",
            "--depth",
            "1",
        ])
        .output()
        .expect("inspect callchain depth 1");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no call chain found from user to ChainTarget within depth 1"),
        "expected no chain at depth 1, got:\n{stdout}"
    );

    // Depth 2 finds the chain: user -> BCaller -> ChainTarget
    let out = Command::new(bin)
        .args([
            "inspect",
            db_chain.to_str().unwrap(),
            "callchain",
            "--from",
            "user",
            "--to",
            "ChainTarget",
            "--depth",
            "2",
        ])
        .output()
        .expect("inspect callchain depth 2");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Chain 1 (depth 2):"),
        "expected Chain 1 (depth 2), got:\n{stdout}"
    );
    assert!(stdout.contains("-direct-> BCaller"));
    assert!(stdout.contains("-direct-> ChainTarget"));
    assert!(stdout.contains("1 chain found"));

    // JSON format output
    let out_json = Command::new(bin)
        .args([
            "inspect",
            db_chain.to_str().unwrap(),
            "callchain",
            "--from",
            "user",
            "--to",
            "ChainTarget",
            "--depth",
            "2",
            "--format",
            "json",
        ])
        .output()
        .expect("inspect callchain json");
    assert!(out_json.status.success());
    let stdout_json = String::from_utf8_lossy(&out_json.stdout);
    assert!(
        stdout_json.contains("\"title\": \"call chains from user to ChainTarget (depth <= 2)\"")
    );
    assert!(stdout_json.contains("\"direction\": \"down\""));
    assert!(stdout_json.contains("\"label\": \"BCaller"));
    assert!(stdout_json.contains("\"label\": \"ChainTarget"));

    // Direction up: from ChainTarget up to user
    let out_up = Command::new(bin)
        .args([
            "inspect",
            db_chain.to_str().unwrap(),
            "callchain",
            "--from",
            "ChainTarget",
            "--to",
            "user",
            "--direction",
            "up",
            "--depth",
            "2",
        ])
        .output()
        .expect("inspect callchain up");
    assert!(out_up.status.success());
    let stdout_up = String::from_utf8_lossy(&out_up.stdout);
    assert!(stdout_up.contains("Chain 1 (depth 2):"));
    assert!(stdout_up.contains("<-direct- BCaller"));
    assert!(stdout_up.contains("<-direct- user"));

    // Indirect call fixture: run -> target
    let db_indirect = build_and_export("indirect_call");
    let out_ind = Command::new(bin)
        .args([
            "inspect",
            db_indirect.to_str().unwrap(),
            "callchain",
            "--from",
            "run",
            "--to",
            "target",
            "--depth",
            "1",
        ])
        .output()
        .expect("inspect indirect callchain");
    assert!(out_ind.status.success());
    let stdout_ind = String::from_utf8_lossy(&out_ind.stdout);
    assert!(stdout_ind.contains("Chain 1 (depth 1):"));
    assert!(stdout_ind.contains("-indirect-> target"));

    // File:line syntax resolution
    let out_pos = Command::new(bin)
        .args([
            "inspect",
            db_chain.to_str().unwrap(),
            "chains",
            "--from",
            "main.c:5",
            "--to",
            "main.c:3",
            "--depth",
            "3",
        ])
        .output()
        .expect("inspect chains by file:line");
    assert!(out_pos.status.success());
    let stdout_pos = String::from_utf8_lossy(&out_pos.stdout);
    assert!(stdout_pos.contains("Chain 1 (depth 2):"));
    assert!(stdout_pos.contains("-direct-> ChainTarget"));
}

#[test]
fn inspect_callchain_edge_cases() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
void target(void) {}
void skip(void) { target(); }
void keep(void) { target(); }
int main(void) {
    skip();
    keep();
    return 0;
}
"#;
    std::fs::write(dir.path().join("main.c"), src).unwrap();
    let filter_file = dir.path().join("filter.json");
    std::fs::write(&filter_file, r#"{"functions": ["keep"]}"#).unwrap();

    let db = TempDb::new("callchain_edge_cases.db");
    let out = Command::new(bin)
        .args([
            "analyze",
            dir.path().to_str().unwrap(),
            "-o",
            db.to_str().unwrap(),
        ])
        .output()
        .expect("analyze runs");
    assert!(out.status.success());

    // 1. Without filter and limit 1: 2 paths exist (main->skip->target, main->keep->target),
    // so limit 1 must truncate.
    let out_lim1 = Command::new(bin)
        .args([
            "inspect",
            db.to_str().unwrap(),
            "callchain",
            "--from",
            "main",
            "--to",
            "target",
            "--limit",
            "1",
        ])
        .output()
        .expect("inspect callchain limit 1");
    assert!(out_lim1.status.success());
    let stdout_lim1 = String::from_utf8_lossy(&out_lim1.stdout);
    assert!(stdout_lim1.contains("1 chain found"));
    assert!(stdout_lim1.contains("(truncated at limit; increase --limit or --depth to see more)"));

    // 2. Filter ^keep$ with limit 1: if main->skip->target was discovered first,
    // filtering leaves 0 results, but truncation warning must be preserved!
    let out_filt_lim1 = Command::new(bin)
        .args([
            "inspect",
            db.to_str().unwrap(),
            "callchain",
            "--from",
            "main",
            "--to",
            "target",
            "--limit",
            "1",
            "--callgraph-filter",
            filter_file.to_str().unwrap(),
        ])
        .output()
        .expect("inspect callchain filtered limit 1");
    assert!(out_filt_lim1.status.success());
    let stdout_filt_lim1 = String::from_utf8_lossy(&out_filt_lim1.stdout);
    assert!(stdout_filt_lim1.contains("no call chain found from main to target within depth 5"));
    assert!(
        stdout_filt_lim1.contains("(truncated at limit; increase --limit or --depth to see more)")
    );

    // 3. With limit 0, keep path is found
    let out_filt_lim0 = Command::new(bin)
        .args([
            "inspect",
            db.to_str().unwrap(),
            "callchain",
            "--from",
            "main",
            "--to",
            "target",
            "--limit",
            "0",
            "--callgraph-filter",
            filter_file.to_str().unwrap(),
        ])
        .output()
        .expect("inspect callchain filtered limit 0");
    assert!(out_filt_lim0.status.success());
    let stdout_filt_lim0 = String::from_utf8_lossy(&out_filt_lim0.stdout);
    assert!(stdout_filt_lim0.contains("keep"));
    assert!(stdout_filt_lim0.contains("1 chain found"));
}

/// Issue #127 review: the flow graph keeps each variable's edge to its own
/// storage location — the connectivity a dataflow walk needs to go from a
/// value into `&x` — independently of which variables the solver seeds.
#[test]
fn dataflow_passes_through_an_address_taken_local() {
    let db = build_and_export("addr_taken_local");
    let conn = open_db(&db).unwrap();
    let (line, col): (i64, i64) = conn
        .query_row(
            "SELECT line, col FROM variables WHERE name = 'y'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let syms = require_symbols_at(&conn, "addr_taken_local", line, col).unwrap();
    assert_eq!(syms[0].name, "y");
    let down = dataflow_graph(&conn, &syms[..1], Direction::Down, 8).unwrap();
    let reached = visited_names(&conn, &down);
    for name in ["x", "ptr", "p"] {
        assert!(
            reached.contains(&name.to_string()),
            "{name} unreachable from y; got {reached:?}"
        );
    }
}

/// #142 review: a global nothing reads or writes has no flow node, but the
/// minimal export still lists it, so inspect finds it by its own declaration
/// and says it has no flow rather than answering for a neighbour.
#[test]
fn a_flowless_global_is_found_as_itself() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("unused.c"),
        "int *neighbour;\n\
         int unused_global;\n\
         void f(void) { static int x; neighbour = &x; }\n",
    )
    .unwrap();
    let db = export_tree(dir.path(), "unused");
    let conn = open_db(db.path()).unwrap();
    let syms = require_symbols_at(&conn, "unused.c", 2, 5).unwrap();
    assert_eq!(syms[0].name, "unused_global");
    let err = dataflow_graph(&conn, &syms[..1], Direction::Down, 3).unwrap_err();
    assert!(err.to_string().contains("unused_global"), "{err}");
}

// --- cpp_smart_pointer_value_flow: exported dataflow (#141) ---

/// The symbol for variable `name` declared in function `func`, found the
/// way the CLI finds it: by the coordinates the variables table records.
/// The name must be unique in `func`, so a fixture that later declares a
/// second one fails here instead of silently testing either.
fn symbol_in(conn: &rusqlite::Connection, func: &str, name: &str) -> SymbolRef {
    let positions: Vec<(String, i64, i64)> = conn
        .prepare(
            "SELECT fl.path, v.line, v.col FROM variables v \
             JOIN functions f ON f.id = v.fn_id JOIN files fl ON fl.id = v.file_id \
             WHERE f.name = ?1 AND v.name = ?2",
        )
        .unwrap()
        .query_map([func, name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(positions.len(), 1, "{func}::{name}: {positions:?}");
    let (path, line, col) = &positions[0];
    require_symbols_at(conn, path, *line, *col)
        .unwrap()
        .into_iter()
        .find(|s| s.name == name && s.fn_name.as_deref() == Some(func))
        .unwrap_or_else(|| panic!("{func}::{name} at {path}:{line}:{col}"))
}

#[test]
fn smart_pointer_dataflow_exports_unwrap_edges() {
    let root = fixture("cpp_smart_pointer_value_flow");
    for full_detail in [false, true] {
        let db = export_tree_with(&root, "smart_pointer_unwrap", full_detail);
        let conn = open_db(&db).unwrap();
        let unwraps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM flow_edges e \
                 JOIN flow_nodes s ON s.id = e.src_node JOIN variables sv ON sv.id = s.var_id \
                 JOIN flow_nodes d ON d.id = e.dst_node JOIN variables dv ON dv.id = d.var_id \
                 JOIN functions f ON f.id = sv.fn_id \
                 WHERE e.kind = 'unwrap' AND f.name = 'read' AND sv.name = 'sp' \
                   AND dv.name LIKE '\\_recv%' ESCAPE '\\'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unwraps, 2, "full={full_detail}: sp->value and sp->cb");

        let sp = symbol_in(&conn, "read", "sp");
        let down = dataflow_graph(&conn, &[sp], Direction::Down, 12).unwrap();
        assert!(
            down.edges.iter().any(|e| e.label == "unwrap"),
            "full={full_detail}: the inspector labels the crossing unwrap"
        );
        assert!(
            visited_names(&conn, &down).contains(&"value".to_string()),
            "full={full_detail}: sp reaches value"
        );
    }
}

/// Whether `names` has `want`, or a name starting with `want`'s stem when
/// it ends in `*` (generated temporaries: `_recv*`, `_gep*`, `_load*`).
fn has_node(names: &[String], want: &str) -> bool {
    match want.strip_suffix('*') {
        Some(stem) => names.iter().any(|name| name.starts_with(stem)),
        None => names.iter().any(|name| name == want),
    }
}

/// A dataflow walk the fixture must support: from `var` in `func`, in `dir`,
/// reaching every node in `nodes` over edges including every kind in `kinds`.
struct DataflowRow {
    func: &'static str,
    var: &'static str,
    dir: Direction,
    nodes: &'static [&'static str],
    kinds: &'static [&'static str],
}

#[test]
fn smart_pointer_dataflow_connects_wrappers_and_promotions() {
    let rows = [
        DataflowRow {
            func: "read",
            var: "sp",
            dir: Direction::Down,
            nodes: &["_recv*", "_gep*", "value"],
            kinds: &["unwrap", "gep", "load"],
        },
        DataflowRow {
            func: "promote_local",
            var: "wp",
            dir: Direction::Down,
            nodes: &["promoted", "promoted_value"],
            kinds: &["copy", "unwrap"],
        },
        DataflowRow {
            func: "promote_local",
            var: "promoted",
            dir: Direction::Up,
            nodes: &["wp"],
            kinds: &["copy"],
        },
        DataflowRow {
            func: "promote_field",
            var: "msg",
            dir: Direction::Down,
            nodes: &["_gep*", "_load*", "field_promoted", "field_value"],
            kinds: &["gep", "load", "copy", "unwrap"],
        },
        DataflowRow {
            func: "promote_field",
            var: "field_promoted",
            dir: Direction::Up,
            nodes: &["_load*", "_gep*", "msg"],
            kinds: &["copy", "load", "gep"],
        },
        DataflowRow {
            func: "promote_into_field",
            var: "wp",
            dir: Direction::Down,
            nodes: &["_gep*"],
            kinds: &["store"],
        },
    ];
    let root = fixture("cpp_smart_pointer_value_flow");
    for full_detail in [false, true] {
        let db = export_tree_with(&root, "smart_pointer_dataflow", full_detail);
        let conn = open_db(&db).unwrap();
        for row in &rows {
            let (func, var, dir) = (row.func, row.var, row.dir);
            let start = symbol_in(&conn, func, var);
            let graph = dataflow_graph(&conn, &[start], dir, 12).unwrap();
            let names = visited_names(&conn, &graph);
            for node in row.nodes {
                assert!(
                    has_node(&names, node),
                    "full={full_detail} {func}::{var} {dir:?}: missing {node} in {names:?}"
                );
            }
            for kind in row.kinds {
                assert!(
                    graph.edges.iter().any(|e| e.label == *kind),
                    "full={full_detail} {func}::{var} {dir:?}: no {kind} edge"
                );
            }
        }
        // The direct field read is visible at the CLI's default depth.
        let sp = symbol_in(&conn, "read", "sp");
        let shallow = dataflow_graph(&conn, &[sp], Direction::Down, 3).unwrap();
        assert!(
            has_node(&visited_names(&conn, &shallow), "value"),
            "full={full_detail}"
        );
    }
}

#[test]
fn smart_pointer_dataflow_cli_reports_both_directions() {
    for export_flag in [None, Some("--full-export")] {
        cli_reports_both_directions(export_flag);
    }
}

fn cli_reports_both_directions(export_flag: Option<&str>) {
    let bin = env!("CARGO_BIN_EXE_trace");
    let db = TempDb::new("smart_pointer_cli.db");
    let fixture = fixture("cpp_smart_pointer_value_flow");
    let out = Command::new(bin)
        .args([
            "analyze",
            fixture.to_str().unwrap(),
            "-o",
            db.to_str().unwrap(),
        ])
        .args(export_flag)
        .output()
        .expect("analyze runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let conn = open_db(&db).unwrap();
    for (func, var, direction, reached) in [
        ("read", "sp", "down", "value"),
        ("promote_field", "field_promoted", "up", "msg"),
    ] {
        let start = symbol_in(&conn, func, var);
        let (line, col) = (start.line.to_string(), start.col.to_string());
        let out = Command::new(bin)
            .args([
                "inspect",
                db.to_str().unwrap(),
                "dataflow",
                "--file",
                &start.path,
                "--line",
                &line,
                "--col",
                &col,
                "--direction",
                direction,
            ])
            .output()
            .expect("dataflow runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains(&format!("{reached} [var#")),
            "{export_flag:?} {func}::{var} {direction} must reach {reached}:\n{text}"
        );
    }
}

#[test]
fn inspect_callgraph_up_for_implicit_member_pointer_fn_ptr() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let tmp = TempDb::new("trace_inspect_implicit_fn_ptr.db");
    let out = Command::new(bin)
        .args([
            "analyze",
            fixture("cpp_implicit_fn_ptr").to_str().unwrap(),
            "--full-export",
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
            "callgraph",
            "--file",
            "main.cpp",
            "--line",
            "5",
            "--direction",
            "up",
        ])
        .output()
        .expect("inspect callgraph up runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Dispatcher::Dispatch"),
        "inspect callgraph up should report Dispatcher::Dispatch as caller of target_callback, got:\n{stdout}"
    );
}

/// Issue #151 (docs/ANALYSIS.md, "Dereferenced operands"): what `dataflow`
/// reports down from `func`'s locals `starts`, uncut, as value-flow
/// `(from, to, label)` edges for [`common::flow_chain`].
fn dataflow_down(
    conn: &rusqlite::Connection,
    func: &str,
    starts: &[&str],
) -> Vec<(i64, i64, String)> {
    let starts: Vec<_> = starts
        .iter()
        .map(|name| symbol_in(conn, func, name))
        .collect();
    let graph = dataflow_graph(conn, &starts, Direction::Down, 12).unwrap();
    assert!(!graph.truncated, "{func}: dataflow is cut off at depth 12");
    (graph.edges.into_iter())
        .map(|e| (e.from, e.to, e.label))
        .collect()
}

/// The value-flow nodes of `func`'s local `name`.
fn flow_nodes_of(conn: &rusqlite::Connection, func: &str, name: &str) -> Vec<i64> {
    conn.prepare("SELECT id FROM flow_nodes WHERE var_id = ?1 AND kind = 'var'")
        .unwrap()
        .query_map([symbol_in(conn, func, name).var_id], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Issue #151 guard: `dataflow` shows `o->inner->pp` reading through two
/// field projections off `o`.
#[test]
fn dataflow_of_an_arrow_root_guard() {
    let root = fixture("deref_member_reads");
    for full_detail in [false, true] {
        let db = export_tree_with(&root, "deref_member_reads", full_detail);
        let conn = open_db(&db).unwrap();
        let edges = dataflow_down(&conn, "read_arrow_root", &["o"]);
        let o = flow_nodes_of(&conn, "read_arrow_root", "o");
        let y = flow_nodes_of(&conn, "read_arrow_root", "arrow_y");
        let mut read = common::flow_chain(&edges, &o, &["gep", "gep", "load"]);
        read.extend(common::flow_chain(
            &edges,
            &o,
            &["gep", "load", "gep", "load"],
        ));
        assert!(
            y.iter().any(|n| read.contains(n)),
            "full={full_detail}: {edges:?}"
        );
    }
}

/// Issue #151: `dataflow` shows `*h->pp` loading the `pp` member's value,
/// then through it, with no load edge straight from `h`.
#[test]
fn dataflow_of_a_dereferenced_member_reads_the_member() {
    let root = fixture("deref_member_reads");
    for full_detail in [false, true] {
        let db = export_tree_with(&root, "deref_member_reads", full_detail);
        let conn = open_db(&db).unwrap();
        for prefix in ["", "cpp_"] {
            let func = format!("{prefix}read_member");
            let edges = dataflow_down(&conn, &func, &["h"]);
            let h = flow_nodes_of(&conn, &func, "h");
            let y = flow_nodes_of(&conn, &func, &format!("{prefix}member_y"));
            let loaded = common::flow_chain(&edges, &h, &["gep", "load", "load"]);
            let what = format!("full={full_detail} {func}: {edges:?}");
            assert!(y.iter().any(|n| loaded.contains(n)), "{what}");
            let direct = |(from, to, label): &(i64, i64, String)| {
                label == "load" && h.contains(from) && y.contains(to)
            };
            assert!(!edges.iter().any(direct), "{what}");
        }
    }
}

/// Issue #151: `dataflow` shows `*h->pp = v` storing through the `pp`
/// member's value, not into `h`.
#[test]
fn dataflow_of_a_member_write_stores_through_the_member() {
    let root = fixture("deref_member_reads");
    for full_detail in [false, true] {
        let db = export_tree_with(&root, "deref_member_reads", full_detail);
        let conn = open_db(&db).unwrap();
        let edges = dataflow_down(&conn, "write_member", &["h", "v"]);
        let h = flow_nodes_of(&conn, "write_member", "h");
        let v = flow_nodes_of(&conn, "write_member", "v");
        let stored = common::flow_chain(&edges, &v, &["store"]);
        let member_value = common::flow_chain(&edges, &h, &["gep", "load"]);
        let what = format!("full={full_detail}: {edges:?}");
        assert!(!stored.is_disjoint(&member_value), "{what}");
        assert!(!h.iter().any(|n| stored.contains(n)), "{what}");
    }
}

// Source-level presentation uses the same canonical records as raw inspect.
#[test]
fn source_dataflow_reference_identity_operations_calls_and_formats() {
    use std::collections::BTreeSet;
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let db = build_and_export("dataflow_presentation");
    let conn = open_db(&db).unwrap();
    let start = |file: &str, needle: &str, name: &str| {
        let source = std::fs::read_to_string(fixture("dataflow_presentation").join(file)).unwrap();
        let (line, text) = source
            .lines()
            .enumerate()
            .find(|(_, s)| s.contains(needle))
            .unwrap();
        let col = text.find(name).unwrap() + 1;
        require_symbols_at(&conn, file, line as i64 + 1, col as i64)
            .unwrap()
            .remove(0)
    };
    let assigned = start("cases.cpp", "char *assigned = payload;", "assigned");
    let view = dataflow_view(&conn, &[assigned], Direction::Down, 16).unwrap();
    let meta = GraphMeta {
        title: "possible flows",
        direction: "down",
        depth: 16,
        summary: "",
    };
    let text = render_dataflow(&view, RenderFormat::Text, &meta);
    assert!(text.contains("assigned [var#"));
    assert!(text.contains("copied [var#"));
    assert!(text.contains("write field"));
    assert!(text.contains("envelope"));
    assert!(text.contains("return value"));
    assert!(text.contains("indirect call"));
    assert!(!text.contains("_gep"));
    assert!(!text.contains("points_to"));
    assert!(view
        .edges
        .iter()
        .any(|e| e.location.line == 21 && e.operations.iter().any(|o| o == "assign")));
    for direction in [Direction::Up, Direction::Down] {
        for (file, needle, name) in [
            ("main.c", "struct Client *receiver = &client;", "receiver"),
            (
                "main.c",
                "char *forwarded = identity(payload);",
                "forwarded",
            ),
            (
                "cases.cpp",
                "static void dispatch(char *payload = default_payload,",
                "payload",
            ),
            ("cases.cpp", "char **out = &default_output)", "out"),
            (
                "cases.cpp",
                "static void optional_pointer(char *payload = nullptr)",
                "payload",
            ),
            ("a.cpp", "static char *saved;", "saved"),
            ("b.cpp", "static char *saved;", "saved"),
            (
                "cases.cpp",
                "char *overloaded_result = dispatch(&envelope);",
                "overloaded_result",
            ),
            (
                "cases.cpp",
                "char *namespaced_result = alternate::dispatch(second);",
                "namespaced_result",
            ),
            (
                "cases.cpp",
                "char *recursive_result = recursive_identity(first, 2);",
                "recursive_result",
            ),
        ] {
            let view = dataflow_view(&conn, &[start(file, needle, name)], direction, 16).unwrap();
            let direction_meta = GraphMeta {
                direction: if direction == Direction::Up {
                    "up"
                } else {
                    "down"
                },
                ..meta
            };
            for format in [
                RenderFormat::Text,
                RenderFormat::Json,
                RenderFormat::Graphviz,
                RenderFormat::Mermaid,
            ] {
                let output = render_dataflow(&view, format, &direction_meta);
                assert!(!output.contains("_gep"), "{file} {name}: {output}");
                assert!(!output.contains("call_site_id"), "{output}");
                assert!(!output.contains("call#"), "{output}");
                if format == RenderFormat::Json {
                    let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
                    assert_eq!(doc["nodes"].as_array().unwrap().len(), view.nodes.len());
                    assert_eq!(doc["edges"].as_array().unwrap().len(), view.edges.len());
                } else if format == RenderFormat::Text {
                    assert!(!output.contains("call#"), "{output}");
                    for node in view.nodes.values().filter(|n| n.kind == "function") {
                        assert!(
                            !output.contains(&format!("{}::{} [fn#", node.name, node.name)),
                            "{output}"
                        );
                    }
                }
            }
        }
    }
    let incoming = dataflow_view(
        &conn,
        &[start(
            "cases.cpp",
            "static void dispatch(char *payload = default_payload,",
            "payload",
        )],
        Direction::Up,
        16,
    )
    .unwrap();
    let calls: BTreeSet<_> = incoming
        .edges
        .iter()
        .filter_map(|e| e.call_site_id)
        .collect();
    assert!(
        calls.len() >= 3,
        "each explicit payload call occurrence survives"
    );
    // Only the requested parameter appears when incoming traversal has no
    // dependency on the other argument positions.
    assert!(!incoming
        .nodes
        .values()
        .any(|n| n.name == "handler" || n.name == "out"));
    let header_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM functions WHERE name='header_identity'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(header_count, 1);
    let statics: Vec<i64> = conn
        .prepare("SELECT id FROM functions WHERE name='process' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(statics.len(), 2);
    assert_ne!(statics[0], statics[1]);
    let macro_view = dataflow_view(
        &conn,
        &[start(
            "macro_cases.cpp",
            "static void normal_receiver(char *payload)",
            "payload",
        )],
        Direction::Up,
        12,
    )
    .unwrap();
    let macro_calls: Vec<_> = macro_view
        .edges
        .iter()
        .filter(|e| e.spelling.is_some())
        .collect();
    assert_eq!(macro_calls.len(), 2);
    assert_ne!(macro_calls[0].call_site_id, macro_calls[1].call_site_id);
    assert!(macro_calls
        .iter()
        .all(|e| e.location.path.ends_with("macro_cases.cpp")
            && e.spelling.as_ref().unwrap().path.ends_with("macros.hpp")));
    let outer = start("macro_cases.cpp", "char *p = first;", "p =");
    let inner = start("macro_cases.cpp", "char *p = second;", "p =");
    assert_ne!(outer.var_id, inner.var_id);
    for (sym, destination) in [(outer, "outer_saved"), (inner, "inner_saved")] {
        let view = dataflow_view(&conn, &[sym], Direction::Down, 12).unwrap();
        assert!(view.nodes.values().any(|n| n.name == destination));
    }
    let generated = dataflow_view(
        &conn,
        &[start(
            "macro_cases.cpp",
            "static char *generated_saved;",
            "generated_saved",
        )],
        Direction::Up,
        12,
    )
    .unwrap();
    let scope = generated
        .scopes
        .values()
        .find(|s| s.name == "generated_receiver")
        .unwrap();
    assert!(scope.location.path.ends_with("macro_cases.cpp"));
    assert_eq!(scope.location.line, 8);
    // Missing default facts stay missing. Presentation must not fabricate them.
    let defaults = dataflow_view(
        &conn,
        &[start(
            "cases.cpp",
            "char *default_payload = default_text;",
            "default_payload",
        )],
        Direction::Down,
        16,
    )
    .unwrap();
    assert!(!defaults.edges.iter().any(|e| e.call_site_id.is_some()));
}

#[test]
fn source_dataflow_retains_assignment_sites_and_distinguishes_field_and_pointer_writes() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("main.c"),"struct Holder { char *field; char **slot; };\nvoid flow(char *payload, struct Holder *h, char **out) {\n  char *_ret_user = payload;\n  h->field = payload;\n  *h->slot = payload;\n  _ret_user = payload;\n  char *_gep_user = _ret_user;\n  *out = _gep_user;\n}\n").unwrap();
    let db = export_tree(tree.path(), "source_operations");
    let conn = open_db(&db).unwrap();
    let payload = symbol_in(&conn, "flow", "payload");
    let view = trace_db::dataflow_view(&conn, &[payload], Direction::Down, 8).unwrap();
    let real = view.nodes.values().find(|n| n.name == "_ret_user").unwrap();
    let sites: std::collections::BTreeSet<_> = view
        .edges
        .iter()
        .filter(|e| e.to == real.id && e.operations == ["assign"])
        .map(|e| e.location.line)
        .collect();
    assert_eq!(
        sites,
        std::collections::BTreeSet::from([3, 6]),
        "repeated assignments must retain both operation sites"
    );
    assert!(view.nodes.values().any(|n| n.name == "_gep_user"));
    assert!(view
        .edges
        .iter()
        .any(|e| e.operations.contains(&"write field".into())
            && e.expression == "h->field = payload"));
    assert!(
        view.edges
            .iter()
            .any(|e| e.operations.contains(&"write pointer".into())
                && e.expression == "*h->slot = payload"),
        "writing through a pointer-valued member is not assigning that member"
    );
    assert!(view
        .edges
        .iter()
        .any(|e| e.operations.contains(&"write pointer".into()) && e.location.line == 8));
}

#[test]
fn source_dataflow_groups_parameterless_returns_by_canonical_function_and_call_occurrence() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("main.c"),"char *saved;\nchar *no_parameters(void) { return saved; }\nvoid use(char *input) {\n  saved = input;\n  char *first = no_parameters();\n  char *second = no_parameters();\n}\n").unwrap();
    let db = export_tree(tree.path(), "return_occurrences");
    let conn = open_db(&db).unwrap();
    let view = trace_db::dataflow_view(
        &conn,
        &[symbol_in(&conn, "use", "input")],
        Direction::Down,
        8,
    )
    .unwrap();
    let scope = view
        .scopes
        .values()
        .find(|s| s.name == "no_parameters")
        .unwrap();
    let returns: Vec<_> = view
        .edges
        .iter()
        .filter(|e| e.callee_fn_id == scope.fn_id && e.operations.contains(&"return value".into()))
        .collect();
    assert_eq!(returns.len(), 2);
    assert!(returns.iter().all(|e| e.call_site_id.is_some()));
    assert_ne!(returns[0].call_site_id, returns[1].call_site_id);
    assert_eq!(
        returns
            .iter()
            .map(|e| e.location.line)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([5, 6])
    );
    let meta = trace_db::GraphMeta {
        title: "returns",
        direction: "down",
        depth: 8,
        summary: "",
    };
    let output = trace_db::render_dataflow(&view, trace_db::RenderFormat::Text, &meta);
    assert!(output.contains(&format!("no_parameters [fn#{}] ()", scope.fn_id.unwrap().0)));
    assert_eq!(output.matches("    return value at ").count(), 2);
    assert!(!output.contains("call#"), "{output}");
}

#[test]
fn source_dataflow_return_origins_preserve_unannotated_global_initializer() {
    for full_detail in [false, true] {
        let db = export_tree_with(
            &fixture("dataflow_global_return_origins"),
            "global_returns",
            full_detail,
        );
        let conn = open_db(&db).unwrap();
        let origins: Vec<(i64, String)> = conn.prepare(
            "SELECT DISTINCT line,expression FROM flow_origins WHERE operation='return value' ORDER BY line",
        ).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(origins.iter().map(|o| o.0).collect::<Vec<_>>(), [3, 5]);
        let calls: i64 = conn
            .query_row("SELECT count(*) FROM flow_return_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            calls, 1,
            "only the function assignment has a recorded call occurrence"
        );
        // Also cover the closest-position fallback for earlier v7 exports.
        for bound_origins in [true, false] {
            if !bound_origins {
                conn.execute_batch("DROP TABLE flow_call_origins").unwrap();
            }
            for (direction, line) in [(Direction::Down, 1), (Direction::Up, 3)] {
                let root = require_symbols_at(&conn, "main.cpp", line, 7)
                    .unwrap()
                    .remove(0);
                let view = trace_db::dataflow_view(&conn, &[root], direction, 1).unwrap();
                let returns: Vec<_> = view
                    .edges
                    .iter()
                    .filter(|e| e.operations == ["return value"])
                    .collect();
                assert_eq!(
                    returns.len(),
                    2,
                    "{full_detail}/{bound_origins}/{direction:?}"
                );
                for (line, expression) in &origins {
                    let edge = returns
                        .iter()
                        .find(|e| e.provenance.iter().any(|o| o.location.line == *line))
                        .unwrap();
                    assert_eq!(&edge.expression, expression);
                    assert_eq!(edge.call_site_id.is_some(), *line == 5);
                    assert_eq!(edge.callee_fn_id.is_some(), *line == 5);
                    assert_eq!(edge.provenance.len(), 1);
                    assert_eq!(edge.provenance[0].expression, *expression);
                }
            }
        }
    }
}

#[test]
fn source_dataflow_return_occurrences_are_unique_across_global_assignments() {
    use std::fmt::Write;
    let tree = tempfile::tempdir().unwrap();
    let mut source = String::from("char *source;\nchar *other;\nchar *dest;\nchar *get() { return source; }\nchar *get_other() { if (source) return source; return other; }\n");
    for i in 0..300 {
        writeln!(source, "void f{i}() {{ dest = get(); }}").unwrap();
    }
    // A different callee sharing the destination must keep both return sources.
    source.push_str("void another() { dest = get_other(); }\n");
    std::fs::write(tree.path().join("main.cpp"), source).unwrap();
    for full_detail in [false, true] {
        let db = export_tree_with(tree.path(), "shared_global_returns", full_detail);
        let conn = open_db(&db).unwrap();
        let (total, distinct): (i64,i64) = conn.query_row(
            "SELECT (SELECT count(*) FROM flow_return_calls), (SELECT count(*) FROM (SELECT DISTINCT src_node,dst_node,call_site_id,callee_fn_id FROM flow_return_calls))", [], |r| Ok((r.get(0)?,r.get(1)?)),
        ).unwrap();
        assert_eq!(distinct, 302);
        assert_eq!(
            total, distinct,
            "export work must not multiply occurrences by callers"
        );
        let wrong_source: i64 = conn.query_row(
            "SELECT count(*) FROM flow_return_calls r JOIN flow_nodes n ON n.id=r.src_node JOIN variables v ON v.id=n.var_id JOIN functions f ON f.id=r.callee_fn_id WHERE (f.name='get' AND v.name!='source') OR (f.name='get_other' AND v.name NOT IN ('source','other'))", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(wrong_source, 0);
        let root = require_symbols_at(&conn, "main.cpp", 3, 7)
            .unwrap()
            .remove(0);
        let view = trace_db::dataflow_view(&conn, &[root], Direction::Up, 1).unwrap();
        assert_eq!(
            view.edges
                .iter()
                .filter(|e| e.operations == ["return value"])
                .count(),
            302
        );
    }
}

#[test]
fn source_dataflow_text_follows_selected_scope_through_calls_and_returns() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let db = build_and_export("dataflow_presentation");
    let conn = open_db(&db).unwrap();
    for (direction, word, function, variable, expected) in [
        (
            Direction::Down,
            "down",
            "main",
            "payload",
            ["main", "dispatch", "identity"],
        ),
        (
            Direction::Up,
            "up",
            "dispatch",
            "forwarded",
            ["dispatch", "identity", "main"],
        ),
    ] {
        let view =
            dataflow_view(&conn, &[symbol_in(&conn, function, variable)], direction, 3).unwrap();
        let meta = GraphMeta {
            title: "flows",
            direction: word,
            depth: 3,
            summary: "",
        };
        let json = render_dataflow(&view, RenderFormat::Json, &meta);
        let text = render_dataflow(&view, RenderFormat::Text, &meta);
        let headings: Vec<_> = text.lines().filter(|line| line.ends_with(':')).collect();
        assert_eq!(headings.len(), 3, "{text}");
        for (heading, name) in headings.iter().zip(expected) {
            assert!(heading.starts_with(&format!("{name} [fn#")), "{text}");
        }
        assert_eq!(text.matches(" → ").count(), view.edges.len(), "{text}");
        assert!(
            text.contains(&format!("selected {variable} [var#")),
            "{text}"
        );
        assert!(text.contains("argument 2] at main.c:37:5"), "{text}");
        assert!(text.contains("argument 1] at main.c:29:23"), "{text}");
        let returned = text
            .lines()
            .find(|line| line.contains("    return value"))
            .unwrap();
        assert!(
            returned.find("value [var#").unwrap() < returned.find("forwarded [var#").unwrap(),
            "{text}"
        );
        assert_eq!(
            text.contains("truncated at visible depth limit"),
            view.truncated
        );
        assert_eq!(render_dataflow(&view, RenderFormat::Json, &meta), json);
    }
}

#[test]
fn source_dataflow_text_headers_map_arguments_to_all_canonical_parameters() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("main.c"),
        "void dispatch(int mode, char *payload) {\n    char *saved = payload;\n}\nvoid caller(char *payload) {\n    dispatch(0, payload);\n}\n").unwrap();
    for full_detail in [false, true] {
        let db = export_tree_with(tree.path(), "parameter_headers", full_detail);
        let conn = open_db(&db).unwrap();
        for (direction, word, root) in [
            (Direction::Down, "down", "caller"),
            (Direction::Up, "up", "dispatch"),
        ] {
            let mut view =
                dataflow_view(&conn, &[symbol_in(&conn, root, "payload")], direction, 1).unwrap();
            let scope = view.scopes.values().find(|s| s.name == "dispatch").unwrap();
            let callee = scope.fn_id.unwrap();
            assert_eq!(scope.parameters.len(), 2);
            let mode = &scope.parameters[0];
            let payload = &scope.parameters[1];
            assert_eq!(
                (&*mode.name, &*mode.type_name, mode.arg_index),
                ("mode", "int", 0)
            );
            assert_eq!(
                (&*payload.name, &*payload.type_name, payload.arg_index),
                ("payload", "char*", 1)
            );
            assert!(
                !view.nodes.values().any(|n| n.var_id == Some(mode.var_id)),
                "header metadata must not add nodes"
            );
            let meta = GraphMeta {
                title: "flows",
                direction: word,
                depth: 1,
                summary: "",
            };
            let text = render_dataflow(&view, RenderFormat::Text, &meta);
            assert!(text.contains(&format!("dispatch [fn#{}] (argument 1: mode: int [var#{}], argument 2: payload: char* [var#{}])", callee.0, mode.var_id.0, payload.var_id.0)), "{text}");
            let edge = view.edges.iter().find(|e| e.arg_index == Some(1)).unwrap();
            let src = &view.nodes[&edge.from];
            let dst = &view.nodes[&edge.to];
            let (src_label, dst_label) = if direction == Direction::Down {
                ("payload", "dispatch::payload")
            } else {
                ("caller::payload", "payload")
            };
            assert!(text.contains(&format!("{src_label} [var#{}] → {dst_label} [var#{}]    pass argument [fn#{}, argument 2] at main.c:5:5", src.var_id.unwrap().0, dst.var_id.unwrap().0, callee.0)), "{text}");
            assert!(!text.contains("call#"), "{text}");
            assert!(!text.contains("dispatch dispatch::payload"), "{text}");
            assert!(edge.call_site_id.is_some());
            let json = render_dataflow(&view, RenderFormat::Json, &meta);
            let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(doc["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|op| op["arg_index"] == 1)));
            for format in [
                RenderFormat::Json,
                RenderFormat::Graphviz,
                RenderFormat::Mermaid,
            ] {
                let output = render_dataflow(&view, format, &meta);
                assert!(!output.contains("call_site_id"), "{output}");
                assert!(!output.contains("call#"), "{output}");
            }
            for scope in view.scopes.values_mut() {
                scope.parameters.clear();
            }
            assert_eq!(
                render_dataflow(&view, RenderFormat::Json, &meta),
                json,
                "header metadata does not change JSON"
            );
        }
        conn.execute_batch("DROP TABLE flow_parameters").unwrap();
        let view = dataflow_view(
            &conn,
            &[symbol_in(&conn, "caller", "payload")],
            Direction::Down,
            1,
        )
        .unwrap();
        let text = render_dataflow(
            &view,
            RenderFormat::Text,
            &GraphMeta {
                title: "flows",
                direction: "down",
                depth: 1,
                summary: "",
            },
        );
        assert!(text.contains("(parameters unavailable)"), "{text}");
    }
}

#[test]
fn source_dataflow_text_parameter_positions_include_implicit_receivers() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(
        tree.path().join("main.cpp"),
        "struct S { void method(int unused, char *payload) { char *saved = payload; } };\n",
    )
    .unwrap();
    let db = export_tree(tree.path(), "receiver_header");
    let conn = open_db(&db).unwrap();
    let view = dataflow_view(
        &conn,
        &[symbol_in(&conn, "S::method", "payload")],
        Direction::Down,
        1,
    )
    .unwrap();
    let scope = view
        .scopes
        .values()
        .find(|s| s.name == "S::method")
        .unwrap();
    assert_eq!(
        scope
            .parameters
            .iter()
            .map(|p| (&*p.name, p.arg_index))
            .collect::<Vec<_>>(),
        [("this", 0), ("unused", 1), ("payload", 2)]
    );
    let text = render_dataflow(
        &view,
        RenderFormat::Text,
        &GraphMeta {
            title: "flows",
            direction: "down",
            depth: 1,
            summary: "",
        },
    );
    assert!(text.contains("argument 1: this:"), "{text}");
    assert!(
        text.contains(&format!(
            "argument 3: payload: char* [var#{}]",
            scope.parameters[2].var_id.0
        )),
        "{text}"
    );
}

#[test]
fn source_dataflow_recursive_call_keeps_assignment_with_the_same_endpoints() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let db = build_and_export("dataflow_provenance");
    let conn = open_db(&db).unwrap();
    for (direction, word, root) in [(Direction::Down, "down", "q"), (Direction::Up, "up", "p")] {
        let view = dataflow_view(&conn, &[symbol_in(&conn, "recur", root)], direction, 1).unwrap();
        let transitions: Vec<_> = view
            .edges
            .iter()
            .filter(|e| view.nodes[&e.from].name == "q" && view.nodes[&e.to].name == "p")
            .collect();
        assert_eq!(transitions.len(), 2, "{transitions:#?}");
        let assignment = transitions
            .iter()
            .find(|e| e.call_site_id.is_none())
            .unwrap();
        assert_eq!(assignment.operations, ["assign"]);
        assert_eq!(assignment.location.line, 3);
        assert_eq!(assignment.provenance[0].location.line, 3);
        let argument = transitions
            .iter()
            .find(|e| e.call_site_id.is_some())
            .unwrap();
        assert_eq!(argument.operations, ["pass argument"]);
        assert_eq!(argument.arg_index, Some(0));
        assert_eq!(argument.location.line, 4);
        let meta = GraphMeta {
            title: "flows",
            direction: word,
            depth: 1,
            summary: "",
        };
        let text = render_dataflow(&view, RenderFormat::Text, &meta);
        assert!(text.contains("assign at main.c:3:5"), "{text}");
        assert!(text.contains("pass argument [fn#"), "{text}");
        assert!(!text.contains("call#"), "{text}");
        assert_eq!(text.matches(" → ").count(), view.edges.len());
    }
}

#[test]
fn source_dataflow_explored_duplicate_constraint_keeps_both_assignment_sites() {
    use std::collections::BTreeSet;
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let root = fixture("dataflow_provenance");
    let opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let program = build_program(&root, &opts).unwrap();
    assert!(
        program.variants_merged > 0,
        "a conditional variant must actually merge"
    );
    let function = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "variant")
        .unwrap();
    let var = |name: &str| {
        program
            .symbols
            .variables
            .iter()
            .find(|v| v.fn_id == Some(function.id) && v.name == name)
            .unwrap()
            .id
    };
    let constraint = trace_ir::FlowConstraint::Copy {
        dst: var("q"),
        src: var("p"),
    };
    assert_eq!(
        program
            .flow
            .iter()
            .filter(|flow| **flow == constraint)
            .count(),
        1
    );
    let origins = &program.flow_origins[&constraint];
    assert_eq!(origins.len(), 2, "{origins:#?}");
    assert_eq!(
        origins
            .iter()
            .map(|(span, _)| span.line)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([10, 12])
    );
    let (pag, analysis) = analyze(&program);
    let db = TempDb::new("explored_provenance.db");
    export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &ExportOptions {
            output: db.to_path_buf(),
            trace_version: env!("CARGO_PKG_VERSION").to_owned(),
            include_points_to: false,
            full_detail: false,
            model_files: Vec::new(),
        },
    )
    .unwrap();
    let conn = open_db(&db).unwrap();
    let sites: Vec<i64> = conn
        .prepare("SELECT line FROM flow_origins WHERE line IN (10,12) ORDER BY line")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(sites, [10, 12]);
    for (direction, word, root) in [(Direction::Down, "down", "p"), (Direction::Up, "up", "q")] {
        let view =
            dataflow_view(&conn, &[symbol_in(&conn, "variant", root)], direction, 1).unwrap();
        assert_eq!(view.edges.len(), 2, "{:#?}", view.edges);
        assert_eq!(
            view.edges
                .iter()
                .map(|e| e.location.line)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([10, 12])
        );
        let meta = GraphMeta {
            title: "flows",
            direction: word,
            depth: 1,
            summary: "",
        };
        let text = render_dataflow(&view, RenderFormat::Text, &meta);
        assert!(text.contains("assign at main.c:10:5"), "{text}");
        assert!(text.contains("assign at main.c:12:5"), "{text}");
    }
}

#[test]
fn source_dataflow_indirect_returns_keep_sources_with_their_root_callees() {
    use std::collections::BTreeSet;
    for full_detail in [false, true] {
        let db = export_tree_with(&fixture("dataflow_export"), "indirect_returns", full_detail);
        let conn = open_db(&db).unwrap();
        let binding: (String, String) = conn
            .query_row(
                "SELECT callee.name,dst.name FROM call_sites cs
             JOIN variables callee ON callee.id=cs.callee_var
             JOIN variables dst ON dst.id=cs.return_dst WHERE dst.name='out'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(binding, ("fp".into(), "out".into()));
        let rows: Vec<(String, String)> = conn
            .prepare(
                "SELECT v.name,f.name FROM flow_return_calls r
             JOIN flow_nodes n ON n.id=r.src_node JOIN variables v ON v.id=n.var_id
             JOIN functions f ON f.id=r.callee_fn_id
             JOIN flow_nodes dst ON dst.id=r.dst_node JOIN variables out ON out.id=dst.var_id
             WHERE out.name='out' ORDER BY v.name,f.name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            Vec::from([
                ("a".into(), "fa".into()),
                ("a".into(), "fc".into()),
                ("b".into(), "fb".into()),
            ])
        );
        let view = trace_db::dataflow_view(
            &conn,
            &[symbol_in(&conn, "indirect_returns", "out")],
            Direction::Up,
            1,
        )
        .unwrap();
        let associations: BTreeSet<_> = view
            .edges
            .iter()
            .filter(|e| e.operations.contains(&"return value".into()))
            .map(|e| {
                (
                    view.nodes[&e.from].name.clone(),
                    view.scopes[&format!("fn:{}", e.callee_fn_id.unwrap().0)]
                        .name
                        .clone(),
                )
            })
            .collect();
        assert_eq!(associations, rows.into_iter().collect());
    }
}

#[test]
fn source_dataflow_field_write_is_visible_from_base_and_value_roots() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    for full_detail in [false, true] {
        let db = export_tree_with(&fixture("dataflow_field_root"), "field_root", full_detail);
        let conn = open_db(&db).unwrap();
        let mut destination = None;
        for (root, operation) in [("s", "access field"), ("p", "write field")] {
            let symbol = symbol_in(&conn, "f", root);
            let view =
                dataflow_view(&conn, std::slice::from_ref(&symbol), Direction::Down, 1).unwrap();
            assert_eq!(view.edges.len(), 1, "{root}: {view:#?}");
            let edge = &view.edges[0];
            assert_eq!(view.nodes[&edge.from].name, root);
            assert_eq!(edge.operations, [operation]);
            let field = &view.nodes[&edge.to];
            assert_eq!(field.kind, "expression");
            assert_eq!(field.name, "s->field");
            assert_eq!(field.location.line, 3);
            assert_eq!(field.depth, 1);
            assert!(!view.truncated);
            let identity = (field.id, field.name.clone(), field.location.clone());
            if let Some(previous) = &destination {
                assert_eq!(
                    previous, &identity,
                    "root-independent expression identity and location"
                );
            }
            destination = Some(identity);
            let meta = GraphMeta {
                title: "field write",
                direction: "down",
                depth: 1,
                summary: "",
            };
            for format in [
                RenderFormat::Text,
                RenderFormat::Json,
                RenderFormat::Graphviz,
                RenderFormat::Mermaid,
            ] {
                let output = render_dataflow(&view, format, &meta);
                let label = if format == RenderFormat::Mermaid {
                    field.name.replace('>', "&gt;")
                } else {
                    field.name.clone()
                };
                assert!(output.contains(&label), "{format:?}: {output}");
            }
            let shallow =
                dataflow_view(&conn, std::slice::from_ref(&symbol), Direction::Down, 0).unwrap();
            assert!(shallow.edges.is_empty());
            assert!(shallow.truncated);
            let up = dataflow_view(&conn, &[symbol], Direction::Up, 1).unwrap();
            assert!(
                up.edges.is_empty(),
                "field writes are not incoming edges to {root}"
            );
            assert!(!up.nodes.values().any(|n| n.kind == "expression"));
        }
    }
}

#[test]
fn source_dataflow_later_defined_field_write_retains_operation_origin() {
    for full_detail in [false, true] {
        let db = export_tree_with(&fixture("later_defined_init"), "later_field", full_detail);
        let conn = open_db(&db).unwrap();
        let symbols = require_symbols_at(&conn, "late.c", 9, 19).unwrap();
        assert_eq!(symbols[0].name, "g_tbl");
        let view = trace_db::dataflow_view(&conn, &symbols[..1], Direction::Down, 1).unwrap();
        let field = view
            .nodes
            .values()
            .find(|node| node.name == "g_tbl.init" && node.kind == "expression")
            .expect("deferred field write must be visible from the base");
        assert_eq!((field.location.line, field.location.col), (18, 5));
        let meta = trace_db::GraphMeta {
            title: "later field",
            direction: "down",
            depth: 1,
            summary: "",
        };
        for format in [
            trace_db::RenderFormat::Text,
            trace_db::RenderFormat::Json,
            trace_db::RenderFormat::Graphviz,
            trace_db::RenderFormat::Mermaid,
        ] {
            let output = trace_db::render_dataflow(&view, format, &meta);
            assert!(output.contains(&field.name), "{format:?}: {output}");
        }
        let upstream = trace_db::dataflow_view(&conn, &symbols[..1], Direction::Up, 1).unwrap();
        assert!(!upstream
            .nodes
            .values()
            .any(|node| node.kind == "expression" && node.name == "g_tbl.init"));

        assert!(view
            .edges
            .iter()
            .any(|edge| edge.to == field.id && edge.operations == ["access field"]));
        let origins: Vec<(i64, i64, String)> = conn.prepare(
            "SELECT line,col,expression FROM flow_origins WHERE operation='write field' ORDER BY line,col"
        ).unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
            .unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert!(origins
            .iter()
            .any(|(line, col, text)| *line == 18 && *col == 5 && text == "g_tbl.init = LaterBody"));
    }
}

#[test]
fn source_dataflow_aggregate_initializers_name_field_destinations() {
    use std::collections::BTreeSet;
    for full_detail in [false, true] {
        let db = export_tree_with(
            &fixture("dataflow_export"),
            "field_destinations",
            full_detail,
        );
        let conn = open_db(&db).unwrap();
        let view = trace_db::dataflow_view(
            &conn,
            &[symbol_in(&conn, "initializers", "v")],
            Direction::Down,
            1,
        )
        .unwrap();
        let destinations: BTreeSet<_> = view
            .edges
            .iter()
            .filter(|e| e.operations.contains(&"write field".into()))
            .map(|e| (view.nodes[&e.to].name.as_str(), e.location.line))
            .collect();
        assert_eq!(
            destinations,
            BTreeSet::from([
                ("positional.f", 5),
                ("designated.f", 6),
                ("nested.inner.f", 7),
            ])
        );
        for e in view
            .edges
            .iter()
            .filter(|e| e.operations.contains(&"write field".into()))
        {
            assert_eq!(view.nodes[&e.to].kind, "expression");
            assert!(view.nodes[&e.to].var_id.is_none());
            assert_eq!(e.location, e.provenance[0].location);
            assert!(e.expression.contains('v'));
        }
    }
}

#[test]
fn source_dataflow_field_array_destinations_preserve_comparisons_in_subscripts() {
    use std::collections::BTreeSet;
    use trace_db::{render_dataflow, GraphMeta, RenderFormat};
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(
        tree.path().join("main.c"),
        "struct S { char *table[2]; };\n\
         void stores(struct S *s, int i, char *v) {\n\
             s->table[i == 0] = v;\n\
             s->table[i != 0] = v;\n\
             s->table[i <= 0] = v;\n\
             s->table[(i = 0)] = v;\n\
         }\n",
    )
    .unwrap();
    for full_detail in [false, true] {
        let db = export_tree_with(tree.path(), "field_array_lhs", full_detail);
        let conn = open_db(&db).unwrap();
        let view = trace_db::dataflow_view(
            &conn,
            &[symbol_in(&conn, "stores", "v")],
            Direction::Down,
            1,
        )
        .unwrap();
        let writes: Vec<_> = view
            .edges
            .iter()
            .filter(|e| e.operations.contains(&"write field".into()))
            .collect();
        assert_eq!(
            writes
                .iter()
                .map(|e| (view.nodes[&e.to].name.clone(), e.location.line))
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                ("s->table[i == 0]".into(), 3),
                ("s->table[i != 0]".into(), 4),
                ("s->table[i <= 0]".into(), 5),
                ("s->table[(i = 0)]".into(), 6),
            ])
        );
        let meta = GraphMeta {
            title: "field array destinations",
            direction: "down",
            depth: 1,
            summary: "",
        };
        for format in [
            RenderFormat::Text,
            RenderFormat::Json,
            RenderFormat::Graphviz,
            RenderFormat::Mermaid,
        ] {
            let rendered = render_dataflow(&view, format, &meta);
            if format == RenderFormat::Json {
                let json: serde_json::Value = serde_json::from_str(&rendered).unwrap();
                for edge in &writes {
                    let entity = &view.nodes[&edge.to];
                    assert!(json["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|node| node["name"].as_str() == Some(&entity.name)));
                }
            } else {
                for edge in &writes {
                    let name = &view.nodes[&edge.to].name;
                    let label = if format == RenderFormat::Mermaid {
                        name.replace('<', "&lt;").replace('>', "&gt;")
                    } else {
                        name.clone()
                    };
                    assert!(rendered.contains(&label), "{format:?}: {rendered}");
                }
            }
        }
    }
}

#[test]
fn source_dataflow_repeated_returns_keep_one_transition_per_call_with_macro_locations() {
    use std::collections::BTreeSet;
    let tree = tempfile::tempdir().unwrap();
    let mut source = String::from(
        "char *id(char *p) { return p; }\n#define ID(v) id(v)\nvoid run(char *p) {\n    char *x;\n",
    );
    for _ in 0..128 {
        source.push_str("    x = id(p);\n    x = ID(p);\n");
    }
    source.push_str("}\n");
    std::fs::write(tree.path().join("main.c"), source).unwrap();
    for full_detail in [false, true] {
        let db = export_tree_with(tree.path(), "repeated_returns", full_detail);
        let conn = open_db(&db).unwrap();
        let mut stmt = conn.prepare("SELECT cs.id,r.callee_fn_id,cs.line,cs.col,cs.expansion_line,cs.expansion_col
            FROM flow_return_calls r JOIN call_sites cs ON cs.id=r.call_site_id ORDER BY cs.id,r.callee_fn_id").unwrap();
        let records: Vec<_> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, u32>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(records.len(), 256);
        for (direction, function, variable) in
            [(Direction::Down, "id", "p"), (Direction::Up, "run", "x")]
        {
            let view = trace_db::dataflow_view(
                &conn,
                &[symbol_in(&conn, function, variable)],
                direction,
                1,
            )
            .unwrap();
            let returns: Vec<_> = view
                .edges
                .iter()
                .filter(|e| e.operations == ["return value"])
                .collect();
            assert_eq!(returns.len(), 256);
            assert_eq!(
                returns
                    .iter()
                    .map(|e| (e.from, e.to, e.call_site_id, e.callee_fn_id))
                    .collect::<BTreeSet<_>>()
                    .len(),
                256
            );
            for &(call, callee, line, col, expansion_line, expansion_col) in &records {
                let edge = returns
                    .iter()
                    .find(|e| e.call_site_id == Some(trace_ir::CallSiteId(call)))
                    .unwrap();
                assert_eq!(edge.callee_fn_id, Some(trace_ir::FnId(callee)));
                assert_eq!(
                    (edge.location.line, edge.location.col),
                    (expansion_line.unwrap_or(line), expansion_col.unwrap_or(col))
                );
                if expansion_line.is_some() {
                    let spelling = edge.spelling.as_ref().unwrap();
                    assert_eq!((spelling.line, spelling.col), (line, col));
                } else {
                    assert!(edge.spelling.is_none());
                }
                assert_eq!(edge.provenance[0].location.path, edge.location.path);
                assert_eq!(edge.provenance[0].location.line, edge.location.line);
                assert_eq!(edge.provenance[0].location.col, 5);
                assert_eq!(edge.expression, "x = id(p)");
                assert_eq!(edge.provenance[0].expression, edge.expression);
                assert_eq!(edge.provenance.len(), 1);
            }
            assert_eq!(returns.iter().filter(|e| e.spelling.is_some()).count(), 128);
        }
    }
}

#[test]
fn source_dataflow_exports_repeated_assignment_origins_once_per_site() {
    let tree = tempfile::tempdir().unwrap();
    let mut source = String::from("void repeat(char *v) {\n    char *out;\n");
    for _ in 0..500 {
        source.push_str("    out = v;\n");
    }
    source.push_str("}\n");
    std::fs::write(tree.path().join("main.c"), source).unwrap();
    for full_detail in [false, true] {
        let db = export_tree_with(tree.path(), "repeated_origins", full_detail);
        let conn = open_db(&db).unwrap();
        let counts: (i64, i64) = conn
            .query_row(
                "SELECT count(*),count(DISTINCT line) FROM flow_origins WHERE operation='copy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (500, 500));
        let view = trace_db::dataflow_view(
            &conn,
            &[symbol_in(&conn, "repeat", "v")],
            Direction::Down,
            1,
        )
        .unwrap();
        assert_eq!(view.edges.len(), 500);
        assert_eq!(
            view.edges
                .iter()
                .map(|e| e.location.line)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            500
        );
    }
}

#[test]
fn dataflow_cli_distinguishes_same_line_candidates_in_other_files_from_neighboring_lines() {
    for with_neighbor in [false, true] {
        let tree = common::scratch(&[
            (
                "a_sample.c",
                "char *selected;\n\n\n\nvoid a(char *input) { selected = input; }\n",
            ),
            (
                "b_sample.c",
                if with_neighbor {
                    "char *other;\nchar *neighbor;\n\n\nvoid b(char *input) { other = input; neighbor = input; }\n"
                } else {
                    "char *other;\n\n\n\nvoid b(char *input) { other = input; }\n"
                },
            ),
        ]);
        let db = common::cli_analyze(tree.path(), &[]);
        let conn = open_db(&db).unwrap();
        for (line, col) in [(1, 7), (1, 1), (2, 1)] {
            let cands = require_symbols_at(&conn, "sample.c", line, col).unwrap();
            let expected_names = match (line, with_neighbor) {
                (2, true) => vec!["neighbor", "selected", "other"],
                (_, true) => vec!["selected", "other", "neighbor"],
                (_, false) => vec!["selected", "other"],
            };
            assert_eq!(
                cands.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                expected_names
            );
            let query = |file| {
                Command::new(env!("CARGO_BIN_EXE_trace"))
                    .args([
                        "inspect",
                        db.to_str().unwrap(),
                        "dataflow",
                        "--file",
                        file,
                        "--line",
                        &line.to_string(),
                        "--col",
                        &col.to_string(),
                        "--depth",
                        "1",
                        "--format",
                        "json",
                    ])
                    .output()
                    .unwrap()
            };
            let output = query("sample.c");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{stderr}");
            assert_eq!(stderr.lines().count(), 1, "{stderr}");
            assert_eq!(stderr.matches("note:").count(), 1, "{stderr}");
            assert_eq!(stderr.matches("using ").count(), 1, "{stderr}");
            assert_eq!(stderr.contains("no declaration exactly"), col != 7);
            let counts = match (line, with_neighbor) {
                (1, false) => "1 same-line candidate in the selected file, 1 same-line candidate in other matching files, 0 nearby candidates",
                (1, true) => "1 same-line candidate in the selected file, 1 same-line candidate in other matching files, 1 nearby candidate",
                (2, false) => "0 same-line candidates in the selected file, 0 same-line candidates in other matching files, 2 nearby candidates",
                (2, true) => "1 same-line candidate in the selected file, 0 same-line candidates in other matching files, 2 nearby candidates",
                _ => unreachable!(),
            };
            assert!(stderr.contains(counts), "{stderr}");
            assert!(
                stderr.contains(&format!("using {} [var#", cands[0].name)),
                "{stderr}"
            );
            assert!(stderr.contains("b_sample.c:1:6"), "{stderr}");
            let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(doc["schema"], "dataflow-source-v1");
            assert_eq!(doc["direction"], "down");
            let selected = query(
                std::path::Path::new(&cands[0].path)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap(),
            );
            assert!(selected.status.success());
            assert_eq!(
                output.stdout, selected.stdout,
                "selection preserves graph output"
            );
        }
    }
}

#[test]
fn dataflow_cli_describes_cross_file_nearby_candidates_on_stderr_and_rejects_invalid_positions() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(
        tree.path().join("can_test.c"),
        format!(
            "{}{}char *hdfDev;\n\n\n\n\n\n\nvoid entry(char *p) {{ hdfDev = p; }}\n",
            "\n".repeat(32),
            " ".repeat(25)
        ),
    )
    .unwrap();
    std::fs::write(
        tree.path().join("hdf_can_test.cpp"),
        format!("{}class C {{\n void f() {{}}\n}};\n", "\n".repeat(33)),
    )
    .unwrap();
    let db = export_tree(tree.path(), "nearby_candidates");
    let conn = open_db(&db).unwrap();
    let cands = trace_db::require_symbols_at(&conn, "can_test.c", 33, 31).unwrap();
    assert_eq!(cands[0].name, "hdfDev");
    assert!(cands
        .iter()
        .any(|s| s.name == "this" && s.path.ends_with("hdf_can_test.cpp") && s.line == 35));
    let bin = env!("CARGO_BIN_EXE_trace");
    for format in ["text", "json", "graphviz", "mermaid"] {
        let output = Command::new(bin)
            .args([
                "inspect",
                db.to_str().unwrap(),
                "dataflow",
                "--file",
                "can_test.c",
                "--line",
                "33",
                "--col",
                "31",
                "--format",
                format,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stderr.contains("1 same-line candidate in the selected file, 0 same-line candidates in other matching files, 1 nearby candidate"),
            "{stderr}"
        );
        assert!(stderr.contains("can_test.c:33:31"), "{stderr}");
        assert_eq!(stderr.matches("using ").count(), 1, "{stderr}");
        assert!(stderr.contains("hdf_can_test.cpp:35:"), "{stderr}");
        assert!(!stdout.contains("note:"), "{stdout}");
        if format == "json" {
            serde_json::from_str::<serde_json::Value>(&stdout).unwrap();
        }
        for (line, col) in [("33", "0"), ("33", "-1"), ("0", "31"), ("-1", "31")] {
            let output = Command::new(bin)
                .args([
                    "inspect",
                    db.to_str().unwrap(),
                    "dataflow",
                    "--file",
                    "can_test.c",
                    &format!("--line={line}"),
                    &format!("--col={col}"),
                    "--format",
                    format,
                ])
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("positions are 1-based"));
        }
    }
}

#[test]
fn dataflow_cli_shows_every_call_target_once_with_compact_diagrams() {
    let tree = tempfile::tempdir().unwrap();
    let mut source = String::new();
    for i in 0..138 {
        source.push_str(&format!("void target{i:03}(void) {{}}\n"));
    }
    source.push_str("void (*fp)(void);\nvoid entry(void) {\n");
    for i in 0..138 {
        source.push_str(&format!("fp = target{i:03};\n"));
    }
    source.push_str("fp();\n}\n");
    std::fs::write(tree.path().join("main.c"), source).unwrap();
    let db = export_tree(tree.path(), "call_targets");
    let bin = env!("CARGO_BIN_EXE_trace");
    let run = |format: &str| {
        let output = Command::new(bin)
            .args([
                "inspect",
                db.to_str().unwrap(),
                "dataflow",
                "--file",
                "main.c",
                "--line",
                "139",
                "--col",
                "8",
                "--depth",
                "4",
                "--format",
                format,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let text = run("text");
    assert!(!text.contains("call#"), "{text}");
    assert_eq!(text.matches("possible targets: 138").count(), 1, "{text}");
    for i in 0..138 {
        assert_eq!(
            text.matches(&format!("        target{i:03} [fn#")).count(),
            1,
            "{text}"
        );
    }
    let positions: Vec<_> = (0..138)
        .map(|i| text.find(&format!("        target{i:03} [fn#")).unwrap())
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!text.contains("more targets"));
    assert!(!text.contains("display limit"));
    assert!(text.lines().all(|line| line.len() < 400), "{text}");
    assert_eq!(text, run("text"), "target order must be deterministic");
    let json = run("json");
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    let target = doc["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "call_target")
        .unwrap();
    assert_eq!(target["callees"].as_array().unwrap().len(), 138);
    assert!(target.get("targets").is_none());
    let functions = doc["scopes"]["functions"].as_array().unwrap();
    for callee in target["callees"].as_array().unwrap() {
        assert!(functions.iter().any(|f| f["id"] == *callee));
    }
    assert!(!target["name"].as_str().unwrap().contains("possible"));
    for format in ["graphviz", "mermaid"] {
        let diagram = run(format);
        assert!(diagram.contains("possible targets: 138"), "{diagram}");
        assert!(!diagram.contains("target137"), "{diagram}");
    }
}

#[test]
fn dataflow_cli_shows_all_transitions_at_the_selected_depth_in_both_directions() {
    let tree = tempfile::tempdir().unwrap();
    let mut source = String::from("void flow(char *v) {\nchar *out;\n");
    for _ in 0..120 {
        source.push_str("out = v;\n");
    }
    source.push_str("char *next = out;\n}\n");
    std::fs::write(tree.path().join("main.c"), source).unwrap();
    let db = export_tree(tree.path(), "complete_transitions");
    let conn = open_db(&db).unwrap();
    let bin = env!("CARGO_BIN_EXE_trace");
    let run = |format: &str, direction: &str, depth: &str| {
        let (line, col) = if direction == "down" {
            ("1", "17")
        } else {
            ("123", "7")
        };
        let output = Command::new(bin)
            .args([
                "inspect",
                db.to_str().unwrap(),
                "dataflow",
                "--file",
                "main.c",
                "--line",
                line,
                "--col",
                col,
                "--direction",
                direction,
                "--depth",
                depth,
                "--format",
                format,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    for (direction, depth, count, truncated) in [
        ("down", "1", 120, true),
        ("down", "2", 121, false),
        ("up", "1", 1, true),
        ("up", "2", 121, false),
    ] {
        let text = run("text", direction, depth);
        assert_eq!(text.matches(" → ").count(), count, "{text}");
        assert!(!text.contains("display limit"));
        assert!(!text.contains("omitted"));
        assert_eq!(text.contains("truncated at visible depth"), truncated);
        assert_eq!(text, run("text", direction, depth));
        let doc: serde_json::Value = serde_json::from_str(&run("json", direction, depth)).unwrap();
        assert_eq!(doc["schema"], "dataflow-source-v1");
        assert_eq!(doc["direction"], direction);
        let root = symbol_in(
            &conn,
            "flow",
            if direction == "down" { "v" } else { "next" },
        );
        let location = format!(" at {}:{}:{} ", root.path, root.line, root.col);
        assert!(doc["title"].as_str().unwrap().contains(&location));
        assert!(text.lines().next().unwrap().contains(&location));
        assert!(doc["title"]
            .as_str()
            .unwrap()
            .ends_with(&format!("({direction}, visible depth {depth}):")));
        assert!(text
            .lines()
            .next()
            .unwrap()
            .ends_with(&format!("({direction}, visible depth {depth}):")));
        assert_eq!(doc["edges"].as_array().unwrap().len(), count);
        assert_eq!(doc["truncated"], truncated);
        for format in ["graphviz", "mermaid"] {
            let output = run(format, direction, depth);
            let arrow = if format == "graphviz" {
                " -> "
            } else {
                " -->|"
            };
            assert_eq!(output.matches(arrow).count(), count, "{output}");
            assert_eq!(output.contains("truncated at visible depth"), truncated);
        }
    }
    let help = Command::new(bin)
        .args(["inspect", db.to_str().unwrap(), "dataflow", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for flag in ["--all-callees", "--max-transitions"] {
        assert!(!help.contains(flag));
        let rejected = Command::new(bin)
            .args(["inspect", db.to_str().unwrap(), "dataflow", flag])
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("unexpected argument"));
    }
}

#[test]
fn dataflow_field_storage_distinguishes_parent_chains_without_merging_nodes() {
    use std::collections::BTreeSet;
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("main.c"), "struct D { struct D *priv; char *service; };\nstruct D hdfDev;\nvoid bind(char *value) { hdfDev.service = value; }\n").unwrap();
    let program = build_program(tree.path(), &PreprocessOptions::new()).unwrap();
    let (mut pag, analysis) = analyze(&program);
    let root = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "hdfDev" && v.fn_id.is_none())
        .unwrap()
        .id;
    let ty = program
        .types
        .type_id_by_tag("D", trace_ir::TypeKind::Struct)
        .unwrap();
    let priv_field = program.types.field_id_by_name(ty, "priv").unwrap();
    let service = program.types.field_id_by_name(ty, "service").unwrap();
    let mut parent = pag.var_location[&root];
    let mut field_nodes = BTreeSet::new();
    // Real instance-sensitive locations have the same root VarId but different
    // parent locations. Force four legal depths independently of solver flow.
    for depth in 1..=4 {
        let child = pag.ensure_field_loc(&program, parent, service).unwrap();
        field_nodes.insert(pag.loc_node[&child]);
        if depth < 4 {
            parent = pag.ensure_field_loc(&program, parent, priv_field).unwrap();
        }
    }
    assert_eq!(field_nodes.len(), 4);
    for full_detail in [false, true] {
        let db = TempDb::new("field_storage_paths.db");
        export_to_sqlite(
            &program,
            &pag,
            &analysis,
            &ExportOptions {
                output: db.to_path_buf(),
                trace_version: env!("CARGO_PKG_VERSION").into(),
                include_points_to: false,
                full_detail,
                model_files: Vec::new(),
            },
        )
        .unwrap();
        let conn = open_db(&db).unwrap();
        let symbol = trace_db::require_symbols_at(&conn, "main.c", 2, 10)
            .unwrap()
            .remove(0);
        for direction in [Direction::Down, Direction::Up] {
            let view = trace_db::dataflow_view(&conn, std::slice::from_ref(&symbol), direction, 1)
                .unwrap();
            let selected: BTreeSet<_> = view
                .nodes
                .values()
                .filter(|n| field_nodes.contains(&n.id) && n.depth == 0)
                .map(|n| n.id)
                .collect();
            assert_eq!(selected, field_nodes);
            for (i, path) in [
                "hdfDev.service",
                "hdfDev.priv.service",
                "hdfDev.priv.priv.service",
                "hdfDev.priv.priv.priv.service",
            ]
            .iter()
            .enumerate()
            {
                let expected_prefix = format!("field {path} (abstract field storage [node#");
                assert_eq!(
                    view.nodes
                        .values()
                        .filter(|n| n.name.starts_with(&expected_prefix))
                        .count(),
                    1,
                    "path at depth {}",
                    i + 1
                );
            }
            let paths: BTreeSet<_> = view
                .nodes
                .values()
                .filter(|n| field_nodes.contains(&n.id))
                .map(|n| n.name.as_str())
                .collect();
            assert_eq!(paths.len(), 4);
        }
    }
}

#[test]
fn parameter_lookup_uses_identifier_coordinates() {
    let source = std::fs::read_to_string(fixture("dataflow_parameters").join("main.cpp")).unwrap();
    for full_detail in [false, true] {
        let db = export_tree_with(&fixture("dataflow_parameters"), "parameters", full_detail);
        let conn = open_db(&db).unwrap();
        for (line, name) in [
            (2, "p"),
            (2, "s"),
            (3, "callback"),
            (3, "values"),
            (3, "ref"),
            (5, "value"),
            (6, "object"),
        ] {
            let text = source.lines().nth(line - 1).unwrap();
            let col = text.find(name).unwrap() as i64 + 1;
            for offset in 0..name.len() {
                let candidates =
                    trace_db::find_symbols_at(&conn, "main.cpp", line as i64, col + offset as i64)
                        .unwrap();
                assert_eq!(candidates[0].name, name, "{line}:{}", col + offset as i64);
                assert_eq!((candidates[0].line, candidates[0].col), (line as i64, col));
            }
        }
    }
}

#[test]
fn repeated_arguments_preserve_occurrences_and_source_assignments() {
    use std::collections::BTreeSet;
    let tree = tempfile::tempdir().unwrap();
    let mut source = String::from(
        "#define AGAIN(v) recur(v)\nvoid recur(char *p) {\n    char *q = p;\n    p = q;\n",
    );
    for _ in 0..128 {
        source.push_str("    recur(q);\n    AGAIN(q);\n");
    }
    source.push_str("}\n");
    std::fs::write(tree.path().join("main.c"), source).unwrap();
    for full_detail in [false, true] {
        let db = export_tree_with(tree.path(), "repeated_arguments", full_detail);
        let conn = open_db(&db).unwrap();
        for (dir, root) in [(Direction::Down, "q"), (Direction::Up, "p")] {
            let view =
                trace_db::dataflow_view(&conn, &[symbol_in(&conn, "recur", root)], dir, 1).unwrap();
            let calls: Vec<_> = view
                .edges
                .iter()
                .filter(|e| e.operations == ["pass argument"])
                .collect();
            assert_eq!(calls.len(), 256);
            assert_eq!(
                calls
                    .iter()
                    .map(|e| e.call_site_id.unwrap())
                    .collect::<BTreeSet<_>>()
                    .len(),
                256
            );
            assert_eq!(calls.iter().filter(|e| e.spelling.is_some()).count(), 128);
            for (i, call) in calls.iter().enumerate() {
                assert_eq!(call.location.line, i as i64 + 5);
                assert_eq!(call.location.col, 5);
                assert_eq!(call.arg_index, Some(0));
                assert!(call.callee_fn_id.is_some());
                assert_eq!(call.provenance[0].location, call.location);
                if let Some(spelling) = &call.spelling {
                    assert_eq!(spelling.line, 1);
                }
            }
            assert!(view.edges.iter().any(|e| e.location.line == 4
                && e.operations == ["assign"]
                && e.call_site_id.is_none()));
        }
    }
}

#[test]
fn source_dataflow_cli_analyze_keeps_flow_facts_through_export() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_lifecycle"), &args);
        let conn = open_db(&db).unwrap();
        let columns: Vec<String> = conn
            .prepare("PRAGMA index_info(idx_functions_file_range)")
            .unwrap()
            .query_map([], |r| r.get(2))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(columns, ["file_id", "is_defined", "line_start", "line_end"]);
        let assignments: Vec<i64> = conn
            .prepare(
                "SELECT o.line FROM flow_origins o
             JOIN flow_nodes src ON src.id=o.src_node JOIN variables sv ON sv.id=src.var_id
             JOIN flow_nodes dst ON dst.id=o.dst_node JOIN variables dv ON dv.id=dst.var_id
             WHERE sv.name='payload' AND dv.name='local' AND o.kind='copy' ORDER BY o.line",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            assignments,
            vec![4, 5],
            "assignment sites must survive CLI export"
        );
        let returns: i64 = conn.query_row(
            "SELECT count(*) FROM flow_return_calls r JOIN call_sites cs ON cs.id=r.call_site_id
             JOIN functions f ON f.id=r.callee_fn_id WHERE f.name='id' AND cs.line=6",
            [], |r| r.get(0),
        ).unwrap();
        assert!(
            returns > 0,
            "resolved return occurrences must survive CLI export"
        );
        for root in ["s", "payload"] {
            let view = trace_db::dataflow_view(
                &conn,
                &[symbol_in(&conn, "flow", root)],
                Direction::Down,
                8,
            )
            .unwrap();
            let field = view
                .nodes
                .values()
                .find(|n| n.name.replace(' ', "") == "s->p")
                .expect("field write must remain visible");
            assert_eq!(field.kind, "expression");
            assert_eq!(field.location.line, 6);
            if root == "payload" {
                assert!(view
                    .edges
                    .iter()
                    .any(|e| e.operations.contains(&"return value".into())
                        && e.call_site_id.is_some()
                        && e.callee_fn_id.is_some()
                        && e.location.line == 6));
            }
        }
    }
}

#[test]
fn source_dataflow_large_initializer_exports_element_text_and_field_destinations() {
    let tree = tempfile::tempdir().unwrap();
    let fields = 1000;
    let mut source = String::from("char payload;\nstruct Many {\n");
    for i in 0..fields {
        source.push_str(&format!("    char *f{i};\n"));
    }
    source.push_str("};\nstruct Many table = {\n");
    for i in 0..fields {
        source.push_str(&format!("    .f{i} = &payload,\n"));
    }
    source.push_str("};\n");
    std::fs::write(tree.path().join("main.c"), source).unwrap();
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(tree.path(), &args);
        let conn = open_db(&db).unwrap();
        let (count, total, max): (usize, usize, usize) = conn
            .query_row(
                "SELECT count(*),sum(length(expression)),max(length(expression)) FROM flow_origins",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, fields * 3);
        assert!(total <= fields * 128, "exported {total} expression bytes");
        assert!(max <= 64, "exported an entire aggregate as an origin");
        let payload = require_symbols_at(&conn, "main.c", 1, 6).unwrap();
        let view = trace_db::dataflow_view(&conn, &payload[..1], Direction::Down, 1).unwrap();
        let destinations: std::collections::BTreeSet<_> = view
            .nodes
            .values()
            .filter(|node| node.kind == "expression")
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(destinations.len(), fields);
        assert!(destinations.contains("table.f0") && destinations.contains("table.f999"));
        for edge in view
            .edges
            .iter()
            .filter(|edge| edge.operations.contains(&"write field".into()))
        {
            assert!(edge.expression.len() <= 64);
            assert!(edge.location.line >= fields as i64 + 5);
        }
    }
}

#[test]
fn null_argument_ir_and_solver_preserve_positions_without_pointees() {
    use trace_ir::FlowConstraint;
    let root = fixture("dataflow_arguments");
    let program = build_program(&root, &PreprocessOptions::new()).unwrap();
    let dispatch = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "epta::dispatch")
        .unwrap();
    let receiver = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "receiver" && program.symbols.function(v.fn_id.unwrap()).name == "main")
        .unwrap();
    let calls: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.callee_fn_id == Some(dispatch.id))
        .collect();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.var_args.len(), 3);
        assert_eq!(
            call.var_args[0],
            (0, receiver.id),
            "receiver survives lowering in position zero"
        );
        assert_eq!(
            call.var_args
                .iter()
                .map(|(idx, _)| *idx)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
    assert_ne!(calls[0].id, calls[1].id);
    let null_actual = calls[1].var_args[2].1;
    assert!(program
        .flow
        .contains(&FlowConstraint::NullPointer { dst: null_actual }));
    let (pag, analysis) = trace_analysis::analyze_with_options(
        &program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    assert!(analysis.solve.converged);
    let null_node = pag.null_node.expect("explicit null value node");
    assert_eq!(
        pag.nodes[null_node.0 as usize].kind,
        trace_analysis::PagNodeKind::NullPointer
    );
    assert!(analysis
        .points_to
        .get(&null_node)
        .is_none_or(|pts| pts.is_empty()));
    for flow in &program.flow {
        if let FlowConstraint::NullPointer { dst } = flow {
            assert!(
                program.flow_origins[flow]
                    .iter()
                    .all(|(_, text)| text.as_ref() == "nullptr"),
                "null arguments retain their own literal provenance"
            );
            let node = pag.var_node[dst];
            assert!(analysis
                .points_to
                .get(&node)
                .is_none_or(|pts| pts.is_empty()));
            assert!(
                !pag.var_location.contains_key(dst),
                "null literal gets no object address"
            );
        }
    }
    assert!(analysis
        .arg_flow_edges
        .iter()
        .any(|e| e.call_site == calls[1].id
            && e.arg_index == 2
            && e.actual_var == Some(null_actual)
            && e.formal == dispatch.params[2]));
    for call in calls {
        assert!(analysis
            .arg_flow_edges
            .iter()
            .any(|e| e.call_site == call.id
                && e.arg_index == 0
                && e.actual_var == Some(receiver.id)
                && e.formal == dispatch.params[0]));
    }
    let null_callback = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "only_null")
        .unwrap();
    assert!(
        !analysis
            .call_edges
            .iter()
            .any(|e| e.caller == null_callback.id),
        "null cannot resolve an indirect call"
    );
    let receiver_node = pag.var_node[&dispatch.params[0]];
    assert!(
        !analysis.points_to[&receiver_node].is_empty(),
        "ordinary receiver propagation is preserved"
    );
}

#[test]
fn dataflow_argument_cli_connections_match_all_formats_and_roots() {
    use trace_db::{dataflow_view, RenderFormat};
    let root = fixture("dataflow_arguments");
    for args in [vec![], vec!["--full-export", "--debug-points-to"]] {
        let db = common::cli_analyze(&root, &args);
        let conn = open_db(&db).unwrap();
        let dispatch: u32 = conn
            .query_row(
                "SELECT id FROM functions WHERE name='epta::dispatch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let dispatch = trace_ir::FnId(dispatch);
        let occurrences: Vec<(i64, i64)> = conn.prepare(
            "SELECT cs.line,a.arg_index FROM flow_calls a JOIN call_sites cs ON cs.id=a.call_site_id
             JOIN flow_nodes n ON n.id=a.dst_node WHERE n.fn_id=?1 ORDER BY cs.line,a.arg_index"
        ).unwrap().query_map([dispatch.0], |r| Ok((r.get(0)?,r.get(1)?))).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(
            occurrences,
            vec![(15, 0), (15, 1), (15, 2), (16, 0), (16, 1), (16, 2)]
        );
        for (function, name) in [
            ("main", "receiver"),
            ("main", "payload"),
            ("main", "payload1"),
            ("epta::dispatch", "receiver"),
            ("epta::dispatch", "payload"),
            ("epta::dispatch", "payload1"),
        ] {
            let symbol = symbol_in(&conn, function, name);
            for direction in [Direction::Down, Direction::Up] {
                let view =
                    dataflow_view(&conn, std::slice::from_ref(&symbol), direction, 16).unwrap();
                assert!(!view.truncated);
                let argument_edges: Vec<_> = view
                    .edges
                    .iter()
                    .filter(|e| e.callee_fn_id == Some(dispatch) && e.arg_index.is_some())
                    .collect();
                let connections: std::collections::BTreeSet<_> = argument_edges
                    .iter()
                    .map(|e| {
                        (
                            view.nodes[&e.from].name.as_str(),
                            view.nodes[&e.to].name.as_str(),
                            e.arg_index.unwrap(),
                            e.location.line,
                        )
                    })
                    .collect();
                let expected = match (function, name, direction) {
                    ("main", "receiver", Direction::Down)
                    | ("epta::dispatch", "receiver", Direction::Up) => vec![
                        ("receiver", "receiver", 0, 15),
                        ("receiver", "receiver", 0, 16),
                    ],
                    ("main", "payload", Direction::Down) => vec![
                        ("payload", "payload", 1, 15),
                        ("payload", "payload", 1, 16),
                        ("payload1", "payload1", 2, 15),
                    ],
                    ("epta::dispatch", "payload", Direction::Up) => {
                        vec![("payload", "payload", 1, 15), ("payload", "payload", 1, 16)]
                    }
                    ("main", "payload1", Direction::Down) => vec![("payload1", "payload1", 2, 15)],
                    ("epta::dispatch", "payload1", Direction::Up) => vec![
                        ("payload1", "payload1", 2, 15),
                        ("nullptr", "payload1", 2, 16),
                    ],
                    _ => vec![],
                };
                assert_eq!(
                    connections,
                    expected.into_iter().collect(),
                    "{function}::{name} {direction:?}"
                );
                assert_eq!(
                    argument_edges.len(),
                    connections.len(),
                    "separate occurrences keep separate transitions"
                );
                if let Some(null) = view.nodes.values().find(|n| n.name == "nullptr") {
                    assert_eq!(null.kind, "null_pointer");
                    assert!(null.var_id.is_none() && null.fn_id.is_none());
                }
                if function == "main" && name == "payload" && direction == Direction::Down {
                    assert!(!view.nodes.values().any(|n| n.name == "nullptr" || n.name == "receiver"),
                        "independent arguments do not belong to the selected value's reachable graph");
                }
                let direction_name = if direction == Direction::Down {
                    "down"
                } else {
                    "up"
                };
                for (format, kind) in [
                    ("text", RenderFormat::Text),
                    ("json", RenderFormat::Json),
                    ("graphviz", RenderFormat::Graphviz),
                    ("mermaid", RenderFormat::Mermaid),
                ] {
                    let output = Command::new(env!("CARGO_BIN_EXE_trace"))
                        .args([
                            "inspect",
                            db.to_str().unwrap(),
                            "dataflow",
                            "--file",
                            "main.cpp",
                            "--line",
                            &symbol.line.to_string(),
                            "--col",
                            &symbol.col.to_string(),
                            "--direction",
                            direction_name,
                            "--depth",
                            "16",
                            "--format",
                            format,
                        ])
                        .output()
                        .unwrap();
                    assert!(
                        output.status.success(),
                        "{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let text = String::from_utf8(output.stdout).unwrap();
                    assert!(!text.contains("call_site_id") && !text.contains("call#"));
                    if kind == RenderFormat::Json {
                        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
                        let nodes = doc["nodes"].as_array().unwrap();
                        assert_eq!(nodes.len(), view.nodes.len());
                        for (node, internal) in nodes.iter().zip(view.nodes.values()) {
                            assert_eq!(node["id"], internal.id.0);
                            assert_eq!(node["name"], internal.name);
                            assert_eq!(node["kind"], internal.kind);
                            assert_eq!(node["depth"], internal.depth);
                            assert_eq!(
                                node["location"],
                                serde_json::to_value(&internal.location).unwrap()
                            );
                            assert_eq!(
                                node["callees"],
                                serde_json::to_value(&internal.callees).unwrap()
                            );
                            let scope = if let Some(id) = internal.fn_id {
                                serde_json::json!({"kind":"function","id":id.0})
                            } else if let Some(id) = internal.var_id {
                                let storage: String = conn
                                    .query_row(
                                        "SELECT kind FROM variables WHERE id=?1",
                                        [id.0],
                                        |row| row.get(0),
                                    )
                                    .unwrap();
                                match storage.as_str() {
                                    "global" => serde_json::json!({"kind":"global","id":id.0}),
                                    "file_static" => serde_json::json!({"kind":"static","id":id.0}),
                                    _ => serde_json::Value::Null,
                                }
                            } else {
                                serde_json::json!({"kind":"value","id":internal.id.0})
                            };
                            assert_eq!(node["scope"], scope);
                            for field in ["fn_id", "var_id", "targets", "spelling"] {
                                assert!(node.get(field).is_none());
                            }
                        }
                        let edges = doc["edges"].as_array().unwrap();
                        assert_eq!(edges.len(), view.edges.len());
                        let identity = |edge: &serde_json::Value| {
                            serde_json::to_string(&serde_json::json!([
                                edge["from"],
                                edge["to"],
                                edge["expression"],
                                edge["location"]
                            ]))
                            .unwrap()
                        };
                        let mut actual: Vec<_> = edges.iter().map(identity).collect();
                        let mut expected: Vec<_> = view
                            .edges
                            .iter()
                            .map(|edge| {
                                serde_json::to_string(&serde_json::json!([
                                    edge.from,
                                    edge.to,
                                    edge.expression,
                                    edge.location
                                ]))
                                .unwrap()
                            })
                            .collect();
                        actual.sort();
                        expected.sort();
                        assert_eq!(actual, expected);
                        for edge in edges {
                            for field in [
                                "fn_id",
                                "callee_fn_id",
                                "callee_id",
                                "arg_index",
                                "spelling",
                                "provenance",
                            ] {
                                assert!(edge.get(field).is_none());
                            }
                            for op in edge["operations"].as_array().unwrap() {
                                if let Some(callee) = op.get("callee_id") {
                                    assert!(callee.as_u64().is_some());
                                }
                                if op["kind"].as_str().unwrap().starts_with("pass argument") {
                                    assert!(op["arg_index"].as_u64().unwrap() <= 2);
                                } else {
                                    assert!(op.get("arg_index").is_none());
                                }
                            }
                        }
                    } else {
                        let rows: Vec<_> = text
                            .lines()
                            .filter(|line| match kind {
                                RenderFormat::Text => line.contains('→'),
                                RenderFormat::Graphviz => line.contains(" -> "),
                                RenderFormat::Mermaid => line.contains(" -->"),
                                _ => unreachable!(),
                            })
                            .collect();
                        assert_eq!(
                            rows.len(),
                            view.edges.len(),
                            "{function}::{name} {format} {direction_name}: {text}"
                        );
                        for edge in &argument_edges {
                            let label = if kind == RenderFormat::Text {
                                format!(
                                    "fn#{}, argument {}] at main.cpp:{}:",
                                    dispatch.0,
                                    edge.arg_index.unwrap() + 1,
                                    edge.location.line
                                )
                            } else {
                                format!(
                                    "argument {}] at main.cpp:{}:",
                                    edge.arg_index.unwrap() + 1,
                                    edge.location.line
                                )
                            };
                            let matching: Vec<_> =
                                rows.iter().filter(|row| row.contains(&label)).collect();
                            assert_eq!(matching.len(), 1, "{text}");
                            match kind {
                                RenderFormat::Graphviz => assert!(matching[0]
                                    .contains(&format!("n{} -> n{}", edge.from.0, edge.to.0))),
                                RenderFormat::Mermaid => {
                                    assert!(matching[0].contains(&format!("n{} -->", edge.from.0)));
                                    assert!(matching[0].ends_with(&format!(" n{}", edge.to.0)));
                                }
                                RenderFormat::Text => {
                                    for node in [edge.from, edge.to] {
                                        let entity = &view.nodes[&node];
                                        if let Some(id) = entity.var_id {
                                            assert!(
                                                matching[0].contains(&format!("[var#{}]", id.0))
                                            );
                                        } else {
                                            assert!(matching[0].contains(&entity.name));
                                        }
                                    }
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn source_dataflow_comments_do_not_delimit_write_destinations() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    for full_detail in [false, true] {
        let db = export_tree_with(
            &fixture("dataflow_comment_destinations"),
            "comment_destinations",
            full_detail,
        );
        let conn = open_db(&db).unwrap();
        for (function, lhs, expression) in [
            (
                "block",
                "s->field /* = note */",
                "s->field /* = note */ = p",
            ),
            (
                "delimiters",
                "s->field /* [ \" */",
                "s->field /* [ \" */ = p",
            ),
            (
                "line",
                "s->field // = [ \"",
                "s->field // = [ \"\n        = p",
            ),
        ] {
            for root in ["p", "s"] {
                let down = dataflow_view(
                    &conn,
                    &[symbol_in(&conn, function, root)],
                    Direction::Down,
                    1,
                )
                .unwrap();
                let destination = down
                    .nodes
                    .values()
                    .find(|node| node.name == lhs)
                    .unwrap_or_else(|| panic!("{function}/{root}: {:?}", down.nodes));
                assert_eq!(destination.kind, "expression");
                assert!(down
                    .edges
                    .iter()
                    .all(|edge| down.nodes.contains_key(&edge.from)
                        && down.nodes.contains_key(&edge.to)));
                if root == "p" {
                    assert!(down.edges.iter().any(|edge| edge.expression == expression));
                    let meta = GraphMeta {
                        title: "comments",
                        direction: "down",
                        depth: 1,
                        summary: "",
                    };
                    assert!(render_dataflow(&down, RenderFormat::Text, &meta).contains(expression));
                    let json: serde_json::Value =
                        serde_json::from_str(&render_dataflow(&down, RenderFormat::Json, &meta))
                            .unwrap();
                    assert!(json["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|node| node["name"] == lhs));
                    assert!(json["edges"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|edge| edge["expression"] == expression));
                }
            }
        }
    }
}

#[test]
fn source_dataflow_review_preserves_exact_expressions_and_initializer_scopes() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let db = build_and_export("dataflow_review");
    let conn = open_db(&db).unwrap();
    let down = dataflow_view(
        &conn,
        &[symbol_in(&conn, "spelling", "q")],
        Direction::Down,
        4,
    )
    .unwrap();
    assert!(down.edges.iter().any(|edge| edge.expression == "r = q"));
    assert!(down
        .edges
        .iter()
        .any(|edge| edge.expression == "envelope.payload = r"));
    assert!(down
        .nodes
        .values()
        .any(|node| node.name == "envelope.payload"));
    let returns = dataflow_view(
        &conn,
        &[symbol_in(&conn, "spelling", "r")],
        Direction::Up,
        1,
    )
    .unwrap();
    let expressions: std::collections::BTreeSet<_> = returns
        .edges
        .iter()
        .filter(|edge| edge.operations == ["return value"])
        .map(|edge| edge.expression.as_str())
        .collect();
    assert_eq!(
        expressions,
        std::collections::BTreeSet::from([
            "r = identity(q)",
            "r = identity(r)",
            "r = fp(q)",
            "r = fp(r)",
        ])
    );
    assert_eq!(
        returns
            .edges
            .iter()
            .filter(|edge| edge.operations == ["return value"])
            .count(),
        5
    );
    for edge in returns
        .edges
        .iter()
        .filter(|edge| edge.operations == ["return value"])
    {
        assert_eq!(edge.provenance.len(), 1);
        assert_eq!(edge.provenance[0].expression, edge.expression);
        assert_eq!(edge.provenance[0].location.line, edge.location.line);
    }
    let meta = GraphMeta {
        title: "review",
        direction: "up",
        depth: 1,
        summary: "",
    };
    let text = render_dataflow(&returns, RenderFormat::Text, &meta);
    let json: serde_json::Value =
        serde_json::from_str(&render_dataflow(&returns, RenderFormat::Json, &meta)).unwrap();
    for expression in expressions {
        assert!(text.contains(expression), "{text}");
        assert!(json["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["expression"] == expression));
    }
    let macro_edge = returns
        .edges
        .iter()
        .find(|edge| edge.spelling.is_some() && edge.operations == ["return value"])
        .unwrap();
    assert_eq!(macro_edge.location.line, 15);
    assert_eq!(macro_edge.spelling.as_ref().unwrap().line, 6);
    for name in ["operations", "connection"] {
        let symbol = require_symbols_at(
            &conn,
            "main.c",
            if name == "operations" { 4 } else { 5 },
            12,
        )
        .unwrap()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap();
        let view = dataflow_view(&conn, &[symbol], Direction::Down, 3).unwrap();
        let field = view
            .nodes
            .values()
            .find(|node| {
                node.name.contains(if name == "operations" {
                    "operations.send"
                } else {
                    "connection.ops"
                })
            })
            .unwrap();
        let scope = &view.scopes[&field.scope];
        assert_eq!(scope.fn_id, None);
        assert_eq!(scope.name, "global");
        assert!(scope.location.path.ends_with("main.c"));
    }
}

#[test]
fn source_dataflow_cli_emits_one_selection_note_for_exact_and_inexact_positions() {
    let db = build_and_export("dataflow_review");
    let conn = open_db(&db).unwrap();
    let q = symbol_in(&conn, "spelling", "q");
    for col in [q.col, 1] {
        let output = Command::new(env!("CARGO_BIN_EXE_trace"))
            .args([
                "inspect",
                db.to_str().unwrap(),
                "dataflow",
                "--file",
                "main.c",
                "--line",
                &q.line.to_string(),
                "--col",
                &col.to_string(),
                "--depth",
                "1",
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.matches("using ").count(), 1, "{stderr}");
        assert_eq!(stderr.matches("note:").count(), 1, "{stderr}");
        assert_eq!(stderr.contains("no declaration exactly"), col != q.col);
        let doc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            doc["title"],
            format!(
                "dataflow for {} [var#{}] at {}:{}:{} (down, visible depth 1):",
                q.name, q.var_id, q.path, q.line, q.col
            )
        );
    }
}

#[test]
fn source_dataflow_multiline_returns_retain_assignment_and_macro_provenance() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("main.c"), "char *id(char *p) { return p; }\n#define ID(v) id(v)\nvoid run(char *p) {\n    char *out;\n    out =\n        id(p);\n    out =\n        ID(p);\n}\n").unwrap();
    let db = export_tree(tree.path(), "multiline_return");
    let conn = open_db(&db).unwrap();
    for (direction, function, name) in [(Direction::Up, "run", "out"), (Direction::Down, "id", "p")]
    {
        let view = dataflow_view(&conn, &[symbol_in(&conn, function, name)], direction, 1).unwrap();
        let returns: Vec<_> = view
            .edges
            .iter()
            .filter(|edge| edge.operations == ["return value"])
            .collect();
        assert_eq!(returns.len(), 2);
        let direct = returns.iter().find(|edge| edge.spelling.is_none()).unwrap();
        assert_eq!(direct.expression, "out =\n        id(p)");
        assert_eq!(direct.location.line, 6);
        assert_eq!(direct.provenance[0].location.line, 5);
        let expanded = returns.iter().find(|edge| edge.spelling.is_some()).unwrap();
        assert_eq!(expanded.expression, "out = id(p)");
        assert_eq!(expanded.location.line, 8);
        assert_eq!(expanded.spelling.as_ref().unwrap().line, 2);
        assert_eq!(expanded.provenance[0].location.line, 7);
        let meta = GraphMeta {
            title: "multiline",
            direction: if direction == Direction::Down {
                "down"
            } else {
                "up"
            },
            depth: 1,
            summary: "",
        };
        for format in [RenderFormat::Text, RenderFormat::Json] {
            let output = render_dataflow(&view, format, &meta);
            if format == RenderFormat::Json {
                let json: serde_json::Value = serde_json::from_str(&output).unwrap();
                for edge in &returns {
                    assert!(json["edges"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["expression"] == edge.expression));
                }
            } else {
                for edge in &returns {
                    assert!(output.contains(&edge.expression), "{output}");
                }
            }
        }
    }
    conn.execute("DROP TABLE flow_call_origins", []).unwrap();
    let older = dataflow_view(&conn, &[symbol_in(&conn, "run", "out")], Direction::Up, 1).unwrap();
    let expressions: std::collections::BTreeSet<_> = older
        .edges
        .iter()
        .filter(|edge| edge.operations == ["return value"])
        .map(|edge| edge.expression.as_str())
        .collect();
    assert_eq!(
        expressions,
        ["out =\n        id(p)", "out = id(p)"]
            .into_iter()
            .collect()
    );
}

#[test]
fn source_dataflow_same_invocation_returns_keep_distinct_operations() {
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("main.c"), "char *id(char *p) { return p; }\n#define BOTH(dst,p,q) dst = id(p); dst = id(q)\nvoid run(char *p, char *q) {\n char *out;\n BOTH(out,p,q);\n}\n").unwrap();
    let db = export_tree(tree.path(), "same_invocation_returns");
    let conn = open_db(&db).unwrap();
    for (direction, function, name) in [(Direction::Up, "run", "out"), (Direction::Down, "id", "p")]
    {
        let view = dataflow_view(&conn, &[symbol_in(&conn, function, name)], direction, 1).unwrap();
        let returns: Vec<_> = view
            .edges
            .iter()
            .filter(|edge| edge.operations == ["return value"])
            .collect();
        assert_eq!(returns.len(), 2);
        assert_ne!(returns[0].call_site_id, returns[1].call_site_id);
        let expressions: std::collections::BTreeSet<_> =
            returns.iter().map(|e| e.expression.as_str()).collect();
        assert_eq!(
            expressions,
            ["out = id(p)", "out = id(q)"].into_iter().collect()
        );
        for edge in &returns {
            assert_eq!(edge.location.line, 5);
            assert_eq!(edge.spelling.as_ref().unwrap().line, 2);
            assert_eq!(edge.provenance[0].location.line, 5);
            assert_eq!(edge.provenance[0].expression, edge.expression);
        }
        let meta = GraphMeta {
            title: "macro returns",
            direction: "up",
            depth: 1,
            summary: "",
        };
        for format in [RenderFormat::Json, RenderFormat::Text] {
            let output = render_dataflow(&view, format, &meta);
            for expression in &expressions {
                assert!(output.contains(expression), "{output}");
            }
        }
    }
}

#[test]
fn source_dataflow_argument_expressions_survive_export_macros_and_virtual_clones() {
    use std::collections::BTreeSet;
    use trace_db::{dataflow_view, render_dataflow, GraphMeta, RenderFormat};
    for full_detail in [false, true] {
        let db = export_tree_with(
            &fixture("dataflow_call_expressions"),
            "call_expressions",
            full_detail,
        );
        let conn = open_db(&db).unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT x.expression FROM flow_calls a JOIN flow_call_expressions x ON x.call_site_id=a.call_site_id").unwrap();
        let recorded: BTreeSet<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected = [
            "consume(payload)",
            "callback(payload)",
            "consume((payload))",
            "identity(payload)",
            "consume(forwarded)",
            "receiver->send(payload)",
            "consume(identity(payload))",
            "consume(\n        payload\n    )",
            "run(payload, consume, &receiver)",
        ];
        for expression in expected {
            assert!(recorded.contains(expression), "{expression}: {recorded:?}");
        }
        let virtual_sites: i64 = conn.query_row("SELECT COUNT(*) FROM flow_call_expressions x JOIN call_sites cs ON cs.id=x.call_site_id WHERE x.expression='receiver->send(payload)'", [], |r| r.get(0)).unwrap();
        assert_eq!(
            virtual_sites, 2,
            "both virtual candidates retain the call expression"
        );
        let mut macro_stmt = conn.prepare("SELECT DISTINCT x.expression FROM flow_call_expressions x JOIN call_sites cs ON cs.id=x.call_site_id WHERE cs.expansion_line=11").unwrap();
        let macro_expressions: BTreeSet<String> = macro_stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            macro_expressions,
            ["consume(payload)".into(), "consume((payload))".into()]
                .into_iter()
                .collect()
        );
        for direction in [Direction::Down, Direction::Up] {
            let roots = if direction == Direction::Down {
                vec![symbol_in(&conn, "run", "payload")]
            } else {
                vec![
                    symbol_in(&conn, "consume", "value"),
                    symbol_in(&conn, "Base::send", "value"),
                    symbol_in(&conn, "Derived::send", "value"),
                ]
            };
            let view = dataflow_view(&conn, &roots, direction, 8).unwrap();
            let mut seen = BTreeSet::new();
            for edge in &view.edges {
                // Collapsed transitions can have a different primary operation;
                // every constituent argument must retain its own call text.
                for origin in edge
                    .provenance
                    .iter()
                    .filter(|origin| origin.arg_index.is_some())
                {
                    assert!(!origin.expression.is_empty());
                    assert!(recorded.contains(&origin.expression));
                    seen.insert(origin.expression.clone());
                }
                if edge.operations.len() == 1 && edge.operations[0].starts_with("pass argument") {
                    let site = edge.call_site_id.unwrap();
                    let expression: String = conn
                        .query_row(
                            "SELECT expression FROM flow_call_expressions WHERE call_site_id=?1",
                            [site.0],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(edge.expression, expression);
                    assert!(edge
                        .provenance
                        .iter()
                        .any(|origin| origin.arg_index == edge.arg_index
                            && origin.expression == expression));
                }
            }
            for expression in expected[..8].iter() {
                assert!(
                    seen.contains(*expression),
                    "{direction:?}: {expression}: {seen:?}"
                );
            }
            let meta = GraphMeta {
                title: "calls",
                direction: if direction == Direction::Down {
                    "down"
                } else {
                    "up"
                },
                depth: 8,
                summary: "",
            };
            let json = render_dataflow(&view, RenderFormat::Json, &meta);
            assert_eq!(json, render_dataflow(&view, RenderFormat::Json, &meta));
            let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
            for edge in doc["edges"].as_array().unwrap() {
                for op in edge["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|op| op["kind"].as_str().unwrap().starts_with("pass argument"))
                {
                    assert!(!op["expression"].as_str().unwrap().is_empty(), "{op}");
                }
            }
            let returned = view
                .edges
                .iter()
                .find(|edge| {
                    edge.operations == ["return value"]
                        && edge.expression == "*forwarded = identity(payload)"
                })
                .unwrap();
            assert_eq!(
                returned.provenance[0].expression,
                "*forwarded = identity(payload)"
            );
        }
    }
}

/// Exercise the CLI renderer against the database produced by CLI analysis.
fn source_origin_cli_output(
    db: &TempDb,
    symbol: &SymbolRef,
    direction: &str,
    depth: &str,
    format: &str,
) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_trace"))
        .args([
            "inspect",
            db.to_str().unwrap(),
            "dataflow",
            "--file",
            &symbol.path,
            "--line",
            &symbol.line.to_string(),
            "--col",
            &symbol.col.to_string(),
            "--direction",
            direction,
            "--depth",
            depth,
            "--format",
            format,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn source_dataflow_cli_pointer_store_exposes_destination_without_incoming_edge() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_source_origins"), &args);
        let conn = open_db(&db).unwrap();
        let stores: i64 = conn.query_row(
            "SELECT COUNT(*) FROM flow_origins WHERE operation='store' AND expression='*h->pp = value'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(stores, 1);
        let root = symbol_in(&conn, "write", "h");
        let view = trace_db::dataflow_view(&conn, std::slice::from_ref(&root), Direction::Down, 2)
            .unwrap();
        let destination = view
            .nodes
            .values()
            .find(|node| node.name == "*h->pp")
            .unwrap();
        assert_eq!(destination.kind, "expression");
        assert_eq!(destination.location.line, 3);
        assert!(!view.nodes.values().any(|node| node.name == "value"));
        assert!(!view
            .edges
            .iter()
            .any(|edge| edge.operations.iter().any(|op| op == "write pointer")));
        for format in ["text", "json", "graphviz", "mermaid"] {
            let output =
                source_origin_cli_output(&db, &root, "down", "2", format).replace("&gt;", ">");
            assert!(output.contains("*h->pp"), "{output}");
            assert!(output.contains("*h->pp = value"), "{output}");
            assert!(!output.contains("write pointer"), "{output}");
        }
        let value = symbol_in(&conn, "write", "value");
        let output = source_origin_cli_output(&db, &value, "down", "1", "json");
        assert!(
            output.contains("write pointer") && output.contains("*h->pp = value"),
            "{output}"
        );
    }
}

#[test]
fn source_dataflow_cli_indirect_member_wiring_reclassifies_provenance() {
    for args in [vec!["--jobs", "1"], vec!["--jobs", "4", "--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_indirect_member"), &args);
        let conn = open_db(&db).unwrap();
        let root = symbol_in(&conn, "run", "s");
        let view = trace_db::dataflow_view(&conn, std::slice::from_ref(&root), Direction::Down, 4)
            .unwrap();
        let wiring: Vec<_> = view
            .edges
            .iter()
            .filter(|edge| view.nodes[&edge.to].kind == "call_target")
            .collect();
        assert!(!wiring.is_empty(), "{view:#?}");
        assert!(
            view.edges
                .iter()
                .any(|edge| edge.operations == ["assign"] && edge.expression == "alias = s"),
            "{view:#?}"
        );
        for edge in wiring {
            assert!(edge
                .operations
                .iter()
                .any(|op| op == "resolve indirect call"));
            assert!(!edge.operations.iter().any(|op| op == "assign"));
            assert!(
                !edge.provenance.iter().any(|op| op.operation == "assign"),
                "{edge:#?}"
            );
        }
        let json = source_origin_cli_output(&db, &root, "down", "4", "json");
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
        let calls: Vec<_> = doc["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["kind"] == "call_target")
            .map(|node| &node["id"])
            .collect();
        assert_eq!(calls.len(), 2);
        let incoming: Vec<_> = doc["edges"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|edge| calls.contains(&&edge["to"]))
            .collect();
        assert!(!incoming.is_empty(), "{json}");
        for edge in incoming {
            let ops = edge["operations"].as_array().unwrap();
            assert!(
                ops.iter().any(|op| op["kind"] == "resolve indirect call"),
                "{edge}"
            );
            assert!(!ops.iter().any(|op| op["kind"] == "assign"), "{edge}");
        }
        for format in ["text", "graphviz", "mermaid"] {
            let output = source_origin_cli_output(&db, &root, "down", "4", format);
            assert!(output.contains("resolve indirect call"), "{output}");
            assert!(output.contains("alias = s"), "{output}");
        }
    }
}

#[test]
fn source_dataflow_cli_internal_overloads_keep_initializer_origins() {
    let mut previous = None;
    let mut previous_outputs = None;
    for args in [
        vec!["--jobs", "1"],
        vec!["--jobs", "4"],
        vec!["--jobs", "1", "--full-export"],
        vec!["--jobs", "4", "--full-export"],
    ] {
        let db = common::cli_analyze(&fixture("dataflow_internal_overload_origins"), &args);
        let conn = open_db(&db).unwrap();
        let mut outputs = Vec::new();
        for (name, line, col, expected) in [
            (
                "arr",
                3,
                8,
                vec![("cb", 3, 27), ("&cb", 3, 31), ("cb", 3, 36)],
            ),
            (
                "fp",
                4,
                8,
                vec![
                    ("cb", 4, 22),
                    ("fp = cb", 6, 5),
                    ("fp = &cb", 7, 5),
                    ("fp = cb", 8, 5),
                ],
            ),
        ] {
            let root = require_symbols_at(&conn, "main.cpp", line, col)
                .unwrap()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap();
            let view =
                trace_db::dataflow_view(&conn, std::slice::from_ref(&root), Direction::Up, 1)
                    .unwrap();
            assert_eq!(view.edges.len(), expected.len() * 2, "{view:#?}");
            let sources: std::collections::BTreeSet<_> =
                view.edges.iter().map(|e| e.from).collect();
            assert_eq!(
                sources.len(),
                2,
                "both overloads keep their installation sites"
            );
            for source in sources {
                for (expression, line, col) in &expected {
                    let edge = view
                        .edges
                        .iter()
                        .find(|edge| {
                            edge.from == source
                                && edge.expression == *expression
                                && edge.location.line == *line
                                && edge.location.col == *col
                        })
                        .unwrap();
                    assert!(edge.location.path.ends_with("main.cpp"));
                    assert_eq!(edge.location.col, *col);
                    assert_eq!(edge.operations, ["take address"]);
                    assert_eq!(edge.provenance.len(), 1);
                    assert_eq!(edge.provenance[0].expression, *expression);
                    assert_eq!(edge.provenance[0].location, edge.location);
                }
            }
            for format in ["text", "json", "graphviz", "mermaid"] {
                let output = source_origin_cli_output(&db, &root, "up", "1", format);
                if format == "json" {
                    let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
                    assert_eq!(doc["edges"].as_array().unwrap().len(), expected.len() * 2);
                    for edge in doc["edges"].as_array().unwrap() {
                        assert!(expected.iter().any(|(expression, line, col)| {
                            edge["expression"] == *expression
                                && edge["location"]["line"] == *line
                                && edge["location"]["col"] == *col
                        }));
                        let ops = edge["operations"].as_array().unwrap();
                        assert_eq!(ops.len(), 1);
                        assert_eq!(ops[0]["kind"], "take address");
                        assert_eq!(ops[0]["expression"], edge["expression"]);
                        assert_eq!(ops[0]["location"], edge["location"]);
                    }
                } else {
                    assert!(output.contains(&format!("main.cpp:{line}:")), "{output}");
                }
                outputs.push(output);
            }
        }
        let origins: Vec<(i64, i64, String, i64, i64, String)> = conn
            .prepare("SELECT src_node,dst_node,operation,line,col,expression FROM flow_origins ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(origins.iter().filter(|row| row.2 == "addr_of").count(), 14);
        if let Some(previous) = &previous {
            assert_eq!(
                &origins, previous,
                "minimal/full exports keep origin insertion order"
            );
        }
        previous = Some(origins);
        if let Some(previous_outputs) = &previous_outputs {
            assert_eq!(
                &outputs, previous_outputs,
                "minimal/full renderings are deterministic at jobs 1/4"
            );
        }
        previous_outputs = Some(outputs);
    }
}

#[test]
fn source_dataflow_cli_array_entries_keep_element_sites_across_files() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_source_origins"), &args);
        let conn = open_db(&db).unwrap();
        for (name, line, col, expected) in [
            ("handlers", 3, 8, vec![("first", 3, 30), ("second", 3, 37)]),
            (
                "remote_handlers",
                6,
                8,
                vec![
                    ("remote_first", 7, 5),
                    ("&remote_second", 8, 5),
                    ("remote_first", 9, 5),
                ],
            ),
        ] {
            let root = require_symbols_at(&conn, "arrays.c", line, col)
                .unwrap()
                .into_iter()
                .find(|s| s.name == name)
                .unwrap();
            let view =
                trace_db::dataflow_view(&conn, std::slice::from_ref(&root), Direction::Up, 1)
                    .unwrap();
            assert_eq!(view.edges.len(), expected.len(), "{view:#?}");
            for (expression, line, col) in &expected {
                let edge = view
                    .edges
                    .iter()
                    .find(|edge| edge.expression == *expression && edge.location.line == *line)
                    .unwrap();
                assert!(edge.location.path.ends_with("arrays.c"));
                assert_eq!(edge.location.col, *col);
                assert_eq!(edge.operations, ["take address"]);
                assert_eq!(edge.provenance[0].expression, *expression);
                assert_eq!(edge.provenance[0].location, edge.location);
                if name == "remote_handlers" {
                    assert!(view.nodes[&edge.from]
                        .location
                        .path
                        .ends_with("functions.c"));
                }
            }
            for format in ["text", "json", "graphviz", "mermaid"] {
                let output =
                    source_origin_cli_output(&db, &root, "up", "1", format).replace("&amp;", "&");
                for (expression, line, _) in &expected {
                    assert!(output.contains(expression), "{output}");
                    if format == "json" {
                        let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
                        assert!(doc["edges"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|edge| edge["expression"] == *expression
                                && edge["location"]["line"] == *line));
                    } else {
                        assert!(output.contains(&format!("arrays.c:{line}:")), "{output}");
                    }
                }
            }
        }
    }
}

#[test]
fn source_dataflow_cli_explicit_constructors_keep_text_macro_sites_and_nested_ownership() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_source_origins"), &args);
        let conn = open_db(&db).unwrap();
        let expected = [
            ("local(input)", 9, None),
            ("new Box(input)", 10, None),
            ("nested_local{identity(input)}", 11, None),
            ("new Box(identity(input))", 12, None),
            ("macro_local(input)", 6, Some(13)),
            ("new Box(input)", 7, Some(14)),
            ("brace_local{input}", 15, None),
            ("new Box{input}", 16, None),
        ];
        let rows: Vec<(String, i64, Option<i64>)> = conn.prepare(
            "SELECT x.expression,cs.line,cs.expansion_line FROM call_sites cs JOIN flow_call_expressions x ON x.call_site_id=cs.id WHERE cs.callee_text='Box::Box' ORDER BY cs.id"
        ).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(rows.len(), expected.len(), "{rows:?}");
        for (expression, line, expansion) in expected {
            assert!(
                rows.contains(&(expression.into(), line, expansion)),
                "{expression}: {rows:?}"
            );
        }
        let nested: Vec<String> = conn.prepare(
            "SELECT x.expression FROM call_sites cs JOIN flow_call_expressions x ON x.call_site_id=cs.id WHERE cs.callee_text='identity'"
        ).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(nested, ["identity(input)", "identity(input)"]);
        let initializers: Vec<String> = conn.prepare(
            "SELECT x.expression FROM call_sites cs JOIN flow_call_expressions x ON x.call_site_id=cs.id WHERE cs.callee_text IN ('Base::Base','Member::Member') ORDER BY cs.id"
        ).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(initializers, ["Base(p)", "member(p)"]);
        for (direction, root) in [
            ("down", symbol_in(&conn, "Derived::Derived", "p")),
            ("up", symbol_in(&conn, "Base::Base", "p")),
            ("up", symbol_in(&conn, "Member::Member", "p")),
        ] {
            let output = source_origin_cli_output(&db, &root, direction, "1", "json");
            let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
            for expression in if direction == "down" {
                vec!["Base(p)", "member(p)"]
            } else if root.fn_name.as_deref() == Some("Base::Base") {
                vec!["Base(p)"]
            } else {
                vec!["member(p)"]
            } {
                assert!(
                    doc["edges"].as_array().unwrap().iter().any(|edge| {
                        edge["operations"].as_array().unwrap().iter().any(|op| {
                            op["kind"] == "pass argument"
                                && op["expression"] == expression
                                && op["location"]["line"] == 22
                        })
                    }),
                    "{output}"
                );
                let text = source_origin_cli_output(&db, &root, direction, "1", "text");
                assert!(text.contains(expression), "{text}");
            }
        }
        for (direction, root) in [
            ("down", symbol_in(&conn, "build", "input")),
            ("up", symbol_in(&conn, "Box::Box", "p")),
        ] {
            let output = source_origin_cli_output(&db, &root, direction, "8", "json");
            let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
            let operations: Vec<_> = doc["edges"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|edge| edge["operations"].as_array().unwrap())
                .filter(|op| op["kind"].as_str().unwrap().starts_with("pass argument"))
                .collect();
            let text = source_origin_cli_output(&db, &root, direction, "8", "text");
            for (expression, line, expansion) in expected {
                assert!(
                    operations.iter().any(|op| op["expression"] == expression
                        && op["location"]["line"] == expansion.unwrap_or(line)),
                    "{direction}: {expression}: {output}"
                );
                // A collapsed return can become the prominent text operation;
                // JSON above verifies every nested constructor argument's text.
                if !expression.contains("identity(") {
                    assert!(text.contains(expression), "{text}");
                }
            }
            assert!(operations
                .iter()
                .all(|op| !op["expression"].as_str().unwrap().is_empty()));
            if direction == "down" {
                assert!(operations
                    .iter()
                    .any(|op| op["expression"] == "identity(input)"));
            }
        }
    }
}

#[test]
fn source_dataflow_cli_nested_lambda_retains_its_return_assignment() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_nested_lambda_return"), &args);
        let conn = open_db(&db).unwrap();
        let bindings: Vec<(String, i64, i64)> = conn.prepare(
            "SELECT DISTINCT o.expression,o.line,o.col FROM flow_origins o JOIN flow_return_calls r ON r.src_node=o.src_node AND r.dst_node=o.dst_node JOIN call_sites cs ON cs.id=r.call_site_id WHERE cs.callee_text='id' AND o.operation='return value'"
        ).unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(bindings, [("inner = id(p)".into(), 8, 9)]);
        let binding: (String, i64, i64) = conn.query_row(
            "SELECT o.expression,o.line,o.col FROM flow_call_origins o JOIN call_sites cs ON cs.id=o.call_site_id WHERE cs.callee_text='id'",
            [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))
        ).unwrap();
        assert_eq!(binding, ("inner = id(p)".into(), 8, 9));
        let root = trace_db::find_symbols_at(&conn, "main.cpp", 7, 15)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "inner")
            .unwrap();
        for format in ["text", "json", "graphviz", "mermaid"] {
            let output = source_origin_cli_output(&db, &root, "up", "1", format);
            assert!(output.contains("inner = id(p)"), "{output}");
            if format == "json" {
                let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
                assert_eq!(doc["schema"], "dataflow-source-v1");
                assert!(
                    doc["edges"].as_array().unwrap().iter().any(|edge| {
                        edge["operations"].as_array().unwrap().iter().any(|op| {
                            op["kind"] == "return value"
                                && op["expression"] == "inner = id(p)"
                                && op["location"]["line"] == 8
                                && op["location"]["col"] == 9
                        })
                    }),
                    "{output}"
                );
            }
        }
    }
}

#[test]
fn source_dataflow_cli_orders_files_before_lines_in_both_directions_and_all_formats() {
    let db = common::cli_analyze(&fixture("dataflow_source_origins"), &[]);
    let conn = open_db(&db).unwrap();
    for direction in ["down", "up"] {
        let root = symbol_in(&conn, "ordered_source", "value");
        for format in ["text", "json", "graphviz", "mermaid"] {
            let output = source_origin_cli_output(&db, &root, direction, "1", format);
            if format == "json" {
                let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
                let edges = doc["edges"].as_array().unwrap();
                assert_eq!(edges.len(), 2, "{output}");
                let expression = if direction == "down" {
                    "*output = ordered_source(input)"
                } else {
                    "ordered_source(input)"
                };
                assert_eq!(edges[0]["expression"], expression);
                assert_eq!(edges[1]["expression"], expression);
                assert!(edges[0]["location"]["path"]
                    .as_str()
                    .unwrap()
                    .ends_with("a_order.c"));
                assert!(edges[1]["location"]["path"]
                    .as_str()
                    .unwrap()
                    .ends_with("z_order.c"));
                assert_eq!(edges[0]["location"]["line"], 9);
                assert_eq!(edges[1]["location"]["line"], 4);
            } else {
                let rows: Vec<_> = output
                    .lines()
                    .filter(|line| match format {
                        "text" => line.contains(" → "),
                        "graphviz" => line.contains(" -> "),
                        "mermaid" => line.contains(" -->"),
                        _ => unreachable!(),
                    })
                    .collect();
                assert_eq!(rows.len(), 2, "{output}");
                assert!(
                    rows[0].contains("a_order.c:9:"),
                    "{direction}, {format}: {output}"
                );
                assert!(
                    rows[1].contains("z_order.c:4:"),
                    "{direction}, {format}: {output}"
                );
            }
            assert_eq!(
                output,
                source_origin_cli_output(&db, &root, direction, "1", format)
            );
        }
    }
}

#[test]
fn source_dataflow_cli_cpp_literals_cannot_hide_pointer_stores() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_cpp_literal_destinations"), &args);
        let conn = open_db(&db).unwrap();
        for (function, lhs) in [
            ("digits", "*s->slots[1'000]"),
            ("raw", r#"*s->slots[index(R"tag(" ] } = /* ')tag")]"#),
            (
                "multiline",
                "*s->slots[index(u8R\"tag(\" ] =\n\\\n/* ')tag\")]",
            ),
        ] {
            let expression = format!("{lhs} = p");
            let stored: i64 = conn.query_row(
                "SELECT COUNT(*) FROM flow_origins WHERE kind='store' AND operation='store' AND expression=?1",
                [&expression], |r| r.get(0),
            ).unwrap();
            assert_eq!(
                stored, 1,
                "{function}: store must survive analysis and export"
            );
            let root = symbol_in(&conn, function, "p");
            let view =
                trace_db::dataflow_view(&conn, std::slice::from_ref(&root), Direction::Down, 1)
                    .unwrap();
            let destination = view
                .nodes
                .values()
                .find(|node| node.name == lhs)
                .unwrap_or_else(|| panic!("{function}: {view:#?}"));
            assert_eq!(destination.kind, "expression");
            assert!(view.edges.iter().any(|edge| edge.to == destination.id
                && edge.operations == ["write pointer"]
                && edge.expression == expression));
            let text = source_origin_cli_output(&db, &root, "down", "1", "text");
            assert!(text.contains(lhs) && text.contains(&expression), "{text}");
            let output = source_origin_cli_output(&db, &root, "down", "1", "json");
            let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert!(doc["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["name"] == lhs));
            assert!(doc["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["expression"] == expression
                    && edge["operations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|op| op["kind"] == "write pointer")));
        }
    }
}

#[test]
fn source_dataflow_cli_member_initializers_preserve_terminal_field_writes() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_member_initializer_origins"), &args);
        let conn = open_db(&db).unwrap();
        for (function, parameter, expression, line) in [
            ("Box::Box", "value", "field(value)", 3),
            ("Braced::Braced", "value", "field{value}", 7),
            ("Shadow::Shadow", "field", "field(field)", 11),
            ("Macro::Macro", "value", "field(value)", 16),
            ("Nested::Nested", "value", "field(identity(value))", 21),
        ] {
            let stores: i64 = conn.query_row(
                "SELECT COUNT(*) FROM flow_origins WHERE kind='store' AND operation='write field' AND line=?1 AND expression=?2",
                rusqlite::params![line, expression], |r| r.get(0),
            ).unwrap();
            assert_eq!(stores, 1, "{function}: store needs initializer provenance");
            let root = symbol_in(&conn, function, parameter);
            let view =
                trace_db::dataflow_view(&conn, std::slice::from_ref(&root), Direction::Down, 4)
                    .unwrap();
            let destination = view
                .nodes
                .values()
                .find(|node| node.name == "this.field")
                .unwrap_or_else(|| panic!("{function}: {view:#?}"));
            assert_eq!(destination.kind, "expression");
            assert!(view.edges.iter().any(|edge| edge.to == destination.id
                && edge
                    .provenance
                    .iter()
                    .any(|op| op.operation == "write field"
                        && op.expression == expression
                        && op.location.line == line)));
            let output = source_origin_cli_output(&db, &root, "down", "4", "json");
            let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert!(
                doc["edges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|edge| edge["operations"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|op| op["kind"] == "write field"
                            && op["expression"] == expression
                            && op["location"]["line"] == line)),
                "{output}"
            );
            let text = source_origin_cli_output(&db, &root, "down", "4", "text");
            assert!(
                text.contains("this.field") && text.contains("write field"),
                "{text}"
            );
            if function != "Nested::Nested" {
                assert!(text.contains(expression), "{text}");
                let shallow = trace_db::dataflow_view(&conn, &[root], Direction::Down, 1).unwrap();
                assert_eq!(shallow.edges.len(), 1, "{function}: {shallow:#?}");
            }
            // Reaching a member from `this` loads incoming metadata only.
            let base = symbol_in(&conn, function, "this");
            let from_base = trace_db::dataflow_view(&conn, &[base], Direction::Down, 1).unwrap();
            assert!(from_base
                .nodes
                .values()
                .any(|node| node.name == "this.field"));
            assert!(!from_base
                .edges
                .iter()
                .any(|edge| edge.operations.iter().any(|op| op == "write field")));
        }
        let function_stores: i64 = conn.query_row(
            "SELECT COUNT(*) FROM flow_origins WHERE kind='store' AND expression='field(Later)'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(
            function_stores, 1,
            "function-address member initializers retain provenance"
        );
    }
}

#[test]
fn source_dataflow_cli_recovers_original_expressions_after_unicode() {
    for args in [vec![], vec!["--full-export"]] {
        let db = common::cli_analyze(&fixture("dataflow_unicode_origins"), &args);
        let conn = open_db(&db).unwrap();
        for expression in [
            "q /*keep assignment*/ = p",
            "q /*keep return*/ = identity /*keep callee*/ ( p )",
            "q /*跨🙂*/ =\n        p",
        ] {
            let origins: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM flow_origins WHERE expression=?1",
                    [expression],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(origins > 0, "original expression missing: {expression}");
        }
        for expression in [
            "consume /*keep call*/ ( q )",
            "identity /*keep callee*/ ( p )",
        ] {
            let origins: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM flow_call_expressions WHERE expression=?1",
                    [expression],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(origins, 1, "original call expression missing: {expression}");
        }
        let root = symbol_in(&conn, "unicode", "p");
        let output = source_origin_cli_output(&db, &root, "down", "8", "json");
        let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
        let expressions: Vec<_> = doc["edges"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|edge| edge["operations"].as_array().unwrap())
            .map(|op| op["expression"].as_str().unwrap())
            .collect();
        for expression in [
            "q /*keep assignment*/ = p",
            "q /*跨🙂*/ =\n        p",
            "consume /*keep call*/ ( q )",
            "identity /*keep callee*/ ( p )",
        ] {
            assert!(expressions.contains(&expression), "{output}");
        }
        let text = source_origin_cli_output(&db, &root, "down", "8", "text");
        assert!(text.contains("q /*keep assignment*/ = p"), "{text}");
        assert!(text.contains("consume /*keep call*/ ( q )"), "{text}");
    }
}
