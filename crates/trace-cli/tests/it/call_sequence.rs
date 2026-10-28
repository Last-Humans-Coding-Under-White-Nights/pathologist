//! Call sequencing (`--sequencing`; docs/ANALYSIS.md, "Call sequencing
//! (control-flow sidecar)").

use crate::common::{
    analysis_rows, default_opts, export_program, export_program_full, fixture, only_function,
    scratch, text_rows, TempDb,
};
use std::path::Path;
use trace_ir::{FunctionCfg, Program};
use trace_preproc::PreprocessOptions;

#[test]
fn sequencing_is_off_by_default() {
    let root = fixture("direct_call");
    let program = trace_parse::build_program(&root, &default_opts(&root)).expect("build");
    assert!(!program.call_sequencing);
    assert!(program.cfgs.is_empty());
}

#[test]
fn sequencing_flag_reaches_the_program() {
    let root = fixture("direct_call");
    let opts = default_opts(&root).with_call_sequencing(true);
    let program = trace_parse::build_program(&root, &opts).expect("build");
    assert!(program.call_sequencing);
}

/// `root` built with `--sequencing` on top of `opts`.
fn sequenced(root: &Path, opts: PreprocessOptions) -> Program {
    trace_parse::build_program(root, &opts.with_call_sequencing(true)).expect("build")
}

/// Every graph of the functions named `name`, in function and variant order.
fn cfgs_named<'p>(program: &'p Program, name: &str) -> Vec<&'p FunctionCfg> {
    program
        .cfgs
        .iter()
        .filter(|(&f, _)| program.symbols.function(f).name == name)
        .flat_map(|(_, variants)| variants)
        .collect()
}

/// Each event's site callees, in event order; one inner list per event.
fn event_callees(program: &Program, cfg: &FunctionCfg) -> Vec<Vec<String>> {
    cfg.events
        .iter()
        .map(|e| {
            e.sites
                .iter()
                .map(|&s| {
                    program
                        .symbols
                        .call_site_by_id(s)
                        .expect("event names a merged site")
                        .callee_name
                        .as_str()
                        .to_string()
                })
                .collect()
        })
        .collect()
}

/// Every graph names merged sites its own function calls from, and its
/// variant numbers count up from zero.
fn assert_merged_ids(program: &Program) {
    for (&function, variants) in &program.cfgs {
        for (position, cfg) in variants.iter().enumerate() {
            assert_eq!(cfg.function, function);
            assert_eq!(cfg.variant, position as u32);
            assert!(cfg.tu.is_some(), "merge records the unit");
            for event in &cfg.events {
                assert!(!event.sites.is_empty());
                for &site in &event.sites {
                    let site = program.symbols.call_site_by_id(site).expect("merged site");
                    assert_eq!(site.caller, function);
                }
            }
        }
    }
}

#[test]
fn merged_graph_names_merged_functions_and_sites() {
    let root = fixture("call_sequence");
    let program = sequenced(&root, default_opts(&root));
    assert_merged_ids(&program);
    // `branches.c` defines a `run` of its own.
    let run: Vec<_> = cfgs_named(&program, "run")
        .into_iter()
        .filter(|g| {
            program.symbols.files[g.tu.unwrap().0 as usize]
                .path
                .ends_with("straight.c")
        })
        .collect();
    assert_eq!(run.len(), 1);
    assert_eq!(
        event_callees(&program, run[0]),
        [["prepare"], ["work"], ["finish"]]
    );
}

#[test]
fn shared_header_body_keeps_one_graph() {
    let dir = scratch(&[
        (
            "shared.h",
            "void a(); void b();\n\
             inline void shared_inline() { a(); b(); }\n\
             static inline void shared_static() { b(); a(); }\n",
        ),
        (
            "one.cpp",
            "#include \"shared.h\"\nvoid one() { shared_inline(); shared_static(); }\n",
        ),
        (
            "two.cpp",
            "#include \"shared.h\"\nvoid two() { shared_inline(); shared_static(); }\n",
        ),
    ]);
    let program = sequenced(dir.path(), default_opts(dir.path()));
    assert_merged_ids(&program);
    for (name, order) in [("shared_inline", ["a", "b"]), ("shared_static", ["b", "a"])] {
        let graphs = cfgs_named(&program, name);
        assert_eq!(graphs.len(), 1, "{name}: one graph for both units");
        assert_eq!(event_callees(&program, graphs[0]), [[order[0]], [order[1]]]);
        // Every record of the body's calls is an event's site.
        let function = graphs[0].function;
        for site in program
            .symbols
            .call_sites
            .iter()
            .filter(|s| s.caller == function)
        {
            assert!(
                graphs[0].events.iter().any(|e| e.sites.contains(&site.id)),
                "{name}: site {:?} has no event",
                site.id
            );
        }
    }
}

#[test]
fn explore_variants_stay_separate_graphs() {
    let dir = scratch(&[
        (
            "BUILD.gn",
            "source_set(\"t\") {\n  defines = [ \"BACKEND=1\", \"BACKEND=2\" ]\n}\n",
        ),
        (
            "main.c",
            "void prepare(void); void send_fast(void); void send_slow(void);\n\
             void send(void) {\n\
                 prepare();\n\
             #if BACKEND == 1\n\
                 send_fast();\n\
             #elif BACKEND == 2\n\
                 send_slow();\n\
             #endif\n\
             }\n\
             int main(void) { send(); return 0; }\n",
        ),
    ]);
    let opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let program = sequenced(dir.path(), opts);
    assert_merged_ids(&program);
    // The body every configuration shares is one graph.
    assert_eq!(cfgs_named(&program, "main").len(), 1);
    let send = cfgs_named(&program, "send");
    let shapes: Vec<_> = send.iter().map(|g| event_callees(&program, g)).collect();
    for wanted in ["send_fast", "send_slow"] {
        assert!(
            shapes
                .iter()
                .any(|s| s == &[vec!["prepare".to_string()], vec![wanted.to_string()]]),
            "{wanted} variant missing: {shapes:?}"
        );
    }
    // No graph mixes two configurations.
    for shape in &shapes {
        let calls: Vec<_> = shape.iter().flatten().collect();
        assert!(
            !(calls.contains(&&"send_fast".to_string())
                && calls.contains(&&"send_slow".to_string()))
        );
    }
}

#[test]
fn virtual_override_records_are_alternatives_of_one_event() {
    // The override lives in a unit the call never sees, so only the
    // post-merge virtual expansion records it.
    let dir = scratch(&[
        ("base.h", "struct Base { virtual void step(); };\n"),
        (
            "derived.cpp",
            "#include \"base.h\"\n\
             struct Derived : Base { void step() override; };\n\
             void Derived::step() {}\n",
        ),
        (
            "main.cpp",
            "#include \"base.h\"\n\
             void before(); void after();\n\
             void drive(Base *b) { before(); b->step(); after(); }\n",
        ),
    ]);
    let program = sequenced(dir.path(), default_opts(dir.path()));
    assert_merged_ids(&program);
    let drive = cfgs_named(&program, "drive");
    assert_eq!(drive.len(), 1);
    let drive = drive[0];
    let callees = event_callees(&program, drive);
    assert_eq!(callees.len(), 3, "{callees:?}");
    assert_eq!(callees[0], ["before"]);
    assert_eq!(callees[1], ["Base::step", "Derived::step"]);
    assert_eq!(callees[2], ["after"]);
    // Every record of the dispatch, the expanded override included, is a site
    // of that one event; none is an event ordered against another.
    let function = drive.function;
    let step_sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == function && s.callee_name.as_str().ends_with("::step"))
        .map(|s| s.id)
        .collect();
    assert_eq!(step_sites.len(), 2);
    for site in step_sites {
        let holders: Vec<_> = drive
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.sites.contains(&site))
            .collect();
        assert_eq!(holders.len(), 1);
    }
}

#[test]
fn dependency_bodies_have_no_graph() {
    let dir = scratch(&[
        (
            "dep/api.h",
            "void inner(); void other();\ninline void implementation() { inner(); other(); }\n",
        ),
        (
            "main.cpp",
            "#include <api.h>\nvoid target() { implementation(); }\n",
        ),
    ]);
    let dep = dir.path().join("dep");
    let program = sequenced(
        dir.path(),
        PreprocessOptions::new()
            .with_dep(&dep)
            .with_include(dep.clone()),
    );
    assert_merged_ids(&program);
    assert!(cfgs_named(&program, "implementation").is_empty());
    let target = cfgs_named(&program, "target");
    assert_eq!(target.len(), 1);
    assert_eq!(event_callees(&program, target[0]), [["implementation"]]);
}

