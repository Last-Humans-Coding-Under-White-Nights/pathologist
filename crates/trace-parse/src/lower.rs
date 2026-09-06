use crate::deps::IncludeGraph;
use crate::discover::discover_source_files;
use crate::index_cache::IndexSourceCache;
use crate::merge::{merge_unit_index, merge_unit_symbols, merge_unit_types, UnitIndex};
use crate::parse::node_text;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use trace_ir::{
    CallSite, Diagnostic, DiagnosticSeverity, FieldId, FlowConstraint, FnId, Function, Linkage,
    Program, ReturnFlow, ScalarKind, Span, StorageClass, TypeDesc, VarId, Variable,
};
use trace_preproc::{macro_table_from_defines, Language, MacroTable, PreprocessOptions};
use tree_sitter::Node;

/// Nested AST walk cap. Pathological left-deep trees (comma-operator
/// chains of thousands of terms) would otherwise overflow the thread stack.
const MAX_AST_WALK_DEPTH: u32 = 512;
const INDEX_PROGRESS_EVERY: usize = 50;
const PREPROCESS_STAGE: &str = "preprocess";

fn index_progress(msg: impl std::fmt::Display) {
    let _ = writeln!(std::io::stderr(), "{msg}");
    let _ = std::io::stderr().flush();
}

fn index_item_progress(i: usize, n: usize, msg: impl std::fmt::Display) {
    if std::env::var_os("TRACE_INDEX_VERBOSE").is_some()
        || i == 0
        || i + 1 == n
        || (i + 1).is_multiple_of(INDEX_PROGRESS_EVERY)
    {
        index_progress(msg);
    }
}

/// A function-name reference whose resolution was deferred because the
/// function is only defined later in the translation unit. C requires no
/// forward declaration for these uses when the definition appears later
/// in the same file, so lowering must not depend on encounter order.
enum PendingFnRef {
    /// `base.field = FnName`: emit `AddrOfFn` into a temp and `Store` it
    /// into the already-materialized field address `dst`.
    FieldStore {
        dst: VarId,
        name: String,
        span: Span,
    },
    /// `dst = FnName` in an initializer/assignment RHS.
    RhsIdent { dst: VarId, name: String },
    /// `dst = &FnName`.
    AddrOfIdent { dst: VarId, name: String },
    /// `return FnName;` from function `owner`.
    ReturnIdent { owner: FnId, name: String },
    /// `return &FnName;` from function `owner`.
    ReturnAddrOf { owner: FnId, name: String },
}

struct LowerContext {
    current_fn: Option<FnId>,
    current_file: trace_ir::FileId,
    locals: HashMap<String, VarId>,
    /// Origin map for the preprocessed text being lowered. Lookup failures
    /// fall back to TU-anchored spans.
    line_map: Option<std::sync::Arc<trace_preproc::LineMap>>,
    primary_path: PathBuf,
    /// Function references deferred to end-of-unit resolution.
    /// `RefCell` keeps `&LowerContext` receivers usable in expression
    /// helpers while still allowing deferred entries to be recorded.
    pending: RefCell<Vec<PendingFnRef>>,
    /// C++ namespace stack; `None` = anonymous namespace level.
    ns_stack: Vec<Option<String>>,
    /// Namespaces made visible by `using namespace X;` (bare spellings).
    using_nss: Vec<String>,
    /// Specific members imported by `using X::member;` as `(base, qual)`.
    /// A bare `member()` call then resolves to the exact `qual` entry.
    using_name_imports: Vec<(String, String)>,
    /// Enclosing class while lowering in-class member definitions.
    class_ctx: Option<ClassCtx>,
    /// Gates C++-specific lowering (qualified members, CHA, namespaces).
    /// True for C++ TUs/headers and for `.h` files reached from a C++ TU.
    is_cpp: bool,
    /// `new_expression` node IDs already handled by `expr_to_rhs_flow` so
    /// `walk_function_body` skips them (avoids duplicate call sites with
    /// incorrect `this`-parameter wiring).
    handled_new_exprs: RefCell<std::collections::HashSet<usize>>,
    /// Cache for `resolve_callee_with_loads`: maps the `func` node id of a
    /// field-expression callee to the load variable created for the fn-ptr
    /// load.  Without this, `emit_field_value_store` (via `resolve_callee_var`)
    /// and the later `collect_call_at_node` (via `resolve_callee_with_loads`)
    /// would create *two different* load variables for the same expression,
    /// breaking the `CallReturnIndirect` → `indirect_return_dst` mapping.
    callee_load_cache: RefCell<HashMap<usize, Option<VarId>>>,
    /// `call_expression` node id → `CallReturn` destination, so the matching
    /// `CallSite` can carry `return_dst` for `dlsym` models.
    call_return_dst: RefCell<HashMap<usize, VarId>>,
    /// Recursion depth of `lower_tree` (comma-operator chains in
    /// `clang/test/Sema/deep_recursion.c` are thousands of nested
    /// `binary_expression` nodes).
    ast_depth: u32,
    ast_depth_warned: bool,
}

/// C++ class scope during member lowering.
#[derive(Clone)]
struct ClassCtx {
    /// Fully qualified class name (`ns::Cls`) — matches function/type names.
    qual_name: String,
}

impl LowerContext {
    fn namespace_scope(&self) -> String {
        self.ns_stack
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
            .join("::")
    }

    fn qualify(&self, name: &str) -> String {
        let mut parts: Vec<String> = self.ns_stack.iter().flatten().cloned().collect();
        parts.push(name.to_string());
        parts.join("::")
    }

    /// Qualify a declared function name with the enclosing class / namespace:
    /// in-class definitions get the class prefix; free functions get the
    /// namespace prefix (no-op for C — empty stack). Out-of-class member
    /// spellings (`Shape::area`, `Cls::~Cls`) may omit the namespace: prefix
    /// it unless the name already starts with one of the enclosing
    /// namespaces (fully qualified spelling). Shared by definitions and
    /// prototypes so header-parsed declarations register the same qualified
    /// name a later out-of-line definition does.
    fn qualify_decl(&self, raw_name: &str) -> String {
        canonicalize_conversion_target(&self.qualify_decl_spelling(raw_name))
    }

    fn qualify_decl_spelling(&self, raw_name: &str) -> String {
        let normalized_raw = normalize_declared_name(raw_name);
        // A leading `::` (explicit global qualification such as
        // `::qualified_global`) means global scope: the enclosing namespace
        // prefix must NOT be prepended.  We strip the `::` to keep one
        // spelling everywhere (merges work, `functions_in_namespace` needs
        // only one comparison) but return early before the namespace-qualify
        // branches run.
        let is_global_qualified = normalized_raw.starts_with("::");
        let normalized_raw = normalized_raw.strip_prefix("::").unwrap_or(&normalized_raw);
        if is_global_qualified {
            return normalized_raw.to_string();
        }
        // Only a `::` *before* the name proper is a scope: a conversion
        // operator's target type carries its own (`operator ns::S`,
        // `operator std::string`) and qualifies nothing.
        let is_scope_qualified = scope_part(normalized_raw).contains("::");
        if let Some(cls) = &self.class_ctx {
            if is_scope_qualified {
                normalized_raw.to_string()
            } else {
                format!("{}::{}", cls.qual_name, normalized_raw)
            }
        } else if is_scope_qualified {
            let first_seg = normalized_raw.split("::").next().unwrap_or("");
            let already_qualified =
                self.ns_stack.iter().flatten().any(|ns| ns == first_seg) || !self.is_cpp;
            if already_qualified {
                normalized_raw.to_string()
            } else {
                self.qualify(normalized_raw)
            }
        } else {
            self.qualify(raw_name)
        }
    }

    fn in_anonymous_namespace(&self) -> bool {
        self.ns_stack.iter().any(|level| level.is_none())
    }
}

fn register_local(ctx: &mut LowerContext, name: String, id: VarId) {
    if ctx.current_fn.is_some() {
        ctx.locals.insert(name, id);
    }
}

pub fn build_program(root: &Path, opts: &PreprocessOptions) -> Result<Program, String> {
    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1);
    build_program_with_jobs(root, opts, jobs)
}

pub fn build_program_with_jobs(
    root: &Path,
    opts: &PreprocessOptions,
    jobs: usize,
) -> Result<Program, String> {
    let jobs = jobs.max(1);
    let mut program = Program::new(root.to_path_buf());
    program.include_paths = opts.include_paths.clone();
    program.defines = opts
        .defines
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let (files, headers) = discover_source_files(root);
    let files = normalize_discovered_paths(files);
    let headers = normalize_discovered_paths(headers);
    index_progress(format!(
        "discover: {} TUs, {} headers under {}",
        files.len(),
        headers.len(),
        root.display()
    ));
    if files.is_empty() && headers.is_empty() {
        return Err(format!(
            "no C/C++ source files found under {}",
            root.display()
        ));
    }

    let mut include_graph = IncludeGraph::build(root, &files, &headers);
    index_progress(format!(
        "include-graph: {} files, {} include edges",
        include_graph.project_files.len(),
        include_graph.edges.values().map(|v| v.len()).sum::<usize>()
    ));
    let file_order = include_graph.index_order(&files);

    let basename_index = Arc::new(include_graph.basename_index.clone());
    let include_expansion_cache = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let eff_opts = project_preprocess_opts(root, opts, &include_graph)
        .for_indexing()
        .with_include_expansion_cache(Arc::clone(&include_expansion_cache))
        .with_basename_index(basename_index)
        .with_inline_include_bodies(false);

    // Warm each header under a FRESH macro environment seeded only from the
    // command-line defines. Sharing one accumulating table across headers let
    // include guards defined by earlier-warmed headers starve later headers'
    // expansions: the starved (empty) text was frozen into the expansion cache
    // and replayed to translation units, silently dropping every declaration
    // behind those guards (verified FN class on real corpora). Dedup comes
    // from the shared expansion cache instead; per-header tables only prevent
    // cross-header guard leakage.
    //
    // Translation units still inherit a single table — the union of all
    // headers' final macro states — because cached expansions are replayed
    // without executing their #define directives, so TU-local code needs the
    // macros those headers define.
    //
    // One union per language: a `-D` body and a header's replacement lists
    // are token sequences, and the C and C++ lexers disagree on raw strings
    // and ud-suffixes, so a TU inherits the table lexed its own way. Both
    // unions still hold EVERY warmed header's macros (same-language warm
    // preferred, the other language's re-lexed for the destination as
    // fallback): a header not reached from a unit of that language is rare
    // in one but not the other, and the union's job — twin-guard dedup and
    // a macro superset for orphan and PCH headers — must not depend on
    // which language reached it.
    let union_macros: HashMap<Language, Arc<std::sync::RwLock<MacroTable>>> =
        [Language::C, Language::Cpp]
            .into_iter()
            .map(|l| {
                let table = macro_table_from_defines(&opts.defines, l);
                (l, Arc::new(std::sync::RwLock::new(table)))
            })
            .collect();
    let project_headers: Vec<PathBuf> = include_graph
        .project_files
        .iter()
        .filter(|p| is_index_header(p))
        .cloned()
        .collect();
    let c_sources: HashSet<PathBuf> = files.iter().cloned().collect();

    // `.h` is language-ambiguous. Parse it as C++ when a C++ TU can reach it
    // (the pre-PCH behavior: header tokens were spliced into that TU).
    // `.hpp`/`.hh`/… are always C++ via `Language::from_path`.
    let cpp_tus: HashSet<PathBuf> = files
        .iter()
        .filter(|p| crate::discover::is_cpp_path(p))
        .cloned()
        .collect();
    let c_tus: HashSet<PathBuf> = files
        .iter()
        .filter(|p| !cpp_tus.contains(*p))
        .cloned()
        .collect();
    let forced_language = eff_opts.language;

    // A header is lexed as the unit including it, and the expansion cache
    // is keyed by (path, language), so a header reached from both C and C++
    // units is warmed once per language: each unit then replays the
    // tokenization its own lexer would produce (raw strings and
    // ud-suffixes are one token in C++, identifier + literal in C). The
    // first language listed is the one the header is parsed as (C++ when a
    // C++ TU can reach it, as `index_language` decides) and feeds the source
    // cache; any other only fills the expansion cache and its union table.
    // An explicit `PreprocessOptions::with_language` lexes everything as
    // that language.
    //
    // Reachability is decided on the include graph, and warming grows that
    // graph: a `#include MACRO` the raw scanner cannot see is discovered
    // only while preprocessing the header spelling it. A `.h` first reached
    // from C alone may turn out to be reachable from a C++ unit through
    // such an edge, and it is then parsed as C++ — so its cached text must
    // be the C++ preprocess, not the C one. The pass therefore runs to a
    // fixed point: after each round the discovered edges are added, every
    // header's language list is recomputed, and any header whose list
    // changed (or that only became reachable) is evicted and warmed again
    // under a fresh table. Rounds beyond the first touch only reclassified
    // headers, and the graph only grows, so this terminates.
    let source_cache = IndexSourceCache::new();
    let mut warmed_as: HashMap<PathBuf, Vec<Language>> = HashMap::new();
    // What a header's second-language warm run reported. The cached
    // (first-language) run reaches the export through the header's own
    // unit; this run's text is discarded, so its diagnostics would go with
    // it. Keyed by header so an evicted, re-warmed header replaces (or
    // drops) its entry; ordered so the rows come out in a fixed order.
    let mut second_language_diagnostics: BTreeMap<PathBuf, Vec<trace_preproc::Diagnostic>> =
        BTreeMap::new();
    let mut round = 0usize;
    loop {
        for (from, headers) in source_cache.included_by_file() {
            include_graph.add_preprocess_includes(&from, &headers);
        }
        let reachable_from_c = include_graph.reachable_from(&c_sources);
        let reachable_from_cpp = include_graph.reachable_from(&cpp_tus);
        let reachable_from_c_units = include_graph.reachable_from(&c_tus);
        let warm_languages = |path: &PathBuf| -> Vec<Language> {
            if let Some(l) = forced_language {
                return vec![l];
            }
            let mut langs = Vec::new();
            if crate::discover::is_cpp_header_path(path) || reachable_from_cpp.contains(path) {
                langs.push(Language::Cpp);
            }
            if reachable_from_c_units.contains(path) {
                langs.push(Language::C);
            }
            if langs.is_empty() {
                langs.push(Language::from_path(path));
            }
            langs
        };
        let headers_for_macro_warm: Vec<(PathBuf, Vec<Language>)> = include_graph
            .index_order(
                &project_headers
                    .iter()
                    .filter(|p| reachable_from_c.contains(*p))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .map(|p| {
                let langs = warm_languages(&p);
                (p, langs)
            })
            .filter(|(p, langs)| warmed_as.get(p) != Some(langs))
            .collect();
        if headers_for_macro_warm.is_empty() {
            break;
        }
        round += 1;
        let warm_n = headers_for_macro_warm.len();
        index_progress(format!(
            "warm[{round}]: {warm_n} reachable headers (jobs={jobs} after this sequential pass)"
        ));
        for (i, (path, languages)) in headers_for_macro_warm.iter().enumerate() {
            let t = Instant::now();
            index_item_progress(
                i,
                warm_n,
                format!("warm: {}/{} {}", i + 1, warm_n, path.display()),
            );
            if warmed_as.insert(path.clone(), languages.clone()).is_some() {
                source_cache.evict(path, &include_graph);
                second_language_diagnostics.remove(path);
            }
            let mut failed = false;
            let mut warmed: Vec<(Language, Arc<std::sync::RwLock<MacroTable>>)> = Vec::new();
            for (k, language) in languages.iter().copied().enumerate() {
                let header_macros: Arc<std::sync::RwLock<MacroTable>> = Arc::new(
                    std::sync::RwLock::new(macro_table_from_defines(&opts.defines, language)),
                );
                let header_prep_opts = eff_opts
                    .clone()
                    .with_shared_macros(Arc::clone(&header_macros))
                    .with_accumulate_macros(true)
                    .with_language(language);
                let result = if k == 0 {
                    source_cache
                        .get_or_preprocess(path, &include_graph, &header_prep_opts)
                        .map(|_| ())
                } else {
                    source_cache
                        .preprocess_uncached(path, &include_graph, &header_prep_opts)
                        .map(|src| {
                            second_language_diagnostics.insert(path.clone(), src.diagnostics);
                        })
                };
                if let Err(e) = result {
                    program.add_diagnostic(Diagnostic {
                        severity: DiagnosticSeverity::Warning,
                        file: None,
                        line: 0,
                        message: format!(
                            "macro warm preprocess failed for {}: {e}",
                            path.display()
                        ),
                        stage: PREPROCESS_STAGE.into(),
                    });
                    failed = true;
                    break;
                }
                warmed.push((language, header_macros));
            }
            if failed {
                continue;
            }
            for (language, union) in &union_macros {
                let Some((from, done)) = warmed
                    .iter()
                    .find(|(l, _)| l == language)
                    .or_else(|| warmed.first())
                else {
                    continue;
                };
                if let (Ok(mut union), Ok(done)) = (union.write(), done.read()) {
                    for (name, def) in done.iter() {
                        let def = if from == language {
                            def.clone()
                        } else {
                            def.relexed(*language)
                        };
                        union.insert(name.clone(), def);
                    }
                }
            }
            index_item_progress(
                i,
                warm_n,
                format!(
                    "warm-done: {}/{} {:.1}s",
                    i + 1,
                    warm_n,
                    t.elapsed().as_secs_f64()
                ),
            );
        }
    }

    // Macro includes discovered while warming can make a previously "orphan"
    // header reachable from a `.c`. Those must be PCH'd into `header_ir` so
    // TUs can merge their prototypes; they must not stay on the orphan path
    // (merged into the global program only, invisible at TU lower time).
    let reachable_from_c = include_graph.reachable_from(&c_sources);
    let cpp_parse: Arc<HashSet<PathBuf>> = Arc::new(include_graph.reachable_from(&cpp_tus));
    let mut pch_headers: Vec<PathBuf> = project_headers
        .iter()
        .filter(|p| reachable_from_c.contains(*p))
        .cloned()
        .collect();
    for (_, hs) in source_cache.included_by_file() {
        pch_headers.extend(
            hs.into_iter()
                .filter(|h| include_graph.project_files.contains(h) && is_index_header(h)),
        );
    }
    pch_headers.sort();
    pch_headers.dedup();
    let pch_order = Arc::new(include_graph.index_order(&pch_headers));
    let pch_set: HashSet<PathBuf> = pch_headers.iter().cloned().collect();
    let orphan_headers: Vec<PathBuf> = include_graph.index_order(
        &project_headers
            .iter()
            .filter(|p| !pch_set.contains(*p))
            .cloned()
            .collect::<Vec<_>>(),
    );

    // Parallel phases must treat the expansion cache as read-only: warm-pass
    // entries were produced sequentially and deterministically, while worker
    // inserts are first-writer-wins races that make output scheduling-
    // dependent. Misses expand inline under each TU's own macro/guard state.
    // One option set per language; each file takes the one matching the
    // language it is lexed and parsed as (`index_language`).
    let index_opts: HashMap<Language, PreprocessOptions> = union_macros
        .iter()
        .map(|(l, table)| {
            let o = eff_opts
                .clone()
                .with_shared_macros(Arc::clone(table))
                .with_frozen_expansion_cache(true)
                .with_language(*l);
            (*l, o)
        })
        .collect();

    index_progress(format!(
        "parse: {} orphan headers, {} TUs (jobs={jobs})",
        orphan_headers.len(),
        file_order.len()
    ));

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())?;

    let pch_t = Instant::now();
    index_progress(format!("pch: parse {} headers once", pch_headers.len()));
    // Include-graph order (included files before includers), including
    // preprocess-only edges so a header is never PCH'd in the same wave as a
    // nested type the raw `#include` scanner missed. Nested merge copies
    // types/typedefs only; TUs pull prototypes from every reachable header.
    // Cyclic leftovers are indexed in `index_order`, never as a parallel wave.
    let mut header_ir_map: HashMap<PathBuf, Arc<UnitIndex>> = HashMap::new();
    let (pch_waves, pch_cycles) = if jobs == 1 {
        (vec![pch_order.as_ref().clone()], Vec::new())
    } else {
        include_graph.index_waves(&pch_headers)
    };
    for wave in pch_waves {
        if wave.is_empty() {
            continue;
        }
        if jobs == 1 || wave.len() == 1 {
            for path in &wave {
                let unit = index_source_file(
                    path,
                    root,
                    &include_graph,
                    &index_opts[&index_language(path, &cpp_parse, forced_language)],
                    &source_cache,
                    Some(&header_ir_map),
                    pch_order.as_ref(),
                );
                header_ir_map.insert(include_graph.intern_path(path), Arc::new(unit));
            }
        } else {
            let snapshot = header_ir_map.clone();
            let units: Vec<(PathBuf, UnitIndex)> = pool.install(|| {
                wave.par_iter()
                    .map(|path| {
                        (
                            path.clone(),
                            index_source_file(
                                path,
                                root,
                                &include_graph,
                                &index_opts[&index_language(path, &cpp_parse, forced_language)],
                                &source_cache,
                                Some(&snapshot),
                                pch_order.as_ref(),
                            ),
                        )
                    })
                    .collect()
            });
            for (path, unit) in units {
                header_ir_map.insert(include_graph.intern_path(&path), Arc::new(unit));
            }
        }
    }
    for path in include_graph.index_order(&pch_cycles) {
        let unit = index_source_file(
            &path,
            root,
            &include_graph,
            &index_opts[&index_language(&path, &cpp_parse, forced_language)],
            &source_cache,
            Some(&header_ir_map),
            pch_order.as_ref(),
        );
        header_ir_map.insert(include_graph.intern_path(&path), Arc::new(unit));
    }
    let header_ir = Arc::new(header_ir_map);
    index_progress(format!(
        "pch-done: {:.1}s ({} units)",
        pch_t.elapsed().as_secs_f64(),
        header_ir.len()
    ));
    for path in include_graph.index_order(&header_ir.keys().cloned().collect::<Vec<_>>()) {
        if let Some(unit) = header_ir.get(&path) {
            merge_unit_index(&mut program, unit.as_ref());
        }
    }
    program.types.complete_nested_tags();

    pool.install(|| {
        if jobs == 1 {
            for (i, path) in orphan_headers.iter().enumerate() {
                let t = Instant::now();
                index_item_progress(
                    i,
                    orphan_headers.len(),
                    format!(
                        "parse-orphan: {}/{} {}",
                        i + 1,
                        orphan_headers.len(),
                        path.display()
                    ),
                );
                merge_unit_index(
                    &mut program,
                    &index_source_file(
                        path,
                        root,
                        &include_graph,
                        &index_opts[&index_language(path, &cpp_parse, forced_language)],
                        &source_cache,
                        Some(&header_ir),
                        pch_order.as_ref(),
                    ),
                );
                index_item_progress(
                    i,
                    orphan_headers.len(),
                    format!(
                        "parse-orphan-done: {}/{} {:.1}s",
                        i + 1,
                        orphan_headers.len(),
                        t.elapsed().as_secs_f64()
                    ),
                );
            }
        } else {
            let mut header_units: HashMap<PathBuf, UnitIndex> = orphan_headers
                .par_iter()
                .map(|path| {
                    (
                        path.clone(),
                        index_source_file(
                            path,
                            root,
                            &include_graph,
                            &index_opts[&index_language(path, &cpp_parse, forced_language)],
                            &source_cache,
                            Some(&header_ir),
                            pch_order.as_ref(),
                        ),
                    )
                })
                .collect();
            for path in &orphan_headers {
                if let Some(unit) = header_units.remove(path) {
                    merge_unit_index(&mut program, &unit);
                }
            }
        }
    });

    pool.install(|| {
        if jobs == 1 {
            for (i, path) in file_order.iter().enumerate() {
                let t = Instant::now();
                index_item_progress(
                    i,
                    file_order.len(),
                    format!("parse: {}/{} {}", i + 1, file_order.len(), path.display()),
                );
                merge_unit_index(
                    &mut program,
                    &index_source_file(
                        path,
                        root,
                        &include_graph,
                        &index_opts[&index_language(path, &cpp_parse, forced_language)],
                        &source_cache,
                        Some(&header_ir),
                        pch_order.as_ref(),
                    ),
                );
                index_item_progress(
                    i,
                    file_order.len(),
                    format!(
                        "parse-done: {}/{} {:.1}s",
                        i + 1,
                        file_order.len(),
                        t.elapsed().as_secs_f64()
                    ),
                );
            }
        } else {
            let mut units: HashMap<PathBuf, UnitIndex> = file_order
                .par_iter()
                .map(|path| {
                    (
                        path.clone(),
                        index_source_file(
                            path,
                            root,
                            &include_graph,
                            &index_opts[&index_language(path, &cpp_parse, forced_language)],
                            &source_cache,
                            Some(&header_ir),
                            pch_order.as_ref(),
                        ),
                    )
                })
                .collect();
            for path in &file_order {
                if let Some(unit) = units.remove(path) {
                    merge_unit_index(&mut program, &unit);
                }
            }
        }
    });

    program.types.complete_nested_tags();
    // After the merges, so the headers involved already hold their ids
    // and forwarding does not reorder the file table.
    for (path, diagnostics) in second_language_diagnostics {
        let unit_file = program
            .symbols
            .add_file_interned(include_graph.intern_path(&path));
        add_preprocess_diagnostics(&mut program, &include_graph, unit_file, &diagnostics);
    }
    program.include_deps = include_graph.edge_list();
    for dir in &include_graph.include_dirs {
        if !program.include_paths.iter().any(|p| p == dir) {
            program.include_paths.push(dir.clone());
        }
    }

    finalize_extern_callees(&mut program);
    expand_virtual_overrides(&mut program);

    Ok(program)
}

