mod common;

use common::*;
use trace_analysis::{analyze, ResolutionKind};
use trace_db::open_db;
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn direct_call_exact_edge() {
    let root = fixture("direct_call");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "main",
        "helper",
        ResolutionKind::Direct
    ));
}

#[test]
fn false_positive_narrowed_fn_ptr() {
    let root = fixture("false_positive");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "narrowed",
            "a",
            ResolutionKind::Indirect
        ) || has_edge(&program, &analysis, "narrowed", "a", ResolutionKind::Direct),
        "expected narrowed -> a"
    );
    assert!(
        must_not_have_edge(&program, &analysis, "narrowed", "b"),
        "false positive: narrowed -> b"
    );
    assert!(
        must_not_have_edge(&program, &analysis, "narrowed", "c"),
        "false positive: narrowed -> c"
    );
}

#[test]
fn fn_ptr_init_resolves_target() {
    let root = fixture("fn_ptr_init");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "caller",
            "target",
            ResolutionKind::Indirect
        ) || has_edge(
            &program,
            &analysis,
            "caller",
            "target",
            ResolutionKind::Direct
        ),
        "caller should reach target via function pointer"
    );
    assert!(
        !program.flow.is_empty(),
        "expected flow constraints from initializer"
    );
}

#[test]
fn fn_ptr_field_assign_resolves_target() {
    let root = fixture("fn_ptr_field");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_edge(
            &program,
            &analysis,
            "caller",
            "target",
            ResolutionKind::Indirect
        ),
        "field assign then call should resolve"
    );
}

#[test]
fn fn_ptr_designated_init_resolves_target() {
    let root = fixture("fn_ptr_designated");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_edge(
            &program,
            &analysis,
            "caller",
            "target",
            ResolutionKind::Indirect
        ),
        "designated .handler = target should resolve indirect call"
    );
}

#[test]
fn fn_ptr_vtable_multi_hop_resolves_target() {
    let root = fixture("fn_ptr_vtable");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_edge(
            &program,
            &analysis,
            "dispatch",
            "target",
            ResolutionKind::Indirect
        ),
        "multi-hop interFace->handler should resolve"
    );
}

#[test]
fn camera_subdev_ops_setconfig_resolves_via_call_return() {
    let root = fixture("camera_subdev_ops");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_edge(
            &program,
            &analysis,
            "CommonDeviceSetConfig",
            "CameraCmdSensorSetConfig",
            ResolutionKind::Indirect
        ),
        "subDevOps->setConfig should resolve via GetSensorDeviceOps return"
    );
}

#[test]
fn in_out_ptr_has_store_flow() {
    let root = fixture("in_out_ptr");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program
            .flow
            .iter()
            .any(|f| matches!(f, trace_ir::FlowConstraint::Store { .. })),
        "expected Store constraint from *pp = &global_x"
    );
    let (pag, analysis) = analyze_with_pts(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "caller",
        "init",
        ResolutionKind::Direct
    ));
    // Issue #127: `init(&p)` passes p's address.
    assert_points_to(&program, &pag, &analysis, "pp", "p");
}

#[test]
fn arg_flow_pointer_param() {
    let root = fixture("arg_flow");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_edge(
        &program,
        &analysis,
        "provider",
        // `consume` is prototype-only: statically resolved, but classified
        // external because no definition exists under the fixture root.
        "consume",
        ResolutionKind::External
    ));
    assert!(
        arg_flow_count(&analysis) >= 1,
        "expected arg-flow from provider to consume"
    );
}

#[test]
fn sub_struct_field_assignment_flow() {
    let root = fixture("sub_struct");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program.flow.iter().any(|f| matches!(
            f,
            trace_ir::FlowConstraint::Store { .. } | trace_ir::FlowConstraint::GepField { .. }
        )),
        "expected field/store flow from o->inner.p = v"
    );
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "user",
        "assign_field",
        ResolutionKind::Direct
    ));
}

#[test]
fn multi_tu_unique_ids_and_export() {
    let root = fixture("indirect_call");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program.symbols.function_ids_unique(),
        "function ids must be unique across translation units"
    );
    let (pag, analysis) = analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    let conn = open_db(db.path()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM functions", [], |r| r.get(0))
        .unwrap();
    assert!(count >= 4, "expected functions from both TUs");
}

#[test]
fn indirect_call_via_param() {
    let root = fixture("indirect_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let edges = callees_of(&program, &analysis, "via_param");
    assert!(
        edges.iter().any(|(name, _)| name == "callee"),
        "via_param should call callee indirectly, got {:?}",
        edges
    );
    assert!(
        !edges
            .iter()
            .any(|(name, res)| name == "cb" && *res == ResolutionKind::Direct),
        "must not treat param cb as direct function name"
    );
}

#[test]
fn indirect_call_fixture_precise() {
    let root = fixture("indirect_call");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_edge(
            &program,
            &analysis,
            "run",
            "target",
            ResolutionKind::Indirect
        ) || has_edge(&program, &analysis, "run", "target", ResolutionKind::Direct)
    );

    let dispatcher_edges = callees_of(&program, &analysis, "dispatcher");
    assert!(
        !dispatcher_edges
            .iter()
            .any(|(n, r)| n == "use_fn_ptr" && *r == ResolutionKind::Direct),
        "false positive dispatcher -> use_fn_ptr: {:?}",
        dispatcher_edges
    );
}

