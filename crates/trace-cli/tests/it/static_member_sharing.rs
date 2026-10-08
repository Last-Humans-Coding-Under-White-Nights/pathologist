//! #143: a C++ static data member is one variable across translation units
//! when no link information is available (`docs/ANALYSIS.md`, "Static data
//! member storage").

use crate::common;

use common::{
    analysis_rows, cli_analyze, default_opts, fixture, has_edge, must_not_have_edge, scratch,
    write_compile_commands, TempDb,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use trace_analysis::{analyze, AnalysisResult, ResolutionKind};
use trace_ir::{Program, VarId};
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

const HEADER: &str =
    "#pragma once\ntypedef void (*Callback)();\nstruct Holder { static Callback cb; };\n";

fn build(root: &Path, jobs: usize) -> (Program, AnalysisResult) {
    let program = build_program_with_jobs(root, &default_opts(root), jobs).unwrap();
    let (_, result) = analyze(&program);
    (program, result)
}

/// The variables whose canonical spelling is `name`.
fn members(program: &Program, name: &str) -> Vec<VarId> {
    program
        .symbols
        .variables
        .iter()
        .filter(|v| v.lookup_name() == name)
        .map(|v| v.id)
        .collect()
}

/// The one variable whose canonical spelling is `name`.
fn only_member(program: &Program, name: &str, case: &str) -> VarId {
    let ids = members(program, name);
    assert_eq!(ids.len(), 1, "{case}: {name} {ids:?}");
    ids[0]
}

/// The callers of every call site whose callee variable is `member`. A site
/// keeps its spelled callee (`"h->cb"`, `"Holder::cb"`), so selecting by
/// `callee_var` is the only spelling-independent way to find them.
fn callers_through(program: &Program, member: VarId) -> BTreeSet<String> {
    program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.callee_var == Some(member))
        .map(|s| program.symbols.function(s.caller).name.clone())
        .collect()
}

fn assert_indirect(program: &Program, result: &AnalysisResult, caller: &str, callee: &str) {
    assert!(
        has_edge(program, result, caller, callee, ResolutionKind::Indirect),
        "no indirect edge {caller} -> {callee}"
    );
}

/// Issue #143's exact example.
#[test]
fn static_member_callback_crosses_translation_units() {
    let root = fixture("cpp_static_member_cross_tu");
    let (program, result) = build(&root, 1);
    assert_indirect(&program, &result, "run", "handler");
    let id = only_member(&program, "Holder::cb", "fixture");
    let m = program.symbols.variable(id);
    assert!(m.is_static_member && m.target.is_none() && !m.is_defined);
    assert_eq!(
        callers_through(&program, id),
        BTreeSet::from(["run".to_string()])
    );
}

const READERS: &str = "#include \"holder.h\"\n\
    void run_qualified() { Holder::cb(); }\n\
    void run_pointer(Holder *h) { h->cb(); }\n\
    void run_object(Holder h) { h.cb(); }\n";
const READER_NAMES: [&str; 3] = ["run_qualified", "run_pointer", "run_object"];

/// Writer variants: runtime store only, out-of-class initialized definition,
/// out-of-class uninitialized definition.
const WRITERS: [(&str, &str); 3] = [
    (
        "store",
        "#include \"holder.h\"\nvoid handler() {}\nvoid setup(Holder *h) { h->cb = handler; }\n",
    ),
    (
        "init_def",
        "#include \"holder.h\"\nvoid handler() {}\nCallback Holder::cb = handler;\n",
    ),
    (
        "plain_def",
        "#include \"holder.h\"\nvoid handler() {}\nCallback Holder::cb;\nvoid setup() { Holder::cb = handler; }\n",
    ),
];

