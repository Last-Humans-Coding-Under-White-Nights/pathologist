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

/// #132 end to end: non-local destinations of `dst = f()` see the calling
/// file's `static f`, and not another unit's.
#[test]
fn static_call_return_reaches_non_local_destinations() {
    let root = fixture("static_return_to_global");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    for (caller, callee) in [
        ("use_a", "ret_static"),
        ("use_b", "ret_static"),
        ("use_c", "ret_static_cpp"),
    ] {
        assert!(
            has_edge(&program, &analysis, caller, callee, ResolutionKind::Direct),
            "{caller} -> {callee}"
        );
    }
    // Two units define `ret_static`; use_b's edge must land on b.c's.
    let use_b = only_function(&program, "use_b");
    let callee_files: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == use_b)
        .map(|e| {
            let f = program.symbols.function(e.callee);
            program.symbols.files[f.file.0 as usize].path.clone()
        })
        .collect();
    assert_eq!(callee_files.len(), 1, "{callee_files:?}");
    assert!(callee_files[0].ends_with("b.c"), "{callee_files:?}");
    for (var, loc) in [
        ("seen_a", "global_a"),        // global destination
        ("seen_static_a", "global_a"), // file-static destination
        ("seen_init_a", "global_a"),   // file-scope initializer (no caller)
        ("seen_b", "global_b"),        // b.c's own static, not a.c's
        ("cpp_seen", "global_c"),
        ("cpp_seen_static", "global_c"),
        ("cpp_seen_init", "global_c"),
    ] {
        assert_eq!(
            points_to_names(&program, &pag, &analysis, var),
            vec![loc.to_string()],
            "{var}"
        );
    }
}

/// `CallReturn.caller` is the function the call is written in, remapped to
/// the merged program's ids; a file-scope initializer has none.
#[test]
fn call_return_records_its_enclosing_function() {
    let root = fixture("static_return_to_global");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let caller_of = |dst: trace_ir::VarId| {
        program
            .flow
            .iter()
            .find_map(|f| match f {
                trace_ir::FlowConstraint::CallReturn { dst: d, caller, .. } if *d == dst => {
                    Some(*caller)
                }
                _ => None,
            })
            .expect("CallReturn for the destination")
    };
    for (dst, caller) in [
        ("seen_a", Some("use_a")),
        ("seen_static_a", Some("use_a")),
        ("seen_b", Some("use_b")),
        ("cpp_seen", Some("use_c")),
        ("cpp_seen_static", Some("use_c")),
        ("seen_init_a", None),
        ("cpp_seen_init", None),
    ] {
        assert_eq!(
            caller_of(only_variable(&program, dst)),
            caller.map(|c| only_function(&program, c)),
            "{dst}"
        );
    }
}

