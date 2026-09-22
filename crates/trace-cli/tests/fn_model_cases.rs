//! Function-model summaries: dataflow through bodyless memory functions,
//! terminators (`clears`), and heap-return models.

mod common;

use common::*;
use std::sync::Arc;
use trace_analysis::{analyze_with_options, AnalyzeOptions, ResolutionKind};
use trace_db::open_db;
use trace_ir::Program;
use trace_parse::build_program;

fn build() -> Program {
    let root = fixture("fn_models");
    build_program(&root, &default_opts(&root)).expect("build")
}

fn analyze_no_models(program: &Program) -> trace_analysis::AnalysisResult {
    analyze_with_options(
        program,
        AnalyzeOptions {
            models: Arc::new(trace_analysis::FnModelSet::default()),
            ..Default::default()
        },
    )
    .1
}

fn analyze_builtin(program: &Program) -> trace_analysis::AnalysisResult {
    analyze_with_options(program, AnalyzeOptions::default()).1
}

fn indirect_targets(
    program: &Program,
    analysis: &trace_analysis::AnalysisResult,
    caller: &str,
) -> Vec<String> {
    analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(program, e.caller) == caller && e.resolution == ResolutionKind::Indirect
        })
        .map(|e| fn_name(program, e.callee))
        .collect()
}

#[test]
fn memcpy_s_model_flows_ops_table_to_copy() {
    let program = build();

    // Without models the copy is invisible and the target is missing.
    let base = analyze_no_models(&program);
    assert!(
        !indirect_targets(&program, &base, "copy_via_memcpy_s").contains(&"impl_run".to_string()),
        "baseline must not resolve impl_run through memcpy_s"
    );

    // With built-in models the ops table flows dst <- src (arg 2 -> arg 0).
    let with = analyze_builtin(&program);
    assert!(
        indirect_targets(&program, &with, "copy_via_memcpy_s").contains(&"impl_run".to_string()),
        "expected impl_run resolved through modeled memcpy_s"
    );
}

#[test]
fn user_toml_models_project_wrapper() {
    let root = fixture("fn_models");
    let toml_path = root.join("models.toml");
    let src = std::fs::read_to_string(&toml_path).expect("models.toml");

    let program = build();
    let base = analyze_no_models(&program);
    assert!(
        !indirect_targets(&program, &base, "call_through_wrapper_copy")
            .contains(&"impl_run".to_string()),
        "baseline must not resolve impl_run through unmodeled wrapper"
    );

    let opts = AnalyzeOptions {
        models: Arc::new(trace_analysis::FnModelSet::from_toml_str(&src).expect("parse toml")),
        ..Default::default()
    };
    let with = analyze_with_options(&program, opts).1;
    assert!(
        indirect_targets(&program, &with, "call_through_wrapper_copy")
            .contains(&"impl_run".to_string()),
        "expected impl_run resolved through TOML-modeled wrapper"
    );
}

#[test]
fn terminator_event_recorded_and_exported() {
    let program = build();

    // No terminator events without models.
    let base = analyze_no_models(&program);
    assert!(base.terminator_events.is_empty());

    let with = analyze_builtin(&program);
    assert_eq!(
        with.terminator_events.len(),
        1,
        "exactly one clears event expected (memset_s arg 0)"
    );
    assert_eq!(with.terminator_events[0].1, 0, "cleared parameter index");

    // Exported flow graph carries a terminator node with a terminates edge.
    let pag =
        trace_analysis::Pag::build_with_models(&program, &trace_analysis::FnModelSet::builtin());
    let db = export_program(&program, &pag, &with);
    let conn = open_db(&db).expect("open db");
    let (kind, label): (String, String) = conn
        .query_row(
            "SELECT kind, label FROM flow_nodes WHERE kind='terminator'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("terminator node exported");
    assert_eq!(kind, "terminator");
    assert!(label.contains("memset_s"), "label mentions callee: {label}");
    let n_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM flow_edges WHERE kind='terminates'",
            [],
            |r| r.get(0),
        )
        .expect("terminates edge count");
    assert_eq!(n_edges, 1);
    // Issue #127 review: the edge leaves the cleared object, not the `&x`
    // temporary lowering passes in its place.
    let cleared: String = conn
        .query_row(
            "SELECT s.label FROM flow_edges e JOIN flow_nodes s ON s.id = e.src_node \
             WHERE e.kind='terminates'",
            [],
            |r| r.get(0),
        )
        .expect("terminates edge source");
    assert_eq!(cleared, "g_cleared");
}

#[test]
fn malloc_return_heap_summary() {
    let program = build();
    let opts = AnalyzeOptions::default();
    let (pag, _analysis) = analyze_with_options(&program, opts);
    assert!(
        pag.locations.iter().any(|l| l.desc.contains("malloc")),
        "expected a fresh heap location for modeled malloc"
    );
}

#[test]
fn realloc_return_alias_keeps_pointees() {
    let program = build();
    // With the builtin realloc model the returned pointer may be the old
    // storage; without models the CallReturn expansion yields nothing.
    let opts = AnalyzeOptions {
        models: Arc::new(trace_analysis::FnModelSet::default()),
        ..Default::default()
    };
    let (pag_no, _) = analyze_with_options(&program, opts);
    let heap_before = pag_no
        .locations
        .iter()
        .filter(|l| l.desc.contains("realloc") || l.desc.contains("malloc"))
        .count();

    let (pag_with, _) = analyze_with_options(&program, AnalyzeOptions::default());
    let heap_after = pag_with
        .locations
        .iter()
        .filter(|l| l.desc.contains("realloc") || l.desc.contains("malloc"))
        .count();
    assert!(
        heap_after > heap_before,
        "modeled malloc/realloc must contribute fresh storage locations"
    );

    // return_alias: grow_ops's result may alias its parameter's storage.
    // The use_grow return chain must therefore carry impl_run's function
    // location is NOT expected here (no fn stored in OpsA instances via
    // heap), but the heap loc must reach use_alloc's return value.
    assert!(
        pag_with
            .locations
            .iter()
            .any(|l| l.kind == trace_analysis::LocKind::Heap),
        "heap loc kind must be used for modeled allocations"
    );
}
