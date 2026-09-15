use crate::deps::IncludeGraph;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeSet;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use trace_preproc::{
    preprocess_file, Diagnostic, ExpansionCache, ExpansionJournal, IncludeExpansion, Language,
    LineMap, PreprocessOptions,
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
    /// Header → indices of the cached expansions this run replayed for it
    /// (see `trace_preproc::ExpansionVariants`). A header here contributed
    /// no text to `text`, so its declarations must be merged from the units
    /// built out of *these* expansions.
    ///
    /// A list rather than one index: a header included twice under different
    /// macros — an X-macro table, or one whose guard the unit `#undef`s —
    /// replays two expansions, and both belong to this unit (#56).
    pub replayed_variants: Arc<HashMap<PathBuf, Vec<usize>>>,
    /// Everything the preprocessor reported while producing `text`, in
    /// emission order, attributed to the file it happened in (nested
    /// includes included). Empty for raw sources.
    pub diagnostics: Vec<Diagnostic>,
    /// Conditional chains recorded during preprocessing (when `record_conditionals`
    /// is enabled).
    pub conditionals: Vec<trace_preproc::ConditionalChain>,
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
    inner: Arc<RwLock<HashMap<PathBuf, CachedSource>>>,
    /// Lowering turns source-read errors into unit diagnostics. Disk-cache
    /// failures must additionally abort the build, rather than publish missing IR.
    load_errors: Arc<Mutex<BTreeSet<String>>>,
}

#[derive(Debug, Clone)]
enum CachedSource {
    Resident(Arc<PreprocessedSource>),
    Spilled(Arc<SpilledSource>),
    /// Provenance without the text: a warmed header nothing indexes on its
    /// own. Asking for its text again is a logic error, not a cache miss.
    Released(Arc<PreprocessedSource>),
}

impl CachedSource {
    fn metadata(&self) -> &PreprocessedSource {
        match self {
            Self::Resident(src) | Self::Released(src) => src,
            Self::Spilled(src) => &src.metadata,
        }
    }
}

/// Bytes of one serialized `LineMapEntry` in a spill file.
const LINE_MAP_ENTRY_BYTES: usize = 16;

/// `src` with its text and mappings dropped: what the provenance queries
/// need, and nothing that a spill or release keeps out of memory.
fn without_text(src: &PreprocessedSource) -> PreprocessedSource {
    let mut metadata = src.clone();
    metadata.text = Arc::from("");
    metadata.line_map = Arc::new(LineMap::new());
    metadata
}

/// Only text and mapping entries go to disk. Metadata remains available to
/// provenance queries without loading payloads or rerunning preprocessing.
#[derive(Debug)]
struct SpilledSource {
    metadata: PreprocessedSource,
    file: tempfile::TempPath,
    text_len: usize,
    entry_count: usize,
    origin_files: Vec<PathBuf>,
}

