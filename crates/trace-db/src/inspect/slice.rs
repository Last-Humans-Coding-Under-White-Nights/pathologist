//! `trace inspect slice`: a bounded two-stage value slice annotated with
//! execution contexts (`docs/ANALYSIS.md`, "Value slice").

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension, Row};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use trace_analysis::MEMORY_ACCESS_CAP;

use super::{loc_kind_from_schema_str, loc_kind_schema_str, FlowNodeKind, LocKind};
use crate::FlowSite;

/// Depth limits of the two stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceOptions {
    /// Stage 1: edges followed backwards from the start.
    pub up_depth: u32,
    /// Stage 2: edges followed forwards from each source.
    pub down_depth: u32,
}

impl Default for SliceOptions {
    fn default() -> Self {
        Self {
            up_depth: 6,
            down_depth: 6,
        }
    }
}

/// Where a slice starts: the value-flow nodes of a variable or of the
/// memory a field-access expression designates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SliceStart {
    /// The identifier the position names.
    pub name: String,
    /// `variable` (a declaration or use of a variable) or `field` (a field
    /// access: the cells it reads or writes).
    pub kind: String,
    /// The queried position, on the file the database records.
    pub at: FlowSite,
    /// `flow_nodes` ids the slice starts from.
    pub nodes: Vec<i64>,
}

/// One execution context a node of the slice is reached by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SliceContext {
    /// `C<execution_contexts.id>`, or `root` for code reached from a
    /// function no recorded call reaches.
    pub id: String,
    /// `thread`, `pool_task`, `serial_task`, `ipc_handler`, `unknown`, or
    /// `root`.
    pub kind: String,
    /// The entry function (`None` for `root`).
    pub entry: Option<String>,
    /// The call site that starts it (`None` for an entry nothing starts).
    pub start_site: Option<FlowSite>,
    /// The modelled callee that starts it (`pthread_create`, ...).
    pub api: Option<String>,
    pub multi_instance: Option<String>,
    pub self_concurrent: bool,
}

/// One node of the slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SliceNode {
    /// `flow_nodes.id`.
    pub id: i64,
    /// `var`, `loc`, `constant` (`nullptr`), `call_target` or `terminator`.
    pub kind: String,
    pub label: String,
    /// `variables.kind` of a variable node (`param`, `local`, `global`, ...).
    pub var_kind: Option<String>,
    /// Location kind of a `loc` node (`heap`, `field`, `field_summary`, ...).
    pub loc_kind: Option<String>,
    /// The function the node belongs to: a variable's, a local's storage's.
    pub function: Option<String>,
    /// Declaration of a variable node.
    pub decl: Option<FlowSite>,
    /// `private` (one invocation's: locals, parameters, temporaries, their
    /// storage and its field cells), `shared` (other memory and globals) or
    /// `constant` (function and string addresses, `nullptr`).
    pub sharing: String,
    /// Any of `start`, `source`, `sink`, `boundary`.
    pub roles: Vec<String>,
    /// For a source: `allocation`, `address`, `function`, `string`,
    /// `field_address`, `parameter`, `global`, `field_summary`, `null`,
    /// `unrecorded_load` or `unknown`.
    pub source: Option<String>,
    /// `up` and/or `down`: the stages that reached it.
    pub stages: Vec<String>,
    /// Ids of the contexts that reach it (see `SliceContext::id`).
    pub contexts: Vec<String>,
}

/// One edge of the slice: a `flow_edges` row at one of the statements its
/// `flow_origins` rows record, or at none for a derived edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SliceEdge {
    pub from: i64,
    pub to: i64,
    /// `flow_edges.kind`.
    pub kind: String,
    /// `up` and/or `down`.
    pub stages: Vec<String>,
    /// The statement that moves the value: a `flow_origins` position
    /// (`None` for a derived edge, which has none).
    pub site: Option<FlowSite>,
    /// The function that statement is in, by the edge-scope rule
    /// (`docs/ANALYSIS.md`, "Where a value moves").
    pub function: Option<String>,
    /// The value may change hands between contexts here; a hint, not a
    /// proven race.
    pub cross_context: bool,
    /// Why: `contexts_differ`, `self_concurrent`, `start`.
    pub reasons: Vec<String>,
}

/// A bounded two-stage value slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueSlice {
    pub start: SliceStart,
    pub up_depth: u32,
    pub down_depth: u32,
    /// A stage stopped at its depth limit with nodes left to expand.
    pub truncated_up: bool,
    pub truncated_down: bool,
    pub contexts: Vec<SliceContext>,
    pub nodes: Vec<SliceNode>,
    pub edges: Vec<SliceEdge>,
    /// What the slice does not show.
    pub limits: Vec<String>,
}

/// What every slice leaves out, stated in each output.
fn limits() -> Vec<String> {
    let mut limits: Vec<String> = LIMITS.iter().map(|l| l.to_string()).collect();
    limits.push(format!(
        "an access through a pointer that may reach more than {MEMORY_ACCESS_CAP} memory \
         cells is not followed: a load's value is then an `unrecorded_load` source, and a \
         store's destination memory is not in the slice"
    ));
    limits
}

const LIMITS: [&str; 4] = [
    "scalar values are not tracked: the slice follows pointer and function values, \
     so integers and plain data the code reads or writes are not in it",
    "a cross-context flag is a hint, not a proven race: locks, ordering, joins and \
     object lifetimes are not modelled",
    "the analysis is flow- and context-insensitive and a field summary merges every \
     instance of a field, so a slice can join flows that never happen together",
    "a field summary or global reached from elsewhere is a boundary: its other writers \
     and readers are not followed (start a slice from it to see them)",
];

const UP: &str = "up";
const DOWN: &str = "down";

/// The position in columns `i..i + 3` (path, line, col) of `row`, if all
/// three are recorded.
fn pos_at(row: &Row<'_>, i: usize) -> rusqlite::Result<Option<FlowSite>> {
    Ok(
        match (
            row.get::<_, Option<String>>(i)?,
            row.get(i + 1)?,
            row.get(i + 2)?,
        ) {
            (Some(path), Some(line), Some(col)) => Some(FlowSite { path, line, col }),
            _ => None,
        },
    )
}

/// The last component of a qualified name.
fn short(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Variable kinds whose storage outlives any one invocation.
fn is_static_storage(var_kind: &str) -> bool {
    matches!(var_kind, "global" | "file_static" | "fn_static")
}

/// The role a `flow_edges.kind` plays in a value slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    /// The value at the source becomes (part of) the value at the
    /// destination: `copy`, `call_arg`, `unwrap`, `dlsym`, `mem_read`,
    /// `mem_write`.
    Value,
    /// A variable and its own storage location hold one value: followed both
    /// ways.
    Storage,
    /// `addr_of`: the destination holds the source location's address.
    AddrOf,
    /// `gep`: the destination holds a field address computed from the
    /// source pointer.
    Gep,
    /// `load` (source dereferenced), `store` (destination dereferenced),
    /// `terminates` (source cleared).
    Load,
    Store,
    Terminates,
    Other,
}

impl EdgeKind {
    fn of(kind: &str) -> Self {
        match kind {
            "copy" | "call_arg" | "unwrap" | "dlsym" | "mem_read" | "mem_write" => Self::Value,
            "points_to" => Self::Storage,
            "addr_of" => Self::AddrOf,
            "gep" => Self::Gep,
            "load" => Self::Load,
            "store" => Self::Store,
            "terminates" => Self::Terminates,
            _ => Self::Other,
        }
    }
}

/// A slice edge's identity: its two ends and the ordinal of its `(kind,
/// site)` among the edges between them. The adjacency lists of both ends
/// hold every edge between them in one order, so either gives an edge the
/// same identity.
type EdgeKey = (i64, i64, u32);

#[derive(Debug, Clone)]
struct FlowEdge {
    id: EdgeKey,
    src: i64,
    dst: i64,
    kind: String,
    role: EdgeKind,
    /// The function `site` is in.
    fn_id: Option<i64>,
    site: Option<FlowSite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sharing {
    Private,
    Shared,
    Constant,
}

impl Sharing {
    fn name(self) -> &'static str {
        match self {
            Sharing::Private => "private",
            Sharing::Shared => "shared",
            Sharing::Constant => "constant",
        }
    }
}

/// Location kinds whose storage outlives any one invocation.
fn is_static_loc(kind: LocKind) -> bool {
    matches!(
        kind,
        LocKind::Global | LocKind::FileStatic | LocKind::FnStatic
    )
}

#[derive(Debug, Clone)]
struct NodeRow {
    /// `flow_nodes.kind` as written (`unknown` for a node with no row).
    kind_name: String,
    /// The kind, parsed: `None` for the `nullptr` constant or an unknown
    /// kind.
    kind: Option<FlowNodeKind>,
    /// The `nullptr` constant node.
    constant: bool,
    label: String,
    /// Location kind of a `loc` node.
    loc_kind: Option<LocKind>,
    var_kind: Option<String>,
    /// The variable a `var` node is, or whose storage or field cell a `loc`
    /// node is, with its name.
    var: Option<(i64, String)>,
    /// That variable is one lowering made (`variables.is_synthetic`), never
    /// a name in the source.
    synthetic: bool,
    /// The function a private node belongs to.
    home: Option<i64>,
    decl: Option<FlowSite>,
    /// A `loc` node in one invocation's storage: a local's or parameter's
    /// own location, or a field cell of one.
    automatic: bool,
}

impl NodeRow {
    fn is(&self, kind: FlowNodeKind) -> bool {
        self.kind == Some(kind)
    }

    /// A variable of static storage.
    fn static_var(&self) -> bool {
        self.is(FlowNodeKind::Var) && self.var_kind.as_deref().is_some_and(is_static_storage)
    }

    fn sharing(&self) -> Sharing {
        match self.kind {
            _ if self.static_var() => Sharing::Shared,
            _ if self.constant => Sharing::Constant,
            Some(FlowNodeKind::Loc)
                if matches!(self.loc_kind, Some(LocKind::Function | LocKind::StringLit)) =>
            {
                Sharing::Constant
            }
            Some(FlowNodeKind::Loc) if self.automatic => Sharing::Private,
            Some(FlowNodeKind::Loc) => Sharing::Shared,
            _ => Sharing::Private,
        }
    }

    /// A variable of one invocation.
    fn private_var(&self) -> bool {
        self.is(FlowNodeKind::Var) && self.sharing() == Sharing::Private
    }

    /// The function of a private node.
    fn private_home(&self) -> Option<i64> {
        self.home.filter(|_| self.sharing() == Sharing::Private)
    }

    /// A field summary or a global or static: not expanded unless it is the
    /// start.
    fn boundary(&self) -> Option<&'static str> {
        match self.loc_kind {
            Some(LocKind::FieldSummary) => Some("field_summary"),
            Some(k) if is_static_loc(k) => Some("global"),
            _ if self.static_var() => Some("global"),
            _ => None,
        }
    }

    /// What a node no edge leads into is, as a source.
    fn natural_source(&self) -> &'static str {
        if self.is(FlowNodeKind::Var) && self.var_kind.as_deref() == Some("param") {
            "parameter"
        } else if self.static_var() || self.loc_kind.is_some_and(is_static_loc) {
            "global"
        } else {
            "unknown"
        }
    }

    /// What the address of this location is, as a source.
    fn origin(&self) -> &'static str {
        match self.loc_kind {
            Some(LocKind::Heap) => "allocation",
            Some(LocKind::Function) => "function",
            Some(LocKind::StringLit) => "string",
            _ => "address",
        }
    }

    /// The field name of a field cell or field summary.
    fn field_name(&self) -> Option<&str> {
        match self.loc_kind? {
            LocKind::Field => self.label.split(" of ").next(),
            LocKind::FieldSummary => self.label.rsplit_once('.').map(|(_, f)| f),
            _ => None,
        }
    }
}

