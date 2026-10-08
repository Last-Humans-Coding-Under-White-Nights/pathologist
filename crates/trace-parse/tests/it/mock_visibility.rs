use std::path::Path;
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn c_production_body_survives_a_same_named_test_definition() {
    let tree = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("api.h", "int hook(void);"),
        (
            "src/hook.c",
            "#include \"api.h\"\nint hook(void) { return 1; }",
        ),
        (
            "test/hook.c",
            "#include \"api.h\"\nint hook(void) { return 2; }",
        ),
        (
            "src/use.c",
            "#include \"api.h\"\nint use(void) { return hook(); }",
        ),
    ] {
        let file = tree.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|s| program.symbols.function(s.caller).name == "use")
        .unwrap();
    let targets = program.callees_of(site);
    assert_eq!(targets.len(), 1);
    assert!(
        program.symbols.files[program.symbols.function(targets[0]).file.0 as usize]
            .path
            .ends_with("src/hook.c")
    );
}

#[test]
fn production_fallback_does_not_select_test_bodies_or_missing_external_mock_headers() {
    let tree = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("include/worker.h", "struct Worker { int Run(); }; struct OnlyMock { static int Missing(); };"),
        ("src/worker.cpp", "#include \"worker.h\"\nint Worker::Run() { return 1; }"),
        ("test/mock.cpp", "#include \"worker.h\"\nint Worker::Run() { return 2; } int OnlyMock::Missing() { return 9; }"),
        ("test/mock/external.h", "struct External { static int Value() { return 3; } };"),
        ("test/helper.h", "inline int shared_utility() { return 4; }"),
        ("include/wrapper.h", "#include \"worker.h\"\ninline int wrap(Worker *w) { return w->Run(); }"),
        ("src/wrapper.cpp", "#include \"wrapper.h\"\nint wrapped(Worker*w) { return wrap(w); }"),
        ("test/wrapper.cpp", "#include \"wrapper.h\"\nint Worker::Run() { return 5; } int test_wrapped(Worker*w) { return wrap(w); }"),
        ("src/use.cpp", "#include \"worker.h\"\n#include \"external.h\"\n#include \"../test/helper.h\"\nint use(Worker *w) { return w->Run() + External::Value() + shared_utility() + OnlyMock::Missing(); }"),
        ("test/use.cpp", "#include \"mock/external.h\"\nint test_use() { return External::Value(); }"),
    ] {
        let path = tree.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
    for site in program
        .symbols
        .call_sites
        .iter()
        .filter(|s| program.symbols.function(s.caller).name == "wrap")
    {
        let bodies = program.callees_of(site);
        assert!(!bodies.is_empty());
        assert!(
            bodies.iter().all(|&id| program.symbols.files
                [program.symbols.function(id).span.file.0 as usize]
                .path
                .ends_with("src/worker.cpp")),
            "a production header must not acquire mocks from incidental test TU contexts"
        );
    }
    for (caller, callee, expected) in [
        ("use", "Worker::Run", vec!["src/worker.cpp"]),
        ("use", "External::Value", vec![]),
        ("use", "OnlyMock::Missing", vec![]),
        ("use", "shared_utility", vec!["test/helper.h"]),
        ("test_use", "External::Value", vec!["test/mock/external.h"]),
    ] {
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|s| program.symbols.function(s.caller).name == caller && s.callee_name == callee)
            .unwrap();
        let bodies: Vec<_> = program
            .callees_of(site)
            .into_iter()
            .filter_map(|id| {
                let f = program.symbols.function(id);
                f.is_defined.then(|| {
                    program.symbols.files[f.span.file.0 as usize]
                        .path
                        .strip_prefix(tree.path().canonicalize().unwrap())
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
            })
            .collect();
        assert_eq!(bodies, expected, "{caller} -> {callee}");
        if expected.is_empty() {
            let context = program.symbols.function(site.caller).file;
            let first = program
                .symbols
                .resolve_function_in_scope(callee, Some(context));
            assert!(
                first.is_none_or(|id| !program.symbols.function(id).is_defined),
                "single-result lookup must share eligibility"
            );
        }
    }
}

#[test]
fn declaring_header_separates_production_and_two_mock_families() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mock_visibility");
    let program = build_program(&root, &PreprocessOptions::default()).unwrap();
    for (caller, name, suffix) in [
        ("ns::use", "ns::Worker::Run", "src/worker.cpp"),
        ("ns::test_use", "ns::Worker::Run", "test/mock/worker.h"),
        ("ns::second_use", "ns::Worker::Run", "test/second.cpp"),
        ("ns::submit", "ns::Worker::SubmitTask", "src/worker.cpp"),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| program.symbols.function(s.caller).name == caller && s.callee_name == name)
            .collect();
        assert_eq!(sites.len(), 1, "{caller}: one syntactic/ranked site");
        let callees = program.callees_of(sites[0]);
        let files: Vec<_> = callees
            .iter()
            .map(|&id| {
                let f = program.symbols.function(id);
                program.symbols.files[f.file.0 as usize].path.clone()
            })
            .collect();
        assert_eq!(callees.len(), 1, "{caller}: {files:?}");
        assert!(files[0].ends_with(suffix), "{caller}: {files:?}");
        if caller == "ns::submit" {
            assert_eq!(program.symbols.function(callees[0]).explicit_arity, Some(1));
        }
    }
}