#[test]
fn fn_static_local_variable() {
    let root = fixture("fn_static_local");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let handler = program.symbols.variable(only_variable(&program, "handler"));
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
        let v = program.symbols.variable(only_variable(&program, name));
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
    let mp = only_variable(&program, "mp");
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

#[test]
fn qualified_variables_issue_133() {
    let root = fixture("cpp_qualified_variables");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    for name in ["ptr", "member", "from_ns", "from_member", "seen_arg"] {
        assert_points_to(&program, &pag, &analysis, name, "global");
    }
}

/// `trace analyze <root> <args>` into a fresh database.
fn cli_analyze(root: &std::path::Path, args: &[&str]) -> TempDb {
    let db = TempDb::new("cli.db");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(root)
        .args(args)
        .arg("-o")
        .arg(db.path())
        .output()
        .expect("run trace analyze");
    assert!(
        out.status.success(),
        "trace analyze failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

/// Every row of `sql`, each column rendered as text.
fn text_rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| {
        (0..columns)
            .map(|i| Ok(format!("{:?}", row.get::<_, rusqlite::types::Value>(i)?)))
            .collect()
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// The exported value-flow graph, nodes and edges by id.
fn flow_graph(db: &TempDb) -> Vec<Vec<String>> {
    let conn = open_db(db).unwrap();
    let mut rows = text_rows(&conn, "SELECT * FROM flow_nodes ORDER BY id");
    rows.extend(text_rows(&conn, "SELECT * FROM flow_edges ORDER BY id"));
    rows
}

/// #133 through the CLI and the issue's own query: every name in the
/// reproducer points to `global`, and minimal and full exports carry the same
/// flow graph.
#[test]
fn qualified_variables_issue_133_cli_export() {
    let root = fixture("cpp_qualified_variables");
    let full = cli_analyze(
        &root,
        &["--jobs", "1", "--full-export", "--debug-points-to"],
    );
    let conn = open_db(&full).unwrap();
    let rows: Vec<(String, String)> = conn
        .prepare(
            "SELECT n.label, l.desc FROM points_to pt \
             JOIN locations l ON l.id = pt.loc_id \
             JOIN flow_nodes n ON n.id = pt.var_node_id \
             WHERE n.label IN ('ptr', 'member', 'from_ns', 'from_member', 'seen_arg')",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for name in ["ptr", "member", "from_ns", "from_member", "seen_arg"] {
        assert!(
            rows.contains(&(name.to_string(), "global".to_string())),
            "{name} -> global missing: {rows:?}"
        );
    }
    let minimal = cli_analyze(&root, &["--jobs", "1"]);
    assert_eq!(flow_graph(&minimal), flow_graph(&full));
}

/// Every analysis row of `db`, table by table and sorted, leaving out the run
/// metadata (`analysis_run`, which records when and how the run happened).
fn analysis_rows(db: &TempDb) -> Vec<(String, Vec<Vec<String>>)> {
    let conn = open_db(db).unwrap();
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name <> 'analysis_run' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    tables
        .into_iter()
        .map(|table| {
            let mut rows = text_rows(&conn, &format!("SELECT * FROM \"{table}\""));
            rows.sort();
            (table, rows)
        })
        .collect()
}

/// Repeated runs at one job and at eight export the same analysis rows, ids
/// included: scheduling must not reach the data (AGENTS.md invariant 10).
#[test]
fn qualified_variables_export_is_deterministic_across_jobs() {
    let root = fixture("cpp_qualified_variables");
    let run = |jobs: &str| {
        analysis_rows(&cli_analyze(
            &root,
            &["--jobs", jobs, "--full-export", "--debug-points-to"],
        ))
    };
    let reference = run("1");
    assert!(
        reference
            .iter()
            .any(|(table, rows)| table == "points_to" && !rows.is_empty()),
        "the comparison must cover solved data"
    );
    for jobs in ["1", "8", "8"] {
        assert_eq!(run(jobs), reference, "jobs={jobs} exported different rows");
    }
}

/// Build and solve one C++ translation unit written to a scratch tree.
fn analyze_cpp_source(
    src: &str,
) -> (
    trace_ir::Program,
    trace_analysis::Pag,
    trace_analysis::AnalysisResult,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("main.cpp"), src).unwrap();
    let root = dir.path().to_path_buf();
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = analyze_with_pts(&program);
    (program, pag, analysis)
}

/// #133: `a::ptr` reads namespace `a`'s variable, not `b::ptr` and not a
/// local `ptr` in scope at the read.
#[test]
fn qualified_read_selects_its_namespace() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X a_obj, b_obj, local_obj;\n\
         namespace a { X *ptr = &a_obj; }\n\
         namespace b { X *ptr = &b_obj; }\n\
         X *scoped_out;\n\
         void read_scoped() {\n\
             X *ptr = &local_obj;\n\
             scoped_out = a::ptr;\n\
             (void)ptr;\n\
         }\n",
    );
    assert_points_to(&program, &pag, &analysis, "scoped_out", "a_obj");
    assert_not_points_to(&program, &pag, &analysis, "scoped_out", "b_obj");
    assert_not_points_to(&program, &pag, &analysis, "scoped_out", "local_obj");
}

/// #133: `::ptr` inside namespace `a` reads the global, not `a::ptr`.
#[test]
fn global_qualified_read_skips_the_enclosing_namespace() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X global_obj, ns_obj;\n\
         X *ptr = &global_obj;\n\
         namespace a {\n\
         X *ptr = &ns_obj;\n\
         X *root_out;\n\
         void read_root() { root_out = ::ptr; }\n\
         }\n",
    );
    assert_points_to(&program, &pag, &analysis, "root_out", "global_obj");
    assert_not_points_to(&program, &pag, &analysis, "root_out", "ns_obj");
}

/// #133: an unqualified name inside namespace `a` reads `a`'s variable
/// before the global one of the same name.
#[test]
fn unqualified_read_inside_a_namespace_selects_its_variable() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X global_obj, ns_obj;\n\
         X *ptr = &global_obj;\n\
         namespace a {\n\
         X *ptr = &ns_obj;\n\
         X *near_out;\n\
         void read_near() { near_out = ptr; }\n\
         }\n",
    );
    assert_points_to(&program, &pag, &analysis, "near_out", "ns_obj");
    assert_not_points_to(&program, &pag, &analysis, "near_out", "global_obj");
}

/// #133: an assignment to `a::ptr` reaches a later read of `a::ptr`.
#[test]
fn qualified_store_reaches_qualified_read() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X initial, replacement;\n\
         namespace a { X *ptr = &initial; }\n\
         X *stored_out;\n\
         void write_then_read() {\n\
             a::ptr = &replacement;\n\
             stored_out = a::ptr;\n\
         }\n",
    );
    assert_points_to(&program, &pag, &analysis, "stored_out", "replacement");
}

/// #133: `take(&(a::ptr))` and `take(static_cast<X **>(&a::ptr))` pass the
/// variable's cell, so the callee's load reads the variable's value.
#[test]
fn addr_of_qualified_argument_passes_the_cell() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X obj;\n\
         namespace a { X *ptr = &obj; }\n\
         X *paren_loaded;\n\
         X *cast_loaded;\n\
         static void take_paren(X **pp) { paren_loaded = *pp; }\n\
         static void take_cast(X **pc) { cast_loaded = *pc; }\n\
         void pass_cells() {\n\
             take_paren(&(a::ptr));\n\
             take_cast(static_cast<X **>(&a::ptr));\n\
         }\n",
    );
    assert_local_points_to(&program, &pag, &analysis, "take_paren", "pp", "ptr");
    assert_local_points_to(&program, &pag, &analysis, "take_cast", "pc", "ptr");
    assert_points_to(&program, &pag, &analysis, "paren_loaded", "obj");
    assert_points_to(&program, &pag, &analysis, "cast_loaded", "obj");
}

