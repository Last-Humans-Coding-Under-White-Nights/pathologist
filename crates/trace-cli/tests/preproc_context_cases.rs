//! A translation unit's macro environment must reach the headers it includes
//! (#55). The include-expansion cache used to be keyed on `(path, language)`
//! alone, so the first expansion of a header — produced by the warm pass under
//! the CLI-defines-only environment — was replayed into every consumer, and a
//! `#define` a TU made before its `#include` had no effect on the header.

mod common;

use common::*;
use trace_analysis::analyze;
use trace_parse::build_program;

/// One TU defining `USE_FAST` before including the header must see the
/// header's `#ifdef USE_FAST` arm. The `#else` arm belongs to a
/// configuration no TU in this tree presents, so it must not be lowered at
/// all: nothing may reach `slow_path`.
#[test]
fn tu_define_reaches_its_header() {
    let root = fixture("preproc/tu_macro_context");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        callees_of(&program, &analysis, "impl")
            .iter()
            .any(|(name, _)| name == "fast_path"),
        "TU's `#define USE_FAST` did not reach cfg.h: {:?}",
        callees_of(&program, &analysis, "impl")
    );
    assert!(
        must_not_have_edge(&program, &analysis, "impl", "slow_path"),
        "the arm no TU selects must not be lowered"
    );
    assert!(has_any_edge(&program, &analysis, "a_main", "impl"));
}

/// Two TUs, one header, two configurations. Both arms are now genuinely
/// present in the tree, so both must be lowered — call resolution unions
/// them (may-analysis), rather than one TU's expansion being replayed into
/// the other.
#[test]
fn two_tus_get_their_own_expansions() {
    let root = fixture("preproc/tu_macro_context_two_tu");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let from_impl: Vec<String> = callees_of(&program, &analysis, "impl")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(
        from_impl.iter().any(|n| n == "fast_path"),
        "a.c's configuration missing: {from_impl:?}"
    );
    assert!(
        from_impl.iter().any(|n| n == "slow_path"),
        "b.c's configuration missing: {from_impl:?}"
    );

    // Both arms were really lowered, each at its own line, rather than one
    // expansion being replayed into both units.
    let arm_line = |name: &str| {
        program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.span.line)
    };
    assert_eq!(arm_line("fast_path"), Some(4));
    assert_eq!(arm_line("slow_path"), Some(6));

    // The two definitions are external `impl`s of the same signature, so the
    // symbol table holds one of them and it carries both configurations'
    // call sites. That union is the may-analysis answer: which arm a given
    // unit compiled is not modelled, and over-approximating is invariant #2.
    assert_eq!(
        program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == "impl")
            .count(),
        1
    );
    assert!(has_any_edge(&program, &analysis, "b_main", "impl"));
}

/// The warm pass accumulated every header's `#define`s into one table handed
/// to every TU, so a macro reached a unit that never included the header
/// defining it. Nothing in `other.c` includes `only.h`, so `LEAKED_CALL()`
/// must stay an unexpanded identifier rather than becoming a call.
#[test]
fn warm_macros_do_not_leak_between_tus() {
    let root = fixture("preproc/warm_macro_leak");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        must_not_have_edge(&program, &analysis, "other", "leaked_target"),
        "a macro from a header `other.c` never includes reached it"
    );
    // The unit that does include the header is unaffected.
    assert!(has_any_edge(&program, &analysis, "uses", "unrelated"));
}

/// `__cplusplus` is predefined for a C++ unit and absent from a C unit
/// (#70). `a.cpp` and `b.c` are the same text: the `#if __cplusplus >=
/// 201103L` declaration must be indexed from the C++ unit only, and the
/// header both include must take its `#ifdef __cplusplus` arm in the C++
/// unit only — each unit's expansion of the shared header is its own (the
/// #55 fixture shape, with the language rather than a TU `#define` as the
/// environment that differs).
#[test]
fn cplusplus_is_predefined_for_cpp_units_only() {
    let root = fixture("preproc/cplusplus_predefined");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let cxx11: Vec<String> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "cxx11_only")
        .map(|f| {
            program
                .symbols
                .files
                .get(f.file.0 as usize)
                .map(|fi| fi.path.display().to_string())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(cxx11.len(), 1, "cxx11_only: {cxx11:?}");
    assert!(
        cxx11[0].ends_with("a.cpp"),
        "indexed from the C unit: {cxx11:?}"
    );

    assert!(
        has_any_edge(&program, &analysis, "a_main", "cxx_path"),
        "a.cpp did not take lang.h's `#ifdef __cplusplus` arm: {:?}",
        callees_of(&program, &analysis, "a_main")
    );
    assert!(
        must_not_have_edge(&program, &analysis, "a_main", "c_path"),
        "a.cpp took lang.h's `#else` arm"
    );
    assert!(
        has_any_edge(&program, &analysis, "b_main", "c_path"),
        "b.c did not take lang.h's `#else` arm: {:?}",
        callees_of(&program, &analysis, "b_main")
    );
    assert!(
        must_not_have_edge(&program, &analysis, "b_main", "cxx_path"),
        "b.c took lang.h's `#ifdef __cplusplus` arm"
    );
}
