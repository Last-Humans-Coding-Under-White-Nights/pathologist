//! A header shared by C and C++ translation units is lexed per language.
mod common;

use common::{default_opts, fixture, has_edge, must_not_have_edge};
use trace_analysis::{analyze, ResolutionKind};
use trace_parse::build_program;

#[test]
fn shared_header_macros_replay_in_each_units_language() {
    // shared.h defines `R` and `C` as macros and uses them in `R"(x)"` and
    // `'a'C`. In the C unit those are macro invocations that call `helper`;
    // in the C++ unit they are a raw string and a user-defined literal. The
    // header is warmed once per language and the expansion cache is keyed
    // by language, so the C++ tokenization is never replayed into a.c.
    let root = fixture("mixed_lang");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for name in ["c_raw", "c_char", "cpp_raw", "cpp_char"] {
        assert!(
            program.symbols.functions.iter().any(|f| f.name == name),
            "{name} must be indexed"
        );
    }
    for caller in ["c_raw", "c_char"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "helper",
                ResolutionKind::Direct
            ),
            "{caller}: the C lexer must expand R / C into helper calls"
        );
    }
    for caller in ["cpp_raw", "cpp_char"] {
        assert!(
            must_not_have_edge(&program, &analysis, caller, "helper"),
            "{caller}: a raw string / ud-suffix is not a macro invocation in C++"
        );
    }
}

#[test]
fn header_macros_do_not_reach_units_that_do_not_include_it() {
    // c_only.h is reached only from a.c and cpp_only.h only from b.cpp.
    // Each unit uses the OTHER header's macro without including it:
    // a.c's `c_leak` names CHAR_LEAK (cpp_only.h) and b.cpp's `cpp_leak`
    // names RAW_LEAK (c_only.h). The warm pass used to union every header's
    // macros into one table per language and hand it to every unit, so both
    // expanded and called `helper`. A unit's macro environment is now what
    // its own `#include`s give it (#55), so neither name is a macro here and
    // neither call happens.
    let root = fixture("mixed_lang");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for name in ["c_leak", "cpp_leak"] {
        assert!(
            program.symbols.functions.iter().any(|f| f.name == name),
            "{name} must be indexed"
        );
        assert!(
            must_not_have_edge(&program, &analysis, name, "helper"),
            "{name}: a macro from a header this unit never includes reached it"
        );
    }
}

#[test]
fn header_reclassified_by_a_macro_include_is_preprocessed_in_its_parse_language() {
    // late.h is reached from a.c directly and from b.cpp only through
    // `#include LATE_H` in via.h, which the raw include scanner misses. The
    // first warm pass therefore lexes late.h as C (R expands to a helper
    // call). After via.h's warm discovers the edge, late.h is parsed as
    // C++, so its cached text must be the C++ preprocess: `R"(x)"` is one
    // raw string there and `late_raw` calls nothing.
    let root = fixture("mixed_lang_late");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "late_raw"),
        "late_raw must be indexed"
    );
    assert!(
        !program
            .diagnostics
            .iter()
            .any(|d| d.message.starts_with("parse errors in")),
        "no unit may have parse errors: {:?}",
        program.diagnostics
    );
    assert!(
        must_not_have_edge(&program, &analysis, "late_raw", "helper"),
        "late.h is parsed as C++ (reachable from b.cpp), so it must be preprocessed as C++"
    );
    for caller in ["c_user", "cpp_user"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "late_raw",
                ResolutionKind::Direct
            ),
            "{caller} must see late_raw"
        );
    }
}