/// #133: `put(&a::ptr)` writing `*p = &replacement` is seen by a direct read
/// of `a::ptr` afterwards.
#[test]
fn out_param_write_reaches_qualified_read() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X replacement;\n\
         namespace a { X *ptr; }\n\
         X *written_out;\n\
         static void put(X **p) { *p = &replacement; }\n\
         void fill() {\n\
             put(&a::ptr);\n\
             written_out = a::ptr;\n\
         }\n",
    );
    assert_points_to(&program, &pag, &analysis, "written_out", "replacement");
}

/// #133: `return a::ptr` returns the value and `return &a::ptr` the cell;
/// callers keep them apart.
#[test]
fn qualified_return_value_and_address_stay_distinct() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X obj;\n\
         namespace a { X *ptr = &obj; }\n\
         X *get_value() { return a::ptr; }\n\
         X **get_cell() { return &a::ptr; }\n\
         X *value_out;\n\
         X **cell_out;\n\
         void use_returns() {\n\
             value_out = get_value();\n\
             cell_out = get_cell();\n\
         }\n",
    );
    assert_points_to(&program, &pag, &analysis, "value_out", "obj");
    assert_not_points_to(&program, &pag, &analysis, "value_out", "ptr");
    assert_points_to(&program, &pag, &analysis, "cell_out", "ptr");
    assert_not_points_to(&program, &pag, &analysis, "cell_out", "obj");
}

/// #133: `&a::ref` for a namespace-scope reference is its referent's
/// address, not a cell of the reference binding.
#[test]
fn addr_of_qualified_reference_passes_the_referent() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X object;\n\
         namespace a { X &ref = object; }\n\
         X *ref_seen;\n\
         static void take_x(X *p) { ref_seen = p; }\n\
         void pass_ref() { take_x(&a::ref); }\n",
    );
    assert_points_to(&program, &pag, &analysis, "ref_seen", "object");
    assert_not_points_to(&program, &pag, &analysis, "ref_seen", "ref");
}

/// #133: `cb = &a::handler` still stores the qualified function, which the
/// indirect call then reaches.
#[test]
fn qualified_function_designator_still_resolves() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "namespace a { void handler() {} }\n\
         typedef void (*cb_t)();\n\
         void run_designator() {\n\
             cb_t cb;\n\
             cb = &a::handler;\n\
             cb();\n\
         }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "run_designator",
        "a::handler",
        ResolutionKind::Indirect
    ));
}

/// #133: `a::cb()` calls through the namespace-scope callback variable.
#[test]
fn qualified_callback_variable_is_an_indirect_callee() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "void handler() {}\n\
         typedef void (*cb_t)();\n\
         namespace a { cb_t cb = handler; }\n\
         void run_qualified_cb() { a::cb(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "run_qualified_cb",
        "handler",
        ResolutionKind::Indirect
    ));
}

/// #133: an unresolved `missing::ptr` never falls back to the bare `ptr`.
#[test]
fn unresolved_qualified_name_does_not_fall_back_to_the_bare_name() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X bare_obj;\n\
         X *ptr = &bare_obj;\n\
         X *missing_out;\n\
         X *bare_out;\n\
         void read_missing() {\n\
             missing_out = missing::ptr;\n\
             bare_out = ptr;\n\
         }\n",
    );
    assert_points_to(&program, &pag, &analysis, "bare_out", "bare_obj");
    assert_not_points_to(&program, &pag, &analysis, "missing_out", "bare_obj");
}

/// #133: a qualified variable receiver (`a::obj->Go()`, `a::inst.Go()`) is
/// typed like a plain one, so the member call reaches its class's method.
#[test]
fn qualified_receiver_dispatches_its_method() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "struct Impl { void Go(); };\n\
         void Impl::Go() {}\n\
         struct Other { void Go(); };\n\
         void Other::Go() {}\n\
         namespace a { Impl *obj; Impl inst; }\n\
         void run_ptr() { a::obj->Go(); }\n\
         void run_obj() { a::inst.Go(); }\n",
    );
    for caller in ["run_ptr", "run_obj"] {
        assert!(
            has_any_edge(&program, &analysis, caller, "Impl::Go"),
            "{caller} must reach Impl::Go"
        );
        assert!(
            must_not_have_edge(&program, &analysis, caller, "Other::Go"),
            "{caller} must not reach Other::Go"
        );
    }
}

/// #133: a scope spelled with template arguments designates no variable:
/// `Box<int>::member` must not bind the unrelated `Box::member` its stripped
/// spelling would name.
#[test]
fn templated_scope_does_not_bind_a_stripped_spelling() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X unrelated;\n\
         namespace Box { X *member = &unrelated; }\n\
         X *templated_out;\n\
         void read_templated() { templated_out = Box<int>::member; }\n",
    );
    assert_not_points_to(&program, &pag, &analysis, "templated_out", "unrelated");
}