#[test]
fn preproc_if0_skips_dead_branch() {
    let path = fixture("preproc/if0.c");
    let result = trace_preproc::preprocess_file(&path, &PreprocessOptions::new()).unwrap();
    assert!(
        !result.output.contains("42"),
        "dead branch must not define or emit HIDDEN=42"
    );
    assert!(
        result.output.contains("visible = 1")
            || result.output.contains("visible =1")
            || result.output.contains("int visible")
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("missing_header")),
        "must not attempt include from #if 0 branch"
    );
}

#[test]
fn export_sqlite_has_call_and_arg_tables() {
    let root = fixture("arg_flow");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    let conn = open_db(&db).unwrap();
    let calls: i64 = conn
        .query_row("SELECT COUNT(*) FROM call_edges", [], |r| r.get(0))
        .unwrap();
    assert!(calls >= 1);
}

#[test]
fn static_direct_call_resolves() {
    let root = fixture("static_direct_call");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "caller",
        "helper",
        ResolutionKind::Direct
    ));
}

#[test]
fn fn_arg_flow_exported() {
    let root = fixture("fn_arg_flow");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "user",
        "register_cb",
        ResolutionKind::Direct
    ));
    assert!(
        has_fn_arg_flow(&program, &analysis, "user", "register_cb", 0, "handler"),
        "expected fn pointer actual handler wired to register_cb formal"
    );

    let (pag, analysis) = analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    let conn = open_db(&db).unwrap();
    let fn_flow: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM arg_flow_edges WHERE actual_fn_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        fn_flow >= 1,
        "expected function-pointer arg flow in SQLite export"
    );
}

#[test]
fn static_call_return_expands() {
    let root = fixture("static_call_return");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "user",
        "GetOps",
        ResolutionKind::Direct
    ));
    assert!(
        program
            .flow
            .iter()
            .any(|f| matches!(f, trace_ir::FlowConstraint::CallReturn { .. })),
        "expected CallReturn constraint from GetOps() assignment"
    );
}

#[test]
fn fn_static_local_variable() {
    let root = fixture("fn_static_local");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let handler = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "handler")
        .expect("handler variable");
    assert_eq!(
        handler.storage,
        trace_ir::StorageClass::FnStatic,
        "function-local static must be FnStatic, not Local"
    );

    let (pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "user",
        "target",
        ResolutionKind::Indirect
    ));

    let db = export_program_full(&program, &pag, &analysis);
    let conn = open_db(&db).unwrap();
    let kind: String = conn
        .query_row(
            "SELECT kind FROM variables WHERE name = 'handler'",
            [],
            |r| r.get(0),
        )
        .expect("handler exported in full export");
    assert_eq!(kind, "fn_static");
}

#[test]
fn header_inline_call_indexed_from_header_unit() {
    let root = fixture("header_inline_call");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let header_only = program.symbols.functions.iter().find(|f| {
        f.name == "HeaderOnlyCaller"
            && program
                .symbols
                .files
                .get(f.file.0 as usize)
                .is_some_and(|fi| fi.path.ends_with("orphan_call.h"))
    });
    assert!(
        header_only.is_some(),
        "orphan_call.h must be indexed as its own unit (not included by any .c)"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "HeaderOnlyCaller",
            "ExternalTarget",
            ResolutionKind::Direct
        ) || has_edge(
            &program,
            &analysis,
            "HeaderOnlyCaller",
            "ExternalTarget",
            ResolutionKind::Indirect
        ),
        "call inside header-only inline function should resolve"
    );
    assert!(
        program
            .symbols
            .files
            .iter()
            .any(|f| f.path.ends_with("helper.h")),
        "helper.h is included by main.c and must appear as an attributed origin file"
    );
}

#[test]
fn header_chain_reachable_from_c_attributed_to_headers() {
    let root = fixture("header_chain");
    let program = build_program(&root, &default_opts(&root)).expect("build");

    // Headers reachable from a .c are no longer separate indexing units,
    // but they must appear as origin files for their lowered entities.
    assert!(
        program
            .symbols
            .files
            .iter()
            .any(|f| f.path.ends_with("chain_b.h")),
        "chain_b.h must be an attributed origin file"
    );
    let b_caller = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "BCaller")
        .expect("BCaller from chain_b.h should appear via main.c TU expansion");
    assert!(
        program
            .symbols
            .files
            .get(b_caller.span.file.0 as usize)
            .is_some_and(|fi| fi.path.ends_with("chain_b.h")),
        "BCaller should be attributed to its defining header, not the translation unit"
    );
}