/// Cached reads of the flow graph, one node or adjacency list at a time.
struct FlowDb<'c> {
    conn: &'c Connection,
    nodes: FxHashMap<i64, Rc<NodeRow>>,
    out: FxHashMap<i64, Rc<[FlowEdge]>>,
    inc: FxHashMap<i64, Rc<[FlowEdge]>>,
    fn_names: FxHashMap<i64, String>,
    /// The function holding each site, by the edge-scope rule.
    owners: super::DefinitionOwners,
}

/// The flow edges whose `$col` is `?1`, each with the positions its
/// `flow_origins` rows record (none for a derived edge): `flow_origins` is
/// the one record of where an edge happens (`docs/ANALYSIS.md`, "Where a
/// value moves").
macro_rules! edges_sql {
    ($col:literal) => {
        concat!(
            "SELECT e.src_node, e.dst_node, e.kind, o.file_id, p.path, o.line, o.col \
             FROM flow_edges e \
             LEFT JOIN flow_origins o ON o.src_node = e.src_node AND o.dst_node = e.dst_node \
                                     AND o.kind = e.kind \
             LEFT JOIN files p ON p.id = o.file_id \
             WHERE e.",
            $col,
            " = ?1"
        )
    };
}

/// The memory edges (`docs/ANALYSIS.md`, "Memory access edges") a load
/// whose `$load` is `?1` reads over and a store whose `$store` is `?1`
/// writes over: `mem_read` from each cell a load reads into its destination,
/// `mem_write` from a store's value into each cell it writes, each at the
/// load's or store's `flow_origins` positions.
macro_rules! memory_sql {
    ($load:literal, $store:literal) => {
        concat!(
            "SELECT a.cell_node, e.dst_node, 'mem_read', o.file_id, p.path, o.line, o.col \
             FROM flow_memory_access a JOIN flow_edges e ON e.id = a.edge_id \
             LEFT JOIN flow_origins o ON o.src_node = e.src_node AND o.dst_node = e.dst_node \
                                     AND o.kind = e.kind \
             LEFT JOIN files p ON p.id = o.file_id \
             WHERE ",
            $load,
            " = ?1 AND e.kind = 'load' AND a.cell_node IS NOT NULL \
             UNION ALL \
             SELECT e.src_node, a.cell_node, 'mem_write', o.file_id, p.path, o.line, o.col \
             FROM flow_memory_access a JOIN flow_edges e ON e.id = a.edge_id \
             LEFT JOIN flow_origins o ON o.src_node = e.src_node AND o.dst_node = e.dst_node \
                                     AND o.kind = e.kind \
             LEFT JOIN files p ON p.id = o.file_id \
             WHERE ",
            $store,
            " = ?1 AND e.kind = 'store' AND a.cell_node IS NOT NULL"
        )
    };
}

/// One row of [`edges_sql`] or [`memory_sql`].
struct EdgeRow {
    src: i64,
    dst: i64,
    kind: String,
    file_id: Option<i64>,
    site: Option<FlowSite>,
}

impl EdgeRow {
    fn same_edge(&self, other: &EdgeRow) -> bool {
        (self.src, self.dst, &self.kind) == (other.src, other.dst, &other.kind)
    }
}

impl<'c> FlowDb<'c> {
    fn new(conn: &'c Connection) -> Self {
        Self {
            conn,
            nodes: FxHashMap::default(),
            out: FxHashMap::default(),
            inc: FxHashMap::default(),
            fn_names: FxHashMap::default(),
            owners: super::DefinitionOwners::default(),
        }
    }

    fn node(&mut self, id: i64) -> Result<Rc<NodeRow>> {
        if let Some(row) = self.nodes.get(&id) {
            return Ok(Rc::clone(row));
        }
        let row = self
            .conn
            .prepare_cached(
                "SELECT n.kind, n.label, n.detail, n.fn_id, v.name, v.kind, v.fn_id, \
                        vp.path, v.line, v.col, v.id, COALESCE(v.is_synthetic, 0) \
                 FROM flow_nodes n \
                 LEFT JOIN variables v ON v.id = n.var_id \
                 LEFT JOIN files vp ON vp.id = v.file_id \
                 WHERE n.id = ?1",
            )?
            .query_row([id], |r| {
                let kind_name: String = r.get(0)?;
                let label: String = r.get(1)?;
                let detail: String = r.get(2)?;
                let node_fn: Option<i64> = r.get(3)?;
                let var_name: Option<String> = r.get(4)?;
                let var_kind: Option<String> = r.get(5)?;
                let var_fn: Option<i64> = r.get(6)?;
                let var_id: Option<i64> = r.get(10)?;
                let decl = pos_at(r, 7)?;
                let kind = FlowNodeKind::from_schema_str(&kind_name);
                let is_var = kind == Some(FlowNodeKind::Var);
                let loc_kind = match kind {
                    Some(FlowNodeKind::Loc) => loc_kind_from_schema_str(&detail),
                    _ => None,
                };
                // A local's own location, or a field cell of a local or
                // parameter (its variable is the one whose storage holds
                // it). Either carries its function as the node's `fn_id`.
                let automatic = match loc_kind {
                    Some(LocKind::Local) => true,
                    Some(LocKind::Field) => {
                        matches!(var_kind.as_deref(), Some("local" | "param"))
                    }
                    _ => false,
                };
                let home = match kind {
                    Some(FlowNodeKind::Var) => var_fn,
                    Some(FlowNodeKind::Loc) if automatic => node_fn,
                    Some(FlowNodeKind::Terminator) => node_fn,
                    _ => None,
                };
                let var = var_id.zip(var_name);
                let synthetic = r.get::<_, i64>(11)? != 0;
                Ok(NodeRow {
                    synthetic,
                    label: match (is_var, &var) {
                        (true, Some((_, name))) => name.clone(),
                        (true, None) if label.is_empty() => format!("node{id}"),
                        _ => label,
                    },
                    var,
                    constant: kind_name == "constant",
                    kind_name,
                    kind,
                    loc_kind,
                    var_kind: if is_var { var_kind } else { None },
                    home,
                    decl: if is_var { decl } else { None },
                    automatic,
                })
            })
            .optional()?
            .unwrap_or_else(|| NodeRow {
                kind_name: "unknown".into(),
                kind: None,
                constant: false,
                label: format!("node{id}"),
                loc_kind: None,
                var_kind: None,
                var: None,
                synthetic: false,
                home: None,
                decl: None,
                automatic: false,
            });
        let row = Rc::new(row);
        self.nodes.insert(id, Rc::clone(&row));
        Ok(row)
    }

