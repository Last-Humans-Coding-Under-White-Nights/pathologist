// `configured` needs `super::` access to this module's private helpers.
#[path = "configured.rs"]
mod configured;

use crate::deps::IncludeGraph;
use crate::discover::discover_source_files;
use crate::gn_defines::Candidate;
use crate::index_cache::{IndexSourceCache, PreprocessedSource};
use crate::merge::{
    merge_unit_header_preamble, merge_unit_index, merge_unit_symbols, merge_unit_types,
    merge_unit_variants, UnitIndex,
};
use crate::parse::node_text;
use rayon::prelude::*;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use trace_ir::{
    is_anonymous_tag, CallSite, Diagnostic, DiagnosticSeverity, FieldId, FlowConstraint, FnId,
    Function, Linkage, Program, ReturnFlow, ScalarKind, Span, StorageClass, TypeDesc, VarId,
    Variable,
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
    /// Lazy LineMap file-index -> symbol file ID. Preserve first-encounter
    /// interning order while avoiding path comparisons/hashing per span.
    origin_file_ids: Vec<Cell<Option<trace_ir::FileId>>>,
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
    /// Fully qualified classes whose scope a type name is looked up in,
    /// innermost last: the class whose body is being lowered, or the class a
    /// member function's parameters and body belong to. `RefCell` because a
    /// class body is lowered from `&LowerContext` type helpers.
    type_scope: RefCell<Vec<String>>,
    /// Typedefs and `using` aliases declared in the blocks of the function
    /// being lowered, innermost last; each block drops its own on exit.
    local_aliases: Vec<(String, TypeDesc)>,
    /// Gates C++-specific lowering (qualified members, CHA, namespaces).
    /// True for C++ TUs/headers and for `.h` files reached from a C++ TU.
    is_cpp: bool,
    /// `new_expression` node IDs already handled by `expr_to_rhs_flow` so
    /// `walk_function_body` skips them (avoids duplicate call sites with
    /// incorrect `this`-parameter wiring).
    handled_new_exprs: RefCell<HashSet<usize>>,
    /// Cache for `resolve_callee_with_loads`: maps the `func` node id of a
    /// field-expression callee to the whole answer it gave.
    /// `emit_field_value_store` (via `resolve_callee_var`) and the later
    /// `collect_call_at_node` (via `resolve_callee_with_loads`) reach the same
    /// node, and recomputing would create *two different* load variables for
    /// one expression — breaking the `CallReturnIndirect` →
    /// `indirect_return_dst` mapping — and a second summary receiver for an
    /// overloaded arrow. Memoizing the tuple rather than the load variable
    /// also keeps the second answer equal to the first: a node that resolves
    /// through `resolve_callee` names its receiver variable, which a cached
    /// bare `None` would have dropped.
    callee_load_cache: RefCell<HashMap<usize, CalleeRef>>,
    /// A call node has one lexical scope and static return type. Reuse
    /// receiver probes across outer-chain lookup and the body's later walk.
    call_receiver_cache: RefCell<HashMap<usize, TypeDesc>>,
    /// `call_expression` node id → `CallReturn` destination, so the matching
    /// `CallSite` can carry `return_dst` for `dlsym` models.
    call_return_dst: RefCell<HashMap<usize, VarId>>,
    /// Recursion depth of `lower_tree` (comma-operator chains in
    /// `clang/test/Sema/deep_recursion.c` are thousands of nested
    /// `binary_expression` nodes).
    ast_depth: u32,
    ast_depth_warned: bool,
    reference_vars: HashSet<VarId>,
    /// Every local registered in the C++ function being lowered, with the
    /// variable of that name it hid; a block unwinds its own on exit.
    local_scope_log: Vec<(String, Option<VarId>)>,
    /// The unit's syntax tree, for descending to a node's ancestors.
    tree: tree_sitter::Tree,
    /// Whether the unit can hold a `template_declaration` at all: a C++ unit
    /// whose text has the keyword. Most units have none, and then no
    /// signature needs its ancestors.
    has_templates: bool,
    has_weak: bool,
    record_link_ownership: bool,
    pending_flow_owners: HashMap<usize, FnId>,
    pending_initializer_owners: HashMap<usize, VarId>,
}

/// C++ class scope during member lowering.
#[derive(Clone)]
struct ClassCtx {
    /// Fully qualified class name (`ns::Cls`) — matches function/type names.
    qual_name: String,
}

impl LowerContext {
    fn namespace_scope(&self) -> String {
        let mut scope = String::new();
        for segment in self.ns_stack.iter().flatten() {
            if !scope.is_empty() {
                scope.push_str("::");
            }
            scope.push_str(segment);
        }
        scope
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
    if ctx.current_fn.is_none() {
        return;
    }
    // Only C++ blocks unwind the names they declare (walk_function_body).
    if ctx.is_cpp {
        let shadowed = ctx.locals.insert(name.clone(), id);
        ctx.local_scope_log.push((name, shadowed));
    } else {
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
    let result = build_program_inner(root, opts, jobs);
    trace_ir::release_thread_path_caches();
    // All per-run caches and the indexing pool have dropped at this point.
    crate::memory::reclaim_unused_pages();
    result
}

fn build_program_inner(
    root: &Path,
    opts: &PreprocessOptions,
    jobs: usize,
) -> Result<Program, String> {
    let jobs = jobs.max(1);
    // Include resolution memoizes `is_file` for the run (`is_file_cached`);
    // start from what the tree looks like now, not from what a previous run in
    // this process saw.
    trace_ir::start_file_probe_epoch();
    let mut program = Program::new(root.to_path_buf());
    program.include_paths = opts.include_paths.clone();
    // Set before any path is interned: `SymbolTable` decides `FileInfo::is_dep`
    // as it interns, so every later dep question is an O(1) file lookup (#60).
    program.symbols.set_dep_roots(
        opts.dep_roots
            .iter()
            .map(|d| trace_ir::canonicalize(d))
            .collect(),
    );
    program.defines = opts
        .defines
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    program.explore = opts.explore;
    program.explore_budget = opts.explore_budget;

    // A dependency root contributes headers only: its sources are never
    // translation units, even when the root sits inside the analyzed tree,
    // and its headers are discovered separately (#60).
    let dep_roots: Vec<PathBuf> = program.dep_roots().to_vec();
    let under_dep = |p: &PathBuf| dep_roots.iter().any(|dep| p.starts_with(dep));
    let (files, headers) = discover_source_files(root);
    let mut files = normalize_discovered_paths(files);
    let database = crate::compile_commands::CompilationDatabase::load(root, opts)?;
    files.extend(database.commands.keys().cloned());
    files.sort();
    files.dedup();
    for message in &database.warnings {
        program.add_diagnostic(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: None,
            line: 0,
            message: message.clone(),
            stage: "compile_commands".into(),
        });
    }
    let mut headers = normalize_discovered_paths(headers);
    files.retain(|p| !under_dep(p));
    headers.retain(|p| !under_dep(p));
    let dep_headers = normalize_discovered_paths(
        dep_roots
            .iter()
            .flat_map(|dep| discover_source_files(dep).1)
            .collect(),
    );
    let dep_note = if dep_roots.is_empty() {
        String::new()
    } else {
        format!(
            " + {} headers from {} dependency roots",
            dep_headers.len(),
            dep_roots.len()
        )
    };
    index_progress(format!(
        "discover: {} TUs, {} headers under {}{}",
        files.len(),
        headers.len(),
        root.display(),
        dep_note
    ));
    if files.is_empty() && headers.is_empty() {
        return Err(format!(
            "no C/C++ source files found under {}",
            root.display()
        ));
    }

    let mut include_graph =
        IncludeGraph::build_with_deps(root, &files, &headers, &dep_roots, &dep_headers);
    let links =
        crate::link_commands::LinkDatabase::load(root, &database, opts.link_commands.as_deref())?;
    for message in &links.warnings {
        program.add_diagnostic(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: None,
            line: 0,
            message: message.clone(),
            stage: "link_commands".into(),
        });
    }
    if !database.commands.is_empty() || !links.targets.is_empty() {
        return configured::build(
            program,
            root,
            opts,
            jobs,
            &files,
            &headers,
            include_graph,
            database,
            links,
        );
    }
    index_progress(format!(
        "include-graph: {} files, {} include edges",
        include_graph.project_files.len(),
        include_graph.edges.values().map(|v| v.len()).sum::<usize>()
    ));
    let file_order = include_graph.index_order(&files);

    let basename_index = Arc::new(include_graph.basename_index.clone());
    let include_expansion_cache =
        Arc::new(std::sync::RwLock::new(rustc_hash::FxHashMap::default()));
    let eff_opts = project_preprocess_opts(root, opts, &include_graph)
        .for_indexing()
        .with_include_expansion_cache(Arc::clone(&include_expansion_cache))
        .with_basename_index(basename_index)
        .with_inline_include_bodies(false);
    let eff_opts = eff_opts.with_record_conditionals(opts.explore || opts.record_conditionals);

    let gn_candidates = if opts.explore && opts.explore_budget > 0 {
        index_progress("explore: scanning GN candidate defines".to_string());
        Some(crate::explore::scan_project_gn_candidates(root))
    } else {
        None
    };
    let base_defines: BTreeMap<String, String> = opts
        .defines
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Warm each header under a FRESH macro environment seeded only from the
    // command-line defines. Sharing one accumulating table across headers let
    // include guards defined by earlier-warmed headers starve later headers'
    // expansions: the starved (empty) text was frozen into the expansion cache
    // and replayed to translation units, silently dropping every declaration
    // behind those guards (verified FN class on real corpora). Dedup comes
    // from the shared expansion cache instead; per-header tables only prevent
    // cross-header guard leakage.
    //
    // Translation units do NOT inherit a union of every header's macros.
    // That union used to exist because cached expansions were replayed
    // without executing their `#define`s; `splice_cached` replays an entry's
    // `ops` now, so a unit gets exactly the macros of the headers it
    // actually includes. Keeping the union was not merely redundant, it was
    // wrong twice over: a macro reached units that never included the header
    // defining it, and — since the union holds every header's include guard
    // — every header would read its own guard as already defined, so no
    // cached expansion could ever match its consumer's environment (#55).
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
    let mut warmed_as: HashMap<PathBuf, Vec<Language>> = HashMap::default();
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
                        .map(|(src, _)| {
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

    let reachable_from_c = include_graph.reachable_from(&c_sources);
    let cpp_parse: Arc<HashSet<PathBuf>> = Arc::new(include_graph.reachable_from(&cpp_tus));
    // See `index_language`: with no C translation unit in the tree, a `.h`
    // the include graph cannot tie to a C++ unit is still C++.
    let no_c_units = c_tus.is_empty();

    // One option set per language; each file takes the one matching the
    // language it is lexed and parsed as (`index_language`). Each unit starts
    // from the command-line defines alone and acquires header macros by
    // executing its own `#include`s.
    //
    // `index_opts` freezes the expansion cache; `discover_opts` does not.
    // Freezing used to be what kept output independent of thread scheduling,
    // since a worker insert was a first-writer-wins race. Fingerprinting
    // changes that: an expansion is a function of its file and of the macro
    // environment its fingerprint records, so two workers that reach the same
    // fingerprint produce the same entry and the race is not observable. What
    // IS still scheduling-dependent is which unit pioneers a variant and
    // therefore expands the header into its own text — so discovery writes
    // the cache and its texts are thrown away, and every text that reaches
    // the parser comes from the frozen pass below.
    let index_opts: HashMap<Language, PreprocessOptions> = [Language::C, Language::Cpp]
        .into_iter()
        .map(|l| {
            let o = eff_opts
                .clone()
                .with_frozen_expansion_cache(true)
                .with_language(l);
            (l, o)
        })
        .collect();
    let discover_opts: HashMap<Language, PreprocessOptions> = [Language::C, Language::Cpp]
        .into_iter()
        .map(|l| (l, eff_opts.clone().with_language(l)))
        .collect();

    let pool = index_pool(jobs)?;

    // Preprocess every translation unit BEFORE choosing the PCH set, in two
    // passes.
    //
    // Discovery runs against a writable cache. The warm pass only ever saw
    // each header on its own, under the command-line defines, but a unit
    // reaches a header after its siblings' `#define`s are in force, so that
    // one expansion matches almost nothing. Units that find no match expand
    // the header and publish what they built, so the environments this tree
    // actually presents end up in the cache — and units sharing an
    // environment, which is most of them, end up sharing one expansion.
    //
    // Settle then re-runs the units that expanded something, against the
    // frozen result. Each now finds the expansion its own environment
    // produced, so its text is file-local again and the header bodies are
    // lowered once each instead of once per consumer.
    let tu_paths: HashSet<PathBuf> = file_order.iter().cloned().collect();
    let pre_t = Instant::now();
    index_progress(format!(
        "preprocess: {} TUs (jobs={jobs})",
        file_order.len()
    ));
    let discover_t = Instant::now();
    let discovery = crate::expansion_discovery::Discovery {
        units: &file_order,
        graph: &include_graph,
        sources: &source_cache,
        expansions: &include_expansion_cache,
        opts: &discover_opts,
        language: &|p| index_language(p, &cpp_parse, no_c_units, forced_language),
    }
    .run(&pool, jobs);
    let discover_secs = discover_t.elapsed().as_secs_f64();
    // A unit that matched every include it reached already has the text the
    // settle pass would build for it: its includes hit the same expansions
    // either way, and with nothing expanded it opened no cache frame, so
    // writability changed nothing it produced. Only the units that expanded
    // something have to run again — against their own published expansions,
    // which is what turns an inlined body back into a shared one.
    let dirty: HashSet<PathBuf> = source_cache.units_that_inlined(&tu_paths);
    source_cache.evict_all(&dirty);
    // Discovery keeps every text resident until the pass is over. The units
    // that stand are spilled here, in parallel; the dirty ones are spilled by
    // the settle pass that rebuilds them, so nothing is written twice.
    pool.install(|| {
        file_order
            .par_iter()
            .filter(|path| !dirty.contains(*path))
            .try_for_each(|path| source_cache.spill(path, &include_graph))
    })?;
    let settle_t = Instant::now();
    pool.install(|| {
        dirty.par_iter().try_for_each(|path| {
            let lang = index_language(path, &cpp_parse, no_c_units, forced_language);
            let _ = source_cache.get_or_preprocess(path, &include_graph, &index_opts[&lang]);
            source_cache.spill(path, &include_graph)
        })
    })?;
    // Split the two passes apart: discovery and the settle pass have
    // different cures, so a single total hides which one to attack (#83).
    index_progress(format!(
        "preprocess-done: {:.1}s ({:.1}s discovery, {} runs discarded, {} in order + {:.1}s settle of {} of {} units)",
        pre_t.elapsed().as_secs_f64(),
        discover_secs,
        discovery.discarded,
        discovery.in_order,
        settle_t.elapsed().as_secs_f64(),
        dirty.len(),
        file_order.len()
    ));

    // Preprocessing probes are not used by header/TU lowering. Drop both
    // serial and worker memos before retaining the header IR cache.
    trace_ir::release_thread_path_caches();
    pool.broadcast(|_| trace_ir::release_thread_path_caches());
    crate::memory::reclaim_unused_pages();

    // A header every unit expanded for itself must not ALSO get a PCH unit:
    // that unit would carry whichever configuration the warm pass happened to
    // take, which may be one no translation unit in this tree has. A header
    // some unit did take from the cache still needs one, and so does every
    // header reachable from it (nested PCH merges types from those).
    let provenance = source_cache.header_provenance(&tu_paths);
    // Nested PCH merges types from the headers a PCH'd header includes, so a
    // header consumed from the cache keeps the whole closure below it.
    let consumed_closure = include_graph.reachable_from(&provenance.consumed_paths);
    let skip_headers: HashSet<PathBuf> = provenance
        .inlined
        .iter()
        .filter(|h| !consumed_closure.contains(*h))
        .cloned()
        .collect();
    let lang_of = |p: &Path| index_language(p, &cpp_parse, no_c_units, forced_language);
    // Close the consumed set over nesting: replaying an expansion pins the
    // expansions it in turn replayed, and a consumer never visits those.
    let wanted_variants =
        close_over_nested_variants(&provenance.consumed, &include_expansion_cache);

    // Macro includes discovered while warming can make a previously "orphan"
    // header reachable from a `.c`. Those must be PCH'd into `header_ir` so
    // TUs can merge their prototypes; they must not stay on the orphan path
    // (merged into the global program only, invisible at TU lower time).
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
    pch_headers.retain(|p| !skip_headers.contains(p));
    let pch_order = Arc::new(include_graph.index_order(&pch_headers));
    let pch_set: HashSet<PathBuf> = pch_headers.iter().cloned().collect();
    // Orphans get the same treatment: indexing a header standalone that every
    // unit already expanded for itself would reintroduce the configuration
    // the PCH filter just dropped.
    let orphan_headers: Vec<PathBuf> = include_graph.index_order(
        &project_headers
            .iter()
            .filter(|p| {
                !pch_set.contains(*p) && !skip_headers.contains(*p) && !program.is_dep_path(p)
            })
            .cloned()
            .collect::<Vec<_>>(),
    );

    index_progress(format!(
        "parse: {} orphan headers, {} TUs (jobs={jobs})",
        orphan_headers.len(),
        file_order.len()
    ));
    // A warmed header's text is read back only when the header is indexed on
    // its own. Every other warmed header keeps its provenance for the
    // queries above and drops its text without ever writing it to disk.
    let orphan_set: HashSet<&Path> = orphan_headers.iter().map(PathBuf::as_path).collect();
    pool.install(|| {
        warmed_as.par_iter().try_for_each(|(path, _)| {
            if orphan_set.contains(path.as_path()) {
                source_cache.spill(path, &include_graph)
            } else {
                source_cache.release_text(path, &include_graph);
                Ok(())
            }
        })
    })?;

    // One unit per expansion some translation unit actually replayed. A
    // header whose only stored expansion nothing consumed contributes
    // nothing: that is the warm pass's configuration, and if no unit
    // presents it, it is not a configuration this tree has.
    let pch_units: Vec<(PathBuf, usize)> = pch_headers
        .iter()
        .flat_map(|p| {
            let mut vs =
                variants_to_lower(&wanted_variants, &include_expansion_cache, p, lang_of(p));
            vs.sort_unstable();
            vs.into_iter().map(move |v| (p.clone(), v))
        })
        .collect();
    let pch_t = Instant::now();
    index_progress(format!(
        "pch: parse {} expansions of {} headers",
        pch_units.len(),
        pch_headers.len()
    ));
    // Include-graph order (included files before includers), including
    // preprocess-only edges so a header is never PCH'd in the same wave as a
    // nested type the raw `#include` scanner missed. Nested merge copies
    // types/typedefs only; TUs pull prototypes from every reachable header.
    // Cyclic leftovers are indexed in `index_order`, never as a parallel wave.
    let mut header_ir_map: HeaderIr = HashMap::default();
    let (pch_waves, pch_cycles) = if jobs == 1 {
        (vec![pch_order.as_ref().clone()], Vec::new())
    } else {
        include_graph.index_waves(&pch_headers)
    };
    for wave in pch_waves {
        if wave.is_empty() {
            continue;
        }
        let wave_units: Vec<(PathBuf, usize)> = wave
            .iter()
            .flat_map(|p| {
                let mut vs =
                    variants_to_lower(&wanted_variants, &include_expansion_cache, p, lang_of(p));
                vs.sort_unstable();
                vs.into_iter().map(move |v| (p.clone(), v))
            })
            .collect();
        if wave_units.is_empty() {
            continue;
        }
        if jobs == 1 || wave_units.len() == 1 {
            for (path, variant) in &wave_units {
                let unit = index_header_variant(
                    path,
                    *variant,
                    root,
                    &include_graph,
                    &include_expansion_cache,
                    index_language(path, &cpp_parse, no_c_units, forced_language),
                    Some(&header_ir_map),
                    pch_order.as_ref(),
                );
                push_header_ir(
                    &mut header_ir_map,
                    &include_graph,
                    path,
                    lang_of(path),
                    *variant,
                    unit,
                );
            }
        } else {
            // The map is only extended between waves, so workers read it in
            // place rather than from a per-wave copy.
            let snapshot = &header_ir_map;
            let units: Vec<(PathBuf, usize, UnitIndex)> = pool.install(|| {
                wave_units
                    .par_iter()
                    .map(|(path, variant)| {
                        (
                            path.clone(),
                            *variant,
                            index_header_variant(
                                path,
                                *variant,
                                root,
                                &include_graph,
                                &include_expansion_cache,
                                index_language(path, &cpp_parse, no_c_units, forced_language),
                                Some(snapshot),
                                pch_order.as_ref(),
                            ),
                        )
                    })
                    .collect()
            });
            for (path, variant, unit) in units {
                push_header_ir(
                    &mut header_ir_map,
                    &include_graph,
                    &path,
                    lang_of(&path),
                    variant,
                    unit,
                );
            }
        }
    }
    for path in include_graph.index_order(&pch_cycles) {
        let mut variants = variants_to_lower(
            &wanted_variants,
            &include_expansion_cache,
            &path,
            lang_of(&path),
        );
        variants.sort_unstable();
        for variant in variants {
            let unit = index_header_variant(
                &path,
                variant,
                root,
                &include_graph,
                &include_expansion_cache,
                index_language(&path, &cpp_parse, no_c_units, forced_language),
                Some(&header_ir_map),
                pch_order.as_ref(),
            );
            push_header_ir(
                &mut header_ir_map,
                &include_graph,
                &path,
                lang_of(&path),
                variant,
                unit,
            );
        }
    }
    let header_ir = Arc::new(header_ir_map);
    crate::memory::reclaim_unused_pages();
    index_progress(format!(
        "pch-done: {:.1}s ({} units)",
        pch_t.elapsed().as_secs_f64(),
        header_ir.len()
    ));
    for path in include_graph.index_order(&header_ir.keys().cloned().collect::<Vec<_>>()) {
        if let Some(units) = header_ir.get(&path) {
            // A dependency header's own unit is not a translation unit of
            // the target: merge prototypes, not its bodies' call sites (#60).
            let is_dep = program.is_dep_path(&path);
            for (_, _, unit) in units {
                if is_dep {
                    merge_unit_symbols(&mut program, unit.as_ref());
                } else {
                    merge_unit_header_preamble(&mut program, unit.as_ref());
                }
            }
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
                        &index_opts[&index_language(path, &cpp_parse, no_c_units, forced_language)],
                        &source_cache,
                        Some(&header_ir),
                        pch_order.as_ref(),
                    ),
                );
                source_cache.evict(path, &include_graph);
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
            index_in_window(
                &orphan_headers,
                jobs,
                |path| {
                    let unit = index_source_file(
                        path,
                        root,
                        &include_graph,
                        &index_opts[&index_language(path, &cpp_parse, no_c_units, forced_language)],
                        &source_cache,
                        Some(&header_ir),
                        pch_order.as_ref(),
                    );
                    source_cache.evict(path, &include_graph);
                    unit
                },
                |unit| merge_unit_index(&mut program, &unit),
            );
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
                let lang = index_language(path, &cpp_parse, no_c_units, forced_language);
                let (base_unit, var_units) = index_source_file_with_variants(
                    path,
                    root,
                    &include_graph,
                    &index_opts[&lang],
                    &source_cache,
                    Some(&header_ir),
                    pch_order.as_ref(),
                    gn_candidates.as_ref(),
                    &base_defines,
                    opts.explore_budget,
                );
                source_cache.evict(path, &include_graph);
                merge_unit_variants(&mut program, &base_unit, &var_units);
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
            index_in_window(
                &file_order,
                jobs,
                |path| {
                    let lang = index_language(path, &cpp_parse, no_c_units, forced_language);
                    let units = index_source_file_with_variants(
                        path,
                        root,
                        &include_graph,
                        &index_opts[&lang],
                        &source_cache,
                        Some(&header_ir),
                        pch_order.as_ref(),
                        gn_candidates.as_ref(),
                        &base_defines,
                        opts.explore_budget,
                    );
                    // Header provenance and PCH construction are complete.
                    // Keep the source through variant generation, then release it.
                    source_cache.evict(path, &include_graph);
                    units
                },
                |(base_unit, var_units)| merge_unit_variants(&mut program, &base_unit, &var_units),
            );
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
    let inferred_dirs = include_graph.include_dirs.clone();
    source_cache.check_load_errors()?;
    finalize_program(&mut program, &include_graph, inferred_dirs);

    Ok(program)
}

/// The closing steps every indexing path shares. `extra_dirs` are the search
/// directories that path observed, recorded in first-seen order.
fn finalize_program(
    program: &mut Program,
    graph: &IncludeGraph,
    extra_dirs: impl IntoIterator<Item = PathBuf>,
) {
    program.include_deps = graph.edge_list();
    let mut seen: HashSet<PathBuf> = program.include_paths.iter().cloned().collect();
    for dir in extra_dirs {
        if seen.insert(dir.clone()) {
            program.include_paths.push(dir);
        }
    }
    finalize_extern_callees(program);
    // Members first: a call is bound when its callee takes `this`, which a
    // member defined without its class in view only has afterwards.
    add_missing_this_params(program);
    bind_calls_past_this(program);
    expand_virtual_overrides(program);
    expand_internal_overload_refs(program);
}

/// Widen each reference to a function by name to every overload the name may
/// mean. A name resolves to the first internal-linkage entry of its file
/// (`static void cb(int)`), while `void (*p)(double) = cb;` takes its overload
/// `cb(double)`. A function pointer's type does not carry the parameter types
/// that would pick one, so the reference keeps every overload, as when one
/// entry held all their bodies.
fn expand_internal_overload_refs(program: &mut Program) {
    if !program.symbols.has_internal_overloads() {
        return;
    }
    let symbols = &program.symbols;
    let var_file = |var: VarId| symbols.variable_by_id(var).map(|v| v.span.file);
    let overloads = |callee: FnId, file: Option<trace_ir::FileId>| {
        file.into_iter()
            .flat_map(move |file| symbols.internal_overloads_seen_from(callee, file))
    };
    let mut added = Vec::new();
    for constraint in &program.flow {
        match *constraint {
            FlowConstraint::AddrOfFn { dst, callee } => added.extend(
                overloads(callee, var_file(dst))
                    .map(|callee| FlowConstraint::AddrOfFn { dst, callee }),
            ),
            FlowConstraint::ArrayFnMember { array, callee } => added.extend(
                overloads(callee, var_file(array))
                    .map(|callee| FlowConstraint::ArrayFnMember { array, callee }),
            ),
            _ => {}
        }
    }
    let mut returns: Vec<(FnId, ReturnFlow)> = Vec::new();
    for (&owner, flows) in &program.fn_returns {
        let file = symbols.function_by_id(owner).map(|f| f.span.file);
        for flow in flows {
            if let ReturnFlow::AddrOfFn { callee } = *flow {
                returns.extend(
                    overloads(callee, file).map(|callee| (owner, ReturnFlow::AddrOfFn { callee })),
                );
            }
        }
    }
    program.flow.extend(added);
    for (owner, flow) in returns {
        program.fn_returns.entry(owner).or_default().push(flow);
    }
    program.symbols.pass_internal_overloads_as_args();
}

/// Give a member defined in a unit that never saw its class the implicit
/// `this` it has everywhere else. Lowering prepends `this` only when the
/// definition's unit knows the class: one whose header include is unresolved
/// lowered `void Remote::Shared(Callback cb)` with `cb` at position 0, yet
/// merged with the in-class prototype, whose explicit arity agrees, and every
/// call bound past `this` found no parameter at position 1. The merge records
/// those definitions (`SymbolTable::members_missing_this`); the prototype was
/// registered under its class, so the name's qualifier is the class. A class
/// is not looked up by name here: unrelated programs in one tree can declare
/// a class and a namespace of the same name.
fn add_missing_this_params(program: &mut Program) {
    for fn_id in program.symbols.take_members_missing_this() {
        let Some(i) = program.symbols.function_index(fn_id) else {
            continue;
        };
        let f = &program.symbols.functions[i];
        // A conversion operator's target type is no qualifier:
        // `H::operator ns::S` is a member of `H` (`scope_part`).
        let scope = scope_part(&f.name);
        let cls = if scope.len() < f.name.len() {
            scope.strip_suffix("::")
        } else {
            f.name.rsplit_once("::").map(|(cls, _)| cls)
        };
        let Some(cls) = cls else {
            continue;
        };
        let (cls, span, target) = (cls.to_owned(), f.span, f.target);
        let this_id = add_this_param(program, &cls, fn_id, span);
        program.symbols.variable_mut(this_id).target = target;
        let params = &mut program.symbols.functions[i].params;
        params.insert(0, this_id);
        for &param in &params[1..] {
            let var = &mut program.symbols.variables[param.0 as usize];
            var.param_index = var.param_index.map(|index| index + 1);
        }
    }
}

/// Bind every call whose callee takes `this` past it, when lowering did not.
/// Lowering binds a call's arguments where its unit can tell the callee is a
/// member (`CallSite::args_bound_past_this`). A unit that never saw the class
/// cannot: a qualified call whose header include is unresolved recorded
/// `Remote::Shared(cb)` from position 0, and so did a call beside a
/// definition that gains its `this` only after the merge. The callees are
/// the ones the solver wires (`SymbolTable::callees_of`); a member's name is
/// always qualified, so an unqualified name is not looked up.
fn bind_calls_past_this(program: &mut Program) {
    for i in 0..program.symbols.call_sites.len() {
        let cs = &program.symbols.call_sites[i];
        if cs.args_bound_past_this || (cs.var_args.is_empty() && cs.fn_args.is_empty()) {
            continue;
        }
        let takes_this = match cs.callee_fn_id {
            Some(callee) => is_member_function(program, callee),
            None => {
                cs.callee_name.contains("::") && {
                    let callees = program.callees_of(cs);
                    !callees.is_empty() && callees.iter().all(|&f| is_member_function(program, f))
                }
            }
        };
        if !takes_this {
            continue;
        }
        let site = &mut program.symbols.call_sites[i];
        shift_past_this(
            &mut site.var_args,
            &mut site.fn_args,
            &mut site.addr_of_member_args,
        );
        site.args_bound_past_this = true;
    }
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
    if !program.link_targets.is_empty() {
        finalize_target_extern_callees(program);
        return;
    }
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
    // The sites each name may claim, in table order, gathered once: a scan
    // of the whole table per name was quadratic (1.5s of camera's index).
    let mut sites_by_name: HashMap<&str, Vec<usize>> = names
        .iter()
        .map(|(name, _, _)| (name.as_str(), Vec::new()))
        .collect();
    for (i, cs) in program.symbols.call_sites.iter().enumerate() {
        if !cs.is_direct && cs.callee_var.is_none() {
            if let Some(sites) = sites_by_name.get_mut(cs.callee_name.as_str()) {
                sites.push(i);
            }
        }
    }
    for (name, file, line) in &names {
        // A symbol already exists for this name (in-tree prototype or
        // definition): leave the site untouched so the solver's name-based
        // recovery classifies it — defined-elsewhere resolves to a real
        // Direct edge with param wiring; prototype-only becomes External.
        // Synthesizing over it would orphan the real definition.
        if program.symbols.resolve_function(name).is_some() {
            continue;
        }
        let fid = program.symbols.alloc_fn_id();
        program.symbols.push_synthetic_function(trace_ir::Function {
            is_weak: false,
            target: None,
            id: fid,
            name: name.clone(),
            linkage: trace_ir::Linkage::External,
            return_type: trace_ir::TypeId(0),
            params: Vec::new(),
            locals: Vec::new(),
            is_cpp: false,
            span: trace_ir::Span {
                file: *file,
                line: *line,
                col: 0,
            },
            end_line: *line,
            file: *file,
            is_defined: false,
            param_type_ids: Vec::new(),
            explicit_arity: None,
            default_args: 0,
            owner_unresolved: false,
            variadic: false,
            defaulted_in_class: false,
            declared_in_class: false,
            is_virtual: false,
            is_final: false,
            tu: None,
        });
        for &i in &sites_by_name[name.as_str()] {
            let cs = &mut program.symbols.call_sites[i];
            cs.is_direct = true;
            cs.callee_fn_id = Some(fid);
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
/// An unresolved external belongs to the same image as its caller. A name
/// present only in another image must not prevent synthesizing this declaration.
fn finalize_target_extern_callees(program: &mut Program) {
    // Group first, resolve once per group. Resolving per site re-walks the
    // scope table for every call — the cost the whole-program pass above was
    // restructured to avoid (#108).
    let mut grouped: std::collections::BTreeMap<(&str, Option<trace_ir::TargetId>), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, cs) in program.symbols.call_sites.iter().enumerate() {
        if cs.callee_var.is_some()
            || cs.callee_fn_id.is_some()
            || !is_synthesizable_extern(&cs.callee_name)
        {
            continue;
        }
        let target = program.symbols.function(cs.caller).target;
        grouped
            .entry((cs.callee_name.as_str(), target))
            .or_default()
            .push(i);
    }
    let missing: std::collections::BTreeMap<(String, Option<trace_ir::TargetId>), Vec<usize>> =
        grouped
            .into_iter()
            .filter(|((name, target), _)| {
                // Unscoped by file: the group spans call sites in many files,
                // and a file-`static` visible from just one of them must not
                // decide for the rest. Matches the whole-program pass, which
                // asks `resolve_function` without a file.
                program
                    .symbols
                    .resolve_function_candidates_in_target(name, None, *target)
                    .is_empty()
            })
            .map(|((name, target), sites)| ((name.to_owned(), target), sites))
            .collect();
    for ((name, target), sites) in missing {
        let span = program.symbols.call_sites[sites[0]].span;
        let id = program.symbols.alloc_fn_id();
        program.symbols.push_synthetic_function(Function {
            id,
            name,
            target,
            is_weak: false,
            linkage: trace_ir::Linkage::External,
            return_type: program.types.unknown(),
            params: Vec::new(),
            locals: Vec::new(),
            span,
            end_line: span.line,
            file: span.file,
            is_defined: false,
            param_type_ids: Vec::new(),
            explicit_arity: None,
            owner_unresolved: false,
            variadic: false,
            defaulted_in_class: false,
            declared_in_class: false,
            default_args: 0,
            is_virtual: false,
            is_final: false,
            is_cpp: false,
            tu: None,
        });
        for i in sites {
            let cs = &mut program.symbols.call_sites[i];
            cs.callee_fn_id = Some(id);
            cs.is_direct = true;
        }
    }
}

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
        // Every file's anonymous namespace spells a class the same, so a
        // member the call cannot see (in neither its file nor a header that
        // file includes) is another class's: it does not make the callee
        // virtual, never stops dispatch as `final`, and is no target when its
        // class is the receiver's. When the receiver is itself such a class
        // seen here, so is its whole hierarchy, since a subclass names it: a
        // member of a class in that hierarchy is a target only where the call
        // sees it, not a same-named external class's or its subclasses'. A
        // base declared outside an anonymous namespace still dispatches to the
        // overrides in every file.
        //
        // A callee bound to such a member was looked up where its class is
        // seen, which a header can make another file than the call's.
        let bound_in = (f.linkage == trace_ir::Linkage::Internal).then_some(f.file);
        let sees = |file: trace_ir::FileId| {
            program.symbols.file_sees(cs.span.file, file)
                || bound_in.is_some_and(|b| program.symbols.file_sees(b, file))
        };
        let visible = |t: FnId| {
            let t = program.symbols.function(t);
            t.linkage != trace_ir::Linkage::Internal || sees(t.file)
        };
        let virtual_dispatch = kind.is_destructor()
            || f.is_virtual
            || program
                .symbols
                .functions_named(&kind.name_on(&cls))
                .into_iter()
                .any(|t| program.symbols.function(t).is_virtual && visible(t));
        if !virtual_dispatch {
            continue;
        }
        let root = cs.receiver_class.as_deref().unwrap_or(&cls);
        let expected_arity = method_explicit_arity(program, fid);
        let mut targets = if bound_in.is_some() || program.class_is_anonymous_in(root, sees) {
            let hierarchy: Vec<String> = program
                .subclass_closure(root)
                .iter()
                .map(|c| kind.name_on(c))
                .collect();
            let seen_here = |t: FnId| {
                let target = program.symbols.function(t);
                visible(t) && (sees(target.file) || !hierarchy.contains(&target.name))
            };
            program.method_targets_among(root, &kind, &sees, &seen_here)
        } else {
            let own_name = kind.name_on(root);
            let mut targets = program.method_targets_among(root, &kind, &sees, &|_| true);
            // A member the call cannot see is a member of another file's
            // anonymous class: a target only when, as its own file sees it,
            // that class derives from the receiver's class or an external
            // class under it, not merely shares a name with one of them.
            let hierarchy = std::cell::OnceCell::new();
            targets.retain(|&t| {
                let target = program.symbols.function(t);
                if visible(t) {
                    return true;
                }
                target.name != own_name
                    && method_kind_of_function(&target.name).is_some_and(|(owner, _)| {
                        let hierarchy: &Vec<String> =
                            hierarchy.get_or_init(|| program.subclass_closure(root));
                        program.anonymous_class_derives_from(
                            &owner,
                            &|file| program.symbols.file_sees(target.file, file),
                            &|base| hierarchy.iter().any(|c| c == base),
                        )
                    })
            });
            targets
        };
        targets.retain(|&t| {
            program.symbols.function(t).target == f.target
                && arity_compatible(expected_arity, method_explicit_arity(program, t))
        });
        for t in targets {
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
                args_bound_past_this: cs.args_bound_past_this,
                span: cs.span,
                is_direct: true,
                receiver_class: cs.receiver_class.clone(),
                return_dst: cs.return_dst,
                tu: cs.tu,
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

/// Units the workers may take ahead of the merge, per worker, and the bounds
/// on the total. This bounds the IR held before merging (at most the window
/// indexed or in flight) without making anyone wait for a batch: a slow unit
/// holds up the merge, not the other workers. The cap keeps a many-core host
/// from holding dozens of lowered units behind one straggler; two per worker
/// measured the same as four on Clang with eight workers.
const INDEX_WINDOW_PER_WORKER: usize = 2;
const INDEX_WINDOW_MIN: usize = 4;
const INDEX_WINDOW_MAX: usize = 32;

/// The window for `jobs` workers.
fn index_window(jobs: usize) -> usize {
    jobs.max(1)
        .saturating_mul(INDEX_WINDOW_PER_WORKER)
        .clamp(INDEX_WINDOW_MIN, INDEX_WINDOW_MAX)
}

/// Index `items` on `jobs` workers and merge them in the order given.
///
/// Workers take units in order and park a finished unit until the merge
/// reaches it; the merge runs on this thread while the workers go on. A
/// worker that is [`index_window`] units ahead of the merge waits for it, so
/// the pending IR is bounded by the window rather than by the corpus, and
/// the merge order is that of `items` regardless of which unit finishes
/// first.
///
/// A panic in `index` or `merge` unwinds through the scope as it would
/// without the window: whoever panics marks the run cancelled on the way
/// out and wakes every waiter, so no thread waits for a result that will
/// never come, and the scope re-raises the panic once the rest have left.
fn index_in_window<T: Send>(
    items: &[PathBuf],
    jobs: usize,
    index: impl Fn(&PathBuf) -> T + Sync,
    mut merge: impl FnMut(T) + Send,
) {
    struct Window<T> {
        /// Next unit a worker takes.
        next: usize,
        /// Units merged so far; the window is measured from here.
        merged: usize,
        /// Finished units the merge has not reached yet.
        done: BTreeMap<usize, T>,
        /// Set by a thread unwinding from a panic; everyone else stops.
        cancelled: bool,
    }
    /// The two things waited for: the merge waits for the next unit to be
    /// done, the workers wait for the window to move. Separate so that a
    /// finished unit wakes only the merge and a merged one only the workers.
    struct Signals {
        unit_done: Condvar,
        window_moved: Condvar,
    }
    /// Marks the run cancelled if the holder unwinds from a panic.
    struct CancelOnPanic<'a, T> {
        state: &'a Mutex<Window<T>>,
        signals: &'a Signals,
    }
    impl<T> Drop for CancelOnPanic<'_, T> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cancelled = true;
                self.signals.unit_done.notify_all();
                self.signals.window_moved.notify_all();
            }
        }
    }
    let jobs = jobs.max(1);
    let window = index_window(jobs);
    let state = Mutex::new(Window {
        next: 0,
        merged: 0,
        done: BTreeMap::new(),
        cancelled: false,
    });
    let signals = Signals {
        unit_done: Condvar::new(),
        window_moved: Condvar::new(),
    };
    let (index, state, signals) = (&index, &state, &signals);
    // The critical sections below only move indices and park units, so a
    // lock poisoned by a panic elsewhere holds a consistent window; the
    // cancellation flag is what stops the run.
    let lock = || state.lock().unwrap_or_else(|e| e.into_inner());
    rayon::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(move |_| {
                let _cancel = CancelOnPanic { state, signals };
                loop {
                    let i = {
                        let mut st = lock();
                        while !st.cancelled
                            && st.next >= st.merged + window
                            && st.next < items.len()
                        {
                            st = signals
                                .window_moved
                                .wait(st)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        if st.cancelled || st.next >= items.len() {
                            return;
                        }
                        st.next += 1;
                        st.next - 1
                    };
                    let unit = index(&items[i]);
                    lock().done.insert(i, unit);
                    signals.unit_done.notify_one();
                }
            });
        }
        let _cancel = CancelOnPanic { state, signals };
        for i in 0..items.len() {
            let unit = {
                let mut st = lock();
                loop {
                    if st.cancelled {
                        // A worker's panic is re-raised by the scope.
                        return;
                    }
                    if let Some(unit) = st.done.remove(&i) {
                        break unit;
                    }
                    st = signals
                        .unit_done
                        .wait(st)
                        .unwrap_or_else(|e| e.into_inner());
                }
            };
            merge(unit);
            lock().merged = i + 1;
            signals.window_moved.notify_all();
        }
    });
}

