//! Adversarial C patterns: macros, memcpy, casts, tables.
//! Tests document both expected behavior and known soundness gaps.

mod common;

use common::*;
use std::sync::Arc;
use trace_analysis::{
    analyze, analyze_with_options, AnalysisResult, AnalyzeOptions, ResolutionKind,
};
use trace_ir::{FlowConstraint, Program};
use trace_parse::build_program;
use trace_preproc::preprocess_file;

fn has_any_edge(program: &Program, analysis: &AnalysisResult, caller: &str, callee: &str) -> bool {
    !must_not_have_edge(program, analysis, caller, callee)
}

// --- Macros (preprocessor must expand before parse) ---

#[test]
fn macro_field_access_expands_to_store_flow() {
    let root = fixture("macro_field");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program.flow.iter().any(|f| {
            matches!(
                f,
                FlowConstraint::Store { .. } | FlowConstraint::GepField { .. }
            )
        }),
        "FIELD_P macro should expand to field store in macro_assign"
    );
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_edge(
            &program,
            &analysis,
            "macro_user",
            "sink",
            // `sink` is prototype-only in this fixture: resolved statically,
            // but classified external because no definition exists here.
            ResolutionKind::External
        ),
        "macro_user -> sink"
    );
}

#[test]
fn nested_field_macros_emit_gep_chain() {
    let root = fixture("macro_nested_field");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let geps = program
        .flow
        .iter()
        .filter(|f| matches!(f, FlowConstraint::GepField { .. }))
        .count();
    assert!(
        geps >= 2,
        "WRAP_SLOT should expand to nested field path, got {geps} GEPs"
    );
}

#[test]
fn macro_indirect_call_resolves_target() {
    let root = fixture("macro_indirect");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "via_macro_indirect", "target"),
        "INVOKE(fp) should resolve to target"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "via_macro_direct_name",
            "decoy",
            ResolutionKind::Direct
        ),
        "INVOKE(decoy) is a direct call"
    );
    assert!(
        must_not_have_edge(&program, &analysis, "via_macro_indirect", "decoy"),
        "via_macro_indirect must not reach decoy"
    );
}

#[test]
fn union_field_macro_store_flow() {
    let root = fixture("union_macro");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program
            .flow
            .iter()
            .any(|f| matches!(f, FlowConstraint::Store { .. })),
        "UNION_P macro should produce store flow"
    );
}

#[test]
fn preproc_expands_field_macro_before_parse() {
    let path = fixture("macro_field").join("main.c");
    let pre = preprocess_file(&path, &default_opts(&fixture("macro_field"))).unwrap();
    assert!(
        !pre.output.contains("FIELD_P"),
        "macro must be expanded: {}",
        pre.output
    );
    assert!(
        pre.output.contains("inner") && pre.output.contains("p"),
        "expanded field access should remain: {}",
        pre.output
    );
}

/// Function-like `#define FIELD_P(o) ...` expands and produces field store flow.
#[test]
fn function_like_field_macro_produces_flow() {
    let root = fixture("macro_fnlike");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program.flow.iter().any(|f| {
            matches!(
                f,
                FlowConstraint::Store { .. } | FlowConstraint::GepField { .. }
            )
        }),
        "FIELD_P(o) macro should expand to field store flow"
    );
}

// --- Comma operator and casts ---

#[test]
fn comma_operator_indirect_call() {
    let root = fixture("comma_fnptr");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "comma_indirect", "alpha"),
        "comma assignment then call should reach alpha"
    );
}

#[test]
fn cast_chain_preserves_indirect_target() {
    let root = fixture("cast_fnptr");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "cast_indirect", "through_cast"),
        "opaque void* cast round-trip should still call through_cast"
    );
}

// --- May-analysis: branch merge ---

#[test]
fn branch_merge_reports_both_callees() {
    let root = fixture("merge_branches");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "ambiguous_branch", "path_a"),
        "may-analysis: path_a reachable"
    );
    assert!(
        has_any_edge(&program, &analysis, "ambiguous_branch", "path_b"),
        "may-analysis: path_b reachable"
    );
}

// --- False-positive guards (soundness) ---

#[test]
fn memcpy_into_side_buffer_does_not_widen_fn_ptr() {
    let root = fixture("memcpy_false_pos");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "memcpy_side_buffer", "fn_a"),
        "fp still calls fn_a"
    );
    assert!(
        must_not_have_edge(&program, &analysis, "memcpy_side_buffer", "fn_b"),
        "memcpy to side buffer must not connect fp to fn_b"
    );
}

