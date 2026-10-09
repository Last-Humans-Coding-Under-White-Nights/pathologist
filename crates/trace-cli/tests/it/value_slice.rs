//! Value slice (#205): from a variable or field access, up to the value's
//! sources, then down to its sinks, each node annotated with the execution
//! contexts that reach it and each edge where the value may change hands
//! between contexts flagged.

use crate::common;

use common::*;
use rusqlite::Connection;
use std::sync::OnceLock;
use trace_db::{
    render_slice, resolve_slice_start, value_slice, RenderFormat, SliceEdge, SliceNode,
    SliceOptions, ValueSlice,
};

const FILE: &str = "value_slice/main.cpp";

fn db() -> &'static TempDb {
    static DB: OnceLock<TempDb> = OnceLock::new();
    DB.get_or_init(|| cli_analyze(&fixture("value_slice"), &[]))
}

fn conn() -> Connection {
    Connection::open(db().path()).unwrap()
}

/// 1-based `(line, col)` of the `nth` (0-based) occurrence of `needle` in
/// the fixture, plus `offset` columns.
fn pos(needle: &str, nth: usize, offset: i64) -> (i64, i64) {
    let text = std::fs::read_to_string(fixture("value_slice").join("main.cpp")).unwrap();
    let at = text
        .match_indices(needle)
        .nth(nth)
        .unwrap_or_else(|| panic!("occurrence {nth} of `{needle}`"))
        .0;
    let before = &text[..at];
    let line = before.matches('\n').count() as i64 + 1;
    let col = (at - before.rfind('\n').map_or(0, |n| n + 1)) as i64 + 1;
    (line, col + offset)
}

fn slice_with(needle: &str, nth: usize, offset: i64, opts: SliceOptions) -> ValueSlice {
    let conn = conn();
    let (line, col) = pos(needle, nth, offset);
    let start = resolve_slice_start(&conn, FILE, line, col, None).unwrap();
    value_slice(&conn, &start, &opts).unwrap()
}

fn slice(needle: &str, nth: usize, offset: i64) -> ValueSlice {
    slice_with(needle, nth, offset, SliceOptions::default())
}

fn find_node<'a>(s: &'a ValueSlice, label: &str, function: Option<&str>) -> Option<&'a SliceNode> {
    s.nodes
        .iter()
        .find(|n| n.label == label && n.function.as_deref() == function)
}

fn node<'a>(s: &'a ValueSlice, label: &str, function: Option<&str>) -> &'a SliceNode {
    find_node(s, label, function).unwrap_or_else(|| {
        panic!(
            "node `{label}` in {function:?}: {:#?}",
            s.nodes
                .iter()
                .map(|n| (&n.label, &n.function))
                .collect::<Vec<_>>()
        )
    })
}

fn edge<'a>(s: &'a ValueSlice, from: &SliceNode, kind: &str, to: &SliceNode) -> &'a SliceEdge {
    s.edges
        .iter()
        .find(|e| e.from == from.id && e.to == to.id && e.kind == kind)
        .unwrap_or_else(|| panic!("edge {} -{kind}-> {}: {:#?}", from.label, to.label, s.edges))
}

fn context_entries(s: &ValueSlice, node: &SliceNode) -> Vec<String> {
    let mut out: Vec<String> = node
        .contexts
        .iter()
        .map(|id| {
            let c = s.contexts.iter().find(|c| &c.id == id).unwrap();
            format!("{}:{}", c.kind, c.entry.as_deref().unwrap_or("-"))
        })
        .collect();
    out.sort();
    out
}

const CALLBACK: &str = "summary:IDeviceStub.callback_";

/// The function a `flow_origins` row `o` is written in, by the edge-scope
/// rule (`docs/ANALYSIS.md`, "Where a value moves"): the innermost
/// definition whose line range in the row's file holds its line, strictly
/// inside it when several hold the line.
const SITE_FUNCTION: &str = "(SELECT CASE WHEN COUNT(DISTINCT f.id) = 1 THEN MIN(f.name) END \
     FROM functions f WHERE f.file_id = o.file_id AND f.is_defined = 1 \
       AND o.line BETWEEN f.line_start AND f.line_end \
       AND NOT EXISTS (SELECT 1 FROM functions g \
         WHERE g.file_id = o.file_id AND g.is_defined = 1 AND g.id <> f.id \
           AND o.line BETWEEN g.line_start AND g.line_end \
           AND NOT (g.line_start <= f.line_start AND f.line_end <= g.line_end \
                    AND o.line > f.line_start AND o.line < f.line_end)))";

/// The memory edges `flow_memory_access` records, as a table `(src_node,
/// dst_node, kind, op_src, op_dst, op_kind)`: `mem_read` from each cell a
/// load reads into its destination, `mem_write` from a store's value into
/// each cell it writes, with the `flow_edges` key of the load or store,
/// whose `flow_origins` rows are the edge's sites.
const MEMORY_EDGES: &str = "(SELECT \
       CASE e.kind WHEN 'load' THEN a.cell_node ELSE e.src_node END AS src_node, \
       CASE e.kind WHEN 'load' THEN e.dst_node ELSE a.cell_node END AS dst_node, \
       CASE e.kind WHEN 'load' THEN 'mem_read' ELSE 'mem_write' END AS kind, \
       e.src_node AS op_src, e.dst_node AS op_dst, e.kind AS op_kind \
     FROM flow_memory_access a JOIN flow_edges e ON e.id = a.edge_id \
     WHERE a.cell_node IS NOT NULL)";

/// Joins `flow_origins o` to the load or store of memory edge `m`.
const MEMORY_ORIGINS: &str = "JOIN flow_origins o ON o.src_node = m.op_src \
     AND o.dst_node = m.op_dst AND o.kind = m.op_kind";

#[test]
fn memory_edges_join_a_store_and_a_load_of_one_member() {
    let conn = conn();
    // A temporary's number is the unit's, not the test's business.
    let untemp = |name: String| -> String {
        if name.starts_with('_') || name.starts_with('$') {
            name.trim_end_matches(|c: char| c.is_ascii_digit())
                .to_string()
        } else {
            name
        }
    };
    // Each memory access names its load or store; where it happens is
    // that operation's `flow_origins` rows, the single record of sites.
    let rows: Vec<(String, String, String, String, i64)> = conn
        .prepare(&format!(
            "SELECT DISTINCT m.kind, COALESCE(v1.name, n1.label), COALESCE(v2.name, n2.label), \
                    {SITE_FUNCTION}, o.line \
             FROM {MEMORY_EDGES} m {MEMORY_ORIGINS} \
             JOIN flow_nodes n1 ON n1.id = m.src_node LEFT JOIN variables v1 ON v1.id = n1.var_id \
             JOIN flow_nodes n2 ON n2.id = m.dst_node LEFT JOIN variables v2 ON v2.id = n2.var_id \
             WHERE n1.label LIKE '%callback_' OR n2.label LIKE '%callback_' \
             ORDER BY m.kind, o.line"
        ))
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .map(|(k, a, b, f, l)| (k, untemp(a), untemp(b), f, l))
        .collect();
    let row = |kind: &str, from: &str, to: &str, function: &str, needle: &str| {
        (
            kind.to_string(),
            from.to_string(),
            to.to_string(),
            function.to_string(),
            pos(needle, 0, 0).0,
        )
    };
    assert_eq!(
        rows,
        vec![
            row(
                "mem_read",
                CALLBACK,
                "cb",
                "IDeviceStub::Report",
                "cb = callback_"
            ),
            // Each call through the member reads it into a receiver
            // temporary.
            row(
                "mem_read",
                CALLBACK,
                "_load",
                "IDeviceStub::Notify",
                "callback_->OnError"
            ),
            (
                "mem_read".to_string(),
                CALLBACK.to_string(),
                "_load".to_string(),
                "IDeviceStub::Notify".to_string(),
                pos("callback_->OnError", 1, 0).0,
            ),
            row(
                "mem_read",
                CALLBACK,
                "_load",
                "IDeviceStub::Current",
                "return callback_"
            ),
            row(
                "mem_write",
                "cb",
                CALLBACK,
                "IDeviceStub::SetCallback",
                "callback_ = cb"
            ),
        ]
    );
}

#[test]
fn a_field_access_starts_at_the_cells_it_designates() {
    let conn = conn();
    let (line, col) = pos("callback_;", 0, 2);
    let start = resolve_slice_start(&conn, FILE, line, col, None).unwrap();
    assert_eq!(start.kind, "field");
    assert_eq!(start.name, "callback_");
    assert!(start.at.path.ends_with(FILE), "{:?}", start.at);
    let labels: Vec<String> = start
        .nodes
        .iter()
        .map(|&n| {
            conn.query_row("SELECT label FROM flow_nodes WHERE id = ?1", [n], |r| {
                r.get(0)
            })
            .unwrap()
        })
        .collect();
    assert_eq!(labels, vec![CALLBACK.to_string()]);
    // The identifier given explicitly picks the same cells without the source.
    let named = resolve_slice_start(&conn, FILE, line, col, Some("callback_")).unwrap();
    assert_eq!(named, start);
}

#[test]
fn a_declaration_starts_at_the_variable() {
    let conn = conn();
    let (line, col) = pos("*listener = new", 0, 1);
    let start = resolve_slice_start(&conn, FILE, line, col, None).unwrap();
    assert_eq!(
        (start.kind.as_str(), start.name.as_str()),
        ("variable", "listener")
    );
    assert!(!start.nodes.is_empty());
}

#[test]
fn a_position_naming_nothing_is_an_error() {
    let conn = conn();
    let (line, _) = pos("// A chain of copies", 0, 0);
    let err = resolve_slice_start(&conn, FILE, line, 5, None).unwrap_err();
    assert!(
        err.to_string().contains("no variable or field access"),
        "{err}"
    );
}

#[test]
fn a_position_between_values_asks_for_the_name() {
    let conn = conn();
    // On the `=` of `alias = cb`: two variables move, no identifier picks one.
    let (line, col) = pos("alias = cb", 0, 6);
    let err = resolve_slice_start(&conn, FILE, line, col, None).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("alias") && msg.contains("cb") && msg.contains("--name"),
        "{msg}"
    );
    let named = resolve_slice_start(&conn, FILE, line, col, Some("cb")).unwrap();
    assert_eq!(
        (named.kind.as_str(), named.name.as_str()),
        ("variable", "cb")
    );
}

