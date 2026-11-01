use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;
use trace_merge::{merge_databases, MergeOptions, WarningKind};

/// Three legacy analysis exports. The first two share declarations and an
/// image-owned header body; C adds a competing api and resolves absent().
fn inputs(root: &Path) -> Vec<PathBuf> {
    ["a", "b", "c"]
        .into_iter()
        .map(|name| {
            let path = root.join(format!("{name}.db"));
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(trace_db::TABLES_V7).unwrap();
            conn.execute(
                "INSERT INTO analysis_run VALUES(1,'test',7,?1,'now','{}')",
                [format!("/src/{name}")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO files VALUES(1,?1,'hash',0),(2,'/src/shared.h','hash',0)",
                [format!("/src/{name}/code.c")],
            )
            .unwrap();
            // Same target labels deliberately name separate input images.
            conn.execute_batch(
                "INSERT INTO link_targets VALUES(1,'app','app');
                 INSERT INTO target_sources VALUES(1,1),(1,2);",
            )
            .unwrap();
            let sql = match name {
                "a" => {
                    "
INSERT INTO functions VALUES
 (1,'a_run',1,10,19,'external','a_run()',1,0,0,1),
 (2,'api',2,1,1,'external','api()',0,0,0,1),
 (3,'local',1,20,22,'internal','local()',1,0,0,1),
 (4,'bridge',2,2,2,'external','bridge()',0,0,0,1),
 (5,'proxy',2,30,35,'external','proxy()',1,0,0,1),
 (6,'stub',1,40,45,'external','stub()',1,0,0,1),
 (7,'absent',2,3,3,'external','absent()',0,0,0,1),
 (8,'unknown',1,0,0,'external','unknown()',1,0,0,1);
INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES
 (1,1,1,11,1,'api()',1),
 (2,1,1,12,1,'local()',1),
 (3,1,1,12,1,'local()',1),
 (4,1,1,13,1,'bridge()',1),
 (5,1,1,14,1,'absent()',1),
 (6,1,1,15,1,'fn()',0),
 (7,8,1,1,1,'local()',1),
 (8,5,2,31,1,'local()',1);
INSERT INTO call_edges VALUES
 (1,1,1,2,'external'),(2,2,1,3,'direct'),(3,3,1,3,'direct'),
 (4,4,1,4,'external'),(5,5,1,7,'external'),(6,6,1,3,'indirect'),
 (7,7,8,3,'direct'),(8,8,5,3,'direct'),(9,NULL,5,6,'ipc');"
                }
                "b" => {
                    "
INSERT INTO functions VALUES
 (1,'api',1,10,12,'external','api()',1,0,0,1),
 (2,'bridge',1,20,22,'external','bridge()',1,0,0,1),
 (3,'b_run',1,30,35,'external','b_run()',1,0,0,1),
 (4,'local',1,40,45,'internal','local()',1,0,0,1),
 (5,'proxy',2,30,35,'external','proxy()',1,0,0,1),
 (6,'absent',2,3,3,'external','absent()',0,0,0,1);
INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES
 (1,3,1,31,1,'local()',1),(2,5,2,31,1,'local()',1);
INSERT INTO call_edges VALUES(1,1,3,4,'direct'),(2,2,5,4,'direct');"
                }
                _ => {
                    "
INSERT INTO functions VALUES
 (1,'api',1,10,12,'external','api()',1,0,0,1),
 (2,'c_run',1,20,25,'external','c_run()',1,0,0,1),
 (3,'local',1,30,35,'internal','local()',1,0,0,1),
 (4,'absent',1,40,45,'external','absent()',1,0,0,1);
INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct)
 VALUES(1,2,1,21,1,'local()',1);
INSERT INTO call_edges VALUES(1,1,2,3,'direct');"
                }
            };
            conn.execute_batch(sql).unwrap();
            path
        })
        .collect()
}

// Source facts group semantically equal edges without relying on row IDs or
// the identity metadata being tested. Repeated occurrences retain separate IDs.
type Edges = BTreeMap<String, BTreeMap<i64, String>>;

