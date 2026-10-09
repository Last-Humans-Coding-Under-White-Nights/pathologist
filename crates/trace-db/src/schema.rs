// v7 is an additive compatibility family, not a capability discriminator.
// New tables, nullable/defaulted columns, and indexes may extend it without
// changing existing meanings. Consumers must check the structures they use;
// table presence alone does not imply populated data (notably merge outputs).
// Incompatible layouts or meanings require a version bump. See
// docs/SQLITE_SCHEMA.md, "Version and capability contract".
pub const SCHEMA_VERSION: i64 = 7;

// Keep the complete public schema and the bulk-export phases in sync without
// duplicating SQL. Query indexes are built before provenance export; other
// non-unique secondary indexes remain deferred until bulk insertion finishes.
macro_rules! define_schema {
    ($tables:literal, $query_indexes:literal, $indexes:literal) => {
        pub const SCHEMA_V7: &str = concat!($tables, $query_indexes, $indexes);
        pub const TABLES_V7: &str = $tables;
        pub const PROVENANCE_INDEXES_V7: &str = $query_indexes;
        pub const INDEXES_V7: &str = concat!($query_indexes, $indexes);
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
    is_synthetic INTEGER NOT NULL DEFAULT 0,
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
    is_direct INTEGER NOT NULL,
    callee_var INTEGER,
    return_dst INTEGER
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

-- Where threads, tasks and IPC requests start running code: one row per
-- resolved callback invocation (`call_site_id` set), then one per entry no
-- call site starts (`call_site_id` NULL): an override of a member a framework
-- runs, or an IPC stub handler. `call_edges` holds the same starts as
-- ordinary edges. Additive to v7: earlier v7 exports lack the table.
CREATE TABLE IF NOT EXISTS execution_contexts (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    entry_fn_id INTEGER NOT NULL REFERENCES functions(id),
    call_site_id INTEGER REFERENCES call_sites(id),
    api_fn_id INTEGER REFERENCES functions(id),
    param_index INTEGER,
    receiver_var_id INTEGER REFERENCES variables(id),
    model TEXT,
    multi_instance TEXT NOT NULL,
    self_concurrent INTEGER NOT NULL
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
    label TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    var_id INTEGER REFERENCES variables(id),
    fn_id INTEGER REFERENCES functions(id),
    call_site_id INTEGER REFERENCES call_sites(id)
);

-- Optional presentation provenance; raw graph contracts stay unchanged.
CREATE TABLE IF NOT EXISTS flow_parameters (
    fn_id INTEGER NOT NULL REFERENCES functions(id),
    arg_index INTEGER NOT NULL,
    var_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    type_name TEXT NOT NULL,
    PRIMARY KEY (fn_id, arg_index)
);

CREATE TABLE IF NOT EXISTS flow_field_locations (
    node_id INTEGER PRIMARY KEY REFERENCES flow_nodes(id),
    parent_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    field_name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS flow_field_access (
    base_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    dst_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    field_name TEXT NOT NULL,
    PRIMARY KEY (base_node, dst_node, field_name)
);
CREATE TABLE IF NOT EXISTS flow_return_calls (
    src_node INTEGER NOT NULL, dst_node INTEGER NOT NULL,
    call_site_id INTEGER NOT NULL REFERENCES call_sites(id),
    callee_fn_id INTEGER NOT NULL REFERENCES functions(id)
);
CREATE TABLE IF NOT EXISTS flow_calls (
    src_node INTEGER NOT NULL, dst_node INTEGER NOT NULL,
    call_site_id INTEGER NOT NULL REFERENCES call_sites(id), arg_index INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS flow_call_expressions (
    call_site_id INTEGER PRIMARY KEY REFERENCES call_sites(id),
    expression TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS flow_call_origins (
    call_site_id INTEGER PRIMARY KEY REFERENCES call_sites(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    line INTEGER NOT NULL, col INTEGER NOT NULL, expression TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS flow_origins (
    src_node INTEGER NOT NULL, dst_node INTEGER NOT NULL, kind TEXT NOT NULL,
    file_id INTEGER NOT NULL REFERENCES files(id), line INTEGER NOT NULL,
    col INTEGER NOT NULL, expression TEXT NOT NULL, operation TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS flow_edges (
    id INTEGER PRIMARY KEY,
    src_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    dst_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    kind TEXT NOT NULL
);

-- The memory cell each `load` reads and each `store` writes, by the
-- `flow_edges` row of the load or store (whose `flow_origins` rows are its
-- sites); `cell_node` NULL: the access's cells are not recorded. Additive to
-- v7: earlier v7 exports lack the table, and trace-merge output leaves it
-- empty. See docs/SQLITE_SCHEMA.md, "flow_memory_access".
CREATE TABLE IF NOT EXISTS flow_memory_access (
    edge_id INTEGER NOT NULL REFERENCES flow_edges(id),
    cell_node INTEGER REFERENCES flow_nodes(id)
);

CREATE VIEW IF NOT EXISTS flow_nodes_text AS
SELECT
    n.id,
    n.kind,
    CASE
        WHEN n.kind = 'var' AND (n.label IS NULL OR n.label = '') THEN
            COALESCE(v.name, 'var' || COALESCE(n.var_id, n.id))
        ELSE n.label
    END AS label,
    CASE
        WHEN n.kind = 'var' AND (n.detail IS NULL OR n.detail = '') THEN
            CASE
                WHEN v.id IS NOT NULL THEN
                    v.kind || ' @' || v.line || CASE WHEN f.name IS NOT NULL THEN ' in ' || f.name ELSE '' END
                ELSE ''
            END
        ELSE n.detail
    END AS detail,
    n.var_id,
    n.fn_id
FROM flow_nodes n
LEFT JOIN variables v ON v.id = n.var_id
LEFT JOIN functions f ON f.id = COALESCE(n.fn_id, v.fn_id);

"#,
    r#"
CREATE INDEX IF NOT EXISTS idx_call_sites_indirect_return ON call_sites(callee_var, return_dst)
    WHERE callee_var IS NOT NULL AND return_dst IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_call_edges_callsite ON call_edges(call_site_id);
"#,
    r#"
CREATE INDEX IF NOT EXISTS idx_call_edges_callee ON call_edges(callee_fn_id);
CREATE INDEX IF NOT EXISTS idx_call_edges_caller ON call_edges(caller_fn_id);
CREATE INDEX IF NOT EXISTS idx_arg_flow_callsite ON arg_flow_edges(call_site_id);
CREATE INDEX IF NOT EXISTS idx_functions_name ON functions(name);
CREATE INDEX IF NOT EXISTS idx_functions_file_range ON functions(file_id,is_defined,line_start,line_end);
CREATE INDEX IF NOT EXISTS idx_flow_edges_src ON flow_edges(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_edges_dst ON flow_edges(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_origins_src_node ON flow_origins(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_origins_dst_node ON flow_origins(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_calls_src_node ON flow_calls(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_calls_dst_node ON flow_calls(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_return_calls_src_node ON flow_return_calls(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_return_calls_dst_node ON flow_return_calls(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_field_access_dst ON flow_field_access(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_memory_access_edge ON flow_memory_access(edge_id);
CREATE INDEX IF NOT EXISTS idx_flow_memory_access_cell ON flow_memory_access(cell_node);
CREATE INDEX IF NOT EXISTS idx_variables_parameter_twins ON variables(fn_id,name) WHERE kind='param';
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
        staged.execute_batch(TABLES_V7).unwrap();
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
        staged.execute_batch(INDEXES_V7).unwrap();
        staged.execute_batch("COMMIT;").unwrap();

        let complete = Connection::open_in_memory().unwrap();
        complete.execute_batch(SCHEMA_V7).unwrap();
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