/// #133: a qualified function passed as an argument, plain or by address,
/// reaches the callee's indirect call — each form through its own callee, so
/// neither can hide a regression in the other.
#[test]
fn qualified_function_argument_is_passed() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "namespace a { void handler() {} }\n\
         typedef void (*cb_t)();\n\
         static void invoke_plain(cb_t cb) { cb(); }\n\
         static void invoke_addr(cb_t cb) { cb(); }\n\
         void pass_handlers() {\n\
             invoke_plain(a::handler);\n\
             invoke_addr(&a::handler);\n\
         }\n",
    );
    for caller in ["invoke_plain", "invoke_addr"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "a::handler",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
    }
}

/// #133: inside its class's member function, an unqualified static data
/// member name reads the member, not a global of the same name.
#[test]
fn unqualified_static_member_read_inside_its_class() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X member_obj, global_obj;\n\
         X *member = &global_obj;\n\
         struct Holder { static X *member; void read(); };\n\
         X *Holder::member = &member_obj;\n\
         X *member_out;\n\
         void Holder::read() { member_out = member; }\n",
    );
    assert_points_to(&program, &pag, &analysis, "member_out", "member_obj");
    assert_not_points_to(&program, &pag, &analysis, "member_out", "global_obj");
}

/// #133: a static data member reached through an object (`h.m`, `hp->m`,
/// `this->m`) is the member's one storage, as `Holder::m` is — not an
/// instance field, which a static member no longer has.
#[test]
fn static_member_through_an_object_is_the_member_itself() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X initial, by_dot, by_this, by_addr;\n\
         struct Holder { static X *m; void set() { this->m = &by_this; } };\n\
         X *Holder::m = &initial;\n\
         X *arrow_out, *dot_out, *qualified_out, *addr_out;\n\
         static void put(X **pp) { *pp = &by_addr; }\n\
         void through(Holder &h, Holder *hp) {\n\
             h.m = &by_dot;\n\
             arrow_out = hp->m;\n\
             put(&h.m);\n\
         }\n\
         struct Derived : Holder {};\n\
         X *derived_out;\n\
         void read() {\n\
             Holder h; Derived d;\n\
             dot_out = h.m; qualified_out = Holder::m; derived_out = d.m;\n\
         }\n",
    );
    for out in ["arrow_out", "dot_out", "qualified_out", "derived_out"] {
        for object in ["initial", "by_dot", "by_this", "by_addr"] {
            assert_points_to(&program, &pag, &analysis, out, object);
        }
    }
}

/// #133 review: calling a static callback member through an object
/// (`h.cb()`, `h->cb()`) calls through the member variable, as `H::cb()`
/// does — not a member function `H::cb` that does not exist.
#[test]
fn static_callback_member_called_through_an_object() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void good() {}\n\
         struct H { static CB cb; };\n\
         CB H::cb = good;\n\
         void call_dot(H &h) { h.cb(); }\n\
         void call_arrow(H *h) { h->cb(); }\n",
    );
    for caller in ["call_dot", "call_arrow"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "good",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
    }
}

/// #133 review: an out-of-class static member definition's initializer is
/// in its class's scope, so `CB I::target = source;` reads `I::source`.
#[test]
fn static_member_definition_initializer_sees_its_class() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void good() {}\n\
         void outer_fn() {}\n\
         CB source = outer_fn;\n\
         struct I { static CB source; static CB target; };\n\
         CB I::source = good;\n\
         CB I::target = source;\n\
         void call() { I::target(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(!has_edge(
        &program,
        &analysis,
        "call",
        "outer_fn",
        ResolutionKind::Indirect
    ));
}

/// #133 review: every declarator of one static member declaration is its
/// own member with its own initializer.
#[test]
fn every_declarator_of_a_static_member_declaration_is_registered() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void good() {}\n\
         void bad() {}\n\
         struct J { inline static CB first = good, second = bad; };\n\
         void call_first() { J::first(); }\n\
         void call_second() { J::second(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call_first",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(!has_edge(
        &program,
        &analysis,
        "call_first",
        "bad",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "call_second",
        "bad",
        ResolutionKind::Indirect
    ));
    assert!(!has_edge(
        &program,
        &analysis,
        "call_second",
        "good",
        ResolutionKind::Indirect
    ));
}

/// #133 review: a static data member reached through an object types the
/// receiver of a call on it, as `H::m` does.
#[test]
fn static_member_through_an_object_types_a_call_receiver() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "struct T { void dot(); void arrow(); };\n\
         void T::dot() {}\n\
         void T::arrow() {}\n\
         struct H { static T m; };\n\
         T H::m;\n\
         void use_dot(H &h) { h.m.dot(); }\n\
         void use_arrow(H *hp) { hp->m.arrow(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "use_dot",
        "T::dot",
        ResolutionKind::Direct
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "use_arrow",
        "T::arrow",
        ResolutionKind::Direct
    ));
}