#[test]
#[cfg(unix)]
fn macro_warm_preprocess_failure_is_nonfatal() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "#include \"good.h\"\nvoid main_fn(void) {}\n",
    )
    .unwrap();
    std::fs::write(root.join("good.h"), "void helper(void);\n").unwrap();
    std::fs::write(root.join("bad.h"), "void bad_helper(void);\n").unwrap();
    std::fs::write(
        root.join("also.c"),
        "#include \"bad.h\"\nvoid also_fn(void) {}\n",
    )
    .unwrap();
    let bad = root.join("bad.h");
    let mut perms = std::fs::metadata(&bad).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&bad, perms).unwrap();

    let program = build_program(root, &PreprocessOptions::new()).expect("build continues");
    let _ = std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644));
    assert!(
        program.diagnostics.iter().any(|d| {
            d.stage == "preprocess" && d.message.contains("macro warm preprocess failed")
        }),
        "expected macro warm warning for unreadable reachable header: {:?}",
        program.diagnostics
    );
    assert!(
        program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "main_fn"),
        "main.c should still be indexed after macro warm failure"
    );
}

#[test]
fn array_table_designated_init_resolves_targets() {
    let root = fixture("array_table_designated");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // Designated-init global table via helper-returned element pointer.
    assert!(
        has_edge(
            &program,
            &analysis,
            "caller_helper_ptr",
            "raw_obtain",
            ResolutionKind::Indirect
        ),
        "helper-ptr designated init: raw_obtain missing"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "caller_helper_ptr",
            "ipc_obtain",
            ResolutionKind::Indirect
        ),
        "helper-ptr designated init: ipc_obtain missing"
    );

    // Direct subscript access on the same table.
    assert!(
        has_edge(
            &program,
            &analysis,
            "caller_direct",
            "raw_obtain",
            ResolutionKind::Indirect
        ) && has_edge(
            &program,
            &analysis,
            "caller_direct",
            "ipc_obtain",
            ResolutionKind::Indirect
        ),
        "direct subscript designated init targets missing"
    );

    // Tentative (initializer-less) array + runtime stores into elements.
    assert!(
        has_edge(
            &program,
            &analysis,
            "run",
            "impl_a",
            ResolutionKind::Indirect
        ) && has_edge(
            &program,
            &analysis,
            "run",
            "impl_b",
            ResolutionKind::Indirect
        ),
        "runtime store into tentative array element: impl_a/impl_b missing"
    );

    // Local array with designated initializers.
    assert!(
        has_edge(
            &program,
            &analysis,
            "caller_local",
            "loc_a",
            ResolutionKind::Indirect
        ) && has_edge(
            &program,
            &analysis,
            "caller_local",
            "loc_b",
            ResolutionKind::Indirect
        ),
        "local designated-init array targets missing"
    );
}

/// One `(stage, severity, file path, line, message)` row per program diagnostic.
fn diagnostic_rows(program: &trace_ir::Program) -> Vec<(String, String, String, u32, String)> {
    program
        .diagnostics
        .iter()
        .map(|d| {
            let path = d
                .file
                .and_then(|f| program.symbols.files.get(f.0 as usize))
                .map(|f| f.path.display().to_string())
                .unwrap_or_default();
            (
                d.stage.clone(),
                format!("{:?}", d.severity),
                path,
                d.line,
                d.message.clone(),
            )
        })
        .collect()
}

fn temp_root(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("trace_{tag}_"))
        .tempdir()
        .unwrap()
}

#[test]
fn preprocess_diagnostics_reach_program_and_export() {
    let dir = temp_root("preproc_diag");
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "int before;\n#include \"does_not_exist.h\"\n#frobnicate\nint main(void) { return 0; }\n",
    )
    .unwrap();

    let program = build_program(root, &PreprocessOptions::new()).expect("build");
    let rows = diagnostic_rows(&program);
    let preproc: Vec<_> = rows.iter().filter(|r| r.0 == "preprocess").collect();
    assert_eq!(preproc.len(), 2, "{rows:?}");
    assert!(
        preproc.iter().any(|r| r.1 == "Warning"
            && r.2.ends_with("main.c")
            && r.3 == 2
            && r.4.contains("include file not found")
            && r.4.contains("does_not_exist.h")),
        "{rows:?}"
    );
    assert!(
        preproc.iter().any(|r| r.1 == "Warning"
            && r.2.ends_with("main.c")
            && r.3 == 3
            && r.4.contains("unknown directive #frobnicate")),
        "{rows:?}"
    );

    let (pag, analysis) = analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    let conn = open_db(db.path()).unwrap();
    let exported: Vec<(String, String, i64, String)> = conn
        .prepare(
            "SELECT d.severity, f.path, d.line, d.message FROM diagnostics d \
             JOIN files f ON f.id = d.file_id WHERE d.stage = 'preprocess' ORDER BY d.line",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(exported.len(), 2, "{exported:?}");
    assert_eq!(exported[0].0, "warning");
    assert!(exported[0].1.ends_with("main.c"), "{exported:?}");
    assert_eq!(exported[0].2, 2);
    assert!(exported[0].3.contains("does_not_exist.h"), "{exported:?}");
    assert_eq!(exported[1].2, 3);
    assert!(exported[1].3.contains("#frobnicate"), "{exported:?}");
}

