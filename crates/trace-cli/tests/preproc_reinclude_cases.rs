//! A repeated `#include` is suppressed only on a reason the file itself
//! stated (#56). `process_file` used to skip any path it had already
//! processed in this run, before looking at include guards at all, so every
//! deliberate re-inclusion was lost — X-macro tables above all, silently and
//! without a diagnostic.

mod common;

use common::*;
use trace_analysis::analyze;
use trace_parse::build_program;

fn names(program: &trace_ir::Program) -> Vec<String> {
    let mut v: Vec<String> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.is_defined)
        .map(|f| f.name.clone())
        .collect();
    v.sort();
    v
}

/// The issue's reproduction: one table, two `#define`s of its entry macro.
/// Only the first expansion survived, so `use_alpha` / `use_beta` and both
/// of their call edges were absent from the index.
#[test]
fn x_macro_table_expands_once_per_definition() {
    let root = fixture("preproc/xmacro_table");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert_eq!(
        names(&program),
        vec!["fn_alpha", "fn_beta", "use_alpha", "use_beta"],
        "the second `#include` of the table was suppressed"
    );
    assert!(has_any_edge(&program, &analysis, "use_alpha", "fn_alpha"));
    assert!(has_any_edge(&program, &analysis, "use_beta", "fn_beta"));
}

/// A guard suppresses only while its name is defined. `#undef`ing it and
/// including the header again must expand the body a second time, under the
/// macros in force *then* — and both expansions must reach the index.
#[test]
fn undefining_a_guard_re_expands_its_header() {
    let root = fixture("preproc/guard_undef_reinclude");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_any_edge(
        &program,
        &analysis,
        "first_body",
        "first_target"
    ));
    assert!(has_any_edge(
        &program,
        &analysis,
        "second_body",
        "second_target"
    ));
    assert!(has_any_edge(&program, &analysis, "t_main", "first_body"));
    assert!(has_any_edge(&program, &analysis, "t_main", "second_body"));
}

/// `a.h` includes `b.h` includes `a.h`, both guarded. Termination now comes
/// from the guard rather than from the blanket path set, and neither body
/// may be lost to it.
#[test]
fn guarded_recursive_includes_terminate_with_both_bodies() {
    let root = fixture("preproc/guarded_recursive_include");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_any_edge(&program, &analysis, "a_fn", "a_target"));
    assert!(has_any_edge(&program, &analysis, "b_fn", "b_target"));
    assert!(has_any_edge(&program, &analysis, "t_main", "a_fn"));
    assert!(has_any_edge(&program, &analysis, "t_main", "b_fn"));
}

/// `#pragma once` read with its conditional active holds for the rest of the
/// translation unit: undefining the controlling macro afterwards does not
/// bring the header back.
#[test]
fn active_pragma_once_holds_for_the_rest_of_the_unit() {
    let root = fixture("preproc/pragma_once_active");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert_eq!(names(&program), vec!["first_body", "t_main"]);
    assert!(has_any_edge(
        &program,
        &analysis,
        "first_body",
        "first_target"
    ));
    assert!(must_not_have_edge(
        &program,
        &analysis,
        "second_body",
        "second_target"
    ));
}

/// The other half, and necessarily its own run: reached with the conditional
/// inactive the `#pragma once` is in a skipped group and states nothing, so
/// the header re-expands.
#[test]
fn inactive_pragma_once_does_not_suppress() {
    let root = fixture("preproc/pragma_once_inactive");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_any_edge(
        &program,
        &analysis,
        "first_body",
        "first_target"
    ));
    assert!(has_any_edge(
        &program,
        &analysis,
        "second_body",
        "second_target"
    ));
}

/// A diamond over guarded headers still expands the shared base once: the
/// guard-driven skip keeps feeding the cache frames, so the re-splice the
/// blanket suppression was there to avoid does not come back.
#[test]
fn diamond_include_expands_the_shared_base_once() {
    let root = fixture("preproc/diamond_include");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert_eq!(
        program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == "base_fn" && f.is_defined)
            .count(),
        1,
        "base.h expanded more than once"
    );
    assert!(has_any_edge(&program, &analysis, "left_fn", "base_fn"));
    assert!(has_any_edge(&program, &analysis, "right_fn", "base_fn"));
}
