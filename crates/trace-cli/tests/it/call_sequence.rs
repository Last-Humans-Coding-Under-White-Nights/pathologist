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
        &trace_db::SequenceScope::Function(function.id),
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
    assert!(capability.complete && capability.notice.is_none());
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
    assert!(!capability.complete);
    assert!(capability.notice.unwrap().contains("--sequencing"));
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

#[test]
fn handler_may_follow_the_try_body() {
    use trace_db::Relation::*;
    let conn = fixture_db();
    let at = |line, callee| event(&conn, "exceptions.cpp", line, callee);
    // Reached over an exception edge: a path, never a guarantee.
    assert_eq!(relation(&conn, &at(11, "a"), &at(14, "h")), MayPrecede);
    assert_eq!(relation(&conn, &at(14, "h"), &at(11, "a")), MayFollow);
    assert_eq!(
        relation(&conn, &at(11, "a"), &at(16, "c")),
        GuaranteedBefore
    );
    // Nothing runs after a `throw`.
    assert_eq!(
        relation(&conn, &at(21, "before"), &at(23, "dead")),
        Exclusive
    );
    let graph = function_sequence(&conn, "guarded", None, &Default::default());
    assert!(graph.edges.iter().any(|e| e.kind == "exception"));
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