#[test]
fn a_file_filter_matching_several_files_is_refused() {
    // Line 3, column 10 is a use of `aa` in one file and the declaration of
    // `qq` in the other.
    let dir = scratch(&[
        (
            "a/main.c",
            "int *ga;\nvoid Fa(int *aa) {\n    ga = aa;\n}\n",
        ),
        (
            "b/main.c",
            "int *gb;\nvoid Fb(int *bb) {\n    int *qq = bb;\n    gb = qq;\n}\n",
        ),
    ]);
    let db = cli_analyze(dir.path(), &[]);
    let conn = Connection::open(db.path()).unwrap();
    let err = resolve_slice_start(&conn, "main.c", 3, 10, None).unwrap_err();
    assert!(err.to_string().contains("give more of the path"), "{err}");
    let b = resolve_slice_start(&conn, "b/main.c", 3, 10, None).unwrap();
    assert_eq!((b.kind.as_str(), b.name.as_str()), ("variable", "qq"));
    let a = resolve_slice_start(&conn, "a/main.c", 3, 10, None).unwrap();
    assert_eq!((a.kind.as_str(), a.name.as_str()), ("variable", "aa"));
}

#[test]
fn stage_one_reaches_the_allocation_behind_the_member() {
    let s = slice("callback_;", 0, 2);
    let start = node(&s, CALLBACK, None);
    assert!(start.roles.contains(&"start".to_string()), "{start:?}");
    let writer = node(&s, "cb", Some("IDeviceStub::SetCallback"));
    assert!(writer.stages.contains(&"up".to_string()));
    edge(&s, writer, "mem_write", start);
    // main's `new Listener()` reaches the member through `SetCallback(listener)`.
    let heap = s
        .nodes
        .iter()
        .find(|n| n.source.as_deref() == Some("allocation"))
        .unwrap_or_else(|| panic!("an allocation source: {:#?}", s.nodes));
    assert!(heap.roles.contains(&"source".to_string()));
    let alloc = s
        .edges
        .iter()
        .find(|e| e.from == heap.id && e.kind == "addr_of")
        .expect("the allocation's addr_of edge");
    assert_eq!(alloc.function.as_deref(), Some("main"));
    assert_eq!(
        alloc.site.as_ref().unwrap().line,
        pos("*listener = new", 0, 1).0
    );
    assert!(!s.truncated_up);
}

#[test]
fn stage_two_reaches_the_reader_and_its_sinks() {
    let s = slice("callback_;", 0, 2);
    let start = node(&s, CALLBACK, None);
    let reader = node(&s, "cb", Some("IDeviceStub::Report"));
    assert!(reader.stages.contains(&"down".to_string()));
    edge(&s, start, "mem_read", reader);
    let alias = node(&s, "alias", Some("IDeviceStub::Report"));
    edge(&s, reader, "copy", alias);
    assert!(alias.roles.contains(&"sink".to_string()), "{alias:?}");
}

#[test]
fn a_call_through_the_member_reads_it() {
    let s = slice("callback_;", 0, 2);
    let start = node(&s, CALLBACK, None);
    let line = pos("callback_->OnError", 0, 0).0;
    let read = s
        .edges
        .iter()
        .find(|e| {
            e.from == start.id
                && e.kind == "mem_read"
                && e.function.as_deref() == Some("IDeviceStub::Notify")
        })
        .unwrap_or_else(|| panic!("the receiver read in Notify: {:#?}", s.edges));
    assert_eq!(read.site.as_ref().unwrap().line, line);
    assert!(read.cross_context, "{read:?}");
}

#[test]
fn a_getter_hands_the_member_to_every_caller() {
    let s = slice("callback_;", 0, 2);
    node(&s, "p1", Some("IDeviceStub::Probe1"));
    node(&s, "p2", Some("IDeviceStub::Probe2"));
}

#[test]
fn an_allocation_returns_only_to_the_call_it_came_from() {
    let s = slice("*mineA = Alloc", 0, 1);
    assert!(s
        .nodes
        .iter()
        .any(|n| n.source.as_deref() == Some("allocation")));
    node(&s, "made", Some("Alloc"));
    assert!(
        find_node(&s, "mineB", Some("UseB")).is_none(),
        "UseB's call allocates its own object: {:#?}",
        s.nodes
    );
}

#[test]
fn a_pass_through_returns_the_value_to_the_call_it_came_in_through() {
    let s = slice("*c = a", 0, 1);
    assert_eq!(s.start.name, "c");
    let x = node(&s, "x", Some("Pass"));
    assert!(
        !x.roles.contains(&"sink".to_string()),
        "the value goes on out of Pass: {x:?}"
    );
    let b = node(&s, "b", Some("Relay"));
    edge(&s, x, "copy", b);
    assert!(b.roles.contains(&"sink".to_string()), "{b:?}");
    assert!(
        find_node(&s, "other", Some("Bystander")).is_none(),
        "Bystander's call passes its own value: {:#?}",
        s.nodes
    );
}

#[test]
fn a_recursive_function_nothing_calls_is_roots_code() {
    let s = slice("hook_ = v", 0, 0);
    let start = node(&s, "summary:Walker.hook_", None);
    let v = node(&s, "v", Some("Walker::Walk"));
    assert_eq!(context_entries(&s, v), ["root:-"]);
    assert_eq!(
        v.source.as_deref(),
        Some("parameter"),
        "only the recursion passes `v` a value: {v:?}"
    );
    let e = edge(&s, v, "mem_write", start);
    assert!(e.cross_context, "{e:?}");
    assert!(e.reasons.contains(&"contexts_differ".to_string()), "{e:?}");
    assert_eq!(
        context_entries(&s, start),
        ["root:-", "thread:Walker::Watch"]
    );
}

#[test]
fn memory_a_thread_started_in_a_loop_touches_alone_is_self_concurrent() {
    let s = slice("last_ = got", 0, 0);
    let start = node(&s, "summary:Pool.last_", None);
    let got = node(&s, "got", Some("Pool::Work"));
    let e = edge(&s, got, "mem_write", start);
    assert!(e.cross_context, "{e:?}");
    assert_eq!(e.reasons, ["self_concurrent"]);
    let worker = s
        .contexts
        .iter()
        .find(|c| c.entry.as_deref() == Some("Pool::Work"))
        .unwrap();
    assert_eq!(worker.multi_instance.as_deref(), Some("loop"));
    assert!(worker.self_concurrent);
    assert_eq!(context_entries(&s, start), ["thread:Pool::Work"]);
}

#[test]
fn the_ipc_write_and_the_thread_read_are_cross_context() {
    let s = slice("callback_;", 0, 2);
    let start = node(&s, CALLBACK, None);
    let writer = node(&s, "cb", Some("IDeviceStub::SetCallback"));
    let reader = node(&s, "cb", Some("IDeviceStub::Report"));
    // `main` calls the handler too, but a handler is reached by its own
    // context only. (`root` reads the member through the getter.)
    assert_eq!(
        context_entries(&s, writer),
        ["ipc_handler:IDeviceStub::SetCallback"]
    );
    assert_eq!(context_entries(&s, reader), ["thread:IDeviceStub::Report"]);
    assert_eq!(
        context_entries(&s, start),
        [
            "ipc_handler:IDeviceStub::SetCallback",
            "root:-",
            "thread:IDeviceStub::Report"
        ]
    );
    for e in [
        edge(&s, writer, "mem_write", start),
        edge(&s, start, "mem_read", reader),
    ] {
        assert!(e.cross_context, "{e:?}");
        assert!(e.reasons.contains(&"contexts_differ".to_string()), "{e:?}");
    }
    let thread = s
        .contexts
        .iter()
        .find(|c| c.entry.as_deref() == Some("IDeviceStub::Report"))
        .unwrap();
    assert_eq!(thread.api.as_deref(), Some("std::thread"));
    assert_eq!(
        thread.start_site.as_ref().unwrap().line,
        pos("worker_ = std::thread", 0, 0).0
    );
    let ipc = s.contexts.iter().find(|c| c.kind == "ipc_handler").unwrap();
    assert!(ipc.self_concurrent);
}

#[test]
fn a_copy_within_one_invocation_is_not_flagged() {
    let s = slice("callback_;", 0, 2);
    let reader = node(&s, "cb", Some("IDeviceStub::Report"));
    let alias = node(&s, "alias", Some("IDeviceStub::Report"));
    let e = edge(&s, reader, "copy", alias);
    assert!(!e.cross_context, "{e:?}");
    assert_eq!(reader.sharing, "private");
}

#[test]
fn a_value_handed_to_a_thread_start_is_flagged() {
    let s = slice("buf = new", 0, 0);
    let produced = node(&s, "buf", Some("Produce"));
    let consumed = node(&s, "buf", Some("Consume"));
    let e = s
        .edges
        .iter()
        .find(|e| e.from == produced.id && e.to == consumed.id)
        .expect("the hand-off to the thread");
    assert!(e.cross_context, "{e:?}");
    assert_eq!(e.reasons, ["start"]);
    assert_eq!(context_entries(&s, consumed), ["thread:Consume"]);
    assert_eq!(context_entries(&s, produced), ["root:-"]);
}

#[test]
fn memory_one_ipc_handler_touches_alone_is_self_concurrent() {
    let s = slice("spare_ = next", 0, 0);
    let start = node(&s, "summary:IDeviceStub.spare_", None);
    let next = node(&s, "next", Some("IDeviceStub::Swap"));
    let e = edge(&s, next, "mem_write", start);
    assert!(e.cross_context, "{e:?}");
    assert_eq!(e.reasons, ["self_concurrent"]);
    assert_eq!(
        context_entries(&s, start),
        ["ipc_handler:IDeviceStub::Swap"]
    );
}

#[test]
fn a_local_structs_field_is_the_invocations_own() {
    // `Post` is a self-concurrent IPC handler. Its local `opt`'s field cell
    // is one request's alone; the heap copy's field cell is not.
    let s = slice("ICallback *f)\n    {\n        MessageOption opt", 0, 11);
    assert_eq!(s.start.name, "f");
    let f = node(&s, "f", Some("IOptionStub::Post"));
    let local = node(&s, "cb of opt", Some("IOptionStub::Post"));
    assert_eq!(local.sharing, "private", "{local:?}");
    let e = edge(&s, f, "mem_write", local);
    assert!(!e.cross_context, "{e:?}");
    let heap = node(&s, "cb", None);
    assert_eq!(heap.loc_kind.as_deref(), Some("field"));
    assert_eq!(heap.sharing, "shared", "{heap:?}");
    let e = edge(&s, f, "mem_write", heap);
    assert_eq!(e.reasons, ["self_concurrent"], "{e:?}");
}