#[test]
fn suppressed_weak_body_has_no_graph_in_its_target() {
    let dir = scratch(&[
        (
            "weak.c",
            "void w1(void); void w2(void);\n\
             __attribute__((weak)) void hook(void) { w1(); w2(); }\n",
        ),
        ("strong.c", "void s1(void);\nvoid hook(void) { s1(); }\n"),
    ]);
    let root = dir.path();
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!([
            {"directory": root, "file": "weak.c", "output": "weak.o",
             "arguments": ["cc", "-c", "weak.c", "-o", "weak.o"]},
            {"directory": root, "file": "strong.c", "output": "strong.o",
             "arguments": ["cc", "-c", "strong.c", "-o", "strong.o"]}
        ])
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("link_commands.json"),
        serde_json::json!([
            {"directory": root, "output": "full",
             "arguments": ["cc", "weak.o", "strong.o", "-o", "full"]}
        ])
        .to_string(),
    )
    .unwrap();
    let program = sequenced(root, default_opts(root));
    assert_merged_ids(&program);
    let hooks = cfgs_named(&program, "hook");
    let shapes: Vec<_> = hooks.iter().map(|g| event_callees(&program, g)).collect();
    assert_eq!(shapes, [[["s1"]]], "only the strong body is sequenced");
}

/// The `call_sequence` fixture built with `opts`, analysed and exported in
/// minimal (`full = false`) or full mode.
fn exported(sequencing: bool, full: bool) -> (Program, trace_analysis::AnalysisResult, TempDb) {
    let root = fixture("call_sequence");
    let program =
        trace_parse::build_program(&root, &default_opts(&root).with_call_sequencing(sequencing))
            .expect("build");
    let (pag, analysis) = trace_analysis::analyze(&program);
    let db = if full {
        export_program_full(&program, &pag, &analysis)
    } else {
        export_program(&program, &pag, &analysis)
    };
    (program, analysis, db)
}

const CFG_TABLES: [&str; 7] = [
    "cfg_functions",
    "cfg_blocks",
    "cfg_edges",
    "cfg_events",
    "cfg_event_sites",
    "cfg_event_order",
    "cfg_gaps",
];

/// Every `cfg_*` table's rows, sorted, by table name.
fn cfg_rows(db: &TempDb) -> Vec<(String, Vec<Vec<String>>)> {
    analysis_rows(db)
        .into_iter()
        .filter(|(table, _)| CFG_TABLES.contains(&table.as_str()))
        .collect()
}

fn sequencing_marker(db: &TempDb) -> serde_json::Value {
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let options: String = conn
        .query_row("SELECT options_json FROM analysis_run", [], |r| r.get(0))
        .unwrap();
    serde_json::from_str::<serde_json::Value>(&options).unwrap()["call_sequencing"].clone()
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn export_writes_every_graph_and_marks_the_capability() {
    let (program, _analysis, db) = exported(true, false);
    assert_eq!(sequencing_marker(&db), serde_json::json!(true));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let graphs: Vec<_> = program.cfgs.values().flatten().collect();
    let sum = |f: fn(&FunctionCfg) -> usize| graphs.iter().map(|g| f(g)).sum::<usize>() as i64;
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_functions"),
        graphs.len() as i64
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_blocks"),
        sum(|g| g.blocks.len())
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_events"),
        sum(|g| g.events.len())
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_event_order"),
        sum(|g| g.order.len())
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_gaps"),
        sum(|g| g.gaps.len())
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM cfg_event_sites"),
        sum(|g| g.events.iter().map(|e| e.sites.len()).sum())
    );
    // A body with a gap is partial; the others are complete.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM cfg_functions f JOIN strings s ON s.id = f.coverage_id \
             WHERE s.text = 'partial'"
        ),
        graphs.iter().filter(|g| !g.gaps.is_empty()).count() as i64
    );
    // `straight.c`'s `run`: prepare, work, finish in one block, in order.
    let order = text_rows(
        &conn,
        "SELECT a.callee_text, b.callee_text FROM cfg_event_order o \
         JOIN cfg_event_sites sa ON sa.event_id = o.event_id \
         JOIN call_sites a ON a.id = sa.call_site_id \
         JOIN cfg_event_sites sb ON sb.event_id = o.next_event_id \
         JOIN call_sites b ON b.id = sb.call_site_id \
         JOIN cfg_events e ON e.id = o.event_id \
         JOIN files f ON f.id = e.file_id \
         WHERE f.path LIKE '%straight.c' ORDER BY e.line",
    );
    let text = |s: &str| format!("{:?}", rusqlite::types::Value::Text(s.into()));
    assert_eq!(
        order,
        [
            vec![text("prepare"), text("work")],
            vec![text("work"), text("finish")]
        ]
    );
    // Kind tags are seeded with small ids, ahead of names and expressions.
    assert!(
        count(
            &conn,
            "SELECT MAX(kind_id) FROM (SELECT kind_id FROM cfg_blocks \
             UNION SELECT kind_id FROM cfg_edges UNION SELECT kind_id FROM cfg_events \
             UNION SELECT kind_id FROM cfg_gaps UNION SELECT coverage_id FROM cfg_functions)"
        ) < 128
    );
}

#[test]
fn export_retains_every_sequenced_site() {
    let dir = scratch(&[(
        "a.c",
        "int open(int);\nvoid connect(void) { open(1); }\nvoid other(void) { open(2); }\n",
    )]);
    let mut program = sequenced(dir.path(), default_opts(dir.path()));
    let (pag, mut analysis) = trace_analysis::analyze(&program);
    // Without edges or argument flow, the ordinary filter drops a direct
    // site (docs/SQLITE_SCHEMA.md, "Call site export filter").
    analysis.call_edges.clear();
    analysis.arg_flow_edges.clear();
    // Only `connect`'s graph names its site; `other`'s stays unsequenced.
    let connect = only_function(&program, "connect");
    program.cfgs.retain(|&f, _| f == connect);
    let site = program.cfgs[&connect][0].events[0].sites[0];
    assert!(program.symbols.call_site_by_id(site).unwrap().is_direct);
    let db = export_program(&program, &pag, &analysis);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    assert_eq!(
        text_rows(&conn, "SELECT id FROM call_sites"),
        [vec![format!(
            "{:?}",
            rusqlite::types::Value::Integer(site.0 as i64)
        )]],
        "the sequenced site is kept, the other one still filtered"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM cfg_event_sites es \
             LEFT JOIN call_sites cs ON cs.id = es.call_site_id WHERE cs.id IS NULL"
        ),
        0,
        "every site an event names is exported"
    );
}

#[test]
fn export_is_deterministic() {
    let (_, _, first) = exported(true, false);
    let (_, _, second) = exported(true, false);
    let rows = cfg_rows(&first);
    assert_eq!(rows.len(), CFG_TABLES.len());
    assert!(rows.iter().all(|(_, r)| !r.is_empty()), "{rows:?}");
    assert_eq!(analysis_rows(&first), analysis_rows(&second));
}

#[test]
fn minimal_and_full_exports_agree() {
    let (_, _, minimal) = exported(true, false);
    let (_, _, full) = exported(true, true);
    let rows = cfg_rows(&minimal);
    assert!(rows.iter().any(|(_, r)| !r.is_empty()));
    assert_eq!(rows, cfg_rows(&full));
    let sites = |db: &TempDb| {
        let conn = rusqlite::Connection::open(db.path()).unwrap();
        text_rows(&conn, "SELECT * FROM call_sites ORDER BY id")
    };
    assert_eq!(sites(&minimal), sites(&full));
}

#[test]
fn export_without_sequencing_has_no_cfg_rows() {
    let (_, _, db) = exported(false, false);
    assert_eq!(sequencing_marker(&db), serde_json::json!(false));
    let rows = cfg_rows(&db);
    assert_eq!(rows.len(), CFG_TABLES.len(), "the tables exist");
    assert!(rows.iter().all(|(_, r)| r.is_empty()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM strings WHERE text IN ('fallthrough', 'landing', 'complete')"
        ),
        0,
        "kind tags are interned only for a sequenced export"
    );
}

// --- `inspect sequence` (docs/ANALYSIS.md, "Call sequencing") ---

/// `root` built with `--sequencing` on top of `opts`, analysed and exported.
fn sequenced_db(root: &Path, opts: PreprocessOptions) -> TempDb {
    let program = sequenced(root, opts);
    let (pag, analysis) = trace_analysis::analyze(&program);
    export_program(&program, &pag, &analysis)
}

/// The `call_sequence` fixture, exported once per test binary.
fn fixture_db() -> rusqlite::Connection {
    static DB: std::sync::OnceLock<TempDb> = std::sync::OnceLock::new();
    let db = DB.get_or_init(|| {
        let root = fixture("call_sequence");
        sequenced_db(&root, default_opts(&root))
    });
    rusqlite::Connection::open(db.path()).unwrap()
}

/// The event calling `callee` written on `file:line`.
fn event(conn: &rusqlite::Connection, file: &str, line: i64, callee: &str) -> trace_db::EventRef {
    trace_db::resolve_event(conn, file, line, None, Some(callee)).expect("event")
}

/// The relation the query reports from `a` to `b`.
fn relation(
    conn: &rusqlite::Connection,
    a: &trace_db::EventRef,
    b: &trace_db::EventRef,
) -> trace_db::Relation {
    let scope = trace_db::SequenceScope::Between(a.clone(), b.clone());
    let graph = trace_db::sequence_graph(conn, &scope, &trace_db::SequenceOptions::default())
        .expect("sequence");
    graph
        .relation
        .expect("a between query relates its ends")
        .kind
}

