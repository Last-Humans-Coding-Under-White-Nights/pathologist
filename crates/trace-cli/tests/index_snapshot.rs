mod common;

use rusqlite::{types::Value, Connection};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path, process::Command};

fn run(command: &str, input: &Path, output: &Path, flags: &[&str]) {
    let result = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg(command)
        .arg(input)
        .arg("-o")
        .arg(output)
        .args(flags)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn copy_fixture(name: &str, destination: &Path) {
    let source = common::fixture(name);
    for entry in walkdir::WalkDir::new(&source) {
        let entry = entry.unwrap();
        let path = destination.join(entry.path().strip_prefix(&source).unwrap());
        if entry.file_type().is_dir() {
            fs::create_dir_all(path).unwrap();
        } else {
            fs::copy(entry.path(), path).unwrap();
        }
    }
}

// Compare every exported column, including IDs and solver metadata, except
// the timestamp of the run. Both minimal and full exports use this check.
fn database_rows(path: &Path) -> BTreeMap<String, Vec<Vec<Value>>> {
    let conn = Connection::open(path).unwrap();
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let quoted = format!("\"{}\"", table.replace('"', "\"\""));
            let columns = conn
                .prepare(&format!("SELECT * FROM {quoted}"))
                .unwrap()
                .column_names()
                .into_iter()
                .filter(|c| table != "analysis_run" || *c != "created_at")
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect::<Vec<_>>();
            let mut stmt = conn
                .prepare(&format!("SELECT {} FROM {quoted}", columns.join(",")))
                .unwrap();
            let mut rows = stmt
                .query_map([], |row| {
                    (0..columns.len())
                        .map(|i| row.get::<_, Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows.sort_by_cached_key(|row| format!("{row:?}"));
            (table, rows)
        })
        .collect()
}

fn assert_round_trip(root: &Path, scratch: &Path, frontend_flags: &[&str], full: bool) {
    let direct = scratch.join("direct.db");
    let replay = scratch.join("replay.db");
    let index = scratch.join("project.index");
    let mut flags = frontend_flags.to_vec();
    flags.extend(["--jobs", "1", "--solve-budget-pops", "0"]);
    if full {
        flags.extend(["--full-export", "--debug-points-to"]);
    }
    run("analyze", root, &direct, &flags);
    let mut flags = frontend_flags.to_vec();
    flags.extend(["--jobs", "4"]);
    run("index", root, &index, &flags);
    // Make sources and build metadata unavailable to the second stage.
    fs::rename(root, scratch.join("archived-sources")).unwrap();
    for (i, pair) in frontend_flags
        .windows(2)
        .filter(|pair| pair[0] == "--dep")
        .enumerate()
    {
        let dep = Path::new(pair[1]);
        if dep.exists() {
            fs::rename(dep, scratch.join(format!("archived-dependency-{i}"))).unwrap();
        }
    }
    let mut flags = vec!["--solve-budget-pops", "0"];
    if full {
        flags.extend(["--full-export", "--debug-points-to"]);
    }
    for pair in frontend_flags
        .windows(2)
        .filter(|pair| pair[0] == "--models")
    {
        flags.extend(["--models", pair[1]]);
    }
    run("analyze-index", &index, &replay, &flags);
    let direct = database_rows(&direct);
    let replay = database_rows(&replay);
    assert_eq!(
        direct.keys().collect::<Vec<_>>(),
        replay.keys().collect::<Vec<_>>()
    );
    for (table, rows) in direct {
        assert_eq!(rows, replay[&table], "table {table}");
    }
}

#[test]
fn snapshots_match_direct_analysis_without_sources() {
    for name in [
        "macro_call_positions",
        "header_inline_call",
        "header_chain",
        "mixed_lang_late",
        "cpp_smart_pointer_value_flow",
        "macro_virtual_cross_tu",
        "static_return_to_global",
        "idl_basic",
        "cpp_template_parameter_bases",
        "cpp_anonymous_members",
        "cpp_static_member_cross_tu",
        "test_partition",
    ] {
        for full in [false, true] {
            let scratch = tempfile::tempdir().unwrap();
            let root = scratch.path().join("source");
            copy_fixture(name, &root);
            assert_round_trip(&root, scratch.path(), &[], full);
        }
    }
}

#[test]
fn snapshots_preserve_dependency_declarations() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("source");
    copy_fixture("dep_root", &root);
    let target = root.join("target");
    let dep = root.join("dep");
    assert_round_trip(
        &target,
        scratch.path(),
        &["--dep", dep.to_str().unwrap()],
        true,
    );
}

#[test]
fn snapshots_preserve_exploration_and_configuration_families() {
    for configured in [false, true] {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("source");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("BUILD.gn"),
            "config(\"feature\") { defines = [\"FEATURE_A\", \"FEATURE_B\"] }\n",
        )
        .unwrap();
        fs::write(root.join("main.cpp"), "void a() {} void b() {}\n#if defined(FEATURE_A)\nvoid selected() { a(); }\n#else\nvoid selected() { b(); }\n#endif\nvoid invoke() { selected(); }\n").unwrap();
        if configured {
            fs::write(root.join("compile_commands.json"), json!([
                {"directory": root, "file":"main.cpp", "arguments":["c++", "-c", "main.cpp"]},
                {"directory": root, "file":"main.cpp", "arguments":["c++", "-DFEATURE_A", "-c", "main.cpp"]}
            ]).to_string()).unwrap();
        }
        assert_round_trip(&root, scratch.path(), &["--explore"], true);
    }
}

