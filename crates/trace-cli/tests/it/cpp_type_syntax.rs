//! #167, #169, #170 together: computed `decltype` bases, GNU `__typeof__`
//! aliases and parenthesized member-function pointers across two project
//! translation units sharing a declaration-only dependency header, with a
//! project macro spelling each construct. Checks the exported database
//! (minimal and full), determinism across runs and job counts, and the
//! in-memory descriptors the export has no table for
//! (docs/ANALYSIS.md, "C++ parse-input normalization").

use crate::common;

use common::{analysis_rows, cli_analyze, export_program, export_program_full, scratch, text_rows};
use std::path::Path;
use trace_analysis::analyze;
use trace_ir::{Program, TypeDesc};
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

const DEP_API: &str = "// Declaration-only dependency.\n\
template<class T> T make();\n\
\n\
struct Base { void base(); };\n\
\n\
template<class T>\n\
struct Derived : decltype(make<T>()), Base {\n\
    void member() { helper(); }\n\
    void helper();\n\
};\n\
\n\
template<class T, class U>\n\
struct sum_result {\n\
    typedef __typeof__(T() + U()) type;\n\
    typedef __typeof__(int) itype;\n\
    typedef __typeof(T *) ptype;\n\
};\n\
\n\
template<class R, class C, class... Args>\n\
struct Holder {\n\
    using Member = R (C::*)(Args...) const;\n\
    typedef R (C::*Named)(Args...) const;\n\
    void useful();\n\
};\n\
\n\
int dep_after();\n";

const FIRST_CPP: &str = "#include \"dep/api.hpp\"\n\
#define COMPUTED(T) decltype(make<T>())\n\
#define MY_TYPEOF(e) __typeof__(e)\n\
#define MEMBER(R, C) R (C::*)() const\n\
\n\
struct Mine { int m() const; };\n\
\n\
template<class T>\n\
struct First : COMPUTED(T), Base {\n\
    typedef MY_TYPEOF(T() + 1) type;\n\
    typedef MY_TYPEOF(int *) ptype;\n\
    using Member = MEMBER(int, Mine);\n\
    typedef int (Mine::*Named)() const;\n\
    void first() { make<T>(); }\n\
};\n\
\n\
int first_defined() { return 1; }\n\
int main() { return first_defined(); }\n";

const SECOND_CPP: &str = "#include \"dep/api.hpp\"\n\
\n\
struct Other { int o(double) const; };\n\
template<class T> struct Second : decltype(make<T>()) {\n\
    using M = int (Other::*)(double) const;\n\
    void second();\n\
};\n\
int second_defined() { return 2; }\n\
int second_caller() { return second_defined() + dep_after(); }\n";

fn tree() -> tempfile::TempDir {
    scratch(&[
        ("dep/api.hpp", DEP_API),
        ("first.cpp", FIRST_CPP),
        ("second.cpp", SECOND_CPP),
    ])
}

fn build(root: &Path, jobs: usize) -> Program {
    build_program_with_jobs(
        root,
        &PreprocessOptions::new().with_dep(root.join("dep")),
        jobs,
    )
    .expect("build program")
}

fn alias(program: &Program, name: &str) -> TypeDesc {
    program
        .types
        .resolve_alias(name)
        .unwrap_or_else(|| {
            panic!(
                "missing alias {name}; have {:?}",
                program.types.all_aliases().keys().collect::<Vec<_>>()
            )
        })
        .clone()
}

fn function<'a>(program: &'a Program, name: &str) -> &'a trace_ir::Function {
    let found: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == name)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "{name}: {:?}",
        program
            .symbols
            .functions
            .iter()
            .map(|f| &f.name)
            .collect::<Vec<_>>()
    );
    found[0]
}

#[test]
fn combined_constructs_index_declarations_aliases_and_positions() {
    let root = tree();
    let program = build(root.path(), 1);
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );

    // #167: concrete bases survive, computed ones are omitted, bodies lower.
    for cls in ["Derived", "First"] {
        assert_eq!(program.bases_of(cls), ["Base"], "{cls}");
    }
    assert_eq!(program.bases_of("Second"), Vec::<String>::new());
    assert!(program
        .inheritance()
        .iter()
        .all(|(_, b)| !b.contains("decltype")));

    // #169: alias identities and descriptors.
    assert_eq!(alias(&program, "sum_result::type"), TypeDesc::Unknown);
    assert_eq!(alias(&program, "sum_result::itype"), TypeDesc::Int);
    assert!(matches!(
        alias(&program, "sum_result::ptype"),
        TypeDesc::Ptr(_)
    ));
    assert_eq!(alias(&program, "First::type"), TypeDesc::Unknown);
    assert_eq!(
        alias(&program, "First::ptype"),
        TypeDesc::Ptr(Box::new(TypeDesc::Int))
    );
    for wrong in ["sum_result::T()+", "sum_result::int", "First::T()+"] {
        assert!(program.types.resolve_alias(wrong).is_none(), "{wrong}");
    }

    // #170: coarse function-pointer shape in every context.
    for member in [
        "Holder::Member",
        "Holder::Named",
        "First::Member",
        "First::Named",
        "Second::M",
    ] {
        let desc = alias(&program, member);
        let inner = match &desc {
            TypeDesc::Ptr(inner) => inner.as_ref(),
            other => other,
        };
        assert!(
            matches!(inner, TypeDesc::FnPtr { ret, .. } if !matches!(ret.as_ref(), TypeDesc::FnPtr { .. })),
            "{member}: {desc:?}"
        );
    }

    // Declarations, ownership and original positions (name column).
    let member = function(&program, "Derived::member");
    assert!(program.symbols.file_is_dep(member.span.file));
    assert!(!member.is_defined);
    assert!(member.locals.is_empty());
    assert_eq!(member.span.line, 8);
    assert_eq!(function(&program, "Holder::useful").span.line, 23);
    let dep_after = function(&program, "dep_after");
    assert_eq!((dep_after.span.line, dep_after.span.col), (26, 5));
    let first = function(&program, "First::first");
    assert!(!program.symbols.file_is_dep(first.span.file));
    assert!(first.is_defined);
    assert_eq!(first.span.line, 14);
    let first_defined = function(&program, "first_defined");
    // Definitions anchor their column like any other definition does.
    assert_eq!(first_defined.span.line, 17);
    assert_eq!(
        first_defined.span.col,
        function(&program, "second_defined").span.col
    );
    assert_eq!(function(&program, "Second::second").span.line, 6);
    assert_eq!(function(&program, "second_caller").span.line, 9);
    assert!(function(&program, "main").is_defined);

    // No phantom symbols from operands or template parameters.
    for phantom in ["R", "C", "Args", "T", "U", "type", "Member", "Named"] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == phantom),
            "{phantom}"
        );
    }
    // Calls come from project bodies only; operands are never calls.
    let callers: std::collections::BTreeSet<String> = program
        .symbols
        .call_sites
        .iter()
        .map(|cs| common::fn_name(&program, cs.caller))
        .collect();
    assert_eq!(
        callers.into_iter().collect::<Vec<_>>(),
        ["First::first", "main", "second_caller"]
    );
}

