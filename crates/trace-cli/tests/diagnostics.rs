use std::process::{Command, Output};

fn inspect(db: &std::path::Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("inspect")
        .arg(db)
        .arg("diagnostics")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    output
}

#[test]
fn diagnostics_filters_and_formats_work_on_both_schema_versions() {
    let dir = tempfile::tempdir().unwrap();
    for (version, schema) in [(7, trace_db::SCHEMA_V7), (8, trace_db::SCHEMA_V8)] {
        let db = dir.path().join(format!("v{version}.db"));
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(schema).unwrap();
        conn.execute_batch(
            "INSERT INTO files(id,path,sha256) VALUES(0,'src/a_%.cpp','');
             INSERT INTO diagnostics VALUES(3,'warning',NULL,0,'solver stopped early','analyze');
             INSERT INTO diagnostics VALUES(1,'warning',0,7,'missing header','preprocess');
             INSERT INTO diagnostics VALUES(2,'error',0,9,'unexpected token','parse');",
        )
        .unwrap();
        drop(conn);

        let output = inspect(&db, &["--format", "json"]);
        let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 3);
        assert_eq!(rows[0]["message"], "missing header");
        assert_eq!(rows[0]["file"], "src/a_%.cpp");
        assert_eq!(rows[0]["line"], 7);
        assert!(rows[2]["file"].is_null());

        let output = inspect(
            &db,
            &["--stage", "parse", "--severity", "error", "--file", "_%"],
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "src/a_%.cpp:9: error [parse]: unexpected token\n"
        );
        let output = inspect(&db, &["--stage", "analyze"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "warning [analyze]: solver stopped early\n"
        );
        let output = inspect(&db, &["--file", "aX%", "--format", "json"]);
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "[]");
        let output = inspect(&db, &["--stage", "analyze", "--file", "src"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "No matching diagnostics."
        );
    }
}

#[test]
fn analyze_points_to_exported_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.c"),
        "#include \"missing_header.h\"\nint main(void) { return 0; }\n",
    )
    .unwrap();
    let db = dir.path().join("result.db");
    let output = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(dir.path())
        .arg("-o")
        .arg(&db)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("diagnostic(s) recorded; view with: trace inspect"),
        "{stderr}"
    );
    let output = inspect(&db, &["--format", "json"]);
    let rows: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(rows.as_array().unwrap().iter().any(|row| {
        row["message"]
            .as_str()
            .unwrap()
            .contains("missing_header.h")
    }));
}