/// #133 review: `return h.m;` / `return hp->m;` return the static member's
/// value, as `return H::m;` does.
#[test]
fn static_member_through_an_object_is_returned() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X object;\n\
         struct H { static X *m; };\n\
         X *H::m = &object;\n\
         X *via_object(H &h) { return h.m; }\n\
         X *via_pointer(H *hp) { return hp->m; }\n\
         X *object_out, *pointer_out;\n\
         void use(H &h) { object_out = via_object(h); pointer_out = via_pointer(&h); }\n",
    );
    assert_points_to(&program, &pag, &analysis, "object_out", "object");
    assert_points_to(&program, &pag, &analysis, "pointer_out", "object");
}

/// #133 review: a derived class's own member hides a base class's static
/// data member of that name, through an object and unqualified alike.
#[test]
fn a_derived_member_hides_a_base_static_member() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void target_a() {}\n\
         void target_c() {}\n\
         struct B { static cb_t cb; };\n\
         cb_t B::cb = target_a;\n\
         struct D : B { void cb(); };\n\
         void D::cb() {}\n\
         void call(D &d) { d.cb(); }\n\
         struct Base { static cb_t m; };\n\
         cb_t Base::m = target_a;\n\
         struct D2 : Base { cb_t m; void set(); };\n\
         void D2::set() { m = target_c; }\n\
         void store(D2 *d) { d->m = target_c; }\n\
         void call_base() { Base::m(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "D::cb",
        ResolutionKind::Direct
    ));
    assert!(!has_edge(
        &program,
        &analysis,
        "call",
        "target_a",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "call_base",
        "target_a",
        ResolutionKind::Indirect
    ));
    assert!(!has_edge(
        &program,
        &analysis,
        "call_base",
        "target_c",
        ResolutionKind::Indirect
    ));
}

/// #133 review: a static reference member is a reference binding, so
/// `&H::ref` is the referent's address, not the reference's own cell.
#[test]
fn a_static_reference_member_passes_its_referent() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X object;\n\
         X *out_def, *out_inline;\n\
         struct H { static X &ref; inline static X &iref = object; };\n\
         X &H::ref = object;\n\
         static void take_def(X *p) { out_def = p; }\n\
         static void take_inline(X *q) { out_inline = q; }\n\
         void use() { take_def(&H::ref); take_inline(&H::iref); }\n",
    );
    assert_points_to(&program, &pag, &analysis, "out_def", "object");
    assert_not_points_to(&program, &pag, &analysis, "out_def", "ref");
    assert_points_to(&program, &pag, &analysis, "out_inline", "object");
    assert_not_points_to(&program, &pag, &analysis, "out_inline", "iref");
}

/// #133 review: a field path through a static member object (`h.obj.cb`)
/// is rooted at the member's variable, the object `Holder::obj.cb` names.
#[test]
fn a_field_path_through_a_static_member_object() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void good() {}\n\
         void arrow_good() {}\n\
         struct Obj { cb_t cb; };\n\
         struct Holder { static Obj obj; static Obj *ptr; };\n\
         Obj Holder::obj;\n\
         Obj *Holder::ptr;\n\
         void set(Holder &h, Holder *hp) { h.obj.cb = good; hp->ptr->cb = arrow_good; }\n\
         void call() { Holder::obj.cb(); Holder::ptr->cb(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "arrow_good",
        ResolutionKind::Indirect
    ));
}

/// #133 review: a qualified callable object (`ns::f()`, `H::sf()`) calls
/// its class's `operator()`, as an unqualified one does.
#[test]
fn a_qualified_callable_object_calls_its_operator() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "struct F { void operator()(); };\n\
         void F::operator()() {}\n\
         namespace ns { F f; }\n\
         struct H { static F sf; };\n\
         F H::sf;\n\
         void call_ns() { ns::f(); }\n\
         void call_member() { H::sf(); }\n",
    );
    for caller in ["call_ns", "call_member"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "F::operator()",
                ResolutionKind::Direct
            ),
            "{caller}"
        );
    }
}

/// #133 review: an out-of-class static member definition written under
/// `using namespace` defines the member the header declared, as a member
/// function definition there does (one variable, reached by the body).
#[test]
fn a_static_member_defined_under_a_using_directive() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void target_a() {}\n\
         namespace OHOS { struct FooTest { static cb_t proxy_; void Run(); }; }\n\
         using namespace OHOS;\n\
         cb_t FooTest::proxy_ = target_a;\n\
         void FooTest::Run() { proxy_(); }\n",
    );
    let proxies = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "proxy_")
        .count();
    assert_eq!(proxies, 1);
    assert!(has_edge(
        &program,
        &analysis,
        "OHOS::FooTest::Run",
        "target_a",
        ResolutionKind::Indirect
    ));
}

/// #133 review: a direct-initialized out-of-class static member definition
/// (`cb_t H::direct(target_a);`) initializes the member; it declares no
/// function.
#[test]
fn a_direct_initialized_static_member_definition() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void target_a() {}\n\
         struct H { static cb_t direct; };\n\
         cb_t H::direct(target_a);\n\
         void call() { H::direct(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "target_a",
        ResolutionKind::Indirect
    ));
    assert!(
        !program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "H::direct"),
        "no function H::direct"
    );
}