fn edges(path: &Path) -> Edges {
    let conn = Connection::open(path).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
    let mut stmt = conn
        .prepare(
            "SELECT json_array(cf.path,c.name,c.signature,ct.output,
                 sf.path,s.line,s.col,s.callee_text,s.is_direct,
                 ef.path,s.expansion_line,s.expansion_col,
                 df.path,d.name,d.signature,d.is_defined,dt.output),e.id,e.resolution
         FROM call_edges e
         JOIN functions c ON c.id=e.caller_fn_id JOIN files cf ON cf.id=c.file_id
         JOIN functions d ON d.id=e.callee_fn_id JOIN files df ON df.id=d.file_id
         LEFT JOIN call_sites s ON s.id=e.call_site_id LEFT JOIN files sf ON sf.id=s.file_id
         LEFT JOIN files ef ON ef.id=s.expansion_file_id
         LEFT JOIN link_targets ct ON ct.id=c.target_id
         LEFT JOIN link_targets dt ON dt.id=d.target_id",
        )
        .unwrap();
    let mut out: Edges = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for row in stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
    {
        let (key, id, resolution) = row.unwrap();
        assert!(id >= 0);
        assert!(ids.insert(id), "duplicate edge ID {id}");
        out.entry(key).or_default().insert(id, resolution);
    }
    out
}

fn merge(inputs: &[PathBuf], output: &Path) -> Edges {
    let report = merge_databases(
        inputs,
        &MergeOptions {
            output: output.into(),
            verbose: false,
        },
    )
    .unwrap();
    let out = edges(output);
    assert_eq!(
        out.values().map(BTreeMap::len).sum::<usize>(),
        report.call_edges_total
    );
    out
}

#[test]
fn edge_ids_survive_added_candidates_and_all_input_permutations() {
    let tmp = tempdir().unwrap();
    let inputs = inputs(tmp.path());
    let baseline = merge(&inputs[..2], &tmp.path().join("ab.db"));
    assert_eq!(baseline.values().map(BTreeMap::len).sum::<usize>(), 11);
    assert!(
        baseline.values().any(|v| v.len() == 2),
        "repeated macro-like occurrences stay distinct"
    );
    let reversed = merge(
        &[inputs[1].clone(), inputs[0].clone()],
        &tmp.path().join("ba.db"),
    );
    assert_eq!(baseline, reversed);

    let mut abc = None;
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let output = tmp.path().join("abc.db");
        let merged = merge(&order.map(|i| inputs[i].clone()), &output);
        if let Some(expected) = &abc {
            assert_eq!(&merged, expected, "order={order:?}");
        } else {
            abc = Some(merged);
        }
    }
    let abc = abc.unwrap();
    let mut removed = 0;
    let mut changed_resolution = 0;
    for (key, old) in &baseline {
        if let Some(new) = abc.get(key) {
            for (id, resolution) in old {
                let now = new.get(id).expect("surviving edge must retain its ID");
                if now != resolution {
                    assert_eq!((resolution.as_str(), now.as_str()), ("direct", "ambiguous"));
                    changed_resolution += 1;
                }
            }
        } else {
            assert!(
                key.contains("absent"),
                "only the unresolved callee is replaced: {key}"
            );
            removed += old.len();
            assert!(
                old.keys()
                    .all(|id| abc.values().all(|v| !v.contains_key(id))),
                "removed IDs must not name another edge"
            );
        }
    }
    assert_eq!(removed, 1);
    assert_eq!(changed_resolution, 1);
}

#[test]
fn merged_inputs_retain_identities_after_renaming_and_further_merges() {
    let tmp = tempdir().unwrap();
    let inputs = inputs(tmp.path());
    let ab_path = tmp.path().join("ab.db");
    let ab = merge(&inputs[..2], &ab_path);
    let renamed = tmp.path().join("renamed.db");
    fs::rename(ab_path, &renamed).unwrap();
    let again_path = tmp.path().join("again.db");
    assert_eq!(ab, merge(std::slice::from_ref(&renamed), &again_path));
    let nested = merge(&[inputs[2].clone(), renamed], &tmp.path().join("nested.db"));
    let flat = merge(&inputs, &tmp.path().join("flat.db"));
    for (key, values) in &nested {
        for id in values.keys() {
            assert!(
                flat[key].contains_key(id),
                "flat/nested edge differs: {key}"
            );
        }
    }
    assert_eq!(
        nested,
        merge(
            &[inputs[2].clone(), again_path],
            &tmp.path().join("nested-again.db")
        )
    );
}

#[test]
fn shared_unscoped_header_survivor_does_not_change_edge_ids() {
    let tmp = tempdir().unwrap();
    let inputs = inputs(tmp.path());
    for input in &inputs {
        Connection::open(input)
            .unwrap()
            .execute_batch("UPDATE functions SET target_id=NULL")
            .unwrap();
    }
    let ab = merge(&inputs[..2], &tmp.path().join("ab.db"));
    let ba = merge(
        &[inputs[1].clone(), inputs[0].clone()],
        &tmp.path().join("ba.db"),
    );
    assert_eq!(ab, ba);
    let conn = Connection::open(tmp.path().join("ab.db")).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM functions WHERE name='proxy'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1,
        "this exercises deduplication to different first-input survivors"
    );
}

