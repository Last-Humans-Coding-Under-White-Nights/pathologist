//! #204: where a value moves. `flow_origins` records the original-file
//! position of the statement behind each flow edge, and the enclosing
//! function follows from that position and the function's definition range
//! (`docs/SQLITE_SCHEMA.md`, "Source sites of a value move"). Fixture:
//! `tests/fixtures/flow_origin_sites/`.

use crate::common::*;
use trace_db::open_db;
use trace_parse::build_program;

/// Where one exported operation says it happens: enclosing function name,
/// file name (last path component), line and column.
type Site = (Option<String>, String, i64, i64);

/// The documented query (`docs/SQLITE_SCHEMA.md`, "Source sites of a value
/// move"), narrowed to the `kind` edges from variable `?2` (its value or its
/// storage location) into variable `?3`, when given. The enclosing function is
/// the innermost definition whose line range in the operation's file holds
/// the line: when several hold it, the one every other encloses, if the line
/// is strictly inside it (not its first or last); a line held by a single
/// definition is that definition's, its first and last lines included.
/// Otherwise, none or no single innermost one, it is NULL.
const ORIGIN_SITES: &str = "\
    SELECT (SELECT CASE WHEN COUNT(DISTINCT f.id) = 1 THEN MIN(f.name) END \
            FROM functions f \
            WHERE f.file_id = o.file_id AND f.is_defined = 1 \
              AND o.line BETWEEN f.line_start AND f.line_end \
              AND NOT EXISTS ( \
                SELECT 1 FROM functions g \
                WHERE g.file_id = o.file_id AND g.is_defined = 1 AND g.id <> f.id \
                  AND o.line BETWEEN g.line_start AND g.line_end \
                  AND NOT (g.line_start <= f.line_start AND f.line_end <= g.line_end \
                           AND o.line > f.line_start AND o.line < f.line_end))) \
           AS function, \
           p.path, o.line, o.col \
    FROM flow_origins o \
    JOIN files p ON p.id = o.file_id \
    JOIN flow_nodes s ON s.id = o.src_node \
    JOIN flow_nodes d ON d.id = o.dst_node \
    LEFT JOIN variables sv ON sv.id = s.var_id \
    LEFT JOIN functions sf ON sf.id = s.fn_id \
    LEFT JOIN variables dv ON dv.id = d.var_id \
    WHERE o.kind = ?1 AND (sv.name = ?2 OR (s.var_id IS NULL AND sf.name = ?2)) \
      AND (?3 IS NULL OR dv.name = ?3)";

fn export_fixture() -> TempDb {
    let root = fixture("flow_origin_sites");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (pag, analysis) = trace_analysis::analyze(&program);
    export_program(&program, &pag, &analysis)
}

