//! System include bundles share IR while retaining configuration and scope.
use std::fs;
use trace_analysis::analyze;
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

#[test]
fn extensionless_system_bundles_keep_variants_fragments_and_macro_origins() {
    let fixture = crate::common::fixture("preproc/system_header_units");
    let root = trace_ir::canonicalize(&fixture.join("project"));
    let system = trace_ir::canonicalize(&fixture.join("system"));
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system.clone());
    let mut signature = None;
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        let (_, analysis) = analyze(&program);
        let mut edges: Vec<_> = analysis
            .call_edges
            .iter()
            .map(|edge| {
                (
                    program.symbols.function(edge.caller).name.clone(),
                    program.symbols.function(edge.callee).name.clone(),
                )
            })
            .collect();
        edges.sort();
        for (caller, callee) in [
            ("A", "SDK::First::Run"),
            ("B", "SDK::Second::Run"),
            ("C", "SDK::First::Run"),
        ] {
            assert!(edges.contains(&(caller.into(), callee.into())), "{edges:?}");
        }
        assert!(program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "SDK::Nested::Visit"));
        assert!(!program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "Nested::Visit"));
        assert!(!program
            .symbols
            .call_sites
            .iter()
            .any(|c| c.callee_name == "MustNotAppear"));
        let hidden = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "SDK::Hidden")
            .unwrap();
        assert!(!hidden.is_defined && hidden.locals.is_empty());
        let macro_calls: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|c| c.callee_name == "SDK::Read")
            .collect();
        assert_eq!(macro_calls.len(), 3);
        for site in macro_calls {
            assert_eq!(
                program.symbols.files[site.span.file.0 as usize].path,
                system.join("sdk")
            );
            assert_eq!(site.span.line, 12);
            assert!(site.expansion_span.is_some());
        }
        let current = (
            format!("{:?}", program.symbols.functions),
            format!("{:?}", program.symbols.call_sites),
            edges,
        );
        if let Some(expected) = &signature {
            assert_eq!(&current, expected);
        }
        signature = Some(current);
    }
}

#[test]
fn system_fragments_in_scopes_stay_inline_and_c_cpp_bundles_stay_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(
        system.join("api"),
        "#pragma once\n#ifdef __cplusplus\nvoid CppApi();\n#else\nvoid CApi(void);\n#endif\n",
    )
    .unwrap();
    fs::write(system.join("member.inc"), "void Visit();\n").unwrap();
    fs::write(
        root.join("a.c"),
        "#include <api>\nvoid RunC(void) { CApi(); }\n",
    )
    .unwrap();
    fs::write(root.join("b.cpp"), "#include <api>\nnamespace N {\n#include <member.inc>\nvoid RunN() { Visit(); }\n}\nstruct S {\n#include <member.inc>\n};\nvoid RunCpp() { CppApi(); }\n").unwrap();
    fs::write(
        root.join("c.cpp"),
        "namespace X {\n#include <api>\nvoid RunX() { CppApi(); }\n}\n",
    )
    .unwrap();
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        let (_, result) = analyze(&program);
        for (caller, callee) in [
            ("RunC", "CApi"),
            ("RunCpp", "CppApi"),
            ("N::RunN", "N::Visit"),
            ("X::RunX", "X::CppApi"),
        ] {
            assert!(
                result
                    .call_edges
                    .iter()
                    .any(|e| program.symbols.function(e.caller).name == caller
                        && program.symbols.function(e.callee).name == callee),
                "missing {caller} -> {callee}"
            );
        }
        assert!(program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "S::Visit"));
        assert!(!program.symbols.functions.iter().any(|f| f.name == "Visit"));
    }
}

#[test]
fn overlapping_bundles_import_the_variant_that_owns_a_guarded_fragment() {
    check_guarded_fragment_context(false);
}

#[test]
fn project_headers_import_the_exact_variant_of_their_system_include() {
    check_guarded_fragment_context(true);
}