#[test]
fn original_database_copies_and_renames_keep_ids_and_deduplicate_by_content() {
    let tmp = tempdir().unwrap();
    let inputs = inputs(tmp.path());
    let baseline = merge(&inputs, &tmp.path().join("baseline.db"));
    for persisted in [false, true] {
        if persisted {
            for input in &inputs {
                trace_db::record_database_identity(&Connection::open(input).unwrap()).unwrap();
            }
        }
        let copies = tmp.path().join(format!("copies-{persisted}"));
        fs::create_dir(&copies).unwrap();
        let copies: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| {
                let copied = copies.join(format!("renamed-{i}.sqlite"));
                fs::copy(input, &copied).unwrap();
                copied
            })
            .collect();
        assert_eq!(
            baseline,
            merge(
                &[copies[2].clone(), copies[0].clone(), copies[1].clone()],
                &tmp.path().join("moved.db")
            )
        );

        let output = tmp.path().join("duplicates.db");
        let report = merge_databases(
            &[
                inputs[0].clone(),
                copies[0].clone(),
                inputs[1].clone(),
                inputs[2].clone(),
            ],
            &MergeOptions {
                output: output.clone(),
                verbose: false,
            },
        )
        .unwrap();
        assert_eq!(report.input_dbs.len(), 3);
        assert_eq!(
            report
                .warnings
                .iter()
                .filter(|w| w.kind == WarningKind::DuplicateInput)
                .count(),
            1
        );
        assert_eq!(baseline, edges(&output));
    }
    // Persisting a global ID is itself identity-neutral. The legacy read-only
    // fallback and a new producer's recorded digest establish the same namespace.
    assert_eq!(baseline, merge(&inputs, &tmp.path().join("persisted.db")));
    let relocated: Vec<_> = inputs
        .iter()
        .enumerate()
        .map(|(i, input)| {
            let path = tmp.path().join(format!("actually-renamed-{i}.db"));
            fs::rename(input, &path).unwrap();
            path
        })
        .collect();
    assert_eq!(
        baseline,
        merge(&relocated, &tmp.path().join("relocated.db"))
    );
}

#[test]
fn different_contents_with_the_same_repository_metadata_are_distinct_inputs() {
    let tmp = tempdir().unwrap();
    let inputs = inputs(tmp.path());
    let variant = tmp.path().join("variant.db");
    fs::copy(&inputs[0], &variant).unwrap();
    let conn = Connection::open(&variant).unwrap();
    let original = trace_db::database_identity(&conn).unwrap();
    // The source tree and repository revision can agree while analysis
    // configurations and the exported call targets differ.
    conn.execute_batch(
        "UPDATE analysis_run SET options_json='{\"defines\":[\"FEATURE\"]}';
         DELETE FROM call_edges WHERE id=6;",
    )
    .unwrap();
    assert_ne!(original, trace_db::record_database_identity(&conn).unwrap());
    let output = tmp.path().join("variants.db");
    let report = merge_databases(
        &[inputs[0].clone(), variant],
        &MergeOptions {
            output: output.clone(),
            verbose: false,
        },
    )
    .unwrap();
    assert_eq!(report.input_dbs.len(), 2);
    assert!(!report
        .warnings
        .iter()
        .any(|w| w.kind == WarningKind::DuplicateInput));
    edges(&output);
}

