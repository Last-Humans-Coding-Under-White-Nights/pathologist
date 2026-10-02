//! Source-level view of the exported may-flow graph. The raw query API stays
//! available for consumers that need PAG nodes and constraint kinds.
use crate::{basename, Direction, GraphMeta, RenderFormat, SymbolRef};
use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write;
use trace_ir::{CallSiteId, FnId, PagNodeId, TargetId, VarId};

#[path = "dataflow_json.rs"]
mod json;

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlowSite {
    pub path: String,
    pub line: i64,
    pub col: i64,
}
impl FlowSite {
    fn display(&self) -> String {
        if self.line == 0 {
            return String::new();
        }
        if self.col > 0 {
            format!("{}:{}:{}", basename(&self.path), self.line, self.col)
        } else {
            format!("{}:{}", basename(&self.path), self.line)
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct FlowParameter {
    pub arg_index: u32,
    pub var_id: VarId,
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowScope {
    pub fn_id: Option<FnId>,
    pub name: String,
    pub location: FlowSite,
    pub signature: String,
    pub target_id: Option<TargetId>,
    /// Header metadata only; parameters outside the graph are not graph nodes.
    #[serde(skip)]
    pub parameters: Vec<FlowParameter>,
}
impl FlowScope {
    fn text_display(&self) -> String {
        let Some(id) = self.fn_id else {
            return self.display();
        };
        let parameters = if self.parameters.is_empty() && !self.signature.ends_with("()") {
            "parameters unavailable".into()
        } else {
            self.parameters
                .iter()
                .map(|p| {
                    format!(
                        "argument {}: {}: {} [var#{}]",
                        p.arg_index + 1,
                        p.name,
                        p.type_name,
                        p.var_id.0
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let location = if self.location.line == 0 {
            " [external]".into()
        } else {
            format!(" — {}", self.location.display())
        };
        format!("{} [fn#{}] ({parameters}){location}", self.name, id.0)
    }
    fn qualified(&self) -> String {
        match self.fn_id {
            Some(id) => format!("{} [fn#{}]", self.name, id.0),
            None => self.display(),
        }
    }
    fn display(&self) -> String {
        match self.fn_id {
            Some(id) if self.location.line == 0 => {
                format!("{} [fn#{}] [external]", self.name, id.0)
            }
            Some(id) => format!("{} [fn#{}] — {}", self.name, id.0, self.location.display()),
            None if self.location.path.is_empty() => self.name.clone(),
            None => format!("{} — {}", self.name, basename(&self.location.path)),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct FlowCallee {
    pub fn_id: FnId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowEntity {
    pub id: PagNodeId,
    pub var_id: Option<VarId>,
    pub fn_id: Option<FnId>,
    pub name: String,
    pub kind: String,
    pub scope: String,
    pub location: FlowSite,
    pub depth: u32,
    #[serde(skip_serializing)]
    pub call_site_id: Option<CallSiteId>,
    pub callees: Vec<FnId>,
    /// Complete structured targets; display labels never contain target lists.
    pub targets: Vec<FlowCallee>,
    pub spelling: Option<FlowSite>,
}
impl FlowEntity {
    fn text_display(&self) -> String {
        self.display()
    }
    fn display(&self) -> String {
        match (self.var_id, self.fn_id, self.kind.as_str()) {
            (Some(id), _, "variable" | "parameter") => format!("{} [var#{}]", self.name, id.0),
            (_, Some(id), "function") => format!("{} [fn#{}]", self.name, id.0),
            _ => self.name.clone(),
        }
    }
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlowOperationSite {
    pub operation: String,
    pub location: FlowSite,
    pub expression: String,
    /// Captured on the original call occurrence, before hidden-node collapse.
    #[serde(skip)]
    pub arg_index: Option<i64>,
    /// Callee of this original operation, retained across hidden-node collapse.
    #[serde(skip)]
    pub callee_fn_id: Option<FnId>,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct FlowStep {
    pub from: PagNodeId,
    pub to: PagNodeId,
    pub operations: Vec<String>,
    /// Each recorded operation survives collapsing, including distinct sites.
    pub provenance: Vec<FlowOperationSite>,
    pub location: FlowSite,
    pub expression: String,
    #[serde(skip_serializing)]
    pub call_site_id: Option<CallSiteId>,
    pub arg_index: Option<i64>,
    pub callee_fn_id: Option<FnId>,
    pub spelling: Option<FlowSite>,
    pub scope: String,
}
#[derive(Debug, Default, Serialize)]
pub struct DataflowView {
    #[serde(skip)]
    json_metadata: json::Metadata,
    pub scopes: BTreeMap<String, FlowScope>,
    pub nodes: BTreeMap<PagNodeId, FlowEntity>,
    pub edges: Vec<FlowStep>,
    pub truncated: bool,
}
fn operation(kind: &str) -> &str {
    match kind {
        "copy" => "assign",
        "load" => "read pointer",
        "store" => "write pointer",
        "gep" => "access field",
        "addr_of" => "take address",
        "call_arg" => "pass argument",
        "unwrap" => "unwrap pointer",
        "terminates" => "clear value",
        "dlsym" => "resolve function",
        _ => kind,
    }
}

/// Require an analysis export with the structures used by the source-level view.
pub fn require_source_dataflow_metadata(conn: &Connection) -> Result<()> {
    // Legacy/scratch databases may have no run metadata; retain their
    // actionable structural diagnostics instead of failing on this query.
    let options: Option<String> =
        if crate::inspect::column_exists(conn, "analysis_run", "options_json")? {
            conn.query_row(
                "SELECT options_json FROM analysis_run ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
        } else {
            None
        };
    if let Some(options) = options {
        let metadata: serde_json::Value = serde_json::from_str(&options)?;
        if metadata["stage"] == "merge" {
            bail!(
                "source-level dataflow is unavailable in trace-merge output: the merger \
                 preserves call graphs but does not merge PAG flow graphs or provenance; \
                 inspect an original trace analyze database, or run trace analyze on \
                 the combined source tree to obtain a database supporting dataflow"
            );
        }
    }
    crate::inspect::require_synthetic_metadata(conn)?;
    let has_metadata: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_origins') AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_calls') AND EXISTS(SELECT 1 FROM pragma_table_info('flow_nodes') WHERE name='call_site_id') AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_return_calls') AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_field_access') AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_field_locations')", [], |r|r.get(0)
    )?;
    if !has_metadata {
        bail!("source-level dataflow requires provenance metadata; re-run trace analyze");
    }
    Ok(())
}

/// Collapse technical nodes before counting visible BFS steps. Every original
/// edge remains oriented source-to-destination in either traversal direction.
fn call_scope(
    conn: &Connection,
    view: &mut DataflowView,
    caller: Option<i64>,
    location: &FlowSite,
) -> Result<String> {
    if let Some(caller) = caller {
        let key = format!("fn:{caller}");
        if view.scopes.contains_key(&key) {
            return Ok(key);
        }
        // A function assigning a global can have no owned nodes in this
        // neighborhood. Load its header once by identity, not once per call.
        let scope = conn.query_row("SELECT f.name,p.path,f.line_start,f.signature,f.target_id FROM functions f JOIN files p ON p.id=f.file_id WHERE f.id=?1", [caller], |row| Ok(FlowScope {
            fn_id: Some(FnId(caller as u32)), name: row.get(0)?,
            location: FlowSite { path: row.get(1)?, line: row.get(2)?, col: 0 },
            signature: row.get(3)?, target_id: row.get::<_, Option<u32>>(4)?.map(TargetId), parameters: Vec::new(),
        })).optional()?;
        if let Some(scope) = scope {
            view.scopes.insert(key.clone(), scope);
            return Ok(key);
        }
    }
    let key = format!("global:{}", location.path);
    view.scopes.entry(key.clone()).or_insert_with(|| FlowScope {
        fn_id: None,
        name: "global".into(),
        location: FlowSite {
            path: location.path.clone(),
            line: 0,
            col: 0,
        },
        signature: String::new(),
        target_id: None,
        parameters: Vec::new(),
    });
    Ok(key)
}

/// A zero/one-cost walk: technical nodes cost zero, source entities cost one.
/// The extra visible frontier is needed to distinguish truncation from cycles.
#[derive(Default)]
struct Neighborhood {
    nodes: BTreeSet<i64>,
    metadata: BTreeSet<i64>,
    order: Vec<(i64, u32)>,
    edges: Vec<(i64, i64, String)>,
    aliases: BTreeMap<i64, i64>,
}

fn visible_neighborhood(
    conn: &Connection,
    symbols: &[SymbolRef],
    dir: Direction,
    depth: u32,
) -> Result<Neighborhood> {
    let starts = crate::inspect::dataflow_starts(conn, symbols)?;
    let mut result = Neighborhood::default();
    let mut levels = BTreeMap::<i64, u32>::new();
    let mut queue = VecDeque::new();
    let mut technical = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut alias_siblings = BTreeMap::<i64, BTreeSet<i64>>::new();
    let mut node = conn.prepare("SELECT n.kind,n.detail,n.var_id,COALESCE(v.is_synthetic,0) FROM flow_nodes n LEFT JOIN variables v ON v.id=n.var_id WHERE n.id=?1")?;
    let mut twins =
        conn.prepare("SELECT id,kind,detail FROM flow_nodes WHERE var_id=?1 ORDER BY id")?;
    let mut writes = conn.prepare("SELECT expression,operation FROM flow_origins WHERE dst_node=?1 ORDER BY src_node,kind,file_id,line,col")?;
    let mut parents = conn.prepare(
        "SELECT base_node FROM flow_field_access WHERE dst_node=?1 ORDER BY base_node,field_name",
    )?;
    let mut storage_parent =
        conn.prepare("SELECT parent_node FROM flow_field_locations WHERE node_id=?1")?;
    let key = if dir == Direction::Down {
        "src_node"
    } else {
        "dst_node"
    };
    let mut edges = conn.prepare(&format!("SELECT src_node,dst_node,kind FROM flow_edges WHERE {key}=?1 AND kind!='points_to' ORDER BY src_node,dst_node,kind"))?;
    for id in &starts {
        levels.insert(*id, 0);
        queue.push_back(*id);
        result.order.push((*id, 0));
    }
    while let Some(id) = queue.pop_front() {
        let level = levels[&id];
        result.nodes.insert(id);
        // Load the classification and its dependencies once. Parent metadata
        // is followed independently from edges; incoming stores are never
        // introduced merely to classify an expression from its field base.
        let mut dependencies = VecDeque::from([id]);
        let mut new_dependencies = Vec::new();
        while let Some(dep) = dependencies.pop_front() {
            if !result.metadata.insert(dep) {
                continue;
            }
            new_dependencies.push(dep);
            let info = node
                .query_row([dep], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                })
                .optional()?;
            let Some((kind, detail, var, synthetic)) = info else {
                continue;
            };
            let mut hidden =
                synthetic || matches!(detail.as_str(), "field_summary" | "array_summary");
            if let Some(var) = var {
                let siblings = twins
                    .query_map([var], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let canonical = siblings
                    .iter()
                    .rev()
                    .find(|(_, k, _)| k == "var")
                    .map(|(id, _, _)| *id);
                for (sibling, k, d) in siblings {
                    if k == "var"
                        || (k == "loc"
                            && matches!(
                                d.as_str(),
                                "global" | "file_static" | "fn_static" | "local"
                            ))
                    {
                        dependencies.push_back(sibling);
                        if let Some(canonical) = canonical {
                            aliases.insert(sibling, canonical);
                            alias_siblings.entry(canonical).or_default().insert(sibling);
                        }
                    }
                }
                if kind == "loc"
                    && matches!(
                        detail.as_str(),
                        "global" | "file_static" | "fn_static" | "local"
                    )
                    && canonical.is_none()
                {
                    hidden = true;
                }
            }
            let bases = parents
                .query_map([dep], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            dependencies.extend(bases.iter().copied());
            if let Some(parent) = storage_parent
                .query_row([dep], |r| r.get::<_, i64>(0))
                .optional()?
            {
                dependencies.push_back(parent);
            }
            if hidden {
                for row in writes.query_map([dep], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })? {
                    let (expression, op) = row?;
                    if matches!(operation(&op), "write field" | "write pointer")
                        && assignment_lhs(&expression).is_some()
                    {
                        hidden = false;
                        break;
                    }
                    // Aggregate writes need a source-rooted field path below.
                }
            }
            technical.insert(dep, hidden);
        }
        // Classify aliases together: an origin may name the storage twin
        // while the traversal reaches its canonical value node first.
        for dep in new_dependencies {
            let mut current = dep;
            let mut seen = BTreeSet::new();
            let mut has_field = false;
            while seen.insert(current) {
                let base = parents
                    .query_row([current], |r| r.get::<_, i64>(0))
                    .optional()?;
                let Some(base) = base else {
                    break;
                };
                has_field = true;
                current = *aliases.get(&base).unwrap_or(&base);
            }
            if has_field && !technical.get(&current).copied().unwrap_or(true) {
                let initializer = writes
                    .query_map([dep], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .iter()
                    .any(|(x, op)| {
                        op == "write field"
                            && (x.trim().starts_with('{') || assignment_lhs(x).is_none())
                    });
                if initializer {
                    technical.insert(dep, false);
                }
            }
            if !technical.get(&dep).copied().unwrap_or(false) {
                if let Some(canonical) = aliases.get(&dep) {
                    for sibling in &alias_siblings[canonical] {
                        technical.insert(*sibling, false);
                    }
                }
            }
        }
        let canonical = *aliases.get(&id).unwrap_or(&id);
        // Alias siblings are the same visible entity and cost no depth.
        let siblings: Vec<_> = alias_siblings
            .get(&canonical)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for sibling in siblings {
            if levels.get(&sibling).is_none_or(|old| *old > level) {
                levels.insert(sibling, level);
                queue.push_front(sibling);
            }
        }
        // The node's own cost is charged on expansion, so classification can
        // use all its incoming provenance before deciding whether to stop.
        let cost = u32::from(
            !technical.get(&id).copied().unwrap_or(false)
                && !starts
                    .iter()
                    .any(|root| *aliases.get(root).unwrap_or(root) == canonical),
        );
        let next_level = level.saturating_add(cost);
        if next_level > depth {
            continue;
        }
        for row in edges.query_map([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (s, d, k) = row?;
            let next = if dir == Direction::Down { d } else { s };
            result.edges.push(if dir == Direction::Down {
                (s, d, k)
            } else {
                (d, s, k)
            });
            if levels.get(&next).is_none_or(|old| *old > next_level) {
                levels.insert(next, next_level);
                queue.push_back(next);
            }
        }
    }
    result.edges.sort();
    result.edges.dedup();
    result.aliases = aliases;
    Ok(result)
}

pub fn dataflow_view(
    conn: &Connection,
    symbols: &[SymbolRef],
    dir: Direction,
    depth: u32,
) -> Result<DataflowView> {
    require_source_dataflow_metadata(conn)?;
    let raw = visible_neighborhood(conn, symbols, dir, depth)?;
    // IDs come exclusively from SQLite integer columns. Metadata dependencies
    // (aliases and field parents) are included without adding traversal edges.
    let ids = raw
        .metadata
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let filter = |sql: &str, predicate: &str| {
        let (body, order) = sql.split_once(" ORDER BY ").unwrap();
        format!("{body} WHERE {predicate} ORDER BY {order}")
    };
    let selected = |sql: &str, column: &str| filter(sql, &format!("{column} IN ({ids})"));
    let reached_ids = raw
        .nodes
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let expanded_ids = raw
        .edges
        .iter()
        .map(|edge| edge.0)
        .collect::<BTreeSet<_>>()
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let occurrences = |sql: &str, table: &str| {
        let (start, end) = if dir == Direction::Down {
            ("src_node", "dst_node")
        } else {
            ("dst_node", "src_node")
        };
        filter(
            sql,
            &format!("{table}.{start} IN ({expanded_ids}) AND {table}.{end} IN ({reached_ids})"),
        )
    };
    let mut view = DataflowView::default();
    let mut entities = BTreeMap::new();
    let aliases = &raw.aliases;
    let mut hidden = BTreeSet::new();
    let mut stmt = conn.prepare(&selected("SELECT n.id,n.kind,n.label,n.detail,n.var_id,COALESCE(v.fn_id,n.fn_id),v.name,v.kind,v.is_synthetic,p.path,v.line,v.col,f.name,fp.path,f.line_start,f.signature,f.target_id FROM flow_nodes n LEFT JOIN variables v ON v.id=n.var_id LEFT JOIN files p ON p.id=v.file_id LEFT JOIN functions f ON f.id=COALESCE(v.fn_id,n.fn_id) LEFT JOIN files fp ON fp.id=f.file_id ORDER BY n.id", "n.id"))?;
    let rows = stmt.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let kind: String = r.get(1)?;
        let label: String = r.get(2)?;
        let detail: String = r.get(3)?;
        let vid: Option<i64> = r.get(4)?;
        let fid: Option<i64> = r.get(5)?;
        let name: Option<String> = r.get(6)?;
        let storage: Option<String> = r.get(7)?;
        let synthetic: Option<bool> = r.get(8)?;
        let function_entity = kind == "loc" && detail == "function";
        let path = r
            .get::<_, Option<String>>(if function_entity { 13 } else { 9 })?
            .unwrap_or_default();
        let line = r
            .get::<_, Option<i64>>(if function_entity { 14 } else { 10 })?
            .unwrap_or(0);
        let col = r.get::<_, Option<i64>>(11)?.unwrap_or(0);
        let fn_name: Option<String> = r.get(12)?;
        let owner = fid;
        let storage_scope = if storage.as_deref() == Some("local") && owner.is_none() {
            "global"
        } else {
            storage.as_deref().unwrap_or("values")
        };
        let scope = if let Some(f) = owner {
            format!("fn:{f}")
        } else {
            format!("{storage_scope}:{path}")
        };
        let fs = FlowScope {
            fn_id: owner.map(|id| FnId(id as u32)),
            name: fn_name.clone().unwrap_or_else(|| storage_scope.to_owned()),
            location: FlowSite {
                path: r
                    .get::<_, Option<String>>(13)?
                    .unwrap_or_else(|| path.clone()),
                line: r.get::<_, Option<i64>>(14)?.unwrap_or(0),
                col: 0,
            },
            signature: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
            target_id: r.get::<_, Option<u32>>(16)?.map(TargetId),
            parameters: Vec::new(),
        };
        let e = FlowEntity {
            id: PagNodeId(id as u32),
            var_id: vid.map(|id| VarId(id as u32)),
            fn_id: owner.map(|id| FnId(id as u32)),
            name: if detail == "function" {
                fn_name.clone().unwrap_or(label)
            } else if detail == "field" {
                format!("field {label}")
            } else {
                name.unwrap_or(label)
            },
            kind: if kind == "var" {
                if storage.as_deref() == Some("param") {
                    "parameter"
                } else {
                    "variable"
                }
            } else if detail == "heap" {
                "allocation"
            } else if detail == "null_pointer" {
                "null_pointer"
            } else if detail == "string_lit" {
                "constant"
            } else if detail == "function" {
                "function"
            } else {
                kind.as_str()
            }
            .into(),
            scope,
            location: FlowSite { path, line, col },
            depth: 0,
            call_site_id: None,
            callees: Vec::new(),
            targets: Vec::new(),
            spelling: None,
        };
        Ok((e, fs, synthetic.unwrap_or(false), kind, detail))
    })?;
    let metadata = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    for (e, fs, syn, kind, detail) in metadata {
        if kind == "loc"
            && matches!(
                detail.as_str(),
                "global" | "file_static" | "fn_static" | "local"
            )
            && !aliases.contains_key(&(e.id.0 as i64))
        {
            hidden.insert(e.id.0 as i64);
        }
        if syn || matches!(detail.as_str(), "field_summary" | "array_summary") {
            hidden.insert(e.id.0 as i64);
        }
        view.scopes.entry(e.scope.clone()).or_insert(fs);
        entities.insert(e.id.0 as i64, e);
    }
    let mut stmt=conn.prepare(&selected("SELECT n.id,cs.id,cs.is_direct,p.path,cs.line,cs.col,ep.path,cs.expansion_line,cs.expansion_col FROM flow_nodes n JOIN call_sites cs ON cs.id=n.call_site_id JOIN files p ON p.id=cs.file_id LEFT JOIN files ep ON ep.id=cs.expansion_file_id ORDER BY n.id", "n.id"))?;
    // Load only existing resolutions in one pass. Terminators may share a
    // call site with a target, but sites without edges need no separate query.
    let mut targets = conn.prepare(&selected("SELECT DISTINCT n.id,f.id,f.name FROM flow_nodes n JOIN call_edges e ON e.call_site_id=n.call_site_id JOIN functions f ON f.id=e.callee_fn_id ORDER BY n.id,f.id", "n.id"))?;
    for target in targets.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (node, id, name) = target?;
        if let Some(entity) = entities.get_mut(&node) {
            entity.callees.push(FnId(id));
            entity.targets.push(FlowCallee {
                fn_id: FnId(id),
                name,
            });
        }
    }
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, bool>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<i64>>(7)?,
            r.get::<_, Option<i64>>(8)?,
        ))
    })? {
        let (id, cs, direct, path, line, col, ep, el, ec) = row?;
        if let Some(entity) = entities.get_mut(&id) {
            entity.call_site_id = Some(CallSiteId(cs));
            entity.spelling = ep.as_ref().map(|_| FlowSite {
                path: path.clone(),
                line,
                col,
            });
            entity.location = if let Some(path) = ep {
                FlowSite {
                    path,
                    line: el.unwrap_or(0),
                    col: ec.unwrap_or(0),
                }
            } else {
                FlowSite { path, line, col }
            };
            if entity.kind == "call_target" {
                entity.name = format!(
                    "{}call {}",
                    if direct { "" } else { "indirect " },
                    entity.name
                );
            }
        }
    }
    let mut field_locations = BTreeMap::new();
    let mut stmt = conn.prepare(&selected(
        "SELECT node_id,parent_node,field_name FROM flow_field_locations ORDER BY node_id",
        "node_id",
    ))?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (node, parent, field) = row?;
        field_locations.insert(node, (*aliases.get(&parent).unwrap_or(&parent), field));
    }
    for &node in field_locations.keys() {
        if let Some(path) = field_destination(node, &field_locations, &entities, &hidden) {
            if let Some(entity) = entities.get_mut(&node) {
                entity.name = format!("field {path} (abstract field storage [node#{node}])");
            }
        }
    }
    for &node in field_locations.keys() {
        let mut parent = node;
        let mut seen = BTreeSet::new();
        while let Some((next, _)) = field_locations.get(&parent) {
            if !seen.insert(parent) {
                break;
            }
            parent = *next;
        }
        if let Some(owner) = entities.get(&parent).cloned() {
            if let Some(entity) = entities.get_mut(&node) {
                entity.scope = owner.scope;
                entity.fn_id = owner.fn_id;
                entity.location = owner.location;
            }
        }
    }
    let canonical = |id: i64| *aliases.get(&id).unwrap_or(&id);
    let mut steps = Vec::new();
    let mut origins = BTreeMap::<(i64, i64, String), Vec<(FlowSite, String, String)>>::new();
    let write_ids = raw
        .metadata
        .iter()
        .filter(|id| hidden.contains(&canonical(**id)))
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt=conn.prepare(&filter("SELECT o.src_node,o.dst_node,o.kind,p.path,o.line,o.col,o.expression,o.operation FROM flow_origins o JOIN files p ON p.id=o.file_id ORDER BY o.src_node,o.dst_node,o.kind,p.path,o.line,o.col", &format!("o.dst_node IN ({ids}) AND (o.src_node IN ({reached_ids}) OR (o.operation IN ('write field','write pointer','store') AND o.dst_node IN ({write_ids})))")))?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            FlowSite {
                path: r.get(3)?,
                line: r.get(4)?,
                col: r.get(5)?,
            },
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
        ))
    })? {
        let (s, d, k, l, x, op) = row?;
        origins.entry((s, d, k)).or_default().push((l, x, op));
    }
    let mut field_access = BTreeMap::new();
    let mut stmt = conn.prepare(&selected("SELECT dst_node,base_node,field_name FROM flow_field_access ORDER BY dst_node,base_node,field_name", "dst_node"))?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (dst, base, name) = row?;
        let (dst, base) = (canonical(dst), canonical(base));
        // A twin-to-twin access becomes a self reference after projection.
        // It cannot contribute a field path; keep the deterministic smallest
        // genuine parent if several raw records converge on one destination.
        if dst != base {
            let candidate = (base, name);
            field_access
                .entry(dst)
                .and_modify(|parent| {
                    if candidate < *parent {
                        *parent = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
    }
    // Visibility is a property of the recorded field write, not of the root.
    // A base reaches the GEP destination without traversing its incoming Store.
    // Classify encountered destinations from indexed incoming metadata before projection.
    for ((_, dst, _), sites) in &origins {
        for (location, expression, op) in sites {
            if matches!(operation(op), "write field" | "write pointer") {
                expose_write_expression(
                    canonical(*dst),
                    op == "write field",
                    expression,
                    location,
                    &field_access,
                    &mut entities,
                    &mut hidden,
                );
            }
        }
    }
    // Reuse canonical parameter-twin and root selection from the raw API.
    for edge in &raw.edges {
        if edge.2 == "points_to" {
            continue;
        }
        let (s, d) = if dir == Direction::Down {
            (edge.0, edge.1)
        } else {
            (edge.1, edge.0)
        };
        let scope = entities
            .get(&d)
            .map(|e| e.scope.clone())
            .unwrap_or_default();
        let sites = origins
            .get(&(s, d, edge.2.clone()))
            .cloned()
            .unwrap_or_else(|| vec![(FlowSite::default(), String::new(), edge.2.clone())]);
        for (location, expression, op) in sites {
            steps.push(FlowStep {
                from: PagNodeId(canonical(s) as u32),
                to: PagNodeId(canonical(d) as u32),
                operations: vec![operation(&op).into()],
                provenance: vec![FlowOperationSite {
                    callee_fn_id: None,
                    arg_index: None,
                    operation: operation(&op).into(),
                    location: location.clone(),
                    expression: expression.clone(),
                }],
                location,
                expression,
                call_site_id: None,
                arg_index: None,
                callee_fn_id: None,
                spelling: None,
                scope: scope.clone(),
            });
        }
    }
    // Restore call occurrence identity and parameter position, even where the
    // solver exported a generic copy for the same pair. Recorded source
    // operations can share those endpoints (e.g. recursive calls); keep them.
    let mut argument_pairs = BTreeSet::new();
    let mut argument_steps = Vec::new();
    let has_call_expressions: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_call_expressions')",
        [], |r| r.get(0),
    )?;
    let (expression_column, expression_join) = if has_call_expressions {
        (
            "x.expression",
            "LEFT JOIN flow_call_expressions x ON x.call_site_id=a.call_site_id",
        )
    } else {
        ("NULL", "")
    };
    let mut stmt = conn.prepare(&occurrences(&format!("SELECT a.call_site_id,a.arg_index,a.src_node,a.dst_node,cs.caller_fn_id,p.path,cs.line,cs.col,ep.path,cs.expansion_line,cs.expansion_col,cs.is_direct,{expression_column} FROM flow_calls a JOIN call_sites cs ON cs.id=a.call_site_id JOIN files p ON p.id=cs.file_id LEFT JOIN files ep ON ep.id=cs.expansion_file_id {expression_join} ORDER BY a.call_site_id,a.arg_index,a.src_node,a.dst_node"), "a"))?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<i64>>(4)?,
            FlowSite {
                path: r.get(5)?,
                line: r.get(6)?,
                col: r.get(7)?,
            },
            r.get::<_, Option<String>>(8)?,
            r.get::<_, Option<i64>>(9)?,
            r.get::<_, Option<i64>>(10)?,
            r.get::<_, bool>(11)?,
            r.get::<_, Option<String>>(12)?.unwrap_or_default(),
        ))
    })? {
        let (cs, arg, s, d, caller, sp, ep, el, ec, direct, expression) = row?;
        if !raw.nodes.contains(&s) || !raw.nodes.contains(&d) {
            continue;
        }
        argument_pairs.insert((
            PagNodeId(canonical(s) as u32),
            PagNodeId(canonical(d) as u32),
        ));
        let location = if let Some(path) = ep {
            FlowSite {
                path,
                line: el.unwrap_or(0),
                col: ec.unwrap_or(0),
            }
        } else {
            sp.clone()
        };
        argument_steps.push(FlowStep {
            from: PagNodeId(canonical(s) as u32),
            to: PagNodeId(canonical(d) as u32),
            operations: vec![if direct {
                "pass argument"
            } else {
                "pass argument via indirect call"
            }
            .into()],
            provenance: vec![FlowOperationSite {
                callee_fn_id: entities.get(&d).and_then(|e| e.fn_id),
                arg_index: Some(arg),
                operation: if direct {
                    "pass argument"
                } else {
                    "pass argument via indirect call"
                }
                .into(),
                location: location.clone(),
                expression: expression.clone(),
            }],
            location: location.clone(),
            expression,
            call_site_id: Some(CallSiteId(cs as u32)),
            arg_index: Some(arg),
            callee_fn_id: entities.get(&d).and_then(|e| e.fn_id),
            spelling: if location != sp { Some(sp) } else { None },
            scope: call_scope(conn, &mut view, caller, &location)?,
        });
    }
    // Filter generic wiring once, before appending call occurrences. Repeated
    // calls must not rescan the growing list of annotated transitions.
    steps.retain(|e| {
        !(argument_pairs.contains(&(e.from, e.to))
            && e.call_site_id.is_none()
            && !e.provenance.iter().any(|origin| origin.location.line > 0))
    });
    steps.extend(argument_steps);
    // Recorded origins may repeat a return endpoint pair for every occurrence.
    // Call identity already comes from flow_return_calls; index endpoints once
    // rather than cloning every origin for each call and deduplicating later.
    let return_pairs: BTreeSet<_> = steps
        .iter()
        .filter(|e| e.operations.iter().any(|op| op == "return value"))
        .map(|e| (e.from, e.to))
        .collect();
    // Index source operations by endpoint pair and file, then select
    // the closest preceding operation position for each call occurrence.
    // Calls on the same line keep their own assignment expressions.
    let mut return_origins = BTreeMap::new();
    for edge in &steps {
        if edge.operations.iter().any(|op| op == "return value") && !edge.expression.is_empty() {
            return_origins
                .entry((edge.from, edge.to, edge.location.path.clone()))
                .or_insert_with(BTreeMap::new)
                .entry((edge.location.line, edge.location.col))
                .or_insert(edge);
        }
    }
    let mut replaced_return_origins = BTreeMap::<_, BTreeSet<_>>::new();
    let mut return_occurrences = BTreeSet::new();
    let mut return_steps = Vec::new();
    // Optional additive v7 metadata: older databases retain the indexed
    // endpoint/source-position fallback, but new exports bind exact call IDs.
    let has_call_origins: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_call_origins')",
        [], |r| r.get(0),
    )?;
    let mut call_origins = has_call_origins.then(|| conn.prepare(
        "SELECT p.path,o.line,o.col,o.expression FROM flow_call_origins o JOIN files p ON p.id=o.file_id WHERE o.call_site_id=?1",
    )).transpose()?;
    let mut stmt=conn.prepare(&occurrences("SELECT r.src_node,r.dst_node,cs.id,cs.caller_fn_id,p.path,cs.line,cs.col,ep.path,cs.expansion_line,cs.expansion_col,f.id,f.name,fp.path,f.line_start,f.signature,f.target_id FROM flow_return_calls r JOIN call_sites cs ON cs.id=r.call_site_id JOIN files p ON p.id=cs.file_id LEFT JOIN files ep ON ep.id=cs.expansion_file_id JOIN functions f ON f.id=r.callee_fn_id JOIN files fp ON fp.id=f.file_id ORDER BY r.src_node,r.dst_node,cs.id,f.id", "r"))?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, u32>(2)?,
            r.get::<_, Option<i64>>(3)?,
            FlowSite {
                path: r.get(4)?,
                line: r.get(5)?,
                col: r.get(6)?,
            },
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<i64>>(8)?,
            r.get::<_, Option<i64>>(9)?,
            FlowScope {
                fn_id: Some(FnId(r.get(10)?)),
                name: r.get(11)?,
                location: FlowSite {
                    path: r.get(12)?,
                    line: r.get(13)?,
                    col: 0,
                },
                signature: r.get(14)?,
                target_id: r.get::<_, Option<u32>>(15)?.map(TargetId),
                parameters: Vec::new(),
            },
        ))
    })? {
        let (src, dst, cs, caller, sp, ep, el, ec, scope) = row?;
        let pair = (
            PagNodeId(canonical(src) as u32),
            PagNodeId(canonical(dst) as u32),
        );
        let callee = scope.fn_id.unwrap();
        if !return_pairs.contains(&pair)
            || !return_occurrences.insert((pair, CallSiteId(cs), callee))
        {
            continue;
        }
        view.scopes
            .entry(format!("fn:{}", callee.0))
            .or_insert(scope);
        let location = if let Some(path) = ep {
            FlowSite {
                path,
                line: el.unwrap_or(0),
                col: ec.unwrap_or(0),
            }
        } else {
            sp.clone()
        };
        let origin = return_origins
            .get(&(pair.0, pair.1, location.path.clone()))
            .and_then(|sites| {
                sites
                    .range(..=(location.line, location.col))
                    .next_back()
                    .map(|(_, edge)| edge)
            });
        let bound_origin = if let Some(stmt) = &mut call_origins {
            stmt.query_row([cs], |r| {
                Ok(FlowOperationSite {
                    callee_fn_id: None,
                    arg_index: None,
                    operation: "return value".into(),
                    location: FlowSite {
                        path: r.get(0)?,
                        line: r.get(1)?,
                        col: r.get(2)?,
                    },
                    expression: r.get(3)?,
                })
            })
            .optional()?
        } else {
            None
        };
        // Lowering can attribute the constraint to the call expression while
        // the bound occurrence records its enclosing assignment. Both are
        // replaced by this occurrence, but a preceding initializer is not.
        if let Some(origin) = origin.filter(|edge| edge.location == location) {
            replaced_return_origins
                .entry(pair)
                .or_default()
                .extend(origin.provenance.iter().cloned());
        }
        let (expression, mut provenance) = if let Some(origin) = bound_origin {
            (origin.expression.clone(), vec![origin])
        } else {
            let expression = origin
                .map(|edge| edge.expression.clone())
                .unwrap_or_default();
            let provenance = origin
                .map(|edge| edge.provenance.clone())
                .unwrap_or_else(|| {
                    vec![FlowOperationSite {
                        callee_fn_id: None,
                        arg_index: None,
                        operation: "return value".into(),
                        location: location.clone(),
                        expression: expression.clone(),
                    }]
                });
            (expression, provenance)
        };
        for origin in &mut provenance {
            if origin.operation == "return value" {
                replaced_return_origins
                    .entry(pair)
                    .or_default()
                    .insert(origin.clone());
                origin.callee_fn_id = Some(callee);
            }
        }
        return_steps.push(FlowStep {
            from: pair.0,
            to: pair.1,
            operations: vec!["return value".into()],
            call_site_id: Some(CallSiteId(cs)),
            arg_index: None,
            callee_fn_id: Some(callee),
            spelling: if sp != location { Some(sp) } else { None },
            scope: call_scope(conn, &mut view, caller, &location)?,
            expression,
            provenance,
            location,
        });
    }
    // A recorded call replaces only its matching source operation. Other
    // origins can share these endpoints without an annotated call occurrence
    // (notably file-scope initializers), and must survive as source transitions.
    steps.retain(|e| {
        !(e.call_site_id.is_none()
            && e.operations.iter().any(|op| op == "return value")
            && replaced_return_origins
                .get(&(e.from, e.to))
                .is_some_and(|origins| e.provenance.iter().all(|origin| origins.contains(origin))))
    });
    steps.extend(return_steps);
    for step in &mut steps {
        if let Some(entity) = entities
            .get(&(step.to.0 as i64))
            .filter(|e| e.call_site_id.is_some())
        {
            step.call_site_id = entity.call_site_id;
            step.location = entity.location.clone();
            step.spelling = entity.spelling.clone();
            if entity.kind == "call_target" {
                step.operations.retain(|op| op != "assign");
                step.operations.push("resolve indirect call".into());
                // JSON renders provenance before the operation labels. The
                // synthetic copy resolves a target; it is no source assignment.
                for origin in &mut step.provenance {
                    if origin.operation == "assign" {
                        origin.operation = "resolve indirect call".into();
                    }
                }
            }
        }
        // Retain source attribution when a reached pointer store carries an origin.
        if step.operations.iter().any(|op| op == "write pointer") {
            expose_write_expression(
                step.to.0 as i64,
                false,
                &step.expression,
                &step.location,
                &field_access,
                &mut entities,
                &mut hidden,
            );
        }
    }
    steps.sort();
    steps.dedup();
    let mut adj = BTreeMap::<i64, Vec<FlowStep>>::new();
    for e in steps {
        let start = if dir == Direction::Down { e.from } else { e.to };
        adj.entry(start.0 as i64).or_default().push(e);
    }
    let roots: BTreeSet<_> = raw
        .order
        .iter()
        .filter(|(_, d)| *d == 0)
        .map(|(id, _)| canonical(*id))
        .filter(|id| !hidden.contains(id))
        .collect();
    let mut reached = BTreeMap::new();
    let mut queue = VecDeque::new();
    for id in roots {
        reached.insert(id, 0);
        queue.push_back(id);
    }
    while let Some(id) = queue.pop_front() {
        let level = reached[&id];
        let (visible, _) = collapse_hidden(id, dir, &adj, &hidden);
        for (next, e) in visible {
            if level == depth {
                if !reached.contains_key(&next) {
                    view.truncated = true;
                } else {
                    view.edges.push(e);
                }
                continue;
            }
            view.edges.push(e);
            if let std::collections::btree_map::Entry::Vacant(entry) = reached.entry(next) {
                entry.insert(level + 1);
                queue.push_back(next);
            }
        }
    }
    for (id, depth) in reached {
        if let Some(mut e) = entities.remove(&id) {
            e.depth = depth;
            view.nodes.insert(PagNodeId(id as u32), e);
        }
    }
    view.edges.sort();
    view.edges.dedup();
    let used: BTreeSet<_> = view
        .nodes
        .values()
        .map(|e| e.scope.clone())
        .chain(view.edges.iter().map(|e| e.scope.clone()))
        .chain(
            view.edges
                .iter()
                .filter_map(|e| e.callee_fn_id.map(|id| format!("fn:{}", id.0))),
        )
        .collect();
    view.scopes.retain(|k, _| used.contains(k));
    // Older exports still render, but never guess parameter positions or types.
    let has_parameters: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='flow_parameters')",
        [],
        |r| r.get(0),
    )?;
    if has_parameters {
        let mut stmt = conn.prepare("SELECT arg_index,var_id,name,type_name FROM flow_parameters WHERE fn_id=?1 ORDER BY arg_index,var_id")?;
        for scope in view.scopes.values_mut() {
            if let Some(id) = scope.fn_id {
                scope.parameters = stmt
                    .query_map([id.0], |r| {
                        Ok(FlowParameter {
                            arg_index: r.get(0)?,
                            var_id: VarId(r.get(1)?),
                            name: r.get(2)?,
                            type_name: r.get(3)?,
                        })
                    })?
                    .collect::<rusqlite::Result<_>>()?;
            }
        }
    }
    view.json_metadata = json::Metadata::load(conn, &view)?;
    Ok(view)
}