/// #133 review: the arguments of a direct-initialized static member
/// definition are looked up in the member's class, as a copy initializer's
/// are: `fallback` is `H::fallback`, not the global of that name.
#[test]
fn a_direct_initialized_static_member_looks_its_argument_up_in_the_class() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void target_a() {}\n\
         void target_b() {}\n\
         cb_t fallback = target_b;\n\
         struct H { static cb_t fallback; static cb_t cb; };\n\
         cb_t H::fallback = target_a;\n\
         cb_t H::cb(fallback);\n\
         void call() { H::cb(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "target_a",
        ResolutionKind::Indirect
    ));
    assert!(!has_any_edge(&program, &analysis, "call", "target_b"));
    assert!(
        !program.symbols.functions.iter().any(|f| f.name == "H::cb"),
        "no function H::cb"
    );
}

/// #133 review: an argument list lowering cannot resolve still defines a
/// known static member: no function `H::val`, and the variable is defined.
#[test]
fn a_direct_initialized_static_member_with_an_unresolved_argument() {
    let (program, _pag, _analysis) = analyze_cpp_source(
        "struct H { static int val; };\n\
         int H::val(kUnknown);\n",
    );
    assert!(
        !program.symbols.functions.iter().any(|f| f.name == "H::val"),
        "no function H::val"
    );
    let val = program
        .symbols
        .variables
        .iter()
        .find(|v| v.qualified_name.as_deref() == Some("H::val"))
        .expect("variable H::val");
    assert!(val.is_defined);
}

/// #133 review: the inverse — a qualified name no static member declares
/// keeps `cb_t H::unknown(cb_t);` a function declaration.
#[test]
fn a_direct_init_shaped_declaration_of_an_unknown_member_declares_a_function() {
    let (program, _pag, _analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         struct H { static cb_t known; cb_t unknown(cb_t); };\n\
         cb_t H::unknown(cb_t);\n",
    );
    assert!(program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "H::unknown"));
    assert!(!program
        .symbols
        .variables
        .iter()
        .any(|v| v.name == "unknown"));
}

/// #133 review: `h.table[i]()` on a static member table loads the element,
/// as `H::table[i]()` does: a store through the element reaches the call.
#[test]
fn a_static_member_table_element_call_through_an_object() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void target_a() {}\n\
         void target_b() {}\n\
         struct H { static cb_t table[2]; };\n\
         cb_t H::table[2] = { target_a };\n\
         void set(int i) { H::table[i] = target_b; }\n\
         void call(H h, int i) { h.table[i](); }\n",
    );
    for target in ["target_a", "target_b"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "call",
                target,
                ResolutionKind::Indirect
            ),
            "{target}"
        );
    }
    let call = program
        .symbols
        .call_sites
        .iter()
        .find(|c| fn_name(&program, c.caller) == "call")
        .expect("call site in call");
    let table = program
        .symbols
        .variables
        .iter()
        .find(|v| v.qualified_name.as_deref() == Some("H::table"))
        .expect("H::table");
    assert_ne!(
        call.callee_var,
        Some(table.id),
        "calls the element, not the table"
    );
}

/// #133 review: in the scope walk (a scoped variable `M::f` shares the
/// leaf), a namespace's file `static` function hides a global variable of
/// its name, as an external one does: `take(f)` does not pass `::f`.
#[test]
fn a_namespace_file_static_function_hides_an_outer_variable() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void target_a() {}\n\
         cb_t f = target_a;\n\
         namespace M { cb_t f; }\n\
         namespace N {\n\
         static void f() {}\n\
         void take(cb_t p) { p(); }\n\
         void g() { take(f); }\n\
         }\n",
    );
    assert!(!has_any_edge(&program, &analysis, "N::take", "target_a"));
}

/// #133 review: a call through a qualified table's element (`H::table[i]()`,
/// `ns::table[i]()`) loads the element, as `table[i]()` does.
#[test]
fn a_qualified_table_element_call_loads_the_element() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*cb_t)();\n\
         void target_a() {}\n\
         void target_b() {}\n\
         struct H { static cb_t table[2]; };\n\
         cb_t H::table[2] = { target_a, target_b };\n\
         namespace ns { cb_t ntable[1] = { target_b }; }\n\
         void call_member(int i) { H::table[i](); }\n\
         void call_ns(int i) { ns::ntable[i](); }\n",
    );
    for target in ["target_a", "target_b"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                "call_member",
                target,
                ResolutionKind::Indirect
            ),
            "{target}"
        );
    }
    assert!(has_edge(
        &program,
        &analysis,
        "call_ns",
        "target_b",
        ResolutionKind::Indirect
    ));
}

/// #133 review: `&h.ref` on a static reference member is the referent's
/// address, as `&H::ref` is.
#[test]
fn an_object_form_static_reference_passes_its_referent() {
    let (program, pag, analysis) = analyze_cpp_source(
        "struct X { int v; };\n\
         X object;\n\
         X *out;\n\
         struct H { static X &ref; };\n\
         X &H::ref = object;\n\
         static void take(X *p) { out = p; }\n\
         void use(H &h) { take(&h.ref); }\n",
    );
    assert_points_to(&program, &pag, &analysis, "out", "object");
    assert_not_points_to(&program, &pag, &analysis, "out", "ref");
}

