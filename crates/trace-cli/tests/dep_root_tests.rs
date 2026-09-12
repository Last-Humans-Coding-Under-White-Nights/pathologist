//! Dependency roots (`--dep <root>`, issue #60): what a dependency tree
//! contributes to the analysis, and what it must not.

mod common;

use common::{fixture, fn_name, TempDb};
use std::process::Command;
use trace_analysis::{analyze, ResolutionKind};
use trace_db::{export_to_sqlite, open_db, CallEdgeFilter, ExportOptions};
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

#[test]
fn dep_root_excludes_sources_and_resolves_headers() {
    let target = fixture("dep_root/target");
    let dep = fixture("dep_root/dep");

    let opts = PreprocessOptions::new().with_dep(dep);
    let program = build_program_with_jobs(&target, &opts, 1).expect("build program with dep root");

    // A source under the dependency root is never a translation unit, and an
    // unreached dependency header is never indexed as a standalone orphan.
    let names: Vec<&str> = program
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        !names.contains(&"UnusedDepSourceFunc"),
        "dependency source indexed as a TU; functions: {names:?}"
    );
    assert!(
        !names.contains(&"OrphanDepFunc"),
        "unreached dependency header indexed as an orphan unit; functions: {names:?}"
    );

    // Dependency headers are reached, and classified apart from target files.
    let classified: Vec<(&str, bool)> = program
        .symbols
        .files
        .iter()
        .filter_map(|f| Some((f.path.file_name()?.to_str()?, f.is_dep)))
        .collect();
    assert!(
        classified.contains(&("refbase.h", true)),
        "refbase.h must be reached and flagged as a dependency file; got {classified:?}"
    );
    assert!(
        classified.contains(&("main.cpp", false)),
        "main.cpp must not be flagged as a dependency file; got {classified:?}"
    );

    // A body written in a dependency header contributes its signature only.
    let arrow = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "sptr::operator->")
        .expect("declared operator-> must reach the symbol table");
    assert!(
        !arrow.is_defined && arrow.locals.is_empty(),
        "dependency body must not be defined or carry locals: {arrow:?}"
    );

    // The declared `operator->` return unwraps `sptr<TargetService>`, so member
    // calls resolve on TargetService rather than on the wrapper.
    let (_pag, analysis) = analyze(&program);

    let main_targets: Vec<String> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == "main" && e.resolution == ResolutionKind::Direct)
        .map(|e| fn_name(&program, e.callee))
        .collect();

    for expected in ["TargetService::BaseMethod", "TargetService::ServiceAction"] {
        assert!(
            main_targets.iter().any(|t| t == expected),
            "main should resolve {expected} through operator-> unwrapping, got {main_targets:?}"
        );
    }

    // No phantom edge lands on the wrapper itself.
    let callees: Vec<String> = analysis
        .call_edges
        .iter()
        .map(|e| fn_name(&program, e.callee))
        .collect();
    assert!(
        !callees.iter().any(|c| c.starts_with("sptr::")),
        "must not resolve edges on the wrapper itself, got {callees:?}"
    );

    // A dependency body emits no call site, so nothing declared in a
    // dependency header is ever a caller — `sptr::operator->` calls
    // `dummy_dep_callee` and that call must not reach the analysis.
    let dep_callers: Vec<String> = analysis
        .call_edges
        .iter()
        .filter(|e| program.is_dep_file(program.symbols.function(e.caller).file))
        .map(|e| fn_name(&program, e.caller))
        .collect();
    assert!(
        dep_callers.is_empty(),
        "dependency bodies must contribute no call sites, got callers {dep_callers:?}"
    );

    // Declarations still resolve as callees: that is what `--dep` is for.
    assert!(
        callees.iter().any(|c| c == "dummy_dep_callee"),
        "a target call to a dependency declaration must resolve, got {callees:?}"
    );
}

