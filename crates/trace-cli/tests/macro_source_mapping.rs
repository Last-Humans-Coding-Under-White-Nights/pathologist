mod common;

use common::*;
use rusqlite::Connection;
use trace_analysis::analyze;
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
}
