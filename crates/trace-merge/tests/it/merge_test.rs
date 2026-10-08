use rusqlite::Connection;
use std::fs;
use std::process::Command;
use tempfile::tempdir;
use trace_merge::{merge_databases, MergeOptions, WarningKind};

fn trace_bin() -> &'static str {
    env!("CARGO_BIN_EXE_trace-merge")
}

fn trace_cli_bin() -> std::path::PathBuf {
    static BIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let mut path = std::path::PathBuf::from(env!("CARGO_BIN_EXE_trace-merge"));
        path.set_file_name(format!("trace{}", std::env::consts::EXE_SUFFIX));
        if !path.exists() {
            let status = Command::new("cargo")
                .args(["build", "-q", "-p", "trace-cli"])
                .status()
                .expect("failed to build trace-cli");
            assert!(status.success(), "failed to build trace-cli");
        }
        path
    })
    .clone()
}

/// `trace analyze repo -o db` with the workspace's own CLI.
fn analyze_repo(repo: &std::path::Path, db: &std::path::Path) {
    let status = Command::new(trace_cli_bin())
        .args([
            "analyze",
            repo.to_str().unwrap(),
            "-o",
            db.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run trace analyze");
    assert!(status.success(), "trace analyze {}", repo.display());
}

#[test]
fn test_cross_repo_callgraph_restoration() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Repo 1: caller calling an external function `compute_hash`
    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("main.c"),
        r#"
        extern int compute_hash(int input);
        int run_app(int x) {
            return compute_hash(x);
        }
        "#,
    )
    .unwrap();

    // Repo 2: callee defining `compute_hash`
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("hash.c"),
        r#"
        int helper(int v) {
            return v * 31;
        }
        int compute_hash(int input) {
            return helper(input);
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("repo_a.db");
    let db_b = root.join("repo_b.db");
    let db_merged = root.join("unified.db");

    // Run trace analyze on repo A
    analyze_repo(&repo_a, &db_a);

    // Run trace analyze on repo B
    analyze_repo(&repo_b, &db_b);

    // Verify before merge: db_a has external call edge to compute_hash
    {
        let conn_a = Connection::open(&db_a).unwrap();
        let resolution: String = conn_a
            .query_row(
                "SELECT ce.resolution FROM call_edges ce \
                 JOIN functions caller ON caller.id = ce.caller_fn_id \
                 JOIN functions callee ON callee.id = ce.callee_fn_id \
                 WHERE caller.name = 'run_app' AND callee.name = 'compute_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolution, "external");

        let is_defined: i64 = conn_a
            .query_row(
                "SELECT is_defined FROM functions WHERE name = 'compute_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_defined, 0);
    }

    // Now run merge
    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge failed");

    assert_eq!(report.input_dbs.len(), 2);
    assert_eq!(report.cross_repo_calls_resolved, 1);
    assert_eq!(report.external_calls_unresolved, 0);

    // Verify in merged DB:
    // 1. compute_hash is now is_defined = 1
    // 2. call edge from run_app to compute_hash is now resolution = 'direct'
    {
        let conn_m = Connection::open(&db_merged).unwrap();
        let (resolution, is_defined, callee_file): (String, i64, String) = conn_m
            .query_row(
                "SELECT ce.resolution, callee.is_defined, f.path FROM call_edges ce \
                 JOIN functions caller ON caller.id = ce.caller_fn_id \
                 JOIN functions callee ON callee.id = ce.callee_fn_id \
                 JOIN files f ON f.id = callee.file_id \
                 WHERE caller.name = 'run_app' AND callee.name = 'compute_hash'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(resolution, "direct");
        assert_eq!(is_defined, 1);
        assert!(callee_file.contains("hash.c"));
    }

    // Verify `trace inspect callchain` works across repositories on the merged DB!
    let output = Command::new("cargo")
        .args([
            "run",
            "-q",
            "-p",
            "trace-cli",
            "--",
            "inspect",
            db_merged.to_str().unwrap(),
            "callchain",
            "--from",
            "run_app",
            "--to",
            "helper",
        ])
        .output()
        .expect("failed to run inspect callchain");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("run_app"));
    assert!(stdout.contains("compute_hash"));
    assert!(stdout.contains("helper"));
}

#[test]
fn test_merge_problem_reporting() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Repo 1: calls an unresolved external `missing_system_api`, and calls `shared_calc`
    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("main.c"),
        r#"
        extern void missing_system_api(void);
        extern int shared_calc(int x);
        int client(int a) {
            missing_system_api();
            return shared_calc(a);
        }
        "#,
    )
    .unwrap();

    // Repo 2: defines `shared_calc` (strong)
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("calc_b.c"),
        r#"
        int shared_calc(int x) {
            return x + 10;
        }
        "#,
    )
    .unwrap();

    // Repo 3: also defines `shared_calc` (conflicting strong definition)
    let repo_c = root.join("repo_c");
    fs::create_dir_all(&repo_c).unwrap();
    fs::write(
        repo_c.join("calc_c.c"),
        r#"
        int shared_calc(int x) {
            return x * 2;
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("repo_a.db");
    let db_b = root.join("repo_b.db");
    let db_c = root.join("repo_c.db");
    let db_merged = root.join("merged_with_problems.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b), (&repo_c, &db_c)] {
        analyze_repo(repo, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b, &db_c],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should complete with warnings");

    assert_eq!(report.input_dbs.len(), 3);
    assert_eq!(report.external_calls_unresolved, 1);

    // Verify multiple definition collision warning was generated
    let collision_warning = report
        .warnings
        .iter()
        .find(|w| w.kind == WarningKind::MultipleDefinitions);
    assert!(collision_warning.is_some());
    let w = collision_warning.unwrap();
    assert_eq!(w.symbol.as_deref(), Some("shared_calc"));
    assert!(w.message.contains("multiple strong definitions"));

    // Verify unresolved external warning was generated
    let unresolved_warning = report
        .warnings
        .iter()
        .find(|w| w.kind == WarningKind::UnresolvedExternal);
    assert!(unresolved_warning.is_some());
    let u = unresolved_warning.unwrap();
    assert_eq!(u.symbol.as_deref(), Some("missing_system_api"));

    // Verify diagnostics recorded in the merged database
    let conn = Connection::open(&db_merged).unwrap();
    let merge_diags: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM diagnostics WHERE stage = 'merge'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(merge_diags >= 2);
}

#[test]
fn test_cli_trace_merge() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();

    fs::write(
        repo_a.join("a.c"),
        "extern void bar(void); void foo(void) { bar(); }",
    )
    .unwrap();
    fs::write(repo_b.join("b.c"), "void bar(void) {}").unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_out = root.join("unified_cli.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b)] {
        analyze_repo(repo, db);
    }

    // Run trace-merge CLI binary
    let output = Command::new(trace_bin())
        .args([
            db_a.to_str().unwrap(),
            db_b.to_str().unwrap(),
            "-o",
            db_out.to_str().unwrap(),
            "--verbose",
        ])
        .output()
        .expect("failed to run trace-merge binary");

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("merge complete"));
    assert!(stderr.contains("cross-repo resolved"));
    assert!(db_out.exists());
}

#[test]
fn test_weak_symbol_override_across_repos() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();

    // Repo A defines a weak function `get_version` and calls it
    fs::write(
        repo_a.join("a.c"),
        r#"
        __attribute__((weak)) int get_version(void) {
            return 1;
        }
        int check_version(void) {
            return get_version();
        }
        "#,
    )
    .unwrap();

    // Repo B defines a strong `get_version`
    fs::write(
        repo_b.join("b.c"),
        r#"
        int get_version(void) {
            return 2;
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged_weak.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b)] {
        analyze_repo(repo, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    assert!(report
        .warnings
        .iter()
        .any(|w| w.kind == WarningKind::WeakOverride));

    // Verify in merged DB that the call from check_version is retargeted to Repo B's strong definition
    let conn = Connection::open(&db_merged).unwrap();
    let (callee_is_weak, callee_file): (i64, String) = conn
        .query_row(
            "SELECT callee.is_weak, f.path FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             JOIN files f ON f.id = callee.file_id \
             WHERE caller.name = 'check_version' AND callee.name = 'get_version'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(callee_is_weak, 0); // Strong definition won!
    assert!(callee_file.contains("b.c"));
}

#[test]
fn test_invalid_and_duplicate_inputs() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Empty inputs
    let err = merge_databases::<&str>(
        &[],
        &MergeOptions {
            output: root.join("out.db"),
            verbose: false,
        },
    );
    assert!(err.is_err());
    assert!(err.unwrap_err().to_string().contains("no input databases"));

    // Non-existent input
    let err = merge_databases(
        &[root.join("nonexistent.db")],
        &MergeOptions {
            output: root.join("out.db"),
            verbose: false,
        },
    );
    assert!(err.is_err());
}

#[test]
fn test_cross_repo_cpp_overload_resolution() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Repo 1: caller calling overloaded external functions
    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("main.cpp"),
        r#"
        int process(int x);
        int process(double y);

        int caller_int(int a) {
            return process(a);
        }

        int caller_double(double b) {
            return process(b);
        }
        "#,
    )
    .unwrap();

    // Repo 2: callee defining both overloads
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("impl.cpp"),
        r#"
        int process(int x) {
            return x * 2;
        }

        int process(double y) {
            return (int)(y * 3.0);
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged.db");

    analyze_repo(&repo_a, &db_a);

    analyze_repo(&repo_b, &db_b);

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    // Both calls should be resolved without treating overloads as multiple definition collisions
    assert_eq!(report.cross_repo_calls_resolved, 2);
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.kind == WarningKind::MultipleDefinitions)
            .count(),
        0
    );

    let conn = Connection::open(&db_merged).unwrap();
    let int_edge: (String, String, String) = conn
        .query_row(
            "SELECT caller.name, callee.signature, ce.resolution FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             WHERE caller.name = 'caller_int'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(int_edge.0, "caller_int");
    assert_eq!(int_edge.1, "process(int)");
    assert_eq!(int_edge.2, "direct");

    let double_edge: (String, String, String) = conn
        .query_row(
            "SELECT caller.name, callee.signature, ce.resolution FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             WHERE caller.name = 'caller_double'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(double_edge.0, "caller_double");
    assert_eq!(double_edge.1, "process(double)");
    assert_eq!(double_edge.2, "direct");
}

#[test]
fn test_ambiguous_calls_produce_edges_to_all_candidates() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Lib A defines strong `init` and weak `hook`
    let lib_a = root.join("lib_a");
    fs::create_dir_all(&lib_a).unwrap();
    fs::write(
        lib_a.join("a.c"),
        r#"
        void init(void) {}
        __attribute__((weak)) void hook(void) {}
        "#,
    )
    .unwrap();

    // Lib B defines strong `init` and weak `hook`
    let lib_b = root.join("lib_b");
    fs::create_dir_all(&lib_b).unwrap();
    fs::write(
        lib_b.join("b.c"),
        r#"
        void init(void) {}
        __attribute__((weak)) void hook(void) {}
        "#,
    )
    .unwrap();

    // App calls `init` and `hook`
    let app = root.join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("main.c"),
        r#"
        extern void init(void);
        extern void hook(void);
        void run(void) {
            init();
            hook();
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("lib_a.db");
    let db_b = root.join("lib_b.db");
    let db_app = root.join("app.db");
    let db_merged = root.join("merged.db");

    for (dir, db) in [(&lib_a, &db_a), (&lib_b, &db_b), (&app, &db_app)] {
        analyze_repo(dir, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b, &db_app],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    assert!(report.ambiguous_calls >= 2);

    let conn = Connection::open(&db_merged).unwrap();

    // Check that `init` has edges to BOTH definitions in libA and libB
    let mut stmt = conn
        .prepare(
            "SELECT f.path, ce.resolution FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             JOIN files f ON f.id = callee.file_id \
             WHERE caller.name = 'run' AND callee.name = 'init' \
             ORDER BY f.path",
        )
        .unwrap();
    let init_edges: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        init_edges.len(),
        2,
        "must emit edge to both strong candidates for may-analysis: {init_edges:?}"
    );
    assert!(init_edges.iter().all(|(_, res)| res == "ambiguous"));
    assert!(init_edges.iter().any(|(p, _)| p.contains("lib_a")));
    assert!(init_edges.iter().any(|(p, _)| p.contains("lib_b")));

    // Check that `hook` has edges to BOTH weak definitions in libA and libB
    let mut stmt = conn
        .prepare(
            "SELECT f.path, callee.is_defined, ce.resolution FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             JOIN files f ON f.id = callee.file_id \
             WHERE caller.name = 'run' AND callee.name = 'hook' \
             ORDER BY f.path",
        )
        .unwrap();
    let hook_edges: Vec<(String, i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        hook_edges.len(),
        2,
        "must emit edge to both weak candidates for may-analysis: {hook_edges:?}"
    );
    assert!(hook_edges
        .iter()
        .all(|(_, is_def, res)| *is_def == 1 && res == "ambiguous"));
    assert!(hook_edges.iter().any(|(p, _, _)| p.contains("lib_a")));
    assert!(hook_edges.iter().any(|(p, _, _)| p.contains("lib_b")));
}

#[test]
fn test_array_parameter_decay_signature_matching() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Caller declares `void take_arr(int a[])`
    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("caller.c"),
        r#"
        extern void take_arr(int a[]);
        void call_it(void) {
            int nums[5] = {0};
            take_arr(nums);
        }
        "#,
    )
    .unwrap();

    // Callee defines `void take_arr(int *a)`
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("callee.c"),
        r#"
        void take_arr(int *a) {
            (void)a;
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged.db");

    for (dir, db) in [(&repo_a, &db_a), (&repo_b, &db_b)] {
        analyze_repo(dir, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    assert_eq!(report.cross_repo_calls_resolved, 1);
    assert_eq!(report.external_calls_unresolved, 0);

    let conn = Connection::open(&db_merged).unwrap();
    let (resolution, is_defined): (String, i64) = conn
        .query_row(
            "SELECT ce.resolution, callee.is_defined FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             WHERE caller.name = 'call_it' AND callee.name = 'take_arr'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(resolution, "direct");
    assert_eq!(is_defined, 1);
}

#[test]
fn test_multi_weak_summary_count_and_edge_deduplication() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    let repo_c = root.join("repo_c");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    fs::create_dir_all(&repo_c).unwrap();

    // Repo A defines a weak function `get_plugin` and calls it locally
    fs::write(
        repo_a.join("a.c"),
        r#"
        __attribute__((weak)) int get_plugin(void) {
            return 1;
        }
        int run_plugin(void) {
            return get_plugin();
        }
        "#,
    )
    .unwrap();

    // Repo B defines another weak definition of `get_plugin`
    fs::write(
        repo_b.join("b.c"),
        r#"
        __attribute__((weak)) int get_plugin(void) {
            return 2;
        }
        "#,
    )
    .unwrap();

    // Repo C defines a third weak definition of `get_plugin`
    fs::write(
        repo_c.join("c.c"),
        r#"
        __attribute__((weak)) int get_plugin(void) {
            return 3;
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_c = root.join("c.db");
    let db_merged = root.join("merged.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b), (&repo_c, &db_c)] {
        analyze_repo(repo, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b, &db_c],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    // Summary line reports ambiguous call count matching the multi-weak arm!
    assert_eq!(report.ambiguous_calls, 1);

    let conn = Connection::open(&db_merged).unwrap();

    // Verify all 3 weak definitions have ambiguous edges
    let edges: Vec<(String, String)> = conn
        .prepare(
            "SELECT f.path, ce.resolution FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             JOIN files f ON f.id = callee.file_id \
             WHERE caller.name = 'run_plugin' AND callee.name = 'get_plugin' \
             ORDER BY f.path",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(edges.len(), 3, "must emit edge to all 3 weak candidates");
    assert!(edges.iter().all(|(_, res)| res == "ambiguous"));

    // Verify no duplicate (call_site_id, callee_fn_id) edges exist in the entire database
    let dup_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ( \
                 SELECT call_site_id, callee_fn_id, COUNT(*) as cnt \
                 FROM call_edges \
                 WHERE call_site_id IS NOT NULL \
                 GROUP BY call_site_id, callee_fn_id \
                 HAVING cnt > 1 \
             )",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dup_count, 0,
        "no duplicate (call_site_id, callee_fn_id) edges allowed"
    );
}

#[test]
fn test_fallback_overloads_emit_ambiguous_and_no_duplicates() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();

    // Repo B defines multiple overloads of `query`
    fs::write(
        repo_b.join("query.cpp"),
        r#"
        int query(int a, int b) { return a + b; }
        int query(int a, int b, int c) { return a + b + c; }
        "#,
    )
    .unwrap();

    // Repo A calls an unmatching external declaration of `query` (fallback to same-name)
    fs::write(
        repo_a.join("caller.cpp"),
        r#"
        extern int query(int a);
        int invoke(int x) {
            return query(x);
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b)] {
        analyze_repo(repo, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    assert_eq!(report.ambiguous_calls, 1);

    let conn = Connection::open(&db_merged).unwrap();

    // Fallback must emit ambiguous edges to BOTH overloads in repo B,
    // rather than picking one arbitrarily with same-database/same-target preference.
    let edges: Vec<(String, String)> = conn
        .prepare(
            "SELECT callee.signature, ce.resolution FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             WHERE caller.name = 'invoke' AND callee.name = 'query' \
             ORDER BY callee.signature",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        edges.len(),
        2,
        "must emit ambiguous edge to both fallback overloads"
    );
    assert!(edges.iter().all(|(_, res)| res == "ambiguous"));

    // Verify no duplicate (call_site_id, callee_fn_id) edges
    let dup_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ( \
                 SELECT call_site_id, callee_fn_id, COUNT(*) as cnt \
                 FROM call_edges \
                 WHERE call_site_id IS NOT NULL \
                 GROUP BY call_site_id, callee_fn_id \
                 HAVING cnt > 1 \
             )",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dup_count, 0);
}

#[test]
fn test_fn_prefix_overload_resolution_not_treated_as_generic() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Repo B defines overloads of a function whose name starts with "fn_"
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("compute.cpp"),
        r#"
        int fn_compute(int a) { return a * 2; }
        int fn_compute(double a) { return (int)(a * 3.0); }
        "#,
    )
    .unwrap();

    // Repo A calls fn_compute(int)
    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("main.cpp"),
        r#"
        extern int fn_compute(int a);
        int run(int x) { return fn_compute(x); }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b)] {
        analyze_repo(repo, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    assert_eq!(report.cross_repo_calls_resolved, 1);
    assert_eq!(report.ambiguous_calls, 0);

    let conn = Connection::open(&db_merged).unwrap();
    let edges: Vec<(String, String)> = conn
        .prepare(
            "SELECT callee.signature, ce.resolution FROM call_edges ce \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             WHERE caller.name = 'run' AND callee.name = 'fn_compute'",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(edges.len(), 1, "must resolve direct to fn_compute(int)");
    assert_eq!(edges[0].0, "fn_compute(int)");
    assert_eq!(edges[0].1, "direct");
}

#[test]
fn test_indirect_call_resolution_preserves_indirect_resolution() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Repo A has an indirect call through a function pointer pointing to an external declaration
    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("main.c"),
        r#"
        extern int calculate(int x);
        int run(int (*fp)(int), int x) { return fp(x); }
        int entry(int x) { return run(calculate, x); }
        "#,
    )
    .unwrap();

    // Repo B defines calculate
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("calc.c"),
        r#"
        int calculate(int x) { return x * 10; }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b)] {
        analyze_repo(repo, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    assert_eq!(report.cross_repo_calls_resolved, 1);

    let conn = Connection::open(&db_merged).unwrap();
    let (resolution, is_direct): (String, i64) = conn
        .query_row(
            "SELECT ce.resolution, cs.is_direct FROM call_edges ce \
             JOIN call_sites cs ON cs.id = ce.call_site_id \
             JOIN functions caller ON caller.id = ce.caller_fn_id \
             JOIN functions callee ON callee.id = ce.callee_fn_id \
             WHERE caller.name = 'run' AND callee.name = 'calculate'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        resolution, "indirect",
        "must preserve original indirect resolution"
    );
    assert_eq!(is_direct, 0, "call site must remain indirect");
}

#[test]
fn test_weak_override_only_emitted_for_cross_repo_override() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Repo A has both weak and strong definitions (intra-repo weak override)
    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("a.c"),
        r#"
        __attribute__((weak)) int local_hook(void) { return 1; }
        int local_hook(void) { return 2; }
        int caller_a(void) { return local_hook(); }
        "#,
    )
    .unwrap();

    // Repo B is an unrelated repo
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("b.c"),
        r#"
        int unrelated(void) { return 42; }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged.db");

    for (repo, db) in [(&repo_a, &db_a), (&repo_b, &db_b)] {
        analyze_repo(repo, db);
    }

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged,
            verbose: true,
        },
    )
    .expect("merge should succeed");

    // Intra-repo weak overrides must NOT trigger WeakOverride warning
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.kind == WarningKind::WeakOverride),
        "intra-repo weak override must not generate cross-repo WeakOverride warning"
    );
}