/// One matrix cell: every reader spelling reaches `handler` through the one
/// member, and a definition's file wins.
fn check_sharing(root: &Path, jobs: usize, writer_file: &str, defined: bool, case: &str) {
    let (program, result) = build(root, jobs);
    let id = only_member(&program, "Holder::cb", case);
    for reader in READER_NAMES {
        assert_indirect(&program, &result, reader, "handler");
    }
    let expected: BTreeSet<_> = READER_NAMES.iter().map(|r| r.to_string()).collect();
    assert_eq!(callers_through(&program, id), expected, "{case}");
    if defined {
        let m = program.symbols.variable(id);
        assert!(m.is_defined, "{case}");
        let file = &program.symbols.files[m.span.file.0 as usize].path;
        assert_eq!(
            file.file_name().unwrap(),
            writer_file,
            "{case}: definition's file wins"
        );
    }
}

/// Three readers exercise two distinct lowering routes (`static_member_access`
/// for `->`/`.`; `resolve_callee`'s qualified arm for `Holder::cb()`), so each
/// is asserted individually, across writer shape, source order, compilation
/// database presence and job count.
#[test]
fn static_member_sharing_order_and_configuration_matrix() {
    for (label, writer) in WRITERS {
        for (writer_file, reader_file) in [("a.cpp", "z.cpp"), ("z.cpp", "a.cpp")] {
            for with_db in [false, true] {
                let dir = scratch(&[
                    ("holder.h", HEADER),
                    (writer_file, writer),
                    (reader_file, READERS),
                ]);
                if with_db {
                    write_compile_commands(dir.path(), &[writer_file, reader_file]);
                }
                for jobs in [1, 8] {
                    let case = format!("{label} writer={writer_file} db={with_db} jobs={jobs}");
                    check_sharing(dir.path(), jobs, writer_file, label != "store", &case);
                }
            }
        }
    }
}

/// A third TU and an inline header function read the shared member through
/// the expansion cache.
#[test]
fn cached_header_readers_reference_the_shared_member() {
    let header =
        "#pragma once\ntypedef void (*Callback)();\nstruct Holder { static Callback cb; };\n\
                  inline void run_inline() { Holder::cb(); }\n";
    let dir = scratch(&[
        ("holder.h", header),
        (
            "a.cpp",
            "#include \"holder.h\"\nvoid handler() {}\nvoid setup() { Holder::cb = handler; }\n",
        ),
        (
            "b.cpp",
            "#include \"holder.h\"\nvoid run_b() { run_inline(); }\n",
        ),
        (
            "c.cpp",
            "#include \"holder.h\"\nvoid run_c(Holder *h) { h->cb(); }\n",
        ),
    ]);
    for jobs in [1, 8] {
        let (program, result) = build(dir.path(), jobs);
        let id = only_member(&program, "Holder::cb", &format!("jobs={jobs}"));
        assert_indirect(&program, &result, "run_inline", "handler");
        assert_indirect(&program, &result, "run_c", "handler");
        let callers = callers_through(&program, id);
        assert!(
            callers.contains("run_inline") && callers.contains("run_c"),
            "jobs={jobs}: {callers:?}"
        );
    }
}

/// Two configurations of one source initialize an inline member differently;
/// the union survives (may-analysis).
#[test]
fn inline_static_member_preserves_all_configuration_values() {
    let dir = scratch(&[
        ("BUILD.gn", "config(\"x\") { defines = [ \"ALT\" ] }\n"),
        (
            "main.cpp",
            "typedef void (*Callback)();\nvoid base() {}\nvoid alt() {}\n\
             #ifdef ALT\n#define INIT alt\n#else\n#define INIT base\n#endif\n\
             struct Holder { inline static Callback cb = INIT; };\nvoid run() { Holder::cb(); }\n",
        ),
    ]);
    let program =
        build_program_with_jobs(dir.path(), &PreprocessOptions::new().with_explore(true), 1)
            .unwrap();
    assert!(program.variants_merged > 0);
    let (_, result) = analyze(&program);
    only_member(&program, "Holder::cb", "explored");
    assert_indirect(&program, &result, "run", "base");
    assert_indirect(&program, &result, "run", "alt");
}