/// Classify plain-identifier calls that resolve to no tree-local symbol
/// (libc without tree headers, logging backends referenced only inside
/// macros, vendor externs). Each unique name becomes one synthesized
/// `Function` row with `is_defined: false`, and its call sites are marked
/// resolved-to-external so the solver emits `External` edges instead of
/// counting them as unresolved indirect noise. Synthesized entries are
/// pushed directly (not via `add_function`) so they stay out of the name
/// resolution maps and cannot shadow real definitions or feed param wiring.
fn finalize_extern_callees(program: &mut Program) {
    let mut names: Vec<(String, trace_ir::FileId, u32)> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| {
            !cs.is_direct
                && cs.callee_var.is_none()
                && cs.callee_fn_id.is_none()
                && is_synthesizable_extern(&cs.callee_name)
        })
        .map(|cs| (cs.callee_name.clone(), cs.span.file, cs.span.line))
        .collect();
    names.sort();
    names.dedup_by(|a, b| a.0 == b.0);
    for (name, file, line) in names {
        // A symbol already exists for this name (in-tree prototype or
        // definition): leave the site untouched so the solver's name-based
        // recovery classifies it — defined-elsewhere resolves to a real
        // Direct edge with param wiring; prototype-only becomes External.
        // Synthesizing over it would orphan the real definition.
        if program.symbols.resolve_function(&name).is_some() {
            continue;
        }
        let fid = program.symbols.alloc_fn_id();
        program.symbols.push_synthetic_function(trace_ir::Function {
            id: fid,
            name: name.clone(),
            linkage: trace_ir::Linkage::External,
            return_type: trace_ir::TypeId(0),
            params: Vec::new(),
            locals: Vec::new(),
            is_cpp: false,
            span: trace_ir::Span { file, line, col: 0 },
            end_line: line,
            file,
            is_defined: false,
            param_type_ids: Vec::new(),
            is_virtual: false,
            is_final: false,
        });
        for cs in program.symbols.call_sites.iter_mut() {
            if !cs.is_direct && cs.callee_var.is_none() && cs.callee_name == name {
                cs.is_direct = true;
                cs.callee_fn_id = Some(fid);
            }
        }
    }
}

/// A call target that is syntactically a bare identifier — no field access,
/// subscript, cast noise, or macro-artifact punctuation.
fn is_plain_ident(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Names that should become synthesized `external` callees instead of
/// unresolved-indirect noise: C identifiers, plus C++ qualified names
/// (`std::string::c_str`, `FileUtil::Exists`) that are not field/arrow
/// expressions. Arrow text (`p->method`) stays indirect so fn-ptr and
/// callable-field sites can still be resolved by the solver.
fn is_synthesizable_extern(name: &str) -> bool {
    // The space rejected here is the one a field/arrow spelling or a stray
    // declarator fragment carries. An operator name has one of its own
    // (`operator new`, `operator ns::S`) and is a perfectly good callee, so
    // it is exempt — without this, `::operator new(n)` stopped synthesizing
    // its external and lost the call edge to it.
    let spurious_space = name.contains(' ') && scope_part(name).len() == name.len();
    is_plain_ident(name)
        || (name.contains("::")
            && !name.contains("->")
            && !name.contains('.')
            && !spurious_space
            && !name.contains('('))
}

/// After all TUs are merged, expand virtual (and destructor) call sites
/// across the full subclass closure. Lowering-time CHA only sees classes
/// already parsed in that TU, so `Base::go` calling `hook()` before
/// `Derived` is declared — or `Plugin::OnEvent` overrides in other TUs —
/// would otherwise keep a single target.
fn expand_virtual_overrides(program: &mut Program) {
    let mut seen: std::collections::HashSet<(FnId, u32, u32, FnId)> = program
        .symbols
        .call_sites
        .iter()
        .filter_map(|cs| {
            cs.callee_fn_id
                .map(|c| (cs.caller, cs.span.line, cs.span.col, c))
        })
        .collect();
    let snapshot = program.symbols.call_sites.clone();
    for cs in snapshot {
        let Some(fid) = cs.callee_fn_id else {
            continue;
        };
        let f = program.symbols.function(fid);
        if !f.is_cpp {
            continue;
        }
        let Some((cls, kind)) = method_kind_of_function(&f.name) else {
            continue;
        };
        if matches!(kind, trace_ir::MethodKind::Ctor) {
            continue;
        }
        let own = program.symbols.functions_named(&kind.name_on(&cls));
        let virtual_dispatch = kind.is_destructor()
            || f.is_virtual
            || own.iter().any(|t| program.symbols.function(*t).is_virtual);
        if !virtual_dispatch {
            continue;
        }
        let root = cs.receiver_class.as_deref().unwrap_or(&cls);
        let expected_arity = method_explicit_arity(program, fid);
        for t in program.method_targets(root, &kind) {
            if !arity_compatible(expected_arity, method_explicit_arity(program, t)) {
                continue;
            }
            let key = (cs.caller, cs.span.line, cs.span.col, t);
            if !seen.insert(key) {
                continue;
            }
            let call_id = program.symbols.alloc_call_id();
            let name = program.symbols.function(t).name.clone();
            program.symbols.call_sites.push(CallSite {
                id: call_id,
                caller: cs.caller,
                callee_name: name,
                callee_var: None,
                callee_fn_id: Some(t),
                var_args: cs.var_args.clone(),
                fn_args: cs.fn_args.clone(),
                addr_of_member_args: cs.addr_of_member_args.clone(),
                span: cs.span,
                is_direct: true,
                receiver_class: cs.receiver_class.clone(),
                return_dst: cs.return_dst,
            });
        }
    }
}

fn method_kind_of_function(name: &str) -> Option<(String, trace_ir::MethodKind)> {
    let segs: Vec<&str> = name.split("::").collect();
    if segs.len() < 2 {
        return None;
    }
    let short = *segs.last()?;
    let cls = segs[..segs.len() - 1].join("::");
    let last_cls = last_segment_of(&cls);
    let kind = if short.starts_with('~') {
        trace_ir::MethodKind::Dtor
    } else if short == last_cls {
        trace_ir::MethodKind::Ctor
    } else {
        trace_ir::MethodKind::Named(short.to_string())
    };
    Some((cls, kind))
}

fn normalize_discovered_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|p| trace_ir::canonicalize(&p))
        .collect()
}

fn project_preprocess_opts(
    root: &Path,
    opts: &PreprocessOptions,
    graph: &IncludeGraph,
) -> PreprocessOptions {
    let mut eff = opts.clone();
    for dir in &graph.include_dirs {
        if !eff.include_paths.iter().any(|p| p == dir) {
            eff.include_paths.push(dir.clone());
        }
    }
    if eff.source_cache.is_none() && !graph.source_cache.is_empty() {
        eff.source_cache = Some(Arc::new(graph.source_cache.clone()));
    }
    let _ = root;
    eff
}

fn is_index_header(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        crate::discover::HEADER_EXTENSIONS
            .iter()
            .any(|h| h.eq_ignore_ascii_case(e))
    })
}

/// Headers whose IR must be visible before lowering `self_canon`.
///
/// PCH (`types_only`): direct include-graph edges plus this file's preprocess
/// `included_headers`. Child units already nested-merged grandchild types, so
/// walking the full reachable DAG only repeats `merge_types`.
///
/// TUs: graph reachability so nested prototypes stay available even when a
/// cached splice omits them from `included_headers` (direct-only dropped
/// `DispatchToMessage` from `hdf_wifi_core.c` via `sidecar.h`).
///
/// Order follows `pch_order` (global include topo) rather than a per-file
/// `index_order`, which would redo Kahn's algorithm for every TU.
fn headers_to_merge<'a>(
    graph: &'a IncludeGraph,
    pch_order: &'a [PathBuf],
    self_canon: &'a Path,
    included_headers: &'a [PathBuf],
    types_only: bool,
) -> Vec<&'a Path> {
    let mut wanted: HashSet<&Path> = HashSet::new();
    if types_only {
        if let Some(edges) = graph.edges.get(self_canon) {
            for h in edges {
                if h.as_path() != self_canon && is_index_header(h) {
                    wanted.insert(h.as_path());
                }
            }
        }
    } else {
        for p in graph.reachable_paths(self_canon) {
            if p != self_canon && is_index_header(p) {
                wanted.insert(p);
            }
        }
    }
    for h in included_headers {
        if h.as_path() != self_canon && is_index_header(h) {
            wanted.insert(h.as_path());
        }
    }
    let mut out = Vec::with_capacity(wanted.len());
    for h in pch_order {
        if wanted.remove(h.as_path()) {
            out.push(h.as_path());
        }
    }
    let mut rest: Vec<&Path> = wanted.into_iter().collect();
    rest.sort();
    out.extend(rest);
    out
}

/// The one language decision for a file during indexing: it is lexed,
/// preprocessed AND parsed (tree-sitter grammar, C++ lowering rules) as
/// this. An explicit `PreprocessOptions::with_language` wins; otherwise a
/// file reachable from a C++ TU is C++ (a `.h` included from `.cpp`), and
/// anything else follows its extension.
fn index_language(path: &Path, cpp_parse: &HashSet<PathBuf>, forced: Option<Language>) -> Language {
    forced.unwrap_or_else(|| {
        if cpp_parse.contains(path) {
            Language::Cpp
        } else {
            Language::from_path(path)
        }
    })
}

fn source_lang(language: Language) -> crate::parse::SourceLang {
    match language {
        Language::C => crate::parse::SourceLang::C,
        Language::Cpp => crate::parse::SourceLang::Cpp,
    }
}

#[allow(clippy::too_many_arguments)]
fn index_source_file(
    path: &Path,
    root: &Path,
    graph: &IncludeGraph,
    index_opts: &PreprocessOptions,
    source_cache: &IndexSourceCache,
    header_ir: Option<&HashMap<PathBuf, Arc<UnitIndex>>>,
    pch_order: &[PathBuf],
) -> UnitIndex {
    let mut program = Program::new(root.to_path_buf());
    match process_indexed_file(
        &mut program,
        path,
        graph,
        index_opts,
        source_cache,
        header_ir,
        pch_order,
    ) {
        Ok(()) => {
            if std::env::var_os("TRACE_DEBUG_UNIT").is_some() {
                let hdr = program
                    .symbols
                    .functions
                    .iter()
                    .filter(|f| {
                        program
                            .symbols
                            .files
                            .get(f.span.file.0 as usize)
                            .is_some_and(|fi| is_index_header(&fi.path))
                    })
                    .count();
                eprintln!(
                    "[unit] {} fns={} header_origin_fns={}",
                    path.display(),
                    program.symbols.functions.len(),
                    hdr
                );
            }
            program_into_unit(path.to_path_buf(), program)
        }
        Err(e) => UnitIndex {
            path: path.to_path_buf(),
            diagnostics: vec![Diagnostic {
                severity: DiagnosticSeverity::Error,
                file: None,
                line: 0,
                message: e,
                stage: "parse".into(),
            }],
            ..Default::default()
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn process_indexed_file(
    program: &mut Program,
    path: &Path,
    graph: &IncludeGraph,
    index_opts: &PreprocessOptions,
    source_cache: &IndexSourceCache,
    header_ir: Option<&HashMap<PathBuf, Arc<UnitIndex>>>,
    pch_order: &[PathBuf],
) -> Result<(), String> {
    let pre = source_cache.get_or_preprocess(path, graph, index_opts)?;
    let self_canon = graph.intern_path(path);
    let file_id = program.symbols.add_file_interned(&self_canon);
    add_preprocess_diagnostics(program, graph, file_id, &pre.diagnostics);
    if let Some(ir) = header_ir {
        // Nested PCH copies types/typedefs only so ancestor units stay small.
        // TUs merge prototypes from every reachable header (the defining
        // unit keeps call sites/flow). Direct-edges + included_headers used
        // to miss `sidecar.h` for `hdf_wifi_core.c`, dropping
        // `DispatchToMessage` from `DeviceNodeExtDispatch`.
        let types_only = is_index_header(path);
        let headers = headers_to_merge(
            graph,
            pch_order,
            &self_canon,
            &pre.included_headers,
            types_only,
        );
        for h in headers {
            if let Some(unit) = ir.get(h) {
                if types_only {
                    merge_unit_types(program, unit);
                } else {
                    merge_unit_symbols(program, unit);
                }
            }
            let hid = program.symbols.add_file_interned(h);
            if hid != file_id {
                program.symbols.register_included_header(file_id, hid);
            }
        }
        program.types.complete_nested_tags();
    }
    if let Some(dir) = std::env::var_os("TRACE_DUMP_TU_DIR") {
        let fname = format!(
            "tu_{}.i",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("x")
        );
        let _ = std::fs::write(std::path::Path::new(&dir).join(fname), pre.text.as_ref());
    }
    // `index_opts` was chosen by `index_language`, so its language is the
    // one the text was lexed as; the grammar must not be re-derived from
    // the path or a forced language would preprocess as one language and
    // parse as the other.
    let lang = source_lang(
        index_opts
            .language
            .unwrap_or_else(|| Language::from_path(path)),
    );
    let parsed = crate::parse::parse_source_with_lang(Arc::clone(&pre.text), lang)?;
    if crate::parse::has_parse_errors(&parsed.tree) {
        program.add_diagnostic(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: None,
            line: 0,
            message: format!("parse errors in {}", path.display()),
            stage: "parse".into(),
        });
    }

    let mut ctx = LowerContext {
        current_fn: None,
        current_file: file_id,
        locals: HashMap::new(),
        line_map: Some(std::sync::Arc::clone(&pre.line_map)),
        primary_path: self_canon,
        pending: RefCell::new(Vec::new()),
        ns_stack: Vec::new(),
        using_nss: Vec::new(),
        using_name_imports: Vec::new(),
        class_ctx: None,
        is_cpp: lang == crate::parse::SourceLang::Cpp,
        handled_new_exprs: RefCell::new(std::collections::HashSet::new()),
        callee_load_cache: RefCell::new(HashMap::new()),
        call_return_dst: RefCell::new(HashMap::new()),
        ast_depth: 0,
        ast_depth_warned: false,
    };
    lower_tree(
        program,
        &mut ctx,
        parsed.source.as_ref(),
        parsed.tree.root_node(),
    );
    resolve_pending_fn_refs(program, &ctx);
    program.types.complete_nested_tags();
    Ok(())
}

/// Forward what the preprocessor reported for this unit (missing includes,
/// unknown directives, unterminated `#if`, mid-file stops, …) so the rows
/// reach the `diagnostics` export with `stage = 'preprocess'`. Each one is
/// attributed to the file it happened in; a diagnostic without an origin
/// file lands on the unit itself.
fn add_preprocess_diagnostics(
    program: &mut Program,
    graph: &IncludeGraph,
    unit_file: trace_ir::FileId,
    diagnostics: &[trace_preproc::Diagnostic],
) {
    for d in diagnostics {
        let file = match &d.file {
            Some(p) => program.symbols.add_file_interned(graph.intern_path(p)),
            None => unit_file,
        };
        let severity = match d.severity {
            trace_preproc::DiagnosticSeverity::Error => DiagnosticSeverity::Error,
            trace_preproc::DiagnosticSeverity::Warning => DiagnosticSeverity::Warning,
        };
        let diagnostic = Diagnostic {
            severity,
            file: Some(file),
            line: d.line,
            message: d.message.clone(),
            stage: PREPROCESS_STAGE.into(),
        };
        if program.dedup.insert_preprocess_diagnostic(
            diagnostic.file,
            diagnostic.line,
            &diagnostic.message,
        ) {
            program.add_diagnostic(diagnostic);
        }
    }
}

/// Second-chance resolution for references recorded while lowering: the
/// whole unit's symbol table is now populated, so definitions that appear
/// after their use site are visible.
fn resolve_pending_fn_refs(program: &mut Program, ctx: &LowerContext) {
    let pending: Vec<PendingFnRef> = ctx.pending.borrow_mut().drain(..).collect();
    for item in pending {
        match item {
            PendingFnRef::FieldStore { dst, name, span } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    let tmp = alloc_ret_temp_spanned(program, ctx, span);
                    program
                        .flow
                        .push(FlowConstraint::AddrOfFn { dst: tmp, callee });
                    program.flow.push(FlowConstraint::Store { dst, src: tmp });
                }
            }
            PendingFnRef::RhsIdent { dst, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program.flow.push(FlowConstraint::AddrOfFn { dst, callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    // A tentative global defined after the use site.
                    program.flow.push(FlowConstraint::Copy { dst, src });
                }
            }
            PendingFnRef::AddrOfIdent { dst, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program.flow.push(FlowConstraint::AddrOfFn { dst, callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    program.flow.push(FlowConstraint::AddrOfVar { dst, src });
                }
            }
            PendingFnRef::ReturnIdent { owner, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::AddrOfFn { callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::Copy { src });
                }
            }
            PendingFnRef::ReturnAddrOf { owner, name } => {
                if let Some(callee) = resolve_function_named(program, ctx, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::AddrOfFn { callee });
                } else if let Some(src) = lookup_var(ctx, program, &name) {
                    program
                        .fn_returns
                        .entry(owner)
                        .or_default()
                        .push(ReturnFlow::AddrOfVar { src });
                }
            }
        }
    }
}

fn program_into_unit(path: PathBuf, mut program: Program) -> UnitIndex {
    UnitIndex {
        files: program
            .symbols
            .files
            .iter()
            .map(|f| f.path.clone())
            .collect(),
        path,
        types: program.types,
        functions: program.symbols.functions,
        variables: program.symbols.variables,
        call_sites: program.symbols.call_sites,
        flow: program.flow,
        fn_returns: program.fn_returns.into_iter().collect(),
        diagnostics: program.diagnostics,
        anon_type_counter: program.anon_type_counter,
        inheritance: std::mem::take(&mut program.inheritance),
        template_bases: std::mem::take(&mut program.template_bases),
        final_classes: std::mem::take(&mut program.final_classes),
    }
}

fn lower_typedef(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    if let Some(decl) = node.child_by_field_name("declarator") {
        let (alias, _) = parse_declarator_name(source, decl);
        if let Some(type_node) = node.child_by_field_name("type") {
            if type_node.kind() == "struct_specifier" || type_node.kind() == "union_specifier" {
                let tag = lower_struct_specifier(program, ctx, source, type_node);
                if !alias.is_empty() && !tag.is_empty() && alias != tag {
                    let kind = if type_node.kind() == "union_specifier" {
                        TypeDesc::Union {
                            name: tag.clone(),
                            fields: Vec::new(),
                        }
                    } else {
                        TypeDesc::Struct {
                            name: tag.clone(),
                            fields: Vec::new(),
                        }
                    };
                    program.types.intern(kind.clone());
                    // Register even when alias == tag: later `Tag *x`
                    // declarations resolve through the alias table
                    // (`type_desc_from_node`), and without an entry the
                    // pointer degrades to Int, killing field decomposition.
                    program.types.register_alias(&alias, kind);
                }
            } else if let Some(desc) = typedef_underlying_desc(program, ctx, source, node) {
                program.types.register_alias(&alias, desc);
            }
        }
    }
}

fn lower_tree(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    if ctx.ast_depth >= MAX_AST_WALK_DEPTH {
        if !ctx.ast_depth_warned {
            program.add_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: None,
                line: 0,
                message: format!(
                    "AST walk depth exceeded ({MAX_AST_WALK_DEPTH}); skipping deeper nodes"
                ),
                stage: "parse".into(),
            });
            ctx.ast_depth_warned = true;
        }
        return;
    }
    ctx.ast_depth += 1;
    match node.kind() {
        "function_definition" => lower_function(program, ctx, source, node),
        "declaration" => lower_declaration(program, ctx, source, node, None),
        "struct_specifier" | "union_specifier" | "class_specifier" => {
            let tag = lower_struct_specifier(program, ctx, source, node);
            // In C++, `struct` is identical to `class` except for default
            // visibility — structs may have constructors, destructors, and
            // member functions that must be lowered just like classes.
            if node.kind() == "class_specifier" || (ctx.is_cpp && node.kind() == "struct_specifier")
            {
                lower_class_members(program, ctx, source, node, &tag);
            }
        }
        "namespace_definition" => lower_namespace(program, ctx, source, node),
        "using_declaration" => lower_using_declaration(ctx, source, node),
        // Templates are lowered once, as a merged representative of all
        // instantiations; explicit specializations fold into the same entry
        // (documented imprecision). The inner definition carries everything.
        "template_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "template_parameter_list" {
                    continue;
                }
                lower_tree(program, ctx, source, child);
            }
        }
        "type_definition" => lower_typedef(program, ctx, source, node),
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                lower_tree(program, ctx, source, child);
            }
        }
    }
    ctx.ast_depth = ctx.ast_depth.saturating_sub(1);
}

fn lower_namespace(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    let name = node.children(&mut node.walk()).find_map(|c| {
        if c.kind() == "namespace_identifier" {
            Some(normalize_qualified(node_text(source, &c)))
        } else {
            None
        }
    });
    ctx.ns_stack.push(name.clone());
    // `using namespace X;` / `using X::f;` inside a namespace block are
    // scoped to the block in real C++ (a directive written here must not
    // keep affecting resolution after the block closes — a leak could let
    // the overload ranking collapse away the correct in-scope edge).
    // Directives from enclosing namespaces are inherited (snapshot length).
    let using_nss_len = ctx.using_nss.len();
    let using_imports_len = ctx.using_name_imports.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "declaration_list" {
            let mut inner = child.walk();
            for decl in child.children(&mut inner) {
                lower_tree(program, ctx, source, decl);
            }
        }
    }
    ctx.using_nss.truncate(using_nss_len);
    ctx.using_name_imports.truncate(using_imports_len);
    ctx.ns_stack.pop();
}

/// Enclosing namespaces of the current scope, innermost first, as joined
/// prefixes (`namespace a { namespace b { … } }` yields `a::b`, `a`).
/// When `include_self` is true the innermost namespace itself is also
/// included (needed by `expand_using_target` so that `using namespace detail;`
/// written inside `relns::directive_host` also yields `relns::directive_host`
/// as a prefix — C++ resolves the first segment against the enclosing scope).
fn enclosing_namespace_prefixes(ctx: &LowerContext, include_self: bool) -> Vec<String> {
    let chain: Vec<&str> = ctx.ns_stack.iter().flatten().map(String::as_str).collect();
    let end = if include_self {
        chain.len()
    } else {
        chain.len().saturating_sub(1)
    };
    (0..=end).rev().map(|len| chain[..len].join("::")).collect()
}

/// All namespaces a relative `using` target may denote: the target with its
/// first segment qualified by each enclosing namespace (innermost first)
/// followed by the literal spelling. Real C++ looks the first segment up in
/// enclosing scopes, so `using namespace detail;` inside `namespace a` may
/// mean `detail` **or** `a::detail`; `using inner::fold;` may mean
/// `inner::fold` or `a::inner::fold`. Recording every plausible form keeps
/// the lookup sound (over-approximation) — a resolve-time miss would degrade
/// the call to an external stub. Leading-`::` targets are already globally
/// qualified and returned unchanged.
fn expand_using_target(ctx: &LowerContext, target: &str) -> Vec<String> {
    let (first, rest) = match target.find("::") {
        Some(idx) => (&target[..idx], Some(&target[idx + 2..])),
        None => (target, None),
    };
    if first.is_empty() {
        return vec![target.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    for prefix in enclosing_namespace_prefixes(ctx, true) {
        let qualified_first = format!("{prefix}::{first}");
        match rest {
            Some(r) => out.push(format!("{qualified_first}::{r}")),
            None => out.push(qualified_first),
        }
    }
    out.push(target.to_string());
    out
}

fn lower_using_declaration(ctx: &mut LowerContext, source: &str, node: Node) {
    let mut is_ns_using = false;
    let mut target: Option<String> = None;
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "namespace" => is_ns_using = true,
            "identifier" | "qualified_identifier" | "namespace_identifier" => {
                target = Some(normalize_qualified(node_text(source, &child)));
            }
            _ => {}
        }
    }
    if let Some(t) = target {
        if is_ns_using {
            for qualified in expand_using_target(ctx, &t) {
                if !ctx.using_nss.contains(&qualified) {
                    ctx.using_nss.push(qualified);
                }
            }
        } else if t.contains("::") {
            // `using lib::bump;` — a specific name import. Record
            // `(base, full-qualified)` so a later bare `bump()` call also
            // considers the exact imported entry (sound over-approximation:
            // the declaration specifically names this function). Relative
            // spellings are expanded against the enclosing namespaces too.
            let base = t.rsplit("::").next().unwrap_or(&t).to_string();
            for qualified in expand_using_target(ctx, &t) {
                let pair = (base.clone(), qualified);
                if !ctx.using_name_imports.contains(&pair) {
                    ctx.using_name_imports.push(pair);
                }
            }
        }
    }
}

/// Does this member declaration declare a function (method/ctor/dtor)
/// rather than a data member? Function-pointer members (`int (*cb)(int);`)
/// parse through a `function_declarator` too but their name sits behind a
/// parenthesized/pointer declarator — those are data.
fn member_decl_is_function(node: Node) -> bool {
    fn walk(n: Node) -> bool {
        match n.kind() {
            // Type position, not declarator position — see member_short_name.
            "decltype" => return false,
            "destructor_name" => return true,
            // `operator T()` — a conversion operator, whose declarator names
            // the converted-to type instead of an identifier (#46).
            "operator_cast" => return true,
            // `MACRO operator unsigned long() const;` — a multi-word
            // primitive target is recovered as loose keywords inside the
            // `ERROR` (`operator`, `unsigned`, `long`), leaving no declarator
            // anywhere for the test below to find, so the member was read as a
            // data field and dropped from the index. An `ERROR` the keyword
            // opens is a conversion operator whatever it holds.
            "ERROR" if n.child(0).is_some_and(|k| k.kind() == "operator") => return true,
            "function_declarator" => {
                if let Some(inner) = n.child_by_field_name("declarator") {
                    return matches!(
                        inner.kind(),
                        "field_identifier"
                            | "identifier"
                            | "qualified_identifier"
                            | "operator_name"
                            // `MACRO operator Vec<int>() const;` — recovery
                            // leaves the target's argument list attached to
                            // the declarator, which is then a template
                            // method. Missing it dropped the member outright.
                            | "template_method"
                    ) || walk(inner);
                }
                return false;
            }
            _ => {}
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                if walk(c) {
                    return true;
                }
            }
        }
        false
    }
    walk(node)
}

