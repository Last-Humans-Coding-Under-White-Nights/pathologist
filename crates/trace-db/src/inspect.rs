//! Query layer behind `trace inspect`: locate entities by source position
//! and construct call / dataflow graphs as bounded BFS traversals.
//!
//! All lookups work purely off the exported SQLite database — no re-analysis.

use anyhow::{bail, Result};
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

pub use trace_analysis::LocKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Forward: callees (call graph) / where a value flows to (dataflow).
    Down,
    /// Backward: callers (call graph) / where a value comes from (dataflow).
    Up,
}

impl Direction {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "down" => Ok(Direction::Down),
            "up" => Ok(Direction::Up),
            other => bail!("invalid direction `{other}` (expected `up` or `down`)"),
        }
    }
}

/// Final path component of `path`, accepting either separator.
///
/// Both separators everywhere: a database is portable, so a Windows-produced
/// one is often inspected on Linux. A Unix name holding a backslash renders
/// short, which is display-only.
pub fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[derive(Debug, Clone)]
pub struct FunctionRef {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub is_defined: bool,
}

impl std::fmt::Display for FunctionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let base = basename(&self.path);
        write!(
            f,
            "{} ({}:{}-{}){}",
            self.name,
            base,
            self.line_start,
            self.line_end,
            if self.is_defined { "" } else { " [external]" }
        )
    }
}

/// Node kind of a `flow_nodes` row, as categorized for value-flow graphs.
/// Call graphs have no PAG node kinds (their nodes are functions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowNodeKind {
    Var,
    Loc,
    CallTarget,
    Terminator,
}

impl FlowNodeKind {
    fn from_schema_str(s: &str) -> Option<Self> {
        match s {
            "var" => Some(Self::Var),
            "loc" => Some(Self::Loc),
            "call_target" => Some(Self::CallTarget),
            "terminator" => Some(Self::Terminator),
            _ => None,
        }
    }
}

/// Map the schema's `flow_nodes.detail` string for `loc` nodes back to the
/// abstract-location kind it was exported from (`LocKind`).
pub fn loc_kind_from_schema_str(s: &str) -> Option<LocKind> {
    match s {
        "global" => Some(LocKind::Global),
        "file_static" => Some(LocKind::FileStatic),
        "fn_static" => Some(LocKind::FnStatic),
        "local" => Some(LocKind::Local),
        "heap" => Some(LocKind::Heap),
        "field" => Some(LocKind::Field),
        "field_summary" => Some(LocKind::FieldSummary),
        "array_summary" => Some(LocKind::ArraySummary),
        "function" => Some(LocKind::Function),
        "string_lit" => Some(LocKind::StringLit),
        _ => None,
    }
}

/// A graph query result: flat node/edge sets plus BFS discovery order so the
/// CLI can print an indented view without re-traversing.
#[derive(Debug, Default)]
pub struct QueryGraph {
    pub nodes: FxHashMap<i64, GraphNode>,
    /// `(node id, depth)` in discovery order; each node appears once.
    pub order: Vec<(i64, u32)>,
    /// Traversal edges; labels are call resolutions for the call graph and
    /// constraint kinds for dataflow. Cross-edge revisits included once per
    /// (from, to) pair at first discovery.
    pub edges: Vec<GraphEdge>,
    /// True when unvisited neighbors remained at the depth limit.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: i64,
    pub label: String,
    pub detail: String,
    /// PAG node kind for value-flow graphs; `None` for call graphs (whose
    /// nodes are functions, not PAG nodes).
    pub kind: Option<FlowNodeKind>,
    /// Abstract-location category for `kind == Some(FlowNodeKind::Loc)`,
    /// e.g. `Heap` / `Field`; `None` otherwise.
    pub loc_kind: Option<LocKind>,
}

#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub from: i64,
    pub to: i64,
    pub label: String,
    /// Call/flow source site attached to the edge (empty when the edge has no
    /// site, e.g. value-flow edges). Render it with `EdgeSite::display`.
    pub site: EdgeSite,
}

/// Call/flow source site attached to a graph edge: full source path plus
/// 1-based line/col (both 0 when there is no site).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EdgeSite {
    pub path: String,
    pub line: i64,
    pub col: i64,
}

impl EdgeSite {
    /// True when the edge has no source site.
    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
    }

    /// The display form used by the renderers: `basename:line` (empty when
    /// there is no site).
    pub fn display(&self) -> String {
        if self.path.is_empty() {
            String::new()
        } else {
            let base = basename(&self.path);
            format!("{base}:{}", self.line)
        }
    }
}

/// Functions whose `[line_start, line_end]` range contains `line` in files
/// whose path contains `file_substring`, best match first (definitions
/// before prototypes, narrower ranges before wider ones). Empty when nothing
/// contains the line.
pub fn find_functions_at(
    conn: &Connection,
    file_substring: &str,
    line: i64,
) -> Result<Vec<FunctionRef>> {
    if file_substring.is_empty() {
        bail!("file filter must not be empty");
    }
    let mut stmt = conn.prepare(
        "SELECT f.id, f.name, p.path, f.line_start, f.line_end, f.is_defined \
         FROM functions f JOIN files p ON p.id = f.file_id \
         WHERE p.path LIKE ?1 AND f.line_start <= ?2 AND f.line_end >= ?2",
    )?;
    let pattern = format!("%{file_substring}%");
    let rows = stmt.query_map(rusqlite::params![pattern, line], |row| {
        Ok(FunctionRef {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            line_start: row.get(3)?,
            line_end: row.get(4)?,
            is_defined: row.get::<_, i64>(5)? != 0,
        })
    })?;
    let mut out: Vec<FunctionRef> = rows.collect::<std::result::Result<_, _>>()?;
    out.sort_by_key(|f| {
        (
            std::cmp::Reverse(f.is_defined),
            f.line_end - f.line_start,
            f.name.clone(),
        )
    });
    Ok(out)
}

pub fn load_function_labels(conn: &Connection) -> Result<FxHashMap<i64, GraphNode>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.name, p.path, f.line_start, f.is_defined \
         FROM functions f JOIN files p ON p.id = f.file_id",
    )?;
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let name: String = row.get(1)?;
        let path: String = row.get(2)?;
        let line: i64 = row.get(3)?;
        let defined: i64 = row.get(4)?;
        let file_name = basename(&path).to_string();
        Ok((
            id,
            GraphNode {
                id,
                label: name,
                detail: if defined != 0 {
                    format!("{file_name}:{line}")
                } else {
                    format!("{file_name}:{line} [external]")
                },
                kind: None,
                loc_kind: None,
            },
        ))
    })?;
    let mut map = FxHashMap::default();
    for r in rows {
        let (id, node) = r?;
        map.insert(id, node);
    }
    Ok(map)
}