/// Address-of and return-value flow through the member share storage.
#[test]
fn static_member_address_and_return_flow_share_storage() {
    let dir = scratch(&[
        ("holder.h", HEADER),
        (
            "a.cpp",
            "#include \"holder.h\"\nvoid handler() {}\nvoid set(Callback *slot) { *slot = handler; }\nvoid setup() { set(&Holder::cb); }\n",
        ),
        // `get()()` — invoking a call's result directly — is not lowered as a
        // call through the returned value (pre-existing, unrelated to #143);
        // bind it first.
        (
            "b.cpp",
            "#include \"holder.h\"\nCallback get() { return Holder::cb; }\nvoid run() { Callback f = get(); f(); }\n",
        ),
    ]);
    let (program, result) = build(dir.path(), 1);
    only_member(&program, "Holder::cb", "address and return");
    assert_indirect(&program, &result, "run", "handler");
}

#[test]
fn anonymous_static_members_remain_tu_local() {
    let header = "#pragma once\ntypedef void (*Callback)();\nnamespace { struct Hidden { static Callback cb; }; }\n";
    let dir = scratch(&[
        ("holder.h", header),
        (
            "a.cpp",
            "#include \"holder.h\"\nvoid writer_only() {}\nvoid setup() { Hidden::cb = writer_only; }\n",
        ),
        (
            "b.cpp",
            "#include \"holder.h\"\nvoid run() { Hidden::cb(); }\n",
        ),
    ]);
    let (program, result) = build(dir.path(), 1);
    assert!(
        members(&program, "Hidden::cb").len() > 1,
        "internal member is per unit"
    );
    assert!(must_not_have_edge(&program, &result, "run", "writer_only"));
}

#[test]
fn qualified_static_member_owners_do_not_collide() {
    let header = "#pragma once\ntypedef void (*Callback)();\n\
                  namespace a { struct Holder { static Callback cb; }; }\n\
                  namespace b { struct Holder { static Callback cb; }; }\n";
    let dir = scratch(&[
        ("holder.h", header),
        (
            "w.cpp",
            "#include \"holder.h\"\nvoid handler_a() {}\nvoid handler_b() {}\n\
             void setup() { a::Holder::cb = handler_a; b::Holder::cb = handler_b; }\n",
        ),
        (
            "r.cpp",
            "#include \"holder.h\"\nvoid run_a() { a::Holder::cb(); }\nvoid run_b() { b::Holder::cb(); }\n",
        ),
    ]);
    let (program, result) = build(dir.path(), 1);
    only_member(&program, "a::Holder::cb", "two owners");
    only_member(&program, "b::Holder::cb", "two owners");
    assert_indirect(&program, &result, "run_a", "handler_a");
    assert_indirect(&program, &result, "run_b", "handler_b");
    assert!(must_not_have_edge(&program, &result, "run_a", "handler_b"));
    assert!(must_not_have_edge(&program, &result, "run_b", "handler_a"));
}

/// Decision 1: ordinary and namespace `extern` globals are still per unit.
/// Whether their edges resolve is outside this issue's contract and is not
/// asserted.
#[test]
fn ordinary_and_namespace_globals_are_not_shared() {
    let header = "#pragma once\ntypedef void (*Callback)();\nextern Callback plain_cb;\n\
                  namespace ns { extern Callback ns_cb; }\nstruct Holder { static Callback cb; };\n";
    let dir = scratch(&[
        ("holder.h", header),
        (
            "w.cpp",
            "#include \"holder.h\"\nvoid handler() {}\n\
             void setup() { plain_cb = handler; ns::ns_cb = handler; Holder::cb = handler; }\n",
        ),
        (
            "r.cpp",
            "#include \"holder.h\"\nvoid run_plain() { plain_cb(); }\nvoid run_ns() { ns::ns_cb(); }\nvoid run_member() { Holder::cb(); }\n",
        ),
    ]);
    let (program, result) = build(dir.path(), 1);
    assert!(members(&program, "plain_cb").len() > 1);
    assert!(members(&program, "ns::ns_cb").len() > 1);
    only_member(&program, "Holder::cb", "beside globals");
    assert_indirect(&program, &result, "run_member", "handler");
}