/// #133 review: `h.fun()` / `h->fun()` on a static callable member calls its
/// `operator()`, as `H::fun()` does.
#[test]
fn an_object_form_static_callable_calls_its_operator() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "struct F { void operator()(); };\n\
         void F::operator()() {}\n\
         struct H { static F fun; };\n\
         F H::fun;\n\
         void call_dot(H &h) { h.fun(); }\n\
         void call_arrow(H *h) { h->fun(); }\n",
    );
    for caller in ["call_dot", "call_arrow"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "F::operator()",
                ResolutionKind::Direct
            ),
            "{caller}"
        );
    }
}

/// #133 review: an instance field that hides an outer variable is the
/// callee of a bare call in a member body, through `this`.
#[test]
fn a_field_hiding_an_outer_variable_is_called_through_this() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void bad() {}\n\
         void good() {}\n\
         CB cb = bad;\n\
         namespace other { CB cb; }\n\
         struct H { CB cb; void run(); };\n\
         void H::run() { cb(); }\n\
         void init(H *h) { h->cb = good; }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "H::run",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(!has_any_edge(&program, &analysis, "H::run", "bad"));
}

/// #133 review: a namespace function hides an outer variable of its name
/// whether or not an unrelated scoped variable shares the name.
#[test]
fn a_namespace_function_hides_an_outer_variable_either_way() {
    for other in ["", "namespace other { CB cb = bad; }\n"] {
        let (program, _pag, analysis) = analyze_cpp_source(&format!(
            "typedef void (*CB)();\n\
             void bad() {{}}\n\
             CB cb = bad;\n\
             {other}\
             namespace ns {{ void cb() {{}} void run() {{ cb(); }} }}\n"
        ));
        assert!(
            has_edge(
                &program,
                &analysis,
                "ns::run",
                "ns::cb",
                ResolutionKind::Direct
            ),
            "{other:?}"
        );
        assert!(
            !has_any_edge(&program, &analysis, "ns::run", "bad"),
            "{other:?}"
        );
    }
}

/// #133 review: `using namespace ns;` and `using ns::cb;` bring a
/// namespace variable into scope, as they do a function.
#[test]
fn a_using_directive_or_declaration_brings_a_namespace_variable_in() {
    for using in ["using namespace ns;", "using ns::cb;"] {
        let (program, _pag, analysis) = analyze_cpp_source(&format!(
            "typedef void (*CB)();\n\
             void good() {{}}\n\
             namespace ns {{ CB cb = good; }}\n\
             {using}\n\
             void call() {{ cb(); }}\n"
        ));
        assert!(
            has_edge(
                &program,
                &analysis,
                "call",
                "good",
                ResolutionKind::Indirect
            ),
            "{using}"
        );
    }
}

/// #133 review: under `using namespace OHOS;` a qualified `Foo::inst_`
/// names `OHOS::Foo::inst_`.
#[test]
fn a_using_directive_qualifies_a_static_member_read() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void good() {}\n\
         namespace OHOS { struct Foo { static CB inst_; }; CB Foo::inst_ = good; }\n\
         using namespace OHOS;\n\
         void call() { Foo::inst_(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "good",
        ResolutionKind::Indirect
    ));
}

/// #133 review: a variable, parameter or instance field shadows a function
/// of its name in a value: `out = f;` copies the variable or loads the field.
#[test]
fn a_variable_or_field_shadows_a_function_in_a_value() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void f() {}\n\
         void good() {}\n\
         CB out_local;\n\
         CB out_field;\n\
         void read_local() { CB f = good; out_local = f; }\n\
         struct H { CB f; void read_field(); };\n\
         void H::read_field() { out_field = f; }\n\
         void init(H *h) { h->f = good; }\n\
         void call_local() { out_local(); }\n\
         void call_field() { out_field(); }\n",
    );
    for caller in ["call_local", "call_field"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "good",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
        assert!(!has_any_edge(&program, &analysis, caller, "f"), "{caller}");
    }
}

/// #133 review: an instance field hides an outer function of its name, so a
/// bare call in a member body calls through `this->cb`.
#[test]
fn a_field_hides_an_outer_function_in_a_bare_call() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void cb() {}\n\
         void good() {}\n\
         struct H { CB cb; void run(); };\n\
         void H::run() { cb(); }\n\
         void init(H *h) { h->cb = good; }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "H::run",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(!has_any_edge(&program, &analysis, "H::run", "cb"));
}

/// #133 review: an inner `using` declaration is asked before an outer one.
#[test]
fn an_inner_using_declaration_wins_over_an_outer_one() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void good() {}\n\
         void bad() {}\n\
         namespace a { CB x = bad; }\n\
         namespace b { CB x = good; }\n\
         using a::x;\n\
         void call() { using b::x; x(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(!has_any_edge(&program, &analysis, "call", "bad"));
}

/// #133 review: at file scope a function of the name is asked before a
/// variable a `using` brings in.
#[test]
fn a_file_scope_function_wins_over_a_using_imported_variable() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void bad() {}\n\
         namespace ns { CB cb = bad; }\n\
         using namespace ns;\n\
         void cb() {}\n\
         void call() { cb(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "call",
        "cb",
        ResolutionKind::Direct
    ));
    assert!(!has_any_edge(&program, &analysis, "call", "bad"));
}

