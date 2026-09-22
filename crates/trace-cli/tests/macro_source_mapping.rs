mod common;

use common::*;
use rusqlite::Connection;
use trace_analysis::analyze;
use trace_db::{call_edges, find_functions_at, CallEdgeFilter};
use trace_parse::build_program;

#[test]
fn macro_body_calls_export_spelling_and_expansion_positions() {
    let root = fixture("macro_call_positions");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let calls_h = file_id(&program, &root, "calls.h");
    let main_c = file_id(&program, &root, "main.c");

    let mut sites = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| site.callee_name == "target")
        .collect::<Vec<_>>();
    sites.sort_by_key(|site| site.expansion_span.map(|span| span.line));
    assert_eq!(sites.len(), 2, "one call record per macro invocation");
    assert!(sites
        .iter()
        .all(|site| site.span == trace_ir::Span::new(calls_h, 3, 25)));
    assert_eq!(
        sites
            .iter()
            .map(|site| site.expansion_span)
            .collect::<Vec<_>>(),
        vec![
            Some(trace_ir::Span::new(main_c, 5, 20)),
            Some(trace_ir::Span::new(main_c, 6, 21)),
        ]
    );
    assert!(
        program
            .symbols
            .functions
            .iter()
            .all(|function| function.name != "TRACE_REQUEST"),
        "the macro must not become an IR function"
    );

    let (pag, analysis) = analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    let conn = Connection::open(db.path()).expect("open export");
    let rows = conn
        .prepare(
            "SELECT sf.path, cs.line, cs.col, ef.path, cs.expansion_line, cs.expansion_col \
             FROM call_sites cs \
             JOIN files sf ON sf.id = cs.file_id \
             LEFT JOIN files ef ON ef.id = cs.expansion_file_id \
             WHERE cs.callee_text = 'target' ORDER BY cs.expansion_line",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
                row.get::<_, u32>(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.0.ends_with("calls.h")));
    assert!(rows.iter().all(|row| (row.1, row.2) == (3, 25)));
    assert!(rows.iter().all(|row| row.3.ends_with("main.c")));
    assert_eq!(
        rows.iter().map(|row| (row.4, row.5)).collect::<Vec<_>>(),
        vec![(5, 20), (6, 21)]
    );

    let macro_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM functions WHERE name = 'TRACE_REQUEST'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        macro_rows, 0,
        "the macro must not become a database function"
    );

    let filtered = call_edges(
        &conn,
        &CallEdgeFilter {
            from: None,
            to: Some("target"),
            file: Some("main.c"),
            exclude_deps: false,
        },
    )
    .unwrap();
    assert_eq!(
        filtered.len(),
        2,
        "--file must match the macro invocation file"
    );
}

#[test]
fn macro_provided_closing_brace_keeps_the_function_end_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.c"),
        "#define END }\nvoid target(void) {}\n\nvoid caller(void) {\n    target();\nEND\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let caller = program
        .symbols
        .functions
        .iter()
        .find(|function| function.name == "caller")
        .expect("caller");
    assert_eq!((caller.span.line, caller.end_line), (4, 6));

    let (pag, analysis) = analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    let conn = Connection::open(db.path()).expect("open export");
    let functions = find_functions_at(&conn, "main.c", 5).unwrap();
    assert!(
        functions.iter().any(|function| function.name == "caller"),
        "the function-at-line query must include caller's body"
    );
}

#[test]
fn member_calls_use_the_macro_spelled_member_with_an_argument_receiver() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("api.h"),
        "struct Obj { void send(); };\n#define BOTH(o) o->send(); o->send()\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.cpp"),
        "#include \"api.h\"\nvoid Obj::send() {}\nvoid caller(Obj *p) { BOTH(p); }\n",
    )
    .unwrap();

    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let api = file_id(&program, dir.path(), "api.h");
    let main = file_id(&program, dir.path(), "main.cpp");
    let mut sites = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| {
            fn_name(&program, site.caller) == "caller" && site.callee_name == "Obj::send"
        })
        .collect::<Vec<_>>();
    sites.sort_by_key(|site| site.span.col);

    assert_eq!(sites.len(), 2, "both replacement-list calls must survive");
    assert!(sites
        .iter()
        .all(|site| site.span.file == api && site.span.line == 2));
    assert_ne!(sites[0].span.col, sites[1].span.col);
    assert!(sites
        .iter()
        .all(|site| { site.expansion_span == Some(trace_ir::Span::new(main, 3, 23)) }));
}