/// Expand each metadata state once per growth of its finite fact sets. The
/// origin is fixed for this invocation; the endpoint and source/call metadata
/// form the key. Operations and provenance are unioned, never path-enumerated.
/// Keeping metadata in the key preserves distinct call/argument occurrences.
fn collapse_hidden(
    origin: i64,
    dir: Direction,
    adj: &BTreeMap<i64, Vec<FlowStep>>,
    hidden: &BTreeSet<i64>,
) -> (Vec<(i64, FlowStep)>, usize) {
    let mut states = BTreeMap::<FlowStep, FlowStep>::new();
    let mut pending = VecDeque::new();
    let mut queued = BTreeSet::new();
    fn admit(
        mut step: FlowStep,
        states: &mut BTreeMap<FlowStep, FlowStep>,
        pending: &mut VecDeque<FlowStep>,
        queued: &mut BTreeSet<FlowStep>,
    ) {
        step.operations.sort();
        step.operations.dedup();
        step.provenance.sort();
        step.provenance.dedup();
        let mut key = step.clone();
        key.operations.clear();
        key.provenance.clear();
        let changed = if let Some(old) = states.get_mut(&key) {
            let before = (old.operations.len(), old.provenance.len());
            old.operations.extend(step.operations);
            old.operations.sort();
            old.operations.dedup();
            old.provenance.extend(step.provenance);
            old.provenance.sort();
            old.provenance.dedup();
            before != (old.operations.len(), old.provenance.len())
        } else {
            states.insert(key.clone(), step);
            true
        };
        if changed && queued.insert(key.clone()) {
            pending.push_back(key);
        }
    }
    for step in adj.get(&origin).into_iter().flatten() {
        admit(step.clone(), &mut states, &mut pending, &mut queued);
    }
    let mut expansions = 0;
    while let Some(key) = pending.pop_front() {
        queued.remove(&key);
        let step = states[&key].clone();
        let next = if dir == Direction::Down {
            step.to
        } else {
            step.from
        };
        if !hidden.contains(&(next.0 as i64)) {
            continue;
        }
        expansions += 1;
        for onward in adj.get(&(next.0 as i64)).into_iter().flatten() {
            let mut merged = step.clone();
            if dir == Direction::Down {
                merged.to = onward.to;
            } else {
                merged.from = onward.from;
            }
            merged.operations.extend(onward.operations.clone());
            merged.provenance.extend(onward.provenance.clone());
            if onward.location.line > 0 {
                merged.location = onward.location.clone();
                merged.expression = onward.expression.clone();
            }
            if onward.call_site_id.is_some() {
                merged.call_site_id = onward.call_site_id;
                merged.arg_index = onward.arg_index;
                merged.callee_fn_id = onward.callee_fn_id;
                merged.spelling = onward.spelling.clone();
                merged.scope = onward.scope.clone();
            }
            admit(merged, &mut states, &mut pending, &mut queued);
        }
    }
    let mut visible = Vec::new();
    for mut step in states.into_values() {
        let next = if dir == Direction::Down {
            step.to
        } else {
            step.from
        };
        if hidden.contains(&(next.0 as i64)) {
            continue;
        }
        if dir == Direction::Down {
            step.from = PagNodeId(origin as u32);
        } else {
            step.to = PagNodeId(origin as u32);
        }
        visible.push((next.0 as i64, step));
    }
    visible.sort();
    visible.dedup();
    (visible, expansions)
}