#[test]
fn same_header_competing_definitions_and_unknown_provenance_remain_candidates() {
    for declared in [true, false] {
        let tree = tempfile::tempdir().unwrap();
        let header = if declared {
            "struct Worker { int Run(); };"
        } else {
            "int Run();"
        };
        std::fs::write(tree.path().join("worker.h"), header).unwrap();
        let name = if declared { "Worker::Run" } else { "Run" };
        for (file, n) in [("a.cpp", 1), ("b.cpp", 2)] {
            std::fs::write(
                tree.path().join(file),
                format!("#include \"worker.h\"\nint {name}() {{ return {n}; }}"),
            )
            .unwrap();
        }
        let caller = if declared {
            "int use(Worker *w) { return w->Run(); }"
        } else {
            "int use() { return Run(); }"
        };
        std::fs::write(
            tree.path().join("use.cpp"),
            format!("#include \"worker.h\"\n{caller}"),
        )
        .unwrap();
        let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|s| program.symbols.function(s.caller).name == "use")
            .unwrap();
        assert_eq!(
            program.callees_of(site).len(),
            2,
            "both compatible definitions survive"
        );
        assert_eq!(
            program
                .symbols
                .return_flow_candidates(site.caller, name)
                .len(),
            2
        );
    }
}

#[test]
fn declaration_family_is_independent_of_file_discovery_order() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mock_visibility");
    for production_first in [true, false] {
        let tree = tempfile::tempdir().unwrap();
        for (folder, source) in [
            (if production_first { "a" } else { "z" }, "src"),
            (if production_first { "z" } else { "a" }, "test/mock"),
        ] {
            std::fs::create_dir(tree.path().join(folder)).unwrap();
            for entry in std::fs::read_dir(fixture.join(source)).unwrap() {
                let entry = entry.unwrap();
                std::fs::copy(
                    entry.path(),
                    tree.path().join(folder).join(entry.file_name()),
                )
                .unwrap();
            }
        }
        let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|s| program.symbols.function(s.caller).name == "ns::use")
            .unwrap();
        let callees = program.callees_of(site);
        assert_eq!(callees.len(), 1);
        let f = program.symbols.function(callees[0]);
        assert!(program.symbols.files[f.file.0 as usize]
            .path
            .ends_with("worker.cpp"));
        assert_eq!(
            program
                .symbols
                .return_flow_candidates(site.caller, "ns::Worker::Run"),
            callees
        );
    }
}

