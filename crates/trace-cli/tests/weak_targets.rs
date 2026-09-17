mod common;

use common::TempDb;
use rusqlite::Connection;
use serde_json::json;
use std::{fs, path::Path, process::Command};

/// Run `trace analyze` over `root` with a full export and open the result.
///
/// The database lives outside the analyzed tree: written inside it, it would
/// be a file the analyzer walks on the next run.
fn analyze(root: &Path, jobs: Option<usize>) -> (TempDb, Connection) {
    let db = TempDb::new("analysis.db");
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace"));
    command.arg("analyze").arg(root);
    if let Some(jobs) = jobs {
        command.arg("--jobs").arg(jobs.to_string());
    }
    let output = command
        .arg("--full-export")
        .arg("-o")
        .arg(db.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conn = Connection::open(db.path()).unwrap();
    (db, conn)
}

/// Exercise lowering, target selection, Andersen propagation, and SQLite export.
#[test]
fn exported_edges_follow_each_targets_selected_weak_definitions() {
    assert_selected_weak_definitions("c");
}

#[test]
fn cpp_direct_initialized_weak_global_obeys_target_override() {
    assert_selected_weak_definitions("cpp");
}

fn assert_selected_weak_definitions(extension: &str) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (name, source) in [
        (
            "caller",
            r#"
typedef void (*callback)(void);
void hook(void);
extern callback global_callback;
extern callback configured;
struct Ops { callback run; };
extern struct Ops configured_ops;
callback get_hook(void) { return hook; }
callback forwarded(void) { return get_hook(); }
void direct(void) { hook(); }
void address(void) { callback p = &hook; p(); }
void returned(void) { callback p = forwarded(); p(); }
void invoke_global(void) { global_callback(); }
void invoke_configured(void) { configured(); }
void invoke_aggregate(void) { configured_ops.run(); }
"#,
        ),
        (
            "fallback",
            r#"
typedef void (*callback)(void);
callback global_callback;
void fallback_only(void);
__attribute__((weak)) void hook(void) {
    global_callback = fallback_only;
}
void fallback_only(void) {}
extern callback configured __attribute__((weak));
callback configured = fallback_only;
struct Ops { callback run; };
__attribute__((weak)) struct Ops configured_ops = { fallback_only };
"#,
        ),
        (
            "strong",
            r#"
typedef void (*callback)(void);
extern callback global_callback;
void strong_only(void) {}
void hook(void) { global_callback = strong_only; }
callback configured = strong_only;
struct Ops { callback run; };
struct Ops configured_ops = { strong_only };
"#,
        ),
    ] {
        let source = if extension == "cpp" {
            source.replace(
                "callback configured = fallback_only;",
                "callback configured(fallback_only);",
            )
        } else {
            source.to_owned()
        };
        fs::write(root.join(format!("{name}.{extension}")), source).unwrap();
    }
    let commands: Vec<_> = ["caller", "fallback", "strong"]
        .into_iter()
        .map(|name| {
            json!({
                "directory": root, "file": format!("{name}.{extension}"), "output": format!("{name}.o"),
                "arguments": ["cc", "-c", format!("{name}.{extension}"), "-o", format!("{name}.o")]
            })
        })
        .collect();
    fs::write(
        root.join("compile_commands.json"),
        json!(commands).to_string(),
    )
    .unwrap();
    fs::write(root.join("link_commands.json"), json!([
        {"directory":root,"output":"full","arguments":["cc","caller.o","fallback.o","strong.o","-o","full"]},
        {"directory":root,"output":"fallback","arguments":["cc","caller.o","fallback.o","-o","fallback"]}
    ]).to_string()).unwrap();

    let mut previous = None;
    for jobs in [1, 4] {
        let (_db, conn) = analyze(root, Some(jobs));
        // An IPC bridge models a process boundary and is expected to cross
        // images; every other edge must stay inside one.
        let invalid_scope: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM call_edges e
             JOIN functions caller ON caller.id = e.caller_fn_id
             JOIN functions callee ON callee.id = e.callee_fn_id
             WHERE e.resolution <> 'ipc'
               AND (caller.target_id IS NULL OR callee.target_id IS NULL
                OR caller.target_id <> callee.target_id)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invalid_scope, 0, "unscoped or cross-target call edge");

        let mut query = conn
            .prepare(
                "SELECT ct.name, caller.name, dt.name, callee.name, file.path
             FROM call_edges edge
             JOIN functions caller ON caller.id = edge.caller_fn_id
             JOIN functions callee ON callee.id = edge.callee_fn_id
             JOIN link_targets ct ON ct.id = caller.target_id
             JOIN link_targets dt ON dt.id = callee.target_id
             JOIN files file ON file.id = callee.file_id
             ORDER BY ct.name, caller.name, dt.name, callee.name, file.path",
            )
            .unwrap();
        let edges: Vec<(String, String, String, String, String)> = query
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(!edges.is_empty());
        for (caller_target, _, callee_target, _, _) in &edges {
            assert_eq!(caller_target, callee_target, "cross-target edge: {edges:?}");
        }
        // This fixture has no IPC classes, so the exclusion above changes
        // nothing here; `ipc_bridges_span_link_targets` covers that case.
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM call_edges WHERE resolution = 'ipc'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        for (target, hook_file, callback) in [
            ("full", format!("strong.{extension}"), "strong_only"),
            ("fallback", format!("fallback.{extension}"), "fallback_only"),
        ] {
            for caller in ["direct", "address", "returned"] {
                let selected: Vec<_> = edges
                    .iter()
                    .filter(|(ct, from, _, to, _)| ct == target && from == caller && to == "hook")
                    .collect();
                assert_eq!(selected.len(), 1, "{target}/{caller}: {edges:?}");
                assert!(
                    selected[0].4.ends_with(hook_file.as_str()),
                    "{target}/{caller}: {selected:?}"
                );
            }
            for caller in ["invoke_global", "invoke_configured", "invoke_aggregate"] {
                let selected: Vec<_> = edges
                    .iter()
                    .filter(|(ct, from, _, _, _)| ct == target && from == caller)
                    .map(|(_, _, _, to, _)| to.as_str())
                    .collect();
                assert_eq!(selected, [callback], "{target}/{caller}: {edges:?}");
            }
            let is_weak: i32 = conn.query_row(
                "SELECT f.is_weak FROM functions f JOIN link_targets t ON t.id = f.target_id WHERE t.name = ?1 AND f.name = 'hook' AND f.is_defined = 1",
                [target], |row| row.get(0),
            ).unwrap();
            assert_eq!(is_weak, i32::from(target == "fallback"));
        }
        if let Some(previous) = &previous {
            assert_eq!(
                &edges, previous,
                "parallel indexing changed target-specific edges"
            );
        }
        previous = Some(edges);
    }
}

