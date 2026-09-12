//! Shared helpers for trace integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use trace_analysis::{AnalysisResult, ResolutionKind};
use trace_ir::Program;
use trace_preproc::PreprocessOptions;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

pub fn default_opts(root: &Path) -> PreprocessOptions {
    let include_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/include");
    PreprocessOptions::new()
        .with_include(root.to_path_buf())
        .with_include(include_dir)
}

pub fn fn_name(program: &Program, id: trace_ir::FnId) -> String {
    program.symbols.function(id).name.clone()
}

pub fn has_edge(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee: &str,
    resolution: ResolutionKind,
) -> bool {
    analysis.call_edges.iter().any(|e| {
        fn_name(program, e.caller) == caller
            && fn_name(program, e.callee) == callee
            && e.resolution == resolution
    })
}

/// An edge from `caller` to `callee` at any resolution kind.
pub fn has_any_edge(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee: &str,
) -> bool {
    analysis
        .call_edges
        .iter()
        .any(|e| fn_name(program, e.caller) == caller && fn_name(program, e.callee) == callee)
}

pub fn must_not_have_edge(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee: &str,
) -> bool {
    !analysis
        .call_edges
        .iter()
        .any(|e| fn_name(program, e.caller) == caller && fn_name(program, e.callee) == callee)
}

pub fn callees_of(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
) -> Vec<(String, ResolutionKind)> {
    analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(program, e.caller) == caller)
        .map(|e| (fn_name(program, e.callee), e.resolution))
        .collect()
}

/// A SQLite export in a temporary directory of its own; both go away when
/// the value is dropped, including on a failed assertion.
pub struct TempDb {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl TempDb {
    pub fn new(file_name: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(file_name);
        Self { _dir: dir, path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempDb {
    type Target = Path;
    fn deref(&self) -> &Path {
        self.path()
    }
}

impl AsRef<Path> for TempDb {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

pub fn export_program(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
) -> TempDb {
    export_program_with_options(program, pag, analysis, false)
}

pub fn export_program_full(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
) -> TempDb {
    export_program_with_options(program, pag, analysis, true)
}

fn export_program_with_options(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
    full_detail: bool,
) -> TempDb {
    let out = TempDb::new("trace_export.db");
    trace_db::export_to_sqlite(
        program,
        pag,
        analysis,
        &trace_db::ExportOptions {
            output: out.path().to_path_buf(),
            trace_version: env!("CARGO_PKG_VERSION").to_owned(),
            include_points_to: false,
            full_detail,
            model_files: Vec::new(),
        },
    )
    .expect("export");
    out
}

pub fn arg_flow_count(analysis: &AnalysisResult) -> usize {
    analysis.arg_flow_edges.len()
}

pub fn has_fn_arg_flow(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee: &str,
    arg_index: u32,
    actual_fn: &str,
) -> bool {
    let caller_id = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == caller)
        .map(|f| f.id);
    let actual_id = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == actual_fn)
        .map(|f| f.id);
    let (Some(caller_id), Some(actual_id)) = (caller_id, actual_id) else {
        return false;
    };
    let call_site_ids: std::collections::HashSet<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller_id && fn_name(program, e.callee) == callee)
        .map(|e| e.call_site)
        .collect();
    analysis.arg_flow_edges.iter().any(|e| {
        call_site_ids.contains(&e.call_site)
            && e.arg_index == arg_index
            && e.actual_fn == Some(actual_id)
    })
}