    /// The edges out of or into `id`: its `flow_edges` rows and memory
    /// edges, one per distinct site, in `(src, dst, kind, site)` order.
    fn edges(&mut self, id: i64, outgoing: bool) -> Result<Rc<[FlowEdge]>> {
        let cache = if outgoing { &self.out } else { &self.inc };
        if let Some(list) = cache.get(&id) {
            return Ok(Rc::clone(list));
        }
        let queries = if outgoing {
            [
                edges_sql!("src_node"),
                memory_sql!("a.cell_node", "e.src_node"),
            ]
        } else {
            [
                edges_sql!("dst_node"),
                memory_sql!("e.dst_node", "a.cell_node"),
            ]
        };
        let mut rows: Vec<EdgeRow> = Vec::new();
        for sql in queries {
            let mut stmt = self.conn.prepare_cached(sql)?;
            let found = stmt.query_map([id], |r| {
                Ok(EdgeRow {
                    src: r.get(0)?,
                    dst: r.get(1)?,
                    kind: r.get(2)?,
                    file_id: r.get(3)?,
                    site: pos_at(r, 4)?,
                })
            })?;
            for row in found {
                rows.push(row?);
            }
        }
        rows.sort_unstable_by(|a, b| {
            (a.src, a.dst, &a.kind, &a.site).cmp(&(b.src, b.dst, &b.kind, &b.site))
        });
        let mut list: Vec<FlowEdge> = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            // Origins at one position with different expressions or
            // operations are one site. A memory edge some of whose loads or
            // stores have no site keeps the others' sites.
            let repeated = list.last().is_some_and(|last| {
                (last.src, last.dst, &last.kind, &last.site)
                    == (row.src, row.dst, &row.kind, &row.site)
            });
            let siteless =
                row.site.is_none() && rows.get(i + 1).is_some_and(|next| next.same_edge(row));
            if repeated || siteless {
                continue;
            }
            let ordinal = match list.last() {
                Some(last) if (last.src, last.dst) == (row.src, row.dst) => last.id.2 + 1,
                _ => 0,
            };
            let fn_id = match (row.file_id, &row.site) {
                (Some(file), Some(at)) => self.owners.owner(self.conn, file, at.line)?,
                _ => None,
            };
            list.push(FlowEdge {
                id: (row.src, row.dst, ordinal),
                src: row.src,
                dst: row.dst,
                role: EdgeKind::of(&row.kind),
                kind: row.kind.clone(),
                fn_id,
                site: row.site.clone(),
            });
        }
        let list: Rc<[FlowEdge]> = list.into();
        let cache = if outgoing {
            &mut self.out
        } else {
            &mut self.inc
        };
        cache.insert(id, Rc::clone(&list));
        Ok(list)
    }

    /// The pointers of the loads into `id` whose cells are not recorded
    /// (`flow_memory_access` rows without a cell).
    fn unrecorded_loads_into(&self, id: i64) -> Result<Vec<i64>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT e.src_node FROM flow_edges e \
                 JOIN flow_memory_access a ON a.edge_id = e.id \
                 WHERE e.dst_node = ?1 AND e.kind = 'load' AND a.cell_node IS NULL",
            )?
            .query_map([id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn fn_name(&mut self, id: i64) -> Result<String> {
        if let Some(name) = self.fn_names.get(&id) {
            return Ok(name.clone());
        }
        let name: String = self
            .conn
            .prepare_cached("SELECT name FROM functions WHERE id = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?
            .unwrap_or_else(|| format!("fn{id}"));
        self.fn_names.insert(id, name.clone());
        Ok(name)
    }

    /// Whether `inner` is a lambda written in `outer`'s body, or in a
    /// lambda written there: a lambda is named `{owner}::$lambda{line}:{col}`
    /// after the function whose body it is written in.
    fn written_in(&mut self, outer: i64, inner: i64) -> Result<bool> {
        if outer == inner {
            return Ok(false);
        }
        let outer = self.fn_name(outer)?;
        let inner = self.fn_name(inner)?;
        Ok(inner
            .strip_prefix(outer.as_str())
            .is_some_and(|rest| rest.starts_with("::$lambda")))
    }

    /// Whether `e`, between a variable of `from` and one of `to`, is a
    /// lambda's access to a variable it captured: one of the two is a
    /// lambda written in the other, and the move is written in the lambda's
    /// body, where the lambda names the captured variable as that variable
    /// itself. A return out of the lambda (`other = make();`) is written in
    /// the enclosing function's body, and is a return like any
    /// (docs/ANALYSIS.md, "Value slice", stage 2).
    fn capture_access(&mut self, e: &FlowEdge, from: i64, to: i64) -> Result<bool> {
        let Some(site_fn) = e.fn_id else {
            return Ok(false);
        };
        for (outer, inner) in [(from, to), (to, from)] {
            if site_fn == inner && self.written_in(outer, inner)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// A call a value enters or returns out of: `(caller, callee)`.
type CallKey = (i64, i64);

/// A value leaving a function through its return value: from a variable of
/// one function into a non-parameter variable of another (a parameter is
/// what a call passes in).
fn is_return(from: &NodeRow, to: &NodeRow) -> bool {
    from.private_var()
        && to.private_var()
        && from.home.is_some()
        && to.home.is_some()
        && from.home != to.home
        && to.var_kind.as_deref() != Some("param")
}

/// A value passed into a call: from a variable of one function into a
/// parameter of another.
fn call_entry(from: &NodeRow, to: &NodeRow) -> Option<CallKey> {
    match (from.home, to.home) {
        (Some(caller), Some(callee))
            if caller != callee
                && from.private_var()
                && to.private_var()
                && to.var_kind.as_deref() == Some("param") =>
        {
            Some((caller, callee))
        }
        _ => None,
    }
}

/// A context a function is reached by: an `execution_contexts` row, or the
/// root pseudo-context. Orders rows by id, then `Root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum CtxKey {
    Row(i64),
    Root,
}

impl CtxKey {
    fn id(self) -> String {
        match self {
            CtxKey::Row(id) => format!("C{id}"),
            CtxKey::Root => "root".into(),
        }
    }
}

struct ContextRow {
    kind: String,
    entry: i64,
    /// The function the start site is in.
    starter: Option<i64>,
    start_site: Option<FlowSite>,
    api: Option<i64>,
    multi_instance: String,
    self_concurrent: bool,
}

/// The contexts a function is reached by, shared by every function of one
/// strongly connected component of ordinary calls.
type CtxSet = Rc<BTreeSet<CtxKey>>;

/// Which contexts reach a function: the entries and roots it is reachable
/// from over the call graph without crossing a start edge or an IPC bridge,
/// or entering the entry of a context no call site starts.
struct ContextIndex<'c> {
    conn: &'c Connection,
    rows: BTreeMap<i64, ContextRow>,
    by_entry: FxHashMap<i64, Vec<i64>>,
    /// The start edges, `(call site, entry)`.
    starts: FxHashSet<(i64, i64)>,
    /// Entries of a context no call site starts: framework entries and IPC
    /// stub handlers.
    dispatched: FxHashSet<i64>,
    /// Callers over ordinary call edges (start edges, IPC bridges and the
    /// calls into an entry no call site starts left out), read per function
    /// as a slice reaches it, not for the whole call graph at once; `None`
    /// for a function with no ordinary caller.
    callers: FxHashMap<i64, Option<Rc<[i64]>>>,
    /// Functions whose callers are read, transitively, and whose component
    /// is known.
    resolved: FxHashSet<i64>,
    /// Functions whose code `root` runs: those of a strongly connected
    /// component of ordinary calls that no edge from outside it enters and
    /// that holds no context entry (a caller-less function, or a recursion
    /// nothing else calls).
    roots: FxHashSet<i64>,
    /// Per function an ordinary edge touches: the first function of its
    /// component, under which `memo` keeps the component's contexts.
    representative: FxHashMap<i64, i64>,
    memo: FxHashMap<i64, CtxSet>,
}

/// Over `callers`' graph: the functions in a strongly connected component
/// no edge from outside enters and no member of which is in `entries`, and
/// per function the first function of its component.
fn root_components(
    callers: &FxHashMap<i64, Vec<i64>>,
    entries: &FxHashMap<i64, Vec<i64>>,
) -> (FxHashSet<i64>, FxHashMap<i64, i64>) {
    // Every function an ordinary edge touches, in a fixed order.
    let mut fns: BTreeSet<i64> = callers.keys().copied().collect();
    fns.extend(callers.values().flatten().copied());
    let fns: Vec<i64> = fns.into_iter().collect();
    let index_of: FxHashMap<i64, usize> = fns.iter().enumerate().map(|(i, &f)| (f, i)).collect();
    let preds: Vec<Vec<usize>> = fns
        .iter()
        .map(|f| {
            callers
                .get(f)
                .into_iter()
                .flatten()
                .map(|c| index_of[c])
                .collect()
        })
        .collect();
    let comp = trace_analysis::strongly_connected_components(&preds);
    let comps = comp.iter().max().map_or(0, |&c| c + 1);
    let mut entered = vec![false; comps];
    let mut first: Vec<Option<i64>> = vec![None; comps];
    for (v, list) in preds.iter().enumerate() {
        if list.iter().any(|&p| comp[p] != comp[v]) || entries.contains_key(&fns[v]) {
            entered[comp[v]] = true;
        }
        first[comp[v]].get_or_insert(fns[v]);
    }
    let roots = (0..fns.len())
        .filter(|&v| !entered[comp[v]])
        .map(|v| fns[v])
        .collect();
    let representative = fns
        .iter()
        .zip(&comp)
        .map(|(&f, &c)| (f, first[c].unwrap_or(f)))
        .collect();
    (roots, representative)
}

impl<'c> ContextIndex<'c> {
    fn load(conn: &'c Connection) -> Result<Self> {
        let mut rows = BTreeMap::new();
        let mut by_entry: FxHashMap<i64, Vec<i64>> = FxHashMap::default();
        let mut starts: FxHashSet<(i64, i64)> = FxHashSet::default();
        let mut dispatched: FxHashSet<i64> = FxHashSet::default();
        {
            let mut stmt = conn.prepare(
                "SELECT c.id, c.kind, c.entry_fn_id, c.call_site_id, c.api_fn_id, \
                        c.multi_instance, c.self_concurrent, cs.caller_fn_id, p.path, cs.line, cs.col \
                 FROM execution_contexts c \
                 LEFT JOIN call_sites cs ON cs.id = c.call_site_id \
                 LEFT JOIN files p ON p.id = cs.file_id \
                 ORDER BY c.id",
            )?;
            let mut q = stmt.query([])?;
            while let Some(r) = q.next()? {
                let id: i64 = r.get(0)?;
                let entry: i64 = r.get(2)?;
                let call_site: Option<i64> = r.get(3)?;
                match call_site {
                    Some(cs) => starts.insert((cs, entry)),
                    None => dispatched.insert(entry),
                };
                let start_site = pos_at(r, 8)?;
                rows.insert(
                    id,
                    ContextRow {
                        kind: r.get(1)?,
                        entry,
                        starter: r.get(7)?,
                        start_site,
                        api: r.get(4)?,
                        multi_instance: r.get(5)?,
                        self_concurrent: r.get::<_, i64>(6)? != 0,
                    },
                );
                by_entry.entry(entry).or_default().push(id);
            }
        }
        Ok(Self {
            conn,
            rows,
            by_entry,
            starts,
            dispatched,
            callers: FxHashMap::default(),
            resolved: FxHashSet::default(),
            roots: FxHashSet::default(),
            representative: FxHashMap::default(),
            memo: FxHashMap::default(),
        })
    }

    /// The ordinary callers of `f`, read once: its call edges less a start
    /// edge, and less every call into it when it is the entry of a context
    /// no call site starts. In-tree code calling such an entry is the
    /// dispatch the context stands for (c_utils' `ThreadStart` calling
    /// `Thread::Run`, a stub's `OnRemoteRequest` switch).
    fn callers_of(&mut self, f: i64) -> Result<Option<Rc<[i64]>>> {
        if let Some(list) = self.callers.get(&f) {
            return Ok(list.clone());
        }
        let list: Option<Rc<[i64]>> = if self.dispatched.contains(&f) {
            None
        } else {
            let callers: Vec<i64> = self
                .conn
                .prepare_cached(
                    "SELECT caller_fn_id, call_site_id FROM call_edges \
                     WHERE callee_fn_id = ?1 AND resolution <> 'ipc' ORDER BY id",
                )?
                .query_map([f], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
                })?
                .filter_map(|r| match r {
                    Ok((caller, site)) => {
                        (!site.is_some_and(|s| self.starts.contains(&(s, f)))).then_some(Ok(caller))
                    }
                    Err(e) => Some(Err(e)),
                })
                .collect::<rusqlite::Result<_>>()?;
            (!callers.is_empty()).then(|| callers.into())
        };
        self.callers.insert(f, list.clone());
        Ok(list)
    }

    /// Read the callers of `f` and of its callers, transitively, and find
    /// the components among them. The set is closed under callers, so a
    /// component with a member in it is wholly in it, and whether an edge
    /// from outside enters it is seen.
    fn resolve(&mut self, f: i64) -> Result<()> {
        if self.resolved.contains(&f) {
            return Ok(());
        }
        let mut closure: BTreeSet<i64> = BTreeSet::from([f]);
        let mut queue = VecDeque::from([f]);
        while let Some(g) = queue.pop_front() {
            for &c in self.callers_of(g)?.iter().flat_map(|l| l.iter()) {
                if closure.insert(c) {
                    queue.push_back(c);
                }
            }
        }
        let callers: FxHashMap<i64, Vec<i64>> = closure
            .iter()
            .filter_map(|g| Some((*g, self.callers[g].as_ref()?.to_vec())))
            .collect();
        let (roots, representative) = root_components(&callers, &self.by_entry);
        self.roots.extend(roots);
        self.representative.extend(representative);
        self.resolved.extend(closure);
        Ok(())
    }

    /// The contexts that reach `f`. Every function of a component reaches
    /// the same callers, so the set is computed once per component.
    fn of_fn(&mut self, f: i64) -> Result<CtxSet> {
        if self.rows.is_empty() {
            // No context: every function is root's code, without the call
            // graph.
            return Ok(self
                .memo
                .entry(f)
                .or_insert_with(|| Rc::new(BTreeSet::from([CtxKey::Root])))
                .clone());
        }
        self.resolve(f)?;
        let f = self.representative.get(&f).copied().unwrap_or(f);
        if let Some(set) = self.memo.get(&f) {
            return Ok(Rc::clone(set));
        }
        let mut seen: FxHashSet<i64> = FxHashSet::default();
        let mut queue = VecDeque::from([f]);
        seen.insert(f);
        let mut out = BTreeSet::new();
        while let Some(g) = queue.pop_front() {
            let entry = match self.by_entry.get(&g) {
                Some(ids) => {
                    out.extend(ids.iter().map(|&id| CtxKey::Row(id)));
                    true
                }
                None => false,
            };
            match self.callers_of(g)? {
                Some(list) => {
                    if self.roots.contains(&g) {
                        out.insert(CtxKey::Root);
                    }
                    for &c in list.iter() {
                        if seen.insert(c) {
                            queue.push_back(c);
                        }
                    }
                }
                None if !entry => {
                    out.insert(CtxKey::Root);
                }
                None => {}
            }
        }
        let out = Rc::new(out);
        self.memo.insert(f, Rc::clone(&out));
        Ok(out)
    }

    fn self_concurrent(&self, key: CtxKey) -> bool {
        match key {
            CtxKey::Row(id) => self.rows.get(&id).is_some_and(|r| r.self_concurrent),
            CtxKey::Root => false,
        }
    }
}

/// Accumulates what the two stages reach.
#[derive(Default)]
struct Reached {
    nodes: BTreeMap<i64, (BTreeSet<&'static str>, BTreeSet<&'static str>)>,
    sources: BTreeMap<i64, &'static str>,
    edges: BTreeMap<EdgeKey, (FlowEdge, BTreeSet<&'static str>)>,
}

impl Reached {
    fn node(&mut self, id: i64, stage: &'static str) {
        self.nodes.entry(id).or_default().0.insert(stage);
    }

    fn role(&mut self, id: i64, role: &'static str) {
        self.nodes.entry(id).or_default().1.insert(role);
    }

    fn source(&mut self, id: i64, kind: &'static str) {
        self.role(id, "source");
        self.sources.entry(id).or_insert(kind);
    }

    fn edge(&mut self, e: &FlowEdge, stage: &'static str) {
        self.node(e.src, stage);
        self.node(e.dst, stage);
        self.edges
            .entry(e.id)
            .or_insert_with(|| (e.clone(), BTreeSet::new()))
            .1
            .insert(stage);
    }
}

/// Where stage 2 starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Root {
    /// A node whose value is followed.
    Value(i64),
    /// A location whose address is followed out of its `addr_of` edges.
    Address(i64),
}

/// Stage 2's queue. A root is queued again whenever a walk reaches it
/// shallower than before, so its successors get the extra depth budget: a
/// return released late (once its call is entered) can arrive after a
/// longer path queued the root. The start's walk, which returns to every
/// caller, covers a source's walk at the same depth or deeper; while the
/// root's entry still waits in the queue, a new arrival takes that entry
/// over instead (the smaller depth, the start's walk if either is), so the
/// root is queued once at a time. Depths only decrease, so the walk ends.
#[derive(Default)]
struct Frontier {
    seen: FxHashMap<Root, Seen>,
    queue: VecDeque<(Root, u32, bool)>,
    /// Entries popped so far: the queue position of entry `i` is
    /// `i - popped`.
    popped: usize,
}

/// How far stage 2 has got with a root: per walk, the start's and a
/// source's, as each keeps its own depth.
#[derive(Default)]
struct Seen {
    start: Walk,
    source: Walk,
}

/// How far one walk has got with a root.
#[derive(Default, Clone, Copy)]
struct Walk {
    /// The smallest depth the walk reached the root at, if it did.
    best: Option<u32>,
    /// The number of the walk's latest entry for the root, which may still
    /// be queued.
    entry: Option<usize>,
}

impl Seen {
    /// Whether an arrival at `depth` adds nothing to what is queued or
    /// expanded already: the start's walk at that depth or less covers it,
    /// and so does a source's walk when the arrival is a source's.
    fn covers(&self, depth: u32, from_source: bool) -> bool {
        let covered = |walk: Walk| walk.best.is_some_and(|b| b <= depth);
        covered(self.start) || (from_source && covered(self.source))
    }

    fn walk(&mut self, from_source: bool) -> &mut Walk {
        if from_source {
            &mut self.source
        } else {
            &mut self.start
        }
    }
}

impl Frontier {
    fn push(&mut self, root: Root, depth: u32, from_source: bool) {
        let next = self.popped + self.queue.len();
        let seen = self.seen.entry(root).or_default();
        if seen.covers(depth, from_source) {
            return;
        }
        // The queue position of a walk's entry for the root, while it waits.
        let queued = |walk: Walk, popped: usize, len: usize| {
            walk.entry
                .and_then(|e| e.checked_sub(popped))
                .filter(|&at| at < len)
        };
        let (popped, len) = (self.popped, self.queue.len());
        if !from_source {
            // The start's walk covers a source's at its depth or deeper:
            // while that entry waits, the start's walk takes it over.
            if let Some(at) = queued(seen.source, popped, len) {
                if self.queue[at].1 >= depth {
                    self.queue[at] = (root, depth, false);
                    seen.source.entry = None;
                    seen.start = Walk {
                        best: Some(depth),
                        entry: Some(popped + at),
                    };
                    return;
                }
            }
        }
        let walk = seen.walk(from_source);
        walk.best = Some(walk.best.map_or(depth, |b| b.min(depth)));
        match queued(*walk, popped, len) {
            // Not expanded yet: the entry takes the arrival over.
            Some(at) => self.queue[at].1 = depth,
            None => {
                walk.entry = Some(next);
                self.queue.push_back((root, depth, from_source));
            }
        }
    }

    fn pop(&mut self) -> Option<(Root, u32, bool)> {
        let entry = self.queue.pop_front()?;
        self.popped += 1;
        Some(entry)
    }

    fn reached(&self, node: i64) -> bool {
        self.seen.contains_key(&Root::Value(node))
    }
}

/// The columns the slice reads beyond the flow graph's own, per table: the
/// operation sites (`flow_origins`), the memory accesses and the execution
/// contexts.
const SLICE_COLUMNS: [(&str, &[&str]); 4] = [
    (
        "flow_origins",
        &[
            "src_node",
            "dst_node",
            "kind",
            "file_id",
            "line",
            "col",
            "expression",
        ],
    ),
    ("flow_memory_access", &["edge_id", "cell_node"]),
    ("flow_call_expressions", &["call_site_id", "expression"]),
    (
        "execution_contexts",
        &[
            "id",
            "kind",
            "entry_fn_id",
            "call_site_id",
            "api_fn_id",
            "multi_instance",
            "self_concurrent",
        ],
    ),
];

/// The slice reads an analysis export's flow graph, operation sites and
/// execution contexts. Under the v7 capability contract
/// (`docs/SQLITE_SCHEMA.md`, "Version and capability contract") it checks
/// those structures, and refuses a `trace-merge` output, whose flow tables
/// hold no flow graph.
fn require_slice_schema(conn: &Connection) -> Result<()> {
    if crate::dataflow::is_merge_output(conn)? {
        bail!(
            "`inspect slice` is unavailable in trace-merge output: the merger preserves \
             call graphs but not PAG flow graphs or their provenance; inspect an original \
             trace analyze database"
        );
    }
    super::require_flow_tables(conn)?;
    super::require_synthetic_metadata(conn)?;
    for (table, columns) in SLICE_COLUMNS {
        for column in columns {
            if !super::column_exists(conn, table, column)? {
                bail!(
                    "`{table}.{column}` missing: database predates the operation sites, \
                     memory accesses or execution contexts `inspect slice` reads; re-run \
                     `trace analyze` with this binary"
                );
            }
        }
    }
    Ok(())
}

/// The identifier around the 1-based `col` of `line` in the file at `path`,
/// if the file is readable here and the column is inside an identifier.
#[cfg(test)]
fn identifier_at(path: &str, line: i64, col: i64) -> Option<String> {
    column_text(&source_line(path, line)?, col)
}

/// The identifier around the 1-based `col` of `row`, or `None` when the
/// column is on no identifier (declarator punctuation, an operator, a
/// space).
fn column_text(row: &str, col: i64) -> Option<String> {
    identifier_span(row, col).map(|(start, end)| row[start..end].to_string())
}

/// The 1-based `line` of the file at `path`, if the file is readable here.
fn source_line(path: &str, line: i64) -> Option<String> {
    let text = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&text);
    let row = text
        .lines()
        .nth(usize::try_from(line).ok()?.checked_sub(1)?)?;
    Some(row.to_string())
}

/// A byte of an identifier: `_`, an ASCII letter or digit, or any byte of a
/// character outside ASCII, which in a declarator or at the column can only
/// be an identifier's (`größe`, `ñame`); a comment is not scanned.
fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric() || !b.is_ascii()
}