/// A Binder call crosses a process boundary, so the proxy and the stub it
/// dispatches to are linked into different images. The bridge must survive
/// that — scoping detection to one target silently removed the whole feature.
#[test]
fn ipc_bridges_span_link_targets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("client.cpp"),
        "struct IRemote { virtual int SendRequest(int) = 0; };\n\
         struct FooProxy {\n\
           IRemote *remote;\n\
           int Run(int a) { return remote->SendRequest(a); }\n\
         };\n\
         int drive(FooProxy *p) { return p->Run(1); }\n",
    )
    .unwrap();
    fs::write(
        root.join("service.cpp"),
        "struct FooStub {\n\
           int Run(int a) { return a + 1; }\n\
           int OnRemoteRequest(int a) { return Run(a); }\n\
         };\n\
         int serve(FooStub *s) { return s->OnRemoteRequest(2); }\n",
    )
    .unwrap();
    let commands: Vec<_> = ["client", "service"]
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
    fs::write(
        root.join("link_commands.json"),
        json!([
            {"directory": root, "output": "client", "arguments": ["c++", "client.o", "-o", "client"]},
            {"directory": root, "output": "service", "arguments": ["c++", "service.o", "-o", "service"]}
        ])
        .to_string(),
    )
    .unwrap();
    let (_db, conn) = analyze(root, None);
    let mut stmt = conn
        .prepare(
            "SELECT caller.name, callee.name, ct.name, dt.name
             FROM call_edges e
             JOIN functions caller ON caller.id = e.caller_fn_id
             JOIN functions callee ON callee.id = e.callee_fn_id
             JOIN link_targets ct ON ct.id = caller.target_id
             JOIN link_targets dt ON dt.id = callee.target_id
             WHERE e.resolution = 'ipc'",
        )
        .unwrap();
    let bridges: Vec<(String, String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        bridges,
        vec![(
            "FooProxy::Run".into(),
            "FooStub::Run".into(),
            "client".into(),
            "service".into()
        )],
        "proxy and stub live in different images"
    );
}
