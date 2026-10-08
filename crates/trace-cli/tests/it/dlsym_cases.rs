//! `dlsym` / `GetProcAddress` models: string constants become fn-ptr targets.

use crate::common;

use common::*;
use trace_analysis::{analyze, analyze_with_options, AnalyzeOptions, ResolutionKind};
use trace_parse::build_program;

fn program() -> trace_ir::Program {
    let root = fixture("dlsym");
    build_program(&root, &default_opts(&root)).expect("build")
}

fn reaches_target(
    program: &trace_ir::Program,
    analysis: &trace_analysis::AnalysisResult,
    caller: &str,
) {
    assert!(
        has_edge(
            program,
            analysis,
            caller,
            "target",
            ResolutionKind::Indirect
        ),
        "{caller} should reach target via dlsym-modeled function pointer"
    );
    assert!(
        must_not_have_edge(program, analysis, caller, "other"),
        "{caller} must not fan out to unrelated functions"
    );
}

#[test]
fn dlsym_literal_and_constant_name_resolve_target() {
    let program = program();
    let (_pag, analysis) = analyze(&program);

    for caller in [
        "call_literal",
        "call_var",
        "call_copy",
        "call_concat",
        "call_wrap",
        "call_global",
        "call_getproc",
        "call_cast_invoke",
    ] {
        reaches_target(&program, &analysis, caller);
    }
}

#[test]
fn dlsym_unknown_name_does_not_invent_callees() {
    let program = program();
    let (_pag, analysis) = analyze(&program);

    for caller in ["call_missing", "call_unknown"] {
        assert!(
            must_not_have_edge(&program, &analysis, caller, "target"),
            "{caller} must not resolve target without a matching string constant"
        );
        assert!(
            must_not_have_edge(&program, &analysis, caller, "other"),
            "{caller} must not fan out to all exports"
        );
    }
}

#[test]
fn dlsym_requires_builtin_model() {
    let program = program();
    let analysis = analyze_with_options(
        &program,
        AnalyzeOptions {
            models: std::sync::Arc::new(trace_analysis::FnModelSet::default()),
            ..Default::default()
        },
    )
    .1;
    assert!(
        must_not_have_edge(&program, &analysis, "call_literal", "target"),
        "without the dlsym model, a string-literal lookup stays unresolved"
    );
}
