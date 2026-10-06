use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};
use trace_db::{editor_function, editor_functions_at, ExportOptions};
use trace_ir::{FnId, Program};
use trace_lsp::{
    locations::{local_path, path_uri, PathMapping},
    Server,
};

struct Fixture {
    _scratch: TempDir,
    root: PathBuf,
    db: PathBuf,
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let scratch = tempdir().unwrap();
        let root = scratch.path().join("source with spaces");
        copy_tree(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures")
                .join(name),
            &root,
        );
        let db = scratch.path().join("analysis.db");
        Self {
            _scratch: scratch,
            root,
            db,
        }
    }

    fn program(&self) -> Program {
        trace_parse::build_program(
            &self.root,
            &trace_preproc::PreprocessOptions::new().with_include(self.root.clone()),
        )
        .unwrap()
    }

    fn export(&self, program: &Program, full: bool) {
        let (pag, analysis) = trace_analysis::analyze(program);
        let mut opts = ExportOptions::minimal(self.db.clone());
        opts.full_detail = full;
        trace_db::export_to_sqlite(program, &pag, &analysis, &opts).unwrap();
    }

    fn functions(&self, name: &str) -> Vec<trace_db::EditorFunction> {
        let conn = Connection::open(&self.db).unwrap();
        let ids = conn
            .prepare("SELECT id FROM functions WHERE name=?1 ORDER BY id")
            .unwrap()
            .query_map([name], |r| r.get::<_, u32>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        ids.into_iter()
            .map(|id| editor_function(&conn, FnId(id)).unwrap().unwrap())
            .collect()
    }

    fn server(&self) -> Server {
        let mut server = Server::open(&self.db, &[]).unwrap();
        server
            .request("initialize", json!({"capabilities": {}}))
            .unwrap();
        server
    }
}

fn item(server: &mut Server, function: &trace_db::EditorFunction) -> Value {
    let items = server
        .request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument": {"uri": path_uri(&local_path(Path::new(&function.path))).unwrap()},
                "position": {"line": function.line_start - 1, "character": 1}
            }),
        )
        .unwrap();
    items
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["data"]["function"] == function.id.0)
        .unwrap()
        .clone()
}

fn calls(server: &mut Server, item: &Value, incoming: bool) -> Value {
    server
        .request(
            if incoming {
                "callHierarchy/incomingCalls"
            } else {
                "callHierarchy/outgoingCalls"
            },
            json!({"item":item}),
        )
        .unwrap()
}

fn named_call<'a>(calls: &'a Value, name: &str, incoming: bool) -> &'a Value {
    calls
        .as_array()
        .unwrap()
        .iter()
        .find(|call| call[if incoming { "from" } else { "to" }]["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {calls}"))
}

fn without_session(mut value: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Array(a) => a.iter_mut().for_each(strip),
            Value::Object(o) => {
                o.remove("session");
                o.values_mut().for_each(strip);
            }
            _ => {}
        }
    }
    strip(&mut value);
    value
}