/// Indexing workers recurse on deep expressions and need a larger stack.
fn index_pool(jobs: usize) -> Result<rayon::ThreadPool, String> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .stack_size(16 * 1024 * 1024)
        .build()
        .map_err(|e| e.to_string())
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
        eff.source_cache = Some(Arc::new(trace_preproc::SourceCache::new(
            graph.source_cache.clone(),
        )));
    }
    let _ = root;
    eff
}

pub(crate) fn is_index_header(path: &Path) -> bool {
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
    let mut wanted: HashSet<&Path> = HashSet::default();
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
/// The language a file is lexed and parsed as during indexing.
///
/// `cpp_parse` holds everything the include graph can reach from a C++
/// translation unit. A file outside it falls back to its extension, and
/// `.h` is language-ambiguous, so an orphan header — one no translation
/// unit includes — used to be read as C even in a tree that contains no C
/// at all. That is not a harmless default: the C lexer splits `->*`, and
/// the C grammar does not know namespaces, so such a header lowered a
/// second, unqualified copy of every declaration it pulled in and interned
/// a phantom function for the operand of every pointer-to-member call
/// (#37). `no_c_units` settles the ambiguity where the tree itself does:
/// with no C translation unit anywhere, an ambiguous header is C++. A
/// mixed tree keeps the extension fallback, where the ambiguity is real.
/// Lowered units for one header: one per cached expansion of it that some
/// translation unit replayed (see `trace_preproc::ExpansionVariants`).
/// Ordered by variant index.
type HeaderIr = HashMap<PathBuf, Vec<(Language, usize, Arc<UnitIndex>)>>;

fn push_header_ir(
    map: &mut HeaderIr,
    graph: &IncludeGraph,
    path: &Path,
    language: Language,
    variant: usize,
    unit: UnitIndex,
) {
    let slot = map.entry(graph.intern_path(path)).or_default();
    slot.push((language, variant, Arc::new(unit)));
    slot.sort_by_key(|(_, v, _)| *v);
}

/// Every `(header, variant)` a unit depends on, given the ones units
/// replayed directly.
///
/// Replaying an expansion pins the expansions it in turn replayed: indexing
/// keeps each header's text file-local, so a nested header contributes no
/// text to its includer's entry and needs its own unit — and the consumer
/// never visits it, so only the entry itself knows which one applies.
fn close_over_nested_variants(
    consumed: &HashSet<(PathBuf, Language, usize)>,
    cache: &trace_preproc::ExpansionCache,
) -> HashMap<(PathBuf, Language), HashSet<usize>> {
    let mut wanted: HashMap<(PathBuf, Language), HashSet<usize>> = HashMap::default();
    let mut queue: Vec<(PathBuf, Language, usize)> = consumed.iter().cloned().collect();
    while let Some((path, language, variant)) = queue.pop() {
        if !wanted
            .entry((path.clone(), language))
            .or_default()
            .insert(variant)
        {
            continue;
        }
        // A nested record was made by the same run, so it indexes the same
        // language's list.
        let nested = {
            let Ok(guard) = cache.read() else { continue };
            guard
                .get(&(path, language))
                .and_then(|vs| vs.get(variant))
                .map(|e| Arc::clone(&e.nested_variants))
        };
        if let Some(nested) = nested {
            queue.extend(nested.iter().map(|(p, v)| (p.clone(), language, *v)));
        }
    }
    wanted
}

/// Which expansions of `path` to lower.
///
/// Normally the ones units replayed, directly or through a nesting chain.
/// A header nothing recorded is one no unit reached — it is only in the PCH
/// set because the include graph says a `.c` could get there — and it is
/// still indexed, from every expansion the warm pass built, so that dropping
/// a configuration is never how a declaration goes missing.
fn variants_to_lower(
    wanted: &HashMap<(PathBuf, Language), HashSet<usize>>,
    cache: &trace_preproc::ExpansionCache,
    path: &Path,
    language: Language,
) -> Vec<usize> {
    if let Some(vs) = wanted.get(&(path.to_path_buf(), language)) {
        return vs.iter().copied().collect();
    }
    cache
        .read()
        .ok()
        .and_then(|guard| {
            guard
                .get(&(path.to_path_buf(), language))
                .map(|vs| vs.len())
        })
        .map(|n| (0..n).collect())
        .unwrap_or_default()
}

/// Lower one stored expansion of a header into its own unit.
///
/// The text comes from the cache rather than from re-preprocessing the
/// header: re-preprocessing would rebuild it under the command-line defines
/// alone, which is the very configuration mismatch this is here to avoid.
#[allow(clippy::too_many_arguments)]
fn index_header_variant(
    path: &Path,
    variant: usize,
    root: &Path,
    graph: &IncludeGraph,
    cache: &trace_preproc::ExpansionCache,
    language: Language,
    header_ir: Option<&HeaderIr>,
    pch_order: &[PathBuf],
) -> UnitIndex {
    let expansion = cache.read().ok().and_then(|guard| {
        guard
            .get(&(graph.intern_path(path), language))
            .and_then(|vs| vs.get(variant))
            .cloned()
    });
    let Some(expansion) = expansion else {
        return UnitIndex {
            path: path.to_path_buf(),
            ..Default::default()
        };
    };
    let pre = Arc::new(PreprocessedSource::from_expansion(&expansion, language));
    let mut program = Program::new(root.to_path_buf());
    match lower_prepared_source(
        &mut program,
        path,
        graph,
        pre,
        language,
        false,
        header_ir,
        pch_order,
    ) {
        Ok(()) => {
            let mut unit = program_into_unit(path.to_path_buf(), program);
            // A header's unit is merged into every unit below it; a TU's
            // once, which the precomputation would not pay for.
            unit.merge_descs = crate::merge::merge_descs_of(&unit.types);
            unit
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

fn index_language(
    path: &Path,
    cpp_parse: &HashSet<PathBuf>,
    no_c_units: bool,
    forced: Option<Language>,
) -> Language {
    forced.unwrap_or_else(|| {
        if cpp_parse.contains(path) {
            return Language::Cpp;
        }
        match Language::from_path(path) {
            Language::Cpp => Language::Cpp,
            Language::C if no_c_units && is_index_header(path) => Language::Cpp,
            Language::C => Language::C,
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
    header_ir: Option<&HeaderIr>,
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
fn index_source_file_with_variants(
    path: &Path,
    root: &Path,
    graph: &IncludeGraph,
    index_opts: &PreprocessOptions,
    source_cache: &IndexSourceCache,
    header_ir: Option<&HeaderIr>,
    pch_order: &[PathBuf],
    gn_candidates: Option<&std::collections::HashMap<String, Vec<Candidate>>>,
    base_defines: &BTreeMap<String, String>,
    explore_budget: usize,
) -> (UnitIndex, Vec<UnitIndex>) {
    let mut base_unit = index_source_file(
        path,
        root,
        graph,
        index_opts,
        source_cache,
        header_ir,
        pch_order,
    );
    let mut variant_units = Vec::new();
    let Some(candidates) = gn_candidates.filter(|c| explore_budget > 0 && !c.is_empty()) else {
        return (base_unit, variant_units);
    };
    // The base lowering has already populated this cache. A failure is already
    // recorded on base_unit, so do not emit a duplicate diagnostic.
    let Ok(pre) = source_cache.get_or_preprocess(path, graph, index_opts) else {
        return (base_unit, variant_units);
    };
    let language = index_opts
        .language
        .unwrap_or_else(|| Language::from_path(path));

    let (variants, truncated) = crate::explore::generate_feasible_variants(
        &pre.conditionals,
        candidates,
        base_defines,
        explore_budget,
    );
    let mut warn_explore = |message: String| {
        base_unit.diagnostics.push(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: None,
            line: 0,
            message,
            stage: "explore".into(),
        });
    };

    if truncated > 0 {
        warn_explore(format!(
            "variant exploration budget ({explore_budget}) reached; omitted {truncated} candidate activation goal(s)"
        ));
    }

    for variant in variants {
        let mut var_opts = index_opts.clone();
        var_opts.record_conditionals = false;
        var_opts.defines.extend(variant.defines.iter().cloned());
        match source_cache.preprocess_uncached(path, graph, &var_opts) {
            Ok((var_pre, _)) => {
                let mut var_program = Program::new(root.to_path_buf());
                match lower_prepared_source(
                    &mut var_program,
                    path,
                    graph,
                    Arc::new(var_pre),
                    language,
                    var_opts.record_link_ownership,
                    header_ir,
                    pch_order,
                ) {
                    Ok(()) => {
                        variant_units.push(program_into_unit(path.to_path_buf(), var_program));
                    }
                    Err(e) => {
                        warn_explore(format!("variant lower failed for {}: {e}", path.display()));
                    }
                }
            }
            Err(e) => {
                warn_explore(format!(
                    "variant preprocess failed for {}: {e}",
                    path.display()
                ));
            }
        }
    }

    (base_unit, variant_units)
}

#[allow(clippy::too_many_arguments)]
fn process_indexed_file(
    program: &mut Program,
    path: &Path,
    graph: &IncludeGraph,
    index_opts: &PreprocessOptions,
    source_cache: &IndexSourceCache,
    header_ir: Option<&HeaderIr>,
    pch_order: &[PathBuf],
) -> Result<(), String> {
    let pre = source_cache.get_or_preprocess(path, graph, index_opts)?;
    // `index_opts` was chosen by `index_language`, so its language is the
    // one the text was lexed as; the grammar must not be re-derived from
    // the path or a forced language would preprocess as one language and
    // parse as the other.
    let language = index_opts
        .language
        .unwrap_or_else(|| Language::from_path(path));
    lower_prepared_source(
        program,
        path,
        graph,
        pre,
        language,
        index_opts.record_link_ownership,
        header_ir,
        pch_order,
    )
}

/// Lower already-preprocessed text into a fresh `program`.
///
/// No files may have been interned: dependency roots must be installed before
/// file classification or preamble merging. Callers create one Program per unit.
#[allow(clippy::too_many_arguments)]
fn lower_prepared_source(
    program: &mut Program,
    path: &Path,
    graph: &IncludeGraph,
    pre: Arc<PreprocessedSource>,
    language: Language,
    record_link_ownership: bool,
    header_ir: Option<&HeaderIr>,
    pch_order: &[PathBuf],
) -> Result<(), String> {
    program.symbols.set_dep_roots(graph.dep_roots.clone());
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
            if let Some(units) = ir.get(h) {
                // The expansion this unit replayed, when it has one. A
                // header it reached only through another header's cached
                // expansion leaves no record here — `PreprocessedSource`
                // carries the includer's `nested_variants` for exactly the
                // headers that entry pulled in, and anything still missing
                // is merged from every stored expansion, which is additive
                // for the declarations these two modes copy.
                for (unit_lang, variant, unit) in units {
                    // A header reached from both C and C++ is lowered once,
                    // in the language `index_language` chose. A unit of the
                    // other language recorded an index into that language's
                    // own variant list, where it means something else, so it
                    // is not a record for this unit at all.
                    let wanted = (*unit_lang == language)
                        .then(|| pre.replayed_variants.get(h))
                        .flatten();
                    if wanted.is_some_and(|w| !w.contains(variant)) {
                        continue;
                    }
                    if types_only {
                        merge_unit_types(program, unit);
                    } else {
                        merge_unit_symbols(program, unit);
                    }
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
    let lang = source_lang(language);
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

    let is_cpp = lang == crate::parse::SourceLang::Cpp;
    let mut ctx = LowerContext {
        current_fn: None,
        current_file: file_id,
        locals: HashMap::default(),
        line_map: Some(std::sync::Arc::clone(&pre.line_map)),
        primary_path: self_canon,
        origin_file_ids: vec![Cell::new(None); pre.line_map.files.len()],
        pending: RefCell::new(Vec::new()),
        ns_stack: Vec::new(),
        using_nss: Vec::new(),
        using_name_imports: Vec::new(),
        class_ctx: None,
        type_scope: RefCell::new(Vec::new()),
        local_aliases: Vec::new(),
        is_cpp,
        handled_new_exprs: RefCell::new(HashSet::default()),
        callee_load_cache: RefCell::new(HashMap::default()),
        call_receiver_cache: RefCell::new(HashMap::default()),
        call_return_dst: RefCell::new(HashMap::default()),
        ast_depth: 0,
        ast_depth_warned: false,
        reference_vars: HashSet::default(),
        local_scope_log: Vec::new(),
        tree: parsed.tree.clone(),
        has_templates: is_cpp && parsed.source.contains("template"),
        has_weak: source_may_annotate_weak(&parsed.source) || program.symbols.has_weak_symbols(),
        record_link_ownership,
        pending_flow_owners: HashMap::default(),
        pending_initializer_owners: HashMap::default(),
    };
    lower_tree(
        program,
        &mut ctx,
        parsed.source.as_ref(),
        parsed.tree.root_node(),
    );
    // Pragmas apply to the entire unit, even when placed after a definition.
    // Only a unit that spells the directive needs the line scan; after include
    // expansion the text is measured in megabytes.
    let weak_names: HashSet<&str> = if ctx.has_weak && parsed.source.contains("pragma") {
        parsed
            .source
            .lines()
            .filter_map(|line| {
                let mut words = line.trim_start().strip_prefix('#')?.split_whitespace();
                (words.next() == Some("pragma") && words.next() == Some("weak"))
                    .then(|| words.next().map(|word| word.split('=').next().unwrap()))
                    .flatten()
            })
            .collect()
    } else {
        HashSet::default()
    };
    if !weak_names.is_empty() {
        program.symbols.mark_has_weak_symbols();
        // Weak binding is a property of an external symbol. A `static`
        // function, a local, a parameter or a lowering temporary that happens
        // to share the pragma's name has no linkage to weaken, and marking it
        // would export a meaningless `is_weak` row.
        for function in &mut program.symbols.functions {
            function.is_weak |= function.linkage == trace_ir::Linkage::External
                && weak_names.contains(function.name.as_str());
        }
        // A namespaced global is excluded for the same reason the selection
        // below excludes it: `app::cb` is mangled, so the pragma's
        // unqualified `cb` is not the symbol it names.
        for variable in &mut program.symbols.variables {
            variable.is_weak |= variable.storage == StorageClass::Global
                && !variable.is_namespaced
                && weak_names.contains(variable.name.as_str());
        }
    }
    // By now the symbol table knows whether anything was actually marked, which
    // is narrower than `has_weak` (a guess made from the source text).
    if program.symbols.has_weak_symbols() {
        // Weak binding is a property of the external symbol, not one spelling
        // of its declaration. A later annotation also applies to earlier rows.
        // A namespaced global is excluded on both sides: its unqualified name
        // is not its symbol name, so `a::cb` must not weaken `b::cb`.
        let shares_linkage_name =
            |v: &Variable| v.storage == StorageClass::Global && !v.is_namespaced;
        let weak_globals: HashSet<String> = program
            .symbols
            .variables
            .iter()
            .filter(|v| shares_linkage_name(v) && v.is_weak)
            .map(|v| v.name.clone())
            .collect();
        if !weak_globals.is_empty() {
            for variable in &mut program.symbols.variables {
                if shares_linkage_name(variable) && weak_globals.contains(&variable.name) {
                    variable.is_weak = true;
                }
            }
        }
    }
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
        if program.dedup.insert_diagnostic(
            diagnostic.file,
            diagnostic.line,
            &diagnostic.message,
            &diagnostic.stage,
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
    for (index, item) in pending.into_iter().enumerate() {
        let flow_start = program.flow.len();
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
        if let Some(&owner) = ctx.pending_initializer_owners.get(&index) {
            let end = program.flow.len();
            if end > flow_start {
                program
                    .global_initializer_ranges
                    .entry(owner)
                    .or_default()
                    .push(flow_start..end);
            }
        }
        if let Some(&owner) = ctx.pending_flow_owners.get(&index) {
            let end = program.flow.len();
            if end > flow_start {
                program
                    .function_flow_ranges
                    .entry(owner)
                    .or_default()
                    .push(flow_start..end);
            }
        }
    }
}

fn program_into_unit(path: PathBuf, mut program: Program) -> UnitIndex {
    let inheritance = program.take_inheritance();
    UnitIndex {
        compilation_index: None,
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
        function_flow_ranges: program.function_flow_ranges,
        global_initializer_ranges: program.global_initializer_ranges,
        fn_returns: program.fn_returns.into_iter().collect(),
        diagnostics: program.diagnostics,
        anon_type_counter: program.anon_type_counter,
        inheritance,
        template_bases: std::mem::take(&mut program.template_bases),
        arrow_returns: std::mem::take(&mut program.arrow_returns),
        template_returns: std::mem::take(&mut program.template_returns),
        final_classes: std::mem::take(&mut program.final_classes),
        anonymous_classes: std::mem::take(&mut program.anonymous_classes),
        anonymous_final_classes: std::mem::take(&mut program.anonymous_final_classes),
        anonymous_bases: std::mem::take(&mut program.anonymous_bases),
        namespaces: std::mem::take(&mut program.namespaces),
        merge_descs: Vec::new(),
    }
}

/// What a callee expression resolves to: its name, whether the name is a
/// direct target, and the variable holding the value called through it.
type CalleeRef = (String, bool, Option<VarId>);

/// A typedef or `using` alias is reachable by its bare name and, inside a
/// namespace, by its qualified one (`Outer::ScopedAlias`), so a qualified
/// spelling of it is matched whole rather than by its last segment. A bare use
/// looks the qualified names up first (see [`find_in_scope`]); the bare entry
/// is the global fallback.
///
/// An alias declared in a class body is a member of the class and is
/// reachable only as `Cls::Alias`: classes routinely declare the same alias
/// names (`Ptr`, `iterator`), and a bare entry would let one class's alias
/// answer for a use outside every one of them.
fn register_typedef_alias(
    program: &mut Program,
    ctx: &LowerContext,
    node: Node,
    alias: &str,
    desc: TypeDesc,
) {
    if ctx.is_cpp
        && node
            .parent()
            .is_some_and(|p| p.kind() == "field_declaration_list")
    {
        if let Some(cls) = ctx.type_scope.borrow().last() {
            program
                .types
                .register_alias(&format!("{cls}::{alias}"), desc);
        }
        return;
    }
    if ctx.is_cpp && ctx.ns_stack.iter().any(Option::is_some) {
        program
            .types
            .register_alias(&ctx.qualify(alias), desc.clone());
    }
    program.types.register_alias(alias, desc);
}

/// A `typedef` or `using Alias = T;` at namespace or class scope.
fn lower_alias(program: &mut Program, ctx: &LowerContext, source: &str, node: Node) {
    if let Some((alias, desc)) = declared_alias(program, ctx, source, node) {
        register_typedef_alias(program, ctx, node, &alias, desc);
    }
}

/// The name a `typedef` or a `using Alias = T;` declares and the type it
/// stands for. `using` is a typedef written the other way round (#91): the
/// same alias table and the same scopes.
fn declared_alias(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<(String, TypeDesc)> {
    match node.kind() {
        "type_definition" => typedef_alias(program, ctx, source, node),
        "alias_declaration" => using_alias(program, ctx, source, node),
        _ => None,
    }
}

/// The name a `typedef` declares and the type it stands for.
fn typedef_alias(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<(String, TypeDesc)> {
    let decl = node.child_by_field_name("declarator")?;
    let (alias, _) = parse_declarator_name(source, decl);
    let type_node = node.child_by_field_name("type")?;
    if alias.is_empty() {
        return None;
    }
    if type_node.kind() == "struct_specifier" || type_node.kind() == "union_specifier" {
        let tag = lower_struct_specifier(program, ctx, source, type_node);
        if tag.is_empty() {
            return None;
        }
        let kind = if type_node.kind() == "union_specifier" {
            TypeDesc::Union {
                name: tag.clone(),
                fields: Vec::new(),
            }
        } else {
            TypeDesc::Struct {
                name: tag,
                fields: Vec::new(),
            }
        };
        program.types.intern(kind.clone());
        // The declarator's own modifiers belong to the alias:
        // `typedef struct Session *SessionPtr` names a POINTER, and
        // registering the bare tag made every `SessionPtr s` a struct value,
        // so `s->fd` decomposed against a non-pointer and the points-to graph
        // lost the edge.
        let shaped = walk_declarator_shape(decl, kind);
        program.types.intern(shaped.clone());
        // Registered even when alias == tag: later `Tag *x` declarations
        // resolve through the alias table (`type_desc_from_node`), and
        // without an entry the pointer degrades to Int, killing field
        // decomposition.
        return Some((alias, shaped));
    }
    let desc = typedef_underlying_desc(program, ctx, source, node)?;
    Some((alias, desc))
}

/// The name a `using Alias = T;` declares and the type it stands for. An
/// alias template (`template<class T> using Vec = ...`) is not lowered: its
/// uses carry arguments a plain alias entry would drop.
fn using_alias(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<(String, TypeDesc)> {
    if node
        .parent()
        .is_some_and(|p| p.kind() == "template_declaration")
    {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    let ty = node.child_by_field_name("type")?;
    let alias = normalize_qualified(node_text(source, &name));
    if alias.is_empty() {
        return None;
    }
    let base = type_desc_from_node(program, ctx, source, ty);
    let desc = abstract_declarator_shape(base, ty.child_by_field_name("declarator"), true);
    Some((alias, desc))
}

/// The type a typedef or `using` alias declared in an enclosing block of the
/// function being lowered stands for, the innermost declaration first.
fn local_alias<'a>(ctx: &'a LowerContext, name: &str) -> Option<&'a TypeDesc> {
    ctx.local_aliases
        .iter()
        .rev()
        .find(|(alias, _)| alias == name)
        .map(|(_, desc)| desc)
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
        "type_definition" | "alias_declaration" => lower_alias(program, ctx, source, node),
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
    // `namespace A::B {` (C++17) opens two scopes at once; tree-sitter
    // spells it as one `nested_namespace_specifier`. It used to read as an
    // anonymous namespace, which registered everything inside under the
    // bare name and with internal linkage.
    let levels: Vec<Option<String>> = match node.children(&mut node.walk()).find(|c| {
        matches!(
            c.kind(),
            "namespace_identifier" | "nested_namespace_specifier"
        )
    }) {
        Some(c) if c.kind() == "nested_namespace_specifier" => {
            // `namespace A::inline B {` (C++20) keeps the keyword inside
            // the segment; the scope is named `B`.
            normalize_qualified(node_text(source, &c))
                .split("::")
                .map(|seg| seg.trim())
                // Only a keyword, never a namespace whose name starts
                // with one: `inline B` is `B`, `inlineB` is itself.
                .map(|seg| {
                    seg.strip_prefix("inline")
                        .filter(|rest| rest.starts_with(char::is_whitespace))
                        .map_or(seg, str::trim_start)
                })
                .filter(|seg| !seg.is_empty())
                .map(|seg| Some(seg.to_string()))
                .collect()
        }
        Some(c) => vec![Some(normalize_qualified(node_text(source, &c)))],
        None => vec![None],
    };
    for level in &levels {
        ctx.ns_stack.push(level.clone());
        program.namespaces.insert(ctx.namespace_scope());
    }
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
    for _ in &levels {
        ctx.ns_stack.pop();
    }
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
            // A nested class's body: its members are its own (#92). Walking
            // in made `class It { int x; int Next(); };` a function member of
            // the outer class, named after the first field it held.
            "field_declaration_list" => return false,
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
    let member_tag = member_class_tag(ctx, source, node);
    let reg_name = match &member_tag {
        Some(tag) if tag.nests => tag.spelling.clone(),
        _ if !is_cpp_class => name.clone(),
        // `class Outer::Inner { ... }` defines the class `Outer` declared.
        _ if name.contains("::") => {
            declared_tag_in_scope(program, ctx, &name).unwrap_or_else(|| name.clone())
        }
        // `struct Node *next;` refers to a class declared elsewhere: the tag
        // the lookup finds, else a new one in the innermost namespace.
        _ if is_class_reference(node) => {
            declared_tag_in_scope(program, ctx, &name).unwrap_or_else(|| ctx.qualify(&name))
        }
        _ => ctx.qualify(&name),
    };
    if let Some(tag) = member_tag.as_ref().filter(|tag| !tag.nests) {
        // The C++ spelling of a struct that keeps its C tag still names it.
        program.types.register_alias(
            &tag.spelling,
            TypeDesc::Struct {
                name: reg_name.clone(),
                fields: Vec::new(),
            },
        );
    }

    // C++: `class D : B, A { ... }` — record inheritance for virtual
    // dispatch expansion. Unqualified bases resolve against the current
    // namespace scope (usings ignored here; documented imprecision).
    // `struct D : B` is the same relationship (only default access differs).
    if is_cpp_class {
        let has_body = node.child_by_field_name("body").is_some();
        if has_body {
            program.types.define_struct(&reg_name);
        } else {
            program.types.declare_struct(&reg_name);
        }
        let derived = reg_name.clone();
        let anonymous_in =
            (has_body && ctx.in_anonymous_namespace()).then(|| node_span(program, ctx, node).file);
        if let Some(file) = anonymous_in {
            program.mark_class_anonymous(&derived, file);
        }
        if class_specifier_is_final(source, node) {
            match anonymous_in {
                Some(file) => program.mark_anonymous_class_final(&derived, file),
                None => program.mark_class_final(&derived),
            }
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
                    let raw = normalize_qualified(base_text);
                    let short = strip_template_args(&raw);
                    // A base clause names a class, and names it through a
                    // `using namespace` directive as readily as through the
                    // enclosing scopes — which are asked first, as always.
                    let through_directive = declared_class_in_scope(program, ctx, &short)
                        .is_none()
                        .then(|| class_seen_from(program, ctx, &short))
                        .flatten();
                    // Both inheritance and template substitution name the same
                    // resolved class, including partially qualified spellings.
                    let resolved_base = through_directive
                        .unwrap_or_else(|| qualify_class_name(program, ctx, &short));
                    let template_spelling = normalize_template_spelling(base_text);
                    if let Some(at) = template_spelling.find('<') {
                        let qualified = if template_tail(&template_spelling).is_empty() {
                            format!("{resolved_base}{}", &template_spelling[at..])
                        } else {
                            // Outer<A>::Inner<B> has arguments on separate
                            // classes; appending from the first '<' would
                            // duplicate Inner and attach A to the wrong class.
                            template_spelling
                        };
                        let is_dependent =
                            spelling_is_dependent(ctx, source, base, base_text, Spelling::Source);
                        let declaration_scope = ctx
                            .type_scope
                            .borrow()
                            .last()
                            .cloned()
                            .unwrap_or_else(|| ctx.namespace_scope());
                        program.add_template_base(
                            &derived,
                            &qualified,
                            &declaration_scope,
                            is_dependent,
                        );
                    }
                    if let Some(file) = anonymous_in {
                        program.add_anonymous_base(&derived, file, &resolved_base);
                    }
                    program.add_inheritance(&derived, &resolved_base);
                }
            }
        }
    }

    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        // Field types, member classes and member aliases are looked up in
        // the class's own scope first.
        if is_cpp_class {
            // A member class is in its outer class's scope even when it keeps
            // its C tag, so what it nests is spelled `Outer::Inner::Deep`.
            let scope = member_tag.map_or_else(|| reg_name.clone(), |tag| tag.spelling);
            ctx.type_scope.borrow_mut().push(scope);
        }
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "field_declaration" if !member_decl_is_function(child) => {
                    // `class Inner { ... };` and `class Inner;` declare a
                    // type and no field.
                    if is_cpp_class && child.child_by_field_name("declarator").is_none() {
                        if let Some(spec) = child
                            .child_by_field_name("type")
                            .filter(|t| is_named_class_specifier(*t))
                        {
                            lower_struct_specifier(program, ctx, source, spec);
                            continue;
                        }
                    }
                    if let Some((fname, field_type)) =
                        type_desc_from_field_declaration(program, ctx, source, child)
                    {
                        if !fname.is_empty() {
                            fields.push((fname, field_type));
                        }
                    }
                }
                "template_declaration" if is_cpp_class => {
                    if let Some(spec) = member_class_definition(child) {
                        lower_struct_specifier(program, ctx, source, spec);
                    }
                }
                "type_definition" | "alias_declaration" if is_cpp_class => {
                    lower_alias(program, ctx, source, child);
                }
                _ => {}
            }
        }
        if is_cpp_class {
            ctx.type_scope.borrow_mut().pop();
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
/// then in-class definitions with the class context active. Member classes
/// are part of both passes, so every body sees the prototypes of the whole
/// class tree, declared before it or after, as C++'s complete-class context
/// does.
fn lower_class_members(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    cls_qual: &str,
) {
    register_class_prototypes(program, ctx, source, node, cls_qual);
    lower_class_definitions(program, ctx, source, node, cls_qual);
}

/// The members of a class body the two member passes look at.
fn class_members(node: Node) -> Vec<Node> {
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    body.children(&mut body.walk())
        .filter(|m| {
            matches!(
                m.kind(),
                "function_definition"
                    | "field_declaration"
                    | "declaration"
                    | "template_declaration"
            )
        })
        .collect()
}

/// The first pass of [`lower_class_members`]: member prototypes.
fn register_class_prototypes(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    cls_qual: &str,
) {
    ctx.type_scope.borrow_mut().push(cls_qual.to_string());
    for m in class_members(node) {
        // A member class registers its own members under its own spelling.
        if let Some(spec) = member_class_definition(m) {
            if let Some(tag) = member_class_tag(ctx, source, spec) {
                register_class_prototypes(program, ctx, source, spec, &tag.spelling);
            }
            continue;
        }
        // A class-scope `template <typename T> T GetNumber() {...}` declares
        // the member's primary; the template parameter list is not an
        // argument — unwrap it and register the nested member like any other.
        if m.kind() == "template_declaration" {
            if let Some(inner) = template_member_decl(m) {
                if member_decl_is_function(inner) {
                    register_member_prototype(program, ctx, source, inner, cls_qual);
                }
            }
            continue;
        }
        if m.kind() == "field_declaration" && member_decl_is_function(m) {
            register_member_prototype(program, ctx, source, m, cls_qual);
        }
        // A ctor written `Cls(int);` inside the class parses as a plain
        // declaration wrapping a function_declarator.
        if m.kind() == "declaration" && member_decl_is_function(m) && !continues_previous_member(m)
        {
            register_member_prototype(program, ctx, source, m, cls_qual);
        }
    }
    ctx.type_scope.borrow_mut().pop();
}

/// The second pass of [`lower_class_members`]: in-class definitions. Member
/// classes go first, so an outer body finds a nested class's inline members;
/// a nested body reaching an outer member defined further down resolves by
/// name (see the constructor rule in `collect_call_at_node`).
fn lower_class_definitions(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    cls_qual: &str,
) {
    ctx.type_scope.borrow_mut().push(cls_qual.to_string());
    let members = class_members(node);
    for &m in &members {
        if let Some(spec) = member_class_definition(m) {
            if let Some(tag) = member_class_tag(ctx, source, spec) {
                lower_class_definitions(program, ctx, source, spec, &tag.spelling);
            }
        }
    }
    // Every definition's entry exists before any body is lowered, so a body
    // finds a member the class defines further down, overloads included (#96).
    let saved = ctx.class_ctx.replace(ClassCtx {
        qual_name: cls_qual.to_string(),
    });
    let signatures: Vec<_> = members
        .iter()
        .map(|&m| {
            if m.kind() == "template_declaration" {
                template_member_decl(m).unwrap_or(m)
            } else {
                m
            }
        })
        .filter(|&member| {
            member.kind() == "function_definition"
                || (member.kind() == "field_declaration"
                    && member_decl_is_function(member)
                    && node_has_compound_body(member))
        })
        .filter_map(|member| {
            lower_function_signature(program, ctx, source, member).map(|sig| (member, sig))
        })
        .collect();
    for (member, signature) in signatures {
        lower_function_body(program, ctx, source, member, signature);
    }
    ctx.class_ctx = saved;
    ctx.type_scope.borrow_mut().pop();
}

/// A `class` / `struct` specifier with a name.
fn is_named_class_specifier(node: Node) -> bool {
    matches!(node.kind(), "class_specifier" | "struct_specifier")
        && node.child_by_field_name("name").is_some()
}

/// A `class` / `struct` specifier with a name and a body.
fn is_named_class_definition(node: Node) -> bool {
    is_named_class_specifier(node) && node.child_by_field_name("body").is_some()
}

/// A body-less specifier inside a declaration of something else
/// (`struct Node *next;`, `void f(class Impl *p)`): it refers to a class
/// rather than declaring one, as a standalone `class Impl;` does.
fn is_class_reference(spec: Node) -> bool {
    spec.child_by_field_name("body").is_none()
        && spec
            .parent()
            .is_some_and(|p| p.child_by_field_name("declarator").is_some())
}

/// The class a member declaration defines: `class Inner { ... };`, with or
/// without a declarator, or `template<...> class Inner { ... };`.
fn member_class_definition(member: Node) -> Option<Node> {
    match member.kind() {
        "field_declaration" => member
            .child_by_field_name("type")
            .filter(|t| is_named_class_definition(*t)),
        "template_declaration" => member
            .named_children(&mut member.walk())
            .find(|c| is_named_class_definition(*c)),
        _ => None,
    }
}

/// How a class defined as a member of another is named (#92).
struct MemberClassTag {
    /// The C++ spelling, `Outer::Inner`.
    spelling: String,
    /// Whether the class registers under that spelling. A struct C would also
    /// accept, nested only in such structs, keeps the namespace tag instead:
    /// C gives a nested struct file scope, and a header shared by C and C++
    /// units has to name it alike in both, or the two units' layouts and
    /// field summaries split.
    nests: bool,
}

/// The [`MemberClassTag`] of a class declared in the body of the class being
/// lowered, with its body or ahead of it (`struct Impl;`). `None` for a
/// specifier that is no member, only refers to a class, is spelled
/// qualified, or is lowered without its outer class in scope.
fn member_class_tag(ctx: &LowerContext, source: &str, spec: Node) -> Option<MemberClassTag> {
    // The parent check first: most specifiers are no member, and it walks no
    // fields.
    if !ctx.is_cpp
        || enclosing_member_class(spec).is_none()
        || !is_named_class_specifier(spec)
        || is_class_reference(spec)
    {
        return None;
    }
    let nests =
        std::iter::successors(Some(spec), |c| enclosing_member_class(*c)).any(is_cpp_only_class);
    let raw = node_text(source, &spec.child_by_field_name("name")?);
    let name = strip_template_args(&normalize_qualified(raw));
    if name.contains("::") {
        return None;
    }
    let scope = ctx.type_scope.borrow();
    Some(MemberClassTag {
        spelling: format!("{}::{name}", scope.last()?),
        nests,
    })
}

/// The class specifier whose body declares `spec` as a member.
fn enclosing_member_class(spec: Node) -> Option<Node> {
    let member = spec.parent()?;
    if !matches!(member.kind(), "field_declaration" | "template_declaration") {
        return None;
    }
    let body = member
        .parent()
        .filter(|b| b.kind() == "field_declaration_list")?;
    body.parent()
        .filter(|o| matches!(o.kind(), "class_specifier" | "struct_specifier"))
}

/// Whether a class specifier uses anything a C struct cannot: the `class`
/// keyword, a template head, bases, or members other than data fields.
fn is_cpp_only_class(spec: Node) -> bool {
    if spec.kind() == "class_specifier"
        || spec
            .parent()
            .is_some_and(|p| p.kind() == "template_declaration")
    {
        return true;
    }
    if spec
        .children(&mut spec.walk())
        .any(|c| c.kind() == "base_class_clause")
    {
        return true;
    }
    let Some(body) = spec.child_by_field_name("body") else {
        return false;
    };
    body.named_children(&mut body.walk())
        .any(|m| match m.kind() {
            "field_declaration" => member_decl_is_function(m),
            kind => matches!(
                kind,
                "function_definition"
                    | "declaration"
                    | "access_specifier"
                    | "alias_declaration"
                    | "type_definition"
                    | "template_declaration"
                    | "using_declaration"
                    | "friend_declaration"
                    | "static_assert_declaration"
            ),
        })
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
    if short == "operator->" {
        register_arrow_return(program, ctx, source, node, cls_qual);
    }
    let flags = virtual_flags(source, node);
    let provisional_id = program.symbols.alloc_fn_id();
    // Prototypes carry no parameter variables; they merge into their
    // definitions, which supply the real param list. The count they declare
    // keeps overloads apart until then (`Get()` beside `Get(Mode&)`).
    let params: Vec<VarId> = Vec::new();
    let (explicit_arity, shape) = find_params(node).map_or((0, ParamListShape::default()), |p| {
        declared_param_counts(source, p)
    });
    let span = node_span(program, ctx, node);
    let ret_type = node
        .child_by_field_name("type")
        .map(|t| parse_type_node(program, ctx, source, t))
        .unwrap_or_else(|| program.types.void());
    let ret_type = declared_return_type(program, ctx, source, node, ret_type, &full_name);
    program.symbols.add_function(Function {
        is_weak: false,
        target: None,
        id: provisional_id,
        name: full_name,
        linkage: if ctx.in_anonymous_namespace() {
            Linkage::Internal
        } else {
            Linkage::External
        },
        return_type: ret_type,
        params,
        locals: Vec::new(),
        span,
        end_line: span.line,
        file: ctx.current_file,
        is_defined: false,
        param_type_ids: Vec::new(),
        explicit_arity: Some(explicit_arity),
        default_args: shape.defaults,
        owner_unresolved: false,
        variadic: shape.variadic,
        defaulted_in_class: false,
        declared_in_class: true,
        is_virtual: flags.is_virtual,
        is_final: flags.is_final,
        is_cpp: ctx.is_cpp,
        tu: Some(ctx.current_file),
    });
}

/// Whether a unit can carry a weak annotation at all, gating the per-declaration
/// tree walks below.
///
/// `declaration_is_weak` only ever matches inside an attribute, and the pragma
/// scan only inside a directive, so the bare word is not enough: `weak` occurs
/// in `std::weak_ptr`, `weak_ordering` and any comment, which would arm the
/// whole weak path for essentially every C++ unit that includes `<memory>`.
/// Each `contains` is a single memchr-accelerated pass over text the tree walks
/// would otherwise be charged for.
fn source_may_annotate_weak(source: &str) -> bool {
    source.contains("weak")
        && (source.contains("__attribute__") || source.contains("[[") || source.contains("pragma"))
}

/// Weak linkage is a property of an external symbol. A local, a parameter and
/// a file-`static` have no linkage to weaken — GCC ignores the attribute there
/// — so recording it would only put a meaningless `is_weak` row in the export
/// and arm target selection against a symbol that never participates in a link.
fn weak_global(ctx: &LowerContext, storage: StorageClass, source: &str, node: Node) -> bool {
    storage == StorageClass::Global && ctx.has_weak && declaration_is_weak(source, node)
}

/// Whether a global's unqualified name is *not* the name a linker resolves.
///
/// Any enclosing namespace makes it so, anonymous ones included: those have
/// internal linkage and are not shared symbols at all. Target-scoped
/// unification and weak override are both keyed on the unqualified name, so
/// they must leave such a global alone.
fn namespaced_global(ctx: &LowerContext, storage: StorageClass) -> bool {
    storage == StorageClass::Global && !ctx.ns_stack.is_empty()
}

/// The declarator this reference sits in, when the declaration introduces more
/// than one (`int a, b;`). `None` for the single-declarator case, where no
/// sibling exists to confuse and nothing needs excluding.
///
/// One cursor pass and no allocation: this runs for every declaration of a unit
/// that might carry a weak annotation, and building a list just to learn "there
/// is exactly one" cost a malloc apiece.
fn own_declarator<'t>(decl: Node<'t>, node: Node<'t>) -> Option<Node<'t>> {
    let mut cursor = decl.walk();
    let (mut count, mut own) = (0usize, None);
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some("declarator") {
                count += 1;
                let declarator = cursor.node();
                if declarator.start_byte() <= node.start_byte()
                    && node.end_byte() <= declarator.end_byte()
                {
                    own = Some(declarator);
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    (count > 1).then_some(own).flatten()
}

/// Inspect declaration syntax only: never walk a function body or initializer.
///
/// An attribute among the declaration specifiers applies to every declarator
/// (`__attribute__((weak)) void a(void), b(void);` weakens both), but one
/// written inside a declarator applies only to it: in
/// `void a(void) __attribute__((weak)), b(void);` the compiler weakens `a`
/// alone. Scanning the whole declaration would weaken `b` too, and a spurious
/// weak mark on `b` lets a real strong definition of `b` be suppressed.
fn declaration_is_weak(source: &str, node: Node) -> bool {
    let Some(decl) = enclosing_decl(
        node,
        &["declaration", "field_declaration", "function_definition"],
    ) else {
        return false;
    };
    let own = own_declarator(decl, node);
    fn visit(source: &str, node: Node) -> bool {
        if matches!(
            node.kind(),
            "compound_statement"
                | "parameter_list"
                | "initializer_list"
                | "struct_specifier"
                | "class_specifier"
        ) {
            return false;
        }
        if matches!(node.kind(), "attribute_specifier" | "attribute_declaration") {
            let mut cursor = node.walk();
            return node.named_children(&mut cursor).any(|args| {
                let mut cursor = args.walk();
                // Bound, not returned directly: the iterator borrows `cursor`,
                // so the temporary has to drop before it does.
                let found = args.named_children(&mut cursor).any(|arg| {
                    arg.kind() == "identifier"
                        && matches!(node_text(source, &arg), "weak" | "__weak__")
                });
                found
            });
        }
        let value = node.child_by_field_name("value");
        let mut cursor = node.walk();
        let found = node
            .named_children(&mut cursor)
            .any(|child| Some(child) != value && visit(source, child));
        found
    }
    // One scan for both shapes: with a single declarator nothing is excluded,
    // so the multi-declarator case is the same walk with siblings filtered out.
    let value = decl.child_by_field_name("value");
    let mut cursor = decl.walk();
    let mut found = false;
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let sibling_declarator = cursor.field_name() == Some("declarator")
                && own.is_some_and(|own| own.id() != child.id());
            if child.is_named()
                && !sibling_declarator
                && Some(child) != value
                && visit(source, child)
            {
                found = true;
                break;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    // A trailing attribute after a MISSING token lands in the following ERROR
    // node. Only meaningful for a lone declarator; with several there is no
    // telling which one the stray attribute belonged to.
    found
        || (own.is_none()
            && decl
                .child(decl.child_count().saturating_sub(1))
                .is_some_and(|last| last.is_missing())
            && decl
                .next_named_sibling()
                .is_some_and(|next| next.is_error() && visit(source, next)))
}

fn lower_function(program: &mut Program, ctx: &mut LowerContext, source: &str, node: Node) {
    if let Some(signature) = lower_function_signature(program, ctx, source, node) {
        lower_function_body(program, ctx, source, node, signature);
    }
}

/// A function definition's registered entry, whose body is still to be
/// lowered.
struct FunctionSignature {
    fn_id: FnId,
    params: Vec<VarId>,
    eff_class: Option<String>,
    /// The class or namespace the definition's names are looked up in.
    lookup_scope: Option<String>,
}

/// Register a function definition's entry and parameters, without its body.
/// `None` when there is no body to lower: no name, or a dependency body.
fn lower_function_signature(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> Option<FunctionSignature> {
    let decl = error_parked_declarator(node)
        .or_else(|| node.child_by_field_name("declarator"))
        .or_else(|| find_function_declarator(node))?;
    // An in-class conversion operator behind a leading macro keeps a
    // declarator, but that declarator names the type it converts *to*; only
    // the member walk knows to read the keyword stranded beside it. Without
    // this the definition landed on `C::S` — defined, colliding with the
    // class `S` itself — while its declaration stayed undefined.
    let raw_name = match conversion_keyword_error(node) {
        Some(_) => member_short_name(source, node)?,
        None => parse_declarator_name(source, decl).0,
    };
    if raw_name.is_empty() {
        return None;
    }
    let mut name = ctx.qualify_decl(&raw_name);
    // Out-of-class member definitions (`void Cls::f() {}`, ctors, dtors):
    // recover the owning class from the longest `::`-prefix that names a
    // known class type — or, when none does, the class a `using namespace`
    // directive brings the prefix to. hdf's hc-gen writes
    // `AstObject::IsNode()` under `using namespace OHOS::Hardware`; that
    // defines `OHOS::Hardware::AstObject::IsNode`, where the header declared
    // it, and the definition merges into the prototype's entry.
    let eff_class: Option<String> = match &ctx.class_ctx {
        Some(c) => Some(c.qual_name.clone()),
        None => derive_owner_class(program, &name).or_else(|| {
            let (prefix, member) = name.rsplit_once("::")?;
            let cls = class_seen_from(program, ctx, prefix)?;
            name = format!("{cls}::{member}");
            Some(cls)
        }),
    };
    if let Some(prefix) = name.strip_suffix("::operator->") {
        // Without the class header in the tree there is no tag to derive
        // the owner from; the operator's own name says which wrapper it
        // is, and its return type is still what the arrow yields.
        let cls = eff_class.clone().unwrap_or_else(|| prefix.to_owned());
        register_arrow_return(program, ctx, source, node, &cls);
    }
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
    let ret_type = declared_return_type(program, ctx, source, node, ret_type, &name);
    let provisional_id = program.symbols.alloc_fn_id();
    let provisional_start = (
        program.symbols.variables.len(),
        program.symbols.call_sites.len(),
    );
    let mut params = Vec::new();
    // Names in the parameters and body of a definition spelled `N::f` are
    // looked up in `N`, whether `N` is a class or a namespace (`N::f(A)`
    // means `N::A`). Lowering such a definition as `f(int)` collapsed
    // distinct overloads and stranded their declarations.
    let owner_unresolved = eff_class.is_none() && scope_part(&raw_name).contains("::");
    let lookup_scope = if owner_unresolved {
        scope_part(&name)
            .rsplit_once("::")
            .map(|(scope, _)| scope.to_owned())
    } else {
        eff_class.clone()
    };
    let scoped = enter_lookup_scope(ctx, lookup_scope.as_ref());
    // Implicit `this` for member functions, ctors and dtors.
    if let Some(cls) = &eff_class {
        let span = node_span(program, ctx, node);
        params.push(add_this_param(program, cls, provisional_id, span));
    }
    let shape = lower_parameters(program, ctx, source, decl, provisional_id, &mut params);

    // `static` on a member makes it a static member, not a function with
    // internal linkage. Registered internal, a static member template's body
    // never merged with its external prototype on the same line, and the unit
    // merge dropped it as that prototype's duplicate.
    // A member of a class in an anonymous namespace has internal linkage
    // wherever its definition is written: after the namespace closes, or in
    // the `.cpp` including the class's header.
    let definition_file = node_span(program, ctx, node).file;
    let is_static = (eff_class.is_none() && declaration_is_static(source, node))
        || ctx.in_anonymous_namespace()
        || eff_class.as_ref().is_some_and(|cls| {
            program.class_is_anonymous_in(cls, |f| program.symbols.file_sees(definition_file, f))
        });
    let flags = virtual_flags(source, node);

    let span = node_span(program, ctx, node);
    let is_dep = program.is_dep_file(span.file);
    let end_line = if is_dep {
        span.line
    } else {
        node_end_line(program, ctx, node, span)
    };
    let fn_id = program.symbols.add_function(Function {
        // Only an external symbol has linkage to weaken; GCC ignores the
        // attribute on a `static` and lowering must not record it either.
        is_weak: !is_static && ctx.has_weak && declaration_is_weak(source, node),
        target: None,
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
        is_defined: !is_dep,
        param_type_ids: Vec::new(),
        explicit_arity: Some((params.len() - usize::from(eff_class.is_some())) as u32),
        default_args: shape.defaults,
        owner_unresolved,
        variadic: shape.variadic,
        // `A() = default;` in its class: not user-provided (C++17 aggregates).
        defaulted_in_class: ctx.class_ctx.is_some()
            && node
                .children(&mut node.walk())
                .any(|c| matches!(c.kind(), "default_method_clause" | "delete_method_clause")),
        declared_in_class: ctx.class_ctx.is_some(),
        is_virtual: flags.is_virtual,
        is_final: flags.is_final,
        is_cpp: ctx.is_cpp,
        tu: Some(ctx.current_file),
    });
    reassign_fn_id(program, provisional_id, fn_id, provisional_start);
    if scoped {
        ctx.type_scope.borrow_mut().pop();
    }
    // Preserve the signature without allocating body IR or following calls.
    (!is_dep).then_some(FunctionSignature {
        fn_id,
        params,
        eff_class,
        lookup_scope,
    })
}

/// Push the class or namespace a definition's names are looked up in, unless
/// it is the innermost scope already; whether it was pushed. The parameters
/// and body of `C::f` look type names up in `C`; the return type of an
/// out-of-class definition does not (`It C::Make()` spells `C::It`), and an
/// in-class definition has its class in scope already.
fn enter_lookup_scope(ctx: &LowerContext, scope: Option<&String>) -> bool {
    let scoped = ctx.is_cpp && scope.is_some_and(|s| ctx.type_scope.borrow().last() != Some(s));
    if let Some(s) = scope.filter(|_| scoped) {
        ctx.type_scope.borrow_mut().push(s.clone());
    }
    scoped
}

fn lower_function_body(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    signature: FunctionSignature,
) {
    let FunctionSignature {
        fn_id,
        params,
        eff_class,
        lookup_scope,
    } = signature;
    let flow_start = program.flow.len();
    let pending_start = if ctx.record_link_ownership && ctx.has_weak {
        ctx.pending.borrow().len()
    } else {
        0
    };
    let scoped = enter_lookup_scope(ctx, lookup_scope.as_ref());
    // The body of a free function spelled `N::f` looks function names up in
    // `N` as well: open the namespaces the definition names but does not sit
    // in. A scope that neither this unit nor a header it includes opens as a
    // namespace is a class it cannot see (`Ast::Lookup` under
    // `using namespace OHOS::Hardware`), not a namespace.
    let ns_depth = ctx.ns_stack.len();
    if let Some(scope) = lookup_scope
        .as_ref()
        .filter(|scope| eff_class.is_none() && program.namespaces.contains(*scope))
    {
        let open = ctx.namespace_scope();
        let unopened = if open.is_empty() {
            Some(scope.as_str())
        } else {
            scope
                .strip_prefix(&open)
                .and_then(|rest| rest.strip_prefix("::"))
        };
        for segment in unopened.into_iter().flat_map(|rest| rest.split("::")) {
            ctx.ns_stack.push(Some(segment.to_owned()));
        }
    }
    ctx.current_fn = Some(fn_id);
    ctx.locals.clear();
    ctx.local_scope_log.clear();
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
    let local_aliases_len = ctx.local_aliases.len();

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
    ctx.local_aliases.truncate(local_aliases_len);

    if ctx.record_link_ownership && ctx.has_weak {
        let flow_end = program.flow.len();
        if flow_end > flow_start {
            program
                .function_flow_ranges
                .entry(fn_id)
                .or_default()
                .push(flow_start..flow_end);
        }
        for index in pending_start..ctx.pending.borrow().len() {
            ctx.pending_flow_owners.insert(index, fn_id);
        }
    }
    ctx.current_fn = None;
    ctx.locals.clear();
    ctx.local_scope_log.clear();
    ctx.ns_stack.truncate(ns_depth);
    ctx.class_ctx = saved_class;
    if scoped {
        ctx.type_scope.borrow_mut().pop();
    }
}

/// Longest `a::b::Cls` prefix of a qualified function name that resolves to
/// an interned class/struct tag.
fn derive_owner_class(program: &Program, qualified_name: &str) -> Option<String> {
    qualified_name
        .rmatch_indices("::")
        .map(|(at, _)| &qualified_name[..at])
        .find(|prefix| {
            program
                .types
                .type_id_by_tag(prefix, trace_ir::TypeKind::Struct)
                .is_some()
        })
        .map(str::to_owned)
}

/// The implicit `this` of a member of `cls`: a `Ptr(Struct{cls})` parameter
/// at position 0 of `fn_id`.
fn add_this_param(program: &mut Program, cls: &str, fn_id: FnId, span: Span) -> VarId {
    let this_type = program
        .types
        .intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: cls.to_owned(),
            fields: Vec::new(),
        })));
    let this_id = program.symbols.alloc_var_id();
    program.symbols.add_variable(Variable {
        is_defined: false,
        is_weak: false,
        target: None,
        is_namespaced: false,
        id: this_id,
        name: "this".to_string(),
        type_id: this_type,
        storage: StorageClass::Param,
        fn_id: Some(fn_id),
        param_index: Some(0),
        span,
        is_pointer: true,
    });
    this_id
}

/// Whether a node in a parameter list declares a parameter.
///
/// `optional_parameter_declaration` is tree-sitter's C++ node for a parameter
/// carrying a default argument (`void f(int a, int b = 0)`). Matching only
/// `parameter_declaration` dropped those, so a prototype looked lower-arity
/// than its own definition and the two stopped merging (#83).
fn is_parameter_node(kind: &str) -> bool {
    matches!(
        kind,
        "parameter_declaration" | "optional_parameter_declaration"
    )
}

/// What a parameter list declares beyond the parameters themselves: how many
/// carry a default argument, and whether it ends in `...` or a parameter pack,
/// either of which takes any number of further arguments.
#[derive(Default)]
struct ParamListShape {
    defaults: u32,
    variadic: bool,
}

impl ParamListShape {
    fn record(&mut self, param: Node) {
        match param.kind() {
            "optional_parameter_declaration" => self.defaults += 1,
            "..." | "variadic_parameter_declaration" => self.variadic = true,
            _ => {}
        }
    }
}

/// The parameters a list declares, counted the way `lower_parameter` lowers
/// them (`(void)` declares none), and its [`ParamListShape`].
fn declared_param_counts(source: &str, params_node: Node) -> (u32, ParamListShape) {
    let is_void = |param: Node| {
        param.child_by_field_name("declarator").is_none()
            && param
                .child_by_field_name("type")
                .is_some_and(|t| node_text(source, &t) == "void")
    };
    let mut declared = 0;
    let mut shape = ParamListShape::default();
    for param in params_node.children(&mut params_node.walk()) {
        if is_parameter_node(param.kind()) && !is_void(param) {
            declared += 1;
        }
        shape.record(param);
    }
    (declared, shape)
}

/// Lower the parameters `decl` lists onto `params`, after any `this` already
/// there, and return the list's [`ParamListShape`].
fn lower_parameters(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    decl: Node,
    fn_id: FnId,
    params: &mut Vec<VarId>,
) -> ParamListShape {
    let mut shape = ParamListShape::default();
    let Some(params_node) = find_params(decl) else {
        return shape;
    };
    for param in params_node.children(&mut params_node.walk()) {
        shape.record(param);
        if !is_parameter_node(param.kind()) {
            continue;
        }
        let index = params.len() as u32;
        if let Some(var) = lower_parameter(program, ctx, source, param, fn_id, index) {
            params.push(var);
        }
    }
    shape
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
    if declarator.is_some_and(|d| d.kind() == "reference_declarator") {
        ctx.reference_vars.insert(var_id);
    }
    let span = node_span(program, ctx, node);
    program.symbols.add_variable(Variable {
        is_defined: false,
        is_weak: false,
        target: None,
        is_namespaced: false,
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

/// Infer only from known value types and declared callees. This does not
/// emit calls/flows, resolve function pointers, or expand virtual overrides:
/// an expression has its statically selected declaration's return type.
fn auto_initializer_type(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    value: Node,
) -> Option<TypeDesc> {
    let value = peel_expression(value);
    let desc = if value.kind() == "call_expression" {
        match call_result_shape(program, ctx, source, value)? {
            CallResult::Decided(desc) => desc?,
            CallResult::Overload {
                candidates,
                receiver,
            } => {
                let args: Vec<TypeDesc> = call_arg_nodes(value)
                    .into_iter()
                    .map(|arg| arg_expr_type(program, ctx, source, arg))
                    .collect();
                call_result_among(program, ctx, source, value, candidates, receiver, &args)?
            }
        }
    } else {
        // A cast or `new` spells its type in the source; any other expression
        // has the type lowering gave its variable or field.
        let spelled_dependent = |ctx: &LowerContext| {
            value.child_by_field_name("type").is_some_and(|ty| {
                let spelling = node_text(source, &ty);
                spelling_is_dependent(ctx, source, value, spelling, Spelling::Source)
            })
        };
        let (desc, dependent) = match value.kind() {
            "cast_expression" => (
                arg_expr_type(program, ctx, source, value),
                spelled_dependent(ctx),
            ),
            "new_expression" => (
                receiver_desc(program, ctx, source, value)?,
                spelled_dependent(ctx),
            ),
            _ => {
                let desc = receiver_desc(program, ctx, source, value)?;
                let dependent = type_is_dependent(ctx, source, value, &desc);
                (desc, dependent)
            }
        };
        if dependent {
            return None;
        }
        desc
    };
    receiver_type(program, ctx, desc)
}

/// The argument expressions of a call, in the positions overload ranking
/// counts them by. A comment is not an argument; [`collect_call_args`] reads
/// the same list off the punctuation, for the arguments lowering emits.
fn call_arg_nodes(call: Node) -> Vec<Node> {
    match call.child_by_field_name("arguments") {
        Some(list) => list
            .named_children(&mut list.walk())
            .filter(|n| n.kind() != "comment")
            .collect(),
        None => Vec::new(),
    }
}

/// What a call expression's result type is decided by, before any argument
/// is typed. Typing an argument probes receivers and, at a cast, interns a
/// type, so a caller types them only for a shape that reads them: the
/// two-phase form of "the type a call yields, when one declaration decides it".
enum CallResult {
    /// The callee's shape alone decides the type — or that there is none.
    Decided(Option<TypeDesc>),
    /// One of several declarations' return types, chosen by argument types.
    Overload {
        candidates: Overloads,
        /// The receiver's spelling: the class template whose arguments a
        /// member's bare-parameter return substitutes.
        receiver: Option<String>,
    },
}

/// The declarations a call's result is chosen among.
enum Overloads {
    Declared(Vec<FnId>),
    /// A free function looked up by name once the argument types are known:
    /// argument-dependent lookup reads them.
    ByName(String),
}

/// The first phase of [`CallResult`]: everything the callee's shape decides.
/// `None` when the call has no result type at all — the callee is a variable,
/// or the receiver cannot be typed.
fn call_result_shape(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    value: Node,
) -> Option<CallResult> {
    // Kinds are read off the callee as written, as collect_call_at_node reads
    // them: a parenthesized name is not a bare name.
    let func = value.child_by_field_name("function")?;
    let raw = normalize_spacing(node_text(source, &func));
    let name = strip_template_args(&raw);
    if let Some(wrapper) = smart_ptr_factory(ctx, &name) {
        return Some(CallResult::Decided(smart_ptr_type(
            program,
            ctx,
            source,
            value,
            wrapper,
            &raw,
            Spelling::Source,
        )));
    }
    if matches!(func.kind(), "identifier" | "qualified_identifier") {
        let spelled = normalize_qualified(node_text(source, &func));
        if lookup_var(ctx, program, &spelled).is_none() {
            // `T(args)` constructs a `T`, as the call site reads it.
            if let Some(cls) = constructed_class(program, ctx, &spelled) {
                let desc = (!spelling_is_dependent(ctx, source, value, &spelled, Spelling::Source))
                    .then_some(TypeDesc::Struct {
                        name: cls,
                        fields: Vec::new(),
                    });
                return Some(CallResult::Decided(desc));
            }
        }
    }
    let mut receiver = None;
    let candidates = if func.kind() == "field_expression" {
        let recv = func.child_by_field_name("argument")?;
        let field = normalize_qualified(node_text(source, &func.child_by_field_name("field")?));
        let arrow = is_arrow_access(func);
        let desc = receiver_desc(program, ctx, source, recv)?;
        if !arrow && call_arg_nodes(value).is_empty() {
            if let TypeDesc::Struct { name, .. } = &desc {
                if let Some(strong) = weak_ptr_upgrade(program, name, &field) {
                    return Some(CallResult::Decided(smart_ptr_type(
                        program,
                        ctx,
                        source,
                        value,
                        &strong,
                        name,
                        Spelling::Lowered,
                    )));
                }
            }
        }
        // An arrow substitutes on its pointee, keeping template arguments
        // that member-name lookup deliberately strips.
        receiver = Some(if arrow {
            resolve_operator_arrow_receiver(program, desc)?
        } else {
            class_spelling_of_desc(&desc)?.to_string()
        });
        let cls = receiver_lookup_name(receiver.as_deref()?);
        Overloads::Declared(declared_members_upward(
            program,
            &cls,
            &trace_ir::MethodKind::Named(strip_template_args(&field)),
        ))
    } else {
        let (name, _, var) = resolve_callee(program, ctx, source, func);
        if var.is_some() {
            return None;
        }
        // An implicit member hides an outer free function of the same name,
        // just as it does for collect_call_at_node.
        let members = match &ctx.class_ctx {
            Some(cc) if is_bare_callee_node(func, &name) => declared_members_upward(
                program,
                &cc.qual_name,
                &trace_ir::MethodKind::Named(name.clone()),
            ),
            _ => Vec::new(),
        };
        if members.is_empty() {
            Overloads::ByName(name)
        } else {
            // `Get()` in a member body is `this->Get()`, so the enclosing
            // class is the receiver a base's substitution facts apply to.
            receiver = ctx.class_ctx.as_ref().map(|cc| cc.qual_name.clone());
            Overloads::Declared(members)
        }
    };
    Some(CallResult::Overload {
        candidates,
        receiver,
    })
}

/// The second phase of [`CallResult`]: the one return type the candidates
/// that take `args` agree on.
fn call_result_among(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    value: Node,
    candidates: Overloads,
    receiver: Option<String>,
    args: &[TypeDesc],
) -> Option<TypeDesc> {
    let candidates = match candidates {
        Overloads::Declared(found) => found,
        Overloads::ByName(name) => {
            let func = value.child_by_field_name("function")?;
            cpp_callee_candidates(program, ctx, func, &name, args)
        }
    };
    // A member of a template parameter -- the receiver's class (`T *t`), the
    // scope (`T::make()`) or a base (`struct M : Base`) -- has no declaration
    // yet, even when a real class shares the parameter's spelling.
    if ctx.has_templates {
        let mut enclosing: Option<Vec<Node>> = None;
        if candidates.iter().any(|&f| {
            let Some((owner, _)) = program.symbols.function(f).name.rsplit_once("::") else {
                return false;
            };
            let enclosing = enclosing.get_or_insert_with(|| ancestors(ctx, value).collect());
            mentions_template_parameter(source, enclosing.iter().copied(), owner, Spelling::Lowered)
        }) {
            return None;
        }
    }
    let candidates = filter_targets_by_argc(program, candidates, args.len(), args, args.is_empty());
    // Unknown arguments score as ties in call-edge ranking, so they rank
    // nothing out. The candidates left must all take the arguments and agree
    // on one return type; a partial match is no evidence for one of several.
    let candidates = if args.contains(&TypeDesc::Unknown) {
        candidates
    } else {
        rank_overloads(program, &candidates, args)
    };
    agreed(candidates.iter().map(|&f| {
        let function = program.symbols.function(f);
        let arity = method_explicit_arity(program, f)?;
        if !arity_takes(function, arity, args.len()) {
            return None;
        }
        let desc = program.types.get(function.return_type).desc.as_ref();
        if !matches!(desc, TypeDesc::Unknown) {
            return Some(desc.clone());
        }
        substituted_template_return(program, ctx, receiver.as_deref()?, &function.name, arity)
    }))
}

/// The one value a candidate set agrees on: nothing when it is empty, when a
/// candidate names no value, or when two candidates disagree. A partial match
/// is no evidence for one of several.
fn agreed<T: PartialEq>(mut candidates: impl Iterator<Item = Option<T>>) -> Option<T> {
    let first = candidates.next()??;
    candidates
        .all(|value| value.as_ref() == Some(&first))
        .then_some(first)
}

/// What a class-template member declared to return a bare type parameter
/// yields for a receiver that names its arguments: `Holder<Widget *>::Get`
/// returns `Widget *`. Substitution facts are shared by same-name
/// declarations (in-class prototypes carry no parameter types), so the
/// same-arity ones must agree on one return.
///
/// The facts belong to the class that declares `member`, which a receiver
/// inherits it from (`struct Adapter : Holder<Widget *>`) as readily as it
/// spells it. The arguments to substitute are then the ones the receiver's
/// own spelling carries, or the ones its base spelling names.
fn substituted_template_return(
    program: &Program,
    ctx: &LowerContext,
    receiver: &str,
    member: &str,
    arity: usize,
) -> Option<TypeDesc> {
    let (owner, _) = member.rsplit_once("::")?;
    let facts = program.template_returns_of(owner, member)?;
    let (args, scope) = template_arguments_for(program, receiver, owner)?;
    agreed(
        facts
            .iter()
            .filter(|fact| fact.arity as usize == arity)
            .map(|fact| substituted_parameter(program, ctx, &args, scope, fact)),
    )
}

/// The template arguments `receiver` gives `owner`: its own, when it spells
/// that class template, else those of the base it inherits `owner` through.
/// Only a base spelled with arguments substitutes; `struct D : Holder<T>`
/// inside another template names no concrete type to substitute.
fn template_arguments_for<'a>(
    program: &'a Program,
    receiver: &str,
    owner: &str,
) -> Option<(Vec<String>, Option<&'a str>)> {
    let cls = receiver_lookup_name(receiver);
    if cls == owner {
        return Some((template_arguments(receiver), None));
    }
    // However many classes up, walked as `declared_members_upward` walks them.
    let mut queue = std::collections::VecDeque::from([cls.into_owned()]);
    let mut seen = std::collections::BTreeSet::new();
    while let Some(cur) = queue.pop_front() {
        if let Some(base) = program
            .template_bases_of(&cur)
            .iter()
            .find(|base| receiver_lookup_name(&base.spelling) == owner)
        {
            return (!base.is_dependent).then(|| {
                (
                    template_arguments(&base.spelling),
                    Some(base.declaration_scope.as_str()),
                )
            });
        }
        for base in program.bases_of(&cur) {
            if seen.len() < MAX_BASE_LOOKUP && seen.insert(base.clone()) {
                queue.push_back(base);
            }
        }
    }
    None
}

/// One fact substituted: the template argument at the parameter position it
/// records, under the pointer layers the argument spells plus the ones the
/// declaration adds (`T *Get()`). An argument naming no class in view is no
/// receiver type, so it yields nothing rather than a name to invent members on.
fn substituted_parameter(
    program: &Program,
    ctx: &LowerContext,
    args: &[String],
    scope: Option<&str>,
    fact: &trace_ir::TemplateReturn,
) -> Option<TypeDesc> {
    let (base, suffix) = split_pointer_suffix(args.get(fact.parameter?)?);
    let base = sanitize_type_name(base);
    // Inherited arguments belong to the base declaration, regardless of
    // where the receiver is used. Direct receiver spellings use call scope.
    let name = match scope {
        Some(scope) => held_class_in_declaration_scope(program, scope, &base),
        None => held_class(program, ctx, &base),
    }
    .or_else(|| is_std_smart_ptr_name(&receiver_lookup_name(&base)).then(|| base.clone()))?;
    let mut desc = TypeDesc::Struct {
        name,
        fields: Vec::new(),
    };
    for _ in 0..suffix.matches('*').count() + fact.pointer_depth {
        desc = TypeDesc::Ptr(Box::new(desc));
    }
    Some(desc)
}

/// Resolve a stored base argument without consulting the caller's scope.
fn held_class_in_declaration_scope(
    program: &Program,
    declaration_scope: &str,
    arg: &str,
) -> Option<String> {
    let lookup = receiver_lookup_name(arg);
    let mut scope = declaration_scope;
    let name = loop {
        let candidate = if lookup.starts_with("::") || scope.is_empty() {
            lookup.strip_prefix("::").unwrap_or(&lookup).to_string()
        } else {
            format!("{scope}::{lookup}")
        };
        if let Some(name) = declared_class_name(program, &candidate) {
            break name;
        }
        if scope.is_empty() || lookup.starts_with("::") {
            if is_std_smart_ptr_name(&lookup) {
                break lookup.into_owned();
            }
            return None;
        }
        scope = scope.rsplit_once("::").map_or("", |(parent, _)| parent);
    };
    if !arg.contains('<') {
        return Some(name);
    }
    // A later chained call will substitute these arguments from another
    // receiver, so retain their declaration scope in the spelling too.
    let args = template_arguments(arg)
        .into_iter()
        .map(|arg| {
            let (base, suffix) = split_pointer_suffix(&arg);
            let base = sanitize_type_name(base);
            let qualified =
                held_class_in_declaration_scope(program, declaration_scope, &base).unwrap_or(base);
            format!("{qualified}{suffix}")
        })
        .collect::<Vec<_>>();
    Some(format!("{name}<{}>{}", args.join(","), template_tail(arg)))
}

/// `desc` when it can type a receiver: a class, union or function pointer
/// under its pointer layers. A scalar can be the stand-in lowering gives a
/// type name it could not resolve, which would rank overloads by a guess. A
/// standard smart pointer whose argument names no class as written, while the
/// scopes here name one by it (`std::shared_ptr<TraceStrategy>` declared under
/// `using namespace OHOS::HiviewDFX`), would reach members under a spelling
/// the index may not hold; it is left untyped rather than guessed.
fn receiver_type(program: &Program, ctx: &LowerContext, desc: TypeDesc) -> Option<TypeDesc> {
    match desc {
        TypeDesc::Ptr(inner) => Some(TypeDesc::Ptr(Box::new(receiver_type(
            program, ctx, *inner,
        )?))),
        TypeDesc::Struct { ref name, .. } if is_std_smart_ptr_name(&receiver_lookup_name(name)) => {
            let held = template_arguments(name)
                .first()
                .map(|arg| receiver_lookup_name(arg.trim()).into_owned());
            let ambiguous = held.is_some_and(|held| {
                declared_class_name(program, &held).is_none()
                    && class_seen_from(program, ctx, &held).is_some_and(|seen| seen != held)
            });
            (!ambiguous).then_some(desc)
        }
        TypeDesc::Struct { .. } | TypeDesc::Union { .. } | TypeDesc::FnPtr { .. } => Some(desc),
        _ => None,
    }
}

/// The class a smart pointer's template argument names from this scope,
/// cv-qualifiers dropped and template arguments kept (`const Box<int>` holds
/// `ns::Box<int>`).
fn held_class(program: &Program, ctx: &LowerContext, arg: &str) -> Option<String> {
    let arg = arg.trim();
    let arg = arg.strip_prefix("const ").unwrap_or(arg);
    let arg = arg.strip_suffix(" const").unwrap_or(arg).trim();
    let class = class_seen_from(program, ctx, &receiver_lookup_name(arg))?;
    Some(match arg.find('<') {
        Some(at) => format!("{class}{}", &arg[at..]),
        None => class,
    })
}

/// The class `name` denotes from the current scope: through the enclosing
/// classes and namespaces, else through a `using namespace N;` in scope. A
/// type declared under such a directive spells N's classes bare, and lowering
/// files a bare name it cannot find under the namespace it sits in
/// (`strat_user::Strategy` for `hdfx::Strategy`).
fn class_seen_from(program: &Program, ctx: &LowerContext, name: &str) -> Option<String> {
    declared_class_in_scope(program, ctx, name).or_else(|| {
        let mut bare = name;
        for segment in ctx.ns_stack.iter().flatten() {
            match bare
                .strip_prefix(segment.as_str())
                .and_then(|rest| rest.strip_prefix("::"))
            {
                Some(rest) => bare = rest,
                None => break,
            }
        }
        ctx.using_nss
            .iter()
            .find_map(|ns| declared_class_name(program, &format!("{ns}::{bare}")))
    })
}

/// Standard factories whose one template argument names the class the
/// returned smart pointer holds.
const SMART_PTR_FACTORIES: &[(&str, &str)] = &[
    ("std::make_shared", "std::shared_ptr"),
    ("std::make_unique", "std::unique_ptr"),
];

/// The smart pointer the factory a call spells `name` returns: `std::make_shared`,
/// `::std::make_shared`, or a bare `make_shared` that `using namespace std;` or
/// `using std::make_shared;` brings into scope.
fn smart_ptr_factory(ctx: &LowerContext, name: &str) -> Option<&'static str> {
    let name = global_lookup_name(name);
    SMART_PTR_FACTORIES
        .iter()
        .find(|(factory, _)| {
            *factory == name
                || factory.strip_prefix("std::") == Some(name)
                    && (ctx.using_nss.iter().any(|ns| ns == "std")
                        || ctx
                            .using_name_imports
                            .iter()
                            .any(|(base, qualified)| base == name && qualified == factory))
        })
        .map(|&(_, wrapper)| wrapper)
}

/// Weak pointers, by their last name segment, and the method that yields the
/// matching strong pointer.
const WEAK_PTR_UPGRADES: &[(&str, &str, &str)] = &[
    ("weak_ptr", "lock", "shared_ptr"),
    ("wptr", "promote", "sptr"),
];

/// The strong pointer `method` on a weak pointer typed `name` yields, spelled
/// in the weak pointer's own scope (`OHOS::CameraStandard::wptr` promotes to
/// `OHOS::CameraStandard::sptr`). Only for a wrapper whose body is not in the
/// tree: a declared one has members of its own to look up.
fn weak_ptr_upgrade(program: &Program, name: &str, method: &str) -> Option<String> {
    let cls = receiver_lookup_name(name);
    if !is_undefined_wrapper(program, name, &cls) {
        return None;
    }
    let last = last_type_segment(&cls);
    let &(_, _, strong) = WEAK_PTR_UPGRADES
        .iter()
        .find(|(weak, upgrade, _)| *weak == last && *upgrade == method)?;
    Some(format!("{}{strong}", &cls[..cls.len() - last.len()]))
}

/// `wrapper<class>` for a smart-pointer model whose one template argument is
/// the one `spelled` carries (`make_shared<T>`, `weak_ptr<T>`), unless that
/// argument names an enclosing template parameter.
fn smart_ptr_type(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    value: Node,
    wrapper: &str,
    spelled: &str,
    from: Spelling,
) -> Option<TypeDesc> {
    let [arg]: [String; 1] = template_arguments(spelled).try_into().ok()?;
    let class = held_class(program, ctx, &arg)?;
    (!spelling_is_dependent(ctx, source, value, &arg, from)).then(|| TypeDesc::Struct {
        name: format!("{wrapper}<{class}>"),
        fields: Vec::new(),
    })
}

/// The ancestors of `node`, root first, excluding `node` itself.
///
/// Every `Node::parent` call re-descends from the root and scans each level's
/// children, so walking a deep node's ancestors one `parent` at a time costs a
/// descent per level. Every signature in a unit with templates asks for its
/// enclosing ones, and a preprocessed unit has thousands of top-level
/// declarations: descend once per question.
fn ancestors<'t>(ctx: &'t LowerContext, node: Node<'t>) -> impl Iterator<Item = Node<'t>> {
    std::iter::successors(Some(ctx.tree.root_node()), move |cur| {
        cur.child_with_descendant(node)
    })
    .take_while(move |cur| cur.id() != node.id())
}

/// Whether `spelling` mentions a parameter of any `template_declaration` in
/// `path`.
fn mentions_template_parameter<'t>(
    source: &str,
    path: impl IntoIterator<Item = Node<'t>>,
    spelling: &str,
    from: Spelling,
) -> bool {
    path.into_iter()
        .filter(|n| n.kind() == "template_declaration")
        .filter_map(|n| n.child_by_field_name("parameters"))
        .any(|params| {
            template_parameter_names(source, params)
                .into_iter()
                .flatten()
                .any(|name| spelling_mentions(spelling, from, name))
        })
}

/// The name of each parameter of a `template_parameter_list`, by position;
/// `None` for an unnamed one. Comments between parameters are not positions.
/// A type parameter's name is its identifier (`class T`, `class... Ts`,
/// `class T = int`, `template<class> class TT`); a value parameter's is in its
/// declarator (`size_t N`, `Kind K = {}`, `int... Ns`), never its type.
fn template_parameter_names<'s>(source: &'s str, params: Node) -> Vec<Option<&'s str>> {
    fn type_identifier(node: Node) -> Option<Node> {
        node.named_children(&mut node.walk())
            .find(|n| n.kind() == "type_identifier")
    }
    params
        .named_children(&mut params.walk())
        .filter(|p| p.kind() != "comment")
        .map(|param| {
            match param.kind() {
                "type_parameter_declaration" | "variadic_type_parameter_declaration" => {
                    type_identifier(param)
                }
                "optional_type_parameter_declaration" => param.child_by_field_name("name"),
                "template_template_parameter_declaration" => param
                    .named_children(&mut param.walk())
                    .find(|n| n.kind() != "template_parameter_list")
                    .and_then(type_identifier),
                "parameter_declaration"
                | "optional_parameter_declaration"
                | "variadic_parameter_declaration" => param
                    .child_by_field_name("declarator")
                    .and_then(declarator_identifier),
                _ => None,
            }
            .map(|ident| node_text(source, &ident))
            // Error recovery can leave a zero-width name, which every spelling
            // would contain.
            .filter(|name| !name.is_empty())
        })
        .collect()
}

/// The identifier a declarator declares, through pointer, reference and
/// pack layers; `None` for an abstract declarator.
fn declarator_identifier(node: Node) -> Option<Node> {
    if node.kind() == "identifier" {
        return Some(node);
    }
    node.child_by_field_name("declarator")
        .or_else(|| {
            node.named_children(&mut node.walk())
                .find(|n| n.kind() == "identifier" || n.kind().ends_with("_declarator"))
        })
        .and_then(declarator_identifier)
}

/// Where a type spelling tested for template parameters comes from.
#[derive(Clone, Copy)]
enum Spelling {
    /// As written: `OHOS::Event` names a member of `OHOS`, never a parameter.
    Source,
    /// As lowering qualified it: a parameter `T` of a member template of `C`
    /// reads `C::T`, so a qualified identifier can still be the parameter.
    Lowered,
}

/// Whether the type spelling names `name` as a whole identifier, outside any
/// qualification when it is a `Spelling::Source`.
fn spelling_mentions(spelling: &str, from: Spelling, name: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    spelling.match_indices(name).any(|(at, _)| {
        let before = &spelling[..at];
        !before.ends_with(is_ident)
            && !spelling[at + name.len()..].starts_with(is_ident)
            && (matches!(from, Spelling::Lowered) || !before.trim_end().ends_with("::"))
    })
}

/// Dependent returns stay unknown in the shared function signature. Bare
/// class parameters have separate substitution facts for concrete receivers.
/// Check names in all enclosing template parameter lists.
fn return_is_dependent(ctx: &LowerContext, source: &str, node: Node) -> bool {
    ctx.has_templates
        && return_is_dependent_under(source, &ancestors(ctx, node).collect::<Vec<_>>(), node)
}

/// [`return_is_dependent`] against an ancestor path already walked.
/// `ancestors` rescans from the root once per level, so a caller with more
/// than one question about the enclosing templates walks it once.
fn return_is_dependent_under(source: &str, path: &[Node], node: Node) -> bool {
    // The declaration whose `type` is the return type: `node` itself, or the
    // nearest ancestor with one (a prototype's declarator sits under it).
    let Some((enclosing, ty)) = node
        .child_by_field_name("type")
        .map(|ty| (path, ty))
        .or_else(|| {
            path.iter()
                .enumerate()
                .rev()
                .find_map(|(i, n)| Some((&path[..i], n.child_by_field_name("type")?)))
        })
    else {
        return false;
    };
    enclosing.iter().any(|n| n.kind() == "template_declaration")
        && spelling_names_dependent_type(source, enclosing, node_text(source, &ty), 0)
}

/// Whether a type spelled in a template names one of its parameters, directly
/// or through a member alias of an enclosing class
/// (`using Ptr = std::shared_ptr<T>; Ptr Get();`), a few aliases deep.
fn spelling_names_dependent_type(
    source: &str,
    enclosing: &[Node],
    spelling: &str,
    depth: u8,
) -> bool {
    const MAX_ALIAS_DEPTH: u8 = 4;
    mentions_template_parameter(
        source,
        enclosing.iter().copied(),
        spelling,
        Spelling::Source,
    ) || depth < MAX_ALIAS_DEPTH
        && enclosing
            .iter()
            .filter(|n| n.kind() == "field_declaration_list")
            .flat_map(|body| body.named_children(&mut body.walk()).collect::<Vec<_>>())
            .filter_map(|member| member_alias(source, member))
            .any(|(name, aliased)| {
                spelling_mentions(spelling, Spelling::Source, name)
                    && spelling_names_dependent_type(source, enclosing, aliased, depth + 1)
            })
}

/// The name a member `using` alias or `typedef` declares, and the text of the
/// type it stands for.
fn member_alias<'s>(source: &'s str, member: Node) -> Option<(&'s str, &'s str)> {
    match member.kind() {
        "alias_declaration" => Some((
            node_text(source, &member.child_by_field_name("name")?),
            node_text(source, &member.child_by_field_name("type")?),
        )),
        "type_definition" => {
            let name = std::iter::successors(member.child_by_field_name("declarator"), |d| {
                d.child_by_field_name("declarator")
            })
            .find(|d| d.kind() == "type_identifier")?;
            Some((node_text(source, &name), node_text(source, &member)))
        }
        _ => None,
    }
}

fn spelling_is_dependent(
    ctx: &LowerContext,
    source: &str,
    node: Node,
    spelling: &str,
    from: Spelling,
) -> bool {
    ctx.has_templates && mentions_template_parameter(source, ancestors(ctx, node), spelling, from)
}

fn type_is_dependent(ctx: &LowerContext, source: &str, node: Node, desc: &TypeDesc) -> bool {
    match desc {
        TypeDesc::Ptr(inner) | TypeDesc::Array { elem: inner, .. } => {
            type_is_dependent(ctx, source, node, inner)
        }
        TypeDesc::Struct { name, .. } | TypeDesc::Union { name, .. } => {
            spelling_is_dependent(ctx, source, node, name, Spelling::Lowered)
        }
        _ => false,
    }
}

/// Only a bare parameter of the owning class template can be substituted.
/// Function templates and dependent compound types remain unknown. `path` is
/// `node`'s ancestors and `pointer_depth` its declarator's pointer layers,
/// both already computed by [`declared_return_type`].
fn register_template_return(
    program: &mut Program,
    source: &str,
    path: &[Node],
    node: Node,
    name: &str,
    pointer_depth: usize,
) {
    let Some((owner, _)) = name.rsplit_once("::") else {
        return;
    };
    let Some(ty) = node.child_by_field_name("type") else {
        return;
    };
    let spelling = node_text(source, &ty).trim();
    let Some((depth, template, class)) = path
        .iter()
        .rev()
        .filter(|n| n.kind() == "template_declaration")
        .enumerate()
        .find_map(|(depth, template)| {
            let class = template
                .named_children(&mut template.walk())
                .find(|n| matches!(n.kind(), "class_specifier" | "struct_specifier"))?;
            Some((depth, *template, class))
        })
    else {
        return;
    };
    let Some(class_name) = class.child_by_field_name("name") else {
        return;
    };
    if node_text(source, &class_name) != last_type_segment(owner) {
        return;
    }
    let Some(params) = template.child_by_field_name("parameters") else {
        return;
    };
    // Facts are shared by same-name/same-arity declarations. An unsupported
    // overload must block substitution, not borrow another overload's return.
    let parameter = (depth == 0)
        .then(|| {
            template_parameter_names(source, params)
                .iter()
                .position(|p| *p == Some(spelling))
        })
        .flatten();
    let arity = find_params(node).map_or(0, |p| declared_param_counts(source, p).0);
    let fact = trace_ir::TemplateReturn {
        arity,
        parameter,
        pointer_depth,
    };
    program.add_template_return(owner, name, &fact);
}

fn declared_return_type(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    base: trace_ir::TypeId,
    name: &str,
) -> trace_ir::TypeId {
    let depth = node
        .child_by_field_name("declarator")
        .and_then(fn_decl_under_pointer)
        .map_or(0, |(_, depth, _)| depth);
    if !ctx.has_templates {
        return pointer_layers(program, base, depth);
    }
    // One ancestor walk answers both questions the enclosing templates
    // decide: whether the return is dependent, and which parameter a
    // concrete receiver substitutes for it.
    let path: Vec<Node> = ancestors(ctx, node).collect();
    let dependent = return_is_dependent_under(source, &path, node);
    if dependent || matches!(program.types.get(base).desc.as_ref(), TypeDesc::Unknown) {
        register_template_return(program, source, &path, node, name, depth);
    }
    if dependent {
        return program.types.unknown();
    }
    pointer_layers(program, base, depth)
}

/// `ty` under `depth` pointer layers.
fn pointer_layers(program: &mut Program, ty: trace_ir::TypeId, depth: usize) -> trace_ir::TypeId {
    (0..depth).fold(ty, |ty, _| {
        let inner = program.types.get(ty).desc.as_ref().clone();
        program.types.ptr_to(inner)
    })
}

/// The type of a local declared `decl` with base type `type_id`. An `auto`
/// local takes its initializer's type, which already includes pointer layers
/// (`auto *p` deduces the pointee); `auto&` takes it too, as the value the
/// reference names. An initializer with fewer pointer layers than the
/// declarator asks for (`auto *p = value()`) deduces nothing.
fn local_type(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    type_node: Node,
    type_id: trace_ir::TypeId,
    decl: Node,
    value: Option<Node>,
) -> trace_ir::TypeId {
    let inferred = value
        .filter(|_| is_placeholder_type(ctx, type_node))
        .and_then(|value| auto_initializer_type(program, ctx, source, value))
        .filter(|desc| pointer_depth(desc) >= declarator_pointer_depth(decl));
    let desc = inferred.unwrap_or_else(|| {
        walk_declarator_shape(decl, program.types.get(type_id).desc.as_ref().clone())
    });
    program.types.intern(desc)
}

/// A C++ `auto` type specifier.
fn is_placeholder_type(ctx: &LowerContext, type_node: Node) -> bool {
    ctx.is_cpp && type_node.kind() == "placeholder_type_specifier"
}

/// How many pointer layers `desc` has.
fn pointer_depth(desc: &TypeDesc) -> usize {
    std::iter::successors(Some(desc), |desc| desc.pointee()).count() - 1
}

/// How many `*` a declarator puts in front of its name, through references
/// and parentheses (`**p`, `*&p`).
fn declarator_pointer_depth(decl: Node) -> usize {
    std::iter::successors(Some(decl), |d| {
        d.child_by_field_name("declarator").or_else(|| {
            matches!(
                d.kind(),
                "reference_declarator" | "parenthesized_declarator"
            )
            .then(|| d.named_child(0))
            .flatten()
        })
    })
    .filter(|d| d.kind() == "pointer_declarator")
    .count()
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

    let lower_initialized = |program: &mut Program,
                             ctx: &mut LowerContext,
                             owner: Node,
                             decl: Node,
                             value: Option<Node>| {
        let capture_initializer = ctx.record_link_ownership
            && ctx.has_weak
            && ctx.current_fn.is_none()
            && value.is_some();
        let flow_start = program.flow.len();
        let pending_start = if capture_initializer {
            ctx.pending.borrow().len()
        } else {
            0
        };
        let ty = local_type(program, ctx, source, type_node, type_id, decl, value);
        let var = lower_one_declarator(
            program,
            ctx,
            source,
            owner,
            decl,
            ty,
            is_static,
            storage_override,
            value,
        );
        if let Some(var) = var.filter(|_| capture_initializer) {
            let end = program.flow.len();
            if end > flow_start {
                program
                    .global_initializer_ranges
                    .entry(var)
                    .or_default()
                    .push(flow_start..end);
            }
            for index in pending_start..ctx.pending.borrow().len() {
                ctx.pending_initializer_owners.insert(index, var);
            }
        }
        // An explicit reference's type carries an alias layer that readers
        // peel; an `auto&` local's is already the value's own type.
        if let Some(var) = var.filter(|_| is_placeholder_type(ctx, type_node)) {
            ctx.reference_vars.remove(&var);
        }
    };

    // A condition declaration (`if (auto p = f())`) stores its initializer
    // directly on the declaration, without an init_declarator child. Its
    // variable starts at the declarator, as an init_declarator's does.
    if let (Some(decl), Some(value)) = (
        node.child_by_field_name("declarator"),
        node.child_by_field_name("value"),
    ) {
        lower_initialized(program, ctx, decl, decl, Some(value));
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "init_declarator" => {
                let decl = child.child_by_field_name("declarator").unwrap_or(child);
                lower_initialized(
                    program,
                    ctx,
                    child,
                    decl,
                    child.child_by_field_name("value"),
                );
            }
            // Inside a body, `cb && cb();` parses as a reference-returning
            // prototype; a real one is declared at namespace scope.
            "declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "function_declarator"
            | "array_declarator"
                if child.kind() != "reference_declarator" || ctx.current_fn.is_none() =>
            {
                // A pointer-returning function declaration (`T *f(void);`) is a
                // `pointer_declarator` wrapping a `function_declarator`; it must
                // register a function, not a variable that shadows the name --
                // unless it defines an object, `T w(a);` or `T *p(buf);`.
                if let Some((fdecl, ptr_depth, binds_reference)) = fn_decl_under_pointer(child) {
                    let ty = pointer_layers(program, type_id, ptr_depth);
                    // `T &r(a);` binds a reference, which constructs nothing;
                    // a reference variable here is not modeled.
                    match direct_init_arguments(program, ctx, source, fdecl) {
                        Some(_) if binds_reference => {}
                        Some(args) => lower_direct_init(
                            program,
                            ctx,
                            source,
                            fdecl,
                            ty,
                            is_static,
                            storage_override,
                            args,
                        ),
                        None => lower_function_decl(program, ctx, source, fdecl, ty, is_static),
                    }
                    continue;
                }
                let shaped_type_id = {
                    let base = program.types.get(type_id).desc.as_ref().clone();
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
                let storage = storage_override.unwrap_or_else(|| storage_for(ctx, is_static));
                program.symbols.add_variable(Variable {
                    is_defined: ctx.current_fn.is_none() && !declaration_is_extern(source, node),
                    // `child`, not `node`: an attribute inside a sibling
                    // declarator is that sibling's alone. `extern` is a
                    // declaration specifier, so it stays declaration-scoped.
                    is_weak: weak_global(ctx, storage, source, child),
                    target: None,
                    is_namespaced: namespaced_global(ctx, storage),
                    id: var_id,
                    name: name.clone(),
                    type_id,
                    storage,
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

/// The explicit arguments of `T w(a, b);` when it defines an object rather
/// than declaring a function. C++ reads it as a function declaration only
/// when the parenthesized names are types; tree-sitter cannot tell and always
/// does, so `Worker w(OnReady);` registered a function `w` and constructed
/// nothing. A list of bare names that each resolve to a variable or a function
/// in scope is an argument list. `T w();` stays a declaration, as it is in C++.
/// At file scope a name counts as a global or file `static` variable (the
/// file's own or an included header's) or a function.
fn direct_init_arguments(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    decl: Node,
) -> Option<CallArgs> {
    if !ctx.is_cpp || decl.kind() != "function_declarator" {
        return None;
    }
    if decl.child_by_field_name("declarator")?.kind() != "identifier" {
        return None;
    }
    let params = decl.child_by_field_name("parameters")?;
    let mut args = CallArgs::empty();
    for param in params.named_children(&mut params.walk()) {
        if param.kind() == "comment" {
            continue;
        }
        if param.kind() != "parameter_declaration"
            || param.child_by_field_name("declarator").is_some()
        {
            return None;
        }
        let name_node = param.child_by_field_name("type")?;
        if !matches!(name_node.kind(), "type_identifier" | "qualified_identifier") {
            return None;
        }
        let name = normalize_qualified(node_text(source, &name_node));
        let index = args.argc;
        if let Some(&var) = ctx.locals.get(&name) {
            args.var_args.push((index, var));
        } else if names_type_in_scope(program, ctx, &name) {
            // A type declared nearer than any global of that name hides it:
            // `using Value = int;` makes `Worker make(Value);` a declaration.
            return None;
        } else if ctx
            .class_ctx
            .as_ref()
            .is_some_and(|c| class_has_data_field(program, &c.qual_name, &name))
        {
            // A data member is a value, so this defines an object:
            // `std::lock_guard<std::mutex> g(mu_);`. Its value is not a
            // variable here, and the position stays without an actual, as a
            // literal's does. Checked before `lookup_var`: in class scope the
            // member hides a variable of its name outside the class.
        } else if let Some(var) = lookup_var(ctx, program, &name) {
            args.var_args.push((index, var));
        } else {
            args.fn_args
                .push((index, resolve_function_named(program, ctx, &name)?));
        }
        args.argc += 1;
        args.arg_desc.push(TypeDesc::Unknown);
    }
    (args.argc > 0).then_some(args)
}

/// Lower `T w(args);` as the object definition [`direct_init_arguments`]
/// recognized: the local, then its constructor call with the object as
/// `this`, or for a type without one, `w` initialized from its one argument.
#[allow(clippy::too_many_arguments)]
fn lower_direct_init(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    decl: Node,
    type_id: trace_ir::TypeId,
    is_static: bool,
    storage_override: Option<StorageClass>,
    args: CallArgs,
) {
    let Some(name_node) = decl.child_by_field_name("declarator") else {
        return;
    };
    let capture_initializer = ctx.record_link_ownership && ctx.has_weak && ctx.current_fn.is_none();
    let flow_start = program.flow.len();
    let pending_start = if capture_initializer {
        ctx.pending.borrow().len()
    } else {
        0
    };
    let Some(object) = lower_one_declarator(
        program,
        ctx,
        source,
        decl,
        name_node,
        type_id,
        is_static,
        storage_override,
        None,
    ) else {
        return;
    };
    if ctx.current_fn.is_none() {
        // Even `extern T object(value)` is a definition when initialized.
        program.symbols.variable_mut(object).is_defined = true;
    }
    let span = node_span(program, ctx, decl);
    if program.is_dep_file(span.file) {
        return;
    }
    let Some(cls) = named_class(program, type_id) else {
        match (args.var_args.as_slice(), args.fn_args.as_slice()) {
            ([(_, src)], []) => program.flow.push(FlowConstraint::Copy {
                dst: object,
                src: *src,
            }),
            ([], [(_, callee)]) => program.flow.push(FlowConstraint::AddrOfFn {
                dst: object,
                callee: *callee,
            }),
            _ => {}
        }
        if capture_initializer {
            let end = program.flow.len();
            if end > flow_start {
                program
                    .global_initializer_ranges
                    .entry(object)
                    .or_default()
                    .push(flow_start..end);
            }
            for index in pending_start..ctx.pending.borrow().len() {
                ctx.pending_initializer_owners.insert(index, object);
            }
        }
        return;
    };
    let Some(caller) = ctx.current_fn else {
        return;
    };
    emit_member_sites(
        program,
        caller,
        &cls,
        &trace_ir::MethodKind::Ctor,
        Some(object),
        args,
        span,
    );
}

/// Whether `name` spelled in the current scope denotes a type: a
/// function-local alias, or a class or typedef declared in an enclosing class
/// or namespace. At the global scope only a typedef counts, since a global
/// variable or function of the same name hides a class there (`struct stat`
/// beside `stat`).
fn names_type_in_scope(program: &Program, ctx: &LowerContext, name: &str) -> bool {
    local_alias(ctx, name).is_some()
        || find_in_scope(program, ctx, name, |candidate, global| {
            let class = !global
                && (program.types.is_struct_declared(candidate)
                    || program
                        .types
                        .type_id_by_tag(candidate, trace_ir::TypeKind::Struct)
                        .is_some());
            (class || program.types.resolve_alias(candidate).is_some()).then_some(())
        })
        .is_some()
}

/// The class a declared type names, if any: a named struct, not an anonymous
/// one.
fn named_class(program: &Program, type_id: trace_ir::TypeId) -> Option<String> {
    match program.types.get(type_id).desc.as_ref() {
        TypeDesc::Struct { name, .. } if !is_anonymous_tag(name) => Some(name.clone()),
        _ => None,
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
) -> Option<VarId> {
    if is_function_pointer_declarator(decl) {
        let (name, _is_ptr) = parse_declarator_name(source, decl);
        if name.is_empty() {
            return None;
        }
        let var_id = program.symbols.alloc_var_id();
        let span = node_span(program, ctx, span_node);
        let storage = storage_override.unwrap_or_else(|| storage_for(ctx, is_static));
        program.symbols.add_variable(Variable {
            is_defined: ctx.current_fn.is_none()
                && (init_expr.is_some() || !declaration_is_extern(source, span_node)),
            is_weak: weak_global(ctx, storage, source, span_node),
            target: None,
            is_namespaced: namespaced_global(ctx, storage),
            id: var_id,
            name: name.clone(),
            type_id,
            storage,
            fn_id: ctx.current_fn,
            param_index: None,
            span,
            is_pointer: true,
        });
        register_local(ctx, name, var_id);
        if program.is_dep_file(span.file) {
            return Some(var_id);
        }
        if let Some(init) = init_expr {
            if init.kind() == "initializer_list"
                && (is_array_type(program, type_id) || declarator_is_array(decl))
            {
                lower_fn_ptr_array_init(program, ctx, source, var_id, init);
            }
            extract_flow_from_expr(program, ctx, source, init, Some(var_id));
        }
        return Some(var_id);
    }

    if decl.kind() == "function_declarator" && !is_function_pointer_declarator(decl) {
        lower_function_decl(program, ctx, source, decl, type_id, is_static);
        return None;
    }

    let (name, is_ptr) = parse_declarator_name(source, decl);
    if name.is_empty() {
        return None;
    }
    let var_id = program.symbols.alloc_var_id();
    if decl.kind() == "reference_declarator" {
        ctx.reference_vars.insert(var_id);
    }
    let span = node_span(program, ctx, span_node);
    let storage = storage_override.unwrap_or_else(|| storage_for(ctx, is_static));
    program.symbols.add_variable(Variable {
        is_defined: ctx.current_fn.is_none()
            && (init_expr.is_some() || !declaration_is_extern(source, span_node)),
        is_weak: weak_global(ctx, storage, source, span_node),
        target: None,
        is_namespaced: namespaced_global(ctx, storage),
        id: var_id,
        name: name.clone(),
        type_id,
        storage,
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: is_ptr,
    });
    register_local(ctx, name, var_id);
    if program.is_dep_file(span.file) {
        return Some(var_id);
    }
    // Constructor invocation spelled as a declaration: `Cls o(1, 2);`.
    // tree-sitter parks the argument list in init_declarator's `value`
    // field, so an argument_list "initializer" IS the ctor call. `Cls o{1, 2};`
    // calls a constructor too when the class declares a user-provided one; a
    // class without one is an aggregate, whose braces initialize its fields.
    let mut braced_ctor = false;
    if ctx.is_cpp && span_node.kind() == "init_declarator" && ctx.current_fn.is_some() {
        if let Some(cls) = named_class(program, type_id) {
            // A constructor defaulted or deleted in its class is not
            // user-provided and leaves the class an aggregate (C++17).
            braced_ctor = init_expr.is_some_and(|n| n.kind() == "initializer_list")
                && declared_members_upward(program, &cls, &trace_ir::MethodKind::Ctor)
                    .iter()
                    .any(|&ctor| !program.symbols.function(ctor).defaulted_in_class);
            let ctor_args: Option<Node> = match init_expr {
                Some(n) if n.kind() == "argument_list" || braced_ctor => Some(n),
                _ => span_node
                    .children(&mut span_node.walk())
                    .find(|c| c.kind() == "argument_list"),
            };
            if ctor_args.is_some() {
                let span = node_span(program, ctx, span_node);
                let call_args = collect_call_args(program, ctx, source, ctor_args);
                // The implicit `this` points to the object being constructed.
                emit_member_sites(
                    program,
                    ctx.current_fn.unwrap(),
                    &cls,
                    &trace_ir::MethodKind::Ctor,
                    Some(var_id),
                    call_args,
                    span,
                );
            }
        }
    }
    if let Some(init) = init_expr {
        if init.kind() != "argument_list" && !braced_ctor {
            // A ctor argument list is not a value flowing into the object.
            if init.kind() == "initializer_list"
                && (is_array_type(program, type_id) || declarator_is_array(decl))
            {
                lower_fn_ptr_array_init(program, ctx, source, var_id, init);
            }
            extract_flow_from_expr(program, ctx, source, init, Some(var_id));
        }
    }
    Some(var_id)
}

/// `ArrayFnMember` facts are only sound for array-typed tables (unknown-index
/// element access merges all members). Parking members of a plain struct
/// initializer into the variable node would let any field load observe every
/// member function regardless of field identity.
fn is_array_type(program: &Program, type_id: trace_ir::TypeId) -> bool {
    matches!(
        program.types.get(type_id).desc.as_ref(),
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
    let ret_type = if return_is_dependent(ctx, source, decl) {
        program.types.unknown()
    } else {
        ret_type
    };
    let provisional_id = program.symbols.alloc_fn_id();
    let provisional_start = (
        program.symbols.variables.len(),
        program.symbols.call_sites.len(),
    );
    let mut params = Vec::new();
    let shape = lower_parameters(program, ctx, source, decl, provisional_id, &mut params);
    let span = node_span(program, ctx, decl);
    let explicit_arity = Some(params.len() as u32);
    let fn_id = program.symbols.add_function(Function {
        is_weak: !is_static && ctx.has_weak && declaration_is_weak(source, decl),
        target: None,
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
        explicit_arity,
        default_args: shape.defaults,
        owner_unresolved: false,
        variadic: shape.variadic,
        defaulted_in_class: false,
        declared_in_class: false,
        is_virtual: false,
        is_final: false,
        is_cpp: ctx.is_cpp,
        tu: Some(ctx.current_file),
    });
    reassign_fn_id(program, provisional_id, fn_id, provisional_start);
}

fn reassign_fn_id(program: &mut Program, from: FnId, to: FnId, start: (usize, usize)) {
    if from == to {
        return;
    }
    let mut moved: Vec<VarId> = Vec::new();
    // `from` was freshly allocated at `start`: earlier entities cannot refer
    // to it. Header-heavy TUs otherwise rescan the entire imported preamble
    // for every declaration. Keep the ownership check for nested declarations.
    for var in &mut program.symbols.variables[start.0..] {
        if var.fn_id == Some(from) {
            var.fn_id = Some(to);
            moved.push(var.id);
        }
    }
    for cs in &mut program.symbols.call_sites[start.1..] {
        if cs.caller == from {
            cs.caller = to;
        }
    }
    // Re-pointed declarations supply the authoritative parameter list;
    // appending them onto the entry's existing params would double-count
    // prototype + definition parameters as distinct overload slots.
    if !moved.is_empty() {
        if let Some(slot) = program.symbols.function_index(to) {
            program.symbols.functions[slot].params = moved;
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
        // A typedef or `using` alias in a body is scoped to its block like a
        // directive, and never reaches the unit's alias table.
        // One declared in a function-local class belongs to that class.
        "type_definition" | "alias_declaration"
            if node
                .parent()
                .is_none_or(|p| p.kind() != "field_declaration_list") =>
        {
            if let Some(alias) = declared_alias(program, ctx, source, node) {
                ctx.local_aliases.push(alias);
            }
        }
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
                    let call_args = collect_call_args(program, ctx, source, args);
                    // `this` stays unwired; the solver creates an imprecise
                    // summary node for it (sound over-approximation).
                    emit_member_sites(
                        program,
                        caller,
                        &cls,
                        &trace_ir::MethodKind::Ctor,
                        None,
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
                        None,
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
    let local_aliases_len = ctx.local_aliases.len();
    // In C++ a local lives until the end of its block, or of the statement
    // whose condition or init-statement declares it (`if (auto p = f())`,
    // `for (int i = 0; ...)`); an outer variable it hid is back afterwards.
    // C lowering keeps one name per function.
    let is_local_scope = ctx.is_cpp
        && (is_block
            || matches!(
                node.kind(),
                "if_statement"
                    | "for_statement"
                    | "for_range_loop"
                    | "while_statement"
                    | "switch_statement"
                    | "catch_clause"
            ));
    let local_scope_len = ctx.local_scope_log.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_function_body(program, ctx, source, child, caller);
    }
    if is_block {
        ctx.using_nss.truncate(using_nss_len);
        ctx.using_name_imports.truncate(using_imports_len);
        ctx.local_aliases.truncate(local_aliases_len);
    }
    if is_local_scope {
        for (name, shadowed) in ctx.local_scope_log.drain(local_scope_len..).rev() {
            match shadowed {
                Some(outer) => ctx.locals.insert(name, outer),
                None => ctx.locals.remove(&name),
            };
        }
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
        let (op_is_member_access, op_is_arrow) = member_access_op(func);
        if op_is_member_access {
            if let Some(field) = func.child_by_field_name("field") {
                if matches!(
                    field.kind(),
                    "field_identifier" | "destructor_name" | "template_type" | "template_method"
                ) {
                    if let Some((recv, recv_cls)) =
                        func.child_by_field_name("argument").and_then(|recv| {
                            infer_static_class(program, ctx, source, recv).map(|c| (recv, c))
                        })
                    {
                        // `p->m()` looks `m` up on what `p`'s `operator->`
                        // yields, not on `p`'s own class (#64). `r.m()` looks
                        // it up on `r`'s own class, which is already in hand.
                        let cls = if op_is_arrow {
                            let Some(cls) = member_receiver_class(program, ctx, source, recv, true)
                            else {
                                // The wrapper's `operator->` names no class, so
                                // neither does `m`. Record the site unresolved
                                // instead of inventing a member on the wrapper.
                                let call_args = collect_call_args(
                                    program,
                                    ctx,
                                    source,
                                    node.child_by_field_name("arguments"),
                                );
                                emit_unresolved_site(
                                    program,
                                    caller,
                                    field_callee_text(source, func),
                                    recv_cls,
                                    call_args,
                                    span,
                                );
                                return;
                            };
                            cls
                        } else {
                            recv_cls
                        };
                        // The name the index holds the class under. A receiver
                        // spelled with its template arguments — which a class
                        // template with substitution facts keeps — would
                        // otherwise miss every member it declares.
                        let cls = receiver_lookup_name(&cls).into_owned();
                        let kind = if field.kind() == "destructor_name" {
                            trace_ir::MethodKind::Dtor
                        } else {
                            trace_ir::MethodKind::Named(strip_template_args(&normalize_qualified(
                                node_text(source, &field),
                            )))
                        };
                        let targets = member_targets_upward(program, &cls, &kind);
                        if !targets.is_empty() {
                            let call_args = collect_call_args(
                                program,
                                ctx,
                                source,
                                node.child_by_field_name("arguments"),
                            );
                            emit_member_targets(
                                program, caller, &cls, &kind, None, call_args, span, targets,
                            );
                            return;
                        }
                        // Functor field: `h->cb()` where `cb` is a class
                        // with `operator()`, not a method named `cb`.
                        if let Some(field_cls) = infer_static_class(program, ctx, source, func) {
                            let field_cls = receiver_lookup_name(&field_cls).into_owned();
                            let op = trace_ir::MethodKind::Named("operator()".to_string());
                            let field_targets = member_targets_upward(program, &field_cls, &op);
                            if !field_targets.is_empty() {
                                let call_args = collect_call_args(
                                    program,
                                    ctx,
                                    source,
                                    node.child_by_field_name("arguments"),
                                );
                                emit_member_targets(
                                    program,
                                    caller,
                                    &field_cls,
                                    &op,
                                    None,
                                    call_args,
                                    span,
                                    field_targets,
                                );
                                return;
                            }
                        }
                        // Callable data members (`std::function`, fn-ptr
                        // fields) are not methods: fall through to the
                        // generic field-load path so they resolve like
                        // C function pointers. Anything else names no member
                        // of the class, so the site stays unresolved and the
                        // solver synthesizes an external entry for it.
                        let field_name =
                            strip_template_args(&normalize_qualified(node_text(source, &field)));
                        if !class_has_data_field(program, &cls, &field_name) {
                            let call_args = collect_call_args(
                                program,
                                ctx,
                                source,
                                node.child_by_field_name("arguments"),
                            );
                            emit_unresolved_site(
                                program,
                                caller,
                                kind.name_on(&cls),
                                cls,
                                call_args,
                                span,
                            );
                            return;
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
            let cls = receiver_lookup_name(&cls).into_owned();
            let short = strip_template_args(&normalize_qualified(node_text(source, &func)));
            if lookup_var(ctx, program, &short).is_none() {
                let kind = trace_ir::MethodKind::Named(short);
                let targets = member_targets_upward(program, &cls, &kind);
                if !targets.is_empty() {
                    let call_args = collect_call_args(
                        program,
                        ctx,
                        source,
                        node.child_by_field_name("arguments"),
                    );
                    emit_member_targets(
                        program, caller, &cls, &kind, None, call_args, span, targets,
                    );
                    return;
                }
            }
        }
    }
    if ctx.is_cpp && matches!(func.kind(), "identifier" | "qualified_identifier") {
        let spelled = normalize_qualified(node_text(source, &func));
        let target = match lookup_var(ctx, program, &spelled) {
            // `T(args)` where `T` names a class with a declared constructor
            // constructs one. Inside a member class the outer class is no
            // longer the implicit `this`, which is what used to catch
            // `Outer(*this)` in a nested builder (#92).
            None => constructed_class(program, ctx, &spelled)
                .map(|cls| (cls, trace_ir::MethodKind::Ctor)),
            // Functor / callable object: `f()` where `f` has `operator()`.
            Some(v) if func.kind() == "identifier" => var_static_class(program, v)
                .map(|cls| (cls, trace_ir::MethodKind::Named("operator()".to_string())))
                .filter(|(cls, kind)| !member_targets_upward(program, cls, kind).is_empty()),
            Some(_) => None,
        };
        if let Some((cls, kind)) = target {
            let call_args =
                collect_call_args(program, ctx, source, node.child_by_field_name("arguments"));
            emit_member_sites(program, caller, &cls, &kind, None, call_args, span);
            return;
        }
    }

    let (mut callee_name, mut is_direct, callee_var) =
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
        is_direct =
            resolve_function_named(program, ctx, global_lookup_name(&callee_name)).is_some();
    }
    if !is_direct && is_likely_macro_callee(&callee_name) {
        return;
    }
    let mut args = collect_call_args(program, ctx, source, node.child_by_field_name("arguments"));
    let argc = args.argc as usize;
    // Only ranking reads the types; the sites below need the positions.
    let arg_desc = std::mem::take(&mut args.arg_desc);

    // ---- Resolution ----
    // C preserves the exact legacy semantics: one scoped lookup, zero or
    // one target. C++ resolves over the candidate set with:
    //   * unqualified identifiers: namespace-aware lookup — ordinary lookup
    //     (global, enclosing namespaces, `using namespace` directives)
    //     merged with ADL namespaces drawn from the argument types, so
    //     `std::swap(a, b)` and cross-namespace free functions resolve;
    //   * qualified names (`ns::f`): enclosing-scope lookup, with `::`
    //     restricting lookup to the global scope.
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
        let candidates = cpp_callee_candidates(program, ctx, func, &callee_name, &arg_desc);
        // A qualified method call (`Base::m(a)`, `Cls::Static(a)`) lands here
        // too: its `this` is not one of the arguments.
        let by_arity = filter_targets_by_argc(program, candidates, argc, &arg_desc, argc == 0);
        let ranked = if by_arity.len() > 1 {
            rank_overloads(program, &by_arity, &arg_desc)
        } else {
            by_arity
        };
        if ranked.len() > 1
            && ranked
                .iter()
                .any(|&f| program.symbols.function(f).is_defined)
        {
            ranked
                .iter()
                .copied()
                .filter(|&cand| {
                    let fc = program.symbols.function(cand);
                    if fc.is_defined {
                        return true;
                    }
                    !ranked.iter().any(|&other| {
                        other != cand
                            && program.symbols.function(other).is_defined
                            && has_same_signature(program, cand, other)
                    })
                })
                .collect()
        } else {
            ranked
        }
    } else {
        Vec::new()
    };

    if chosen.is_empty() {
        let call_id = program.symbols.alloc_call_id();
        program.symbols.call_sites.push(CallSite {
            id: call_id,
            caller,
            callee_name,
            callee_var,
            callee_fn_id: None,
            var_args: args.var_args,
            fn_args: args.fn_args,
            addr_of_member_args: args.addr_of_member_args,
            args_bound_past_this: false,
            span,
            is_direct,
            receiver_class: None,
            return_dst,
            tu: Some(ctx.current_file),
        });
        return;
    }
    // An overload tie (same arity, types undecidable here) emits one site per
    // candidate — a bounded, explicit may-approximation. Argument positions
    // are the callee's parameter positions, and parameter 0 of a member
    // function reached by its qualified name is `this`.
    for (i, &t) in chosen.iter().enumerate() {
        let is_last = i + 1 == chosen.len();
        let site_args = args.take_for(is_last);
        let bound = argc > 0 && ctx.is_cpp && is_member_function(program, t);
        let site_args = if bound {
            site_args.bind_past_this(None)
        } else {
            site_args
        };
        let call_id = program.symbols.alloc_call_id();
        program.symbols.call_sites.push(CallSite {
            id: call_id,
            caller,
            callee_name: if t != chosen[0] {
                format!("{}::{}", callee_name, program.symbols.function(t).id.0)
            } else if is_last {
                std::mem::take(&mut callee_name)
            } else {
                callee_name.clone()
            },
            callee_var,
            callee_fn_id: Some(t),
            var_args: site_args.var_args,
            fn_args: site_args.fn_args,
            addr_of_member_args: site_args.addr_of_member_args,
            args_bound_past_this: bound,
            span,
            is_direct: true,
            receiver_class: None,
            return_dst,
            tu: Some(ctx.current_file),
        });
    }
}

/// Collected call arguments: value args as `(index, var)`, function
/// arguments (address-of-function) as `(index, fn)`, positions recorded
/// as `&base.member` addresses, and the syntactic argument count
/// (literals included — needed for arity filtering).
#[derive(Clone)]
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
    /// Bind the explicit arguments to a member function's parameters: one
    /// position on, past the implicit `this` it takes as parameter 0, which
    /// is `receiver` when the caller has the object (a construction does).
    fn bind_past_this(mut self, receiver: Option<VarId>) -> Self {
        shift_past_this(
            &mut self.var_args,
            &mut self.fn_args,
            &mut self.addr_of_member_args,
        );
        if let Some(receiver) = receiver {
            self.var_args.insert(0, (0, receiver));
        }
        self
    }

    fn empty() -> Self {
        Self {
            var_args: Vec::new(),
            fn_args: Vec::new(),
            addr_of_member_args: Vec::new(),
            argc: 0,
            arg_desc: Vec::new(),
        }
    }

    /// One argument list for each of several sites emitted from one call: the
    /// last site takes it, the ones before it copy.
    fn take_for(&mut self, last: bool) -> Self {
        if last {
            std::mem::replace(self, Self::empty())
        } else {
            self.clone()
        }
    }
}

/// Move argument positions one on, past the implicit `this` a member function
/// takes as parameter 0.
fn shift_past_this(
    var_args: &mut [(u32, VarId)],
    fn_args: &mut [(u32, FnId)],
    addr_of_member_args: &mut [u32],
) {
    for (index, _) in var_args {
        *index += 1;
    }
    for (index, _) in fn_args {
        *index += 1;
    }
    for index in addr_of_member_args {
        *index += 1;
    }
}

/// Whether parameter 0 of `fid` is the implicit `this` lowering prepends to
/// a member function's definition (static members included).
fn has_this_param(program: &Program, fid: FnId) -> bool {
    program.symbols.has_this_param(fid)
}

/// Whether `fid` is a member function. An in-class prototype carries no
/// parameters, yet merges into a definition whose parameter 0 is `this`, so
/// a parameterless entry is a member when it was declared in its class.
fn is_member_function(program: &Program, fid: FnId) -> bool {
    let f = program.symbols.function(fid);
    if f.params.is_empty() {
        f.is_cpp && f.declared_in_class
    } else {
        has_this_param(program, fid)
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
            if !matches!(arg.kind(), "(" | ")" | "{" | "}" | ",") {
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
    // A parenthesized cast is the same argument as a bare one, and
    // `known_arg_type` peels before it answers. Reading the kind off the
    // unpeeled node ranked `f((T)x)` and `f(((T)x))` differently.
    let node = peel_expression(node);
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
        _ => known_arg_type(program, ctx, source, node),
    }
}

fn known_arg_type(program: &Program, ctx: &LowerContext, source: &str, node: Node) -> TypeDesc {
    let node = peel_expression(node);
    match node.kind() {
        // A cast's operand type is not the type of the expression. Probing
        // a receiver cannot register a new type, so keep it unknown.
        "cast_expression" => TypeDesc::Unknown,
        "number_literal" => number_literal_desc(node_text(source, &node)),
        "char_literal" => TypeDesc::Char,
        "true" | "false" => TypeDesc::Bool,
        "string_literal" => TypeDesc::Ptr(Box::new(TypeDesc::Char)),
        "nullptr" => TypeDesc::Ptr(Box::new(TypeDesc::Unknown)),
        _ => {
            if let Some(v) = resolve_expr_var(program, ctx, source, node) {
                let tid = program.symbols.variable(v).type_id;
                let desc = program.types.get(tid).desc.as_ref().clone();
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
            .skip(usize::from(has_this_param(program, f)))
            .enumerate()
            .map(|(i, pv)| {
                let pdesc = program
                    .types
                    .get(program.symbols.variable(*pv).type_id)
                    .desc
                    .as_ref()
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
/// identifiers (`ns::f`) and template/field/pointer callees do not use ADL.
///
/// `template_function` is included because `resolve_callee` already strips
/// the `<...>` argument list, so the `callee_name` passed to
/// `resolve_cpp_name_candidates` is the bare base name (e.g. `GetNumber`,
/// not `GetNumber<int>`), which correctly indexes into the `base_by_name`
/// bucket.
fn is_bare_callee_node(func: Node, name: &str) -> bool {
    matches!(func.kind(), "identifier" | "template_function") && !name.contains("::")
}

fn has_same_signature(program: &Program, a: FnId, b: FnId) -> bool {
    let fa = program.symbols.function(a);
    let fb = program.symbols.function(b);
    if fa.variadic != fb.variadic {
        return false;
    }
    let a_skip = usize::from(has_this_param(program, a));
    let b_skip = usize::from(has_this_param(program, b));
    let a_params = &fa.params[a_skip..];
    let b_params = &fb.params[b_skip..];
    if a_params.is_empty() || b_params.is_empty() {
        let a_arity = fa.explicit_arity.or(Some(a_params.len() as u32));
        let b_arity = fb.explicit_arity.or(Some(b_params.len() as u32));
        return a_arity.zip(b_arity).is_none_or(|(ea, eb)| ea == eb);
    }
    if a_params.len() != b_params.len() {
        return false;
    }
    let param_t = |f: &Function, skip: usize, idx: usize| -> Option<trace_ir::TypeId> {
        f.param_type_ids.get(skip + idx).copied().or_else(|| {
            f.params
                .get(skip + idx)
                .map(|&v| program.symbols.variable(v).type_id)
        })
    };
    for i in 0..a_params.len() {
        match (param_t(fa, a_skip, i), param_t(fb, b_skip, i)) {
            (Some(ta), Some(tb)) => {
                if !trace_ir::same_param_type(&program.types, ta, tb) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// The functions a C++ callee `func`, spelled `name`, may call: ordinary and
/// argument-dependent lookup for a bare name, enclosing-scope lookup for a
/// qualified name (global-only when it starts with `::`).
fn cpp_callee_candidates(
    program: &Program,
    ctx: &LowerContext,
    func: Node,
    name: &str,
    arg_desc: &[TypeDesc],
) -> Vec<FnId> {
    if is_bare_callee_node(func, name) {
        resolve_cpp_name_candidates(program, ctx, name, arg_desc)
    } else if func.kind() == "qualified_identifier" || func.kind() == "template_function" {
        let lookup = |candidate: &str| {
            let found = program
                .symbols
                .resolve_function_candidates(candidate, Some(ctx.current_file));
            (!found.is_empty()).then_some(found)
        };
        find_in_scope(program, ctx, name, |candidate, _| lookup(candidate))
            .or_else(|| {
                // A `using namespace` directive makes the namespace's own
                // scopes visible too, so `Service::Write()` under
                // `using namespace app` names `app::Service::Write` — as the
                // bare-name path already resolves `Write()`.
                if name.starts_with("::") {
                    return None;
                }
                // A prototype is as good a hit as a body: lowering sees one
                // unit, and the definition in another merges into the same
                // entry — which is why a definition written under the
                // directive qualifies the same way (`qualify_class_name`).
                ctx.using_nss
                    .iter()
                    .find_map(|ns| lookup(&format!("{ns}::{name}")))
            })
            .unwrap_or_default()
    } else {
        program
            .symbols
            .resolve_function_candidates(global_lookup_name(name), Some(ctx.current_file))
    }
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

/// A member call nothing could be resolved for, spelled the way the source
/// spells it. The `->` kept in the name is the point: the solver refuses to
/// resolve a name holding one (`CallSite::resolves_by_name`), so no callee is invented —
/// and an invented one is indistinguishable downstream from a real call to a
/// function outside the tree (#64). `args` are the explicit arguments, bound
/// past the member's `this`.
fn emit_unresolved_site(
    program: &mut Program,
    caller: FnId,
    callee_name: String,
    receiver_class: String,
    args: CallArgs,
    span: Span,
) {
    let CallArgs {
        var_args,
        fn_args,
        addr_of_member_args,
        argc: _,
        arg_desc: _,
    } = args.bind_past_this(None);
    let call_id = program.symbols.alloc_call_id();
    program.symbols.call_sites.push(CallSite {
        id: call_id,
        caller,
        callee_name,
        callee_var: None,
        callee_fn_id: None,
        var_args,
        fn_args,
        addr_of_member_args,
        args_bound_past_this: true,
        span,
        is_direct: false,
        receiver_class: Some(receiver_class),
        return_dst: None,
        tu: program.symbols.function_by_id(caller).and_then(|f| f.tu),
    });
}

/// Emit call sites for `cls::member` — the override set across derived
/// classes, found by walking up the inheritance chain to the nearest
/// declaring class and expanding its subclasses. `args` are the explicit
/// arguments, bound here past the member's `this`, which is `receiver` when
/// the caller has the object (#93, #94).
fn emit_member_sites(
    program: &mut Program,
    caller: FnId,
    cls: &str,
    kind: &trace_ir::MethodKind,
    receiver: Option<VarId>,
    args: CallArgs,
    span: Span,
) {
    let cls = receiver_lookup_name(cls);
    let targets = member_targets_upward(program, &cls, kind);
    emit_member_targets(program, caller, &cls, kind, receiver, args, span, targets);
}

/// [`emit_member_sites`] for a caller that already probed the override set —
/// telling a method apart from a callable data member needs it, and the probe
/// is the expensive half. `cls` is the lookup name `targets` were found under,
/// so both name the same class.
#[allow(clippy::too_many_arguments)]
fn emit_member_targets(
    program: &mut Program,
    caller: FnId,
    cls: &str,
    kind: &trace_ir::MethodKind,
    receiver: Option<VarId>,
    args: CallArgs,
    span: Span,
    targets: Vec<FnId>,
) {
    let tu = program.symbols.function_by_id(caller).and_then(|f| f.tu);
    let mut args = args.bind_past_this(receiver);
    let argc = args.argc;
    let targets = filter_targets_by_argc(program, targets, argc as usize, &args.arg_desc, true);
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
            var_args: args.var_args,
            fn_args: args.fn_args,
            addr_of_member_args: args.addr_of_member_args,
            args_bound_past_this: true,
            span,
            is_direct: false,
            receiver_class: Some(cls.to_string()),
            return_dst: None,
            tu,
        });
        return;
    }
    let last_index = targets.len() - 1;
    for (index, t) in targets.into_iter().enumerate() {
        let site_args = args.take_for(index == last_index);
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
            var_args: site_args.var_args,
            fn_args: site_args.fn_args,
            addr_of_member_args: site_args.addr_of_member_args,
            args_bound_past_this: true,
            span,
            is_direct: true,
            receiver_class: Some(cls.to_string()),
            return_dst: None,
            tu,
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
                match program.types.get(fl.type_id).desc.as_ref().clone() {
                    TypeDesc::Struct { name, .. } => Some(name),
                    _ => None,
                }
            });
        if let Some(target) = target_cls {
            // `Base(a)` and `m_(a)` hold an argument list, `Base{a}` and
            // `m_{a}` an initializer list; either way `this` is not in it.
            let args = fi
                .children(&mut fi.walk())
                .find(|c| matches!(c.kind(), "argument_list" | "initializer_list"));
            let span = node_span(program, ctx, fi);
            let call_args = collect_call_args(program, ctx, source, args);
            emit_member_sites(
                program,
                caller,
                &target,
                &trace_ir::MethodKind::Ctor,
                None,
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
                return Some(program.types.get(fl.type_id).desc.as_ref().clone());
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

/// Explicit (non-`this`) parameter count: the parameter list when the entry
/// has one, else the count its declaration recorded (an in-class prototype
/// lowers no parameter variables). `None` means nothing records it, so the
/// candidate must be kept.
fn method_explicit_arity(program: &Program, fid: FnId) -> Option<usize> {
    let f = program.symbols.function(fid);
    if f.params.is_empty() {
        return f.explicit_arity.map(|n| n as usize);
    }
    Some(f.params.len() - usize::from(has_this_param(program, fid)))
}

/// Whether `fid` can take arguments of these static types, and the types say
/// so with confidence: every argument's type is known, no pointer binds an
/// arithmetic parameter, and no floating-point value binds a pointer. A class
/// parameter may convert, and a prototype without parameter variables names
/// no types to contradict the arguments.
fn confidently_takes(program: &Program, fid: FnId, arg_desc: &[TypeDesc]) -> bool {
    if arg_desc.iter().any(|arg| matches!(arg, TypeDesc::Unknown)) {
        return false;
    }
    let f = program.symbols.function(fid);
    f.params
        .iter()
        .skip(usize::from(has_this_param(program, fid)))
        .zip(arg_desc)
        .all(|(&param, arg)| {
            let param = &program
                .types
                .get(program.symbols.variable(param).type_id)
                .desc;
            let floating = matches!(arg.scalar_kind(), ScalarKind::Float | ScalarKind::Double);
            !(arg.is_pointer_like() && param.scalar_kind() != ScalarKind::Aggregate
                || floating && param.is_pointer_like())
        })
}

/// Whether two declarations of one explicit arity plausibly declare the same
/// function: their parameter types agree up to the qualification of a class
/// name, a type unknown on either side matching anything. A type name the
/// unit could not resolve lowers as `int`, so `int` matches a class too:
/// `string` under an unresolved `using namespace std` beside `std::string`.
/// An entry without parameter variables names no types.
fn same_signature_loosely(program: &Program, a: FnId, b: FnId) -> bool {
    fn same_type(a: &TypeDesc, b: &TypeDesc) -> bool {
        match (a, b) {
            (TypeDesc::Unknown, _) | (_, TypeDesc::Unknown) => true,
            (TypeDesc::Struct { .. }, TypeDesc::Int) | (TypeDesc::Int, TypeDesc::Struct { .. }) => {
                true
            }
            (TypeDesc::Ptr(a), TypeDesc::Ptr(b)) => same_type(a, b),
            (TypeDesc::Struct { name: a, .. }, TypeDesc::Struct { name: b, .. }) => {
                last_type_segment(a) == last_type_segment(b)
            }
            _ => a == b,
        }
    }
    let explicit = |f: FnId| {
        let params = &program.symbols.function(f).params;
        &params[usize::from(has_this_param(program, f))..]
    };
    let (pa, pb) = (explicit(a), explicit(b));
    pa.is_empty()
        || pb.is_empty()
        || pa.iter().zip(pb).all(|(&x, &y)| {
            same_type(
                program
                    .types
                    .get(program.symbols.variable(x).type_id)
                    .desc
                    .as_ref(),
                program
                    .types
                    .get(program.symbols.variable(y).type_id)
                    .desc
                    .as_ref(),
            )
        })
}

fn arity_compatible(expected: Option<usize>, got: Option<usize>) -> bool {
    match (expected, got) {
        (None, _) | (_, None) => true,
        (Some(a), Some(b)) => a == b,
    }
}

/// Whether `f`, declaring `declared` explicit parameters, takes `argc`
/// arguments on its own defaults and variadic tail.
fn arity_takes(f: &Function, declared: usize, argc: usize) -> bool {
    match declared.cmp(&argc) {
        std::cmp::Ordering::Equal => true,
        std::cmp::Ordering::Less => f.variadic,
        std::cmp::Ordering::Greater => argc + f.default_args as usize >= declared,
    }
}

/// The targets a call passing `argc` arguments fits: no more arguments than a
/// target declares, and no fewer than its parameters without a default. A
/// target whose arity nothing records stays when `keep_unknown` says so. A
/// variadic target (`f(void*, ...)`) takes more arguments than it declares,
/// but C++ ranks it below any fixed one that can take the call, so it is
/// dropped when a fixed target confidently can ([`confidently_takes`]):
/// `Log(p, p)` reaches `Log(void*, ...)` beside `Log(int, int)`, a same-named
/// pack template does not displace a fitting fixed member, and an argument of
/// unknown type keeps both. When none fits, all stay.
///
/// C++ writes default arguments on the declaration, so a target also takes the
/// defaults of a same-arity target of the same signature
/// ([`same_signature_loosely`]): a definition the index could not reunite with
/// its prototype (`string` beside `std::string`) still accepts `TrimStr(s)`
/// for `TrimStr(const std::string&, char = ' ')`, while `Format(cb, cb)` does
/// not borrow `Format(int, int = 0)`'s.
fn filter_targets_by_argc(
    program: &Program,
    targets: Vec<FnId>,
    argc: usize,
    arg_desc: &[TypeDesc],
    keep_unknown: bool,
) -> Vec<FnId> {
    let arities: Vec<Option<usize>> = targets
        .iter()
        .map(|&f| method_explicit_arity(program, f))
        .collect();
    let defaults = |f: FnId| program.symbols.function(f).default_args as usize;
    let fits = |f: FnId, arity: Option<usize>| match arity {
        Some(declared) if arity_takes(program.symbols.function(f), declared, argc) => true,
        Some(declared) => {
            declared > argc
                && targets.iter().zip(&arities).any(|(&other, &other_arity)| {
                    other_arity == Some(declared)
                        && argc + defaults(other) >= declared
                        && same_signature_loosely(program, f, other)
                })
        }
        None => keep_unknown,
    };
    let mut by_arity: Vec<FnId> = targets
        .iter()
        .zip(&arities)
        .filter(|&(&f, &arity)| fits(f, arity))
        .map(|(&f, _)| f)
        .collect();
    let variadic = |f: FnId| program.symbols.function(f).variadic;
    if by_arity.iter().any(|&f| variadic(f))
        && by_arity
            .iter()
            .any(|&f| !variadic(f) && confidently_takes(program, f, arg_desc))
    {
        by_arity.retain(|&f| !variadic(f));
    }
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
    let provisional_start = (
        program.symbols.variables.len(),
        program.symbols.call_sites.len(),
    );
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
            if is_parameter_node(param.kind()) {
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
        is_weak: false,
        target: None,
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
        explicit_arity: Some(params.len() as u32),
        default_args: 0,
        owner_unresolved: false,
        variadic: false,
        defaulted_in_class: false,
        declared_in_class: false,
        is_virtual: false,
        is_final: false,
        is_cpp: true,
        tu: Some(ctx.current_file),
    });
    reassign_fn_id(program, provisional_id, fn_id, provisional_start);
    let saved_fn = ctx.current_fn;
    let saved_locals = ctx.locals.clone();
    let saved_class = ctx.class_ctx.clone();

    let mut captures_this = false;
    let mut default_capture_all = false;
    let mut captured_vars: Vec<String> = Vec::new();
    let mut init_captures: Vec<(String, bool, Node, Node)> = Vec::new();

    if let Some(cap_node) = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "lambda_capture_specifier")
    {
        for child in cap_node.children(&mut cap_node.walk()) {
            match child.kind() {
                "this" => captures_this = true,
                "lambda_default_capture" => {
                    default_capture_all = true;
                    if saved_class.is_some() || saved_locals.contains_key("this") {
                        captures_this = true;
                    }
                }
                "identifier" => {
                    let name = node_text(source, &child).to_string();
                    captured_vars.push(name);
                }
                "lambda_capture_initializer" => {
                    if let (Some(left), Some(right)) = (
                        child.child_by_field_name("left"),
                        child.child_by_field_name("right"),
                    ) {
                        let is_ref = node_text(source, &child).trim_start().starts_with('&');
                        let name = node_text(source, &left).trim().to_string();
                        init_captures.push((name, is_ref, left, right));
                    }
                }
                _ => {}
            }
        }
    }

    let mut lambda_locals: HashMap<String, VarId> = HashMap::default();

    if default_capture_all {
        for (name, &var_id) in &saved_locals {
            if name != "this" {
                lambda_locals.insert(name.clone(), var_id);
            }
        }
    } else {
        for name in captured_vars {
            if let Some(&var_id) = saved_locals.get(&name) {
                lambda_locals.insert(name, var_id);
            }
        }
    }

    for (name, is_ref, left, right) in init_captures {
        if let Some(caller) = saved_fn {
            walk_function_body(program, ctx, source, right, caller);
        }
        let right_text = node_text(source, &right).trim();
        if is_ref && right.kind() == "identifier" {
            let orig_var = saved_locals
                .get(right_text)
                .copied()
                .or_else(|| lookup_var(ctx, program, right_text));
            if let Some(orig_var) = orig_var {
                lambda_locals.insert(name, orig_var);
                continue;
            }
        }
        let var_id = program.symbols.alloc_var_id();
        let span = node_span(program, ctx, left);
        let type_id = infer_static_class(program, ctx, source, right)
            .map(|cls| {
                program
                    .types
                    .intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
                        name: cls,
                        fields: Vec::new(),
                    })))
            })
            .unwrap_or_else(|| program.types.int());
        let is_ptr = matches!(program.types.get(type_id).desc.as_ref(), TypeDesc::Ptr(_));
        program.symbols.add_variable(Variable {
            is_defined: false,
            is_weak: false,
            target: None,
            is_namespaced: false,
            id: var_id,
            name: name.clone(),
            type_id,
            storage: StorageClass::Local,
            fn_id: Some(fn_id),
            param_index: None,
            span,
            is_pointer: is_ptr,
        });
        extract_flow_from_expr(program, ctx, source, right, Some(var_id));
        lambda_locals.insert(name, var_id);
    }

    ctx.class_ctx = saved_class.clone();
    if captures_this {
        if let Some(&this_var) = saved_locals.get("this") {
            lambda_locals.insert("this".to_string(), this_var);
        }
    }

    for &param in &params {
        if let Some(v) = program.symbols.variable_by_id(param) {
            lambda_locals.insert(v.name.clone(), param);
        }
    }

    ctx.current_fn = Some(fn_id);
    ctx.locals = lambda_locals;
    if let Some(body) = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "compound_statement")
    {
        walk_function_body(program, ctx, source, body, fn_id);
    }
    ctx.current_fn = saved_fn;
    ctx.locals = saved_locals;
    ctx.class_ctx = saved_class;
    Some(fn_id)
}

/// Walk UP the inheritance chain from `cls` until some class declares the
/// member. Non-virtual declarations (and ctors) resolve to exactly the
/// declaring entries; `virtual` ones — and destructors, where
/// delete-through-base is the dominant pattern — expand downward through
/// the subclass closure as the dynamic-dispatch target set.
fn member_targets_upward(program: &Program, cls: &str, kind: &trace_ir::MethodKind) -> Vec<FnId> {
    let own = declared_members_upward(program, cls, kind);
    // Only a virtual call expands to subclasses. A constructor not in view
    // stays unresolved; the closure made `: Base(a)` reach the derived
    // constructor it is written in.
    if own.is_empty() && !matches!(kind, trace_ir::MethodKind::Ctor) {
        return program.method_targets(cls, kind);
    }
    let virtual_dispatch =
        kind.is_destructor() || own.iter().any(|t| program.symbols.function(*t).is_virtual);
    if virtual_dispatch {
        // Expand from the *static* type so `final` classes/methods cut off
        // sibling and descendant overrides.
        program.method_targets(cls, kind)
    } else {
        own
    }
}

/// The entries the nearest declaring class up the inheritance chain has for
/// `kind` — the lookup half of [`member_targets_upward`], without the
/// downward subclass expansion it falls back to on a miss. Callers that ask
/// about a member of the receiver's own class (`operator->`) want only this:
/// the closure walk is the expensive half and answers a different question.
fn declared_members_upward(program: &Program, cls: &str, kind: &trace_ir::MethodKind) -> Vec<FnId> {
    // Most receivers either declare the member or have no indexed bases.
    // Avoid allocating a traversal queue and visited names for those cases.
    let own = program.symbols.functions_named(&kind.name_on(cls));
    if !own.is_empty() || !program.has_bases(cls) {
        return own;
    }
    let mut queue = std::collections::VecDeque::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    queue.push_back(cls.to_string());
    seen.insert(cls.to_string());
    while let Some(cur) = queue.pop_front() {
        let own = program.symbols.functions_named(&kind.name_on(&cur));
        if !own.is_empty() {
            return own;
        }
        for base in program.bases_of(&cur) {
            if seen.insert(base.clone()) {
                queue.push_back(base);
            }
        }
    }
    Vec::new()
}

/// Longest `operator->` chain followed before giving up. Real wrappers nest
/// one deep, occasionally two; the cap only bounds a chain that a partial
/// index has made circular.
const MAX_ARROW_DEPTH: usize = 8;

/// The standard smart pointers, taken on their name alone when their class is
/// not in the index to be asked — the common case, their header being outside
/// the tree. A guess from a name, not a resolution; every other wrapper is
/// recognised by the `operator->` it declares (#64).
fn is_std_smart_ptr_name(cls: &str) -> bool {
    matches!(
        last_type_segment(cls),
        "shared_ptr" | "unique_ptr" | "weak_ptr"
    )
}

/// A template instantiation whose class body is not in the tree -- absent,
/// or only forward-declared -- and so a candidate for the argument guess
/// (#86). A nested type of one (`Outer<A>::Inner`, an iterator) is a type
/// of its own, not a wrapper around `A`.
fn is_undefined_wrapper(program: &Program, name: &str, cls: &str) -> bool {
    name.contains('<')
        && !program.types.is_struct_defined(cls)
        && template_tail(name).trim().is_empty()
}

/// Whether `cls` declares `operator->`, in this unit's symbols or in a fact
/// merged from a header unit (see [`register_arrow_return`]).
fn declares_arrow(program: &Program, cls: &str) -> bool {
    program.arrow_returns.iter().any(|f| f.class_name == cls)
        || !declared_members_upward(
            program,
            cls,
            &trace_ir::MethodKind::Named("operator->".into()),
        )
        .is_empty()
}

/// The class a `Struct` tag is looked up under. A wrapper instantiation
/// keeps its arguments in its name (`sptr<CaptureSession>`) so that a `->` on
/// it can substitute them; the class itself is spelled without them, whether
/// the wrapper is declared or not (#86).
/// Borrowed when the spelling already is that name, which is the common case
/// on the member-call path: [`strip_template_args`] takes the same fast path
/// but always allocates.
fn receiver_lookup_name(name: &str) -> Cow<'_, str> {
    let trimmed = name.trim_end();
    if trimmed.ends_with('>') {
        Cow::Owned(strip_template_args(trimmed))
    } else {
        Cow::Borrowed(trimmed)
    }
}

/// `name` when the index declares a class by it, else the class a `typedef`
/// spelled `name` stands for (typedefs register under their bare spelling).
fn declared_class_name(program: &Program, name: &str) -> Option<String> {
    // Tags are registered without the global-scope prefix.
    let name = name.strip_prefix("::").unwrap_or(name);
    if program.types.is_struct_declared(name) {
        return Some(name.to_owned());
    }
    // A typedef is registered under its qualified name as well as its bare
    // one, so the spelling is matched whole: `A::B::T` never lands on an
    // unrelated namespace's `T`.
    let alias = program.types.resolve_alias(name)?;
    match alias {
        TypeDesc::Struct { name, .. } if program.types.is_struct_declared(name) => {
            Some(name.clone())
        }
        _ => None,
    }
}

/// The class `x->m` looks `m` up on, given `x`'s declared type (#64).
///
/// A raw pointer is the built-in arrow: the pointee's class. A class value is
/// the overloaded one: what its declared `operator->` returns, followed again
/// while that is itself a class, up to [`MAX_ARROW_DEPTH`] links with cycles
/// cut. A class declaring no `operator->` is its own receiver.
///
/// `None` where the chain names no class — overloads that disagree, a return
/// type the index cannot name, a cycle. The caller leaves such a site
/// unresolved: a member invented on the wrapper (`sptr::AddOutput`) would be
/// an edge to an undefined external function, indistinguishable downstream
/// from a real call out of tree.
fn resolve_operator_arrow(program: &Program, desc: TypeDesc) -> Option<String> {
    let name = resolve_operator_arrow_receiver(program, desc)?;
    Some(if name.trim_end().ends_with('>') {
        strip_template_args(&name)
    } else {
        name
    })
}

/// The arrow target's full spelling, for template return substitution.
fn resolve_operator_arrow_receiver(program: &Program, desc: TypeDesc) -> Option<String> {
    let mut current = desc;
    let mut seen = HashSet::default();
    for _ in 0..=MAX_ARROW_DEPTH {
        if let TypeDesc::Ptr(inner) = current {
            return class_spelling_of_desc(&inner).map(str::to_string);
        }
        let TypeDesc::Struct { ref name, .. } = current else {
            return None;
        };
        let cls = strip_template_args(name);
        if is_undefined_wrapper(program, name, &cls) && !is_std_smart_ptr_name(&cls) {
            // An undefined wrapper -- absent from the tree, or only
            // forward-declared in it -- unwraps to its sole argument when
            // that names a class the index knows (#86). An out-of-line
            // `operator->` defines the wrapper as it registers its return;
            // one indexed without a readable return type still forbids the
            // guess.
            if program
                .symbols
                .has_function_named(&format!("{cls}::operator->"))
            {
                return None;
            }
            let args = template_arguments(name);
            let [arg] = args.as_slice() else {
                return None;
            };
            let head = declared_class_name(program, &receiver_lookup_name(arg))?;
            return Some(match arg.find('<') {
                Some(at) => format!("{head}{}", &arg[at..]),
                None => head,
            });
        }
        if !seen.insert(name.clone()) {
            return None;
        }
        let args = template_arguments(name);
        let kind = trace_ir::MethodKind::Named("operator->".into());
        let ops = declared_members_upward(program, &cls, &kind);
        // The facts for the class that declares the operator — `cls` itself
        // or the base the lookup stopped at.
        let owners: Vec<_> = ops
            .iter()
            .filter_map(|id| {
                program
                    .symbols
                    .function(*id)
                    .name
                    .strip_suffix("::operator->")
            })
            .collect();
        let facts: Vec<_> = program
            .arrow_returns
            .iter()
            .filter(|f| f.class_name == cls || owners.contains(&f.class_name.as_str()))
            .collect();
        if facts.is_empty() {
            if ops.is_empty() {
                // Preserve the existing standard-library fallback, whose
                // pointee declaration may also live outside the tree.
                if is_std_smart_ptr_name(&cls) {
                    return args.first().map(|s| sanitize_type_name(s));
                }
                // A nested type of a template whose body is not in the tree
                // (`Outer<A>::Inner`, an iterator) has nothing to look the
                // member up on: neither a guess nor a member invented on it.
                if name.contains('<') && !program.types.is_struct_defined(&cls) {
                    return None;
                }
                return Some(name.clone());
            }
            // Declared, but with a return type nothing recorded.
            return None;
        }
        // Overloads (`T *operator->()` and its `const` twin) must agree: the
        // pointee of a wrapper is one class, so disagreement means the lookup
        // found two different members and choosing one would invent an edge.
        let mut next = None;
        for fact in facts {
            let mut target = if let Some(index) = fact.parameter {
                // A parameter is a position in the declaring template's own
                // list. Declared on a base (`Derived<X> : Base<Y>`), it
                // indexes the base's arguments, which are not `args`.
                if fact.class_name != cls {
                    return None;
                }
                TypeDesc::Struct {
                    name: args.get(index)?.clone(),
                    fields: Vec::new(),
                }
            } else {
                fact.target.clone()
            };
            if fact.pointer {
                target = TypeDesc::Ptr(Box::new(target));
            }
            if next.as_ref().is_some_and(|first| first != &target) {
                return None;
            }
            next = Some(target);
        }
        current = next?;
    }
    None
}

/// The class a member access on `recv` looks the member up on: `recv`'s own
/// class for `.`, what its arrow yields for `->` (see
/// [`resolve_operator_arrow`]).
fn member_receiver_class(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    recv: Node,
    arrow: bool,
) -> Option<String> {
    if !arrow {
        return infer_static_class(program, ctx, source, recv);
    }
    resolve_operator_arrow(program, receiver_desc(program, ctx, source, recv)?)
}

/// What `*e` yields. A pointer dereferences to its pointee. A smart pointer
/// dereferences through its `operator->`: its `operator*` names the same
/// pointee, so `(*sp).m()` looks `m` up where `sp->m()` does. Any other
/// class value has an `operator*` this index does not follow — an iterator's
/// yields whatever the container holds — so the result is unknown, rather
/// than the class itself with a member invented on it
/// (`std::vector::iterator::GetCameras`).
fn deref_desc(program: &Program, desc: TypeDesc) -> Option<TypeDesc> {
    match desc {
        TypeDesc::Ptr(inner) => Some(*inner),
        TypeDesc::Struct { ref name, .. } => {
            let cls = strip_template_args(name);
            // A wrapper whose body is not in the tree unwraps on `*` as it
            // does on `->` (#86): `resolve_operator_arrow` holds the one
            // guess, so `(*sp).m()` and `sp->m()` agree.
            if !(is_undefined_wrapper(program, name, &cls)
                || declares_arrow(program, &cls)
                || is_std_smart_ptr_name(&cls))
            {
                return None;
            }
            resolve_operator_arrow(program, desc).map(|name| TypeDesc::Struct {
                name,
                fields: Vec::new(),
            })
        }
        _ => None,
    }
}

/// The receiver's declared type with its pointer layers intact — unlike
/// [`infer_static_class`], which peels them — so that `->` can tell a raw
/// pointer (built-in arrow, the pointee's members) from a class value
/// (overloaded arrow, whatever `operator->` yields). A reference lowers as a
/// pointer everywhere else; here it is the value it refers to.
fn receiver_desc(
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<TypeDesc> {
    let node = peel_expression(node);
    match node.kind() {
        "this" => Some(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: ctx.class_ctx.as_ref()?.qual_name.clone(),
            fields: Vec::new(),
        }))),
        "identifier" => {
            if let Some(v) = lookup_var(ctx, program, node_text(source, &node)) {
                let mut desc = program
                    .types
                    .get(program.symbols.variable(v).type_id)
                    .desc
                    .as_ref()
                    .clone();
                if ctx.reference_vars.contains(&v) {
                    if let TypeDesc::Ptr(inner) = desc {
                        desc = *inner;
                    }
                }
                return Some(desc);
            }
            class_field_desc(
                program,
                &ctx.class_ctx.as_ref()?.qual_name,
                node_text(source, &node),
            )
        }
        "field_expression" => {
            let base = node.child_by_field_name("argument")?;
            let cls = member_receiver_class(program, ctx, source, base, is_arrow_access(node))?;
            class_field_desc(
                program,
                &cls,
                node_text(source, &node.child_by_field_name("field")?),
            )
        }
        "pointer_expression" => {
            let desc = receiver_desc(program, ctx, source, node.named_child(0)?)?;
            match pointer_op(source, node).as_deref() {
                Some("*") => deref_desc(program, desc),
                Some("&") => Some(TypeDesc::Ptr(Box::new(desc))),
                _ => None,
            }
        }
        "call_expression" => {
            if let Some(cached) = ctx.call_receiver_cache.borrow().get(&node.id()) {
                return Some(cached.clone());
            }
            let desc = match call_result_shape(program, ctx, source, node) {
                Some(CallResult::Decided(desc)) => desc,
                Some(CallResult::Overload {
                    candidates,
                    receiver,
                }) => {
                    let args: Vec<TypeDesc> = call_arg_nodes(node)
                        .into_iter()
                        .map(|arg| known_arg_type(program, ctx, source, arg))
                        .collect();
                    call_result_among(program, ctx, source, node, candidates, receiver, &args)
                }
                None => None,
            };
            // Only a hit is memoized. A miss can be the index being
            // incomplete at probe time — lowering keeps registering types and
            // substitution facts as it walks — so it is asked again.
            if let Some(desc) = &desc {
                ctx.call_receiver_cache
                    .borrow_mut()
                    .insert(node.id(), desc.clone());
            }
            desc
        }
        "new_expression" => Some(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: new_expression_class(program, ctx, source, node)?,
            fields: Vec::new(),
        }))),
        _ => infer_static_class(program, ctx, source, node).map(|name| TypeDesc::Struct {
            name,
            fields: Vec::new(),
        }),
    }
}

/// Record what `cls::operator->` returns, for the call sites that follow it
/// (#64). It is kept as a fact of its own rather than read back from the
/// function's return type because a header unit reaches the units that
/// include it as *types only*: a wrapper-typed field declared in a different
/// header from the wrapper is lowered while the wrapper's members are not in
/// that unit's symbol table, and the facts are merged alongside the types.
///
/// Inside a class template the declared type may be a parameter: `T *` is
/// recorded by `T`'s position, for the call site to substitute from the
/// instantiation's arguments. A type that merely mentions a parameter
/// (`sptr<T>`) is recorded as unknown — never guessed to be argument zero.
fn register_arrow_return(
    program: &mut Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
    cls: &str,
) {
    // An out-of-line operator also proves that this is a defined wrapper.
    program.types.define_struct(cls);
    let Some(t) = node.child_by_field_name("type") else {
        return;
    };
    let spelling = node_text(source, &t).trim();
    let mut ancestor = node.parent();
    let mut parameter = None;
    let mut dependent = false;
    while let Some(parent) = ancestor {
        if parent.kind() == "template_declaration" {
            if let Some(params) = parent.child_by_field_name("parameters") {
                // Positions must line up with the instantiation's argument
                // list, which template_parameter_names keeps comments out of.
                for (i, name) in template_parameter_names(source, params)
                    .into_iter()
                    .enumerate()
                {
                    if name == Some(spelling) {
                        parameter = Some(i);
                    }
                    dependent |= name
                        .is_some_and(|name| spelling_mentions(spelling, Spelling::Source, name));
                }
            }
            break;
        }
        ancestor = parent.parent();
    }
    let mut target = if dependent && parameter.is_none() {
        TypeDesc::Unknown
    } else {
        type_desc_from_node(program, ctx, source, t)
    };
    let pointer = node
        .child_by_field_name("declarator")
        .is_some_and(|d| d.kind() == "pointer_declarator");
    // A named pointer typedef already includes its terminating pointer layer.
    let pointer = if let TypeDesc::Ptr(inner) = target {
        target = *inner;
        true
    } else {
        pointer
    };
    let fact = trace_ir::ArrowReturn {
        class_name: cls.to_owned(),
        target,
        parameter,
        pointer,
    };
    if !program.arrow_returns.contains(&fact) {
        program.arrow_returns.push(fact);
    }
}

/// The top-level arguments of a template spelling: `W<A, B<C>, D>` yields
/// `A`, `B<C>` and `D`; a spelling without `<` yields nothing.
/// Arguments split at the top level of `raw`'s first list, nesting and a
/// function type's own parameter list respected. Quoting is not tracked: a
/// C++20 structural argument spelled as a string literal with a comma in it
/// (`Tag<"a, b">`) would split there. No such spelling occurs in the corpora.
fn template_arguments(raw: &str) -> Vec<String> {
    let Some(start) = raw.find('<') else {
        return Vec::new();
    };
    let mut depth = 0;
    let mut paren = 0;
    let mut from = start + 1;
    let mut args = Vec::new();
    for (i, c) in raw.char_indices().skip_while(|(i, _)| *i <= start) {
        match c {
            // A function type's parameter list (`Callback<void(int, char)>`)
            // has commas of its own.
            '(' => paren += 1,
            ')' if paren > 0 => paren -= 1,
            _ if paren > 0 => {}
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            ',' | '>' if depth == 0 => {
                let arg = raw[from..i].trim();
                // `W<>` has no arguments. Pushing the empty slice would make
                // it a one-argument spelling, which the wrapper rule then
                // qualifies to a bare `ns::` and looks up as a class.
                if !(arg.is_empty() && args.is_empty() && c == '>') {
                    args.push(arg.to_owned());
                }
                from = i + 1;
                if c == '>' {
                    break;
                }
            }
            _ => {}
        }
    }
    args
}

/// What follows a spelling's first template argument list: `::Inner` for
/// `Outer<A>::Inner`, empty for `Outer<A>`, for a spelling without
/// arguments and for an unbalanced one.
fn template_tail(raw: &str) -> &str {
    let Some(start) = raw.find('<') else {
        return "";
    };
    let mut depth = 0i32;
    let mut paren = 0i32;
    for (i, c) in raw[start..].char_indices() {
        match c {
            '(' => paren += 1,
            ')' if paren > 0 => paren -= 1,
            _ if paren > 0 => {}
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return &raw[start + i + 1..];
                }
            }
            _ => {}
        }
    }
    ""
}

/// A keyword scalar, `auto`, or a standard integer / `nullptr_t` name: never
/// a class, so never qualified to the namespace it is spelled in.
fn is_fundamental_type_name(name: &str) -> bool {
    !name.is_empty()
        && name.split_whitespace().all(|word| {
            matches!(
                word,
                "int"
                    | "char"
                    | "void"
                    | "bool"
                    | "float"
                    | "double"
                    | "short"
                    | "long"
                    | "signed"
                    | "unsigned"
                    | "wchar_t"
                    | "char8_t"
                    | "char16_t"
                    | "char32_t"
                    | "size_t"
                    | "ssize_t"
                    | "ptrdiff_t"
                    | "intptr_t"
                    | "uintptr_t"
                    | "int8_t"
                    | "int16_t"
                    | "int32_t"
                    | "int64_t"
                    | "uint8_t"
                    | "uint16_t"
                    | "uint32_t"
                    | "uint64_t"
                    | "intmax_t"
                    | "uintmax_t"
                    | "nullptr_t"
                    | "auto"
            )
        })
}

/// A template spelling with every class in it qualified to the current
/// scope, so that the argument substituted at a `->` names the class the way
/// the index does: `sptr<Plugin>` inside `namespace ohos` is
/// `ohos::sptr<ohos::Plugin>`.
fn qualify_template_spelling(program: &Program, ctx: &LowerContext, raw: &str) -> String {
    // The wrapper itself is looked up like its arguments: `sptr<T>` inside
    // `namespace OHOS::CameraStandard` is `OHOS::sptr`, whose `operator->`
    // and members it must keep.
    let head = qualify_class_name(
        program,
        ctx,
        &normalize_qualified(type_name_before_template(raw)),
    );
    qualify_template_spelling_under(program, ctx, raw, &head)
}

/// [`qualify_template_spelling`] with its head already settled: the
/// arguments are qualified, and a tail (`::Inner<B>` of `Outer<A>::Inner<B>`)
/// keeps its separator and its own head as written, since a member type of
/// `Outer` is not looked up through the enclosing namespaces.
fn qualify_template_spelling_under(
    program: &Program,
    ctx: &LowerContext,
    raw: &str,
    head: &str,
) -> String {
    let args: Vec<_> = template_arguments(raw)
        .into_iter()
        .map(|arg| {
            let (base, suffix) = split_pointer_suffix(&arg);
            let clean = sanitize_type_name(base);
            let qualified = if clean.contains('(') {
                // A function type (`void(int, char)`) names no class.
                normalize_spacing(&clean)
            } else if clean.contains('<') {
                qualify_template_spelling(program, ctx, &clean)
            } else if is_template_literal(&clean)
                || is_fundamental_type_name(&clean)
                || primitive_scalar_desc(&clean).is_some()
            {
                clean
            } else {
                qualify_class_name(program, ctx, &normalize_qualified(&clean))
            };
            format!("{qualified}{suffix}")
        })
        .collect();
    // `Outer<A>::Inner` is a type of its own: the tail stays on the tag,
    // which is what keeps it from reading as a wrapper around `A`.
    let tail = template_tail(raw).trim();
    let tail = if tail.is_empty() {
        String::new()
    } else if tail.contains('<') {
        let tail_head = normalize_spacing(type_name_before_template(tail));
        qualify_template_spelling_under(program, ctx, tail, &tail_head)
    } else {
        normalize_spacing(tail)
    };
    format!("{head}<{}>{tail}", args.join(","))
}

/// The declared class a class name spelled in the current scope names -- a
/// template head or argument, a base, a `new` or a cast -- looked up through
/// [`find_in_scope`] for a bare spelling and a partially qualified one alike
/// (`CameraStandard::CameraInput` inside `namespace OHOS`), with a typedef
/// standing for the class it names. Where nothing is declared, the spelling
/// qualifies to the innermost namespace as it always did.
///
/// A `using namespace` directive is not searched: a type spelling that names
/// a class only through one is ambiguous with the spelling the index holds,
/// and is left untyped rather than guessed (`receiver_type`). The two places
/// that *name* a class rather than type a value — a base clause, and the
/// owner of an out-of-line member definition — resolve through the directive
/// themselves, as the compiler does (`class_seen_from`).
fn qualify_class_name(program: &Program, ctx: &LowerContext, name: &str) -> String {
    declared_class_in_scope(program, ctx, name).unwrap_or_else(
        // Tags are registered without the global-scope prefix, so a `::T`
        // nothing declares drops it too.
        || match name.strip_prefix("::") {
            Some(global) => global.to_owned(),
            None => qualify_type_name(ctx, name),
        },
    )
}

/// The declared class a name spelled in the current scope stands for: through
/// a function-local alias, else through [`find_in_scope`].
fn declared_class_in_scope(program: &Program, ctx: &LowerContext, name: &str) -> Option<String> {
    if let Some(TypeDesc::Struct { name, .. }) = local_alias(ctx, name) {
        return program.types.is_struct_declared(name).then(|| name.clone());
    }
    find_in_scope(program, ctx, name, |candidate, _| {
        declared_class_name(program, candidate)
    })
}

/// The class tag a `class` / `struct` keyword spelling names, found through
/// [`find_in_scope`]. Only a declared tag answers: a typedef cannot follow the
/// keyword, and the alias a C-compatible member struct gets
/// (`Config::Parser` for `Parser`) would move an out-of-line
/// `struct Config::Parser { ... }` onto the C tag, apart from the definitions
/// of its methods.
fn declared_tag_in_scope(program: &Program, ctx: &LowerContext, name: &str) -> Option<String> {
    find_in_scope(program, ctx, name, |candidate, _| {
        program
            .types
            .is_struct_declared(candidate)
            .then(|| candidate.to_owned())
    })
}

/// The class `name(args)` constructs: the declared class the scope lookup
/// finds, unless a function of that name is declared at the same or a nearer
/// scope and hides it (`void Foo(int)` in `ns` hides a global `struct Foo`).
/// A class's own constructor (`Later::Later`) is its injected name, not a
/// function in the way.
fn constructed_class_in_scope(program: &Program, ctx: &LowerContext, name: &str) -> Option<String> {
    if let Some(TypeDesc::Struct { name, .. }) = local_alias(ctx, name) {
        return program.types.is_struct_declared(name).then(|| name.clone());
    }
    let last = last_type_segment(name);
    find_in_scope(program, ctx, name, |candidate, _| {
        let is_constructor = candidate
            .strip_suffix(last)
            .and_then(|scope| scope.strip_suffix("::"))
            .is_some_and(|scope| last_type_segment(scope) == last);
        if names_function(program, ctx, candidate) && !is_constructor {
            return Some(None);
        }
        declared_class_name(program, candidate).map(Some)
    })
    .flatten()
}

/// The class a call spelled `T(args)` constructs: `T` names a class with a
/// declared constructor. Inside a class whose body is still being lowered, its
/// name can only construct it, and its constructor may be defined below the
/// call; the site resolves by name.
fn constructed_class(program: &Program, ctx: &LowerContext, spelled: &str) -> Option<String> {
    constructed_class_in_scope(program, ctx, spelled).filter(|cls| {
        names_function(program, ctx, &trace_ir::MethodKind::Ctor.name_on(cls))
            || ctx.type_scope.borrow().iter().any(|scope| scope == cls)
    })
}

/// Whether a function is declared under `name` where this unit can see it,
/// internal linkage (`static`, an anonymous namespace) included.
fn names_function(program: &Program, ctx: &LowerContext, name: &str) -> bool {
    program.symbols.has_function_named(name)
        || program
            .symbols
            .resolve_function_in_scope(name, Some(ctx.current_file))
            .is_some()
}

/// How many base classes a type-name lookup asks about per scope; a
/// hierarchy is rarely deeper, and a cyclic one must not loop.
const MAX_BASE_LOOKUP: usize = 32;

/// Look a type name up the way C++ does from the current scope (#90): in the
/// class being lowered, its bases, and each class around it, then in each
/// enclosing namespace, innermost first, and last at the global scope. `probe`
/// is asked about each candidate spelling in turn and the first answer wins;
/// its flag is set for the last, global, candidate. A `::`-prefixed name is
/// asked about at the global scope only.
fn find_in_scope<T>(
    program: &Program,
    ctx: &LowerContext,
    name: &str,
    mut probe: impl FnMut(&str, bool) -> Option<T>,
) -> Option<T> {
    if let Some(global) = name.strip_prefix("::") {
        return probe(global, true);
    }
    // Held across the probes: they only read the type and symbol tables, and
    // a clone of the class names per lookup would allocate on a hot path.
    let classes = ctx.type_scope.borrow();
    // A class's own spelling runs through the namespaces it sits in; those
    // are asked about after every enclosing class, so a class local to a
    // member function (`ns::Local`) reaches the function's class
    // (`ns::Enclosing`) before `ns`.
    let is_namespace = |scope: &str| {
        let mut rest = scope;
        for segment in ctx.ns_stack.iter().flatten() {
            match rest.strip_prefix(segment.as_str()) {
                Some("") => return true,
                Some(tail) if tail.starts_with("::") => rest = &tail[2..],
                _ => return false,
            }
        }
        false
    };
    let mut candidate = String::new();
    let mut ask = |scope: &str| {
        candidate.clear();
        candidate.push_str(scope);
        candidate.push_str("::");
        candidate.push_str(name);
        probe(&candidate, false)
    };
    // Innermost first. A member class's entry spells its outer classes, so
    // a scope an entry further in already covers is not asked about again.
    for (depth, class) in classes.iter().enumerate().rev() {
        let inner = &classes[depth + 1..];
        let mut prefix = class.as_str();
        while !prefix.is_empty()
            && !is_namespace(prefix)
            && !inner.iter().any(|cls| {
                cls.strip_prefix(prefix)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
            })
        {
            if let Some(hit) = ask(prefix) {
                return Some(hit);
            }
            // A class's members include those of its bases, before the scopes
            // around it: `Node` in `struct D : Base` is `Base::Node`.
            let mut bases: Vec<&str> = program.base_names(prefix).collect();
            let mut seen = 0;
            while seen < bases.len() && seen < MAX_BASE_LOOKUP {
                let base = bases[seen];
                if let Some(hit) = ask(base) {
                    return Some(hit);
                }
                bases.extend(program.base_names(base));
                seen += 1;
            }
            prefix = prefix.rfind("::").map_or("", |at| &prefix[..at]);
        }
    }
    let namespaces = ctx.ns_stack.iter().flatten().count();
    for depth in (1..=namespaces).rev() {
        candidate.clear();
        for segment in ctx.ns_stack.iter().flatten().take(depth) {
            candidate.push_str(segment);
            candidate.push_str("::");
        }
        candidate.push_str(name);
        if let Some(hit) = probe(&candidate, false) {
            return Some(hit);
        }
    }
    probe(name, true)
}

/// What a C++ type name spelled in the current scope denotes, found through
/// [`find_in_scope`]: a class declared under a candidate spelling, or a
/// typedef registered under one, with the alias's pointer shape kept. A bare
/// alias at the global scope is left to the caller, which reads the flat
/// alias table after the primitive spellings as C does; one spelled `::T` is
/// answered here.
fn scoped_type_desc(program: &Program, ctx: &LowerContext, name: &str) -> Option<TypeDesc> {
    let spelled_global = name.starts_with("::");
    find_in_scope(program, ctx, name, |candidate, global| {
        if program.types.is_struct_declared(candidate)
            || program
                .types
                .type_id_by_tag(candidate, trace_ir::TypeKind::Struct)
                .is_some()
        {
            return Some(TypeDesc::Struct {
                name: candidate.to_owned(),
                fields: Vec::new(),
            });
        }
        if global && !spelled_global && !candidate.contains("::") {
            return None;
        }
        program.types.resolve_alias(candidate).cloned()
    })
}

/// A template argument split into what names the type and the pointer /
/// reference punctuation after it, every level kept and cv-qualifiers at any
/// level dropped: `T * const *` is (`T`, `**`), `T &` is (`T`, `&`).
fn split_pointer_suffix(arg: &str) -> (&str, String) {
    let mut rest = arg.trim();
    let mut suffix = String::new();
    loop {
        let bare = strip_trailing_cv(rest).trim_end();
        match bare.strip_suffix(['*', '&']) {
            Some(shorter) => {
                suffix.insert(0, bare.chars().next_back().unwrap_or('*'));
                rest = shorter;
            }
            None => return (bare, suffix),
        }
    }
}

/// A non-type template argument spelled as a literal (`4`, `-1`, `true`,
/// `'a'`): never a class, so never qualified to a namespace.
fn is_template_literal(arg: &str) -> bool {
    arg.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, '\'' | '"' | '-'))
        || matches!(arg, "true" | "false" | "nullptr")
}

/// `T const` / `T volatile` spelled after the type, also directly after
/// pointer or reference punctuation (`T*const`).
fn strip_trailing_cv(mut s: &str) -> &str {
    loop {
        let t = s.trim_end();
        match t
            .strip_suffix("const")
            .or_else(|| t.strip_suffix("volatile"))
        {
            Some(rest) if rest.ends_with(|c: char| c.is_whitespace() || matches!(c, '*' | '&')) => {
                s = rest;
            }
            _ => return t,
        }
    }
}

/// Whether a `field_expression` spells `->` rather than `.`.
fn is_arrow_access(node: Node) -> bool {
    member_access_op(node).1
}

/// `(is member access, is `->`)` for a `field_expression`, in one walk.
fn member_access_op(node: Node) -> (bool, bool) {
    let mut arrow = false;
    let mut dot = false;
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "->" => arrow = true,
            "." => dot = true,
            _ => {}
        }
    }
    (arrow || dot, arrow)
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
                if ctx.is_cpp {
                    // `*sp` on a smart pointer is the pointee, as `sp->` is.
                    let desc = deref_desc(program, receiver_desc(program, ctx, source, arg)?)?;
                    return class_name_of_desc(&desc);
                }
                return infer_static_class(program, ctx, source, arg);
            }
            None
        }
        "field_expression" => {
            let base = node.child_by_field_name("argument")?;
            let field = node.child_by_field_name("field")?;
            let base_cls = infer_static_class(program, ctx, source, base)?;
            // `w->f` reads `f` off what `w`'s `operator->` yields (#64).
            let base_cls = if ctx.is_cpp && is_arrow_access(node) {
                member_receiver_class(program, ctx, source, base, true)?
            } else {
                base_cls
            };
            let fname = normalize_qualified(node_text(source, &field));
            class_field_static_class(program, &base_cls, &fname)
        }
        "cast_expression" => {
            // `(T *)p` names `T`; the descriptor's declarator is not part of it.
            let descriptor = node.child_by_field_name("type")?;
            let type_node = descriptor.child_by_field_name("type").unwrap_or(descriptor);
            let raw = normalize_qualified(node_text(source, &type_node));
            let qualified = qualify_class_name(program, ctx, &strip_template_args(&raw));
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
        "call_expression" => class_name_of_desc(&receiver_desc(program, ctx, source, node)?),
        "new_expression" => new_expression_class(program, ctx, source, node),
        _ => None,
    }
}

fn var_static_class(program: &Program, v: VarId) -> Option<String> {
    let var = program.symbols.variable(v);
    class_name_of_desc(program.types.get(var.type_id).desc.as_ref())
}

/// Peel `Ptr` layers (including references, which lower as pointers) to a
/// class/struct tag: `T &` / `T *` yield `T`. A smart pointer is its own
/// class here (`sptr<T>` yields `sptr`, for `sp.Get()`); what it points to
/// is the arrow's business, see [`resolve_operator_arrow`].
fn class_name_of_desc(desc: &TypeDesc) -> Option<String> {
    class_spelling_of_desc(desc).map(|name| receiver_lookup_name(name).into_owned())
}

fn class_spelling_of_desc(desc: &TypeDesc) -> Option<&str> {
    match desc {
        TypeDesc::Struct { name, .. } => Some(name),
        TypeDesc::Ptr(inner) => class_spelling_of_desc(inner),
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
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "qualified_identifier" | "type_identifier" | "template_type" => {
                let raw = normalize_qualified(node_text(source, &child));
                return Some(qualify_class_name(program, ctx, &strip_template_args(&raw)));
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
    let TypeDesc::Struct { name, .. } = program.types.get(ty).desc.as_ref().clone() else {
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
    let TypeDesc::Struct { name, .. } = program.types.get(ty).desc.as_ref().clone() else {
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
        match node.named_child(0) {
            Some(inner) => node = inner,
            None => break,
        }
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
        is_defined: false,
        is_weak: false,
        target: None,
        is_namespaced: false,
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
    let mut arrows = Vec::new();
    let mut cur = peel_expression(node);
    while cur.kind() == "field_expression" {
        field_names.push(field_name_from_node(source, cur)?);
        arrows.push(is_arrow_access(cur));
        // Peel inside the walk, not only at the root: `(a->b)->c` is one
        // chain, and stopping at the parentheses would resolve `c` against
        // `a`'s layout — the base `resolve_lvalue_var` peels down to anyway.
        cur = peel_expression(cur.child_by_field_name("argument")?);
    }
    let mut base = resolve_lvalue_var(program, ctx, source, cur)?;
    field_names.reverse();
    arrows.reverse();

    // Keep the receiver's pointer provenance before layout lookup strips it.
    // References and explicit dereferences denote the referred-to value.
    let mut raw_pointer = ctx.is_cpp
        && arrows.first() == Some(&true)
        && if cur.kind() == "identifier" {
            // The common case only needs a type tag, not a cloned layout.
            match program
                .types
                .get(variable_type_id(program, base)?)
                .desc
                .as_ref()
            {
                TypeDesc::Ptr(inner) if ctx.reference_vars.contains(&base) => {
                    matches!(**inner, TypeDesc::Ptr(_))
                }
                TypeDesc::Ptr(_) => true,
                _ => false,
            }
        } else {
            matches!(
                receiver_desc(program, ctx, source, cur),
                Some(TypeDesc::Ptr(_))
            )
        };
    let mut type_id = struct_type_for_var(program, base)?;
    let mut field_ids = Vec::new();
    let mut path_start = 0;
    let mut summary_receiver = None;
    // `(*sp).field` crosses into the same separate object as `sp->field`.
    // Resolving the lvalue base alone retains `sp`'s layout. Only an
    // overloaded dereference needs a summary receiver; `(*raw).field`
    // keeps the raw pointer's existing points-to flow.
    if ctx.is_cpp
        && cur.kind() == "pointer_expression"
        && pointer_op(source, cur).as_deref() == Some("*")
    {
        if let Some(operand @ TypeDesc::Struct { .. }) = cur
            .named_child(0)
            .and_then(|operand| receiver_desc(program, ctx, source, operand))
        {
            let TypeDesc::Struct { name, .. } = deref_desc(program, operand)? else {
                return None;
            };
            type_id = program
                .types
                .type_id_by_tag(&name, trace_ir::TypeKind::Struct)?;
            summary_receiver = Some(type_id);
        }
    }
    for (step, (fname, arrow)) in field_names.iter().zip(arrows).enumerate() {
        // `sp->f` is the pointee's `f`; `sp.f` stays the wrapper's own, so
        // only an overloaded arrow steps through a smart pointer. A raw
        // pointer's built-in arrow stops at the wrapper itself.
        if arrow && !raw_pointer {
            let pointee = peel_wrapper_to_pointee(program, type_id);
            if pointee != type_id {
                // An overloaded arrow crosses into a separate object. Give
                // the GEP a pointee-typed receiver so PAG's existing summary
                // fallback connects it to raw-pointer accesses to that type.
                // Keeping the wrapper prefix would model an inline subobject
                // and resolve fields against the wrong layout in the solver.
                summary_receiver = Some(pointee);
                field_ids.clear();
                path_start = step;
                type_id = pointee;
            }
        }
        let fid = program.types.field_id_by_name(type_id, fname)?;
        field_ids.push(fid);
        let layout = program.types.get(type_id);
        type_id = layout.layout.fields.get(&fid)?.type_id;
        raw_pointer = matches!(program.types.get(type_id).desc.as_ref(), TypeDesc::Ptr(_));
        type_id = peel_ptr_to_struct(program, type_id);
    }
    // Method names also pass through decomposition. Allocate only after the
    // whole path resolves to fields, and only for the final overloaded arrow.
    if let Some(pointee) = summary_receiver {
        base = alloc_recv_temp(program, ctx, node, pointee);
    }
    field_names.drain(..path_start);
    Some((base, field_ids, field_names))
}

/// `sp->f` on a smart pointer is `f` of the pointee, as `sp->m()` is the
/// pointee's member: a wrapper with a declared `operator->`, a standard
/// one, or one whose body is not in the tree (#86) steps to the class the
/// arrow yields when that class has a layout to look `f` up in.
fn peel_wrapper_to_pointee(program: &Program, type_id: trace_ir::TypeId) -> trace_ir::TypeId {
    let TypeDesc::Struct { name, .. } = program.types.get(type_id).desc.as_ref() else {
        return type_id;
    };
    if name.is_empty() {
        return type_id;
    }
    // One rule for both spellings: `is_undefined_wrapper` already requires
    // arguments, and `declares_arrow` searches the hierarchy, so a concrete
    // class inheriting `operator->` from a base is a wrapper here too.
    let cls = strip_template_args(name);
    let wrapper = is_undefined_wrapper(program, name, &cls)
        || is_std_smart_ptr_name(&cls)
        || declares_arrow(program, &cls);
    if !wrapper {
        return type_id;
    }
    let desc = TypeDesc::Struct {
        name: name.clone(),
        fields: Vec::new(),
    };
    resolve_operator_arrow(program, desc)
        .and_then(|pointee| {
            program
                .types
                .type_id_by_tag(&pointee, trace_ir::TypeKind::Struct)
        })
        .unwrap_or(type_id)
}

fn peel_ptr_to_struct(program: &mut Program, type_id: trace_ir::TypeId) -> trace_ir::TypeId {
    let inner = match program.types.get(type_id).desc.as_ref() {
        TypeDesc::Ptr(inner) => Some((**inner).clone()),
        _ => None,
    };
    inner.map_or(type_id, |desc| program.types.intern(desc))
}

fn struct_type_for_var(program: &mut Program, var: VarId) -> Option<trace_ir::TypeId> {
    let mut type_id = variable_type_id(program, var)?;
    for _ in 0..4 {
        match &program.types.get(type_id).desc.as_ref().clone() {
            TypeDesc::Ptr(inner) => {
                // A template instantiation can exist only inside this pointer
                // descriptor so far; intern its layout before looking it up.
                type_id = program.types.resolve_type_id(inner);
                if type_id == program.types.unknown() {
                    type_id = program.types.intern((**inner).clone());
                }
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
            } else if let Some(callee_var) = resolve_callee_var(program, ctx, source, node) {
                program
                    .flow
                    .push(FlowConstraint::CallReturnIndirect { dst, callee_var });
                ctx.call_return_dst.borrow_mut().insert(node.id(), dst);
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
                let call_args = collect_call_args(program, ctx, source, args);
                if let Some(caller) = ctx.current_fn {
                    emit_member_sites(
                        program,
                        caller,
                        &cls,
                        &trace_ir::MethodKind::Ctor,
                        Some(alloc_tmp),
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
            // A declared name shadows a function of the same name, so the
            // variable is asked first. Order matters now that this arm records
            // `AddrOfFn` rather than merely suppressing a fact: resolving the
            // function first would drop `return local;`'s real copy flow and
            // invent a pointer to an unrelated function.
            if let Some(src) = lookup_var(ctx, program, name) {
                Some(ReturnFlow::Copy { src })
            } else if let Some(callee) = program
                .symbols
                .resolve_function_in_scope(name, Some(ctx.current_file))
            {
                Some(ReturnFlow::AddrOfFn { callee })
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
    program: &Program,
    ctx: &LowerContext,
    source: &str,
    node: Node,
) -> Option<String> {
    let name = resolve_direct_call_name(source, node)?;
    if lookup_var(ctx, program, &name).is_some() {
        return None;
    }
    Some(name)
}

/// A receiver standing for the object an overloaded `->` yields, typed as
/// that pointee so its field step resolves against the pointee's layout.
/// Its own name prefix keeps it out of the `_ret` ordinal space that a
/// variant merge pairs temporaries in: whether this temp exists depends on
/// the receiver's type, so sharing that space would let one configuration's
/// call-return temp land on another's receiver.
fn alloc_recv_temp(
    program: &mut Program,
    ctx: &LowerContext,
    span_node: Node,
    pointee: trace_ir::TypeId,
) -> VarId {
    let var_id = program.symbols.alloc_var_id();
    let span = node_span(program, ctx, span_node);
    program.symbols.add_variable(Variable {
        is_defined: false,
        is_weak: false,
        target: None,
        is_namespaced: false,
        id: var_id,
        name: format!("_recv{}", var_id.0),
        type_id: pointee,
        storage: StorageClass::Local,
        fn_id: ctx.current_fn,
        param_index: None,
        span,
        is_pointer: true,
    });
    var_id
}

fn alloc_ret_temp(program: &mut Program, ctx: &LowerContext, span_node: Node) -> VarId {
    let span = node_span(program, ctx, span_node);
    alloc_ret_temp_spanned(program, ctx, span)
}

fn alloc_ret_temp_spanned(program: &mut Program, ctx: &LowerContext, span: Span) -> VarId {
    let var_id = program.symbols.alloc_var_id();
    program.symbols.add_variable(Variable {
        is_defined: false,
        is_weak: false,
        target: None,
        is_namespaced: false,
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
    // `Worker w((OnReady));` passes the function in parentheses, the usual
    // spelling that keeps a direct initialization from reading as a
    // declaration.
    let node = peel_expression(node);
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
) -> CalleeRef {
    let node = peel_expression(node);
    if node.kind() != "field_expression" {
        return resolve_callee(program, ctx, source, node);
    }
    // Answer before decomposition: it emits loads and allocates a summary
    // receiver for an overloaded arrow, neither of which a second visit to
    // the same node may repeat.
    if let Some(cached) = ctx.callee_load_cache.borrow().get(&node.id()) {
        return cached.clone();
    }
    let mut result = None;
    if let Some((base, field_ids, field_names)) = decompose_field_path(program, ctx, source, node) {
        let text = field_callee_text(source, node);
        if let Some(load_var) =
            emit_field_fn_ptr_load(program, ctx, source, node, base, &field_ids, &field_names)
        {
            result = Some((text, false, Some(load_var)));
        }
    }
    let result = result.unwrap_or_else(|| resolve_callee(program, ctx, source, node));
    ctx.callee_load_cache
        .borrow_mut()
        .insert(node.id(), result.clone());
    result
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
                is_defined: false,
                is_weak: false,
                target: None,
                is_namespaced: false,
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
        if matches!(
            program.types.get(field_type_id).desc.as_ref(),
            TypeDesc::Ptr(_)
        ) {
            let load_var = program.symbols.alloc_var_id();
            let span = node_span(program, ctx, span_node);
            program.symbols.add_variable(Variable {
                is_defined: false,
                is_weak: false,
                target: None,
                is_namespaced: false,
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
            type_id = program.types.resolve_type_id(
                match program.types.get(field_type_id).desc.as_ref() {
                    TypeDesc::Ptr(inner) => inner,
                    _ => unreachable!(),
                },
            );
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

/// The name the index holds a callee spelled `name` under. A leading `::` only
/// says the lookup starts at the global scope; the spelling keeps it, so an
/// unresolved `::operator new` stays that external.
fn global_lookup_name(name: &str) -> &str {
    name.strip_prefix("::").unwrap_or(name)
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
    // A function-local `static` is among `locals`, as every variable declared
    // in a body is.
    program.symbols.file_static_named(ctx.current_file, name)
}

fn declaration_is_extern(source: &str, node: Node) -> bool {
    let Some(node) = enclosing_decl(node, &["declaration", "field_declaration"]) else {
        return false;
    };
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).any(|child| {
        child.kind() == "storage_class_specifier" && node_text(source, &child) == "extern"
    });
    found
}

/// Climb to the enclosing declaration node of one of `kinds`.
fn enclosing_decl<'t>(mut node: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    while !kinds.contains(&node.kind()) {
        node = node.parent()?;
    }
    Some(node)
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
/// non-fn-ptr `function_declarator`, the number of pointer levels, and whether
/// a reference layer sits above it (`T &f(...)`).
/// Variables (plain pointers, arrays, fn-ptr vars) yield `None`.
fn fn_decl_under_pointer(decl: Node) -> Option<(Node, usize, bool)> {
    let mut cur = decl;
    let mut depth = 0usize;
    let mut reference = false;
    loop {
        match cur.kind() {
            "pointer_declarator" => {
                depth += 1;
                cur = cur
                    .child_by_field_name("declarator")
                    .or_else(|| cur.named_child(0))?;
            }
            "parenthesized_declarator" | "reference_declarator" => {
                reference |= cur.kind() == "reference_declarator";
                cur = cur.named_child(0)?;
            }
            "function_declarator" => {
                if is_function_pointer_declarator(cur) {
                    return None;
                }
                return Some((cur, depth, reference));
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
            // refine supported initializers separately.
            return TypeDesc::Unknown;
        }
        if let Some(desc) = local_alias(ctx, &raw) {
            return desc.clone();
        }
        if is_callable_wrapper(&raw) {
            // `std::function<...>` holds a function value; intern as FnPtr
            // so AddrOfFn stores are not rejected by the slot guard.
            return TypeDesc::FnPtr {
                ret: Box::new(TypeDesc::Int),
                params: Vec::new(),
            };
        }
        // A smart-pointer instantiation keeps its arguments in its tag
        // (`sptr<Plugin>`), which is what `p->m()` substitutes `T` from at
        // the call site; the wrapper stays the variable's own class, so
        // `p.Get()` is the wrapper's member and not the pointee's (#64).
        if ctx.is_cpp && text.contains('<') {
            let cls = qualify_class_name(
                program,
                ctx,
                &normalize_qualified(type_name_before_template(text)),
            );
            let tail = template_tail(text).trim();
            let member = tail
                .starts_with("::")
                .then(|| format!("{cls}{}", strip_template_args(&normalize_spacing(tail))));
            // A member alias of the template (`Holder<int>::Ptr`) stands for
            // what it names, whether or not the template is a wrapper.
            if let Some(desc) = member
                .as_deref()
                .and_then(|m| program.types.resolve_alias(m))
            {
                return desc.clone();
            }
            if !program.types.is_struct_defined(&cls)
                || declares_arrow(program, &cls)
                || is_std_smart_ptr_name(&cls)
                || program.has_template_returns(&cls)
            {
                let fields = program
                    .types
                    .type_id_by_tag(&cls, trace_ir::TypeKind::Struct)
                    .and_then(|id| match program.types.get(id).desc.as_ref() {
                        TypeDesc::Struct { fields, .. } => Some(fields.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                return TypeDesc::Struct {
                    name: qualify_template_spelling(program, ctx, text),
                    fields,
                };
            }
            // A defined class template spelled with its arguments
            // (`BlockingQueue<std::any>`): the class the lookup found,
            // arguments dropped. Re-deriving the tag from the innermost
            // namespace named a class this unit never saw, or nothing. A
            // member type of it (`BlockingQueue<std::any>::Iterator`) keeps
            // that class as its prefix.
            if tail.is_empty() || member.is_some() {
                return TypeDesc::Struct {
                    name: member.unwrap_or(cls),
                    fields: Vec::new(),
                };
            }
        }
        let stripped = strip_template_args(&raw);
        let scoped = ctx.is_cpp && !is_fundamental_type_name(&stripped);
        if scoped {
            if let Some(desc) = scoped_type_desc(program, ctx, &stripped) {
                return desc;
            }
        }
        // The scoped lookup has already asked about the innermost spelling.
        let tag_hit = !scoped
            && program
                .types
                .type_id_by_tag(&ctx.qualify(&stripped), trace_ir::TypeKind::Struct)
                .is_some();
        let looks_class = stripped.contains("::")
            || stripped != raw // had template args stripped
            || tag_hit;
        if !looks_class {
            // Plain C typedef aliases keep the legacy path below.
        } else {
            let qualified = qualify_type_name(ctx, &stripped);
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
    let desc = type_desc_from_node(program, ctx, source, op.child_by_field_name("type")?);
    let desc = abstract_declarator_shape(desc, op.child_by_field_name("declarator"), false);
    Some(program.types.intern(desc))
}

/// `desc` wrapped in the pointer, reference and function layers of an
/// abstract declarator (`T *`, `void (*)(int)`). A function declarator with
/// nothing nested is the function type itself when `bare_function_is_type`
/// (`using F = void(int);`), and the declaring member's own parameter list
/// otherwise (`operator T *()`).
fn abstract_declarator_shape(
    mut desc: TypeDesc,
    declarator: Option<Node>,
    bare_function_is_type: bool,
) -> TypeDesc {
    let mut cur = declarator;
    while let Some(n) = cur {
        cur = match n.kind() {
            "abstract_pointer_declarator" | "abstract_reference_declarator" => {
                desc = TypeDesc::Ptr(Box::new(desc));
                n.child_by_field_name("declarator")
                    .or_else(|| n.named_child(0))
            }
            "abstract_parenthesized_declarator" => n.named_child(0),
            "abstract_array_declarator" => {
                desc = TypeDesc::Array {
                    elem: Box::new(desc),
                    size: None,
                };
                n.child_by_field_name("declarator")
            }
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
                None => {
                    if bare_function_is_type {
                        desc = TypeDesc::FnPtr {
                            ret: Box::new(desc),
                            params: Vec::new(),
                        };
                    }
                    break;
                }
            },
            _ => break,
        };
    }
    desc
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

/// A class spelling in the current scope, with the enclosing namespace
/// applied only to an unqualified one.
fn qualify_type_name(ctx: &LowerContext, name: &str) -> String {
    if name.contains("::") {
        name.to_string()
    } else {
        ctx.qualify(name)
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
    let s = strip_trailing_cv(s.trim().trim_end_matches(['*', '&']).trim());
    s.trim_end_matches(['*', '&']).trim().to_string()
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

/// The symbol `FileId` a LineMap entry's origin file is interned under,
/// memoized per unit by LineMap file index: a span's origin is asked for once
/// per node, and the path comparison and hash behind it once per file.
/// `resolve` is how the caller turns an origin path into an id — interning it
/// when it may add one, looking it up when it may not.
fn origin_file_id(
    ctx: &LowerContext,
    line_map: &trace_preproc::LineMap,
    entry: &trace_preproc::LineMapEntry,
    resolve: impl FnOnce(&Path) -> Option<trace_ir::FileId>,
) -> Option<trace_ir::FileId> {
    let cached = &ctx.origin_file_ids[entry.file as usize];
    if let Some(fid) = cached.get() {
        return Some(fid);
    }
    let origin = line_map.path_of(entry);
    let fid = if origin != ctx.primary_path {
        resolve(origin)?
    } else {
        ctx.current_file
    };
    cached.set(Some(fid));
    Some(fid)
}

fn node_span(program: &mut Program, ctx: &LowerContext, node: Node) -> Span {
    if let Some(line_map) = &ctx.line_map {
        if let Some(entry) = line_map.lookup(node.start_byte()) {
            // Always report original-file coordinates. Code from an
            // `#include`d file is attributed to its original header;
            // TU-local code keeps the primary file but gets its original
            // (pre-expansion) line/col, so reported lines match what a
            // user sees in their editor (AGENTS.md LineMap invariant).
            let fid = origin_file_id(ctx, line_map, entry, |origin| {
                Some(program.symbols.add_file_interned(origin))
            })
            .unwrap_or(ctx.current_file);
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
                    let fid = origin_file_id(ctx, line_map, entry, |origin| {
                        program.symbols.file_by_path(origin)
                    });
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

#[cfg(test)]
mod index_window_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn review_scoped_weak_attributes() {
        for attribute in ["gnu::weak", "gnu::__weak__"] {
            let source: Arc<str> = format!("[[{attribute}]] void hook() {{}}\n").into();
            let parsed =
                crate::parse::parse_source_with_lang(source.clone(), crate::parse::SourceLang::Cpp)
                    .unwrap();
            let declaration = parsed.tree.root_node().named_child(0).unwrap();
            assert!(
                declaration_is_weak(&source, declaration),
                "{}",
                parsed.tree.root_node().to_sexp()
            );
        }
    }

    #[test]
    fn review_raw_spaced_pragma_marks_external_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("weak.c");
        let source = "void hook(void) {}\nint value;\nstatic int local;\n#  pragma weak hook\n#\tpragma weak value\n# pragma weak local\n";
        std::fs::write(&path, source).unwrap();
        let graph = IncludeGraph::build(dir.path(), std::slice::from_ref(&path), &[]);
        let pre = PreprocessedSource {
            text: source.into(),
            line_map: Default::default(),
            included_headers: Default::default(),
            inlined_headers: Default::default(),
            language: Language::C,
            replayed_variants: Default::default(),
            diagnostics: Vec::new(),
            conditionals: Vec::new(),
        };
        let mut program = Program::new(dir.path().to_path_buf());
        lower_prepared_source(
            &mut program,
            &path,
            &graph,
            Arc::new(pre),
            Language::C,
            false,
            None,
            &[],
        )
        .unwrap();
        assert!(
            program
                .symbols
                .functions
                .iter()
                .find(|f| f.name == "hook")
                .unwrap()
                .is_weak
        );
        assert!(
            program
                .symbols
                .variables
                .iter()
                .find(|v| v.name == "value")
                .unwrap()
                .is_weak
        );
        assert!(
            !program
                .symbols
                .variables
                .iter()
                .find(|v| v.name == "local")
                .unwrap()
                .is_weak
        );
    }

    #[test]
    fn weak_global_direct_initialization_has_definition_and_flow_owner() {
        for storage in ["", "extern "] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("weak.cpp");
            std::fs::write(&path, format!("typedef void (*callback)(void);\nvoid fallback(void) {{}}\nextern callback configured __attribute__((weak));\n{storage}callback configured(fallback);\n")).unwrap();
            let graph = IncludeGraph::build(dir.path(), std::slice::from_ref(&path), &[]);
            let unit = index_source_file(
                &path,
                dir.path(),
                &graph,
                &PreprocessOptions {
                    record_link_ownership: true,
                    ..PreprocessOptions::new().with_include(dir.path().to_path_buf())
                },
                &IndexSourceCache::new(),
                None,
                &[],
            );
            let global = unit
                .variables
                .iter()
                .rfind(|v| v.name == "configured")
                .unwrap();
            assert!(global.is_defined, "{global:?}");
            assert!(global.is_weak, "{global:?}");
            assert!(!unit.flow.is_empty());
            let ranges = unit
                .global_initializer_ranges
                .get(&global.id)
                .expect("direct initializer owner");
            for index in 0..unit.flow.len() {
                assert!(
                    ranges.iter().any(|range| range.contains(&index)),
                    "unowned direct initializer flow {index}"
                );
            }
        }
    }

    #[test]
    fn weak_global_initializer_ranges_cover_aggregate_temporaries_and_deferred_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("weak.c");
        std::fs::write(&path, "void known(void) {}\nstruct Ops { void (*first)(void); void (*second)(void); };\n__attribute__((weak)) struct Ops configured = { known, later };\nvoid later(void) {}\n").unwrap();
        let graph = IncludeGraph::build(dir.path(), std::slice::from_ref(&path), &[]);
        let ordinary = index_source_file(
            &path,
            dir.path(),
            &graph,
            &PreprocessOptions::new().with_include(dir.path().to_path_buf()),
            &IndexSourceCache::new(),
            None,
            &[],
        );
        assert!(ordinary.global_initializer_ranges.is_empty());
        assert!(ordinary.function_flow_ranges.is_empty());
        assert!(ordinary
            .variables
            .iter()
            .any(|v| v.name == "configured" && v.is_weak));
        let unit = index_source_file(
            &path,
            dir.path(),
            &graph,
            &PreprocessOptions {
                record_link_ownership: true,
                ..PreprocessOptions::new().with_include(dir.path().to_path_buf())
            },
            &IndexSourceCache::new(),
            None,
            &[],
        );
        let global = unit
            .variables
            .iter()
            .find(|v| v.name == "configured")
            .unwrap()
            .id;
        assert!(
            unit.flow.len() >= 4,
            "aggregate must emit temp/GEP/store facts: {:?}",
            unit.flow
        );
        let ranges = unit
            .global_initializer_ranges
            .get(&global)
            .expect("initializer owner");
        for index in 0..unit.flow.len() {
            assert!(
                ranges.iter().any(|range| range.contains(&index)),
                "initializer flow {index} lacks owner: {:?}",
                unit.flow
            );
        }
    }

    #[test]
    fn weak_body_ranges_include_immediate_and_deferred_global_writes() {
        for (header, annotation) in [(false, "attribute"), (false, "pragma"), (true, "attribute")] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("weak.c");
            let attr = if annotation == "attribute" {
                "__attribute__((weak))"
            } else {
                ""
            };
            let pragma = if annotation == "pragma" {
                "#pragma weak weak_body"
            } else {
                ""
            };
            let source = format!("void known(void) {{}}\nvoid (*global)(void);\n{attr} void weak_body(void) {{ global = known; global = later; }}\nvoid later(void) {{}}\n{pragma}\n");
            let headers = if header {
                let header_path = dir.path().join("weak.h");
                std::fs::write(&header_path, source).unwrap();
                std::fs::write(&path, "#include \"weak.h\"\n").unwrap();
                vec![header_path]
            } else {
                std::fs::write(&path, source).unwrap();
                Vec::new()
            };
            let graph = IncludeGraph::build(dir.path(), std::slice::from_ref(&path), &headers);
            let unit = index_source_file(
                &path,
                dir.path(),
                &graph,
                &PreprocessOptions {
                    record_link_ownership: true,
                    ..PreprocessOptions::new().with_include(dir.path().to_path_buf())
                },
                &IndexSourceCache::new(),
                None,
                &[],
            );
            let owner = unit
                .functions
                .iter()
                .find(|f| f.name == "weak_body")
                .unwrap()
                .id;
            let global = unit
                .variables
                .iter()
                .find(|v| v.name == "global")
                .unwrap()
                .id;
            let writes: Vec<_> = unit
                .flow
                .iter()
                .enumerate()
                .filter_map(|(index, flow)| {
                    matches!(flow, FlowConstraint::AddrOfFn { dst, .. } if *dst == global)
                        .then_some(index)
                })
                .collect();
            assert_eq!(writes.len(), 2, "{:?}", unit.flow);
            for index in writes {
                assert!(
                    unit.function_flow_ranges[&owner]
                        .iter()
                        .any(|range| range.contains(&index)),
                    "unowned flow {index}"
                );
            }
        }
    }

    #[test]
    fn units_merge_in_order_while_workers_run_ahead_within_the_window() {
        let jobs = 4;
        let items: Vec<PathBuf> = (0..40).map(|i| PathBuf::from(format!("/u{i}.c"))).collect();
        let pool = index_pool(jobs).unwrap();
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let merged_count = AtomicUsize::new(0);
        let mut merged: Vec<usize> = Vec::new();
        pool.install(|| {
            index_in_window(
                &items,
                jobs,
                |path| {
                    let i: usize = path.to_str().unwrap()[2..]
                        .trim_end_matches(".c")
                        .parse()
                        .unwrap();
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // The first unit is the straggler: everyone else finishes
                    // long before it, and must not wait for it to be taken.
                    let delay = if i == 0 { 40 } else { 1 };
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    // A worker can only be this far past the merge.
                    assert!(i < merged_count.load(Ordering::SeqCst) + index_window(jobs));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    i
                },
                |i| {
                    merged.push(i);
                    merged_count.fetch_add(1, Ordering::SeqCst);
                },
            )
        });
        assert_eq!(merged, (0..40).collect::<Vec<_>>());
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "workers did not run in parallel"
        );
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "payload of another type".to_string())
    }

    #[test]
    fn a_panic_on_either_side_unwinds_instead_of_hanging() {
        let jobs = 3;
        let items: Vec<PathBuf> = (0..30).map(|i| PathBuf::from(format!("/u{i}.c"))).collect();
        let pool = index_pool(jobs).unwrap();
        let unit_of = |path: &PathBuf| -> usize {
            path.to_str().unwrap()[2..]
                .trim_end_matches(".c")
                .parse()
                .unwrap()
        };
        // A worker panics while the merge waits for its unit and the other
        // workers wait on the window.
        let worker = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| {
                index_in_window(
                    &items,
                    jobs,
                    |path| {
                        let i = unit_of(path);
                        assert!(i != 7, "unit 7 fails to index");
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        i
                    },
                    |_| {},
                )
            })
        }));
        let message = panic_message(worker.unwrap_err());
        assert!(message.contains("unit 7 fails to index"), "{message}");
        // The merge panics while workers wait on the window ahead of it.
        let merge = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install(|| {
                index_in_window(
                    &items,
                    jobs,
                    |path| unit_of(path),
                    |i| assert!(i != 2, "unit 2 fails to merge"),
                )
            })
        }));
        let message = panic_message(merge.unwrap_err());
        assert!(message.contains("unit 2 fails to merge"), "{message}");
        // The pool is still usable afterwards.
        let mut merged = Vec::new();
        pool.install(|| index_in_window(&items[..5], jobs, unit_of, |i| merged.push(i)));
        assert_eq!(merged, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn empty_and_single_worker_inputs_complete() {
        let pool = index_pool(2).unwrap();
        let mut seen = Vec::new();
        pool.install(|| index_in_window(&[], 2, |_| 0usize, |i| seen.push(i)));
        assert!(seen.is_empty());
        let items = [PathBuf::from("/a.c"), PathBuf::from("/b.c")];
        pool.install(|| {
            index_in_window(
                &items,
                1,
                |p| p.clone(),
                |p| seen.push(p.to_str().unwrap().len()),
            )
        });
        assert_eq!(seen, vec![4, 4]);
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
mod template_spelling_helpers {
    use super::{
        is_fundamental_type_name, is_template_literal, split_pointer_suffix, strip_trailing_cv,
        template_arguments, template_tail,
    };

    #[test]
    fn arguments_split_outside_parentheses_and_nested_lists() {
        assert_eq!(template_arguments("W<A>"), ["A"]);
        assert_eq!(template_arguments("W<A, B>"), ["A", "B"]);
        assert_eq!(template_arguments("W<Pair<A, B>, C>"), ["Pair<A, B>", "C"]);
        assert_eq!(
            template_arguments("W<void(int, char)>"),
            ["void(int, char)"]
        );
        assert_eq!(
            template_arguments("W<int(*)(int, int), B>"),
            ["int(*)(int, int)", "B"]
        );
        assert!(template_arguments("Plain").is_empty());
        // `W<>` has no arguments, not one empty one: a `""` argument would be
        // qualified to a bare `ns::` and looked up as a class name.
        assert!(template_arguments("W<>").is_empty());
        assert!(template_arguments("std::tuple<>").is_empty());
        assert_eq!(template_arguments("W<A, >"), ["A", ""]);
    }

    #[test]
    fn tail_is_what_follows_the_first_argument_list() {
        assert_eq!(template_tail("W<A>"), "");
        assert_eq!(template_tail("W<A>::Inner"), "::Inner");
        assert_eq!(template_tail("W<Pair<A, B>>::iterator"), "::iterator");
        assert_eq!(template_tail("W<void(int, char)>::R"), "::R");
        assert_eq!(template_tail("Plain"), "");
        assert_eq!(template_tail("W<A"), "");
    }

    #[test]
    fn trailing_cv_is_stripped_past_any_whitespace() {
        assert_eq!(strip_trailing_cv("A const"), "A");
        assert_eq!(strip_trailing_cv("A\tconst"), "A");
        assert_eq!(strip_trailing_cv("A  const volatile "), "A");
        assert_eq!(strip_trailing_cv("A*const"), "A*");
        assert_eq!(strip_trailing_cv("A&volatile"), "A&");
        assert_eq!(strip_trailing_cv("A *const"), "A *");
        assert_eq!(strip_trailing_cv("myconst"), "myconst");
        assert_eq!(strip_trailing_cv("A"), "A");
    }

    #[test]
    fn pointer_levels_survive_qualifiers_at_any_level() {
        assert_eq!(split_pointer_suffix("T"), ("T", String::new()));
        assert_eq!(split_pointer_suffix("T *"), ("T", "*".to_owned()));
        assert_eq!(split_pointer_suffix("T*const"), ("T", "*".to_owned()));
        assert_eq!(split_pointer_suffix("T * const *"), ("T", "**".to_owned()));
        assert_eq!(
            split_pointer_suffix("const T&"),
            ("const T", "&".to_owned())
        );
        assert_eq!(split_pointer_suffix("T const"), ("T", String::new()));
    }

    #[test]
    fn literals_are_never_classes() {
        for arg in [
            "4", "-1", "0x10", "true", "false", "nullptr", "'a'", "\"s\"",
        ] {
            assert!(is_template_literal(arg), "{arg}");
        }
        for arg in ["", "T", "N", "truth", "nullptr_t"] {
            assert!(!is_template_literal(arg), "{arg}");
        }
    }

    #[test]
    fn fundamental_names_are_keywords_and_fixed_width_integers() {
        for name in [
            "int",
            "char",
            "void",
            "unsigned long long",
            "signed char",
            "int32_t",
            "size_t",
        ] {
            assert!(is_fundamental_type_name(name), "{name}");
        }
        for name in ["nullptr_t", "intmax_t", "uintmax_t", "auto"] {
            assert!(is_fundamental_type_name(name), "{name}");
        }
        for name in ["", "Widget", "int_holder", "ns::int", "intptr"] {
            assert!(!is_fundamental_type_name(name), "{name}");
        }
    }
}

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