#[test]
fn comma_without_assignment_keeps_first_fn_ptr() {
    let root = fixture("comma_fnptr");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "comma_still_alpha", "alpha"),
        "(void)0, fp() should still call alpha"
    );
    assert!(
        must_not_have_edge(&program, &analysis, "comma_still_alpha", "beta"),
        "comma must not widen to beta"
    );
}

// --- libc memcpy/memmove modeling (function models, see docs/ANALYSIS.md) ---

/// `memcpy(&fp, &src, n)` copies fn-ptr bits: the builtin model resolves the
/// indirect call through the copy. Without models the gap remains.
#[test]
fn memcpy_fnptr_indirect_resolved_via_model() {
    let root = fixture("memcpy_fnptr");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let with_models = analyze(&program);
    assert!(
        has_any_edge(&program, &with_models.1, "memcpy_indirect", "real_target"),
        "builtin mem_copy model must carry the fn-ptr through memcpy"
    );
    assert!(
        has_any_edge(&program, &with_models.1, "memcpy_no_fn_edge", "real_target"),
        "plain fp() path still works"
    );
    assert!(
        must_not_have_edge(&program, &with_models.1, "memcpy_no_fn_edge", "ghost"),
        "memset/memcpy on blob must not invent ghost edge"
    );

    // Baseline without models keeps the documented limitation.
    let no_models = analyze_with_options(
        &program,
        AnalyzeOptions {
            models: Arc::new(trace_analysis::FnModelSet::default()),
            ..Default::default()
        },
    );
    assert!(
        !has_any_edge(&program, &no_models.1, "memcpy_indirect", "real_target"),
        "without models the copied fn-ptr stays unresolved"
    );
}

/// memmove staging of fn-ptr — same model coverage as memcpy.
#[test]
fn memmove_staged_fnptr_resolved_via_model() {
    let root = fixture("memmove_fnptr");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let with_models = analyze(&program);
    assert!(
        has_any_edge(&program, &with_models.1, "memmove_indirect", "mover_target"),
        "builtin mem_copy model carries the fn-ptr through staged memmove"
    );
}

/// Subscript on fn-ptr table: may-analysis resolves to all initializer targets.
#[test]
fn fn_ptr_table_over_approximates_all_entries() {
    let root = fixture("fn_ptr_table");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program
            .flow
            .iter()
            .any(|f| matches!(f, FlowConstraint::ArrayFnMember { .. })),
        "expected ArrayFnMember for each table initializer"
    );
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "dispatch_table", "row0"),
        "table[0]() reaches row0"
    );
    assert!(
        !has_any_edge(&program, &analysis, "dispatch_table", "row1"),
        "table[0]() prunes row1 via Phase S4 index refinement"
    );
    assert!(
        has_any_edge(&program, &analysis, "dispatch_unknown", "row0")
            && has_any_edge(&program, &analysis, "dispatch_unknown", "row1"),
        "unknown index: reaches both row0 and row1 (over-approx)"
    );
}

/// memcpy of a struct that embeds a fn-ptr — the MemCopy model wires the
/// fn-ptr field through the whole-object copy, and GEP processing resolves
/// the indirect call.
#[test]
fn memcpy_struct_fnptr_resolves_indirect() {
    let root = fixture("memcpy_struct_fn");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "struct_holder_memcpy", "embedded"),
        "struct fn-ptr field should resolve through memcpy MemCopy model"
    );
}

/// memcpy_s into a sub-field (`memcpy_s(&drv->chipData, ..., chip, ...)`)
/// must flow the source op-table function pointers through the MemCopy model
/// so that indirect calls through the destination resolve.
#[test]
fn memcpy_s_member_field_resolves_fnptrs() {
    let root = fixture("memcpy_s_member_field");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_any_edge(
            &program,
            &analysis,
            "test_indirect_through_memcpy_s",
            "SetPpgEnable"
        ),
        "Enable op should resolve through memcpy_s into sub-field"
    );
    assert!(
        has_any_edge(
            &program,
            &analysis,
            "test_indirect_through_memcpy_s",
            "SetPpgDisable"
        ),
        "Disable op should resolve through memcpy_s into sub-field"
    );
    assert!(
        has_any_edge(
            &program,
            &analysis,
            "test_indirect_through_memcpy_s",
            "SetPpgReadData"
        ),
        "ReadData op should resolve through memcpy_s into sub-field"
    );
}
