//! `trace inspect sequence`: bounded call sequences rebuilt from the
//! call-order CFGs of a `--sequencing` export (docs/ANALYSIS.md, "Call
//! sequencing (control-flow sidecar)").
//!
//! The query is demand-driven: it loads only the graphs of the functions it
//! reaches, inlines callees up to a depth and budget, and never stores or
//! enumerates paths. Relations come from the loaded graph of one function:
//! the local order inside a block, block reachability, and dominators over
//! non-exception edges.

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use rustc_hash::FxHashMap;
use serde::Serialize;
use std::collections::VecDeque;
use std::fmt::Write;
use std::rc::Rc;
use trace_ir::{EdgeKind, FileId, FunctionCfg, GapKind, Span};

/// `analysis_run.options_json.call_sequencing`: `true` from
/// `trace analyze --sequencing`, `"partial"` from a trace-merge of inputs
/// only some of which were sequenced. What `inspect sequence` reports it
/// read: every function's graph (`complete`), or only some (`partial`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequencingMarker {
    Complete,
    Partial,
    Absent,
}

impl SequencingMarker {
    pub fn read(conn: &Connection) -> Result<Self> {
        // Run options that are not JSON record no call order.
        let options = match super::run_options(conn) {
            Ok(options) => options,
            Err(err) if err.is::<serde_json::Error>() => None,
            Err(err) => return Err(err),
        };
        Ok(match options.as_ref().map(|o| &o["call_sequencing"]) {
            Some(serde_json::Value::Bool(true)) => Self::Complete,
            Some(serde_json::Value::String(s)) if s == "partial" => Self::Partial,
            _ => Self::Absent,
        })
    }

    /// The marker of a `trace analyze` run, with or without `--sequencing`.
    pub fn analysed(sequencing: bool) -> Self {
        if sequencing {
            Self::Complete
        } else {
            Self::Absent
        }
    }

    /// The marker of a trace-merge output: complete only when every input
    /// is, absent only when none is sequenced.
    pub fn merged(inputs: &[Self]) -> Self {
        if inputs.iter().all(|&s| s == Self::Complete) {
            Self::Complete
        } else if inputs.iter().all(|&s| s == Self::Absent) {
            Self::Absent
        } else {
            Self::Partial
        }
    }

    /// The `options_json.call_sequencing` value [`Self::read`] reads back.
    pub fn to_json(self) -> serde_json::Value {
        match self {
            Self::Complete => serde_json::json!(true),
            Self::Partial => serde_json::json!("partial"),
            Self::Absent => serde_json::json!(false),
        }
    }

    /// The capability a sequence reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Absent => "absent",
        }
    }

    /// What a query of a partial database should say about it.
    pub fn notice(self) -> Option<&'static str> {
        (self == Self::Partial).then_some(
            "call sequencing is partial: functions from merged inputs analysed without \
             `--sequencing` have no call-order graph",
        )
    }
}

/// Fail with the flag to regenerate a database that records no call order;
/// a partial one is accepted (see [`SequencingMarker::notice`]).
pub fn require_sequencing(conn: &Connection) -> Result<SequencingMarker> {
    let missing = "database records no call order; re-run `trace analyze --sequencing` \
                   to build the call-order graphs `inspect sequence` reads";
    if !super::table_exists(conn, "cfg_functions")? {
        bail!(missing);
    }
    match SequencingMarker::read(conn)? {
        SequencingMarker::Absent => bail!(missing),
        marker => Ok(marker),
    }
}

/// Bounds of one query (`trace inspect sequence --depth ...`).
#[derive(Debug, Clone, Serialize)]
pub struct SequenceOptions {
    /// Call levels inlined below the starting function.
    pub depth: u32,
    /// Events shown across all frames; an inlined callee that would exceed
    /// it is cut with a `max_events` marker.
    pub max_events: usize,
    /// Frames (function graphs) shown, the starting ones included.
    pub max_frames: usize,
    /// Targets of one event considered for inlining.
    pub max_targets: usize,
    /// Inline the callback an execution-context start runs.
    pub inline_contexts: bool,
}

impl Default for SequenceOptions {
    fn default() -> Self {
        Self {
            depth: 1,
            max_events: 200,
            max_frames: 32,
            max_targets: 4,
            inline_contexts: false,
        }
    }
}

/// One call found by source position: a `cfg_events` row (`event` of graph
/// `cfg`), and the same call in the function's other variants.
#[derive(Debug, Clone, Serialize)]
pub struct EventRef {
    pub event: i64,
    pub cfg: i64,
    pub function: i64,
    pub callee: String,
    pub path: String,
    pub line: i64,
    pub col: i64,
    /// `(event, cfg)` of the call in every variant of `function` compiling
    /// it, `(event, cfg)` first; empty means that one alone.
    #[serde(skip)]
    pub variants: Vec<(i64, i64)>,
}

impl EventRef {
    /// `(event, cfg)` of each variant's copy of the call.
    fn copies(&self) -> Vec<(i64, i64)> {
        if self.variants.is_empty() {
            vec![(self.event, self.cfg)]
        } else {
            self.variants.clone()
        }
    }

    fn display(&self) -> String {
        format!("{}() at {}", self.callee, self.position())
    }

    fn position(&self) -> String {
        format!("{}:{}:{}", super::basename(&self.path), self.line, self.col)
    }
}

#[derive(Debug, Clone)]
pub enum SequenceScope {
    /// The variants of a function's body; with `file`, those compiled from
    /// or written in a file matching it (all of them when none is).
    Function { function: i64, file: Option<String> },
    /// The part of an event's body reachable from it.
    From(EventRef),
    /// The blocks on paths from the first event to the second, and their
    /// relation.
    Between(EventRef, EventRef),
}

/// The function `name` names (as `resolve_function_target`), its bodies
/// narrowed to `file`. Units that each define one external function share
/// its row, which records only the first definition's file, so `file` may
/// name a body rather than the function.
pub fn resolve_sequence_function(
    conn: &Connection,
    name: &str,
    file: Option<&str>,
) -> Result<SequenceScope> {
    require_sequencing(conn)?;
    let function = match super::resolve_function_target(conn, Some(name), file, None) {
        Ok(f) => f.id,
        Err(err) => {
            let Some(file) = file else { return Err(err) };
            match super::resolve_function_target(conn, Some(name), None, None) {
                Ok(f) if !variants_in(conn, f.id, file)?.is_empty() => f.id,
                _ => return Err(err),
            }
        }
    };
    Ok(SequenceScope::Function {
        function,
        file: file.map(str::to_owned),
    })
}

