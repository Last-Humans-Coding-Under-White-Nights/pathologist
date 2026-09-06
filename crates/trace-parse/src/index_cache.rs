use crate::deps::IncludeGraph;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use trace_preproc::{
    preprocess_file, Diagnostic, IncludeExpansion, Language, LineMap, PreprocessOptions,
};

/// Preprocessed text plus its origin map for one canonical file path.
#[derive(Debug, Clone)]
pub struct PreprocessedSource {
    pub text: Arc<str>,
    /// Maps preprocessed offsets back to original `(file, line, col)`.
    /// Empty when the file was not preprocessed (raw source: tree-sitter
    /// positions already refer to original locations).
    pub line_map: Arc<LineMap>,
    /// Canonical `#include` closure from this preprocess run.
    pub included_headers: Arc<Vec<PathBuf>>,
    /// The subset of `included_headers` this run expanded into `text` itself
    /// rather than replaying from the shared expansion cache (#55). Indexing
    /// runs with `inline_include_bodies` off, so these are the headers whose
    /// declarations are in THIS unit and must not also be merged from a PCH
    /// unit built under a different macro environment.
    pub inlined_headers: Arc<Vec<PathBuf>>,
    /// The language this text was lexed as; `replayed_variants` indexes the
    /// variant list for this language only.
    pub language: Language,
    /// Header → index of the cached expansion this run replayed for it (see
    /// `trace_preproc::ExpansionVariants`). A header here contributed no
    /// text to `text`, so its declarations must be merged from the unit
    /// built out of *this* expansion.
    pub replayed_variants: Arc<HashMap<PathBuf, usize>>,
    /// Everything the preprocessor reported while producing `text`, in
    /// emission order, attributed to the file it happened in (nested
    /// includes included). Empty for raw sources.
    pub diagnostics: Vec<Diagnostic>,
}

/// Where a set of units' headers came from.
#[derive(Debug, Default)]
pub struct HeaderProvenance {
    /// Headers some unit expanded into its own text.
    pub inlined: HashSet<PathBuf>,
    /// `(header, language, variant)` triples some unit replayed from the
    /// cache. Each needs its own lowered unit. The language is part of the
    /// key: variant lists are per `(path, language)`, so an index from a C
    /// unit means nothing in the C++ list.
    pub consumed: HashSet<(PathBuf, Language, usize)>,
    /// Headers some unit took from the cache, variant aside.
    pub consumed_paths: HashSet<PathBuf>,
}

/// Preprocessed source text for indexing (one entry per canonical file path).
#[derive(Debug, Clone, Default)]
pub struct IndexSourceCache {
    inner: Arc<RwLock<HashMap<PathBuf, Arc<PreprocessedSource>>>>,
}

impl IndexSourceCache {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get_or_preprocess(
        &self,
        path: &Path,
        graph: &IncludeGraph,
        eff_opts: &PreprocessOptions,
    ) -> Result<Arc<PreprocessedSource>, String> {
        let canonical = graph.intern_path(path);
        if let Ok(guard) = self.inner.read() {
            if let Some(src) = guard.get(&canonical) {
                return Ok(Arc::clone(src));
            }
        }

        let src = Arc::new(read_index_source(path, graph, eff_opts)?);
        if let Ok(mut guard) = self.inner.write() {
            guard.entry(canonical).or_insert_with(|| Arc::clone(&src));
        }
        Ok(src)
    }

    /// Preprocess `path` without storing the result here, for the side
    /// effects carried by `eff_opts` (include-expansion cache, shared macro
    /// table) and for what the run reported. The warm pass uses it for the
    /// second language of a header reached from both C and C++ units: this
    /// cache keeps the text in the language the header is parsed as, but
    /// that language's lexer may not see what this one reports (a `#` line
    /// inside a C++ raw string is a directive in C), so the caller forwards
    /// the returned `diagnostics`.
    pub fn preprocess_uncached(
        &self,
        path: &Path,
        graph: &IncludeGraph,
        eff_opts: &PreprocessOptions,
    ) -> Result<PreprocessedSource, String> {
        read_index_source(path, graph, eff_opts)
    }

    /// Drop every entry in `paths`, so the next `get_or_preprocess` runs the
    /// preprocessor again.
    pub fn evict_all(&self, paths: &HashSet<PathBuf>) {
        if let Ok(mut guard) = self.inner.write() {
            guard.retain(|p, _| !paths.contains(p));
        }
    }