fn check_guarded_fragment_context(project_header: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(system.join("common.inc"), "#ifndef COMMON_INC\n#define COMMON_INC\n#if SELECT == 1\nstruct First { void Run(); }; typedef First Value;\n#else\nstruct Second { void Run(); }; typedef Second Value;\n#endif\n#endif\n").unwrap();
    fs::write(
        system.join("first"),
        "#pragma once\n#include \"common.inc\"\n",
    )
    .unwrap();
    let (second, include) = if project_header {
        (root.join("second.h"), "#include \"second.h\"")
    } else {
        (system.join("second"), "#include <second>")
    };
    fs::write(
        second,
        "#pragma once\n#include \"common.inc\"\nValue *Make();\n",
    )
    .unwrap();
    for (file, select, caller) in [("a.cpp", 1, "A"), ("b.cpp", 2, "B"), ("c.cpp", 1, "C")] {
        fs::write(root.join(file), format!("#define SELECT {select}\n#include <first>\n#undef SELECT\n#define SELECT 0\n{include}\nvoid {caller}() {{ Make()->Run(); }}\n")).unwrap();
    }
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for cap in [0, 1, 8] {
        opts.max_expansion_variants = cap;
        for jobs in [1, 3] {
            let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
            for (caller, receiver) in [("A", "First"), ("B", "Second"), ("C", "First")] {
                assert!(
                    program.symbols.call_sites.iter().any(|site| program
                        .symbols
                        .function(site.caller)
                        .name
                        == caller
                        && site.receiver_class.as_deref() == Some(receiver)),
                    "{caller} must keep {receiver}, got {:?}",
                    program.symbols.call_sites
                );
            }
        }
    }
}

#[test]
fn system_headers_using_project_aliases_keep_the_consumers_declaration_context() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(system.join("dependent"), "Value *Make();\n").unwrap();
    for (file, receiver, caller) in [("a.cpp", "First", "A"), ("b.cpp", "Second", "B")] {
        fs::write(root.join(file), format!("struct {receiver} {{ void Run(); }}; typedef {receiver} Value;\n#include <dependent>\nvoid {caller}() {{ Make()->Run(); }}\n")).unwrap();
    }
    fs::write(
        root.join("types.h"),
        "struct Third { void Run(); }; typedef Third Value;\n",
    )
    .unwrap();
    fs::write(
        root.join("c.cpp"),
        "#include \"types.h\"\n#include <dependent>\nvoid C() { Make()->Run(); }\n",
    )
    .unwrap();
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        for (caller, receiver) in [("A", "First"), ("B", "Second"), ("C", "Third")] {
            assert!(
                program.symbols.call_sites.iter().any(|site| program
                    .symbols
                    .function(site.caller)
                    .name
                    == caller
                    && site.receiver_class.as_deref() == Some(receiver)),
                "{caller} must keep {receiver}"
            );
        }
    }
}

#[test]
fn project_macro_defined_methods_survive_shared_system_preambles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(
        system.join("text"),
        "#pragma once\nnamespace std { struct Text {}; }\n",
    )
    .unwrap();
    fs::write(
        root.join("macros.h"),
        "#pragma once\n#define DESCRIPTOR() static std::Text GetDescriptor() { return {}; }\n",
    )
    .unwrap();
    fs::write(
        root.join("api.h"),
        "#pragma once\n#include <text>\n#include \"macros.h\"\nstruct Api { DESCRIPTOR() };\n",
    )
    .unwrap();
    for name in ["a", "b", "c"] {
        fs::write(
            root.join(format!("{name}.cpp")),
            format!("#include \"api.h\"\nvoid {name}() {{ Api::GetDescriptor(); }}\n"),
        )
        .unwrap();
    }
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        let method = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "Api::GetDescriptor" && f.is_defined)
            .expect("the project macro must still define its method");
        assert!(!program.symbols.file_is_dep(method.span.file));
        let (_, analysis) = analyze(&program);
        for caller in ["a", "b", "c"] {
            assert!(analysis.call_edges.iter().any(|edge| {
                program.symbols.function(edge.caller).name == caller && edge.callee == method.id
            }));
        }
    }
}