/// A function's sequence with `options`.
fn function_sequence(
    conn: &rusqlite::Connection,
    name: &str,
    file: Option<&str>,
    options: &trace_db::SequenceOptions,
) -> trace_db::SequenceGraph {
    let function = trace_db::resolve_function_target(conn, Some(name), file, None).unwrap();
    trace_db::sequence_graph(
        conn,
        &trace_db::SequenceScope::Function {
            function: trace_db::FunctionRowId(function.id),
            file: None,
        },
        options,
    )
    .expect("sequence")
}

fn callees(graph: &trace_db::SequenceGraph) -> Vec<&str> {
    graph.events.iter().map(|e| e.callee.as_str()).collect()
}

#[test]
fn sequence_query_requires_the_capability() {
    let (_, _, db) = exported(false, false);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let err = trace_db::require_sequencing(&conn).unwrap_err().to_string();
    assert!(err.contains("--sequencing"), "{err}");
    let err = trace_db::resolve_event(&conn, "branches.c", 8, None, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("--sequencing"), "{err}");
    let conn = fixture_db();
    let capability = trace_db::require_sequencing(&conn).unwrap();
    assert!(capability == trace_db::SequencingMarker::Complete && capability.notice().is_none());
}

#[test]
fn branch_arms_are_exclusive() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "branches.c", line, callee);
    let (prepare, send, recover, finish) = (
        at(8, "prepare"),
        at(10, "send"),
        at(12, "recover"),
        at(13, "finish"),
    );
    assert_eq!(relation(&conn, &send, &recover), Exclusive);
    assert_eq!(relation(&conn, &recover, &send), Exclusive);
    assert_eq!(relation(&conn, &prepare, &finish), GuaranteedBefore);
    assert_eq!(relation(&conn, &prepare, &send), GuaranteedBefore);
    // A path, not a guarantee: `send` runs only when `ok` holds.
    assert_eq!(relation(&conn, &send, &finish), MayPrecede);
    assert_eq!(relation(&conn, &finish, &prepare), MayFollow);

    // The whole body: no edge or order joins the two arms. (Both C fixtures
    // define `run`, so the function has two graphs; start from an event.)
    let graph = trace_db::sequence_graph(
        &conn,
        &trace_db::SequenceScope::From(prepare.clone()),
        &Default::default(),
    )
    .unwrap();
    assert_eq!(callees(&graph), ["prepare", "send", "recover", "finish"]);
    let block_of = |callee: &str| {
        graph
            .events
            .iter()
            .find(|e| e.callee == callee)
            .unwrap()
            .block
    };
    let (send_block, recover_block) = (block_of("send"), block_of("recover"));
    assert_ne!(send_block, recover_block);
    assert!(!graph.edges.iter().any(|e| {
        (e.src, e.dst) == (send_block, recover_block)
            || (e.src, e.dst) == (recover_block, send_block)
    }));
    assert!(
        graph.order.is_empty(),
        "one event per block: {:?}",
        graph.order
    );
}

#[test]
fn between_keeps_only_blocks_on_a_path() {
    let conn = fixture_db();
    let prepare = event(&conn, "branches.c", 8, "prepare");
    let send = event(&conn, "branches.c", 10, "send");
    let graph = trace_db::sequence_graph(
        &conn,
        &trace_db::SequenceScope::Between(prepare, send),
        &Default::default(),
    )
    .unwrap();
    assert_eq!(
        callees(&graph),
        ["prepare", "send"],
        "`recover` is off the path"
    );
}

#[test]
fn every_call_reaching_the_second_end_is_cited() {
    let dir = scratch(&[(
        "reach.c",
        "void first(void); void target(void);\n\
         void helper(void) { target(); }\n\
         void run(int c) {\n\
           first();\n\
           if (c) helper();\n\
           else helper();\n\
         }\n",
    )]);
    let db = sequenced_db(dir.path(), default_opts(dir.path()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let first = event(&conn, "reach.c", 4, "first");
    let target = event(&conn, "reach.c", 2, "target");
    let graph = trace_db::sequence_graph(
        &conn,
        &trace_db::SequenceScope::Between(first, target.clone()),
        &Default::default(),
    )
    .unwrap();
    let relation = graph.relation.expect("a between query relates its ends");
    let calls: Vec<trace_db::CfgEventId> = [5, 6]
        .map(|line| event(&conn, "reach.c", line, "helper").event)
        .into();
    assert_eq!(relation.via, calls, "both reaching calls are recorded");
    // Typed row ids serialize as the bare `cfg_events.id` numbers.
    let ids: Vec<i64> = calls.iter().map(|c| c.0).collect();
    assert_eq!(
        serde_json::to_value(&relation).unwrap()["via"],
        serde_json::json!(ids)
    );
    let from = serde_json::to_value(&relation.from).unwrap();
    assert_eq!(from["event"], serde_json::json!(relation.from.event.0));
    assert_eq!(from["cfg"], serde_json::json!(relation.from.cfg.0));
    assert_eq!(
        from["function"],
        serde_json::json!(relation.from.function.0)
    );
    let cited =
        "target() at reach.c:2:21 runs inside the calls at reach.c:5:8, reach.c:6:6".to_string();
    assert!(relation.notes.contains(&cited), "{:?}", relation.notes);
    let json = serde_json::to_value(&relation).unwrap();
    assert_eq!(json["via"], serde_json::json!(calls));
}

#[test]
fn expression_order_follows_the_language() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "shortcircuit.c", line, callee);
    // `a() && b()`: `b` runs only after `a` returned true.
    let (a, b) = (at(13, "a"), at(13, "b"));
    assert_eq!(relation(&conn, &a, &b), GuaranteedBefore);
    let graph = function_sequence(&conn, "both", None, &Default::default());
    let block_of = |callee: &str| {
        graph
            .events
            .iter()
            .find(|e| e.callee == callee)
            .unwrap()
            .block
    };
    assert!(graph
        .edges
        .iter()
        .any(|e| (e.src, e.dst, e.kind.as_str()) == (block_of("a"), block_of("b"), "branch_true")));
    // `f(g(), h())`: arguments are unordered, both precede the call.
    let (g, h, f) = (at(19, "g"), at(19, "h"), at(19, "f"));
    assert_eq!(relation(&conn, &g, &h), Unordered);
    assert_eq!(relation(&conn, &h, &g), Unordered);
    assert_eq!(relation(&conn, &g, &f), SequencedBefore);
    assert_eq!(relation(&conn, &h, &f), SequencedBefore);
    // `f1(g()) + k()`: the operands are unordered.
    let (g, f1, k) = (at(24, "g"), at(24, "f1"), at(24, "k"));
    assert_eq!(relation(&conn, &g, &f1), SequencedBefore);
    assert_eq!(relation(&conn, &f1, &k), Unordered);
    assert_eq!(relation(&conn, &g, &k), Unordered);
    // `(p(), q())`: the comma operator is a sequence point.
    assert_eq!(relation(&conn, &at(29, "p"), &at(29, "q")), SequencedBefore);
}