struct VirtualFlags {
    is_virtual: bool,
    is_final: bool,
}

fn virtual_flags(source: &str, node: Node) -> VirtualFlags {
    let mut is_virtual = false;
    let mut is_final = false;
    fn walk(source: &str, n: Node, is_virtual: &mut bool, is_final: &mut bool) {
        for child in n.children(&mut n.walk()) {
            match child.kind() {
                "virtual" => *is_virtual = true,
                "virtual_specifier" => {
                    *is_virtual = true;
                    if node_text(source, &child).contains("final") {
                        *is_final = true;
                    }
                }
                _ => walk(source, child, is_virtual, is_final),
            }
        }
    }
    walk(source, node, &mut is_virtual, &mut is_final);
    VirtualFlags {
        is_virtual,
        is_final,
    }
}

fn class_specifier_is_final(source: &str, node: Node) -> bool {
    node.children(&mut node.walk())
        .any(|c| c.kind() == "virtual_specifier" && node_text(source, &c).contains("final"))
}

fn lower_struct_specifier(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> String {
    let is_union = node.kind() == "union_specifier";
    let name_node = node.child_by_field_name("name");
    let mut name = name_node
        .map(|n| {
            let raw = node_text(source, &n).to_string();
            if n.kind() == "template_type" || raw.contains('<') {
                // `Box<int>` specializations register under the bare template
                // tag; members merge with the primary template's.
                strip_template_args(&normalize_qualified(&raw))
            } else if raw.contains("::") || raw.contains(char::is_whitespace) {
                normalize_qualified(&raw)
            } else {
                raw
            }
        })
        .unwrap_or_default();

    if name.is_empty() {
        program.anon_type_counter += 1;
        name = format!("anon_{}", program.anon_type_counter);
    }

    // Classes (and C++ structs, which are classes with different defaults)
    // register under their fully qualified tag so type references, owner-class
    // derivation and member resolution all agree on one name.
    let is_cpp_class = ctx.is_cpp && matches!(node.kind(), "class_specifier" | "struct_specifier");
    let reg_name = if is_cpp_class && !name.contains("::") {
        ctx.qualify(&name)
    } else {
        name.clone()
    };

    // C++: `class D : B, A { ... }` — record inheritance for virtual
    // dispatch expansion. Unqualified bases resolve against the current
    // namespace scope (usings ignored here; documented imprecision).
    // `struct D : B` is the same relationship (only default access differs).
    if is_cpp_class {
        let derived = reg_name.clone();
        if class_specifier_is_final(source, node) {
            program.mark_class_final(&derived);
        }
        for child in node.children(&mut node.walk()) {
            if child.kind() != "base_class_clause" {
                continue;
            }
            // `class D : virtual public B, public C` — `virtual` is an
            // unnamed token next to the type; we still record the edge
            // (CHA treats virtual and non-virtual bases the same for
            // override sets; diamond sharing is a layout concern).
            for base in child.children(&mut child.walk()) {
                if !base.is_named() && base.kind() != "qualified_identifier" {
                    continue;
                }
                if matches!(
                    base.kind(),
                    "type_identifier"
                        | "qualified_identifier"
                        | "template_type"
                        | "namespace_identifier"
                ) {
                    let base_text = node_text(source, &base);
                    let template_spelling = normalize_template_spelling(base_text);
                    if template_spelling.contains('<') {
                        let qualified = if template_spelling.contains("::") {
                            template_spelling
                        } else {
                            ctx.qualify(&template_spelling)
                        };
                        program.add_template_base(&derived, &qualified, &ctx.namespace_scope());
                    }
                    let raw = normalize_qualified(base_text);
                    let base = strip_template_args(&raw);
                    let base = if base.contains("::") {
                        base
                    } else {
                        ctx.qualify(&base)
                    };
                    program.add_inheritance(&derived, &base);
                }
            }
        }
    }

    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "field_declaration" && !member_decl_is_function(child) {
                if let Some((fname, field_type)) =
                    type_desc_from_field_declaration(program, ctx, source, child)
                {
                    if !fname.is_empty() {
                        fields.push((fname, field_type));
                    }
                }
            }
        }
    }

    // Classes always intern a layout entry (even when data fields are
    // absent) so type references, owner-class derivation and member-call
    // resolution can find them by tag.
    // C++ classes always intern a layout entry; C++ structs do too — a
    // method-only body (`struct Ctor { Ctor(); };`) must still resolve as
    // a class type for member-initializer and receiver inference.
    if !fields.is_empty() || node.kind() == "class_specifier" || ctx.is_cpp {
        if is_union {
            program.types.compute_union_layout(reg_name.clone(), fields);
        } else {
            program
                .types
                .compute_struct_layout(reg_name.clone(), fields);
        }
    }
    reg_name
}

/// Lower the member functions of a class body: prototypes first (so later
/// definitions in other TUs merge against them and virtuality is recorded),
/// then in-class definitions with the class context active.
fn lower_class_members(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    cls_qual: &str,
) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    let members: Vec<Node> = body
        .children(&mut cursor)
        .filter(|m| {
            matches!(
                m.kind(),
                "function_definition"
                    | "field_declaration"
                    | "declaration"
                    | "template_declaration"
            )
        })
        .collect();
    for m in &members {
        // A class-scope `template <typename T> T GetNumber() {...}` declares
        // the member's primary; the template parameter list is not an
        // argument — unwrap it and register the nested member like any other.
        if m.kind() == "template_declaration" {
            if let Some(inner) = template_member_decl(*m) {
                if member_decl_is_function(inner) {
                    register_member_prototype(program, ctx, source, inner, cls_qual);
                }
            }
            continue;
        }
        if m.kind() == "field_declaration" && member_decl_is_function(*m) {
            register_member_prototype(program, ctx, source, *m, cls_qual);
        }
        // A ctor written `Cls(int);` inside the class parses as a plain
        // declaration wrapping a function_declarator.
        if m.kind() == "declaration"
            && member_decl_is_function(*m)
            && !continues_previous_member(*m)
        {
            register_member_prototype(program, ctx, source, *m, cls_qual);
        }
    }
    let saved = ctx.class_ctx.clone();
    for m in &members {
        let unwrapped = if m.kind() == "template_declaration" {
            template_member_decl(*m)
        } else {
            None
        };
        let member = unwrapped.unwrap_or(*m);
        let in_class_def = member.kind() == "function_definition"
            || (member.kind() == "field_declaration"
                && member_decl_is_function(member)
                && node_has_compound_body(member));
        if in_class_def {
            ctx.class_ctx = Some(ClassCtx {
                qual_name: cls_qual.to_string(),
            });
            lower_function(program, ctx, source, member);
        }
    }
    ctx.class_ctx = saved;
}

/// Unwrap the declaration wrapped by a `template_declaration` (skipping the
/// `template_parameter_list`), if any.
fn template_member_decl(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| !matches!(c.kind(), "template_parameter_list" | "template"))
        .filter(|c| {
            matches!(
                c.kind(),
                "function_definition" | "field_declaration" | "declaration"
            )
        })
        .last()
}

/// The bare name a member declares: method identifier, ctor class-name, or
/// destructor spelling (`~Cls`).
fn member_short_name(source: &str, node: Node) -> Option<String> {
    fn walk(source: &str, n: Node) -> Option<String> {
        match n.kind() {
            // A `decltype(...)` sits in the *type* position and holds an
            // expression, not a declarator. Walking into it takes the first
            // `identifier` of that expression as the member's name, so
            // `decltype(*p_) Deref() const;` was indexed as `p_` and `Deref`
            // was lost — silently, since the file parses cleanly (#29).
            "decltype" => return None,
            // An `ERROR` node holds whichever half of the declaration
            // tree-sitter could not place, and which half that is depends on
            // where the unknown attribute macro sat:
            //
            // - `FFI_EXPORT CArr Get(long);` — the macro took the `type`
            //   field, so the leftover return type is the ERROR and the real
            //   declarator its sibling. Walking in takes `CArr` as the name
            //   and loses `Get`, the way a `decltype` operand used to;
            // - `int j() const NOEXCEPT_MACRO;` — the macro trails the
            //   declarator, so the ERROR *is* the declarator and the macro is
            //   a sibling `field_identifier`. Skipping it names every such
            //   member after its macro, collapsing a class that annotates all
            //   of them alike into one symbol.
            //
            // A declarator of its own tells the two apart — and when the
            // member carries both macros at once
            // (`EXPORT_API int Get(long) GUARDED_BY(mu_);`) the ERROR holds
            // both halves, so only its declarators may be read: the leftover
            // type sits beside them and comes first, which named the member
            // `C::int` and collapsed every member sharing a return type.
            "ERROR" => {
                return n
                    .children(&mut n.walk())
                    .filter(|c| c.kind().ends_with("_declarator"))
                    .find_map(|c| walk(source, c));
            }
            // A C++11 attribute (`[[nodiscard]]`, `[[gnu::pure]]`) or a GNU
            // one sits in front of the declaration, holds an identifier of
            // its own, and declares nothing — the walk used to name the
            // member after it, collapsing every annotated member of a class
            // into one symbol.
            "attribute_declaration"
            | "attribute_specifier"
            | "ms_declspec_modifier"
            | "ms_call_modifier" => return None,
            "destructor_name" => return Some(normalize_qualified(node_text(source, &n))),
            "operator_name" => return Some(normalize_qualified(node_text(source, &n))),
            "operator_cast" => return Some(conversion_operator_name(source, n)),
            "function_declarator" => {
                if let Some(inner) = n.child_by_field_name("declarator") {
                    return walk(source, inner);
                }
                return None;
            }
            "field_identifier" | "identifier" => {
                return Some(normalize_qualified(node_text(source, &n)));
            }
            _ => {}
        }
        // `MACRO ~D();` stalls on the tilde the same way, leaving `~` alone
        // in an `ERROR` and `D` standing as the declarator — so the
        // destructor read as the constructor `D`, and `delete p` then
        // expanded over an override set missing it.
        if let Some(err) = n
            .children(&mut n.walk())
            .find(|c| c.kind() == "ERROR" && node_text(source, c).trim_end() == "~")
        {
            let rest = n
                .children(&mut n.walk())
                .filter(|c| c.start_byte() >= err.end_byte())
                .find_map(|c| walk(source, c))?;
            return Some(format!("~{}", rest.trim_start()));
        }
        // An unknown attribute macro in front of a conversion operator leaves
        // the `operator` keyword stranded in an `ERROR` and the target type
        // standing where the member name belongs, so the walk below would
        // name `MACRO operator ns::S() const;` after its target — `S`, a name
        // that collides with the class of that name and matches no
        // declaration of the real member. The name is whatever the rest of
        // the declaration resolves to, under the keyword.
        //
        // How much of the rest the `ERROR` swallowed varies: the keyword
        // alone (`ERROR [operator]`), the keyword and the target's own scope
        // (`ERROR [operator ns::]`), the target as well when a second macro
        // trails the member (`ERROR [operator int() const]`, `GUARDED_BY(m)`
        // outside it), or the target and its parameter list with no structure
        // left inside at all (`ERROR [operator unsigned long() const]`). Where
        // to look for the target follows from which of these it is; how to
        // read it does not.
        if let Some(err) = conversion_keyword_error(n) {
            let keyword_end = err.child(0).map_or(err.end_byte(), |k| k.end_byte());
            // The target is *spelled*, not walked. Walking a declarator picks
            // the single identifier it is named by, which for a target is only
            // its last segment: that dropped every other part of the spelling
            // — `ns::` from `operator ns::S`, `<int>` from
            // `operator Vec<int>`, `(*)` from `operator int (*)` — leaving
            // names (`C::operator S`, `C::operator Vec`, `C::operator int`)
            // that no unannotated spelling of the same member produces, and
            // that collide with the class of that name or with a sibling
            // conversion in the same class.
            //
            // The spelling runs from the keyword to where the operator's
            // *own* parameter list begins, and that list is the one hanging
            // off the declarator, so the target ends where the declarator's
            // own `declarator` field does: in `[operator ns::S() const]`,
            // `function_declarator [S() const]` is named by `S`, and the text
            // up to the end of `S` is `ns::S` — scope and all, since the
            // scope is contiguous with it in the source however the `ERROR`
            // split the two. Cutting at the declarator's start would keep the
            // scope but lose the segment; cutting at the `ERROR`'s end would
            // swallow `() const`.
            //
            // Which side of the `ERROR`'s end the declarator sits on only
            // decides where to look for it, not how to read it. Whether the
            // `ERROR` reaches the operator's own `(` says which side to look:
            // reaching it means the target is in there and any declarator
            // beside it belongs to the trailing macro, which named the member
            // `C::operator unsigned long()const GUARDED_BY`.
            let tail = &source[keyword_end..err.end_byte()];
            let own_declarator = err
                .children(&mut err.walk())
                .filter(|c| c.kind().ends_with("_declarator"))
                .last()
                .or_else(|| {
                    if tail.contains('(') {
                        return None;
                    }
                    n.children(&mut n.walk()).find(|c| {
                        c.start_byte() >= err.end_byte() && c.kind().ends_with("_declarator")
                    })
                });
            if let Some(decl) = own_declarator {
                let end = decl
                    .child_by_field_name("declarator")
                    .map_or_else(|| decl.start_byte(), |d| d.end_byte());
                let target = normalize_spacing(&source[keyword_end..end]);
                return Some(format!("operator {}", target.trim_start()));
            }
            // The `ERROR` reached the `(` but gave the target no declarator of
            // its own: a multi-word primitive target is recovered as loose
            // keywords (`ERROR [operator unsigned long() const]`, children
            // `operator`, `unsigned`, `long`). The spelling still ends where
            // the parameter list starts, so cut there. Without this the member
            // was named after the trailing macro, or dropped outright when
            // there was none for the walk to fall through to.
            if let Some(paren) = tail.find('(') {
                let target = normalize_spacing(&tail[..paren]);
                if !target.is_empty() {
                    return Some(format!("operator {}", target.trim_start()));
                }
            }
            // No declarator on either side: the `ERROR` swallowed the keyword
            // and whatever scope the target carried (`ERROR [operator ns::]`),
            // and the rest of the target is a bare name beside it. Reading
            // only that name dropped the scope, so a macro-annotated
            // declaration was spelled `D::operator S` where every other path
            // spells the same member `D::operator ns::S`.
            let carried_scope = normalize_spacing(tail);
            let rest = n
                .children(&mut n.walk())
                .filter(|c| c.start_byte() >= err.end_byte())
                .find_map(|c| walk(source, c))?;
            return Some(format!("operator {carried_scope}{}", rest.trim_start()));
        }
        for i in 0..n.child_count() {
            if let Some(c) = n.child(i) {
                if let Some(found) = walk(source, c) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(source, node)
}

/// Whether a class-body `declaration` is only the tail of the member before
/// it, split off by error recovery rather than declared in its own right.
///
/// A member ending in a *missing* `;` is one the author wrote no `;` after,
/// so whatever tree-sitter parked after it is the rest of that same
/// declaration. An unknown attribute macro trailing a conversion operator to
/// a pointer or reference is recovered exactly so:
/// `EXPORT_API operator Payload *() const GUARDED_BY(m);` leaves the operator
/// in a `field_declaration` closed by a missing `;` and `GUARDED_BY(m);`
/// standing as a `declaration` of its own. Registering that named a phantom
/// `C::GUARDED_BY` — undefined, and the member every call site annotated
/// alike resolves to instead of the real one.
///
/// The caller tests only `declaration` members, which is what keeps this
/// narrow: a genuinely separate member after a missing `;` (`void f()` then
/// `void g();`) recovers as a `field_declaration` and never reaches here. The
/// one thing it does swallow is a ctor declaration after a member whose `;` the
/// author really did forget, and that spelling is not valid C++ either way.
fn continues_previous_member(node: Node) -> bool {
    let Some(prev) = node.prev_named_sibling() else {
        return false;
    };
    prev.child_count()
        .checked_sub(1)
        .and_then(|last| prev.child(last))
        .is_some_and(|c| c.is_missing() && c.kind() == ";")
}

fn register_member_prototype(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    cls_qual: &str,
) {
    let Some(short) = member_short_name(source, node) else {
        return;
    };
    if short.is_empty() || short == "operator" {
        return;
    }
    let full_name = canonicalize_conversion_target(&format!("{}::{}", cls_qual, short));
    let flags = virtual_flags(source, node);
    let provisional_id = program.symbols.alloc_fn_id();
    // Prototypes carry no parameter variables; they merge into their
    // definitions, which supply the real param list for arity filtering.
    let params: Vec<VarId> = Vec::new();
    let span = node_span(program, ctx, node);
    program.symbols.add_function(Function {
        id: provisional_id,
        name: full_name,
        linkage: if ctx.in_anonymous_namespace() {
            Linkage::Internal
        } else {
            Linkage::External
        },
        return_type: program.types.void(),
        params,
        locals: Vec::new(),
        span,
        end_line: span.line,
        file: ctx.current_file,
        is_defined: false,
        param_type_ids: Vec::new(),
        is_virtual: flags.is_virtual,
        is_final: flags.is_final,
        is_cpp: ctx.is_cpp,
    });
}

fn lower_function(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    let Some(decl) = error_parked_declarator(node)
        .or_else(|| node.child_by_field_name("declarator"))
        .or_else(|| find_function_declarator(node))
    else {
        return;
    };
    // An in-class conversion operator behind a leading macro keeps a
    // declarator, but that declarator names the type it converts *to*; only
    // the member walk knows to read the keyword stranded beside it. Without
    // this the definition landed on `C::S` — defined, colliding with the
    // class `S` itself — while its declaration stayed undefined.
    let raw_name = match conversion_keyword_error(node) {
        Some(_) => match member_short_name(source, node) {
            Some(n) => n,
            None => return,
        },
        None => parse_declarator_name(source, decl).0,
    };
    if raw_name.is_empty() {
        return;
    }
    let name = ctx.qualify_decl(&raw_name);
    // Out-of-class member definitions (`void Cls::f() {}`, ctors, dtors):
    // recover the owning class from the longest `::`-prefix that names a
    // known class type.
    let eff_class: Option<String> = match &ctx.class_ctx {
        Some(c) => Some(c.qual_name.clone()),
        None => derive_owner_class(program, &name),
    };
    let ret_type = match node.child_by_field_name("type") {
        Some(t) => parse_type_node(program, ctx, source, t),
        // A conversion operator has no `type` field: what it returns is the
        // type it converts to, which lives inside its `operator_cast`.
        None => match declarator_operator_cast(decl)
            .and_then(|op| conversion_target_type(program, ctx, source, op))
        {
            Some(t) => t,
            None => program.types.int(),
        },
    };
    let provisional_id = program.symbols.alloc_fn_id();
    let mut params = Vec::new();
    // Implicit `this` for member functions, ctors and dtors.
    if let Some(cls) = &eff_class {
        let this_type = program
            .types
            .intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
                name: cls.clone(),
                fields: Vec::new(),
            })));
        let this_id = program.symbols.alloc_var_id();
        let span = node_span(program, ctx, node);
        program.symbols.add_variable(Variable {
            id: this_id,
            name: "this".to_string(),
            type_id: this_type,
            storage: StorageClass::Param,
            fn_id: Some(provisional_id),
            param_index: Some(0),
            span,
            is_pointer: true,
        });
        params.push(this_id);
    }
    if let Some(params_node) = find_params(decl) {
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() == "parameter_declaration" {
                if let Some(var) = lower_parameter(
                    program,
                    ctx,
                    source,
                    param,
                    provisional_id,
                    params.len() as u32,
                ) {
                    params.push(var);
                }
            }
        }
    }

    let is_static = declaration_is_static(source, node) || ctx.in_anonymous_namespace();
    let flags = virtual_flags(source, node);

    let span = node_span(program, ctx, node);
    let end_line = node_end_line(program, ctx, node, span);
    let fn_id = program.symbols.add_function(Function {
        id: provisional_id,
        name: name.clone(),
        linkage: if is_static {
            Linkage::Internal
        } else {
            Linkage::External
        },
        return_type: ret_type,
        params: params.clone(),
        locals: Vec::new(),
        span,
        end_line,
        file: ctx.current_file,
        is_defined: true,
        param_type_ids: Vec::new(),
        is_virtual: flags.is_virtual,
        is_final: flags.is_final,
        is_cpp: ctx.is_cpp,
    });
    reassign_fn_id(program, provisional_id, fn_id);
    ctx.current_fn = Some(fn_id);
    ctx.locals.clear();
    for &param in &params {
        if let Some(v) = program.symbols.variable_by_id(param) {
            ctx.locals.insert(v.name.clone(), param);
        }
    }
    // Out-of-class definitions still resolve implicit `this->` members.
    let saved_class = ctx.class_ctx.clone();
    if let Some(cls) = &eff_class {
        let needs_set = match &ctx.class_ctx {
            Some(c) => c.qual_name != *cls,
            None => true,
        };
        if needs_set {
            ctx.class_ctx = Some(ClassCtx {
                qual_name: cls.clone(),
            });
        }
    }

    // `using namespace X;` / `using X::f;` inside a function body must not
    // leak into the rest of the TU: real C++ scopes them to the enclosing
    // block, and leaking can turn the may-approximation into an
    // under-approximation when the arity/type ranking later collapses to one
    // candidate. Record the pre-body lengths and drop only the directives
    // added during this function's walk (file-scope directives are inherited
    // and must stay).
    let using_nss_len = ctx.using_nss.len();
    let using_imports_len = ctx.using_name_imports.len();

    if let Some(body_node) = node.child_by_field_name("body") {
        walk_function_body(program, ctx, source, body_node, fn_id);
    }
    // Constructor-initializer lists are siblings of the body on
    // function_definition; they carry base/member ctor calls.
    if ctx.is_cpp {
        let init_lists: Vec<_> = node
            .children(&mut node.walk())
            .filter(|c| c.kind() == "field_initializer_list")
            .collect();
        for il in init_lists {
            walk_function_body(program, ctx, source, il, fn_id);
        }
    }
    ctx.using_nss.truncate(using_nss_len);
    ctx.using_name_imports.truncate(using_imports_len);

    ctx.current_fn = None;
    ctx.locals.clear();
    ctx.class_ctx = saved_class;
}

/// Longest `a::b::Cls` prefix of a qualified function name that resolves to
/// an interned class/struct tag.
fn derive_owner_class(program: &Program, qualified_name: &str) -> Option<String> {
    let segs: Vec<&str> = qualified_name.split("::").collect();
    if segs.len() < 2 {
        return None;
    }
    for split in (1..segs.len()).rev() {
        let candidate = segs[..split].join("::");
        if program
            .types
            .type_id_by_tag(&candidate, trace_ir::TypeKind::Struct)
            .is_some()
        {
            return Some(candidate);
        }
    }
    None
}

fn lower_parameter(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
    index: u32,
) -> Option<VarId> {
    // Abstract / unnamed parameters (`void foo(int)`, `void foo(int *)`)
    // still occupy an arity slot. Dropping them collapsed C++ overloads.
    let (mut name, is_ptr) = match node.child_by_field_name("declarator") {
        Some(decl) => parse_declarator_name(source, decl),
        None => (String::new(), false),
    };
    let unnamed = name.is_empty();
    let declarator = node.child_by_field_name("declarator");
    let base_desc = node
        .child_by_field_name("type")
        .map(|t| type_desc_from_node(program, ctx, source, t))
        .unwrap_or(TypeDesc::Int);
    // `void f(void)` is zero arguments, not one unnamed void param.
    if unnamed && matches!(base_desc, TypeDesc::Void) && !is_ptr {
        return None;
    }
    if unnamed {
        name = format!("$arg{index}");
    }
    // `parse_declarator_name` reduces the declarator to a single pointer
    // bool, which would collapse `int **p` to `Ptr(Int)` and make `int**`
    // overloads indistinguishable from `int*`. Recover the full nesting
    // from the declarator shape instead (`int **p` -> Ptr(Ptr(Int))).
    // References keep the single pointer level: their alias semantics are
    // what matter, and they are not shaped by the walk.
    let shape = declarator
        .map(|d| walk_declarator_shape(d, base_desc.clone()))
        .unwrap_or_else(|| base_desc.clone());
    let type_desc = if is_ptr && !matches!(shape, TypeDesc::Ptr(_) | TypeDesc::Array { .. }) {
        TypeDesc::Ptr(Box::new(base_desc))
    } else {
        shape
    };
    let type_id = program.types.intern(type_desc);
    let var_id = program.symbols.alloc_var_id();
    let span = node_span(program, ctx, node);
    program.symbols.add_variable(Variable {
        id: var_id,
        name: name.clone(),
        type_id,
        storage: StorageClass::Param,
        fn_id: Some(fn_id),
        param_index: Some(index),
        span,
        is_pointer: is_ptr,
    });
    if !unnamed {
        register_local(ctx, name, var_id);
    }
    Some(var_id)
}