#[test]
fn later_system_bundles_keep_the_prior_declaration_context_without_an_include() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(system.join("common.inc"), "#ifndef COMMON_INC\n#define COMMON_INC\n#if SELECT == 1\nstruct First { void Run(); }; typedef First Value;\n#else\nstruct Second { void Run(); }; typedef Second Value;\n#endif\n#endif\n").unwrap();
    fs::write(
        system.join("first"),
        "#pragma once\n#include \"common.inc\"\n",
    )
    .unwrap();
    fs::write(system.join("second"), "#pragma once\nValue *Make();\n").unwrap();
    for (file, select, caller) in [("a.cpp", 1, "A"), ("b.cpp", 2, "B"), ("c.cpp", 1, "C")] {
        fs::write(root.join(file), format!("#define SELECT {select}\n#include <first>\n#undef SELECT\n#define SELECT 0\n#include <second>\nvoid {caller}() {{ Make()->Run(); }}\n")).unwrap();
    }
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for cap in [0, 1, 8] {
        opts.max_expansion_variants = cap;
        for jobs in [1, 3] {
            let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
            for (caller, receiver) in [("A", "First"), ("B", "Second"), ("C", "First")] {
                assert!(
                    program.symbols.call_sites.iter().any(|site| program
                        .symbols
                        .function(site.caller)
                        .name
                        == caller
                        && site.receiver_class.as_deref() == Some(receiver)),
                    "{caller} must keep {receiver}, got {:?}",
                    program.symbols.call_sites
                );
            }
        }
    }
}

#[test]
fn opposite_system_include_orders_keep_each_variants_type_prerequisites() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(system.join("one"), "#pragma once\n#ifdef TWO\nValue *Make1();\n#else\nstruct Left { void Run(); }; typedef Left Value;\n#endif\n#define ONE\n").unwrap();
    fs::write(system.join("two"), "#pragma once\n#ifdef ONE\nValue *Make2();\n#else\nstruct Right { void Run(); }; typedef Right Value;\n#endif\n#define TWO\n").unwrap();
    fs::write(
        root.join("a.cpp"),
        "#include <one>\n#include <two>\nvoid A() { Make2()->Run(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("b.cpp"),
        "#include <two>\n#include <one>\nvoid B() { Make1()->Run(); }\n",
    )
    .unwrap();
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        for (caller, receiver) in [("A", "Left"), ("B", "Right")] {
            assert!(
                program.symbols.call_sites.iter().any(|site| {
                    program.symbols.function(site.caller).name == caller
                        && site.receiver_class.as_deref() == Some(receiver)
                }),
                "{caller} must keep {receiver}, got {:?}",
                program.symbols.call_sites
            );
        }
    }
}

#[test]
fn system_declarations_continued_by_the_includer_stay_inline() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(system.join("record"), "struct Record { void Run(); }\n").unwrap();
    fs::write(
        system.join("values"),
        "namespace SDK { struct Value { void Run(); }; }\n",
    )
    .unwrap();
    fs::write(system.join("init"), "SDK::Value first{}\n").unwrap();
    fs::write(
        root.join("a.cpp"),
        "#include <record>\nrecord;\nvoid A() { record.Run(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("b.cpp"),
        "#include <values>\n#include <init>\n, second;\nvoid B() { second.Run(); }\n",
    )
    .unwrap();
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        let (_, analysis) = analyze(&program);
        for (caller, callee) in [("A", "Record::Run"), ("B", "SDK::Value::Run")] {
            assert!(
                analysis.call_edges.iter().any(|edge| {
                    program.symbols.function(edge.caller).name == caller
                        && program.symbols.function(edge.callee).name == callee
                }),
                "missing {caller} -> {callee}"
            );
        }
    }
}