/// The byte offset of the 1-based `col` of `row`: LineMap columns count
/// Unicode scalar values, not bytes.
fn byte_at(row: &str, col: i64) -> Option<usize> {
    let index = usize::try_from(col).ok()?.checked_sub(1)?;
    row.char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(row.len()))
        .nth(index)
}

/// The byte range of the identifier around the 1-based `col` of `row`.
fn identifier_span(row: &str, col: i64) -> Option<(usize, usize)> {
    let bytes = row.as_bytes();
    let at = byte_at(row, col)?;
    if !bytes.get(at).copied().is_some_and(is_ident_byte) {
        return None;
    }
    let start = bytes[..at]
        .iter()
        .rposition(|&b| !is_ident_byte(b))
        .map_or(0, |i| i + 1);
    let end = bytes[at..]
        .iter()
        .position(|&b| !is_ident_byte(b))
        .map_or(bytes.len(), |i| at + i);
    (!bytes[start].is_ascii_digit()).then_some((start, end))
}

/// Whether the declaration `decl` names the identifier at `col` of its line,
/// rather than merely sharing it. `row` is that line when it is readable
/// here: the declarator from the recorded column (the declarator's start; a
/// parameter's identifier) must reach the identifier through
/// [`declarator_reaches`]. A readable column on no identifier declares
/// nothing. Without the source line here, the column must fall inside the
/// declared name as recorded ([`super::SymbolRef::covers`]).
fn declares_identifier_at(decl: &super::SymbolRef, row: Option<&str>, col: i64) -> bool {
    let Some(row) = row else {
        return decl.covers(decl.line, col);
    };
    match (identifier_span(row, col), byte_at(row, decl.col)) {
        (Some((start, _)), Some(from)) => declarator_reaches(row, from, start),
        _ => false,
    }
}

/// Whether, scanning `row` from byte `from` by tokens, the identifier at
/// byte `at` comes before anything that ends a declarator's prefix: only
/// type, qualifier and scope names, `::`, `*`, `&`, `(`, `...`, a template
/// argument list, and an attribute (`__attribute__((...))`, `[[...]]`,
/// `alignas(...)`, `__declspec(...)`) may lie between, whatever their
/// contents. So `fns` in `int (*fns[2])(int)` is reached; a `[`, a number,
/// `=`, `,`, `)`, `.` or `->` outside those lists ends the search, and so
/// does an identifier inside one (a template argument, an attribute's).
fn declarator_reaches(row: &str, from: usize, at: usize) -> bool {
    let b = row.as_bytes();
    // The byte after the group opened at `i` by `open` and closed by
    // `close`, nested, if it closes before `at`.
    let skip_group = |i: usize, open: u8, close: u8| -> Option<usize> {
        let mut depth = 0usize;
        for (k, &c) in b.iter().enumerate().take(at).skip(i) {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(k + 1);
                }
            }
        }
        None
    };
    let mut i = from;
    while i < at {
        let c = b[i];
        i = if c.is_ascii_whitespace() || matches!(c, b'*' | b'&' | b'(') {
            i + 1
        } else if b[i..].starts_with(b"::") {
            i + 2
        } else if b[i..].starts_with(b"...") {
            i + 3
        } else if c == b'<' {
            match skip_group(i, b'<', b'>') {
                Some(next) => next,
                None => return false,
            }
        } else if b[i..].starts_with(b"[[") {
            match skip_group(i, b'[', b']') {
                Some(next) => next,
                None => return false,
            }
        } else if is_ident_byte(c) && !c.is_ascii_digit() {
            let end = b[i..]
                .iter()
                .position(|&c| !is_ident_byte(c))
                .map_or(b.len(), |n| i + n);
            let word = &row[i..end];
            if matches!(
                word,
                "__attribute__" | "__attribute" | "__declspec" | "alignas" | "_Alignas"
            ) {
                let open = b[end..]
                    .iter()
                    .position(|c| !c.is_ascii_whitespace())
                    .map(|n| end + n);
                match open.filter(|&o| b[o] == b'(') {
                    Some(o) => match skip_group(o, b'(', b')') {
                        Some(next) => next,
                        None => return false,
                    },
                    None => return false,
                }
            } else {
                end
            }
        } else {
            return false;
        };
    }
    i == at
}

/// Whether the identifier at byte `start` of `row` is spelled as a member
/// access: `.` or `->` comes before it, whitespace aside (`s->p`, `s -> p`,
/// `(*s).p`). A scope operator (`S::p`) is not one: that names a static
/// data member, a variable.
fn member_access_at(row: &str, start: usize) -> bool {
    let before = row[..start].trim_end();
    before.ends_with('.') || before.ends_with("->")
}

/// `name` is `ident`, or a qualified name ending in `::ident`.
fn names(name: &str, ident: &str) -> bool {
    name == ident
        || name
            .strip_suffix(ident)
            .is_some_and(|rest| rest.ends_with("::"))
}

/// Ids by the name they go by.
type Named = BTreeMap<String, BTreeSet<i64>>;

/// A node an operation or call on the queried line touches, with the span
/// of that operation or call, when one is recorded: from its recorded line
/// and column to the end of its recorded expression, as `(line, column)`
/// positions, the end exclusive.
struct Touched {
    file_id: i64,
    node: i64,
    span: Option<((i64, i64), (i64, i64))>,
}

/// Add `id` under `name`, allocating the key only for a new name.
fn add_named(map: &mut Named, name: &str, id: i64) {
    match map.get_mut(name) {
        Some(ids) => {
            ids.insert(id);
        }
        None => {
            map.insert(name.to_string(), BTreeSet::from([id]));
        }
    }
}

/// `n` and the nodes joined to it by `points_to` either way, transitively: a
/// variable and its own storage, which hold one value. Sorted.
fn storage_class(db: &mut FlowDb<'_>, n: i64) -> Result<Vec<i64>> {
    let mut class = vec![n];
    let mut i = 0;
    while let Some(&m) = class.get(i) {
        i += 1;
        for outgoing in [true, false] {
            for e in db.edges(m, outgoing)?.iter() {
                let other = if outgoing { e.dst } else { e.src };
                if e.role == EdgeKind::Storage && !class.contains(&other) {
                    class.push(other);
                }
            }
        }
    }
    class.sort_unstable();
    Ok(class)
}

/// Whether anything outside `class` brings a member other than `n` a value,
/// an address or a field address, or loads into it.
fn fed_from_outside(db: &mut FlowDb<'_>, n: i64, class: &[i64]) -> Result<bool> {
    for &m in class.iter().filter(|&&m| m != n) {
        for e in db.edges(m, false)?.iter() {
            match e.role {
                EdgeKind::Value | EdgeKind::Storage if class.binary_search(&e.src).is_err() => {
                    return Ok(true)
                }
                EdgeKind::AddrOf | EdgeKind::Gep | EdgeKind::Load => return Ok(true),
                _ => {}
            }
        }
    }
    Ok(false)
}