#[test]
fn memory_one_thread_keeps_to_itself_is_not_flagged() {
    let s = slice("mine = new", 0, 0);
    node(&s, "also", Some("Own"));
    let flagged: Vec<_> = s.edges.iter().filter(|e| e.cross_context).collect();
    assert!(flagged.is_empty(), "{flagged:#?}");
}

#[test]
fn a_lambda_task_writing_through_a_reference_capture_runs_in_its_thread() {
    // The thread body is a lambda written inside `Reset`. Its statement
    // `gp = local;` is the lambda's (the innermost definition holding the
    // line), not `Reset`'s, although `local` is `Reset`'s own local.
    let src = "namespace std { class thread { public: template<class F, class... A> \
               thread(F f, A... a); void join(); }; }\n\
               int g;\nint *gp;\n\
               void Reset() {\n    int *local = &g;\n    std::thread t([&local]() {\n        \
               gp = local;\n    });\n}\n\
               void Use() {\n    int *p = gp;\n    *p = 1;\n}\n\
               int main() {\n    Reset();\n    Use();\n    return 0;\n}\n";
    let (dir, _db, conn) = scratch_db(&[("main.cpp", src)]);
    let file = dir.path().join("main.cpp");
    let line = src.lines().position(|l| l.contains("int *p = gp")).unwrap();
    let col = col_of(src.lines().nth(line).unwrap(), "gp", 0);
    let start =
        resolve_slice_start(&conn, file.to_str().unwrap(), line as i64 + 1, col, None).unwrap();
    let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
    let local = node(&s, "local", Some("Reset"));
    let gp = node(&s, "gp", None);
    let write = edge(&s, local, "copy", gp);
    assert!(
        write
            .function
            .as_deref()
            .is_some_and(|f| f.starts_with("Reset::$lambda")),
        "{write:?}"
    );
    assert!(write.cross_context, "{write:?}");
    assert!(
        write.reasons.contains(&"contexts_differ".to_string()),
        "{write:?}"
    );
    let entries = context_entries(&s, gp);
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0], "root:-");
    assert!(
        entries[1].starts_with("thread:Reset::$lambda"),
        "{entries:?}"
    );
}

#[test]
fn a_lambda_reading_and_writing_a_reference_capture_is_followed_and_flagged() {
    // `local` is `Reset`'s; the thread's lambda reads it (`copy = local`) and
    // writes it (`local = other`). Neither is a call's return or entry: the
    // read is followed down into the lambda, and both move the value
    // between `Reset`'s context and the thread's.
    let src = "namespace std { class thread { public: template<class F, class... A> \
               thread(F f, A... a); void join(); }; }\n\
               int g;\nint h;\n\
               void Reset()\n{\n    int *local = &g;\n    int *other = &h;\n    \
               std::thread t([&local, &other]() {\n        int *copy = local;\n        \
               local = other;\n    });\n    int *also = local;\n}\n\
               int main()\n{\n    Reset();\n    return 0;\n}\n";
    let (dir, _db, conn) = scratch_db(&[("main.cpp", src)]);
    let s = scratch_slice(&conn, &dir, "main.cpp", src, "also = local");
    let local = node(&s, "local", Some("Reset"));
    let other = node(&s, "other", Some("Reset"));
    let copy = node(&s, "copy", Some("Reset::$lambda8:19"));
    assert!(!s.truncated_up && !s.truncated_down, "{s:#?}");
    let read = edge(&s, local, "copy", copy);
    assert!(read.stages.contains(&"down".to_string()), "{read:?}");
    let write = edge(&s, other, "copy", local);
    for e in [read, write] {
        assert_eq!(e.function.as_deref(), Some("Reset::$lambda8:19"), "{e:?}");
        assert!(e.cross_context, "{e:?}");
        assert!(e.reasons.contains(&"contexts_differ".to_string()), "{e:?}");
    }
    let outer = edge(&s, local, "copy", node(&s, "also", Some("Reset")));
    assert!(!outer.cross_context, "{outer:?}");
    assert!(
        context_entries(&s, copy)
            .iter()
            .any(|c| c.starts_with("thread:Reset::$lambda")),
        "{:?}",
        context_entries(&s, copy)
    );
}

#[test]
fn a_return_out_of_a_lambda_is_still_a_return() {
    // `make` is a lambda written in `F`, but `other = make();` is a return
    // into `F`, written in `F`'s body: from the allocation, the value
    // returns only along the calls stage 1 came up through (`Helper`'s), not
    // to `F`'s own call of the lambda.
    let src = "struct Buffer { Buffer *next; };\n\
               Buffer *Helper(Buffer *(*mk)())\n{\n    return mk();\n}\n\
               void F()\n{\n    auto make = []() {\n        Buffer *made = new Buffer();\n        \
               return made;\n    };\n    Buffer *other = make();\n    \
               Buffer *selected = Helper(make);\n    Buffer *also = selected;\n}\n\
               int main()\n{\n    F();\n    return 0;\n}\n";
    let (dir, _db, conn) = scratch_db(&[("main.cpp", src)]);
    let s = scratch_slice(&conn, &dir, "main.cpp", src, "also = selected");
    assert!(
        find_node(&s, "made", Some("F::$lambda8:17")).is_some(),
        "{:#?}",
        s.nodes
    );
    assert!(
        find_node(&s, "selected", Some("F")).is_some(),
        "{:#?}",
        s.nodes
    );
    assert!(
        find_node(&s, "other", Some("F")).is_none(),
        "{:#?}",
        s.nodes
    );
    assert!(!s.truncated_down, "{s:#?}");
}

#[test]
fn a_field_summary_reached_from_elsewhere_is_a_boundary() {
    // The stub's `SetCallback(ICallback *cb)` parameter, not the proxy's.
    let s = slice("ICallback *cb)\n    {\n        callback_", 0, 11);
    assert_eq!(s.start.name, "cb");
    let summary = node(&s, CALLBACK, None);
    assert!(
        summary.roles.contains(&"boundary".to_string()),
        "{summary:?}"
    );
    assert!(
        find_node(&s, "cb", Some("IDeviceStub::Report")).is_none(),
        "a boundary is not expanded"
    );
}

#[test]
fn a_global_reached_is_a_boundary() {
    let s = slice("seen = g_listener", 0, 0);
    let global = node(&s, "g_listener", None);
    assert!(global.roles.contains(&"boundary".to_string()), "{global:?}");
    assert_eq!(global.source.as_deref(), Some("global"));
    assert_eq!(global.sharing, "shared");
    assert!(
        find_node(&s, "$new", Some("InstallGlobal")).is_none()
            && !s
                .nodes
                .iter()
                .any(|n| n.source.as_deref() == Some("allocation")),
        "the global's own writer is past the boundary: {:#?}",
        s.nodes
    );
}

#[test]
fn a_global_start_is_expanded_both_ways() {
    let s = slice("g_listener = nullptr", 0, 0);
    assert_eq!(
        (s.start.kind.as_str(), s.start.name.as_str()),
        ("variable", "g_listener")
    );
    assert!(
        s.nodes
            .iter()
            .any(|n| n.source.as_deref() == Some("allocation")),
        "InstallGlobal's allocation: {:#?}",
        s.nodes
    );
    node(&s, "seen", Some("FireGlobal"));
}

#[test]
fn a_static_member_start_is_expanded_both_ways() {
    let s = slice("current_ = cb", 0, 0);
    assert_eq!(
        (s.start.kind.as_str(), s.start.name.as_str()),
        ("variable", "current_")
    );
    node(&s, "cb", Some("Registry::Set"));
    node(&s, "now", Some("Registry::Fire"));
}

#[test]
fn depth_limits_bound_each_stage() {
    let short = slice_with(
        "e = d",
        0,
        0,
        SliceOptions {
            up_depth: 2,
            down_depth: 6,
        },
    );
    assert!(short.truncated_up);
    node(&short, "c", Some("Chain"));
    assert!(find_node(&short, "b", Some("Chain")).is_none());

    let long = slice_with(
        "e = d",
        0,
        0,
        SliceOptions {
            up_depth: 10,
            down_depth: 6,
        },
    );
    assert!(!long.truncated_up);
    node(&long, "a", Some("Chain"));
    assert!(long
        .nodes
        .iter()
        .any(|n| n.source.as_deref() == Some("allocation")));

    let down = slice_with(
        "ICallback *a)",
        0,
        11,
        SliceOptions {
            up_depth: 0,
            down_depth: 1,
        },
    );
    assert!(down.truncated_down);
    node(&down, "b", Some("Chain"));
    assert!(find_node(&down, "c", Some("Chain")).is_none());
}