fn expose_write_expression(
    node: i64,
    field_write: bool,
    recorded: &str,
    location: &FlowSite,
    field_access: &BTreeMap<i64, (i64, String)>,
    entities: &mut BTreeMap<i64, FlowEntity>,
    hidden: &mut BTreeSet<i64>,
) {
    if !hidden.contains(&node) {
        return;
    }
    let recorded = recorded.trim();
    let lhs = assignment_lhs(recorded);
    let destination = if field_write && (recorded.starts_with('{') || lhs.is_none()) {
        // Initializers have no assignment LHS; follow their recorded GEP chain.
        field_destination(node, field_access, entities, hidden)
    } else {
        lhs.map(str::to_owned)
    };
    if let Some(expression) = destination.filter(|s| !s.is_empty()) {
        if let Some(entity) = entities.get_mut(&node) {
            hidden.remove(&node);
            entity.name = expression;
            entity.var_id = None;
            entity.kind = "expression".into();
            entity.location = location.clone();
        }
    }
}

/// Find the outer assignment, ignoring comments, operators and literals within its LHS.
fn assignment_lhs(expression: &str) -> Option<&str> {
    use trace_preproc::{Language, Lexer, TokenKind};

    // Use the shared lexer so digit separators, raw strings, encoding prefixes,
    // comments and line splices cannot change delimiter nesting. Keep the
    // original source slice: token spellings need not preserve numeric separators.
    let tokens = Lexer::new(expression, Language::Cpp).tokenize();
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        let TokenKind::Punct(punct) = token.kind else {
            continue;
        };
        match punct {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" if depth == 0 => {
                // The preprocessor lexer represents <<= and >>= as two tokens.
                let start = if punct == "="
                    && index > 0
                    && matches!(tokens[index - 1].kind, TokenKind::Punct("<<" | ">>"))
                {
                    &tokens[index - 1]
                } else {
                    token
                };
                // Lexer columns count characters, while Rust slices use bytes.
                let line_start: usize = expression
                    .split_inclusive('\n')
                    .take(start.line.checked_sub(1)? as usize)
                    .map(str::len)
                    .sum();
                let col_offset = expression
                    .get(line_start..)?
                    .char_indices()
                    .nth(start.col.checked_sub(1)? as usize)?
                    .0;
                return Some(expression[..line_start + col_offset].trim());
            }
            _ => {}
        }
    }
    None
}