#[test]
fn preprocess_diagnostics_are_deduplicated_and_deterministic_across_jobs() {
    let dir = temp_root("preproc_diag_dedup");
    let root = dir.path();
    std::fs::write(
        root.join("common.h"),
        "#include \"missing_in_header.h\"\n#common_directive\nvoid helper(void);\n",
    )
    .unwrap();
    // Enough units to cross the parallel parse batches (four per worker at
    // jobs=4) while sharing one header, so the merge order is exercised across
    // batch boundaries.
    for i in 0..19 {
        let tu = format!("tu_{i}.c");
        std::fs::write(
            root.join(&tu),
            format!(
                "#include \"common.h\"\n#tu_{i}_directive\nvoid tu_{i}_fn(void) {{ helper(); }}\n",
            ),
        )
        .unwrap();
    }

    let mut baseline = None;
    for jobs in [1, 4] {
        let program = trace_parse::build_program_with_jobs(root, &PreprocessOptions::new(), jobs)
            .expect("build");
        let mut rows = diagnostic_rows(&program);
        rows.sort();
        let hits: Vec<_> = rows
            .iter()
            .filter(|r| r.0 == "preprocess" && r.4.contains("missing_in_header.h"))
            .collect();
        assert_eq!(hits.len(), 1, "jobs={jobs}: {rows:?}");
        assert!(hits[0].2.ends_with("common.h"), "jobs={jobs}: {rows:?}");
        assert_eq!(hits[0].3, 1, "jobs={jobs}: {rows:?}");
        assert_eq!(hits[0].1, "Warning", "jobs={jobs}: {rows:?}");
        assert_eq!(rows.len(), 21, "jobs={jobs}: {rows:?}");
        assert!(
            rows.iter().all(|r| r.0 == "preprocess"),
            "jobs={jobs}: {rows:?}"
        );

        // The merged program must not depend on scheduling either: ids are
        // assigned in merge order, which is the file order at every job count.
        let merged = format!("{:?}\n{:?}", program.symbols, program.types);
        if let Some((expected_rows, expected_merged)) = &baseline {
            assert_eq!(&rows, expected_rows, "diagnostics changed with jobs={jobs}");
            assert_eq!(
                &merged, expected_merged,
                "merged program changed with jobs={jobs}"
            );
        } else {
            baseline = Some((rows, merged));
        }
    }
}

/// The discovery pass runs its units on the pool and commits them in unit
/// order (#88): whatever the scheduling, the program must be the one the
/// serial pass builds.
#[test]
fn parallel_discovery_builds_the_serial_program() {
    let dir = temp_root("parallel_discovery");
    write_discovery_tree(dir.path(), |_| "c");
    assert_discovery_is_serial(dir.path(), &["op_fast", "op_slow"]);
}

/// The same tree with every third unit C++: headers shared by C and C++ units
/// have one variant list per language, and `#ifdef __cplusplus` makes the two
/// lists differ.
#[test]
fn parallel_discovery_builds_the_serial_program_across_languages() {
    let dir = temp_root("parallel_discovery_languages");
    write_discovery_tree(dir.path(), |i| if i % 3 == 0 { "cpp" } else { "c" });
    assert_discovery_is_serial(dir.path(), &["op_c", "op_cpp"]);
}

/// Forty units that reach shared headers under different macros, so runs
/// ahead of the commit point publish variants that runs behind them would
/// replay, clash over, or only be shifted by. `extension` names unit `i`'s
/// language.
fn write_discovery_tree(root: &std::path::Path, extension: impl Fn(usize) -> &'static str) {
    std::fs::write(
        root.join("config.h"),
        "#ifndef CONFIG_H\n#define CONFIG_H\n\
         #ifdef WIDE\ntypedef long word;\n#else\ntypedef int word;\n#endif\n\
         #include \"ops.h\"\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        root.join("ops.h"),
        "#ifndef OPS_H\n#define OPS_H\n\
         #ifdef __cplusplus\nextern \"C\" {\nint op_cpp(int);\n}\n#else\nint op_c(int);\n#endif\n\
         #ifdef FAST\nword op_fast(word);\n#define OP op_fast\n\
         #else\nword op_slow(word);\n#define OP op_slow\n#endif\n#endif\n",
    )
    .unwrap();
    // Unguarded, so each inclusion re-expands under the unit's own `ENTRY`.
    std::fs::write(root.join("table.h"), "ENTRY(first)\nENTRY(second)\n").unwrap();
    for i in 0..40 {
        let mut text = String::new();
        if i % 3 == 0 {
            text.push_str("#define WIDE\n");
        }
        if i % 5 < 2 {
            text.push_str("#define FAST\n");
        }
        if i % 2 == 0 {
            text.push_str("#include \"ops.h\"\n");
        }
        text.push_str(&format!(
            "#include \"config.h\"\n\
             #define ENTRY(n) word n##_{i}(word x) {{ return OP(x); }}\n\
             #include \"table.h\"\n#undef ENTRY\n\
             int (*table_{i}[])(word) = {{ first_{i}, second_{i} }};\n"
        ));
        std::fs::write(root.join(format!("unit_{i:02}.{}", extension(i))), text).unwrap();
    }
}