type Adjacency = FxHashMap<i64, Vec<(i64, &'static str, EdgeSite)>>;

fn load_call_adjacency(conn: &Connection, dir: Direction) -> Result<Adjacency> {
    let mut stmt = conn.prepare(
        "SELECT ce.caller_fn_id, ce.callee_fn_id, ce.resolution, p.path, cs.line, cs.col \
         FROM call_edges ce \
         LEFT JOIN call_sites cs ON cs.id = ce.call_site_id \
         LEFT JOIN files p ON p.id = cs.file_id",
    )?;
    let mut adj: Adjacency = FxHashMap::default();
    let rows = stmt.query_map([], |row| {
        // Synthetic edges (IPC bridges) have no call site; render them with a
        // placeholder location rather than mis-attributing a real call site.
        let path: Option<String> = row.get(3)?;
        let line: Option<i64> = row.get(4)?;
        let col: Option<i64> = row.get(5)?;
        let site = match (&path, line, col) {
            (Some(p), Some(l), Some(c)) => EdgeSite {
                path: p.clone(),
                line: l,
                col: c,
            },
            _ => EdgeSite::default(),
        };
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            site,
        ))
    })?;
    for r in rows {
        let (caller, callee, resolution, site) = r?;
        match dir {
            Direction::Down => {
                adj.entry(caller)
                    .or_default()
                    .push((callee, leak_resolution(&resolution), site))
            }
            Direction::Up => {
                adj.entry(callee)
                    .or_default()
                    .push((caller, leak_resolution(&resolution), site))
            }
        }
    }
    Ok(adj)
}

fn leak_resolution(resolution: &str) -> &'static str {
    match resolution {
        "direct" => "direct",
        "indirect" => "indirect",
        "ambiguous" => "ambiguous",
        "external" => "external",
        "ipc" => "ipc",
        _ => "call",
    }
}

/// True when `table` exists. Table names here are compile-time constants.
fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(n != 0)
}

/// True when `table` has a `column`. Table names are compile-time constants;
/// the column name is bound as a parameter.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let sql = format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1");
    let n: i64 = conn.query_row(&sql, [column], |r| r.get(0))?;
    Ok(n != 0)
}

/// Require the schema-v3 caller column used by synthetic call edges.
pub fn require_call_edge_caller(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "call_edges", "caller_fn_id")? {
        bail!(
            "`call_edges.caller_fn_id` missing: database predates synthetic call-edge export; \
             re-run `trace analyze` with this binary"
        );
    }
    Ok(())
}

/// Filter for `call_edges`, mirroring `trace calls --from/--to/--file`.
pub struct CallEdgeFilter<'a> {
    /// Substring/qualified-suffix filter on the caller name.
    pub from: Option<&'a str>,
    /// Substring/qualified-suffix filter on the callee name.
    pub to: Option<&'a str>,
    /// File-substring filter matching the call-site path, the callee path, or
    /// (for site-less edges) the caller's own path.
    pub file: Option<&'a str>,
}

/// One call edge row, including the caller's own source path (needed for
/// displaying synthetic IPC-bridge edges, which carry no call site).
pub struct CallEdgeRef {
    pub caller_id: i64,
    pub caller_name: String,
    pub caller_path: String,
    /// Call-site source path; `None` for synthetic (IPC bridge) edges.
    pub call_site_path: Option<String>,
    /// 1-based call-site line; `None` for synthetic edges.
    pub call_site_line: Option<i64>,
    /// 1-based call-site column; `None` for synthetic edges.
    pub call_site_col: Option<i64>,
    pub callee_name: String,
    pub callee_path: String,
    pub resolution: String,
}