fn cli_slice(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("inspect")
        .arg(db().path())
        .arg("slice")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn text_output_states_contexts_flags_and_limits() {
    let (line, col) = pos("callback_;", 0, 2);
    let out = cli_slice(&[
        "--file",
        FILE,
        "--line",
        &line.to_string(),
        "--col",
        &col.to_string(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    for needle in [
        "value slice from callback_ (field)",
        "ipc_handler",
        "IDeviceStub::SetCallback",
        "-mem_write@main.cpp:",
        "cross-context",
        "contexts_differ",
        "scalar values are not tracked",
        "a hint, not a proven race",
    ] {
        assert!(text.contains(needle), "`{needle}` in:\n{text}");
    }
}

#[test]
fn json_output_carries_spans_contexts_and_flags() {
    let (line, col) = pos("callback_;", 0, 2);
    let out = cli_slice(&[
        "--file",
        FILE,
        "--line",
        &line.to_string(),
        "--col",
        &col.to_string(),
        "--format",
        "json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["start"]["name"], "callback_");
    assert!(!doc["limits"].as_array().unwrap().is_empty());
    let flagged: Vec<&serde_json::Value> = doc["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["cross_context"] == true)
        .collect();
    assert!(!flagged.is_empty());
    for e in flagged {
        assert!(e["site"]["line"].as_i64().unwrap() > 0, "{e}");
        assert!(e["site"]["path"].as_str().unwrap().ends_with(FILE), "{e}");
    }
    assert!(doc["contexts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["kind"] == "thread" && c["entry"] == "IDeviceStub::Report"));
    // The in-library renderer writes the same document.
    let conn = conn();
    let start = resolve_slice_start(&conn, FILE, line, col, None).unwrap();
    let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
    let rendered: serde_json::Value =
        serde_json::from_str(&render_slice(&s, RenderFormat::Json).unwrap()).unwrap();
    assert_eq!(rendered, doc);
}

#[test]
fn graph_formats_are_rejected() {
    let (line, col) = pos("callback_;", 0, 2);
    let out = cli_slice(&[
        "--file",
        FILE,
        "--line",
        &line.to_string(),
        "--col",
        &col.to_string(),
        "--format",
        "graphviz",
    ]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("text or json"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn slices_are_deterministic_across_jobs() {
    let root = fixture("value_slice");
    let render = |db: &TempDb| {
        let conn = Connection::open(db.path()).unwrap();
        let (line, col) = pos("callback_;", 0, 2);
        let start = resolve_slice_start(&conn, FILE, line, col, None).unwrap();
        let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
        render_slice(&s, RenderFormat::Json).unwrap()
    };
    let one = cli_analyze(&root, &["--jobs", "1"]);
    let eight = cli_analyze(&root, &["--jobs", "8"]);
    assert_eq!(render(&one), render(&eight));
    assert_eq!(analysis_rows(&one), analysis_rows(&eight));
}

/// Memory edges of the scratch program `source`, as `(kind, cell label)`.
fn memory_edges_of(source: &str) -> Vec<(String, String)> {
    memory_edges_in(&[("wide.c", source)])
        .into_iter()
        .map(|(kind, cell, _)| (kind, cell))
        .collect()
}

/// Memory edges of the scratch program `files`, as
/// `(kind, cell label, function)`.
fn memory_edges_in(files: &[(&str, &str)]) -> Vec<(String, String, String)> {
    let dir = scratch(files);
    let db = cli_analyze(dir.path(), &[]);
    let conn = Connection::open(db.path()).unwrap();
    let mut rows: Vec<(String, String, String)> = conn
        .prepare(&format!(
            "SELECT DISTINCT m.kind, n.label, {SITE_FUNCTION} AS function \
             FROM {MEMORY_EDGES} m {MEMORY_ORIGINS} \
             JOIN flow_nodes n ON n.id = CASE m.kind WHEN 'mem_read' THEN m.src_node \
                                                     ELSE m.dst_node END \
             WHERE function IS NOT NULL"
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.sort();
    rows
}

#[test]
fn memory_accesses_name_their_load_or_store() {
    // Schema v7 stays additive: `flow_edges` keeps its four columns and its
    // constraint kinds, and each memory access is a `flow_memory_access`
    // row naming the `load` or `store` it was read off, whose
    // `flow_origins` rows are its sites (none are repeated for it).
    let conn = conn();
    let columns = |table: &str| -> Vec<String> {
        conn.prepare(&format!(
            "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
        ))
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    assert_eq!(
        columns("flow_edges"),
        ["id", "src_node", "dst_node", "kind"]
    );
    assert_eq!(columns("flow_memory_access"), ["edge_id", "cell_node"]);
    let version: i64 = conn
        .query_row("SELECT schema_version FROM analysis_run", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 7);
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM flow_edges WHERE kind IN ('mem_read', 'mem_write') \
             UNION ALL SELECT COUNT(*) FROM flow_origins WHERE kind IN ('mem_read', 'mem_write')"
        ),
        0
    );
    for kind in ["load", "store"] {
        assert!(
            count(&format!(
                "SELECT COUNT(*) FROM flow_memory_access a JOIN flow_edges e ON e.id = a.edge_id \
                 WHERE e.kind = '{kind}' AND a.cell_node IS NOT NULL"
            )) > 0,
            "{kind}"
        );
    }
    // Only loads and stores access memory, and an access without recorded
    // cells is one cell-less row, never beside a cell.
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM flow_memory_access a LEFT JOIN flow_edges e ON e.id = a.edge_id \
             WHERE e.kind IS NULL OR e.kind NOT IN ('load', 'store')"
        ),
        0
    );
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM flow_memory_access a WHERE a.cell_node IS NULL \
             AND (SELECT COUNT(*) FROM flow_memory_access b WHERE b.edge_id = a.edge_id) > 1"
        ),
        0
    );
}

#[test]
fn inspect_dataflow_stays_on_the_constraint_edges() {
    // From `SetCallback`'s parameter, the memory edges would lead on through
    // the member to the thread's reader; `inspect dataflow` does not take
    // them: that walk is the slice's.
    let conn = conn();
    let (line, col) = pos("ICallback *cb)\n    {\n        callback_", 0, 11);
    let symbols = trace_db::require_symbols_at(&conn, FILE, line, col).unwrap();
    assert_eq!(symbols[0].name, "cb");
    for dir in [trace_db::Direction::Down, trace_db::Direction::Up] {
        let view =
            trace_db::dataflow_view(&conn, std::slice::from_ref(&symbols[0]), dir, 6).unwrap();
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains("mem_read") && !json.contains("mem_write"),
            "{dir:?}: {json}"
        );
        assert!(
            !json.contains("Report"),
            "{dir:?} reaches the reader: {json}"
        );
        let graph =
            trace_db::dataflow_graph(&conn, std::slice::from_ref(&symbols[0]), dir, 6).unwrap();
        // An edge kind the legacy graph does not name is labelled `flow`.
        let labels: Vec<&str> = graph.edges.iter().map(|e| e.label.as_str()).collect();
        assert!(!labels.contains(&"flow"), "{dir:?}: {labels:?}");
    }
}

#[test]
fn a_merge_output_is_refused() {
    // trace-merge keeps call graphs only: its flow tables are empty.
    let db = cli_analyze(&fixture("value_slice"), &[]);
    let conn = Connection::open(db.path()).unwrap();
    conn.execute(
        "UPDATE analysis_run SET options_json = json_set(options_json, '$.stage', 'merge')",
        [],
    )
    .unwrap();
    let (line, col) = pos("callback_;", 0, 2);
    let err = resolve_slice_start(&conn, FILE, line, col, None).unwrap_err();
    assert!(err.to_string().contains("trace-merge"), "{err}");
}

#[test]
fn a_wide_field_access_folds_into_the_field_summary() {
    // `p` may point to 17 objects: 17 instance cells of `next` and its
    // summary are more than an access keeps, so only the summary is left.
    let mut src = String::from("struct S { struct S *next; };\n");
    for i in 0..17 {
        src.push_str(&format!("struct S s{i};\n"));
    }
    src.push_str("void Pick(struct S **out, int i) {\n");
    for i in 0..17 {
        src.push_str(&format!("    if (i == {i}) *out = &s{i};\n"));
    }
    src.push_str("}\nvoid Link(struct S *v) { struct S *p; Pick(&p, 0); p->next = v; }\n");
    let rows = memory_edges_of(&src);
    let link: Vec<&(String, String)> = rows
        .iter()
        .filter(|(kind, cell)| {
            kind == "mem_write" && (cell.starts_with("summary:S.next") || cell.starts_with("next"))
        })
        .collect();
    assert_eq!(
        link,
        vec![&("mem_write".to_string(), "summary:S.next".to_string())],
        "{rows:#?}"
    );
}

#[test]
fn an_access_wider_than_the_cap_is_left_out() {
    // `*q` may be any of 17 globals: no summary stands for them all.
    let mut src = String::new();
    for i in 0..17 {
        src.push_str(&format!("int *g{i};\n"));
    }
    src.push_str("void Pick(int ***out, int i) {\n");
    for i in 0..17 {
        src.push_str(&format!("    if (i == {i}) *out = &g{i};\n"));
    }
    src.push_str(
        "}\nvoid Set(int *v) { int **q; Pick(&q, 0); *q = v; }\n\
         void Narrow(int *v) { int **r = &g0; *r = v; }\n",
    );
    let dir = scratch(&[("wide.c", &src)]);
    let db = cli_analyze(dir.path(), &[]);
    let conn = Connection::open(db.path()).unwrap();
    // Per store: its function and the cells recorded for it, a cell-less
    // access as `None`.
    let stores: Vec<(String, Option<String>)> = conn
        .prepare(&format!(
            "SELECT DISTINCT {SITE_FUNCTION}, n.label FROM flow_memory_access a \
             JOIN flow_edges e ON e.id = a.edge_id \
             JOIN flow_origins o ON o.src_node = e.src_node AND o.dst_node = e.dst_node \
                                AND o.kind = e.kind \
             LEFT JOIN flow_nodes n ON n.id = a.cell_node \
             WHERE e.kind = 'store' ORDER BY 1, 2"
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .filter(|(f, _): &(String, Option<String>)| f == "Set" || f == "Narrow")
        .collect();
    // `Narrow`'s one-cell store is recorded; `Set`'s is recorded as an
    // access without cells.
    assert_eq!(
        stores,
        [
            ("Narrow".to_string(), Some("g0 of g0".to_string())),
            ("Set".to_string(), None),
        ]
    );
}

#[test]
fn a_load_wider_than_the_cap_is_an_unrecorded_load_source() {
    // `*q` may be any of 17 globals: the load's cells are not recorded, so
    // the slice says so rather than calling its value unknown.
    let mut src = String::new();
    for i in 0..17 {
        src.push_str(&format!("int *g{i};\n"));
    }
    src.push_str("void Pick(int ***out, int i) {\n");
    for i in 0..17 {
        src.push_str(&format!("    if (i == {i}) *out = &g{i};\n"));
    }
    src.push_str("}\nvoid Get(void) {\n    int **q;\n    Pick(&q, 0);\n    int *w = *q;\n}\n");
    let line = src.lines().position(|l| l.contains("int *w")).unwrap() as i64 + 1;
    let dir = scratch(&[("wide.c", &src)]);
    let db = cli_analyze(dir.path(), &[]);
    let conn = Connection::open(db.path()).unwrap();
    let start = resolve_slice_start(&conn, "wide.c", line, 10, None).unwrap();
    assert_eq!(start.name, "w");
    let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
    assert!(
        s.nodes
            .iter()
            .any(|n| n.source.as_deref() == Some("unrecorded_load")),
        "{:#?}",
        s.nodes
    );
    assert!(
        s.limits.iter().any(|l| l.contains("more than 16")),
        "{:#?}",
        s.limits
    );
}

#[test]
fn an_unrecorded_load_beside_another_value_is_still_a_source() {
    // `p` gets `q` by a copy and `*pp` by a load wider than the cap: the
    // copy leads on to `q`, and the load is still an `unrecorded_load`.
    let mut src = String::new();
    for i in 0..17 {
        src.push_str(&format!("int *g{i};\n"));
    }
    src.push_str("void Pick(int ***out, int i) {\n");
    for i in 0..17 {
        src.push_str(&format!("    if (i == {i}) *out = &g{i};\n"));
    }
    src.push_str(
        "}\nvoid Get(int *q) {\n    int **pp;\n    Pick(&pp, 0);\n    int *p = q;\n    \
         p = *pp;\n}\n",
    );
    let line = src.lines().position(|l| l.contains("int *p")).unwrap() as i64 + 1;
    let dir = scratch(&[("wide.c", &src)]);
    let db = cli_analyze(dir.path(), &[]);
    let conn = Connection::open(db.path()).unwrap();
    let start = resolve_slice_start(&conn, "wide.c", line, 10, None).unwrap();
    assert_eq!(start.name, "p");
    let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
    let q = node(&s, "q", Some("Get"));
    assert_eq!(q.source.as_deref(), Some("parameter"), "{q:?}");
    let p = node(&s, "p", Some("Get"));
    assert_eq!(p.source.as_deref(), Some("unrecorded_load"), "{p:?}");
    let pp = node(&s, "pp", Some("Get"));
    let load = edge(&s, pp, "load", p);
    assert_eq!(load.stages, ["up"], "{load:?}");
}

#[test]
fn a_field_holding_another_fields_address_touches_only_its_own_cell() {
    // `s->p` holds `&t.x`: reading or writing `s->p` touches `p`'s cell,
    // not the cell its value points to. `s` gets its object only after the
    // store, so the field access already sees the cell's contents.
    let rows = memory_edges_of(
        "struct T { int *x; };\nstruct S { int **p; };\nstruct T t;\nstruct S g;\n\
         struct S *holder;\nstruct S **slot;\n\
         void Get(int **v) {\n    struct S *s = *slot;\n    int **y = s->p;\n    s->p = v;\n}\n\
         void Set(struct T *other) {\n    g.p = &other->x;\n    holder = &g;\n    slot = &holder;\n}\n\
         int main(void) { Set(&t); Get(0); return 0; }\n",
    );
    let touched = |kind: &str, cell: &str| rows.iter().any(|(k, c)| k == kind && c == cell);
    assert!(touched("mem_read", "p of g"), "{rows:#?}");
    assert!(touched("mem_write", "p of g"), "{rows:#?}");
    for kind in ["mem_read", "mem_write"] {
        for cell in ["x of t", "summary:T.x"] {
            assert!(!touched(kind, cell), "{kind} {cell}: {rows:#?}");
        }
    }
}

#[test]
fn every_call_through_the_member_reads_it_at_its_own_line() {
    // `Notify` calls through `callback_` twice: each call reads it.
    let s = slice("callback_->OnError", 1, 2);
    let start = node(&s, CALLBACK, None);
    let line = pos("callback_->OnError", 1, 0).0;
    assert!(
        s.edges.iter().any(|e| e.from == start.id
            && e.kind == "mem_read"
            && e.function.as_deref() == Some("IDeviceStub::Notify")
            && e.site.as_ref().is_some_and(|site| site.line == line)),
        "the second receiver read in Notify: {:#?}",
        s.edges
    );
}

#[test]
fn a_field_its_base_type_lacks_touches_the_solvers_fallback_summary() {
    // `b.c` sees `struct S` with `b` first; the merged program keeps
    // `a.c`'s layout, which has no `b` at that position. The solver resolves
    // `p->b` to the positional fallback summary of `p`'s declared type, so
    // the store and the load touch that summary.
    let rows = memory_edges_in(&[
        (
            "a.c",
            "struct S { int *a; int *b; };\nstruct S obj;\nstruct S *g = &obj;\nint u;\n\
             void SetA(void) { g->a = &u; }\n",
        ),
        (
            "b.c",
            "struct S { int *b; };\nextern struct S *g;\nint v;\n\
             void SetB(void) { struct S *p = g; p->b = &v; }\n\
             int *GetB(void) { struct S *p = g; int *r = p->b; return r; }\n",
        ),
    ]);
    let touched = |kind: &str, cell: &str, function: &str| {
        rows.iter()
            .any(|(k, c, f)| k == kind && c == cell && f == function)
    };
    assert!(touched("mem_write", "summary:S.a", "SetB"), "{rows:#?}");
    assert!(touched("mem_read", "summary:S.a", "GetB"), "{rows:#?}");
}

/// A scratch program whose `a.f` holds the address of field `f` in each of
/// `count` unrelated structs `B0`.., reached through a pointer so the address
/// carries `summary:Bi.f`; `Set` stores into and `Get` loads `p->f`. `p`
/// gets its object only after the stores, so the field access sees the
/// cell's contents.
fn same_named_fields(count: usize) -> String {
    let mut src = String::from("struct A { int **f; };\n");
    for i in 0..count {
        src.push_str(&format!("struct B{i} {{ int *f; }};\nstruct B{i} b{i};\n"));
    }
    src.push_str(
        "struct A a;\nstruct A *holder;\nstruct A **slot;\n\
         void Set(int **v) { struct A *p = *slot; p->f = v; }\n\
         int **Get(void) { struct A *p = *slot; int **r = p->f; return r; }\n\
         void Fill(void) {\n",
    );
    for i in 0..count {
        src.push_str(&format!(
            "    struct B{i} *q{i} = &b{i};\n    a.f = &q{i}->f;\n"
        ));
    }
    src.push_str(
        "    holder = &a;\n    slot = &holder;\n}\n\
         int main(void) { Fill(); Set(0); Get(); return 0; }\n",
    );
    src
}

#[test]
fn a_field_access_leaves_an_unrelated_same_named_summary_alone() {
    // `a.f` holds `&b0.f`, so `summary:B0.f` is among the contents of the
    // cells `p->f` designates: the access touches `a`'s `f`, not `B0`'s.
    let rows = memory_edges_in(&[("same.c", &same_named_fields(1))]);
    let touched = |kind: &str, cell: &str, function: &str| {
        rows.iter()
            .any(|(k, c, f)| k == kind && c == cell && f == function)
    };
    assert!(touched("mem_write", "f of a", "Set"), "{rows:#?}");
    assert!(touched("mem_read", "f of a", "Get"), "{rows:#?}");
    assert!(
        !rows
            .iter()
            .any(|(_, cell, _)| cell.starts_with("summary:B")),
        "{rows:#?}"
    );
}

#[test]
fn unrelated_same_named_summaries_do_not_push_an_access_over_the_cap() {
    // Sixteen unrelated `f` summaries among `a.f`'s contents: counted as
    // cells they would take `p->f` past the cap and leave it unrecorded.
    let rows = memory_edges_in(&[("same.c", &same_named_fields(16))]);
    let touched = |kind: &str, cell: &str, function: &str| {
        rows.iter()
            .any(|(k, c, f)| k == kind && c == cell && f == function)
    };
    assert!(touched("mem_write", "f of a", "Set"), "{rows:#?}");
    assert!(touched("mem_read", "f of a", "Get"), "{rows:#?}");
    assert!(
        !rows
            .iter()
            .any(|(_, cell, _)| cell.starts_with("summary:B")),
        "{rows:#?}"
    );
}

/// The scratch program `files` analyzed, and a connection to its database.
fn scratch_db(files: &[(&str, &str)]) -> (tempfile::TempDir, TempDb, Connection) {
    let dir = scratch(files);
    let db = cli_analyze(dir.path(), &[]);
    let conn = Connection::open(db.path()).unwrap();
    (dir, db, conn)
}

/// 1-based column of the `nth` (0-based) occurrence of `needle` in `line`.
fn col_of(line: &str, needle: &str, nth: usize) -> i64 {
    line.match_indices(needle).nth(nth).unwrap().0 as i64 + 1
}

#[test]
fn a_field_named_like_the_declaration_on_its_line_starts_at_the_field() {
    // `int *p = obj->p;`: the declared `p` starts at the variable, the
    // field's `p` at the cells `obj->p` reads.
    let decl = "    int *p = obj->p;";
    let (dir, _db, conn) = scratch_db(&[(
        "field.c",
        &format!(
            "struct S {{ int *p; }};\nint *g;\nvoid F(struct S *obj) {{\n{decl}\n    g = p;\n}}\n\
             struct S s;\nint x;\nint main(void) {{ s.p = &x; F(&s); return 0; }}\n"
        ),
    )]);
    let file = dir.path().join("field.c");
    let file = file.to_str().unwrap();
    let var = resolve_slice_start(&conn, file, 4, col_of(decl, "p", 0), None).unwrap();
    assert_eq!((var.kind.as_str(), var.name.as_str()), ("variable", "p"));
    for name in [None, Some("p")] {
        let field = resolve_slice_start(&conn, file, 4, col_of(decl, "p", 1), name).unwrap();
        assert_eq!(
            (field.kind.as_str(), field.name.as_str()),
            ("field", "p"),
            "{name:?}: {field:?}"
        );
    }
}

#[test]
fn a_same_named_field_does_not_take_over_a_variable_use() {
    // `s->p = p;`: the `p` after `->` is the field, the other the parameter,
    // whatever the spacing, and `--name` keeps the spelling's choice.
    for stmt in ["    s->p = p;", "    s -> p = p;", "    (*s).p = p;"] {
        let (dir, _db, conn) = scratch_db(&[(
            "same.c",
            &format!(
                "struct S {{ int *p; }};\nvoid F(struct S *s, int *p)\n{{\n{stmt}\n}}\n\
                 struct S g;\nint x;\nint main(void) {{ F(&g, &x); return 0; }}\n"
            ),
        )]);
        let file = dir.path().join("same.c");
        let file = file.to_str().unwrap();
        let at = |nth, name| resolve_slice_start(&conn, file, 4, col_of(stmt, "p", nth), name);
        let field = at(0, None).unwrap();
        assert_eq!(field.kind, "field", "{stmt:?}: {field:?}");
        assert_eq!(at(0, Some("p")).unwrap(), field, "{stmt:?}");
        let var = at(1, None).unwrap();
        assert_eq!(var.kind, "variable", "{stmt:?}: {var:?}");
        assert_eq!(at(1, Some("p")).unwrap(), var, "{stmt:?}");
        let s = value_slice(&conn, &var, &SliceOptions::default()).unwrap();
        assert_eq!(
            node(&s, "p", Some("F")).source,
            None,
            "{stmt:?}: {:#?}",
            s.nodes
        );
        let x = node(&s, "x of x", None);
        assert_eq!(x.source.as_deref(), Some("address"), "{stmt:?}: {x:#?}");
    }
}

#[test]
fn an_access_starts_at_the_cells_of_its_own_operation() {
    // `a->p = x; b->p = y;`: the operation whose expression spans the column
    // is the one the identifier belongs to, so each `p` starts at its own
    // class's cells, not at every `p` the line writes; a call's argument
    // (`y` in `sink(y)`) is told from a same-named operand the same way.
    let stmt = "    a->p = x; b->p = y; sink(y);";
    let src = format!(
        "struct A {{ int *p; }};\nstruct B {{ int *p; }};\nvoid sink(int *q);\n\
         void F(struct A *a, struct B *b, int *x, int *y)\n{{\n{stmt}\n}}\n\
         struct A ga; struct B gb; int gx, gy;\n\
         int main(void) {{ F(&ga, &gb, &gx, &gy); return 0; }}\n"
    );
    let (dir, _db, conn) = scratch_db(&[("two.c", &src)]);
    let file = dir.path().join("two.c");
    let file = file.to_str().unwrap();
    let labels = |start: &trace_db::SliceStart| -> Vec<String> {
        let s = value_slice(&conn, start, &SliceOptions::default()).unwrap();
        let mut labels: Vec<String> = s
            .nodes
            .iter()
            .filter(|n| n.roles.iter().any(|r| r == "start"))
            .map(|n| n.label.clone())
            .collect();
        labels.sort();
        labels
    };
    for name in [None, Some("p")] {
        let a = resolve_slice_start(&conn, file, 6, col_of(stmt, "p", 0), name).unwrap();
        assert_eq!(a.kind, "field", "{name:?}: {a:?}");
        assert_eq!(labels(&a), ["p of ga", "summary:A.p"], "{name:?}: {a:?}");
        let b = resolve_slice_start(&conn, file, 6, col_of(stmt, "p", 1), name).unwrap();
        assert_eq!(labels(&b), ["p of gb", "summary:B.p"], "{name:?}: {b:?}");
    }
    let y = resolve_slice_start(&conn, file, 6, col_of(stmt, "y", 1), None).unwrap();
    assert_eq!(
        (y.kind.as_str(), y.name.as_str()),
        ("variable", "y"),
        "{y:?}"
    );
    assert_eq!(labels(&y), ["y"], "{y:?}");
}

#[test]
fn a_field_access_among_a_calls_arguments_starts_at_its_own_cells() {
    // `sink(a->p, b->p)`: each access is an operation of its own argument,
    // spanning that argument's text, so the `p` the column is on starts at
    // its own class's cells, not at both classes'.
    let stmt = "    sink(a->p, b->p);";
    let src = format!(
        "struct A {{ int *p; }};\nstruct B {{ int *p; }};\nvoid sink(int *q, int *r);\n\
         void F(struct A *a, struct B *b)\n{{\n{stmt}\n}}\n\
         struct A ga; struct B gb; int gx, gy;\n\
         int main(void) {{ ga.p = &gx; gb.p = &gy; F(&ga, &gb); return 0; }}\n"
    );
    let (dir, _db, conn) = scratch_db(&[("args.c", &src)]);
    let file = dir.path().join("args.c");
    let file = file.to_str().unwrap();
    let labels = |start: &trace_db::SliceStart| -> Vec<String> {
        let s = value_slice(&conn, start, &SliceOptions::default()).unwrap();
        let mut labels: Vec<String> = s
            .nodes
            .iter()
            .filter(|n| n.roles.iter().any(|r| r == "start"))
            .map(|n| n.label.clone())
            .collect();
        labels.sort();
        labels
    };
    for name in [None, Some("p")] {
        let a = resolve_slice_start(&conn, file, 6, col_of(stmt, "p", 0), name).unwrap();
        assert_eq!(a.kind, "field", "{name:?}: {a:?}");
        assert_eq!(labels(&a), ["p of ga", "summary:A.p"], "{name:?}: {a:?}");
        let b = resolve_slice_start(&conn, file, 6, col_of(stmt, "p", 1), name).unwrap();
        assert_eq!(labels(&b), ["p of gb", "summary:B.p"], "{name:?}: {b:?}");
    }
}

#[test]
fn a_use_on_a_continuation_line_is_found() {
    // `p =` with `q;` on the next line is one operation, recorded on its
    // first line with the newline kept in its expression; so is a call
    // whose arguments run on. A position on any line of it finds it.
    let src =
        "void sink(int *a, int *b);\nvoid take(int *a);\nint *g;\nvoid F(int *q, int *r)\n{\n    \
               int *p;\n    p =\n        q;\n    g = p;\n    sink(p,\n         r);\n    \
               take(\n        p\n    );\n}\n\
               int x, y;\nint main(void) { F(&x, &y); return 0; }\n";
    let (dir, _db, conn) = scratch_db(&[("multi.c", src)]);
    let file = dir.path().join("multi.c");
    let file = file.to_str().unwrap();
    for (line, col, expect) in [(8, 9, "q"), (11, 10, "r"), (10, 10, "p"), (13, 9, "p")] {
        for name in [None, Some(expect)] {
            let start = resolve_slice_start(&conn, file, line, col, name).unwrap();
            assert_eq!(
                (start.kind.as_str(), start.name.as_str()),
                ("variable", expect),
                "{line}:{col} {name:?}: {start:?}"
            );
        }
    }
}

#[test]
fn a_call_argument_starts_at_the_variable_passed() {
    // `sink(p);` moves `p` only as an argument: the line has no operation of
    // its own, only the call's. The slice goes down into `sink`, whose
    // same-named parameter is reached, not started from: the line never
    // spells it.
    let src = "void sink(int *p);\nvoid f(int *p)\n{\n    sink(p);\n}\n\
               int x;\nint main(void) { f(&x); return 0; }\n";
    let (dir, _db, conn) = scratch_db(&[("arg.c", src)]);
    let file = dir.path().join("arg.c");
    let file = file.to_str().unwrap();
    for name in [None, Some("p")] {
        let start = resolve_slice_start(&conn, file, 4, 10, name).unwrap();
        assert_eq!(start.kind, "variable", "{name:?}: {start:?}");
        let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
        let starts: Vec<_> = s
            .nodes
            .iter()
            .filter(|n| n.roles.iter().any(|r| r == "start"))
            .collect();
        assert!(
            !starts.is_empty() && starts.iter().all(|n| n.function.as_deref() == Some("f")),
            "{name:?}: {starts:#?}"
        );
        assert!(find_node(&s, "p", Some("sink")).is_some(), "{:#?}", s.nodes);
        let x = node(&s, "x of x", None);
        assert_eq!(x.source.as_deref(), Some("address"), "{x:#?}");
    }
}

#[test]
fn an_indirect_call_starts_at_the_variable_called_through() {
    // `cb();` has no operation of its own and passes nothing: the line's
    // only value is the variable the call goes through. A position on it
    // starts where the declaration's does, at `cb`.
    let src = "void target(void);\nvoid f(void)\n{\n    void (*cb)(void) = target;\n    cb();\n}\n\
               int main(void) { f(); return 0; }\n";
    let (dir, _db, conn) = scratch_db(&[("indirect.c", src)]);
    let file = dir.path().join("indirect.c");
    let file = file.to_str().unwrap();
    let declared = resolve_slice_start(&conn, file, 4, 12, None).unwrap();
    assert_eq!(
        (declared.kind.as_str(), declared.name.as_str()),
        ("variable", "cb"),
        "{declared:?}"
    );
    for (col, name) in [(5, None), (6, None), (5, Some("cb"))] {
        let called = resolve_slice_start(&conn, file, 5, col, name).unwrap();
        assert_eq!(
            (&called.kind, &called.name, &called.nodes),
            (&declared.kind, &declared.name, &declared.nodes),
            "5:{col} {name:?}"
        );
    }
    let s = value_slice(&conn, &declared, &SliceOptions::default()).unwrap();
    let cb = node(&s, "cb", Some("f"));
    assert!(cb.roles.iter().any(|r| r == "start"), "{cb:#?}");
    assert!(
        s.nodes
            .iter()
            .any(|n| n.source.as_deref() == Some("function")),
        "{:#?}",
        s.nodes
    );
}

#[test]
fn an_address_taken_parameter_nothing_passes_is_still_a_source() {
    // `&p` gives `p` storage: the parameter and its storage point at each
    // other, and neither brings a value from a caller.
    let (dir, _db, conn) = scratch_db(&[(
        "addr.c",
        "void entry(int *p) {\n    int **addr = &p;\n    int *q = p;\n    int *other = p;\n}\n",
    )]);
    let file = dir.path().join("addr.c");
    let start = resolve_slice_start(&conn, file.to_str().unwrap(), 3, 10, None).unwrap();
    assert_eq!(start.name, "q");
    let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
    let p = node(&s, "p", Some("entry"));
    assert_eq!(p.source.as_deref(), Some("parameter"), "{:#?}", s.nodes);
    assert!(
        find_node(&s, "other", Some("entry")).is_some(),
        "{:#?}",
        s.nodes
    );
    assert!(!s.truncated_up && !s.truncated_down, "{s:#?}");

    // A store through the address feeds the storage: `p` is no source, and
    // the slice goes on to what was stored.
    let (dir, _db, conn) = scratch_db(&[(
        "addr.c",
        "int x;\nvoid entry(int *p) {\n    int **addr = &p;\n    *addr = &x;\n    int *q = p;\n}\n",
    )]);
    let file = dir.path().join("addr.c");
    let start = resolve_slice_start(&conn, file.to_str().unwrap(), 5, 10, None).unwrap();
    let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
    let p = node(&s, "p", Some("entry"));
    assert_eq!(p.source, None, "{:#?}", s.nodes);
    assert!(
        s.nodes
            .iter()
            .any(|n| n.source.as_deref() == Some("address")),
        "{:#?}",
        s.nodes
    );
}

#[test]
fn a_file_filter_is_a_literal_substring() {
    // `_` in `a_b.c` is no wildcard: `axb.c` does not match it.
    let src = |f: &str| format!("int *g; void {f}(int *v) {{ int *p = v; g = p; }}\n");
    let col = col_of(&src("fa"), "p =", 0);
    let (dir, _db, conn) = scratch_db(&[("a_b.c", &src("fa")), ("axb.c", &src("fx"))]);
    for file in [
        "a_b.c".to_string(),
        dir.path().join("a_b.c").to_str().unwrap().to_string(),
    ] {
        for name in [None, Some("p")] {
            let start = resolve_slice_start(&conn, &file, 1, col, name)
                .unwrap_or_else(|e| panic!("{file} {name:?}: {e}"));
            assert!(start.at.path.ends_with("a_b.c"), "{:?}", start.at);
        }
    }
    // The other position lookups read the filter the same way.
    let fns = trace_db::find_functions_at(&conn, "a_b.c", 1).unwrap();
    let fns: Vec<&str> = fns.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(fns, ["fa"]);
    let syms = trace_db::find_symbols_at(&conn, "a_b.c", 1, col).unwrap();
    assert!(
        !syms.is_empty() && syms.iter().all(|s| s.path.ends_with("a_b.c")),
        "{syms:?}"
    );
}

#[test]
fn a_return_and_an_allocation_have_their_own_origins() {
    // `return s->p;` loads the member at the returned value, as an
    // initializer would; `new S()` takes the heap object's address at the
    // allocation. Both are lowered operations, so `flow_origins` records
    // them, and so the memory edge of the load.
    let src = "struct S { int *p; };\n\
               int *Get(S *s) {\n    return s->p;\n}\n\
               S *Make() {\n    S *m = new S();\n    return m;\n}\n";
    let (_dir, _db, conn) = scratch_db(&[("ret.cpp", src)]);
    let sites = |kind: &str| -> Vec<(String, i64, i64, String)> {
        conn.prepare(&format!(
            "SELECT o.kind, o.line, o.col, {SITE_FUNCTION} FROM flow_origins o \
             WHERE o.kind = ?1 ORDER BY o.line, o.col"
        ))
        .unwrap()
        .query_map([kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    let at = |kind: &str, line: i64, col: i64, f: &str| (kind.to_string(), line, col, f.into());
    assert_eq!(sites("load"), [at("load", 3, 12, "Get")]);
    let reads: Vec<(String, i64, i64, String)> = conn
        .prepare(&format!(
            "SELECT DISTINCT m.kind, o.line, o.col, {SITE_FUNCTION} \
             FROM {MEMORY_EDGES} m {MEMORY_ORIGINS} WHERE m.kind = 'mem_read'"
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(reads, [at("mem_read", 3, 12, "Get")]);
    let heap: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM flow_origins o JOIN flow_nodes n ON n.id = o.src_node \
             WHERE o.kind = 'addr_of' AND n.detail = 'heap' AND o.line = 6 AND o.col = 12 \
               AND o.expression = 'new S()'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(heap, 1);
}

#[test]
fn a_null_pointer_is_a_source_that_is_not_followed() {
    // `nullptr` is one constant node for the whole program: stage 2 does not
    // walk on from it into every other null assignment.
    let src = "struct C {\n    int *p_;\n    void Set(int *v) { p_ = v; }\n    \
               void Clear() { p_ = nullptr; }\n};\n\
               void Other() {\n    int *q = nullptr;\n    int *r = q;\n}\n";
    let (dir, _db, conn) = scratch_db(&[("null.cpp", src)]);
    let file = dir.path().join("null.cpp");
    let line = src.lines().position(|l| l.contains("p_ = v")).unwrap() as i64 + 1;
    let col = col_of(src.lines().nth(line as usize - 1).unwrap(), "p_", 0);
    let start = resolve_slice_start(&conn, file.to_str().unwrap(), line, col, None).unwrap();
    assert_eq!((start.kind.as_str(), start.name.as_str()), ("field", "p_"));
    let s = value_slice(&conn, &start, &SliceOptions::default()).unwrap();
    let null = s
        .nodes
        .iter()
        .find(|n| n.kind == "constant")
        .unwrap_or_else(|| panic!("the null source: {:#?}", s.nodes));
    assert_eq!(null.source.as_deref(), Some("null"), "{null:?}");
    assert_eq!(null.sharing, "constant");
    assert_eq!(null.stages, ["up"], "{null:?}");
    for other in ["q", "r"] {
        assert!(
            find_node(&s, other, Some("Other")).is_none(),
            "{other}: {:#?}",
            s.nodes
        );
    }
    assert!(s
        .edges
        .iter()
        .filter(|e| e.from == null.id)
        .all(|e| !e.cross_context));
}

#[test]
fn a_field_address_base_is_still_expanded_on_another_path() {
    // `p` gets a field address computed from `base` (a `field_address`
    // source, not expanded through that `gep`) and, through `alias`, the
    // value of `base` itself, which `&object` sets. Recording the first
    // must not keep the second from expanding `base`: both sources survive,
    // whichever the walk discovers first.
    let gep_first = "struct S { int field; };\nvoid F(void) {\n    struct S object;\n    \
                     struct S *base = &object;\n    int *p = &base->field;\n    \
                     int *alias = (int *)base;\n    p = alias;\n}\n";
    let value_first = "struct S { int field; };\nvoid F(void) {\n    struct S object;\n    \
                       struct S *base = &object;\n    int *p = (int *)base;\n    \
                       int *q = &base->field;\n    int *r = q;\n    p = r;\n}\n";
    for src in [gep_first, value_first] {
        let (dir, _db, conn) = scratch_db(&[("gep.c", src)]);
        let file = dir.path().join("gep.c");
        let line = src.lines().nth(4).unwrap();
        let start =
            resolve_slice_start(&conn, file.to_str().unwrap(), 5, col_of(line, "p", 0), None)
                .unwrap();
        assert_eq!(start.name, "p");
        let opts = SliceOptions {
            up_depth: 20,
            down_depth: 20,
        };
        let s = value_slice(&conn, &start, &opts).unwrap();
        let base = node(&s, "base", Some("F"));
        assert_eq!(
            base.source.as_deref(),
            Some("field_address"),
            "{src}{:#?}",
            s.nodes
        );
        let object = s
            .nodes
            .iter()
            .find(|n| n.source.as_deref() == Some("address"))
            .unwrap_or_else(|| panic!("the `&object` source: {src}{:#?}", s.nodes));
        edge(&s, object, "addr_of", base);
        assert!(!s.truncated_up && !s.truncated_down, "{s:#?}");
    }
}

#[test]
fn a_call_entry_beyond_the_depth_limit_releases_no_return() {
    // `start`'s value reaches `A.a` through `*out = s`; `G(a)` passes it to
    // `G.x`, which returns it to `A.b`. That return from a source's walk is
    // allowed only once the walk entered `G` from `A`: at `--down-depth 2`
    // the entry edge `A.a -> G.x` is beyond the limit, so the return is not
    // followed; at 3 the entry is in the slice and so is the return.
    let src = "typedef void (*CB)(void);\nCB G(CB x) { return x; }\n\
               CB F(CB s, CB *out) {\n    G(s);\n    *out = s;\n    return s;\n}\n\
               void A(void) {\n    CB a;\n    CB start = F(0, &a);\n    CB b = G(a);\n}\n";
    let (dir, _db, conn) = scratch_db(&[("depth.c", src)]);
    let file = dir.path().join("depth.c");
    let line = src.lines().nth(9).unwrap();
    let start = resolve_slice_start(
        &conn,
        file.to_str().unwrap(),
        10,
        col_of(line, "start", 0),
        None,
    )
    .unwrap();
    assert_eq!(start.name, "start");
    let at = |down_depth: u32| {
        let opts = SliceOptions {
            up_depth: 10,
            down_depth,
        };
        value_slice(&conn, &start, &opts).unwrap()
    };
    let has_edge = |s: &ValueSlice, from: (&str, &str), to: (&str, &str)| match (
        find_node(s, from.0, Some(from.1)),
        find_node(s, to.0, Some(to.1)),
    ) {
        (Some(f), Some(t)) => s.edges.iter().any(|e| e.from == f.id && e.to == t.id),
        _ => false,
    };
    let s = at(2);
    assert!(!has_edge(&s, ("a", "A"), ("x", "G")), "{:#?}", s.edges);
    assert!(
        !has_edge(&s, ("x", "G"), ("b", "A")),
        "a return without its call entry: {:#?}",
        s.edges
    );
    assert!(s.truncated_down, "{s:#?}");
    let s = at(3);
    assert!(has_edge(&s, ("a", "A"), ("x", "G")), "{:#?}", s.edges);
    assert!(has_edge(&s, ("x", "G"), ("b", "A")), "{:#?}", s.edges);
}

/// Slices the `start` declared on line `line` of the scratch program `src`
/// at each down-depth in `depths`.
fn slices_of_start(src: &str, line: usize, depths: &[u32]) -> Vec<ValueSlice> {
    let (dir, _db, conn) = scratch_db(&[("depth.c", src)]);
    let file = dir.path().join("depth.c");
    let text = src.lines().nth(line - 1).unwrap();
    let start = resolve_slice_start(
        &conn,
        file.to_str().unwrap(),
        line as i64,
        col_of(text, "start", 0),
        None,
    )
    .unwrap();
    assert_eq!(start.name, "start");
    depths
        .iter()
        .map(|&down_depth| {
            let opts = SliceOptions {
                up_depth: 10,
                down_depth,
            };
            value_slice(&conn, &start, &opts).unwrap()
        })
        .collect()
}

#[test]
fn a_released_return_shortens_a_queued_path() {
    // `start`'s source `F.s` reaches `A.b` two ways: through `u` and the
    // store `*other = u` (depth 3), and through `*out = s` into `A.a`, the
    // call `G(a)` and the return `G.x -> A.b` (depth 2), which waits for
    // that call to be entered and so is released only after the longer
    // path queued `b`. The shorter arrival counts: `b -> c` is at depth 3.
    let with_other = "typedef void (*CB)(void);\nCB G(CB x) { return x; }\n\
                      CB F(CB s, CB *out, CB *other) {\n    G(s);\n    CB u = s;\n    \
                      *other = u;\n    *out = s;\n    return s;\n}\n\
                      void A(void) {\n    CB a, b;\n    CB start = F(0, &a, &b);\n    \
                      b = G(a);\n    CB c = b;\n}\n";
    let without_other = "typedef void (*CB)(void);\nCB G(CB x) { return x; }\n\
                         CB F(CB s, CB *out, CB *other) {\n    G(s);\n    CB u = s;\n    \
                         *out = s;\n    return s;\n}\n\
                         void A(void) {\n    CB a, b;\n    CB start = F(0, &a, &b);\n    \
                         b = G(a);\n    CB c = b;\n}\n";
    let has_edge = |s: &ValueSlice, from: (&str, &str), to: (&str, &str)| match (
        find_node(s, from.0, Some(from.1)),
        find_node(s, to.0, Some(to.1)),
    ) {
        (Some(f), Some(t)) => s.edges.iter().any(|e| e.from == f.id && e.to == t.id),
        _ => false,
    };
    for (src, line) in [(with_other, 12), (without_other, 11)] {
        let slices = slices_of_start(src, line, &[2, 3, 4]);
        let (s2, s3, s4) = (&slices[0], &slices[1], &slices[2]);
        for s in [s3, s4] {
            assert!(has_edge(s, ("x", "G"), ("b", "A")), "{src}{:#?}", s.edges);
            assert!(has_edge(s, ("b", "A"), ("c", "A")), "{src}{:#?}", s.edges);
        }
        // `c` is at depth 3: nothing past the limit.
        assert!(
            find_node(s2, "c", Some("A")).is_none(),
            "{src}{:#?}",
            s2.nodes
        );
        assert!(s2.truncated_down, "{src}{s2:#?}");
        // The depth-3 slice is the whole one.
        assert_eq!(
            s3.edges.len(),
            s4.edges.len(),
            "{src}{:#?}\n{:#?}",
            s3.edges,
            s4.edges
        );
        assert!(!s3.truncated_down, "{src}{s3:#?}");
    }
}

#[test]
fn declarator_punctuation_is_no_identifier() {
    // On the `*` of `int *p = q;`, as on its `=`, no identifier picks one of
    // the two values the line moves: the query asks for the name rather
    // than taking the declaration whose recorded range holds the column.
    let decl = " int *p = q;";
    let src = format!("int *g;\nvoid F(int *q) {{\n{decl}\n g = p;\n}}\n");
    let (dir, _db, conn) = scratch_db(&[("punct.c", &src)]);
    let file = dir.path().join("punct.c");
    let file = file.to_str().unwrap();
    for col in [col_of(decl, "*", 0), col_of(decl, "=", 0)] {
        let err = resolve_slice_start(&conn, file, 3, col, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("p") && msg.contains("q") && msg.contains("--name"),
            "column {col}: {msg}"
        );
        let named = resolve_slice_start(&conn, file, 3, col, Some("p")).unwrap();
        assert_eq!(
            (named.kind.as_str(), named.name.as_str()),
            ("variable", "p"),
            "column {col}"
        );
    }
    let on_p = resolve_slice_start(&conn, file, 3, col_of(decl, "p", 0), None).unwrap();
    assert_eq!((on_p.kind.as_str(), on_p.name.as_str()), ("variable", "p"));
}

/// The slice from the `nth` occurrence of `needle` on the line holding it
/// in the scratch file `file` of `src`.
fn scratch_slice(
    conn: &Connection,
    dir: &tempfile::TempDir,
    file: &str,
    src: &str,
    needle: &str,
) -> ValueSlice {
    let path = dir.path().join(file);
    let line = src.lines().position(|l| l.contains(needle)).unwrap();
    let col = col_of(src.lines().nth(line).unwrap(), needle, 0);
    let start =
        resolve_slice_start(conn, path.to_str().unwrap(), line as i64 + 1, col, None).unwrap();
    value_slice(conn, &start, &SliceOptions::default()).unwrap()
}

#[test]
fn an_entry_dispatched_from_in_tree_code_runs_in_its_own_context_only() {
    // c_utils' shape: `Thread::Start` starts `ThreadStart` on a pthread, and
    // `ThreadStart` calls the override of `OHOS::Thread::Run`, itself an
    // entry (the built-in `entry` model). The two contexts are one thread:
    // `Worker::Run`'s heap cell, written there alone, is not flagged.
    let src = "extern \"C\" int pthread_create(void *tid, void *attr, \
               void *(*fn)(void *), void *arg);\n\
               namespace OHOS {\nclass Thread {\npublic:\n    virtual ~Thread() {}\n    \
               bool Start();\nprotected:\n    virtual bool Run() = 0;\nprivate:\n    \
               static void *ThreadStart(void *args);\n};\n\
               void *Thread::ThreadStart(void *args)\n{\n    \
               Thread *t = static_cast<Thread *>(args);\n    t->Run();\n    return nullptr;\n}\n\
               bool Thread::Start()\n{\n    long tid = 0;\n    \
               pthread_create(&tid, nullptr, ThreadStart, this);\n    return true;\n}\n}\n\
               struct Item {\n    int *p;\n};\nint g;\n\
               class Worker : public OHOS::Thread {\nprotected:\n    bool Run() override\n    {\n        \
               Item *it = new Item();\n        int *v = &g;\n        it->p = v;\n        \
               return true;\n    }\n};\n\
               int main()\n{\n    Worker w;\n    w.Start();\n    return 0;\n}\n";
    let (dir, _db, conn) = scratch_db(&[("thread.cpp", src)]);
    let s = scratch_slice(&conn, &dir, "thread.cpp", src, "v;");
    let v = node(&s, "v", Some("Worker::Run"));
    assert_eq!(context_entries(&s, v), ["thread:Worker::Run"]);
    let flagged: Vec<_> = s.edges.iter().filter(|e| e.cross_context).collect();
    assert!(flagged.is_empty(), "{flagged:#?}");

    // An IPC stub handler the stub's own `OnRemoteRequest` switch calls:
    // the switch is the IPC dispatch the handler's context stands for, so
    // the handler is not also root's code. Its heap cell is flagged only as
    // self-concurrent.
    let src = "class IRemoteObject {\npublic:\n    \
               int SendRequest(int code, void *data, void *reply, void *option);\n};\n\
               IRemoteObject *Remote();\nstruct Item {\n    int *p;\n};\nint g;\n\
               class IFooProxy {\npublic:\n    int Put(int *x);\n};\n\
               int IFooProxy::Put(int *x)\n{\n    \
               Remote()->SendRequest(1, nullptr, nullptr, nullptr);\n    return 0;\n}\n\
               class IFooStub {\npublic:\n    \
               int OnRemoteRequest(int code, int *x)\n    {\n        switch (code) {\n        \
               case 1:\n            return Put(x);\n        }\n        return 0;\n    }\n    \
               int Put(int *x)\n    {\n        Item *it = new Item();\n        \
               int *w = x;\n        it->p = w;\n        return 0;\n    }\n};\n";
    let (dir, _db, conn) = scratch_db(&[("ipc.cpp", src)]);
    let s = scratch_slice(&conn, &dir, "ipc.cpp", src, "w;");
    let w = node(&s, "w", Some("IFooStub::Put"));
    assert_eq!(context_entries(&s, w), ["ipc_handler:IFooStub::Put"]);
    for e in s.edges.iter().filter(|e| e.cross_context) {
        assert_eq!(e.reasons, ["self_concurrent"], "{e:?}");
    }
}

#[test]
fn a_declarator_with_brackets_or_an_attribute_declares_its_identifier() {
    // Between the declarator's start and its name: an array declarator's
    // parentheses, an attribute's argument list. The cursor on the name
    // starts at the variable, though the line also reads a field of that
    // name; the cursor on the field starts at the field.
    let src = "struct O { int *at; };\nint *g;\nvoid G(O *o)\n{\n    \
               int *__attribute__((unused)) at = o->at;\n    g = at;\n}\n";
    let (dir, _db, conn) = scratch_db(&[("attr.cpp", src)]);
    let path = dir.path().join("attr.cpp");
    let path = path.to_str().unwrap();
    let text = src.lines().nth(4).unwrap();
    let start = resolve_slice_start(&conn, path, 5, col_of(text, "at", 1), None).unwrap();
    assert_eq!(
        (start.kind.as_str(), start.name.as_str()),
        ("variable", "at")
    );
    let start = resolve_slice_start(&conn, path, 5, col_of(text, "at", 2), None).unwrap();
    assert_eq!((start.kind.as_str(), start.name.as_str()), ("field", "at"));

    let cases = [
        (
            "fns.cpp",
            "int A(int x) { return x; }\nint (*g)(int);\n\
             void G()\n{\n    int (*fns[2])(int) = {A, A};\n    g = fns[0];\n}\n",
            "int (*fns[2])(int)",
            "fns",
        ),
        (
            "arr.cpp",
            "namespace std { template <class T, unsigned long N> struct array { T v[N]; }; }\n\
             struct Cb { int x; };\nCb *g;\n\
             void G(Cb *in)\n{\n    std::array<Cb *, 4> cbs = {in, in, in, in};\n    \
             g = cbs.v[0];\n}\n",
            "std::array<Cb *, 4> cbs",
            "cbs",
        ),
        (
            "buf.c",
            "char *g;\nvoid G(char *in)\n{\n    char *buf[16] = {in};\n    g = buf[0];\n}\n",
            "char *buf[16]",
            "buf",
        ),
    ];
    for (file, src, decl, ident) in cases {
        let (dir, _db, conn) = scratch_db(&[(file, src)]);
        let path = dir.path().join(file);
        let line = src.lines().position(|l| l.contains(decl)).unwrap();
        let text = src.lines().nth(line).unwrap();
        let col = col_of(text, decl, 0) + decl.rfind(ident).unwrap() as i64;
        let start = resolve_slice_start(&conn, path.to_str().unwrap(), line as i64 + 1, col, None)
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(
            (start.kind.as_str(), start.name.as_str()),
            ("variable", ident),
            "{file}: {start:?}"
        );
    }
}