/// Build `root` at several job counts and require the program the serial pass
/// builds every time, naming `expected` somewhere in it.
fn assert_discovery_is_serial(root: &std::path::Path, expected: &[&str]) {
    let mut baseline = None;
    for jobs in [1, 2, 4, 8] {
        let program = trace_parse::build_program_with_jobs(root, &PreprocessOptions::new(), jobs)
            .expect("build");
        let built = format!(
            "{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
            program.symbols,
            program.types,
            program.flow,
            program.fn_returns,
            diagnostic_rows(&program)
        );
        match &baseline {
            Some(serial) => assert!(built == *serial, "the program changed with jobs={jobs}"),
            None => {
                assert!(expected.iter().all(|name| built.contains(name)));
                baseline = Some(built);
            }
        }
    }
}

#[test]
fn unterminated_if_fixture_exports_preprocess_error() {
    let root = fixture("preproc");
    let program = build_program(&root, &PreprocessOptions::new()).expect("build");
    let rows = diagnostic_rows(&program);
    let hits: Vec<_> = rows
        .iter()
        .filter(|r| r.0 == "preprocess" && r.4.contains("unterminated #if"))
        .collect();
    assert_eq!(hits.len(), 2, "{rows:?}");
    for file in [
        "unterminated_if_header.h",
        "conditional_unterminated_comment.c",
    ] {
        let hit = hits.iter().find(|r| r.2.ends_with(file)).expect(file);
        assert_eq!(hit.1, "Error", "{rows:?}");
        assert_eq!(hit.3, 1, "{rows:?}");
    }
}

#[test]
fn preprocess_diagnostics_survive_second_language_warm() {
    // `shared.h` is reached from a C and a C++ unit, so the warm pass runs
    // it under both languages but caches only one. In C++ the `#frobnicate`
    // line is inside a raw string literal; in C it is an unknown directive.
    // That C-only diagnostic must still reach the program.
    let dir = temp_root("preproc_diag_two_langs");
    let root = dir.path();
    std::fs::write(
        root.join("shared.h"),
        "const char *s = R\"(\n#frobnicate\n)\";\nvoid helper(void);\n",
    )
    .unwrap();
    std::fs::write(
        root.join("a.c"),
        "#include \"shared.h\"\nvoid a_fn(void) {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        "#include \"shared.h\"\nvoid b_fn() {}\n",
    )
    .unwrap();

    let program = build_program(root, &PreprocessOptions::new()).expect("build");
    let rows = diagnostic_rows(&program);
    let hits: Vec<_> = rows
        .iter()
        .filter(|r| r.0 == "preprocess" && r.4.contains("frobnicate"))
        .collect();
    assert_eq!(hits.len(), 1, "{rows:?}");
    assert!(hits[0].2.ends_with("shared.h"), "{rows:?}");
    assert_eq!(hits[0].3, 2, "{rows:?}");
}

fn analyze_with_pts(
    program: &trace_ir::Program,
) -> (trace_analysis::Pag, trace_analysis::AnalysisResult) {
    trace_analysis::analyze_with_options(
        program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    )
}

/// Issue #127 (R1): a load through `int **` returns what the store wrote.
#[test]
fn load_through_pointer_typed_cell_keeps_flow() {
    let root = fixture("load_ptr_cell");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "direct", "g");
    assert_points_to(&program, &pag, &analysis, "viaload", "g");
}

/// Issue #127 (R4): out-parameter written through `int **`, read back by the
/// caller through the same cell (`*t`).
#[test]
fn out_param_via_variable_reaches_caller() {
    let root = fixture("load_ptr_cell");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "taken_via_var", "g");
}

/// Issue #127: a `Ptr`-typed cell holding a struct address loads it back.
#[test]
fn load_through_cell_holding_aggregate_address() {
    let root = fixture("load_ptr_cell");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "boxload", "bx");
}

/// Issue #127 (R5): the formal of `f(&r)` points to `r`.
#[test]
fn addr_of_argument_passes_the_address() {
    let root = fixture("addr_of_arg");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "seen", "r");
}

/// Issue #127 (R2): `get_buf(&q)` passes q's address, so formal `out` points to `q`.
#[test]
fn addr_of_arg_passes_address_to_formal() {
    let root = fixture("addr_of_arg");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_local_points_to(&program, &pag, &analysis, "get_buf", "out", "q");
}

/// Issue #127 (R7): `f(&s)` with `s = &x` passes s's address, not x.
#[test]
fn addr_of_initialized_pointer_passes_address_not_pointee() {
    let root = fixture("addr_of_arg");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "seen", "s");
    assert_not_points_to(&program, &pag, &analysis, "seen", "x");
}

fn var_cell_sync() -> (
    trace_ir::Program,
    trace_analysis::Pag,
    trace_analysis::AnalysisResult,
) {
    let root = fixture("var_cell_sync");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    (program, pag, analysis)
}