#[test]
fn real_graph_minimal_and_full_are_equivalent_and_read_only() {
    let fixture = Fixture::new("lsp_calls");
    let program = fixture.program();
    let mut previous = None;
    for full in [false, true] {
        fixture.export(&program, full);
        let before = fs::read(&fixture.db).unwrap();
        let mut server = fixture.server();
        let caller = item(&mut server, &fixture.functions("caller")[0]);
        let range: trace_lsp::locations::Range =
            serde_json::from_value(caller["range"].clone()).unwrap();
        let selection: trace_lsp::locations::Range =
            serde_json::from_value(caller["selectionRange"].clone()).unwrap();
        assert!(range.start <= selection.start && selection.end <= range.end);
        let outgoing = calls(&mut server, &caller, false);
        let alpha = named_call(&outgoing, "alpha", false);
        assert_eq!(
            alpha["fromRanges"].as_array().unwrap().len(),
            3,
            "repeated calls plus macro expansion: {outgoing}"
        );
        let text = fs::read_to_string(fixture.root.join("main.cpp")).unwrap();
        let (macro_line, macro_text) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("REQUEST_ALPHA"))
            .unwrap();
        assert!(alpha["fromRanges"].as_array().unwrap().iter().any(|range| range["start"] == json!({"line":macro_line,"character":macro_text.find("REQUEST_ALPHA").unwrap()})));
        let beta = named_call(&outgoing, "beta", false);
        let (beta_line, beta_text) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("/*é😀*/ beta"))
            .unwrap();
        assert_eq!(
            beta["fromRanges"][0]["start"],
            json!({"line":beta_line,"character":beta_text[..beta_text.find("beta").unwrap()].encode_utf16().count()})
        );
        let prototype = named_call(&outgoing, "prototype_only", false);
        assert!(prototype["to"]["detail"]
            .as_str()
            .unwrap()
            .contains("declaration"));
        assert!(outgoing
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["to"]["name"] != "no_source_external"));
        let incoming = calls(&mut server, &alpha["to"], true);
        assert_eq!(
            named_call(&incoming, "caller", true)["fromRanges"],
            alpha["fromRanges"]
        );
        let recursive = item(&mut server, &fixture.functions("recursive")[0]);
        let recursive_out = calls(&mut server, &recursive, false);
        assert_eq!(
            named_call(&recursive_out, "recursive", false)["to"]["data"],
            recursive["data"]
        );
        let recursive_in = calls(&mut server, &recursive, true);
        assert_eq!(
            named_call(&recursive_in, "recursive", true)["from"]["data"],
            recursive["data"]
        );
        let cycle_a = item(&mut server, &fixture.functions("cycle_a")[0]);
        let cycle_out = calls(&mut server, &cycle_a, false);
        let cycle_b = &named_call(&cycle_out, "cycle_b", false)["to"];
        assert_eq!(
            named_call(&calls(&mut server, cycle_b, false), "cycle_a", false)["to"]["data"],
            cycle_a["data"]
        );
        let indirect = item(&mut server, &fixture.functions("indirect")[0]);
        let indirect_out = calls(&mut server, &indirect, false);
        assert_eq!(indirect_out.as_array().unwrap().len(), 2);
        named_call(&indirect_out, "alpha", false);
        named_call(&indirect_out, "beta", false);
        let ambiguous = item(&mut server, &fixture.functions("ambiguous_user")[0]);
        let ambiguous_out = calls(&mut server, &ambiguous, false);
        assert_eq!(ambiguous_out.as_array().unwrap().len(), 2);
        assert_ne!(
            ambiguous_out[0]["to"]["data"],
            ambiguous_out[1]["to"]["data"]
        );
        assert!(ambiguous_out
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["to"]["name"] == "ambiguous_target"));
        let unresolved = item(&mut server, &fixture.functions("unresolved")[0]);
        assert_eq!(calls(&mut server, &unresolved, false), json!([]));
        let overloaded = fixture.functions("overloaded");
        assert_eq!(overloaded.len(), 2);
        let overload_items = overloaded
            .iter()
            .map(|f| item(&mut server, f))
            .collect::<Vec<_>>();
        assert_ne!(overload_items[0]["detail"], overload_items[1]["detail"]);
        assert_ne!(overload_items[0]["data"], overload_items[1]["data"]);
        let helpers = fixture.functions("helper");
        assert_eq!(helpers.len(), 2);
        let helper_items = helpers
            .iter()
            .map(|f| item(&mut server, f))
            .collect::<Vec<_>>();
        assert_ne!(helper_items[0]["uri"], helper_items[1]["uri"]);
        assert_ne!(helper_items[0]["data"], helper_items[1]["data"]);
        let snapshot = without_session(json!([
            caller,
            outgoing,
            incoming,
            recursive_out,
            recursive_in,
            cycle_out,
            indirect_out,
            ambiguous_out,
            overload_items,
            helper_items
        ]));
        if let Some(previous) = &previous {
            assert_eq!(&snapshot, previous);
        }
        previous = Some(snapshot);
        drop(server);
        assert_eq!(
            before,
            fs::read(&fixture.db).unwrap(),
            "serving must not change database bytes"
        );
    }
}