/// The graphs of `function` compiled from, or with a block written in, a
/// file whose path contains `file`.
fn variants_in(conn: &Connection, function: i64, file: &str) -> Result<Vec<i64>> {
    Ok(conn
        .prepare_cached(
            "SELECT cf.id FROM cfg_functions cf \
             LEFT JOIN files t ON t.id = cf.tu_file_id \
             WHERE cf.fn_id = ?1 AND (t.path LIKE ?2 ESCAPE '!' OR EXISTS ( \
                 SELECT 1 FROM cfg_blocks b JOIN files f ON f.id = b.file_id \
                 WHERE b.cfg_id = cf.id AND f.path LIKE ?2 ESCAPE '!')) \
             ORDER BY cf.variant, cf.id",
        )?
        .query_map(
            rusqlite::params![function, super::contains_pattern(file)],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?)
}

/// The event written at `file:line` (spelling or macro expansion position),
/// narrowed by column and callee name (`name` or a `::name` suffix).
pub fn resolve_event(
    conn: &Connection,
    file: &str,
    line: i64,
    col: Option<i64>,
    callee: Option<&str>,
) -> Result<EventRef> {
    if file.is_empty() {
        bail!("file filter must not be empty");
    }
    require_sequencing(conn)?;
    let pattern = super::contains_pattern(file);
    // A call spelled in a macro body matches by spelling at every
    // invocation; its invocation position is kept to suggest instead. One
    // row per site, in site order, with the event's callee spelling.
    let mut stmt = conn.prepare(
        "SELECT e.id, e.cfg_id, cf.fn_id, f.path, e.line, e.col, \
                CASE WHEN e.line = ?1 AND f.path LIKE ?2 ESCAPE '!' THEN x.path END, \
                e.expansion_line, cs.callee_text \
         FROM cfg_events e \
         JOIN cfg_functions cf ON cf.id = e.cfg_id \
         JOIN files f ON f.id = e.file_id \
         LEFT JOIN files x ON x.id = e.expansion_file_id \
         LEFT JOIN cfg_event_sites es ON es.event_id = e.id \
         LEFT JOIN call_sites cs ON cs.id = es.call_site_id \
         WHERE ((e.line = ?1 AND f.path LIKE ?2 ESCAPE '!' AND (?3 IS NULL OR e.col = ?3)) \
             OR (e.expansion_line = ?1 AND x.path LIKE ?2 ESCAPE '!' \
                 AND (?3 IS NULL OR e.expansion_col = ?3))) \
         ORDER BY e.id, cs.id",
    )?;
    // The events found, each with its sites' callee spellings and the
    // macro invocation (`file:line`) it was matched at.
    let mut found: Vec<(EventRef, Vec<String>, Option<String>)> = Vec::new();
    let mut rows = stmt.query(rusqlite::params![line, pattern, col])?;
    while let Some(r) = rows.next()? {
        let event: i64 = r.get(0)?;
        if found.last().is_none_or(|f| f.0.event != event) {
            let expansion: (Option<String>, Option<i64>) = (r.get(6)?, r.get(7)?);
            let event = EventRef {
                event,
                cfg: r.get(1)?,
                function: r.get(2)?,
                callee: String::new(),
                path: r.get(3)?,
                line: r.get(4)?,
                col: r.get(5)?,
                variants: Vec::new(),
            };
            let invocation = expansion
                .0
                .zip(expansion.1)
                .map(|(path, line)| format!("{}:{line}", super::basename(&path)));
            found.push((event, Vec::new(), invocation));
        }
        if let (Some(f), Some(text)) = (found.last_mut(), r.get::<_, Option<String>>(8)?) {
            f.1.push(text);
        }
    }
    // Each variant of a function compiling the call has its own copy of
    // it: copies at one position with one callee, each in another graph of
    // the function, are one candidate (two events of one graph stay two).
    let mut candidates: Vec<EventRef> = Vec::new();
    let mut invocation = None;
    for (mut event, callees, expansion) in found {
        if callee.is_some_and(|want| !callees.iter().any(|c| super::names_function(c, want))) {
            continue;
        }
        if invocation.is_none() {
            invocation = expansion;
        }
        event.callee = callees.into_iter().next().unwrap_or_default();
        let copy = (event.event, event.cfg);
        let same = candidates.iter_mut().find(|c| {
            (c.function, &c.path, c.line, c.col, &c.callee)
                == (
                    event.function,
                    &event.path,
                    event.line,
                    event.col,
                    &event.callee,
                )
                && c.variants.iter().all(|&(_, cfg)| cfg != event.cfg)
        });
        match same {
            Some(c) => c.variants.push(copy),
            None => {
                event.variants.push(copy);
                candidates.push(event);
            }
        }
    }
    match candidates.len() {
        0 => bail!("no call event at {file}:{line}{}", narrowed(col, callee)),
        1 => Ok(candidates.remove(0)),
        n => {
            let mut listed = candidates
                .iter()
                .take(MAX_LISTED_CANDIDATES)
                .map(|c| format!("{} at {}", c.callee, c.position()))
                .collect::<Vec<_>>()
                .join(", ");
            if n > MAX_LISTED_CANDIDATES {
                write!(listed, ", and {} more", n - MAX_LISTED_CANDIDATES).unwrap();
            }
            let hint = match invocation {
                Some(at) => format!(
                    "; calls spelled in a macro body match at every invocation, \
                     so name the invocation's position (such as {at})"
                ),
                None => String::new(),
            };
            bail!(
                "{n} call events at {file}:{line}{}: {listed}; pick one with --col or --callee{hint}",
                narrowed(col, callee),
            )
        }
    }
}

/// Candidates an ambiguous position error lists before counting the rest.
const MAX_LISTED_CANDIDATES: usize = 10;

fn narrowed(col: Option<i64>, callee: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(c) = col {
        write!(out, ":{c}").unwrap();
    }
    if let Some(c) = callee {
        write!(out, " calling {c}").unwrap();
    }
    out
}

/// How the invocation of one event relates to another's; see
/// docs/ANALYSIS.md, "Events and what \"before\" means".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    SequencedBefore,
    GuaranteedBefore,
    MayPrecede,
    MayFollow,
    MayBoth,
    Exclusive,
    Unordered,
    Unknown,
}

impl Relation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SequencedBefore => "sequenced_before",
            Self::GuaranteedBefore => "guaranteed_before",
            Self::MayPrecede => "may_precede",
            Self::MayFollow => "may_follow",
            Self::MayBoth => "may_both",
            Self::Exclusive => "exclusive",
            Self::Unordered => "unordered",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SeqLoc {
    pub path: String,
    pub line: i64,
    pub col: i64,
}

impl SeqLoc {
    fn display(&self) -> String {
        format!("{}:{}:{}", super::basename(&self.path), self.line, self.col)
    }
}

/// A region of a frame whose order is not modeled; `events` are query-local
/// ids, absent when the region's events were spliced out or are not shown.
#[derive(Debug, Clone, Serialize)]
pub struct SeqGap {
    pub kind: String,
    pub loc: Option<SeqLoc>,
    pub events: Option<(usize, usize)>,
}

