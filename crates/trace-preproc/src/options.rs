use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// The language a translation unit is lexed as. Lexing is not identical
/// across the two: C++11 raw string literals (`R"(…)"`) and user-defined
/// literal suffixes (`"x"_s`, `'c'_w`) are single tokens in C++ but two
/// tokens in C, where `R` and the suffix are identifiers that may well be
/// macros (`#define R …` / `'a'C`); and `->*` is one token in C++ but
/// `->` + `*` in C (#37).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    C,
    Cpp,
}

impl Language {
    /// Language implied by a file's extension: the C++ TU and header
    /// spellings GCC recognizes (`.cpp`, `.cc`, `.cxx`, `.c++`, `.C`,
    /// `.hpp`, `.hh`, `.hxx`, `.h++`, `.H`, `.inl`, `.ipp`) are C++;
    /// everything else, including the language-ambiguous `.h`, is C. This
    /// is the one place that decides; the indexer's discovery and grammar
    /// choice derive from it.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(
                "cpp" | "cc" | "cxx" | "c++" | "C" | "hpp" | "hh" | "hxx" | "h++" | "H" | "inl"
                | "ipp",
            ) => Language::Cpp,
            _ => Language::C,
        }
    }
}

/// Key of the include-expansion cache: a header's canonical path and the
/// [`Language`] it was lexed as. A header has no language of its own — it
/// is lexed as the translation unit including it — and the two lexers
/// disagree on raw strings, ud-suffixes and `->*`, so a header reached
/// from both C and C++ units gets one entry per language rather than the
/// first unit's tokenization replayed into the other.
pub type ExpansionKey = (PathBuf, Language);

/// What a cached expansion read from the macro environment that produced it.
///
/// [`ExpansionKey`] says *which* file was expanded; this says *when the
/// result is valid*. A cache entry is a function of the header's text and of
/// every macro its expansion consulted, so replaying it into a translation
/// unit whose environment differs in any of those names hands that unit an
/// expansion of a configuration it does not have (#55).
///
/// Three properties this must have, each of which is a way to get it wrong:
///
/// - **Undefined is a binding, not an absence.** An identifier read while no
///   macro defined it belongs in [`Self::undefined`]. It expanded to itself
///   here, but it may be a macro in another unit, and this entry must not
///   match there. Recording only names that actually expanded misses exactly
///   that case.
/// - **Nested includes contribute upward.** A header's reads include those of
///   every header it pulls in, or an entry matches while a nested expansion
///   embedded in its text silently does not.
/// - **Conditional reads count.** `#if` / `#ifdef` / `defined()` consult the
///   environment even though they substitute nothing.
///
/// A name the expansion defined before reading it is not a dependency: its
/// binding at that point came from the entry's own `ops`, which every
/// consumer replays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacroFingerprint {
    /// Embedded text and macro effects require incompatible environments.
    /// This propagates through enclosing entries even when a local binding
    /// would otherwise mask the conflicting dependency names.
    pub incompatible: bool,
    /// Names read while bound, each with a content hash of the binding (see
    /// `binding_hash`). Ordered by first read, for stable diagnostics.
    pub defined: Vec<(Arc<str>, u64)>,
    /// Names read while unbound.
    pub undefined: HashSet<Arc<str>>,
}

impl MacroFingerprint {
    /// Order-independent digest, for deciding whether a variant of this
    /// expansion is already stored. `defined` is ordered by first read, and
    /// two runs can reach the same bindings by different routes, so the
    /// digest must not depend on the order either half was built in.
    pub fn signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        // XOR of per-entry hashes: commutative, so insertion order drops out.
        let mut acc = u64::from(self.incompatible);
        let mut one = |tag: u8, name: &str, hash: u64| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (tag, name, hash).hash(&mut h);
            acc ^= h.finish();
        };
        for (name, hash) in &self.defined {
            one(0, name, *hash);
        }
        for name in &self.undefined {
            one(1, name, 0);
        }
        acc
    }
}