#[test]
fn macro_spelling_and_synthetic_ipc_edges() {
    let fixture = Fixture::new("macro_call_positions");
    fixture.export(&fixture.program(), false);
    let mut server = fixture.server();
    for caller in ["first", "second"] {
        let function = &fixture.functions(caller)[0];
        let caller_item = item(&mut server, function);
        let outgoing = calls(&mut server, &caller_item, false);
        let target = named_call(&outgoing, "target", false);
        assert_eq!(
            target["fromRanges"][0]["start"]["line"],
            function.line_start - 1
        );
        assert_ne!(target["to"]["uri"], caller_item["uri"]);
        let incoming = calls(&mut server, &target["to"], true);
        assert_eq!(
            named_call(&incoming, caller, true)["fromRanges"],
            target["fromRanges"]
        );
    }
    let fixture = Fixture::new("ipc_basic");
    fixture.export(&fixture.program(), false);
    let mut server = fixture.server();
    let proxy = item(&mut server, &fixture.functions("IFooProxy::GetInfo")[0]);
    let outgoing = calls(&mut server, &proxy, false);
    let bridge = named_call(&outgoing, "IFooStub::HandleGetInfo", false);
    assert_eq!(bridge["fromRanges"], json!([]));
    let incoming = calls(&mut server, &bridge["to"], true);
    assert_eq!(
        named_call(&incoming, "IFooProxy::GetInfo", true)["fromRanges"],
        json!([])
    );
}

#[test]
fn exact_paths_duplicate_basenames_and_remote_remapping() {
    let fixture = Fixture::new("lsp_calls");
    fs::create_dir(fixture.root.join("other")).unwrap();
    fs::write(
        fixture.root.join("other/main.cpp"),
        "void other_file() {}\n",
    )
    .unwrap();
    fixture.export(&fixture.program(), false);
    let mut server = fixture.server();
    let other = item(&mut server, &fixture.functions("other_file")[0]);
    assert_eq!(other["name"], "other_file");
    let missing_uri = path_uri(&fixture.root.join("absent/main.cpp")).unwrap();
    assert_eq!(
        server
            .request(
                "textDocument/prepareCallHierarchy",
                json!({"textDocument":{"uri":missing_uri},"position":{"line":0,"character":0}})
            )
            .unwrap(),
        Value::Null
    );
    drop(server);
    let conn = Connection::open(&fixture.db).unwrap();
    let paths = trace_db::editor_file_paths(&conn).unwrap();
    let remote_prefix = local_path(fixture._scratch.path()).join("remote/project");
    for path in paths {
        let remote = remote_prefix
            .join(
                Path::new(&path)
                    .strip_prefix(local_path(&fixture.root))
                    .unwrap(),
            )
            .display()
            .to_string();
        conn.execute("UPDATE files SET path=?1 WHERE path=?2", [&remote, &path])
            .unwrap();
    }
    drop(conn);
    let mapping: PathMapping = format!("{}={}", remote_prefix.display(), fixture.root.display())
        .parse()
        .unwrap();
    let mut mapped = Server::open(&fixture.db, &[mapping]).unwrap();
    mapped.request("initialize", json!({})).unwrap();
    let result = mapped
        .request(
            "textDocument/prepareCallHierarchy",
            json!({"textDocument":{"uri":other["uri"]},"position":{"line":0,"character":0}}),
        )
        .unwrap();
    assert_eq!(result[0]["name"], "other_file");
    assert_eq!(result[0]["uri"], other["uri"]);
}