/// Issue #127 (R9, R10): a store through `&v` is visible to a direct read of `v`.
#[test]
fn cell_store_reaches_direct_read() {
    let (program, pag, analysis) = var_cell_sync();
    assert_points_to(&program, &pag, &analysis, "loc_ident", "g");
    assert_points_to(&program, &pag, &analysis, "glob_ident", "g");
}

/// Issue #127 (R11, R12): a direct write to `v` is visible to a load through `&v`,
/// and a global's self-location seed never leaks into its cell.
#[test]
fn direct_write_reaches_load_through_cell() {
    let (program, pag, analysis) = var_cell_sync();
    assert_points_to(&program, &pag, &analysis, "rev_local", "h");
    assert_points_to(&program, &pag, &analysis, "rev_glob", "h");
    assert_not_points_to(&program, &pag, &analysis, "rev_glob", "G2");
}

/// Issue #127 (R13): a callback returned through an out-parameter is called.
#[test]
fn out_param_callback_resolves_indirect_call() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(has_edge(
        &program,
        &analysis,
        "run_cb",
        "handler",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 (R8): the issue's own last example, `get_buf(&q); taken = q;`.
#[test]
fn addr_of_out_param_reaches_direct_read() {
    let root = fixture("addr_of_arg");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "taken", "g");
}

/// Issue #127: only variables whose address is taken are synced. `p = G3`
/// copies G3's value (plus the solver's self-location seed), so `*p = &k`
/// writes what G3 points to, never G3 itself.
#[test]
fn store_through_value_copy_does_not_write_the_global() {
    let (program, pag, analysis) = var_cell_sync();
    assert_points_to(&program, &pag, &analysis, "seed_read", "slot");
    assert_not_points_to(&program, &pag, &analysis, "seed_read", "k");
}

/// Issue #127 review: `f(&r)` with a C++ reference `r` passes the referent's
/// address, which is what the reference variable itself holds.
#[test]
fn addr_of_reference_passes_the_referent() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "seen", "global");
    assert_not_points_to(&program, &pag, &analysis, "seen", "r");
}

/// Issue #127 review: taking `&G4` somewhere does not make the solver's
/// self-location seed a real address: `p = G4; *p = &k` still writes what
/// G4 points to, never G4.
#[test]
fn address_taken_global_keeps_value_copies_apart() {
    let (program, pag, analysis) = var_cell_sync();
    assert_points_to(&program, &pag, &analysis, "g4_read", "slot");
    assert_not_points_to(&program, &pag, &analysis, "g4_read", "k");
}

/// Issue #127 review: `*out = &fn` and `*out = (cb_t)fn` store the function
/// like `*out = fn` does.
#[test]
fn out_param_callback_by_address_or_cast_resolves() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "run_by_address",
        "by_address",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "run_by_cast",
        "by_cast",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 review: deferred stores' temporaries belong to the function
