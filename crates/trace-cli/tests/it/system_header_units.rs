//! System include bundles share IR while retaining configuration and scope.
use std::fs;
use trace_analysis::analyze;
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

#[test]
fn shared_system_preambles_preserve_reference_address_flow() {
    for nested in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let system = dir.path().join("system");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&system).unwrap();
        fs::write(system.join("api"), "#pragma once\nextern int &ref;\nstruct Holder { static int &member; };\nextern int *plain;\n").unwrap();
        let include = if nested {
            fs::write(root.join("shared.h"), "#pragma once\n#include <api>\n").unwrap();
            "\"shared.h\""
        } else {
            "<api>"
        };
        for caller in ["First", "Second"] {
            fs::write(root.join(format!("{caller}.cpp")), format!("#include {include}\nvoid {caller}() {{ int *p = &ref; int *s = &Holder::member; int **ordinary = &plain; }}\n")).unwrap();
        }
        let mut baseline = None;
        for variants in [0, 8] {
            let opts = PreprocessOptions::new()
                .with_system_include(&system)
                .with_max_expansion_variants(variants);
            for jobs in [1, 3] {
                let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
                let (pag, analysis) = analyze(&program);
                let db = crate::common::export_program_full(&program, &pag, &analysis);
                let conn = trace_db::open_db(db.path()).unwrap();
                let mut statement = conn.prepare("SELECT DISTINCT f.name, sv.name, dv.name, e.kind FROM flow_edges e JOIN flow_nodes s ON s.id = e.src_node JOIN variables sv ON sv.id = s.var_id JOIN flow_nodes d ON d.id = e.dst_node JOIN variables dv ON dv.id = d.var_id JOIN functions f ON f.id = dv.fn_id WHERE f.name IN ('First', 'Second') AND dv.name IN ('p', 's', 'ordinary') AND e.kind IN ('copy', 'addr_of') ORDER BY 1, 2, 3, 4").unwrap();
                let rows: Vec<(String, String, String, String)> = statement
                    .query_map([], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                assert_eq!(rows.len(), 6, "{rows:?}");
                for (_, _, destination, kind) in &rows {
                    assert_eq!(
                        kind,
                        if destination == "ordinary" {
                            "addr_of"
                        } else {
                            "copy"
                        },
                        "nested={nested}, variants={variants}, jobs={jobs}: {rows:?}"
                    );
                }
                if let Some(expected) = &baseline {
                    assert_eq!(
                        &rows, expected,
                        "nested={nested}, variants={variants}, jobs={jobs}"
                    );
                }
                baseline = Some(rows);
            }
        }
    }
}

#[test]
fn unsupported_pragma_operands_keep_following_declarations() {
    let fixture = crate::common::fixture("preproc/pragma_recovery");
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&fixture, &PreprocessOptions::new(), jobs).unwrap();
        let (_, analysis) = analyze(&program);
        for name in ["AfterAlias", "AfterUtf", "AfterUndefined", "AfterMultiple"] {
            assert!(
                analysis
                    .call_edges
                    .iter()
                    .any(|edge| program.symbols.function(edge.caller).name == name
                        && program.symbols.function(edge.callee).name == "Before"),
                "missing {name} -> Before: {:?}",
                program.diagnostics
            );
        }
        assert!(!program
            .diagnostics
            .iter()
            .any(|d| d.message.contains("preprocess stopped")));
    }
}