#[test]
fn target_distinct_identities_and_same_line_candidates() {
    let fixture = Fixture::new("lsp_calls");
    fs::write(
        fixture.root.join("same.cpp"),
        "void same_line_one() {} void same_line_two() {}\n",
    )
    .unwrap();
    let commands = ["main", "twin", "same"].map(|name| json!({"directory":fixture.root,"file":format!("{name}.cpp"),"output":format!("{name}.o"),"arguments":["c++","-c",format!("{name}.cpp"),"-o",format!("{name}.o")]}));
    fs::write(
        fixture.root.join("compile_commands.json"),
        json!(commands).to_string(),
    )
    .unwrap();
    fs::write(fixture.root.join("link_commands.json"), json!([
        {"directory":fixture.root,"output":"app_a","arguments":["c++","main.o","twin.o","same.o","-o","app_a"]},
        {"directory":fixture.root,"output":"app_b","arguments":["c++","main.o","same.o","-o","app_b"]}
    ]).to_string()).unwrap();
    fixture.export(&fixture.program(), false);
    let mut server = fixture.server();
    let callers = fixture.functions("caller");
    assert_eq!(callers.len(), 2);
    let items = callers
        .iter()
        .map(|f| item(&mut server, f))
        .collect::<Vec<_>>();
    assert_ne!(items[0]["data"], items[1]["data"]);
    assert_ne!(items[0]["detail"], items[1]["detail"]);
    for (caller, item) in callers.iter().zip(&items) {
        let outgoing = calls(&mut server, item, false);
        let alpha = &named_call(&outgoing, "alpha", false)["to"];
        let conn = Connection::open(&fixture.db).unwrap();
        let callee = editor_function(
            &conn,
            FnId(alpha["data"]["function"].as_u64().unwrap() as u32),
        )
        .unwrap()
        .unwrap();
        assert_eq!(caller.target_id, callee.target_id);
    }
    let same = fixture.functions("same_line_one");
    let candidates = server.request("textDocument/prepareCallHierarchy", json!({"textDocument":{"uri":path_uri(Path::new(&same[0].path)).unwrap()},"position":{"line":0,"character":30}})).unwrap();
    assert_eq!(
        candidates.as_array().unwrap().len(),
        4,
        "two same-line functions in two images"
    );
}

#[test]
fn validation_unknown_positions_missing_sources_and_stale_data() {
    let fixture = Fixture::new("lsp_calls");
    fixture.export(&fixture.program(), false);
    let function = fixture.functions("caller").remove(0);
    fs::remove_file(&function.path).unwrap();
    let mut server = fixture.server();
    let caller = item(&mut server, &function);
    assert_eq!(
        caller["range"]["end"],
        json!({"line":function.line_end,"character":0})
    );
    assert_eq!(caller["selectionRange"]["start"], caller["range"]["start"]);
    let outgoing = calls(&mut server, &caller, false);
    assert!(outgoing
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["fromRanges"].as_array().unwrap())
        .all(|r| r["start"]["character"] == 0));
    for position in [
        json!({"line":99999,"character":0}),
        json!({"line":u32::MAX,"character":0}),
    ] {
        assert_eq!(
            server
                .request(
                    "textDocument/prepareCallHierarchy",
                    json!({"textDocument":{"uri":caller["uri"]},"position":position})
                )
                .unwrap(),
            Value::Null
        );
    }
    for uri in ["invalid", "https://example.org/main.cpp"] {
        assert_eq!(
            server
                .request(
                    "textDocument/prepareCallHierarchy",
                    json!({"textDocument":{"uri":uri},"position":{"line":0,"character":0}})
                )
                .unwrap_err()
                .code,
            -32602
        );
    }
    let mut invalid = caller.clone();
    invalid["data"]["function"] = json!(u32::MAX);
    assert_eq!(
        server
            .request("callHierarchy/outgoingCalls", json!({"item":invalid}))
            .unwrap_err()
            .code,
        -32602
    );
    for data in [
        Value::Null,
        json!({"function":0}),
        json!({"session":"stale","function":0}),
        json!({"session":caller["data"]["session"],"function":-1}),
    ] {
        let mut invalid = caller.clone();
        invalid["data"] = data;
        assert_eq!(
            server
                .request("callHierarchy/incomingCalls", json!({"item":invalid}))
                .unwrap_err()
                .code,
            -32602
        );
    }
    let mut second = fixture.server();
    assert_eq!(
        second
            .request("callHierarchy/outgoingCalls", json!({"item":caller}))
            .unwrap_err()
            .code,
        -32602
    );
    let mut unlocated = caller.clone();
    unlocated["data"]["function"] = json!(fixture.functions("no_source_external")[0].id.0);
    assert_eq!(
        server
            .request("callHierarchy/outgoingCalls", json!({"item":unlocated}))
            .unwrap_err()
            .code,
        -32602
    );
    let mut renamed = caller.clone();
    renamed["name"] = json!("not-the-function-name");
    assert_eq!(
        calls(&mut server, &renamed, false),
        outgoing,
        "follow-up requests use identity, not name"
    );
}

