pub const SCHEMA_VERSION: i64 = 6;

// Keep the complete public schema and the bulk-export phases in sync without
// duplicating SQL. The exporter defers only non-unique secondary indexes.
macro_rules! define_schema {
    ($tables:literal, $indexes:literal) => {
        pub const SCHEMA_V6: &str = concat!($tables, $indexes);
        pub(crate) const TABLES_V6: &str = $tables;
        pub(crate) const INDEXES_V6: &str = $indexes;
    };
}

define_schema!(
    r#"
CREATE TABLE IF NOT EXISTS analysis_run (
    id INTEGER PRIMARY KEY,
    trace_version TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    target_root TEXT NOT NULL,
    created_at TEXT NOT NULL,
    options_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    sha256 TEXT NOT NULL,
    is_dep INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS link_targets (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    output TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS target_sources (
    target_id INTEGER NOT NULL REFERENCES link_targets(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    PRIMARY KEY (target_id, file_id)
);

CREATE TABLE IF NOT EXISTS target_dependencies (
    target_id INTEGER NOT NULL REFERENCES link_targets(id),
    dependency_id INTEGER NOT NULL REFERENCES link_targets(id),
    PRIMARY KEY (target_id, dependency_id)
);

CREATE TABLE IF NOT EXISTS functions (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    file_id INTEGER NOT NULL REFERENCES files(id),
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    linkage TEXT NOT NULL,
    signature TEXT NOT NULL,
    is_defined INTEGER NOT NULL,
    is_dep INTEGER NOT NULL DEFAULT 0,
    is_weak INTEGER NOT NULL DEFAULT 0,
    target_id INTEGER REFERENCES link_targets(id)
);

CREATE TABLE IF NOT EXISTS types (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    layout_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS variables (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    fn_id INTEGER REFERENCES functions(id),
    type_id INTEGER NOT NULL REFERENCES types(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    line INTEGER NOT NULL,
    col INTEGER NOT NULL DEFAULT 0,
    is_weak INTEGER NOT NULL DEFAULT 0,
    target_id INTEGER REFERENCES link_targets(id)
);

CREATE TABLE IF NOT EXISTS call_sites (
    id INTEGER PRIMARY KEY,
    caller_fn_id INTEGER NOT NULL REFERENCES functions(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    expansion_file_id INTEGER REFERENCES files(id),
    expansion_line INTEGER,
    expansion_col INTEGER,
    callee_text TEXT NOT NULL,
    is_direct INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS call_edges (
    id INTEGER PRIMARY KEY,
    call_site_id INTEGER REFERENCES call_sites(id),
    caller_fn_id INTEGER NOT NULL REFERENCES functions(id),
    callee_fn_id INTEGER NOT NULL REFERENCES functions(id),
    resolution TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS arg_flow_edges (
    id INTEGER PRIMARY KEY,
    call_site_id INTEGER NOT NULL REFERENCES call_sites(id),
    arg_index INTEGER NOT NULL,
    actual_var_id INTEGER REFERENCES variables(id),
    actual_fn_id INTEGER REFERENCES functions(id),
    formal_var_id INTEGER NOT NULL REFERENCES variables(id)
);

CREATE TABLE IF NOT EXISTS locations (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    desc TEXT NOT NULL,
    type_id INTEGER REFERENCES types(id)
);

CREATE TABLE IF NOT EXISTS points_to (
    var_node_id INTEGER NOT NULL,
    loc_id INTEGER NOT NULL REFERENCES locations(id),
    PRIMARY KEY (var_node_id, loc_id)
);

CREATE TABLE IF NOT EXISTS diagnostics (
    id INTEGER PRIMARY KEY,
    severity TEXT NOT NULL,
    file_id INTEGER REFERENCES files(id),
    line INTEGER NOT NULL,
    message TEXT NOT NULL,
    stage TEXT NOT NULL
);

-- Value-flow graph (PAG) for `inspect dataflow`. Nodes mirror PAG nodes;
-- edges are the post-solve constraint set, including parameter copies wired
-- dynamically during solving (so the table is the interprocedural
-- value-flow view, not just the lowering-time constraints).
CREATE TABLE IF NOT EXISTS flow_nodes (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    label TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    var_id INTEGER REFERENCES variables(id),
    fn_id INTEGER REFERENCES functions(id)
);

CREATE TABLE IF NOT EXISTS flow_edges (
    id INTEGER PRIMARY KEY,
    src_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    dst_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    kind TEXT NOT NULL
);

"#,
    r#"
CREATE INDEX IF NOT EXISTS idx_call_edges_callee ON call_edges(callee_fn_id);
CREATE INDEX IF NOT EXISTS idx_call_edges_caller ON call_edges(caller_fn_id);
CREATE INDEX IF NOT EXISTS idx_call_edges_callsite ON call_edges(call_site_id);
CREATE INDEX IF NOT EXISTS idx_arg_flow_callsite ON arg_flow_edges(call_site_id);
CREATE INDEX IF NOT EXISTS idx_functions_name ON functions(name);
CREATE INDEX IF NOT EXISTS idx_flow_edges_src ON flow_edges(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_edges_dst ON flow_edges(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_nodes_var ON flow_nodes(var_id);
-- Partial: without link metadata every `target_id` is NULL, and a partial
-- index over no rows costs nothing to build or store.
CREATE INDEX IF NOT EXISTS idx_functions_target ON functions(target_id) WHERE target_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_variables_target ON variables(target_id) WHERE target_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_target_sources_file ON target_sources(file_id);
CREATE INDEX IF NOT EXISTS idx_target_dependencies_dependency ON target_dependencies(dependency_id);
"#
);

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn deferred_indexes_preserve_schema_and_insertion_constraints() {
        let staged = Connection::open_in_memory().unwrap();
        staged.execute_batch("BEGIN IMMEDIATE;").unwrap();
        staged.execute_batch(TABLES_V6).unwrap();
        staged
            .execute(
                "INSERT INTO files (id, path, sha256) VALUES (1, 'a.c', '')",
                [],
            )
            .unwrap();
        assert!(staged
            .execute(
                "INSERT INTO files (id, path, sha256) VALUES (1, 'b.c', '')",
                []
            )
            .is_err());
        assert!(staged
            .execute(
                "INSERT INTO files (id, path, sha256) VALUES (2, 'a.c', '')",
                []
            )
            .is_err());
        staged.execute_batch(INDEXES_V6).unwrap();
        staged.execute_batch("COMMIT;").unwrap();

        let complete = Connection::open_in_memory().unwrap();
        complete.execute_batch(SCHEMA_V6).unwrap();
        let schema = |conn: &Connection| {
            conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
                .unwrap()
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(schema(&staged), schema(&complete));
        assert_eq!(
            staged
                .query_row("SELECT path FROM files WHERE id=1", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "a.c"
        );
    }
}
