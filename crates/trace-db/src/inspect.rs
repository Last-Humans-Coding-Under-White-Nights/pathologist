//! Query layer behind `trace inspect`: locate entities by source position
//! and construct call / dataflow graphs as bounded BFS traversals.
//!
//! All lookups work purely off the exported SQLite database — no re-analysis.

use anyhow::{bail, Result};
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

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
        let base = self.path.rsplit('/').next().unwrap_or(&self.path);
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
}

#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub from: i64,
    pub to: i64,
    pub label: String,
    pub site: String,
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

fn load_function_labels(conn: &Connection) -> Result<FxHashMap<i64, GraphNode>> {
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
        let file_name = path.rsplit('/').next().unwrap_or(&path).to_string();
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

type Adjacency = FxHashMap<i64, Vec<(i64, &'static str, String)>>;

fn load_call_adjacency(conn: &Connection, dir: Direction) -> Result<Adjacency> {
    let mut stmt = conn.prepare(
        "SELECT ce.caller_fn_id, ce.callee_fn_id, ce.resolution, p.path, cs.line \
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
        let site = match (&path, line) {
            (Some(p), Some(l)) => {
                format!("{}:{}", p.rsplit('/').next().unwrap_or(p), l)
            }
            _ => "(ipc bridge)".to_string(),
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
    // Collapse exact duplicates (same pair, same annotation).
    graph
        .edges
        .dedup_by(|a, b| a.from == b.from && a.to == b.to && a.site == b.site);
    Ok(graph)
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
        let base = self.path.rsplit('/').next().unwrap_or(&self.path);
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
                .push((dst, kind_static, String::new()));
            rev.entry(dst)
                .or_default()
                .push((src, kind_static, String::new()));
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
            Ok((
                id,
                GraphNode {
                    id,
                    label: shown,
                    detail,
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
                    site: String::new(),
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
        // Edge annotations carry resolution + call site.
        let e = format!("{:?}", g.edges);
        assert!(
            g.edges.iter().any(|e| e.from == 10
                && e.to == 11
                && e.label == "direct"
                && e.site == "main.c:15"),
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
}