#[test]
fn dependency_static_declaration_shares_project_storage() {
    let dir = scratch(&[
        (
            "dep/holder.h",
            "#pragma once\ntypedef void (*Callback)();\nstruct Holder { static Callback cb; };\n\
             inline void dep_body() { Holder::cb(); }\n",
        ),
        (
            "src/w.cpp",
            "#include \"holder.h\"\nvoid handler() {}\nCallback Holder::cb = handler;\n",
        ),
        (
            "src/r.cpp",
            "#include \"holder.h\"\nvoid run() { Holder::cb(); }\n",
        ),
    ]);
    let src = dir.path().join("src");
    let opts = default_opts(&src)
        .with_dep(dir.path().join("dep"))
        .with_include(dir.path().join("dep"));
    let program = build_program_with_jobs(&src, &opts, 1).unwrap();
    let (_, result) = analyze(&program);
    let id = only_member(&program, "Holder::cb", "dependency header");
    let m = program.symbols.variable(id);
    assert!(
        m.is_defined && !program.is_dep_file(m.span.file),
        "the project definition's span wins"
    );
    assert_indirect(&program, &result, "run", "handler");
    assert!(
        program
            .symbols
            .call_sites
            .iter()
            .all(|s| program.symbols.function(s.caller).name != "dep_body"),
        "a dependency body contributes no call sites"
    );
}

/// `trace analyze <root> --full-export`, opened.
fn analyze_full(root: &Path) -> (TempDb, rusqlite::Connection) {
    let db = cli_analyze(root, &["--full-export"]);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    (db, conn)
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

const EDGES_BY_NAME: &str = "FROM call_edges e JOIN functions a ON a.id=e.caller_fn_id \
                             JOIN functions b ON b.id=e.callee_fn_id";

/// Call edges from `caller` to `callee`, at any resolution.
fn edge_count(conn: &rusqlite::Connection, caller: &str, callee: &str) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) {EDGES_BY_NAME} WHERE a.name=?1 AND b.name=?2"),
        rusqlite::params![caller, callee],
        |r| r.get(0),
    )
    .unwrap()
}