impl SpilledSource {
    fn store(src: &PreprocessedSource) -> std::io::Result<Self> {
        let mut file = tempfile::Builder::new()
            .prefix("trace-source-")
            .tempfile()?;
        {
            let mut writer = BufWriter::new(file.as_file_mut());
            writer.write_all(src.text.as_bytes())?;
            // Through the buffered writer an entry at a time: no second
            // copy of the map on the heap while it is being shed.
            let mut entry = [0u8; LINE_MAP_ENTRY_BYTES];
            for e in &src.line_map.entries {
                for (slot, value) in [e.output_offset, e.file, e.line, e.col]
                    .into_iter()
                    .enumerate()
                {
                    entry[slot * 4..slot * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
                writer.write_all(&entry)?;
            }
            writer.flush()?;
        }
        Ok(Self {
            metadata: without_text(src),
            file: file.into_temp_path(),
            text_len: src.text.len(),
            entry_count: src.line_map.entries.len(),
            origin_files: src.line_map.files.clone(),
        })
    }

    fn load(&self) -> std::io::Result<PreprocessedSource> {
        // Open only while loading, giving each reader its own cursor and
        // avoiding one retained file descriptor per spilled source.
        let mut reader = BufReader::new(std::fs::File::open(&self.file)?);
        let mut text = vec![0; self.text_len];
        reader.read_exact(&mut text)?;
        let text = String::from_utf8(text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Decoded a chunk at a time into the final vector, so the raw bytes
        // of the whole map never sit beside it.
        const CHUNK_ENTRIES: usize = 4096;
        let mut entries = Vec::with_capacity(self.entry_count);
        let mut bytes = vec![0; CHUNK_ENTRIES.min(self.entry_count) * LINE_MAP_ENTRY_BYTES];
        let mut remaining = self.entry_count;
        while remaining > 0 {
            let n = remaining.min(CHUNK_ENTRIES);
            let chunk = &mut bytes[..n * LINE_MAP_ENTRY_BYTES];
            reader.read_exact(chunk)?;
            entries.extend(
                chunk
                    .as_chunks::<LINE_MAP_ENTRY_BYTES>()
                    .0
                    .iter()
                    .map(|entry| {
                        let value = |i| u32::from_le_bytes(entry[i..i + 4].try_into().unwrap());
                        trace_preproc::LineMapEntry {
                            output_offset: value(0),
                            file: value(4),
                            line: value(8),
                            col: value(12),
                        }
                    }),
            );
            remaining -= n;
        }
        let mut src = self.metadata.clone();
        src.text = Arc::from(text);
        src.line_map = Arc::new(LineMap {
            files: self.origin_files.clone(),
            entries,
        });
        Ok(src)
    }
}

impl IndexSourceCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_preprocess(
        &self,
        path: &Path,
        graph: &IncludeGraph,
        eff_opts: &PreprocessOptions,
    ) -> Result<Arc<PreprocessedSource>, String> {
        let canonical = graph.intern_path(path);
        let cached = self
            .inner
            .read()
            .ok()
            .and_then(|guard| guard.get(&canonical).cloned());
        match cached {
            Some(CachedSource::Resident(src)) => return Ok(src),
            Some(CachedSource::Spilled(src)) => {
                return src.load().map(Arc::new).map_err(|e| {
                    let message = format!("load preprocessed source {}: {e}", path.display());
                    self.load_errors
                        .lock()
                        .expect("source load error lock")
                        .insert(message.clone());
                    message
                })
            }
            Some(CachedSource::Released(_)) => {
                let message = format!(
                    "preprocessed text of {} was released before it was indexed",
                    path.display()
                );
                self.load_errors
                    .lock()
                    .expect("source load error lock")
                    .insert(message.clone());
                return Err(message);
            }
            None => {}
        }

        let src = Arc::new(read_index_source(path, graph, eff_opts)?.0);
        self.store(canonical, Arc::clone(&src));
        Ok(src)
    }

    /// Store `src` as the text of `path`, unless an entry is already there.
    pub fn insert(&self, path: &Path, graph: &IncludeGraph, src: PreprocessedSource) {
        self.store(graph.intern_path(path), Arc::new(src));
    }

    fn store(&self, canonical: PathBuf, src: Arc<PreprocessedSource>) {
        if let Ok(mut guard) = self.inner.write() {
            guard
                .entry(canonical)
                .or_insert(CachedSource::Resident(src));
        }
    }

    /// Drop the text and mappings of `path`, keeping its provenance for the
    /// header queries. For a source nothing reads back, this is a spill
    /// without the file.
    pub(crate) fn release_text(&self, path: &Path, graph: &IncludeGraph) {
        let canonical = graph.intern_path(path);
        let Ok(mut guard) = self.inner.write() else {
            return;
        };
        if let Some(CachedSource::Resident(src)) = guard.get(&canonical) {
            let released = CachedSource::Released(Arc::new(without_text(src)));
            guard.insert(canonical, released);
        }
    }

    /// Release a large source payload after preprocessing while retaining its
    /// original bytes on disk. Smaller entries stay resident: the saving is
    /// not worth a temporary file each, and most units are under this.
    pub(crate) fn spill(&self, path: &Path, graph: &IncludeGraph) -> Result<(), String> {
        const SPILL_THRESHOLD: usize = 512 * 1024;
        let canonical = graph.intern_path(path);
        let cached = self
            .inner
            .read()
            .map_err(|e| e.to_string())?
            .get(&canonical)
            .cloned();
        let Some(CachedSource::Resident(src)) = cached else {
            return Ok(());
        };
        // Nested includes can leave a large allocation after mappings are
        // truncated. Base the threshold on retained capacity, not live length.
        let bytes = src.text.len().saturating_add(
            src.line_map
                .entries
                .capacity()
                .saturating_mul(LINE_MAP_ENTRY_BYTES),
        );
        if bytes <= SPILL_THRESHOLD {
            return Ok(());
        }
        // I/O happens outside the cache lock so other settling workers proceed.
        let spilled = Arc::new(
            SpilledSource::store(&src)
                .map_err(|e| format!("spill preprocessed source {}: {e}", path.display()))?,
        );
        let mut guard = self.inner.write().map_err(|e| e.to_string())?;
        if matches!(guard.get(&canonical), Some(CachedSource::Resident(current)) if Arc::ptr_eq(current, &src))
        {
            guard.insert(canonical, CachedSource::Spilled(spilled));
        }
        Ok(())
    }

    pub(crate) fn check_load_errors(&self) -> Result<(), String> {
        let errors = self.load_errors.lock().unwrap_or_else(|e| e.into_inner());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.iter().cloned().collect::<Vec<_>>().join("\n"))
        }
    }