#[test]
fn test_header_function_dedup_clears_is_dep_for_owner_repo() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    let include_dir = repo_b.join("include");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&include_dir).unwrap();

    fs::write(
        include_dir.join("service.h"),
        r#"
        #ifndef SERVICE_H
        #define SERVICE_H
        extern int svc_action(void);
        #endif
        "#,
    )
    .unwrap();

    // Repo A uses repo_b's include as a dependency root (--dep)
    fs::write(
        repo_a.join("consumer.c"),
        r#"
        #include "service.h"
        int use_svc(void) { return svc_action(); }
        "#,
    )
    .unwrap();

    // Repo B is the owner of service.h, analyzed directly without --dep
    fs::write(
        repo_b.join("owner.c"),
        r#"
        #include "service.h"
        int owner_init(void) { return svc_action(); }
        "#,
    )
    .unwrap();

    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    let db_merged = root.join("merged.db");

    // Analyze repo A with --dep pointing to include_dir
    let status_a = Command::new("cargo")
        .args([
            "run",
            "-q",
            "-p",
            "trace-cli",
            "--",
            "analyze",
            repo_a.to_str().unwrap(),
            "--include",
            include_dir.to_str().unwrap(),
            "--dep",
            include_dir.to_str().unwrap(),
            "-o",
            db_a.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status_a.success());

    // Analyze repo B without --dep
    let status_b = Command::new("cargo")
        .args([
            "run",
            "-q",
            "-p",
            "trace-cli",
            "--",
            "analyze",
            repo_b.to_str().unwrap(),
            "--include",
            include_dir.to_str().unwrap(),
            "-o",
            db_b.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status_b.success());

    // Merge databases: db_a first (where svc_action had is_dep = 1), then db_b (owner where is_dep = 0)
    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: true,
        },
    )
    .expect("merge should succeed");

    assert!(report.header_functions_deduped >= 1);

    let conn = Connection::open(&db_merged).unwrap();
    let is_dep: i64 = conn
        .query_row(
            "SELECT is_dep FROM functions WHERE name = 'svc_action'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        is_dep, 0,
        "deduplicated header function must have is_dep = 0 when owned by a non-dep repo"
    );
}