fn lower_declaration(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    storage_override: Option<StorageClass>,
) {
    let type_node = match node.child_by_field_name("type") {
        Some(t) => t,
        None => return,
    };
    let type_id = parse_type_node(program, ctx, source, type_node);
    let is_static = declaration_is_static(source, node);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "init_declarator" => {
                let decl = child.child_by_field_name("declarator").unwrap_or(child);
                // `auto p = new T(...)`: recover the pointee class so member
                // calls through p resolve by static type.
                let mut eff_type_id = type_id;
                let mut auto_new = false;
                if type_node.kind() == "placeholder_type_specifier" {
                    if let Some(value) = child.child_by_field_name("value") {
                        if value.kind() == "new_expression" {
                            auto_new = true;
                            if let Some(cls) = new_expression_class(program, ctx, source, value) {
                                let inner = program.types.intern(TypeDesc::Struct {
                                    name: cls,
                                    fields: Vec::new(),
                                });
                                eff_type_id = program.types.intern(TypeDesc::Ptr(Box::new(
                                    program.types.get(inner).desc.clone(),
                                )));
                            }
                        }
                    }
                }
                // A pointer/array declarator (`int *p`) must wrap the base
                // type like params already do; otherwise the local reads as
                // its base scalar and pointer args mis-rank `f(int)` over
                // the real `f(int*)`.
                if !auto_new {
                    let base = program.types.get(type_id).desc.clone();
                    eff_type_id = program.types.intern(walk_declarator_shape(decl, base));
                }
                lower_one_declarator(
                    program,
                    ctx,
                    source,
                    child,
                    decl,
                    eff_type_id,
                    is_static,
                    storage_override,
                    child.child_by_field_name("value"),
                );
            }
            "declarator" | "pointer_declarator" | "function_declarator" | "array_declarator" => {
                // A pointer-returning function declaration (`T *f(void);`) is a
                // `pointer_declarator` wrapping a `function_declarator`; it must
                // register a function, not a variable that shadows the name.
                if let Some((fdecl, ptr_depth)) = fn_decl_under_pointer(child) {
                    let mut ret = type_id;
                    for _ in 0..ptr_depth {
                        ret = program.types.intern(trace_ir::TypeDesc::Ptr(Box::new(
                            program.types.get(ret).desc.clone(),
                        )));
                    }
                    lower_function_decl(program, ctx, source, fdecl, ret, is_static);
                    continue;
                }
                let shaped_type_id = {
                    let base = program.types.get(type_id).desc.clone();
                    program.types.intern(walk_declarator_shape(child, base))
                };
                lower_one_declarator(
                    program,
                    ctx,
                    source,
                    child,
                    child,
                    shaped_type_id,
                    is_static,
                    storage_override,
                    None,
                );
            }
            "identifier" => {
                let name = node_text(source, &child).to_string();
                if name.is_empty() {
                    continue;
                }
                let var_id = program.symbols.alloc_var_id();
                let span = node_span(program, ctx, child);
                program.symbols.add_variable(Variable {
                    id: var_id,
                    name: name.clone(),
                    type_id,
                    storage: storage_override.unwrap_or_else(|| storage_for(ctx, is_static)),
                    fn_id: ctx.current_fn,
                    param_index: None,
                    span,
                    is_pointer: false,
                });
                register_local(ctx, name, var_id);
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_one_declarator(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    span_node: Node,
    decl: Node,
    type_id: trace_ir::TypeId,
    is_static: bool,
    storage_override: Option<StorageClass>,
    init_expr: Option<Node>,
) {
    if is_function_pointer_declarator(decl) {
        let (name, _is_ptr) = parse_declarator_name(source, decl);
        if name.is_empty() {
            return;
        }
        let var_id = program.symbols.alloc_var_id();
        let span = node_span(program, ctx, span_node);
        program.symbols.add_variable(Variable {
            id: var_id,
            name: name.clone(),
            type_id,
            storage: storage_override.unwrap_or_else(|| storage_for(ctx, is_static)),
            fn_id: ctx.current_fn,
            param_index: None,
            span,
            is_pointer: true,
        });
        register_local(ctx, name, var_id);
        if let Some(init) = init_expr {
            if init.kind() == "initializer_list"
                && (is_array_type(program, type_id) || declarator_is_array(decl))
            {
                lower_fn_ptr_array_init(program, ctx, source, var_id, init);
            }
            extract_flow_from_expr(program, ctx, source, init, Some(var_id));
        }
        return;
    }

    if decl.kind() == "function_declarator" && !is_function_pointer_declarator(decl) {
        lower_function_decl(program, ctx, source, decl, type_id, is_static);
        return;
    }

    let (name, is_ptr) = parse_declarator_name(source, decl);
    if name.is_empty() {
        return;
    }
    let var_id = program.symbols.alloc_var_id();
    let span = node_span(program, ctx, span_node);
    program.symbols.add_variable(Variable {
        id: var_id,
        name: name.clone(),
        type_id,
        storage: storage_override.unwrap_or_else(|| storage_for(ctx, is_static)),
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: is_ptr,
    });
    register_local(ctx, name, var_id);
    // Constructor invocation spelled as a declaration: `Cls o(1, 2);`.
    // tree-sitter parks the argument list in init_declarator's `value`
    // field, so an argument_list "initializer" IS the ctor call.
    let ctor_args: Option<Node> = match init_expr {
        Some(n) if n.kind() == "argument_list" => Some(n),
        _ => span_node
            .children(&mut span_node.walk())
            .find(|c| c.kind() == "argument_list"),
    };
    if ctx.is_cpp
        && ctor_args.is_some()
        && span_node.kind() == "init_declarator"
        && ctx.current_fn.is_some()
    {
        if let Some(cls) = match program.types.get(type_id).desc.clone() {
            TypeDesc::Struct { name, .. } if !name.is_empty() && !name.starts_with("anon_") => {
                Some(name)
            }
            _ => None,
        } {
            let span = node_span(program, ctx, span_node);
            let mut call_args = collect_call_args(program, ctx, source, ctor_args);
            // The implicit `this` (param 0) points to the object being
            // constructed; shift explicit args to start at index 1.
            for v in &mut call_args.var_args {
                v.0 += 1;
            }
            call_args.var_args.insert(0, (0, var_id));
            emit_member_sites(
                program,
                ctx.current_fn.unwrap(),
                &cls,
                &trace_ir::MethodKind::Ctor,
                call_args,
                span,
            );
        }
    }
    if let Some(init) = init_expr {
        if init.kind() != "argument_list" {
            // A ctor argument list is not a value flowing into the object.
            if init.kind() == "initializer_list"
                && (is_array_type(program, type_id) || declarator_is_array(decl))
            {
                lower_fn_ptr_array_init(program, ctx, source, var_id, init);
            }
            extract_flow_from_expr(program, ctx, source, init, Some(var_id));
        }
    }
}

/// `ArrayFnMember` facts are only sound for array-typed tables (unknown-index
/// element access merges all members). Parking members of a plain struct
/// initializer into the variable node would let any field load observe every
/// member function regardless of field identity.
fn is_array_type(program: &Program, type_id: trace_ir::TypeId) -> bool {
    matches!(
        program.types.get(type_id).desc,
        trace_ir::TypeDesc::Array { .. }
    )
}

fn declarator_is_array(decl: Node) -> bool {
    if decl.kind() == "array_declarator" {
        return true;
    }
    let mut found = false;
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if declarator_is_array(child) {
            found = true;
            break;
        }
    }
    found
}

fn lower_fn_ptr_array_init(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    array: VarId,
    init: Node,
) {
    let mut cursor = init.walk();
    for child in init.children(&mut cursor) {
        if matches!(child.kind(), "(" | ")" | ",") {
            continue;
        }
        // Arrays of structs: `{ {TYPE, Fn}, ... }` or `{ {.init = Fn}, ... }`.
        // Recurse so element expressions nested in inner lists are visited.
        if child.kind() == "initializer_list" {
            lower_fn_ptr_array_init(program, ctx, source, array, child);
            continue;
        }
        if child.kind() == "initializer_pair" || child.kind() == "designated_initializer" {
            if let Some(value) = init_pair_value_node(child) {
                // `[i] = { ... }` with a nested *positional* element list has
                // no field info — park members via ArrayFnMember (sound blob).
                // Lists carrying field designators are handled precisely by
                // `lower_designated_initializer` (via `extract_flow_from_expr`)
                // so members stay bound to their own field.
                if value.kind() == "initializer_list" {
                    if !list_has_field_designators(value) {
                        lower_fn_ptr_array_init(program, ctx, source, array, value);
                    }
                } else {
                    push_array_fn_member(program, ctx, source, array, value);
                }
            }
            continue;
        }
        push_array_fn_member(program, ctx, source, array, child);
    }
}

fn init_pair_value_node(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| c.is_named() && c.kind() != "field_designator")
        .last()
}

/// True when any direct `initializer_pair`/`designated_initializer` child of
/// this list carries a `.field =` designator (as opposed to purely positional
/// contents).
fn list_has_field_designators(list: Node) -> bool {
    let mut cursor = list.walk();
    for c in list.children(&mut cursor) {
        if c.kind() != "initializer_pair" && c.kind() != "designated_initializer" {
            continue;
        }
        let mut inner = c.walk();
        for g in c.children(&mut inner) {
            if g.kind() == "field_designator" {
                return true;
            }
        }
    }
    false
}

fn push_array_fn_member(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    array: VarId,
    elem: Node,
) {
    if let Some(callee) = resolve_call_fn_arg(program, ctx, source, elem) {
        program
            .flow
            .push(FlowConstraint::ArrayFnMember { array, callee });
    }
}

fn lower_function_decl(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    decl: Node,
    ret_type: trace_ir::TypeId,
    is_static: bool,
) {
    let (name, _) = parse_declarator_name(source, decl);
    if name.is_empty() {
        return;
    }
    let name = ctx.qualify_decl(&name);
    let provisional_id = program.symbols.alloc_fn_id();
    let mut params = Vec::new();
    if let Some(params_node) = find_params(decl) {
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() == "parameter_declaration" {
                if let Some(var) = lower_parameter(
                    program,
                    ctx,
                    source,
                    param,
                    provisional_id,
                    params.len() as u32,
                ) {
                    params.push(var);
                }
            }
        }
    }
    let span = node_span(program, ctx, decl);
    let fn_id = program.symbols.add_function(Function {
        id: provisional_id,
        name,
        linkage: if is_static {
            Linkage::Internal
        } else {
            Linkage::External
        },
        return_type: ret_type,
        params,
        locals: Vec::new(),
        span,
        // Prototypes have no body: the range is the declaration itself.
        end_line: span.line,
        file: ctx.current_file,
        is_defined: false,
        param_type_ids: Vec::new(),
        is_virtual: false,
        is_final: false,
        is_cpp: ctx.is_cpp,
    });
    reassign_fn_id(program, provisional_id, fn_id);
}

fn reassign_fn_id(program: &mut Program, from: FnId, to: FnId) {
    if from == to {
        return;
    }
    let mut moved: Vec<VarId> = Vec::new();
    for var in &mut program.symbols.variables {
        if var.fn_id == Some(from) {
            var.fn_id = Some(to);
            moved.push(var.id);
        }
    }
    for cs in &mut program.symbols.call_sites {
        if cs.caller == from {
            cs.caller = to;
        }
    }
    // Re-pointed declarations supply the authoritative parameter list;
    // appending them onto the entry's existing params would double-count
    // prototype + definition parameters as distinct overload slots.
    if !moved.is_empty() {
        if let Some(func) = program.symbols.functions.iter_mut().find(|f| f.id == to) {
            func.params = moved;
        }
    }
}

fn walk_function_body(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    caller: FnId,
) {
    if ctx.ast_depth >= MAX_AST_WALK_DEPTH {
        if !ctx.ast_depth_warned {
            program.add_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: None,
                line: 0,
                message: format!(
                    "AST walk depth exceeded ({MAX_AST_WALK_DEPTH}); skipping deeper nodes"
                ),
                stage: "parse".into(),
            });
            ctx.ast_depth_warned = true;
        }
        return;
    }
    ctx.ast_depth += 1;
    match node.kind() {
        "declaration" => lower_declaration(program, ctx, source, node, None),
        // `using namespace X;` / `using X::f;` scoped to a function body.
        // Collected into `ctx` for the duration of the body walk only —
        // `lower_function` snapshots/restores around the walk, so a
        // function-body directive never leaks into other functions (a leak
        // could let the name ranking collapse away the correct in-scope
        // edge: an under-approximation).
        "using_declaration" if ctx.is_cpp => lower_using_declaration(ctx, source, node),
        "assignment_expression" => {
            extract_flow_from_expr(program, ctx, source, node, None);
        }
        "call_expression" => collect_call_at_node(program, ctx, source, node, caller),
        "lambda_expression" if ctx.is_cpp => {
            let _ = lower_lambda_expression(program, ctx, source, node);
            ctx.ast_depth = ctx.ast_depth.saturating_sub(1);
            return;
        }
        "return_statement" => collect_return_statement(program, ctx, source, node, caller),
        // C++ object lifecycle: constructor invocations and destructor runs.
        #[allow(clippy::collapsible_match)]
        "new_expression" if ctx.is_cpp => {
            // Skip if already handled by expr_to_rhs_flow (declaration init).
            if !ctx.handled_new_exprs.borrow().contains(&node.id()) {
                if let Some(cls) = new_expression_class(program, ctx, source, node) {
                    let args = node
                        .children(&mut node.walk())
                        .find(|c| c.kind() == "argument_list");
                    let span = node_span(program, ctx, node);
                    let mut call_args = collect_call_args(program, ctx, source, args);
                    // Shift explicit args by 1 so param 0 (`this`) does not
                    // collide with the first explicit argument.  We leave
                    // `this` unwired; the solver creates an imprecise summary
                    // node for it (sound over-approximation).
                    for v in &mut call_args.var_args {
                        v.0 += 1;
                    }
                    emit_member_sites(
                        program,
                        caller,
                        &cls,
                        &trace_ir::MethodKind::Ctor,
                        call_args,
                        span,
                    );
                }
            }
        }
        "delete_expression" if ctx.is_cpp => {
            let operand = node
                .children(&mut node.walk())
                .filter(|c| c.is_named())
                .last();
            if let Some(operand) = operand {
                if let Some(cls) = infer_static_class(program, ctx, source, operand) {
                    let span = node_span(program, ctx, node);
                    emit_member_sites(
                        program,
                        caller,
                        &cls,
                        &trace_ir::MethodKind::Dtor,
                        CallArgs::empty(),
                        span,
                    );
                }
            }
        }
        "field_initializer_list" if ctx.is_cpp => {
            lower_field_initializer_list(program, ctx, source, node, caller);
        }
        _ => {}
    }
    // `using namespace X;` / `using X::f;` are scoped to the enclosing block
    // in C++. A directive at function-body top level applies to the whole
    // body (restored when the root `compound_statement` exits, matching
    // `lower_function`'s snapshot); one inside an inner block (e.g. an
    // `if`/`for`/`while` body, itself a `compound_statement`) applies only to
    // that block. Snapshot before walking each block's children and restore
    // on exit so a nested directive never leaks to sibling blocks or the rest
    // of the function (a leak could let the ranking collapse away the correct
    // in-scope edge: an under-approximation).
    let is_block = node.kind() == "compound_statement";
    let using_nss_len = ctx.using_nss.len();
    let using_imports_len = ctx.using_name_imports.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_function_body(program, ctx, source, child, caller);
    }
    if is_block {
        ctx.using_nss.truncate(using_nss_len);
        ctx.using_name_imports.truncate(using_imports_len);
    }
    ctx.ast_depth = ctx.ast_depth.saturating_sub(1);
}