/// Sites of the `kind` operations moving `src` into `dst` (any destination
/// when `None`), sorted so a test can compare them whole. `src` names a
/// variable, or a function for an address taken of it.
fn sites(db: &TempDb, kind: &str, src: &str, dst: Option<&str>) -> Vec<Site> {
    let conn = open_db(db).unwrap();
    let mut stmt = conn.prepare(ORIGIN_SITES).unwrap();
    let mut rows: Vec<Site> = stmt
        .query_map(rusqlite::params![kind, src, dst], |r| {
            let path: String = r.get(1)?;
            Ok((
                r.get(0)?,
                std::path::Path::new(&path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                r.get(2)?,
                r.get(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.sort();
    rows
}

fn at(func: Option<&str>, file: &str, line: i64) -> (Option<String>, String, i64) {
    (func.map(Into::into), file.into(), line)
}

fn lines(sites: Vec<Site>) -> Vec<(Option<String>, String, i64)> {
    sites.into_iter().map(|(f, p, l, _)| (f, p, l)).collect()
}

#[test]
fn origins_record_the_line_and_function_of_each_constraint_kind() {
    let db = export_fixture();
    // `int *a = &g;`
    assert_eq!(
        lines(sites(&db, "addr_of", "g", Some("a"))),
        vec![at(Some("moves"), "main.c", 7)]
    );
    // `int *b = a;`
    assert_eq!(
        lines(sites(&db, "copy", "a", Some("b"))),
        vec![at(Some("moves"), "main.c", 8)]
    );
    // `int *c = *pp;`
    assert_eq!(
        lines(sites(&db, "load", "pp", Some("c"))),
        vec![at(Some("moves"), "main.c", 9)]
    );
    // `*pp = b;`: the stored value flows into the pointer it is stored through.
    assert_eq!(
        lines(sites(&db, "store", "b", Some("pp"))),
        vec![at(Some("moves"), "main.c", 10)]
    );
    // `int *d = n->val;`
    assert_eq!(
        lines(sites(&db, "gep", "n", None)),
        vec![at(Some("moves"), "main.c", 11)]
    );
}

/// A store spelled in a macro replacement list is attributed to the
/// invocation, as other entities inside an expansion are (AGENTS.md
/// invariant 1): `STORE(pp, c);` at 12:5 in `moves`, not the `#define` in
/// `sites.h`.
#[test]
fn origin_inside_a_macro_expansion_is_at_the_invocation() {
    let db = export_fixture();
    assert_eq!(
        sites(&db, "store", "c", Some("pp")),
        vec![(Some("moves".into()), "main.c".into(), 12, 5)]
    );
}

/// A body written in a header keeps the header's file and its own function
/// once merged into the program. Both `main.c` and `other.c` include it, and
/// the merged body keeps one site for the statement.
#[test]
fn origin_in_a_header_body_names_the_header_and_its_function() {
    let db = export_fixture();
    assert_eq!(
        lines(sites(&db, "gep", "hn", None)),
        vec![at(Some("next_of"), "sites.h", 10)]
    );
}

/// A function-pointer table initializer records each element at its own
/// position (`docs/ANALYSIS.md`, "Source-level dataflow presentation"):
/// inside `table_init` for the local `table`, at file scope (no function)
/// for `ftable`.
#[test]
fn function_table_elements_are_at_their_own_positions() {
    let db = export_fixture();
    assert_eq!(
        sites(&db, "addr_of", "tab1", Some("table")),
        vec![(Some("table_init".into()), "main.c".into(), 23, 24)]
    );
    assert_eq!(
        sites(&db, "addr_of", "tab2", Some("ftable")),
        vec![(None, "main.c".into(), 27, 27)]
    );
}

/// With `--explore`, a statement only a variant compiles keeps its own site,
/// even where the base configuration writes the same fact on another line.
#[test]
fn variant_only_statement_keeps_its_own_site() {
    let dir = scratch(&[
        (
            "main.c",
            "void f(int **pp, int *b, int *c)\n{\n#ifdef FEATURE_A\n    *pp = b;\n#else\n    *pp = c;\n    *pp = b;\n#endif\n}\n",
        ),
        ("BUILD.gn", "defines = [ \"FEATURE_A\" ]\n"),
    ]);
    let opts = default_opts(dir.path())
        .with_explore(true)
        .with_explore_budget(4);
    let program = build_program(dir.path(), &opts).expect("build");
    assert_eq!(program.variants_merged, 1);
    let (pag, analysis) = trace_analysis::analyze(&program);
    let db = export_program(&program, &pag, &analysis);
    assert_eq!(
        lines(sites(&db, "store", "b", Some("pp"))),
        vec![at(Some("f"), "main.c", 4), at(Some("f"), "main.c", 7)]
    );
}

/// Edges with no statement of their own have no origin: implicit variable →
/// location `points_to` edges, `call_arg` edges, and the parameter copies the
/// solver wires at a resolved call (`n` → `hn` for `next_of(n)`). Every
/// address, load, store and field edge lowering wrote has one.
#[test]
fn only_lowered_operations_have_origins() {
    let db = export_fixture();
    let conn = open_db(&db).unwrap();
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert!(count("SELECT COUNT(*) FROM flow_edges WHERE kind = 'points_to'") > 0);
    assert_eq!(
        count("SELECT COUNT(*) FROM flow_origins WHERE kind IN ('points_to', 'call_arg')"),
        0
    );
    assert!(count("SELECT COUNT(*) FROM flow_edges e JOIN flow_nodes s ON s.id = e.src_node JOIN variables sv ON sv.id = s.var_id JOIN flow_nodes d ON d.id = e.dst_node JOIN variables dv ON dv.id = d.var_id WHERE e.kind = 'copy' AND sv.name = 'n' AND dv.name = 'hn'") > 0);
    assert_eq!(sites(&db, "copy", "n", Some("hn")), vec![]);
    assert_eq!(
        count(
            "SELECT COUNT(*) FROM flow_edges e \
             WHERE e.kind IN ('addr_of', 'load', 'store', 'gep') AND NOT EXISTS ( \
               SELECT 1 FROM flow_origins o WHERE o.src_node = e.src_node \
               AND o.dst_node = e.dst_node AND o.kind = e.kind)"
        ),
        0
    );
}

/// Positions are part of the exported data, so they must not depend on
/// scheduling (AGENTS.md invariant 10). `main.c` and `other.c` both include
/// `sites.h`, so the header body's sites are unioned at merge.
#[test]
fn origins_are_deterministic_across_jobs() {
    let root = fixture("flow_origin_sites");
    let run = |jobs: &str| {
        let db = cli_analyze(&root, &["--jobs", jobs]);
        let conn = open_db(&db).unwrap();
        (
            text_rows(&conn, "SELECT rowid, * FROM flow_origins ORDER BY rowid"),
            text_rows(&conn, "SELECT * FROM flow_edges ORDER BY id"),
        )
    };
    let reference = run("1");
    assert!(reference
        .0
        .iter()
        .any(|row| row.iter().any(|c| c == "Integer(12)")));
    for jobs in ["1", "8", "8"] {
        assert_eq!(run(jobs), reference, "jobs={jobs} exported different rows");
    }
}

/// Two statements of a shared header body that move the same value keep a
/// site each, while the copies of each statement that the two including
/// units contribute still merge into one site per statement.
#[test]
fn shared_header_statements_moving_the_same_value_keep_their_own_sites() {
    let dir = scratch(&[
        (
            "shared.h",
            "static inline void repeated(int **p, int *q) {\n    *p = q;\n    *p = q;\n}\n",
        ),
        (
            "a.cpp",
            "#include \"shared.h\"\nvoid use_a(int **p, int *q) { repeated(p, q); }\n",
        ),
        (
            "b.cpp",
            "#include \"shared.h\"\nvoid use_b(int **p, int *q) { repeated(p, q); }\n",
        ),
    ]);
    for jobs in ["1", "8"] {
        let db = cli_analyze(dir.path(), &["--jobs", jobs]);
        assert_eq!(
            lines(sites(&db, "store", "q", Some("p"))),
            vec![
                at(Some("repeated"), "shared.h", 2),
                at(Some("repeated"), "shared.h", 3)
            ],
            "jobs={jobs}"
        );
    }
}

/// The function `trace inspect dataflow` scopes the edge at `at` (line,
/// column) to, from a slice at `file:line:col` in `direction`; `None` when
/// the edge's scope is no function.
fn inspect_scope(
    db: &TempDb,
    (file, line, col): (&str, i64, i64),
    direction: &str,
    at: (i64, i64),
) -> Option<String> {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("inspect")
        .arg(db.path())
        .args(["dataflow", "--file", file])
        .args(["--line", &line.to_string(), "--col", &col.to_string()])
        .args(["--direction", direction, "--format", "json"])
        .output()
        .expect("run trace inspect");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let edge = json["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["location"]["line"] == at.0 && e["location"]["col"] == at.1)
        .unwrap_or_else(|| panic!("no edge at {at:?}: {json:#}"));
    if edge["scope"]["kind"] != "function" {
        return None;
    }
    json["scopes"]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == edge["scope"]["id"])
        .map(|f| f["name"].as_str().unwrap().to_owned())
}

/// `trace inspect dataflow` resolves an operation's function by the same
/// rule: the edge `scope` of the store in the `STORE` expansion is `moves`,
/// and of the field read in the shared header body `next_of`.
#[test]
fn inspect_scopes_operations_by_the_enclosing_function() {
    let root = fixture("flow_origin_sites");
    let db = cli_analyze(&root, &[]);
    // `int *c = *pp;` reaches `STORE(pp, c);`.
    assert_eq!(
        inspect_scope(&db, ("main.c", 9, 10), "down", (12, 5)).as_deref(),
        Some("moves")
    );
    // `struct node *hx = hn->next;`
    assert_eq!(
        inspect_scope(&db, ("sites.h", 10, 18), "up", (10, 23)).as_deref(),
        Some("next_of")
    );
}

/// A lambda is a definition of its own written inside another. A site on a
/// line inside the lambda (not its first or last) is the lambda's, the
/// innermost definition holding it; a site on a line both the lambda and its
/// enclosing function hold at a boundary (a one-line lambda) has none.
#[test]
fn a_lambda_body_site_is_the_lambdas() {
    let src = "int g;\nint *gp;\nvoid Reset() {\n    int *local = &g;\n    \
               auto body = [&local]() {\n        gp = local;\n    };\n    body();\n    \
               auto once = [&local]() { gp = local; };\n    once();\n}\n";
    let dir = scratch(&[("main.cpp", src)]);
    let db = cli_analyze(dir.path(), &[]);
    let copies = sites(&db, "copy", "local", Some("gp"));
    let [once, body] = <[Site; 2]>::try_from(copies).unwrap();
    assert_eq!((body.1.as_str(), body.2, body.3), ("main.cpp", 6, 9));
    assert!(
        body.0
            .as_deref()
            .is_some_and(|f| f.starts_with("Reset::$lambda")),
        "{body:?}"
    );
    assert_eq!(once, (None, "main.cpp".into(), 9, 30));
    assert_eq!(
        lines(sites(&db, "addr_of", "g", Some("local"))),
        vec![at(Some("Reset"), "main.cpp", 4)]
    );
    // `trace inspect dataflow` scopes the edges by the same rule.
    assert_eq!(
        inspect_scope(&db, ("main.cpp", 4, 10), "down", (6, 9)),
        body.0
    );
    assert_eq!(
        inspect_scope(&db, ("main.cpp", 4, 10), "down", (9, 30)),
        None
    );
}
