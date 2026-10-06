use serde_json::{json, Value};
use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use trace_lsp::{
    locations::{local_path, path_uri},
    transport::{read_frame, write_frame},
};

struct Client {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Client {
    fn start(db: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_trace-lsp"))
            .arg("--db")
            .arg(db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }

    fn send(&mut self, value: Value) {
        write_frame(&mut self.input, &value).unwrap();
    }

    fn response(&mut self) -> Value {
        serde_json::from_slice(
            &read_frame(&mut self.output)
                .unwrap()
                .expect("framed response"),
        )
        .unwrap()
    }

    fn request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let response = self.response();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        response
    }

    fn notify(&mut self, method: &str) {
        self.send(json!({"jsonrpc":"2.0","method":method}));
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_lifecycle_call_hierarchy_and_errors() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("a space.c");
    std::fs::write(
        &source,
        "void callee(void) {}\nvoid caller(void) { callee(); callee(); }\n",
    )
    .unwrap();
    let db = scratch.path().join("analysis.db");
    let program =
        trace_parse::build_program(scratch.path(), &trace_preproc::PreprocessOptions::new())
            .unwrap();
    let (pag, analysis) = trace_analysis::analyze(&program);
    trace_db::export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &trace_db::ExportOptions::minimal(db.clone()),
    )
    .unwrap();
    let original = std::fs::read(&db).unwrap();
    let mut client = Client::start(&db);
    assert_eq!(
        client.request(json!(0), "callHierarchy/outgoingCalls", json!({}))["error"]["code"],
        -32002
    );
    client.notify("initialized"); // pre-initialization notifications are ignored
    let initialized = client.request(json!("init"), "initialize", json!({"capabilities":{}}));
    assert_eq!(
        initialized["result"]["capabilities"],
        json!({"callHierarchyProvider":true,"positionEncoding":"utf-16"})
    );
    client.notify("initialized");
    assert_eq!(
        client.request(json!(1), "initialize", json!({}))["error"]["code"],
        -32600
    );
    let prepared = client.request(json!(2), "textDocument/prepareCallHierarchy", json!({
        "textDocument":{"uri":path_uri(&local_path(&source)).unwrap()},"position":{"line":1,"character":12}
    }));
    let caller = &prepared["result"][0];
    assert_eq!(caller["name"], "caller");
    let outgoing = client.request(
        json!(3),
        "callHierarchy/outgoingCalls",
        json!({"item":caller}),
    );
    assert_eq!(outgoing["result"].as_array().unwrap().len(), 1);
    assert_eq!(outgoing["result"][0]["to"]["name"], "callee");
    assert_eq!(
        outgoing["result"][0]["fromRanges"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let incoming = client.request(
        json!(4),
        "callHierarchy/incomingCalls",
        json!({"item":outgoing["result"][0]["to"]}),
    );
    assert_eq!(incoming["result"][0]["from"]["data"], caller["data"]);
    assert_eq!(
        incoming["result"][0]["fromRanges"],
        outgoing["result"][0]["fromRanges"]
    );
    assert_eq!(
        client.request(json!(5), "textDocument/hover", json!({}))["error"]["code"],
        -32601
    );
    assert_eq!(
        client.request(
            json!(6),
            "textDocument/prepareCallHierarchy",
            json!({"position":{"line":-1}})
        )["error"]["code"],
        -32602
    );
    assert_eq!(
        client.request(
            json!(7),
            "callHierarchy/incomingCalls",
            json!({"item":{"data":null}})
        )["error"]["code"],
        -32602
    );
    client.notify("unknown/notification");
    client.notify("textDocument/didChange");
    client
        .input
        .write_all(b"Content-Length: 1\r\n\r\n{")
        .unwrap();
    client.input.flush().unwrap();
    let error = client.response();
    assert!(error["id"].is_null());
    assert_eq!(error["error"]["code"], -32700);
    client.send(json!({"jsonrpc":"1.0","id":8,"method":"bad"}));
    assert_eq!(client.response()["error"]["code"], -32600);
    assert!(client.request(json!(9), "shutdown", Value::Null)["result"].is_null());
    assert_eq!(
        client.request(
            json!(10),
            "callHierarchy/outgoingCalls",
            json!({"item":caller})
        )["error"]["code"],
        -32600
    );
    client.notify("exit");
    assert!(client.child.wait().unwrap().success());
    assert!(
        read_frame(&mut client.output).unwrap().is_none(),
        "stdout contains only framed responses"
    );
    assert_eq!(original, std::fs::read(db).unwrap());
}

#[test]
fn startup_failures_write_only_stderr_and_never_create_database() {
    let scratch = tempfile::tempdir().unwrap();
    let missing = scratch.path().join("missing.db");
    let output = Command::new(env!("CARGO_BIN_EXE_trace-lsp"))
        .arg("--db")
        .arg(&missing)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open analysis database"));
    assert!(!missing.exists());
}

#[test]
fn exit_before_shutdown_has_failure_status() {
    let scratch = tempfile::tempdir().unwrap();
    let source = scratch.path().join("empty.c");
    std::fs::write(source, "void function(void) {}\n").unwrap();
    let db = scratch.path().join("analysis.db");
    let program =
        trace_parse::build_program(scratch.path(), &trace_preproc::PreprocessOptions::new())
            .unwrap();
    let (pag, analysis) = trace_analysis::analyze(&program);
    trace_db::export_to_sqlite(
        &program,
        &pag,
        &analysis,
        &trace_db::ExportOptions::minimal(db.clone()),
    )
    .unwrap();
    let mut client = Client::start(&db);
    client.notify("exit");
    assert_eq!(client.child.wait().unwrap().code(), Some(1));
    assert!(read_frame(&mut client.output).unwrap().is_none());
}
