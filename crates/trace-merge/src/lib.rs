//! Cross-repository database merge and callgraph reconstruction.
//!
//! Merges independent per-repository SQLite databases produced by `trace analyze`
//! into a single unified database. Cross-repository call edges are reconstructed by
//! resolving `external` function calls (`is_defined = 0`) to matching definitions
//! (`is_defined = 1`, `linkage = 'external'`) in other repositories.
//!
//! No dataflow or pointer analysis is performed during this stage.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use rustc_hash::{FxHashMap, FxHashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use trace_db::{MultiInstance, INDEXES_V7, SCHEMA_VERSION, TABLES_V7};

// Use the same projections for capability checks and ingestion. Additive v7
// flow metadata is optional here because the merger only preserves call graphs.
const READ_FILES: &str = "SELECT id, path, sha256, is_dep FROM files ORDER BY id";
const READ_TARGETS: &str = "SELECT id, name, output FROM link_targets ORDER BY id";
const READ_TARGET_SOURCES: &str =
    "SELECT target_id, file_id FROM target_sources ORDER BY target_id, file_id";
const READ_TARGET_DEPENDENCIES: &str =
    "SELECT target_id, dependency_id FROM target_dependencies ORDER BY target_id, dependency_id";
const READ_FUNCTIONS: &str = "SELECT id, name, file_id, line_start, line_end, linkage, signature, is_defined, is_dep, is_weak, target_id \
             FROM functions ORDER BY id";
const READ_CALL_SITES: &str = "SELECT id, caller_fn_id, file_id, line, col, expansion_file_id, expansion_line, expansion_col, callee_text, is_direct \
             FROM call_sites ORDER BY id";
const READ_CALL_EDGES: &str = "SELECT id, call_site_id, caller_fn_id, callee_fn_id, resolution \
             FROM call_edges ORDER BY id";
const READ_DIAGNOSTICS: &str =
    "SELECT id, severity, file_id, line, message, stage FROM diagnostics ORDER BY id";
// Optional: an earlier v7 export has no `execution_contexts` table and
// contributes no rows; a table that is present must have these columns.
const READ_EXECUTION_CONTEXTS: &str = "SELECT kind, entry_fn_id, call_site_id, api_fn_id, param_index, model, multi_instance, self_concurrent \
             FROM execution_contexts ORDER BY id";

#[derive(Debug, Clone)]
pub struct MergeOptions {
    pub output: PathBuf,
    pub verbose: bool,
}

#[derive(Debug, Clone, Default)]
pub struct MergeReport {
    pub input_dbs: Vec<PathBuf>,
    pub files_total: usize,
    pub files_deduped: usize,
    pub functions_total: usize,
    pub functions_defined: usize,
    pub functions_declarations: usize,
    pub header_functions_deduped: usize,
    pub call_sites_total: usize,
    pub call_edges_total: usize,
    pub cross_repo_calls_resolved: usize,
    pub external_calls_unresolved: usize,
    pub ambiguous_calls: usize,
    pub warnings: Vec<MergeWarning>,
}

#[derive(Debug, Clone)]
pub struct MergeWarning {
    pub kind: WarningKind,
    pub message: String,
    pub symbol: Option<String>,
    pub file_id: Option<i64>,
    pub line: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningKind {
    MultipleDefinitions,
    UnresolvedExternal,
    WeakOverride,
    DuplicateInput,
    /// An input exported before `execution_contexts` existed: it contributes
    /// no execution contexts to the merged database.
    MissingExecutionContexts,
}

#[derive(Debug, Clone)]
struct DbFile {
    id: i64,
    path: String,
    sha256: String,
    is_dep: i64,
}

#[derive(Debug, Clone)]
struct DbLinkTarget {
    id: i64,
    name: String,
    output: String,
}

#[derive(Debug, Clone)]
struct DbFunction {
    id: i64,
    name: String,
    file_id: i64,
    line_start: i64,
    line_end: i64,
    linkage: String,
    signature: String,
    is_defined: i64,
    is_dep: i64,
    is_weak: i64,
    target_id: Option<i64>,
}

#[derive(Debug, Clone)]
struct DbCallSite {
    id: i64,
    caller_fn_id: i64,
    file_id: i64,
    line: i64,
    col: i64,
    expansion_file_id: Option<i64>,
    expansion_line: Option<i64>,
    expansion_col: Option<i64>,
    callee_text: String,
    is_direct: i64,
}

/// An `execution_contexts` row, ids already remapped into the merged
/// database.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DbExecutionContext {
    kind: String,
    entry_fn_id: i64,
    call_site_id: Option<i64>,
    api_fn_id: Option<i64>,
    param_index: Option<i64>,
    /// The model stating the context; the receiver variable is not carried,
    /// as no variable is.
    model: Option<String>,
    multi_instance: String,
    self_concurrent: i64,
}

/// What makes two `execution_contexts` rows one context: call site, modelled
/// callee, parameter, entry, kind and model.
type ContextIdentity<'a> = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i64,
    &'a str,
    Option<&'a str>,
);

impl DbExecutionContext {
    /// What makes two rows one context: the start, what it runs and the
    /// model saying so, not the evidence recorded for it.
    fn identity(&self) -> ContextIdentity<'_> {
        (
            self.call_site_id,
            self.api_fn_id,
            self.param_index,
            self.entry_fn_id,
            &self.kind,
            self.model.as_deref(),
        )
    }
}