#[test]
fn system_namespace_imports_preserve_lookup_in_the_consumer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&system).unwrap();
    for imports in [
        "using namespace SDK;",
        "using SDK::Api; using SDK::Make;",
        "using namespace SDK; // nested consumer",
    ] {
        fs::write(system.join("api"), format!("#pragma once\nnamespace SDK {{ void Api(); struct Obj {{ void Go(); }}; Obj *Make(); }}\n{imports}\n")).unwrap();
        let body = "void Run() { Api(); Make()->Go(); }";
        let body = if imports.ends_with("// nested consumer") {
            format!("namespace Client {{ {body} }}")
        } else {
            body.into()
        };
        fs::write(root.join("main.cpp"), format!("#include <api>\n{body}\n")).unwrap();
        // Exercise effects inside a cached project child as well as in the
        // system bundle itself; neither may hide the consumer's imports.
        for nested in [false, true] {
            if nested {
                fs::rename(system.join("api"), root.join("imports.h")).unwrap();
                fs::write(system.join("api"), "#pragma once\n#include \"imports.h\"\n").unwrap();
            }
            for variants in [0, 8] {
                let opts = PreprocessOptions::new()
                    .with_include(root.clone())
                    .with_system_include(&system)
                    .with_max_expansion_variants(variants);
                for jobs in [1, 3] {
                    let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
                    let (_, analysis) = analyze(&program);
                    for callee in ["SDK::Api", "SDK::Make", "SDK::Obj::Go"] {
                        assert!(analysis.call_edges.iter().any(|edge|
                            program.symbols.function(edge.caller).name.ends_with("Run")
                            && program.symbols.function(edge.callee).name == callee),
                            "imports={imports}, nested={nested}, variants={variants}, jobs={jobs}: missing {callee}");
                    }
                }
            }
        }
        // Keep the next iteration's project tree independent.
        fs::write(root.join("imports.h"), "#pragma once\n").unwrap();
    }
}

#[test]
fn scoped_system_namespace_imports_keep_their_original_scope() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&system).unwrap();
    fs::write(system.join("api"), "#pragma once\nnamespace SDK { struct Obj { void Go(); }; Obj *Make(); }\nnamespace Client { using SDK::Make; inline auto value = Make(); }\n").unwrap();
    fs::write(
        root.join("main.cpp"),
        "#include <api>\nvoid Run() { Client::value->Go(); }\nvoid Outside() { Make(); }\n",
    )
    .unwrap();
    for variants in [0, 8] {
        let opts = PreprocessOptions::new()
            .with_system_include(&system)
            .with_max_expansion_variants(variants);
        for jobs in [1, 3] {
            let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
            let (_, analysis) = analyze(&program);
            assert!(analysis.call_edges.iter().any(|edge| program
                .symbols
                .function(edge.caller)
                .name
                == "Run"
                && program.symbols.function(edge.callee).name == "SDK::Obj::Go"));
            assert!(!analysis.call_edges.iter().any(|edge| program
                .symbols
                .function(edge.caller)
                .name
                == "Outside"
                && program.symbols.function(edge.callee).name == "SDK::Make"));
        }
    }
}

#[test]
fn system_bundle_initializers_import_prerequisite_factory_declarations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&system).unwrap();
    fs::write(
        system.join("a"),
        "#pragma once\nnamespace SDK { struct Obj { void Run(); }; Obj *Make(); }\n",
    )
    .unwrap();
    fs::write(
        system.join("b"),
        "#pragma once\ninline auto value = SDK::Make();\n",
    )
    .unwrap();
    fs::write(
        root.join("main.cpp"),
        "#include <a>\n#include <b>\nvoid Main() { value->Run(); }\n",
    )
    .unwrap();
    for variants in [0, 8] {
        let opts = PreprocessOptions::new()
            .with_system_include(&system)
            .with_max_expansion_variants(variants);
        for jobs in [1, 3] {
            let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
            assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
            let values: Vec<_> = program
                .symbols
                .variables
                .iter()
                .filter(|v| v.name == "value")
                .collect();
            assert!(!values.is_empty());
            for value in values {
                let desc = &program.types.get(value.type_id).desc;
                assert!(
                    matches!(desc.as_ref(), trace_ir::TypeDesc::Ptr(inner)
                    if matches!(inner.as_ref(), trace_ir::TypeDesc::Struct { name, .. } if name == "SDK::Obj")),
                    "{desc:?}"
                );
            }
            let (_, analysis) = analyze(&program);
            assert!(
                analysis
                    .call_edges
                    .iter()
                    .any(|edge| program.symbols.function(edge.caller).name == "Main"
                        && program.symbols.function(edge.callee).name == "SDK::Obj::Run"),
                "variants={variants}, jobs={jobs}"
            );
        }
    }
}

