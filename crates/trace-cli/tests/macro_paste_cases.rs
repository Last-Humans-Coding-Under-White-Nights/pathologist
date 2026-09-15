//! Preprocessor token pasting and X-macro generated struct/handler tables.

mod common;

use common::*;
use trace_analysis::{analyze, ResolutionKind};
use trace_parse::build_program;
use trace_preproc::{preprocess_file, PreprocessOptions};

#[test]
fn token_paste_expands_concatenated_symbols() {
    let src = "#define CAT(a,b) a ## b\nint CAT(x,y);\n";
    let result = preprocess_string(Path::new("t.c"), src);
    assert!(
        result.contains("int xy") || result.contains("int x y"),
        "{}",
        result
    );
    assert!(!result.contains("CAT"));
}

#[test]
fn token_paste_builds_handler_name() {
    let root = fixture("macro_paste");
    let path = root.join("main.c");
    let pre = preprocess_file(&path, &default_opts(&root)).unwrap();
    assert!(
        pre.output.contains("gamma_handler"),
        "expected pasted handler name: {}",
        pre.output
    );
    assert!(!pre.output.contains("HANDLER"));
    assert!(!pre.output.contains("CAT"));
}

#[test]
fn xmacro_generates_structs_handlers_and_table() {
    let root = fixture("macro_xmacro");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program
            .types
            .all()
            .iter()
            .any(|t| matches!(t.desc.as_ref(), trace_ir::TypeDesc::Struct { name, .. } if name.contains("alpha") || name.contains("beta"))),
        "X-macro should emit alpha_ctx / beta_ctx structs"
    );
    assert!(
        program
            .flow
            .iter()
            .any(|f| matches!(f, trace_ir::FlowConstraint::ArrayFnMember { .. })),
        "op_table initializer should register ArrayFnMember facts"
    );
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_edge(
            &program,
            &analysis,
            "driver",
            "alpha_handler",
            ResolutionKind::Indirect
        ) || has_any_indirect(&program, &analysis, "driver", "alpha_handler"),
        "driver -> alpha_handler via generated table"
    );
}

fn preprocess_string(file: &std::path::Path, src: &str) -> String {
    trace_preproc::preprocess_string(src, file, &PreprocessOptions::new()).output
}

fn has_any_indirect(
    program: &trace_ir::Program,
    analysis: &trace_analysis::AnalysisResult,
    caller: &str,
    callee: &str,
) -> bool {
    analysis
        .call_edges
        .iter()
        .any(|e| fn_name(program, e.caller) == caller && fn_name(program, e.callee) == callee)
}

use std::path::Path;
