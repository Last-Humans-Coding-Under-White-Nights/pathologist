mod common;
use common::TempDb;
use rusqlite::Connection;
use std::{fs, path::Path, process::Command};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (file, source) in [
        (
            "prod.cpp",
            "struct S { int run() { return 1; } }; int prod(S *s) { return s->run(); }",
        ),
        (
            "mock.cpp",
            "struct S { int run() { return 2; } }; int test(S *s) { return s->run(); }",
        ),
        ("shared.cpp", "int shared() { return 3; }"),
        (
            "BUILD.gn",
            r#"source_set("common") { sources = [ "shared.cpp" ] }
            ohos_shared_library("prod") { sources = [ "prod.cpp" ] deps = [ ":common" ] }
            ohos_unittest("test") { sources = [ "mock.cpp" ] deps = [ ":common" ] }"#,
        ),
    ] {
        fs::write(dir.path().join(file), source).unwrap();
    }
    dir
}
fn analyze(root: &Path, full: bool) -> (TempDb, Connection) {
    let db = TempDb::new("gn.db");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trace"));
    cmd.arg("analyze").arg(root).arg("-o").arg(db.path());
    if full {
        cmd.arg("--full-export");
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conn = Connection::open(db.path()).unwrap();
    (db, conn)
}
#[test]
fn inferred_targets_export_sources_and_isolate_shared_instances() {
    let dir = fixture();
    for full in [false, true] {
        let (_db, c) = analyze(dir.path(), full);
        let count: i64 = c
            .query_row("SELECT count(*) FROM link_targets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let sources: i64 = c
            .query_row("SELECT count(*) FROM target_sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sources, 3);
        let shared: i64 = c
            .query_row(
                "SELECT count(DISTINCT target_id) FROM functions WHERE name='shared'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(shared, 3);
        let wrong: i64 = c.query_row("SELECT count(*) FROM call_edges e JOIN functions a ON a.id=e.caller_fn_id JOIN functions b ON b.id=e.callee_fn_id WHERE a.name IN ('prod','test') AND (a.target_id IS NULL OR a.target_id<>b.target_id)", [], |r|r.get(0)).unwrap();
        assert_eq!(wrong, 0);
        let edges: i64 = c.query_row("SELECT count(*) FROM call_edges e JOIN functions a ON a.id=e.caller_fn_id WHERE a.name IN ('prod','test')", [], |r|r.get(0)).unwrap();
        assert_eq!(edges, 2);
    }
}
#[test]
fn malformed_authoritative_metadata_does_not_activate_gn_fallback() {
    let dir = fixture();
    fs::write(dir.path().join("link_commands.json"), "{").unwrap();
    let (_db, c) = analyze(dir.path(), false);
    let count: i64 = c
        .query_row("SELECT count(*) FROM link_targets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let warnings: i64 = c
        .query_row(
            "SELECT count(*) FROM diagnostics WHERE stage='link_commands'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(warnings > 0);
}

#[test]
fn partial_gn_graph_cannot_hide_a_definition_from_unassigned_callers() {
    let dir = fixture();
    fs::write(
        dir.path().join("loose.cpp"),
        "int shared(); int loose() { return shared(); }",
    )
    .unwrap();
    let build = fs::read_to_string(dir.path().join("BUILD.gn")).unwrap();
    fs::write(
        dir.path().join("BUILD.gn"),
        format!("{build}\nshared_library(\"unknown\") {{ sources = generated }}"),
    )
    .unwrap();
    let (_db, c) = analyze(dir.path(), false);
    let defined: i64 = c.query_row("SELECT count(*) FROM call_edges e JOIN functions a ON a.id=e.caller_fn_id JOIN functions b ON b.id=e.callee_fn_id WHERE a.name='loose' AND b.name='shared' AND b.is_defined=1", [], |r|r.get(0)).unwrap();
    assert_eq!(
        defined, 1,
        "incomplete inferred ownership must not erase the real definition"
    );
    let targets: i64 = c
        .query_row("SELECT count(*) FROM link_targets", [], |r| r.get(0))
        .unwrap();
    assert_eq!(targets, 3, "known membership remains available as metadata");
}

#[test]
fn unlisted_sources_also_make_inferred_ownership_incomplete() {
    let dir = fixture();
    fs::write(
        dir.path().join("unlisted.cpp"),
        "int shared(); int unlisted() { return shared(); }",
    )
    .unwrap();
    let (_db, c) = analyze(dir.path(), false);
    let defined: i64 = c.query_row("SELECT count(*) FROM call_edges e JOIN functions a ON a.id=e.caller_fn_id JOIN functions b ON b.id=e.callee_fn_id WHERE a.name='unlisted' AND b.name='shared' AND b.is_defined=1", [], |r|r.get(0)).unwrap();
    assert_eq!(defined, 1);
}

#[test]
fn informational_target_metadata_does_not_change_unscoped_calls() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.cpp"),
        "void use(int *p) { auto *x = reinterpret_cast<char*>(p); }",
    )
    .unwrap();
    let (_old_db, old) = analyze(dir.path(), false);
    fs::write(dir.path().join("BUILD.gn"), "source_set(\"a\") { sources = [\"a.cpp\"] } source_set(\"unknown\") { sources = generated }").unwrap();
    let (_new_db, new) = analyze(dir.path(), false);
    let edges = |c: &Connection| -> Vec<String> {
        c.prepare("SELECT b.name FROM call_edges e JOIN functions b ON b.id=e.callee_fn_id ORDER BY b.name").unwrap().query_map([], |r|r.get(0)).unwrap().map(Result::unwrap).collect()
    };
    assert_eq!(edges(&new), edges(&old));
}

/// A complete GN tree goes through the configured indexing path; it must
/// record the inferred header directories in `options_json.include_paths`
/// exactly as the bare-tree path does.
#[test]
fn inferred_include_dirs_are_recorded_on_the_configured_path() {
    let include_paths = |with_build: bool| {
        let dir = fixture();
        fs::create_dir_all(dir.path().join("inc")).unwrap();
        fs::write(dir.path().join("inc/api.h"), "int api();").unwrap();
        fs::write(
            dir.path().join("prod.cpp"),
            "#include <api.h>\nstruct S { int run() { return api(); } }; int prod(S *s) { return s->run(); }",
        )
        .unwrap();
        if !with_build {
            fs::remove_file(dir.path().join("BUILD.gn")).unwrap();
        }
        let (_db, c) = analyze(dir.path(), false);
        let targets: i64 = c
            .query_row("SELECT count(*) FROM link_targets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(targets > 0, with_build);
        let options: String = c
            .query_row("SELECT options_json FROM analysis_run", [], |r| r.get(0))
            .unwrap();
        let options: serde_json::Value = serde_json::from_str(&options).unwrap();
        let root = dir.path().canonicalize().unwrap();
        options["include_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                Path::new(p.as_str().unwrap())
                    .strip_prefix(&root)
                    .unwrap()
                    .to_path_buf()
            })
            .collect::<Vec<_>>()
    };
    let configured = include_paths(true);
    assert!(
        configured.contains(&Path::new("inc").to_path_buf()),
        "{configured:?}"
    );
    assert_eq!(configured, include_paths(false));
}
