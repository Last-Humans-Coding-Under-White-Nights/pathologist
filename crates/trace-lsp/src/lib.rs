//! Read-only LSP access to a pre-generated trace database. No live analysis.

pub mod locations;
pub mod transport;

use anyhow::{bail, Context, Result};
use locations::{
    function_range, mapped_path, normalize, path_uri, uri_path, PathMapping, Position, Range,
    Source,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use trace_db::{
    editor_calls, editor_file_paths, editor_function, editor_functions_at, open_editor_snapshot,
    Direction, EditorFunction,
};
use trace_ir::FnId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyItem {
    pub name: String,
    pub kind: u32,
    #[serde(default)]
    pub detail: String,
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
    pub data: ItemData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemData {
    session: String,
    function: FnId,
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(-32603, format!("{error:#}"))
    }
}

#[derive(PartialEq, Eq)]
enum State {
    New,
    Initialized,
    Shutdown,
}

pub struct Server {
    conn: Connection,
    session: String,
    state: State,
    /// Exact local path -> original database path(s). Built once; requests
    /// query the stored string using files.path's existing UNIQUE index.
    paths: BTreeMap<PathBuf, Vec<String>>,
    local_paths: BTreeMap<String, PathBuf>,
    sources: BTreeMap<PathBuf, Option<Source>>,
}

impl Server {
    pub fn open(database: &Path, mappings: &[PathMapping]) -> Result<Self> {
        let conn = open_editor_snapshot(database)?;
        let stored_paths = editor_file_paths(&conn)?;
        let base = if stored_paths
            .iter()
            .any(|path| Path::new(path).is_relative())
        {
            relative_path_base(&conn, database)?
        } else {
            PathBuf::new()
        };
        let mut paths: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        let mut local_paths = BTreeMap::new();
        for stored in stored_paths {
            let path = mapped_path(&normalize(&base.join(&stored)), mappings);
            paths.entry(path.clone()).or_default().push(stored.clone());
            local_paths.insert(stored, path);
        }
        static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
        let session = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self {
            conn,
            session,
            state: State::New,
            paths,
            local_paths,
            sources: BTreeMap::new(),
        })
    }

    fn source(&mut self, path: &Path) -> Option<&Source> {
        self.sources
            .entry(path.to_path_buf())
            .or_insert_with(|| Source::read(path))
            .as_ref()
    }

    fn item(&mut self, function: &EditorFunction) -> Result<Option<CallHierarchyItem>> {
        if !trace_db::has_source_location(i64::from(function.line_start)) {
            return Ok(None);
        }
        let path = self
            .local_paths
            .get(&function.path)
            .context("function file missing from database")?
            .clone();
        let uri = path_uri(&path)?;
        let range = function_range(function.line_start, function.line_end, self.source(&path));
        let mut detail = format!(
            "{} [{}; {}{}]",
            function.signature,
            function.linkage,
            if function.is_defined {
                "definition"
            } else {
                "declaration"
            },
            if function.is_dep { "; dependency" } else { "" }
        );
        if let Some(target) = function.target_id {
            detail.push_str(&format!(
                " [target {}: {} ({})]",
                target.0,
                function.target_name.as_deref().unwrap_or(""),
                function.target_output.as_deref().unwrap_or("")
            ));
        }
        Ok(Some(CallHierarchyItem {
            name: function.name.clone(),
            kind: 12,
            detail,
            uri,
            range,
            selection_range: Range::point(range.start),
            data: ItemData {
                session: self.session.clone(),
                function: function.id,
            },
        }))
    }

    fn prepare(&mut self, uri: &str, position: Position) -> std::result::Result<Value, RpcError> {
        let path = uri_path(uri).map_err(|e| RpcError::invalid(e.to_string()))?;
        let Some(stored_paths) = self.paths.get(&path).cloned() else {
            return Ok(Value::Null);
        };
        let Some(line) = position.line.checked_add(1) else {
            return Ok(Value::Null);
        };
        let mut functions = Vec::new();
        for stored in stored_paths {
            functions.extend(editor_functions_at(&self.conn, &stored, line)?);
        }
        functions.sort_by_key(|f| f.id);
        let mut items = Vec::new();
        for function in functions {
            if let Some(item) = self.item(&function)? {
                items.push(item);
            }
        }
        Ok(if items.is_empty() {
            Value::Null
        } else {
            json!(items)
        })
    }

    fn hierarchy(
        &mut self,
        item: Value,
        direction: Direction,
    ) -> std::result::Result<Value, RpcError> {
        let item: CallHierarchyItem = serde_json::from_value(item)
            .map_err(|e| RpcError::invalid(format!("invalid call hierarchy item: {e}")))?;
        if item.data.session != self.session {
            return Err(RpcError::invalid(
                "call hierarchy item is from another server session",
            ));
        }
        let root = editor_function(&self.conn, item.data.function)?
            .ok_or_else(|| RpcError::invalid("unknown function ID"))?;
        if self.item(&root)?.is_none() {
            return Err(RpcError::invalid("function has no source location"));
        }
        let mut groups: BTreeMap<FnId, Vec<trace_db::EditorCall>> = BTreeMap::new();
        let calls = editor_calls(&self.conn, root.id, direction)?;
        // Fetch each peer once, not once per call site. Identity is the DB ID,
        // never a name or URI supplied by the client.
        for call in calls {
            groups.entry(call.peer).or_default().push(call);
        }
        let mut results = Vec::new();
        for (peer_id, sites) in groups {
            let mut ranges = BTreeSet::new();
            let peer = editor_function(&self.conn, peer_id)?
                .context("call edge references missing function")?;
            let Some(peer_item) = self.item(&peer)? else {
                continue;
            };
            let caller = match direction {
                Direction::Down => &root,
                Direction::Up => &peer,
            };
            let caller_path = self
                .local_paths
                .get(&caller.path)
                .context("caller file missing")?
                .clone();
            for call in sites {
                if let Some(line) = call.line.filter(|l| *l > 0) {
                    let position = self.source(&caller_path).map_or(
                        Position {
                            line: line - 1,
                            character: 0,
                        },
                        |source| source.position(line, call.col.unwrap_or(1)),
                    );
                    ranges.insert(Range::point(position));
                }
            }
            results.push(match direction {
                Direction::Down => json!({"to": peer_item, "fromRanges": ranges}),
                Direction::Up => json!({"from": peer_item, "fromRanges": ranges}),
            });
        }
        Ok(json!(results))
    }

    /// Handle one request; unknown notifications are ignored by the transport.
    pub fn request(&mut self, method: &str, params: Value) -> std::result::Result<Value, RpcError> {
        if self.state == State::Shutdown {
            return Err(RpcError::new(-32600, "server has shut down"));
        }
        if method == "initialize" {
            if self.state != State::New {
                return Err(RpcError::new(-32600, "server already initialized"));
            }
            if !params.is_object() {
                return Err(RpcError::invalid("initialize params must be an object"));
            }
            self.state = State::Initialized;
            return Ok(
                json!({"capabilities": {"callHierarchyProvider": true, "positionEncoding": "utf-16"},
                "serverInfo": {"name": "trace-lsp", "version": env!("CARGO_PKG_VERSION")}}),
            );
        }
        if self.state == State::New {
            return Err(RpcError::new(-32002, "server not initialized"));
        }
        match method {
            "shutdown" => {
                if !params.is_null() {
                    return Err(RpcError::invalid("shutdown takes no params"));
                }
                self.state = State::Shutdown;
                Ok(Value::Null)
            }
            "textDocument/prepareCallHierarchy" => {
                #[derive(Deserialize)]
                struct Document {
                    uri: String,
                }
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Prepare {
                    text_document: Document,
                    position: Position,
                }
                let p: Prepare =
                    serde_json::from_value(params).map_err(|e| RpcError::invalid(e.to_string()))?;
                self.prepare(&p.text_document.uri, p.position)
            }
            "callHierarchy/incomingCalls" | "callHierarchy/outgoingCalls" => {
                let item = params
                    .get("item")
                    .ok_or_else(|| RpcError::invalid("missing item"))?
                    .clone();
                self.hierarchy(
                    item,
                    if method == "callHierarchy/incomingCalls" {
                        Direction::Up
                    } else {
                        Direction::Down
                    },
                )
            }
            _ => Err(RpcError::new(
                -32601,
                format!("unsupported request: {method}"),
            )),
        }
    }

    pub fn shutdown_requested(&self) -> bool {
        self.state == State::Shutdown
    }
}

/// Relative file paths have no per-run ownership in the existing schema.
/// Accept only one unambiguous source root; merged metadata lists input DBs.
fn relative_path_base(conn: &Connection, database: &Path) -> Result<PathBuf> {
    let database_path = database.canonicalize()?;
    let directory = database_path.parent().context("database has no parent")?;
    let mut roots = BTreeSet::new();
    let mut stmt =
        conn.prepare("SELECT target_root, options_json FROM analysis_run ORDER BY id")?;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (root, options) = row?;
        let options: Value = serde_json::from_str(&options)
            .context("invalid analysis metadata for relative source paths")?;
        if options.get("stage").and_then(Value::as_str) == Some("merge") {
            bail!("cannot locate relative source paths in a merged database: target_root lists input databases, not a source root; regenerate with absolute source paths");
        }
        if root.is_empty() {
            bail!("cannot locate relative source paths: analysis_run has an empty target_root");
        }
        roots.insert(normalize(&directory.join(root)));
    }
    if roots.len() != 1 {
        bail!("cannot locate relative source paths: analysis_run has multiple source roots and files have no per-run ownership; regenerate with absolute source paths");
    }
    Ok(roots.into_iter().next().unwrap())
}
