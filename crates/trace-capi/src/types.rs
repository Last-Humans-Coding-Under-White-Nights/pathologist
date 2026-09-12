//! `#[repr(C)]` types mirrored by `include/trace.h`. Field order, widths and
//! enum values are ABI; keep this file and the header in lockstep.

use std::ffi::{c_char, c_void};

/// Status code returned by every `trace_*` function; 0 means success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum TraceStatus {
    TraceOk = 0,
    TraceErrInvalidArg = 1,
    TraceErrIo = 2,
    TraceErrAnalysis = 3,
    TraceErrNotFound = 4,
    TraceErrPanic = 5,
}

/// Traversal direction for graph queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum TraceDirection {
    TraceDirectionDown = 0,
    TraceDirectionUp = 1,
}

/// PAG node kind of a value-flow graph node (`flow_nodes.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum TraceNodeKind {
    TraceNodeVar = 0,
    TraceNodeLoc = 1,
    TraceNodeCallTarget = 2,
    TraceNodeTerminator = 3,
    TraceNodeUnknown = -1,
}

/// Abstract-location category of a `loc` node (`flow_nodes.detail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum TraceLocKind {
    TraceLocGlobal = 0,
    TraceLocFileStatic = 1,
    TraceLocFnStatic = 2,
    TraceLocLocal = 3,
    TraceLocHeap = 4,
    TraceLocField = 5,
    TraceLocFieldSummary = 6,
    TraceLocArraySummary = 7,
    TraceLocFunction = 8,
    TraceLocStringLit = 9,
    TraceLocUnknown = -1,
}

/// How a call graph edge was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum TraceResolution {
    TraceResolutionDirect = 0,
    TraceResolutionIndirect = 1,
    TraceResolutionAmbiguous = 2,
    TraceResolutionExternal = 3,
    /// Synthetic edge injected for an IPC proxy/stub (or client/bridge) pair;
    /// has no real call site. Added after 0.1; extend, don't renumber.
    TraceResolutionIpc = 4,
    TraceResolutionUnknown = -1,
}

/// Constraint kind carried by a value-flow (dataflow) edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum TraceFlowKind {
    TraceFlowCopy = 0,
    TraceFlowAddrOf = 1,
    TraceFlowLoad = 2,
    TraceFlowStore = 3,
    TraceFlowGep = 4,
    TraceFlowPointsTo = 5,
    TraceFlowCallArg = 6,
    TraceFlowTerminates = 7,
    TraceFlowDlsym = 8,
    TraceFlowUnknown = -1,
}

/// Storage class of a symbol (`variables.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum TraceSymbolKind {
    TraceSymGlobal = 0,
    TraceSymFileStatic = 1,
    TraceSymFnStatic = 2,
    TraceSymParam = 3,
    TraceSymLocal = 4,
    TraceSymUnknown = -1,
}

/// A function found by `trace_db_find_functions`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct TraceFunction {
    pub id: i64,
    pub name: *const c_char,
    pub path: *const c_char,
    pub line_start: i64,
    pub line_end: i64,
    pub is_defined: i32,
}

/// A list of functions. Strings inside `items` are owned by the list's
/// arena; free the whole list with `trace_function_list_free`.
#[repr(C)]
pub struct TraceFunctionList {
    pub items: *mut TraceFunction,
    pub count: usize,
    pub _impl: *mut c_void,
}

/// A symbol (variable) found by `trace_db_find_symbols`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct TraceSymbol {
    pub var_id: i64,
    pub name: *const c_char,
    /// One of the `TRACE_SYM_*` values or `TRACE_SYM_UNKNOWN` (-1). Stored as
    /// a raw `i32` (not the Rust enum) because callers may pass it back into
    /// `trace_db_dataflow`, and reading an out-of-band discriminant as an
    /// enum would be UB.
    pub kind: i32,
    pub fn_name: *const c_char,
    pub path: *const c_char,
    pub line: i64,
    pub col: i64,
}

/// A list of symbols; free with `trace_symbol_list_free`.
#[repr(C)]
pub struct TraceSymbolList {
    pub items: *mut TraceSymbol,
    pub count: usize,
    pub _impl: *mut c_void,
}

/// A call graph edge from `trace_db_call_edges`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct TraceCallEdge {
    pub caller_id: i64,
    pub caller_name: *const c_char,
    pub caller_path: *const c_char,
    pub callee_name: *const c_char,
    pub callee_path: *const c_char,
    pub resolution: TraceResolution,
    pub path: *const c_char,
    pub line: i32,
    pub col: i32,
}

/// A list of call edges; free with `trace_call_edge_list_free`.
#[repr(C)]
pub struct TraceCallEdgeList {
    pub items: *mut TraceCallEdge,
    pub count: usize,
    pub _impl: *mut c_void,
}

/// A node of a call or value-flow graph.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct TraceGraphNode {
    pub id: i64,
    pub depth: i64,
    pub kind: TraceNodeKind,
    pub loc_kind: TraceLocKind,
    pub label: *const c_char,
    pub detail: *const c_char,
}

/// An edge of a call or value-flow graph. One of `resolution`/`flow_kind` is
/// meaningful depending on the graph type; the other is `*_Unknown`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct TraceGraphEdge {
    pub from: i64,
    pub to: i64,
    pub resolution: TraceResolution,
    pub flow_kind: TraceFlowKind,
    pub path: *const c_char,
    pub line: i32,
    pub col: i32,
}

/// A call or value-flow graph result; free with `trace_graph_free`.
#[repr(C)]
pub struct TraceGraph {
    pub nodes: *mut TraceGraphNode,
    pub n_nodes: usize,
    pub edges: *mut TraceGraphEdge,
    pub n_edges: usize,
    pub truncated: i32,
    pub _impl: *mut c_void,
}

/// Options for `trace_index`. Set `size = sizeof(trace_index_options)` so
/// the library can reject ABI-mismatched consumers; when `size` is 0 it is
/// not checked (lenient mode).
#[repr(C)]
pub struct TraceIndexOptions {
    pub size: usize,
    pub root: *const c_char,
    pub output_db: *const c_char,
    pub includes: *const *const c_char,
    pub n_includes: usize,
    pub defines: *const *const c_char,
    pub n_defines: usize,
    pub jobs: i32,
    pub full_export: i32,
    pub debug_points_to: i32,
    pub models: *const *const c_char,
    pub n_models: usize,
}

/// Summary counters filled by `trace_index` on success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct TraceIndexResult {
    pub files: u64,
    pub functions: u64,
    pub call_edges: u64,
    pub arg_flow_edges: u64,
}