fn collect_call_at_node(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    caller: FnId,
) {
    let func = match node.child_by_field_name("function") {
        Some(f) => f,
        None => return,
    };
    let span = node_span(program, ctx, node);
    let return_dst = ctx.call_return_dst.borrow().get(&node.id()).copied();

    // ---- C++ member calls with statically-typed receivers ----
    // `recv.method(args)` / `p->method(args)` / explicit `x.~T()` /
    // virtual dispatch through base pointers. Receivers we cannot type
    // fall through to the generic indirect handling below (vtable-slot
    // style flow resolution), preserving soundness.
    if ctx.is_cpp && func.kind() == "field_expression" {
        let op_is_member_access = func
            .children(&mut func.walk())
            .any(|c| c.kind() == "." || c.kind() == "->");
        if op_is_member_access {
            if let Some(field) = func.child_by_field_name("field") {
                if matches!(
                    field.kind(),
                    "field_identifier" | "destructor_name" | "template_type" | "template_method"
                ) {
                    if let Some(recv) = func.child_by_field_name("argument") {
                        if let Some(cls) = infer_static_class(program, ctx, source, recv) {
                            let kind = if field.kind() == "destructor_name" {
                                trace_ir::MethodKind::Dtor
                            } else {
                                trace_ir::MethodKind::Named(strip_template_args(
                                    &normalize_qualified(node_text(source, &field)),
                                ))
                            };
                            let field_name = strip_template_args(&normalize_qualified(node_text(
                                source, &field,
                            )));
                            let has_method =
                                !member_targets_upward(program, &cls, &kind).is_empty();
                            if has_method {
                                let call_args = collect_call_args(
                                    program,
                                    ctx,
                                    source,
                                    node.child_by_field_name("arguments"),
                                );
                                emit_member_sites(program, caller, &cls, &kind, call_args, span);
                                return;
                            }
                            // Functor field: `h->cb()` where `cb` is a class
                            // with `operator()`, not a method named `cb`.
                            if let Some(field_cls) = infer_static_class(program, ctx, source, func)
                            {
                                let op = trace_ir::MethodKind::Named("operator()".to_string());
                                if !member_targets_upward(program, &field_cls, &op).is_empty() {
                                    let call_args = collect_call_args(
                                        program,
                                        ctx,
                                        source,
                                        node.child_by_field_name("arguments"),
                                    );
                                    emit_member_sites(
                                        program, caller, &field_cls, &op, call_args, span,
                                    );
                                    return;
                                }
                            }
                            // Callable data members (`std::function`, fn-ptr
                            // fields) are not methods: fall through to the
                            // generic field-load path so they resolve like
                            // C function pointers.
                            if !class_has_data_field(program, &cls, &field_name) {
                                let call_args = collect_call_args(
                                    program,
                                    ctx,
                                    source,
                                    node.child_by_field_name("arguments"),
                                );
                                emit_member_sites(program, caller, &cls, &kind, call_args, span);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    // Bare `method(args)` inside a C++ method is implicit `this->method`.
    // Must run before name lookup, which would otherwise synthesize an
    // unqualified external stub (`OnEvent` vs `Plugin::OnEvent`).
    if ctx.is_cpp && func.kind() == "identifier" {
        if let Some(cls) = ctx.class_ctx.as_ref().map(|c| c.qual_name.clone()) {
            let short = strip_template_args(&normalize_qualified(node_text(source, &func)));
            if lookup_var(ctx, program, &short).is_none() {
                let kind = trace_ir::MethodKind::Named(short);
                if !member_targets_upward(program, &cls, &kind).is_empty() {
                    let call_args = collect_call_args(
                        program,
                        ctx,
                        source,
                        node.child_by_field_name("arguments"),
                    );
                    emit_member_sites(program, caller, &cls, &kind, call_args, span);
                    return;
                }
            }
        }
        // Functor / callable object: `f()` where `f` has `operator()`.
        if let Some(v) = lookup_var(ctx, program, node_text(source, &func)) {
            if let Some(cls) = var_static_class(program, v) {
                let kind = trace_ir::MethodKind::Named("operator()".to_string());
                if !member_targets_upward(program, &cls, &kind).is_empty() {
                    let call_args = collect_call_args(
                        program,
                        ctx,
                        source,
                        node.child_by_field_name("arguments"),
                    );
                    emit_member_sites(program, caller, &cls, &kind, call_args, span);
                    return;
                }
            }
        }
    }

    let (callee_name, mut is_direct, callee_var) =
        resolve_callee_with_loads(program, ctx, source, func);
    // Macro-expansion artifacts (stringified log fragments and similar
    // token soup) surface as call sites whose "callee" text embeds string
    // literals; real callees are plain identifiers or field paths. Note
    // that whitespace is legitimate here — preprocessed text keeps token
    // spacing (`tbl [ i ]->fn`) — so only quotes are rejected.
    if callee_name.contains('"') {
        return;
    }
    if !is_direct && callee_var.is_none() {
        is_direct = resolve_function_named(program, ctx, &callee_name).is_some();
    }
    if !is_direct && is_likely_macro_callee(&callee_name) {
        return;
    }
    let collected = collect_call_args(program, ctx, source, node.child_by_field_name("arguments"));
    let argc = collected.argc as usize;
    let CallArgs {
        var_args,
        fn_args,
        addr_of_member_args,
        argc: _,
        arg_desc,
    } = collected;

    // ---- Resolution ----
    // C preserves the exact legacy semantics: one scoped lookup, zero or
    // one target. C++ resolves over the candidate set with:
    //   * unqualified identifiers: namespace-aware lookup — ordinary lookup
    //     (global, enclosing namespaces, `using namespace` directives)
    //     merged with ADL namespaces drawn from the argument types, so
    //     `std::swap(a, b)` and cross-namespace free functions resolve;
    //   * qualified names (`ns::f`): exact lookup over the candidates.
    // Arity filters the set; an arity-filtered empty set falls back to every
    // candidate so varargs declarations keep their targets. When several
    // same-arity overloads survive, rank them by argument/parameter types so
    // `f(1)` picks `f(int)` rather than emitting every overload.
    let chosen: Vec<FnId> = if !ctx.is_cpp {
        program
            .symbols
            .resolve_function_in_scope(&callee_name, Some(ctx.current_file))
            .into_iter()
            .collect()
    } else if callee_var.is_none() {
        let candidates = if is_bare_callee_node(func) {
            resolve_cpp_name_candidates(program, ctx, &callee_name, &arg_desc)
        } else {
            program
                .symbols
                .resolve_function_candidates(&callee_name, Some(ctx.current_file))
        };
        let by_arity: Vec<FnId> = candidates
            .iter()
            .copied()
            .filter(|&f| program.symbols.function(f).params.len() == argc)
            .collect();
        if by_arity.is_empty() {
            candidates
        } else if by_arity.len() > 1 {
            rank_overloads(program, &by_arity, &arg_desc)
        } else {
            by_arity
        }
    } else {
        Vec::new()
    };

    match chosen.len() {
        0 => {
            let call_id = program.symbols.alloc_call_id();
            program.symbols.call_sites.push(CallSite {
                id: call_id,
                caller,
                callee_name,
                callee_var,
                callee_fn_id: None,
                var_args,
                fn_args,
                addr_of_member_args,
                span,
                is_direct,
                receiver_class: None,
                return_dst,
            });
        }
        1 => {
            let call_id = program.symbols.alloc_call_id();
            program.symbols.call_sites.push(CallSite {
                id: call_id,
                caller,
                callee_name,
                callee_var,
                callee_fn_id: Some(chosen[0]),
                var_args,
                fn_args,
                addr_of_member_args,
                span,
                is_direct: true,
                receiver_class: None,
                return_dst,
            });
        }
        n => {
            // Overload tie (same arity, types undecidable here): emit one
            // site per candidate — a bounded, explicit may-approximation.
            for t in chosen.iter().copied() {
                let call_id = program.symbols.alloc_call_id();
                program.symbols.call_sites.push(CallSite {
                    id: call_id,
                    caller,
                    callee_name: if n > 1 && t != chosen[0] {
                        format!("{}::{}", callee_name, program.symbols.function(t).id.0)
                    } else {
                        callee_name.clone()
                    },
                    callee_var,
                    callee_fn_id: Some(t),
                    var_args: var_args.clone(),
                    fn_args: fn_args.clone(),
                    addr_of_member_args: addr_of_member_args.clone(),
                    span,
                    is_direct: true,
                    receiver_class: None,
                    return_dst,
                });
            }
        }
    }
}

/// Collected call arguments: value args as `(index, var)`, function
/// arguments (address-of-function) as `(index, fn)`, positions recorded
/// as `&base.member` addresses, and the syntactic argument count
/// (literals included — needed for arity filtering).
struct CallArgs {
    var_args: Vec<(u32, VarId)>,
    fn_args: Vec<(u32, FnId)>,
    addr_of_member_args: Vec<u32>,
    argc: u32,
    /// One slot per argument position (aligned with `argc`), holding the
    /// best-effort static type of the passed expression (literals, casts,
    /// variable types, pointer decay). Used to rank same-arity C++ overloads.
    arg_desc: Vec<TypeDesc>,
}

impl CallArgs {
    fn empty() -> Self {
        Self {
            var_args: Vec::new(),
            fn_args: Vec::new(),
            addr_of_member_args: Vec::new(),
            argc: 0,
            arg_desc: Vec::new(),
        }
    }
}

/// Collect call arguments once; shared by the member-call, overload and
/// legacy paths.
fn collect_call_args(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    args_node: Option<Node>,
) -> CallArgs {
    let mut var_args = Vec::new();
    let mut fn_args = Vec::new();
    let mut addr_of_member_args = Vec::new();
    let mut arg_desc = Vec::new();
    let mut arg_index = 0u32;
    if let Some(args_node) = args_node {
        for arg in args_node.children(&mut args_node.walk()) {
            if arg.kind() != "(" && arg.kind() != ")" && arg.kind() != "," {
                let adesc = arg_expr_type(program, ctx, source, arg);
                // Parameter positions are syntactic: every argument slot
                // advances the index even when the expression yields no IR
                // variable (literals, sizeof, casts). Compressing indices
                // would mis-attribute later arguments to earlier formals
                // (e.g. `memcpy_s(d, sizeof(*d), s, n)` recording `s` at
                // position 1) and corrupt both interprocedural wiring and
                // function-model effects.
                if let Some(v) = resolve_expr_var(program, ctx, source, arg) {
                    // A field/subscript argument passes the *value* stored in
                    // that memory (e.g. `take(g_h.h, 0)` passes the fn-ptr in
                    // `g_h.h`). `resolve_expr_var` yields the base object, so
                    // materialize a load temp and pass that instead.
                    if matches!(arg.kind(), "field_expression" | "subscript_expression") {
                        let temp = alloc_ret_temp(program, ctx, arg);
                        if let Some(flow) = expr_to_rhs_flow(program, ctx, source, arg, temp) {
                            program.flow.push(flow);
                            var_args.push((arg_index, temp));
                            arg_desc.push(adesc);
                            arg_index += 1;
                            continue;
                        }
                    }
                    var_args.push((arg_index, v));
                    // `&base.member` / `&arr[i]` resolve to the base
                    // variable; flag the position so function-model alias
                    // effects can refuse to copy the whole container.
                    if is_addr_of_member(source, arg) {
                        addr_of_member_args.push(arg_index);
                    }
                } else if let Some(s) = string_literal_value(source, arg) {
                    let temp = alloc_ret_temp(program, ctx, arg);
                    program.flow.push(FlowConstraint::StringConst {
                        dst: temp,
                        value: s,
                    });
                    var_args.push((arg_index, temp));
                } else if let Some(gep) = addr_of_field_path(program, ctx, source, arg) {
                    var_args.push((arg_index, gep));
                } else if let Some(fn_id) = resolve_call_fn_arg(program, ctx, source, arg) {
                    fn_args.push((arg_index, fn_id));
                }
                arg_desc.push(adesc);
                arg_index += 1;
            }
        }
    }
    CallArgs {
        var_args,
        fn_args,
        addr_of_member_args,
        argc: arg_index,
        arg_desc,
    }
}

/// Best-effort static type of a call argument expression, for overload
/// ranking. Unknown for anything unresolvable.
fn arg_expr_type(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> TypeDesc {
    match node.kind() {
        "cast_expression" => {
            if let Some(ty) = node.child_by_field_name("type") {
                // `type_desc_from_node` reads scalar text with `contains`,
                // so a pointer cast would classify the value as a scalar and
                // exactly match the wrong overload (`(float*)p` -> $f(float)$).
                // Count the pointer levels on the type text and wrap that
                // many times, so `(int*)p` is Ptr(Int) and `(int**)p` is
                // Ptr(Ptr(Int)) — never one level short, which would bind
                // `(char**)p` to a `f(char*)` overload.
                let text = node_text(source, &ty);
                if text.contains('*') || text.contains('[') {
                    let wraps = text.matches('*').count() + usize::from(text.contains('['));
                    let mut desc = type_desc_from_node(program, ctx, source, ty);
                    for _ in 0..wraps {
                        desc = TypeDesc::Ptr(Box::new(desc));
                    }
                    return desc;
                }
                return type_desc_from_node(program, ctx, source, ty);
            }
            TypeDesc::Unknown
        }
        "number_literal" => number_literal_desc(node_text(source, &node)),
        "char_literal" => TypeDesc::Char,
        "true" | "false" => TypeDesc::Bool,
        "string_literal" => TypeDesc::Ptr(Box::new(TypeDesc::Char)),
        "nullptr" => TypeDesc::Ptr(Box::new(TypeDesc::Unknown)),
        _ => {
            if let Some(v) = resolve_expr_var(program, ctx, source, node) {
                let tid = program.symbols.variable(v).type_id;
                let desc = program.types.get(tid).desc.clone();
                // Field/subscript args pass the *stored value*: a pointer
                // member is passed as its pointee (array decay), and a
                // struct-by-value member as the struct.
                if matches!(node.kind(), "field_expression" | "subscript_expression") {
                    match &desc {
                        TypeDesc::Ptr(inner) if node.kind() == "subscript_expression" => {
                            // `arr[i]`: the element value is the pointee.
                            return (**inner).clone();
                        }
                        TypeDesc::Ptr(inner)
                            if matches!(
                                **inner,
                                TypeDesc::Struct { .. } | TypeDesc::Union { .. }
                            ) =>
                        {
                            // `p->field`: the pointee is the *receiver*, not
                            // the member's value — classifying the arg as the
                            // struct mis-ranks `f(p->field)` with `f(int)`
                            // overloads exactly like the `s.name` case.
                            return TypeDesc::Unknown;
                        }
                        TypeDesc::Ptr(inner) => return (**inner).clone(),
                        TypeDesc::Struct { .. } | TypeDesc::Union { .. } => {
                            // `s.name`: `resolve_expr_var` only recovers the
                            // receiver, so `Struct(s)` would be a confident
                            // wrong pick for a scalar member. Degrade so
                            // ranking keeps the full candidate set.
                            return TypeDesc::Unknown;
                        }
                        _ => return desc,
                    }
                }
                return desc;
            }
            TypeDesc::Unknown
        }
    }
}

fn number_literal_desc(text: &str) -> TypeDesc {
    let lower = text.trim().to_ascii_lowercase();
    let is_hex = lower.starts_with("0x") || lower.starts_with("0b");
    let is_float = lower.contains('.') || ((lower.contains('e') || lower.contains('p')) && !is_hex);
    if is_float {
        if lower.ends_with('f') || lower.ends_with("lf") {
            TypeDesc::Float
        } else {
            TypeDesc::Double
        }
    } else if lower.ends_with('u') || lower.ends_with('l') {
        TypeDesc::Long
    } else {
        TypeDesc::Int
    }
}

/// Pick among same-arity C++ candidates using argument-type ranking. Exact
/// matches (score 0) beat every conversion; a unique best winner is chosen.
/// Any ambiguity (ties, no exact match, unresolvable args) keeps the whole
/// arity-set — the may-approximation — rather than guessing.
fn rank_overloads(program: &Program, candidates: &[FnId], arg_desc: &[TypeDesc]) -> Vec<FnId> {
    let score = |f: FnId| -> usize {
        let params = &program.symbols.function(f).params;
        params
            .iter()
            .enumerate()
            .map(|(i, pv)| {
                let pdesc = program
                    .types
                    .get(program.symbols.variable(*pv).type_id)
                    .desc
                    .clone();
                let adesc = arg_desc.get(i).cloned().unwrap_or(TypeDesc::Unknown);
                param_match_rank(&adesc, &pdesc)
            })
            .sum()
    };
    let ranked: Vec<(FnId, usize)> = candidates.iter().copied().map(|f| (f, score(f))).collect();
    let min = ranked.iter().map(|(_, s)| *s).min().unwrap_or(0);
    let second = ranked
        .iter()
        .filter(|(_, s)| *s != min)
        .map(|(_, s)| *s)
        .min();
    match (min, second) {
        (0, Some(s2)) if s2 > 0 => ranked
            .into_iter()
            .filter(|(_, s)| *s == min)
            .map(|(f, _)| f)
            .collect(),
        _ => candidates.to_vec(),
    }
}

fn param_match_rank(arg: &TypeDesc, param: &TypeDesc) -> usize {
    use ScalarKind as S;
    if arg == param {
        return 0;
    }
    match (arg, param) {
        (TypeDesc::Unknown, _) | (_, TypeDesc::Unknown) => 0,
        (a, p) if a.is_pointer_like() || p.is_pointer_like() => {
            if a.is_pointer_like() && p.is_pointer_like() {
                2
            } else {
                4
            }
        }
        (a, p) => match (a.scalar_kind(), p.scalar_kind()) {
            (S::Float | S::Double, S::Float | S::Double) => 1,
            (S::Float | S::Double, S::Int | S::Short | S::Long | S::LongLong | S::Char) => 3,
            (S::Int | S::Short | S::Long | S::LongLong | S::Char, S::Float | S::Double) => 2,
            (_, S::Bool) | (S::Bool, _) => 3,
            _ => 1,
        },
    }
}

/// True when the callee is a bare unqualified identifier — the only shape
/// to which ADL and `using`/`using namespace` lookup apply. Qualified
/// identifiers (`ns::f`) and template/field/pointer callees keep exact
/// lookup.
///
/// `template_function` is included because `resolve_callee` already strips
/// the `<...>` argument list, so the `callee_name` passed to
/// `resolve_cpp_name_candidates` is the bare base name (e.g. `GetNumber`,
/// not `GetNumber<int>`), which correctly indexes into the `base_by_name`
/// bucket.
fn is_bare_callee_node(func: tree_sitter::Node) -> bool {
    matches!(func.kind(), "identifier" | "template_function")
}

/// ADL (argument-dependent / Koenig) namespaces: the enclosing namespaces
/// of the argument types. For a `std::vector<int>` argument the associated
/// namespace is `std`; for a bare global struct it is the global namespace
/// (dropped). Pointer/array/element layers are peeled. May-analysis:
/// whenever an argument's struct type is visible in multiple namespaces, all
/// of them are candidates.
fn adl_namespaces(arg_desc: &[TypeDesc]) -> Vec<String> {
    let mut namespaces: Vec<String> = Vec::new();
    fn collect(desc: &TypeDesc, out: &mut Vec<String>) {
        let inner = match desc {
            TypeDesc::Ptr(i) | TypeDesc::Array { elem: i, .. } => i.as_ref(),
            TypeDesc::Struct { name, .. } | TypeDesc::Union { name, .. } => {
                // Derive the enclosing namespace from the qualified tag.
                // A leading `::` (e.g. `::kit::Widget`) is the global-scope
                // marker, not part of the namespace name, so strip it before
                // querying (`functions_in_namespace` treats `kit` and `::kit`
                // as interchangeable, so ADL must too).
                // For nested qualified tags like `N::Outer::Inner`, add every
                // prefix (`N::Outer` and `N`): the intermediate segments may
                // be classes (not namespaces), and in real C++ only enclosing
                // namespaces contribute to ADL — but without type information
                // we conservatively add all prefixes (sound over-approximation).
                let stripped = name.trim_start_matches("::");
                let mut prefix_end = stripped.find("::");
                while let Some(sep) = prefix_end {
                    let ns = stripped[..sep].to_string();
                    if !out.iter().any(|n| n == &ns) {
                        out.push(ns);
                    }
                    prefix_end = stripped[sep + 2..].find("::").map(|i| i + sep + 2);
                }
                return;
            }
            _ => return,
        };
        collect(inner, out);
    }
    for d in arg_desc {
        collect(d, &mut namespaces);
    }
    namespaces
}

/// Best-effort namespace-aware candidate set for an unqualified C++ call:
/// ordinary lookup namespaces plus ADL namespaces plus explicitly imported
/// `using X::f;` members, deduplicated. The caller applies arity filtering
/// + overload ranking.
///
/// Ordinary lookup follows the C++ rule: check enclosing namespaces
/// innermost-to-outermost, stopping at the first that declares a function
/// with the requested base name (standard hiding rule).  `using namespace`
/// directives are checked *after* enclosing namespaces and always add
/// candidates without hiding (they make the named namespace's declarations
/// visible in the scope of the directive).  ADL namespaces and
/// `using X::f;` imports are merged last (they add candidates, never
/// replace).
fn resolve_cpp_name_candidates(
    program: &Program,
    ctx: &LowerContext,
    base: &str,
    arg_desc: &[TypeDesc],
) -> Vec<FnId> {
    let mut out: Vec<FnId> = Vec::new();
    // Phase 1: enclosing namespaces (innermost to outermost).  Stop at the
    // first scope that declares any function with `base` — standard C++
    // hiding rule (inner declarations shadow outer ones).
    let present: Vec<&str> = ctx.ns_stack.iter().flatten().map(String::as_str).collect();
    for i in (0..=present.len()).rev() {
        let ns = present[..i].join("::");
        let mut found = program.symbols.functions_in_namespace(&ns, base);
        if i == 0 {
            // The global rung: also fold in file-scoped internal linkage
            // (static / header-static) entries, which are declarations in
            // the global namespace.  Consulting them here — not ahead of the
            // whole walk — lets them participate in the hiding rule: an
            // inner-namespace declaration shadows a global/static, and a
            // file-static shadows an extern global of the same name.
            for id in program
                .symbols
                .resolve_function_candidates(base, Some(ctx.current_file))
            {
                if !found.contains(&id) {
                    found.push(id);
                }
            }
        }
        if !found.is_empty() {
            for id in found {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
            break;
        }
    }
    // Phase 2: using namespace directives (always add, never hide — they
    // make the named namespace's declarations visible in the directive's
    // scope, alongside any enclosing-namespace candidates).
    for ns in &ctx.using_nss {
        for id in program.symbols.functions_in_namespace(ns, base) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    // Phase 3: ADL namespaces add candidates without replacing.
    for ns in adl_namespaces(arg_desc) {
        for id in program.symbols.functions_in_namespace(&ns, base) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    // Phase 4: `using X::f;` imports: the exact qualified entry may not
    // fall under any of the ordinary/ADL namespaces above (e.g. nested
    // `X::Y::f`).
    for (import_base, import_qual) in &ctx.using_name_imports {
        if import_base == base {
            // Resolve within the current file's scope so a `static`
            // definition (or header-static) imported via `using X::f;`
            // resolves instead of degrading to an external stub.
            for id in program
                .symbols
                .resolve_function_candidates(import_qual, Some(ctx.current_file))
            {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// Emit call sites for `cls::member` — the override set across derived
/// classes, found by walking up the inheritance chain to the nearest
/// declaring class and expanding its subclasses.
fn emit_member_sites(
    program: &mut Program,
    caller: FnId,
    cls: &str,
    kind: &trace_ir::MethodKind,
    args: CallArgs,
    span: Span,
) {
    let CallArgs {
        var_args,
        fn_args,
        addr_of_member_args,
        argc,
        arg_desc: _,
    } = args;
    let targets = filter_targets_by_argc(
        program,
        member_targets_upward(program, cls, kind),
        argc as usize,
    );
    let display = kind.name_on(cls);
    if targets.is_empty() {
        // Unknown method: keep an unresolved site; the solver synthesizes
        // an external entry (mirrors plain-identifier C behavior).
        let call_id = program.symbols.alloc_call_id();
        program.symbols.call_sites.push(CallSite {
            id: call_id,
            caller,
            callee_name: display,
            callee_var: None,
            callee_fn_id: None,
            var_args,
            fn_args,
            addr_of_member_args,
            span,
            is_direct: false,
            receiver_class: Some(cls.to_string()),
            return_dst: None,
        });
        return;
    }
    for t in targets {
        let (call_id, name) = {
            let id = program.symbols.alloc_call_id();
            let nm = program.symbols.function(t).name.clone();
            (id, nm)
        };
        program.symbols.call_sites.push(CallSite {
            id: call_id,
            caller,
            callee_name: name,
            callee_var: None,
            callee_fn_id: Some(t),
            var_args: var_args.clone(),
            fn_args: fn_args.clone(),
            addr_of_member_args: addr_of_member_args.clone(),
            span,
            is_direct: true,
            receiver_class: Some(cls.to_string()),
            return_dst: None,
        });
    }
}

/// Constructor member-initializer lists: `Derived() : Base(1, 2), sub_(3) {}`.
/// A name matching a direct base constructs that base; anything else
/// constructs the declared class of the data member.
fn lower_field_initializer_list(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    caller: FnId,
) {
    let Some(cc) = ctx.class_ctx.clone() else {
        return;
    };
    let cls = cc.qual_name;
    let bases = program.bases_of(&cls);
    let cls_type = program
        .types
        .type_id_by_tag(&cls, trace_ir::TypeKind::Struct);
    for fi in node.children(&mut node.walk()) {
        if fi.kind() != "field_initializer" {
            continue;
        }
        let Some(name_node) = fi
            .children(&mut fi.walk())
            .find(|c| matches!(c.kind(), "field_identifier" | "identifier"))
        else {
            continue;
        };
        let fname = normalize_qualified(node_text(source, &name_node));
        let target_cls: Option<String> = bases
            .iter()
            .find(|b| last_segment_of(b) == fname)
            .cloned()
            .or_else(|| {
                let fid = cls_type?;
                let info = program.types.get(fid);
                let (_, fl) = info.layout.fields.iter().find(|(_, f)| f.name == fname)?;
                match program.types.get(fl.type_id).desc.clone() {
                    TypeDesc::Struct { name, .. } => Some(name),
                    _ => None,
                }
            });
        if let Some(target) = target_cls {
            let args = fi
                .children(&mut fi.walk())
                .find(|c| c.kind() == "argument_list");
            let span = node_span(program, ctx, fi);
            let call_args = collect_call_args(program, ctx, source, args);
            emit_member_sites(
                program,
                caller,
                &target,
                &trace_ir::MethodKind::Ctor,
                call_args,
                span,
            );
        }
    }
}

fn last_segment_of(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn node_has_compound_body(node: Node) -> bool {
    node.children(&mut node.walk())
        .any(|c| c.kind() == "compound_statement")
}

fn class_has_data_field(program: &Program, cls: &str, field: &str) -> bool {
    class_field_desc(program, cls, field).is_some()
}

/// Type of `field` on `cls` or a base (instance-insensitive layout).
fn class_field_desc(program: &Program, cls: &str, field: &str) -> Option<TypeDesc> {
    let mut queue = std::collections::VecDeque::new();
    let mut seen = std::collections::BTreeSet::new();
    queue.push_back(cls.to_string());
    seen.insert(cls.to_string());
    while let Some(cur) = queue.pop_front() {
        if let Some(tid) = program
            .types
            .type_id_by_tag(&cur, trace_ir::TypeKind::Struct)
        {
            let info = program.types.get(tid);
            if let Some((_, fl)) = info.layout.fields.iter().find(|(_, f)| f.name == field) {
                return Some(program.types.get(fl.type_id).desc.clone());
            }
        }
        for base in program.bases_of(&cur) {
            if seen.insert(base.clone()) {
                queue.push_back(base);
            }
        }
    }
    None
}

fn class_field_static_class(program: &Program, cls: &str, field: &str) -> Option<String> {
    class_name_of_desc(&class_field_desc(program, cls, field)?)
}

/// Explicit (non-`this`) parameter count. `None` means the prototype listed
/// no parameters, so arity is unknown and the candidate must be kept.
fn method_explicit_arity(program: &Program, fid: FnId) -> Option<usize> {
    let f = program.symbols.function(fid);
    if f.params.is_empty() {
        return None;
    }
    let first = program.symbols.variable(f.params[0]);
    let n = f.params.len();
    Some(if first.name == "this" { n - 1 } else { n })
}

fn arity_compatible(expected: Option<usize>, got: Option<usize>) -> bool {
    match (expected, got) {
        (None, _) | (_, None) => true,
        (Some(a), Some(b)) => a == b,
    }
}

fn filter_targets_by_argc(program: &Program, targets: Vec<FnId>, argc: usize) -> Vec<FnId> {
    let by_arity: Vec<FnId> = targets
        .iter()
        .copied()
        .filter(|&t| arity_compatible(Some(argc), method_explicit_arity(program, t)))
        .collect();
    if by_arity.is_empty() {
        targets
    } else {
        by_arity
    }
}

/// Lower a C++ lambda to a synthetic function (`$lambda@line:col` under the
/// enclosing function). Captures are unmodeled; the body is walked as a
/// nested function so inner calls participate in the call graph. Repeated
/// lowering of the same node reuses the first FnId.
fn lower_lambda_expression(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> Option<FnId> {
    let span = node_span(program, ctx, node);
    let owner = ctx
        .current_fn
        .map(|f| program.symbols.function(f).name.clone())
        .unwrap_or_else(|| "<tu>".to_string());
    let name = format!("{owner}::$lambda{}:{}", span.line, span.col);
    if let Some(existing) = program
        .symbols
        .resolve_function_in_scope(&name, Some(ctx.current_file))
    {
        return Some(existing);
    }
    let provisional_id = program.symbols.alloc_fn_id();
    let mut params = Vec::new();
    if let Some(params_node) = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "parameter_list" || c.kind() == "abstract_function_declarator")
        .and_then(|n| {
            if n.kind() == "parameter_list" {
                Some(n)
            } else {
                n.children(&mut n.walk())
                    .find(|c| c.kind() == "parameter_list")
            }
        })
    {
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() == "parameter_declaration" {
                if let Some(var) = lower_parameter(
                    program,
                    ctx,
                    source,
                    param,
                    provisional_id,
                    params.len() as u32,
                ) {
                    params.push(var);
                }
            }
        }
    }
    let end_line = node_end_line(program, ctx, node, span);
    let fn_id = program.symbols.add_function(trace_ir::Function {
        id: provisional_id,
        name,
        linkage: trace_ir::Linkage::Internal,
        return_type: program.types.int(),
        params: params.clone(),
        locals: Vec::new(),
        span,
        end_line,
        file: ctx.current_file,
        is_defined: true,
        param_type_ids: Vec::new(),
        is_virtual: false,
        is_final: false,
        is_cpp: true,
    });
    reassign_fn_id(program, provisional_id, fn_id);
    let saved_fn = ctx.current_fn;
    let saved_locals = ctx.locals.clone();
    ctx.current_fn = Some(fn_id);
    ctx.locals.clear();
    for &param in &params {
        if let Some(v) = program.symbols.variable_by_id(param) {
            ctx.locals.insert(v.name.clone(), param);
        }
    }
    if let Some(body) = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "compound_statement")
    {
        walk_function_body(program, ctx, source, body, fn_id);
    }
    ctx.current_fn = saved_fn;
    ctx.locals = saved_locals;
    Some(fn_id)
}

/// Walk UP the inheritance chain from `cls` until some class declares the
/// member. Non-virtual declarations (and ctors) resolve to exactly the
/// declaring entries; `virtual` ones — and destructors, where
/// delete-through-base is the dominant pattern — expand downward through
/// the subclass closure as the dynamic-dispatch target set.
fn member_targets_upward(program: &Program, cls: &str, kind: &trace_ir::MethodKind) -> Vec<FnId> {
    let declared_on = |c: &str| -> Vec<FnId> { program.symbols.functions_named(&kind.name_on(c)) };
    let mut queue = std::collections::VecDeque::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    queue.push_back(cls.to_string());
    seen.insert(cls.to_string());
    while let Some(cur) = queue.pop_front() {
        let own = declared_on(&cur);
        if !own.is_empty() {
            let virtual_dispatch =
                kind.is_destructor() || own.iter().any(|t| program.symbols.function(*t).is_virtual);
            return if virtual_dispatch {
                // Expand from the *static* type so `final` classes/methods
                // cut off sibling and descendant overrides.
                program.method_targets(cls, kind)
            } else {
                own
            };
        }
        for base in program.bases_of(&cur) {
            if seen.insert(base.clone()) {
                queue.push_back(base);
            }
        }
    }
    let down = program.method_targets(cls, kind);
    if !down.is_empty() {
        return down;
    }
    Vec::new()
}

/// Static class of a receiver expression, when inferable from declared
/// types (`this`, locals/globals, fields along typed chains, casts, news).
fn infer_static_class(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<String> {
    let node = peel_expression(node);
    match node.kind() {
        "this" => ctx.class_ctx.as_ref().map(|c| c.qual_name.clone()),
        "identifier" => {
            let name = node_text(source, &node);
            if let Some(v) = lookup_var(ctx, program, name) {
                return var_static_class(program, v);
            }
            // Bare `plugin_->OnEvent()` inside a method is implicit
            // `this->plugin_`; locals/params already lost, so look the
            // name up as a data member of the enclosing class (and bases).
            let cls = ctx.class_ctx.as_ref()?.qual_name.clone();
            class_field_static_class(program, &cls, name)
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            if op.as_deref() == Some("*") {
                let arg = node.named_child(0)?;
                return infer_static_class(program, ctx, source, arg);
            }
            None
        }
        "field_expression" => {
            let base = node.child_by_field_name("argument")?;
            let field = node.child_by_field_name("field")?;
            let base_cls = infer_static_class(program, ctx, source, base)?;
            let fname = normalize_qualified(node_text(source, &field));
            class_field_static_class(program, &base_cls, &fname)
        }
        "cast_expression" => {
            let type_node = node.child_by_field_name("type")?;
            let raw = normalize_qualified(node_text(source, &type_node));
            let stripped = strip_template_args(&raw);
            let qualified = if stripped.contains("::") {
                stripped
            } else {
                ctx.qualify(&stripped)
            };
            if program
                .types
                .type_id_by_tag(&qualified, trace_ir::TypeKind::Struct)
                .is_some()
            {
                Some(qualified)
            } else {
                None
            }
        }
        "new_expression" => new_expression_class(program, ctx, source, node),
        _ => None,
    }
}

fn var_static_class(program: &Program, v: VarId) -> Option<String> {
    let var = program.symbols.variable(v);
    class_name_of_desc(&program.types.get(var.type_id).desc)
}

/// Peel `Ptr` layers (including references, which lower as pointers) to a
/// class/struct tag. `shared_ptr<T>` interned as `Ptr(Struct{T})` and
/// `T &` / `T *` all yield `T`.
fn class_name_of_desc(desc: &TypeDesc) -> Option<String> {
    match desc {
        TypeDesc::Struct { name, .. } => Some(name.clone()),
        TypeDesc::Ptr(inner) => class_name_of_desc(inner),
        _ => None,
    }
}

/// The constructed class spelled in a `new T(...)` expression.
fn new_expression_class(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<String> {
    let _ = program;
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "qualified_identifier" | "type_identifier" | "template_type" => {
                let raw = normalize_qualified(node_text(source, &child));
                let stripped = strip_template_args(&raw);
                let qualified = if stripped.contains("::") {
                    stripped
                } else {
                    ctx.qualify(&stripped)
                };
                return Some(qualified);
            }
            _ => {}
        }
    }
    None
}

fn extract_flow_from_expr(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    assign_target: Option<VarId>,
) {
    if node.kind() == "assignment_expression" {
        let lhs = peel_expression(
            node.child_by_field_name("left")
                .or_else(|| node.named_child(0))
                .unwrap(),
        );
        let rhs = node
            .child_by_field_name("right")
            .or_else(|| node.named_child(1))
            .unwrap();
        if is_deref_lhs(source, lhs) {
            if let Some(arg) = deref_operand(lhs) {
                if let Some(ptr) = resolve_lvalue_var(program, ctx, source, arg) {
                    if let Some(src) = expr_to_store_src(program, ctx, source, rhs) {
                        program.flow.push(FlowConstraint::Store { dst: ptr, src });
                    } else if rhs.kind() == "call_expression" {
                        if let Some(callee_name) = resolve_direct_call(program, ctx, source, rhs) {
                            let ret_temp = alloc_ret_temp(program, ctx, node);
                            emit_call_return(program, ctx, rhs, ret_temp, callee_name);
                            program.flow.push(FlowConstraint::Store {
                                dst: ptr,
                                src: ret_temp,
                            });
                        }
                    }
                }
            }
        } else if lhs.kind() == "field_expression" {
            emit_field_store(program, ctx, source, lhs, rhs);
        } else if let Some(dst) = resolve_lvalue_var(program, ctx, source, lhs) {
            if let Some(flow) = expr_to_rhs_flow(program, ctx, source, rhs, dst) {
                program.flow.push(flow);
            }
        }
        return;
    }

    if node.kind() == "initializer_list" {
        if let Some(base) = assign_target {
            lower_initializer_list(program, ctx, source, node, base);
            return;
        }
    }

    if let Some(dst) = assign_target {
        if let Some(flow) = expr_to_rhs_flow(program, ctx, source, node, dst) {
            program.flow.push(flow);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_flow_from_expr(program, ctx, source, child, None);
    }
}

fn lower_initializer_list(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    base: VarId,
) {
    // Positional struct initializers (`static struct Ops o = { Fn };`):
    // map each bare value to its declared field by position and lower it
    // as a regular field store (function addresses included).
    let field_names = positional_struct_fields(program, base);
    let mut pos = 0usize;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "designated_initializer" | "initializer_pair" => {
                lower_designated_initializer(program, ctx, source, child, base);
            }
            "(" | ")" | "," | ";" | "{" | "}" => {}
            _ => {
                if !field_names.is_empty() {
                    if let Some(fname) = field_names.get(pos).and_then(|f| f.clone()) {
                        if let Some(fid) = field_id_for(program, base, &fname) {
                            emit_field_value_store(
                                program,
                                ctx,
                                source,
                                child,
                                base,
                                &[fid],
                                &[fname],
                                child,
                            );
                        }
                    }
                }
                pos += 1;
            }
        }
    }
}

/// Declared field names of the struct type behind `base`, in order.
fn positional_struct_fields(program: &Program, base: VarId) -> Vec<Option<String>> {
    let ty = program.symbols.variable(base).type_id;
    let TypeDesc::Struct { name, .. } = program.types.get(ty).desc.clone() else {
        return Vec::new();
    };
    if name.is_empty() {
        return Vec::new();
    }
    let Some(tid) = program
        .types
        .type_id_by_tag(&name, trace_ir::TypeKind::Struct)
    else {
        return Vec::new();
    };
    program
        .types
        .get(tid)
        .layout
        .fields
        .iter()
        .map(|(_, f)| Some(f.name.clone()))
        .collect()
}

fn field_id_for(program: &Program, base: VarId, fname: &str) -> Option<trace_ir::FieldId> {
    let ty = program.symbols.variable(base).type_id;
    let TypeDesc::Struct { name, .. } = program.types.get(ty).desc.clone() else {
        return None;
    };
    let tid = program
        .types
        .type_id_by_tag(&name, trace_ir::TypeKind::Struct)?;
    program.types.field_id_by_name(tid, fname)
}

fn lower_designated_initializer(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    base: VarId,
) {
    let mut field_names = Vec::new();
    let mut value = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "field_designator" => {
                let mut inner = child.walk();
                for c in child.children(&mut inner) {
                    if c.kind() == "field_identifier" {
                        field_names.push(node_text(source, &c).to_string());
                    }
                }
            }
            // `[i]` selects an array element; element access is
            // index-insensitive in this IR, so the subscript itself carries no
            // information — just don't mistake it for the value.
            "subscript_designator" | "=" => {}
            _ if value.is_none() && child.is_named() && child.kind() != "field_designator" => {
                value = Some(child)
            }
            _ => {}
        }
    }
    let Some(value_node) = value else {
        return;
    };
    let mut type_id = match struct_type_for_var(program, base) {
        Some(t) => t,
        None => return,
    };
    if value_node.kind() == "initializer_list" {
        // `[i] = { .f = v }` or `.s = { .g = v }`: descend into the nested
        // list against the same base (array elements are index-insensitive),
        // chaining GEPs for any field designators seen so far.
        let mut current = base;
        for fname in &field_names {
            let Some(fid) = program.types.field_id_by_name(type_id, fname) else {
                return;
            };
            current = alloc_gep_temp(program, ctx, node, current, fid, fname.clone());
            type_id = program.types.get(type_id).layout.fields[&fid].type_id;
            type_id = peel_ptr_to_struct(program, type_id);
        }
        lower_initializer_list(program, ctx, source, value_node, current);
        return;
    }
    if field_names.is_empty() {
        // Designated form without a field designator (`[i] = v` on a plain
        // fn-ptr array): handled by `lower_fn_ptr_array_init`.
        return;
    }
    let mut field_ids = Vec::with_capacity(field_names.len());
    for fname in &field_names {
        let Some(fid) = program.types.field_id_by_name(type_id, fname) else {
            return;
        };
        field_ids.push(fid);
        type_id = program.types.get(type_id).layout.fields[&fid].type_id;
    }
    emit_field_value_store(
        program,
        ctx,
        source,
        node,
        base,
        &field_ids,
        &field_names,
        value_node,
    );
}

fn peel_expression(mut node: Node) -> Node {
    while node.kind() == "parenthesized_expression" {
        node = node.named_child(0).unwrap_or(node);
    }
    node
}

fn peel_casts(mut node: Node) -> Node {
    loop {
        node = peel_expression(node);
        if node.kind() != "cast_expression" {
            break;
        }
        let Some(inner) = node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("expression"))
            .or_else(|| node.named_child(1))
        else {
            break;
        };
        node = inner;
    }
    node
}

fn emit_call_return(
    program: &mut Program,
    ctx: &LowerContext,
    call_node: Node,
    dst: VarId,
    callee_name: String,
) {
    program
        .flow
        .push(FlowConstraint::CallReturn { dst, callee_name });
    ctx.call_return_dst.borrow_mut().insert(call_node.id(), dst);
}

fn is_symbol_lookup_callee(name: &str) -> bool {
    matches!(
        name.rsplit("::").next().unwrap_or(name),
        "dlsym" | "dlvsym" | "GetProcAddress"
    )
}

/// Decode a C/C++ string literal or concatenated string into its contents.
fn string_literal_value(source: &str, node: Node) -> Option<String> {
    let node = peel_casts(node);
    match node.kind() {
        "string_literal" => decode_c_string_literal(node_text(source, &node)),
        "concatenated_string" => {
            let mut out = String::new();
            let mut any = false;
            for child in node.children(&mut node.walk()) {
                if child.kind() == "string_literal" {
                    out.push_str(&decode_c_string_literal(node_text(source, &child))?);
                    any = true;
                }
            }
            any.then_some(out)
        }
        _ => None,
    }
}

fn decode_c_string_literal(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    for prefix in ["u8", "u", "U", "L"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim_start();
            break;
        }
    }
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'0' => out.push('\0'),
                b'\\' => out.push('\\'),
                b'\'' => out.push('\''),
                b'"' => out.push('"'),
                b'?' => out.push('?'),
                b'a' => out.push('\u{0007}'),
                b'b' => out.push('\u{0008}'),
                b'f' => out.push('\u{000c}'),
                b'v' => out.push('\u{000b}'),
                other => out.push(other as char),
            }
            i += 1;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Some(out)
}

fn emit_field_store(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    lhs: Node,
    rhs: Node,
) {
    let Some((base, field_ids, field_names)) = decompose_field_path(program, ctx, source, lhs)
    else {
        return;
    };
    if field_ids.is_empty() {
        return;
    }
    emit_field_value_store(
        program,
        ctx,
        source,
        lhs,
        base,
        &field_ids,
        &field_names,
        rhs,
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_field_value_store(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    span_node: Node,
    base: VarId,
    field_ids: &[FieldId],
    field_names: &[String],
    value_node: Node,
) {
    let mut current = base;
    for (i, fid) in field_ids.iter().enumerate() {
        if i + 1 == field_ids.len() {
            let gep = alloc_gep_temp(
                program,
                ctx,
                span_node,
                current,
                *fid,
                field_names[i].clone(),
            );
            if let Some(src) = expr_to_store_src(program, ctx, source, value_node) {
                program.flow.push(FlowConstraint::Store { dst: gep, src });
            } else if value_node.kind() == "identifier" {
                let name = node_text(source, &value_node);
                if let Some(callee) = resolve_function_named(program, ctx, name) {
                    let src_temp = alloc_ret_temp(program, ctx, span_node);
                    program.flow.push(FlowConstraint::AddrOfFn {
                        dst: src_temp,
                        callee,
                    });
                    program.flow.push(FlowConstraint::Store {
                        dst: gep,
                        src: src_temp,
                    });
                } else {
                    // Defined later in the unit (no forward declaration):
                    // defer until the whole symbol table is populated.
                    ctx.pending.borrow_mut().push(PendingFnRef::FieldStore {
                        dst: gep,
                        name: name.to_string(),
                        span: node_span(program, ctx, span_node),
                    });
                }
            } else if value_node.kind() == "lambda_expression" && ctx.is_cpp {
                if let Some(callee) = lower_lambda_expression(program, ctx, source, value_node) {
                    let src_temp = alloc_ret_temp(program, ctx, span_node);
                    program.flow.push(FlowConstraint::AddrOfFn {
                        dst: src_temp,
                        callee,
                    });
                    program.flow.push(FlowConstraint::Store {
                        dst: gep,
                        src: src_temp,
                    });
                }
            } else {
                let ret_temp = alloc_ret_temp(program, ctx, span_node);
                let emitted = if value_node.kind() == "call_expression" {
                    if let Some(callee_name) = resolve_direct_call(program, ctx, source, value_node)
                    {
                        emit_call_return(program, ctx, value_node, ret_temp, callee_name);
                        true
                    } else if let Some(callee_var) =
                        resolve_callee_var(program, ctx, source, value_node)
                    {
                        program.flow.push(FlowConstraint::CallReturnIndirect {
                            dst: ret_temp,
                            callee_var,
                        });
                        true
                    } else {
                        false
                    }
                } else {
                    expr_to_rhs_flow(program, ctx, source, value_node, ret_temp)
                        .map(|flow| {
                            program.flow.push(flow);
                        })
                        .is_some()
                };
                if emitted {
                    program.flow.push(FlowConstraint::Store {
                        dst: gep,
                        src: ret_temp,
                    });
                }
            }
        } else {
            current = alloc_gep_temp(
                program,
                ctx,
                span_node,
                current,
                *fid,
                field_names[i].clone(),
            );
        }
    }
}

fn alloc_gep_temp(
    program: &mut Program,
    ctx: &LowerContext,
    span_node: Node,
    base: VarId,
    field: FieldId,
    field_name: String,
) -> VarId {
    let var_id = program.symbols.alloc_var_id();
    let span = node_span(program, ctx, span_node);
    program.symbols.add_variable(Variable {
        id: var_id,
        name: format!("_gep{}", var_id.0),
        type_id: program.types.int(),
        storage: StorageClass::Local,
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: true,
    });
    program.flow.push(FlowConstraint::GepField {
        dst: var_id,
        base,
        field,
        field_name,
    });
    var_id
}

fn field_name_from_node(source: &str, node: Node) -> Option<String> {
    node.child_by_field_name("field")
        .map(|n| node_text(source, &n).to_string())
}

fn decompose_field_path(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<(VarId, Vec<FieldId>, Vec<String>)> {
    let mut field_names = Vec::new();
    let mut cur = peel_expression(node);
    while cur.kind() == "field_expression" {
        field_names.push(field_name_from_node(source, cur)?);
        cur = cur.child_by_field_name("argument")?;
    }
    let base = resolve_lvalue_var(program, ctx, source, cur)?;
    field_names.reverse();

    let mut type_id = struct_type_for_var(program, base)?;
    let mut field_ids = Vec::new();
    for fname in &field_names {
        let fid = program.types.field_id_by_name(type_id, fname)?;
        field_ids.push(fid);
        let layout = program.types.get(type_id);
        type_id = layout.layout.fields.get(&fid)?.type_id;
        type_id = peel_ptr_to_struct(program, type_id);
    }
    Some((base, field_ids, field_names))
}

fn peel_ptr_to_struct(program: &mut Program, type_id: trace_ir::TypeId) -> trace_ir::TypeId {
    let inner = match &program.types.get(type_id).desc {
        TypeDesc::Ptr(inner) => Some((**inner).clone()),
        _ => None,
    };
    inner.map_or(type_id, |desc| program.types.intern(desc))
}

fn struct_type_for_var(program: &mut Program, var: VarId) -> Option<trace_ir::TypeId> {
    let mut type_id = variable_type_id(program, var)?;
    for _ in 0..4 {
        match &program.types.get(type_id).desc.clone() {
            TypeDesc::Ptr(inner) => {
                type_id = program.types.resolve_type_id(inner);
            }
            // Arrays of structs: field access via `arr[i].f` resolves
            // against the element type (index-insensitive over-approx).
            TypeDesc::Array { elem, .. } => {
                let inner = (**elem).clone();
                type_id = program.types.intern(inner);
            }
            TypeDesc::Struct { name, fields } => {
                // Prefer the complete layout interned from a header (PCH)
                // over an empty tag interned from `struct Foo g = { ... }`.
                if fields.is_empty() && !name.is_empty() {
                    if let Some(full) = program
                        .types
                        .type_id_by_tag(name, trace_ir::TypeKind::Struct)
                    {
                        return Some(full);
                    }
                }
                return Some(type_id);
            }
            TypeDesc::Union { name, fields } => {
                if fields.is_empty() && !name.is_empty() {
                    if let Some(full) = program
                        .types
                        .type_id_by_tag(name, trace_ir::TypeKind::Union)
                    {
                        return Some(full);
                    }
                }
                return Some(type_id);
            }
            _ => return Some(type_id),
        }
    }
    Some(type_id)
}

fn variable_type_id(program: &Program, var: VarId) -> Option<trace_ir::TypeId> {
    program.symbols.variable_by_id(var).map(|v| v.type_id)
}

fn pointer_op(source: &str, node: Node) -> Option<String> {
    if node.kind() != "pointer_expression" {
        return None;
    }
    node.child_by_field_name("operator")
        .map(|n| node_text(source, &n).to_string())
        .or_else(|| node.child(0).map(|n| node_text(source, &n).to_string()))
}

fn pointer_arg(node: Node) -> Option<Node> {
    if node.kind() != "pointer_expression" {
        return None;
    }
    node.child_by_field_name("argument")
        .or_else(|| node.named_child(0))
}

fn is_deref_lhs(source: &str, node: Node) -> bool {
    pointer_op(source, node).as_deref() == Some("*")
}

fn deref_operand(node: Node) -> Option<Node> {
    pointer_arg(node)
}

/// Inside a C++ class method, a bare identifier like `infImpl` that is a
/// member of the enclosing class should be implicitly treated as
/// `this->infImpl`.  Returns the GEP temp VarId representing the member
/// address when the identifier matches a class field, `None` otherwise.
fn resolve_implicit_this_member(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    if !ctx.is_cpp {
        return None;
    }
    let cls = ctx.class_ctx.as_ref()?;
    let cls_name = &cls.qual_name;
    let struct_tid = program
        .types
        .type_id_by_tag(cls_name, trace_ir::TypeKind::Struct)?;
    let field_name = node_text(source, &node);
    let field_id = program.types.field_id_by_name(struct_tid, field_name)?;
    let fn_id = ctx.current_fn?;
    let this_var = *program.symbols.function(fn_id).params.first()?;
    Some(alloc_gep_temp(
        program,
        ctx,
        node,
        this_var,
        field_id,
        field_name.to_string(),
    ))
}

/// Lower `&base.f1.f2` into a gep-temp chain so the resulting pointer
/// targets the field's own abstract location (with the field's type),
/// not the flattened outer instance. Returns the final temp var.
fn addr_of_field_path(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    arg: Node,
) -> Option<VarId> {
    let peeled = peel_expression(arg);
    // &field_expression → direct field path
    if peeled.kind() == "field_expression" {
        let (base, field_ids, field_names) = decompose_field_path(program, ctx, source, peeled)?;
        let mut current = base;
        for (i, fid) in field_ids.iter().enumerate() {
            current = alloc_gep_temp(program, ctx, peeled, current, *fid, field_names[i].clone());
        }
        return Some(current);
    }
    // &identifier → check for C++ implicit this->member
    if peeled.kind() == "identifier" {
        if let Some(gep) = resolve_implicit_this_member(program, ctx, source, peeled) {
            return Some(gep);
        }
    }
    // &ptr_expr → peel through pointer_expression with &
    if peeled.kind() == "pointer_expression" && pointer_op(source, peeled).as_deref() == Some("&") {
        if let Some(inner) = pointer_arg(peeled) {
            return addr_of_field_path(program, ctx, source, inner);
        }
    }
    None
}

fn expr_to_store_src(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    match node.kind() {
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                return addr_of_field_path(program, ctx, source, arg)
                    .or_else(|| resolve_lvalue_var(program, ctx, source, arg));
            }
            None
        }
        "identifier" => resolve_lvalue_var(program, ctx, source, node),
        _ => resolve_expr_var(program, ctx, source, node),
    }
}

fn expr_to_rhs_flow(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    dst: VarId,
) -> Option<FlowConstraint> {
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node);
            if let Some(callee) = program
                .symbols
                .resolve_function_in_scope(name, Some(ctx.current_file))
            {
                Some(FlowConstraint::AddrOfFn { dst, callee })
            } else if let Some(src) = lookup_var(ctx, program, name) {
                Some(FlowConstraint::Copy { dst, src })
            } else if let Some(gep) = resolve_implicit_this_member(program, ctx, source, node) {
                Some(FlowConstraint::Load { dst, src: gep })
            } else {
                // Might be a function defined later in the unit.
                ctx.pending.borrow_mut().push(PendingFnRef::RhsIdent {
                    dst,
                    name: name.to_string(),
                });
                None
            }
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                if let Some(callee) = resolve_fn_ref(program, ctx, source, arg) {
                    Some(FlowConstraint::AddrOfFn { dst, callee })
                } else if let Some(gep) = addr_of_field_path(program, ctx, source, arg) {
                    // The gep temp's pts-to is the field's own location.
                    Some(FlowConstraint::Copy { dst, src: gep })
                } else if let Some(src) = resolve_lvalue_var(program, ctx, source, arg) {
                    Some(FlowConstraint::AddrOfVar { dst, src })
                } else {
                    if arg.kind() == "identifier" {
                        // Might be a function defined later in the unit.
                        ctx.pending.borrow_mut().push(PendingFnRef::AddrOfIdent {
                            dst,
                            name: node_text(source, &arg).to_string(),
                        });
                    }
                    None
                }
            } else if op.as_deref() == Some("*") {
                let ptr = resolve_lvalue_var(program, ctx, source, arg)?;
                Some(FlowConstraint::Load { dst, src: ptr })
            } else {
                None
            }
        }
        "cast_expression" => node
            .child_by_field_name("expression")
            .or_else(|| node.named_child(1))
            .and_then(|inner| expr_to_rhs_flow(program, ctx, source, inner, dst)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|inner| expr_to_rhs_flow(program, ctx, source, inner, dst)),
        "lambda_expression" if ctx.is_cpp => {
            let callee = lower_lambda_expression(program, ctx, source, node)?;
            Some(FlowConstraint::AddrOfFn { dst, callee })
        }
        "string_literal" | "concatenated_string" => string_literal_value(source, node)
            .map(|value| FlowConstraint::StringConst { dst, value }),
        "call_expression" => {
            if let Some(callee_name) = resolve_direct_call(program, ctx, source, node) {
                emit_call_return(program, ctx, node, dst, callee_name);
            }
            None
        }
        "field_expression" => {
            let (base, field_ids, field_names) = decompose_field_path(program, ctx, source, node)?;
            let mut current = base;
            for (i, fid) in field_ids.iter().enumerate() {
                if i + 1 == field_ids.len() {
                    let tmp =
                        alloc_gep_temp(program, ctx, node, current, *fid, field_names[i].clone());
                    return Some(FlowConstraint::Load { dst, src: tmp });
                }
                current = alloc_gep_temp(program, ctx, node, current, *fid, field_names[i].clone());
            }
            None
        }
        "new_expression" if ctx.is_cpp => {
            if let Some(cls) = new_expression_class(program, ctx, source, node) {
                // Allocate a temp representing the heap allocation result.
                // The constructor's implicit `this` parameter (param 0) is
                // wired to this temp; explicit args start at index 1.
                let alloc_tmp = alloc_ret_temp(program, ctx, node);
                // Give alloc_tmp the class pointer type so the heap location
                // created by NewHeap carries the correct struct type.
                if let Some(struct_tid) = program
                    .types
                    .type_id_by_tag(&cls, trace_ir::TypeKind::Struct)
                {
                    program.symbols.variable_mut(alloc_tmp).type_id = struct_tid;
                }
                let args = node
                    .children(&mut node.walk())
                    .find(|c| c.kind() == "argument_list");
                let span = node_span(program, ctx, node);
                let mut call_args = collect_call_args(program, ctx, source, args);
                for v in &mut call_args.var_args {
                    v.0 += 1;
                }
                call_args.var_args.insert(0, (0, alloc_tmp));
                if let Some(caller) = ctx.current_fn {
                    emit_member_sites(
                        program,
                        caller,
                        &cls,
                        &trace_ir::MethodKind::Ctor,
                        call_args,
                        span,
                    );
                }
                ctx.handled_new_exprs.borrow_mut().insert(node.id());
                // Create a heap location for the allocated object so the
                // constructor's `this` parameter has concrete pointees.
                program
                    .flow
                    .push(FlowConstraint::NewHeap { dst: alloc_tmp });
                return Some(FlowConstraint::Copy {
                    dst,
                    src: alloc_tmp,
                });
            }
            None
        }
        _ => resolve_expr_var(program, ctx, source, node)
            .map(|src| FlowConstraint::Copy { dst, src }),
    }
}

fn collect_return_statement(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
) {
    let value = node
        .child_by_field_name("value")
        .or_else(|| node.named_child(0));
    let Some(value) = value else {
        return;
    };
    if value.kind() == ";" {
        return;
    }
    collect_return_flow(program, ctx, source, value, fn_id);
}

fn collect_return_flow(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
) {
    if let Some(flow) = return_flow_from_expr(program, ctx, source, node, fn_id) {
        program.fn_returns.entry(fn_id).or_default().push(flow);
    }
}

fn return_flow_from_expr(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    fn_id: FnId,
) -> Option<ReturnFlow> {
    let node = peel_expression(node);
    match node.kind() {
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                if let Some(callee) = resolve_fn_ref(program, ctx, source, arg) {
                    return Some(ReturnFlow::AddrOfFn { callee });
                }
                // `&base.field` returns a pointer to the field subobject;
                // carry it as a Copy of the gep temp's pts-to.
                if let Some(gep) = addr_of_field_path(program, ctx, source, arg) {
                    return Some(ReturnFlow::Copy { src: gep });
                }
                if let Some(src) = resolve_lvalue_var(program, ctx, source, arg) {
                    return Some(ReturnFlow::AddrOfVar { src });
                }
                if arg.kind() == "identifier" {
                    ctx.pending.borrow_mut().push(PendingFnRef::ReturnAddrOf {
                        owner: fn_id,
                        name: node_text(source, &arg).to_string(),
                    });
                }
                return None;
            }
            None
        }
        "identifier" => {
            let name = node_text(source, &node);
            if resolve_function_named(program, ctx, name).is_some() {
                None
            } else if let Some(src) = lookup_var(ctx, program, name) {
                Some(ReturnFlow::Copy { src })
            } else {
                ctx.pending.borrow_mut().push(PendingFnRef::ReturnIdent {
                    owner: fn_id,
                    name: name.to_string(),
                });
                None
            }
        }
        "call_expression" => {
            let callee_name = resolve_direct_call_name(source, node)?;
            if is_symbol_lookup_callee(&callee_name) {
                // Materialize a temp so the inner CallSite gets a return_dst
                // for the dlsym model; the wrapper then copies that temp.
                let temp = alloc_ret_temp(program, ctx, node);
                emit_call_return(program, ctx, node, temp, callee_name);
                Some(ReturnFlow::Copy { src: temp })
            } else {
                Some(ReturnFlow::Call { callee_name })
            }
        }
        "cast_expression" => node
            .child_by_field_name("expression")
            .or_else(|| node.named_child(1))
            .and_then(|inner| return_flow_from_expr(program, ctx, source, inner, fn_id)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|inner| return_flow_from_expr(program, ctx, source, inner, fn_id)),
        _ => None,
    }
}

fn resolve_direct_call_name(source: &str, node: Node) -> Option<String> {
    let func = node.child_by_field_name("function")?;
    let func = peel_expression(func);
    match func.kind() {
        "identifier" => Some(node_text(source, &func).to_string()),
        "pointer_expression" | "parenthesized_expression" => func
            .named_child(0)
            .and_then(|inner| resolve_direct_call_name(source, inner)),
        _ => None,
    }
}

fn resolve_direct_call(
    _program: &Program,
    _ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<String> {
    resolve_direct_call_name(source, node)
}

fn alloc_ret_temp(program: &mut Program, ctx: &LowerContext, span_node: Node) -> VarId {
    let span = node_span(program, ctx, span_node);
    alloc_ret_temp_spanned(program, ctx, span)
}

fn alloc_ret_temp_spanned(program: &mut Program, ctx: &LowerContext, span: Span) -> VarId {
    let var_id = program.symbols.alloc_var_id();
    program.symbols.add_variable(Variable {
        id: var_id,
        name: format!("_ret{}", var_id.0),
        type_id: program.types.int(),
        storage: StorageClass::Local,
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: true,
    });
    var_id
}

fn resolve_lvalue_var(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node);
            lookup_var(ctx, program, name)
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("*") {
                return resolve_lvalue_var(program, ctx, source, arg);
            }
            resolve_lvalue_var(program, ctx, source, arg)
        }
        "field_expression" | "subscript_expression" => node
            .child_by_field_name("argument")
            .and_then(|n| resolve_lvalue_var(program, ctx, source, n)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|n| resolve_lvalue_var(program, ctx, source, n)),
        "cast_expression" => node
            .child_by_field_name("expression")
            .or_else(|| node.named_child(1))
            .and_then(|n| resolve_lvalue_var(program, ctx, source, n)),
        _ => None,
    }
}

/// True when `node` is a `&base.member` or `&arr[i]` address expression.
/// Such arguments resolve to the base variable, so alias-style function
/// models must not treat them as whole-object copies.
fn is_addr_of_member(source: &str, node: Node) -> bool {
    if node.kind() != "pointer_expression" || pointer_op(source, node).as_deref() != Some("&") {
        return false;
    }
    let mut inner = match pointer_arg(node) {
        Some(arg) => arg,
        None => return false,
    };
    while matches!(inner.kind(), "parenthesized_expression" | "cast_expression") {
        inner = match inner.named_child(0) {
            Some(child) => child,
            None => return false,
        };
    }
    matches!(inner.kind(), "field_expression" | "subscript_expression")
}

/// The real declarator of a definition whose `declarator` field is nothing
/// but an unknown attribute macro trailing it (`void C::M() OVERRIDE {}`,
/// `void M() ACQUIRE(mu_) {}`).
///
/// Only a definition taking *no* arguments recovers this way: with a
/// parameter list to anchor it the declarator parses, and the macro is left
/// over as an `ERROR` after it. Without one, `C::M()` is as good a call as it
/// is a declarator, so tree-sitter takes it for one — an `init_declarator`
/// called with `()` at file scope, a plain `function_declarator` in a class
/// body — parks it in an `ERROR`, and hands the `declarator` field to the
/// macro. The definition then lands on the macro's name: a *defined* function
/// called `OVERRIDE`, one per class that annotates a nullary member, while
/// the real member stays undefined and its body unreachable.
///
/// The `ERROR` has to come before the field it displaced; one after it is the
/// ordinary trailing-macro leftover of a declaration that parsed fine.
fn error_parked_declarator(node: Node) -> Option<Node> {
    fn parked_at(n: Node) -> Option<Node> {
        let decl_start = n.child_by_field_name("declarator")?.start_byte();
        let err = n
            .children(&mut n.walk())
            .find(|c| c.kind() == "ERROR" && c.end_byte() <= decl_start)?;
        err.children(&mut err.walk()).find_map(|c| match c.kind() {
            "function_declarator" => Some(c),
            // `C::M()` read as a call: the name is the callee, and the empty
            // argument list is the parameter list it stands in for.
            "init_declarator" => c.child_by_field_name("declarator"),
            _ => None,
        })
    }
    // A pointer- or reference-returning member wraps its declarator, and the
    // repair is parked one level down per layer: `void *C::P() OVERRIDE {}`
    // hangs the `ERROR` off the `pointer_declarator`, not off the
    // definition, so a scan of the definition's own children saw nothing and
    // the body stayed under the macro's name.
    let mut level = node;
    loop {
        if let Some(found) = parked_at(level) {
            return Some(found);
        }
        let next = level.child_by_field_name("declarator")?;
        if !matches!(next.kind(), "pointer_declarator" | "reference_declarator") {
            return None;
        }
        level = next;
    }
}

fn find_function_declarator(node: Node) -> Option<Node> {
    if node.kind() == "function_declarator" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_function_declarator(child) {
            return Some(found);
        }
    }
    None
}