/// whose body produced them.
#[test]
fn deferred_store_temps_have_a_function() {
    let (program, _pag, analysis) = var_cell_sync();
    let orphans: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name.starts_with("_ret") && v.fn_id.is_none())
        .map(|v| (v.name.clone(), v.span.line))
        .collect();
    assert!(orphans.is_empty(), "temps without a function: {orphans:?}");
    assert!(has_edge(
        &program,
        &analysis,
        "run_later_fn",
        "later_fn",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 review: a cell created mid-solve gets its slot guard, so a
/// function value cast into an `int *` cell is not lifted back out.
#[test]
fn mid_solve_cell_keeps_its_slot_guard() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(!has_edge(
        &program,
        &analysis,
        "mid_solve_guard",
        "two_args",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 review: an `AddrOf` that `expand_return_flows` adds mid-solve
/// (`return &v` behind an indirect call) seeds its destination.
#[test]
fn mid_solve_return_address_reaches_the_caller() {
    let (program, pag, analysis) = var_cell_sync();
    assert_local_points_to(
        &program,
        &pag,
        &analysis,
        "mid_solve_guard",
        "c",
        "made_mid_solve",
    );
}

/// Issue #127 review (P1): HDF's driver-loader singleton. The constructor
/// stores the methods through `HdfDriverLoader.super`, the caller loads them
/// through `IDriverLoader *`; both must reach one field summary.
#[test]
fn driver_loader_methods_resolve_through_the_interface() {
    let root = fixture("object_manager_loader");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    for callee in ["HdfDriverLoaderGetDriver", "HdfDriverLoaderReclaimDriver"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "DevHostServiceAddDevice",
                callee,
                ResolutionKind::Indirect
            ),
            "DevHostServiceAddDevice must reach {callee}"
        );
    }
}

/// Issue #127 review: a function stored through a `void **` out-parameter
/// reaches the caller's `void *`: untyped cells accept function values.
#[test]
fn void_pointer_cell_accepts_a_function() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(has_edge(
        &program,
        &analysis,
        "run_sym",
        "sym_handler",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 review: C declarators bind inside-out, so `int *t[4]` is an
/// array of pointers and `void (*h[4])(void)` an array of function pointers.
#[test]
fn declarators_bind_the_c_way() {
    use trace_ir::TypeDesc as TD;
    let (program, _pag, _analysis) = var_cell_sync();
    let desc = |name: &str| {
        let v = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .unwrap();
        program.types.get(v.type_id).desc.as_ref().clone()
    };
    assert!(
        matches!(desc("ptr_table"), TD::Array { elem, .. } if matches!(*elem, TD::Ptr(ref i) if **i == TD::Int)),
        "ptr_table: {:?}",
        desc("ptr_table")
    );
    assert!(
        matches!(desc("handler_table"), TD::Array { elem, .. } if matches!(*elem, TD::Ptr(ref i) if matches!(**i, TD::FnPtr { .. }))),
        "handler_table: {:?}",
        desc("handler_table")
    );
    assert!(
        matches!(desc("int_getter"), TD::Ptr(f) if matches!(*f, TD::FnPtr { ref ret, .. } if matches!(**ret, TD::Ptr(_)))),
        "int_getter: {:?}",
        desc("int_getter")
    );
}

/// Issue #127 review: an array of pointers keeps the storage seed, so a
/// callee writing through it reaches the elements.
#[test]
fn array_of_pointers_is_storage() {
    let (program, pag, analysis) = var_cell_sync();
    assert_points_to(&program, &pag, &analysis, "table_read", "g");
    assert!(has_edge(
        &program,
        &analysis,
        "run_handler_table",
        "table_handler",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 review: a variable whose address an indirect call returns has
/// its current value in its cell, like one whose address is taken directly.
#[test]
fn late_address_taken_cell_holds_the_current_value() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(has_edge(
        &program,
        &analysis,
        "use_cache",
        "cache_handler",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 review: a store of a pointer's value forges no address for it,
/// and a store of its address still hands the address out.
#[test]
fn stores_hand_out_values_and_addresses_as_written() {
    let (program, pag, analysis) = var_cell_sync();
    assert_points_to(&program, &pag, &analysis, "g5_read", "slot");
    assert_not_points_to(&program, &pag, &analysis, "g5_read", "k");
    assert_points_to(&program, &pag, &analysis, "g6_seen", "slot");
}

/// Issue #127 review: `p = &r` for a C++ reference is the referent's address.
#[test]
fn address_of_reference_in_assignment_is_the_referent() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "seen_through_ref", "global");
    assert_not_points_to(&program, &pag, &analysis, "seen_through_ref", "r2");
}

/// Issue #127 review: `&a` for an `auto &a` binding is the referent's address.
#[test]
fn addr_of_auto_reference_passes_the_referent() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "seen_auto_ref", "inst");
    assert_not_points_to(&program, &pag, &analysis, "seen_auto_ref", "a");
}

/// Issue #127 review: a pointer static that is never assigned in the analysed
/// code (so no longer seeded with its own location) keeps its field flow when
/// the type's summary fills past `SUMMARY_MEM_CAP`: its GEP falls back to the
/// summary itself, which a store writes directly — the cap only limits
/// summaries written *through* a concrete field cell.
#[test]
fn unassigned_pointer_static_keeps_its_own_field_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut src = String::from(
        "struct Mgr { void (*cb)(void); };\nstatic struct Mgr *g_mgr;\n\
         static void mgr_target(void) {}\n\
         void set_mgr_cb(void) { g_mgr->cb = mgr_target; }\n\
         void run_mgr_cb(void) { g_mgr->cb(); }\n",
    );
    for i in 0..1100 {
        src.push_str(&format!("void filler{i}(void) {{}}\n"));
    }
    src.push_str("void fill(struct Mgr *m) {\n");
    for i in 0..1100 {
        src.push_str(&format!("    m->cb = filler{i};\n"));
    }
    src.push_str("}\n");
    std::fs::write(dir.path().join("main.c"), src).unwrap();
    let root = dir.path().to_path_buf();
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "run_mgr_cb",
        "mgr_target",
        ResolutionKind::Indirect
    ));
}

/// Issue #127 review: the arg-flow row of `get_buf(&q)` names `q` — the object
/// the argument addresses — not the temporary lowering passes in its place.
#[test]
fn addr_of_argument_flow_names_the_object() {
    let root = fixture("addr_of_arg");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    let actuals: Vec<String> = analysis
        .arg_flow_edges
        .iter()
        .filter_map(|e| e.actual_var)
        .map(|v| program.symbols.variable(v).name.clone())
        .collect();
    assert!(actuals.iter().any(|n| n == "q"), "actuals: {actuals:?}");
    assert!(
        !actuals.iter().any(|n| n.starts_with("_ret")),
        "actuals: {actuals:?}"
    );
}

/// Issue #127 review: `*out = &ns::fn` and `*out = Cls::fn` store the function.
#[test]
fn qualified_function_designators_are_stored() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_any_edge(&program, &analysis, "run_ns", "ns::ns_cb"));
    assert!(has_any_edge(&program, &analysis, "run_cls", "Cls::handler"));
}

/// Review round 4: a `void *` table accepts function values like a `void *`.
#[test]
fn void_pointer_table_accepts_a_function() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(has_edge(
        &program,
        &analysis,
        "run_void_table",
        "void_table_handler",
        ResolutionKind::Indirect
    ));
}

