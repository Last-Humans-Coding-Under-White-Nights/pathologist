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
        // Let cargo decide whether `trace` is fresh and where it lives: the
        // compiler-artifact message carries the executable path, so this works
        // for any target dir, profile or `--target` layout.
        let release = std::path::Path::new(env!("CARGO_BIN_EXE_trace-merge"))
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == "release");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.args([
            "build",
            "-q",
            "-p",
            "trace-cli",
            "--bin",
            "trace",
            "--message-format=json",
        ]);
        if release {
            cmd.arg("--release");
        }
        let output = cmd.output().expect("failed to run cargo build");
        assert!(
            output.status.success(),
            "failed to build trace-cli:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|msg| msg["reason"] == "compiler-artifact" && msg["target"]["name"] == "trace")
            .and_then(|msg| msg["executable"].as_str().map(std::path::PathBuf::from))
            .expect("cargo reported no `trace` executable")
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
fn merge_keeps_unresolved_internal_calls_file_local() {
    let tmp = tempdir().unwrap();
    let mut inputs = Vec::new();
    for (name, source) in [
        (
            "caller",
            "static int hidden(void); int run(void) { return hidden(); }",
        ),
        ("provider", "int hidden(void) { return 1; }"),
    ] {
        let repo = tmp.path().join(name);
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join("code.c"), source).unwrap();
        let db = tmp.path().join(format!("{name}.db"));
        analyze_repo(&repo, &db);
        inputs.push(db);
    }
    let output = tmp.path().join("merged.db");
    let report = merge_databases(
        &inputs,
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap();
    let conn = Connection::open(output).unwrap();
    let callee: (String, i64) = conn.query_row(
        "SELECT f.linkage, f.is_defined FROM call_edges e JOIN functions f ON f.id=e.callee_fn_id
         JOIN functions caller ON caller.id=e.caller_fn_id WHERE caller.name='run'",
        [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(callee, ("internal".into(), 0));
    assert_eq!(report.cross_repo_calls_resolved, 0);
}

/// Analyze a single-source executable with build metadata and a shared include.
fn analyze_executable(
    repo: &std::path::Path,
    include: &std::path::Path,
    db: &std::path::Path,
    dep: bool,
) {
    fs::write(
        repo.join("compile_commands.json"),
        serde_json::json!([
            {"directory": repo, "file": "main.cpp", "output": "main.o",
             "arguments": ["c++", "-I", include, "-c", "main.cpp", "-o", "main.o"]}
        ])
        .to_string(),
    )
    .unwrap();
    fs::write(
        repo.join("link_commands.json"),
        serde_json::json!([
            {"directory": repo, "output": "app", "arguments": ["c++", "main.o", "-o", "app"]}
        ])
        .to_string(),
    )
    .unwrap();
    let mut command = Command::new(trace_cli_bin());
    command.arg("analyze").arg(repo).arg("-o").arg(db);
    if dep {
        command.arg("--dep").arg(include);
    }
    assert!(command.status().unwrap().success());
}

#[test]
fn shared_header_images_do_not_collide_or_make_external_calls_ambiguous() {
    let tmp = tempdir().unwrap();
    let include = tmp.path().join("include");
    fs::create_dir(&include).unwrap();
    fs::write(
        include.join("common.h"),
        "int local(); inline int common() { return local(); }\n",
    )
    .unwrap();
    let mut inputs = Vec::new();
    for name in ["one", "two"] {
        let repo = tmp.path().join(name);
        fs::create_dir(&repo).unwrap();
        fs::write(
            repo.join("main.cpp"),
            "#include \"common.h\"\nint local() { return 1; } int main() { return common(); }\n",
        )
        .unwrap();
        let db = tmp.path().join(format!("{name}.db"));
        analyze_executable(&repo, &include, &db, false);
        inputs.push(db);
    }
    let caller = tmp.path().join("caller");
    fs::create_dir(&caller).unwrap();
    fs::write(
        caller.join("caller.cpp"),
        "int common(); int common(char *);
         int run() { return common(); }
         int fallback(char *p) { return common(p); }
         int callback(int (*fn)()) { return fn(); }
         int start() { return callback(common); }\n",
    )
    .unwrap();
    let caller_db = tmp.path().join("caller.db");
    analyze_repo(&caller, &caller_db);
    inputs.push(caller_db);
    let output = tmp.path().join("merged.db");
    for reverse in [false, true] {
        if reverse {
            inputs.reverse();
        }
        let report = merge_databases(
            &inputs,
            &MergeOptions {
                output: output.clone(),
                verbose: false,
            },
        )
        .unwrap();
        let collisions: Vec<_> = report
            .warnings
            .iter()
            .filter(|w| w.kind == WarningKind::MultipleDefinitions)
            .map(|w| w.symbol.as_deref().unwrap())
            .collect();
        assert_eq!(
            collisions,
            ["local", "main"],
            "shared source copies are not collisions"
        );
        assert_eq!(report.ambiguous_calls, 0);
        assert_eq!(report.cross_repo_calls_resolved, 3);
        let conn = Connection::open(&output).unwrap();
        let common: (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT target_id) FROM functions WHERE name='common' AND is_defined=1",
            [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(common, (2, 2), "retain both image-owned definitions");
        // An external caller must reach both copies and their own local callees.
        let paths: (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT local.file_id) FROM call_edges start
             JOIN functions caller ON caller.id=start.caller_fn_id
             JOIN functions common ON common.id=start.callee_fn_id
             JOIN call_edges body ON body.caller_fn_id=common.id
             JOIN functions local ON local.id=body.callee_fn_id
             WHERE caller.name IN ('run', 'fallback') AND common.name='common' AND local.name='local'
             AND start.resolution='direct' AND common.target_id=local.target_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(paths, (4, 2));
        let indirect: i64 = conn.query_row(
            "SELECT COUNT(*) FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id
             JOIN functions callee ON callee.id=e.callee_fn_id
             WHERE caller.name='callback' AND callee.name='common' AND e.resolution='indirect'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(
            indirect, 2,
            "shared-origin callback copies preserve indirect resolution"
        );
        let false_diagnostics: i64 = conn.query_row(
            "SELECT COUNT(*) FROM diagnostics WHERE stage='merge' AND message LIKE '%common()%'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(false_diagnostics, 0);
    }
}

#[test]
fn shared_header_unresolved_callgraph_matches_joint_analysis() {
    let tmp = tempdir().unwrap();
    let sources = tmp.path().join("sources");
    fs::create_dir(&sources).unwrap();
    let header = sources.join("h.h");
    fs::write(
        &header,
        "typedef void(*fp)(void);\nextern fp g_cb;\nvoid ext(void);\n\
         inline void shared_fn(){ g_cb(); ext(); }\n",
    )
    .unwrap();
    let mut inputs = Vec::new();
    for name in ["one", "two"] {
        let repo = sources.join(name);
        fs::create_dir(&repo).unwrap();
        fs::write(
            repo.join("main.cpp"),
            format!(
                "#include \"{}\"\nvoid {name}() {{ shared_fn(); }}\n",
                header.display()
            ),
        )
        .unwrap();
        let db = tmp.path().join(format!("{name}.db"));
        analyze_repo(&repo, &db);
        inputs.push(db);
    }
    let joint = tmp.path().join("joint.db");
    analyze_repo(&sources, &joint);
    let merged = tmp.path().join("merged.db");
    merge_databases(
        &inputs,
        &MergeOptions {
            output: merged.clone(),
            verbose: false,
        },
    )
    .unwrap();

    let mut graphs = Vec::new();
    for (db, copies) in [(&joint, 1), (&merged, 2)] {
        let conn = Connection::open(db).unwrap();
        let caller: i64 = conn
            .query_row(
                "SELECT id FROM functions WHERE name = 'shared_fn' AND is_defined = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let sites: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM call_sites WHERE caller_fn_id = ?1 AND callee_text = 'g_cb'",
                [caller],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sites, copies, "the merged export retains each input's site");
        let graph = trace_db::call_graph(&conn, caller, trace_db::Direction::Down, 2).unwrap();
        let mut edges: Vec<_> = graph
            .edges
            .iter()
            .map(|edge| {
                (
                    graph.nodes[&edge.to].label.clone(),
                    edge.label.clone(),
                    edge.site.path.clone(),
                    edge.site.line,
                    edge.site.col,
                )
            })
            .collect();
        edges.sort();
        assert_eq!(
            edges.len(),
            2,
            "one external edge and one unresolved leaf: {edges:?}"
        );
        assert_eq!(edges[0].0, "ext");
        assert_eq!(edges[0].1, "external");
        assert_eq!(edges[1].0, "g_cb");
        assert_eq!(edges[1].1, "unresolved");
        graphs.push(edges);
    }
    assert_eq!(graphs[0], graphs[1]);
}

#[test]
fn shared_header_prototypes_deduplicate_across_images_and_keep_owner_visibility() {
    let tmp = tempdir().unwrap();
    let include = tmp.path().join("include");
    fs::create_dir(&include).unwrap();
    fs::write(
        include.join("service.h"),
        "int service(); inline int helper() { return 1; }\n",
    )
    .unwrap();
    let mut inputs = Vec::new();
    for (name, dep) in [("consumer", true), ("owner", false)] {
        let repo = tmp.path().join(name);
        fs::create_dir(&repo).unwrap();
        fs::write(
            repo.join("main.cpp"),
            "#include \"service.h\"\nint main() { return service() + helper(); }\n",
        )
        .unwrap();
        let db = tmp.path().join(format!("{name}.db"));
        analyze_executable(&repo, &include, &db, dep);
        let conn = Connection::open(&db).unwrap();
        let prototype: (i64, i64) = conn
            .query_row(
                "SELECT is_dep, target_id IS NOT NULL FROM functions WHERE name='service'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(prototype, (i64::from(dep), 1));
        inputs.push(db);
    }
    let output = tmp.path().join("merged.db");
    for reverse in [false, true] {
        if reverse {
            inputs.reverse();
        }
        let report = merge_databases(
            &inputs,
            &MergeOptions {
                output: output.clone(),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(report.header_functions_deduped, 1);
        let conn = Connection::open(&output).unwrap();
        let prototypes: (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), SUM(is_dep), SUM(target_id IS NOT NULL) FROM functions WHERE name='service'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(
            prototypes,
            (1, 0, 0),
            "a shared prototype belongs to no single image"
        );
        let helpers: (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(is_dep) FROM functions WHERE name='helper'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            helpers,
            (2, 0),
            "owner visibility reaches the dependency's declaration and the owner's body"
        );
        let visible_calls: i64 = conn.query_row(
            "SELECT COUNT(*) FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id
             JOIN functions callee ON callee.id=e.callee_fn_id
             WHERE callee.name='service' AND caller.is_dep=0 AND callee.is_dep=0",
            [], |r| r.get(0)).unwrap();
        assert_eq!(visible_calls, 2);
    }
}

#[test]
fn merge_preserves_functions_in_distinct_link_targets() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    fs::write(
        repo.join("common.c"),
        "__attribute__((weak)) int leaf(void) { return 1; } int run(void) { return leaf(); }",
    )
    .unwrap();
    fs::write(
        repo.join("compile_commands.json"),
        serde_json::json!([
            {"directory": repo, "file": "common.c", "output": "common.o",
             "arguments": ["cc", "-c", "common.c", "-o", "common.o"]}
        ])
        .to_string(),
    )
    .unwrap();
    fs::write(
        repo.join("link_commands.json"),
        serde_json::json!([
            {"directory": repo, "output": "one", "arguments": ["cc", "common.o", "-o", "one"]},
            {"directory": repo, "output": "two", "arguments": ["cc", "common.o", "-o", "two"]}
        ])
        .to_string(),
    )
    .unwrap();
    let input = tmp.path().join("input.db");
    analyze_repo(&repo, &input);
    let graph = |path: &std::path::Path| {
        let conn = Connection::open(path).unwrap();
        let rows = conn.prepare("SELECT caller.name, ct.name, callee.name, dt.name FROM call_edges e
            JOIN functions caller ON caller.id=e.caller_fn_id JOIN link_targets ct ON ct.id=caller.target_id
            JOIN functions callee ON callee.id=e.callee_fn_id JOIN link_targets dt ON dt.id=callee.target_id
            ORDER BY ct.name, caller.name, dt.name, callee.name").unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        rows
    };
    let before = graph(&input);
    assert_eq!(before.len(), 2);
    let provider = tmp.path().join("provider");
    fs::create_dir(&provider).unwrap();
    fs::write(provider.join("leaf.c"), "int leaf(void) { return 2; }").unwrap();
    let provider_db = tmp.path().join("provider.db");
    analyze_repo(&provider, &provider_db);
    let output = tmp.path().join("merged.db");
    for inputs in [
        vec![&input],
        vec![&input, &provider_db],
        vec![&provider_db, &input],
    ] {
        let report = merge_databases(
            &inputs,
            &MergeOptions {
                output: output.clone(),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(
            graph(&output),
            before,
            "merging must retain each link image's resolved weak functions"
        );
        assert!(!report
            .warnings
            .iter()
            .any(|w| w.kind == WarningKind::WeakOverride));
    }
}

#[test]
fn merge_keeps_all_exact_candidates_in_the_callers_target() {
    let tmp = tempdir().unwrap();
    let input = tmp.path().join("input.db");
    empty_merge_input(&input);
    // Exercise an unresolved edge directly: the ordinary analyzer already
    // resolves these definitions and does not leave that edge for the merger.
    Connection::open(&input).unwrap().execute_batch(
        "INSERT INTO files VALUES (1, '/source/code.cpp', '', 0);
         INSERT INTO link_targets VALUES (1, 'app', 'app'), (2, 'other', 'other');
         INSERT INTO functions (id,name,file_id,line_start,line_end,linkage,signature,is_defined,target_id) VALUES
           (1, 'run', 1, 1, 1, 'external', 'run()', 1, 1),
           (2, 'query', 1, 2, 2, 'external', 'query()', 0, 1),
           (3, 'query', 1, 3, 3, 'external', 'query()', 1, 1),
           (4, 'query', 1, 4, 4, 'external', 'query()', 1, 1),
           (5, 'query', 1, 5, 5, 'external', 'query()', 1, 2);
         INSERT INTO call_sites (id,caller_fn_id,file_id,line,col,callee_text,is_direct)
           VALUES (1,1,1,1,1,'query',1);
         INSERT INTO call_edges VALUES (1,1,1,2,'external');"
    ).unwrap();
    let output = tmp.path().join("merged.db");
    let report = merge_databases(
        &[input],
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap();
    assert_eq!(report.ambiguous_calls, 1);
    let conn = Connection::open(output).unwrap();
    let rows = conn
        .prepare(
            "SELECT f.line_start, e.resolution FROM call_edges e
        JOIN functions f ON f.id=e.callee_fn_id ORDER BY f.line_start",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows, [(3, "ambiguous".into()), (4, "ambiguous".into())]);
}

#[test]
fn merge_fallback_does_not_discard_a_different_weak_overload() {
    let tmp = tempdir().unwrap();
    let mut inputs = Vec::new();
    for (name, source) in [
        ("caller", "int query(char *); int run(char *p) { return query(p); }"),
        ("provider", "int query(int x) { return x; } __attribute__((weak)) int query(double x) { return 2; }"),
    ] {
        let repo = tmp.path().join(name);
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join("code.cpp"), source).unwrap();
        let db = tmp.path().join(format!("{name}.db"));
        analyze_repo(&repo, &db);
        inputs.push(db);
    }
    let output = tmp.path().join("merged.db");
    merge_databases(
        &inputs,
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap();
    let conn = Connection::open(output).unwrap();
    let callees = conn
        .prepare(
            "SELECT f.signature, e.resolution FROM call_edges e
        JOIN functions f ON f.id=e.callee_fn_id JOIN functions caller ON caller.id=e.caller_fn_id
        WHERE caller.name='run' ORDER BY f.signature",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        callees,
        [
            ("query(double)".into(), "ambiguous".into()),
            ("query(int)".into(), "ambiguous".into())
        ]
    );
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

fn empty_merge_input(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(trace_db::TABLES_V7).unwrap();
    conn.execute(
        "INSERT INTO analysis_run (trace_version, schema_version, target_root, created_at, options_json) \
         VALUES ('test', ?1, '/source', '0', '{}')",
        [trace_db::SCHEMA_VERSION],
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn merged_database_permissions_follow_umask() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let input = dir.path().join("input.db");
    empty_merge_input(&input);

    for (mask, expected) in [
        ("022", 0o644),
        ("077", 0o600),
        ("002", 0o644),
        ("000", 0o644),
    ] {
        let output = dir.path().join(format!("merged-{mask}.db"));
        // Change the umask only in a child process; other tests run concurrently.
        let result = Command::new("sh")
            .args([
                "-c",
                "umask \"$1\" && exec \"$2\" \"$3\" -o \"$4\"",
                "trace-merge-umask",
                mask,
            ])
            .arg(trace_bin())
            .arg(&input)
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "merge with umask {mask} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            expected,
            "published database permissions with umask {mask}"
        );
    }
}

#[test]
fn merge_staging_does_not_overwrite_an_input_and_replaces_existing_output() {
    let dir = tempdir().unwrap();
    let output = dir.path().join("unified.db");
    // This input occupied the old fixed staging filename.
    let input = output.with_extension("db.tmp");
    empty_merge_input(&input);
    let before = fs::read(&input).unwrap();
    fs::write(&output, "previous output").unwrap();

    merge_databases(
        &[&input],
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap();

    assert_eq!(
        fs::read(&input).unwrap(),
        before,
        "input must remain intact"
    );
    let conn = Connection::open(&output).unwrap();
    let stage: String = conn
        .query_row(
            "SELECT json_extract(options_json, '$.stage') FROM analysis_run",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stage, "merge");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn failed_merge_publication_keeps_destination_and_cleans_staging() {
    let dir = tempdir().unwrap();
    let input = dir.path().join("input.db");
    empty_merge_input(&input);
    let before = fs::read(&input).unwrap();
    let output = dir.path().join("destination");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("keep"), "existing data").unwrap();

    let error = merge_databases(
        &[&input],
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap_err();

    assert!(error.to_string().contains("failed to publish"));
    assert_eq!(fs::read(&input).unwrap(), before);
    assert_eq!(
        fs::read_to_string(output.join("keep")).unwrap(),
        "existing data"
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
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

#[test]
fn schema_compatibility_checks_versions_and_required_v7_and_v8_structures() {
    let tmp = tempdir().unwrap();
    let old = tmp.path().join("old-v6.db");
    let conn = Connection::open(&old).unwrap();
    // The old version predates source-dataflow columns and tables.
    conn.execute_batch(
        "CREATE TABLE analysis_run(id INTEGER PRIMARY KEY, schema_version INTEGER);
        INSERT INTO analysis_run VALUES(1,6);
        CREATE TABLE variables(id INTEGER PRIMARY KEY, name TEXT);
        CREATE TABLE flow_nodes(id INTEGER PRIMARY KEY, kind TEXT, label TEXT);",
    )
    .unwrap();
    drop(conn);
    let output = tmp.path().join("merged.db");
    let options = MergeOptions {
        output: output.clone(),
        verbose: false,
    };
    let error = merge_databases(&[&old], &options).unwrap_err().to_string();
    assert!(
        error.contains("database has version 6, expected 7"),
        "{error}"
    );
    assert!(error.contains("re-analyze"), "{error}");
    assert!(!output.exists());
    fs::write(&output, b"existing output").unwrap();
    assert!(merge_databases(&[&old], &options).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"existing output");

    let mut current = Vec::new();
    for i in 0..2 {
        let path = tmp.path().join(format!("current-{i}.db"));
        let conn = Connection::open(&path).unwrap();
        // Reconstruct an earlier v7 layout without additive flow metadata
        // or call-site variable bindings, alongside the current schema.
        let schema = if i == 0 {
            trace_db::TABLES_V7.replace(
                "is_direct INTEGER NOT NULL,\n    callee_var INTEGER,\n    return_dst INTEGER",
                "is_direct INTEGER NOT NULL",
            )
        } else {
            trace_db::SCHEMA_V8.to_owned()
        };
        conn.execute_batch(&schema).unwrap();
        if i == 0 {
            conn.execute_batch(
                "DROP TABLE flow_call_origins; DROP TABLE flow_call_expressions;
                 DROP TABLE flow_parameters; DROP TABLE flow_field_locations;
                 DROP TABLE flow_field_access; DROP TABLE flow_origins;
                 DROP TABLE flow_return_calls; DROP TABLE flow_calls;",
            )
            .unwrap();
            assert!(trace_db::require_source_dataflow_metadata(&conn).is_err());
            assert!(conn.prepare("SELECT callee_var FROM call_sites").is_err());
        }
        conn.execute(
            "INSERT INTO analysis_run VALUES(1,'test',?1,'/src','now','{}')",
            [if i == 0 { 7 } else { trace_db::SCHEMA_VERSION }],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files VALUES(1,?1,'hash',0)",
            [format!("/src/{i}.c")],
        )
        .unwrap();
        conn.execute("INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES(1,?1,1,1,2,'external',?1,1)", [format!("f{i}")]).unwrap();
        drop(conn);
        current.push(path);
    }
    // Every input is checked, including an old database following a valid one.
    assert!(merge_databases(&[&current[0], &old], &options).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"existing output");
    let report = merge_databases(&current, &options).unwrap();
    assert_eq!(report.functions_defined, 2);
    let merged = Connection::open(&output).unwrap();
    let version: i64 = merged
        .query_row("SELECT schema_version FROM analysis_run", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, trace_db::SCHEMA_VERSION);
    let count: i64 = merged
        .query_row("SELECT COUNT(*) FROM functions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
    drop(merged);

    // Supported v7 and v8 inputs must both expose the required logical columns.
    for (index, table) in [(0, "call_sites"), (1, "call_sites_data")] {
        let broken = Connection::open(&current[index]).unwrap();
        broken
            .execute_batch(&format!(
                "ALTER TABLE {table} RENAME COLUMN expansion_col TO unavailable_col"
            ))
            .unwrap();
        drop(broken);
        fs::write(&output, b"existing output").unwrap();
        let error = merge_databases(&current, &options).unwrap_err().to_string();
        assert!(
            error.contains("missing trace-merge capability"),
            "{table}: {error}"
        );
        assert!(
            error.contains("call_sites") && error.contains("expansion_col"),
            "{table}: {error}"
        );
        assert!(error.contains("re-analyze"), "{error}");
        assert_eq!(fs::read(&output).unwrap(), b"existing output");
        // Restore this input so the next iteration checks the other version.
        let restored = Connection::open(&current[index]).unwrap();
        restored
            .execute_batch(&format!(
                "ALTER TABLE {table} RENAME COLUMN unavailable_col TO expansion_col"
            ))
            .unwrap();
        restored
            .prepare("SELECT expansion_col FROM call_sites LIMIT 0")
            .unwrap();
    }
}

#[test]
fn merged_export_reports_dataflow_limitation_before_symbol_lookup() {
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    fs::write(
        repo.join("flow.c"),
        "int *identity(int *p) { return p; }\n\
         void relay(int *source) {\n\
             int *dest = identity(source);\n\
         }\n\
         int untouched;\n",
    )
    .unwrap();
    let original = tmp.path().join("original.db");
    analyze_repo(&repo, &original);
    let conn = Connection::open(&original).unwrap();
    trace_db::require_source_dataflow_metadata(&conn).unwrap();
    let origins: i64 = conn
        .query_row("SELECT COUNT(*) FROM flow_origins", [], |r| r.get(0))
        .unwrap();
    assert!(origins > 0);
    let bindings: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM call_sites WHERE return_dst IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(bindings > 0, "fresh export has return destination bindings");
    drop(conn);

    let inspect = |db: &std::path::Path, line: &str, col: &str| {
        Command::new(trace_cli_bin())
            .args([
                "inspect",
                db.to_str().unwrap(),
                "dataflow",
                "--file",
                "flow.c",
                "--line",
                line,
                "--col",
                col,
                "--direction",
                "down",
                "--depth",
                "2",
                "--format",
                "json",
            ])
            .output()
            .unwrap()
    };
    let supported = inspect(&original, "2", "17");
    assert!(
        supported.status.success(),
        "{}",
        String::from_utf8_lossy(&supported.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&supported.stdout).unwrap();
    assert_eq!(json["schema"], "dataflow-source-v1");
    assert_eq!(json["direction"], "down");

    let flowless = inspect(&original, "5", "5");
    assert!(!flowless.status.success());
    let error = String::from_utf8_lossy(&flowless.stderr);
    assert!(
        error.contains("untouched") && error.contains("no value-flow node"),
        "{error}"
    );
    assert!(!error.contains("trace-merge"), "{error}");

    let output = tmp.path().join("merged.db");
    merge_databases(
        &[&original],
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap();
    let merged = Connection::open(&output).unwrap();
    for table in [
        "variables",
        "arg_flow_edges",
        "flow_nodes",
        "flow_edges",
        "flow_origins",
    ] {
        let count: i64 = merged
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "{table} is intentionally not merged");
    }
    let bindings: i64 = merged.query_row("SELECT COUNT(*) FROM call_sites WHERE callee_var IS NOT NULL OR return_dst IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(
        bindings, 0,
        "input variable IDs must not leak into merged output"
    );
    drop(merged);
    let unavailable = inspect(&output, "2", "17");
    assert!(!unavailable.status.success());
    let error = String::from_utf8_lossy(&unavailable.stderr);
    for expected in [
        "trace-merge output",
        "PAG flow graphs or provenance",
        "original trace analyze database",
        "combined source tree",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    assert!(!error.contains("no symbols"), "{error}");
}

#[test]
fn execution_contexts_carry_over_with_remapped_ids() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_a.join("a.c"),
        r#"
        void *Worker(void *arg) { return arg; }
        void StartMany(void *ctx, int n) {
            unsigned long tid;
            while (n--) { pthread_create(&tid, 0, Worker, ctx); }
        }
        "#,
    )
    .unwrap();
    fs::write(
        repo_b.join("b.c"),
        r#"
        void *Other(void *arg) { return arg; }
        void StartOnce(void *ctx) {
            unsigned long tid;
            pthread_create(&tid, 0, Other, ctx);
        }
        "#,
    )
    .unwrap();
    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    analyze_repo(&repo_a, &db_a);
    analyze_repo(&repo_b, &db_b);
    let merged = root.join("merged.db");
    merge_databases(
        &[db_a, db_b],
        &MergeOptions {
            output: merged.clone(),
            verbose: false,
        },
    )
    .unwrap();

    let conn = Connection::open(&merged).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.kind || ' ' || f.name || ' ' || c.name || ' ' || s.callee_text || ' ' || \
                    a.name || ' ' || e.param_index || ' ' || e.multi_instance || ' ' || \
                    e.self_concurrent \
             FROM execution_contexts e \
             JOIN functions f ON f.id = e.entry_fn_id \
             JOIN call_sites s ON s.id = e.call_site_id \
             JOIN functions c ON c.id = s.caller_fn_id \
             JOIN functions a ON a.id = e.api_fn_id \
             ORDER BY e.id",
        )
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        [
            "thread Worker StartMany pthread_create pthread_create 2 loop 1",
            "thread Other StartOnce pthread_create pthread_create 2 unknown 0",
        ]
    );
}

/// A callback one repository only declares and another defines: the start
/// edge is re-linked to the definition, and so is the context it starts,
/// whose entry would otherwise stay the declaration, with no body behind
/// it.
#[test]
fn a_context_started_at_a_declared_callback_enters_its_definition() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_a.join("a.c"),
        r#"
        void *Worker(void *arg);
        void Start(void *ctx) {
            unsigned long tid;
            pthread_create(&tid, 0, Worker, ctx);
        }
        "#,
    )
    .unwrap();
    fs::write(
        repo_b.join("b.c"),
        r#"
        void Step(void *arg) {}
        void *Worker(void *arg) { Step(arg); return arg; }
        "#,
    )
    .unwrap();
    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    analyze_repo(&repo_a, &db_a);
    analyze_repo(&repo_b, &db_b);
    let merged = root.join("merged.db");
    merge_databases(
        &[db_a, db_b],
        &MergeOptions {
            output: merged.clone(),
            verbose: false,
        },
    )
    .unwrap();

    let conn = Connection::open(&merged).unwrap();
    let rows: Vec<(String, i64, i64)> = conn
        .prepare(
            "SELECT f.name, f.is_defined, \
                    (SELECT COUNT(*) FROM call_edges ce \
                     WHERE ce.call_site_id = e.call_site_id AND ce.callee_fn_id = e.entry_fn_id) \
             FROM execution_contexts e JOIN functions f ON f.id = e.entry_fn_id ORDER BY e.id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        [("Worker".to_string(), 1, 1)],
        "the context enters the definition the start edge was re-linked to"
    );
}

/// Merged rows keep the order `trace analyze` writes: every submitted
/// context by call site, then the IPC handlers, whichever input they come
/// from.
#[test]
fn merged_execution_contexts_list_ipc_handlers_last() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_a.join("a.cpp"),
        r#"
        class IRemoteObject {
        public:
            int SendRequest(int code, void *data, void *reply, void *option);
        };
        IRemoteObject *Remote();
        class IFooStub {
        public:
            int HandleGetInfo(int key) { return key; }
        };
        class IFooProxy {
        public:
            int GetInfo(int key);
        };
        int IFooProxy::GetInfo(int key) {
            Remote()->SendRequest(1, 0, 0, 0);
            return key;
        }
        void *Worker(void *arg) { return arg; }
        void StartA(void *ctx) {
            unsigned long tid;
            pthread_create(&tid, 0, Worker, ctx);
        }
        "#,
    )
    .unwrap();
    fs::write(
        repo_b.join("b.c"),
        r#"
        void *Other(void *arg) { return arg; }
        void StartB(void *ctx) {
            unsigned long tid;
            pthread_create(&tid, 0, Other, ctx);
        }
        "#,
    )
    .unwrap();
    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    analyze_repo(&repo_a, &db_a);
    analyze_repo(&repo_b, &db_b);
    let merged = root.join("merged.db");
    merge_databases(
        &[db_a, db_b],
        &MergeOptions {
            output: merged.clone(),
            verbose: false,
        },
    )
    .unwrap();

    let conn = Connection::open(&merged).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.kind || ' ' || f.name FROM execution_contexts e \
             JOIN functions f ON f.id = e.entry_fn_id ORDER BY e.id",
        )
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        [
            "thread Worker",
            "thread Other",
            "ipc_handler IFooStub::HandleGetInfo",
        ]
    );
}

/// `execution_contexts` is an additive v7 table: an input exported before it
/// existed still merges, contributes no rows and is named in a merge-stage
/// diagnostic; an input that has the table must have every column the
/// merger reads.
#[test]
fn execution_contexts_are_optional_in_v7_inputs_and_validated_when_present() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("a.c"),
        r#"
        void *Worker(void *arg) { return arg; }
        void Start(void *ctx) {
            unsigned long tid;
            pthread_create(&tid, 0, Worker, ctx);
        }
        "#,
    )
    .unwrap();
    let current = root.join("current.db");
    analyze_repo(&repo, &current);
    let earlier = root.join("earlier.db");
    empty_merge_input(&earlier);
    Connection::open(&earlier)
        .unwrap()
        .execute_batch("DROP TABLE execution_contexts")
        .unwrap();

    let output = root.join("merged.db");
    let options = MergeOptions {
        output: output.clone(),
        verbose: false,
    };
    let report = merge_databases(&[&current, &earlier], &options).unwrap();
    let missing: Vec<_> = report
        .warnings
        .iter()
        .filter(|w| w.kind == WarningKind::MissingExecutionContexts)
        .collect();
    assert_eq!(missing.len(), 1, "{:?}", report.warnings);
    assert!(
        missing[0].message.contains("earlier.db") && missing[0].message.contains("re-analyze"),
        "{}",
        missing[0].message
    );
    let merged = Connection::open(&output).unwrap();
    let stage: String = merged
        .query_row(
            "SELECT json_extract(options_json, '$.stage') FROM analysis_run",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stage, "merge");
    let rows: Vec<String> = merged
        .prepare(
            "SELECT e.kind || ' ' || f.name FROM execution_contexts e \
             JOIN functions f ON f.id = e.entry_fn_id ORDER BY e.id",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        ["thread Worker"],
        "the current input's rows carry over"
    );
    let diagnostics: i64 = merged
        .query_row(
            "SELECT COUNT(*) FROM diagnostics WHERE stage = 'merge' \
             AND message LIKE '%execution_contexts%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(diagnostics, 1);
    drop(merged);

    // A nominal v7 input whose table lacks a column the merger reads is
    // rejected before any output is replaced.
    Connection::open(&current)
        .unwrap()
        .execute_batch("ALTER TABLE execution_contexts RENAME COLUMN multi_instance TO other")
        .unwrap();
    fs::write(&output, b"existing output").unwrap();
    let error = merge_databases(&[&current], &options)
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing trace-merge capability"), "{error}");
    assert!(
        error.contains("execution_contexts") && error.contains("multi_instance"),
        "{error}"
    );
    assert!(error.contains("re-analyze"), "{error}");
    assert_eq!(fs::read(&output).unwrap(), b"existing output");
}

/// The model naming a context is carried over, an override entry's too; the
/// receiver variable is not, as no variable is.
#[test]
fn merged_execution_contexts_keep_their_model() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let repo_a = root.join("repo_a");
    let repo_b = root.join("repo_b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(
        repo_a.join("a.cpp"),
        r#"
        namespace OHOS {
        void Job() {}
        void Post(AppExecFwk::EventHandler *handler) { handler->PostTask(Job); }
        }
        "#,
    )
    .unwrap();
    fs::write(
        repo_b.join("b.cpp"),
        r#"
        namespace OHOS {
        class Recipient : public IRemoteObject::DeathRecipient {
        public:
            void OnRemoteDied(const wptr<IRemoteObject> &remote) override {}
        };
        }
        "#,
    )
    .unwrap();
    let db_a = root.join("a.db");
    let db_b = root.join("b.db");
    analyze_repo(&repo_a, &db_a);
    analyze_repo(&repo_b, &db_b);
    let merged = root.join("merged.db");
    merge_databases(
        &[db_a, db_b],
        &MergeOptions {
            output: merged.clone(),
            verbose: false,
        },
    )
    .unwrap();

    let conn = Connection::open(&merged).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT e.kind || ' ' || f.name || ' ' || e.model || ' ' || \
                    ifnull(e.receiver_var_id, 'NULL') \
             FROM execution_contexts e \
             JOIN functions f ON f.id = e.entry_fn_id ORDER BY e.id",
        )
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        [
            "serial_task OHOS::Job OHOS::AppExecFwk::EventHandler::PostTask NULL",
            "ipc_handler OHOS::Recipient::OnRemoteDied \
             OHOS::IRemoteObject::DeathRecipient::OnRemoteDied NULL",
        ]
    );
}

/// `trace analyze repo -o db ARGS…` with the workspace's own CLI.
fn analyze_repo_with(repo: &std::path::Path, db: &std::path::Path, args: &[&std::ffi::OsStr]) {
    let status = Command::new(trace_cli_bin())
        .arg("analyze")
        .arg(repo)
        .arg("-o")
        .arg(db)
        .args(args)
        .status()
        .expect("failed to run trace analyze");
    assert!(status.success(), "trace analyze {}", repo.display());
}

/// One repository per `(name, file, source, sequenced)`, analysed into
/// `<name>.db`, `--sequencing` when asked.
fn sequencing_inputs(
    root: &std::path::Path,
    repos: &[(&str, &str, &str, bool)],
) -> Vec<std::path::PathBuf> {
    repos
        .iter()
        .map(|&(name, file, source, sequenced)| {
            let repo = root.join(name);
            fs::create_dir_all(&repo).unwrap();
            fs::write(repo.join(file), source).unwrap();
            let db = root.join(format!("{name}.db"));
            let flag = [std::ffi::OsStr::new("--sequencing")];
            analyze_repo_with(&repo, &db, if sequenced { &flag } else { &[] });
            db
        })
        .collect()
}

fn merge_into(inputs: &[std::path::PathBuf], output: &std::path::Path) -> trace_merge::MergeReport {
    merge_databases(
        inputs,
        &MergeOptions {
            output: output.to_path_buf(),
            verbose: false,
        },
    )
    .unwrap()
}

fn sequencing_marker(conn: &Connection) -> serde_json::Value {
    let options: String = conn
        .query_row("SELECT options_json FROM analysis_run", [], |r| r.get(0))
        .unwrap();
    serde_json::from_str::<serde_json::Value>(&options).unwrap()["call_sequencing"].clone()
}

/// The callees of `function`'s call events, in event order.
fn event_callees(conn: &Connection, function: &str) -> Vec<String> {
    conn.prepare(
        "SELECT s.callee_text FROM cfg_events e JOIN cfg_functions c ON c.id = e.cfg_id \
         JOIN functions f ON f.id = c.fn_id JOIN cfg_event_sites es ON es.event_id = e.id \
         JOIN call_sites s ON s.id = es.call_site_id WHERE f.name = ?1 ORDER BY e.id",
    )
    .unwrap()
    .query_map([function], |r| r.get(0))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// `function`'s immediate sequenced-before pairs, as `a<b` by callee.
fn event_order(conn: &Connection, function: &str) -> Vec<String> {
    let callee = "(SELECT s.callee_text FROM cfg_event_sites es \
                  JOIN call_sites s ON s.id = es.call_site_id WHERE es.event_id = {} LIMIT 1)";
    conn.prepare(&format!(
        "SELECT {} || '<' || {} FROM cfg_event_order o JOIN cfg_events e ON e.id = o.event_id \
         JOIN cfg_functions c ON c.id = e.cfg_id JOIN functions f ON f.id = c.fn_id \
         WHERE f.name = ?1 ORDER BY o.event_id, o.next_event_id",
        callee.replace("{}", "o.event_id"),
        callee.replace("{}", "o.next_event_id"),
    ))
    .unwrap()
    .query_map([function], |r| r.get(0))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Rows that name something outside their own graph or function: an event
/// site of another caller, an event or edge across graphs.
fn misattributed_cfg_rows(conn: &Connection) -> i64 {
    count(
        conn,
        "SELECT (SELECT COUNT(*) FROM cfg_event_sites es JOIN cfg_events e ON e.id = es.event_id \
                 JOIN cfg_functions c ON c.id = e.cfg_id JOIN call_sites s ON s.id = es.call_site_id \
                 WHERE s.caller_fn_id != c.fn_id) \
              + (SELECT COUNT(*) FROM cfg_events e JOIN cfg_blocks b ON b.id = e.block_id \
                 WHERE b.cfg_id != e.cfg_id) \
              + (SELECT COUNT(*) FROM cfg_edges g JOIN cfg_blocks s ON s.id = g.src_block \
                 JOIN cfg_blocks d ON d.id = g.dst_block WHERE s.cfg_id != d.cfg_id) \
              + (SELECT COUNT(*) FROM cfg_functions c JOIN cfg_blocks b ON b.id = c.entry_block \
                 WHERE b.cfg_id != c.id)",
    )
}

const CFG_TABLES: [&str; 7] = [
    "cfg_functions",
    "cfg_blocks",
    "cfg_edges",
    "cfg_events",
    "cfg_event_sites",
    "cfg_event_order",
    "cfg_gaps",
];

fn cfg_dump(conn: &Connection) -> Vec<String> {
    CFG_TABLES
        .iter()
        .flat_map(|table| {
            conn.prepare(&format!("SELECT * FROM {table}"))
                .unwrap()
                .query_map([], |r| {
                    let row: Vec<String> = (0..r.as_ref().column_count())
                        .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                        .collect();
                    Ok(format!("{table} {}", row.join(",")))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
        })
        .collect()
}

const ORDERED: &str =
    "void a(void); void b(void); void c(void);\nvoid run(void) { a(); b(); c(); }\n";
const OTHER: &str = "void x(void); void y(void);\nvoid other(void) { x(); y(); }\n";

#[test]
fn sequenced_inputs_merge_into_a_complete_call_order() {
    let tmp = tempdir().unwrap();
    let inputs = sequencing_inputs(
        tmp.path(),
        &[
            ("repo_a", "a.c", ORDERED, true),
            ("repo_b", "b.c", OTHER, true),
        ],
    );
    let output = tmp.path().join("merged.db");
    let report = merge_into(&inputs, &output);
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.kind == WarningKind::MissingSequencing),
        "{:?}",
        report.warnings
    );
    let conn = Connection::open(&output).unwrap();
    assert_eq!(sequencing_marker(&conn), serde_json::json!(true));
    assert_eq!(event_callees(&conn, "run"), ["a", "b", "c"]);
    assert_eq!(event_order(&conn, "run"), ["a<b", "b<c"]);
    assert_eq!(event_callees(&conn, "other"), ["x", "y"]);
    assert_eq!(event_order(&conn, "other"), ["x<y"]);
    assert_eq!(misattributed_cfg_rows(&conn), 0);
    let units: Vec<String> = conn
        .prepare(
            "SELECT u.path FROM cfg_functions c JOIN files u ON u.id = c.tu_file_id \
             JOIN functions f ON f.id = c.fn_id ORDER BY f.name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        units.len() == 2 && units[0].ends_with("b.c") && units[1].ends_with("a.c"),
        "each graph keeps the unit it was lowered in: {units:?}"
    );
    let inputs_total: i64 = inputs
        .iter()
        .map(|db| {
            count(
                &Connection::open(db).unwrap(),
                "SELECT COUNT(*) FROM cfg_events",
            )
        })
        .sum();
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_events"),
        inputs_total
    );
    let capability = trace_db::require_sequencing(&conn).unwrap();
    assert!(capability == trace_db::SequencingMarker::Complete && capability.notice().is_none());

    // Merging again gives the same rows.
    let again = tmp.path().join("again.db");
    merge_into(&inputs, &again);
    assert_eq!(
        cfg_dump(&Connection::open(&again).unwrap()),
        cfg_dump(&conn)
    );
}

#[test]
fn mixed_inputs_merge_into_partial_sequencing_with_a_diagnostic() {
    let tmp = tempdir().unwrap();
    let inputs = sequencing_inputs(
        tmp.path(),
        &[
            ("repo_a", "a.c", ORDERED, true),
            ("plain", "b.c", OTHER, false),
        ],
    );
    let output = tmp.path().join("merged.db");
    let report = merge_into(&inputs, &output);
    let missing: Vec<_> = report
        .warnings
        .iter()
        .filter(|w| w.kind == WarningKind::MissingSequencing)
        .collect();
    assert_eq!(missing.len(), 1, "{:?}", report.warnings);
    assert!(
        missing[0].message.contains("plain.db") && missing[0].message.contains("--sequencing"),
        "{}",
        missing[0].message
    );
    let conn = Connection::open(&output).unwrap();
    assert_eq!(sequencing_marker(&conn), serde_json::json!("partial"));
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM diagnostics WHERE stage = 'merge' AND severity = 'info' \
             AND message LIKE '%plain.db%--sequencing%'"
        ),
        1
    );
    assert_eq!(event_callees(&conn, "run"), ["a", "b", "c"]);
    assert!(event_callees(&conn, "other").is_empty());
    let capability = trace_db::require_sequencing(&conn).unwrap();
    assert_eq!(capability, trace_db::SequencingMarker::Partial);
    assert!(capability.notice().unwrap().contains("partial"));
    // `inspect sequence` reads the merged graph and says it is partial.
    let run = trace_db::resolve_function_target(&conn, Some("run"), None, None).unwrap();
    let graph = trace_db::sequence_graph(
        &conn,
        &trace_db::SequenceScope::Function {
            function: trace_db::FunctionRowId(run.id),
            file: None,
        },
        &Default::default(),
    )
    .unwrap();
    assert_eq!(graph.capability, "partial");
    let text = trace_db::render_sequence(&graph, trace_db::RenderFormat::Text).unwrap();
    assert!(text.contains("note: call sequencing is partial"), "{text}");
    let a = text.find("a()").expect(&text);
    let b = text.find("b()").expect(&text);
    let c = text.find("c()").expect(&text);
    assert!(a < b && b < c, "{text}");
}

#[test]
fn unsequenced_inputs_merge_without_call_order() {
    let tmp = tempdir().unwrap();
    let inputs = sequencing_inputs(
        tmp.path(),
        &[
            ("repo_a", "a.c", ORDERED, false),
            ("repo_b", "b.c", OTHER, false),
        ],
    );
    let output = tmp.path().join("merged.db");
    let report = merge_into(&inputs, &output);
    assert!(
        !report
            .warnings
            .iter()
            .any(|w| w.kind == WarningKind::MissingSequencing),
        "nothing to diagnose when no input is sequenced: {:?}",
        report.warnings
    );
    let conn = Connection::open(&output).unwrap();
    assert_eq!(sequencing_marker(&conn), serde_json::json!(false));
    assert!(cfg_dump(&conn).is_empty());
    // Kind strings are interned only for a sequenced merge.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM strings WHERE text = 'fallthrough'"
        ),
        0
    );
    assert!(trace_db::require_sequencing(&conn).is_err());
}

/// `repo_a` and `repo_b`, each including `include/common.h` (`header`) and
/// calling its `common`, analysed `--sequencing` with each repo's extra args.
fn header_inputs(
    root: &std::path::Path,
    header: &str,
    args: [&[&str]; 2],
) -> Vec<std::path::PathBuf> {
    let include = root.join("include");
    fs::create_dir_all(&include).unwrap();
    fs::write(include.join("common.h"), header).unwrap();
    let mut inputs = Vec::new();
    for (name, extra) in ["repo_a", "repo_b"].into_iter().zip(args) {
        let repo = root.join(name);
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("main.cpp"),
            format!("#include \"common.h\"\nint {name}_run() {{ return common(); }}\n"),
        )
        .unwrap();
        let db = root.join(format!("{name}.db"));
        let mut all = vec![
            "--sequencing".as_ref(),
            "--include".as_ref(),
            include.as_os_str(),
        ];
        all.extend(extra.iter().map(std::ffi::OsStr::new));
        analyze_repo_with(&repo, &db, &all);
        assert_eq!(
            count(
                &Connection::open(&db).unwrap(),
                "SELECT COUNT(*) FROM cfg_functions c JOIN functions f ON f.id = c.fn_id \
                 WHERE f.name = 'common'"
            ),
            1
        );
        inputs.push(db);
    }
    inputs
}

#[test]
fn a_header_body_merged_from_several_inputs_keeps_one_graph_with_every_inputs_sites() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let inputs = header_inputs(
        root,
        "int local();\ninline int common() { local(); return local(); }\n",
        [&[], &[]],
    );
    let output = root.join("merged.db");
    let report = merge_into(&inputs, &output);
    assert!(report.header_functions_deduped >= 1, "{report:?}");
    let conn = Connection::open(&output).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM functions WHERE name = 'common'"
        ),
        1,
        "the body is one merged function"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM cfg_functions c JOIN functions f ON f.id = c.fn_id \
             WHERE f.name = 'common'"
        ),
        1,
        "one graph for the one merged body"
    );
    // The kept graph's two events each list the first input's site, then
    // the second input's copy of it.
    let first_input_sites = count(
        &Connection::open(&inputs[0]).unwrap(),
        "SELECT COUNT(*) FROM call_sites",
    );
    let sites: Vec<(i64, i64, bool)> = conn
        .prepare(
            "SELECT e.id, es.ordinal, es.call_site_id > ?1 FROM cfg_event_sites es \
             JOIN cfg_events e ON e.id = es.event_id JOIN cfg_functions c ON c.id = e.cfg_id \
             JOIN functions f ON f.id = c.fn_id WHERE f.name = 'common' \
             ORDER BY e.id, es.ordinal",
        )
        .unwrap()
        .query_map([first_input_sites], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let from_second: Vec<bool> = sites.iter().map(|s| s.2).collect();
    assert_eq!(from_second, [false, true, false, true], "{sites:?}");
    assert_eq!(event_callees(&conn, "common"), ["local"; 4]);
    assert_eq!(misattributed_cfg_rows(&conn), 0);
}

