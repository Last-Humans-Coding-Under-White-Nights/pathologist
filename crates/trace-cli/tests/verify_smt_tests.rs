#![cfg(feature = "smt")]

mod common;

use trace_analysis::analyze;
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn callchain_smt_infeasible_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void sink(void) {}

        void step(int x) {
            if (x > 10) {
                sink();
            }
        }

        int main(void) {
            int val = 5;
            if (val < 10) {
                step(val);
            }
            return 0;
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    let db = common::export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(db.path()).expect("open exported database");

    let main_fn = trace_db::require_function_by_name(&conn, "main", None).unwrap();
    let sink_fn = trace_db::require_function_by_name(&conn, "sink", None).unwrap();

    // 1. Without verification: call chain exists in may-analysis graph
    let chains_unverified = trace_db::call_chains(
        &conn,
        main_fn.id,
        sink_fn.id,
        trace_db::Direction::Down,
        5,
        None,
    )
    .unwrap();
    assert_eq!(chains_unverified.chains.len(), 1, "may-analysis finds candidate chain");

    // 2. With SMT verification: val < 10 && x > 10 (with x == val) is UNSAT, so pruned
    let mut chains_verified = chains_unverified.clone();
    chains_verified.chains.retain(|c| {
        trace_db::verify_call_chain(&conn, c) != trace_db::PathFeasibility::Infeasible
    });
    assert_eq!(
        chains_verified.chains.len(),
        0,
        "SMT path verification must prune contradictory chain"
    );
}

#[test]
fn callchain_smt_feasible_retained() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void sink(void) {}

        void step(int x) {
            if (x > 0) {
                sink();
            }
        }

        int main(void) {
            int val = 5;
            if (val > 0) {
                step(val);
            }
            return 0;
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    let db = common::export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(db.path()).expect("open exported database");

    let main_fn = trace_db::require_function_by_name(&conn, "main", None).unwrap();
    let sink_fn = trace_db::require_function_by_name(&conn, "sink", None).unwrap();

    let chains = trace_db::call_chains(
        &conn,
        main_fn.id,
        sink_fn.id,
        trace_db::Direction::Down,
        5,
        None,
    )
    .unwrap();
    assert_eq!(chains.chains.len(), 1);

    let feasibility = trace_db::verify_call_chain(&conn, &chains.chains[0]);
    assert_eq!(
        feasibility,
        trace_db::PathFeasibility::Feasible,
        "consistent condition must be verified as feasible"
    );
}

#[test]
fn callchain_smt_early_return_guard() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void sink(void) {}

        void worker(int *ptr) {
            if (ptr == 0) {
                return;
            }
            sink();
        }

        int main(void) {
            int *p = 0;
            worker(p);
            return 0;
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    let db = common::export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(db.path()).expect("open exported database");

    let main_fn = trace_db::require_function_by_name(&conn, "main", None).unwrap();
    let sink_fn = trace_db::require_function_by_name(&conn, "sink", None).unwrap();

    let chains = trace_db::call_chains(
        &conn,
        main_fn.id,
        sink_fn.id,
        trace_db::Direction::Down,
        5,
        None,
    )
    .unwrap();
    assert_eq!(chains.chains.len(), 1);

    let feasibility = trace_db::verify_call_chain(&conn, &chains.chains[0]);
    assert_eq!(
        feasibility,
        trace_db::PathFeasibility::Infeasible,
        "early return guard must render subsequent call infeasible for null argument"
    );
}

#[test]
fn call_edges_smt_verify_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void sink(void) {}

        int main(void) {
            int x = -1;
            if (x > 0) {
                sink();
            }
            return 0;
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    let db = common::export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(db.path()).expect("open exported database");

    let edges = trace_db::call_edges(
        &conn,
        &trace_db::CallEdgeFilter {
            from: Some("main"),
            to: Some("sink"),
            file: None,
            exclude_deps: false,
        },
    )
    .unwrap();
    assert_eq!(edges.len(), 1, "may-analysis includes guarded edge");

    let edge = &edges[0];
    let chain = trace_db::CallChain {
        nodes: vec![edge.caller_id],
        edges: vec![trace_db::CallChainEdge {
            caller_id: edge.caller_id,
            callee_id: 0,
            resolution: edge.resolution.clone(),
            site: trace_db::EdgeSite {
                path: edge.call_site_path.clone().unwrap(),
                line: edge.call_site_line.unwrap(),
                col: edge.call_site_col.unwrap_or(0),
            },
        }],
    };
    assert_eq!(
        trace_db::verify_call_chain(&conn, &chain),
        trace_db::PathFeasibility::Infeasible,
        "single call edge with impossible guard must be deemed infeasible"
    );
}

#[test]
fn callchain_smt_multi_hop_parameter_propagation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void sink(void) {}

        void hop2(int b) {
            // b will be 8, so b < 7 is UNSAT
            if (b < 7) {
                sink();
            }
        }

        void hop1(int a) {
            if (a > 5) {
                hop2(a - 2);
            }
        }

        int main(void) {
            int init = 10;
            hop1(init);
            return 0;
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    let db = common::export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(db.path()).expect("open exported database");

    let main_fn = trace_db::require_function_by_name(&conn, "main", None).unwrap();
    let sink_fn = trace_db::require_function_by_name(&conn, "sink", None).unwrap();

    let chains = trace_db::call_chains(
        &conn,
        main_fn.id,
        sink_fn.id,
        trace_db::Direction::Down,
        10,
        None,
    )
    .unwrap();
    assert_eq!(chains.chains.len(), 1, "may-analysis finds candidate chain across 3 hops");

    let feasibility = trace_db::verify_call_chain(&conn, &chains.chains[0]);
    assert_eq!(
        feasibility,
        trace_db::PathFeasibility::Infeasible,
        "multi-hop propagation with b = a - 2 must detect 8 < 7 is infeasible"
    );
}

#[test]
fn callchain_smt_multi_hop_feasible() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void sink(void) {}

        void hop2(int b) {
            // b will be 8, so b >= 8 is SAT
            if (b >= 8) {
                sink();
            }
        }

        void hop1(int a) {
            if (a > 5) {
                hop2(a - 2);
            }
        }

        int main(void) {
            int init = 10;
            hop1(init);
            return 0;
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    let db = common::export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(db.path()).expect("open exported database");

    let main_fn = trace_db::require_function_by_name(&conn, "main", None).unwrap();
    let sink_fn = trace_db::require_function_by_name(&conn, "sink", None).unwrap();

    let chains = trace_db::call_chains(
        &conn,
        main_fn.id,
        sink_fn.id,
        trace_db::Direction::Down,
        10,
        None,
    )
    .unwrap();
    assert_eq!(chains.chains.len(), 1);

    let feasibility = trace_db::verify_call_chain(&conn, &chains.chains[0]);
    assert_eq!(
        feasibility,
        trace_db::PathFeasibility::Feasible,
        "multi-hop propagation with b = a - 2 must detect 8 >= 8 is feasible"
    );
}

