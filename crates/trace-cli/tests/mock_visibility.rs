mod common;
use common::TempDb;
use rusqlite::Connection;
use std::{path::Path, process::Command};

#[test]
fn excluded_overload_remains_external_beside_a_production_overload() {
    let tree = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("api.h", "int hook(int); int hook(double);"),
        (
            "src/hook.cpp",
            "#include \"api.h\"\nint hook(int) { return 1; }",
        ),
        (
            "test/hook.cpp",
            "#include \"api.h\"\nint hook(double) { return 2; }",
        ),
        (
            "src/use.cpp",
            "#include \"api.h\"\nint use() { return hook(1.0); }",
        ),
    ] {
        let file = tree.path().join(file);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    let db = TempDb::new("overload.db");
    let result = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(tree.path())
        .arg("-o")
        .arg(db.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let conn = Connection::open(db.path()).unwrap();
    let mut query = conn.prepare("SELECT e.resolution FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id WHERE caller.name='use'").unwrap();
    let resolutions: Vec<String> = query
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(resolutions, ["external"]);
}

#[test]
fn test_placeholders_and_test_only_definitions_do_not_erase_external_edges() {
    for mock in [false, true] {
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir(tree.path().join("test")).unwrap();
        std::fs::write(
            tree.path().join("test/first.cpp"),
            if mock {
                "int missing() { return 1; } int test_use() { return missing(); }"
            } else {
                "int test_use() { return missing(); }"
            },
        )
        .unwrap();
        std::fs::write(
            tree.path().join("zprod.cpp"),
            "int use() { return missing(); }",
        )
        .unwrap();
        let db = TempDb::new("external.db");
        let result = Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("analyze")
            .arg(tree.path())
            .arg("-o")
            .arg(db.path())
            .output()
            .unwrap();
        assert!(result.status.success());
        let conn = Connection::open(db.path()).unwrap();
        let edges: i64 = conn.query_row("SELECT count(*) FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id WHERE caller.name='use' AND callee.name='missing' AND e.resolution='external'", [], |r| r.get(0)).unwrap();
        assert_eq!(
            edges, 1,
            "mock={mock}: production must retain the external call"
        );
    }
}

#[test]
fn exported_production_calls_do_not_reach_mock_definitions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mock_visibility");
    let db = TempDb::new("mock.db");
    let result = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(root)
        .arg("-o")
        .arg(db.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let conn = Connection::open(db.path()).unwrap();
    let mut query = conn.prepare("SELECT caller.name, callee.name, file.path FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id JOIN files file ON file.id=callee.file_id WHERE caller.name IN ('ns::use','ns::submit','ns::test_use') ORDER BY caller.name").unwrap();
    let rows: Vec<(String, String, String)> = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 3, "{rows:?}");
    for (caller, _, file) in rows {
        assert!(
            file.ends_with(if caller == "ns::test_use" {
                "test/mock/worker.h"
            } else {
                "src/worker.cpp"
            }),
            "{caller}: {file}"
        );
    }
}