/// Review round 4: `*p = (void *)&s.b` stores the member's address.
#[test]
fn cast_member_address_is_stored_as_the_member() {
    let (program, _pag, _analysis) = var_cell_sync();
    let var = |name: &str| {
        program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .unwrap()
            .id
    };
    let mp = var("mp");
    let stored: Vec<String> = program
        .flow
        .iter()
        .filter_map(|f| match f {
            trace_ir::FlowConstraint::Store { dst, src } if *dst == mp => {
                Some(program.symbols.variable(*src).name.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        stored.iter().all(|n| n.starts_with("_gep")) && !stored.is_empty(),
        "stored: {stored:?}"
    );
}

/// Review round 4: `memcpy((void *)&s.a, ..)` is a member address for models.
#[test]
fn cast_member_address_argument_is_flagged() {
    let (program, _pag, _analysis) = var_cell_sync();
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|cs| cs.callee_name == "memcpy" && fn_name(&program, cs.caller) == "copy_cast_member")
        .expect("memcpy site");
    assert_eq!(site.addr_of_member_args, vec![0]);
}

/// Review round 4: `drv.ops[i]()` and `pd->ops[i]()` call the table's element.
#[test]
fn field_dispatch_table_calls_resolve() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(has_edge(
        &program,
        &analysis,
        "run_field_table",
        "op_handler",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "run_ptr_field_table",
        "op_handler",
        ResolutionKind::Indirect
    ));
}

/// Review round 4: a callback argument under a cast is still passed.
#[test]
fn cast_callback_argument_is_passed() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(has_edge(
        &program,
        &analysis,
        "reg_cb",
        "reg_target",
        ResolutionKind::Indirect
    ));
}

/// Review round 4: `static_cast<X *>(&x)` passes x's address.
#[test]
fn named_cast_around_address_passes_the_address() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "seen_cast", "cast_target");
}

/// Review of #127: a field array element read by an initializer, an
/// assignment, a table-to-table copy or an argument reads the field that
/// `s.table[i] = fn` stored into.
#[test]
fn field_array_element_reads_see_stores() {
    let (program, _pag, analysis) = var_cell_sync();
    for caller in ["read_elem", "assign_elem", "call_copy", "reg_cb"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "elem_handler",
                ResolutionKind::Indirect
            ),
            "{caller} must reach elem_handler"
        );
    }
}

/// Review of #127: multi-dimensional callback tables, plain and as a struct
/// field, store and call their elements.
#[test]
fn multi_dimensional_tables_resolve() {
    let (program, _pag, analysis) = var_cell_sync();
    assert!(has_edge(
        &program,
        &analysis,
        "run_matrix",
        "matrix_handler",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "run_state",
        "state_handler",
        ResolutionKind::Indirect
    ));
}

/// Review of #127: C++ named casts carry pointer flow through assignments,
/// returns and arguments like C casts do.
#[test]
fn named_casts_carry_pointer_flow() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    for var in ["assigned_cast", "ret_seen", "arg_seen"] {
        assert_points_to(&program, &pag, &analysis, var, "global");
    }
}

/// Review of #127: a field-array element argument or callee under
/// parentheses or a cast is loaded like a bare one.
#[test]
fn wrapped_field_array_element_arguments_are_loaded() {
    let (program, _pag, analysis) = var_cell_sync();
    for callee in ["invoke_paren", "invoke_cast", "call_cast_elem"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                callee,
                "elem_handler",
                ResolutionKind::Indirect
            ),
            "{callee} must reach elem_handler"
        );
    }
}

/// PR review: `return &r` for a C++ reference returns the referent's address.
#[test]
fn returning_address_of_reference_returns_the_referent() {
    let root = fixture("cpp_out_param");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    assert_points_to(&program, &pag, &analysis, "seen_ref_return", "global");
    assert_not_points_to(&program, &pag, &analysis, "seen_ref_return", "r3");
}

/// PR review: a formal of `f(&x)` gets its one argument edge from the
/// address temp; the arg-flow row names `x`, but `x`'s own value does not
/// flow into the formal, so the flow graph adds no `x -> formal` edge.
#[test]
fn addr_of_argument_has_one_ingress_edge_in_the_flow_graph() {
    let root = fixture("addr_of_arg");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(&db).expect("open db");
    let edges: Vec<(String, String)> = conn
        .prepare(
            "SELECT s.label, e.kind FROM flow_edges e \
             JOIN flow_nodes s ON s.id = e.src_node JOIN flow_nodes d ON d.id = e.dst_node \
             JOIN variables v ON v.id = d.var_id JOIN functions f ON f.id = v.fn_id \
             WHERE d.label = 'out' AND f.name = 'get_buf' AND e.kind IN ('copy', 'call_arg')",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        edges.len(),
        1,
        "ingress edges into get_buf's out: {edges:?}"
    );
    assert!(
        edges[0].0.starts_with("_ret"),
        "from the address temp: {edges:?}"
    );
}