    /// Preprocess `path` without storing the result here, for the side
    /// effects carried by `eff_opts` (include-expansion cache, shared macro
    /// table) and for what the run reported. The warm pass uses it for the
    /// second language of a header reached from both C and C++ units: this
    /// cache keeps the text in the language the header is parsed as, but
    /// that language's lexer may not see what this one reports (a `#` line
    /// inside a C++ raw string is a directive in C), so the caller forwards
    /// the returned `diagnostics`.
    ///
    /// The journal is `Some` when `eff_opts` defers publishing and the source
    /// was preprocessed; the discovery pass commits it, then stores the
    /// source with [`Self::insert`].
    pub fn preprocess_uncached(
        &self,
        path: &Path,
        graph: &IncludeGraph,
        eff_opts: &PreprocessOptions,
    ) -> Result<(PreprocessedSource, Option<ExpansionJournal>), String> {
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
            return HashSet::default();
        };
        guard
            .iter()
            .filter(|(p, src)| units.contains(*p) && !src.metadata().inlined_headers.is_empty())
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Drop `path` so the next `get_or_preprocess` runs the preprocessor
    /// again. The warm pass uses it for a header whose language changed
    /// after a macro-spelled include made it reachable from the other
    /// language's units: the text cached so far was lexed the old way. The
    /// index phase uses it for every unit once that unit is lowered, so the
    /// live preprocessed text is bounded by the batch in flight rather than
    /// by the corpus.
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
            let src = src.metadata();
            out.inlined.extend(src.inlined_headers.iter().cloned());
            for (header, variants) in src.replayed_variants.iter() {
                for variant in variants {
                    out.consumed
                        .insert((header.clone(), src.language, *variant));
                }
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
            .map(|(path, src)| {
                (
                    path.clone(),
                    src.metadata().included_headers.as_ref().clone(),
                )
            })
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
            replayed_variants: Arc::new(group_variants(expansion.nested_variants.iter().cloned())),
            language,
            diagnostics: expansion.diagnostics.as_ref().clone(),
            conditionals: Vec::new(),
        }
    }

    /// Commit the deferred run that produced this source (see
    /// [`ExpansionJournal::commit`]), turning the variants it recorded into
    /// indices. False, changing nothing, when the run does not stand.
    pub(crate) fn commit(&mut self, journal: &ExpansionJournal, cache: &ExpansionCache) -> bool {
        let mut replayed: Vec<(PathBuf, usize)> = self
            .replayed_variants
            .iter()
            .flat_map(|(path, variants)| variants.iter().map(|v| (path.clone(), *v)))
            .collect();
        if !journal.commit(cache, &mut replayed) {
            return false;
        }
        // Sorted, as a publishing run's record is (`group_variants`).
        replayed.sort();
        self.replayed_variants = Arc::new(group_variants(replayed));
        true
    }

    /// A source indexed as-is: tree-sitter positions already refer to
    /// original locations, and nothing was preprocessed that could report.
    fn raw(text: Arc<str>) -> Self {
        Self {
            text,
            line_map: Arc::new(LineMap::new()),
            included_headers: Arc::new(Vec::new()),
            inlined_headers: Arc::new(Vec::new()),
            replayed_variants: Arc::new(HashMap::default()),
            language: Language::C,
            diagnostics: Vec::new(),
            conditionals: Vec::new(),
        }
    }
}