/// The expansions stored for one [`ExpansionKey`]: one per macro
/// environment a consumer has actually presented, in insertion order.
///
/// Append-only. `splice_cached` resolves a variant to an index under one
/// read lock and re-reads it under another, which is sound precisely
/// because nothing is ever removed or reordered: an index, once handed
/// out, keeps pointing at the same expansion.
pub type ExpansionVariants = Vec<IncludeExpansion>;

/// Shared, variant-keyed cache of expanded `#include` bodies.
pub type ExpansionCache = Arc<RwLock<HashMap<ExpansionKey, ExpansionVariants>>>;

/// Cached preprocessed body for a `#include`d file (shared across translation units).
#[derive(Debug, Clone)]
pub struct IncludeExpansion {
    pub text: Arc<str>,
    pub files: Arc<HashSet<PathBuf>>,
    /// Diagnostics emitted while producing this expansion, including
    /// diagnostics replayed from nested cached headers. Cache hits append
    /// these to the current preprocessing result in their original order.
    pub diagnostics: Arc<Vec<crate::Diagnostic>>,
    /// Origin map for `text`: offsets are relative to the start of the
    /// expansion. Empty when line-map tracking is disabled.
    pub line_map: Arc<crate::LineMap>,
    /// The `#define` / `#undef` directives this header's processing executed
    /// (nested replays included), in order. Cached expansions are spliced
    /// WITHOUT executing their directives, so a consumer must re-apply these
    /// — otherwise a later header whose body invokes one of those macros
    /// starves during its own warm pass and freezes invocation residue into
    /// its cached text. An ordered log rather than a table diff: a diff
    /// cannot represent a no-op `#undef` or an undef-then-redefine of a
    /// name that existed at both capture boundaries.
    pub ops: Arc<Vec<crate::MacroOp>>,
    /// The macro environment this expansion read (see [`MacroFingerprint`]).
    /// A consumer may replay this entry only when its own environment binds
    /// every one of these names the same way.
    pub deps: Arc<MacroFingerprint>,
    /// The variants this expansion itself replayed for the headers it
    /// brought in. Indexing keeps each header's text file-local, so those
    /// nested headers contribute no text here and need their own lowered
    /// units — and a consumer of this entry never visits them, so it cannot
    /// work out which expansion of each applies. This is that record.
    pub nested_variants: Arc<Vec<(PathBuf, usize)>>,
}

#[derive(Debug, Clone)]
pub struct PreprocessOptions {
    pub include_paths: Vec<PathBuf>,
    pub defines: indexmap::IndexMap<String, String>,
    /// Canonical path → raw file contents (skips disk reads during `#include` expansion).
    pub source_cache: Option<std::sync::Arc<HashMap<PathBuf, std::sync::Arc<str>>>>,
    /// Shared cache of expanded `#include` bodies keyed by canonical path
    /// and lexing language (see [`ExpansionKey`]).
    pub include_expansion_cache: Option<ExpansionCache>,
    /// Basename → project paths for fast include resolution.
    pub basename_index: Option<Arc<HashMap<String, Vec<PathBuf>>>>,
    /// Shared macro table populated during header warm-up; inherited by translation units.
    pub shared_macros: Option<crate::SharedMacroTable>,
    /// When true, `#define` / `#undef` update [`Self::shared_macros`].
    pub accumulate_macros: bool,
    /// When true, `include_expansion_cache` is read-only: hits are replayed,
    /// misses are expanded inline but never inserted. Parallel workers must
    /// set this so first-writer-wins races cannot make output scheduling-
    /// dependent.
    pub frozen_expansion_cache: bool,
    /// When false, skip `LineMap` updates (faster indexing; spans are not remapped yet).
    pub track_line_map: bool,
    /// Stop expanding a file once live output exceeds this many bytes.
    pub max_output_bytes: usize,
    /// Nested `#include` stack cap (`include_stack.len()` at `process_file`).
    pub max_include_depth: usize,
    /// Token-loop iterations (including macro rescan) per preprocess run.
    pub max_expanded_tokens: u64,
    /// When false, `#include` of a cacheable header replays macros/guards
    /// but does not copy the header body into live output. Indexing uses
    /// this so each file's preprocessed text stays file-local (PCH-style
    /// header IR is merged later). Default true keeps standalone
    /// `preprocess_file` self-contained.
    pub inline_include_bodies: bool,
    /// Language the translation unit (and every header it includes) is
    /// lexed as. `None` derives it from the TU path via
    /// [`Language::from_path`].
    pub language: Option<Language>,
    /// How many expansions of one header the cache may hold (see
    /// [`ExpansionVariants`]).
    ///
    /// This bounds storage, not correctness. A header that needs more
    /// environments than this simply stops publishing new ones; a consumer
    /// that finds no match expands the header into its own text, which is
    /// what an unseeded cache does anyway. A cache miss is a performance
    /// event and must never be reported as unexplored configuration.
    pub max_expansion_variants: usize,
    /// When true, every conditional chain the run meets is recorded in
    /// `PreprocessResult::conditionals` (see [`crate::ConditionalChain`]),
    /// for measuring what the configuration excludes (#57). Off by default:
    /// indexing does not need it, and a cached header replays its text
    /// without re-evaluating its conditionals, so the record is complete
    /// only for a run that expands every include itself.
    pub record_conditionals: bool,
}