#[test]
fn ambiguous_position_lists_the_candidates() {
    let conn = fixture_db();
    let err = trace_db::resolve_event(&conn, "shortcircuit.c", 19, None, None)
        .unwrap_err()
        .to_string();
    for callee in ["f", "g", "h"] {
        assert!(err.contains(&format!("{callee} at")), "{err}");
    }
    let err = trace_db::resolve_event(&conn, "shortcircuit.c", 2, None, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no call event"), "{err}");
}

#[test]
fn file_narrows_a_function_to_the_bodies_written_there() {
    let conn = fixture_db();
    // `straight.c` and `branches.c` both define an external `run`: one
    // function row, one graph per body.
    let in_file = |file| {
        let scope = trace_db::resolve_sequence_function(&conn, "run", file).unwrap();
        trace_db::sequence_graph(&conn, &scope, &Default::default()).unwrap()
    };
    let graph = in_file(Some("branches.c"));
    assert_eq!(graph.frames.len(), 1);
    assert_eq!(callees(&graph), ["prepare", "send", "recover", "finish"]);
    let graph = in_file(Some("straight.c"));
    assert_eq!(graph.frames.len(), 1);
    assert_eq!(callees(&graph), ["prepare", "work", "finish"]);
    assert_eq!(in_file(None).frames.len(), 2);
    let err = trace_db::resolve_sequence_function(&conn, "run", Some("loops.c"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("run") && err.contains("loops.c"), "{err}");
}

#[test]
fn ambiguous_macro_body_position_is_capped_and_points_at_invocations() {
    let calls: String = (0..15).map(|_| "    LOG(a());\n").collect();
    let dir = scratch(&[
        ("log.h", "void log_it(int v);\n#define LOG(e) log_it(e)\n"),
        (
            "main.c",
            &format!("#include \"log.h\"\nint a(void);\nvoid many(void)\n{{\n{calls}}}\n"),
        ),
    ]);
    let db = sequenced_db(dir.path(), default_opts(dir.path()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let err = trace_db::resolve_event(&conn, "log.h", 2, None, Some("log_it"))
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("15 call events"), "{err}");
    assert_eq!(err.matches("log_it at").count(), 10, "{err}");
    assert!(err.contains("and 5 more"), "{err}");
    assert!(
        err.contains("macro") && err.contains("main.c:5"),
        "suggests the invocation position: {err}"
    );
    // The invocation position names one event.
    let at = trace_db::resolve_event(&conn, "main.c", 5, None, Some("log_it")).unwrap();
    assert_eq!(at.callee, "log_it");
}

#[test]
fn recursion_is_summarized_once() {
    let conn = fixture_db();
    let options = trace_db::SequenceOptions {
        depth: 5,
        ..Default::default()
    };
    let graph = function_sequence(&conn, "walk", None, &options);
    assert_eq!(graph.frames.len(), 1, "the recursive call is not inlined");
    let kinds: Vec<_> = graph.boundaries.iter().map(|b| b.kind.as_str()).collect();
    assert_eq!(
        kinds.iter().filter(|&&k| k == "recursion").count(),
        1,
        "{kinds:?}"
    );
    let leaf = graph
        .boundaries
        .iter()
        .find(|b| b.name.as_deref() == Some("leaf"))
        .unwrap();
    assert_eq!(
        (leaf.kind.as_str(), leaf.reason.as_deref()),
        ("omitted_body", Some("declaration_only"))
    );
}

/// A caller of an indirect call, a sequenced callee, a prototype and a
/// dependency-header body.
fn boundary_tree() -> tempfile::TempDir {
    scratch(&[
        (
            "dep/api.h",
            "void inner();\ninline void depbody() { inner(); }\n",
        ),
        (
            "main.cpp",
            "#include <api.h>\n\
             void leaf();\n\
             void proto();\n\
             void known() { leaf(); }\n\
             void run(void (*fp)()) {\n\
                 fp();\n\
                 known();\n\
                 proto();\n\
                 depbody();\n\
             }\n",
        ),
    ])
}

fn boundary_db(dir: &tempfile::TempDir) -> TempDb {
    let dep = dir.path().join("dep");
    sequenced_db(
        dir.path(),
        PreprocessOptions::new()
            .with_dep(&dep)
            .with_include(dep.clone()),
    )
}

#[test]
fn boundaries_say_why_a_callee_is_not_inlined() {
    let dir = boundary_tree();
    let db = boundary_db(&dir);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let graph = function_sequence(&conn, "run", None, &Default::default());
    assert_eq!(callees(&graph), ["fp", "known", "proto", "depbody", "leaf"]);
    let boundary = |name: &str| {
        let event = graph.events.iter().find(|e| e.callee == name).unwrap().id;
        graph
            .boundaries
            .iter()
            .find(|b| b.event == Some(event))
            .map(|b| (b.kind.as_str(), b.reason.as_deref()))
    };
    assert_eq!(boundary("fp"), Some(("unknown_callee", None)));
    assert_eq!(boundary("known"), None, "`known` is inlined");
    assert_eq!(
        boundary("proto"),
        Some(("omitted_body", Some("declaration_only")))
    );
    assert_eq!(
        boundary("depbody"),
        Some(("omitted_body", Some("dependency")))
    );
    // An unknown callee is still invoked before the next call.
    let at = |line, callee| event(&conn, "main.cpp", line, callee);
    assert_eq!(
        relation(&conn, &at(6, "fp"), &at(7, "known")),
        trace_db::Relation::SequencedBefore
    );
}

#[test]
fn inlined_callee_returns_to_its_call() {
    let dir = boundary_tree();
    let db = boundary_db(&dir);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let graph = function_sequence(&conn, "run", None, &Default::default());
    assert_eq!(graph.frames.len(), 2);
    let known = graph.events.iter().find(|e| e.callee == "known").unwrap();
    let child = &graph.frames[1];
    assert_eq!(child.name, "known");
    assert_eq!(child.parent_event, Some(known.id));
    let target = known.targets.iter().find(|t| t.name == "known").unwrap();
    assert_eq!(target.frame, Some(child.id));
    let call = graph.edges.iter().find(|e| e.kind == "call").unwrap();
    assert_eq!(
        (call.src, call.dst, call.event),
        (known.block, child.entry, Some(known.id))
    );
    let ret = graph.edges.iter().find(|e| e.kind == "return").unwrap();
    assert_eq!(
        (ret.src, ret.dst, ret.event),
        (child.exit, known.block, Some(known.id))
    );
    // Across the call, `leaf` runs between `known`'s invocation and `proto`.
    let at = |line, callee| event(&conn, "main.cpp", line, callee);
    let leaf = at(4, "leaf");
    assert_eq!(
        relation(&conn, &at(6, "fp"), &leaf),
        trace_db::Relation::SequencedBefore
    );
    assert_eq!(
        relation(&conn, &at(8, "proto"), &leaf),
        trace_db::Relation::MayFollow
    );
}

#[test]
fn budgets_mark_what_they_cut() {
    let dir = boundary_tree();
    let db = boundary_db(&dir);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let options = trace_db::SequenceOptions {
        max_frames: 1,
        ..Default::default()
    };
    let graph = function_sequence(&conn, "run", None, &options);
    assert_eq!(graph.frames.len(), 1);
    let reasons: Vec<_> = graph.truncated.iter().map(|t| t.reason.as_str()).collect();
    assert_eq!(reasons, ["max_frames"]);
    let options = trace_db::SequenceOptions {
        max_events: 4,
        ..Default::default()
    };
    let graph = function_sequence(&conn, "run", None, &options);
    assert_eq!(
        graph.frames.len(),
        1,
        "`known`'s event would exceed the budget"
    );
    let reasons: Vec<_> = graph.truncated.iter().map(|t| t.reason.as_str()).collect();
    assert_eq!(reasons, ["max_events"]);
    let graph = function_sequence(&conn, "run", None, &Default::default());
    assert!(graph.truncated.is_empty());
}

/// Once the frame budget is spent, inlining stops: one marker, not one
/// per remaining call.
#[test]
fn frame_budget_cuts_once() {
    let dir = scratch(&[(
        "calls.c",
        "void leaf(void);\n\
         void f1(void) { leaf(); }\n\
         void f2(void) { leaf(); }\n\
         void f3(void) { leaf(); }\n\
         void run(void) { f1(); f2(); f3(); }\n",
    )]);
    let db = sequenced_db(dir.path(), default_opts(dir.path()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let options = trace_db::SequenceOptions {
        max_frames: 2,
        ..Default::default()
    };
    let graph = function_sequence(&conn, "run", None, &options);
    assert_eq!(graph.frames.len(), 2);
    let reasons: Vec<_> = graph.truncated.iter().map(|t| t.reason.as_str()).collect();
    assert_eq!(reasons, ["max_frames"], "{:?}", graph.truncated);
    let f2 = graph.events.iter().find(|e| e.callee == "f2").unwrap();
    assert_eq!(graph.truncated[0].event, Some(f2.id));
}

/// Two calls of one variant reaching the second end through the same kind
/// of gap state its note once.
#[test]
fn relation_notes_are_not_repeated() {
    let dir = scratch(&[(
        "gaps.c",
        "void target(void);\n\
         void first(void);\n\
         int mid(void) { target(); return 0; }\n\
         void run(void) {\n\
           first();\n\
           int x = ({ int y = mid(); y; });\n\
           int z = ({ int w = mid(); w; });\n\
         }\n",
    )]);
    let db = sequenced_db(dir.path(), default_opts(dir.path()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let first = event(&conn, "gaps.c", 5, "first");
    let target = event(&conn, "gaps.c", 3, "target");
    let graph = trace_db::sequence_graph(
        &conn,
        &trace_db::SequenceScope::Between(first, target),
        &Default::default(),
    )
    .unwrap();
    let relation = graph.relation.expect("a between query relates its ends");
    assert_eq!(relation.kind, trace_db::Relation::Unknown);
    assert_eq!(relation.via.len(), 2, "{relation:?}");
    let mut notes = relation.notes.clone();
    notes.sort();
    notes.dedup();
    assert_eq!(notes.len(), relation.notes.len(), "{:?}", relation.notes);
}

#[test]
fn variants_are_sibling_frames() {
    let dir = scratch(&[
        (
            "BUILD.gn",
            "source_set(\"t\") {\n  defines = [ \"BACKEND=1\", \"BACKEND=2\" ]\n}\n",
        ),
        (
            "main.c",
            "void prepare(void); void send_fast(void); void send_slow(void);\n\
             void send(void) {\n\
                 prepare();\n\
             #if BACKEND == 1\n\
                 send_fast();\n\
             #elif BACKEND == 2\n\
                 send_slow();\n\
             #endif\n\
             }\n",
        ),
    ]);
    let db = sequenced_db(
        dir.path(),
        PreprocessOptions::new()
            .with_explore(true)
            .with_explore_budget(4),
    );
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let graph = function_sequence(&conn, "send", None, &Default::default());
    let variants: Vec<_> = graph
        .frames
        .iter()
        .map(|f| (f.variant, f.parent_event))
        .collect();
    assert!(variants.len() >= 2, "{variants:?}");
    for wanted in ["send_fast", "send_slow"] {
        assert!(graph.events.iter().any(|e| e.callee == wanted), "{wanted}");
    }
    assert!(variants.iter().all(|&(_, parent)| parent.is_none()));
    // No edge or order pair crosses from one configuration to the other.
    let frame_of_block = |b: usize| graph.blocks[b].frame;
    assert!(graph
        .edges
        .iter()
        .all(|e| frame_of_block(e.src) == frame_of_block(e.dst)));
    assert!(graph
        .order
        .iter()
        .all(|&(a, b)| graph.events[a].frame == graph.events[b].frame));
}

/// A call every configuration variant compiles is one call: its position
/// names it in all of them, and a query from it covers each variant.
#[test]
fn a_call_in_several_variants_is_one_position() {
    use trace_db::Relation::*;
    let dir = scratch(&[
        (
            "BUILD.gn",
            "source_set(\"t\") {\n  defines = [ \"BACKEND=1\", \"BACKEND=2\" ]\n}\n",
        ),
        (
            "main.c",
            "void prepare(void); void send_fast(void); void send_slow(void);\n\
             void send(void) {\n\
             prepare();\n\
             #if BACKEND == 1\n\
             send_fast();\n\
             #elif BACKEND == 2\n\
             send_slow();\n\
             #endif\n\
             prepare();\n\
             }\n",
        ),
    ]);
    let db = sequenced_db(
        dir.path(),
        PreprocessOptions::new()
            .with_explore(true)
            .with_explore_budget(4),
    );
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let variants = count(
        &conn,
        "SELECT COUNT(*) FROM cfg_functions cf JOIN functions f ON f.id = cf.fn_id \
         WHERE f.name = 'send'",
    );
    assert!(variants >= 2, "{variants}");
    let prepare = trace_db::resolve_event(&conn, "main.c", 3, None, Some("prepare"))
        .expect("one call across variants");
    assert_eq!(
        trace_db::resolve_event(&conn, "main.c", 3, Some(1), None)
            .expect("narrowed by column")
            .event,
        prepare.event
    );
    // From the call: one frame per variant, each holding both arms' call.
    let graph = trace_db::sequence_graph(
        &conn,
        &trace_db::SequenceScope::From(prepare.clone()),
        &Default::default(),
    )
    .unwrap();
    assert_eq!(graph.frames.len() as i64, variants);
    for wanted in ["send_fast", "send_slow"] {
        assert!(graph.events.iter().any(|e| e.callee == wanted), "{wanted}");
    }
    // Between: every variant runs `prepare` before the closing `prepare`.
    let last = trace_db::resolve_event(&conn, "main.c", 9, None, Some("prepare")).unwrap();
    assert_eq!(relation(&conn, &prepare, &last), SequencedBefore);
    assert_eq!(relation(&conn, &last, &prepare), MayFollow);
    // An arm only one variant compiles still relates.
    let fast = trace_db::resolve_event(&conn, "main.c", 5, None, Some("send_fast")).unwrap();
    assert_eq!(relation(&conn, &prepare, &fast), SequencedBefore);
}

/// A thread start and an IPC proxy/stub pair (as in `ipc_basic`).
const ASYNC_IPC: &str = "#include <thread>\n\
void before(); void after(); void step(); void handle();\n\
void worker() { step(); }\n\
void start() {\n\
    before();\n\
    std::thread t(worker);\n\
    t.join();\n\
    after();\n\
}\n\
class IRemoteObject {\n\
public:\n\
    int SendRequest(int code, void *data, void *reply, void *option);\n\
};\n\
IRemoteObject *Remote();\n\
class IFooStub {\n\
public:\n\
    int HandleGetInfo(int key) { handle(); return key; }\n\
};\n\
class IFooProxy {\n\
public:\n\
    int GetInfo(int key);\n\
};\n\
int IFooProxy::GetInfo(int key) {\n\
    IRemoteObject *remote = Remote();\n\
    remote->SendRequest(1, 0, 0, 0);\n\
    return key;\n\
}\n";

#[test]
fn thread_starts_and_ipc_are_boundaries() {
    use trace_db::Relation::*;
    let dir = scratch(&[("async_ipc.cpp", ASYNC_IPC)]);
    let db = sequenced_db(dir.path(), default_opts(dir.path()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let options = trace_db::SequenceOptions {
        depth: 3,
        ..Default::default()
    };
    let graph = function_sequence(&conn, "start", None, &options);
    let thread = graph
        .events
        .iter()
        .find(|e| e.context_start.is_some())
        .expect("the thread start");
    assert_eq!(thread.context_start.as_deref(), Some("thread"));
    assert!(
        !graph.frames.iter().any(|f| f.name == "worker"),
        "the thread body runs elsewhere"
    );
    assert!(graph.boundaries.iter().any(|b| b.kind == "async_start"
        && b.event == Some(thread.id)
        && b.name.as_deref() == Some("worker")));
    // Nothing orders the thread's calls against the starter's.
    let at = |line, callee| event(&conn, "async_ipc.cpp", line, callee);
    let step = at(3, "step");
    for other in [at(5, "before"), at(7, "join"), at(8, "after")] {
        assert_eq!(relation(&conn, &other, &step), Unknown);
        assert_eq!(relation(&conn, &step, &other), Unknown);
    }
    // With `inline_contexts`, the body is shown, but still never ordered.
    let inlined = trace_db::SequenceOptions {
        inline_contexts: true,
        ..options.clone()
    };
    let graph = function_sequence(&conn, "start", None, &inlined);
    assert!(graph.frames.iter().any(|f| f.name == "worker"));

    let graph = function_sequence(&conn, "IFooProxy::GetInfo", None, &options);
    let bridge: Vec<_> = graph
        .boundaries
        .iter()
        .filter(|b| b.kind == "ipc_bridge")
        .map(|b| b.name.as_deref())
        .collect();
    assert_eq!(bridge, [Some("IFooStub::HandleGetInfo")]);
    assert_eq!(graph.frames.len(), 1, "the stub runs in another process");
    let send = at(25, "IRemoteObject::SendRequest");
    assert_eq!(relation(&conn, &send, &at(17, "handle")), Unknown);
}

#[test]
fn sequencing_leaves_the_call_graph_unchanged() {
    let (_, off, _) = exported(false, false);
    let (_, on, _) = exported(true, false);
    assert_eq!(
        format!("{:?}", off.call_edges),
        format!("{:?}", on.call_edges)
    );
    assert_eq!(
        format!("{:?}", off.arg_flow_edges),
        format!("{:?}", on.arg_flow_edges)
    );
    assert_eq!(
        format!("{:?}", off.execution_contexts),
        format!("{:?}", on.execution_contexts)
    );
    assert_eq!(off.points_to.len(), on.points_to.len());
}

/// The sites `--sequencing` retains for its events (a direct call nothing
/// resolves, as `static_cast` is) are not unresolved call-graph leaves:
/// `inspect callgraph` prints the same graph either way.
#[test]
fn sequencing_leaves_inspect_callgraph_unchanged() {
    let dir = scratch(&[(
        "main.cpp",
        "void known();\nvoid (*hook)();\n\nint run(long v)\n{\n    known();\n    hook();\n    return static_cast<int>(v);\n}\n",
    )]);
    let callgraph = |args: &[&str]| {
        let db = crate::common::cli_analyze(dir.path(), args);
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("inspect")
            .arg(db.path())
            .args(["callgraph", "--file", "main.cpp", "--line", "6"])
            .args(["--direction", "down", "--format", "json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let plain = callgraph(&[]);
    // The unresolved indirect call is a leaf; the direct one without a
    // target is not exported without `--sequencing`.
    assert!(
        plain.contains("hook") && !plain.contains("static_cast"),
        "{plain}"
    );
    assert_eq!(callgraph(&["--sequencing"]), plain);
}

#[test]
fn sequence_renders_in_every_format() {
    let conn = fixture_db();
    let graph = function_sequence(&conn, "args", None, &Default::default());
    let text = trace_db::render_sequence(&graph, trace_db::RenderFormat::Text).unwrap();
    assert!(text.contains("args"), "{text}");
    assert!(text.contains("(order unspecified)"), "{text}");
    assert!(
        text.contains("?? unknown callee") || text.contains("body omitted"),
        "{text}"
    );
    let json = trace_db::render_sequence(&graph, trace_db::RenderFormat::Json).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["frames"][0]["name"], "args");
    assert_eq!(value["events"].as_array().unwrap().len(), 3);
    let mermaid = trace_db::render_sequence(&graph, trace_db::RenderFormat::Mermaid).unwrap();
    assert!(mermaid.starts_with("flowchart TD"), "{mermaid}");
    assert!(mermaid.contains("subgraph"), "{mermaid}");
    let dot = trace_db::render_sequence(&graph, trace_db::RenderFormat::Graphviz).unwrap();
    assert!(dot.starts_with("digraph"), "{dot}");
    assert!(dot.contains("subgraph cluster_"), "{dot}");
}

/// What a budget cut shows in the diagrams too: a dashed edge from the
/// cut call's block to a note naming the budget.
#[test]
fn diagrams_mark_what_budgets_cut() {
    let dir = scratch(&[(
        "calls.c",
        "void leaf(void);\n\
         void helper(void) { leaf(); }\n\
         void run(void) { helper(); }\n",
    )]);
    let db = sequenced_db(dir.path(), default_opts(dir.path()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let options = trace_db::SequenceOptions {
        max_events: 1,
        ..Default::default()
    };
    let graph = function_sequence(&conn, "run", None, &options);
    let reasons: Vec<_> = graph.truncated.iter().map(|t| t.reason.as_str()).collect();
    assert_eq!(reasons, ["max_events"]);
    let block = graph.events[graph.truncated[0].event.unwrap()].block;
    let mermaid = trace_db::render_sequence(&graph, trace_db::RenderFormat::Mermaid).unwrap();
    assert!(
        mermaid.contains("  t0[/\"truncated: max_events\"/]"),
        "{mermaid}"
    );
    assert!(
        mermaid.contains(&format!("  n{block} -.-> t0")),
        "{mermaid}"
    );
    let dot = trace_db::render_sequence(&graph, trace_db::RenderFormat::Graphviz).unwrap();
    assert!(
        dot.contains("  t0 [shape=note, label=\"truncated: max_events\"];"),
        "{dot}"
    );
    assert!(
        dot.contains(&format!("  n{block} -> t0 [style=dashed];")),
        "{dot}"
    );
}

/// A block of many `g(a(), b());` statements is a chain of order diamonds,
/// with 2^n paths through it. The text view groups and annotates it in time
/// polynomial in the events (it walked every path once: an `EmitDriverBind`
/// body of hdf's hdi-gen, 101 events in one block, never finished).
#[test]
fn text_view_of_a_diamond_chain_finishes() {
    let body: String = (0..40).map(|_| "    g(a(), b());\n").collect();
    let dir = scratch(&[(
        "chain.c",
        &format!(
            "void a(void);\nvoid b(void);\nvoid g(int, int);\nvoid chain(void)\n{{\n{body}}}\n"
        ),
    )]);
    let db = sequenced_db(dir.path(), default_opts(dir.path()));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let graph = function_sequence(&conn, "chain", None, &Default::default());
    assert_eq!(graph.events.len(), 120);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(trace_db::render_sequence(
            &graph,
            trace_db::RenderFormat::Text,
        ));
    });
    let text = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the text view finishes")
        .unwrap();
    assert_eq!(
        text.matches("{ a(), b() }  (order unspecified)").count(),
        40
    );
}

#[test]
fn cli_inspect_sequence_end_to_end() {
    let root = fixture("call_sequence");
    let run = |db: &TempDb, args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("inspect")
            .arg(db.path())
            .arg("sequence")
            .args(args)
            .output()
            .unwrap()
    };
    let db = crate::common::cli_analyze(&root, &["--sequencing"]);
    let out = run(&db, &["--file", "branches.c", "--line", "8"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    for callee in ["prepare()", "send()", "recover()", "finish()"] {
        assert!(text.contains(callee), "{text}");
    }
    assert!(
        text.contains("branch_true") && text.contains("branch_false"),
        "{text}"
    );
    let out = run(
        &db,
        &[
            "--file",
            "branches.c",
            "--line",
            "10",
            "--callee",
            "send",
            "--to-file",
            "branches.c",
            "--to-line",
            "12",
            "--format",
            "json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["relation"]["kind"], "exclusive");

    let out = run(&db, &["--function", "run", "--file", "branches.c"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("send()") && !text.contains("work()"),
        "{text}"
    );

    let plain = crate::common::cli_analyze(&root, &[]);
    let out = run(&plain, &["--function", "run"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--sequencing"));
}

#[test]
fn partial_merge_output_is_accepted_with_a_notice() {
    let root = fixture("call_sequence");
    let db = sequenced_db(&root, default_opts(&root));
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    // What trace-merge records when only some inputs were sequenced.
    conn.execute(
        "UPDATE analysis_run SET options_json = \
         json_set(options_json, '$.call_sequencing', 'partial')",
        [],
    )
    .unwrap();
    let capability = trace_db::require_sequencing(&conn).unwrap();
    assert_eq!(capability, trace_db::SequencingMarker::Partial);
    assert!(capability.notice().unwrap().contains("--sequencing"));
    let graph = function_sequence(&conn, "args", None, &Default::default());
    assert_eq!(graph.capability, "partial");
    let text = trace_db::render_sequence(&graph, trace_db::RenderFormat::Text).unwrap();
    assert!(text.contains("note: call sequencing is partial"), "{text}");
}

#[test]
fn loops_stay_cycles() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "loops.c", line, callee);
    let body = at(13, "body");
    assert_eq!(relation(&conn, &body, &body), MayBoth);
    assert_eq!(relation(&conn, &at(12, "init"), &body), GuaranteedBefore);
    assert_eq!(relation(&conn, &body, &at(20, "after")), MayPrecede);
    let graph = function_sequence(&conn, "for_loop", None, &Default::default());
    assert_eq!(
        graph.edges.iter().filter(|e| e.kind == "back_edge").count(),
        1,
        "the loop is one back edge, never unrolled"
    );
    assert_eq!(
        graph.events.iter().filter(|e| e.callee == "body").count(),
        1
    );
}

/// Two calls of one loop block: each iteration runs `first` before
/// `second`, and the next iteration's `first` after this one's `second`.
#[test]
fn calls_of_one_loop_block_may_run_either_way() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let (first, second) = (
        event(&conn, "loops.c", 44, "first"),
        event(&conn, "loops.c", 45, "second"),
    );
    assert_eq!(first.cfg, second.cfg);
    assert_eq!(relation(&conn, &first, &second), MayBoth);
    assert_eq!(relation(&conn, &second, &first), MayBoth);
}

#[test]
fn handler_may_follow_the_try_body() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    // Reached over an exception edge: a path, never a guarantee.
    assert_eq!(relation(&conn, &at(11, "a"), &at(14, "h")), MayPrecede);
    assert_eq!(relation(&conn, &at(14, "h"), &at(11, "a")), MayFollow);
    // `a` is the first event of the `try` block: whatever throws there,
    // `a` was invoked first.
    assert_eq!(
        relation(&conn, &at(11, "a"), &at(16, "c")),
        GuaranteedBefore
    );
    // `a` may throw before `b` runs, and the handler falls through to `c`.
    assert_eq!(relation(&conn, &at(12, "b"), &at(16, "c")), MayPrecede);
    // `d` is first in its block, but `a` or `k` in an earlier block of the
    // `try` may throw, and the handler reaches `e` without `d`.
    assert_eq!(
        relation(&conn, &at(56, "a"), &at(64, "e")),
        GuaranteedBefore
    );
    assert_eq!(relation(&conn, &at(60, "d"), &at(64, "e")), MayPrecede);
    // Nothing runs after a `throw`.
    assert_eq!(
        relation(&conn, &at(21, "before"), &at(23, "dead")),
        Exclusive
    );
    let graph = function_sequence(&conn, "guarded", None, &Default::default());
    assert!(graph.edges.iter().any(|e| e.kind == "exception"));
}

#[test]
fn inner_catch_all_keeps_its_exceptions() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    // The inner `catch (...)` takes whatever `a` throws, and its empty
    // handler throws nothing: the outer handler never runs after `a`.
    assert_eq!(relation(&conn, &at(71, "a"), &at(75, "c")), Exclusive);
}

#[test]
fn typed_inner_catch_lets_exceptions_reach_the_outer_handler() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    // An exception `catch (Error &)` does not match propagates outward.
    assert_eq!(relation(&conn, &at(83, "a"), &at(88, "c")), MayPrecede);
    // The inner handler may throw too.
    assert_eq!(relation(&conn, &at(85, "h"), &at(88, "c")), MayPrecede);
}

#[test]
fn function_try_block_covers_the_initializer_list() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    // A member initializer may throw into the handler, before the body.
    assert_eq!(relation(&conn, &at(104, "make"), &at(107, "h")), MayPrecede);
    // `make` runs before the body; with the member's constructor it is one
    // unordered initializer group, so it is not known to be the first call
    // of the block that throws: a path, not a guarantee.
    assert_eq!(
        relation(&conn, &at(104, "make"), &at(105, "run")),
        MayPrecede
    );
    assert_eq!(
        relation(&conn, &at(105, "run"), &at(104, "make")),
        MayFollow
    );
    // The handler may run without the body when an initializer throws.
    assert_eq!(relation(&conn, &at(105, "run"), &at(107, "h")), MayPrecede);
}

#[test]
fn gaps_cancel_guarantees() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "unsupported.c", line, callee);
    // A statement expression is not modeled: its call's order is unknown.
    assert_eq!(relation(&conn, &at(13, "g"), &at(14, "k")), Unknown);
    // `a() && b()` as an argument: the branch sits in an unordered operand.
    assert_eq!(relation(&conn, &at(33, "a"), &at(33, "b")), Unordered);
    let graph = function_sequence(&conn, "branchy", None, &Default::default());
    assert_eq!(graph.frames[0].coverage, "partial");
    assert_eq!(graph.frames[0].gaps[0].kind, "unordered_branch");
}

#[test]
fn separate_unordered_gaps_keep_statement_order() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "args_order.c", line, callee);
    // Each full expression has an unordered_branch gap of its own; the
    // first statement still runs before the second.
    let (cond1, cond2) = (at(45, "cond1"), at(46, "cond2"));
    assert_eq!(relation(&conn, &cond1, &cond2), GuaranteedBefore);
    assert_eq!(relation(&conn, &at(45, "first"), &cond2), SequencedBefore);
    assert_eq!(relation(&conn, &at(45, "then1"), &cond2), MayPrecede);
    let graph = function_sequence(&conn, "two_ternaries", None, &Default::default());
    let kinds: Vec<_> = graph.frames[0]
        .gaps
        .iter()
        .map(|g| g.kind.as_str())
        .collect();
    assert_eq!(kinds, ["unordered_branch", "unordered_branch"]);
}

#[test]
fn a_shared_unordered_gap_admits_either_order() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    // Whichever argument the builder walks first, an arm of one argument
    // and the other argument may run in either order.
    for line in [35, 40] {
        let at = |callee| event(&conn, "args_order.c", line, callee);
        let (cond, left, right, other) = (at("cond"), at("left"), at("right"), at("other"));
        for (a, b) in [(&left, &other), (&right, &other), (&cond, &other)] {
            assert_eq!(relation(&conn, a, b), Unordered, "line {line}");
            assert_eq!(relation(&conn, b, a), Unordered, "line {line}");
        }
        // The arms of one conditional stay exclusive.
        assert_eq!(relation(&conn, &left, &right), Exclusive, "line {line}");
        assert_eq!(relation(&conn, &right, &left), Exclusive, "line {line}");
        // Every argument precedes the call it is passed to.
        let call = relation(&conn, &other, &at("sink"));
        assert!(
            matches!(call, SequencedBefore | GuaranteedBefore),
            "line {line}: {call:?}"
        );
    }
}

/// Copies of a call in several variants are one call only when they are one
/// invocation: a macro invoked on different lines under different
/// configurations shares its body's spelling, not its invocation.
#[test]
fn macro_invocations_of_different_variants_stay_distinct() {
    let dir = scratch(&[
        (
            "BUILD.gn",
            "source_set(\"t\") {\n  defines = [ \"MODE=1\", \"MODE=2\" ]\n}\n",
        ),
        ("log.h", "void log_it(void);\n#define LOG() log_it()\n"),
        (
            "main.c",
            "#include \"log.h\"\n\
             void prepare(void);\n\
             void run(void) {\n\
             #if MODE == 1\n\
             LOG();\n\
             #elif MODE == 2\n\
             prepare();\n\
             LOG();\n\
             #endif\n\
             }\n",
        ),
    ]);
    let db = sequenced_db(
        dir.path(),
        PreprocessOptions::new()
            .with_explore(true)
            .with_explore_budget(4),
    );
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let variants = count(
        &conn,
        "SELECT COUNT(*) FROM cfg_functions cf JOIN functions f ON f.id = cf.fn_id \
         WHERE f.name = 'run'",
    );
    assert!(variants >= 2, "{variants}");
    let err = trace_db::resolve_event(&conn, "log.h", 2, None, Some("log_it"))
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("2 call events"), "{err}");
    // Each candidate names where it was invoked.
    for at in ["invoked at main.c:5", "invoked at main.c:8"] {
        assert!(err.contains(at), "{at}: {err}");
    }
    assert!(
        err.contains("name the invocation's position"),
        "suggests an invocation position: {err}"
    );
    // Each invocation position names its own call.
    let five = trace_db::resolve_event(&conn, "main.c", 5, None, Some("log_it")).unwrap();
    let eight = trace_db::resolve_event(&conn, "main.c", 8, None, Some("log_it")).unwrap();
    assert_ne!(five.event, eight.event);
    assert_ne!(five.cfg, eight.cfg);
}

/// Control that leaves through an unmodeled transfer (computed or
/// unresolved `goto`, `setjmp`/`longjmp`, an opaque construct) may land
/// anywhere: the missing edges are never evidence that two calls exclude
/// each other or that one is always first.
#[test]
fn unmodeled_transfers_are_not_evidence_of_impossibility() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "unsupported.c", line, callee);
    // `goto *target` may land on `resume`: `b` may follow `a`.
    let (a, b) = (at(46, "a"), at(49, "b"));
    assert_eq!(relation(&conn, &a, &b), MayPrecede);
    assert_eq!(relation(&conn, &b, &a), MayFollow);
    // `goto *target` may land on `skip`, bypassing `a`: a path, never a
    // guarantee.
    assert_eq!(relation(&conn, &at(56, "a"), &at(58, "b")), MayPrecede);
    // `b` runs, `longjmp` returns to `setjmp`, then `a` runs.
    let (a, b) = (at(65, "a"), at(67, "b"));
    assert_eq!(relation(&conn, &a, &b), MayBoth);
    assert_eq!(relation(&conn, &b, &a), MayBoth);
    // `c` may `longjmp` back to the `setjmp` the arms follow.
    let (a, b) = (at(75, "a"), at(77, "b"));
    assert_eq!(relation(&conn, &a, &b), MayBoth);
    assert_eq!(relation(&conn, &b, &a), MayBoth);
    let scope = trace_db::SequenceScope::Between(a, b);
    let graph = trace_db::sequence_graph(&conn, &scope, &Default::default()).unwrap();
    let notes = graph.relation.unwrap().notes;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("unmodeled control transfer")),
        "{notes:?}"
    );
}