/// #133 review: `using namespace A;` inside `Outer` means `Outer::A` before
/// a global `A`, as the directive's own candidates are ordered.
#[test]
fn a_relative_using_directive_prefers_the_enclosing_namespace() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void good() {}\n\
         void bad() {}\n\
         namespace A { CB cb = bad; }\n\
         namespace Outer {\n\
         namespace A { CB cb = good; }\n\
         using namespace A;\n\
         void call() { cb(); }\n\
         }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "Outer::call",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(!has_any_edge(&program, &analysis, "Outer::call", "bad"));
}

/// #133 review: a `using` declaration in a body hides a function outside
/// it, as a local does.
#[test]
fn a_body_using_declaration_hides_a_global_function() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void good() {}\n\
         void cb() {}\n\
         namespace ns { CB cb = good; }\n\
         void run() { using ns::cb; cb(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "run",
        "good",
        ResolutionKind::Indirect
    ));
    assert!(!has_any_edge(&program, &analysis, "run", "cb"));
}

/// #133: a function named bare as a value resolves through the enclosing
/// classes and namespaces, as a call's name does: the class's or the
/// namespace's function, not a same-named one further out.
#[test]
fn a_bare_function_value_resolves_through_the_enclosing_scopes() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void Default() {}\n\
         void f() {}\n\
         struct H {\n\
           static void Default();\n\
           static CB copy_init;\n\
           static CB direct_init;\n\
           static CB assigned;\n\
           void Set();\n\
         };\n\
         void H::Default() {}\n\
         CB H::copy_init = Default;\n\
         CB H::direct_init(Default);\n\
         void H::Set() { assigned = Default; }\n\
         void call_copy_init() { H::copy_init(); }\n\
         void call_direct_init() { H::direct_init(); }\n\
         void call_assigned() { H::assigned(); }\n\
         namespace N {\n\
           void f() {}\n\
           CB out;\n\
           void take(CB p) { p(); }\n\
           CB get() { return f; }\n\
           void assign() { out = f; }\n\
           void assign_addr() { out = &f; }\n\
           void pass() { take(f); }\n\
           void store(CB *slot) { *slot = f; }\n\
           void call_out() { out(); }\n\
           void call_get() { CB got = get(); got(); }\n\
           CB relay() { return get(); }\n\
           void call_relay() { CB got = relay(); got(); }\n\
           CB stored;\n\
           void call_stored() { store(&stored); stored(); }\n\
         }\n",
    );
    for caller in ["call_copy_init", "call_direct_init", "call_assigned"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "H::Default",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
        assert!(
            !has_any_edge(&program, &analysis, caller, "Default"),
            "{caller}"
        );
    }
    for caller in [
        "N::call_out",
        "N::call_get",
        "N::call_relay",
        "N::take",
        "N::call_stored",
    ] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "N::f",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
        assert!(!has_any_edge(&program, &analysis, caller, "f"), "{caller}");
    }
}

/// #133: a bare function value in a namespace resolves to the namespace's
/// function even where a global variable of its name exists, and a
/// relative qualification (`inner::g` inside `outer`) or a `using`
/// directive reaches the intended function too.
#[test]
fn a_function_value_through_relative_scopes_and_using() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "typedef void (*CB)();\n\
         void bad() {}\n\
         CB f = bad;\n\
         namespace M { CB f; }\n\
         namespace N { void f() {} CB out; void g() { out = f; out(); } }\n\
         namespace outer {\n\
           namespace inner { void g() {} }\n\
           CB out;\n\
           void h() { out = inner::g; out(); }\n\
         }\n\
         namespace lib { void handler() {} }\n\
         using namespace lib;\n\
         CB via_using = handler;\n\
         void call_using() { via_using(); }\n",
    );
    assert!(has_edge(
        &program,
        &analysis,
        "N::g",
        "N::f",
        ResolutionKind::Indirect
    ));
    assert!(!has_any_edge(&program, &analysis, "N::g", "bad"));
    assert!(has_edge(
        &program,
        &analysis,
        "outer::h",
        "outer::inner::g",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "call_using",
        "lib::handler",
        ResolutionKind::Indirect
    ));
}

/// #133: in a body, `T obj(Ns::kValue);` with an argument lowering cannot
/// resolve (an enumerator) defines an object, as C++ reads it unless the
/// name is a type: it declares no function `obj`, whose address a later
/// `x = obj;` would otherwise store.
#[test]
fn a_body_direct_init_with_an_unresolved_argument_defines_an_object() {
    let (program, _pag, analysis) = analyze_cpp_source(
        "namespace Json { enum Kind { arrayValue }; struct Value { Value(Kind k); void append(); }; }\n\
         void Json::Value::append() {}\n\
         namespace N {\n\
         Json::Value sink(Json::arrayValue);\n\
         void run() {\n\
           Json::Value log(Json::arrayValue);\n\
           sink = log;\n\
           sink.append();\n\
         }\n\
         }\n",
    );
    assert!(
        !program
            .symbols
            .functions
            .iter()
            .any(|f| f.name.ends_with("log")),
        "no function log"
    );
    assert!(!analysis
        .call_edges
        .iter()
        .any(|e| fn_name(&program, e.callee).ends_with("log")));
}
