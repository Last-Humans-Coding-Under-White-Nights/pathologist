//! A translation unit's macro environment must reach the headers it includes
//! (#55). The include-expansion cache used to be keyed on `(path, language)`
//! alone, so the first expansion of a header — produced by the warm pass under
//! the CLI-defines-only environment — was replayed into every consumer, and a
//! `#define` a TU made before its `#include` had no effect on the header.

mod common;

use common::*;
use trace_analysis::analyze;
use trace_parse::build_program;

#[test]
fn argument_prescan_keeps_active_macro_suppressed_during_rescan() {
    let root = fixture("preproc/macro_prescan_hide");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_any_edge(&program, &analysis, "caller", "f"));
    for name in ["expected_if", "expected_elif"] {
        assert!(program.symbols.resolve_function(name).is_some(), "{name}");
    }
    for name in ["wrong_if", "wrong_first_arm", "wrong_elif"] {
        assert!(program.symbols.resolve_function(name).is_none(), "{name}");
    }
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

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
    // expansion being replayed into both units. The arm bodies' calls are
    // the per-arm facts here: synthesized externals (`fast_path`,
    // `slow_path`) carry no source location of their own, so the arms are
    // told apart by their call sites.
    let arm_call_lines = |name: &str| {
        program
            .symbols
            .call_sites
            .iter()
            .filter(|cs| cs.callee_name == name)
            .map(|cs| cs.span.line)
            .collect::<Vec<_>>()
    };
    assert_eq!(arm_call_lines("fast_path"), vec![4]);
    assert_eq!(arm_call_lines("slow_path"), vec![6]);

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

/// With one expansion slot per header, `decl.h` keeps the expansion the warm
/// pass made without `MODE`, and the one `outer.h` makes under `MODE` cannot
/// be stored. Its classes reach `user.cpp` through `outer.h`'s entry: the
/// call binds to `Session::Add` and dispatches to the override.
#[test]
fn unstored_nested_expansion_reaches_the_includers_consumers() {
    let root = fixture("preproc/unstored_nested_expansion");
    let opts = default_opts(&root).with_max_expansion_variants(1);
    let program = build_program(&root, &opts).expect("build");
    let (_pag, analysis) = analyze(&program);

    for callee in ["Session::Add", "Scan::Add"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "use",
                callee,
                trace_analysis::ResolutionKind::Direct
            ),
            "{callee}: {:?}",
            callees_of(&program, &analysis, "use")
        );
    }
    // `Add(env_t)` as `decl.cpp` defines it, not a second one taking `int`.
    for name in ["Session::Add", "Scan::Add"] {
        let defined: Vec<bool> = program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == name)
            .map(|f| f.is_defined)
            .collect();
        assert_eq!(defined, vec![true], "{name}");
    }
}

/// `decl.h` has both its slots taken when `b.cpp` reaches it under `MODE 1`:
/// `e1.h` holds it, and `e2.h`, included next, skips it by its guard. A unit
/// reaching `decl.h` through `e2.h` alone still sees its classes.
#[test]
fn header_skipping_a_held_header_does_not_hide_it_from_its_consumers() {
    let root = fixture("preproc/unstored_sibling");
    let opts = default_opts(&root).with_max_expansion_variants(2);
    let program = build_program(&root, &opts).expect("build");
    let (_pag, analysis) = analyze(&program);

    for caller in ["use", "b"] {
        for callee in ["Session::Add", "Scan::Add"] {
            assert!(
                has_edge(
                    &program,
                    &analysis,
                    caller,
                    callee,
                    trace_analysis::ResolutionKind::Direct
                ),
                "{caller} -> {callee}: {:?}",
                callees_of(&program, &analysis, caller)
            );
        }
    }
}

/// `test/holder.h` is the header holding `decl.h`'s text. What `decl.h`
/// declares is still `decl.h`'s, production code: `Session::Inl` calls the
/// production `leaf`, not the test double.
#[test]
fn held_header_keeps_its_own_partition() {
    let root = fixture("preproc/unstored_held_partition");
    let opts = default_opts(&root).with_max_expansion_variants(2);
    let program = build_program(&root, &opts).expect("build");
    let (_pag, analysis) = analyze(&program);

    let file_of = |id: trace_ir::FnId| {
        let f = program.symbols.function(id);
        program.symbols.files[f.file.0 as usize]
            .path
            .strip_prefix(trace_ir::canonicalize(&root))
            .unwrap()
            .to_path_buf()
    };
    let leaves: std::collections::BTreeSet<std::path::PathBuf> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == "Session::Inl")
        .filter(|e| fn_name(&program, e.callee) == "leaf")
        .map(|e| file_of(e.callee))
        .collect();
    assert_eq!(
        leaves.into_iter().collect::<Vec<_>>(),
        vec![std::path::PathBuf::from("leaf.cpp")]
    );
    assert!(has_any_edge(&program, &analysis, "use", "Session::Inl"));
}

/// A call a macro spells belongs where the macro is invoked. `CALL_LEAF` is
/// written in the production `decl.h` and invoked in the test header
/// `tdecl.h`, both held by `test/holder.h`: the call is test code and
/// reaches the test double as well.
#[test]
fn held_macro_call_belongs_to_the_header_invoking_it() {
    let root = fixture("preproc/unstored_held_macro");
    let opts = default_opts(&root).with_max_expansion_variants(2);
    let program = build_program(&root, &opts).expect("build");
    let (_pag, analysis) = analyze(&program);

    let leaves: std::collections::BTreeSet<std::path::PathBuf> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == "TestSession::Inl")
        .filter(|e| fn_name(&program, e.callee) == "leaf")
        .map(|e| {
            let f = program.symbols.function(e.callee);
            program.symbols.files[f.file.0 as usize]
                .path
                .strip_prefix(trace_ir::canonicalize(&root))
                .unwrap()
                .to_path_buf()
        })
        .collect();
    assert_eq!(
        leaves.into_iter().collect::<Vec<_>>(),
        vec![
            std::path::PathBuf::from("leaf.cpp"),
            std::path::PathBuf::from("test/mock_leaf.cpp")
        ]
    );
}

/// `test/klass_test.cpp` includes `klass.h` under `#define private public`.
/// The header is the one `klass.cpp` includes: `Twice`, whose body spells
/// `private`, is one function and both units call it.
#[test]
fn unit_renaming_access_specifiers_shares_the_headers_functions() {
    let root = fixture("preproc/access_rename");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let twice = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Twice")
        .count();
    assert_eq!(twice, 1);
    for caller in ["Production", "Test"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "Twice",
                trace_analysis::ResolutionKind::Direct
            ),
            "{caller}: {:?}",
            callees_of(&program, &analysis, caller)
        );
    }
    assert!(has_edge(
        &program,
        &analysis,
        "Twice",
        "Counter::Next",
        trace_analysis::ResolutionKind::Direct
    ));
}