/// A control-hiding construct without calls still hides its transfer: the
/// `goto` inside the statement expression joins the arms.
#[test]
fn a_call_free_opaque_construct_still_escapes() {
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "unsupported.c", line, callee);
    let (a, b) = (at(84, "a"), at(88, "b"));
    assert_eq!(relation(&conn, &a, &b), trace_db::Relation::MayPrecede);
    assert_eq!(relation(&conn, &b, &a), trace_db::Relation::MayFollow);
    let scope = trace_db::SequenceScope::Between(a, b);
    let graph = trace_db::sequence_graph(&conn, &scope, &Default::default()).unwrap();
    let notes = graph.relation.unwrap().notes;
    assert!(
        notes
            .iter()
            .any(|n| n.contains("unmodeled control transfer")),
        "{notes:?}"
    );
}

/// `Both both;` runs each base's constructor and destructor: one event
/// per base, never alternatives of one invocation.
#[test]
fn base_lifecycle_calls_are_separate_events() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "lifecycle.cpp", line, callee);
    let (ctor_a, ctor_b) = (at(78, "BaseA::BaseA"), at(78, "BaseB::BaseB"));
    let (dtor_a, dtor_b) = (at(78, "BaseA::~BaseA"), at(78, "BaseB::~BaseB"));
    let work = at(79, "work");
    assert_ne!(ctor_a.event, ctor_b.event);
    assert_ne!(dtor_a.event, dtor_b.event);
    // Both run; the order of base subobjects is not modeled.
    assert_eq!(relation(&conn, &ctor_a, &ctor_b), Unordered);
    assert_eq!(relation(&conn, &dtor_b, &dtor_a), Unordered);
    for ctor in [&ctor_a, &ctor_b] {
        assert_eq!(relation(&conn, ctor, &work), SequencedBefore);
    }
    for dtor in [&dtor_a, &dtor_b] {
        assert_eq!(relation(&conn, &work, dtor), SequencedBefore);
    }
}