/// Resolve FILE:LINE:COL to the start of a slice. `name` is the identifier
/// at the position; when `None`, it is read from each source file matching
/// `file` that is readable here.
///
/// A position inside a variable's declared name starts at the variable.
/// Otherwise the position is a use on a line some flow edge records: a
/// field access starts at the cells the line reads or writes for that
/// field, a variable at the variable. Without an identifier the line must
/// move exactly one name. A filter matching candidates in several files is
/// refused.
pub fn resolve_slice_start(
    conn: &Connection,
    file: &str,
    line: i64,
    col: i64,
    name: Option<&str>,
) -> Result<SliceStart> {
    if file.is_empty() {
        bail!("file filter must not be empty");
    }
    require_slice_schema(conn)?;
    let pattern = super::contains_pattern(file);
    let paths: Vec<(i64, String)> = conn
        .prepare("SELECT id, path FROM files WHERE path LIKE ?1 ESCAPE '!' ORDER BY path, id")?
        .query_map([&pattern], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let path_of: BTreeMap<i64, &str> = paths.iter().map(|(id, p)| (*id, p.as_str())).collect();

    // The declarations on this line, and the nodes the line touches, per
    // file, each with the span of the operation or call that touches it
    // (its recorded column and expression): the ends of the operations
    // written on the line (its `flow_origins` rows), the cells of the loads
    // and stores among them, the values its calls pass (the actuals of the
    // `flow_calls` rows of its call sites; the formal a call passes to is
    // never spelled on the line), and the variables its indirect calls go
    // through (`call_sites.callee_var`: `cb` in `cb();`, a line that moves
    // nothing else), in every matching file at once. `flow_origins` and
    // `call_sites` have no `(file_id, line)` index, so each arm is a full
    // scan.
    let symbols: Vec<super::SymbolRef> = super::find_symbols_at(conn, file, line, col)?
        .into_iter()
        .filter(|s| s.line == line)
        .collect();
    // An operation or call is on the line when it starts there or its
    // expression, which keeps its newlines, runs on to it (`p =` with `q;`
    // on the next line): the lines it covers are its own and as many more
    // as it has newlines.
    let mut touched: Vec<Touched> = conn
        .prepare(
            "WITH o AS (SELECT o.file_id, o.src_node, o.dst_node, o.kind, o.line, o.col, \
                               o.expression \
                        FROM flow_origins o \
                        JOIN files f ON f.id = o.file_id \
                        WHERE o.line <= ?1 AND f.path LIKE ?2 ESCAPE '!' \
                          AND o.line + length(o.expression) \
                              - length(replace(o.expression, char(10), '')) >= ?1) \
             SELECT file_id, src_node, line, col, expression FROM o \
             UNION SELECT file_id, dst_node, line, col, expression FROM o \
             UNION SELECT o.file_id, a.cell_node, o.line, o.col, o.expression FROM o \
                   JOIN flow_edges e ON e.src_node = o.src_node AND e.dst_node = o.dst_node \
                                    AND e.kind = o.kind \
                   JOIN flow_memory_access a ON a.edge_id = e.id \
                   WHERE o.kind IN ('load', 'store') AND a.cell_node IS NOT NULL \
             UNION SELECT cs.file_id, fc.src_node, cs.line, cs.col, ce.expression \
                   FROM flow_calls fc \
                   JOIN call_sites cs ON cs.id = fc.call_site_id \
                   JOIN files f ON f.id = cs.file_id \
                   LEFT JOIN flow_call_expressions ce ON ce.call_site_id = cs.id \
                   WHERE cs.line <= ?1 AND f.path LIKE ?2 ESCAPE '!' \
                     AND cs.line + length(coalesce(ce.expression, '')) \
                         - length(replace(coalesce(ce.expression, ''), char(10), '')) >= ?1 \
             UNION SELECT cs.file_id, n.id, cs.line, cs.col, ce.expression \
                   FROM call_sites cs \
                   JOIN flow_nodes n ON n.var_id = cs.callee_var AND n.kind = 'var' \
                   JOIN files f ON f.id = cs.file_id \
                   LEFT JOIN flow_call_expressions ce ON ce.call_site_id = cs.id \
                   WHERE cs.callee_var IS NOT NULL \
                     AND cs.line <= ?1 AND f.path LIKE ?2 ESCAPE '!' \
                     AND cs.line + length(coalesce(ce.expression, '')) \
                         - length(replace(coalesce(ce.expression, ''), char(10), '')) >= ?1",
        )?
        .query_map(rusqlite::params![line, pattern], |r| {
            let (from_line, from_col): (i64, i64) = (r.get(2)?, r.get(3)?);
            let expression: Option<String> = r.get(4)?;
            Ok(Touched {
                file_id: r.get(0)?,
                node: r.get(1)?,
                span: expression.filter(|e| !e.is_empty()).map(|e| {
                    let lines = e.matches('\n').count() as i64;
                    let last = e.rsplit('\n').next().unwrap_or("").chars().count() as i64;
                    let to = if lines == 0 {
                        (from_line, from_col + last)
                    } else {
                        (from_line + lines, 1 + last)
                    };
                    ((from_line, from_col), to)
                }),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // The identifier belongs to the operation or call whose span covers the
    // position: where one does, the others on the line are left out (`a->p`
    // and `b->p` in `a->p = x; b->p = y;`). Where none does (a macro
    // invocation spelling the identifier outside the expansion's text), or
    // a span is not recorded, every operation and call on the line stays.
    let covers = |t: &Touched| {
        t.span
            .is_some_and(|(from, to)| from <= (line, col) && (line, col) < to)
    };
    let covered: BTreeSet<i64> = touched
        .iter()
        .filter(|t| covers(t))
        .map(|t| t.file_id)
        .collect();
    touched.retain(|t| !covered.contains(&t.file_id) || t.span.is_none() || covers(t));

    // The source line of each file with a declaration or an operation on
    // it, read once; the identifier at the position, per file; and the
    // files whose line is readable here although the column is on no
    // identifier. Other matching files hold no start.
    let relevant: BTreeSet<String> = symbols
        .iter()
        .map(|s| s.path.as_str())
        .chain(touched.iter().map(|t| path_of[&t.file_id]))
        .map(str::to_string)
        .collect();
    let rows: BTreeMap<&str, Option<String>> = relevant
        .iter()
        .map(|p| (p.as_str(), source_line(p, line)))
        .collect();
    let mut idents: BTreeMap<&str, Option<String>> = BTreeMap::new();
    let mut no_identifier: BTreeSet<&str> = BTreeSet::new();
    for (&p, row) in &rows {
        let ident = match name {
            Some(n) => Some(n.to_string()),
            None => {
                let text = row.as_deref().map(|row| column_text(row, col));
                if text == Some(None) {
                    no_identifier.insert(p);
                }
                text.flatten()
            }
        };
        idents.insert(p, ident);
    }
    let ident_of = |path: &str| idents.get(path).map_or(name, Option::as_deref);
    let row_of = |path: &str| rows.get(path).and_then(Option::as_deref);

    // A declaration on this line of the identifier at the column. It must
    // occupy the identifier at the column: `int *p = obj->p;` declares the
    // first `p`, and the second is a field access. A declaration that only
    // shares the name (`t` in `std::thread t(...); t.join();`) is the start
    // unless the line accesses a field of that name. With no identifier to
    // read, the column must fall inside the declared name as recorded; a
    // readable column on no identifier (the `*` of `int *p = q;`) declares
    // nothing, and the line must then move exactly one value.
    let (declared, same_named): (Vec<super::SymbolRef>, Vec<super::SymbolRef>) = symbols
        .into_iter()
        .filter(|s| match ident_of(&s.path) {
            Some(i) => names(&s.name, i),
            None => s.covers(line, col),
        })
        .partition(|s| match ident_of(&s.path) {
            Some(_) => declares_identifier_at(s, row_of(&s.path), col),
            None => !no_identifier.contains(s.path.as_str()),
        });

    let mut db = FlowDb::new(conn);
    // Per file: field cells and variables, by the name they go by.
    let mut cells: BTreeMap<&str, Named> = BTreeMap::new();
    let mut vars: BTreeMap<&str, Named> = BTreeMap::new();
    for Touched { file_id, node, .. } in touched {
        let path = path_of[&file_id];
        let ident = ident_of(path);
        let row = db.node(node)?;
        if let Some(field) = row.field_name() {
            if ident.is_none_or(|i| i == field) {
                add_named(cells.entry(path).or_default(), field, node);
            }
            continue;
        }
        let Some((var_id, var_name)) = &row.var else {
            continue;
        };
        if row.synthetic {
            continue;
        }
        if ident.is_none_or(|i| names(var_name, i)) {
            add_named(vars.entry(path).or_default(), var_name, *var_id);
        }
    }

    let files: BTreeSet<&str> = declared
        .iter()
        .chain(&same_named)
        .map(|s| s.path.as_str())
        .chain(cells.keys().copied())
        .chain(vars.keys().copied())
        .collect();
    if files.len() > 1 {
        bail!(
            "`{file}` matches several files with a value at {line}:{col}: {}; \
             give more of the path",
            files.into_iter().collect::<Vec<_>>().join(", ")
        );
    }

    // The start at the declarations `declared`, the first one's name.
    let declaration_start = |declared: &[super::SymbolRef]| -> Result<Option<SliceStart>> {
        let Some(first) = declared.first() else {
            return Ok(None);
        };
        let var_ids: Vec<i64> = declared
            .iter()
            .filter(|s| s.name == first.name)
            .map(|s| s.var_id)
            .collect();
        let nodes = super::flow_nodes_of_variables(conn, &var_ids, false)?;
        if nodes.is_empty() {
            bail!(
                "no value-flow node for `{}`: no pointer or function value moves through it",
                first.name
            );
        }
        Ok(Some(SliceStart {
            name: short(&first.name).to_string(),
            kind: "variable".into(),
            at: FlowSite {
                path: first.path.clone(),
                line,
                col,
            },
            nodes,
        }))
    };
    if let Some(start) = declaration_start(&declared)? {
        return Ok(start);
    }

    let path = files.into_iter().next().map(str::to_string);
    let ident = path.as_deref().and_then(ident_of);
    let cells = path
        .as_ref()
        .and_then(|p| cells.remove(p.as_str()))
        .unwrap_or_default();
    let vars = path
        .as_ref()
        .and_then(|p| vars.remove(p.as_str()))
        .unwrap_or_default();
    if ident.is_none() {
        let candidates: BTreeSet<&str> =
            cells.keys().chain(vars.keys()).map(|n| short(n)).collect();
        if candidates.len() > 1 {
            bail!(
                "several values move on {file}:{line} and no identifier at column {col} \
                 picks one: {}; pass --name",
                candidates.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
    }
    // The field's cells go first, unless the identifier at the column is
    // spelled as a variable: the line is readable here, the column is on
    // the identifier, and no `.` or `->` precedes it (`p` and `S::p`, not
    // `s->p` or `(*s).p`). Each falls back to the other when its own has no
    // candidate (`s->p = p;` has both).
    let spelled_as_variable = path.as_deref().and_then(row_of).is_some_and(|row| {
        identifier_span(row, col).is_some_and(|(start, end)| {
            ident == Some(&row[start..end]) && !member_access_at(row, start)
        })
    });
    let site = FlowSite {
        path: path.unwrap_or_default(),
        line,
        col,
    };
    let field = cells.keys().next().cloned().map(|field| SliceStart {
        name: ident.map_or(field, str::to_string),
        kind: "field".into(),
        at: site.clone(),
        nodes: cells
            .values()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    });
    // A variable declared on the line beats a same-named one elsewhere
    // (a callee's parameter the line passes a value to).
    let variable = |site: FlowSite| -> Result<Option<SliceStart>> {
        if let Some(start) = declaration_start(&same_named)? {
            return Ok(Some(start));
        }
        let Some(full) = vars.keys().next() else {
            return Ok(None);
        };
        let var_ids: Vec<i64> = vars
            .values()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let nodes = super::flow_nodes_of_variables(conn, &var_ids, false)?;
        Ok(Some(SliceStart {
            name: ident.map_or_else(|| short(full).to_string(), str::to_string),
            kind: "variable".into(),
            at: site,
            nodes,
        }))
    };
    let start = match field {
        Some(_) if !spelled_as_variable => field,
        _ => variable(site)?.or(field),
    };
    if let Some(start) = start {
        return Ok(start);
    }
    // Whether any matching file has an identifier at the position, for
    // the hint below: read only now, as no file held a start.
    let readable = name.is_some()
        || idents.values().any(Option::is_some)
        || paths.iter().any(|(_, p)| {
            !relevant.contains(p.as_str())
                && source_line(p, line).is_some_and(|row| column_text(&row, col).is_some())
        });
    bail!(
        "no variable or field access `{}` at {file}:{line}:{col} that a pointer or \
         function value moves through{}",
        ident.or(name).unwrap_or("?"),
        if readable {
            ""
        } else {
            " (no identifier at the column in a source file readable here: pass --name)"
        }
    )
}

/// The two-stage value slice from `start`: stage 1 follows value flow
/// backwards to the sources, stage 2 forwards from each source (and the
/// start) to the sinks. Rules: `docs/ANALYSIS.md`, "Value slice".
pub fn value_slice(
    conn: &Connection,
    start: &SliceStart,
    opts: &SliceOptions,
) -> Result<ValueSlice> {
    require_slice_schema(conn)?;
    let mut db = FlowDb::new(conn);
    let starts: FxHashSet<i64> = start.nodes.iter().copied().collect();
    let mut reached = Reached::default();
    let mut truncated_up = false;
    let mut truncated_down = false;
    let mut roots: Vec<Root> = Vec::new();
    // Nodes stage 1 found nothing feeds, made sources.
    let mut alone_sources: FxHashSet<i64> = FxHashSet::default();

    // Stage 1: backwards to the sources. `visited` holds the nodes queued
    // for expansion; `origin_bases` the `addr_of` and `gep` sources
    // recorded, which are not expanded through that edge but may still be
    // on another, ordinary path.
    let mut visited: FxHashSet<i64> = starts.clone();
    let mut origin_bases: FxHashSet<i64> = FxHashSet::default();
    let mut queue: VecDeque<(i64, u32)> = start.nodes.iter().map(|&n| (n, 0)).collect();
    for &n in &start.nodes {
        reached.node(n, UP);
        reached.role(n, "start");
    }
    while let Some((n, depth)) = queue.pop_front() {
        let row = db.node(n)?;
        if !starts.contains(&n) {
            if let Some(kind) = row.boundary() {
                reached.role(n, "boundary");
                reached.source(n, kind);
                continue;
            }
            // `nullptr`, one constant node for the whole program: its value
            // goes to every null assignment, so stage 2 does not walk on
            // from it.
            if row.constant {
                reached.source(n, "null");
                continue;
            }
        }
        let (inc, out) = (db.edges(n, false)?, db.edges(n, true)?);
        let mut steps: Vec<(&FlowEdge, i64)> = Vec::new();
        let mut origins: Vec<&FlowEdge> = Vec::new();
        let mut loads: Vec<&FlowEdge> = Vec::new();
        for e in inc.iter() {
            match e.role {
                EdgeKind::Value | EdgeKind::Storage => steps.push((e, e.src)),
                EdgeKind::AddrOf | EdgeKind::Gep => origins.push(e),
                EdgeKind::Load => loads.push(e),
                _ => {}
            }
        }
        // A load whose cells are not recorded (`MEMORY_ACCESS_CAP`, or a
        // pointer that points at no memory) makes the node a source,
        // whatever else it receives.
        let unrecorded_from = if loads.is_empty() {
            Vec::new()
        } else {
            db.unrecorded_loads_into(n)?
        };
        let mut unrecorded = false;
        for &e in &loads {
            if unrecorded_from.contains(&e.src) {
                unrecorded = true;
                reached.edge(e, UP);
            }
        }
        if unrecorded {
            reached.source(n, "unrecorded_load");
            roots.push(Root::Value(n));
        }
        for e in out.iter() {
            if e.role == EdgeKind::Storage {
                steps.push((e, e.dst));
            }
        }
        // A node's edge into itself (a recursive call passing a parameter
        // on) brings no value from elsewhere, nor does one into its own
        // storage, which holds the same value (`&p` of a parameter `p`)
        // unless something outside the two feeds it.
        let class = if steps
            .iter()
            .any(|(e, o)| *o != n && e.role == EdgeKind::Storage)
        {
            storage_class(&mut db, n)?
        } else {
            vec![n]
        };
        let alone = origins.is_empty()
            && steps.iter().all(|(_, o)| class.binary_search(o).is_ok())
            && !fed_from_outside(&mut db, n, &class)?;
        if alone {
            for (e, _) in &steps {
                reached.edge(e, UP);
            }
            // One source per value, at the variable rather than its storage.
            let named = class.iter().any(|m| alone_sources.contains(m));
            if !unrecorded && !named {
                let mut source = n;
                for &m in &class {
                    if db.node(m)?.is(FlowNodeKind::Var) {
                        source = m;
                        break;
                    }
                }
                alone_sources.insert(source);
                reached.source(source, db.node(source)?.natural_source());
                roots.push(Root::Value(source));
            }
            continue;
        }
        if depth >= opts.up_depth {
            if steps.iter().any(|(_, o)| !visited.contains(o))
                || origins
                    .iter()
                    .any(|e| !visited.contains(&e.src) && !origin_bases.contains(&e.src))
            {
                truncated_up = true;
            }
            continue;
        }
        for &e in &origins {
            reached.edge(e, UP);
            origin_bases.insert(e.src);
            if e.role == EdgeKind::AddrOf {
                let origin = db.node(e.src)?.origin();
                reached.source(e.src, origin);
                roots.push(Root::Address(e.src));
            } else {
                reached.source(e.src, "field_address");
                roots.push(Root::Value(n));
            }
        }
        for (e, other) in &steps {
            reached.edge(e, UP);
            if visited.insert(*other) {
                queue.push_back((*other, depth + 1));
            }
        }
    }

    // Stage 2: forwards from the start and each source to the sinks. A walk
    // from a source leaves a function through its return value only along
    // the calls stage 1 came up through (`returns_up`): each call returns
    // its own value, which the context-insensitive graph merges. From the
    // start every caller is reached (a getter hands one member to all).
    let returns_up: FxHashSet<EdgeKey> = reached.edges.keys().copied().collect();
    // `(caller, callee)` pairs a walk entered the callee from: a value passed
    // into a call comes back out of that call (a pass-through function).
    // Returns skipped before their call was entered wait here for it.
    let mut entered: FxHashSet<CallKey> = FxHashSet::default();
    let mut waiting: BTreeMap<CallKey, Vec<(FlowEdge, u32)>> = BTreeMap::new();
    let mut frontier = Frontier::default();
    // Nodes a step past the depth limit leads to, and calls a step past it
    // enters: the slice is truncated if the walk, which may reach a node
    // shallower later on, ends without them.
    let mut beyond: Vec<i64> = Vec::new();
    let mut unentered: Vec<CallKey> = Vec::new();
    for &n in &start.nodes {
        frontier.push(Root::Value(n), 0, false);
    }
    for root in roots {
        frontier.push(root, 0, true);
    }
    while let Some((root, depth, from_source)) = frontier.pop() {
        let n = match root {
            Root::Address(o) => {
                reached.node(o, DOWN);
                let all = db.edges(o, true)?;
                let out: Vec<&FlowEdge> =
                    all.iter().filter(|e| e.role == EdgeKind::AddrOf).collect();
                if depth >= opts.down_depth {
                    beyond.extend(out.iter().map(|e| e.dst));
                    continue;
                }
                for e in &out {
                    reached.edge(e, DOWN);
                    frontier.push(Root::Value(e.dst), depth + 1, from_source);
                }
                continue;
            }
            Root::Value(n) => n,
        };
        reached.node(n, DOWN);
        let row = db.node(n)?;
        if !starts.contains(&n) && row.boundary().is_some() {
            reached.role(n, "boundary");
            reached.role(n, "sink");
            continue;
        }
        if matches!(
            row.kind,
            Some(FlowNodeKind::CallTarget | FlowNodeKind::Terminator)
        ) {
            reached.role(n, "sink");
            continue;
        }
        let (out, inc) = (db.edges(n, true)?, db.edges(n, false)?);
        let mut steps: Vec<(&FlowEdge, i64)> = Vec::new();
        // Per outgoing value step (the first `entries.len()` of `steps`):
        // the call it enters, if any.
        let mut entries: Vec<Option<CallKey>> = Vec::new();
        let mut uses: Vec<&FlowEdge> = Vec::new();
        let mut held = false;
        for e in out.iter() {
            match e.role {
                EdgeKind::Value | EdgeKind::Storage => {
                    let to = db.node(e.dst)?;
                    if from_source && !returns_up.contains(&e.id) && is_return(&row, &to) {
                        if let (Some(caller), Some(callee)) = (to.home, row.home) {
                            // A lambda reading or writing a variable it
                            // captured is no return: nothing is called.
                            if !db.capture_access(e, callee, caller)?
                                && !entered.contains(&(caller, callee))
                            {
                                held = true;
                                waiting
                                    .entry((caller, callee))
                                    .or_default()
                                    .push((e.clone(), depth));
                                continue;
                            }
                        }
                    }
                    entries.push(call_entry(&row, &to));
                    steps.push((e, e.dst));
                }
                EdgeKind::Load | EdgeKind::Gep | EdgeKind::Terminates => uses.push(e),
                _ => {}
            }
        }
        for e in inc.iter() {
            match e.role {
                EdgeKind::Storage => steps.push((e, e.src)),
                EdgeKind::Store => uses.push(e),
                _ => {}
            }
        }
        if depth >= opts.down_depth {
            // An entry edge not followed leaves the returns waiting for it
            // unreleased.
            truncated_down |= !uses.is_empty();
            beyond.extend(steps.iter().map(|(_, o)| *o));
            unentered.extend(entries.into_iter().flatten());
            continue;
        }
        if (steps.is_empty() && !held) || !uses.is_empty() {
            reached.role(n, "sink");
        }
        for e in &uses {
            reached.edge(e, DOWN);
        }
        // A call is entered once its entry edge is followed, not merely
        // seen: only then are the returns waiting for it released.
        for call in entries.into_iter().flatten() {
            if entered.insert(call) {
                for (r, d) in waiting.remove(&call).unwrap_or_default() {
                    if d >= opts.down_depth {
                        beyond.push(r.dst);
                        continue;
                    }
                    reached.edge(&r, DOWN);
                    frontier.push(Root::Value(r.dst), d + 1, true);
                }
            }
        }
        for (e, other) in &steps {
            reached.edge(e, DOWN);
            frontier.push(Root::Value(*other), depth + 1, from_source);
        }
    }

    truncated_down |= beyond.iter().any(|&o| !frontier.reached(o))
        || unentered.iter().any(|call| {
            !entered.contains(call)
                && waiting
                    .get(call)
                    .is_some_and(|w| w.iter().any(|(r, _)| !frontier.reached(r.dst)))
        });
    annotate(&mut db, start, opts, reached, truncated_up, truncated_down)
}

/// Contexts per node and the cross-context flag per edge.
fn annotate(
    db: &mut FlowDb<'_>,
    start: &SliceStart,
    opts: &SliceOptions,
    reached: Reached,
    truncated_up: bool,
    truncated_down: bool,
) -> Result<ValueSlice> {
    let mut contexts = ContextIndex::load(db.conn)?;
    let mut rows: BTreeMap<i64, Rc<NodeRow>> = BTreeMap::new();
    for &id in reached.nodes.keys() {
        rows.insert(id, db.node(id)?);
    }
    // The function an edge's statement runs in: its own, or that of the
    // private end it moves the value out of or into.
    let accessor = |e: &FlowEdge| -> Option<i64> {
        e.fn_id
            .or_else(|| rows[&e.src].private_home())
            .or_else(|| rows[&e.dst].private_home())
    };
    // Per edge (in `reached.edges` order): its accessor and that function's
    // contexts.
    let mut at: Vec<(Option<i64>, CtxSet)> = Vec::with_capacity(reached.edges.len());
    for (e, _) in reached.edges.values() {
        let f = accessor(e);
        let set = match f {
            Some(f) => contexts.of_fn(f)?,
            None => CtxSet::default(),
        };
        at.push((f, set));
    }
    // Node contexts: a private node's are its function's; any other's are
    // those of the slice's statements that touch it.
    let mut touched: BTreeMap<i64, BTreeSet<CtxKey>> = BTreeMap::new();
    for ((e, _), (f, set)) in reached.edges.values().zip(&at) {
        if f.is_none() {
            continue;
        }
        for end in [e.src, e.dst] {
            let row = &rows[&end];
            if row.private_home().is_none() && row.sharing() != Sharing::Constant {
                touched.entry(end).or_default().extend(set.iter().copied());
            }
        }
    }
    let mut node_ctx: BTreeMap<i64, CtxSet> = touched
        .into_iter()
        .map(|(id, set)| (id, Rc::new(set)))
        .collect();
    for (&id, row) in &rows {
        if let Some(f) = row.private_home() {
            node_ctx.insert(id, contexts.of_fn(f)?);
        }
    }

    let mut edges = Vec::new();
    for ((e, stages), (accessor, at)) in reached.edges.values().zip(&at) {
        let mut reasons: BTreeSet<&'static str> = BTreeSet::new();
        let (u, v) = (&rows[&e.src], &rows[&e.dst]);
        if u.sharing() != Sharing::Constant && v.sharing() != Sharing::Constant {
            let shared: Vec<i64> = [e.src, e.dst]
                .into_iter()
                .filter(|n| rows[n].sharing() == Sharing::Shared)
                .collect();
            if !shared.is_empty() {
                for s in shared {
                    if !at.is_empty() && node_ctx.get(&s).is_some_and(|c| c.len() >= 2) {
                        reasons.insert("contexts_differ");
                    }
                }
                if at.iter().any(|&c| contexts.self_concurrent(c)) {
                    reasons.insert("self_concurrent");
                }
            } else {
                // A value in one invocation's storage moves to another
                // context only at a start, or through a local's address (or
                // that of a field of it).
                if let (Some(hu), Some(hv)) = (u.home, v.home) {
                    if hu != hv
                        && contexts
                            .by_entry
                            .get(&hv)
                            .into_iter()
                            .flatten()
                            .any(|id| contexts.rows[id].starter == Some(hu))
                    {
                        reasons.insert("start");
                    }
                }
                // One invocation's storage touched by another function's
                // code: through the local's address, or as a variable a
                // lambda written in the owner's body captured (the access
                // is written in the lambda's).
                for row in [u, v] {
                    if let (Some(owner), &Some(f)) = (row.home, accessor) {
                        if owner == f
                            || !(row.automatic || (row.private_var() && db.written_in(owner, f)?))
                        {
                            continue;
                        }
                        let (theirs, here) = (contexts.of_fn(owner)?, contexts.of_fn(f)?);
                        if !Rc::ptr_eq(&theirs, &here) && theirs != here {
                            reasons.insert("contexts_differ");
                        }
                    }
                }
            }
        }
        edges.push(SliceEdge {
            from: e.src,
            to: e.dst,
            kind: e.kind.clone(),
            stages: stages.iter().map(|s| s.to_string()).collect(),
            site: e.site.clone(),
            function: e.fn_id.map(|f| db.fn_name(f)).transpose()?,
            cross_context: !reasons.is_empty(),
            reasons: reasons.iter().map(|r| r.to_string()).collect(),
        });
    }
    edges.sort_by(|a, b| (a.from, a.to, &a.kind, &a.site).cmp(&(b.from, b.to, &b.kind, &b.site)));

    let mut used: BTreeSet<CtxKey> = BTreeSet::new();
    let mut nodes = Vec::new();
    for (&id, (stages, roles)) in &reached.nodes {
        let row = &rows[&id];
        let ctx = node_ctx.get(&id).map(|set| set.as_ref());
        used.extend(ctx.into_iter().flatten().copied());
        nodes.push(SliceNode {
            id,
            kind: row.kind_name.clone(),
            label: row.label.clone(),
            var_kind: row.var_kind.clone(),
            loc_kind: row.loc_kind.map(|k| loc_kind_schema_str(k).to_string()),
            function: row.home.map(|f| db.fn_name(f)).transpose()?,
            decl: row.decl.clone(),
            sharing: row.sharing().name().into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
            source: reached.sources.get(&id).map(|s| s.to_string()),
            stages: [UP, DOWN]
                .into_iter()
                .filter(|s| stages.contains(s))
                .map(str::to_string)
                .collect(),
            contexts: ctx.into_iter().flatten().map(|c| c.id()).collect(),
        });
    }

    let mut slice_contexts = Vec::new();
    for key in used {
        slice_contexts.push(match key {
            CtxKey::Root => SliceContext {
                id: key.id(),
                kind: "root".into(),
                entry: None,
                start_site: None,
                api: None,
                multi_instance: None,
                self_concurrent: false,
            },
            CtxKey::Row(row_id) => {
                let r = &contexts.rows[&row_id];
                SliceContext {
                    id: key.id(),
                    kind: r.kind.clone(),
                    entry: Some(db.fn_name(r.entry)?),
                    start_site: r.start_site.clone(),
                    api: r.api.map(|f| db.fn_name(f)).transpose()?,
                    multi_instance: Some(r.multi_instance.clone()),
                    self_concurrent: r.self_concurrent,
                }
            }
        });
    }

    Ok(ValueSlice {
        start: start.clone(),
        up_depth: opts.up_depth,
        down_depth: opts.down_depth,
        truncated_up,
        truncated_down,
        contexts: slice_contexts,
        nodes,
        edges,
        limits: limits(),
    })
}

/// Render a slice as `text` (for people) or `json` (for tools).
pub fn render_slice(slice: &ValueSlice, format: crate::RenderFormat) -> Result<String> {
    match format {
        crate::RenderFormat::Json => {
            let mut out = serde_json::to_string_pretty(slice)?;
            out.push('\n');
            Ok(out)
        }
        crate::RenderFormat::Text => Ok(render_text(slice)),
        _ => bail!("`inspect slice` renders as text or json"),
    }
}

fn render_text(s: &ValueSlice) -> String {
    use std::fmt::Write;
    let by_id: BTreeMap<i64, &SliceNode> = s.nodes.iter().map(|n| (n.id, n)).collect();
    let node_text = |id: i64| -> String {
        match by_id.get(&id) {
            Some(n) => {
                let mut t = format!("#{id} {}", n.label);
                match (&n.var_kind, &n.function, &n.loc_kind) {
                    (Some(k), Some(f), _) => write!(t, " ({k} in {f})").unwrap(),
                    (Some(k), None, _) => write!(t, " ({k})").unwrap(),
                    (None, _, Some(k)) => write!(t, " [{k}]").unwrap(),
                    _ => {}
                }
                t
            }
            None => format!("#{id}"),
        }
    };
    let mut out = String::new();
    writeln!(
        out,
        "value slice from {} ({}) at {}:{}",
        s.start.name,
        s.start.kind,
        super::basename(&s.start.at.path),
        format_args!("{}:{}", s.start.at.line, s.start.at.col)
    )
    .unwrap();
    let starts: Vec<String> = s.start.nodes.iter().map(|&n| node_text(n)).collect();
    writeln!(out, "start: {}", starts.join(", ")).unwrap();
    let stage = |depth: u32, truncated: bool| {
        if truncated {
            format!("depth {depth}, truncated")
        } else {
            format!("depth {depth}, complete")
        }
    };
    writeln!(
        out,
        "stage 1 (up): {}; stage 2 (down): {}",
        stage(s.up_depth, s.truncated_up),
        stage(s.down_depth, s.truncated_down)
    )
    .unwrap();

    writeln!(out, "\ncontexts:").unwrap();
    if s.contexts.is_empty() {
        writeln!(out, "  (none reach the slice)").unwrap();
    }
    for c in &s.contexts {
        let mut line = match &c.entry {
            Some(entry) => format!("  {:<6} {:<12}{entry}", c.id, c.kind),
            None => format!(
                "  {:<6} code reached from a function no recorded call reaches \
                 (main, a framework callback, an exported API)",
                c.id
            ),
        };
        if let Some(site) = &c.start_site {
            write!(line, ", started at {}", site.display()).unwrap();
            if let Some(api) = &c.api {
                write!(line, " by {api}").unwrap();
            }
        }
        if c.self_concurrent {
            line.push_str(", self-concurrent");
        }
        writeln!(out, "{line}").unwrap();
    }

    let listed = |out: &mut String, title: &str, role: &str| {
        writeln!(out, "\n{title}:").unwrap();
        let mut any = false;
        for n in s.nodes.iter().filter(|n| n.roles.iter().any(|r| r == role)) {
            any = true;
            let what = match role {
                "source" => n.source.clone(),
                _ => n
                    .roles
                    .iter()
                    .any(|r| r == "boundary")
                    .then(|| "boundary".to_string()),
            };
            match what {
                Some(kind) => writeln!(out, "  {} <{kind}>", node_text(n.id)).unwrap(),
                None => writeln!(out, "  {}", node_text(n.id)).unwrap(),
            }
        }
        if !any {
            writeln!(out, "  (none)").unwrap();
        }
    };
    listed(&mut out, "sources", "source");
    listed(&mut out, "sinks", "sink");

    for (title, stage) in [("edges, stage 1 (up)", UP), ("edges, stage 2 (down)", DOWN)] {
        writeln!(out, "\n{title}:").unwrap();
        let mut any = false;
        for e in s
            .edges
            .iter()
            .filter(|e| e.stages.iter().any(|x| x == stage))
        {
            any = true;
            let site = e
                .site
                .as_ref()
                .map(|p| format!("@{}", p.display()))
                .unwrap_or_default();
            let mut line = format!(
                "  {} -{}{}-> {}",
                node_text(e.from),
                e.kind,
                site,
                node_text(e.to)
            );
            if e.cross_context {
                write!(line, "  cross-context: {}", e.reasons.join(", ")).unwrap();
            }
            writeln!(out, "{line}").unwrap();
        }
        if !any {
            writeln!(out, "  (none)").unwrap();
        }
    }

    writeln!(out, "\nnode contexts:").unwrap();
    for n in &s.nodes {
        let shown: Vec<&str> = n.contexts.iter().take(6).map(String::as_str).collect();
        let more = n.contexts.len().saturating_sub(shown.len());
        let mut list = shown.join(", ");
        if more > 0 {
            write!(list, ", +{more} more").unwrap();
        }
        if list.is_empty() {
            list.push('-');
        }
        writeln!(out, "  {} {} {{{list}}}", node_text(n.id), n.sharing).unwrap();
    }

    let flagged = s.edges.iter().filter(|e| e.cross_context).count();
    writeln!(
        out,
        "\n{} nodes, {} edges, {} cross-context",
        s.nodes.len(),
        s.edges.len(),
        flagged
    )
    .unwrap();
    writeln!(out, "limits:").unwrap();
    for l in &s.limits {
        writeln!(out, "  - {l}").unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SCHEMA_V7;

    #[test]
    fn a_member_access_is_spelled_with_a_dot_or_an_arrow() {
        for (row, needle, expect) in [
            ("    s->p = p;", "p", true),
            ("    s -> p = p;", "p", true),
            ("    (*s).p = p;", "p", true),
            ("    s->p = p;", "p;", false),
            ("    g = S::p;", "p;", false),
            ("    sink(p);", "p", false),
            ("p = q;", "p", false),
        ] {
            let start = row.find(needle).unwrap();
            assert_eq!(member_access_at(row, start), expect, "{row:?} at {start}");
        }
    }

    #[test]
    fn identifier_at_reads_the_word_under_the_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.cpp");
        std::fs::write(&path, "int x;\n    cb_->OnError(code_2);\n").unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(identifier_at(path, 2, 5).as_deref(), Some("cb_"));
        assert_eq!(identifier_at(path, 2, 7).as_deref(), Some("cb_"));
        assert_eq!(identifier_at(path, 2, 8), None, "`-` is not in a word");
        assert_eq!(identifier_at(path, 2, 18).as_deref(), Some("code_2"));
        assert_eq!(identifier_at(path, 2, 23).as_deref(), Some("code_2"));
        assert_eq!(identifier_at(path, 1, 5).as_deref(), Some("x"));
        assert_eq!(identifier_at(path, 9, 1), None);
        assert_eq!(identifier_at("/no/such/file.cpp", 1, 1), None);
        // Columns count characters, as LineMap's do, not bytes, and an
        // identifier may hold characters outside ASCII.
        std::fs::write(path, "/* é */ int *ñame_x = größe->p;\n").unwrap();
        assert_eq!(identifier_at(path, 1, 15).as_deref(), Some("ñame_x"));
        assert_eq!(identifier_at(path, 1, 14).as_deref(), Some("ñame_x"));
        assert_eq!(identifier_at(path, 1, 13), None, "`*` is not in a word");
        assert_eq!(identifier_at(path, 1, 23).as_deref(), Some("größe"));
        assert_eq!(identifier_at(path, 1, 25).as_deref(), Some("größe"));
        assert_eq!(identifier_at(path, 1, 30).as_deref(), Some("p"));
    }

    #[test]
    fn a_declarator_reaches_its_name_past_its_prefix_only() {
        // `(row, declarator start, identifier, nth occurrence, reached)`.
        let cases = [
            ("    int (*fns[2])(int) = {A, B};", "(", "fns", 0, true),
            ("    int (*fns[N])(int) = {A, B};", "(", "N", 0, false),
            ("    int (*fns[2])(int x) = {};", "(", "x", 0, false),
            ("    int *p = obj->p;", "*", "p", 0, true),
            ("    int *p = obj->p;", "*", "p", 1, false),
            ("    std::array<Cb *, 4> cbs = {};", "std", "cbs", 0, true),
            ("    std::array<Cb *, 4> cbs = {};", "std", "Cb", 0, false),
            ("    std::function<void(int)> cb;", "std", "cb", 0, true),
            ("    char buf[16];", "buf", "buf", 0, true),
            (
                "    Cb *__attribute__((unused)) at = o->at;",
                "*",
                "at",
                0,
                true,
            ),
            (
                "    Cb *__attribute__((unused)) at = o->at;",
                "*",
                "unused",
                0,
                false,
            ),
            (
                "    Cb *__attribute__((unused)) at = o->at;",
                "*",
                "at",
                1,
                false,
            ),
            ("    [[maybe_unused]] Cb *m = n;", "[", "m", 1, true),
            ("    alignas(16) Cb *al = in;", "alignas", "al", 1, true),
            ("void V(A &&...args) {}", "A", "args", 0, true),
            ("Cb *R::current_ = nullptr;", "*", "current_", 0, true),
            ("    Cb *x = in, *y[2] = {};", "*", "y", 0, false),
        ];
        for (row, from, ident, nth, reached) in cases {
            let from = row.find(from).unwrap();
            let at = row
                .match_indices(ident)
                .map(|(i, _)| i)
                .filter(|&i| i == 0 || !is_ident_byte(row.as_bytes()[i - 1]))
                .nth(nth)
                .unwrap();
            assert_eq!(
                declarator_reaches(row, from, at),
                reached,
                "{row:?} from {from} to {ident}#{nth}"
            );
        }
    }

    #[test]
    fn the_start_walk_takes_over_a_queued_source_entry_it_covers() {
        let mut f = Frontier::default();
        let (a, b) = (Root::Value(1), Root::Value(2));
        f.push(a, 2, true);
        f.push(b, 1, true);
        // At the source's depth or less, the start's walk covers the
        // source's and takes its waiting entry over, at its own depth.
        f.push(a, 1, false);
        f.push(a, 3, true);
        f.push(a, 3, false);
        assert_eq!(f.pop(), Some((a, 1, false)), "one entry, the start's walk");
        assert_eq!(f.pop(), Some((b, 1, true)));
        // Expanded already by a source's walk: the start's walk expands it
        // again, once.
        f.push(b, 2, false);
        f.push(b, 2, false);
        assert_eq!(f.pop(), Some((b, 2, false)));
        assert_eq!(f.pop(), None);
    }

    #[test]
    fn a_deeper_start_arrival_keeps_its_own_depth() {
        // The start's walk arriving deeper than a waiting source's entry does
        // not take that entry's depth: each walk keeps its own, and both
        // expand the root.
        let mut f = Frontier::default();
        let a = Root::Value(1);
        f.push(a, 1, true);
        f.push(a, 3, false);
        f.push(a, 2, false);
        assert_eq!(f.pop(), Some((a, 1, true)));
        assert_eq!(f.pop(), Some((a, 2, false)));
        assert_eq!(f.pop(), None);
        // A source's arrival no shallower than either expansion adds
        // nothing; a shallower one is the source's own entry.
        f.push(a, 2, true);
        f.push(a, 1, true);
        assert_eq!(f.pop(), None);
        f.push(a, 0, true);
        assert_eq!(f.pop(), Some((a, 0, true)));
        assert_eq!(f.pop(), None);
    }

    #[test]
    fn a_shallower_arrival_queues_a_root_again() {
        let mut f = Frontier::default();
        let a = Root::Value(1);
        f.push(a, 3, true);
        assert_eq!(f.pop(), Some((a, 3, true)));
        f.push(a, 3, true);
        assert_eq!(f.pop(), None, "no shallower: nothing to add");
        // Shallower than its expansion: queued again, and arrivals of its
        // walk while it waits fold into that entry.
        f.push(a, 2, true);
        f.push(a, 1, true);
        f.push(a, 2, false);
        assert_eq!(f.pop(), Some((a, 1, true)));
        assert_eq!(f.pop(), Some((a, 2, false)));
        // The start's walk at depth 2 covers either walk at 2 or more.
        f.push(a, 2, true);
        f.push(a, 3, false);
        assert_eq!(f.pop(), None);
        f.push(a, 0, true);
        assert_eq!(f.pop(), Some((a, 0, true)));
        assert_eq!(f.pop(), None);
    }

    #[test]
    fn qualified_names() {
        assert!(names("Registry::current_", "current_"));
        assert!(names("current_", "current_"));
        assert!(!names("Registry::xcurrent_", "current_"));
    }

    /// `main` starts `worker` on a thread and calls `util`; an IPC handler
    /// also calls `util`, and a proxy reaches the handler over a bridge.
    fn context_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(SCHEMA_V7).unwrap();
        conn.execute_batch(
            "INSERT INTO files (id, path, sha256) VALUES (1, '/p/main.cpp', '');
             INSERT INTO functions (id, name, file_id, line_start, line_end, linkage, signature, is_defined) VALUES
               (1, 'main', 1, 1, 9, 'external', '', 1),
               (2, 'Stub::Handle', 1, 10, 19, 'external', '', 1),
               (3, 'worker', 1, 20, 29, 'external', '', 1),
               (4, 'util', 1, 30, 39, 'external', '', 1),
               (5, 'Proxy::Handle', 1, 40, 49, 'external', '', 1),
               (6, 'Walk', 1, 50, 59, 'external', '', 1),
               (7, 'Visit', 1, 60, 69, 'external', '', 1),
               (8, 'Even', 1, 70, 79, 'external', '', 1),
               (9, 'Odd', 1, 80, 89, 'external', '', 1),
               (10, 'Spin', 1, 90, 99, 'external', '', 1);
             INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, callee_text, is_direct) VALUES
               (10, 1, 1, 5, 5, 'pthread_create', 1),
               (11, 1, 1, 6, 5, 'util', 1),
               (12, 2, 1, 15, 5, 'util', 1),
               (13, 6, 1, 55, 5, 'Walk', 1),
               (14, 6, 1, 56, 5, 'Visit', 1),
               (15, 8, 1, 75, 5, 'Odd', 1),
               (16, 9, 1, 85, 5, 'Even', 1),
               (17, 3, 1, 25, 5, 'Spin', 1),
               (18, 10, 1, 95, 5, 'Spin', 1);
             INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) VALUES
               (1, 10, 1, 3, 'indirect'),
               (2, 11, 1, 4, 'direct'),
               (3, 12, 2, 4, 'direct'),
               (4, NULL, 5, 2, 'ipc'),
               (5, 13, 6, 6, 'direct'),
               (6, 14, 6, 7, 'direct'),
               (7, 15, 8, 9, 'direct'),
               (8, 16, 9, 8, 'direct'),
               (9, 17, 3, 10, 'direct'),
               (10, 18, 10, 10, 'direct');
             INSERT INTO execution_contexts (id, kind, entry_fn_id, call_site_id, multi_instance, self_concurrent) VALUES
               (1, 'thread', 3, 10, 'unknown', 0),
               (2, 'ipc_handler', 2, NULL, 'unknown', 1);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn contexts_follow_ordinary_calls_only() {
        let conn = context_db();
        let mut index = ContextIndex::load(&conn).unwrap();
        let ids = |set: CtxSet| set.iter().map(|&c| c.id()).collect::<Vec<_>>();
        // The start edge main -> worker does not make main's root reach it.
        assert_eq!(ids(index.of_fn(3).unwrap()), ["C1"]);
        // The bridge does not make the proxy's caller reach the handler.
        assert_eq!(ids(index.of_fn(2).unwrap()), ["C2"]);
        assert_eq!(ids(index.of_fn(4).unwrap()), ["C2", "root"]);
        assert_eq!(ids(index.of_fn(1).unwrap()), ["root"]);
        assert_eq!(ids(index.of_fn(5).unwrap()), ["root"]);
        // A recursion nothing outside it calls is root's code, as is what it
        // calls; one the thread calls is the thread's alone.
        assert_eq!(ids(index.of_fn(6).unwrap()), ["root"]);
        assert_eq!(ids(index.of_fn(7).unwrap()), ["root"]);
        assert_eq!(ids(index.of_fn(8).unwrap()), ["root"]);
        assert_eq!(ids(index.of_fn(9).unwrap()), ["root"]);
        assert_eq!(ids(index.of_fn(10).unwrap()), ["C1"]);
        assert!(index.self_concurrent(CtxKey::Row(2)));
        assert!(!index.self_concurrent(CtxKey::Row(1)));
        assert!(!index.self_concurrent(CtxKey::Root));
    }

    #[test]
    fn an_older_database_is_refused_with_a_way_out() {
        // An earlier v7 export lacks a structure the slice reads.
        for drop in [
            "DROP TABLE execution_contexts",
            "DROP TABLE flow_origins",
            "DROP TABLE flow_memory_access",
        ] {
            let conn = context_db();
            conn.execute_batch(drop).unwrap();
            let err = resolve_slice_start(&conn, "main.cpp", 5, 5, Some("x")).unwrap_err();
            assert!(
                err.to_string().contains("re-run `trace analyze`"),
                "{drop}: {err}"
            );
            let start = SliceStart {
                name: "x".into(),
                kind: "variable".into(),
                at: FlowSite {
                    path: "/p/main.cpp".into(),
                    line: 5,
                    col: 5,
                },
                nodes: vec![1],
            };
            let err = value_slice(&conn, &start, &SliceOptions::default()).unwrap_err();
            assert!(
                err.to_string().contains("re-run `trace analyze`"),
                "{drop}: {err}"
            );
        }
    }

    #[test]
    fn a_merge_output_is_refused() {
        let conn = context_db();
        conn.execute_batch(
            "INSERT INTO analysis_run (trace_version, schema_version, target_root, created_at, \
             options_json) VALUES ('t', 7, '/p', '0', '{\"stage\":\"merge\"}')",
        )
        .unwrap();
        let err = resolve_slice_start(&conn, "main.cpp", 5, 5, Some("x")).unwrap_err();
        assert!(err.to_string().contains("trace-merge"), "{err}");
    }
}