fn type_desc_from_field_declaration(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<(String, TypeDesc)> {
    let decl = node.child_by_field_name("declarator")?;
    let (fname, _) = parse_declarator_name(source, decl);
    if fname.is_empty() {
        return None;
    }
    let base = node
        .child_by_field_name("type")
        .map(|t| type_desc_from_node(program, ctx, source, t))
        .unwrap_or(TypeDesc::Int);
    let desc = if is_function_pointer_declarator(decl) {
        TypeDesc::FnPtr {
            ret: Box::new(base),
            params: Vec::new(),
        }
    } else if declarator_is_pointer_to_fn(decl) {
        // `struct T *(*Ref)(args)`: a pointer-wrapped function declarator.
        // Classifying it as a plain `Ptr(base)` loses the function-ness,
        // and downstream typed-slot guards then reject every function
        // value stored into such fields — killing indirect-call
        // resolution for ops tables assigned outside initializers.
        TypeDesc::FnPtr {
            ret: Box::new(base),
            params: Vec::new(),
        }
    } else if declarator_is_pointer(decl) {
        TypeDesc::Ptr(Box::new(base))
    } else {
        base
    };
    Some((fname, desc))
}

/// True when the declarator chain (through any number of pointer /
/// parenthesized levels) bottoms out in a function declarator — e.g.
/// `T * (*Ref)(args)` or `T (*tab[4])(args)`.
fn declarator_is_pointer_to_fn(decl: Node) -> bool {
    let mut cur = decl;
    while matches!(
        cur.kind(),
        "pointer_declarator" | "parenthesized_declarator"
    ) {
        cur = match cur
            .child_by_field_name("declarator")
            .or_else(|| cur.named_child(0))
        {
            Some(c) => c,
            None => return false,
        };
    }
    is_function_pointer_declarator(cur)
}

fn declarator_is_pointer(decl: Node) -> bool {
    match decl.kind() {
        "pointer_declarator" => true,
        "function_declarator" | "parenthesized_declarator" | "array_declarator" => decl
            .child_by_field_name("declarator")
            .is_some_and(declarator_is_pointer),
        _ => false,
    }
}

fn resolve_call_fn_arg(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> Option<FnId> {
    if ctx.is_cpp && node.kind() == "lambda_expression" {
        return lower_lambda_expression(program, ctx, source, node);
    }
    if let Some(fn_id) = resolve_fn_ref(program, ctx, source, node) {
        return Some(fn_id);
    }
    if node.kind() == "pointer_expression" {
        if let Some(inner) = pointer_arg(node) {
            return resolve_call_fn_arg(program, ctx, source, inner);
        }
    }
    None
}

fn resolve_function_named(program: &Program, ctx: &LowerContext, name: &str) -> Option<FnId> {
    program
        .symbols
        .resolve_function_in_scope(name, Some(ctx.current_file))
        .or_else(|| program.symbols.resolve_function(name))
}

fn resolve_fn_ref(program: &Program, ctx: &LowerContext, source: &str, node: Node) -> Option<FnId> {
    if node.kind() == "identifier" {
        return resolve_function_named(program, ctx, node_text(source, &node));
    }
    None
}

fn resolve_callee_var(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    let func = node.child_by_field_name("function")?;
    let (_, _, var) = resolve_callee_with_loads(program, ctx, source, func);
    var
}

fn resolve_callee_with_loads(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> (String, bool, Option<VarId>) {
    let node = peel_expression(node);
    if node.kind() == "field_expression" {
        if let Some((base, field_ids, field_names)) =
            decompose_field_path(program, ctx, source, node)
        {
            let text = field_callee_text(source, node);
            // Return a cached load var if we already created one for this
            // node (avoids duplicate loads that break CallReturnIndirect
            // mapping).
            let cached = ctx.callee_load_cache.borrow().get(&node.id()).cloned();
            if let Some(cached_var) = cached {
                return (text, false, cached_var);
            }
            if let Some(load_var) =
                emit_field_fn_ptr_load(program, ctx, source, node, base, &field_ids, &field_names)
            {
                ctx.callee_load_cache
                    .borrow_mut()
                    .insert(node.id(), Some(load_var));
                return (text, false, Some(load_var));
            }
            ctx.callee_load_cache.borrow_mut().insert(node.id(), None);
        }
    }
    resolve_callee(program, ctx, source, node)
}

fn field_callee_text(source: &str, node: Node) -> String {
    let mut parts = Vec::new();
    let mut cur = peel_expression(node);
    while cur.kind() == "field_expression" {
        if let Some(field) = cur.child_by_field_name("field") {
            parts.push(node_text(source, &field).to_string());
        }
        cur = cur.child_by_field_name("argument").unwrap_or(cur);
    }
    parts.reverse();
    let base = node_text(source, &cur);
    if parts.is_empty() {
        base.to_string()
    } else {
        format!("{}->{}", base, parts.join("->"))
    }
}

fn emit_field_fn_ptr_load(
    program: &mut Program,
    ctx: &LowerContext,
    _source: &str,
    span_node: Node,
    base: VarId,
    field_ids: &[FieldId],
    field_names: &[String],
) -> Option<VarId> {
    if field_ids.is_empty() {
        return None;
    }
    let mut type_id = struct_type_for_var(program, base)?;
    let mut current = base;
    for (i, fid) in field_ids.iter().enumerate() {
        let gep = alloc_gep_temp(
            program,
            ctx,
            span_node,
            current,
            *fid,
            field_names[i].clone(),
        );
        let field_type_id = program.types.get(type_id).layout.fields.get(fid)?.type_id;
        type_id = field_type_id;
        if i + 1 == field_ids.len() {
            let load_var = program.symbols.alloc_var_id();
            let span = node_span(program, ctx, span_node);
            program.symbols.add_variable(Variable {
                id: load_var,
                name: format!("_load{}", load_var.0),
                type_id: program.types.int(),
                storage: StorageClass::Local,
                fn_id: ctx.current_fn,
                param_index: None,
                span,
                is_pointer: true,
            });
            program.flow.push(FlowConstraint::Load {
                dst: load_var,
                src: gep,
            });
            return Some(load_var);
        }
        if matches!(program.types.get(field_type_id).desc, TypeDesc::Ptr(_)) {
            let load_var = program.symbols.alloc_var_id();
            let span = node_span(program, ctx, span_node);
            program.symbols.add_variable(Variable {
                id: load_var,
                name: format!("_load{}", load_var.0),
                type_id: field_type_id,
                storage: StorageClass::Local,
                fn_id: ctx.current_fn,
                param_index: None,
                span,
                is_pointer: true,
            });
            program.flow.push(FlowConstraint::Load {
                dst: load_var,
                src: gep,
            });
            current = load_var;
            type_id = program
                .types
                .resolve_type_id(match &program.types.get(field_type_id).desc {
                    TypeDesc::Ptr(inner) => inner,
                    _ => unreachable!(),
                });
        } else {
            current = gep;
        }
    }
    None
}

fn is_likely_macro_callee(name: &str) -> bool {
    if name.contains("->") || name.contains('.') || name.contains('(') {
        return false;
    }
    name.len() > 2
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn resolve_callee(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> (String, bool, Option<VarId>) {
    let node = peel_expression(node);
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node).to_string();
            if let Some(v) = lookup_var(ctx, program, &name) {
                return (name, false, Some(v));
            }
            if resolve_function_named(program, ctx, &name).is_some() {
                return (name, true, None);
            }
            (name, false, None)
        }
        // C++: `ns::fn`, `Cls::static_fn` — normalized text resolves by name
        // in the caller; template spellings drop their argument list.
        "qualified_identifier" => (normalize_qualified(node_text(source, &node)), false, None),
        "template_function" => {
            let raw = node_text(source, &node);
            let name = strip_template_args(&normalize_qualified(raw));
            if let Some(v) = lookup_var(ctx, program, &name) {
                return (name, false, Some(v));
            }
            (name, true, None)
        }
        "pointer_expression" | "parenthesized_expression" => node
            .named_child(0)
            .map(|inner| resolve_callee(program, ctx, source, inner))
            .unwrap_or(("<indirect>".into(), false, None)),
        "cast_expression" => node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("expression"))
            .or_else(|| node.named_child(1))
            .map(|inner| resolve_callee(program, ctx, source, inner))
            .unwrap_or(("<indirect>".into(), false, None)),
        "field_expression" => {
            let field = node
                .child_by_field_name("field")
                .map(|n| node_text(source, &n).to_string())
                .unwrap_or_else(|| "field".into());
            let arg = node.child_by_field_name("argument").unwrap();
            if let Some(v) = resolve_lvalue_var(program, ctx, source, arg) {
                return (
                    format!("{}->{}", node_text(source, &arg), field),
                    false,
                    Some(v),
                );
            }
            (field, false, None)
        }
        "subscript_expression" => {
            let arr = node.child_by_field_name("argument").unwrap();
            if let Some(v) = resolve_lvalue_var(program, ctx, source, arr) {
                return (format!("{}[...]", node_text(source, &arr)), false, Some(v));
            }
            ("<indirect>".into(), false, None)
        }
        _ => (node_text(source, &node).to_string(), false, None),
    }
}

fn resolve_expr_var(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<VarId> {
    match node.kind() {
        "identifier" => {
            let name = node_text(source, &node);
            lookup_var(ctx, program, name)
        }
        "pointer_expression" => {
            let op = pointer_op(source, node);
            let arg = pointer_arg(node)?;
            if op.as_deref() == Some("&") {
                return resolve_lvalue_var(program, ctx, source, arg);
            }
            resolve_expr_var(program, ctx, source, arg)
        }
        "field_expression" | "subscript_expression" => node
            .child_by_field_name("argument")
            .and_then(|n| resolve_expr_var(program, ctx, source, n)),
        "parenthesized_expression" => node
            .named_child(0)
            .and_then(|n| resolve_expr_var(program, ctx, source, n)),
        "cast_expression" => node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("expression"))
            .or_else(|| node.named_child(1))
            .and_then(|n| resolve_expr_var(program, ctx, source, n)),
        _ => None,
    }
}

fn lookup_var(ctx: &LowerContext, program: &Program, name: &str) -> Option<VarId> {
    if ctx.current_fn.is_some() {
        if let Some(&id) = ctx.locals.get(name) {
            return Some(id);
        }
    }
    if let Some(&id) = program.symbols.global_by_name.get(name) {
        return Some(id);
    }
    program
        .symbols
        .variables
        .iter()
        .find(|v| {
            v.name == name
                && match v.storage {
                    StorageClass::FileStatic => v.span.file == ctx.current_file,
                    StorageClass::FnStatic => v.fn_id == ctx.current_fn,
                    _ => false,
                }
        })
        .map(|v| v.id)
}

fn declaration_is_static(_source: &str, node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "storage_class_specifier" {
            continue;
        }
        let mut inner = child.walk();
        for token in child.children(&mut inner) {
            if token.kind() == "static" {
                return true;
            }
        }
    }
    false
}