/// `asm goto` may jump to any of its labels: `b` after `out` may run
/// without `a`. A plain `asm` transfers nothing.
#[test]
fn asm_goto_jumps_to_its_labels() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "unsupported.c", line, callee);
    assert_eq!(relation(&conn, &at(95, "a"), &at(97, "b")), MayPrecede);
    assert_eq!(relation(&conn, &at(97, "b"), &at(95, "a")), MayFollow);
    assert_eq!(
        relation(&conn, &at(110, "a"), &at(111, "b")),
        SequencedBefore
    );
}

/// `::_setjmp`, `::longjmp`, `std::setjmp` and `std::longjmp` are the
/// nonlocal jumps their unqualified names are.
#[test]
fn qualified_nonlocal_jumps_are_unmodeled_transfers() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    for (resumed, initial) in [(124, 126), (135, 137)] {
        let (resumed, initial) = (at(resumed, "resumed"), at(initial, "initial"));
        assert_eq!(relation(&conn, &initial, &resumed), MayBoth);
        assert_eq!(relation(&conn, &resumed, &initial), MayBoth);
    }
}

/// A pack expansion in a typed handler (`catch (Fault<Ts...> &)`) does not
/// make it `catch (...)`: what it does not catch reaches the outer handler.
#[test]
fn a_pack_expansion_handler_is_not_a_catch_all() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    assert_eq!(relation(&conn, &at(149, "a"), &at(153, "b")), MayPrecede);
}