fn read_index_source(
    path: &Path,
    graph: &IncludeGraph,
    eff_opts: &PreprocessOptions,
) -> Result<(PreprocessedSource, Option<ExpansionJournal>), String> {
    let canonical = graph.intern_path(path);
    if !should_preprocess(path, eff_opts, graph) {
        if let Some(s) = graph.source_cache.get(&canonical) {
            return Ok((PreprocessedSource::raw(Arc::clone(s)), None));
        }
        return std::fs::read_to_string(path)
            .map(|s| (PreprocessedSource::raw(Arc::from(s)), None))
            .map_err(|e| e.to_string());
    }
    let mut preproc_result = preprocess_file(&canonical, eff_opts).map_err(|e| e.to_string())?;
    let journal = preproc_result.expansion_journal.take();
    // Keep partial output even when preprocessing stopped mid-file. A stop
    // usually happens inside ONE nested header; discarding everything and
    // parsing raw source instead silently drops every `#include`d declaration
    // from the unit (328/440 TUs on a real HDF tree) and feeds the parser
    // unexpanded function-like macros, which is strictly less sound than a
    // truncated-but-consistent prefix (spans stay LineMap-mappable).
    let src = PreprocessedSource {
        text: Arc::from(preproc_result.output),
        line_map: Arc::new(preproc_result.line_map),
        included_headers: Arc::new(preproc_result.included_headers),
        inlined_headers: Arc::new(preproc_result.inlined_headers),
        replayed_variants: Arc::new(group_variants(preproc_result.replayed_variants)),
        language: preproc_result.language,
        diagnostics: preproc_result.diagnostics,
        conditionals: preproc_result.conditionals,
    };
    Ok((src, journal))
}

/// `(path, variant)` pairs into one list of variants per path, in the order
/// they arrive (already sorted upstream, so the lists are reproducible).
fn group_variants(
    pairs: impl IntoIterator<Item = (PathBuf, usize)>,
) -> HashMap<PathBuf, Vec<usize>> {
    let mut out: HashMap<PathBuf, Vec<usize>> = HashMap::default();
    for (path, variant) in pairs {
        let vs = out.entry(path).or_default();
        if !vs.contains(&variant) {
            vs.push(variant);
        }
    }
    out
}