#[test]
fn startup_rejects_missing_invalid_and_incompatible_databases() {
    let scratch = tempdir().unwrap();
    let missing = scratch.path().join("missing.db");
    assert!(Server::open(&missing, &[]).is_err());
    assert!(!missing.exists());
    let invalid = scratch.path().join("invalid.db");
    fs::write(&invalid, "not a sqlite database").unwrap();
    assert!(Server::open(&invalid, &[]).is_err());
    let empty = scratch.path().join("empty.db");
    drop(Connection::open(&empty).unwrap());
    assert!(Server::open(&empty, &[]).is_err());
    let fixture = Fixture::new("lsp_calls");
    fixture.export(&fixture.program(), false);
    let conn = Connection::open(&fixture.db).unwrap();
    conn.execute("UPDATE analysis_run SET schema_version=999", [])
        .unwrap();
    assert!(Server::open(&fixture.db, &[])
        .err()
        .unwrap()
        .to_string()
        .contains("unsupported"));
    conn.execute("UPDATE analysis_run SET schema_version=7", [])
        .unwrap();
    conn.execute("ALTER TABLE call_sites DROP COLUMN expansion_col", [])
        .unwrap();
    assert!(Server::open(&fixture.db, &[]).is_err());
}

fn store_relative_paths(fixture: &Fixture) {
    let conn = Connection::open(&fixture.db).unwrap();
    let root = fixture.root.canonicalize().unwrap();
    for path in trace_db::editor_file_paths(&conn).unwrap() {
        let relative = Path::new(&path).strip_prefix(&root).unwrap();
        conn.execute(
            "UPDATE files SET path=?1 WHERE path=?2",
            rusqlite::params![relative.to_str().unwrap(), path],
        )
        .unwrap();
    }
}