    /// Which of `units` expanded a header into their own text rather than
    /// replaying it.
    pub fn units_that_inlined(&self, units: &HashSet<PathBuf>) -> HashSet<PathBuf> {
        let Ok(guard) = self.inner.read() else {
            return HashSet::new();
        };
        guard
            .iter()
            .filter(|(p, src)| units.contains(*p) && !src.inlined_headers.is_empty())
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Drop `path` so the next `get_or_preprocess` runs the preprocessor
    /// again. The warm pass uses it for a header whose language changed
    /// after a macro-spelled include made it reachable from the other
    /// language's units: the text cached so far was lexed the old way.
    pub fn evict(&self, path: &Path, graph: &IncludeGraph) {
        let canonical = graph.intern_path(path);
        if let Ok(mut guard) = self.inner.write() {
            guard.remove(&canonical);
        }
    }

    /// Where each of `units`' headers came from, in one pass under a single
    /// read lock: the headers some unit expanded into its own text, and the
    /// headers some unit took from the shared expansion cache.
    ///
    /// The two are not complements — a header one unit inlines, another may
    /// replay — and only a header in the first set and NOT the second is
    /// unrepresented in any PCH unit (#55).
    pub fn header_provenance(&self, units: &HashSet<PathBuf>) -> HeaderProvenance {
        let mut out = HeaderProvenance::default();
        let Ok(guard) = self.inner.read() else {
            return out;
        };
        for (path, src) in guard.iter().filter(|(p, _)| units.contains(*p)) {
            out.inlined.extend(src.inlined_headers.iter().cloned());
            for (header, variant) in src.replayed_variants.iter() {
                out.consumed
                    .insert((header.clone(), src.language, *variant));
                out.consumed_paths.insert(header.clone());
            }
            // A header reached only through another header's cached
            // expansion is never visited by this run, so it has no variant
            // of its own here; it still needs a unit, and the include-graph
            // closure of `consumed_paths` covers it.
            out.consumed_paths.extend(
                src.included_headers
                    .iter()
                    .filter(|h| *h != path && !src.inlined_headers.contains(h))
                    .cloned(),
            );
        }
        out
    }

    /// Canonical file → project headers it `#include`d during preprocess.
    pub fn included_by_file(&self) -> Vec<(PathBuf, Vec<PathBuf>)> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        guard
            .iter()
            .map(|(path, src)| (path.clone(), src.included_headers.as_ref().clone()))
            .collect()
    }
}

impl PreprocessedSource {
    /// One stored expansion of a header, ready to lower.
    ///
    /// Indexing preprocesses with `inline_include_bodies` off, so a cached
    /// expansion's text is exactly that header's own contribution — nested
    /// headers keep their own entries — and its `LineMap` offsets are
    /// already relative to the start of that text. Lowering it therefore
    /// yields the same unit re-preprocessing the header would, but under
    /// the macro environment this expansion was actually built in, which is
    /// the one thing re-preprocessing could not reproduce.
    pub fn from_expansion(expansion: &IncludeExpansion, language: Language) -> Self {
        Self {
            text: Arc::clone(&expansion.text),
            line_map: Arc::clone(&expansion.line_map),
            included_headers: Arc::new(expansion.files.iter().cloned().collect()),
            inlined_headers: Arc::new(Vec::new()),
            replayed_variants: Arc::new(expansion.nested_variants.iter().cloned().collect()),
            language,
            diagnostics: expansion.diagnostics.as_ref().clone(),
        }
    }

    /// A source indexed as-is: tree-sitter positions already refer to
    /// original locations, and nothing was preprocessed that could report.
    fn raw(text: Arc<str>) -> Self {
        Self {
            text,
            line_map: Arc::new(LineMap::new()),
            included_headers: Arc::new(Vec::new()),
            inlined_headers: Arc::new(Vec::new()),
            replayed_variants: Arc::new(HashMap::new()),
            language: Language::C,
            diagnostics: Vec::new(),
        }
    }
}

fn read_index_source(
    path: &Path,
    graph: &IncludeGraph,
    eff_opts: &PreprocessOptions,
) -> Result<PreprocessedSource, String> {
    let canonical = graph.intern_path(path);
    if !should_preprocess(path, eff_opts, graph) {
        if let Some(s) = graph.source_cache.get(&canonical) {
            return Ok(PreprocessedSource::raw(Arc::clone(s)));
        }
        return std::fs::read_to_string(path)
            .map(|s| PreprocessedSource::raw(Arc::from(s)))
            .map_err(|e| e.to_string());
    }
    let preproc_result = preprocess_file(&canonical, eff_opts).map_err(|e| e.to_string())?;
    // Keep partial output even when preprocessing stopped mid-file. A stop
    // usually happens inside ONE nested header; discarding everything and
    // parsing raw source instead silently drops every `#include`d declaration
    // from the unit (328/440 TUs on a real HDF tree) and feeds the parser
    // unexpanded function-like macros, which is strictly less sound than a
    // truncated-but-consistent prefix (spans stay LineMap-mappable).
    Ok(PreprocessedSource {
        text: Arc::from(preproc_result.output),
        line_map: Arc::new(preproc_result.line_map),
        included_headers: Arc::new(preproc_result.included_headers),
        inlined_headers: Arc::new(preproc_result.inlined_headers),
        replayed_variants: Arc::new(preproc_result.replayed_variants.into_iter().collect()),
        language: preproc_result.language,
        diagnostics: preproc_result.diagnostics,
    })
}

fn should_preprocess(path: &Path, opts: &PreprocessOptions, graph: &IncludeGraph) -> bool {
    if !opts.defines.is_empty() || !opts.include_paths.is_empty() {
        return true;
    }
    graph.needs_preprocess.contains(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn should_preprocess_uses_effective_include_paths() {
        let path = PathBuf::from("/proj/main.c");
        let mut graph = IncludeGraph {
            root: PathBuf::from("/proj"),
            ..Default::default()
        };
        graph.needs_preprocess.insert(path.clone());

        let empty = PreprocessOptions::default();
        assert!(should_preprocess(&path, &empty, &graph));

        let with_include =
            PreprocessOptions::default().with_include(PathBuf::from("/proj/include"));
        assert!(should_preprocess(&path, &with_include, &graph));
    }

    #[test]
    fn get_or_preprocess_falls_back_when_file_missing() {
        let cache = IndexSourceCache::new();
        let graph = IncludeGraph {
            root: PathBuf::from("/nonexistent"),
            ..Default::default()
        };
        let opts = PreprocessOptions::default();
        let missing = PathBuf::from("/nonexistent/definitely_missing_trace_file.c");
        assert!(cache.get_or_preprocess(&missing, &graph, &opts).is_err());
    }
}