/// Two inputs sharing `invoke`'s header body but passing it different
/// callbacks keep one graph whose call event lists both inputs' sites, so
/// the query follows each input's callback.
#[test]
fn a_header_body_merged_alike_keeps_every_inputs_call_targets() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let include = root.join("include");
    fs::create_dir_all(&include).unwrap();
    fs::write(
        include.join("common.h"),
        "inline void invoke(void (*cb)()) { cb(); }\n",
    )
    .unwrap();
    let mut inputs = Vec::new();
    for name in ["a", "b"] {
        let repo = root.join(format!("repo_{name}"));
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("main.cpp"),
            format!(
                "#include \"common.h\"\nvoid mark_{name}();\n\
                 void cb_{name}() {{ mark_{name}(); }}\n\
                 void run_{name}() {{ invoke(cb_{name}); }}\n"
            ),
        )
        .unwrap();
        let db = root.join(format!("repo_{name}.db"));
        analyze_repo_with(
            &repo,
            &db,
            &[
                "--sequencing".as_ref(),
                "--include".as_ref(),
                include.as_os_str(),
            ],
        );
        inputs.push(db);
    }
    let output = root.join("merged.db");
    merge_into(&inputs, &output);
    let conn = Connection::open(&output).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM cfg_functions c JOIN functions f ON f.id = c.fn_id \
             WHERE f.name = 'invoke'"
        ),
        1,
        "one body compiled alike is one graph"
    );
    // The call edges keep both callbacks ...
    let targets: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT g.name FROM call_edges ce JOIN functions f ON f.id = ce.caller_fn_id \
             JOIN functions g ON g.id = ce.callee_fn_id WHERE f.name = 'invoke' ORDER BY g.name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(targets, ["cb_a", "cb_b"]);
    // ... and so does the one event, primary first.
    assert_eq!(event_callees(&conn, "invoke"), ["cb", "cb"]);
    assert_eq!(misattributed_cfg_rows(&conn), 0);
    let invoke = trace_db::resolve_function_target(&conn, Some("invoke"), None, None).unwrap();
    let graph = trace_db::sequence_graph(
        &conn,
        &trace_db::SequenceScope::Function {
            function: trace_db::FunctionRowId(invoke.id),
            file: None,
        },
        &Default::default(),
    )
    .unwrap();
    let frames: Vec<&str> = graph.frames.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        frames,
        ["invoke", "cb_a", "cb_b"],
        "both callbacks are followed"
    );

    // Merging again gives the same rows.
    let again = root.join("again.db");
    merge_into(&inputs, &again);
    assert_eq!(
        cfg_dump(&Connection::open(&again).unwrap()),
        cfg_dump(&conn)
    );
}

