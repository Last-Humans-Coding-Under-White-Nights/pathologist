//! Index explicit commands with independent macro and include environments.
//! Kept separate from the shared header warm-up path: that path assumes one
//! include search configuration for the entire tree.

use super::{
    add_warnings, finalize_program, index_in_window, index_language, index_pool, index_progress,
    index_source_file, index_source_file_with_variants, project_preprocess_opts,
    with_project_system_paths, HeaderOrder,
};
use crate::compile_commands::CompilationDatabase;
use crate::merge::{merge_unit_index, UnitIndex, VariantMerge};
use crate::{IncludeGraph, IndexSourceCache};
use rustc_hash::FxHashSet;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use trace_ir::Program;
use trace_preproc::{Language, PreprocessOptions};

struct ConfiguredUnits {
    units: Vec<UnitIndex>,
    includes: Vec<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build(
    mut program: Program,
    root: &Path,
    opts: &PreprocessOptions,
    jobs: usize,
    files: &[PathBuf],
    headers: &[PathBuf],
    mut graph: IncludeGraph,
    mut database: CompilationDatabase,
    links: crate::link_commands::LinkDatabase,
    project_compiler_paths: Option<(
        crate::compiler_includes::CompilerSearch,
        crate::compiler_includes::CompilerSearch,
    )>,
    retain_merge_state: bool,
) -> Result<Program, String> {
    let candidates = (opts.explore && opts.explore_budget > 0)
        .then(|| crate::explore::scan_project_gn_candidates(root));
    // Normalized exactly as `lower.rs` normalizes its shared configuration:
    // the per-source loop below re-derives what it needs, but the orphan-header
    // passes use `fallback` as-is and must not inline include bodies or lose
    // line-map attribution.
    let fallback = project_preprocess_opts(root, opts, &graph)
        .for_indexing()
        .with_inline_include_bodies(false)
        .with_basename_index(Arc::new(graph.basename_index.clone()));
    let raw_sources = fallback.source_cache.clone();
    let virtual_dirs = graph.virtual_dirs();
    let cli_include_paths: BTreeSet<_> = opts
        .include_paths
        .iter()
        .map(|path| trace_ir::canonicalize(path))
        .collect();
    let directory = if root.is_file() {
        root.parent().unwrap_or(root)
    } else {
        root
    };
    let cpath_dirs = crate::compiler_includes::cpath_directories(
        &trace_ir::canonicalize(directory),
        std::env::var_os("CPATH").as_deref(),
    );
    let project_options = |mut config: PreprocessOptions, language| {
        if let Some((c, cpp)) = &project_compiler_paths {
            let searched = if language == Language::C { c } else { cpp };
            crate::compiler_includes::remove_system_include_duplicates(
                &mut config,
                &searched.paths,
                &cli_include_paths,
                &cpath_dirs,
            );
        }
        with_project_system_paths(config, language, project_compiler_paths.as_ref())
    };
    let pool = index_pool(jobs)?;
    index_progress(format!(
        "{}: {} commands for {} sources (jobs={jobs})",
        database.source_label(),
        database.commands.values().map(Vec::len).sum::<usize>(),
        database.commands.len()
    ));
    let index = |path: &PathBuf| {
        let (configs, from_database) = configs_for(&database, &fallback, path);
        let mut result = ConfiguredUnits {
            units: Vec::new(),
            includes: Vec::new(),
        };
        // The budget bounds exploration for the source, not for each
        // command: N commands must not buy N times the exploration.
        let mut budget = opts.explore_budget;
        for (command_index, config) in configs.iter().enumerate() {
            let mut config = config.clone();
            config.diagnostic_pool.clone_from(&fallback.diagnostic_pool);
            let mut config = config.for_indexing().with_inline_include_bodies(true);
            // Shared flags name no driver, so use project compiler defaults.
            if !from_database || database.uses_shared_flags() {
                let language = config.language.unwrap_or_else(|| Language::from_path(path));
                config = project_options(config, language);
            }
            config.record_link_ownership = !links.targets.is_empty();
            if from_database {
                add_virtual_include_paths(&mut config, &graph.root, &virtual_dirs);
            }
            // Neither path-keyed source entries nor header expansions are valid
            // across commands with different search paths, even if macros match.
            // A source with no command of its own uses the one shared
            // configuration, where they are valid — and link metadata
            // alone routes every source through here, so discarding the
            // expansion cache for those would make `--link-commands`
            // re-expand every header of every unit.
            if from_database {
                config.include_expansion_cache = None;
                config.shared_macros = None;
                config.accumulate_macros = false;
            }
            config.record_conditionals = opts.explore || opts.record_conditionals;
            if config.source_cache.is_none() {
                config.source_cache.clone_from(&raw_sources);
            }
            let cache = IndexSourceCache::new();
            // Only read when exploration runs; skip the map otherwise.
            let defines = candidates
                .as_ref()
                .map(|_| effective_defines(&config))
                .unwrap_or_default();
            let (mut base, mut variants) = index_source_file_with_variants(
                path,
                root,
                &graph,
                &config,
                &cache,
                None,
                &HeaderOrder::default(),
                candidates.as_ref(),
                &defines,
                budget,
            );
            budget = budget.saturating_sub(variants.len());
            for (_, includes) in cache.included_by_file() {
                result.includes.extend(includes);
            }
            base.compilation_index = Some(command_index);
            for unit in &mut variants {
                unit.compilation_index = Some(command_index);
            }
            result.units.push(base);
            result.units.extend(variants);
        }
        result
    };
    let mut consumed = FxHashSet::default();
    let mut variants_merged = 0;
    let mut includes_by_file = Vec::with_capacity(files.len());
    let mut file_index = 0;
    let scoped = !links.targets.is_empty() && !links.unscoped_inference;
    // Explicit images need later units for weak selection and may reuse them
    // across targets. Ordinary configuration families merge as units arrive.
    let mut linked_units = Vec::new();
    let mut first = None;
    let mut family: Option<VariantMerge> = None;
    index_in_window(&pool, files, jobs, index, |result| {
        variants_merged += result.units.len().saturating_sub(1);
        includes_by_file.push((files[file_index].clone(), result.includes.clone()));
        file_index += 1;
        consumed.extend(result.includes);
        for unit in result.units {
            // Exploratory includes also cannot be treated as orphan headers.
            consumed.extend(unit.files.iter().cloned());
            if scoped {
                linked_units.push(unit);
            } else if let Some(merge) = &mut family {
                merge.push(&mut program, &unit);
            } else if let Some(base) = first.take() {
                // Only build variant deduplication once a second unit exists.
                let mut merge = VariantMerge::start(&mut program, &base, true);
                merge.push(&mut program, &unit);
                family = Some(merge);
            } else {
                first = Some(unit);
            }
        }
    });
    drop(family);
    if scoped {
        // The indexing workers have finished. Return their freed lexer/AST
        // pages before image selection creates its temporary scoped copies.
        crate::memory::reclaim_unused_pages();
        crate::merge::merge_linked_units(&mut program, &linked_units, &links);
    } else if let Some(base) = first {
        VariantMerge::start(&mut program, &base, false);
    }
    // Image selection and merging have consumed these copies. Release them
    // before orphan indexing and finalization reclaim or allocate heap pages.
    drop(linked_units);
    for (path, includes) in includes_by_file {
        graph.add_preprocess_includes(&path, &includes);
    }
    if links.unscoped_inference {
        crate::merge::record_link_targets(&mut program, &links);
    }
    // `merge_unit_variants` counts variants across the whole family; this field
    // means variants per source, so the per-file tally replaces it.
    program.variants_merged = variants_merged;
    let mut cpp_sources = FxHashSet::default();
    let mut no_c_units = true;
    for path in files {
        let (configs, _) = configs_for(&database, &fallback, path);
        for config in configs {
            match config.language.unwrap_or_else(|| Language::from_path(path)) {
                Language::C => no_c_units = false,
                Language::Cpp => {
                    cpp_sources.insert(path.clone());
                }
            }
        }
    }
    let cpp_parse = graph.reachable_from(cpp_sources.iter().chain(&graph.virtual_headers));
    // A virtual header no configuration consumed is indexed like an orphan
    // project header (`IncludeGraph::virtual_headers`).
    let unconsumed_headers: Vec<&PathBuf> = headers
        .iter()
        .chain(&graph.virtual_headers)
        .filter(|path| !consumed.contains(*path))
        .collect();
    // Resolve shared configurations before workers start: validation and error
    // reporting use the same cache as the source pass, once per language.
    let warnings_before = database.warnings.len();
    let header_configs: Vec<_> = unconsumed_headers
        .into_iter()
        .map(|path| {
            let inferred_language = index_language(path, &cpp_parse, no_c_units, opts.language);
            let config = if let Some(mut config) = database.shared_options(inferred_language) {
                config.include_expansion_cache = None;
                config.shared_macros = None;
                config.accumulate_macros = false;
                config.source_cache.clone_from(&raw_sources);
                config.record_link_ownership = !links.targets.is_empty();
                config.diagnostic_pool.clone_from(&fallback.diagnostic_pool);
                add_virtual_include_paths(&mut config, &graph.root, &virtual_dirs);
                config.for_indexing().with_inline_include_bodies(true)
            } else {
                fallback.clone().with_language(inferred_language)
            };
            let language = config.language.unwrap_or_else(|| Language::from_path(path));
            let config = project_options(config, language);
            (path, config)
        })
        .collect();
    add_warnings(
        &mut program,
        "compile_commands",
        &database.warnings[warnings_before..],
    );
    index_in_window(
        &pool,
        &header_configs,
        jobs,
        |(path, config)| {
            let cache = IndexSourceCache::new();
            index_source_file(
                path,
                root,
                &graph,
                config,
                &cache,
                None,
                &HeaderOrder::default(),
            )
        },
        |unit| merge_unit_index(&mut program, &unit),
    );
    program.types.complete_nested_tags();
    // Every search directory any configuration actually used, in first-seen
    // order. `fallback` carries the inferred directories the other path records.
    let observed_dirs: Vec<PathBuf> = database
        .commands
        .values()
        .flatten()
        .chain(database.shared_search_options())
        .chain(std::iter::once(&fallback))
        .flat_map(|config| {
            config
                .quote_include_paths
                .iter()
                .chain(&config.include_paths)
                .chain(&config.system_include_paths)
                .chain(&config.after_include_paths)
                .chain(&config.inferred_include_paths)
        })
        .cloned()
        .collect();
    finalize_program(&mut program, &graph, observed_dirs, retain_merge_state);
    Ok(program)
}

/// Explicit commands and shared flags never name IDL virtual directories.
/// Search them last so a real `-I` header wins (#123), and retain the inference
/// root so inferred-header test partitioning uses the project boundary.
fn add_virtual_include_paths(
    config: &mut PreprocessOptions,
    root: &Path,
    virtual_dirs: &[PathBuf],
) {
    if virtual_dirs.is_empty() {
        return;
    }
    config
        .inference_root
        .get_or_insert_with(|| root.to_path_buf());
    for dir in virtual_dirs {
        if !config.inferred_include_paths.contains(dir) {
            config.inferred_include_paths.push(dir.clone());
        }
    }
}

/// The commands for `path`, or the inferred configuration when it has none.
/// The configurations to index `path` under, and whether they came from the
/// compilation database. Returned together so the caller cannot ask the
/// provenance question separately and drift from the answer given here.
fn configs_for<'a>(
    database: &'a CompilationDatabase,
    fallback: &'a PreprocessOptions,
    path: &Path,
) -> (&'a [PreprocessOptions], bool) {
    match database.commands.get(path) {
        Some(commands) => (commands.as_slice(), true),
        None => (std::slice::from_ref(fallback), false),
    }
}

fn effective_defines(opts: &PreprocessOptions) -> BTreeMap<String, String> {
    let mut defines: BTreeMap<String, String> = opts
        .compiler_defines
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    for op in &opts.command_macros {
        match op {
            trace_preproc::CommandMacro::Define(name, value) => {
                // `-D CALL()=x` carries the parameter list in the name, but the
                // preprocessor defines the macro under the bare identifier and
                // exploration candidates match on that. `-U` needs no such trim:
                // its operand is already a bare name, there and here.
                let ident = name.split('(').next().unwrap_or(name);
                defines.insert(ident.to_string(), value.clone());
            }
            trace_preproc::CommandMacro::Undef(name) => {
                defines.remove(name);
            }
        }
    }
    defines.extend(opts.defines.iter().map(|(k, v)| (k.clone(), v.clone())));
    defines
}
