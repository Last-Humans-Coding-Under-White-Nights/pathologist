//! `PreprocessOptions::with_language` is one decision for lexing and
//! parsing: a forced language picks the grammar too, whatever the
//! extension says.
use crate::common;

use common::{default_opts, fixture, has_edge, must_not_have_edge};
use trace_analysis::{analyze, ResolutionKind};
use trace_parse::build_program;
use trace_preproc::Language;

fn parse_error_files(program: &trace_ir::Program) -> Vec<String> {
    program
        .diagnostics
        .iter()
        .filter(|d| d.stage == "parse" && d.message.starts_with("parse errors in"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn forced_cpp_parses_a_c_file_with_the_cpp_grammar() {
    let root = fixture("forced_lang");
    let opts = default_opts(&root).with_language(Language::Cpp);
    let program = build_program(&root, &opts).expect("build");
    let (_pag, analysis) = analyze(&program);

    let errors = parse_error_files(&program);
    assert!(
        !errors.iter().any(|m| m.contains("cpp_syntax.c")),
        "cpp_syntax.c must be parsed with the C++ grammar: {errors:?}"
    );
    assert!(
        program.symbols.functions.iter().any(|f| f.name == "S::m"),
        "the member function must be lowered as C++"
    );
    assert!(
        has_edge(&program, &analysis, "use_m", "S::m", ResolutionKind::Direct),
        "use_m must call the method"
    );
    assert!(
        must_not_have_edge(&program, &analysis, "S::m", "helper"),
        "R\"(x)\" is one raw string in C++, not a call to the R macro"
    );
}

#[test]
fn forced_c_parses_a_cpp_file_with_the_c_grammar() {
    let root = fixture("forced_lang");
    let opts = default_opts(&root).with_language(Language::C);
    let program = build_program(&root, &opts).expect("build");
    let (_pag, analysis) = analyze(&program);

    let errors = parse_error_files(&program);
    assert!(
        !errors.iter().any(|m| m.contains("c_syntax.cpp")),
        "c_syntax.cpp must be parsed with the C grammar: {errors:?}"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "use_class",
            "class",
            ResolutionKind::Direct
        ),
        "`class` is an ordinary function name in C"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "c_char",
            "helper",
            ResolutionKind::Direct
        ),
        "'a'C is a char literal plus the C macro in C, not a user-defined literal"
    );
}