/// Merged `execution_contexts` rows in the order `trace analyze` writes them:
/// the submitted contexts by call site, modelled callee, parameter, entry,
/// kind and model, then the entries no call site starts (overrides of a
/// framework member, IPC handlers) by entry, kind and model. Rows several
/// inputs contribute for one context (an IPC handler a shared header defines) are
/// one row with the evidence of all: self-concurrent when any is, and the
/// first `multi_instance` that applies in the analysis' order.
fn unify_execution_contexts(mut rows: Vec<DbExecutionContext>) -> Vec<DbExecutionContext> {
    fn precedence(multi_instance: &str) -> usize {
        MultiInstance::parse(multi_instance).map_or(usize::MAX, MultiInstance::rank)
    }
    // Stable: rows of one context stay in input order, so the kept row is
    // the first and ties in `multi_instance` keep the earlier value.
    rows.sort_by(|a, b| {
        (a.call_site_id.is_none(), a.identity()).cmp(&(b.call_site_id.is_none(), b.identity()))
    });
    rows.dedup_by(|later, held| {
        if later.identity() != held.identity() {
            return false;
        }
        held.self_concurrent = held.self_concurrent.max(later.self_concurrent);
        if precedence(&later.multi_instance) < precedence(&held.multi_instance) {
            held.multi_instance = std::mem::take(&mut later.multi_instance);
        }
        true
    });
    rows
}

#[derive(Debug, Clone)]
struct DbCallEdge {
    id: i64,
    call_site_id: Option<i64>,
    caller_fn_id: i64,
    callee_fn_id: i64,
    resolution: String,
}

#[derive(Debug, Clone)]
struct DbDiagnostic {
    id: i64,
    severity: String,
    file_id: Option<i64>,
    line: i64,
    message: String,
    stage: String,
}

#[derive(Debug, Clone)]
struct DefRecord {
    new_fn_id: i64,
    db_idx: usize,
    signature: String,
    file_id: i64,
    file_path: String,
    line_start: i64,
    is_weak: bool,
    target_id: Option<i64>,
}

