//! Index explicit commands with independent macro and include environments.
//! Kept separate from the shared header warm-up path: that path assumes one
//! include search configuration for the entire tree.

use super::{
    finalize_program, index_language, index_pool, index_progress, index_source_file,
    index_source_file_with_variants, project_preprocess_opts,
};
use crate::compile_commands::CompilationDatabase;
use crate::merge::{merge_unit_index, merge_unit_variants, UnitIndex};
use crate::{IncludeGraph, IndexSourceCache};
use rayon::prelude::*;
use rustc_hash::FxHashSet;
use std::collections::BTreeMap;
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
    database: CompilationDatabase,
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
    let pool = index_pool(jobs)?;
    index_progress(format!(
        "compile_commands: {} commands for {} sources (jobs={jobs})",
        database.commands.values().map(Vec::len).sum::<usize>(),
        database.commands.len()
    ));
    let results: Vec<ConfiguredUnits> = pool.install(|| {
        files
            .par_iter()
            .map(|path| {
                let configs = configs_for(&database, &fallback, path);
                let mut result = ConfiguredUnits {
                    units: Vec::new(),
                    includes: Vec::new(),
                };
                // The budget bounds exploration for the source, not for each
                // command: N commands must not buy N times the exploration.
                let mut budget = opts.explore_budget;
                for config in configs {
                    let mut config = config
                        .clone()
                        .for_indexing()
                        .with_inline_include_bodies(true);
                    // Neither path-keyed source entries nor header expansions are valid
                    // across commands with different search paths, even if macros match.
                    config.include_expansion_cache = None;
                    config.shared_macros = None;
                    config.accumulate_macros = false;
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
                    let (base, variants) = index_source_file_with_variants(
                        path,
                        root,
                        &graph,
                        &config,
                        &cache,
                        None,
                        &[],
                        candidates.as_ref(),
                        &defines,
                        budget,
                    );
                    budget = budget.saturating_sub(variants.len());
                    for (_, includes) in cache.included_by_file() {
                        result.includes.extend(includes);
                    }
                    result.units.push(base);
                    result.units.extend(variants);
                }
                result
            })
            .collect()
    });
    let mut consumed = FxHashSet::default();
    let variants_merged = results
        .iter()
        .map(|r| r.units.len().saturating_sub(1))
        .sum::<usize>();
    let mut units = Vec::new();
    for (path, result) in files.iter().zip(results) {
        graph.add_preprocess_includes(path, &result.includes);
        consumed.extend(result.includes);
        // Include files introduced only by exploratory variants also must not
        // be lowered again under an unrelated orphan-header configuration.
        for unit in &result.units {
            consumed.extend(unit.files.iter().cloned());
        }
        units.extend(result.units);
    }
    // Merge the complete configuration family together. A shared header can
    // vary between different source files as well as between commands for one
    // source; ordinary TU deduplication would drop its second body.
    if let Some((base, variants)) = units.split_first() {
        merge_unit_variants(&mut program, base, variants);
    }
    // `merge_unit_variants` counts variants across the whole family; this field
    // means variants per source, so the per-file tally replaces it.
    program.variants_merged = variants_merged;
    let mut cpp_sources = FxHashSet::default();
    let mut no_c_units = true;
    for path in files {
        let configs = configs_for(&database, &fallback, path);
        for config in configs {
            match config.language.unwrap_or_else(|| Language::from_path(path)) {
                Language::C => no_c_units = false,
                Language::Cpp => {
                    cpp_sources.insert(path.clone());
                }
            }
        }
    }
    let cpp_parse = graph.reachable_from(&cpp_sources);
    let unconsumed_headers: Vec<&PathBuf> = headers
        .iter()
        .filter(|path| !consumed.contains(*path))
        .collect();
    if jobs == 1 || unconsumed_headers.len() <= 1 {
        for path in unconsumed_headers {
            let cache = IndexSourceCache::new();
            let config = fallback.clone().with_language(index_language(
                path,
                &cpp_parse,
                no_c_units,
                opts.language,
            ));
            let unit = index_source_file(path, root, &graph, &config, &cache, None, &[]);
            merge_unit_index(&mut program, &unit);
        }
    } else {
        let header_units: Vec<UnitIndex> = pool.install(|| {
            unconsumed_headers
                .par_iter()
                .map(|path| {
                    let cache = IndexSourceCache::new();
                    let config = fallback.clone().with_language(index_language(
                        path,
                        &cpp_parse,
                        no_c_units,
                        opts.language,
                    ));
                    index_source_file(path, root, &graph, &config, &cache, None, &[])
                })
                .collect()
        });
        for unit in &header_units {
            merge_unit_index(&mut program, unit);
        }
    }
    program.types.complete_nested_tags();
    // Every search directory any configuration actually used, in first-seen
    // order. `fallback` carries the inferred directories the other path records.
    let observed_dirs: Vec<PathBuf> = database
        .commands
        .values()
        .flatten()
        .chain(std::iter::once(&fallback))
        .flat_map(|config| {
            config
                .quote_include_paths
                .iter()
                .chain(&config.include_paths)
                .chain(&config.system_include_paths)
        })
        .cloned()
        .collect();
    finalize_program(&mut program, &graph, observed_dirs);
    Ok(program)
}

/// The commands for `path`, or the inferred configuration when it has none.
fn configs_for<'a>(
    database: &'a CompilationDatabase,
    fallback: &'a PreprocessOptions,
    path: &Path,
) -> &'a [PreprocessOptions] {
    database
        .commands
        .get(path)
        .map(Vec::as_slice)
        .unwrap_or(std::slice::from_ref(fallback))
}

fn effective_defines(opts: &PreprocessOptions) -> BTreeMap<String, String> {
    let mut defines = BTreeMap::new();
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
