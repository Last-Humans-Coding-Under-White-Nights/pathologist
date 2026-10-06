//! Exact, identity-preserving queries for snapshot editor integrations.
//! These are opt-in reads; neither export nor the existing CLI queries use them.

use super::Direction;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use std::path::Path;
use trace_ir::{FnId, TargetId};

/// Metadata available even in a minimal export. IDs are database identities,
/// including separate overloads, internal definitions and link images.
#[derive(Debug, Clone)]
pub struct EditorFunction {
    pub id: FnId,
    pub name: String,
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub signature: String,
    pub is_defined: bool,
    pub linkage: String,
    pub is_dep: bool,
    pub target_id: Option<TargetId>,
    pub target_name: Option<String>,
    pub target_output: Option<String>,
}

const FUNCTION_COLUMNS: &str = "f.id, f.name, p.path, f.line_start, f.line_end, \
    f.signature, f.is_defined, f.linkage, f.is_dep, f.target_id, t.name, t.output";
const FUNCTION_JOINS: &str = "FROM functions f JOIN files p ON p.id=f.file_id \
    LEFT JOIN link_targets t ON t.id=f.target_id";

fn function_row(row: &Row<'_>) -> rusqlite::Result<EditorFunction> {
    Ok(EditorFunction {
        id: FnId(row.get(0)?),
        name: row.get(1)?,
        path: row.get(2)?,
        line_start: row.get(3)?,
        line_end: row.get(4)?,
        signature: row.get(5)?,
        is_defined: row.get(6)?,
        linkage: row.get(7)?,
        is_dep: row.get(8)?,
        target_id: row.get::<_, Option<u32>>(9)?.map(TargetId),
        target_name: row.get(10)?,
        target_output: row.get(11)?,
    })
}

/// Open without CREATE or write permission, validate the current schema and
/// pin one read transaction for the server lifetime. Never migrate or index.
/// A rebuild requires closing this connection and opening a new snapshot.
pub fn open_editor_snapshot(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open analysis database {} read-only", path.display()))?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN DEFERRED;")?;
    let versions: Vec<i64> = conn
        .prepare("SELECT schema_version FROM analysis_run")
        .context("invalid analysis database: missing analysis_run metadata")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if versions.is_empty() || versions.iter().any(|v| *v != crate::SCHEMA_VERSION) {
        bail!(
            "unsupported analysis database schema {versions:?}; expected {}",
            crate::SCHEMA_VERSION
        );
    }
    // Preparing these queries validates every column used by the integration,
    // even for an empty database or a falsely labelled schema version.
    conn.prepare(&format!(
        "SELECT {FUNCTION_COLUMNS} {FUNCTION_JOINS} LIMIT 0"
    ))
    .context("incompatible analysis database: missing function metadata")?;
    conn.prepare(
        "SELECT ce.caller_fn_id, ce.callee_fn_id, ce.call_site_id, \
        cs.file_id, cs.line, cs.col, cs.expansion_file_id, cs.expansion_line, cs.expansion_col \
        FROM call_edges ce LEFT JOIN call_sites cs ON cs.id=ce.call_site_id LIMIT 0",
    )
    .context("incompatible analysis database: missing call metadata")?;
    Ok(conn)
}

/// Stored paths, without substring matching. The consumer normalizes paths
/// and maps local URIs back to these exact strings before querying.
pub fn editor_file_paths(conn: &Connection) -> Result<Vec<String>> {
    Ok(conn
        .prepare("SELECT path FROM files ORDER BY path")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// All source-located functions containing a one-based line in one exact file.
/// Keep every candidate; line ranges cannot disambiguate same-line definitions.
pub fn editor_functions_at(
    conn: &Connection,
    path: &str,
    line: u32,
) -> Result<Vec<EditorFunction>> {
    let located = super::has_source_location_sql("f");
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {FUNCTION_COLUMNS} {FUNCTION_JOINS} \
        WHERE p.path=?1 AND {located} AND f.line_start<=?2 AND f.line_end>=?2 \
        ORDER BY f.id"
    ))?;
    let rows = stmt.query_map(params![path, line], function_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn editor_function(conn: &Connection, id: FnId) -> Result<Option<EditorFunction>> {
    Ok(conn
        .query_row(
            &format!("SELECT {FUNCTION_COLUMNS} {FUNCTION_JOINS} WHERE f.id=?1"),
            [id.0],
            function_row,
        )
        .optional()?)
}

/// A peer and an optional one-based source point in the caller's document.
/// Site-less IPC edges, or sites outside that document, carry no invented point.
#[derive(Debug)]
pub struct EditorCall {
    pub peer: FnId,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

/// Expand exactly one level using the existing caller/callee indexes. Preserve
/// every recorded target and prefer the outermost macro invocation in the
/// caller's file over a replacement-list spelling (see Call source locations).
pub fn editor_calls(conn: &Connection, id: FnId, direction: Direction) -> Result<Vec<EditorCall>> {
    let (peer, root) = match direction {
        Direction::Down => ("callee_fn_id", "caller_fn_id"),
        Direction::Up => ("caller_fn_id", "callee_fn_id"),
    };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT ce.{peer}, \
        CASE WHEN cs.expansion_file_id=caller.file_id THEN cs.expansion_line \
             WHEN cs.file_id=caller.file_id THEN cs.line END, \
        CASE WHEN cs.expansion_file_id=caller.file_id THEN cs.expansion_col \
             WHEN cs.file_id=caller.file_id THEN cs.col END \
        FROM call_edges ce JOIN functions caller ON caller.id=ce.caller_fn_id \
        LEFT JOIN call_sites cs ON cs.id=ce.call_site_id \
        WHERE ce.{root}=?1 ORDER BY ce.{peer}, 2, 3, ce.id"
    ))?;
    let rows = stmt.query_map([id.0], |row| {
        Ok(EditorCall {
            peer: FnId(row.get(0)?),
            line: row.get(1)?,
            col: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_file_lookup_preserves_candidates_and_source_predicate() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::SCHEMA_V7).unwrap();
        conn.execute_batch("INSERT INTO files(id,path,sha256) VALUES (0,'a space/main.cpp',''),(1,'b/main.cpp','');
            INSERT INTO link_targets VALUES(0,'app','build/app');
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined,target_id) VALUES
            (0,'overload',0,1,3,'external','void(int)',1,0),
            (1,'overload',0,1,3,'external','void(float)',1,0),
            (2,'overload',1,1,3,'internal','void()',1,NULL),
            (3,'unlocated',0,0,0,'external','void()',0,NULL);").unwrap();
        let functions = editor_functions_at(&conn, "a space/main.cpp", 2).unwrap();
        assert_eq!(
            functions.iter().map(|f| f.id).collect::<Vec<_>>(),
            [FnId(0), FnId(1)]
        );
        assert_eq!(functions[0].target_name.as_deref(), Some("app"));
        assert_ne!(functions[0].signature, functions[1].signature);
        assert_eq!(
            editor_functions_at(&conn, "b/main.cpp", 1).unwrap()[0].id,
            FnId(2)
        );
        assert!(editor_functions_at(&conn, "main.cpp", 1)
            .unwrap()
            .is_empty());
        assert!(editor_functions_at(&conn, "a space/main.cpp", 0)
            .unwrap()
            .is_empty());
        assert!(editor_function(&conn, FnId(999)).unwrap().is_none());
        assert_eq!(
            editor_function(&conn, FnId(3)).unwrap().unwrap().line_start,
            0
        );
    }
}