#[test]
fn member_argument_calls_keep_distinct_macro_spelled_receivers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.cpp"),
        "void first() {}\nvoid second() {}\nstruct Obj { void send(void (*cb)()) { cb(); } };\n#define BOTH(m) a.m(first); b.m(second)\nvoid caller() { Obj a; Obj b; BOTH(send); }\n",
    )
    .unwrap();

    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let main = file_id(&program, dir.path(), "main.cpp");
    let mut sites = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| {
            fn_name(&program, site.caller) == "caller" && site.callee_name == "Obj::send"
        })
        .collect::<Vec<_>>();
    sites.sort_by_key(|site| site.span.col);

    assert_eq!(sites.len(), 2, "both replacement-list calls must survive");
    assert!(sites
        .iter()
        .all(|site| site.span.file == main && site.span.line == 4));
    assert_ne!(sites[0].span.col, sites[1].span.col);
    assert!(sites
        .iter()
        .all(|site| { site.expansion_span == Some(trace_ir::Span::new(main, 5, 31)) }));

    let (_pag, analysis) = analyze(&program);
    let mut callbacks = analysis
        .call_edges
        .iter()
        .filter(|edge| fn_name(&program, edge.caller) == "Obj::send")
        .map(|edge| fn_name(&program, edge.callee))
        .collect::<Vec<_>>();
    callbacks.sort_unstable();
    callbacks.dedup();
    assert_eq!(callbacks, ["first", "second"]);
}

#[test]
fn macro_valued_member_argument_keeps_distinct_macro_spelled_receivers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.cpp"),
        "void first() {}\nvoid second() {}\nstruct Obj { void send(void (*cb)()) { cb(); } };\n#define METHOD send\n#define BOTH(m) a.m(first); b.m(second)\nvoid caller() { Obj a; Obj b; BOTH(METHOD); }\n",
    )
    .unwrap();

    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let main = file_id(&program, dir.path(), "main.cpp");
    let mut sites = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| {
            fn_name(&program, site.caller) == "caller" && site.callee_name == "Obj::send"
        })
        .collect::<Vec<_>>();
    sites.sort_by_key(|site| site.occurrence().span.col);

    assert_eq!(sites.len(), 2, "both replacement-list calls must survive");
    assert!(sites
        .iter()
        .all(|site| site.span == trace_ir::Span::new(main, 4, 16)));
    assert!(sites
        .iter()
        .all(|site| { site.expansion_span == Some(trace_ir::Span::new(main, 6, 36)) }));
    assert!(sites
        .iter()
        .all(|site| site.occurrence().span.file == main && site.occurrence().span.line == 5));
    assert_ne!(
        sites[0].occurrence().span.col,
        sites[1].occurrence().span.col
    );

    let (_pag, analysis) = analyze(&program);
    let mut callbacks = analysis
        .call_edges
        .iter()
        .filter(|edge| fn_name(&program, edge.caller) == "Obj::send")
        .map(|edge| fn_name(&program, edge.callee))
        .collect::<Vec<_>>();
    callbacks.sort_unstable();
    callbacks.dedup();
    assert_eq!(callbacks, ["first", "second"]);
}

#[test]
fn macro_valued_receiver_argument_keeps_distinct_macro_spelled_members() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.cpp"),
        "void first() {}\nvoid second() {}\nstruct Obj { void send(void (*cb)()) { cb(); } };\n#define OBJECT p\n#define BOTH(o) o->send(first); o->send(second)\nvoid caller(Obj *p) { BOTH(OBJECT); }\n",
    )
    .unwrap();

    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let main = file_id(&program, dir.path(), "main.cpp");
    let mut sites = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| {
            fn_name(&program, site.caller) == "caller" && site.callee_name == "Obj::send"
        })
        .collect::<Vec<_>>();
    sites.sort_by_key(|site| site.occurrence().span.col);

    assert_eq!(sites.len(), 2, "both replacement-list calls must survive");
    assert!(sites
        .iter()
        .all(|site| site.span.file == main && site.span.line == 5));
    assert_ne!(sites[0].span.col, sites[1].span.col);
    assert!(sites
        .iter()
        .all(|site| { site.expansion_span == Some(trace_ir::Span::new(main, 6, 23)) }));
    assert_ne!(
        sites[0].occurrence().span.col,
        sites[1].occurrence().span.col
    );

    let (_pag, analysis) = analyze(&program);
    let mut callbacks = analysis
        .call_edges
        .iter()
        .filter(|edge| fn_name(&program, edge.caller) == "Obj::send")
        .map(|edge| fn_name(&program, edge.callee))
        .collect::<Vec<_>>();
    callbacks.sort_unstable();
    callbacks.dedup();
    assert_eq!(callbacks, ["first", "second"]);
}
