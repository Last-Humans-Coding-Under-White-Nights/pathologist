// A schema version is a compatibility family, not a capability discriminator.
// New tables, nullable/defaulted columns, and indexes may extend it without
// changing existing meanings. Consumers must check the structures they use;
// table presence alone does not imply populated data (notably merge outputs).
// Incompatible layouts or meanings require a version bump. See
// docs/SQLITE_SCHEMA.md, "Version and capability contract".
pub const SCHEMA_VERSION: i64 = 8;
/// Versions whose call-graph columns current consumers understand.
pub const SUPPORTED_SCHEMA_VERSIONS: &[i64] = &[7, SCHEMA_VERSION];

/// Optional identity of an immutable analysis snapshot; stored by current
/// exporters and computed read-only for earlier databases.
pub const DATABASE_IDENTITY_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS database_identity (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    algorithm TEXT NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest) = 32)
);
"#;

// Legacy and compact layouts share every unaffected table and index.
macro_rules! common_tables {
    ($variables:literal, $call_sites:literal, $flow_nodes:literal, $flow_edges:literal, $without_rowid:literal) => {
        concat!(
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
)"#,
            $without_rowid,
            r#";

CREATE TABLE IF NOT EXISTS target_dependencies (
    target_id INTEGER NOT NULL REFERENCES link_targets(id),
    dependency_id INTEGER NOT NULL REFERENCES link_targets(id),
    PRIMARY KEY (target_id, dependency_id)
)"#,
            $without_rowid,
            r#";

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

CREATE TABLE IF NOT EXISTS arg_flow_edges (
    id INTEGER PRIMARY KEY,
    call_site_id INTEGER NOT NULL "#,
            concat!("REFERENCES ", $call_sites, "("),
            r#"id),
    arg_index INTEGER NOT NULL,
    actual_var_id INTEGER "#,
            concat!("REFERENCES ", $variables, "("),
            r#"id),
    actual_fn_id INTEGER REFERENCES functions(id),
    formal_var_id INTEGER NOT NULL "#,
            concat!("REFERENCES ", $variables, "("),
            r#"id)
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
    call_site_id INTEGER "#,
            concat!("REFERENCES ", $call_sites, "("),
            r#"id),
    api_fn_id INTEGER REFERENCES functions(id),
    param_index INTEGER,
    receiver_var_id INTEGER "#,
            concat!("REFERENCES ", $variables, "("),
            r#"id),
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
)"#,
            $without_rowid,
            r#";

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

-- Optional presentation provenance; raw graph contracts stay unchanged.

CREATE TABLE IF NOT EXISTS flow_field_locations (
    node_id INTEGER PRIMARY KEY "#,
            concat!("REFERENCES ", $flow_nodes, "("),
            r#"id),
    parent_node INTEGER NOT NULL "#,
            concat!("REFERENCES ", $flow_nodes, "("),
            r#"id),
    field_name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS flow_field_access (
    base_node INTEGER NOT NULL "#,
            concat!("REFERENCES ", $flow_nodes, "("),
            r#"id),
    dst_node INTEGER NOT NULL "#,
            concat!("REFERENCES ", $flow_nodes, "("),
            r#"id),
    field_name TEXT NOT NULL,
    PRIMARY KEY (base_node, dst_node, field_name)
)"#,
            $without_rowid,
            r#";
CREATE TABLE IF NOT EXISTS flow_return_calls (
    src_node INTEGER NOT NULL, dst_node INTEGER NOT NULL,
    call_site_id INTEGER NOT NULL "#,
            concat!("REFERENCES ", $call_sites, "("),
            r#"id),
    callee_fn_id INTEGER NOT NULL REFERENCES functions(id)
);
CREATE TABLE IF NOT EXISTS flow_calls (
    src_node INTEGER NOT NULL, dst_node INTEGER NOT NULL,
    call_site_id INTEGER NOT NULL "#,
            concat!("REFERENCES ", $call_sites, "("),
            r#"id), arg_index INTEGER NOT NULL
);

-- The memory cell each `load` reads and each `store` writes, by the
-- `flow_edges` row of the load or store (whose `flow_origins` rows are its
-- sites); `cell_node` NULL: the access's cells are not recorded. Additive to
-- v7: earlier v7 exports lack the table, and trace-merge output leaves it
-- empty. See docs/SQLITE_SCHEMA.md, "flow_memory_access".
CREATE TABLE IF NOT EXISTS flow_memory_access (
    edge_id INTEGER NOT NULL "#,
            concat!("REFERENCES ", $flow_edges, "("),
            r#"id),
    cell_node INTEGER "#,
            concat!("REFERENCES ", $flow_nodes, "("),
            r#"id)
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

"#
        )
    };
}

macro_rules! legacy_storage {
    () => {
        concat!(
            r#"CREATE TABLE IF NOT EXISTS variables (
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

CREATE TABLE IF NOT EXISTS flow_nodes (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    var_id INTEGER REFERENCES variables(id),
    fn_id INTEGER REFERENCES functions(id),
    call_site_id INTEGER REFERENCES call_sites(id)
);

CREATE TABLE IF NOT EXISTS call_edges (
    id INTEGER PRIMARY KEY,
    call_site_id INTEGER REFERENCES call_sites(id),
    caller_fn_id INTEGER NOT NULL REFERENCES functions(id),
    callee_fn_id INTEGER NOT NULL REFERENCES functions(id),
    resolution TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS flow_edges (
    id INTEGER PRIMARY KEY,
    src_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    dst_node INTEGER NOT NULL REFERENCES flow_nodes(id),
    kind TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS flow_origins (
    src_node INTEGER NOT NULL, dst_node INTEGER NOT NULL, kind TEXT NOT NULL,
    file_id INTEGER NOT NULL REFERENCES files(id), line INTEGER NOT NULL,
    col INTEGER NOT NULL, expression TEXT NOT NULL, operation TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS flow_call_origins (
    call_site_id INTEGER PRIMARY KEY REFERENCES call_sites(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    line INTEGER NOT NULL, col INTEGER NOT NULL, expression TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS flow_call_expressions (
    call_site_id INTEGER PRIMARY KEY REFERENCES call_sites(id),
    expression TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS flow_parameters (
    fn_id INTEGER NOT NULL REFERENCES functions(id),
    arg_index INTEGER NOT NULL,
    var_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    type_name TEXT NOT NULL,
    PRIMARY KEY (fn_id, arg_index)
);
"#
        )
    };
}

macro_rules! compact_storage {
    () => {
        concat!(
            r#"CREATE TABLE IF NOT EXISTS strings (
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS variables_data (
    id INTEGER PRIMARY KEY,
    name_id INTEGER NOT NULL REFERENCES strings(id),
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

CREATE VIEW IF NOT EXISTS variables AS
SELECT o.id,
       (SELECT text FROM strings WHERE id=o.name_id) AS name,
       o.kind,
       o.fn_id,
       o.type_id,
       o.file_id,
       o.line,
       o.col,
       o.is_synthetic,
       o.is_weak,
       o.target_id
FROM variables_data o;

CREATE TABLE IF NOT EXISTS call_sites_data (
    id INTEGER PRIMARY KEY,
    caller_fn_id INTEGER NOT NULL REFERENCES functions(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    expansion_file_id INTEGER REFERENCES files(id),
    expansion_line INTEGER,
    expansion_col INTEGER,
    callee_text_id INTEGER NOT NULL REFERENCES strings(id),
    is_direct INTEGER NOT NULL,
    callee_var INTEGER,
    return_dst INTEGER
);

CREATE VIEW IF NOT EXISTS call_sites AS
SELECT o.id,
       o.caller_fn_id,
       o.file_id,
       o.line,
       o.col,
       o.expansion_file_id,
       o.expansion_line,
       o.expansion_col,
       (SELECT text FROM strings WHERE id=o.callee_text_id) AS callee_text,
       o.is_direct,
       o.callee_var,
       o.return_dst
FROM call_sites_data o;

CREATE TABLE IF NOT EXISTS flow_nodes_data (
    id INTEGER PRIMARY KEY,
    kind_id INTEGER NOT NULL REFERENCES strings(id),
    label_id INTEGER NOT NULL REFERENCES strings(id) DEFAULT 1,
    detail_id INTEGER NOT NULL REFERENCES strings(id) DEFAULT 1,
    var_id INTEGER REFERENCES variables_data(id),
    fn_id INTEGER REFERENCES functions(id),
    call_site_id INTEGER REFERENCES call_sites_data(id)
);

CREATE VIEW IF NOT EXISTS flow_nodes AS
SELECT o.id,
       (SELECT text FROM strings WHERE id=o.kind_id) AS kind,
       (SELECT text FROM strings WHERE id=o.label_id) AS label,
       (SELECT text FROM strings WHERE id=o.detail_id) AS detail,
       o.var_id,
       o.fn_id,
       o.call_site_id
FROM flow_nodes_data o;

CREATE TABLE IF NOT EXISTS call_edges_data (
    id INTEGER PRIMARY KEY,
    call_site_id INTEGER REFERENCES call_sites_data(id),
    caller_fn_id INTEGER NOT NULL REFERENCES functions(id),
    callee_fn_id INTEGER NOT NULL REFERENCES functions(id),
    resolution_id INTEGER NOT NULL REFERENCES strings(id)
);

CREATE VIEW IF NOT EXISTS call_edges AS
SELECT o.id,
       o.call_site_id,
       o.caller_fn_id,
       o.callee_fn_id,
       (SELECT text FROM strings WHERE id=o.resolution_id) AS resolution
FROM call_edges_data o;

CREATE TABLE IF NOT EXISTS flow_edges_data (
    id INTEGER PRIMARY KEY,
    src_node INTEGER NOT NULL REFERENCES flow_nodes_data(id),
    dst_node INTEGER NOT NULL REFERENCES flow_nodes_data(id),
    kind_id INTEGER NOT NULL REFERENCES strings(id)
);

CREATE VIEW IF NOT EXISTS flow_edges AS
SELECT o.id,
       o.src_node,
       o.dst_node,
       (SELECT text FROM strings WHERE id=o.kind_id) AS kind
FROM flow_edges_data o;

CREATE TABLE IF NOT EXISTS flow_origins_data (
    src_node INTEGER NOT NULL, dst_node INTEGER NOT NULL, kind_id INTEGER NOT NULL REFERENCES strings(id),
    file_id INTEGER NOT NULL REFERENCES files(id), line INTEGER NOT NULL,
    col INTEGER NOT NULL, expression_id INTEGER NOT NULL REFERENCES strings(id), operation_id INTEGER NOT NULL REFERENCES strings(id)
);

CREATE VIEW IF NOT EXISTS flow_origins AS
SELECT o.src_node,
       o.dst_node,
       (SELECT text FROM strings WHERE id=o.kind_id) AS kind,
       o.file_id,
       o.line,
       o.col,
       (SELECT text FROM strings WHERE id=o.expression_id) AS expression,
       (SELECT text FROM strings WHERE id=o.operation_id) AS operation
FROM flow_origins_data o;

CREATE TABLE IF NOT EXISTS flow_call_origins_data (
    call_site_id INTEGER PRIMARY KEY REFERENCES call_sites_data(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    line INTEGER NOT NULL, col INTEGER NOT NULL, expression_id INTEGER NOT NULL REFERENCES strings(id)
);

CREATE VIEW IF NOT EXISTS flow_call_origins AS
SELECT o.call_site_id,
       o.file_id,
       o.line,
       o.col,
       (SELECT text FROM strings WHERE id=o.expression_id) AS expression
FROM flow_call_origins_data o;

CREATE TABLE IF NOT EXISTS flow_call_expressions_data (
    call_site_id INTEGER PRIMARY KEY REFERENCES call_sites_data(id),
    expression_id INTEGER NOT NULL REFERENCES strings(id)
);

CREATE VIEW IF NOT EXISTS flow_call_expressions AS
SELECT o.call_site_id,
       (SELECT text FROM strings WHERE id=o.expression_id) AS expression
FROM flow_call_expressions_data o;

CREATE TABLE IF NOT EXISTS flow_parameters_data (
    fn_id INTEGER NOT NULL REFERENCES functions(id),
    arg_index INTEGER NOT NULL,
    var_id INTEGER NOT NULL,
    name_id INTEGER NOT NULL REFERENCES strings(id),
    type_name_id INTEGER NOT NULL REFERENCES strings(id),
    PRIMARY KEY (fn_id, arg_index)
) WITHOUT ROWID;

CREATE VIEW IF NOT EXISTS flow_parameters AS
SELECT o.fn_id,
       o.arg_index,
       o.var_id,
       (SELECT text FROM strings WHERE id=o.name_id) AS name,
       (SELECT text FROM strings WHERE id=o.type_name_id) AS type_name
FROM flow_parameters_data o;

-- Call-order control-flow graphs (`trace analyze --sequencing`): one graph per
-- function body and configuration variant. Kinds and coverage are `strings`
-- ids; block and event ordinals count from 0 within their graph. Empty unless
-- `analysis_run.options_json.call_sequencing` is true. Additive to v8. See
-- docs/SQLITE_SCHEMA.md, "Call sequencing tables".
CREATE TABLE IF NOT EXISTS cfg_functions (
    id INTEGER PRIMARY KEY,
    fn_id INTEGER NOT NULL REFERENCES functions(id),
    tu_file_id INTEGER REFERENCES files(id),
    variant INTEGER NOT NULL,
    entry_block INTEGER NOT NULL REFERENCES cfg_blocks(id),
    exit_block INTEGER NOT NULL REFERENCES cfg_blocks(id),
    exceptional_exit_block INTEGER REFERENCES cfg_blocks(id),
    coverage_id INTEGER NOT NULL REFERENCES strings(id)
);

CREATE TABLE IF NOT EXISTS cfg_blocks (
    id INTEGER PRIMARY KEY,
    cfg_id INTEGER NOT NULL REFERENCES cfg_functions(id),
    ordinal INTEGER NOT NULL,
    kind_id INTEGER NOT NULL REFERENCES strings(id),
    file_id INTEGER REFERENCES files(id),
    line INTEGER,
    col INTEGER
);

CREATE TABLE IF NOT EXISTS cfg_edges (
    src_block INTEGER NOT NULL REFERENCES cfg_blocks(id),
    dst_block INTEGER NOT NULL REFERENCES cfg_blocks(id),
    kind_id INTEGER NOT NULL REFERENCES strings(id),
    PRIMARY KEY (src_block, dst_block, kind_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS cfg_events (
    id INTEGER PRIMARY KEY,
    cfg_id INTEGER NOT NULL REFERENCES cfg_functions(id),
    block_id INTEGER NOT NULL REFERENCES cfg_blocks(id),
    ordinal INTEGER NOT NULL,
    kind_id INTEGER NOT NULL REFERENCES strings(id),
    file_id INTEGER NOT NULL REFERENCES files(id),
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    expansion_file_id INTEGER REFERENCES files(id),
    expansion_line INTEGER,
    expansion_col INTEGER
);

CREATE TABLE IF NOT EXISTS cfg_event_sites (
    event_id INTEGER NOT NULL REFERENCES cfg_events(id),
    call_site_id INTEGER NOT NULL REFERENCES call_sites_data(id),
    ordinal INTEGER NOT NULL,
    PRIMARY KEY (event_id, call_site_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS cfg_event_order (
    event_id INTEGER NOT NULL REFERENCES cfg_events(id),
    next_event_id INTEGER NOT NULL REFERENCES cfg_events(id),
    PRIMARY KEY (event_id, next_event_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS cfg_gaps (
    id INTEGER PRIMARY KEY,
    cfg_id INTEGER NOT NULL REFERENCES cfg_functions(id),
    kind_id INTEGER NOT NULL REFERENCES strings(id),
    file_id INTEGER REFERENCES files(id),
    line INTEGER,
    col INTEGER,
    first_event INTEGER REFERENCES cfg_events(id),
    last_event INTEGER REFERENCES cfg_events(id)
);
"#
        )
    };
}

// Only the compact layout carries the call-sequencing tables.
macro_rules! sequencing_indexes {
    () => {
        r#"
CREATE INDEX IF NOT EXISTS idx_cfg_functions_fn ON cfg_functions(fn_id);
CREATE INDEX IF NOT EXISTS idx_cfg_blocks_cfg ON cfg_blocks(cfg_id);
CREATE INDEX IF NOT EXISTS idx_cfg_edges_dst ON cfg_edges(dst_block);
CREATE INDEX IF NOT EXISTS idx_cfg_events_block ON cfg_events(block_id);
CREATE INDEX IF NOT EXISTS idx_cfg_events_file_line ON cfg_events(file_id, line);
CREATE INDEX IF NOT EXISTS idx_cfg_events_expansion ON cfg_events(expansion_file_id, expansion_line);
CREATE INDEX IF NOT EXISTS idx_cfg_event_sites_site ON cfg_event_sites(call_site_id);
CREATE INDEX IF NOT EXISTS idx_cfg_gaps_cfg ON cfg_gaps(cfg_id);
"#
    };
}

macro_rules! query_indexes {
    ($call_sites:literal, $call_edges:literal) => {
        concat!(
            r#"
CREATE INDEX IF NOT EXISTS idx_call_sites_indirect_return "#,
            concat!("ON ", $call_sites, "("),
            r#"callee_var, return_dst)
    WHERE callee_var IS NOT NULL AND return_dst IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_call_edges_callsite "#,
            concat!("ON ", $call_edges, "("),
            r#"call_site_id);
"#
        )
    };
}

macro_rules! remaining_indexes {
    ($call_edges:literal, $flow_edges:literal, $variables:literal, $flow_nodes:literal, $name:literal, $origins:literal) => {
        concat!(
            r#"
CREATE INDEX IF NOT EXISTS idx_call_edges_callee "#,
            concat!("ON ", $call_edges, "("),
            r#"callee_fn_id);
CREATE INDEX IF NOT EXISTS idx_call_edges_caller "#,
            concat!("ON ", $call_edges, "("),
            r#"caller_fn_id);
CREATE INDEX IF NOT EXISTS idx_arg_flow_callsite ON arg_flow_edges(call_site_id);
CREATE INDEX IF NOT EXISTS idx_functions_name ON functions(name);
CREATE INDEX IF NOT EXISTS idx_functions_file_range ON functions(file_id,is_defined,line_start,line_end);
CREATE INDEX IF NOT EXISTS idx_flow_edges_src "#,
            concat!("ON ", $flow_edges, "("),
            r#"src_node);
CREATE INDEX IF NOT EXISTS idx_flow_edges_dst "#,
            concat!("ON ", $flow_edges, "("),
            r#"dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_calls_src_node ON flow_calls(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_calls_dst_node ON flow_calls(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_return_calls_src_node ON flow_return_calls(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_return_calls_dst_node ON flow_return_calls(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_field_access_dst ON flow_field_access(dst_node);
CREATE INDEX IF NOT EXISTS idx_flow_memory_access_edge ON flow_memory_access(edge_id);
CREATE INDEX IF NOT EXISTS idx_flow_memory_access_cell ON flow_memory_access(cell_node);
CREATE INDEX IF NOT EXISTS idx_variables_parameter_twins "#,
            concat!("ON ", $variables, "("),
            r#"fn_id,"#,
            $name,
            r#") WHERE kind='param';
CREATE INDEX IF NOT EXISTS idx_flow_nodes_var "#,
            concat!("ON ", $flow_nodes, "("),
            r#"var_id);
-- Partial: without link metadata every `target_id` is NULL, and a partial
-- index over no rows costs nothing to build or store.
CREATE INDEX IF NOT EXISTS idx_functions_target ON functions(target_id) WHERE target_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_variables_target "#,
            concat!("ON ", $variables, "("),
            r#"target_id) WHERE target_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_target_sources_file ON target_sources(file_id);
CREATE INDEX IF NOT EXISTS idx_target_dependencies_dependency ON target_dependencies(dependency_id);

CREATE INDEX IF NOT EXISTS idx_flow_origins_src_node ON "#,
            $origins,
            r#"(src_node);
CREATE INDEX IF NOT EXISTS idx_flow_origins_dst_node ON "#,
            $origins,
            r#"(dst_node);
"#
        )
    };
}

pub const TABLES_V7: &str = concat!(
    common_tables!("variables", "call_sites", "flow_nodes", "flow_edges", ""),
    legacy_storage!()
);
pub const PROVENANCE_INDEXES_V7: &str = query_indexes!("call_sites", "call_edges");
pub const INDEXES_V7: &str = concat!(
    query_indexes!("call_sites", "call_edges"),
    remaining_indexes!(
        "call_edges",
        "flow_edges",
        "variables",
        "flow_nodes",
        "name",
        "flow_origins"
    )
);
pub const SCHEMA_V7: &str = concat!(
    common_tables!("variables", "call_sites", "flow_nodes", "flow_edges", ""),
    legacy_storage!(),
    query_indexes!("call_sites", "call_edges"),
    remaining_indexes!(
        "call_edges",
        "flow_edges",
        "variables",
        "flow_nodes",
        "name",
        "flow_origins"
    )
);

pub const TABLES_V8: &str = concat!(
    common_tables!(
        "variables_data",
        "call_sites_data",
        "flow_nodes_data",
        "flow_edges_data",
        " WITHOUT ROWID"
    ),
    compact_storage!()
);
pub const PROVENANCE_INDEXES_V8: &str = query_indexes!("call_sites_data", "call_edges_data");
pub const INDEXES_V8: &str = concat!(
    query_indexes!("call_sites_data", "call_edges_data"),
    remaining_indexes!(
        "call_edges_data",
        "flow_edges_data",
        "variables_data",
        "flow_nodes_data",
        "name_id",
        "flow_origins_data"
    ),
    sequencing_indexes!()
);
pub const SCHEMA_V8: &str = concat!(
    common_tables!(
        "variables_data",
        "call_sites_data",
        "flow_nodes_data",
        "flow_edges_data",
        " WITHOUT ROWID"
    ),
    compact_storage!(),
    query_indexes!("call_sites_data", "call_edges_data"),
    remaining_indexes!(
        "call_edges_data",
        "flow_edges_data",
        "variables_data",
        "flow_nodes_data",
        "name_id",
        "flow_origins_data"
    ),
    sequencing_indexes!()
);

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn deferred_indexes_preserve_schema_and_insertion_constraints() {
        for (tables, indexes, complete_schema) in [
            (TABLES_V7, INDEXES_V7, SCHEMA_V7),
            (TABLES_V8, INDEXES_V8, SCHEMA_V8),
        ] {
            let staged = Connection::open_in_memory().unwrap();
            staged.execute_batch("BEGIN IMMEDIATE;").unwrap();
            staged.execute_batch(tables).unwrap();
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
            staged.execute_batch(indexes).unwrap();
            staged.execute_batch("COMMIT;").unwrap();

            let complete = Connection::open_in_memory().unwrap();
            complete.execute_batch(complete_schema).unwrap();
            let schema = |conn: &Connection| {
                conn.prepare(
                    "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name",
                )
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
}