#[test]
fn relative_paths_use_the_unique_root_across_all_runs() {
    let fixture = Fixture::new("lsp_calls");
    fixture.export(&fixture.program(), false);
    let function = fixture.functions("caller").remove(0);
    store_relative_paths(&fixture);
    let conn = Connection::open(&fixture.db).unwrap();
    let canonical_root = fixture.root.canonicalize().unwrap();
    // A relative root and an equivalent absolute root are unambiguous.
    conn.execute(
        "UPDATE analysis_run SET target_root=?1",
        [fixture.root.file_name().unwrap().to_str().unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO analysis_run(trace_version,schema_version,target_root,created_at,options_json)
         SELECT trace_version,schema_version,?1,created_at,options_json FROM analysis_run",
        [canonical_root.to_str().unwrap()],
    )
    .unwrap();
    let mut server = fixture.server();
    let caller = item(&mut server, &function);
    assert_eq!(caller["uri"], path_uri(Path::new(&function.path)).unwrap());
    assert!(!calls(&mut server, &caller, false)
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn relative_paths_reject_multiple_roots_and_merged_metadata() {
    let fixture = Fixture::new("lsp_calls");
    fixture.export(&fixture.program(), false);
    store_relative_paths(&fixture);
    let conn = Connection::open(&fixture.db).unwrap();
    conn.execute(
        "INSERT INTO analysis_run(trace_version,schema_version,target_root,created_at,options_json)
         SELECT trace_version,schema_version,'other checkout',created_at,options_json FROM analysis_run",
        [],
    )
    .unwrap();
    assert!(Server::open(&fixture.db, &[])
        .err()
        .unwrap()
        .to_string()
        .contains("multiple source roots"));
    conn.execute(
        "DELETE FROM analysis_run WHERE id=(SELECT MAX(id) FROM analysis_run)",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE analysis_run SET target_root='one.db;two.db', options_json=?1",
        [json!({"stage":"merge"}).to_string()],
    )
    .unwrap();
    assert!(Server::open(&fixture.db, &[])
        .err()
        .unwrap()
        .to_string()
        .contains("merged database"));
}

#[test]
fn absolute_paths_ignore_multi_run_and_merged_roots() {
    let fixture = Fixture::new("lsp_calls");
    fixture.export(&fixture.program(), false);
    let function = fixture.functions("caller").remove(0);
    let conn = Connection::open(&fixture.db).unwrap();
    conn.execute(
        "UPDATE analysis_run SET target_root='one.db;two.db', options_json=?1",
        [json!({"stage":"merge"}).to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO analysis_run(trace_version,schema_version,target_root,created_at,options_json)
         SELECT trace_version,schema_version,'other checkout',created_at,options_json FROM analysis_run",
        [],
    )
    .unwrap();
    let mut server = fixture.server();
    let caller = item(&mut server, &function);
    assert_eq!(caller["uri"], path_uri(Path::new(&function.path)).unwrap());
    assert!(!calls(&mut server, &caller, false)
        .as_array()
        .unwrap()
        .is_empty());
    drop(server);
    conn.execute(
        "UPDATE analysis_run SET schema_version=999 WHERE id=(SELECT MAX(id) FROM analysis_run)",
        [],
    )
    .unwrap();
    assert!(
        Server::open(&fixture.db, &[]).is_err(),
        "all run versions remain validated"
    );
}

#[test]
fn startup_waits_for_a_transient_writer_lock() {
    let fixture = Fixture::new("lsp_calls");
    fixture.export(&fixture.program(), false);
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let database = &fixture.db;
        scope.spawn(move || {
            let writer = Connection::open(database).unwrap();
            writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
            writer.execute_batch("COMMIT").unwrap();
        });
        locked_rx.recv().unwrap();
        release_tx.send(()).unwrap();
        let conn = trace_db::open_editor_snapshot(&fixture.db).unwrap();
        let timeout: u32 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
        assert!(conn.execute("DELETE FROM functions", []).is_err());
    });
}

#[test]
fn snapshot_stays_pinned_when_database_is_replaced() {
    let fixture = Fixture::new("lsp_calls");
    let program = fixture.program();
    fixture.export(&program, false);
    let function = fixture.functions("caller").remove(0);
    let mut server = fixture.server();
    let caller = item(&mut server, &function);
    let before = calls(&mut server, &caller, false);
    // Export atomically replaces the file; the existing connection retains
    // the original inode and read transaction until this server is restarted.
    fs::write(fixture.root.join("main.cpp"), "void replacement() {}\n").unwrap();
    fixture.export(&fixture.program(), false);
    assert_eq!(calls(&mut server, &caller, false), before);
    assert!(fixture.functions("caller").is_empty());
    assert_eq!(fixture.functions("replacement").len(), 1);
}

#[test]
fn dependency_declarations_remain_navigable() {
    let fixture = Fixture::new("lsp_calls");
    let dependency = fixture.root.join("dep");
    fs::create_dir(&dependency).unwrap();
    fs::write(
        dependency.join("api.h"),
        "static inline void dependency_api(void) {}\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("dep_user.c"),
        "#include \"dep/api.h\"\nvoid dep_user(void) { dependency_api(); }\n",
    )
    .unwrap();
    let options = trace_preproc::PreprocessOptions::new()
        .with_include(fixture.root.clone())
        .with_dep(local_path(&dependency));
    let program = trace_parse::build_program(&fixture.root, &options).unwrap();
    fixture.export(&program, false);
    let mut server = fixture.server();
    let caller = item(&mut server, &fixture.functions("dep_user")[0]);
    let outgoing = calls(&mut server, &caller, false);
    let declaration = &named_call(&outgoing, "dependency_api", false)["to"];
    assert!(declaration["detail"]
        .as_str()
        .unwrap()
        .contains("declaration; dependency"));
    assert!(declaration["uri"].as_str().unwrap().ends_with("api.h"));
}

#[test]
fn shared_queries_use_existing_indexes() {
    let fixture = Fixture::new("lsp_calls");
    fixture.export(&fixture.program(), false);
    let conn = trace_db::open_editor_snapshot(&fixture.db).unwrap();
    assert!(conn.execute("DELETE FROM functions", []).is_err());
    for column in ["caller_fn_id", "callee_fn_id"] {
        let sql = format!("EXPLAIN QUERY PLAN SELECT * FROM call_edges WHERE {column}=0");
        let details: Vec<String> = conn
            .prepare(&sql)
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            details.iter().any(|s| s.contains("USING INDEX")),
            "{details:?}"
        );
    }
    let path = fixture.functions("caller")[0].path.clone();
    assert!(editor_functions_at(&conn, &path, 19)
        .unwrap()
        .iter()
        .any(|f| f.name == "caller"));
}