/// All call edges (including synthetic IPC bridge edges) filtered per
/// [`CallEdgeFilter`]. Real call sites sort first; synthetic edges have no
/// source site so they sort after.
pub fn call_edges(conn: &Connection, filter: &CallEdgeFilter<'_>) -> Result<Vec<CallEdgeRef>> {
    require_call_edge_caller(conn)?;
    let mut sql = String::from(
        "SELECT caller.id, caller.name, caller_f.path, csf.path, cs.line, cs.col, callee.name, \
                 callee_f.path, ce.resolution \
                 FROM call_edges ce \
                 LEFT JOIN call_sites cs ON cs.id = ce.call_site_id \
                 LEFT JOIN files csf ON csf.id = cs.file_id \
                 JOIN functions caller ON caller.id = ce.caller_fn_id \
                 JOIN files caller_f ON caller_f.id = caller.file_id \
                 JOIN functions callee ON callee.id = ce.callee_fn_id \
                 JOIN files callee_f ON callee_f.id = callee.file_id WHERE 1=1",
    );
    let mut params: Vec<String> = Vec::new();
    if let Some(f) = filter.from {
        push_fn_name_filter(&mut sql, &mut params, "caller.name", f);
    }
    if let Some(t) = filter.to {
        push_fn_name_filter(&mut sql, &mut params, "callee.name", t);
    }
    if let Some(p) = filter.file {
        params.push(format!("%{}%", like_escape(p)));
        let n = params.len();
        sql.push_str(&format!(
            " AND (csf.path LIKE ?{n} ESCAPE '!' OR callee_f.path LIKE ?{n} ESCAPE '!' OR \
             (ce.call_site_id IS NULL AND caller_f.path LIKE ?{n} ESCAPE '!'))"
        ));
    }
    // Sort real call sites first; synthetic (IPC bridge) edges have a NULL
    // path/line so SQLite would otherwise sort them to the top.
    sql.push_str(" ORDER BY CASE WHEN csf.path IS NULL THEN 1 ELSE 0 END, csf.path, cs.line");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        let line: Option<i64> = row.get(4)?;
        let col: Option<i64> = row.get(5)?;
        Ok(CallEdgeRef {
            caller_id: row.get(0)?,
            caller_name: row.get(1)?,
            caller_path: row.get(2)?,
            call_site_path: row.get(3)?,
            call_site_line: line,
            call_site_col: col,
            callee_name: row.get(6)?,
            callee_path: row.get(7)?,
            resolution: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Exact name or C++ qualified suffix (`Foo::Bar` matches `--from Bar`).
/// User text is escaped so SQLite `LIKE` wildcards `_` / `%` are literal.
fn push_fn_name_filter(sql: &mut String, params: &mut Vec<String>, column: &str, name: &str) {
    params.push(name.to_string());
    let eq = params.len();
    params.push(format!("%::{}", like_escape(name)));
    let like = params.len();
    params.push(format!("::{name}"));
    let suffix = params.len();
    sql.push_str(&format!(
        " AND ({column} = ?{eq} OR ({column} LIKE ?{like} ESCAPE '!' AND SUBSTR({column}, -LENGTH(?{suffix})) = ?{suffix}))"
    ));
}

fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '!' | '%' | '_' => {
                out.push('!');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn require_flow_tables(conn: &Connection) -> Result<()> {
    for t in ["flow_nodes", "flow_edges"] {
        if !table_exists(conn, t)? {
            bail!(
                "`{t}` missing: database predates flow-graph export; \
                 re-run `trace analyze` with this binary"
            );
        }
    }
    Ok(())
}

/// Bounded BFS over the call graph from `root_fn_id`. Down follows caller →
/// callee edges, up follows callee → caller edges.
pub fn call_graph(
    conn: &Connection,
    root_fn_id: i64,
    dir: Direction,
    max_depth: u32,
) -> Result<QueryGraph> {
    require_call_edge_caller(conn)?;
    let labels = load_function_labels(conn)?;
    if !labels.contains_key(&root_fn_id) {
        bail!("function id {root_fn_id} not found in database");
    }
    let adj = load_call_adjacency(conn, dir)?;

    let mut graph = QueryGraph::default();
    let mut visited: FxHashSet<i64> = FxHashSet::default();
    struct Entry {
        id: i64,
        depth: u32,
    }
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(Entry {
        id: root_fn_id,
        depth: 0,
    });
    visited.insert(root_fn_id);

    while let Some(Entry { id, depth }) = queue.pop_front() {
        graph.nodes.insert(id, labels[&id].clone());
        graph.order.push((id, depth));
        if depth == max_depth {
            // Truncated only if an actually-unvisited neighbor was cut off;
            // neighbors already reached earlier in the BFS don't count.
            if let Some(neighbors) = adj.get(&id) {
                if neighbors.iter().any(|(to, _, _)| !visited.contains(to)) {
                    graph.truncated = true;
                }
            }
            continue;
        }
        if let Some(neighbors) = adj.get(&id) {
            for (to, label, site) in neighbors {
                graph.edges.push(GraphEdge {
                    from: id,
                    to: *to,
                    label: (*label).to_string(),
                    site: site.clone(),
                });
                if visited.insert(*to) {
                    queue.push_back(Entry {
                        id: *to,
                        depth: depth + 1,
                    });
                }
            }
        }
    }
    // Collapse exact duplicates (same pair, same annotation, same exact
    // source site). The site is compared on the full path/line/col — not the
    // `basename:line` display form — or two calls on one line (different
    // columns) or same-basename files in different directories would collapse
    // into a single distinct edge.
    graph.edges.dedup_by(|a, b| {
        a.from == b.from && a.to == b.to && a.label == b.label && a.site == b.site
    });
    Ok(graph)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallChainEdge {
    pub caller_id: i64,
    pub callee_id: i64,
    pub resolution: String,
    pub site: EdgeSite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallChain {
    pub nodes: Vec<i64>,
    pub edges: Vec<CallChainEdge>,
}

impl CallChain {
    pub fn depth(&self) -> usize {
        self.edges.len()
    }
}

#[derive(Debug, Default, Clone)]
pub struct CallChainsResult {
    pub chains: Vec<CallChain>,
    pub truncated: bool,
}

impl CallChainsResult {
    pub fn to_query_graph(&self, conn: &Connection) -> Result<QueryGraph> {
        let labels = load_function_labels(conn)?;
        Ok(self.to_query_graph_with_labels(&labels))
    }

    pub fn to_query_graph_with_labels(&self, labels: &FxHashMap<i64, GraphNode>) -> QueryGraph {
        let mut graph = QueryGraph {
            truncated: self.truncated,
            ..Default::default()
        };

        let mut node_min_depth: FxHashMap<i64, u32> = FxHashMap::default();

        for chain in &self.chains {
            for (d, &node_id) in chain.nodes.iter().enumerate() {
                if let Some(lbl) = labels.get(&node_id) {
                    graph.nodes.insert(node_id, lbl.clone());
                }
                let entry = node_min_depth.entry(node_id).or_insert(d as u32);
                if (d as u32) < *entry {
                    *entry = d as u32;
                }
            }
            for edge in &chain.edges {
                graph.edges.push(GraphEdge {
                    from: edge.caller_id,
                    to: edge.callee_id,
                    label: edge.resolution.clone(),
                    site: edge.site.clone(),
                });
            }
        }

        graph.edges.sort_by(|a, b| {
            (
                a.from,
                a.to,
                &a.label,
                &a.site.path,
                a.site.line,
                a.site.col,
            )
                .cmp(&(
                    b.from,
                    b.to,
                    &b.label,
                    &b.site.path,
                    b.site.line,
                    b.site.col,
                ))
        });
        graph.edges.dedup_by(|a, b| {
            a.from == b.from && a.to == b.to && a.label == b.label && a.site == b.site
        });

        let mut ordered_nodes: Vec<(i64, u32)> = node_min_depth.into_iter().collect();
        ordered_nodes.sort_by_key(|&(id, d)| (d, id));
        graph.order = ordered_nodes;

        graph
    }
}

/// Find all simple call chains (paths) from `from_fn_id` to `to_fn_id` with length <= `max_depth`.
/// `dir` selects traversal direction: `Down` follows callers -> callees, `Up` follows callees -> callers.
pub fn call_chains(
    conn: &Connection,
    from_fn_id: i64,
    to_fn_id: i64,
    dir: Direction,
    max_depth: u32,
    limit: Option<usize>,
) -> Result<CallChainsResult> {
    require_call_edge_caller(conn)?;
    let labels = load_function_labels(conn)?;
    if !labels.contains_key(&from_fn_id) {
        bail!("start function id {from_fn_id} not found in database");
    }
    if !labels.contains_key(&to_fn_id) {
        bail!("target function id {to_fn_id} not found in database");
    }
    if max_depth == 0 {
        if from_fn_id == to_fn_id {
            return Ok(CallChainsResult {
                chains: vec![CallChain {
                    nodes: vec![from_fn_id],
                    edges: vec![],
                }],
                truncated: false,
            });
        }
        bail!("depth must be >= 1");
    }

    let mut adj = load_call_adjacency(conn, dir)?;
    for list in adj.values_mut() {
        list.sort_by(|a, b| {
            (a.0, a.1, &a.2.path, a.2.line, a.2.col).cmp(&(b.0, b.1, &b.2.path, b.2.line, b.2.col))
        });
        list.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
    }

    let max_chains = match limit {
        Some(0) | None => usize::MAX,
        Some(lim) => lim,
    };

    let mut chains = Vec::new();
    let mut truncated = false;

    if from_fn_id == to_fn_id {
        chains.push(CallChain {
            nodes: vec![from_fn_id],
            edges: vec![],
        });
    }

    let mut current_nodes = vec![from_fn_id];
    let mut current_edges = Vec::new();
    let mut active_nodes = FxHashSet::default();
    active_nodes.insert(from_fn_id);

    #[allow(clippy::too_many_arguments)]
    fn dfs(
        u: i64,
        target: i64,
        dir: Direction,
        depth: u32,
        max_depth: u32,
        adj: &Adjacency,
        current_nodes: &mut Vec<i64>,
        current_edges: &mut Vec<CallChainEdge>,
        active_nodes: &mut FxHashSet<i64>,
        chains: &mut Vec<CallChain>,
        truncated: &mut bool,
        max_chains: usize,
    ) {
        if *truncated {
            return;
        }
        if depth >= max_depth {
            return;
        }
        if let Some(neighbors) = adj.get(&u) {
            for (v, resolution, site) in neighbors {
                let (caller_id, callee_id) = match dir {
                    Direction::Down => (u, *v),
                    Direction::Up => (*v, u),
                };
                let edge = CallChainEdge {
                    caller_id,
                    callee_id,
                    resolution: (*resolution).to_string(),
                    site: site.clone(),
                };

                if *v == target {
                    if chains.len() >= max_chains {
                        *truncated = true;
                        return;
                    }
                    let mut path_nodes = current_nodes.clone();
                    path_nodes.push(*v);
                    let mut path_edges = current_edges.clone();
                    path_edges.push(edge.clone());
                    chains.push(CallChain {
                        nodes: path_nodes,
                        edges: path_edges,
                    });
                    continue;
                }

                if !active_nodes.contains(v) {
                    active_nodes.insert(*v);
                    current_nodes.push(*v);
                    current_edges.push(edge);

                    dfs(
                        *v,
                        target,
                        dir,
                        depth + 1,
                        max_depth,
                        adj,
                        current_nodes,
                        current_edges,
                        active_nodes,
                        chains,
                        truncated,
                        max_chains,
                    );

                    current_edges.pop();
                    current_nodes.pop();
                    active_nodes.remove(v);

                    if *truncated {
                        return;
                    }
                }
            }
        }
    }

    dfs(
        from_fn_id,
        to_fn_id,
        dir,
        0,
        max_depth,
        &adj,
        &mut current_nodes,
        &mut current_edges,
        &mut active_nodes,
        &mut chains,
        &mut truncated,
        max_chains,
    );

    chains.sort_by(|a, b| {
        a.edges
            .len()
            .cmp(&b.edges.len())
            .then_with(|| a.nodes.cmp(&b.nodes))
    });

    Ok(CallChainsResult { chains, truncated })
}

#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub var_id: i64,
    pub name: String,
    pub kind: String,
    pub fn_name: Option<String>,
    pub path: String,
    pub line: i64,
    pub col: i64,
}

impl std::fmt::Display for SymbolRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name, self.kind)?;
        if let Some(fn_name) = &self.fn_name {
            write!(f, " in {fn_name}")?;
        }
        let base = basename(&self.path);
        write!(f, " {base}:{}:{}", self.line, self.col)
    }
}

/// Variables declared on/near `line` in files matching `file_substring`,
/// best candidate first: same-line exact column hits (position inside the
/// declared identifier) rank before same-line nearest-column, then ±1/±2
/// line neighbors. Lookup matches **declaration** positions — variable uses
/// are not recorded in the IR — so query the declaration of interest.
pub fn find_symbols_at(
    conn: &Connection,
    file_substring: &str,
    line: i64,
    col: i64,
) -> Result<Vec<SymbolRef>> {
    if file_substring.is_empty() {
        bail!("file filter must not be empty");
    }
    if !column_exists(conn, "variables", "col")? {
        bail!(
            "`variables.col` missing: database predates declaration-column export; \
             re-run `trace analyze` with this binary"
        );
    }
    let mut stmt = conn.prepare(
        "SELECT v.id, v.name, v.kind, v.line, v.col, p.path, f.name \
         FROM variables v \
         JOIN files p ON p.id = v.file_id \
         LEFT JOIN functions f ON f.id = v.fn_id \
         WHERE p.path LIKE ?1 AND v.line BETWEEN ?2 - 2 AND ?2 + 2",
    )?;
    let pattern = format!("%{file_substring}%");
    let rows = stmt.query_map(rusqlite::params![pattern, line], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut candidates: Vec<(u32, i64, SymbolRef)> = Vec::new();
    for r in rows {
        let (var_id, name, kind, vline, vcol, path, fn_name) = r?;
        let name_len = name.len() as i64;
        let score = rank_symbol(line, col, vline, vcol, name_len);
        candidates.push((
            score,
            vcol,
            SymbolRef {
                var_id,
                name,
                kind,
                fn_name,
                path,
                line: vline,
                col: vcol,
            },
        ));
    }
    candidates.sort_by_key(|(score, vcol, sym)| (*score, *vcol, sym.var_id));
    Ok(candidates.into_iter().map(|(_, _, s)| s).collect())
}

/// Rank a declaration against the queried position; lower wins.
/// 0 = on this line inside the identifier, 1..5 = same line by column
/// distance bucket (25-column bands), 10/11/12 = one/two lines away.
fn rank_symbol(line: i64, col: i64, vline: i64, vcol: i64, name_len: i64) -> u32 {
    let line_dist = (vline - line).abs();
    if line_dist == 0 {
        if col >= vcol && col <= vcol + name_len {
            0
        } else {
            1 + (vcol.abs_diff(col)).min(99) as u32 / 25
        }
    } else {
        9 + line_dist.min(3) as u32
    }
}

/// Bounded BFS over the PAG value-flow graph (`flow_edges`). Down follows
/// src → dst (where the value flows), up follows reversed edges (where it
/// came from). Start nodes are every PAG node of the given variables (the
/// var node plus any storage/field location nodes mapped to it).
pub fn dataflow_graph(
    conn: &Connection,
    symbols: &[SymbolRef],
    dir: Direction,
    max_depth: u32,
) -> Result<QueryGraph> {
    require_flow_tables(conn)?;
    let var_ids: Vec<i64> = symbols.iter().map(|s| s.var_id).collect();
    let mut starts: Vec<i64> = Vec::new();
    for vid in &var_ids {
        let mut stmt =
            conn.prepare("SELECT id FROM flow_nodes WHERE var_id = ?1 ORDER BY kind, id")?;
        let rows = stmt.query_map([vid], |row| row.get::<_, i64>(0))?;
        for r in rows {
            starts.push(r?);
        }
    }
    // Parameter twins: the same C parameter is lowered once per TU that sees
    // its declaration, so arg-flow wiring may attach to the header-prototype
    // copy while the user queried the definition-site copy (or vice versa).
    // After merge all copies share one canonical function record, so twins
    // are same-name params under the *same* fn_id — widening must not reach
    // unrelated same-name functions (e.g. file-`static`s in other files).
    // Runs before the empty-start bail: a queried copy may lack flow nodes
    // entirely while its twin carries the graph.
    let touches_any = starts.iter().any(|&n| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM flow_edges WHERE src_node = ?1 OR dst_node = ?1)",
            [n],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            != 0
    });
    if !touches_any {
        for vid in &var_ids {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT v2.id FROM variables v1 \
                 JOIN variables v2 \
                   ON v2.fn_id = v1.fn_id AND v2.name = v1.name AND v2.kind = 'param' \
                 WHERE v1.id = ?1 AND v2.id != v1.id",
            )?;
            let rows = stmt.query_map([vid], |row| row.get::<_, i64>(0))?;
            for tw in rows.flatten() {
                let mut nstmt =
                    conn.prepare("SELECT id FROM flow_nodes WHERE var_id = ?1 ORDER BY kind, id")?;
                let nrows = nstmt.query_map([tw], |row| row.get::<_, i64>(0))?;
                for r in nrows {
                    let nid = r?;
                    if !starts.contains(&nid) {
                        starts.push(nid);
                    }
                }
            }
        }
    }
    if starts.is_empty() {
        bail!(
            "no value-flow node for symbol(s): {}; \
             the database may predate flow-graph export",
            symbols
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Load adjacency (both directions of every edge once).
    let mut fwd: Adjacency = FxHashMap::default();
    let mut rev: Adjacency = FxHashMap::default();
    {
        let mut stmt = conn.prepare("SELECT src_node, dst_node, kind FROM flow_edges")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for r in rows {
            let (src, dst, kind) = r?;
            let kind_static: &'static str = match kind.as_str() {
                "copy" => "copy",
                "addr_of" => "addr_of",
                "load" => "load",
                "store" => "store",
                "gep" => "gep",
                "points_to" => "points_to",
                "call_arg" => "call_arg",
                "terminates" => "terminates",
                "dlsym" => "dlsym",
                _ => "flow",
            };
            fwd.entry(src)
                .or_default()
                .push((dst, kind_static, EdgeSite::default()));
            rev.entry(dst)
                .or_default()
                .push((src, kind_static, EdgeSite::default()));
        }
    }

    // Node labels.
    let mut labels: FxHashMap<i64, GraphNode> = FxHashMap::default();
    {
        let mut stmt = conn.prepare("SELECT id, kind, label, detail FROM flow_nodes")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let label: String = row.get(2)?;
            let detail: String = row.get(3)?;
            let kind_enum = FlowNodeKind::from_schema_str(&kind);
            let tag = match kind.as_str() {
                "loc" => "loc",
                "call_target" => "target",
                "terminator" => "terminator",
                _ => "",
            };
            let shown = if tag.is_empty() {
                label.clone()
            } else {
                format!("{tag}:{label}")
            };
            let loc_kind = match kind_enum {
                Some(FlowNodeKind::Loc) => loc_kind_from_schema_str(&detail),
                _ => None,
            };
            Ok((
                id,
                GraphNode {
                    id,
                    label: shown,
                    detail,
                    kind: kind_enum,
                    loc_kind,
                },
            ))
        })?;
        for r in rows {
            let (id, node) = r?;
            labels.insert(id, node);
        }
    }

    let mut graph = QueryGraph::default();
    let mut visited: FxHashSet<i64> = FxHashSet::default();
    struct Entry {
        id: i64,
        depth: u32,
    }
    let mut queue = std::collections::VecDeque::new();
    for &s in &starts {
        if visited.insert(s) {
            queue.push_back(Entry { id: s, depth: 0 });
        }
    }
    let adj = match dir {
        Direction::Down => &fwd,
        Direction::Up => &rev,
    };

    while let Some(Entry { id, depth }) = queue.pop_front() {
        if let Some(n) = labels.get(&id) {
            graph.nodes.insert(id, n.clone());
        } else {
            graph.nodes.insert(
                id,
                GraphNode {
                    id,
                    label: format!("node{id}"),
                    detail: String::new(),
                    kind: None,
                    loc_kind: None,
                },
            );
        }
        graph.order.push((id, depth));
        if depth == max_depth {
            // Truncated only if an actually-unvisited neighbor was cut off;
            // neighbors already reached earlier in the BFS don't count.
            if let Some(neighbors) = adj.get(&id) {
                if neighbors.iter().any(|(to, _, _)| !visited.contains(to)) {
                    graph.truncated = true;
                }
            }
            continue;
        }
        if let Some(neighbors) = adj.get(&id) {
            for (to, kind, _) in neighbors {
                graph.edges.push(GraphEdge {
                    from: id,
                    to: *to,
                    label: (*kind).to_string(),
                    site: EdgeSite::default(),
                });
                if visited.insert(*to) {
                    queue.push_back(Entry {
                        id: *to,
                        depth: depth + 1,
                    });
                }
            }
        }
    }
    graph
        .edges
        .dedup_by(|a, b| a.from == b.from && a.to == b.to && a.label == b.label);
    Ok(graph)
}

