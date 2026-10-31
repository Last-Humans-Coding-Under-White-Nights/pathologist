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
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use trace_db::{MultiInstance, SequencingMarker, INDEXES_V8, SCHEMA_VERSION, TABLES_V8};
mod linking;

mod identity;
use identity::Identity;

/// Call-order graph ids read from an input at a time.
const CFG_READ_CHUNK: i64 = 1024;

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
/// Each call-order event's sites, by event, in site order.
const READ_EVENT_SITES: &str =
    "SELECT event_id, call_site_id FROM cfg_event_sites ORDER BY event_id, ordinal";
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
    pub ipc_bridges_added: usize,
    pub member_calls_added: usize,
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
    /// An input analysed without `--sequencing` while another was: its
    /// functions have no call-order graph in the merged database.
    MissingSequencing,
    /// An older export cannot supply cross-repository hierarchy/IPC facts.
    MissingLinkingFacts,
}

/// A key per merged call site (by index), equal for the sites of two inputs
/// that one body compiled alike gave: the same caller, position, callee
/// spelling and directness, and the same count of such sites before it in
/// its input. Sites of one input share a key only when they are
/// alternatives of one event (`alternatives`: the sites of each event with
/// several) and spelled alike, as a merged input lists another input's
/// copy of a header body's call: they count as the one site each input
/// had, so merging a merged input again keys them as merging the inputs at
/// once does.
fn site_keys(
    sites: &[DbCallSite],
    input_sites: &[Vec<usize>],
    alternatives: &[Vec<usize>],
) -> Vec<u32> {
    type Key<'s> = (
        i64,
        i64,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        &'s str,
        i64,
    );
    fn key(cs: &DbCallSite) -> Key<'_> {
        (
            cs.caller_fn_id,
            cs.file_id,
            cs.line,
            cs.col,
            cs.expansion_file_id,
            cs.expansion_line,
            cs.expansion_col,
            cs.callee_text.as_str(),
            cs.is_direct,
        )
    }
    // Each alternative's earliest equal alternative.
    let mut first: FxHashMap<usize, usize> = FxHashMap::default();
    let root = |first: &FxHashMap<usize, usize>, mut s: usize| {
        while let Some(&p) = first.get(&s) {
            s = p;
        }
        s
    };
    for event in alternatives {
        let mut by_key: FxHashMap<Key<'_>, usize> = FxHashMap::default();
        for &site in event {
            let earlier = *by_key.entry(key(&sites[site])).or_insert(site);
            let (a, b) = (root(&first, site), root(&first, earlier));
            if a != b {
                first.insert(a.max(b), a.min(b));
            }
        }
    }
    let mut ids: FxHashMap<(Key<'_>, u32), u32> = FxHashMap::default();
    let mut keys = vec![None; sites.len()];
    for input in input_sites {
        let mut repeats: FxHashMap<Key<'_>, u32> = FxHashMap::default();
        let mut seen = FxHashSet::default();
        for &i in input {
            if !seen.insert(i) {
                continue;
            }
            let earlier = root(&first, i);
            if earlier != i {
                keys[i] = keys[earlier];
                continue;
            }
            let key = key(&sites[i]);
            let repeat = repeats.entry(key).or_default();
            let next = ids.len() as u32;
            let id = *ids.entry((key, *repeat)).or_insert(next);
            keys[i].get_or_insert(id);
            *repeat += 1;
        }
    }
    keys.into_iter()
        .map(|key| key.expect("every site has an input"))
        .collect()
}