#[test]
fn snapshots_preserve_target_scopes_and_weak_selection() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("source");
    fs::create_dir(&root).unwrap();
    for (name, source) in [
        ("caller", "void hook(); void invoke() { hook(); }"),
        (
            "weak",
            "void fallback() {} __attribute__((weak)) void hook() { fallback(); }",
        ),
        ("strong", "void selected() {} void hook() { selected(); }"),
    ] {
        fs::write(root.join(format!("{name}.cpp")), source).unwrap();
    }
    let commands: Vec<_> = ["caller", "weak", "strong"]
        .into_iter()
        .map(|name| {
            json!({
                "directory": root, "file": format!("{name}.cpp"), "output": format!("{name}.o"),
                "arguments": ["c++", "-c", format!("{name}.cpp"), "-o", format!("{name}.o")]
            })
        })
        .collect();
    fs::write(
        root.join("compile_commands.json"),
        json!(commands).to_string(),
    )
    .unwrap();
    fs::write(root.join("link_commands.json"), json!([
        {"directory":root, "output":"full", "arguments":["c++","caller.o","weak.o","strong.o","-o","full"]},
        {"directory":root, "output":"fallback", "arguments":["c++","caller.o","weak.o","-o","fallback"]}
    ]).to_string()).unwrap();
    assert_round_trip(&root, scratch.path(), &[], true);
}

#[test]
fn failed_indexing_keeps_existing_snapshot_and_invalid_snapshots_fail_cleanly() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("empty");
    fs::create_dir(&root).unwrap();
    let index = scratch.path().join("project.index");
    fs::write(&index, "previous snapshot").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("index")
        .arg(&root)
        .arg("-o")
        .arg(&index)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read_to_string(&index).unwrap(), "previous snapshot");
    fs::write(
        root.join("main.c"),
        "void target(void) {} void caller(void) { target(); }\n",
    )
    .unwrap();
    run("index", &root, &index, &[]);
    let original = fs::read(&index).unwrap();
    assert_eq!(&original[..8], b"\x89TRIDX\r\n");
    for alteration in [
        "version",
        "truncated",
        "length",
        "payload",
        "extra-payload",
        "trailing",
        "json",
    ] {
        let mut changed = original.clone();
        match alteration {
            "version" => changed[8..12].copy_from_slice(&999u32.to_le_bytes()),
            "truncated" => {
                changed.pop();
            }
            "length" => changed[12..20].copy_from_slice(&u64::MAX.to_le_bytes()),
            "payload" => changed[20] = 0xc1, // Reserved MessagePack marker.
            "extra-payload" => {
                let length = u64::from_le_bytes(changed[12..20].try_into().unwrap());
                changed.insert(20 + length as usize, 0xc0);
                changed[12..20].copy_from_slice(&(length + 1).to_le_bytes());
            }
            "trailing" => changed.push(0),
            "json" => changed = br#"{"Header":{"format_version":1}}"#.to_vec(),
            _ => unreachable!(),
        }
        fs::write(&index, changed).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("analyze-index")
            .arg(&index)
            .arg("-o")
            .arg(scratch.path().join("invalid.db"))
            .output()
            .unwrap();
        assert!(!result.status.success(), "{alteration}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("read index"),
            "{alteration}"
        );
        assert!(!scratch.path().join("invalid.db").exists());
    }
}

#[test]
fn snapshot_bytes_are_reproducible_across_job_counts() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("source");
    copy_fixture("macro_virtual_cross_tu", &root);
    let first = scratch.path().join("first.index");
    let second = scratch.path().join("second.index");
    run("index", &root, &first, &["--jobs", "1"]);
    run("index", &root, &second, &["--jobs", "4"]);
    assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
}

#[test]
fn model_noise_is_applied_at_index_time() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("main.c"), "void noisy(void) {} void target(void) {}\n#define LOG() noisy()\nvoid caller(void) { LOG(); target(); }\n").unwrap();
    let model = scratch.path().join("models.toml");
    fs::write(&model, "[noise]\nmacros = [\"LOG\"]\n").unwrap();
    let index = scratch.path().join("project.index");
    run("index", &root, &index, &[]);
    let rejected = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze-index")
        .arg(&index)
        .arg("--models")
        .arg(&model)
        .arg("-o")
        .arg(scratch.path().join("rejected.db"))
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("rebuild with trace index --models"));
    assert_round_trip(
        &root,
        scratch.path(),
        &["--models", model.to_str().unwrap()],
        true,
    );
}

#[test]
fn snapshots_preserve_deep_type_descriptors() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("main.c"),
        format!("int {}p; void caller(void) {{}}\n", "*".repeat(150)),
    )
    .unwrap();
    assert_round_trip(&root, scratch.path(), &[], true);
}