/// Merge multiple per-repository SQLite databases into a unified database.
pub fn merge_databases<P: AsRef<Path>>(
    input_paths: &[P],
    options: &MergeOptions,
) -> Result<MergeReport> {
    if input_paths.is_empty() {
        bail!("no input databases specified; at least one database required for merge");
    }

    let mut report = MergeReport::default();
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut seen_inputs: FxHashSet<PathBuf> = FxHashSet::default();

    for p in input_paths {
        let path = p.as_ref();
        let canon = path
            .canonicalize()
            .with_context(|| format!("failed to access input database {}", path.display()))?;
        if !seen_inputs.insert(canon.clone()) {
            report.warnings.push(MergeWarning {
                kind: WarningKind::DuplicateInput,
                message: format!("duplicate input database ignored: {}", path.display()),
                symbol: None,
                file_id: None,
                line: 0,
            });
            continue;
        }
        inputs.push(path.to_path_buf());
    }

    if options.output.exists() {
        if let Ok(out_canon) = options.output.canonicalize() {
            if seen_inputs.contains(&out_canon) {
                bail!(
                    "output database '{}' matches an input database; cannot overwrite an active input database",
                    options.output.display()
                );
            }
        }
    }

    report.input_dbs = inputs.clone();

    // Verify all input databases and read schema versions.
    let mut conns = Vec::new();
    // Per input: whether it has the `execution_contexts` table.
    let mut has_contexts_table: Vec<bool> = Vec::new();
    for path in &inputs {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("failed to open input database {}", path.display()))?;
        let ver: i64 = conn
            .query_row(
                "SELECT schema_version FROM analysis_run ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .with_context(|| {
                format!(
                    "failed to read schema_version from analysis_run in {}",
                    path.display()
                )
            })?;
        if ver != SCHEMA_VERSION {
            bail!(
                "incompatible schema version in {}: database has version {}, expected {}; re-analyze the input with the current trace version",
                path.display(),
                ver,
                SCHEMA_VERSION
            );
        }
        // Preparing every ingestion query checks required tables and columns,
        // including empty inputs, before any output is created or replaced.
        for (table, query) in [
            ("files", READ_FILES),
            ("link_targets", READ_TARGETS),
            ("target_sources", READ_TARGET_SOURCES),
            ("target_dependencies", READ_TARGET_DEPENDENCIES),
            ("functions", READ_FUNCTIONS),
            ("call_sites", READ_CALL_SITES),
            ("call_edges", READ_CALL_EDGES),
            ("diagnostics", READ_DIAGNOSTICS),
        ] {
            conn.prepare(query).map_err(|error| {
                anyhow::anyhow!(
                    "missing trace-merge capability in {} ({table}): {error}; re-analyze the input with the current trace version",
                    path.display()
                )
            })?;
        }
        let has_contexts = trace_db::table_exists(&conn, "execution_contexts")?;
        if has_contexts {
            conn.prepare(READ_EXECUTION_CONTEXTS).map_err(|error| {
                anyhow::anyhow!(
                    "missing trace-merge capability in {} (execution_contexts): {error}; re-analyze the input with the current trace version",
                    path.display()
                )
            })?;
        }
        has_contexts_table.push(has_contexts);
        conns.push(conn);
    }

    // 1. Files Ingestion & Deduplication
    let mut unified_files: Vec<DbFile> = Vec::new();
    let mut file_path_to_new_id: FxHashMap<String, i64> = FxHashMap::default();
    let mut file_remap: Vec<FxHashMap<i64, i64>> = vec![FxHashMap::default(); inputs.len()];

    for (db_idx, conn) in conns.iter().enumerate() {
        let mut stmt = conn
            .prepare(READ_FILES)
            .with_context(|| format!("failed to read files from {}", inputs[db_idx].display()))?;
        let rows = stmt.query_map([], |row| {
            Ok(DbFile {
                id: row.get(0)?,
                path: row.get(1)?,
                sha256: row.get(2)?,
                is_dep: row.get(3)?,
            })
        })?;

        for r in rows {
            let file = r?;
            if let Some(&existing_id) = file_path_to_new_id.get(&file.path) {
                file_remap[db_idx].insert(file.id, existing_id);
                // If it is non-dep in any merged repo, it's non-dep globally.
                if file.is_dep == 0 {
                    unified_files[(existing_id - 1) as usize].is_dep = 0;
                }
                report.files_deduped += 1;
            } else {
                let new_id = (unified_files.len() + 1) as i64;
                file_path_to_new_id.insert(file.path.clone(), new_id);
                file_remap[db_idx].insert(file.id, new_id);
                unified_files.push(DbFile {
                    id: new_id,
                    path: file.path,
                    sha256: file.sha256,
                    is_dep: file.is_dep,
                });
            }
        }
    }
    report.files_total = unified_files.len();

    // 2. Link Targets Ingestion & Remapping
    let mut unified_targets: Vec<DbLinkTarget> = Vec::new();
    let mut target_remap: Vec<FxHashMap<i64, i64>> = vec![FxHashMap::default(); inputs.len()];
    let mut unified_target_sources: Vec<(i64, i64)> = Vec::new();
    let mut unified_target_dependencies: Vec<(i64, i64)> = Vec::new();

    for (db_idx, conn) in conns.iter().enumerate() {
        let mut stmt = conn.prepare(READ_TARGETS)?;
        let rows = stmt.query_map([], |r| {
            Ok(DbLinkTarget {
                id: r.get(0)?,
                name: r.get(1)?,
                output: r.get(2)?,
            })
        })?;
        for r in rows {
            let target = r?;
            let new_id = (unified_targets.len() + 1) as i64;
            target_remap[db_idx].insert(target.id, new_id);
            unified_targets.push(DbLinkTarget {
                id: new_id,
                name: target.name,
                output: target.output,
            });
        }

        let mut src_stmt = conn.prepare(READ_TARGET_SOURCES)?;
        let src_rows =
            src_stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for r in src_rows {
            let (old_t, old_f) = r?;
            if let (Some(&new_t), Some(&new_f)) = (
                target_remap[db_idx].get(&old_t),
                file_remap[db_idx].get(&old_f),
            ) {
                unified_target_sources.push((new_t, new_f));
            }
        }

        let mut dep_stmt = conn.prepare(READ_TARGET_DEPENDENCIES)?;
        let dep_rows =
            dep_stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for r in dep_rows {
            let (old_t, old_d) = r?;
            if let (Some(&new_t), Some(&new_d)) = (
                target_remap[db_idx].get(&old_t),
                target_remap[db_idx].get(&old_d),
            ) {
                unified_target_dependencies.push((new_t, new_d));
            }
        }
    }

    // 3. Functions Ingestion, Deduplication & Classification
    let mut unified_functions: Vec<DbFunction> = Vec::new();
    let mut fn_remap: Vec<FxHashMap<i64, i64>> = vec![FxHashMap::default(); inputs.len()];
    // (new_file_id, line_start, name, signature) -> new_fn_id for header-defined inline functions
    let mut header_def_map: FxHashMap<(i64, i64, String, String), i64> = FxHashMap::default();
    // (new_file_id, line_start, name, signature) -> new_fn_id for header prototypes
    let mut header_decl_map: FxHashMap<(i64, i64, String, String), i64> = FxHashMap::default();
    // Exported definitions catalog: name -> list of definitions across repos
    let mut definitions_by_name: FxHashMap<String, Vec<DefRecord>> = FxHashMap::default();
    // Cache per-DB functions by old id for call edge lookup
    let mut per_db_functions: Vec<FxHashMap<i64, DbFunction>> =
        vec![FxHashMap::default(); inputs.len()];

    for (db_idx, conn) in conns.iter().enumerate() {
        let mut stmt = conn.prepare(READ_FUNCTIONS)?;
        let rows = stmt.query_map([], |r| {
            Ok(DbFunction {
                id: r.get(0)?,
                name: r.get(1)?,
                file_id: r.get(2)?,
                line_start: r.get(3)?,
                line_end: r.get(4)?,
                linkage: r.get(5)?,
                signature: r.get(6)?,
                is_defined: r.get(7)?,
                is_dep: r.get(8)?,
                is_weak: r.get(9)?,
                target_id: r.get(10)?,
            })
        })?;

        for r in rows {
            let func = r?;
            per_db_functions[db_idx].insert(func.id, func.clone());
            let new_file_id = *file_remap[db_idx]
                .get(&func.file_id)
                .context("missing remapped file_id for function")?;
            let new_target_id = func
                .target_id
                .and_then(|t| target_remap[db_idx].get(&t).copied());

            // Check if this is an identical header definition or prototype that can be unified.
            if func.linkage != "internal" && func.line_start > 0 {
                let key = (
                    new_file_id,
                    func.line_start,
                    func.name.clone(),
                    func.signature.clone(),
                );
                if func.is_defined != 0 {
                    if let Some(&existing_fn_id) = header_def_map.get(&key) {
                        fn_remap[db_idx].insert(func.id, existing_fn_id);
                        if func.is_dep == 0 {
                            unified_functions[(existing_fn_id - 1) as usize].is_dep = 0;
                        }
                        report.header_functions_deduped += 1;
                        continue;
                    }
                } else if let Some(&existing_fn_id) = header_decl_map.get(&key) {
                    fn_remap[db_idx].insert(func.id, existing_fn_id);
                    if func.is_dep == 0 {
                        unified_functions[(existing_fn_id - 1) as usize].is_dep = 0;
                    }
                    report.header_functions_deduped += 1;
                    continue;
                }
            }

            let new_fn_id = (unified_functions.len() + 1) as i64;
            fn_remap[db_idx].insert(func.id, new_fn_id);

            if func.linkage != "internal" && func.line_start > 0 {
                let key = (
                    new_file_id,
                    func.line_start,
                    func.name.clone(),
                    func.signature.clone(),
                );
                if func.is_defined != 0 {
                    header_def_map.insert(key, new_fn_id);
                } else {
                    header_decl_map.insert(key, new_fn_id);
                }
            }

            // Register in exported definitions catalog if defined with external linkage.
            if func.is_defined != 0 && func.linkage == "external" {
                let file_path = unified_files[(new_file_id - 1) as usize].path.clone();
                definitions_by_name
                    .entry(func.name.clone())
                    .or_default()
                    .push(DefRecord {
                        new_fn_id,
                        db_idx,
                        signature: func.signature.clone(),
                        file_id: new_file_id,
                        file_path,
                        line_start: func.line_start,
                        is_weak: func.is_weak != 0,
                        target_id: new_target_id,
                    });
            }

            unified_functions.push(DbFunction {
                id: new_fn_id,
                name: func.name,
                file_id: new_file_id,
                line_start: func.line_start,
                line_end: func.line_end,
                linkage: func.linkage,
                signature: func.signature,
                is_defined: func.is_defined,
                is_dep: func.is_dep,
                is_weak: func.is_weak,
                target_id: new_target_id,
            });
        }
    }

    report.functions_total = unified_functions.len();
    report.functions_defined = unified_functions
        .iter()
        .filter(|f| f.is_defined != 0)
        .count();
    report.functions_declarations = report.functions_total - report.functions_defined;

    // Detect multiple strong definition conflicts across repositories.
    let mut names: Vec<&String> = definitions_by_name.keys().collect();
    names.sort(); // Deterministic iteration order

    for name in names {
        let defs = &definitions_by_name[name];
        if defs.len() <= 1 {
            continue;
        }

        // Group definitions by signature so valid C++ overloads are evaluated independently
        let mut defs_by_sig: FxHashMap<&str, Vec<&DefRecord>> = FxHashMap::default();
        for d in defs {
            defs_by_sig.entry(&d.signature).or_default().push(d);
        }

        let mut sigs: Vec<&&str> = defs_by_sig.keys().collect();
        sigs.sort();

        for sig in sigs {
            let sig_defs = &defs_by_sig[sig];
            let strong_defs: Vec<&DefRecord> =
                sig_defs.iter().copied().filter(|d| !d.is_weak).collect();
            let mut strong_db_indices: FxHashSet<usize> = FxHashSet::default();
            for d in &strong_defs {
                strong_db_indices.insert(d.db_idx);
            }

            if strong_db_indices.len() > 1 {
                let locations: Vec<String> = strong_defs
                    .iter()
                    .map(|d| {
                        format!(
                            "{}:{} ({})",
                            d.file_path,
                            d.line_start,
                            inputs[d.db_idx].display()
                        )
                    })
                    .collect();
                report.warnings.push(MergeWarning {
                    kind: WarningKind::MultipleDefinitions,
                    message: format!(
                        "multiple strong definitions for symbol '{}': found in:\n  {}",
                        sig,
                        locations.join("\n  ")
                    ),
                    symbol: Some(name.clone()),
                    file_id: Some(strong_defs[0].file_id),
                    line: strong_defs[0].line_start,
                });
            } else if strong_defs.len() == 1 {
                let remote_weak_defs: Vec<&DefRecord> = sig_defs
                    .iter()
                    .copied()
                    .filter(|d| d.is_weak && d.db_idx != strong_defs[0].db_idx)
                    .collect();
                if !remote_weak_defs.is_empty() {
                    let weak_locs: Vec<String> = remote_weak_defs
                        .iter()
                        .map(|d| {
                            format!(
                                "{}:{} ({})",
                                d.file_path,
                                d.line_start,
                                inputs[d.db_idx].display()
                            )
                        })
                        .collect();
                    report.warnings.push(MergeWarning {
                        kind: WarningKind::WeakOverride,
                        message: format!(
                            "weak definition for symbol '{}' ({}) overridden by strong definition in {}:{}",
                            sig,
                            weak_locs.join(", "),
                            strong_defs[0].file_path,
                            strong_defs[0].line_start
                        ),
                        symbol: Some(name.clone()),
                        file_id: Some(strong_defs[0].file_id),
                        line: strong_defs[0].line_start,
                    });
                }
            }
        }
    }

    // 4. Call Sites Ingestion & Remapping
    let mut unified_call_sites: Vec<DbCallSite> = Vec::new();
    let mut cs_remap: Vec<FxHashMap<i64, i64>> = vec![FxHashMap::default(); inputs.len()];

    for (db_idx, conn) in conns.iter().enumerate() {
        let mut stmt = conn.prepare(READ_CALL_SITES)?;
        let rows = stmt.query_map([], |r| {
            Ok(DbCallSite {
                id: r.get(0)?,
                caller_fn_id: r.get(1)?,
                file_id: r.get(2)?,
                line: r.get(3)?,
                col: r.get(4)?,
                expansion_file_id: r.get(5)?,
                expansion_line: r.get(6)?,
                expansion_col: r.get(7)?,
                callee_text: r.get(8)?,
                is_direct: r.get(9)?,
            })
        })?;

        for r in rows {
            let cs = r?;
            let new_caller = *fn_remap[db_idx]
                .get(&cs.caller_fn_id)
                .context("missing remapped caller_fn_id for call_site")?;
            let new_file_id = *file_remap[db_idx]
                .get(&cs.file_id)
                .context("missing remapped file_id for call_site")?;
            let new_expansion_file_id = cs
                .expansion_file_id
                .and_then(|f| file_remap[db_idx].get(&f).copied());

            let new_cs_id = (unified_call_sites.len() + 1) as i64;
            cs_remap[db_idx].insert(cs.id, new_cs_id);

            unified_call_sites.push(DbCallSite {
                id: new_cs_id,
                caller_fn_id: new_caller,
                file_id: new_file_id,
                line: cs.line,
                col: cs.col,
                expansion_file_id: new_expansion_file_id,
                expansion_line: cs.expansion_line,
                expansion_col: cs.expansion_col,
                callee_text: cs.callee_text,
                is_direct: cs.is_direct,
            });
        }
    }
    report.call_sites_total = unified_call_sites.len();

    // 5. Call Edges Ingestion & Cross-Repository Resolution
    let mut unified_call_edges: Vec<DbCallEdge> = Vec::new();
    let mut emitted: FxHashSet<(Option<i64>, i64)> = FxHashSet::default();
    // Per input edge, by its call site and callee as that input recorded
    // them: the merged functions the edge now goes to. An execution context
    // started at that site with that callee as its entry follows them (5b):
    // a callback declared in one repository and defined in another enters
    // the definition, as the edge does.
    let mut retargets: FxHashMap<(usize, i64, i64), Vec<i64>> = FxHashMap::default();
    let mut unresolved_external_counts: FxHashMap<String, usize> = FxHashMap::default();

    for (db_idx, conn) in conns.iter().enumerate() {
        let mut stmt = conn.prepare(READ_CALL_EDGES)?;
        let rows = stmt.query_map([], |r| {
            Ok(DbCallEdge {
                id: r.get(0)?,
                call_site_id: r.get(1)?,
                caller_fn_id: r.get(2)?,
                callee_fn_id: r.get(3)?,
                resolution: r.get(4)?,
            })
        })?;

        for r in rows {
            let edge = r?;
            let new_caller_id = *fn_remap[db_idx]
                .get(&edge.caller_fn_id)
                .context("missing remapped caller_fn_id for call_edge")?;
            let new_cs_id = edge
                .call_site_id
                .and_then(|cs| cs_remap[db_idx].get(&cs).copied());

            let orig_callee = per_db_functions[db_idx]
                .get(&edge.callee_fn_id)
                .context("callee function not found in original db")?;

            let is_external_call = edge.resolution == "external" || orig_callee.is_defined == 0;
            let is_weak_call = orig_callee.is_weak != 0;

            let resolved_resolution = if edge.resolution == "indirect" || edge.resolution == "ipc" {
                edge.resolution.clone()
            } else {
                "direct".to_string()
            };
            let unresolved_resolution = edge.resolution.clone();

            let (new_callees, new_resolution): (Vec<i64>, String) =
                if is_external_call || is_weak_call {
                    // Try to resolve the external or weak function against all known definitions.
                    if let Some(defs) = definitions_by_name.get(&orig_callee.name) {
                        let is_generic_sig = !orig_callee.signature.contains('(')
                            || orig_callee.signature.ends_with("(...)");
                        let (candidates, is_fallback): (Vec<&DefRecord>, bool) = if is_generic_sig {
                            (defs.iter().collect(), true)
                        } else {
                            let matching: Vec<&DefRecord> = defs
                                .iter()
                                .filter(|d| d.signature == orig_callee.signature)
                                .collect();
                            if matching.is_empty() {
                                // `signature` is a lossy spelling (`int[]` vs `int*`,
                                // signedness/const dropped): fall back to every
                                // same-name definition rather than lose the call.
                                (defs.iter().collect(), true)
                            } else {
                                (matching, false)
                            }
                        };

                        let strong_defs: Vec<&DefRecord> =
                            candidates.iter().copied().filter(|d| !d.is_weak).collect();

                        if is_fallback && candidates.len() > 1 {
                            // Multiple candidate definitions from fallback (e.g. multiple overloads
                            // or same-name functions across repos). If there are strong definitions,
                            // filter to strong ones (ELF semantics: strong overrides weak); if multiple
                            // remain (or all are weak), emit ambiguous edges to all of them rather than
                            // arbitrarily picking one via same-database/same-target preference.
                            let pool: Vec<&DefRecord> = if !strong_defs.is_empty() {
                                strong_defs
                            } else {
                                candidates
                            };

                            if pool.len() == 1 {
                                if pool[0].db_idx != db_idx {
                                    report.cross_repo_calls_resolved += 1;
                                }
                                (vec![pool[0].new_fn_id], resolved_resolution)
                            } else {
                                report.ambiguous_calls += 1;
                                let mut callee_ids: Vec<i64> =
                                    pool.iter().map(|d| d.new_fn_id).collect();
                                callee_ids.sort_unstable();
                                callee_ids.dedup();
                                (callee_ids, "ambiguous".to_string())
                            }
                        } else if strong_defs.len() == 1 {
                            // Exactly one strong definition found!
                            if strong_defs[0].db_idx != db_idx {
                                report.cross_repo_calls_resolved += 1;
                            }
                            (vec![strong_defs[0].new_fn_id], resolved_resolution)
                        } else if strong_defs.is_empty() && candidates.len() == 1 {
                            // Exactly one weak definition found!
                            if candidates[0].db_idx != db_idx {
                                report.cross_repo_calls_resolved += 1;
                            }
                            (vec![candidates[0].new_fn_id], resolved_resolution)
                        } else if strong_defs.len() > 1 {
                            // Multiple conflicting strong definitions for the same signature!
                            let caller_func = per_db_functions[db_idx].get(&edge.caller_fn_id);
                            let same_target_strong = caller_func
                                .and_then(|cf| cf.target_id)
                                .and_then(|t| target_remap[db_idx].get(&t).copied())
                                .and_then(|new_t| {
                                    strong_defs.iter().find(|d| d.target_id == Some(new_t))
                                });

                            if let Some(target_def) = same_target_strong {
                                // Target resolution within same target (target_def.db_idx == db_idx).
                                (vec![target_def.new_fn_id], resolved_resolution)
                            } else {
                                // Truly ambiguous across other repos/targets: emit an edge to
                                // EVERY candidate strong definition to preserve may-analysis reachability.
                                report.ambiguous_calls += 1;
                                let mut callee_ids: Vec<i64> =
                                    strong_defs.iter().map(|d| d.new_fn_id).collect();
                                callee_ids.sort_unstable();
                                callee_ids.dedup();
                                (callee_ids, "ambiguous".to_string())
                            }
                        } else if !candidates.is_empty() {
                            // Multiple weak definitions across repos: emit an edge to
                            // EVERY candidate weak definition to preserve may-analysis reachability.
                            report.ambiguous_calls += 1;
                            let mut callee_ids: Vec<i64> =
                                candidates.iter().map(|d| d.new_fn_id).collect();
                            callee_ids.sort_unstable();
                            callee_ids.dedup();
                            (callee_ids, "ambiguous".to_string())
                        } else if is_weak_call {
                            // Retain local weak definition
                            let remapped = *fn_remap[db_idx]
                                .get(&edge.callee_fn_id)
                                .context("missing remapped callee_fn_id")?;
                            (vec![remapped], edge.resolution)
                        } else {
                            // No matching definition found by signature; remains external/unresolved.
                            report.external_calls_unresolved += 1;
                            *unresolved_external_counts
                                .entry(orig_callee.name.clone())
                                .or_default() += 1;
                            let fallback_id = *fn_remap[db_idx]
                                .get(&edge.callee_fn_id)
                                .context("missing remapped callee_fn_id")?;
                            (vec![fallback_id], unresolved_resolution)
                        }
                    } else if is_weak_call {
                        // Retain local weak definition
                        let remapped = *fn_remap[db_idx]
                            .get(&edge.callee_fn_id)
                            .context("missing remapped callee_fn_id")?;
                        (vec![remapped], edge.resolution)
                    } else {
                        // No definition found in any merged repo; remains external/unresolved.
                        report.external_calls_unresolved += 1;
                        *unresolved_external_counts
                            .entry(orig_callee.name.clone())
                            .or_default() += 1;
                        let fallback_id = *fn_remap[db_idx]
                            .get(&edge.callee_fn_id)
                            .context("missing remapped callee_fn_id")?;
                        (vec![fallback_id], unresolved_resolution)
                    }
                } else {
                    // Intra-repo already-resolved call.
                    let remapped_callee = *fn_remap[db_idx]
                        .get(&edge.callee_fn_id)
                        .context("missing remapped callee_fn_id")?;
                    (vec![remapped_callee], edge.resolution)
                };

            if let Some(cs) = edge.call_site_id {
                retargets
                    .entry((db_idx, cs, edge.callee_fn_id))
                    .or_default()
                    .extend(new_callees.iter().copied());
            }
            for new_callee_id in new_callees {
                if new_cs_id.is_some() && !emitted.insert((new_cs_id, new_callee_id)) {
                    continue;
                }
                let new_edge_id = (unified_call_edges.len() + 1) as i64;
                unified_call_edges.push(DbCallEdge {
                    id: new_edge_id,
                    call_site_id: new_cs_id,
                    caller_fn_id: new_caller_id,
                    callee_fn_id: new_callee_id,
                    resolution: new_resolution.clone(),
                });
            }
        }
    }
    report.call_edges_total = unified_call_edges.len();

    // Collect top unresolved external warnings.
    let mut unresolved_sorted: Vec<(&String, &usize)> = unresolved_external_counts.iter().collect();
    unresolved_sorted.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

    for (sym, count) in unresolved_sorted {
        report.warnings.push(MergeWarning {
            kind: WarningKind::UnresolvedExternal,
            message: format!(
                "external symbol '{}' has {} unresolved call(s) (no definition in any merged database)",
                sym, count
            ),
            symbol: Some(sym.clone()),
            file_id: None,
            line: 0,
        });
    }

    // 5b. Execution contexts, carried over with remapped ids. Each keeps the
    // multi-instance evidence of its own input: a cross-repository edge
    // resolved above is not searched for new cycles or parents. A context a
    // call site starts enters what the edge from that site to its entry now
    // goes to (`retargets`): the definition another repository holds of a
    // callback this one only declares, or each candidate of an ambiguous
    // edge. A row whose function or call site did not survive the merge is
    // dropped, and an input exported before the table existed contributes
    // none.
    let mut carried_contexts: Vec<DbExecutionContext> = Vec::new();
    for (db_idx, conn) in conns.iter().enumerate() {
        if !has_contexts_table[db_idx] {
            report.warnings.push(MergeWarning {
                kind: WarningKind::MissingExecutionContexts,
                message: format!(
                    "input {} has no execution_contexts table; its execution contexts are not carried over (re-analyze it with the current trace version to include them)",
                    inputs[db_idx].display()
                ),
                symbol: None,
                file_id: None,
                line: 0,
            });
            continue;
        }
        let mut stmt = conn.prepare(READ_EXECUTION_CONTEXTS)?;
        let rows = stmt.query_map([], |r| {
            Ok(DbExecutionContext {
                kind: r.get(0)?,
                entry_fn_id: r.get(1)?,
                call_site_id: r.get(2)?,
                api_fn_id: r.get(3)?,
                param_index: r.get(4)?,
                model: r.get(5)?,
                multi_instance: r.get(6)?,
                self_concurrent: r.get(7)?,
            })
        })?;
        // An optional id: absent stays absent; present but not carried
        // (`None`) drops the row.
        let remap_optional = |map: &FxHashMap<i64, i64>, id: Option<i64>| match id {
            Some(id) => map.get(&id).copied().map(Some),
            None => Some(None),
        };
        for r in rows {
            let ctx = r?;
            let entries: Vec<i64> = match ctx
                .call_site_id
                .and_then(|cs| retargets.get(&(db_idx, cs, ctx.entry_fn_id)))
            {
                Some(targets) => {
                    let mut targets = targets.clone();
                    targets.sort_unstable();
                    targets.dedup();
                    targets
                }
                None => fn_remap[db_idx]
                    .get(&ctx.entry_fn_id)
                    .copied()
                    .into_iter()
                    .collect(),
            };
            let (Some(call_site_id), Some(api_fn_id)) = (
                remap_optional(&cs_remap[db_idx], ctx.call_site_id),
                remap_optional(&fn_remap[db_idx], ctx.api_fn_id),
            ) else {
                continue;
            };
            for entry_fn_id in entries {
                carried_contexts.push(DbExecutionContext {
                    entry_fn_id,
                    call_site_id,
                    api_fn_id,
                    ..ctx.clone()
                });
            }
        }
    }
    let unified_contexts = unify_execution_contexts(carried_contexts);

    // 6. Diagnostics Ingestion
    let mut unified_diagnostics: Vec<DbDiagnostic> = Vec::new();
    for (db_idx, conn) in conns.iter().enumerate() {
        let mut stmt = conn.prepare(READ_DIAGNOSTICS)?;
        let rows = stmt.query_map([], |r| {
            Ok(DbDiagnostic {
                id: r.get(0)?,
                severity: r.get(1)?,
                file_id: r.get(2)?,
                line: r.get(3)?,
                message: r.get(4)?,
                stage: r.get(5)?,
            })
        })?;
        for r in rows {
            let diag = r?;
            let new_file_id = diag
                .file_id
                .and_then(|f| file_remap[db_idx].get(&f).copied());
            let new_diag_id = (unified_diagnostics.len() + 1) as i64;
            unified_diagnostics.push(DbDiagnostic {
                id: new_diag_id,
                severity: diag.severity,
                file_id: new_file_id,
                line: diag.line,
                message: diag.message,
                stage: diag.stage,
            });
        }
    }

    // Add merge-stage diagnostics.
    for w in &report.warnings {
        let severity = match w.kind {
            WarningKind::MultipleDefinitions => "warning",
            WarningKind::UnresolvedExternal => "info",
            WarningKind::WeakOverride => "info",
            WarningKind::DuplicateInput => "warning",
            WarningKind::MissingExecutionContexts => "info",
        };
        let new_diag_id = (unified_diagnostics.len() + 1) as i64;
        unified_diagnostics.push(DbDiagnostic {
            id: new_diag_id,
            severity: severity.to_string(),
            file_id: w.file_id,
            line: w.line,
            message: w.message.clone(),
            stage: "merge".to_string(),
        });
    }

    // 7. Write to Merged Output SQLite Database
    let parent = options
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    // A fixed `<output>.db.tmp` can name an input or another merge's output.
    // Keep the unique staging file beside the destination for atomic publication
    // and automatic cleanup on every error path.
    let mut staging = tempfile::Builder::new();
    staging.prefix(".trace-merge-");
    // Preserve SQLite's umask-filtered 0644 creation mode used before;
    // tempfile otherwise creates private (0600) files that stay private on publish.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staging.permissions(fs::Permissions::from_mode(0o644));
    }
    let temp = staging.tempfile_in(parent)?;

    {
        let out_conn = Connection::open(temp.path())?;
        out_conn.execute_batch(
            "PRAGMA foreign_keys = OFF; PRAGMA synchronous = OFF; PRAGMA journal_mode = MEMORY;",
        )?;
        out_conn.execute_batch("BEGIN IMMEDIATE;")?;
        out_conn.execute_batch(TABLES_V7)?;

        // Record analysis_run for the merge
        let options_json = serde_json::json!({
            "stage": "merge",
            "merged_databases": inputs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "cross_repo_calls_resolved": report.cross_repo_calls_resolved,
            "external_calls_unresolved": report.external_calls_unresolved,
            "ambiguous_calls": report.ambiguous_calls,
            "warnings_count": report.warnings.len(),
        })
        .to_string();

        let target_root = inputs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(";");

        out_conn.execute(
            "INSERT INTO analysis_run (trace_version, schema_version, target_root, created_at, options_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                env!("CARGO_PKG_VERSION"),
                SCHEMA_VERSION,
                target_root,
                chrono_lite_now(),
                options_json
            ],
        )?;

        // Bulk insert files
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO files (id, path, sha256, is_dep) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for f in &unified_files {
                stmt.execute(params![f.id, f.path, f.sha256, f.is_dep])?;
            }
        }

        // Bulk insert link targets
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO link_targets (id, name, output) VALUES (?1, ?2, ?3)",
            )?;
            for t in &unified_targets {
                stmt.execute(params![t.id, t.name, t.output])?;
            }
            let mut src_stmt = out_conn.prepare_cached(
                "INSERT INTO target_sources (target_id, file_id) VALUES (?1, ?2)",
            )?;
            for (t, f) in &unified_target_sources {
                src_stmt.execute(params![t, f])?;
            }
            let mut dep_stmt = out_conn.prepare_cached(
                "INSERT INTO target_dependencies (target_id, dependency_id) VALUES (?1, ?2)",
            )?;
            for (t, d) in &unified_target_dependencies {
                dep_stmt.execute(params![t, d])?;
            }
        }

        // Bulk insert functions
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO functions (id, name, file_id, line_start, line_end, linkage, signature, is_defined, is_dep, is_weak, target_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for f in &unified_functions {
                stmt.execute(params![
                    f.id,
                    f.name,
                    f.file_id,
                    f.line_start,
                    f.line_end,
                    f.linkage,
                    f.signature,
                    f.is_defined,
                    f.is_dep,
                    f.is_weak,
                    f.target_id
                ])?;
            }
        }

        // Bulk insert call sites. callee_var and return_dst deliberately stay
        // NULL: input variable IDs have no identity in this call-graph-only output.
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO call_sites (id, caller_fn_id, file_id, line, col, expansion_file_id, expansion_line, expansion_col, callee_text, is_direct) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for cs in &unified_call_sites {
                stmt.execute(params![
                    cs.id,
                    cs.caller_fn_id,
                    cs.file_id,
                    cs.line,
                    cs.col,
                    cs.expansion_file_id,
                    cs.expansion_line,
                    cs.expansion_col,
                    cs.callee_text,
                    cs.is_direct
                ])?;
            }
        }

        // Bulk insert call edges
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO call_edges (id, call_site_id, caller_fn_id, callee_fn_id, resolution) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for edge in &unified_call_edges {
                stmt.execute(params![
                    edge.id,
                    edge.call_site_id,
                    edge.caller_fn_id,
                    edge.callee_fn_id,
                    edge.resolution
                ])?;
            }
        }

        // Bulk insert execution contexts
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO execution_contexts (id, kind, entry_fn_id, call_site_id, api_fn_id, \
                 param_index, model, multi_instance, self_concurrent) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for (i, c) in unified_contexts.iter().enumerate() {
                stmt.execute(params![
                    i as i64 + 1,
                    c.kind,
                    c.entry_fn_id,
                    c.call_site_id,
                    c.api_fn_id,
                    c.param_index,
                    c.model,
                    c.multi_instance,
                    c.self_concurrent
                ])?;
            }
        }

        // Bulk insert diagnostics
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO diagnostics (id, severity, file_id, line, message, stage) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for d in &unified_diagnostics {
                stmt.execute(params![
                    d.id, d.severity, d.file_id, d.line, d.message, d.stage
                ])?;
            }
        }

        // Verify referential integrity
        {
            let mut fk_stmt = out_conn.prepare("PRAGMA foreign_key_check")?;
            let fk_violations: Vec<String> = fk_stmt
                .query_map([], |row| {
                    Ok(format!(
                        "table: {}, rowid: {}, parent: {}, fkid: {}",
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if !fk_violations.is_empty() {
                bail!(
                    "foreign key violation in merged database:\n  {}",
                    fk_violations.join("\n  ")
                );
            }
        }

        // Build secondary indexes
        out_conn.execute_batch(INDEXES_V7)?;
        out_conn.execute_batch("COMMIT;")?;
    }

    temp.persist(&options.output)
        .map_err(|e| e.error)
        .with_context(|| {
            format!(
                "failed to publish merged database {}",
                options.output.display()
            )
        })?;

    Ok(report)
}

fn chrono_lite_now() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", dur.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        call_site: Option<i64>,
        entry: i64,
        multi_instance: &str,
        self_concurrent: i64,
    ) -> DbExecutionContext {
        DbExecutionContext {
            kind: if call_site.is_some() {
                "thread"
            } else {
                "ipc_handler"
            }
            .into(),
            entry_fn_id: entry,
            call_site_id: call_site,
            api_fn_id: call_site.map(|_| 100),
            param_index: call_site.map(|_| 2),
            model: call_site.map(|_| "pthread_create".into()),
            multi_instance: multi_instance.into(),
            self_concurrent,
        }
    }

    #[test]
    fn one_context_from_several_inputs_is_one_row_with_all_their_evidence() {
        let unified = unify_execution_contexts(vec![
            row(Some(1), 7, "parent", 1),
            row(Some(1), 7, "unknown", 0),
            row(Some(1), 7, "loop", 1),
            row(None, 9, "unknown", 1),
            row(None, 9, "unknown", 1),
        ]);
        assert_eq!(
            unified,
            [row(Some(1), 7, "loop", 1), row(None, 9, "unknown", 1)]
        );
        let unified = unify_execution_contexts(vec![
            row(Some(1), 7, "unknown", 0),
            row(Some(1), 7, "cycle", 1),
        ]);
        assert_eq!(unified, [row(Some(1), 7, "cycle", 1)]);
    }

    #[test]
    fn rows_are_ordered_by_call_site_then_ipc_handlers() {
        let unified = unify_execution_contexts(vec![
            row(Some(5), 1, "unknown", 0),
            row(None, 4, "unknown", 1),
            row(Some(2), 3, "unknown", 0),
            row(None, 2, "unknown", 1),
        ]);
        let order: Vec<_> = unified
            .iter()
            .map(|c| (c.call_site_id, c.entry_fn_id))
            .collect();
        assert_eq!(order, [(Some(2), 3), (Some(5), 1), (None, 2), (None, 4)]);
    }
}