/// Prepare each `(table, query)` an input is read with, failing with the
/// first table it lacks a column of.
fn require_reads(conn: &Connection, path: &Path, reads: &[(&str, &str)]) -> Result<()> {
    for (table, query) in reads {
        conn.prepare(query).map_err(|error| {
            anyhow::anyhow!(
                "missing trace-merge capability in {} ({table}): {error}; re-analyze the input with the current trace version",
                path.display()
            )
        })?;
    }
    Ok(())
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

impl DbFunction {
    fn source_origin(&self, file_id: i64) -> Option<FunctionOrigin> {
        (self.linkage != "internal" && self.line_start > 0).then(|| {
            (
                file_id,
                self.line_start,
                self.name.clone(),
                self.signature.clone(),
            )
        })
    }

    fn identity(&self, origin: Option<&FunctionOrigin>, path: &str, input: Identity) -> Identity {
        let Some((_, line, name, signature)) = origin else {
            return input.local("trace-merge-local-function-v1", self.id);
        };
        // Match source deduplication exactly: declarations ignore images;
        // definitions retain the original input's target ownership.
        let target = (self.is_defined != 0)
            .then_some(self.target_id)
            .flatten()
            .map(|id| input.local("trace-merge-target-v1", id));
        Identity::hash(
            "trace-merge-source-function-v1",
            &[
                path.as_bytes(),
                &line.to_be_bytes(),
                name.as_bytes(),
                signature.as_bytes(),
                &[u8::from(self.is_defined != 0)],
                target.as_ref().map_or(&[], Identity::bytes),
            ],
        )
    }
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

impl DefRecord {
    // The catalog is already partitioned by name. Unknown source positions
    // cannot establish that two definitions are copies of the same body.
    fn source_origin(&self) -> (i64, i64, &str, Option<i64>) {
        (
            self.file_id,
            self.line_start,
            &self.signature,
            (self.line_start <= 0).then_some(self.new_fn_id),
        )
    }
}

// Declarations share a source origin across images. Bodies additionally need
// their link target so their functions and outgoing calls stay separate.
type FunctionOrigin = (i64, i64, String, String);

fn resolve_definition_pool(
    pool: &[&DefRecord],
    db_idx: usize,
    resolved_resolution: String,
    report: &mut MergeReport,
) -> (Vec<i64>, String) {
    let first = pool[0];
    let same_origin = pool
        .iter()
        .all(|d| d.source_origin() == first.source_origin());
    let resolution = if same_origin {
        if pool.iter().any(|d| d.db_idx != db_idx) {
            report.cross_repo_calls_resolved += 1;
        }
        resolved_resolution
    } else {
        report.ambiguous_calls += 1;
        "ambiguous".to_string()
    };
    // One source definition can have different outgoing bindings per image.
    // Keep every copy reachable even when they share a direct resolution.
    let mut callee_ids: Vec<i64> = pool.iter().map(|d| d.new_fn_id).collect();
    callee_ids.sort_unstable();
    callee_ids.dedup();
    (callee_ids, resolution)
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
    let mut input_identities = Vec::new();
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

    // Verify all input databases and read schema versions.
    let candidate_inputs = inputs;
    let mut inputs = Vec::new();
    let mut seen_identities = FxHashSet::default();
    let mut conns = Vec::new();
    let mut input_linking = Vec::new();
    // Per input: whether it has the `execution_contexts` table.
    let mut has_contexts_table: Vec<bool> = Vec::new();
    // Per input: whether `trace analyze --sequencing` wrote it.
    let mut input_sequencing: Vec<SequencingMarker> = Vec::new();
    let mut input_has_identities = Vec::new();
    for path in &candidate_inputs {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("failed to open input database {}", path.display()))?;
        // Call-site text joins revisit dictionary pages across source files.
        // Bound each input's cache at 16 MiB rather than SQLite's 2 MiB default.
        conn.execute_batch("PRAGMA cache_size = -16384;")?;
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
        if !trace_db::SUPPORTED_SCHEMA_VERSIONS.contains(&ver) {
            bail!(
                "incompatible schema version in {}: database has version {}, expected 7 or {}; re-analyze the input with the current trace version",
                path.display(),
                ver,
                SCHEMA_VERSION
            );
        }
        // Preparing every ingestion query checks required tables and columns,
        // including empty inputs, before any output is created or replaced.
        require_reads(
            &conn,
            path,
            &[
                ("files", READ_FILES),
                ("link_targets", READ_TARGETS),
                ("target_sources", READ_TARGET_SOURCES),
                ("target_dependencies", READ_TARGET_DEPENDENCIES),
                ("functions", READ_FUNCTIONS),
                ("call_sites", READ_CALL_SITES),
                ("call_edges", READ_CALL_EDGES),
                ("diagnostics", READ_DIAGNOSTICS),
            ],
        )?;
        let has_identities = identity::has_metadata(&conn).with_context(|| {
            format!("invalid stable call-edge identities in {}", path.display())
        })?;
        let database_identity = trace_db::database_identity(&conn)
            .with_context(|| format!("invalid database identity in {}", path.display()))?;
        if !seen_identities.insert(database_identity) {
            report.warnings.push(MergeWarning {
                kind: WarningKind::DuplicateInput,
                message: format!("duplicate database identity ignored: {}", path.display()),
                symbol: None,
                file_id: None,
                line: 0,
            });
            continue;
        }
        input_identities.push(Identity::input(database_identity));
        input_has_identities.push(has_identities);
        let has_contexts = trace_db::table_exists(&conn, "execution_contexts")?;
        if has_contexts {
            require_reads(
                &conn,
                path,
                &[("execution_contexts", READ_EXECUTION_CONTEXTS)],
            )?;
        }
        has_contexts_table.push(has_contexts);
        let sequencing = SequencingMarker::read(&conn).with_context(|| {
            format!(
                "failed to read the call-sequencing marker of {}",
                path.display()
            )
        })?;
        // Read only from an input analysed with `--sequencing`, whose
        // tables must then have every column.
        if sequencing != SequencingMarker::Absent {
            require_reads(&conn, path, &trace_db::CFG_READS)?;
        }
        input_sequencing.push(sequencing);
        inputs.push(path.clone());
        let facts = trace_db::read_linking_facts(&conn)
            .with_context(|| format!("invalid linking facts in {}", path.display()))?;
        if facts.as_ref().is_none_or(|facts| !facts.complete) {
            report.warnings.push(MergeWarning {
                kind: WarningKind::MissingLinkingFacts,
                message: format!("{} lacks complete linking facts; re-analyze the original inputs for full cross-repository member/IPC reconstruction", path.display()),
                symbol: None, file_id: None, line: 0,
            });
        }
        input_linking.push(facts);
        conns.push(conn);
    }
    report.input_dbs = inputs.clone();

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
    let mut target_identities = Vec::new();
    let mut target_by_identity = FxHashMap::default();
    let mut target_remap: Vec<FxHashMap<i64, i64>> = vec![FxHashMap::default(); inputs.len()];
    let mut unified_target_sources: Vec<(i64, i64)> = Vec::new();
    let mut unified_target_dependencies: Vec<(i64, i64)> = Vec::new();

    for (db_idx, conn) in conns.iter().enumerate() {
        // Earlier stable-ID outputs only stored functions/sites. Preserve
        // their endpoint identities too, with an input-local target fallback.
        let inherited_identities = trace_db::table_exists(conn, "merge_target_identities")?
            .then(|| identity::read(conn, "merge_target_identities", "target_id"))
            .transpose()?;
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
            let target_identity = identity::inherited(inherited_identities.as_ref(), target.id)?
                .unwrap_or_else(|| {
                    input_identities[db_idx].local("trace-merge-target-v1", target.id)
                });
            if let Some(&existing_id) = target_by_identity.get(&target_identity) {
                target_remap[db_idx].insert(target.id, existing_id);
                continue;
            }
            let new_id = (unified_targets.len() + 1) as i64;
            target_remap[db_idx].insert(target.id, new_id);
            target_by_identity.insert(target_identity, new_id);
            target_identities.push(target_identity);
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
    unified_target_sources.sort_unstable();
    unified_target_sources.dedup();
    unified_target_dependencies.sort_unstable();
    unified_target_dependencies.dedup();

    // 3. Functions Ingestion, Deduplication & Classification
    let mut unified_functions: Vec<DbFunction> = Vec::new();
    let mut function_identities = Vec::new();
    let mut function_by_identity = FxHashMap::default();
    let mut fn_remap: Vec<FxHashMap<i64, i64>> = vec![FxHashMap::default(); inputs.len()];
    let mut header_def_map: FxHashMap<(Option<i64>, FunctionOrigin), i64> = FxHashMap::default();
    let mut header_decl_map: FxHashMap<FunctionOrigin, i64> = FxHashMap::default();
    // Exported definitions catalog: name -> list of definitions across repos
    let mut definitions_by_name: FxHashMap<String, Vec<DefRecord>> = FxHashMap::default();
    // Cache per-DB functions by old id for call edge lookup
    let mut per_db_functions: Vec<FxHashMap<i64, DbFunction>> =
        vec![FxHashMap::default(); inputs.len()];

    for (db_idx, conn) in conns.iter().enumerate() {
        let inherited_identities = input_has_identities[db_idx]
            .then(|| identity::read(conn, "merge_function_identities", "function_id"))
            .transpose()?;
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
            // File ownership is unified before functions: a shared header
            // owned by any input is project code in every image's copy.
            let is_dep = func
                .is_dep
                .min(unified_files[(new_file_id - 1) as usize].is_dep);
            let origin = func.source_origin(new_file_id);
            let function_identity = identity::inherited(inherited_identities.as_ref(), func.id)?
                .unwrap_or_else(|| {
                    func.identity(
                        origin.as_ref(),
                        &unified_files[(new_file_id - 1) as usize].path,
                        input_identities[db_idx],
                    )
                });

            // Reuse inherited endpoint rows before linking or edge deduplication.
            // This also covers internal and positionless functions, whose source
            // origins alone cannot identify a repeated copy of an input.
            let existing_fn_id = function_by_identity
                .get(&function_identity)
                .copied()
                .or_else(|| {
                    let key = origin.as_ref()?;
                    if func.is_defined != 0 {
                        header_def_map.get(&(new_target_id, key.clone())).copied()
                    } else {
                        header_decl_map.get(key).copied()
                    }
                });
            if let Some(existing_fn_id) = existing_fn_id {
                fn_remap[db_idx].insert(func.id, existing_fn_id);
                function_by_identity.insert(function_identity, existing_fn_id);
                let existing = &mut unified_functions[(existing_fn_id - 1) as usize];
                // A declaration shared by different images has no unique target.
                if func.is_defined == 0 && existing.target_id != new_target_id {
                    existing.target_id = None;
                }
                existing.is_dep = existing.is_dep.min(is_dep);
                if origin.is_some() {
                    report.header_functions_deduped += 1;
                }
                continue;
            }

            let new_fn_id = (unified_functions.len() + 1) as i64;
            fn_remap[db_idx].insert(func.id, new_fn_id);
            function_by_identity.insert(function_identity, new_fn_id);

            if let Some(key) = origin {
                if func.is_defined != 0 {
                    header_def_map.insert((new_target_id, key), new_fn_id);
                } else {
                    header_decl_map.insert(key, new_fn_id);
                }
            }
            function_identities.push(function_identity);

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
                is_dep,
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
            let mut seen_origins = FxHashSet::default();
            let distinct_strong_defs: Vec<&DefRecord> = strong_defs
                .iter()
                .copied()
                .filter(|d| seen_origins.insert(d.source_origin()))
                .collect();

            if strong_db_indices.len() > 1 && distinct_strong_defs.len() > 1 {
                let locations: Vec<String> = distinct_strong_defs
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
            } else if distinct_strong_defs.len() == 1 {
                let remote_weak_defs: Vec<&DefRecord> = sig_defs
                    .iter()
                    .copied()
                    .filter(|d| {
                        d.is_weak && d.target_id.is_none() && d.db_idx != strong_defs[0].db_idx
                    })
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

    drop(header_def_map);
    drop(header_decl_map);
    drop(function_by_identity);
    // 4. Call Sites Ingestion & Remapping
    let mut unified_call_sites: Vec<DbCallSite> = Vec::new();
    let mut site_identities = Vec::new();
    let mut site_by_identity = FxHashMap::default();
    let mut cs_remap: Vec<FxHashMap<i64, i64>> = vec![FxHashMap::default(); inputs.len()];
    // Per input: its sites' indices in `unified_call_sites`.
    let mut input_sites: Vec<Vec<usize>> = Vec::with_capacity(inputs.len());

    for (db_idx, conn) in conns.iter().enumerate() {
        let inherited_identities = input_has_identities[db_idx]
            .then(|| identity::read(conn, "merge_call_site_identities", "call_site_id"))
            .transpose()?;
        let mut mapped_sites = Vec::new();
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
            let site_identity = identity::inherited(inherited_identities.as_ref(), cs.id)?
                .unwrap_or_else(|| {
                    input_identities[db_idx].local("trace-merge-call-site-v1", cs.id)
                });
            if let Some(&existing_id) = site_by_identity.get(&site_identity) {
                cs_remap[db_idx].insert(cs.id, existing_id);
                mapped_sites.push((existing_id - 1) as usize);
                continue;
            }
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
            site_by_identity.insert(site_identity, new_cs_id);
            site_identities.push(site_identity);
            mapped_sites.push((new_cs_id - 1) as usize);

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
        input_sites.push(mapped_sites);
    }
    drop(site_by_identity);
    report.call_sites_total = unified_call_sites.len();

    // 5. Call Edges Ingestion & Cross-Repository Resolution
    let mut unified_call_edges: Vec<DbCallEdge> = Vec::new();
    let mut emitted: FxHashSet<(Option<i64>, i64)> = FxHashSet::default();
    // Per input edge, by its call site and callee as that input recorded
    // them: the merged functions the edge now goes to. An execution context
    // started at that site with that callee as its entry follows them (5b):
    // a callback declared in one repository and defined in another enters
    // the definition, as the edge does.
    // Only submitted execution contexts consult edge redirections. Recording
    // every ordinary edge otherwise retains a vector for almost every call.
    let mut needed_retargets = FxHashSet::default();
    for (db_idx, conn) in conns.iter().enumerate() {
        if !has_contexts_table[db_idx] {
            continue;
        }
        let mut stmt = conn.prepare(READ_EXECUTION_CONTEXTS)?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            if let (Some(site), entry) = row? {
                needed_retargets.insert((db_idx, site, entry));
            }
        }
    }
    let mut retargets: FxHashMap<(usize, i64, i64), Vec<i64>> = FxHashMap::default();
    let mut caller_bindings: FxHashMap<i64, Vec<i64>> = FxHashMap::default();
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
                // Internal declarations cannot bind outside their file. A
                // defined weak symbol in a known image has already undergone
                // that image's link selection; retain that resolved edge.
                if orig_callee.linkage == "external"
                    && (is_external_call || (is_weak_call && orig_callee.target_id.is_none()))
                {
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
                            // Strength only competes within one signature. A strong
                            // overload cannot suppress a different weak overload when
                            // the call's signature is unknown or fails exact matching.
                            let strong_signatures: FxHashSet<&str> = strong_defs
                                .iter()
                                .map(|d| d.signature.as_str())
                                .collect();
                            let pool: Vec<&DefRecord> = candidates
                                .into_iter()
                                .filter(|d| !d.is_weak || !strong_signatures.contains(d.signature.as_str()))
                                .collect();

                            resolve_definition_pool(&pool, db_idx, resolved_resolution, &mut report)
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
                            let caller_target = unified_functions[(new_caller_id - 1) as usize].target_id;
                            let same_target_strong: Vec<&DefRecord> = strong_defs
                                .iter()
                                .copied()
                                .filter(|d| caller_target.is_some() && d.target_id == caller_target)
                                .collect();
                            let pool = if same_target_strong.is_empty() {
                                strong_defs
                            } else {
                                same_target_strong
                            };

                            resolve_definition_pool(&pool, db_idx, resolved_resolution, &mut report)
                        } else if !candidates.is_empty() {
                            resolve_definition_pool(&candidates, db_idx, resolved_resolution, &mut report)
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
                let key = (db_idx, cs, edge.callee_fn_id);
                if needed_retargets.contains(&key) {
                    retargets
                        .entry(key)
                        .or_default()
                        .extend(new_callees.iter().copied());
                }
            }
            if orig_callee.is_defined == 0 {
                let declaration = fn_remap[db_idx][&edge.callee_fn_id];
                for &id in &new_callees {
                    // Include the declaration when a caller still reaches it,
                    // even if other callers bind to a definition.
                    let bindings = caller_bindings.entry(declaration).or_default();
                    if !bindings.contains(&id) {
                        bindings.push(id);
                    }
                }
            }
            for new_callee_id in new_callees {
                if new_cs_id.is_some() && !emitted.insert((new_cs_id, new_callee_id)) {
                    continue;
                }
                unified_call_edges.push(DbCallEdge {
                    id: 0, // Assigned from final endpoints after IPC rebinding.
                    call_site_id: new_cs_id,
                    caller_fn_id: new_caller_id,
                    callee_fn_id: new_callee_id,
                    resolution: new_resolution.clone(),
                });
            }
        }
    }
    // Resolution-only catalogs must not overlap reconstruction and export.
    drop(needed_retargets);
    drop(per_db_functions);
    drop(definitions_by_name);
    drop(emitted);
    let linking_facts = linking::combine(input_linking, &fn_remap, &cs_remap)?;
    let previous_edges = unified_call_edges.len();
    let previous_ipc = unified_call_edges
        .iter()
        .filter(|e| e.resolution == "ipc")
        .count();
    let new_handlers = linking::reconstruct(
        &linking_facts,
        &unified_functions,
        &unified_target_dependencies,
        &mut unified_call_edges,
    );
    report.ipc_bridges_added = unified_call_edges
        .iter()
        .filter(|e| e.resolution == "ipc")
        .count()
        - previous_ipc;
    report.member_calls_added =
        unified_call_edges.len() - previous_edges - report.ipc_bridges_added;

    // A retained summary belongs to every sender binding callers now reach,
    // including declarations that remain unresolved.
    // Reuse the exact callee redirections above rather than resolving its
    // name a second time with a different image/signature policy.
    // Ordinary edges keep their allocation; only additional bindings of a
    // retained summary require copies. Register identities in the original
    // traversal order, including those additional bindings.
    let mut additional_edges = Vec::new();
    let mut edge_identities = FxHashMap::default();
    let mut kept = 0;
    for index in 0..unified_call_edges.len() {
        let edge = &mut unified_call_edges[index];
        let original_caller = edge.caller_fn_id;
        let callers = (edge.resolution == "ipc")
            .then(|| caller_bindings.get(&original_caller))
            .flatten();
        let mut retain = false;
        for (binding, caller) in callers
            .map(Vec::as_slice)
            .unwrap_or(std::slice::from_ref(&original_caller))
            .iter()
            .enumerate()
        {
            let caller_identity = function_identities[(*caller - 1) as usize];
            let callee_identity = function_identities[(edge.callee_fn_id - 1) as usize];
            let site_identity = edge
                .call_site_id
                .map(|id| site_identities[(id - 1) as usize]);
            let identity = Identity::edge(caller_identity, callee_identity, site_identity);
            if !identity::register(&mut edge_identities, identity)? {
                continue;
            }
            if binding == 0 {
                edge.id = identity.edge_id();
                edge.caller_fn_id = *caller;
                retain = true;
            } else {
                let mut copy = edge.clone();
                copy.id = identity.edge_id();
                copy.caller_fn_id = *caller;
                additional_edges.push(copy);
            }
        }
        if retain {
            unified_call_edges.swap(kept, index);
            kept += 1;
        }
    }
    unified_call_edges.truncate(kept);
    unified_call_edges.extend(additional_edges);
    drop(edge_identities);
    drop(caller_bindings);
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
    drop(retargets);
    for entry_fn_id in new_handlers {
        carried_contexts.push(DbExecutionContext {
            kind: "ipc_handler".into(),
            entry_fn_id,
            call_site_id: None,
            api_fn_id: None,
            param_index: None,
            model: None,
            multi_instance: "unknown".into(),
            self_concurrent: 1,
        });
    }
    let unified_contexts = unify_execution_contexts(carried_contexts);

    // 5c. Call-order graphs, carried over with remapped ids. A function
    // keeps one graph per distinct body, numbered in input order, as
    // `trace analyze` keeps configuration variants (both through
    // `trace_ir::add_variant`): a body several inputs share (a header
    // function merged above) is kept once when they compile it alike and
    // once more per different graph. Distinct source copies of header calls
    // are compared with each site standing for its key (`site_keys`). An
    // event none of whose sites was carried is spliced out, leaving an
    // `omitted_sites` gap. Graphs are read a chunk of ids at a time, and a
    // kept graph is held once, beside the hash of its keyed form; neither
    // side of a comparison is copied to compare it.
    let mut unified_cfgs: BTreeMap<trace_ir::FnId, Vec<trace_ir::FunctionCfg>> = BTreeMap::new();
    let mut cfg_hashes: FxHashMap<trace_ir::FnId, Vec<u64>> = FxHashMap::default();
    let any_sequenced = input_sequencing
        .iter()
        .any(|&s| s != SequencingMarker::Absent);
    let site_key = if any_sequenced {
        // The sites of each event with several, in site order.
        let mut alternatives: Vec<Vec<usize>> = Vec::new();
        for (db_idx, conn) in conns.iter().enumerate() {
            if input_sequencing[db_idx] == SequencingMarker::Absent {
                continue;
            }
            let mut stmt = conn.prepare(READ_EVENT_SITES)?;
            let mut rows = stmt.query([])?;
            let mut event = None;
            let mut group = Vec::new();
            while let Some(r) = rows.next()? {
                let (id, site): (i64, i64) = (r.get(0)?, r.get(1)?);
                if event != Some(id) {
                    event = Some(id);
                    if group.len() > 1 {
                        alternatives.push(std::mem::take(&mut group));
                    }
                    group.clear();
                }
                if let Some(&merged) = cs_remap[db_idx].get(&site) {
                    group.push(merged as usize - 1);
                }
            }
            if group.len() > 1 {
                alternatives.push(group);
            }
        }
        site_keys(&unified_call_sites, &input_sites, &alternatives)
    } else {
        Vec::new()
    };
    // A merged site's key.
    let merged_key = |s: trace_ir::CallSiteId| trace_ir::CallSiteId(site_key[s.0 as usize - 1]);
    for (db_idx, conn) in conns.iter().enumerate() {
        if input_sequencing[db_idx] == SequencingMarker::Absent {
            if any_sequenced {
                report.warnings.push(MergeWarning {
                    kind: WarningKind::MissingSequencing,
                    message: format!(
                        "input {} was analysed without --sequencing; its functions have no call-order graph (re-analyze it with `trace analyze --sequencing` to include them)",
                        inputs[db_idx].display()
                    ),
                    symbol: None,
                    file_id: None,
                    line: 0,
                });
            }
            continue;
        }
        let files = &file_remap[db_idx];
        // Every graph's files are checked to be in `files` before it is
        // mapped.
        let map_file = |f: trace_ir::FileId| trace_ir::FileId(files[&i64::from(f.0)] as u32);
        let map_site = |s: trace_ir::CallSiteId| {
            cs_remap[db_idx]
                .get(&i64::from(s.0))
                .map(|&id| trace_ir::CallSiteId(id as u32))
        };
        let last: i64 =
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM cfg_functions", [], |r| {
                r.get(0)
            })?;
        for first in (1..=last).step_by(CFG_READ_CHUNK as usize) {
            let chunk =
                trace_db::CfgId(first)..=trace_db::CfgId(first.saturating_add(CFG_READ_CHUNK - 1));
            for stored in trace_db::read_cfgs(conn, chunk)? {
                let cfg = stored.cfg;
                if let Some(file) = cfg.files().find(|f| !files.contains_key(&i64::from(f.0))) {
                    bail!(
                        "call-order graph {} of {} names file id {}, which is not in its files table",
                        stored.id,
                        inputs[db_idx].display(),
                        file.0
                    );
                }
                let function = *fn_remap[db_idx]
                    .get(&i64::from(cfg.function.0))
                    .context("missing remapped fn_id for cfg_functions")?;
                let function = trace_ir::FnId(function as u32);
                let kept = unified_cfgs.entry(function).or_default();
                let hashes = cfg_hashes.entry(function).or_default();
                let added = trace_ir::add_variant(
                    kept,
                    Some(hashes),
                    &cfg,
                    function,
                    map_site,
                    map_file,
                    merged_key,
                );
                if let Err(at) = added {
                    // Its sites may call what the kept graph's do not (a
                    // callback this input passes): they join the kept
                    // events as alternatives. Their keys repeat the kept
                    // ones, so the keyed form and its hash are unchanged.
                    kept[at].add_alternatives_from(&cfg, map_site);
                }
            }
        }
    }

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
            WarningKind::MissingSequencing => "info",
            WarningKind::MissingLinkingFacts => "info",
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

    drop(conns);
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
        // Stable edge IDs scatter parent lookups during validation. Keep the
        // output working set cached; input connections have already closed.
        out_conn.execute_batch("PRAGMA cache_size = -65536;")?;
        out_conn.execute_batch("BEGIN IMMEDIATE;")?;
        out_conn.execute_batch(TABLES_V8)?;
        out_conn.execute_batch(identity::TABLES)?;
        let mut strings = trace_db::TextPool::new(&out_conn)?;
        if any_sequenced {
            strings.seed_sequencing()?;
        }

        // Record analysis_run for the merge
        let options_json = serde_json::json!({
            "stage": "merge",
            "merged_databases": inputs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "cross_repo_calls_resolved": report.cross_repo_calls_resolved,
            "external_calls_unresolved": report.external_calls_unresolved,
            "ambiguous_calls": report.ambiguous_calls,
            "ipc_bridges_added": report.ipc_bridges_added,
            "member_calls_added": report.member_calls_added,
            "warnings_count": report.warnings.len(),
            "call_sequencing": SequencingMarker::merged(&input_sequencing).to_json(),
            "call_edge_identity_version": 1,
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
                "INSERT INTO call_sites_data (id, caller_fn_id, file_id, line, col, expansion_file_id, expansion_line, expansion_col, callee_text_id, is_direct) \
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
                    strings.intern(&cs.callee_text)?,
                    cs.is_direct
                ])?;
            }
        }

        // Persist the original identities, never the current remapped row IDs,
        // so a merged input can be merged again under another filename.
        for (table, column, identities) in [
            ("merge_target_identities", "target_id", &target_identities),
            (
                "merge_function_identities",
                "function_id",
                &function_identities,
            ),
            (
                "merge_call_site_identities",
                "call_site_id",
                &site_identities,
            ),
        ] {
            let mut stmt = out_conn.prepare_cached(&format!(
                "INSERT INTO {table} ({column}, identity) VALUES (?1, ?2)"
            ))?;
            for (index, identity) in identities.iter().enumerate() {
                stmt.execute(params![index as i64 + 1, identity.bytes()])?;
            }
        }

        trace_db::export_cfg(&out_conn, unified_cfgs.values().flatten(), &mut strings)?;

        // Bulk insert call edges
        {
            let mut stmt = out_conn.prepare_cached(
                "INSERT INTO call_edges_data (id, call_site_id, caller_fn_id, callee_fn_id, resolution_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for edge in &unified_call_edges {
                stmt.execute(params![
                    edge.id,
                    edge.call_site_id,
                    edge.caller_fn_id,
                    edge.callee_fn_id,
                    strings.intern(&edge.resolution)?
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

        trace_db::write_linking_facts(&out_conn, &linking_facts, &mut strings)?;
        verify_foreign_keys(&out_conn)?;

        // Build secondary indexes
        trace_db::record_database_identity(&out_conn)?;
        out_conn.execute_batch(INDEXES_V8)?;
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

fn verify_foreign_keys(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let violations: Vec<String> = stmt
        .query_map([], |row| {
            // SQLite reports NULL instead of a rowid for WITHOUT ROWID tables.
            let rowid = row
                .get::<_, Option<i64>>(1)?
                .map_or_else(|| "NULL".to_owned(), |id| id.to_string());
            Ok(format!(
                "table: {}, rowid: {}, parent: {}, fkid: {}",
                row.get::<_, String>(0)?,
                rowid,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if !violations.is_empty() {
        bail!(
            "foreign key violation in merged database:\n  {}",
            violations.join("\n  ")
        );
    }
    Ok(())
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

    #[test]
    fn foreign_key_errors_report_rowids_and_without_rowid_violations() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute_batch(TABLES_V8).unwrap();
        verify_foreign_keys(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO functions(id,name,file_id,line_start,line_end,linkage,signature,is_defined)
             VALUES(4,'f',999,1,1,'external','f()',1);
             INSERT INTO target_sources VALUES(2,7);",
        ).unwrap();
        let error = verify_foreign_keys(&conn).unwrap_err().to_string();
        assert!(
            error.starts_with("foreign key violation in merged database:"),
            "{error}"
        );
        assert!(
            error.contains("table: functions, rowid: 4, parent: files"),
            "{error}"
        );
        assert!(
            error.contains("table: target_sources, rowid: NULL, parent: files"),
            "{error}"
        );
        assert!(
            error.contains("table: target_sources, rowid: NULL, parent: link_targets"),
            "{error}"
        );
    }

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
