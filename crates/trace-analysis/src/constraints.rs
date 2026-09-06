use trace_ir::{FieldId, LocId, PagNodeId, VarId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstraintKind {
    Copy,
    AddrOf,
    Load,
    Store,
    Gep,
    /// `pts(dst)` gains function locations named by string constants in `pts(src)`.
    Dlsym,
}

#[derive(Debug, Clone)]
pub struct Constraint {
    pub kind: ConstraintKind,
    pub dst: PagNodeId,
    pub src: PagNodeId,
    pub field: Option<FieldId>,
    pub field_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionKind {
    Direct,
    Indirect,
    Ambiguous,
    /// Plain-identifier call to a function with no definition under the
    /// analyzed root (libc, logging backends, vendor externs). Resolved to
    /// a synthesized bodyless entry — not an unresolved indirect site.
    External,
    /// Synthetic edge injected for an IPC proxy→stub bridge. No source-level
    /// call site (the proxy body only has an opaque `SendRequest` call); see
    /// `SYNTHETIC_CALL_SITE`. Distinct from `Direct` so consumers can recognize
    /// and optionally filter bridge edges.
    IpcBridge,
}

#[derive(Debug, Clone)]
pub struct CallGraphEdge {
    pub call_site: trace_ir::CallSiteId,
    pub caller: trace_ir::FnId,
    pub callee: trace_ir::FnId,
    pub resolution: ResolutionKind,
}

#[derive(Debug, Clone)]
pub struct ArgFlowEdge {
    pub call_site: trace_ir::CallSiteId,
    pub arg_index: u32,
    pub actual_var: Option<VarId>,
    pub actual_fn: Option<trace_ir::FnId>,
    pub formal: VarId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocKind {
    Global,
    FileStatic,
    FnStatic,
    Local,
    Heap,
    Field,
    /// Merged field storage across all instances of a struct type (may-analysis).
    FieldSummary,
    ArraySummary,
    Function,
    /// Interned C string literal (value in `AbstractLocation.desc`).
    StringLit,
}

#[derive(Debug, Clone)]
pub struct AbstractLocation {
    pub id: LocId,
    pub kind: LocKind,
    pub var: Option<VarId>,
    pub fn_id: Option<trace_ir::FnId>,
    pub field: Option<FieldId>,
    pub type_id: trace_ir::TypeId,
    pub desc: String,
}