fn is_function_pointer_declarator(decl: Node) -> bool {
    if decl.kind() != "function_declarator" {
        return false;
    }
    matches!(
        decl.child_by_field_name("declarator").map(|n| n.kind()),
        Some("parenthesized_declarator") | Some("pointer_declarator")
    )
}

/// If `decl` denotes a function whose declarator chain starts with one or more
/// pointer levels (`T *f(...)` / `T **f(...)`), return the innermost
/// non-fn-ptr `function_declarator` and the number of pointer levels.
/// Variables (plain pointers, arrays, fn-ptr vars) yield `None`.
fn fn_decl_under_pointer(decl: Node) -> Option<(Node, usize)> {
    let mut cur = decl;
    let mut depth = 0usize;
    loop {
        match cur.kind() {
            "pointer_declarator" => {
                depth += 1;
                cur = cur
                    .child_by_field_name("declarator")
                    .or_else(|| cur.named_child(0))?;
            }
            "parenthesized_declarator" => {
                cur = cur.named_child(0)?;
            }
            "function_declarator" => {
                if is_function_pointer_declarator(cur) {
                    return None;
                }
                return Some((cur, depth));
            }
            _ => return None,
        }
    }
}

fn storage_for(ctx: &LowerContext, is_static: bool) -> StorageClass {
    if ctx.current_fn.is_some() {
        if is_static {
            StorageClass::FnStatic
        } else {
            StorageClass::Local
        }
    } else if is_static {
        StorageClass::FileStatic
    } else {
        StorageClass::Global
    }
}