#[test]
fn dep_root_sqlite_export_and_inspect_filter() {
    let target = fixture("dep_root/target");
    let dep = fixture("dep_root/dep");

    let opts = PreprocessOptions::new().with_dep(dep);
    let program = build_program_with_jobs(&target, &opts, 1).expect("build program with dep root");
    let (pag, analysis) = analyze(&program);

    let db = TempDb::new("dep_test.db");
    export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &ExportOptions {
            output: db.path().to_path_buf(),
            trace_version: "0.1.0".into(),
            include_points_to: false,
            full_detail: true,
            model_files: Vec::new(),
        },
    )
    .expect("export to sqlite");

    let conn = open_db(&db).expect("open exported db");

    // Every exported file and function is attributed to its origin tree.
    let file_flags: Vec<(String, i64)> = conn
        .prepare("SELECT path, is_dep FROM files ORDER BY path")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect()
        })
        .expect("query files");
    for (path, is_dep) in &file_flags {
        let in_dep_root = std::path::Path::new(path)
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|dir| dir == "dep");
        assert_eq!(*is_dep, i64::from(in_dep_root), "wrong is_dep for {path}");
    }

    let fn_flags: Vec<(String, i64)> = conn
        .prepare(
            "SELECT f.name, f.is_dep FROM functions f \
             JOIN files ON files.id = f.file_id ORDER BY f.name",
        )
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect()
        })
        .expect("query functions");
    assert_eq!(
        fn_flags
            .iter()
            .filter(|(_, is_dep)| *is_dep == 1)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "DepBase::BaseMethod",
            "DepBase::~DepBase",
            "dummy_dep_callee",
            "sptr::operator->",
            "sptr::sptr"
        ],
        "unexpected set of dependency functions in {fn_flags:?}"
    );
    assert!(
        fn_flags.contains(&("main".to_owned(), 0)),
        "main must be exported with is_dep = 0, got {fn_flags:?}"
    );

    // Dependency declarations never claim a body.
    let dep_defined: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM functions WHERE is_dep = 1 AND is_defined = 1",
            [],
            |r| r.get(0),
        )
        .expect("query defined dependency functions");
    assert_eq!(
        dep_defined, 0,
        "dependency functions must have is_defined = 0"
    );

    // `--exclude-deps` drops exactly the edges touching a dependency function.
    let edge_names = |exclude_deps: bool| -> Vec<String> {
        trace_db::call_edges(
            &conn,
            &CallEdgeFilter {
                from: None,
                to: None,
                file: None,
                exclude_deps,
            },
        )
        .expect("call_edges")
        .into_iter()
        .map(|e| format!("{} -> {}", e.caller_name, e.callee_name))
        .collect()
    };
    let all = edge_names(false);
    let kept = edge_names(true);
    assert!(
        all.contains(&"UseDependency -> dummy_dep_callee".to_owned()),
        "expected an edge into the dependency, got {all:?}"
    );
    assert!(
        !kept.contains(&"UseDependency -> dummy_dep_callee".to_owned()),
        "--exclude-deps must drop the edge into the dependency, got {kept:?}"
    );
    let dropped: Vec<&String> = all.iter().filter(|e| !kept.contains(e)).collect();
    assert_eq!(
        dropped,
        vec!["UseDependency -> dummy_dep_callee"],
        "--exclude-deps must drop only dependency edges"
    );
}