impl Default for PreprocessOptions {
    fn default() -> Self {
        Self {
            include_paths: Vec::new(),
            defines: indexmap::IndexMap::new(),
            source_cache: None,
            include_expansion_cache: None,
            basename_index: None,
            shared_macros: None,
            accumulate_macros: false,
            frozen_expansion_cache: false,
            track_line_map: false,
            max_output_bytes: 32 * 1024 * 1024,
            max_include_depth: 64,
            max_expanded_tokens: 8_000_000,
            inline_include_bodies: true,
            language: None,
            max_expansion_variants: 8,
            record_conditionals: false,
        }
    }
}

impl PreprocessOptions {
    pub fn new() -> Self {
        Self {
            track_line_map: true,
            ..Self::default()
        }
    }

    /// Options used for indexing: line-map tracking stays on so lowered
    /// entities can be attributed to their original `#include`d file.
    pub fn for_indexing(mut self) -> Self {
        self.track_line_map = true;
        self
    }

    pub fn with_include_expansion_cache(mut self, cache: ExpansionCache) -> Self {
        self.include_expansion_cache = Some(cache);
        self
    }

    pub fn with_basename_index(mut self, index: Arc<HashMap<String, Vec<PathBuf>>>) -> Self {
        self.basename_index = Some(index);
        self
    }

    pub fn with_shared_macros(mut self, table: crate::SharedMacroTable) -> Self {
        self.shared_macros = Some(table);
        self
    }

    pub fn with_accumulate_macros(mut self, accumulate: bool) -> Self {
        self.accumulate_macros = accumulate;
        self
    }

    pub fn with_frozen_expansion_cache(mut self, frozen: bool) -> Self {
        self.frozen_expansion_cache = frozen;
        self
    }

    pub fn with_include(mut self, path: PathBuf) -> Self {
        self.include_paths.push(path);
        self
    }

    pub fn with_define(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.defines.insert(name.into(), value.into());
        self
    }

    pub fn with_max_output_bytes(mut self, n: usize) -> Self {
        self.max_output_bytes = n;
        self
    }

    pub fn with_max_include_depth(mut self, n: usize) -> Self {
        self.max_include_depth = n;
        self
    }

    pub fn with_max_expanded_tokens(mut self, n: u64) -> Self {
        self.max_expanded_tokens = n;
        self
    }

    pub fn with_inline_include_bodies(mut self, inline_bodies: bool) -> Self {
        self.inline_include_bodies = inline_bodies;
        self
    }

    pub fn with_language(mut self, language: Language) -> Self {
        self.language = Some(language);
        self
    }

    pub fn with_max_expansion_variants(mut self, n: usize) -> Self {
        self.max_expansion_variants = n;
        self
    }

    pub fn with_record_conditionals(mut self, record: bool) -> Self {
        self.record_conditionals = record;
        self
    }
}