fn type_desc_from_node(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> TypeDesc {
    let node = peel_cpp_type_node(node);
    if node.kind() == "struct_specifier" || node.kind() == "union_specifier" {
        let name = lower_struct_specifier(program, ctx, source, node);
        if node.kind() == "union_specifier" {
            return TypeDesc::Union {
                name,
                fields: Vec::new(),
            };
        }
        return TypeDesc::Struct {
            name,
            fields: Vec::new(),
        };
    }
    if node.kind() == "class_specifier" {
        let name = lower_struct_specifier(program, ctx, source, node);
        return TypeDesc::Struct {
            name,
            fields: Vec::new(),
        };
    }
    if matches!(
        node.kind(),
        "qualified_identifier" | "type_identifier" | "template_type" | "placeholder_type_specifier"
    ) {
        let text = node_text(source, &node);
        let raw = normalize_qualified(text);
        if node.kind() == "placeholder_type_specifier" {
            // `auto`: unknown until the initializer is examined; callers
            // refine new-expression initializers separately.
            return TypeDesc::Unknown;
        }
        if is_callable_wrapper(&raw) {
            // `std::function<...>` holds a function value; intern as FnPtr
            // so AddrOfFn stores are not rejected by the slot guard.
            return TypeDesc::FnPtr {
                ret: Box::new(TypeDesc::Int),
                params: Vec::new(),
            };
        }
        // Use the unstripped spelling: `normalize_qualified` drops `<T>`,
        // which is the pointee we need for `shared_ptr<Plugin>`.
        if let Some(pointee) = smart_ptr_pointee(text).or_else(|| smart_ptr_pointee(&raw)) {
            let qualified = if pointee.contains("::") {
                pointee
            } else {
                ctx.qualify(&pointee)
            };
            return TypeDesc::Ptr(Box::new(TypeDesc::Struct {
                name: qualified,
                fields: Vec::new(),
            }));
        }
        let stripped = strip_template_args(&raw);
        let tag_hit = program
            .types
            .type_id_by_tag(&ctx.qualify(&stripped), trace_ir::TypeKind::Struct);
        let looks_class = stripped.contains("::")
            || stripped != raw // had template args stripped
            || tag_hit.is_some();
        if !looks_class {
            // Plain C typedef aliases keep the legacy path below.
        } else {
            let qualified = if stripped.contains("::") {
                stripped
            } else {
                ctx.qualify(&stripped)
            };
            return TypeDesc::Struct {
                name: qualified,
                fields: Vec::new(),
            };
        }
    }
    let text = node_text(source, &node);
    if text.contains("union") {
        TypeDesc::Union {
            name: extract_tag_name(source, &node, "union"),
            fields: Vec::new(),
        }
    } else if text.contains("struct") {
        TypeDesc::Struct {
            name: extract_tag_name(source, &node, "struct"),
            fields: Vec::new(),
        }
    } else if let Some(desc) = primitive_scalar_desc(text) {
        desc
    } else if text.contains("char") {
        TypeDesc::Char
    } else if text.contains("void") {
        TypeDesc::Void
    } else {
        // Bare identifiers may be typedef aliases (`fn_t`, `SHandle`);
        // resolving them keeps pointer-ness that would otherwise degrade
        // to `Int` and mislead downstream type checks.
        let alias = text.trim();
        if !alias.contains(char::is_whitespace) && !alias.is_empty() {
            if let Some(desc) = program.types.resolve_alias(alias) {
                return desc.clone();
            }
        }
        TypeDesc::Int
    }
}

/// Distinguish C/C++ primitive scalar parameter types (`double` vs `int`,
/// `short`, `long long`, ...). Same-arity overloads must survive merging, so
/// the scalar category is part of the type identity, not just a size hint.
/// `unsigned` collapses into its signed companion (documented imprecision);
/// `long double` is treated as `double`.
fn primitive_scalar_desc(text: &str) -> Option<TypeDesc> {
    let t = text.trim();
    if t.contains("long double") {
        Some(TypeDesc::Double)
    } else if t.contains("bool") {
        Some(TypeDesc::Bool)
    } else if t.contains("long long") {
        Some(TypeDesc::LongLong)
    } else if t.contains("double") {
        Some(TypeDesc::Double)
    } else if t.contains("float") {
        Some(TypeDesc::Float)
    } else if t.contains("short") {
        Some(TypeDesc::Short)
    } else if t.contains("long") {
        Some(TypeDesc::Long)
    } else if t.contains("unsigned") || t == "signed" {
        Some(TypeDesc::Int)
    } else {
        None
    }
}

fn peel_cpp_type_node(node: Node) -> Node {
    match node.kind() {
        "type_descriptor" => node
            .child_by_field_name("type")
            .map(peel_cpp_type_node)
            .unwrap_or(node),
        "qualified_identifier"
        | "type_identifier"
        | "template_type"
        | "placeholder_type_specifier"
        | "struct_specifier"
        | "class_specifier"
        | "union_specifier"
        | "primitive_type" => node,
        _ => {
            for i in 0..node.named_child_count() {
                if let Some(c) = node.named_child(i) {
                    if matches!(
                        c.kind(),
                        "qualified_identifier" | "template_type" | "type_identifier"
                    ) {
                        return peel_cpp_type_node(c);
                    }
                }
            }
            node
        }
    }
}

/// Resolve a `typedef X *Name;` / `typedef void (*Name)(...);` underlying
/// descriptor by walking the declarator chain for pointer/function/array
/// nesting. Only the shape matters for analysis purposes.
fn typedef_underlying_desc(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<TypeDesc> {
    let type_node = node.child_by_field_name("type")?;
    let decl_node = node.child_by_field_name("declarator")?;
    let base = type_desc_from_node(program, ctx, source, type_node);
    Some(walk_declarator_shape(decl_node, base))
}

fn walk_declarator_shape(node: Node, base: TypeDesc) -> TypeDesc {
    match node.kind() {
        "pointer_declarator" => {
            let inner = node
                .child_by_field_name("declarator")
                .map(|n| walk_declarator_shape(n, base.clone()))
                .unwrap_or(base);
            TypeDesc::Ptr(Box::new(inner))
        }
        "array_declarator" => {
            let inner = node
                .child_by_field_name("declarator")
                .map(|n| walk_declarator_shape(n, base.clone()))
                .unwrap_or(base);
            TypeDesc::Array {
                elem: Box::new(inner),
                size: None,
            }
        }
        "function_declarator" => {
            let inner = node.child_by_field_name("declarator");
            // `typedef void (*Name)(...)`: the pointer sits INSIDE the
            // parenthesized declarator, so it binds to the identifier first
            // and the function suffix applies outside it — the alias is
            // pointer-to-function, not function-returning-pointer. A plain
            // `typedef int f_t(int)` stays a bare FnPtr.
            let ptr_wrapped = inner.and_then(peel_paren_declarator).and_then(|n| {
                if n.kind() == "pointer_declarator" {
                    n.child_by_field_name("declarator")
                } else {
                    None
                }
            });
            if let Some(under) = ptr_wrapped {
                let ret = walk_declarator_shape(under, base);
                return TypeDesc::Ptr(Box::new(TypeDesc::FnPtr {
                    ret: Box::new(ret),
                    params: Vec::new(),
                }));
            }
            let ret = inner
                .map(|n| walk_declarator_shape(n, base.clone()))
                .unwrap_or(base);
            TypeDesc::FnPtr {
                ret: Box::new(ret),
                params: Vec::new(),
            }
        }
        "parenthesized_declarator" => node
            .named_child(0)
            .map(|n| walk_declarator_shape(n, base.clone()))
            .unwrap_or(base),
        _ => base,
    }
}

fn peel_paren_declarator(node: Node) -> Option<Node> {
    match node.kind() {
        "parenthesized_declarator" => node.named_child(0),
        _ => Some(node),
    }
}

fn parse_type_node(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> trace_ir::TypeId {
    let desc = type_desc_from_node(program, ctx, source, node);
    program.types.intern(desc)
}

fn extract_tag_name(source: &str, node: &Node, keyword: &str) -> String {
    let text = node_text(source, node);
    if let Some(rest) = text.split(keyword).nth(1) {
        rest.trim()
            .trim_start_matches(" {")
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap_or("anon")
            .to_string()
    } else {
        "anon".into()
    }
}

/// The part of a declared name that can name a scope: everything before the
/// `operator` keyword, or the whole name when there is none. A conversion
/// operator's target type is part of the name, not a qualification, so
/// `operator ns::S` names no scope while `Cls::operator ns::S` names `Cls`.
fn scope_part(name: &str) -> &str {
    let mut from = 0;
    while let Some(rel) = name[from..].find("operator") {
        let at = from + rel;
        from = at + "operator".len();
        // `operators::f` merely starts with the keyword; `operator ns::S`,
        // `operator=` and `operator new` are the real thing.
        let continues_identifier = name[from..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if (at == 0 || name[..at].ends_with("::")) && !continues_identifier {
            return &name[..at];
        }
    }
    name
}

/// The name a conversion operator declares, taken from its `operator_cast`
/// declarator: `operator const char *() const` → `operator const char*`.
/// The name runs up to the declarator's own parameter list; everything from
/// there on — the `()` and the cv-qualifiers — is declarator, and everything
/// before it spells the target type, pointer and reference layers included.
fn conversion_operator_name(source: &str, node: Node) -> String {
    /// The `(...)` that makes the declarator a function. Stopping at the
    /// enclosing `abstract_function_declarator` instead would cut away the
    /// `(*)` of a conversion to a function pointer, which sits *inside* it:
    /// `operator void (*)() const` would name the member `operator void`,
    /// colliding with a real conversion to `void`.
    fn own_parameters(node: Node) -> Option<Node> {
        let mut cur = node.child_by_field_name("declarator")?;
        while cur.kind() != "abstract_function_declarator" {
            cur = cur
                .child_by_field_name("declarator")
                .or_else(|| cur.named_child(0))?;
        }
        cur.child_by_field_name("parameters").or(Some(cur))
    }
    let end = own_parameters(node).map_or_else(|| node.end_byte(), |p| p.start_byte());
    // `normalize_spacing`, not `normalize_qualified`: the target's template
    // arguments are part of what distinguishes one conversion from another.
    let spelled = normalize_spacing(&source[node.start_byte()..end]);
    // That collapse drops the space before punctuation, which is right
    // everywhere but here: a globally-qualified target (`operator ::ns::S`)
    // would come out `operator::ns::S`, and every later step keys on the
    // keyword being a word of its own. An `operator_cast` is always a
    // conversion, so what follows is a type, never a `<` or `=` to glue on.
    match spelled.strip_prefix("operator") {
        Some(target) => format!("operator {}", target.trim_start()),
        None => spelled,
    }
}

/// `normalize_qualified` for a declared name, except that a conversion
/// operator's target keeps its template arguments — they are part of what
/// tells one conversion in a class from another, and this function runs over
/// names the declarator walk has already normalized once, so stripping here
/// undid that: the out-of-class `H::operator ns::Vec<int>` came back as
/// `H::operator Vec` and no longer met the `operator Vec<int>` its own class
/// declared.
fn normalize_declared_name(raw_name: &str) -> String {
    let scope = scope_part(raw_name);
    match raw_name[scope.len()..].strip_prefix("operator ") {
        Some(target) => format!(
            "{}operator {}",
            normalize_qualified(scope),
            normalize_spacing(target)
        ),
        None => normalize_qualified(raw_name),
    }
}

/// A conversion operator's target with the scopes its own member already sits
/// in dropped from it, so that how far the author had to spell the target out
/// stops deciding which member it is: `ns::Handle::operator ns::S` and the
/// in-class `operator S` (written inside `namespace ns`) both come out
/// `ns::Handle::operator S`.
///
/// Only the member's *own* scopes are dropped. Dropping every scope — which
/// is what this replaced — also merged targets that genuinely differ,
/// `operator a::S` with `operator b::S`, putting two members and two bodies
/// under one symbol. A scope the member does not sit in cannot have been
/// elided by the author at either spelling, so keeping it costs no merge.
///
/// Applied to the assembled name rather than the declarator, because only
/// there are both halves known: an in-class declaration learns its class from
/// `register_member_prototype`, an out-of-class definition carries it in the
/// spelling itself.
fn canonicalize_conversion_target(name: &str) -> String {
    let scope = scope_part(name);
    let Some(target) = name[scope.len()..].strip_prefix("operator ") else {
        return name.to_string();
    };
    // Every qualification the author could have elided, longest first. From
    // inside `a::b::H` a type `a::b::H::T` may be written `T`, `H::T`,
    // `b::H::T`, `a::b::H::T` — any *contiguous run* of the enclosing
    // segments, not just the runs that start at the outermost one. Building
    // only the leading prefixes missed the class-relative spellings, so
    // `H::T` never met the `T` its own class declares.
    let segments: Vec<&str> = scope.trim_end_matches("::").split("::").collect();
    let mut prefixes: Vec<String> = Vec::new();
    if !scope.is_empty() {
        for start in 0..segments.len() {
            for end in start..segments.len() {
                prefixes.push(format!("{}::", segments[start..=end].join("::")));
            }
        }
    }
    prefixes.sort_by_key(|p| std::cmp::Reverse(p.len()));
    // The scopes come off wherever they appear — head and template arguments
    // alike, each position taking the longest prefix that applies to it, and
    // each deciding its own leading `::` by the same rule.
    let target = strip_own_scopes(target, &prefixes);
    format!("{scope}operator {target}")
}

/// Drop, from every qualified name inside `text`, the longest of `prefixes`
/// that name begins with — the target's head and each template argument
/// decided on its own.
///
/// Per position, because the prefixes nest: for a member of `a::b`, the
/// argument of `a::Vec<a::b::T>` starts with `a::b::` while the head starts
/// only with `a::`. Choosing one prefix for the whole spelling let the
/// argument's longer match preempt the head's shorter one, leaving
/// `a::Vec<T>` where the in-class spelling says `Vec<T>`.
///
/// Matching only where a qualified name can begin, and only once there,
/// keeps `N::N::S` — a `S` in an inner `N` — from being consumed twice down
/// to the outer `N::S`.
fn strip_own_scopes(text: &str, prefixes: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut at_name_start = true;
    while !rest.is_empty() {
        if at_name_start {
            // A leading `::` is consumed *with* the scope it re-spells, and
            // only then: it is redundant exactly when what follows names a
            // scope the member sits in. Left alone otherwise, because it is
            // all that separates a global type from one an enclosing
            // namespace shadows. The same rule at every position, so an
            // argument written `V<::a::b::T>` reduces like `V<T>`.
            let probe = rest.strip_prefix("::").unwrap_or(rest);
            if let Some(hit) = prefixes.iter().find(|p| probe.starts_with(p.as_str())) {
                rest = &probe[hit.len()..];
                at_name_start = false;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap_or_default();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
        at_name_start = matches!(ch, '<' | ',' | ' ' | '(' | '*' | '&');
    }
    out
}

/// Where the real name starts inside a `qualified_identifier` error recovery
/// invented, or `None` for an honest one. An unknown attribute macro in front
/// of a return type (`FFI_EXPORT T f(...)`, where no `#define` for
/// `FFI_EXPORT` was in the include path) leaves tree-sitter taking the macro
/// as the type and no rule left for the real one; it recovers by qualifying
/// the name with that type, in one of two shapes:
///
/// - `FFI_EXPORT T f()` — nothing follows the name, so the `::` joining the
///   two is MISSING and the name is the `name` field alone;
/// - `FFI_EXPORT T C::M()` — the definition's own `::` is real, and the class
///   it qualifies is parked in an `ERROR` node just after the leftover type.
///   Neither field is the whole name: `C` sits in that `ERROR`, and `M` (or
///   `B::M`, for a deeper scope) in `name`.
///
/// Read whole the member is spelled `T f` or `T C::M`, which no call site
/// matches, so the definition hides behind a phantom external of the real
/// name.
fn fabricated_qualified_name(node: Node) -> Option<usize> {
    fn repaired_at(n: Node) -> Option<usize> {
        if n.children(&mut n.walk())
            .any(|c| c.kind() == "::" && c.is_missing())
        {
            return n.child_by_field_name("name").map(|x| x.start_byte());
        }
        // Only an `ERROR` standing where the recovery puts it: between the
        // leftover type and the `::` that joins them, which is the one thing
        // that separates a fabricated qualification from an honest one. An
        // `ERROR` *after* the `::` is a different repair on a real scope —
        // `EXPORT C::operator int()` parks the `operator` keyword there (see
        // `conversion_keyword_error`), and taking it would cut the class off
        // the front of the name and send the definition to global scope.
        let scope_end = n.child_by_field_name("scope")?.end_byte();
        let sep = n
            .children(&mut n.walk())
            .find(|c| c.kind() == "::")?
            .start_byte();
        n.children(&mut n.walk())
            .find(|c| c.kind() == "ERROR" && c.start_byte() >= scope_end && c.start_byte() < sep)
            .map(|c| c.start_byte())
    }
    qualified_identifier_chain(node).find_map(repaired_at)
}

/// A `qualified_identifier` together with every one nested in its `name`
/// field. A qualified name nests one level per scope it carries, and recovery
/// leaves its mark at whichever level the fabricated segment landed on — so
/// every scope either half of the name spells pushes that mark one level
/// deeper, out of reach of a scan over direct children.
/// `FFI_EXPORT n::q::S A::B::M()` parks its `A` three levels down, and
/// `EXPORT ns::C::operator ns::S()` its stranded keyword two.
fn qualified_identifier_chain<'a>(node: Node<'a>) -> impl Iterator<Item = Node<'a>> {
    std::iter::successors(Some(node), |n| {
        n.child_by_field_name("name")
            .filter(|c| c.kind() == "qualified_identifier")
    })
}

/// The `operator` keyword parked alone in an `ERROR`, which is what recovery
/// leaves behind when an unknown attribute macro takes the `type` field of a
/// *conversion* operator: with no rule left for the keyword, the target type
/// is left standing where the declared name belongs, so
/// `MACRO operator ns::S() const;` reads as a member named `S` and
/// `EXPORT C::operator int() {}` as one named `int`. Any scope the target
/// carried trails the keyword inside the same node (`ERROR [operator ns::]`),
/// which is why the target is read from the node's end rather than from it.
///
/// A pointer target needs none of this: `MACRO operator int *() const;` keeps
/// a real `operator_name` and only nests an `ERROR` inside it.
fn conversion_keyword_error(node: Node) -> Option<Node> {
    node.children(&mut node.walk())
        .find(|c| c.kind() == "ERROR" && c.child(0).is_some_and(|k| k.kind() == "operator"))
}

/// The `operator_cast` a declarator is, or ends in: `operator T` for an
/// in-class definition, `A::B::operator T` for an out-of-class one.
fn declarator_operator_cast(node: Node) -> Option<Node> {
    let mut cur = node;
    while cur.kind() == "qualified_identifier" {
        cur = cur.child_by_field_name("name")?;
    }
    (cur.kind() == "operator_cast").then_some(cur)
}

/// The type a conversion operator converts to — its `operator_cast`'s `type`
/// field, wrapped in one `Ptr` per pointer or reference layer of the abstract
/// declarator, so `operator T *()` returns `T *` and not `T`. References
/// lower as pointers here as they do everywhere else.
fn conversion_target_type(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    op: Node,
) -> Option<trace_ir::TypeId> {
    let mut desc = type_desc_from_node(program, ctx, source, op.child_by_field_name("type")?);
    let mut cur = op.child_by_field_name("declarator");
    while let Some(n) = cur {
        cur = match n.kind() {
            "abstract_pointer_declarator" | "abstract_reference_declarator" => {
                desc = TypeDesc::Ptr(Box::new(desc));
                n.child_by_field_name("declarator")
                    .or_else(|| n.named_child(0))
            }
            "abstract_parenthesized_declarator" => n.named_child(0),
            "abstract_function_declarator" => match n.child_by_field_name("declarator") {
                // `operator void (*)()` — a declarator nested inside this
                // one means the `(...)` belongs to the *target*, which is
                // therefore a function type; the `(*)` that makes it
                // nameable sits in there and adds its `Ptr` on the way down.
                // Recording a bare `Ptr(Void)` here left the target
                // indistinguishable from a pointer to `void`, so nothing
                // downstream could see it as callable. Parameters stay empty
                // to match every other `FnPtr` this lowering builds, which is
                // what makes this agree with the `typedef void (*FP)();`
                // spelling of the same type.
                Some(inner) => {
                    desc = TypeDesc::FnPtr {
                        ret: Box::new(desc),
                        params: Vec::new(),
                    };
                    Some(inner)
                }
                // `operator T *()` — nothing nested, so this is the member's
                // own parameter list and the target ends here.
                None => break,
            },
            _ => break,
        };
    }
    Some(program.types.intern(desc))
}

fn parse_declarator_name(source: &str, node: Node) -> (String, bool) {
    match node.kind() {
        "identifier" => (node_text(source, &node).to_string(), false),
        "pointer_declarator" => {
            if let Some(inner) = node
                .child_by_field_name("declarator")
                .or_else(|| node.named_child(0))
            {
                let (name, _) = parse_declarator_name(source, inner);
                (name, true)
            } else {
                (String::new(), true)
            }
        }
        // C++ reference parameters alias their argument; treat them like
        // pointers so stores through them land on the caller's memory.
        // tree-sitter-cpp often stores the inner name as a positional
        // child, not a `declarator` field (`&p` / `&&p`).
        "reference_declarator" => {
            if let Some(inner) = node
                .child_by_field_name("declarator")
                .or_else(|| node.named_child(0))
            {
                let (name, _) = parse_declarator_name(source, inner);
                (name, true)
            } else {
                (String::new(), true)
            }
        }
        // An out-of-class conversion operator (`Cls::operator T() const`)
        // hangs its `operator_cast` off the `name` field; the whole-node text
        // would glue the declarator's `()` and cv-qualifiers onto the name.
        "qualified_identifier" => {
            // An unknown attribute macro in front of an out-of-class
            // conversion operator costs it its `operator_cast`: the keyword
            // is stranded in an `ERROR` and the target type takes the `name`
            // field. Spell the member the way every other path spells it, or
            // the definition and its declaration are two members. The scope
            // and the target are read by byte offset around the keyword, so
            // it makes no difference how deep the chain parked it.
            if let Some(err) = qualified_identifier_chain(node).find_map(conversion_keyword_error) {
                let scope = normalize_qualified(&source[node.start_byte()..err.start_byte()]);
                let target = normalize_spacing(&source[err.end_byte()..node.end_byte()]);
                return (format!("{scope}operator {}", target.trim_start()), false);
            }
            // Error recovery invents qualifications of its own, and the name
            // then starts past the return type they carry — see
            // `fabricated_qualified_name`.
            let start = fabricated_qualified_name(node).unwrap_or_else(|| node.start_byte());
            match declarator_operator_cast(node) {
                Some(op) => {
                    let scope = normalize_qualified(&source[start..op.start_byte()]);
                    let name = format!("{scope}{}", conversion_operator_name(source, op));
                    (name, false)
                }
                None => (normalize_qualified(&source[start..node.end_byte()]), false),
            }
        }
        // `~Name` spans two preprocessor tokens joined by whitespace in
        // expansion output; collapse it so protos and defs share one name.
        "destructor_name" => (normalize_qualified(node_text(source, &node)), false),
        "operator_name" => (normalize_qualified(node_text(source, &node)), false),
        "operator_cast" => (conversion_operator_name(source, node), false),
        "function_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                parse_declarator_name(source, inner)
            } else {
                (String::new(), false)
            }
        }
        "parenthesized_declarator" => node
            .named_child(0)
            .map(|n| parse_declarator_name(source, n))
            .unwrap_or((String::new(), false)),
        "array_declarator" => node
            .child_by_field_name("declarator")
            .map(|n| parse_declarator_name(source, n))
            .unwrap_or((String::new(), false)),
        _ => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                parse_declarator_name(source, inner)
            } else {
                (node_text(source, &node).to_string(), false)
            }
        }
    }
}

/// Collapse preprocessor-introduced whitespace inside a qualified name and
/// strip balanced `<...>` argument spans per segment:
/// `outer :: inner :: Box < int >` → `outer::inner::Box`,
/// `clampT < double >` → `clampT`.
///
/// Whitespace is dropped only where it separates a word from punctuation —
/// exactly the gaps a macro expansion introduces (`~ Cls`, `A :: b`). Between
/// two words it is significant and collapses to a single space, so multi-word
/// names keep their shape: `operator new`, `operator const char*`.
fn normalize_qualified(text: &str) -> String {
    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }
    /// Is the segment being built an operator name? `operator<`, `<=`, `<<`
    /// and `<=>` spell the operator; they open no argument list. Without
    /// this the whole family truncated to the bare keyword `operator`.
    ///
    /// Only what directly follows the keyword counts: in a conversion
    /// operator the `<` opens an ordinary argument list of the target type
    /// (`operator Vec<int>`), which is kept — see `normalize_spacing`, the
    /// half of this function a conversion target is normalized with.
    fn in_operator_name(out: &str) -> bool {
        let segment = out.rsplit("::").next().unwrap_or(out);
        segment
            .strip_prefix("operator")
            .is_some_and(|rest| rest.chars().all(|c| matches!(c, '<' | '=' | '>')))
    }
    let mut out = String::with_capacity(text.len());
    let mut angle_depth = 0i32;
    let mut pending_space = false;
    for ch in text.chars() {
        if angle_depth > 0 {
            match ch {
                '<' => angle_depth += 1,
                '>' => angle_depth -= 1,
                _ => {}
            }
            continue;
        }
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if std::mem::take(&mut pending_space) && is_word(ch) && out.ends_with(is_word) {
            out.push(' ');
        }
        match ch {
            '<' if !in_operator_name(&out) => angle_depth += 1,
            _ => out.push(ch),
        }
    }
    out
}

/// The whitespace half of `normalize_qualified`, without the argument
/// stripping: `Vec < int >` → `Vec<int>`, `A :: b` → `A::b`,
/// `operator  new` → `operator new`.
///
/// A conversion operator's target is normalized with this rather than the
/// whole function, because the target type is the *only* thing telling one
/// conversion in a class from another: dropping its arguments made
/// `operator Vec<int>` and `operator Vec<double>` one member, merging two
/// bodies under one symbol. Keeping them costs nothing that stripping bought
/// — the spellings a declaration and its out-of-class definition use differ
/// in *scope*, not in arguments, so `operator ns::Vec<int>` and
/// `operator Vec<int>` still meet once the scope is dropped.
fn normalize_spacing(text: &str) -> String {
    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if std::mem::take(&mut pending_space) && is_word(ch) && out.ends_with(is_word) {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// Collapse whitespace in a template-base spelling without discarding its
/// arguments. The ordinary qualified-name normalizer intentionally strips
/// `<...>`; IPC analysis needs the preserved argument from bases such as
/// `IRemoteStub < IFoo >`.
fn normalize_template_spelling(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// Strip a trailing balanced `<...>` argument list from a type/function
/// spelling: `clampT<double>` → `clampT`, `Box<int>` → `Box`.
fn strip_template_args(text: &str) -> String {
    let trimmed = text.trim_end();
    if !trimmed.ends_with('>') {
        return trimmed.to_string();
    }
    let bytes = trimmed.as_bytes();
    let mut depth = 0i32;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().rev() {
        match b {
            b'>' => {
                if depth == 0 {
                    end = Some(i);
                }
                depth += 1;
            }
            b'<' => {
                depth -= 1;
                if depth == 0 && end.is_some() {
                    return trimmed[..i].trim_end().to_string();
                }
            }
            _ => {}
        }
    }
    trimmed.to_string()
}

fn type_name_before_template(raw: &str) -> &str {
    let s = raw.trim();
    match s.find('<') {
        Some(i) => s[..i].trim(),
        None => s,
    }
}

fn last_type_segment(qual: &str) -> &str {
    qual.rsplit("::").next().unwrap_or(qual)
}

/// `std::function<Sig>` / `::std::function<Sig>` is a callable wrapper:
/// intern as `FnPtr` so stores of function addresses survive the solver
/// slot guard. A last-segment `function` in any other namespace is a
/// normal class (functors named `function`, etc.).
fn is_callable_wrapper(raw: &str) -> bool {
    let name = type_name_before_template(raw.trim());
    name == "std::function" || name == "::std::function"
}

/// `std::shared_ptr<T>` / `unique_ptr` / `weak_ptr` → pointee tag `T`.
fn smart_ptr_pointee(raw: &str) -> Option<String> {
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let head = last_type_segment(type_name_before_template(&compact));
    if !matches!(head, "shared_ptr" | "unique_ptr" | "weak_ptr") {
        return None;
    }
    Some(sanitize_type_name(&template_first_arg(&compact)?))
}

fn template_first_arg(raw: &str) -> Option<String> {
    let start = raw.find('<')? + 1;
    let bytes = raw.as_bytes();
    let mut depth = 1i32;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        match b {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            b',' if depth == 1 => {
                end = Some(i);
                break;
            }
            _ => {}
        }
    }
    let arg = raw[start..end?].trim();
    if arg.is_empty() {
        None
    } else {
        Some(arg.to_string())
    }
}

fn sanitize_type_name(arg: &str) -> String {
    let mut s = arg.trim();
    loop {
        let t = s.trim_start();
        let next = t
            .strip_prefix("const ")
            .or_else(|| t.strip_prefix("volatile "))
            .or_else(|| t.strip_prefix("class "))
            .or_else(|| t.strip_prefix("struct "))
            .or_else(|| t.strip_prefix("typename "));
        if let Some(n) = next {
            s = n;
            continue;
        }
        break;
    }
    s.trim().trim_end_matches(['*', '&']).trim().to_string()
}

fn find_params(decl: Node) -> Option<Node> {
    if decl.kind() == "function_declarator" {
        return decl.child_by_field_name("parameters");
    }
    for i in 0..decl.child_count() {
        if let Some(child) = decl.child(i) {
            if let Some(p) = find_params(child) {
                return Some(p);
            }
        }
    }
    None
}

fn node_span(program: &mut Program, ctx: &LowerContext, node: Node) -> Span {
    if let Some(line_map) = &ctx.line_map {
        if let Some(entry) = line_map.lookup(node.start_byte()) {
            let origin = line_map.path_of(entry);
            // Always report original-file coordinates. Code from an
            // `#include`d file is attributed to its original header;
            // TU-local code keeps the primary file but gets its original
            // (pre-expansion) line/col, so reported lines match what a
            // user sees in their editor (AGENTS.md LineMap invariant).
            let fid = if origin != ctx.primary_path {
                program.symbols.add_file_interned(origin)
            } else {
                ctx.current_file
            };
            return Span::new(fid, entry.line, entry.col);
        }
    }
    let line = node.start_position().row as u32 + 1;
    let col = node.start_position().column as u32 + 1;
    Span::new(ctx.current_file, line, col)
}

/// Original-file end line of `node`, for range queries like "which function
/// contains this line". The end maps back through the LineMap only when the
/// last byte originates from the same file as `span.file` — a body that ends
/// inside a different `#include` origin has no meaningful single-file range,
/// and falls back to the start line. Falls back to raw tree-sitter
/// positions for unpreprocessed sources (no LineMap).
fn node_end_line(program: &Program, ctx: &LowerContext, node: Node, span: Span) -> u32 {
    match ctx.line_map.as_ref() {
        None => node.end_position().row as u32 + 1,
        Some(line_map) => {
            let entry = line_map.lookup(node.end_byte().saturating_sub(1));
            let same_origin = entry
                .map(|entry| {
                    let origin = line_map.path_of(entry);
                    let fid = if origin != ctx.primary_path {
                        program.symbols.file_by_path(origin)
                    } else {
                        Some(ctx.current_file)
                    };
                    fid == Some(span.file)
                })
                .unwrap_or(false);
            if same_origin {
                entry.map(|e| e.line).unwrap_or(span.line)
            } else {
                // End originates in another file (or is unmappable): a body
                // has no meaningful single-file range, so report the start.
                span.line
            }
        }
    }
}

/// Exhaustive check of `canonicalize_conversion_target` over a generated
/// world of scopes and spellings.
///
/// Four defects were found in that function one at a time, each hidden by the
/// fix before it, because its pieces interact: a leading `::`, nested
/// enclosing scopes, template arguments, and repeated segments. Rather than
/// add a case per bug, this enumerates every legal C++ spelling of every type
/// in a small world and asserts the two properties the naming exists to have.
#[cfg(test)]
mod conversion_target_properties {
    use super::canonicalize_conversion_target;

    /// The scopes enclosing a member, innermost first.
    fn enclosing(member_scope: &str) -> Vec<String> {
        let segs: Vec<&str> = member_scope.split("::").collect();
        (1..=segs.len())
            .rev()
            .map(|n| segs[..n].join("::"))
            .collect()
    }

    /// Every way an author could legally spell `fqn` from inside
    /// `member_scope`, in a world where no two types share a short name — so
    /// eliding a scope can never change which type is found.
    fn spellings(fqn: &str, member_scope: &str) -> Vec<String> {
        let mut out = vec![fqn.to_string(), format!("::{fqn}")];
        for scope in enclosing(member_scope) {
            if let Some(rest) = fqn.strip_prefix(&format!("{scope}::")) {
                out.push(rest.to_string());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Is `fqn` reachable by eliding a scope the member sits in? Only those
    /// have *every* spelling agree: for a type outside the member's scopes (a
    /// global `G`, a sibling `x::X`) the bare `G` and the defensive `::G` are
    /// indistinguishable from a shadowed pair without real name lookup, and
    /// are deliberately kept apart.
    fn in_own_scope(fqn: &str, member_scope: &str) -> bool {
        enclosing(member_scope)
            .iter()
            .any(|scope| fqn.starts_with(&format!("{scope}::")))
    }

    /// Distinct types, none sharing a short name.
    fn world(member_scope: &str) -> Vec<String> {
        let mut types = vec!["G".to_string(), "x::X".to_string()];
        for (i, scope) in enclosing(member_scope).iter().enumerate() {
            types.push(format!("{scope}::T{i}"));
            types.push(format!("{scope}::V{i}"));
        }
        types
    }

    /// Plain types plus every `Head<Arg>` pairing, so an argument's scopes
    /// are exercised independently of the head's.
    fn targets(member_scope: &str) -> Vec<(String, Vec<String>)> {
        let types = world(member_scope);
        let mut out: Vec<(String, Vec<String>)> = types
            .iter()
            .map(|t| (t.clone(), spellings(t, member_scope)))
            .collect();
        for head in &types {
            for arg in &types {
                let mut forms = Vec::new();
                for h in spellings(head, member_scope) {
                    for a in spellings(arg, member_scope) {
                        forms.push(format!("{h}<{a}>"));
                    }
                }
                out.push((format!("{head}<{arg}>"), forms));
            }
        }
        out
    }

    /// Every part of a target that has to be reachable for its spellings to
    /// agree: the head, and the argument when there is one.
    fn parts(fqn: &str) -> Vec<&str> {
        match fqn.split_once('<') {
            Some((head, arg)) => vec![head, arg.trim_end_matches('>')],
            None => vec![fqn],
        }
    }

    fn canonical(member_scope: &str, target: &str) -> String {
        canonicalize_conversion_target(&format!("{member_scope}::operator {target}"))
    }

    const MEMBER_SCOPES: [&str; 4] = ["C", "n::H", "a::b::H", "a::b::c::H"];

    #[test]
    fn every_spelling_of_one_type_names_one_member() {
        for member_scope in MEMBER_SCOPES {
            for (fqn, forms) in targets(member_scope) {
                if !parts(&fqn).iter().all(|p| in_own_scope(p, member_scope)) {
                    continue;
                }
                let mut names: Vec<String> =
                    forms.iter().map(|f| canonical(member_scope, f)).collect();
                names.sort();
                names.dedup();
                assert_eq!(
                    names.len(),
                    1,
                    "in {member_scope}, the spellings {forms:?} of `{fqn}` named \
                     {names:?} instead of one member"
                );
            }
        }
    }

    #[test]
    fn no_two_types_are_named_the_same_member() {
        for member_scope in MEMBER_SCOPES {
            let mut owner: std::collections::HashMap<String, (String, String)> =
                std::collections::HashMap::new();
            for (fqn, forms) in targets(member_scope) {
                for form in forms {
                    let name = canonical(member_scope, &form);
                    let entry = owner
                        .entry(name.clone())
                        .or_insert_with(|| (fqn.clone(), form.clone()));
                    assert_eq!(
                        entry.0, fqn,
                        "in {member_scope}, `{form}` ({fqn}) and `{}` ({}) both name \
                         `{name}`",
                        entry.1, entry.0
                    );
                }
            }
        }
    }

    #[test]
    fn canonicalizing_a_canonical_name_changes_nothing() {
        for member_scope in MEMBER_SCOPES {
            for (_, forms) in targets(member_scope) {
                for form in forms {
                    let once = canonical(member_scope, &form);
                    let twice = canonicalize_conversion_target(&once);
                    assert_eq!(once, twice, "unstable for `{form}` in {member_scope}");
                }
            }
        }
    }
}