#[test]
fn test_cannot_overwrite_input_database() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(repo_a.join("a.c"), "void a(void) {}").unwrap();

    let db_a = root.join("a.db");

    analyze_repo(&repo_a, &db_a);

    let err = merge_databases(
        &[&db_a],
        &MergeOptions {
            output: db_a.clone(),
            verbose: false,
        },
    );

    assert!(err.is_err());
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("matches an input database"),
        "must error when output matches an input database: {msg}"
    );
}

/// #193: a member call on a class the calling repository never declares is
/// an external edge named after the class, which the merge relinks to the
/// repository defining the member.
#[test]
fn test_undeclared_receiver_member_call_relinks_to_defining_repo() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let repo_a = root.join("repo_a");
    fs::create_dir_all(&repo_a).unwrap();
    fs::write(
        repo_a.join("main.cpp"),
        r#"
        #include "message_parcel.h"
        namespace OHOS {
        int Send(MessageParcel &data, MessageParcel *reply) {
            data.WriteInt32(1);
            reply->ReadInt32();
            return 0;
        }
        }
        "#,
    )
    .unwrap();

    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_b.join("message_parcel.h"),
        r#"
        namespace OHOS {
        class MessageParcel {
        public:
            bool WriteInt32(int value);
            int ReadInt32();
        };
        }
        "#,
    )
    .unwrap();
    fs::write(
        repo_b.join("message_parcel.cpp"),
        r#"
        #include "message_parcel.h"
        namespace OHOS {
        bool MessageParcel::WriteInt32(int value) { return value != 0; }
        int MessageParcel::ReadInt32() { return 7; }
        }
        "#,
    )
    .unwrap();

    let db_a = root.join("repo_a.db");
    let db_b = root.join("repo_b.db");
    let db_merged = root.join("unified.db");
    analyze_repo(&repo_a, &db_a);
    analyze_repo(&repo_b, &db_b);

    // `OHOS::Send`'s edges as (callee, resolution, callee defined).
    let edges = |db: &std::path::Path| -> Vec<(String, String, i64)> {
        let conn = Connection::open(db).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT callee.name, ce.resolution, callee.is_defined FROM call_edges ce \
                 JOIN functions caller ON caller.id = ce.caller_fn_id \
                 JOIN functions callee ON callee.id = ce.callee_fn_id \
                 WHERE caller.name = 'OHOS::Send' ORDER BY callee.name",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    let row = |callee: &str, resolution: &str, defined: i64| {
        (callee.to_string(), resolution.to_string(), defined)
    };

    assert_eq!(
        edges(&db_a),
        [
            row("OHOS::MessageParcel::ReadInt32", "external", 0),
            row("OHOS::MessageParcel::WriteInt32", "external", 0),
        ]
    );

    let report = merge_databases(
        &[&db_a, &db_b],
        &MergeOptions {
            output: db_merged.clone(),
            verbose: false,
        },
    )
    .expect("merge failed");
    assert_eq!(report.cross_repo_calls_resolved, 2);
    assert_eq!(report.external_calls_unresolved, 0);

    assert_eq!(
        edges(&db_merged),
        [
            row("OHOS::MessageParcel::ReadInt32", "direct", 1),
            row("OHOS::MessageParcel::WriteInt32", "direct", 1),
        ]
    );
}