#[test]
fn project_header_initializers_import_cached_system_prototypes() {
    for nested in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let system = dir.path().join("system");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&system).unwrap();
        fs::write(
            system.join("api"),
            "#pragma once\nnamespace SDK { struct Obj { void Run(); }; Obj *Make(); }\n",
        )
        .unwrap();
        let include = if nested {
            fs::write(root.join("wrapper.h"), "#pragma once\n#include <api>\n").unwrap();
            "\"wrapper.h\""
        } else {
            "<api>"
        };
        fs::write(
            root.join("shared.h"),
            format!("#pragma once\n#include {include}\ninline auto value = SDK::Make();\n"),
        )
        .unwrap();
        for caller in ["First", "Second"] {
            fs::write(
                root.join(format!("{caller}.cpp")),
                format!("#include \"shared.h\"\nvoid {caller}() {{ value->Run(); }}\n"),
            )
            .unwrap();
        }
        for variants in [0, 8] {
            let opts = PreprocessOptions::new()
                .with_system_include(&system)
                .with_max_expansion_variants(variants);
            for jobs in [1, 3] {
                let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
                let values: Vec<_> = program
                    .symbols
                    .variables
                    .iter()
                    .filter(|v| v.name == "value")
                    .collect();
                assert!(!values.is_empty());
                for value in values {
                    let desc = &program.types.get(value.type_id).desc;
                    assert!(
                        matches!(desc.as_ref(), trace_ir::TypeDesc::Ptr(inner)
                    if matches!(inner.as_ref(), trace_ir::TypeDesc::Struct { name, .. } if name == "SDK::Obj")),
                        "variants={variants}, jobs={jobs}: {desc:?}"
                    );
                }
                let (_, analysis) = analyze(&program);
                for caller in ["First", "Second"] {
                    assert!(
                        analysis.call_edges.iter().any(|edge| program
                            .symbols
                            .function(edge.caller)
                            .name
                            == caller
                            && program.symbols.function(edge.callee).name == "SDK::Obj::Run"),
                        "variants={variants}, jobs={jobs}: missing {caller} -> SDK::Obj::Run"
                    );
                }
            }
        }
    }
}

#[test]
fn system_header_weak_pragmas_annotate_consuming_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&system).unwrap();
    fs::write(root.join("main.cpp"), "#include <api>\nvoid hook() {}\n").unwrap();
    for pragma in ["#pragma weak hook", "_Pragma(\"weak hook\")"] {
        fs::write(system.join("api"), format!("#pragma once\n{pragma}\n")).unwrap();
        for variants in [0, 8] {
            let opts = PreprocessOptions::new()
                .with_system_include(&system)
                .with_max_expansion_variants(variants);
            for jobs in [1, 3] {
                let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
                let hook = crate::common::only_function(&program, "hook");
                assert!(
                    program.symbols.function(hook).is_weak,
                    "{pragma}, variants={variants}, jobs={jobs}"
                );
            }
        }
    }
}

#[test]
fn orphan_headers_keep_system_variants_not_consumed_by_translation_units() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&system).unwrap();
    fs::write(system.join("api"), "#pragma once\n#if SELECT\nstruct Active { void Run(); }; using Value = Active;\n#else\nstruct Default { void Run(); }; using Value = Default;\n#endif\nValue *Make();\n").unwrap();
    fs::write(root.join("shared.h"), "#pragma once\n#include <api>\n").unwrap();
    fs::write(
        root.join("main.cpp"),
        "#define SELECT 1\n#include \"shared.h\"\nvoid Main() { Make()->Run(); }\n",
    )
    .unwrap();
    let mut previous = None;
    for include in ["<api>", "\"shared.h\""] {
        fs::write(
            root.join("orphan.h"),
            format!("#include {include}\ninline void Orphan() {{ Make()->Run(); }}\n"),
        )
        .unwrap();
        for variants in [0, 8] {
            let opts = PreprocessOptions::new()
                .with_system_include(&system)
                .with_max_expansion_variants(variants);
            for jobs in [1, 3] {
                let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
                let (_, analysis) = analyze(&program);
                let mut targets: Vec<_> = analysis
                    .call_edges
                    .iter()
                    .filter_map(|edge| {
                        let caller = &program.symbols.function(edge.caller).name;
                        let callee = &program.symbols.function(edge.callee).name;
                        matches!(caller.as_str(), "Main" | "Orphan")
                            .then(|| (caller.clone(), callee.clone()))
                    })
                    .collect();
                targets.sort();
                for (caller, callee) in [("Main", "Active::Run"), ("Orphan", "Default::Run")] {
                    assert!(
                    targets.contains(&(caller.into(), callee.into())),
                    "variants={variants}, jobs={jobs}: missing {caller} -> {callee}: {targets:?}"
                );
                }
                if let Some(expected) = &previous {
                    assert_eq!(&targets, expected);
                }
                previous = Some(targets);
            }
        }
    }
}

