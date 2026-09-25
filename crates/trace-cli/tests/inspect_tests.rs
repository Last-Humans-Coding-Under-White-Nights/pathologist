//! Inspect-mode integration tests: call graph + dataflow graph construction
//! from an exported database, plus end-to-end binary runs.

use std::path::PathBuf;
mod common;

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
            text.contains(&format!("{reached} (")),
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
