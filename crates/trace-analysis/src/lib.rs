//! Andersen-style pointer analysis and call graph construction.

mod callgraph;
mod constraints;
mod contexts;
mod ipc;
mod pag;
mod solver;
mod summaries;

pub use callgraph::*;
pub use constraints::{
    AbstractLocation, ArgFlowEdge, CallGraphEdge, Constraint, ConstraintKind, LocKind,
    ResolutionKind,
};
pub use contexts::{strongly_connected_components, ContextStart, ExecutionContext, MultiInstance};
pub use ipc::detect_ipc_pairs;
pub use pag::*;
pub use solver::{
    analyze, analyze_with_options, default_pops_budget, AnalysisResult, AnalyzeOptions,
    SolveOutcome, SYNTHETIC_CALL_SITE,
};
pub use summaries::*;