fn should_preprocess(path: &Path, opts: &PreprocessOptions, graph: &IncludeGraph) -> bool {
    opts.configures_preprocessing() || graph.needs_preprocess.contains(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn spilling_releases_reserved_map_storage_and_writes_only_live_entries() {
        let path = PathBuf::from("/source.c");
        let graph = IncludeGraph::default();
        let cache = IndexSourceCache::new();
        let mut src = PreprocessedSource::raw(Arc::from("abc"));
        let mut map = LineMap {
            files: vec![path.clone()],
            entries: Vec::with_capacity(65536),
        };
        map.push(0, 0, 1, 1);
        src.line_map = Arc::new(map);
        cache.inner.write().unwrap().insert(
            graph.intern_path(&path),
            CachedSource::Resident(Arc::new(src)),
        );
        cache.spill(&path, &graph).unwrap();
        {
            let guard = cache.inner.read().unwrap();
            let CachedSource::Spilled(src) = &guard[&graph.intern_path(&path)] else {
                panic!("reserved map allocation was retained")
            };
            assert_eq!(std::fs::metadata(&src.file).unwrap().len(), 3 + 16);
        }
        let loaded = cache
            .get_or_preprocess(&path, &graph, &PreprocessOptions::default())
            .unwrap();
        assert_eq!(loaded.text.as_ref(), "abc");
        assert_eq!(loaded.line_map.entries.len(), 1);
        assert_eq!(loaded.line_map.lookup(0).unwrap().line, 1);
    }

    #[test]
    fn spilled_source_preserves_origins_and_metadata_without_reprocessing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("main.cpp");
        let header = root.join("header.h");
        std::fs::write(&header, "#define VALUE 7\nvoid header_fn();\n").unwrap();
        std::fs::write(
            &path,
            format!(
                "#include \"header.h\"\n#include \"missing.h\"\n#if VALUE\n{}\n#endif\n",
                "int variable = VALUE;\n".repeat(30000)
            ),
        )
        .unwrap();
        let graph = IncludeGraph::build(
            &root,
            std::slice::from_ref(&path),
            std::slice::from_ref(&header),
        );
        let opts = PreprocessOptions::new()
            .with_language(Language::Cpp)
            .with_record_conditionals(true);
        let cache = IndexSourceCache::new();
        let original = cache.get_or_preprocess(&path, &graph, &opts).unwrap();
        assert!(!original.line_map.entries.is_empty());
        assert!(!original.diagnostics.is_empty());
        assert!(!original.conditionals.is_empty());
        let units = [graph.intern_path(&path)].into_iter().collect();
        let provenance = cache.header_provenance(&units);
        let includes = cache.included_by_file();
        let dirty = cache.units_that_inlined(&units);
        cache.spill(&path, &graph).unwrap();
        let spill_path = {
            let guard = cache.inner.read().unwrap();
            let CachedSource::Spilled(src) = &guard[&graph.intern_path(&path)] else {
                panic!("large source not spilled")
            };
            assert!(src.metadata.text.is_empty());
            assert!(src.metadata.line_map.entries.is_empty());
            src.file.to_path_buf()
        };
        let after = cache.header_provenance(&units);
        assert_eq!(after.inlined, provenance.inlined);
        assert_eq!(after.consumed, provenance.consumed);
        assert_eq!(after.consumed_paths, provenance.consumed_paths);
        assert_eq!(cache.included_by_file(), includes);
        assert_eq!(cache.units_that_inlined(&units), dirty);
        // Any accidental preprocessing would now produce different text.
        std::fs::write(&path, "void replacement();\n").unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let loaded = cache.get_or_preprocess(&path, &graph, &opts).unwrap();
                    assert_eq!(loaded.text, original.text);
                    assert_eq!(loaded.line_map, original.line_map);
                    assert_eq!(loaded.included_headers, original.included_headers);
                    assert_eq!(loaded.inlined_headers, original.inlined_headers);
                    assert_eq!(loaded.replayed_variants, original.replayed_variants);
                    assert_eq!(loaded.language, original.language);
                    assert_eq!(loaded.conditionals, original.conditionals);
                    assert_eq!(
                        format!("{:?}", loaded.diagnostics),
                        format!("{:?}", original.diagnostics)
                    );
                });
            }
        });
        cache.check_load_errors().unwrap();
        cache.evict_all(&units);
        assert!(
            !spill_path.exists(),
            "eviction must clean up the temporary file"
        );
    }

    #[test]
    fn released_text_keeps_provenance_and_refuses_to_be_read() {
        let path = PathBuf::from("/released.h");
        let graph = IncludeGraph::default();
        let cache = IndexSourceCache::new();
        let mut src = PreprocessedSource::raw(Arc::from("int x;"));
        src.inlined_headers = Arc::new(vec![PathBuf::from("/inner.h")]);
        cache.inner.write().unwrap().insert(
            graph.intern_path(&path),
            CachedSource::Resident(Arc::new(src)),
        );
        let units = [graph.intern_path(&path)].into_iter().collect();
        let before = cache.units_that_inlined(&units);
        cache.release_text(&path, &graph);
        {
            let guard = cache.inner.read().unwrap();
            let CachedSource::Released(src) = &guard[&graph.intern_path(&path)] else {
                panic!("text was not released")
            };
            assert!(src.text.is_empty());
        }
        assert_eq!(cache.units_that_inlined(&units), before);
        assert!(cache
            .get_or_preprocess(&path, &graph, &PreprocessOptions::default())
            .unwrap_err()
            .contains("released"));
        assert!(cache.check_load_errors().is_err());
        // Releasing again, or releasing a spilled entry, is a no-op.
        cache.release_text(&path, &graph);
        assert!(cache.spill(&path, &graph).is_ok());
    }

    #[test]
    fn spilled_read_failure_is_reported_as_a_build_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.c");
        std::fs::write(&path, "x".repeat(600_000)).unwrap();
        let graph = IncludeGraph::build(dir.path(), std::slice::from_ref(&path), &[]);
        let cache = IndexSourceCache::new();
        let opts = PreprocessOptions::default();
        cache.get_or_preprocess(&path, &graph, &opts).unwrap();
        cache.spill(&path, &graph).unwrap();
        {
            let guard = cache.inner.read().unwrap();
            let CachedSource::Spilled(src) = &guard[&graph.intern_path(&path)] else {
                panic!("large source not spilled")
            };
            std::fs::OpenOptions::new()
                .write(true)
                .open(&src.file)
                .unwrap()
                .set_len(0)
                .unwrap();
        }
        assert!(cache
            .get_or_preprocess(&path, &graph, &opts)
            .unwrap_err()
            .contains("load preprocessed source"));
        assert!(cache.check_load_errors().unwrap_err().contains("main.c"));
    }

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