#[test]
fn overlapping_nested_inputs_preserve_reachability_contexts_and_sequencing() {
    let tmp = tempdir().unwrap();
    let mut inputs = Vec::new();
    for (name, source) in [
        (
            "a",
            "
typedef unsigned long pthread_t;
int pthread_create(pthread_t *, const void *, void *(*)(void *), void *);
void other(void);
static void leaf(void) {}
void f(void) { leaf(); other(); }
void *worker(void *p) { f(); return p; }
void start(void) { pthread_t t; pthread_create(&t, 0, worker, 0); }
",
        ),
        (
            "b",
            "void f(void); void other(void) {} void b_run(void) { f(); }",
        ),
        ("c", "void f(void); void c_run(void) { f(); }"),
    ] {
        let repo = tmp.path().join(name);
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join("main.c"), source).unwrap();
        for (file, command) in [
            (
                "compile_commands.json",
                serde_json::json!([
                    {"directory": repo, "file": "main.c", "output": "main.o",
                     "arguments": ["cc", "-c", "main.c", "-o", "main.o"]}
                ]),
            ),
            (
                "link_commands.json",
                serde_json::json!([
                    {"directory": repo, "output": "app", "arguments": ["cc", "main.o", "-o", "app"]}
                ]),
            ),
        ] {
            fs::write(repo.join(file), command.to_string()).unwrap();
        }
        let db = tmp.path().join(format!("{name}.db"));
        super::merge_test::analyze_repo_with(&repo, &db, &["--sequencing".as_ref()]);
        inputs.push(db);
    }
    let ab_path = tmp.path().join("ab.db");
    let ab = merge(&inputs[..2], &ab_path);
    let bc_path = tmp.path().join("bc.db");
    merge(&inputs[1..], &bc_path);
    let abc_path = tmp.path().join("abc.db");
    let abc = merge(&inputs, &abc_path);
    for (case, baseline, expected, overlapping) in [
        (
            "a-ab",
            &ab_path,
            &ab,
            vec![inputs[0].clone(), ab_path.clone()],
        ),
        (
            "ab-a",
            &ab_path,
            &ab,
            vec![ab_path.clone(), inputs[0].clone()],
        ),
        (
            "ab-bc",
            &abc_path,
            &abc,
            vec![ab_path.clone(), bc_path.clone()],
        ),
        (
            "bc-ab",
            &abc_path,
            &abc,
            vec![bc_path.clone(), ab_path.clone()],
        ),
    ] {
        let output = tmp.path().join(format!("{case}.db"));
        let actual = merge(&overlapping, &output);
        let conn = Connection::open(&output).unwrap();
        let reaches_leaf: i64 = conn.query_row(
            "WITH RECURSIVE reachable(id) AS (
                 SELECT id FROM functions WHERE name='b_run' AND is_defined=1
                 UNION SELECT e.callee_fn_id FROM call_edges e JOIN reachable r ON r.id=e.caller_fn_id
             ) SELECT count(*) FROM reachable r JOIN functions f ON f.id=r.id WHERE f.name='leaf'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(
            reaches_leaf, 1,
            "{case}: b_run -> f -> leaf must remain reachable"
        );
        assert_eq!(
            &actual, expected,
            "{case}: preserve every edge ID and binding"
        );
        let baseline = Connection::open(baseline).unwrap();
        for table in [
            "link_targets",
            "target_sources",
            "target_dependencies",
            "functions",
            "call_sites",
            "execution_contexts",
            "cfg_functions",
            "cfg_events",
            "cfg_event_sites",
            "cfg_event_order",
        ] {
            let count = |c: &Connection| {
                c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
            };
            assert_eq!(
                count(&conn),
                count(&baseline),
                "{case}: canonical {table} rows"
            );
        }
        let attached_contexts: i64 = conn
            .query_row(
                "SELECT count(*) FROM execution_contexts x JOIN call_edges e
             ON e.call_site_id=x.call_site_id AND e.callee_fn_id=x.entry_fn_id
             WHERE x.kind='thread'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            attached_contexts, 1,
            "{case}: thread context uses canonical edge endpoints"
        );
        let disconnected_events: i64 = conn.query_row(
            "SELECT count(*) FROM cfg_event_sites es JOIN cfg_events e ON e.id=es.event_id
             JOIN cfg_functions g ON g.id=e.cfg_id JOIN call_sites s ON s.id=es.call_site_id
             WHERE s.caller_fn_id<>g.fn_id OR NOT EXISTS (SELECT 1 FROM call_edges ce WHERE ce.call_site_id=s.id)",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(
            disconnected_events, 0,
            "{case}: sequenced calls retain their graph edges"
        );
    }
}