#[test]
fn declaration_modules_keep_configuration_and_worker_count_independent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(
        system.join("first"),
        "#pragma once\nstruct KIND { int value; };\nvoid Take(KIND *value);\nextern int *slot;\n",
    )
    .unwrap();
    fs::write(system.join("second"),
        "#pragma once\nextern int *slot;\nstatic void Probe(int *value);\nvoid Consume(int *value);\n").unwrap();
    for i in 0..20 {
        fs::write(root.join(format!("u{i:02}.cpp")), format!(
            "#define KIND Value{i}\n#include <first>\n#include <second>\nvoid F{i}(KIND *value) {{ Take(value); Probe(slot); Consume(slot); }}\n"
        )).unwrap();
    }
    let mut opts = PreprocessOptions::new().with_max_expansion_variants(32);
    opts.system_include_paths.push(system);
    let mut previous = None;
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        let (_, analysis) = analyze(&program);
        assert_eq!(program.symbols.call_sites.len(), 60);
        assert_eq!(analysis.call_edges.len(), 60);
        assert!(program
            .symbols
            .functions
            .iter()
            .filter(|f| program.is_dep_file(f.span.file))
            .all(|f| !f.is_defined && f.locals.is_empty()));
        let current = (
            format!("{:?}", program.symbols.functions),
            format!("{:?}", program.symbols.variables),
            format!("{:?}", program.symbols.call_sites),
            format!("{:?}", program.flow),
            format!("{:?}", analysis.call_edges),
        );
        if let Some(expected) = &previous {
            assert_eq!(&current, expected);
        }
        previous = Some(current);
    }
}

#[test]
fn shared_dependency_global_preserves_callback_flow_across_translation_units() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(
        system.join("api"),
        "#pragma once\nextern void (*callback)();\n",
    )
    .unwrap();
    fs::write(
        root.join("a.cpp"),
        "#include <api>\nvoid Target() {}\nvoid Set() { callback = Target; }\n",
    )
    .unwrap();
    fs::write(
        root.join("b.cpp"),
        "#include <api>\nvoid Run() { callback(); }\n",
    )
    .unwrap();
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        let (_, analysis) = analyze(&program);
        assert_eq!(
            program
                .symbols
                .variables
                .iter()
                .filter(|v| v.name == "callback")
                .count(),
            1
        );
        assert!(analysis
            .call_edges
            .iter()
            .any(|edge| program.symbols.function(edge.caller).name == "Run"
                && program.symbols.function(edge.callee).name == "Target"));
    }
}

#[test]
fn standalone_header_keeps_cached_macros_after_expansion_retirement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&system).unwrap();
    fs::write(system.join("api"), "#pragma once\nusing Number = int;\nvoid External(Number *);\n#define CALL_EXTERNAL(value) External(value)\n").unwrap();
    fs::write(root.join("shared.h"), "#pragma once\n#include <api>\n").unwrap();
    fs::write(
        root.join("main.cpp"),
        "#include \"shared.h\"\nvoid Main(Number *value) { CALL_EXTERNAL(value); }\n",
    )
    .unwrap();
    fs::write(
        root.join("orphan.h"),
        "#include \"shared.h\"\ninline void Orphan(Number *value) { CALL_EXTERNAL(value); }\n",
    )
    .unwrap();
    let mut opts = PreprocessOptions::new();
    opts.system_include_paths.push(system);
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        let (_, analysis) = analyze(&program);
        for name in ["Main", "Orphan"] {
            assert!(analysis.call_edges.iter().any(|edge| program
                .symbols
                .function(edge.caller)
                .name
                == name
                && program.symbols.function(edge.callee).name == "External"));
        }
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|site| program.symbols.function(site.caller).name == "Orphan")
            .unwrap();
        let expansion = site.expansion_span.unwrap();
        assert_eq!(
            program.symbols.files[expansion.file.0 as usize].path,
            root.join("orphan.h")
        );
        assert_eq!(expansion.line, 2);
    }
}