/// Reconstruct an initializer's logical field path from GEP metadata. Source
/// assignments retain their spelled LHS (including pointer access syntax).
fn field_destination(
    mut node: i64,
    accesses: &BTreeMap<i64, (i64, String)>,
    entities: &BTreeMap<i64, FlowEntity>,
    hidden: &BTreeSet<i64>,
) -> Option<String> {
    let mut fields = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some((base, name)) = accesses.get(&node) {
        if !seen.insert(node) {
            return None;
        }
        fields.push(name.as_str());
        node = *base;
    }
    if fields.is_empty() || hidden.contains(&node) {
        return None;
    }
    let mut expression = entities.get(&node)?.name.clone();
    for field in fields.into_iter().rev() {
        write!(expression, ".{field}").unwrap();
    }
    Some(expression)
}

pub fn render_dataflow(view: &DataflowView, format: RenderFormat, meta: &GraphMeta) -> String {
    let groups = ordered_scope_transitions(view, matches!(meta.direction, "up" | "flows-from"));
    if format == RenderFormat::Json {
        return json::render(
            view,
            meta,
            groups.iter().flat_map(|(_, edges)| edges.iter().copied()),
        );
    }
    if format != RenderFormat::Text {
        let dot = format == RenderFormat::Graphviz;
        let escape = |s: &str| {
            if dot {
                serde_json::to_string(s).unwrap()
            } else {
                format!("\"{}\"", crate::render::mermaid_escape(s))
            }
        };
        let mut out = if dot {
            format!(
                "digraph dataflow {{\n  label={};\n  node [shape=box];\n",
                escape(meta.title)
            )
        } else {
            "flowchart TD\n".into()
        };
        for (index, (key, scope)) in view.scopes.iter().enumerate() {
            if dot {
                writeln!(
                    out,
                    "  subgraph cluster_{index} {{ label={};",
                    escape(&scope.display())
                )
                .unwrap();
            } else {
                writeln!(out, "  subgraph scope{index}[{}]", escape(&scope.display())).unwrap();
            }
            for n in view.nodes.values().filter(|n| n.scope == *key) {
                if dot {
                    let mut attrs = format!(
                        "label={}, kind={}, path={}, line={}, col={}",
                        escape(&diagram_entity_label(n)),
                        escape(&n.kind),
                        escape(&n.location.path),
                        n.location.line,
                        n.location.col
                    );
                    if let Some(id) = n.var_id {
                        write!(attrs, ", var_id={}", id.0).unwrap();
                    }
                    if let Some(id) = n.fn_id {
                        write!(attrs, ", fn_id={}", id.0).unwrap();
                    }
                    writeln!(out, "    n{} [{}];", n.id.0, attrs).unwrap();
                } else {
                    writeln!(out, "    n{}[{}]", n.id.0, escape(&diagram_entity_label(n))).unwrap();
                }
            }
            out.push_str(if dot { "  }\n" } else { "  end\n" });
        }
        for e in groups.iter().flat_map(|(_, edges)| edges) {
            if dot {
                writeln!(
                    out,
                    "  n{} -> n{} [label={}];",
                    e.from.0,
                    e.to.0,
                    escape(&diagram_step_label(view, e))
                )
                .unwrap();
            } else {
                writeln!(
                    out,
                    "  n{} -->|{}| n{}",
                    e.from.0,
                    escape(&diagram_step_label(view, e)),
                    e.to.0
                )
                .unwrap();
            }
        }
        if view.truncated {
            out.push_str(if dot {
                "  // truncated at visible depth limit\n"
            } else {
                "  %% truncated at visible depth limit\n"
            });
        }
        if dot {
            out.push_str("}\n");
        }
        return out;
    }
    let mut out = format!(
        "{}\nPossible value flows (context-insensitive may-analysis).\n",
        meta.title
    );
    let mut shown_calls = BTreeSet::new();
    for (key, edges) in groups {
        let scope = &view.scopes[key];
        writeln!(out, "\n{}:", scope.text_display()).unwrap();
        for n in view
            .nodes
            .values()
            .filter(|n| n.scope == key && n.depth == 0)
        {
            writeln!(out, "  selected {}", n.text_display()).unwrap();
        }
        for n in view.nodes.values().filter(|n| {
            n.scope == key
                && n.depth != 0
                && matches!(n.kind.as_str(), "function" | "allocation" | "constant")
        }) {
            writeln!(out, "  {} {}", n.kind, n.text_display()).unwrap();
        }
        if edges.is_empty() {
            for n in view.nodes.values().filter(|n| {
                n.scope == key
                    && n.depth != 0
                    && !matches!(n.kind.as_str(), "function" | "allocation" | "constant")
                    && !n.call_site_id.is_some_and(|cs| shown_calls.contains(&cs))
            }) {
                writeln!(out, "  reached {}", n.text_display()).unwrap();
            }
        }
        let describe = |n: &FlowEntity| {
            if n.scope == key || n.kind == "function" {
                n.text_display()
            } else {
                format!(
                    "{}::{}",
                    view.scopes
                        .get(&n.scope)
                        .map(|s| s.name.clone())
                        .unwrap_or_default(),
                    n.text_display()
                )
            }
        };
        for e in edges {
            let destination = &view.nodes[&e.to];
            if destination.kind == "call_target" {
                writeln!(out, "  {}", describe(&view.nodes[&e.from])).unwrap();
                writeln!(
                    out,
                    "    → {}    {}",
                    destination.text_display(),
                    semantic_step_label(view, e, true)
                )
                .unwrap();
                if destination
                    .call_site_id
                    .is_some_and(|cs| shown_calls.insert(cs))
                {
                    let owner = view
                        .scopes
                        .get(&destination.scope)
                        .map(|s| s.qualified())
                        .unwrap_or_default();
                    writeln!(
                        out,
                        "      inside {owner}, {}{}",
                        destination.location.display(),
                        destination
                            .spelling
                            .as_ref()
                            .map(|sp| format!(" (macro spelling {})", sp.display()))
                            .unwrap_or_default()
                    )
                    .unwrap();
                    writeln!(out, "      possible targets: {}", destination.targets.len()).unwrap();
                    for target in &destination.targets {
                        writeln!(out, "        {} [fn#{}]", target.name, target.fn_id.0).unwrap();
                    }
                }
            } else {
                writeln!(
                    out,
                    "  {} → {}    {}",
                    describe(&view.nodes[&e.from]),
                    describe(destination),
                    semantic_step_label(view, e, true)
                )
                .unwrap();
            }
        }
    }
    if view.truncated {
        out.push_str("\n(truncated at visible depth limit; increase --depth)\n");
    }
    out
}
/// Discover scopes by BFS over the bounded view, then order each scope's
/// transitions by recorded source position for all renderers. Discovery order
/// stays independent of source order. Each edge belongs to its traversal
/// origin's scope; edge orientation and the query's depths/truncation stay intact.
fn ordered_scope_transitions(view: &DataflowView, up: bool) -> Vec<(&str, Vec<&FlowStep>)> {
    let endpoints = |e: &FlowStep| if up { (e.to, e.from) } else { (e.from, e.to) };
    let mut adjacent = BTreeMap::<PagNodeId, Vec<&FlowStep>>::new();
    for e in &view.edges {
        adjacent.entry(endpoints(e).0).or_default().push(e);
    }
    for edges in adjacent.values_mut() {
        // Match the query's visible BFS tie order: next entity ID, then the
        // complete edge (including call identity and operation provenance).
        edges.sort_by(|a, b| endpoints(a).1.cmp(&endpoints(b).1).then_with(|| a.cmp(b)));
    }
    let mut groups = Vec::<(&str, Vec<&FlowStep>)>::new();
    let mut scopes = BTreeMap::<&str, usize>::new();
    let mut reached = BTreeSet::new();
    let mut queue = VecDeque::new();
    for n in view.nodes.values().filter(|n| n.depth == 0) {
        reached.insert(n.id);
        queue.push_back(n.id);
        if !scopes.contains_key(n.scope.as_str()) {
            scopes.insert(&n.scope, groups.len());
            groups.push((&n.scope, Vec::new()));
        }
    }
    while let Some(id) = queue.pop_front() {
        let key = view.nodes[&id].scope.as_str();
        let index = scopes[key];
        for e in adjacent.get(&id).into_iter().flatten() {
            groups[index].1.push(*e);
            let next = endpoints(e).1;
            if reached.insert(next) {
                queue.push_back(next);
                let key = view.nodes[&next].scope.as_str();
                if !scopes.contains_key(key) {
                    scopes.insert(key, groups.len());
                    groups.push((key, Vec::new()));
                }
            }
        }
    }
    // Keep metadata-only scopes (e.g. a callee on a collapsed return) visible.
    for key in view.scopes.keys() {
        if !scopes.contains_key(key.as_str()) {
            scopes.insert(key, groups.len());
            groups.push((key, Vec::new()));
        }
    }
    // A caller can supply a disconnected view or direction metadata that
    // differs from the query. Ordering must still preserve every transition.
    for (id, edges) in &adjacent {
        if !reached.contains(id) {
            let key = view.nodes[id].scope.as_str();
            groups[scopes[key]].1.extend(edges);
        }
    }
    for (_, edges) in &mut groups {
        edges.sort_by(|a, b| {
            fn position(e: &FlowStep) -> (bool, &str, i64, i64) {
                if e.location.line > 0 {
                    (
                        false,
                        e.location.path.as_str(),
                        e.location.line,
                        e.location.col,
                    )
                } else {
                    (true, e.location.path.as_str(), 0, 0)
                }
            }
            position(a).cmp(&position(b)).then_with(|| a.cmp(b))
        });
    }
    groups
}

fn diagram_entity_label(entity: &FlowEntity) -> String {
    if entity.kind == "call_target" {
        format!(
            "{}; possible targets: {}",
            entity.display(),
            entity.targets.len()
        )
    } else {
        entity.display()
    }
}
fn diagram_step_label(view: &DataflowView, edge: &FlowStep) -> String {
    let label = semantic_step_label(view, edge, false);
    let node = &view.nodes[&edge.to];
    if node.kind == "call_target" {
        format!("{label}; possible targets: {}", node.targets.len())
    } else {
        label
    }
}

