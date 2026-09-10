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
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "caller",
        "init",
        ResolutionKind::Direct
    ));
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