/// One function graph shown in the result; a function can appear in several
/// frames (variants, or one per calling event).
#[derive(Debug, Clone, Serialize)]
pub struct SeqFrame {
    pub id: usize,
    pub fn_id: i64,
    pub name: String,
    pub loc: Option<SeqLoc>,
    pub cfg_id: i64,
    pub variant: i64,
    /// The translation unit the graph was built in.
    pub unit: Option<String>,
    pub coverage: String,
    pub gaps: Vec<SeqGap>,
    /// The event whose call this frame inlines; `None` for a starting frame.
    pub parent_event: Option<usize>,
    pub entry: usize,
    pub exit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeqBlock {
    pub id: usize,
    pub frame: usize,
    pub ordinal: i64,
    pub kind: String,
    pub loc: Option<SeqLoc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeqTarget {
    pub fn_id: i64,
    pub name: String,
    pub resolution: String,
    /// The first frame inlining this target, when one was built.
    pub frame: Option<usize>,
    pub boundary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeqEvent {
    pub id: usize,
    pub db_id: i64,
    pub frame: usize,
    pub block: usize,
    pub kind: String,
    pub callee: String,
    pub loc: SeqLoc,
    pub expansion: Option<SeqLoc>,
    pub targets: Vec<SeqTarget>,
    /// The execution-context kind this event starts (`thread`, `task`, ...).
    pub context_start: Option<String>,
}

/// A block edge; `call` and `return` edges join a calling block to the
/// inlined frame and carry the calling `event`.
#[derive(Debug, Clone, Serialize)]
pub struct SeqEdge {
    pub src: usize,
    pub dst: usize,
    pub kind: String,
    pub event: Option<usize>,
}

/// Where the query stopped following calls, and why.
#[derive(Debug, Clone, Serialize)]
pub struct SeqBoundary {
    pub kind: String,
    pub frame: usize,
    pub event: Option<usize>,
    pub fn_id: Option<i64>,
    pub name: Option<String>,
    pub reason: Option<String>,
}

/// Something a budget cut.
#[derive(Debug, Clone, Serialize)]
pub struct SeqTruncation {
    pub reason: String,
    pub frame: usize,
    pub event: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeqRelation {
    pub kind: Relation,
    pub from: EventRef,
    pub to: EventRef,
    /// When `to` runs inside a callee: the event of `from`'s function whose
    /// call reaches it.
    pub via: Option<i64>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SequenceGraph {
    pub capability: String,
    pub notice: Option<String>,
    pub scope: String,
    pub frames: Vec<SeqFrame>,
    pub blocks: Vec<SeqBlock>,
    pub events: Vec<SeqEvent>,
    /// Immediate sequenced-before pairs between events of one block.
    pub order: Vec<(usize, usize)>,
    pub edges: Vec<SeqEdge>,
    pub boundaries: Vec<SeqBoundary>,
    pub truncated: Vec<SeqTruncation>,
    pub relation: Option<SeqRelation>,
}

// ---------------------------------------------------------------------------
// Loading

struct Target {
    fn_id: i64,
    name: String,
    resolution: String,
    is_defined: bool,
    is_dep: bool,
}

/// What the query shows of one event beyond its graph row.
struct EventFacts {
    callee: String,
    loc: SeqLoc,
    expansion: Option<SeqLoc>,
    targets: Vec<Target>,
    /// `(kind, entry function)` of each context this event starts.
    contexts: Vec<(String, i64)>,
}

/// One `cfg_functions` row with everything under it; block and event
/// indices are the graph's own ids.
struct LoadedCfg {
    cfg_id: i64,
    name: String,
    loc: Option<SeqLoc>,
    unit: Option<String>,
    coverage: String,
    graph: FunctionCfg,
    /// `cfg_events.id` of each event.
    db_ids: Vec<i64>,
    events: Vec<EventFacts>,
    block_locs: Vec<Option<SeqLoc>>,
    gap_locs: Vec<Option<SeqLoc>>,
    /// `(stub function, name)` of each IPC bridge the function's proxy sends.
    ipc: Vec<(i64, String)>,
    // Indexes over the graph.
    event_ids: FxHashMap<i64, usize>,
    succ: Vec<Vec<usize>>,
    pred: Vec<Vec<usize>>,
    next_events: Vec<Vec<usize>>,
}

// What the query reads of one graph beyond `crate::read_cfgs`; each is a
// lookup by its graph or function id. Events are reached through their
// blocks: `cfg_events` has no index on `cfg_id`, and these run once per
// frame.
const LOAD_GRAPH_SQL: &str = "SELECT fn.name, p.path, fn.line_start, \
                    (SELECT text FROM strings WHERE id = cf.coverage_id), u.path \
             FROM cfg_functions cf JOIN functions fn ON fn.id = cf.fn_id \
             JOIN files p ON p.id = fn.file_id LEFT JOIN files u ON u.id = cf.tu_file_id \
             WHERE cf.id = ?1";
const LOAD_CALLEES_SQL: &str = "SELECT es.event_id, cs.callee_text \
             FROM cfg_blocks b JOIN cfg_events e ON e.block_id = b.id \
             JOIN cfg_event_sites es ON es.event_id = e.id \
             JOIN call_sites cs ON cs.id = es.call_site_id \
             WHERE b.cfg_id = ?1 ORDER BY es.event_id, cs.id";
const LOAD_TARGETS_SQL: &str =
    "SELECT DISTINCT es.event_id, ce.callee_fn_id, f.name, ce.resolution, \
                    f.is_defined, f.is_dep \
             FROM cfg_blocks b JOIN cfg_events e ON e.block_id = b.id \
             JOIN cfg_event_sites es ON es.event_id = e.id \
             JOIN call_edges ce ON ce.call_site_id = es.call_site_id \
             JOIN functions f ON f.id = ce.callee_fn_id \
             WHERE b.cfg_id = ?1 ORDER BY es.event_id, ce.callee_fn_id, ce.resolution";
const LOAD_CONTEXTS_SQL: &str = "SELECT DISTINCT es.event_id, x.kind, x.entry_fn_id \
             FROM cfg_blocks b JOIN cfg_events e ON e.block_id = b.id \
             JOIN cfg_event_sites es ON es.event_id = e.id \
             JOIN execution_contexts x ON x.call_site_id = es.call_site_id \
             WHERE b.cfg_id = ?1 ORDER BY es.event_id, x.entry_fn_id, x.kind";
const LOAD_IPC_SQL: &str = "SELECT DISTINCT ce.callee_fn_id, f.name FROM call_edges ce \
                 JOIN functions f ON f.id = ce.callee_fn_id \
                 WHERE ce.caller_fn_id = ?1 AND ce.resolution = 'ipc' ORDER BY ce.callee_fn_id";

/// Graphs loaded so far, by `cfg_functions.id`.
struct Loader<'c> {
    conn: &'c Connection,
    cfgs: Vec<LoadedCfg>,
    by_id: FxHashMap<i64, usize>,
    by_fn: FxHashMap<i64, Rc<[i64]>>,
    paths: FxHashMap<FileId, Option<String>>,
}

impl<'c> Loader<'c> {
    fn new(conn: &'c Connection) -> Self {
        Self {
            conn,
            cfgs: Vec::new(),
            by_id: FxHashMap::default(),
            by_fn: FxHashMap::default(),
            paths: FxHashMap::default(),
        }
    }

    /// Graph ids of every variant of `function`, in variant order.
    fn cfgs_of(&mut self, function: i64) -> Result<Rc<[i64]>> {
        if let Some(ids) = self.by_fn.get(&function) {
            return Ok(Rc::clone(ids));
        }
        let ids: Rc<[i64]> = self
            .conn
            .prepare_cached("SELECT id FROM cfg_functions WHERE fn_id = ?1 ORDER BY variant, id")?
            .query_map([function], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        self.by_fn.insert(function, Rc::clone(&ids));
        Ok(ids)
    }

    /// The location of `span`, `None` without one or when its file is gone.
    fn loc(&mut self, span: Option<Span>) -> Result<Option<SeqLoc>> {
        let Some(span) = span else {
            return Ok(None);
        };
        let path = match self.paths.get(&span.file) {
            Some(path) => path.clone(),
            None => {
                let path: Option<String> = self
                    .conn
                    .prepare_cached("SELECT path FROM files WHERE id = ?1")?
                    .query_row([span.file.0], |r| r.get(0))
                    .optional()?;
                self.paths.insert(span.file, path.clone());
                path
            }
        };
        Ok(path.map(|path| SeqLoc {
            path,
            line: i64::from(span.line),
            col: i64::from(span.col),
        }))
    }

    fn load(&mut self, cfg_id: i64) -> Result<usize> {
        if let Some(&i) = self.by_id.get(&cfg_id) {
            return Ok(i);
        }
        let conn = self.conn;
        let (name, fn_loc, coverage, unit) = conn.query_row(LOAD_GRAPH_SQL, [cfg_id], |r| {
            let line: i64 = r.get(2)?;
            Ok((
                r.get::<_, String>(0)?,
                super::has_source_location(line)
                    .then(|| {
                        Some(SeqLoc {
                            path: r.get(1).ok()?,
                            line,
                            col: 0,
                        })
                    })
                    .flatten(),
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        let crate::StoredCfg {
            cfg: graph,
            event_ids,
            ..
        } = crate::read_cfgs(conn, cfg_id..=cfg_id)?
            .pop()
            .with_context(|| format!("no call-order graph {cfg_id}"))?;
        let by_db: FxHashMap<i64, usize> = event_ids
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, i))
            .collect();

        let mut events = Vec::with_capacity(graph.events.len());
        for e in &graph.events {
            let occurrence = e.occurrence;
            events.push(EventFacts {
                callee: String::new(),
                loc: self
                    .loc(Some(occurrence.span))?
                    .context("cfg_events.file_id names no file")?,
                expansion: self.loc(occurrence.expansion_span)?,
                targets: Vec::new(),
                contexts: Vec::new(),
            });
        }
        let block_locs = graph
            .blocks
            .iter()
            .map(|b| self.loc(b.span))
            .collect::<Result<_>>()?;
        let gap_locs = graph
            .gaps
            .iter()
            .map(|g| self.loc(g.span))
            .collect::<Result<_>>()?;
        // The first spelling, in site order.
        let mut stmt = conn.prepare_cached(LOAD_CALLEES_SQL)?;
        let mut rows = stmt.query([cfg_id])?;
        let mut last = None;
        while let Some(r) = rows.next()? {
            let event: i64 = r.get(0)?;
            if last != Some(event) {
                last = Some(event);
                events[by_db[&event]].callee = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            }
        }
        // Each target once per event, in function id order.
        let mut stmt = conn.prepare_cached(LOAD_TARGETS_SQL)?;
        let mut rows = stmt.query([cfg_id])?;
        while let Some(r) = rows.next()? {
            let targets = &mut events[by_db[&r.get::<_, i64>(0)?]].targets;
            let fn_id: i64 = r.get(1)?;
            if targets.last().is_some_and(|t| t.fn_id == fn_id) {
                continue;
            }
            targets.push(Target {
                fn_id,
                name: r.get(2)?,
                resolution: r.get(3)?,
                is_defined: r.get::<_, i64>(4)? != 0,
                is_dep: r.get::<_, i64>(5)? != 0,
            });
        }
        let mut stmt = conn.prepare_cached(LOAD_CONTEXTS_SQL)?;
        let mut rows = stmt.query([cfg_id])?;
        while let Some(r) = rows.next()? {
            events[by_db[&r.get::<_, i64>(0)?]]
                .contexts
                .push((r.get(1)?, r.get(2)?));
        }
        // IPC bridges are synthetic edges without a call site: they belong to
        // the proxy function, not to one of its events.
        let ipc = conn
            .prepare_cached(LOAD_IPC_SQL)?
            .query_map([graph.function.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut succ = vec![Vec::new(); graph.blocks.len()];
        let mut pred = vec![Vec::new(); graph.blocks.len()];
        for edge in &graph.edges {
            succ[edge.from.0 as usize].push(edge.to.0 as usize);
            pred[edge.to.0 as usize].push(edge.from.0 as usize);
        }
        let mut next_events = vec![Vec::new(); graph.events.len()];
        for &(a, b) in &graph.order {
            next_events[a.0 as usize].push(b.0 as usize);
        }
        let index = self.cfgs.len();
        self.cfgs.push(LoadedCfg {
            cfg_id,
            name,
            loc: fn_loc,
            unit,
            coverage,
            graph,
            db_ids: event_ids,
            events,
            block_locs,
            gap_locs,
            ipc,
            event_ids: by_db,
            succ,
            pred,
            next_events,
        });
        self.by_id.insert(cfg_id, index);
        Ok(index)
    }
}

// ---------------------------------------------------------------------------
// Graph facts of one loaded function

/// Nodes reached from `from` over one or more steps of `next`, marked in
/// `seen`; stops as soon as one satisfies `stop`, and says whether one did.
/// Each node is expanded once, so a DAG of diamonds stays linear.
fn search<'a>(
    from: usize,
    seen: &mut [bool],
    next: impl Fn(usize) -> &'a [usize],
    stop: impl Fn(usize) -> bool,
) -> bool {
    let mut stack = vec![from];
    while let Some(x) = stack.pop() {
        for &y in next(x) {
            if stop(y) {
                return true;
            }
            if !seen[y] {
                seen[y] = true;
                stack.push(y);
            }
        }
    }
    false
}

/// The blocks `entry` reaches over `succ`, in postorder of a depth-first
/// walk taking each block's successors in order, and which were reached.
fn postorder(entry: usize, succ: &[Vec<usize>]) -> (Vec<usize>, Vec<bool>) {
    let mut post = Vec::with_capacity(succ.len());
    let mut visited = vec![false; succ.len()];
    let mut stack = vec![(entry, 0usize)];
    visited[entry] = true;
    while let Some((b, i)) = stack.pop() {
        if i < succ[b].len() {
            stack.push((b, i + 1));
            let next = succ[b][i];
            if !visited[next] {
                visited[next] = true;
                stack.push((next, 0));
            }
        } else {
            post.push(b);
        }
    }
    (post, visited)
}

/// Whether `a` dominates `b` under the immediate dominators `idom`.
fn dominates(idom: &[Option<usize>], a: usize, b: usize) -> bool {
    let mut at = b;
    loop {
        if at == a {
            return true;
        }
        match idom[at] {
            Some(up) if up != at => at = up,
            _ => return false,
        }
    }
}

impl LoadedCfg {
    fn fn_id(&self) -> i64 {
        i64::from(self.graph.function.0)
    }

    fn entry(&self) -> usize {
        self.graph.entry.0 as usize
    }

    fn exit(&self) -> usize {
        self.graph.exit.0 as usize
    }

    /// The block of event `e`.
    fn block_of(&self, e: usize) -> usize {
        self.graph.events[e].block.0 as usize
    }

    /// Events in block order, each block's in evaluation order.
    fn events_in_order(&self) -> impl Iterator<Item = usize> + '_ {
        self.graph
            .blocks
            .iter()
            .flat_map(|b| b.events.iter().map(|e| e.0 as usize))
    }

    fn event_by_db(&self, db_id: i64) -> Option<usize> {
        self.event_ids.get(&db_id).copied()
    }

    /// The local index of event `db_id`, which must belong to this graph.
    fn event_of(&self, db_id: i64) -> Result<usize> {
        match self.event_by_db(db_id) {
            Some(e) => Ok(e),
            None => bail!("event {db_id} is not in call-order graph {}", self.cfg_id),
        }
    }

    /// Blocks reachable from `from` over one or more edges; `forward = false`
    /// follows edges backwards.
    fn reach(&self, from: usize, forward: bool) -> Vec<bool> {
        let mut seen = vec![false; self.graph.blocks.len()];
        let adjacent = if forward { &self.succ } else { &self.pred };
        search(from, &mut seen, |b| &adjacent[b], |_| false);
        seen
    }

    /// Blocks in reverse postorder from the entry over every edge, so a
    /// branch's arms come before their join; blocks the entry does not reach
    /// follow in ordinal order.
    fn layout(&self) -> Vec<usize> {
        let mut succ = self.succ.clone();
        // Visiting the higher ordinal first puts the lower one first in the
        // reversed order.
        for s in &mut succ {
            s.sort_unstable_by(|a, b| b.cmp(a));
        }
        let (mut order, visited) = postorder(self.entry(), &succ);
        order.reverse();
        order.extend((0..succ.len()).filter(|&b| !visited[b]));
        order
    }

    /// Immediate dominators over non-exception edges from the entry
    /// (Cooper, Harvey and Kennedy); `None` for blocks the entry does not
    /// reach that way.
    fn dominators(&self) -> Vec<Option<usize>> {
        let n = self.graph.blocks.len();
        let mut succ = vec![Vec::new(); n];
        let mut pred = vec![Vec::new(); n];
        for edge in &self.graph.edges {
            if edge.kind != EdgeKind::Exception {
                succ[edge.from.0 as usize].push(edge.to.0 as usize);
                pred[edge.to.0 as usize].push(edge.from.0 as usize);
            }
        }
        let entry = self.entry();
        let (mut rpo, _) = postorder(entry, &succ);
        // Postorder numbers.
        let mut post = vec![usize::MAX; n];
        for (i, &b) in rpo.iter().enumerate() {
            post[b] = i;
        }
        rpo.reverse();
        let mut idom: Vec<Option<usize>> = vec![None; n];
        idom[entry] = Some(entry);
        let intersect = |idom: &[Option<usize>], mut a: usize, mut b: usize| {
            while a != b {
                while post[a] < post[b] {
                    a = idom[a].unwrap();
                }
                while post[b] < post[a] {
                    b = idom[b].unwrap();
                }
            }
            a
        };
        let mut changed = true;
        while changed {
            changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new = None;
                for &p in &pred[b] {
                    if idom[p].is_some() {
                        new = Some(match new {
                            None => p,
                            Some(q) => intersect(&idom, p, q),
                        });
                    }
                }
                if new.is_some() && idom[b] != new {
                    idom[b] = new;
                    changed = true;
                }
            }
        }
        idom
    }

    /// `b` follows `a` in their block's local order.
    fn order_reaches(&self, a: usize, b: usize) -> bool {
        let mut seen = vec![false; self.graph.events.len()];
        search(a, &mut seen, |e| &self.next_events[e], |y| y == b)
    }

    /// Kinds of the gaps whose event range holds `event`.
    fn gaps_over(&self, event: usize) -> Vec<GapKind> {
        self.graph
            .gaps
            .iter()
            .filter(|g| {
                g.events
                    .is_some_and(|(a, b)| a.0 as usize <= event && event <= b.0 as usize)
            })
            .map(|g| g.kind)
            .collect()
    }

    /// How event `a`'s invocation relates to event `b`'s, both of this
    /// graph; `forward` are the blocks `a`'s reaches ([`Self::reach`], so
    /// `forward[block_of(a)]` holds when that block is on a cycle) and
    /// `idom` the graph's [`Self::dominators`].
    fn relate(
        &self,
        a: usize,
        b: usize,
        forward: &[bool],
        idom: &[Option<usize>],
    ) -> (Relation, Vec<String>) {
        let (ba, bb) = (self.block_of(a), self.block_of(b));
        let relation = if a == b {
            if forward[ba] {
                Relation::MayBoth
            } else {
                return (
                    Relation::Unknown,
                    vec!["both positions name the same invocation".into()],
                );
            }
        } else if ba == bb {
            if forward[ba] {
                // The block is on a cycle: a later iteration's `a` follows
                // this one's `b` whatever the block's local order.
                Relation::MayBoth
            } else if self.order_reaches(a, b) {
                Relation::SequencedBefore
            } else if self.order_reaches(b, a) {
                Relation::MayFollow
            } else {
                Relation::Unordered
            }
        } else {
            match (forward[bb], self.reach(bb, true)[ba]) {
                (true, true) => Relation::MayBoth,
                (true, false) if dominates(idom, ba, bb) => Relation::GuaranteedBefore,
                (true, false) => Relation::MayPrecede,
                (false, true) => Relation::MayFollow,
                (false, false) => Relation::Exclusive,
            }
        };
        if matches!(
            relation,
            Relation::SequencedBefore | Relation::GuaranteedBefore
        ) {
            let (ga, gb) = (self.gaps_over(a), self.gaps_over(b));
            let unordered = GapKind::UnorderedBranch;
            if ga.contains(&unordered) && gb.contains(&unordered) {
                return (
                    Relation::Unordered,
                    vec!["both lie in an unordered_branch gap".into()],
                );
            }
            if let Some(kind) = ga.iter().chain(&gb).next() {
                return (
                    Relation::Unknown,
                    vec![format!(
                        "a {} gap covers an end; its order is not modeled",
                        kind.as_str()
                    )],
                );
            }
        }
        (relation, Vec::new())
    }
}

/// The notes stating what a relation of this kind claims.
fn conditions(relation: Relation) -> Vec<String> {
    match relation {
        Relation::SequencedBefore | Relation::GuaranteedBefore => vec![
            "given the second event executes".into(),
            "and every call between them completes normally (no throw, longjmp, exit or \
             noreturn call)"
                .into(),
        ],
        Relation::MayPrecede | Relation::MayFollow | Relation::MayBoth => {
            vec!["a control-flow path exists; its feasibility is not checked".into()]
        }
        Relation::Exclusive => vec!["no control-flow path joins them either way".into()],
        Relation::Unordered => vec!["the language leaves their order unspecified".into()],
        Relation::Unknown => Vec::new(),
    }
}

/// One relation for several calling events: the weakest claim all allow.
fn combine(relations: &[Relation]) -> Relation {
    use Relation::*;
    if relations.windows(2).all(|w| w[0] == w[1]) {
        return relations[0];
    }
    if relations.contains(&Unknown) {
        return Unknown;
    }
    let precede = relations
        .iter()
        .any(|r| matches!(r, SequencedBefore | GuaranteedBefore | MayPrecede | MayBoth));
    let follow = relations.iter().any(|r| matches!(r, MayFollow | MayBoth));
    match (precede, follow) {
        (true, true) => MayBoth,
        (true, false) => MayPrecede,
        (false, true) => MayFollow,
        _ if relations.contains(&Unordered) => Unordered,
        _ => Exclusive,
    }
}

// ---------------------------------------------------------------------------
// The query

/// A target is run by an execution context the event starts, not by the call.
fn starts_context(event: &EventFacts, target: i64) -> bool {
    event.contexts.iter().any(|&(_, entry)| entry == target)
}

/// Why a function has no graph to inline.
fn omitted_reason(target: &Target) -> &'static str {
    if target.is_dep {
        "dependency"
    } else if !target.is_defined {
        "declaration_only"
    } else {
        "not_sequenced"
    }
}

struct Builder<'c, 'o> {
    loader: Loader<'c>,
    options: &'o SequenceOptions,
    graph: SequenceGraph,
    /// Per frame of `graph`, what it was built from.
    frames: Vec<FrameState>,
}

struct FrameState {
    /// The loaded graph.
    cfg: usize,
    /// Query-local event ids by the graph's event index.
    events: FxHashMap<usize, usize>,
    /// Call levels below the starting frames.
    depth: u32,
}

impl Builder<'_, '_> {
    /// Show loaded graph `cfg` as a new frame, restricted to `keep` blocks
    /// (its entry and exit are always shown).
    fn add_frame(
        &mut self,
        cfg: usize,
        parent_event: Option<usize>,
        depth: u32,
        keep: Option<&[bool]>,
    ) -> usize {
        let g = &self.loader.cfgs[cfg];
        let frame = self.graph.frames.len();
        let mut blocks = FxHashMap::default();
        let mut events = FxHashMap::default();
        let layout = g.layout();
        let (entry, exit) = (g.entry(), g.exit());
        for &i in &layout {
            if !(keep.is_none_or(|k| k[i]) || i == entry || i == exit) {
                continue;
            }
            let id = self.graph.blocks.len();
            blocks.insert(i, id);
            self.graph.blocks.push(SeqBlock {
                id,
                frame,
                ordinal: i as i64,
                kind: g.graph.blocks[i].kind.as_str().into(),
                loc: g.block_locs[i].clone(),
            });
        }
        for &i in &layout {
            let Some(&block) = blocks.get(&i) else {
                continue;
            };
            for e in &g.graph.blocks[i].events {
                let e = e.0 as usize;
                let ev = &g.events[e];
                let id = self.graph.events.len();
                events.insert(e, id);
                self.graph.events.push(SeqEvent {
                    id,
                    db_id: g.db_ids[e],
                    frame,
                    block,
                    kind: g.graph.events[e].kind.as_str().into(),
                    callee: ev.callee.clone(),
                    loc: ev.loc.clone(),
                    expansion: ev.expansion.clone(),
                    targets: Vec::new(),
                    context_start: ev.contexts.first().map(|(kind, _)| kind.clone()),
                });
            }
        }
        for edge in &g.graph.edges {
            let (src, dst) = (edge.from.0 as usize, edge.to.0 as usize);
            if let (Some(&src), Some(&dst)) = (blocks.get(&src), blocks.get(&dst)) {
                self.graph.edges.push(SeqEdge {
                    src,
                    dst,
                    kind: edge.kind.as_str().into(),
                    event: None,
                });
            }
        }
        let local = |e: trace_ir::EventId| events.get(&(e.0 as usize)).copied();
        for &(a, b) in &g.graph.order {
            if let (Some(a), Some(b)) = (local(a), local(b)) {
                self.graph.order.push((a, b));
            }
        }
        let gaps = g
            .graph
            .gaps
            .iter()
            .zip(&g.gap_locs)
            .map(|(gap, loc)| SeqGap {
                kind: gap.kind.as_str().into(),
                loc: loc.clone(),
                events: gap.events.and_then(|(a, b)| Some((local(a)?, local(b)?))),
            })
            .collect();
        for (stub, name) in &g.ipc {
            self.graph.boundaries.push(SeqBoundary {
                kind: "ipc_bridge".into(),
                frame,
                event: None,
                fn_id: Some(*stub),
                name: Some(name.clone()),
                reason: None,
            });
        }
        self.graph.frames.push(SeqFrame {
            id: frame,
            fn_id: g.fn_id(),
            name: g.name.clone(),
            loc: g.loc.clone(),
            cfg_id: g.cfg_id,
            variant: i64::from(g.graph.variant),
            unit: g.unit.clone(),
            coverage: g.coverage.clone(),
            gaps,
            parent_event,
            entry: blocks[&entry],
            exit: blocks[&exit],
        });
        self.frames.push(FrameState { cfg, events, depth });
        frame
    }

    /// Functions of `frame` and the frames inlining it.
    fn on_stack(&self, mut frame: usize, function: i64) -> bool {
        loop {
            if self.graph.frames[frame].fn_id == function {
                return true;
            }
            match self.graph.frames[frame].parent_event {
                Some(e) => frame = self.graph.events[e].frame,
                None => return false,
            }
        }
    }

    fn boundary(
        &mut self,
        frame: usize,
        event: usize,
        kind: &str,
        target: Option<&Target>,
        reason: Option<&str>,
    ) {
        self.graph.boundaries.push(SeqBoundary {
            kind: kind.into(),
            frame,
            event: Some(event),
            fn_id: target.map(|t| t.fn_id),
            name: target.map(|t| t.name.clone()),
            reason: reason.map(Into::into),
        });
    }

    fn truncate(&mut self, frame: usize, event: usize, reason: &str) {
        self.graph.truncated.push(SeqTruncation {
            reason: reason.into(),
            frame,
            event: Some(event),
        });
    }

    /// Inline callees frame by frame, breadth first, in event order.
    fn expand(&mut self) -> Result<()> {
        let mut queue: VecDeque<usize> = (0..self.graph.frames.len()).collect();
        while let Some(frame) = queue.pop_front() {
            let FrameState { cfg, depth, .. } = self.frames[frame];
            let mut shown: Vec<(usize, usize)> = self.frames[frame]
                .events
                .iter()
                .map(|(&local, &id)| (id, local))
                .collect();
            shown.sort_unstable();
            for (event, local) in shown {
                let targets = std::mem::take(&mut self.loader.cfgs[cfg].events[local].targets);
                let result = self.expand_event(frame, event, local, depth, &targets, &mut queue);
                self.loader.cfgs[cfg].events[local].targets = targets;
                result?;
            }
        }
        Ok(())
    }

    fn expand_event(
        &mut self,
        frame: usize,
        event: usize,
        local: usize,
        depth: u32,
        targets: &[Target],
        queue: &mut VecDeque<usize>,
    ) -> Result<()> {
        if targets.is_empty() {
            self.boundary(frame, event, "unknown_callee", None, None);
            return Ok(());
        }
        let cfg = self.frames[frame].cfg;
        for (i, target) in targets.iter().enumerate() {
            if i == self.options.max_targets {
                self.truncate(frame, event, "max_targets");
                break;
            }
            let mut shown = SeqTarget {
                fn_id: target.fn_id,
                name: target.name.clone(),
                resolution: target.resolution.clone(),
                frame: None,
                boundary: None,
            };
            let starts = starts_context(&self.loader.cfgs[cfg].events[local], target.fn_id);
            let inline = if target.resolution == "ipc" {
                shown.boundary = Some("ipc_bridge".into());
                false
            } else if starts {
                shown.boundary = Some("async_start".into());
                self.options.inline_contexts
            } else if self.on_stack(frame, target.fn_id) {
                shown.boundary = Some("recursion".into());
                false
            } else {
                true
            };
            if let Some(kind) = shown.boundary.clone() {
                self.boundary(frame, event, &kind, Some(target), None);
            }
            if inline {
                let variants = self.loader.cfgs_of(target.fn_id)?;
                if variants.is_empty() {
                    let reason = omitted_reason(target);
                    shown.boundary = Some("omitted_body".into());
                    self.boundary(frame, event, "omitted_body", Some(target), Some(reason));
                } else if depth < self.options.depth {
                    for &variant in variants.iter() {
                        let child = self.loader.load(variant)?;
                        if self.graph.frames.len() >= self.options.max_frames {
                            self.truncate(frame, event, "max_frames");
                            break;
                        }
                        let events = self.loader.cfgs[child].events.len();
                        if self.graph.events.len() + events > self.options.max_events {
                            self.truncate(frame, event, "max_events");
                            break;
                        }
                        let block = self.graph.events[event].block;
                        let new = self.add_frame(child, Some(event), depth + 1, None);
                        let (entry, exit) =
                            (self.graph.frames[new].entry, self.graph.frames[new].exit);
                        self.graph.edges.push(SeqEdge {
                            src: block,
                            dst: entry,
                            kind: "call".into(),
                            event: Some(event),
                        });
                        self.graph.edges.push(SeqEdge {
                            src: exit,
                            dst: block,
                            kind: "return".into(),
                            event: Some(event),
                        });
                        shown.frame.get_or_insert(new);
                        queue.push_back(new);
                    }
                }
            }
            self.graph.events[event].targets.push(shown);
        }
        Ok(())
    }
}

/// The events of `from`'s graph whose calls reach one of the graphs `to`
/// within `depth` call levels, skipping context starts and IPC bridges:
/// those run elsewhere and order nothing.
fn calls_reaching(loader: &mut Loader, from: usize, to: &[i64], depth: u32) -> Result<Vec<usize>> {
    let mut memo = FxHashMap::default();
    let mut found = Vec::new();
    let events: Vec<usize> = loader.cfgs[from].events_in_order().collect();
    for e in events {
        if event_reaches(loader, from, e, to, depth, &mut memo)? {
            found.push(e);
        }
    }
    Ok(found)
}

/// Whether a call of `event` reaches one of the graphs `to` within `depth`
/// levels; `memo` holds the answer per `(graph, depth)`.
fn event_reaches(
    loader: &mut Loader,
    cfg: usize,
    event: usize,
    to: &[i64],
    depth: u32,
    memo: &mut FxHashMap<(i64, u32), bool>,
) -> Result<bool> {
    if depth == 0 {
        return Ok(false);
    }
    let ev = &loader.cfgs[cfg].events[event];
    let callees: Vec<i64> = ev
        .targets
        .iter()
        .filter(|t| t.resolution != "ipc" && !starts_context(ev, t.fn_id))
        .map(|t| t.fn_id)
        .collect();
    for callee in callees {
        for &variant in loader.cfgs_of(callee)?.iter() {
            if to.contains(&variant) {
                return Ok(true);
            }
            if depth == 1 {
                continue;
            }
            if let Some(&hit) = memo.get(&(variant, depth - 1)) {
                if hit {
                    return Ok(true);
                }
                continue;
            }
            memo.insert((variant, depth - 1), false);
            let child = loader.load(variant)?;
            for e in 0..loader.cfgs[child].events.len() {
                if event_reaches(loader, child, e, to, depth - 1, memo)? {
                    memo.insert((variant, depth - 1), true);
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Build the bounded sequence of `scope`.
pub fn sequence_graph(
    conn: &Connection,
    scope: &SequenceScope,
    options: &SequenceOptions,
) -> Result<SequenceGraph> {
    let marker = require_sequencing(conn)?;
    let mut b = Builder {
        loader: Loader::new(conn),
        options,
        graph: SequenceGraph {
            capability: marker.as_str().into(),
            notice: marker.notice().map(Into::into),
            ..Default::default()
        },
        frames: Vec::new(),
    };
    match scope {
        SequenceScope::Function { function, file } => {
            let mut variants = b.loader.cfgs_of(*function)?;
            if let Some(file) = file {
                let narrowed = variants_in(conn, *function, file)?;
                if !narrowed.is_empty() {
                    variants = narrowed.into();
                }
            }
            let name: Option<(String, i64, i64)> = conn
                .query_row(
                    "SELECT name, is_defined, is_dep FROM functions WHERE id = ?1",
                    [function],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((name, defined, dep)) = name else {
                bail!("no function with id {function}");
            };
            if variants.is_empty() {
                let target = Target {
                    fn_id: *function,
                    name: name.clone(),
                    resolution: String::new(),
                    is_defined: defined != 0,
                    is_dep: dep != 0,
                };
                bail!(
                    "{name} has no call-order graph ({})",
                    omitted_reason(&target)
                );
            }
            b.graph.scope = match file {
                Some(file) => format!("function {name} in {file}"),
                None => format!("function {name}"),
            };
            for &variant in variants.iter() {
                let cfg = b.loader.load(variant)?;
                b.add_frame(cfg, None, 0, None);
            }
        }
        SequenceScope::From(start) => {
            b.graph.scope = format!("from {}", start.display());
            // One frame per variant compiling the call, as for a function.
            for (event, cfg_id) in start.copies() {
                let cfg = b.loader.load(cfg_id)?;
                let g = &b.loader.cfgs[cfg];
                let at = g.block_of(g.event_of(event)?);
                let mut keep = g.reach(at, true);
                keep[at] = true;
                b.add_frame(cfg, None, 0, Some(&keep));
            }
        }
        SequenceScope::Between(from, to) => {
            let to_copies = to.copies();
            let to_cfgs: Vec<i64> = to_copies.iter().map(|&(_, cfg)| cfg).collect();
            let from_copies = from.copies();
            // Per variant of `from`'s function: its relations and notes,
            // and the frame showing the blocks between the ends.
            let mut relations = Vec::new();
            let mut notes: Vec<String> = Vec::new();
            let mut via: Option<(i64, String)> = None;
            let mut unreached = 0;
            let mut frames = Vec::new();
            for &(event, cfg_id) in &from_copies {
                let cfg = b.loader.load(cfg_id)?;
                let a = b.loader.cfgs[cfg].event_of(event)?;
                let (targets, inside) = match to_copies.iter().find(|&&(_, c)| c == cfg_id) {
                    Some(&(t, _)) => (vec![b.loader.cfgs[cfg].event_of(t)?], false),
                    None => (
                        calls_reaching(&mut b.loader, cfg, &to_cfgs, options.depth)?,
                        true,
                    ),
                };
                let g = &b.loader.cfgs[cfg];
                let at = g.block_of(a);
                let forward = g.reach(at, true);
                let mut keep = vec![false; g.graph.blocks.len()];
                keep[at] = true;
                for &t in &targets {
                    let tb = g.block_of(t);
                    let backward = g.reach(tb, false);
                    for (i, k) in keep.iter_mut().enumerate() {
                        *k |= forward[i] && backward[i];
                    }
                    keep[tb] = true;
                }
                frames.push((cfg, keep));
                if targets.is_empty() {
                    unreached += 1;
                    continue;
                }
                // Another variant's notes are not repeated.
                let earlier = notes.len();
                let idom = g.dominators();
                for &t in &targets {
                    let (r, n) = if inside && t == a {
                        // `to` runs inside the call `from` makes.
                        (Relation::GuaranteedBefore, Vec::new())
                    } else {
                        g.relate(a, t, &forward, &idom)
                    };
                    relations.push(r);
                    for n in n {
                        if !notes[..earlier].contains(&n) {
                            notes.push(n);
                        }
                    }
                }
                if inside && via.is_none() {
                    via = Some((g.db_ids[targets[0]], g.events[targets[0]].loc.display()));
                }
            }
            let not_reached = format!(
                "{} is not reached from {}'s function within depth {} by calls \
                 that run in the same execution context",
                to.display(),
                from.display(),
                options.depth
            );
            let relation = if relations.is_empty() {
                SeqRelation {
                    kind: Relation::Unknown,
                    from: from.clone(),
                    to: to.clone(),
                    via: None,
                    notes: vec![not_reached],
                }
            } else {
                let kind = combine(&relations);
                notes.extend(conditions(kind));
                if let Some((_, at)) = &via {
                    notes.push(format!("{} runs inside the call at {at}", to.display()));
                }
                if unreached > 0 {
                    notes.push(format!(
                        "in {unreached} of {} variants, {not_reached}",
                        from_copies.len()
                    ));
                }
                SeqRelation {
                    kind,
                    from: from.clone(),
                    to: to.clone(),
                    via: via.map(|(event, _)| event),
                    notes,
                }
            };
            b.graph.scope = format!("between {} and {}", from.display(), to.display());
            b.graph.relation = Some(relation);
            for (cfg, keep) in frames {
                b.add_frame(cfg, None, 0, Some(&keep));
            }
        }
    }
    b.expand()?;
    Ok(b.graph)
}

// ---------------------------------------------------------------------------
// Rendering

/// Render a sequence as text, JSON, Mermaid or Graphviz.
pub fn render_sequence(graph: &SequenceGraph, format: crate::RenderFormat) -> Result<String> {
    Ok(match format {
        crate::RenderFormat::Json => {
            let mut out = serde_json::to_string_pretty(graph)?;
            out.push('\n');
            out
        }
        crate::RenderFormat::Text => render_text(graph),
        crate::RenderFormat::Mermaid => render_flowchart(graph, false),
        crate::RenderFormat::Graphviz => render_flowchart(graph, true),
    })
}

/// A graph's rows grouped by what renders them, so each view reads every
/// row a bounded number of times.
struct RenderIndex<'g> {
    blocks_of_frame: Vec<Vec<&'g SeqBlock>>,
    /// Each block's events, in event order.
    events_of_block: Vec<Vec<usize>>,
    /// Each event's immediate predecessors and successors, in `order`'s order.
    before: Vec<Vec<usize>>,
    after: Vec<Vec<usize>>,
    /// Each block's non-call edges.
    edges_from: Vec<Vec<&'g SeqEdge>>,
    boundaries_of_event: Vec<Vec<&'g SeqBoundary>>,
    /// Each frame's boundaries not tied to an event.
    boundaries_of_frame: Vec<Vec<&'g SeqBoundary>>,
    truncated_of_event: Vec<Vec<&'g SeqTruncation>>,
}

impl<'g> RenderIndex<'g> {
    fn new(graph: &'g SequenceGraph) -> Self {
        let mut index = Self {
            blocks_of_frame: vec![Vec::new(); graph.frames.len()],
            events_of_block: vec![Vec::new(); graph.blocks.len()],
            before: vec![Vec::new(); graph.events.len()],
            after: vec![Vec::new(); graph.events.len()],
            edges_from: vec![Vec::new(); graph.blocks.len()],
            boundaries_of_event: vec![Vec::new(); graph.events.len()],
            boundaries_of_frame: vec![Vec::new(); graph.frames.len()],
            truncated_of_event: vec![Vec::new(); graph.events.len()],
        };
        for b in &graph.blocks {
            index.blocks_of_frame[b.frame].push(b);
        }
        for e in &graph.events {
            index.events_of_block[e.block].push(e.id);
        }
        for &(p, q) in &graph.order {
            index.after[p].push(q);
            index.before[q].push(p);
        }
        for e in graph.edges.iter().filter(|e| e.event.is_none()) {
            index.edges_from[e.src].push(e);
        }
        for b in &graph.boundaries {
            match b.event {
                Some(e) => index.boundaries_of_event[e].push(b),
                None => index.boundaries_of_frame[b.frame].push(b),
            }
        }
        for t in &graph.truncated {
            if let Some(e) = t.event {
                index.truncated_of_event[e].push(t);
            }
        }
        index
    }

    /// A block's events in evaluation order, each maximal run of mutually
    /// unordered neighbours as one group.
    fn event_groups(&self, block: usize) -> Vec<Vec<usize>> {
        let events = &self.events_of_block[block];
        let position: FxHashMap<usize, usize> =
            events.iter().enumerate().map(|(i, &e)| (e, i)).collect();
        let next: Vec<Vec<usize>> = events
            .iter()
            .map(|&e| {
                self.after[e]
                    .iter()
                    .filter_map(|q| position.get(q).copied())
                    .collect()
            })
            .collect();
        let mut groups = unordered_runs(&next);
        for group in &mut groups {
            for e in group.iter_mut() {
                *e = events[*e];
            }
        }
        groups
    }
}

/// Positions `0..next.len()` in order, each maximal run of neighbours no
/// order path (`next`, immediate successors) joins as one group.
fn unordered_runs(next: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    // The latest immediate predecessor of each position, when every pair
    // runs forward (as an evaluation order lists them).
    let mut latest: Vec<Option<usize>> = vec![None; next.len()];
    let mut forward = true;
    for (p, n) in next.iter().enumerate() {
        for &q in n {
            forward &= p < q;
            latest[q] = latest[q].max(Some(p));
        }
    }
    if forward {
        // A path from a member of the last group (contiguous, mutually
        // unordered) to the next position would pass only through members,
        // so it is one pair; and no path runs back. The next position
        // joins unless a predecessor is a member.
        let mut start = 0;
        for (e, latest) in latest.into_iter().enumerate() {
            if e > 0 && latest.is_none_or(|p| p < start) {
                groups.last_mut().unwrap().push(e);
            } else {
                start = e;
                groups.push(vec![e]);
            }
        }
        return groups;
    }
    let reaches = |a: usize, b: usize| {
        let mut seen = vec![false; next.len()];
        search(a, &mut seen, |x| &next[x], |y| y == b)
    };
    for e in 0..next.len() {
        let joins = groups
            .last()
            .is_some_and(|g| g.iter().all(|&x| !reaches(x, e) && !reaches(e, x)));
        match groups.last_mut() {
            Some(g) if joins => g.push(e),
            _ => groups.push(vec![e]),
        }
    }
    groups
}

fn boundary_text(b: &SeqBoundary) -> String {
    let name = b.name.as_deref().unwrap_or("?");
    match b.kind.as_str() {
        "unknown_callee" => "?? unknown callee".into(),
        "omitted_body" => format!(
            "⊘ body omitted ({}): {name}",
            b.reason.as_deref().unwrap_or("?")
        ),
        "recursion" => format!("↺ recursion: {name}"),
        "async_start" => format!("⇢ async start: {name} runs in another context"),
        "ipc_bridge" => format!("⇢ ipc bridge: {name} runs in another process"),
        other => format!("{other}: {name}"),
    }
}

/// One event; `explicit` lists its immediate predecessors, for a block whose
/// layout alone does not show its order.
fn event_line(index: &RenderIndex, e: &SeqEvent, explicit: bool) -> String {
    let mut line = format!("e{} {}() {}", e.id, e.callee, e.loc.display());
    if explicit && !index.before[e.id].is_empty() {
        let after: Vec<String> = index.before[e.id].iter().map(|a| format!("e{a}")).collect();
        write!(line, " after {}", after.join(", ")).unwrap();
    }
    if e.kind != trace_ir::EventKind::Call.as_str() {
        write!(line, " [{}]", e.kind).unwrap();
    }
    if let Some(x) = &e.expansion {
        write!(line, " (expanded at {})", x.display()).unwrap();
    }
    if let Some(kind) = &e.context_start {
        write!(line, " starts {kind}").unwrap();
    }
    for t in &e.targets {
        if let Some(f) = t.frame {
            write!(line, "  → frame {f} ({})", t.name).unwrap();
        }
    }
    for b in &index.boundaries_of_event[e.id] {
        write!(line, "  {}", boundary_text(b)).unwrap();
    }
    for t in &index.truncated_of_event[e.id] {
        write!(line, "  … truncated ({})", t.reason).unwrap();
    }
    line
}

fn render_text(graph: &SequenceGraph) -> String {
    let index = RenderIndex::new(graph);
    let mut out = String::new();
    writeln!(out, "call sequence {} ({})", graph.scope, graph.capability).unwrap();
    if let Some(notice) = &graph.notice {
        writeln!(out, "note: {notice}").unwrap();
    }
    for f in &graph.frames {
        write!(out, "\nframe {}: {}", f.id, f.name).unwrap();
        if let Some(l) = &f.loc {
            write!(out, " ({}:{})", super::basename(&l.path), l.line).unwrap();
        }
        write!(out, ", variant {}", f.variant).unwrap();
        if let Some(unit) = &f.unit {
            write!(out, " of {}", super::basename(unit)).unwrap();
        }
        write!(out, ", {}", f.coverage).unwrap();
        if let Some(e) = f.parent_event {
            write!(out, ", called at e{e}").unwrap();
        }
        writeln!(out).unwrap();
        for b in &index.blocks_of_frame[f.id] {
            write!(out, "  b{} {}", b.id, b.kind).unwrap();
            if let Some(l) = &b.loc {
                write!(out, " {}", l.display()).unwrap();
            }
            writeln!(out).unwrap();
            let groups = index.event_groups(b.id);
            let explicit = groups.iter().any(|g| g.len() > 1);
            for group in groups {
                if group.len() == 1 {
                    let line = event_line(&index, &graph.events[group[0]], explicit);
                    writeln!(out, "    {line}").unwrap();
                    continue;
                }
                let names: Vec<String> = group
                    .iter()
                    .map(|&e| format!("{}()", graph.events[e].callee))
                    .collect();
                writeln!(out, "    {{ {} }}  (order unspecified)", names.join(", ")).unwrap();
                for &e in &group {
                    let line = event_line(&index, &graph.events[e], explicit);
                    writeln!(out, "      {line}").unwrap();
                }
            }
            let next: Vec<String> = index.edges_from[b.id]
                .iter()
                .map(|e| {
                    if e.kind == EdgeKind::BackEdge.as_str() {
                        format!("↺ b{} (back_edge)", e.dst)
                    } else {
                        format!("b{} ({})", e.dst, e.kind)
                    }
                })
                .collect();
            if !next.is_empty() {
                writeln!(out, "    -> {}", next.join(", ")).unwrap();
            }
        }
        for gap in &f.gaps {
            write!(out, "  gap {}", gap.kind).unwrap();
            if let Some(l) = &gap.loc {
                write!(out, " at {}", l.display()).unwrap();
            }
            if let Some((a, b)) = gap.events {
                write!(out, " over e{a}..e{b}").unwrap();
            }
            writeln!(out).unwrap();
        }
        for b in &index.boundaries_of_frame[f.id] {
            writeln!(out, "  {}", boundary_text(b)).unwrap();
        }
    }
    if let Some(r) = &graph.relation {
        writeln!(
            out,
            "\nrelation: {} {} {}",
            r.from.display(),
            r.kind.as_str(),
            r.to.display()
        )
        .unwrap();
        for note in &r.notes {
            writeln!(out, "  {note}").unwrap();
        }
    }
    writeln!(
        out,
        "\n{} frames, {} events, {} boundaries, {} truncated",
        graph.frames.len(),
        graph.events.len(),
        graph.boundaries.len(),
        graph.truncated.len()
    )
    .unwrap();
    out
}

/// Mermaid flowchart (`dot = false`) or Graphviz digraph: one cluster per
/// frame, one node per block listing its events, boundaries as leaves.
fn render_flowchart(graph: &SequenceGraph, dot: bool) -> String {
    use crate::render::{dot_escape, graph_ident, mermaid_escape};
    let escape = |s: &str| {
        if dot {
            dot_escape(s)
        } else {
            mermaid_escape(s)
        }
    };
    let block = |b: usize| graph_ident(b as i64);
    // Boundaries use the negative identifiers `graph_ident` keeps distinct.
    let leaf = |i: usize| graph_ident(-(i as i64) - 1);
    let index = RenderIndex::new(graph);
    let mut out = String::new();
    if dot {
        writeln!(out, "digraph sequence {{").unwrap();
        writeln!(out, "  label=\"{}\";", escape(&graph.scope)).unwrap();
        writeln!(out, "  node [shape=box];").unwrap();
    } else {
        writeln!(out, "flowchart TD").unwrap();
        writeln!(out, "  %% {}", escape(&graph.scope)).unwrap();
    }
    for f in &graph.frames {
        let title = format!("{} (variant {}, {})", f.name, f.variant, f.coverage);
        if dot {
            writeln!(out, "  subgraph cluster_{} {{", f.id).unwrap();
            writeln!(out, "    label=\"{}\";", escape(&title)).unwrap();
        } else {
            writeln!(out, "  subgraph f{}[\"{}\"]", f.id, escape(&title)).unwrap();
        }
        for b in &index.blocks_of_frame[f.id] {
            let mut lines = vec![escape(&format!("b{} {}", b.id, b.kind))];
            for group in index.event_groups(b.id) {
                let names: Vec<String> = group
                    .iter()
                    .map(|&e| format!("{}()", graph.events[e].callee))
                    .collect();
                lines.push(escape(&if names.len() == 1 {
                    names[0].clone()
                } else {
                    format!("{{ {} }}", names.join(", "))
                }));
            }
            let label = lines.join(if dot { "\\n" } else { "<br/>" });
            if dot {
                writeln!(out, "    {} [label=\"{label}\"];", block(b.id)).unwrap();
            } else {
                writeln!(out, "    {}[\"{label}\"]", block(b.id)).unwrap();
            }
        }
        writeln!(out, "  {}", if dot { "}" } else { "end" }).unwrap();
    }
    for e in &graph.edges {
        let label = match e.event {
            Some(ev) if e.kind == "call" => format!("call {}", graph.events[ev].callee),
            _ => e.kind.clone(),
        };
        let dashed = e.kind == EdgeKind::Exception.as_str();
        if dot {
            let style = if dashed { ", style=dashed" } else { "" };
            writeln!(
                out,
                "  {} -> {} [label=\"{}\"{style}];",
                block(e.src),
                block(e.dst),
                escape(&label)
            )
            .unwrap();
        } else {
            let arrow = if dashed { "-.->" } else { "-->" };
            writeln!(
                out,
                "  {} {arrow}|\"{}\"| {}",
                block(e.src),
                escape(&label),
                block(e.dst)
            )
            .unwrap();
        }
    }
    for (i, b) in graph.boundaries.iter().enumerate() {
        let from = match b.event {
            Some(e) => graph.events[e].block,
            None => graph.frames[b.frame].entry,
        };
        let label = boundary_text(b);
        if dot {
            writeln!(
                out,
                "  {} [shape=note, label=\"{}\"];",
                leaf(i),
                escape(&label)
            )
            .unwrap();
            writeln!(out, "  {} -> {} [style=dashed];", block(from), leaf(i)).unwrap();
        } else {
            writeln!(out, "  {}[/\"{}\"/]", leaf(i), escape(&label)).unwrap();
            writeln!(out, "  {} -.-> {}", block(from), leaf(i)).unwrap();
        }
    }
    if dot {
        writeln!(out, "}}").unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query loads each graph it reaches; a statement that scans a
    /// `cfg_*` table per load makes the query linear in the whole database
    /// per frame (seconds on camera).
    #[test]
    fn loading_a_graph_is_indexed() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::SCHEMA_V8).unwrap();
        let reads = crate::CFG_READS.map(|(_, sql)| sql);
        for sql in reads.into_iter().chain([
            LOAD_GRAPH_SQL,
            LOAD_CALLEES_SQL,
            LOAD_TARGETS_SQL,
            LOAD_CONTEXTS_SQL,
            LOAD_IPC_SQL,
        ]) {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            // One graph: every bound is its id.
            let ids = vec![1; stmt.parameter_count()];
            let plan: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(ids), |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter().all(|step| !step.starts_with("SCAN")),
                "{sql}\n{plan:?}"
            );
        }
    }

    /// Groups as defined: a position joins the last group when no order
    /// path joins it to any member, either way.
    fn runs_by_search(next: &[Vec<usize>]) -> Vec<Vec<usize>> {
        let reaches = |a: usize, b: usize| {
            let mut seen = vec![false; next.len()];
            search(a, &mut seen, |x| &next[x], |y| y == b)
        };
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for e in 0..next.len() {
            match groups.last_mut() {
                Some(g) if g.iter().all(|&x| !reaches(x, e) && !reaches(e, x)) => g.push(e),
                _ => groups.push(vec![e]),
            }
        }
        groups
    }

    #[test]
    fn unordered_runs_agree_with_path_search() {
        // Deterministic pseudo-random forward orders of every density.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut random = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % below
        };
        for round in 0..400 {
            let n = 1 + random(24) as usize;
            let density = 1 + random(6);
            let mut next = vec![Vec::new(); n];
            for (p, succ) in next.iter_mut().enumerate() {
                for q in p + 1..n {
                    if random(density * 2) == 0 {
                        succ.push(q);
                    }
                }
            }
            assert_eq!(
                unordered_runs(&next),
                runs_by_search(&next),
                "round {round}: {next:?}"
            );
        }
        // A pair running backwards takes the general search.
        let next = vec![vec![], vec![], vec![0], vec![]];
        assert_eq!(unordered_runs(&next), runs_by_search(&next));
        assert_eq!(unordered_runs(&next), [vec![0, 1], vec![2, 3]]);
    }

    /// One block of `n` events in a chain, each with a boundary: rendering
    /// reads each row a bounded number of times (scanning the order and the
    /// boundaries per event took minutes here).
    #[test]
    fn rendering_a_long_block_is_linear() {
        let n = 40_000;
        let loc = SeqLoc {
            path: "a.c".into(),
            line: 1,
            col: 1,
        };
        let graph = SequenceGraph {
            capability: "complete".into(),
            scope: "function f".into(),
            frames: vec![SeqFrame {
                id: 0,
                fn_id: 1,
                name: "f".into(),
                loc: None,
                cfg_id: 1,
                variant: 0,
                unit: None,
                coverage: "complete".into(),
                gaps: Vec::new(),
                parent_event: None,
                entry: 0,
                exit: 0,
            }],
            blocks: vec![SeqBlock {
                id: 0,
                frame: 0,
                ordinal: 0,
                kind: "ordinary".into(),
                loc: None,
            }],
            events: (0..n)
                .map(|id| SeqEvent {
                    id,
                    db_id: id as i64,
                    frame: 0,
                    block: 0,
                    kind: "call".into(),
                    callee: "g".into(),
                    loc: loc.clone(),
                    expansion: None,
                    targets: Vec::new(),
                    context_start: None,
                })
                .collect(),
            order: (1..n).map(|e| (e - 1, e)).collect(),
            boundaries: (0..n)
                .map(|e| SeqBoundary {
                    kind: "unknown_callee".into(),
                    frame: 0,
                    event: Some(e),
                    fn_id: None,
                    name: None,
                    reason: None,
                })
                .collect(),
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for format in [crate::RenderFormat::Text, crate::RenderFormat::Mermaid] {
                let _ = tx.send(render_sequence(&graph, format).map(|out| out.len()));
            }
        });
        for _ in 0..2 {
            let len = rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("rendering finishes")
                .unwrap();
            assert!(len > n);
        }
    }
}