fn exported_checks(db: &common::TempDb) {
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let parse_diags = text_rows(
        &conn,
        "SELECT message FROM diagnostics WHERE stage = 'parse'",
    );
    assert_eq!(parse_diags, Vec::<Vec<String>>::new());
    let rows = text_rows(
        &conn,
        "SELECT f.name, f.is_defined, f.is_dep, fi.is_dep, f.line_start \
         FROM functions f JOIN files fi ON fi.id = f.file_id \
         WHERE f.name IN ('Derived::member', 'Holder::useful', 'dep_after', 'First::first', \
                          'first_defined', 'Second::second', 'second_caller', 'main') \
         ORDER BY f.name",
    );
    let expect = |name: &str, defined: i64, dep: i64, line: i64| {
        vec![
            format!("Text({name:?})"),
            format!("Integer({defined})"),
            format!("Integer({dep})"),
            format!("Integer({dep})"),
            format!("Integer({line})"),
        ]
    };
    assert_eq!(
        rows,
        vec![
            expect("Derived::member", 0, 1, 8),
            expect("First::first", 1, 0, 14),
            expect("Holder::useful", 0, 1, 23),
            expect("Second::second", 0, 0, 6),
            expect("dep_after", 0, 1, 26),
            expect("first_defined", 1, 0, 17),
            expect("main", 1, 0, 18),
            expect("second_caller", 1, 0, 9),
        ]
    );
    let phantoms = text_rows(
        &conn,
        "SELECT name FROM functions WHERE name IN ('R', 'C', 'Args', 'T', 'U', 'type', 'Member', 'Named') \
         OR name LIKE '%decltype%' OR name LIKE '%typeof%' OR name LIKE '%::*%'",
    );
    assert_eq!(phantoms, Vec::<Vec<String>>::new());
    let edges = text_rows(
        &conn,
        "SELECT a.name, b.name FROM call_edges e \
         JOIN functions a ON a.id = e.caller_fn_id JOIN functions b ON b.id = e.callee_fn_id \
         ORDER BY a.name, b.name",
    );
    let edge = |a: &str, b: &str| vec![format!("Text({a:?})"), format!("Text({b:?})")];
    assert_eq!(
        edges,
        vec![
            edge("First::first", "make"),
            edge("main", "first_defined"),
            edge("second_caller", "dep_after"),
            edge("second_caller", "second_defined"),
        ]
    );
}

#[test]
fn combined_constructs_export_minimal_and_full() {
    let root = tree();
    let program = build(root.path(), 1);
    let (pag, analysis) = analyze(&program);
    exported_checks(&export_program(&program, &pag, &analysis));
    exported_checks(&export_program_full(&program, &pag, &analysis));
}

#[test]
fn combined_constructs_are_deterministic_across_runs_and_jobs() {
    let root = tree();
    let dep = root.path().join("dep");
    let dep = dep.to_str().unwrap();
    let minimal_once = analysis_rows(&cli_analyze(root.path(), &["--dep", dep, "--jobs", "1"]));
    let minimal_again = analysis_rows(&cli_analyze(root.path(), &["--dep", dep, "--jobs", "1"]));
    let minimal_parallel = analysis_rows(&cli_analyze(root.path(), &["--dep", dep, "--jobs", "4"]));
    assert_eq!(minimal_once, minimal_again);
    assert_eq!(minimal_once, minimal_parallel);
    let full_once = analysis_rows(&cli_analyze(
        root.path(),
        &["--dep", dep, "--jobs", "1", "--full-export"],
    ));
    let full_parallel = analysis_rows(&cli_analyze(
        root.path(),
        &["--dep", dep, "--jobs", "4", "--full-export"],
    ));
    assert_eq!(full_once, full_parallel);
    exported_checks(&cli_analyze(root.path(), &["--dep", dep, "--jobs", "4"]));
}