/// A `while` condition's object is destroyed at the end of each iteration
/// (body end or `continue`), before the back edge to the next evaluation,
/// and again where the loop ends (false condition or `break`).
#[test]
fn a_loop_condition_object_is_destroyed_each_iteration() {
    let conn = fixture_db();
    for name in ["leased", "lease_continued"] {
        let graph = function_sequence(&conn, name, None, &Default::default());
        let block_of = |callee: &str| -> Vec<usize> {
            graph
                .events
                .iter()
                .filter(|e| e.callee == callee)
                .map(|e| e.block)
                .collect()
        };
        let (head, dtors) = (block_of("lease"), block_of("Lease::~Lease"));
        assert_eq!(head.len(), 1, "{name}");
        assert_eq!(dtors.len(), 2, "{name}: once per iteration, once after");
        let back = |b: usize| {
            graph
                .edges
                .iter()
                .any(|e| e.src == b && e.dst == head[0] && e.kind == "back_edge")
        };
        assert_eq!(dtors.iter().filter(|&&b| back(b)).count(), 1, "{name}");
        // The one after the loop runs before `after`, in its block.
        assert!(dtors.contains(&block_of("after")[0]), "{name}");
    }
}

/// Two statements of `n` unordered calls each store order rows linear in
/// `n`, and still relate every call of the first as `sequenced_before`
/// every call of the second.
#[test]
fn consecutive_unordered_statements_stay_sequenced_in_linear_storage() {
    use trace_db::Relation::*;
    let n = 100;
    let sum = vec!["step()"; n].join(" + ");
    let dir = scratch(&[(
        "sums.c",
        &format!("int step(void);\nvoid sums(void)\n{{\n    {sum};\n    {sum};\n}}\n"),
    )]);
    let db = sequenced_db(dir.path(), PreprocessOptions::new());
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM cfg_events"),
        2 * n as i64
    );
    let order = count(&conn, "SELECT count(*) FROM cfg_event_order");
    assert!(order < 4 * n as i64, "{order} order rows");
    let at = |line: i64, i: usize| {
        trace_db::resolve_event(&conn, "sums.c", line, Some(5 + 9 * i as i64), Some("step"))
            .expect("event")
    };
    let (first, last) = (0, n - 1);
    for (a, b) in [(first, last), (last, first), (first, first)] {
        assert_eq!(relation(&conn, &at(4, a), &at(5, b)), SequencedBefore);
        assert_eq!(relation(&conn, &at(5, b), &at(4, a)), MayFollow);
    }
    assert_eq!(relation(&conn, &at(4, first), &at(4, last)), Unordered);
    assert_eq!(relation(&conn, &at(5, last), &at(5, first)), Unordered);
}

