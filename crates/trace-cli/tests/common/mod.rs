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

/// The one function entry named `name`; fails if there are none or several.
pub fn only_function(program: &Program, name: &str) -> trace_ir::FnId {
    let found: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == name)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `{name}`, got {found:?}"
    );
    found[0].id
}

/// The one variable named `name`; fails if there are none or several.
pub fn only_variable(program: &Program, name: &str) -> trace_ir::VarId {
    unique_variable(program, None, name).id
}

/// The one variable named `name`, owned by `function` when given; fails if
/// there are none or several.
fn unique_variable<'a>(
    program: &'a Program,
    function: Option<&str>,
    name: &str,
) -> &'a trace_ir::Variable {
    let found: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == name)
        .filter(|v| function.is_none_or(|f| v.fn_id.is_some_and(|id| fn_name(program, id) == f)))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `{name}` in {function:?}, got {found:?}"
    );
    found[0]
}

/// The `FileId` of `path` under `root`. Files are interned by canonical
/// path, which a temporary directory's path need not be.
pub fn file_id(program: &Program, root: &Path, path: &str) -> trace_ir::FileId {
    let canonical = root.join(path).canonicalize().unwrap();
    program
        .symbols
        .file_by_path(&canonical)
        .unwrap_or_else(|| panic!("{path} is not a file of the program"))
}

/// A `compile_commands.json` in `root` compiling each of `files` as C++.
pub fn write_compile_commands(root: &Path, files: &[&str]) {
    let commands: Vec<_> = files
        .iter()
        .map(|file| {
            serde_json::json!({"directory": root, "file": file, "arguments": ["c++", "-c", file]})
        })
        .collect();
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::to_string(&commands).unwrap(),
    )
    .unwrap();
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

/// Names of the variables whose storage location is in `var`'s points-to
/// set, sorted. `var` must name exactly one variable; use
/// [`assert_local_points_to`] for a name several functions declare. Needs an
/// analysis run with `retain_points_to: true`.
pub fn points_to_names(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
    var: &str,
) -> Vec<String> {
    pts_names_of(program, pag, analysis, None, var)
}

fn pts_names_of(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
    function: Option<&str>,
    var: &str,
) -> Vec<String> {
    let v = unique_variable(program, function, var);
    let Some(pts) = pag
        .var_node
        .get(&v.id)
        .and_then(|n| analysis.points_to.get(n))
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = program
        .symbols
        .variables
        .iter()
        .filter(|w| pag.var_location.get(&w.id).is_some_and(|l| pts.contains(l)))
        .map(|w| w.name.clone())
        .collect();
    names.sort();
    names
}

/// Pointee names are matched program-wide, so a target must name exactly one
/// variable; a name several functions declare would make the check ambiguous.
fn require_unique_target(program: &Program, target: &str) {
    only_variable(program, target);
}

/// Assert that `var` points to `target`, printing the whole set on failure.
pub fn assert_points_to(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
    var: &str,
    target: &str,
) {
    require_unique_target(program, target);
    let names = points_to_names(program, pag, analysis, var);
    assert!(
        names.iter().any(|n| n == target),
        "{var} should point to {target}; points-to = {names:?}"
    );
}

/// Assert that `var` does not point to `target`, printing the whole set on failure.
pub fn assert_not_points_to(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
    var: &str,
    target: &str,
) {
    require_unique_target(program, target);
    let names = points_to_names(program, pag, analysis, var);
    assert!(
        !names.iter().any(|n| n == target),
        "{var} must not point to {target}; points-to = {names:?}"
    );
}

/// Assert that `function`'s local or parameter `var` points to `target`.
pub fn assert_local_points_to(
    program: &Program,
    pag: &trace_analysis::Pag,
    analysis: &AnalysisResult,
    function: &str,
    var: &str,
    target: &str,
) {
    require_unique_target(program, target);
    let names = pts_names_of(program, pag, analysis, Some(function), var);
    assert!(
        names.iter().any(|n| n == target),
        "{function}::{var} should point to {target}; points-to = {names:?}"
    );
}