/// Callee names reached from `caller` inside link target `target` (join
/// through `link_targets`, as `weak_targets.rs` does).
fn callees_in_target(conn: &rusqlite::Connection, caller: &str, target: &str) -> BTreeSet<String> {
    conn.prepare(&format!(
        "SELECT b.name {EDGES_BY_NAME} JOIN link_targets t ON t.id=a.target_id \
         WHERE a.name=?1 AND t.name=?2"
    ))
    .unwrap()
    .query_map(rusqlite::params![caller, target], |r| r.get::<_, String>(0))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// Decision 3 / 6: with link information, every image keeps its own member
/// (strong selection included) and unassigned units are not unified.
#[test]
fn linked_static_members_stay_in_their_images() {
    let dir = scratch(&[
        ("holder.h", HEADER),
        (
            "reader.cpp",
            "#include \"holder.h\"\nvoid run() { Holder::cb(); }\n",
        ),
        (
            "a.cpp",
            "#include \"holder.h\"\nvoid handler_a() {}\nCallback Holder::cb = handler_a;\n",
        ),
        (
            "b.cpp",
            "#include \"holder.h\"\nvoid handler_b() {}\n__attribute__((weak)) Callback Holder::cb = handler_b;\n",
        ),
        (
            "c.cpp",
            "#include \"holder.h\"\nvoid handler_c() {}\nCallback Holder::cb = handler_c;\n",
        ),
        (
            "loose1.cpp",
            "#include \"holder.h\"\nvoid loose1() { Holder::cb(); }\n",
        ),
        (
            "loose2.cpp",
            "#include \"holder.h\"\nvoid loose2() { Holder::cb(); }\n",
        ),
    ]);
    let root = dir.path();
    let objects = ["reader", "a", "b", "c", "loose1", "loose2"];
    let commands: Vec<_> = objects
        .iter()
        .map(|n| {
            serde_json::json!({
                "directory": root, "file": format!("{n}.cpp"), "output": format!("{n}.o"),
                "arguments": ["c++", "-c", format!("{n}.cpp"), "-o", format!("{n}.o")],
            })
        })
        .collect();
    fs::write(
        root.join("compile_commands.json"),
        serde_json::json!(commands).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("link_commands.json"),
        serde_json::json!([
            {"directory": root, "output": "image_a", "arguments": ["c++", "reader.o", "a.o", "-o", "image_a"]},
            {"directory": root, "output": "image_b", "arguments": ["c++", "reader.o", "b.o", "c.o", "-o", "image_b"]},
        ])
        .to_string(),
    )
    .unwrap();
    let (_db, conn) = analyze_full(root);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(DISTINCT target_id) FROM variables WHERE name='cb' AND target_id IS NOT NULL"
        ),
        2
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM variables WHERE name='cb' AND target_id IS NOT NULL"
        ),
        2,
        "one member per image"
    );
    assert_eq!(
        callees_in_target(&conn, "run", "image_a"),
        BTreeSet::from(["handler_a".to_string()])
    );
    assert_eq!(
        callees_in_target(&conn, "run", "image_b"),
        BTreeSet::from(["handler_c".to_string()]),
        "strong displaces weak inside image_b"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM variables WHERE name='cb' AND target_id IS NULL"
        ),
        2,
        "unassigned units keep their own members"
    );
    for loose in ["loose1", "loose2"] {
        for handler in ["handler_a", "handler_b", "handler_c"] {
            assert_eq!(edge_count(&conn, loose, handler), 0, "{loose} -> {handler}");
        }
    }
}

/// Inferred targets that stay unscoped (`links.unscoped_inference`) never
/// scope a symbol; they must not suppress unscoped sharing.
#[test]
fn informational_targets_keep_unscoped_member_sharing() {
    let dir = scratch(&[
        ("holder.h", HEADER),
        (
            "register.cpp",
            "#include \"holder.h\"\nvoid handler() {}\nvoid setup(Holder *h) { h->cb = handler; }\n",
        ),
        (
            "run.cpp",
            "#include \"holder.h\"\nvoid run(Holder *h) { h->cb(); }\n",
        ),
        (
            "BUILD.gn",
            "source_set(\"a\") { sources = [\"register.cpp\"] } source_set(\"unknown\") { sources = generated }\n",
        ),
    ]);
    let (_db, conn) = analyze_full(dir.path());
    assert!(
        count(&conn, "SELECT COUNT(*) FROM link_targets") > 0,
        "targets were recorded as informational metadata"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM variables WHERE name='cb'"),
        1
    );
    assert_eq!(edge_count(&conn, "run", "handler"), 1);
}

/// Minimal and full exports carry the edge and one member row, and jobs 1 and
/// 8 export identical analysis rows (AGENTS.md invariant 10).
#[test]
fn static_member_sharing_exports_and_is_deterministic() {
    let root = fixture("cpp_static_member_cross_tu");
    for full in [false, true] {
        let snapshots: Vec<_> = ["1", "8"]
            .into_iter()
            .map(|jobs| {
                let mut args = vec!["--jobs", jobs];
                if full {
                    args.push("--full-export");
                }
                let db = cli_analyze(&root, &args);
                let conn = rusqlite::Connection::open(db.path()).unwrap();
                assert_eq!(
                    edge_count(&conn, "run", "handler"),
                    1,
                    "full={full} jobs={jobs}"
                );
                if full {
                    assert_eq!(
                        count(&conn, "SELECT COUNT(*) FROM variables WHERE name='cb'"),
                        1,
                        "one exported member row"
                    );
                }
                analysis_rows(&db)
            })
            .collect();
        assert_eq!(
            snapshots[0], snapshots[1],
            "full={full}: jobs 1 and 8 differ"
        );
    }
}