/// The destructor events an object gets on each way out of its scope are
/// copies of one call: a position names them together, and the relation
/// combines theirs. Distinct calls at one position stay ambiguous.
#[test]
fn per_exit_destructor_copies_are_one_position() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "scopes.cpp", line, callee);
    // `while (Lease l = lease()) { work(); }`: one iteration's destructor
    // follows its `work` and precedes the next one's.
    let (dtor, work) = (at(59, "Lease::~Lease"), at(60, "work"));
    assert_eq!(relation(&conn, &dtor, &work), MayBoth);
    assert_eq!(relation(&conn, &work, &dtor), MayBoth);
    assert_eq!(relation(&conn, &dtor, &at(62, "after")), MayPrecede);
    // `Guard g;` in a loop body left by `continue`, `break` and its end.
    let (dtor, work) = (at(33, "Guard::~Guard"), at(38, "work"));
    assert_eq!(relation(&conn, &dtor, &work), MayBoth);
    assert_eq!(relation(&conn, &dtor, &at(40, "after")), MayPrecede);
    // `TWICE(step())` substitutes the argument twice: two calls.
    let err = trace_db::resolve_event(&conn, "macro_calls.c", 10, None, Some("step"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("2 call events"), "{err}");
}

/// The reviewer's query: between a loop-condition object's destructor and
/// the loop body's call, from the command line.
#[test]
fn cli_relates_a_loop_condition_destructor_to_the_body() {
    let root = fixture("call_sequence");
    let db = crate::common::cli_analyze(&root, &["--sequencing"]);
    let kind = |from: (&str, &str), to: (&str, &str)| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("inspect")
            .arg(db.path())
            .arg("sequence")
            .args(["--file", "scopes.cpp", "--line", from.0, "--callee", from.1])
            .args([
                "--to-file",
                "scopes.cpp",
                "--to-line",
                to.0,
                "--to-callee",
                to.1,
            ])
            .args(["--format", "json"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        value["relation"]["kind"].as_str().unwrap().to_owned()
    };
    let (dtor, work) = (("59", "Lease::~Lease"), ("60", "work"));
    assert_eq!(kind(dtor, work), "may_both");
    assert_eq!(kind(work, dtor), "may_both");
}

/// `_Generic` evaluates one association and never its controlling
/// expression.
#[test]
fn generic_selection_branches_to_one_association() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "branches.c", line, callee);
    let (selector, chosen, other) = (at(23, "selector"), at(23, "chosen"), at(23, "other"));
    let picked = at(24, "picked");
    assert_eq!(relation(&conn, &chosen, &other), Exclusive);
    assert_eq!(relation(&conn, &chosen, &picked), MayPrecede);
    assert_eq!(relation(&conn, &other, &picked), MayPrecede);
    for e in [&chosen, &picked] {
        let r = relation(&conn, &selector, e);
        assert!(
            !matches!(r, SequencedBefore | GuaranteedBefore),
            "selector to {}: {r:?}",
            e.callee
        );
    }
}

/// `std::make_shared<Widget>(make_arg())` runs `Widget::Widget` inside the
/// factory: two invocations, not alternative targets of one.
#[test]
fn a_factory_and_its_constructor_are_separate_events() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |callee| event(&conn, "lifecycle.cpp", 90, callee);
    let (factory, ctor, arg) = (at("std::make_shared"), at("Widget::Widget"), at("make_arg"));
    assert_ne!(factory.event, ctor.event);
    assert_eq!(relation(&conn, &arg, &factory), SequencedBefore);
    assert_eq!(relation(&conn, &arg, &ctor), SequencedBefore);
    assert_eq!(relation(&conn, &factory, &ctor), Unordered);
}

/// A `static` or `thread_local` local's initializer runs on the first pass
/// only: it may precede what follows, never guaranteed to.
#[test]
fn a_once_only_local_initializer_is_bypassed() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "lifecycle.cpp", line, callee);
    for (init, after) in [
        (at(102, "Once::Once"), at(103, "after")),
        (at(108, "seed_value"), at(109, "after")),
        (at(114, "Once::Once"), at(115, "after")),
    ] {
        assert_eq!(
            relation(&conn, &init, &after),
            MayPrecede,
            "{}",
            init.callee
        );
    }
}

/// `try { Sentry s; may_throw(); later(); } catch (...) { h(); }`: a call
/// that throws destroys `s` on its way to the handler, so no call of the
/// body is proven to precede every destruction of `s`.
#[test]
fn an_exception_destroys_the_objects_it_leaves() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    let dtor = at(167, "Sentry::~Sentry");
    assert_eq!(relation(&conn, &at(169, "later"), &dtor), MayPrecede);
    assert_eq!(relation(&conn, &at(168, "may_throw"), &dtor), MayPrecede);
    assert_eq!(relation(&conn, &dtor, &at(171, "h")), MayPrecede);
    // Past an inner `try` whose handlers do not catch it.
    let dtor = at(178, "Sentry::~Sentry");
    assert_eq!(relation(&conn, &dtor, &at(185, "h")), MayPrecede);
    assert_eq!(relation(&conn, &at(180, "may_throw"), &dtor), MayPrecede);
}

/// A call in an operand that is never evaluated proves no order; one in an
/// operand that may be evaluated may precede what follows.
#[test]
fn unevaluated_operands_prove_no_order() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |file, line, callee| event(&conn, file, line, callee);
    let then = at("args_order.cpp", 35, "then");
    for (line, callee) in [
        (29, "sized"),
        (30, "typed"),
        (31, "checked"),
        (32, "required"),
    ] {
        let e = at("args_order.cpp", line, callee);
        assert_eq!(relation(&conn, &e, &then), Unknown, "{callee}");
    }
    for (line, callee) in [(33, "identified"), (34, "bound")] {
        let e = at("args_order.cpp", line, callee);
        assert_eq!(relation(&conn, &e, &then), MayPrecede, "{callee}");
    }
    let (sized, after) = (
        at("args_order.c", 55, "sized"),
        at("args_order.c", 56, "after_size"),
    );
    assert_eq!(relation(&conn, &sized, &after), Unknown);
    // `sizeof(int[counted()])` evaluates the bound of the array type.
    let (counted, after) = (
        at("args_order.c", 61, "counted"),
        at("args_order.c", 62, "after_size"),
    );
    assert_eq!(relation(&conn, &counted, &after), MayPrecede);
}
