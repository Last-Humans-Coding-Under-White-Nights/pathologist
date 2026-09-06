//! Integration tests for the trace workspace.

use std::path::PathBuf;
use trace_analysis::analyze;
use trace_db::{export_to_sqlite, open_db, ExportOptions};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

#[test]
fn direct_call_fixture() {
    let root = fixture("direct_call");
    let opts = PreprocessOptions::new().with_include(root.clone());
    let include_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/include");
    let opts = opts.with_include(include_dir);
    let program = build_program(&root, &opts).expect("build program");
    assert!(program.symbols.functions.iter().any(|f| f.name == "main"));
    assert!(program.symbols.functions.iter().any(|f| f.name == "helper"));

    let (_pag, analysis) = analyze(&program);
    assert!(!analysis.call_edges.is_empty());
    assert!(analysis
        .call_edges
        .iter()
        .any(|e| program.symbols.function(e.caller).name == "main"));
}

#[test]
fn export_sqlite_roundtrip() {
    let root = fixture("direct_call");
    let opts = PreprocessOptions::new().with_include(root.clone());
    let include_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/include");
    let opts = opts.with_include(include_dir);
    let program = build_program(&root, &opts).unwrap();
    let (pag, analysis) = analyze(&program);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("trace_test.db");
    export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &ExportOptions {
            output: out.clone(),
            trace_version: "0.1.0 (test000 2026-09-02)".into(),
            include_points_to: false,
            full_detail: false,
            model_files: Vec::new(),
        },
    )
    .unwrap();

    let conn = open_db(&out).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM call_edges", [], |r| r.get(0))
        .unwrap();
    assert!(count >= 1);
    let versions: (String, i64) = conn
        .query_row(
            "SELECT trace_version, schema_version FROM analysis_run",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(versions, ("0.1.0 (test000 2026-09-02)".to_owned(), 3));
}

#[test]
fn preproc_if0_dead_branch() {
    let path = fixture("preproc/if0.c");
    let result = trace_preproc::preprocess_file(&path, &PreprocessOptions::new()).unwrap();
    assert!(!result.output.contains("42"));
    assert!(result.output.contains("visible = 1") || result.output.contains("int visible"));
}

#[test]
fn preproc_object_macro_paren_fixture() {
    let path = fixture("preproc/object_macro_paren.c");
    let result = trace_preproc::preprocess_file(&path, &PreprocessOptions::new()).unwrap();
    let flat = result.output.replace([' ', '\n'], "");
    assert!(flat.contains("half=(.5);"), "{}", result.output);
    assert!(flat.contains("origin=(.x=0,.y=0);"), "{}", result.output);
    assert!(flat.contains("alias=(42);"), "{}", result.output);
    assert!(flat.contains("wrap=(x)x(1);"), "{}", result.output);
    assert!(flat.contains("square=((3)*(3));"), "{}", result.output);
    assert!(flat.contains("spliced=(7);"), "{}", result.output);
    assert!(flat.contains("intafter;"), "{}", result.output);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
}

#[test]
fn builtin_macro_fallbacks_fixture() {
    let root = fixture("builtin_macros");
    let opts = PreprocessOptions::new().with_include(root.clone());
    let program = build_program(&root, &opts).expect("build program");
    let parse_diags: Vec<_> = program
        .diagnostics
        .iter()
        .filter(|d| d.stage == "parse")
        .collect();
    assert!(
        parse_diags.is_empty(),
        "expected no parse diagnostics, got: {parse_diags:?}"
    );
    for name in [
        "FooTest_Bar",
        "DevRead",
        "DevInit",
        "DevExit",
        "MockInterface::LinkNext",
        "MockInterface::Pair",
        "MockInterface::Peek",
        "MockInterface::CheckMap",
        "MockInterface::GetHandler",
        "MockInterface::Start",
        "MockInterface::Attach",
        "MockInterface::Inspect",
        "MockInterface::GetCallback",
        "MockInterface::Send",
        "MockInterface::Poll",
        "MockInterface::OnLinked",
        "MockInterface::GetMemberPtr",
        "MockInterface::Wait",
        // decltype return type, indexable since #29
        "MockInterface::Deref",
    ] {
        assert!(
            program.symbols.functions.iter().any(|f| f.name == name),
            "function {name} missing from index"
        );
    }
}

#[test]
fn preproc_variadic_log_fixture() {
    let path = fixture("preproc/variadic_log.c");
    let result = trace_preproc::preprocess_file(&path, &PreprocessOptions::new()).unwrap();
    let flat = result.output.replace(['\n', ' '], "");
    assert!(
        !flat.contains(",)"),
        "empty varargs must elide the comma through the macro chain: {}",
        result.output
    );
    assert!(
        result.output.contains("42") && !result.output.contains("COUNT"),
        "varargs must stay macro-expandable: {}",
        result.output
    );
    assert!(
        result.output.contains("\"reason\""),
        "string-literal varargs must survive: {}",
        result.output
    );
    assert!(!result.output.contains("__VA_ARGS__"), "{}", result.output);
}