#[test]
fn shared_project_headers_keep_c_and_cpp_declarations_available() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("shared.h"),
        "#pragma once\n#ifdef __cplusplus\nnamespace CppOnly { struct Data { int field; }; }\nusing Value = CppOnly::Data;\nvoid CppApi(Value *);\n#else\ntypedef int Value;\nvoid CApi(Value *);\n#endif\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("one.c"),
        "#include \"shared.h\"\nvoid RunC(Value *value) { CApi(value); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("two.cpp"),
        "#include \"shared.h\"\nvoid RunCpp(Value *value) { CppApi(value); }\n",
    )
    .unwrap();
    for jobs in [1, 3] {
        let program = build_program_with_jobs(dir.path(), &PreprocessOptions::new(), jobs).unwrap();
        let (_, analysis) = analyze(&program);
        for (caller, callee) in [("RunC", "CApi"), ("RunCpp", "CppApi")] {
            assert!(
                analysis
                    .call_edges
                    .iter()
                    .any(|edge| program.symbols.function(edge.caller).name == caller
                        && program.symbols.function(edge.callee).name == callee),
                "missing {caller} -> {callee}"
            );
        }
        let parameter_type = |name| {
            let function = crate::common::only_function(&program, name);
            let parameter = program.symbols.function(function).params[0];
            program
                .types
                .get(program.symbols.variable(parameter).type_id)
                .desc
                .as_ref()
        };
        assert_eq!(
            parameter_type("RunC"),
            &trace_ir::TypeDesc::Ptr(Box::new(trace_ir::TypeDesc::Int))
        );
        assert!(
            matches!(parameter_type("RunCpp"), trace_ir::TypeDesc::Ptr(inner)
            if matches!(inner.as_ref(), trace_ir::TypeDesc::Struct { name, .. } if name == "CppOnly::Data"))
        );
    }
}

#[test]
fn pragma_terminated_bundles_lower_with_their_declaration_prerequisites() {
    let fixture = crate::common::fixture("preproc/pragma_operators");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir(&root).unwrap();
    fs::copy(fixture.join("main.cpp"), root.join("main.cpp")).unwrap();
    let opts = PreprocessOptions::new().with_system_include(&fixture);
    let mut snapshot = None;
    for jobs in [1, 3] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
        let (_, analysis) = analyze(&program);
        assert!(analysis
            .call_edges
            .iter()
            .any(|edge| program.symbols.function(edge.caller).name == "Run"
                && program.symbols.function(edge.callee).name == "Std::Api"));
        let current = format!(
            "{:?}\n{:?}\n{:?}",
            program.symbols.functions, program.symbols.call_sites, analysis.call_edges
        );
        if let Some(expected) = &snapshot {
            assert_eq!(&current, expected);
        }
        snapshot = Some(current);
    }
}

#[test]
fn indexing_profile_reports_bundle_refusals_only_when_enabled() {
    let fixture = crate::common::fixture("direct_call");
    for enabled in [false, true] {
        let db = crate::common::TempDb::new("profile.db");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_trace"));
        command
            .arg("analyze")
            .arg(&fixture)
            .arg("--jobs")
            .arg("1")
            .arg("-o")
            .arg(db.path());
        command.env_remove("TRACE_PROFILE_INDEX");
        if enabled {
            command.env("TRACE_PROFILE_INDEX", "1");
        }
        let output = command.output().unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stderr}");
        let reports: Vec<_> = stderr
            .lines()
            .filter(|line| line.starts_with("index-profile: system-bundles "))
            .collect();
        assert_eq!(reports.len(), usize::from(enabled), "{stderr}");
        if enabled {
            assert_eq!(reports[0], "index-profile: system-bundles refused declarations=0 project_preamble=0 not-at-boundary=0 variant-cap=0 context-sensitive=0 uncovered=0 incomplete=0 incompatible=0");
        }
    }
}

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
            trace_ir::canonicalize(&root.join("orphan.h"))
        );
        assert_eq!(expansion.line, 2);
    }
}