fn semantic_step_label(view: &DataflowView, e: &FlowStep, text: bool) -> String {
    let label = step_label(e, text);
    if e.arg_index.is_none() {
        if let Some(scope) = e
            .callee_fn_id
            .and_then(|id| view.scopes.get(&format!("fn:{}", id.0)))
        {
            return format!("{label} from {}", scope.qualified());
        }
    }
    label
}
fn step_label(e: &FlowStep, text: bool) -> String {
    let mut label = e.operations.join("; ");
    if text {
        if let Some(arg) = e.arg_index {
            label.push_str(" [");
            if let Some(id) = e.callee_fn_id {
                write!(label, "fn#{}, ", id.0).unwrap();
            }
            write!(label, "argument {}]", arg + 1).unwrap();
        }
    } else if let Some(arg) = e.arg_index {
        write!(label, " [argument {}]", arg + 1).unwrap();
    }
    if e.location.line > 0 {
        write!(label, " at {}", e.location.display()).unwrap();
    }
    if let Some(sp) = &e.spelling {
        write!(label, " (macro spelling {})", sp.display()).unwrap();
    }
    let mut shown = BTreeSet::new();
    for origin in &e.provenance {
        if origin.location.line > 0
            && origin.location != e.location
            && shown.insert((&origin.operation, &origin.location))
        {
            write!(
                label,
                "; {} at {}",
                origin.operation,
                origin.location.display()
            )
            .unwrap();
        }
    }
    if !e.expression.is_empty() {
        write!(label, " — {}", e.expression).unwrap();
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_origins_do_not_imply_missing_dataflow_support() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::SCHEMA_V7).unwrap();
        conn.execute(
            "INSERT INTO analysis_run VALUES(1,'test',?1,'/src','now','{}')",
            [crate::SCHEMA_VERSION],
        )
        .unwrap();
        require_source_dataflow_metadata(&conn).unwrap();
        conn.execute(
            "UPDATE analysis_run SET options_json = '{\"stage\":\"merge\"}'",
            [],
        )
        .unwrap();
        let error = require_source_dataflow_metadata(&conn)
            .unwrap_err()
            .to_string();
        assert!(error.contains("trace-merge output"), "{error}");
    }

    #[test]
    fn assignment_lhs_preserves_nested_operators_and_literals() {
        for lhs in [
            "s->table[i == 0]",
            "s->table[i != 0]",
            "s->table[i <= 0]",
            "s->table[(i = 0)]",
            "(*choose(i == 0))->field",
            "s->table[index(\"=\\\"[\")]",
            "s->table['=']",
        ] {
            assert_eq!(assignment_lhs(&format!("{lhs} = v")), Some(lhs));
        }
        for expression in [
            "{ .f = v }",
            "s->table[i == 0]",
            "i == 0",
            "i != 0",
            "i <= 0",
            "i >= 0",
        ] {
            assert_eq!(assignment_lhs(expression), None);
        }
    }

    #[test]
    fn assignment_lhs_keeps_cpp_literals_atomic() {
        for lhs in [
            "*s->slots[1'000]",
            "*s->slots[0xA'B]",
            "*s->slots[0b1'0]",
            "*s->slots[index(.1'0e+2)]",
            r#"*s->slots[index(R"(" ] } = /* ')" )]"#,
            r#"*s->slots[index(R"tag(" ) ] } = /* ')tag")]"#,
            r#"*s->slots[index(u8R"tag(" ) ] = ')tag"_suffix)]"#,
            r#"*s->slots[index(uR"(" ] = ')")]"#,
            r#"*s->slots[index(UR"(" ] = ')")]"#,
            r#"*s->slots[index(LR"(" ] = ')")]"#,
            r#"*s->slots[index(u'\'', U'=', L']')]"#,
            "*s->slots[index(\"é ] = \\\"\") ]",
            "*s->slots[index(R\"tag(\" ] =\n\\\n /* ')tag\")]",
        ] {
            for op in ["=", "+=", "<<=", ">>="] {
                assert_eq!(assignment_lhs(&format!("{lhs} {op} p")), Some(lhs), "{lhs}");
            }
            assert_eq!(assignment_lhs(lhs), None, "{lhs}");
        }
        assert_eq!(assignment_lhs("*p <<\\\n= value"), Some("*p"));
    }

    #[test]
    fn assignment_lhs_ignores_comment_punctuation() {
        for lhs in [
            "s->field /* = [ \" */",
            "s->table[i /* ] = \" */]",
            "*p /* = note */",
            "s->field // = [ \"\n",
            "s->field // note \\\n = [ \"\n",
            "s->field // note \\\r\n = [ \"\r\n",
        ] {
            for op in ["=", "+=", "<<="] {
                assert_eq!(assignment_lhs(&format!("{lhs} {op} v")), Some(lhs.trim()));
            }
        }
        for expression in ["s->field /* = [ \" */", "s->field // = [ \""] {
            assert_eq!(assignment_lhs(expression), None);
        }
    }

    #[test]
    fn compound_assignments_expose_hidden_write_destinations() {
        for op in [
            "=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=",
        ] {
            for (lhs, operation, summary) in [
                ("s->f", "write field", true),
                ("s->table[i == 0]", "write field", false),
                ("*p", "write pointer", false),
            ] {
                let expression = format!("{lhs} {op} v");
                assert_eq!(assignment_lhs(&expression), Some(lhs));
                let conn = database();
                conn.execute_batch(
                    "DELETE FROM flow_edges;
                    INSERT INTO flow_edges VALUES(0,0,1,'store');",
                )
                .unwrap();
                if summary {
                    conn.execute_batch("UPDATE flow_nodes SET kind='loc',detail='field_summary',var_id=NULL WHERE id=1").unwrap();
                }
                conn.execute(
                    "INSERT INTO flow_origins VALUES(0,1,'store',0,10,3,?1,?2)",
                    rusqlite::params![expression, operation],
                )
                .unwrap();
                let view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
                let dest = &view.nodes[&PagNodeId(1)];
                assert_eq!(dest.kind, "expression");
                assert_eq!(dest.name, lhs);
                assert_eq!(view.edges.len(), 1);
                assert_eq!(view.edges[0].provenance[0].expression, expression);
            }
        }
    }

    #[test]
    fn hidden_reconvergence_unions_provenance_with_bounded_expansion() {
        // 32 diamonds have over four billion simple paths, plus a hidden
        // cycle. Fact growth, rather than path count, bounds expansion.
        let mut adj = BTreeMap::<i64, Vec<FlowStep>>::new();
        let mut hidden = BTreeSet::new();
        let mut facts = BTreeSet::new();
        let mut add = |from: u32, to: u32| {
            let provenance = FlowOperationSite {
                callee_fn_id: None,
                arg_index: None,
                operation: "assign".into(),
                location: FlowSite {
                    path: "main.c".into(),
                    line: from as i64 + 1,
                    col: to as i64 + 1,
                },
                expression: format!("{to} = {from}"),
            };
            facts.insert(provenance.clone());
            adj.entry(from as i64).or_default().push(FlowStep {
                from: PagNodeId(from),
                to: PagNodeId(to),
                operations: vec!["assign".into()],
                provenance: vec![provenance],
                location: FlowSite::default(),
                expression: String::new(),
                call_site_id: None,
                arg_index: None,
                callee_fn_id: None,
                spelling: None,
                scope: "fn:0".into(),
            });
        };
        for layer in 0..32 {
            let start = layer * 3;
            add(start, start + 1);
            add(start, start + 2);
            add(start + 1, start + 3);
            add(start + 2, start + 3);
            hidden.extend([start as i64 + 1, start as i64 + 2, start as i64 + 3]);
        }
        add(96, 1);
        add(96, 97);
        for dir in [Direction::Down, Direction::Up] {
            let oriented = if dir == Direction::Down {
                adj.clone()
            } else {
                let mut reverse = BTreeMap::<i64, Vec<FlowStep>>::new();
                for edge in adj.values().flatten() {
                    reverse
                        .entry(edge.to.0 as i64)
                        .or_default()
                        .push(edge.clone());
                }
                reverse
            };
            let origin = if dir == Direction::Down { 0 } else { 97 };
            let (visible, expansions) = collapse_hidden(origin, dir, &oriented, &hidden);
            assert_eq!(visible.len(), 1);
            assert_eq!(
                (visible[0].1.from, visible[0].1.to),
                (PagNodeId(0), PagNodeId(97))
            );
            assert_eq!(
                visible[0]
                    .1
                    .provenance
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
                facts
            );
            assert!(
                expansions <= hidden.len() * (facts.len() + 1),
                "{expansions} expansions"
            );
            assert_eq!(collapse_hidden(origin, dir, &oriented, &hidden).0, visible);
        }
        // Equal endpoints with different call occurrences must stay distinct.
        let mut extra = adj[&96][1].clone();
        extra.call_site_id = Some(CallSiteId(7));
        extra.arg_index = Some(0);
        extra.callee_fn_id = Some(FnId(8));
        adj.get_mut(&96).unwrap().push(extra);
        let (visible, _) = collapse_hidden(0, Direction::Down, &adj, &hidden);
        assert_eq!(visible.len(), 2);
        assert!(visible
            .iter()
            .any(|(_, e)| e.call_site_id == Some(CallSiteId(7))
                && e.arg_index == Some(0)
                && e.callee_fn_id == Some(FnId(8))));
    }

    #[test]
    fn visible_neighborhood_handles_long_hidden_diamonds_and_parameter_twins() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            UPDATE variables SET is_synthetic=1 WHERE id>0;
            DELETE FROM flow_nodes WHERE id=7;
            INSERT INTO variables(id,name,kind,fn_id,type_id,file_id,line,col,is_synthetic) VALUES(7,'v7','local',0,0,0,7,1,1);
            INSERT INTO flow_nodes(id,kind,label,var_id) VALUES(7,'var','v7',7);
            WITH RECURSIVE ids(n) AS (SELECT 8 UNION ALL SELECT n+1 FROM ids WHERE n<98)
            INSERT INTO variables(id,name,kind,fn_id,type_id,file_id,line,col,is_synthetic) SELECT n,'v'||n,'local',0,0,0,n,1,n<97 FROM ids;
            INSERT INTO flow_nodes(id,kind,label,var_id) SELECT id,'var',name,id FROM variables WHERE id>=8;").unwrap();
        for layer in 0..32 {
            let start = layer * 3;
            for (src, dst) in [
                (start, start + 1),
                (start, start + 2),
                (start + 1, start + 3),
                (start + 2, start + 3),
            ] {
                conn.execute(
                    "INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(?1,?2,'copy')",
                    [src, dst],
                )
                .unwrap();
            }
        }
        conn.execute_batch("INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(96,1,'copy'),(96,97,'copy'),(97,98,'copy');
            INSERT INTO variables(id,name,kind,fn_id,type_id,file_id,line,col) VALUES(99,'a','param',0,0,0,100,1);").unwrap();
        let mut twin = symbol(&conn, "a");
        twin.var_id = 99;
        for (direction, root) in [
            (Direction::Down, twin),
            (Direction::Up, symbol(&conn, "v98")),
        ] {
            let view = dataflow_view(&conn, &[root], direction, 1).unwrap();
            assert_eq!(view.nodes.len(), 2);
            assert_eq!(view.edges.len(), 1, "{:?}: {:?}", direction, view.edges);
            assert!(view.truncated);
            let full = dataflow_view(
                &conn,
                &[if direction == Direction::Down {
                    symbol(&conn, "a")
                } else {
                    symbol(&conn, "v98")
                }],
                direction,
                2,
            )
            .unwrap();
            assert_eq!(full.nodes.len(), 3);
            assert!(!full.truncated);
            assert!(full
                .edges
                .iter()
                .any(|e| e.from == PagNodeId(0) && e.to == PagNodeId(97)));
        }
    }

    #[test]
    fn incoming_pointer_write_metadata_classifies_destination_without_adding_store() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,1,'gep'),(1,3,'load'),(3,5,'copy'),(4,1,'store');
            INSERT INTO flow_origins VALUES(4,1,'store',0,10,3,'*slot = c','store');").unwrap();
        let down = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        assert_eq!(
            down.nodes.keys().copied().collect::<Vec<_>>(),
            vec![PagNodeId(0), PagNodeId(1)]
        );
        assert!(down.truncated);
        assert_eq!(down.nodes[&PagNodeId(1)].name, "*slot");
        assert!(!down
            .edges
            .iter()
            .any(|edge| edge.operations.contains(&"write pointer".into())));
        let up = dataflow_view(&conn, &[symbol(&conn, "b")], Direction::Up, 1).unwrap();
        assert_eq!(up.nodes[&PagNodeId(1)].name, "*slot");
        assert!(up.truncated);
    }

    #[test]
    fn shallow_neighborhood_ignores_large_disconnected_data_and_keeps_hidden_cycles() {
        let conn = database();
        let before = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        conn.execute_batch("WITH RECURSIVE ids(n) AS (SELECT 100 UNION ALL SELECT n+1 FROM ids WHERE n<50100)
            INSERT INTO variables(id,name,kind,type_id,file_id,line,col,is_synthetic) SELECT n,'unrelated'||n,'global',0,0,n,1,0 FROM ids;
            INSERT INTO flow_nodes(id,kind,label,var_id) SELECT id,'var',name,id FROM variables WHERE id>=100;
            INSERT INTO flow_edges(src_node,dst_node,kind) SELECT id,id+1,'copy' FROM variables WHERE id>=100 AND id<50100;
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(2,1,'copy');").unwrap();
        for direction in [Direction::Down, Direction::Up] {
            let neighborhood =
                visible_neighborhood(&conn, &[symbol(&conn, "a")], direction, 1).unwrap();
            assert!(
                neighborhood.metadata.len() < 10,
                "loaded {} nodes",
                neighborhood.metadata.len()
            );
            assert!(neighborhood.metadata.iter().all(|id| *id < 100));
            let shallow = dataflow_view(&conn, &[symbol(&conn, "a")], direction, 1).unwrap();
            assert!(shallow.truncated);
            assert!(shallow.nodes.values().all(|node| node.depth <= 1));
        }
        let after = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        assert_eq!(
            before.nodes.keys().collect::<Vec<_>>(),
            after.nodes.keys().collect::<Vec<_>>()
        );
        assert!(after
            .edges
            .iter()
            .any(|edge| edge.from == PagNodeId(0) && edge.to == PagNodeId(3)));
    }

    #[test]
    fn initializer_accesses_canonicalize_both_endpoints_and_inherit_storage_ownership() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            UPDATE variables SET kind='global',fn_id=NULL WHERE id=0;
            UPDATE flow_nodes SET fn_id=NULL WHERE id=0;
            INSERT INTO flow_nodes(id,kind,label,detail,var_id) VALUES(80,'loc','hidden twin','local',1),(81,'loc','field','field',NULL),(82,'loc','nested','field',NULL);
            INSERT INTO flow_field_access(base_node,dst_node,field_name) VALUES(7,80,'send'),(80,1,'stale');
            INSERT INTO flow_field_locations(node_id,parent_node,field_name) VALUES(81,7,'ops'),(82,81,'send');
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,80,'store'),(0,81,'store'),(0,82,'store');
            INSERT INTO flow_origins VALUES(0,80,'store',0,10,3,'{ handler }','write field');").unwrap();
        for direction in [Direction::Down, Direction::Up] {
            let root = if direction == Direction::Down {
                "a"
            } else {
                "internal"
            };
            let view = dataflow_view(&conn, &[symbol(&conn, root)], direction, 1).unwrap();
            assert_eq!(view.nodes[&PagNodeId(1)].name, "a.send");
            assert!(view
                .edges
                .iter()
                .any(|edge| edge.from == PagNodeId(0) && edge.to == PagNodeId(1)));
        }
        let view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        for id in [81, 82] {
            assert_eq!(view.nodes[&PagNodeId(id)].scope, "global:/src/main.c");
            assert_eq!(view.nodes[&PagNodeId(id)].fn_id, None);
        }
        assert!(view.nodes[&PagNodeId(82)].name.contains("a.ops.send"));
        conn.execute_batch(
            "UPDATE variables SET kind='local',fn_id=0 WHERE id=0;
            UPDATE flow_nodes SET fn_id=0 WHERE id IN (0,7);",
        )
        .unwrap();
        let local = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        for id in [81, 82] {
            assert_eq!(local.nodes[&PagNodeId(id)].scope, "fn:0");
            assert_eq!(local.nodes[&PagNodeId(id)].fn_id, Some(FnId(0)));
        }
        assert!(local.nodes[&PagNodeId(82)].name.contains("a.ops.send"));
    }

    #[test]
    fn global_return_assignment_loads_caller_scope_without_owned_flow_nodes() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            UPDATE variables SET kind='global',fn_id=NULL WHERE id IN (0,3);
            UPDATE flow_nodes SET fn_id=NULL WHERE id IN (0,3,7);
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES(1,'caller',0,40,45,'external','caller()',1);
            INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES(0,1,0,41,5,'f',1);
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,3,'copy');
            INSERT INTO flow_return_calls VALUES(0,3,0,0);
            INSERT INTO flow_origins VALUES(0,3,'copy',0,41,1,'b = f()','return value');").unwrap();
        let view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        assert!(!view.nodes.values().any(|node| node.fn_id == Some(FnId(1))));
        assert_eq!(view.scopes["fn:1"].name, "caller");
        assert_eq!(view.scopes["fn:1"].location.line, 40);
        assert_eq!(view.edges[0].scope, "fn:1");
        json_document(&view, Direction::Down, 1);
        let meta = GraphMeta {
            title: "global return",
            direction: "down",
            depth: 1,
            summary: "",
        };
        let text = render_dataflow(&view, RenderFormat::Text, &meta);
        assert!(text.contains("caller [fn#1]"), "{text}");
    }

    #[test]
    fn nullable_callers_render_argument_and_return_occurrences_in_file_scope() {
        let conn = database_schema(
            &crate::SCHEMA_V7.replace("caller_fn_id INTEGER NOT NULL", "caller_fn_id INTEGER"),
        );
        conn.execute_batch("DELETE FROM flow_edges;
            INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES(0,NULL,0,10,5,'f',1);
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,3,'copy');
            INSERT INTO flow_calls VALUES(0,3,0,0);
            INSERT INTO flow_return_calls VALUES(0,3,0,0);
            INSERT INTO flow_origins VALUES(0,3,'copy',0,10,1,'b = f(a)','return value');").unwrap();
        for direction in [Direction::Down, Direction::Up] {
            let root = if direction == Direction::Down {
                "a"
            } else {
                "b"
            };
            let view = dataflow_view(&conn, &[symbol(&conn, root)], direction, 1).unwrap();
            let annotated: Vec<_> = view
                .edges
                .iter()
                .filter(|edge| edge.call_site_id.is_some())
                .collect();
            assert_eq!(annotated.len(), 2);
            assert!(annotated
                .iter()
                .all(|edge| edge.scope == "global:/src/main.c"));
            assert_eq!(view.scopes["global:/src/main.c"].fn_id, None);
            let doc = json_document(&view, direction, 1);
            assert!(doc["edges"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["scope"].is_null()));
            let meta = GraphMeta {
                title: "nullable callers",
                summary: "",
                direction: if direction == Direction::Down {
                    "down"
                } else {
                    "up"
                },
                depth: 1,
            };
            for format in [
                RenderFormat::Json,
                RenderFormat::Text,
                RenderFormat::Graphviz,
            ] {
                let output = render_dataflow(&view, format, &meta);
                assert!(output.contains("global"), "{output}");
                assert!(!output.contains("fn:None"));
            }
        }
    }

    fn json_document(view: &DataflowView, direction: Direction, depth: u32) -> serde_json::Value {
        let meta = GraphMeta {
            title: "flows",
            direction: if direction == Direction::Down {
                "down"
            } else {
                "up"
            },
            depth,
            summary: "",
        };
        let before = serde_json::to_value(view).unwrap();
        let output = render_dataflow(view, RenderFormat::Json, &meta);
        assert_eq!(output, render_dataflow(view, RenderFormat::Json, &meta));
        assert_eq!(before, serde_json::to_value(view).unwrap());
        let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
        let keys = |value: &serde_json::Value| {
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        let expected = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            keys(&doc),
            expected(&[
                "schema",
                "title",
                "direction",
                "depth",
                "truncated",
                "scopes",
                "nodes",
                "edges"
            ])
        );
        assert_eq!(doc["schema"], "dataflow-source-v1");
        assert_eq!(
            keys(&doc["scopes"]),
            expected(&["functions", "globals", "statics", "values"])
        );
        for kind in ["functions", "globals", "statics", "values"] {
            let entries = doc["scopes"][kind].as_array().unwrap();
            let ids: Vec<_> = entries
                .iter()
                .map(|entry| entry["id"].as_u64().unwrap())
                .collect();
            assert!(
                ids.windows(2).all(|pair| pair[0] < pair[1]),
                "{kind}: {ids:?}"
            );
            for entry in entries {
                let fields = if kind == "functions" {
                    vec!["id", "name", "signature", "location"]
                } else {
                    vec!["id", "name", "location"]
                };
                assert_eq!(keys(entry), expected(&fields));
            }
        }
        let check_scope = |scope: &serde_json::Value| {
            if scope.is_null() {
                return;
            }
            assert_eq!(keys(scope), expected(&["kind", "id"]));
            let array = match scope["kind"].as_str().unwrap() {
                "function" => "functions",
                "global" => "globals",
                "static" => "statics",
                "value" => "values",
                other => panic!("{other}"),
            };
            assert!(
                doc["scopes"][array]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["id"] == scope["id"]),
                "unresolved {scope}"
            );
        };
        let check_function = |id: &serde_json::Value| {
            if !id.is_null() {
                assert!(
                    doc["scopes"]["functions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|entry| entry["id"] == *id),
                    "unresolved function {id}"
                );
            }
        };
        let nodes = doc["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), view.nodes.len());
        for (node, internal) in nodes.iter().zip(view.nodes.values()) {
            assert_eq!(
                keys(node),
                expected(&["id", "name", "kind", "scope", "location", "depth", "callees"])
            );
            check_scope(&node["scope"]);
            assert_eq!(node["id"], internal.id.0);
            assert_eq!(node["depth"], internal.depth);
            assert_eq!(
                node["callees"],
                serde_json::to_value(&internal.callees).unwrap()
            );
            for callee in node["callees"].as_array().unwrap() {
                check_function(callee);
            }
        }
        let ordered = ordered_scope_transitions(view, direction == Direction::Up);
        let edges: Vec<_> = ordered.iter().flat_map(|(_, edges)| edges.iter()).collect();
        assert_eq!(doc["edges"].as_array().unwrap().len(), edges.len());
        for (edge, internal) in doc["edges"].as_array().unwrap().iter().zip(edges) {
            assert_eq!(
                keys(edge),
                expected(&[
                    "from",
                    "to",
                    "scope",
                    "expression",
                    "location",
                    "operations"
                ])
            );
            assert_eq!(edge["from"], internal.from.0);
            assert_eq!(edge["to"], internal.to.0);
            assert_eq!(edge["expression"], internal.expression);
            assert_eq!(
                edge["location"],
                serde_json::to_value(&internal.location).unwrap()
            );
            check_scope(&edge["scope"]);
            assert!(edge.get("callee_id").is_none());
            for operation in edge["operations"].as_array().unwrap() {
                let mut fields = vec!["kind", "expression", "location"];
                for field in ["callee_id", "arg_index"] {
                    if let Some(value) = operation.get(field) {
                        assert!(value.as_u64().is_some(), "{operation}");
                        fields.push(field);
                    }
                }
                assert_eq!(keys(operation), expected(&fields));
                if !operation["kind"]
                    .as_str()
                    .unwrap()
                    .starts_with("pass argument")
                {
                    assert!(operation.get("arg_index").is_none());
                }
                check_function(&operation["callee_id"]);
            }
        }
        doc
    }

    #[test]
    fn json_scopes_resolve_all_kinds_and_metadata_only_functions() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            UPDATE variables SET kind='global',fn_id=NULL WHERE id=3;
            UPDATE variables SET kind='file_static',fn_id=NULL WHERE id=4;
            UPDATE variables SET kind='fn_static' WHERE id=5;
            UPDATE variables SET fn_id=NULL WHERE id=6;
            UPDATE flow_nodes SET fn_id=NULL WHERE id IN (3,4,6);
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES
                (2,'callee',0,60,65,'external','callee()',1),
                (9,'represented',0,70,75,'external','represented()',1),
                (40,'writer',0,100,110,'external','writer()',1);
            INSERT INTO variables(id,name,kind,type_id,file_id,line,col) VALUES
                (13,'another_global','global',0,0,31,1),(14,'another_static','file_static',0,0,32,1);
            INSERT INTO flow_nodes(id,kind,label,var_id) VALUES
                (21,'var','another_global',13),(22,'var','another_static',14);
            INSERT INTO flow_nodes(id,kind,label,detail,var_id) VALUES(23,'loc','b field','field',3);
            INSERT INTO flow_nodes(id,kind,label,detail,fn_id) VALUES
                (8,'loc','function represented','function',9),
                (12,'loc','first value','heap',NULL),
                (20,'loc','second value','string_lit',NULL);
            INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES(1,0,0,15,3,'callback',0);
            INSERT INTO flow_nodes(id,kind,label,fn_id,call_site_id) VALUES(11,'call_target','callback()',0,1);
            INSERT INTO call_edges(id,call_site_id,caller_fn_id,callee_fn_id,resolution) VALUES(0,1,0,2,'indirect');
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES
                (0,3,'copy'),(0,4,'copy'),(0,5,'copy'),(0,6,'copy'),
                (0,8,'addr_of'),(0,12,'copy'),(0,20,'copy'),(0,11,'copy'),
                (0,21,'copy'),(0,22,'copy'),(0,23,'copy');
            INSERT INTO flow_origins VALUES(0,3,'copy',0,101,3,'b = a','assign');").unwrap();
        let view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        let doc = json_document(&view, Direction::Down, 1);
        let scope = |id| {
            doc["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["id"] == id)
                .unwrap()["scope"]
                .clone()
        };
        assert_eq!(scope(0), serde_json::json!({"kind":"function","id":0}));
        assert_eq!(scope(3), serde_json::json!({"kind":"global","id":3}));
        assert_eq!(scope(4), serde_json::json!({"kind":"static","id":4}));
        assert_eq!(scope(5), serde_json::json!({"kind":"function","id":0}));
        assert!(scope(6).is_null());
        assert_eq!(scope(8), serde_json::json!({"kind":"function","id":9}));
        assert_eq!(scope(12), serde_json::json!({"kind":"value","id":12}));
        assert_eq!(scope(20), serde_json::json!({"kind":"value","id":20}));
        assert_eq!(
            doc["scopes"]["functions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [0, 2, 9, 40]
        );
        assert_eq!(doc["scopes"]["globals"].as_array().unwrap().len(), 2);
        assert_eq!(doc["scopes"]["statics"].as_array().unwrap().len(), 2);
        assert_eq!(scope(23), serde_json::json!({"kind":"global","id":3}));
        assert_eq!(doc["scopes"]["values"].as_array().unwrap().len(), 2);
        assert_eq!(doc["scopes"]["globals"][0]["name"], "b");
        assert_eq!(doc["scopes"]["globals"][0]["location"]["line"], 3);
        assert_eq!(
            doc["edges"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["to"] == 3)
                .unwrap()["scope"],
            serde_json::json!({"kind":"function","id":40})
        );
        assert!(!view
            .nodes
            .values()
            .any(|n| n.fn_id == Some(FnId(2)) || n.fn_id == Some(FnId(40))));
        assert!(!view.scopes.contains_key("fn:40"));
    }

    #[test]
    fn json_operation_ownership_uses_exact_positions_and_rejects_ambiguity() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            UPDATE variables SET kind='global',fn_id=NULL WHERE id=3;
            UPDATE variables SET kind='file_static',fn_id=NULL WHERE id=4;
            UPDATE flow_nodes SET fn_id=NULL WHERE id IN (3,4);
            INSERT INTO files VALUES(1,'/other/main.c','',0);
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES
                (9,'writer',0,40,45,'external','writer()',1),
                (10,'ambiguous1',0,70,75,'external','ambiguous1()',1),
                (11,'ambiguous2',0,70,75,'external','ambiguous2()',1),
                (12,'declaration',0,60,65,'external','declaration()',0);
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,3,'copy'),(0,4,'copy');
            INSERT INTO flow_origins VALUES
                (0,3,'copy',0,40,3,'b = a','assign'),
                (0,4,'copy',0,45,3,'c = a','assign'),
                (0,3,'copy',0,55,3,'outside = a','assign'),
                (0,3,'copy',0,60,3,'declaration position','assign'),
                (0,3,'copy',0,72,3,'ambiguous position','assign'),
                (0,3,'copy',1,40,3,'different path','assign'),
                (0,3,'copy',0,0,0,'missing position','assign');").unwrap();
        let mut indexed_documents = Vec::new();
        for indexed in [true, false] {
            if !indexed {
                conn.execute_batch("DROP INDEX idx_functions_file_range;")
                    .unwrap();
            }
            for (index, direction) in [Direction::Down, Direction::Up].into_iter().enumerate() {
                let roots = if direction == Direction::Down {
                    vec![symbol(&conn, "a")]
                } else {
                    vec![symbol(&conn, "b"), symbol(&conn, "c")]
                };
                let view = dataflow_view(&conn, &roots, direction, 1).unwrap();
                let doc = json_document(&view, direction, 1);
                if indexed {
                    indexed_documents.push(doc.clone());
                } else {
                    assert_eq!(
                        doc, indexed_documents[index],
                        "the index preserves complete JSON output"
                    );
                }
                assert_eq!(doc["edges"].as_array().unwrap().len(), 7);
                for edge in doc["edges"].as_array().unwrap() {
                    if edge["expression"] == "b = a" || edge["expression"] == "c = a" {
                        assert_eq!(edge["scope"], serde_json::json!({"kind":"function","id":9}));
                    } else {
                        assert!(edge["scope"].is_null(), "{edge}");
                    }
                }
            }
        }
    }

    #[test]
    fn json_recorded_caller_overrides_source_range_and_loads_constituent_functions() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            UPDATE variables SET kind='global',fn_id=NULL WHERE id IN (0,3);
            UPDATE flow_nodes SET fn_id=NULL WHERE id IN (0,3,7);
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES
                (9,'constituent',0,40,45,'external','constituent()',1),
                (40,'caller',0,100,110,'external','caller()',1);
            INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES(0,40,0,10,3,'f',1);
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,3,'copy');
            INSERT INTO flow_origins VALUES(0,3,'copy',0,10,1,'b = f()','return value');
            INSERT INTO flow_return_calls VALUES(0,3,0,0);").unwrap();
        let mut view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        view.edges[0].provenance.push(FlowOperationSite {
            callee_fn_id: None,
            operation: "assign".into(),
            expression: "b = a".into(),
            location: FlowSite {
                path: "/src/main.c".into(),
                line: 41,
                col: 3,
            },
            arg_index: None,
        });
        view.json_metadata = json::Metadata::load(&conn, &view).unwrap();
        let doc = json_document(&view, Direction::Down, 1);
        assert_eq!(
            doc["edges"][0]["scope"],
            serde_json::json!({"kind":"function","id":40})
        );
        assert!(doc["edges"][0].get("callee_id").is_none());
        let returned = doc["edges"][0]["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|op| op["kind"] == "return value")
            .unwrap();
        assert_eq!(returned["callee_id"], 0);
        assert!(returned.get("arg_index").is_none());
        assert_eq!(
            doc["scopes"]["functions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [0, 9, 40]
        );
        assert!(view.nodes.values().all(|n| n.fn_id.is_none()));
        assert!(!view.scopes.contains_key("fn:9"));
    }

    #[test]
    fn json_empty_graph_emits_all_description_arrays() {
        let doc = json_document(&DataflowView::default(), Direction::Down, 0);
        assert_eq!(
            doc["scopes"],
            serde_json::json!({"functions":[],"globals":[],"statics":[],"values":[]})
        );
        assert_eq!(doc["nodes"], serde_json::json!([]));
        assert_eq!(doc["edges"], serde_json::json!([]));
    }

    #[test]
    fn json_operations_preserve_exact_provenance_and_append_missing_kinds() {
        let conn = database();
        conn.execute_batch(
            "DELETE FROM flow_edges;
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,3,'copy');
            INSERT INTO flow_origins VALUES(0,3,'copy',0,10,2,'b = a','assign');",
        )
        .unwrap();
        let mut view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        let edge = &mut view.edges[0];
        let first = edge.provenance[0].clone();
        edge.provenance.push(first.clone());
        let mut different_expression = first.clone();
        different_expression.expression = "b = (a)".into();
        edge.provenance.push(different_expression);
        let mut different_location = first.clone();
        different_location.location.col = 4;
        edge.provenance.push(different_location);
        for index in [0, 1, 0] {
            edge.provenance.push(FlowOperationSite {
                callee_fn_id: None,
                operation: "pass argument".into(),
                expression: "dispatch(a,a)".into(),
                location: first.location.clone(),
                arg_index: Some(index),
            });
        }
        let mut distinct_callee = edge.provenance.last().unwrap().clone();
        distinct_callee.callee_fn_id = Some(FnId(0));
        edge.provenance.push(distinct_callee);
        edge.operations
            .extend(["read pointer".into(), "write pointer".into()]);
        edge.arg_index = Some(99);
        let doc = json_document(&view, Direction::Down, 1);
        let operations = doc["edges"][0]["operations"].as_array().unwrap();
        assert_eq!(operations.len(), 8);
        assert_eq!(operations[0]["expression"], "b = a");
        assert_eq!(operations[1]["expression"], "b = (a)");
        assert_eq!(operations[2]["location"]["col"], 4);
        assert_eq!(operations[3]["arg_index"], 0);
        assert_eq!(operations[4]["arg_index"], 1);
        assert!(operations[3].get("callee_id").is_none());
        assert_eq!(operations[5]["callee_id"], 0);
        assert_eq!(operations[5]["arg_index"], 0);
        for (op, kind) in operations[6..]
            .iter()
            .zip(["read pointer", "write pointer"])
        {
            assert_eq!(op["kind"], kind);
            assert_eq!(op["expression"], "");
            assert_eq!(
                op["location"],
                serde_json::json!({"path":"","line":0,"col":0})
            );
            assert!(op.get("arg_index").is_none());
            assert!(op.get("callee_id").is_none());
        }
        view.edges[0].provenance.clear();
        let doc = json_document(&view, Direction::Down, 1);
        for op in doc["edges"][0]["operations"].as_array().unwrap() {
            assert_eq!(op["expression"], "");
            assert_eq!(
                op["location"],
                serde_json::json!({"path":"","line":0,"col":0})
            );
        }
        assert_eq!(doc["edges"][0]["location"]["line"], 10);
        assert_eq!(doc["edges"][0]["expression"], "b = a");
    }

    #[test]
    fn json_collapsed_calls_keep_each_original_argument_index_in_both_directions() {
        let conn = database();
        conn.execute_batch("DELETE FROM flow_edges;
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES
                (1,'direct_callee',0,40,45,'external','direct_callee()',1),
                (2,'indirect_callee',0,50,55,'external','indirect_callee()',1);
            UPDATE variables SET fn_id=1 WHERE id=1;
            UPDATE variables SET fn_id=2 WHERE id=2;
            INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES
                (0,0,0,10,2,'direct',1),(1,0,0,11,4,'indirect',0);
            INSERT INTO flow_edges(src_node,dst_node,kind) VALUES(0,1,'copy'),(1,2,'copy'),(2,3,'copy');
            INSERT INTO flow_calls VALUES(0,1,0,0),(1,2,1,2);
            INSERT INTO flow_call_expressions VALUES(0,'direct(a)'),(1,'indirect(hidden)');
            INSERT INTO flow_origins VALUES(2,3,'copy',0,12,3,'b = hidden','assign');").unwrap();
        for direction in [Direction::Down, Direction::Up] {
            let root = if direction == Direction::Down {
                "a"
            } else {
                "b"
            };
            let view = dataflow_view(&conn, &[symbol(&conn, root)], direction, 1).unwrap();
            assert_eq!(view.nodes.len(), 2);
            assert_eq!(view.edges.len(), 1);
            assert_eq!(
                (view.edges[0].from, view.edges[0].to),
                (PagNodeId(0), PagNodeId(3))
            );
            assert_eq!(
                view.nodes[&PagNodeId(0)].depth,
                if direction == Direction::Down { 0 } else { 1 }
            );
            assert_eq!(
                view.nodes[&PagNodeId(3)].depth,
                if direction == Direction::Down { 1 } else { 0 }
            );
            let doc = json_document(&view, direction, 1);
            let operations = doc["edges"][0]["operations"].as_array().unwrap();
            assert_eq!(operations.len(), 3);
            let direct = operations
                .iter()
                .find(|op| op["kind"] == "pass argument")
                .unwrap();
            let indirect = operations
                .iter()
                .find(|op| op["kind"] == "pass argument via indirect call")
                .unwrap();
            assert_eq!(direct["expression"], "direct(a)");
            assert_eq!(indirect["expression"], "indirect(hidden)");
            assert_eq!(direct["callee_id"], 1);
            assert_eq!(indirect["callee_id"], 2);
            assert_eq!(direct["arg_index"], 0);
            assert_eq!(direct["location"]["line"], 10);
            assert_eq!(indirect["arg_index"], 2);
            assert_eq!(indirect["location"]["line"], 11);
            assert_eq!(
                doc["edges"][0]["scope"],
                serde_json::json!({"kind":"function","id":0})
            );
        }
        // Single operations use the same zero-based contract.
        conn.execute("UPDATE variables SET is_synthetic=0 WHERE id=1", [])
            .unwrap();
        let view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        let doc = json_document(&view, Direction::Down, 1);
        assert_eq!(doc["edges"][0]["operations"][0]["callee_id"], 1);
        assert_eq!(doc["edges"][0]["operations"][0]["arg_index"], 0);
        assert!(view.truncated);
        assert_eq!(doc["edges"][0]["expression"], "direct(a)");
        conn.execute("DROP TABLE flow_call_expressions", [])
            .unwrap();
        let older = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        let doc = json_document(&older, Direction::Down, 1);
        assert_eq!(doc["edges"][0]["expression"], "");
        assert_eq!(doc["edges"][0]["operations"][0]["expression"], "");
        assert_eq!(doc["edges"][0]["location"]["line"], 10);
    }

    fn database() -> Connection {
        database_schema(crate::SCHEMA_V7)
    }

    fn database_schema(schema: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        conn.execute_batch("INSERT INTO files VALUES(0,'/src/main.c','',0);
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined) VALUES(0,'f',0,1,30,'external','f()',1);
            INSERT INTO types VALUES(0,'ptr','char*',8,'{}');
            INSERT INTO variables(id,name,kind,fn_id,type_id,file_id,line,col,is_synthetic) VALUES
            (0,'a','param',0,0,0,1,10,0),(1,'internal','local',0,0,0,2,1,1),(2,'hidden','local',0,0,0,2,2,1),
            (3,'b','local',0,0,0,3,1,0),(4,'c','local',0,0,0,4,1,0),(5,'d','local',0,0,0,5,1,0),
            (6,'_gep_user','local',0,0,0,6,1,0);
            INSERT INTO flow_nodes(id,kind,label,var_id,fn_id) VALUES (0,'var','a',0,0),(1,'var','internal',1,0),(2,'var','hidden',2,0),
            (3,'var','b',3,0),(4,'var','c',4,0),(5,'var','d',5,0),(6,'var','_gep_user',6,0);
            INSERT INTO flow_nodes(id,kind,label,detail,var_id,fn_id) VALUES(7,'loc','a storage','local',0,0);
            INSERT INTO flow_edges VALUES (0,0,1,'copy'),(1,1,2,'load'),(2,2,3,'copy'),(3,1,4,'copy'),(4,3,5,'copy'),(5,4,5,'copy'),(6,5,0,'copy'),(7,0,7,'points_to'),(8,7,6,'addr_of');").unwrap();
        conn
    }
    fn symbol(conn: &Connection, name: &str) -> SymbolRef {
        let id = conn
            .query_row("SELECT id FROM variables WHERE name=?1", [name], |r| {
                r.get(0)
            })
            .unwrap();
        SymbolRef {
            var_id: id,
            name: name.into(),
            kind: "local".into(),
            fn_name: Some("f".into()),
            path: "/src/main.c".into(),
            line: 1,
            col: 1,
        }
    }
    #[test]
    fn collapse_preserves_branches_merges_cycles_and_visible_depth_in_both_directions() {
        let conn = database();
        let a = symbol(&conn, "a");
        let shallow = dataflow_view(&conn, std::slice::from_ref(&a), Direction::Down, 1).unwrap();
        assert!(shallow.truncated);
        let names: BTreeSet<_> = shallow.nodes.values().map(|e| e.name.as_str()).collect();
        assert_eq!(names, BTreeSet::from(["a", "b", "c", "_gep_user"]));
        assert!(shallow
            .edges
            .iter()
            .any(|e| e.operations.contains(&"read pointer".into())));
        let complete = dataflow_view(&conn, &[a], Direction::Down, 2).unwrap();
        assert!(
            !complete.truncated,
            "the only boundary edge revisits the root"
        );
        // Closing cycle edges at the boundary must still be represented.
        assert!(complete
            .edges
            .iter()
            .any(|e| e.from == PagNodeId(3) && e.to == PagNodeId(5)));
        let up = dataflow_view(&conn, &[symbol(&conn, "d")], Direction::Up, 2).unwrap();
        assert!(!up.truncated);
        assert!(up
            .edges
            .iter()
            .any(|e| e.from == PagNodeId(0) && e.to == PagNodeId(3)));
        assert!(up
            .edges
            .iter()
            .any(|e| e.from == PagNodeId(0) && e.to == PagNodeId(4)));
        assert!(
            complete
                .edges
                .iter()
                .any(|e| e.from == PagNodeId(5) && e.to == PagNodeId(0)),
            "closing cycle edge survives the depth boundary"
        );
        assert!(!complete.nodes.contains_key(&PagNodeId(1)));
        let cands = crate::find_symbols_at(&conn, "main.c", 2, 1).unwrap();
        assert!(!cands
            .iter()
            .any(|e| e.name == "internal" || e.name == "hidden"));
        assert!(crate::find_symbols_at(&conn, "main.c", 6, 1)
            .unwrap()
            .iter()
            .any(|e| e.name == "_gep_user"));
    }
    #[test]
    fn formats_describe_the_same_oriented_graph() {
        let conn = database();
        let view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 3).unwrap();
        let meta = GraphMeta {
            title: "flows",
            direction: "down",
            depth: 3,
            summary: "",
        };
        let json: serde_json::Value =
            serde_json::from_str(&render_dataflow(&view, RenderFormat::Json, &meta)).unwrap();
        assert_eq!(json["edges"].as_array().unwrap().len(), view.edges.len());
        for format in [
            RenderFormat::Text,
            RenderFormat::Graphviz,
            RenderFormat::Mermaid,
        ] {
            let output = render_dataflow(&view, format, &meta);
            assert!(!output.contains("internal"));
            assert!(!output.contains("points_to"));
            assert!(output.contains("_gep_user"));
            assert!(output.contains("read pointer"));
            if format == RenderFormat::Graphviz {
                assert_eq!(output.matches(" -> ").count(), view.edges.len());
            }
            if format == RenderFormat::Mermaid {
                assert_eq!(output.matches(" -->").count(), view.edges.len());
            }
        }
    }
    #[test]
    fn text_preserves_traversal_scopes_and_edges_through_cycles_and_ties() {
        let conn = database();
        conn.execute_batch(
            "INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined)
             VALUES(1,'right',0,10,20,'internal','right()',1),
                   (2,'left',0,20,30,'internal','left()',1),
                   (3,'merge',0,30,40,'internal','merge()',1),
                   (4,'leaf',0,40,50,'internal','leaf()',1);
             UPDATE variables SET fn_id=2 WHERE id=3;
             UPDATE variables SET fn_id=1 WHERE id=4;
             UPDATE variables SET fn_id=3 WHERE id=5;
             UPDATE variables SET fn_id=4 WHERE id=6;
             UPDATE flow_nodes SET fn_id=(SELECT fn_id FROM variables WHERE id=var_id)
             WHERE var_id IS NOT NULL;",
        ).unwrap();
        for (direction, word, root, expected) in [
            (
                Direction::Down,
                "down",
                "a",
                vec!["fn:0", "fn:2", "fn:1", "fn:4", "fn:3"],
            ),
            (
                Direction::Up,
                "up",
                "d",
                vec!["fn:3", "fn:2", "fn:1", "fn:0"],
            ),
        ] {
            let mut view = dataflow_view(&conn, &[symbol(&conn, root)], direction, 2).unwrap();
            // Parallel transitions between the same entities retain distinct
            // operation sites and tie deterministically, independently of input order.
            let mut parallel = view
                .edges
                .iter()
                .find(|e| e.from == PagNodeId(3))
                .unwrap()
                .clone();
            parallel.location = FlowSite {
                path: "/src/main.c".into(),
                line: 7,
                col: 2,
            };
            parallel.spelling = Some(FlowSite {
                path: "/src/macros.h".into(),
                line: 8,
                col: 3,
            });
            parallel.provenance.push(FlowOperationSite {
                callee_fn_id: None,
                arg_index: None,
                operation: "assign".into(),
                location: FlowSite {
                    path: "/src/main.c".into(),
                    line: 9,
                    col: 4,
                },
                expression: "d = b".into(),
            });
            view.edges.push(parallel);
            let meta = GraphMeta {
                title: "flows",
                direction: word,
                depth: 2,
                summary: "",
            };
            let before = render_dataflow(&view, RenderFormat::Json, &meta);
            let text = render_dataflow(&view, RenderFormat::Text, &meta);
            assert_eq!(render_dataflow(&view, RenderFormat::Json, &meta), before);
            assert!(!view.truncated);
            assert!(!text.contains("truncated"));
            let groups = ordered_scope_transitions(&view, direction == Direction::Up);
            assert_eq!(
                groups.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
                expected
            );
            assert_eq!(text.matches(" → ").count(), view.edges.len());
            for (key, edges) in groups {
                assert_eq!(
                    text.matches(&format!("\n{}:\n", view.scopes[key].text_display()))
                        .count(),
                    1
                );
                for e in edges {
                    let origin = if direction == Direction::Up {
                        e.to
                    } else {
                        e.from
                    };
                    assert_eq!(view.nodes[&origin].scope, key);
                }
            }
            assert!(text.contains("macro spelling macros.h:8:3"));
            assert!(text.contains("assign at main.c:9:4"));
            // Even an upstream query keeps the closing cycle source → destination.
            let cycle = text
                .lines()
                .find(|line| line.contains("d [var#5]") && line.contains("a [var#0]"))
                .unwrap();
            assert!(cycle.find("d [var#5]").unwrap() < cycle.find("a [var#0]").unwrap());
            view.edges.reverse();
            assert_eq!(render_dataflow(&view, RenderFormat::Text, &meta), text);
        }
        let shallow = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        let meta = GraphMeta {
            title: "flows",
            direction: "down",
            depth: 1,
            summary: "",
        };
        let text = render_dataflow(&shallow, RenderFormat::Text, &meta);
        assert_eq!(text.matches(" → ").count(), shallow.edges.len());
        assert!(text.ends_with("(truncated at visible depth limit; increase --depth)\n"));
    }

    #[test]
    fn all_formats_order_transitions_by_source_position_within_traversal_scopes() {
        let mut view = DataflowView::default();
        for (id, name) in [(9, "selected_scope"), (1, "other_scope")] {
            view.scopes.insert(
                format!("fn:{id}"),
                FlowScope {
                    fn_id: Some(FnId(id)),
                    name: name.into(),
                    location: FlowSite::default(),
                    signature: String::new(),
                    target_id: None,
                    parameters: Vec::new(),
                },
            );
        }
        for (id, owner) in [(9, 9), (1, 1), (2, 9), (3, 1)] {
            view.nodes.insert(
                PagNodeId(id),
                FlowEntity {
                    id: PagNodeId(id),
                    var_id: Some(VarId(id)),
                    fn_id: Some(FnId(owner)),
                    name: format!("value_{id}"),
                    kind: "variable".into(),
                    scope: format!("fn:{owner}"),
                    location: FlowSite::default(),
                    depth: if id == 9 { 0 } else { 1 },
                    call_site_id: None,
                    callees: Vec::new(),
                    targets: Vec::new(),
                    spelling: None,
                },
            );
        }
        // The input and BFS orders conflict with source order. File paths
        // precede line/column, while unknown locations come last.
        for (from, to, name, path, line, col) in [
            (9, 1, "late", "/src/a.c", 100, 1),
            (9, 2, "column_late", "/src/a.c", 20, 9),
            (9, 2, "path_z", "/src/z.c", 20, 2),
            (9, 2, "tie_b", "/src/a.c", 20, 2),
            (9, 2, "tie_a", "/src/a.c", 20, 2),
            (2, 9, "missing_main", "", 0, 0),
            (1, 3, "early_other", "/src/a.c", 1, 1),
            (3, 1, "missing_other", "/src/a.c", 0, 99),
            (1, 9, "return_other", "/src/a.c", 8, 1),
        ] {
            view.edges.push(FlowStep {
                from: PagNodeId(from),
                to: PagNodeId(to),
                operations: vec![name.into()],
                provenance: Vec::new(),
                location: FlowSite {
                    path: path.into(),
                    line,
                    col,
                },
                expression: String::new(),
                call_site_id: None,
                arg_index: None,
                callee_fn_id: None,
                spelling: None,
                scope: "fn:9".into(),
            });
        }
        // Sort by the prominent recorded site, never macro spelling or an
        // earlier constituent operation in collapsed provenance.
        view.edges[0].spelling = Some(FlowSite {
            path: "/src/macros.h".into(),
            line: 1,
            col: 1,
        });
        view.edges[0].provenance.push(FlowOperationSite {
            callee_fn_id: None,
            arg_index: None,
            operation: "assign".into(),
            location: FlowSite {
                path: "/src/a.c".into(),
                line: 2,
                col: 1,
            },
            expression: "original operation".into(),
        });
        view.truncated = true;
        for (direction, expected) in [
            (
                "down",
                vec![
                    "tie_a",
                    "tie_b",
                    "column_late",
                    "late",
                    "path_z",
                    "missing_main",
                    "early_other",
                    "return_other",
                    "missing_other",
                ],
            ),
            (
                "up",
                vec![
                    "return_other",
                    "tie_a",
                    "tie_b",
                    "column_late",
                    "path_z",
                    "missing_main",
                    "early_other",
                    "late",
                    "missing_other",
                ],
            ),
        ] {
            let meta = GraphMeta {
                title: "flows",
                direction,
                depth: 3,
                summary: "",
            };
            let groups = ordered_scope_transitions(&view, direction == "up");
            let legacy_meta = GraphMeta {
                direction: if direction == "up" {
                    "flows-from"
                } else {
                    "flows-to"
                },
                ..meta
            };
            // Legacy Rust rendering callers retain traversal scope order;
            // the CLI's versioned contract emits down/up.
            let mut legacy: serde_json::Value =
                serde_json::from_str(&render_dataflow(&view, RenderFormat::Json, &legacy_meta))
                    .unwrap();
            legacy["direction"] = direction.into();
            let current: serde_json::Value =
                serde_json::from_str(&render_dataflow(&view, RenderFormat::Json, &meta)).unwrap();
            assert_eq!(current["schema"], "dataflow-source-v1");
            assert_eq!(legacy, current);
            assert_eq!(
                groups.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
                ["fn:9", "fn:1"]
            );
            let ordered: Vec<_> = groups
                .iter()
                .flat_map(|(_, edges)| edges)
                .map(|e| (**e).clone())
                .collect();
            assert_eq!(
                ordered
                    .iter()
                    .map(|e| e.operations[0].as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            for format in [
                RenderFormat::Text,
                RenderFormat::Json,
                RenderFormat::Graphviz,
                RenderFormat::Mermaid,
            ] {
                let unchanged = serde_json::to_value(&view).unwrap();
                let output = render_dataflow(&view, format, &meta);
                if format == RenderFormat::Json {
                    let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
                    let edges = doc["edges"].as_array().unwrap();
                    assert_eq!(edges.len(), ordered.len());
                    for (item, edge) in edges.iter().zip(&ordered) {
                        assert_eq!(item["from"], edge.from.0);
                        assert_eq!(item["to"], edge.to.0);
                        assert_eq!(item["expression"], edge.expression);
                        assert_eq!(
                            item["location"],
                            serde_json::to_value(&edge.location).unwrap()
                        );
                        assert!(item["operations"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|op| op["kind"] == edge.operations[0]));
                    }
                    let nodes = doc["nodes"].as_array().unwrap();
                    assert_eq!(nodes.len(), view.nodes.len());
                    for (item, node) in nodes.iter().zip(view.nodes.values()) {
                        assert_eq!(item["id"], node.id.0);
                        assert_eq!(item["depth"], node.depth);
                    }
                    assert_eq!(doc["truncated"], true);
                    assert_eq!(doc["direction"], direction);
                    assert_eq!(doc["depth"], 3);
                } else {
                    let rows: Vec<_> = output
                        .lines()
                        .filter(|line| match format {
                            RenderFormat::Text => line.contains(" → "),
                            RenderFormat::Graphviz => line.contains(" -> "),
                            RenderFormat::Mermaid => line.contains(" -->"),
                            _ => unreachable!(),
                        })
                        .collect();
                    assert_eq!(rows.len(), expected.len());
                    for ((row, name), edge) in rows.iter().zip(&expected).zip(&ordered) {
                        assert!(row.contains(name), "{output}");
                        assert!(row.starts_with("  ") && !row.starts_with("   "));
                        if format == RenderFormat::Graphviz {
                            assert!(row.contains(&format!("n{} -> n{}", edge.from.0, edge.to.0)));
                        } else if format == RenderFormat::Mermaid {
                            assert!(row.contains(&format!("n{} -->", edge.from.0)));
                            assert!(row.ends_with(&format!(" n{}", edge.to.0)));
                        }
                    }
                    assert!(output.contains("truncated at visible depth limit"));
                    if format == RenderFormat::Text {
                        assert!(
                            output.find("\nselected_scope").unwrap()
                                < output.find("\nother_scope").unwrap()
                        );
                    } else {
                        // Graph scope declarations retain their existing key
                        // order, independent of edge declaration ordering.
                        assert!(
                            output.find("other_scope").unwrap()
                                < output.find("selected_scope").unwrap()
                        );
                    }
                }
                assert_eq!(serde_json::to_value(&view).unwrap(), unchanged);
                view.edges.reverse();
                assert_eq!(render_dataflow(&view, format, &meta), output);
                view.edges.rotate_left(3);
                assert_eq!(render_dataflow(&view, format, &meta), output);
            }
        }
    }

    #[test]
    fn render_ordering_preserves_edges_not_visited_with_the_supplied_direction() {
        let conn = database();
        let view = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 1).unwrap();
        // All edges leave the root; upstream metadata cannot traverse them.
        let meta = GraphMeta {
            title: "flows",
            direction: "up",
            depth: 1,
            summary: "",
        };
        let expected = ordered_scope_transitions(&view, true);
        let edges: Vec<_> = expected.iter().flat_map(|(_, edges)| edges).collect();
        assert_eq!(edges.len(), view.edges.len());
        for format in [
            RenderFormat::Text,
            RenderFormat::Json,
            RenderFormat::Graphviz,
            RenderFormat::Mermaid,
        ] {
            let output = render_dataflow(&view, format, &meta);
            match format {
                RenderFormat::Json => {
                    let doc: serde_json::Value = serde_json::from_str(&output).unwrap();
                    let rendered = doc["edges"].as_array().unwrap();
                    assert_eq!(rendered.len(), edges.len());
                    for (item, edge) in rendered.iter().zip(&edges) {
                        assert_eq!(item["from"], edge.from.0);
                        assert_eq!(item["to"], edge.to.0);
                        assert_eq!(
                            item["location"],
                            serde_json::to_value(&edge.location).unwrap()
                        );
                    }
                    assert_eq!(doc["truncated"], view.truncated);
                }
                RenderFormat::Text => assert_eq!(output.matches(" → ").count(), view.edges.len()),
                RenderFormat::Graphviz => {
                    assert_eq!(output.matches(" -> ").count(), view.edges.len())
                }
                RenderFormat::Mermaid => {
                    assert_eq!(output.matches(" -->").count(), view.edges.len())
                }
            }
        }
    }

    #[test]
    fn retains_targets_function_values_allocations_constants_and_clearing_events() {
        let conn = database();
        conn.execute_batch("INSERT INTO link_targets VALUES(0,'first','first.so'),(1,'second','second.so');
            UPDATE functions SET target_id=0 WHERE id=0;
            INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined,target_id) VALUES(1,'f',0,20,30,'internal','f()',1,1);
            INSERT INTO variables(id,name,kind,fn_id,type_id,file_id,line,col) VALUES(8,'other','param',1,0,0,20,1);
            INSERT INTO call_sites(id,caller_fn_id,file_id,line,col,callee_text,is_direct) VALUES(0,0,0,10,3,'clear',1);
            INSERT INTO flow_nodes(id,kind,label,detail,fn_id,call_site_id) VALUES(8,'terminator','clear clears arg0','',0,0),(9,'loc','allocation','heap',0,NULL),(10,'loc','constant','string_lit',NULL,NULL),(11,'loc','f','function',1,NULL);
            INSERT INTO flow_nodes(id,kind,label,var_id,fn_id) VALUES(12,'var','other',8,1);
            INSERT INTO flow_edges VALUES(20,0,8,'terminates'),(21,9,0,'addr_of'),(22,10,0,'addr_of'),(23,11,0,'addr_of'),(24,0,12,'copy');").unwrap();
        let down = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 3).unwrap();
        assert_eq!(down.scopes["fn:0"].target_id, Some(TargetId(0)));
        assert_eq!(down.scopes["fn:1"].target_id, Some(TargetId(1)));
        let clear = down.edges.iter().find(|e| e.to == PagNodeId(8)).unwrap();
        assert_eq!(clear.call_site_id, Some(CallSiteId(0)));
        assert_eq!(
            clear.location,
            FlowSite {
                path: "/src/main.c".into(),
                line: 10,
                col: 3
            }
        );
        assert!(clear.operations.contains(&"clear value".into()));
        assert!(down.nodes[&PagNodeId(8)].targets.is_empty());
        // A clearing event can also share a resolved call site. The batched
        // lookup retains its targets as well as the call-target node's targets.
        conn.execute_batch("INSERT INTO call_edges(id,call_site_id,caller_fn_id,callee_fn_id,resolution) VALUES(0,0,0,1,'direct');
            INSERT INTO flow_nodes(id,kind,label,fn_id,call_site_id) VALUES(13,'call_target','clear',0,0);
            INSERT INTO flow_edges VALUES(25,0,13,'copy');").unwrap();
        let resolved = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Down, 3).unwrap();
        for id in [8, 13] {
            assert_eq!(resolved.nodes[&PagNodeId(id)].callees, vec![FnId(1)]);
            assert_eq!(resolved.nodes[&PagNodeId(id)].targets[0].name, "f");
        }
        let up = dataflow_view(&conn, &[symbol(&conn, "a")], Direction::Up, 3).unwrap();
        for (id, kind) in [(9, "allocation"), (10, "constant"), (11, "function")] {
            assert_eq!(up.nodes[&PagNodeId(id)].kind, kind);
            if kind == "function" {
                let entity = &up.nodes[&PagNodeId(id)];
                assert_eq!(
                    entity.location,
                    FlowSite {
                        path: "/src/main.c".into(),
                        line: 20,
                        col: 0
                    }
                );
                let meta = GraphMeta {
                    title: "function",
                    summary: "",
                    direction: "up",
                    depth: 3,
                };
                let json: serde_json::Value =
                    serde_json::from_str(&render_dataflow(&up, RenderFormat::Json, &meta)).unwrap();
                assert!(json["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|node| node["kind"] == "function" && node["location"]["line"] == 20));
                let dot = render_dataflow(&up, RenderFormat::Graphviz, &meta);
                assert!(dot.lines().any(|line| line.contains("n11 [")
                    && line.contains("path=\"/src/main.c\", line=20, col=0")));
            }
        }
    }
}