#[test]
fn visible_family_keeps_defaults_variadics_and_ambiguous_overloads() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(tree.path().join("real.h"), "struct S { int defaults(int x, int y=0) { return x; } int var(int x, ...) { return x; } int tie(int x) { return x; } int tie(double x) { return 0; } };\n").unwrap();
    std::fs::write(tree.path().join("mock.cpp"), "struct S { int defaults(int x,int y=0) { return y; } int var(int x,...) { return x; } int tie(int x) { return x; } };\n").unwrap();
    std::fs::write(tree.path().join("caller.cpp"), "#include \"real.h\"\nint unknown(); int use(S *s) { s->defaults(1); s->var(1,2,3); return s->tie(unknown()); }\n").unwrap();
    let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
    for name in ["S::defaults", "S::var", "S::tie"] {
        let callees: std::collections::BTreeSet<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| program.symbols.function(s.caller).name == "use" && s.callee_name == name)
            .flat_map(|s| program.callees_of(s))
            .collect();
        assert!(!callees.is_empty(), "{name}");
        for id in callees {
            let f = program.symbols.function(id);
            assert!(
                program.symbols.files[f.file.0 as usize]
                    .path
                    .ends_with("real.h"),
                "{name}"
            );
        }
    }
}

/// The files, relative to `tree`, defining the bodies the first call `caller`
/// makes to `callee` binds.
fn bound_body(program: &trace_ir::Program, tree: &Path, caller: &str, callee: &str) -> Vec<String> {
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|s| program.symbols.function(s.caller).name == caller && s.callee_name == callee)
        .unwrap_or_else(|| panic!("{caller} -> {callee}"));
    program
        .callees_of(site)
        .into_iter()
        .map(|id| program.symbols.function(id))
        .filter(|f| f.is_defined)
        .map(|f| {
            program.symbols.files[f.span.file.0 as usize]
                .path
                .strip_prefix(tree.canonicalize().unwrap())
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn write_tree(tree: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = tree.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

#[test]
fn production_include_takes_a_production_header_after_an_earlier_mock_one() {
    // `mock/` sorts ahead of `zsrc/` in the inferred search list; the
    // production includer still reads the production header.
    let tree = tempfile::tempdir().unwrap();
    write_tree(
        tree.path(),
        &[
            ("mock/pick.h", "static inline int pick(void) { return 2; }"),
            ("zsrc/pick.h", "static inline int pick(void) { return 1; }"),
            (
                "app/use.c",
                "#include \"pick.h\"\nint use(void) { return pick(); }",
            ),
        ],
    );
    let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
    assert_eq!(
        bound_body(&program, tree.path(), "use", "pick"),
        ["zsrc/pick.h"]
    );
}

#[test]
fn basename_fallback_prefers_the_single_production_match() {
    // No search directory holds `gone/foo.h`, so the basename index answers:
    // two files share the name, but only one is production.
    let tree = tempfile::tempdir().unwrap();
    write_tree(
        tree.path(),
        &[
            ("src/foo.h", "static inline int foo(void) { return 1; }"),
            (
                "test/mock/foo.h",
                "static inline int foo(void) { return 2; }",
            ),
            (
                "app/use.c",
                "#include \"gone/foo.h\"\nint use(void) { return foo(); }",
            ),
        ],
    );
    let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
    assert_eq!(
        bound_body(&program, tree.path(), "use", "foo"),
        ["src/foo.h"]
    );
}

#[test]
fn single_result_lookup_prefers_a_production_body_over_a_prototype() {
    // `hook(double)` stays a bodiless prototype registered ahead of the
    // production `hook(int)` body. The test body is dropped for production
    // code, and the lookup must answer with the production body, not the
    // prototype that precedes it.
    let tree = tempfile::tempdir().unwrap();
    write_tree(
        tree.path(),
        &[
            ("api.h", "int hook(double); int hook(int);"),
            (
                "src/hook.cpp",
                "#include \"api.h\"\nint hook(int) { return 1; }",
            ),
            (
                "test/zhook.cpp",
                "#include \"api.h\"\nint hook(int) { return 2; }",
            ),
            (
                "src/use.cpp",
                "#include \"api.h\"\nint use() { return hook(1); }",
            ),
        ],
    );
    let program = build_program(tree.path(), &PreprocessOptions::default()).unwrap();
    let caller = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "use")
        .unwrap();
    let id = program
        .symbols
        .resolve_function_in_scope("hook", Some(caller.file))
        .unwrap();
    let f = program.symbols.function(id);
    assert!(f.is_defined, "a prototype answered the lookup");
    assert!(program.symbols.files[f.span.file.0 as usize]
        .path
        .ends_with("src/hook.cpp"));
}