/// Convenience wrapper used by tests and the CLI: resolve position → best
/// function, with a helpful error when nothing matches.
pub fn require_function_at(conn: &Connection, file: &str, line: i64) -> Result<FunctionRef> {
    let mut cands = find_functions_at(conn, file, line)?;
    if cands.is_empty() {
        let nearest = nearest_functions(conn, file, line, 3)?;
        bail!(
            "no function contains {}:{}; nearby definitions: {}",
            file,
            line,
            nearest
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(cands.remove(0))
}

fn nearest_functions(
    conn: &Connection,
    file: &str,
    line: i64,
    limit: usize,
) -> Result<Vec<FunctionRef>> {
    let pattern = format!("%{file}%");
    let mut stmt = conn.prepare(
        "SELECT f.id, f.name, p.path, f.line_start, f.line_end, f.is_defined \
         FROM functions f JOIN files p ON p.id = f.file_id \
         WHERE p.path LIKE ?1 AND f.is_defined != 0 \
         ORDER BY min(abs(f.line_start - ?2), abs(f.line_end - ?2)), f.line_start LIMIT ?3",
    )?;
    let rows = stmt.query_map(rusqlite::params![pattern, line, limit as i64], |row| {
        Ok(FunctionRef {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            line_start: row.get(3)?,
            line_end: row.get(4)?,
            is_defined: row.get::<_, i64>(5)? != 0,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Functions whose name equals `name` or ends with `::name` (C++ qualified method),
/// optionally filtering by file path substring. Definitions rank before prototypes,
/// narrower ranges before wider ones.
pub fn find_functions_by_name(
    conn: &Connection,
    name: &str,
    file_substring: Option<&str>,
) -> Result<Vec<FunctionRef>> {
    if name.is_empty() {
        bail!("function name filter must not be empty");
    }
    let mut sql = String::from(
        "SELECT f.id, f.name, p.path, f.line_start, f.line_end, f.is_defined \
         FROM functions f JOIN files p ON p.id = f.file_id \
         WHERE (f.name = ?1 OR (f.name LIKE ?2 ESCAPE '!' AND SUBSTR(f.name, -LENGTH(?3)) = ?3))",
    );
    let mut params = vec![
        name.to_string(),
        format!("%::{}", like_escape(name)),
        format!("::{name}"),
    ];
    if let Some(fs) = file_substring {
        params.push(format!("%{}%", like_escape(fs)));
        let idx = params.len();
        sql.push_str(&format!(" AND p.path LIKE ?{idx} ESCAPE '!'"));
    }
    sql.push_str(" ORDER BY f.is_defined DESC, (f.line_end - f.line_start) ASC, f.name ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok(FunctionRef {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            line_start: row.get(3)?,
            line_end: row.get(4)?,
            is_defined: row.get::<_, i64>(5)? != 0,
        })
    })?;
    let cands: Vec<FunctionRef> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let cands = cands
        .into_iter()
        .filter(|f| f.name == name || f.name.ends_with(&format!("::{name}")))
        .collect();
    Ok(cands)
}

/// Look up a function by name (or qualified suffix) and optional file substring.
/// Returns the single match or the single defined match. Bails with candidate
/// list if ambiguous.
pub fn require_function_by_name(
    conn: &Connection,
    name: &str,
    file_substring: Option<&str>,
) -> Result<FunctionRef> {
    let cands = find_functions_by_name(conn, name, file_substring)?;
    if cands.is_empty() {
        if let Some(fs) = file_substring {
            bail!("no function matching '{name}' found in files containing '{fs}'");
        } else {
            bail!("no function matching '{name}' found in database");
        }
    }
    let defined_count = cands.iter().filter(|c| c.is_defined).count();
    if cands.len() == 1 || defined_count == 1 {
        if let Some(def) = cands.iter().find(|c| c.is_defined) {
            return Ok(def.clone());
        }
        return Ok(cands[0].clone());
    }
    let descriptions: Vec<String> = cands.iter().map(|f| f.to_string()).collect();
    bail!(
        "multiple functions match '{name}':\n  {}\nDisambiguate with file substring or file:line",
        descriptions.join("\n  ")
    );
}

/// Flexible function resolution supporting:
/// - `line` + `file` -> `require_function_at`
/// - `name` containing `file:line` (e.g. `main.c:10`) -> `require_function_at`
/// - `name` -> `require_function_by_name` (with optional `file` filter)
pub fn resolve_function_target(
    conn: &Connection,
    name: Option<&str>,
    file: Option<&str>,
    line: Option<i64>,
) -> Result<FunctionRef> {
    if let Some(l) = line {
        let f = file.ok_or_else(|| anyhow::anyhow!("line specified without file"))?;
        require_function_at(conn, f, l)
    } else if let Some(n) = name {
        if let Some((f, l_str)) = n.rsplit_once(':') {
            if let Ok(l) = l_str.parse::<i64>() {
                if !f.is_empty() {
                    return require_function_at(conn, f, l);
                }
            }
        }
        require_function_by_name(conn, n, file)
    } else if let Some(f) = file {
        bail!("file specified without line or function name: '{f}'");
    } else {
        bail!("must specify function name or file and line");
    }
}

/// Resolve position → best symbol, with a helpful error when nothing matches.
pub fn require_symbols_at(
    conn: &Connection,
    file: &str,
    line: i64,
    col: i64,
) -> Result<Vec<SymbolRef>> {
    let cands = find_symbols_at(conn, file, line, col)?;
    if cands.is_empty() {
        bail!("no variable declared near {file}:{line}:{col}");
    }
    Ok(cands)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SCHEMA_V3;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        // files: 1 = /proj/main.c
        conn.execute(
            "INSERT INTO files (id, path, sha256) VALUES (1, '/proj/main.c', '')",
            [],
        )
        .unwrap();
        // functions: main [10..20], helper [22..24], proto [30..30] (undefined)
        for (id, name, ls, le, defined) in [
            (10, "main", 10, 20, 1),
            (11, "helper", 22, 24, 1),
            (12, "proto", 30, 30, 0),
        ] {
            conn.execute(
                "INSERT INTO functions (id, name, file_id, line_start, line_end, linkage, signature, is_defined) \
                 VALUES (?1, ?2, 1, ?3, ?4, 'external', 'fn', ?5)",
                rusqlite::params![id, name, ls, le, defined],
            )
            .unwrap();
        }
        // call sites: main->helper @15; helper->proto @23
        conn.execute(
            "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES (100, 10, 1, 15, 5, 'helper', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES (101, 11, 1, 23, 5, 'proto', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES (200, 100, 10, 11, 'direct')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES (201, 101, 11, 12, 'external')",
            [],
        )
        .unwrap();
        // variables: g @5:5 global; x @12:9 local; y @12:18 local
        for (id, name, kind, line, col, fn_id) in [
            (20, "g", "global", 5, 5, Option::<i64>::None),
            (21, "x", "local", 12, 9, Some(10)),
            (22, "y", "local", 12, 18, Some(10)),
        ] {
            conn.execute(
                "INSERT INTO variables (id, name, kind, fn_id, type_id, file_id, line, col) \
                 VALUES (?1, ?2, ?3, ?4, 0, 1, ?5, ?6)",
                rusqlite::params![id, name, kind, fn_id, line, col],
            )
            .unwrap();
        }
        // flow graph: g(300) -copy-> x(301); x -store-> cell(302); y(303) isolated;
        // reverse edge cell -copy-> g so `up` differs from `down`.
        for (id, kind, label, var_id) in [
            (300, "var", "g", Some(20)),
            (301, "var", "x", Some(21)),
            (302, "loc", "cell of g", None),
            (303, "var", "y", Some(22)),
        ] {
            conn.execute(
                "INSERT INTO flow_nodes (id, kind, label, detail, var_id, fn_id) VALUES (?1, ?2, ?3, '', ?4, NULL)",
                rusqlite::params![id, kind, label, var_id],
            )
            .unwrap();
        }
        for (src, dst, kind) in [(300, 301, "copy"), (301, 302, "store"), (302, 300, "copy")] {
            conn.execute(
                "INSERT INTO flow_edges (id, src_node, dst_node, kind) VALUES (?1 + 400, ?2, ?3, ?4)",
                rusqlite::params![src, src, dst, kind],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn find_functions_at_prefers_narrow_definition() {
        let conn = test_conn();
        let hits = find_functions_at(&conn, "main.c", 16).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "main");
        assert!(hits[0].is_defined);

        // Boundary lines belong to the function.
        assert_eq!(
            find_functions_at(&conn, "main.c", 20).unwrap()[0].name,
            "main"
        );
        assert!(find_functions_at(&conn, "main.c", 21).unwrap().is_empty());
        assert!(find_functions_at(&conn, "main.c", 9).unwrap().is_empty());
    }

    #[test]
    fn require_function_at_error_lists_neighbours() {
        let conn = test_conn();
        let err = require_function_at(&conn, "main.c", 99).unwrap_err();
        assert!(err.to_string().contains("no function contains"), "{err}");
        assert!(err.to_string().contains("helper (main.c:22-24)"), "{err}");
    }

    #[test]
    fn call_graph_down_respects_depth_and_flags_truncation() {
        let conn = test_conn();
        let g = call_graph(&conn, 10, Direction::Down, 1).unwrap();
        let names: Vec<&str> = g
            .order
            .iter()
            .map(|&(id, _)| g.nodes[&id].label.as_str())
            .collect();
        assert_eq!(names, ["main", "helper"]);
        assert!(g.truncated, "helper has an unvisited external edge");

        let g = call_graph(&conn, 10, Direction::Down, 5).unwrap();
        assert_eq!(g.order.len(), 3);
        assert!(!g.truncated);
        // Edge annotations carry resolution + the exact source site (display
        // form, full path, and the 1-based line/col).
        let e = format!("{:?}", g.edges);
        assert!(
            g.edges.iter().any(|e| e.from == 10
                && e.to == 11
                && e.label == "direct"
                && e.site.display() == "main.c:15"
                && e.site.path == "/proj/main.c"
                && e.site.line == 15
                && e.site.col == 5),
            "edges: {e}"
        );
    }

    #[test]
    fn call_graph_up_finds_callers() {
        let conn = test_conn();
        // Reverse reachability: helper <- main.
        let g = call_graph(&conn, 11, Direction::Up, 3).unwrap();
        let names: Vec<&str> = g
            .order
            .iter()
            .map(|&(id, _)| g.nodes[&id].label.as_str())
            .collect();
        assert_eq!(names, ["helper", "main"]);
        // proto <- helper <- main (a full "how do we reach proto" path).
        let g = call_graph(&conn, 12, Direction::Up, 3).unwrap();
        let names: Vec<&str> = g
            .order
            .iter()
            .map(|&(id, _)| g.nodes[&id].label.as_str())
            .collect();
        assert_eq!(names, ["proto", "helper", "main"]);
    }

    #[test]
    fn call_graph_survives_cycles() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES (102, 11, 1, 24, 9, 'main', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES (202, 102, 11, 10, 'direct')",
            [],
        )
        .unwrap();
        let g = call_graph(&conn, 10, Direction::Down, 10).unwrap();
        assert!(!g.truncated);
        // main -> {helper, }, helper -> {proto, main(seen)}: 3 distinct nodes.
        assert_eq!(g.order.len(), 3, "cycle must not revisit nodes");
    }

    #[test]
    fn symbol_lookup_ranks_exact_column_first() {
        let conn = test_conn();
        let syms = find_symbols_at(&conn, "main.c", 12, 10).unwrap();
        assert_eq!(syms[0].name, "x");
        // Nearest-column fallback on the same line.
        let syms = find_symbols_at(&conn, "main.c", 12, 40).unwrap();
        assert_eq!(syms[0].name, "y");
        // ±2 line window still finds the global.
        let syms = find_symbols_at(&conn, "main.c", 6, 5).unwrap();
        assert_eq!(syms[0].name, "g");
    }

    #[test]
    fn dataflow_traverses_both_directions_with_kinds() {
        let conn = test_conn();
        let syms = require_symbols_at(&conn, "main.c", 5, 5).unwrap();
        assert_eq!(syms[0].name, "g");

        let down = dataflow_graph(&conn, &syms, Direction::Down, 4).unwrap();
        let reached: Vec<(i64, u32)> = down.order.clone();
        assert_eq!(
            reached,
            vec![(300, 0), (301, 1), (302, 2)],
            "down follows copy then store"
        );
        assert!(down
            .edges
            .iter()
            .any(|e| e.from == 301 && e.to == 302 && e.label == "store"));

        let up = dataflow_graph(&conn, &syms, Direction::Up, 4).unwrap();
        // The synthetic graph is cyclic (g→x→cell→g), so reverse traversal
        // reaches every node as well; order differs from down.
        assert_eq!(up.order.first(), Some(&(300, 0)));
        assert_eq!(up.order.len(), 3);
        assert!(
            up.edges.iter().any(|e| e.from == 300 && e.to == 302),
            "up follows reversed edges into g"
        );
    }

    #[test]
    fn dataflow_depth_limit_sets_truncated() {
        let conn = test_conn();
        let syms = require_symbols_at(&conn, "main.c", 12, 9).unwrap(); // x
        let g = dataflow_graph(&conn, &syms[..1], Direction::Down, 1).unwrap();
        assert_eq!(g.order.len(), 2);
        assert!(g.truncated);
    }

    #[test]
    fn dataflow_errors_without_flow_node() {
        let conn = test_conn();
        let orphan = SymbolRef {
            var_id: 999,
            name: "orphan".into(),
            kind: "local".into(),
            fn_name: None,
            path: "/proj/main.c".into(),
            line: 50,
            col: 1,
        };
        let err = dataflow_graph(&conn, &[orphan], Direction::Down, 3).unwrap_err();
        assert!(err.to_string().contains("no value-flow node"));
    }

    #[test]
    fn truncated_only_when_boundary_neighbors_unvisited() {
        let conn = test_conn();
        // Pure two-node cycle: at the depth limit every neighbor is already
        // visited, so nothing was actually cut off.
        for (id, name, ls) in [(13, "c1", 40), (14, "c2", 42)] {
            conn.execute(
                "INSERT INTO functions (id, name, file_id, line_start, line_end, linkage, signature, is_defined) \
                 VALUES (?1, ?2, 1, ?3, ?4, 'external', 'fn', 1)",
                rusqlite::params![id, name, ls, ls + 1],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES (103, 13, 1, 41, 5, 'c2', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES (104, 14, 1, 43, 5, 'c1', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES (203, 103, 13, 14, 'direct')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES (204, 104, 14, 13, 'direct')",
            [],
        )
        .unwrap();

        let g = call_graph(&conn, 13, Direction::Down, 1).unwrap();
        assert_eq!(g.order.len(), 2);
        assert!(
            !g.truncated,
            "cycle back-edge to a visited node is not truncation"
        );

        // Cutting off before an unvisited node still reports truncation.
        let g = call_graph(&conn, 13, Direction::Down, 0).unwrap();
        assert!(g.truncated);
    }

    #[test]
    fn dataflow_widens_to_same_function_param_twins_only() {
        let conn = test_conn();
        // Twin A of param `p` in main (fn 10): has flow wiring.
        conn.execute(
            "INSERT INTO variables (id, name, kind, fn_id, type_id, file_id, line, col) \
             VALUES (23, 'p', 'param', 10, 0, 1, 4, 18)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO flow_nodes (id, kind, label, detail, var_id, fn_id) VALUES (304, 'var', 'p', '', 23, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO flow_edges (id, src_node, dst_node, kind) VALUES (504, 304, 300, 'copy')",
            [],
        )
        .unwrap();
        // Twin B: same function, no flow nodes — the queried copy.
        conn.execute(
            "INSERT INTO variables (id, name, kind, fn_id, type_id, file_id, line, col) \
             VALUES (24, 'p', 'param', 10, 0, 1, 13, 7)",
            [],
        )
        .unwrap();
        // Decoy: same-name param in a DIFFERENT function record — widening
        // must not reach it even though it has flow wiring.
        conn.execute(
            "INSERT INTO variables (id, name, kind, fn_id, type_id, file_id, line, col) \
             VALUES (25, 'p', 'param', 11, 0, 1, 50, 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO flow_nodes (id, kind, label, detail, var_id, fn_id) VALUES (305, 'var', 'p', '', 25, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO flow_edges (id, src_node, dst_node, kind) VALUES (505, 305, 300, 'copy')",
            [],
        )
        .unwrap();

        let syms = require_symbols_at(&conn, "main.c", 13, 7).unwrap();
        assert_eq!(syms[0].var_id, 24, "exact position selects twin B");

        let g = dataflow_graph(&conn, std::slice::from_ref(&syms[0]), Direction::Down, 4).unwrap();
        let ids: Vec<i64> = g.order.iter().map(|&(id, _)| id).collect();
        assert!(ids.contains(&304), "same-function twin widened: {ids:?}");
        assert!(ids.contains(&300), "twin's edge traversed into g");
        assert!(
            !ids.contains(&305),
            "same-name param of another function must stay out: {ids:?}"
        );
    }

    #[test]
    fn stale_schema_errors_are_actionable() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE variables ( \
                id INTEGER PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL, \
                fn_id INTEGER, type_id INTEGER NOT NULL, file_id INTEGER NOT NULL, \
                line INTEGER NOT NULL );",
        )
        .unwrap();

        let err = find_symbols_at(&conn, "main.c", 12, 9).unwrap_err();
        assert!(err.to_string().contains("variables.col"), "{err}");
        assert!(err.to_string().contains("re-run"), "{err}");

        conn.execute_batch(
            "CREATE TABLE call_edges ( \
                id INTEGER PRIMARY KEY, call_site_id INTEGER, \
                callee_fn_id INTEGER NOT NULL, resolution TEXT NOT NULL );",
        )
        .unwrap();
        let err = require_call_edge_caller(&conn).unwrap_err();
        assert!(err.to_string().contains("call_edges.caller_fn_id"), "{err}");
        assert!(err.to_string().contains("re-run"), "{err}");

        let orphan = SymbolRef {
            var_id: 999,
            name: "orphan".into(),
            kind: "local".into(),
            fn_name: None,
            path: "/proj/main.c".into(),
            line: 50,
            col: 1,
        };
        let err = dataflow_graph(&conn, &[orphan], Direction::Down, 3).unwrap_err();
        assert!(
            err.to_string().contains("predates flow-graph export"),
            "{err}"
        );
    }

    #[test]
    fn direction_parse_rejects_garbage() {
        assert!(Direction::parse("down").is_ok());
        assert!(Direction::parse("up").is_ok());
        assert!(Direction::parse("sideways").is_err());
    }

    #[test]
    fn callgraph_filter_prunes_after_bfs() {
        // Mirror the CLI flow: BFS then display-only filter.
        let conn = test_conn();
        let filter = crate::CallGraphFilter::from_json(r#"{ "functions": ["helper"] }"#).unwrap();
        let mut g = call_graph(&conn, 10, Direction::Down, 5).unwrap();
        assert_eq!(g.order.len(), 3, "full BFS reaches main, helper, proto");

        crate::filter_query_graph(&mut g, &filter);

        let names: Vec<&str> = g
            .order
            .iter()
            .map(|&(id, _)| g.nodes[&id].label.as_str())
            .collect();
        // main--helper edge survives (helper matches); helper->proto survives
        // (helper matches); proto is left of *both* its edges' matching side
        // only via helper, and stays because the helper->proto edge is kept.
        assert_eq!(names, vec!["main", "helper", "proto"], "names: {names:?}");
        assert!(g.nodes.contains_key(&10));
        assert!(g.nodes.contains_key(&11));
        assert!(g.nodes.contains_key(&12));
    }

    #[test]
    fn call_chains_basic() {
        let conn = test_conn();
        // Depth 1 from main to proto (down): no chains
        let r1 = call_chains(&conn, 10, 12, Direction::Down, 1, None).unwrap();
        assert!(r1.chains.is_empty());

        // Depth 2 from main to proto (down): main -> helper -> proto
        let r2 = call_chains(&conn, 10, 12, Direction::Down, 2, None).unwrap();
        assert_eq!(r2.chains.len(), 1);
        assert_eq!(r2.chains[0].nodes, vec![10, 11, 12]);
        assert_eq!(r2.chains[0].edges.len(), 2);
        assert_eq!(r2.chains[0].edges[0].caller_id, 10);
        assert_eq!(r2.chains[0].edges[0].callee_id, 11);
        assert_eq!(r2.chains[0].edges[0].site.line, 15);
        assert_eq!(r2.chains[0].edges[1].caller_id, 11);
        assert_eq!(r2.chains[0].edges[1].callee_id, 12);
        assert_eq!(r2.chains[0].edges[1].site.line, 23);

        // Convert to query graph
        let qg = r2.to_query_graph(&conn).unwrap();
        assert_eq!(qg.nodes.len(), 3);
        assert_eq!(qg.edges.len(), 2);

        // Depth 2 from proto to main (up): proto <- helper <- main
        let r_up = call_chains(&conn, 12, 10, Direction::Up, 2, None).unwrap();
        assert_eq!(r_up.chains.len(), 1);
        assert_eq!(r_up.chains[0].nodes, vec![12, 11, 10]);
        assert_eq!(r_up.chains[0].edges.len(), 2);
        // edges still record true caller and callee
        assert_eq!(r_up.chains[0].edges[0].caller_id, 11);
        assert_eq!(r_up.chains[0].edges[0].callee_id, 12);
        assert_eq!(r_up.chains[0].edges[1].caller_id, 10);
        assert_eq!(r_up.chains[0].edges[1].callee_id, 11);
    }

    #[test]
    fn function_by_name_and_target_resolution() {
        let conn = test_conn();
        let main_fn = require_function_by_name(&conn, "main", None).unwrap();
        assert_eq!(main_fn.id, 10);
        assert_eq!(main_fn.name, "main");

        // resolve by name
        let target = resolve_function_target(&conn, Some("helper"), None, None).unwrap();
        assert_eq!(target.id, 11);

        // resolve by file:line string
        let by_pos = resolve_function_target(&conn, Some("main.c:15"), None, None).unwrap();
        assert_eq!(by_pos.id, 10);

        // resolve by explicit file and line
        let by_file_line = resolve_function_target(&conn, None, Some("main.c"), Some(23)).unwrap();
        assert_eq!(by_file_line.id, 11);
    }

    #[test]
    fn call_chains_truncation_only_when_additional_path_exists() {
        let conn = test_conn();
        // In test_conn, only 1 path exists: 10 -> 11 -> 12.
        // With limit 1, it should NOT be marked as truncated.
        let r_single = call_chains(&conn, 10, 12, Direction::Down, 2, Some(1)).unwrap();
        assert_eq!(r_single.chains.len(), 1);
        assert!(
            !r_single.truncated,
            "single existing path with limit 1 must not be truncated"
        );

        // Now add a second path: 10 -> 12 directly
        conn.execute(
            "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES (102, 10, 1, 18, 5, 'proto', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES (202, 102, 10, 12, 'direct')",
            [],
        )
        .unwrap();

        // With 2 paths existing and limit 1, it MUST be marked as truncated
        let r_multi = call_chains(&conn, 10, 12, Direction::Down, 2, Some(1)).unwrap();
        assert_eq!(r_multi.chains.len(), 1);
        assert!(
            r_multi.truncated,
            "two existing paths with limit 1 must be marked truncated"
        );

        // With limit 2 (all paths returned), it should NOT be truncated
        let r_all = call_chains(&conn, 10, 12, Direction::Down, 2, Some(2)).unwrap();
        assert_eq!(r_all.chains.len(), 2);
        assert!(
            !r_all.truncated,
            "all existing paths returned must not be truncated"
        );
    }

    #[test]
    fn function_by_name_case_sensitive_cpp_suffix() {
        let conn = test_conn();
        // Insert functions with different casing: run vs ns::Run vs ns::run
        for (id, name) in [(50, "run"), (51, "ns::Run"), (52, "ns::run")] {
            conn.execute(
                "INSERT INTO functions (id, name, file_id, line_start, line_end, linkage, signature, is_defined) \
                 VALUES (?1, ?2, 1, 100, 110, 'external', 'fn', 1)",
                rusqlite::params![id, name],
            )
            .unwrap();
        }

        // Searching for "run" must match "run" and "ns::run", but NOT "ns::Run"
        let matches_lower = find_functions_by_name(&conn, "run", None).unwrap();
        let names_lower: Vec<&str> = matches_lower.iter().map(|f| f.name.as_str()).collect();
        assert!(names_lower.contains(&"run"));
        assert!(names_lower.contains(&"ns::run"));
        assert!(
            !names_lower.contains(&"ns::Run"),
            "--from run must not select ns::Run"
        );

        // Searching for "Run" must match "ns::Run", but NOT "run" or "ns::run"
        let matches_upper = find_functions_by_name(&conn, "Run", None).unwrap();
        let names_upper: Vec<&str> = matches_upper.iter().map(|f| f.name.as_str()).collect();
        assert!(names_upper.contains(&"ns::Run"));
        assert!(!names_upper.contains(&"run"));
        assert!(!names_upper.contains(&"ns::run"));
    }
}