#[test]
fn overlapping_inputs_preserve_synthetic_and_positionless_endpoints() {
    let tmp = tempdir().unwrap();
    let inputs = inputs(tmp.path());
    Connection::open(&inputs[0])
        .unwrap()
        .execute_batch(
            "INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES
         (9,1,1,16,1,'unknown()',1),(10,1,1,17,1,'proxy()',1);
         INSERT INTO call_edges VALUES(10,9,1,8,'direct'),(11,10,1,5,'direct');",
        )
        .unwrap();
    Connection::open(&inputs[1])
        .unwrap()
        .execute_batch(
            "INSERT INTO functions VALUES(7,'a_run',1,1,1,'external','a_run()',0,0,0,1);
         INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct)
         VALUES(3,3,1,32,1,'a_run()',1);
         INSERT INTO call_edges VALUES(3,3,3,7,'external');",
        )
        .unwrap();
    let ab_path = tmp.path().join("ab.db");
    let expected = merge(&inputs[..2], &ab_path);
    // Earlier stable-ID metadata did not include target identities. Such an
    // input still canonicalizes its inherited function and call-site endpoints.
    let earlier = tmp.path().join("earlier.db");
    fs::copy(&ab_path, &earlier).unwrap();
    let conn = Connection::open(&earlier).unwrap();
    conn.execute_batch("DROP TABLE merge_target_identities")
        .unwrap();
    trace_db::record_database_identity(&conn).unwrap();
    for overlapping in [
        vec![inputs[0].clone(), ab_path.clone()],
        vec![ab_path.clone(), inputs[0].clone()],
        vec![inputs[0].clone(), earlier.clone()],
        vec![earlier, inputs[0].clone()],
    ] {
        let output = tmp.path().join("nested.db");
        assert_eq!(merge(&overlapping, &output), expected);
        let conn = Connection::open(output).unwrap();
        let names: BTreeSet<String> = conn
            .prepare(
                "WITH RECURSIVE reachable(id) AS (
             SELECT id FROM functions WHERE name='b_run' AND is_defined=1
             UNION SELECT e.callee_fn_id FROM call_edges e JOIN reachable r ON r.id=e.caller_fn_id
             ) SELECT f.name FROM reachable r JOIN functions f ON f.id=r.id",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for name in ["a_run", "unknown", "local", "proxy", "stub"] {
            assert!(
                names.contains(name),
                "{overlapping:?}: {name} must remain reachable"
            );
        }
        for table in ["merge_function_identities", "merge_call_site_identities"] {
            let duplicates: i64 = conn.query_row(&format!(
                "SELECT count(*) FROM (SELECT identity FROM {table} GROUP BY identity HAVING count(*)>1)"
            ), [], |r| r.get(0)).unwrap();
            assert_eq!(duplicates, 0, "canonical {table} rows");
        }
    }
}

#[test]
fn damaged_identity_metadata_is_rejected_without_replacing_output() {
    let tmp = tempdir().unwrap();
    let inputs = inputs(tmp.path());
    for damage in [
        "DROP TABLE merge_call_site_identities",
        "DELETE FROM merge_function_identities WHERE function_id=1",
        "DELETE FROM merge_target_identities WHERE target_id=1",
        "DELETE FROM merge_call_site_identities WHERE call_site_id=1",
        "PRAGMA ignore_check_constraints=ON; UPDATE merge_function_identities SET identity=x'00' WHERE function_id=1",
        "UPDATE analysis_run SET options_json=json_set(options_json,'$.call_edge_identity_version',99)",
        "UPDATE database_identity SET algorithm='future'",
        "PRAGMA ignore_check_constraints=ON; UPDATE database_identity SET digest=x'00'",
    ] {
        let merged = tmp.path().join("merged.db");
        merge(&inputs[..2], &merged);
        Connection::open(&merged).unwrap().execute_batch(damage).unwrap();
        let output = tmp.path().join("output.db");
        fs::write(&output, b"previous output").unwrap();
        let result = merge_databases(&[merged], &MergeOptions { output: output.clone(), verbose: false });
        assert!(result.is_err(), "accepted {damage}");
        assert_eq!(fs::read(output).unwrap(), b"previous output");
    }
}

#[test]
fn reconstructed_member_and_ipc_edges_keep_ids_across_overlap_and_staging() {
    let tmp = tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/ipc_cross_repo");
    let inputs: Vec<_> = ["app", "proxy", "stub"]
        .into_iter()
        .map(|part| {
            let db = tmp.path().join(format!("{part}.db"));
            super::merge_test::analyze_repo_with(&fixture.join(part), &db, &[]);
            db
        })
        .collect();
    let flat_path = tmp.path().join("flat.db");
    let flat = merge(&inputs, &flat_path);
    assert!(flat.values().any(|v| v.values().any(|r| r == "ipc")));
    let reverse = merge(
        &[inputs[2].clone(), inputs[1].clone(), inputs[0].clone()],
        &tmp.path().join("reverse.db"),
    );
    assert_eq!(flat, reverse);
    let ab = tmp.path().join("ab.db");
    merge(&inputs[..2], &ab);
    assert_eq!(
        flat,
        merge(&[ab, inputs[2].clone()], &tmp.path().join("nested.db"))
    );
    for order in [
        vec![flat_path.clone(), inputs[0].clone()],
        vec![inputs[0].clone(), flat_path.clone()],
    ] {
        assert_eq!(flat, merge(&order, &tmp.path().join("overlap.db")));
    }
}
