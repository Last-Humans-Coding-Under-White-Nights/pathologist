use crate::common::*;
use rusqlite::Connection;

analyzed_fixture!(cpp_call_sites);

#[test]
fn implicit_member_call_without_class_header() {
    let (program, analysis) = cpp_call_sites();
    assert!(
        has_any_edge(
            program,
            analysis,
            "app::Context::Dispatch",
            "app::Context::GetCallbacks"
        ),
        "sites: {:?}; edges: {:?}",
        program.symbols.call_sites,
        callees_of(program, analysis, "app::Context::Dispatch"),
    );
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| fn_name(program, site.caller) == "app::Context::Dispatch")
        .collect();
    let getters: Vec<_> = sites
        .iter()
        .filter_map(|site| site.callee_fn_id)
        .filter(|&id| fn_name(program, id) == "app::Context::GetCallbacks")
        .collect();
    assert_eq!(getters.len(), 2);
    assert_ne!(getters[0], getters[1], "arity selects each overload");
    assert!(has_any_edge(
        program,
        analysis,
        "app::Context::Dispatch",
        "app::Callbacks::run"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "app::Context::Dispatch",
        "app::GetCallbacks"
    ));
}

#[test]
fn callgraph_shows_unresolved_calls_as_distinct_leaves() {
    let (program, analysis) = cpp_call_sites();
    let pag = trace_analysis::Pag::build(program);
    let db = export_program(program, &pag, analysis);
    let conn = Connection::open(db.path()).unwrap();
    let caller = only_function(program, "app::untyped");
    let graph = trace_db::call_graph(&conn, caller.0.into(), trace_db::Direction::Down, 2).unwrap();
    let unresolved: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.label == "unresolved")
        .collect();
    assert_eq!(unresolved.len(), 2);
    for edge in unresolved {
        let leaf = &graph.nodes[&edge.to];
        assert_eq!(leaf.detail, "[unresolved]");
        assert!(leaf.label.ends_with(".run"));
        assert!(!graph.edges.iter().any(|e| e.from == leaf.id));
    }
    assert!(!graph.truncated);
    let shallow =
        trace_db::call_graph(&conn, caller.0.into(), trace_db::Direction::Down, 0).unwrap();
    assert!(shallow.truncated);
    assert_eq!(shallow.nodes.len(), 1);
    let up = trace_db::call_graph(&conn, caller.0.into(), trace_db::Direction::Up, 2).unwrap();
    assert_eq!(up.nodes.len(), 1);

    let meta = trace_db::GraphMeta {
        title: "unresolved calls",
        direction: "callees",
        depth: 2,
        summary: "",
    };
    for format in [
        trace_db::RenderFormat::Text,
        trace_db::RenderFormat::Json,
        trace_db::RenderFormat::Graphviz,
        trace_db::RenderFormat::Mermaid,
    ] {
        let output = trace_db::render_graph(&graph, format, &meta, &mut |id, out| {
            out.push_str(&graph.nodes[&id].label);
        });
        assert!(output.contains("value.run"));
        assert!(output.contains("unknown().run"));
        if format == trace_db::RenderFormat::Graphviz || format == trace_db::RenderFormat::Mermaid {
            assert!(
                !output.contains("n-"),
                "leaf identifiers must be valid: {output}"
            );
        }
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .args([
            "inspect",
            db.path().to_str().unwrap(),
            "callgraph",
            "--file",
            "main.cpp",
            "--line",
            "12",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("value.run ([unresolved])"), "{text}");
    assert!(text.contains("unknown().run ([unresolved])"), "{text}");
    assert!(
        text.contains("2 functions, 2 unresolved calls, 4 edges"),
        "{text}"
    );
}

#[test]
fn chained_calls_have_distinct_method_positions() {
    let (program, _) = cpp_call_sites();
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| fn_name(program, site.caller) == "app::chained")
        .map(|site| (site.callee_name.as_str(), site.span.line, site.span.col))
        .collect();
    assert_eq!(
        sites,
        vec![
            ("app::Chain::finish", 9, 17),
            ("app::Chain::next", 9, 10),
            ("app::Chain::next", 8, 12),
            ("make", 8, 5),
        ]
    );
}