/// A header body the inputs compile differently is kept once per distinct
/// graph, as `trace analyze` keeps configuration variants.
#[test]
fn a_header_body_compiled_differently_keeps_a_variant_per_input() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let inputs = header_inputs(
        root,
        "int local();\nint other();\ninline int common() {\n#ifdef MODE_B\n  other();\n#endif\n  return local();\n}\n",
        [&[], &["-D", "MODE_B"]],
    );
    let output = root.join("merged.db");
    let report = merge_into(&inputs, &output);
    assert!(report.header_functions_deduped >= 1, "{report:?}");
    let conn = Connection::open(&output).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM functions WHERE name = 'common'"
        ),
        1,
        "the body is one merged function"
    );
    let variants: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT c.variant, (SELECT COUNT(*) FROM cfg_events e WHERE e.cfg_id = c.id) \
             FROM cfg_functions c JOIN functions f ON f.id = c.fn_id \
             WHERE f.name = 'common' ORDER BY c.id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(variants, [(0, 1), (1, 2)], "one variant per distinct body");
    assert_eq!(event_callees(&conn, "common"), ["local", "other", "local"]);
    assert_eq!(misattributed_cfg_rows(&conn), 0);
}

/// Swap the ids of two `call_sites` rows everywhere an input names them.
fn swap_call_site_ids(conn: &Connection, a: i64, b: i64) {
    let columns: Vec<(String, String)> = conn
        .prepare(
            "SELECT m.name, p.name FROM sqlite_master m, pragma_table_info(m.name) p \
             WHERE m.type = 'table' AND (p.name = 'call_site_id' \
               OR (m.name = 'call_sites_data' AND p.name = 'id'))",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    for (table, column) in columns {
        for (from, to) in [(a, -1), (b, a), (-1, b)] {
            conn.execute(
                &format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1"),
                [from, to],
            )
            .unwrap();
        }
    }
}

/// Two inputs compiling one header body alike, whose call-site ids for
/// the alternatives of one dispatch are numbered differently, keep one
/// graph: a graph reads back in its written site order, not by id.
#[test]
fn a_body_whose_site_ids_are_numbered_differently_keeps_one_graph() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let inputs = header_inputs(
        root,
        "struct Base { virtual int f(); };\n\
         struct D1 : Base { int f() override { return 1; } };\n\
         struct D2 : Base { int f() override { return 2; } };\n\
         Base *get();\n\
         inline int common() { return get()->f(); }\n",
        [&[], &[]],
    );
    let b = Connection::open(&inputs[1]).unwrap();
    let overrides: Vec<i64> = b
        .prepare(
            "SELECT s.id FROM call_sites s JOIN functions f ON f.id = s.caller_fn_id \
             WHERE f.name = 'common' AND s.callee_text IN ('D1::f', 'D2::f') ORDER BY s.id",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(overrides.len(), 2, "the dispatch has two overrides");
    let one_event: i64 = b
        .query_row(
            "SELECT COUNT(DISTINCT event_id) FROM cfg_event_sites WHERE call_site_id IN (?1, ?2)",
            [overrides[0], overrides[1]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(one_event, 1, "both overrides are alternatives of one event");
    swap_call_site_ids(&b, overrides[0], overrides[1]);
    drop(b);
    let output = root.join("merged.db");
    merge_into(&inputs, &output);
    let conn = Connection::open(&output).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM cfg_functions c JOIN functions f ON f.id = c.fn_id \
             WHERE f.name = 'common'"
        ),
        1,
        "one body compiled alike is one graph"
    );
    let callees: Vec<String> = conn
        .prepare(
            "SELECT s.callee_text FROM cfg_event_sites es JOIN call_sites s ON s.id = es.call_site_id \
             JOIN cfg_events e ON e.id = es.event_id JOIN cfg_functions c ON c.id = e.cfg_id \
             JOIN functions f ON f.id = c.fn_id WHERE f.name = 'common' \
             ORDER BY e.id, es.ordinal",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        callees,
        ["get", "get", "Base::f", "D1::f", "D2::f", "Base::f", "D1::f", "D2::f"],
        "primary first, each input's records in its written order"
    );
}

#[test]
fn an_event_whose_sites_were_not_carried_is_spliced_out() {
    let tmp = tempdir().unwrap();
    let inputs = sequencing_inputs(tmp.path(), &[("repo_a", "a.c", ORDERED, true)]);
    // An input whose `b()` site is gone (the tables are read as written).
    let input = Connection::open(&inputs[0]).unwrap();
    input
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM call_edges_data WHERE call_site_id IN
               (SELECT id FROM call_sites WHERE callee_text = 'b');
             DELETE FROM call_sites_data WHERE id IN
               (SELECT id FROM call_sites WHERE callee_text = 'b');",
        )
        .unwrap();
    let b_line: i64 = input
        .query_row(
            "SELECT e.line FROM cfg_events e WHERE NOT EXISTS \
             (SELECT 1 FROM cfg_event_sites es JOIN call_sites s ON s.id = es.call_site_id \
              WHERE es.event_id = e.id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(input);
    let output = tmp.path().join("merged.db");
    merge_into(&inputs, &output);
    let conn = Connection::open(&output).unwrap();
    assert_eq!(event_callees(&conn, "run"), ["a", "c"]);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_events"),
        2,
        "no event is left without a site"
    );
    assert_eq!(event_order(&conn, "run"), ["a<c"]);
    let gap: (String, Option<i64>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT k.text, g.line, g.first_event, g.last_event FROM cfg_gaps g \
             JOIN strings k ON k.id = g.kind_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(gap, ("omitted_sites".into(), Some(b_line), None, None));
    let coverage: String = conn
        .query_row(
            "SELECT k.text FROM cfg_functions c JOIN strings k ON k.id = c.coverage_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(coverage, "partial");
}

/// An input with more graphs than one read takes: every graph is carried,
/// in id order, the same on every merge.
#[test]
fn graphs_are_carried_across_read_chunks() {
    let tmp = tempdir().unwrap();
    let functions = 1100;
    let mut source = String::from("void a(void); void b(void);\n");
    for i in 0..functions {
        source.push_str(&format!("void f{i}(void) {{ a(); b(); }}\n"));
    }
    let inputs = sequencing_inputs(
        tmp.path(),
        &[
            ("repo_a", "a.c", &source, true),
            ("repo_b", "b.c", ORDERED, true),
        ],
    );
    let output = tmp.path().join("merged.db");
    merge_into(&inputs, &output);
    let conn = Connection::open(&output).unwrap();
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_functions"),
        functions + 1
    );
    assert_eq!(event_order(&conn, "f1099"), ["a<b"]);
    assert_eq!(event_order(&conn, "run"), ["a<b", "b<c"]);
    assert_eq!(misattributed_cfg_rows(&conn), 0);
    let again = tmp.path().join("again.db");
    merge_into(&inputs, &again);
    assert_eq!(
        cfg_dump(&Connection::open(&again).unwrap()),
        cfg_dump(&conn)
    );
}

/// A graph row naming a file its input does not list is an error naming
/// the input, as a call site naming one is, never a panic.
#[test]
fn a_cfg_row_naming_an_unknown_file_is_an_error() {
    for update in [
        "UPDATE cfg_events SET file_id = 9999 WHERE id = (SELECT MIN(id) FROM cfg_events)",
        "UPDATE cfg_events SET expansion_file_id = 9999, expansion_line = 1, expansion_col = 1 \
         WHERE id = (SELECT MIN(id) FROM cfg_events)",
        "UPDATE cfg_blocks SET file_id = 9999, line = 1, col = 1 \
         WHERE id = (SELECT MIN(id) FROM cfg_blocks)",
    ] {
        let tmp = tempdir().unwrap();
        let inputs = sequencing_inputs(tmp.path(), &[("repo_a", "a.c", ORDERED, true)]);
        let input = Connection::open(&inputs[0]).unwrap();
        input.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        input.execute(update, []).unwrap();
        drop(input);
        let error = merge_databases(
            &inputs,
            &MergeOptions {
                output: tmp.path().join("merged.db"),
                verbose: false,
            },
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("9999") && error.contains("repo_a.db"),
            "{update}: {error}"
        );
    }
}

#[test]
fn a_sequenced_input_must_have_every_cfg_column() {
    let tmp = tempdir().unwrap();
    let inputs = sequencing_inputs(tmp.path(), &[("repo_a", "a.c", ORDERED, true)]);
    Connection::open(&inputs[0])
        .unwrap()
        .execute_batch("ALTER TABLE cfg_events RENAME COLUMN expansion_col TO other")
        .unwrap();
    let output = tmp.path().join("merged.db");
    fs::write(&output, b"existing output").unwrap();
    let error = merge_databases(
        &inputs,
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("missing trace-merge capability"), "{error}");
    assert!(
        error.contains("cfg_events") && error.contains("expansion_col"),
        "{error}"
    );
    assert_eq!(fs::read(&output).unwrap(), b"existing output");
}
