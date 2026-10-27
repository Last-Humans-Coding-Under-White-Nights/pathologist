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