#[test]
fn dep_root_cli_end_to_end() {
    let bin = env!("CARGO_BIN_EXE_trace");
    let target = fixture("dep_root/target");
    let dep = fixture("dep_root/dep");
    let tmp = TempDb::new("dep_cli_e2e.db");

    // 1. Successful run with --dep
    let out = Command::new(bin)
        .args([
            "analyze",
            target.to_str().unwrap(),
            "--dep",
            dep.to_str().unwrap(),
            "-o",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("run trace analyze");
    assert!(
        out.status.success(),
        "trace analyze failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. inspect calls works
    let out = Command::new(bin)
        .args(["inspect", tmp.to_str().unwrap(), "calls"])
        .output()
        .expect("run trace inspect calls");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dummy_dep_callee"),
        "calls output should contain the dependency edge: {stdout}"
    );

    // 3. inspect calls --exclude-deps hides the edge into the dependency
    let out = Command::new(bin)
        .args(["inspect", tmp.to_str().unwrap(), "calls", "--exclude-deps"])
        .output()
        .expect("run trace inspect calls --exclude-deps");
    assert!(out.status.success());
    let filtered = String::from_utf8_lossy(&out.stdout);
    assert!(
        filtered.contains("TargetService::BaseMethod"),
        "target edges must survive --exclude-deps: {filtered}"
    );
    assert!(
        !filtered.contains("dummy_dep_callee"),
        "--exclude-deps must hide the dependency edge: {filtered}"
    );

    // 4. Non-existent --dep root fails cleanly
    let out = Command::new(bin)
        .args([
            "analyze",
            target.to_str().unwrap(),
            "--dep",
            "nonexistent_directory_dep_root",
            "-o",
            tmp.to_str().unwrap(),
        ])
        .output()
        .expect("run trace analyze with missing dep");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dependency root does not exist"),
        "expected error about nonexistent dependency root, got: {stderr}"
    );

    // 5. A --dep root that contains or equals the analysis root is rejected at
    // validation. Without the check every source is classified as a dependency
    // and the run dies later with "no C/C++ source files found", which points
    // at the tree instead of at the flag.
    for (dep_arg, relation) in [(target.clone(), "is"), (fixture("dep_root"), "contains")] {
        let out = Command::new(bin)
            .args([
                "analyze",
                target.to_str().unwrap(),
                "--dep",
                dep_arg.to_str().unwrap(),
                "-o",
                tmp.to_str().unwrap(),
            ])
            .output()
            .expect("run trace analyze with a containing dep root");
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!("{relation} the analysis root")),
            "expected the containment error for {}, got: {stderr}",
            dep_arg.display()
        );
        assert!(
            !stderr.contains("no C/C++ source files found"),
            "containment must be caught at validation, not downstream: {stderr}"
        );
    }
}

#[test]
fn dependency_bodies_and_initializers_do_not_emit_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let dep = tmp.path().join("dep");
    std::fs::create_dir(&dep).unwrap();
    std::fs::write(
        dep.join("api.h"),
        r#"
        void callback();
        void (*global_cb)() = callback;
        inline void implementation(void (*param)()) {
            static void (*cached)() = callback;
            param = callback;
            global_cb = callback;
            cached();
        }
    "#,
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("main.cpp"),
        "#include <api.h>\nvoid target() { global_cb(); }\n",
    )
    .unwrap();
    for jobs in [1, 2] {
        let program =
            build_program_with_jobs(tmp.path(), &PreprocessOptions::new().with_dep(&dep), jobs)
                .unwrap();
        assert!(
            !program.symbols.variables.iter().any(|v| v.name == "cached"),
            "dependency function-local static escaped into the program"
        );
        assert!(
            program.flow.is_empty(),
            "dependency value flow leaked: {:?}",
            program.flow
        );
        assert!(program.fn_returns.is_empty());
        assert!(program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "implementation" && f.params.len() == 1));
        assert!(program
            .symbols
            .call_sites
            .iter()
            .all(|c| !program.is_dep_file(c.span.file)));
    }
}

#[test]
fn target_header_reached_through_dependency_keeps_its_body() {
    let tmp = tempfile::tempdir().unwrap();
    let dep = tmp.path().join("dep");
    std::fs::create_dir(&dep).unwrap();
    std::fs::write(dep.join("api.h"), "#include <target.h>\n").unwrap();
    std::fs::write(
        tmp.path().join("target.h"),
        "void callee();\ninline void target_inline() { callee(); }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("main.cpp"),
        "#include <api.h>\nvoid target() { target_inline(); }\n",
    )
    .unwrap();
    let program =
        build_program_with_jobs(tmp.path(), &PreprocessOptions::new().with_dep(&dep), 1).unwrap();
    assert!(
        program
            .symbols
            .call_sites
            .iter()
            .any(|c| fn_name(&program, c.caller) == "target_inline" && c.callee_name == "callee"),
        "target header body was discarded by its dependency includer"
    );
}
