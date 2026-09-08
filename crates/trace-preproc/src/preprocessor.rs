use crate::macros::{lex_macro_body, MacroDef, MacroOp, MacroTable};
use crate::{
    ArmDirective, ArmOutcome, ConditionRead, ConditionalArm, ConditionalChain, Diagnostic,
    DiagnosticSeverity, Language, Lexer, LineMap, PreprocessOptions, Token, TokenKind,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use thiserror::Error;

/// Nested macro-expansion cap (C11 hide-set is the primary recursion brake;
/// this is a backstop for pathological `##` / hide-set edge cases).
const MAX_MACRO_EXPANSION_DEPTH: u32 = 256;

#[derive(Debug, Error)]
pub enum PreprocessError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{message}")]
    Message { message: String },
}

#[derive(Debug, Clone)]
pub struct PreprocessResult {
    pub output: String,
    pub line_map: LineMap,
    pub diagnostics: Vec<Diagnostic>,
    /// Canonical paths processed by this run (`#include` closure).
    pub included_headers: Vec<PathBuf>,
    /// The subset of [`Self::included_headers`] this run expanded itself
    /// instead of replaying from the shared cache — because no entry existed
    /// yet, or because the entry's [`crate::MacroFingerprint`] did not match
    /// this run's macro environment (#55).
    ///
    /// With `inline_include_bodies` off these are the headers whose body is
    /// in THIS unit's text, so the indexer must not also merge a PCH unit
    /// built under a different configuration for them.
    pub inlined_headers: Vec<PathBuf>,
    /// The language this run lexed as. Variant indices below are positions
    /// in the list for THIS language — a header reached from both C and C++
    /// has one list per language and the two are unrelated.
    pub language: crate::Language,
    /// For each header this run replayed from the shared cache, the index of
    /// each variant it matched (see [`crate::ExpansionVariants`]).
    ///
    /// With `inline_include_bodies` off a replayed header contributes no
    /// text, so its declarations have to come from a separately lowered
    /// unit — and it must be the unit built from *this* expansion, not from
    /// whichever configuration the warm pass happened to take.
    ///
    /// One path can appear more than once: a header included twice under
    /// different macros expands twice, and both expansions belong to this
    /// unit (#56). Sorted, so the record does not depend on hash order.
    pub replayed_variants: Vec<(PathBuf, usize)>,
    /// Every conditional chain this run evaluated or skipped, in the order
    /// their opening directives were met, headers included. Empty unless
    /// `PreprocessOptions::record_conditionals` is set (#57).
    pub conditionals: Vec<ConditionalChain>,
}

#[derive(Debug)]
struct PreprocessorState {
    opts: PreprocessOptions,
    /// Language the whole run lexes as: the TU's, for every header it
    /// includes (a header has no language of its own).
    language: Language,
    macros: MacroTable,
    /// Names in `macros` defined only by a builtin fallback (see
    /// `install_builtin_macros`). These expand normally but are invisible to
    /// `#ifdef` / `#ifndef` / `defined()` so an `#ifndef`-guarded real
    /// definition in source still takes effect; any source (re)definition or
    /// `#undef` clears the mark. Only ever shrinks during a run.
    fallback_macros: HashSet<String>,
    include_stack: Vec<PathBuf>,
    /// Canonical paths this run has processed, for
    /// `PreprocessResult::included_headers` and for the `files` set of each
    /// cached expansion. Membership alone is NOT a reason to skip a repeated
    /// `#include` — see `file_guards` (#56).
    included_files: HashSet<PathBuf>,
    /// The reason each processed path may be skipped when it is included
    /// again: its `#pragma once`, or the include guard wrapping it. A path
    /// absent here has no reason, and re-expands.
    ///
    /// A wrapper is read off the token stream before the file runs
    /// (`detect_include_guard`) and a `#pragma once` at the directive
    /// itself; either is inherited from `IncludeExpansion::guards` when a
    /// header is replayed from the cache instead of visited.
    file_guards: HashMap<PathBuf, crate::FileGuard>,
    /// How many times this run has brought each path in — expanded or
    /// replayed from the cache — against
    /// `PreprocessOptions::max_file_expansions`.
    file_expansions: HashMap<PathBuf, usize>,
    /// Included files this run expanded itself (see
    /// `PreprocessResult::inlined_headers`).
    inlined_files: HashSet<PathBuf>,
    /// The expansion this run resolved each processed path to — the variant
    /// replayed from the cache, or the one this run composed. With several
    /// variants per path the cache alone can no longer answer "what did this
    /// header expand to *here*", and both the guard-skip path and
    /// `compose_cache_text` need exactly that.
    entry_used: HashMap<PathBuf, crate::IncludeExpansion>,
    /// Variant index of the LATEST cache hit per path — what the guard-skip
    /// site records on an enclosing frame, and what
    /// `IncludeExpansion::nested_variants` inherits.
    variant_used: HashMap<PathBuf, usize>,
    /// Every `(path, variant)` this run replayed at an `#include` of that
    /// path itself, for `PreprocessResult::replayed_variants`. Distinct from
    /// `variant_used` because a header included twice under different macros
    /// replays two expansions and a map keyed by path can only name one.
    ///
    /// `IncludeExpansion::nested_variants` is deliberately not recorded
    /// here. Those records do still reach `replayed_variants`, through
    /// `variant_used`, which `finish` unions in — but only ever one variant
    /// per nested path, since `splice_cached` inserts them with
    /// `Entry::or_insert`. Keeping a second, per-pair set of them was the
    /// version that regressed: a path then accumulated a variant from every
    /// entry that mentioned it, the unit merged header units it never
    /// reached, and hdf lost 514 direct call edges to the resolution that
    /// shifted under.
    direct_variants: HashSet<(PathBuf, usize)>,
    conditional_stack: Vec<CondFrame>,
    /// Depth of `conditional_stack` when the current file started. Frames
    /// below it belong to includers: `#elif`/`#else`/`#endif` in this file
    /// may only act on frames at or above it (see `process_file_tokens`).
    cond_base: usize,
    output: String,
    line_map: LineMap,
    diagnostics: Vec<Diagnostic>,
    /// Diagnostic identities already emitted in this preprocessing run.
    /// Cached parent expansions can carry the same nested-header report;
    /// retain its first occurrence rather than multiplying it by cache path.
    diagnostic_keys: HashSet<(Option<PathBuf>, u32, String)>,
    current_file: PathBuf,
    current_line: u32,
    /// Bytes each processed file contributed to `output`. Files whose
    /// expansion was fully skipped (e.g. by an already-defined include
    /// guard) record 0 and must not be claimed as content-bearing by a
    /// parent's cached `IncludeExpansion::files`.
    emitted_bytes: HashMap<PathBuf, usize>,
    /// Interned index of `current_file` in `line_map.files`; `u32::MAX`
    /// means "not interned yet" (re-interned lazily when the file changes).
    lm_cur_file: u32,
    /// Current nested macro-expansion depth (hide-set rescan frames).
    expansion_depth: u32,
    expansion_limit_warned: bool,
    /// Token-loop iterations this run (macro rescan included).
    tokens_processed: u64,
    /// In-progress cached-header frames (warm pass). Guard-skipped includes
    /// are recorded here so the finished entry can embed nested expansions
    /// without copying them into live `output` (that exponentiates on
    /// diamond include graphs).
    cache_frames: Vec<CacheFrame>,
    /// Macro directives executed while any cache frame is open (nested
    /// replays included), in order. Each frame remembers its start index
    /// and captures its suffix into `IncludeExpansion::ops`; cleared when
    /// the last frame closes.
    macro_ops: Vec<MacroOp>,
    /// Memoized `binding_hash` per macro name, invalidated by
    /// `insert_macro` / `remove_macro`. A read hook that hashed a
    /// replacement list on every identifier occurrence would charge the
    /// header's whole token stream for each of its macros.
    macro_hashes: HashMap<String, u64>,
    /// Set once a run-wide limit (output cap, token budget, include depth)
    /// has cut an expansion short. Everything composed from here on is
    /// missing content, so nothing further may be published to the shared
    /// expansion cache.
    expansion_incomplete: bool,
    /// Conditional chains recorded so far (`record_conditionals`);
    /// `CondFrame::chain` indexes into it.
    conditionals: Vec<ConditionalChain>,
    /// While a condition is being evaluated with recording on: every name
    /// the evaluation consulted, in order, repeats included. `None`
    /// otherwise, which is what keeps the per-identifier read hook free.
    cond_reads: Option<Vec<ConditionRead>>,
    /// Does the current file have an include guard wrapping the whole of
    /// it? Decided by `detect_include_guard` when the file's token stream
    /// starts running. Per file: saved and restored around each include.
    guarded_file: bool,
}

/// One level of `#if`/`#elif`/`#else` nesting. A per-level bool is not
/// enough: `#elif`/`#else` must know whether *any* earlier branch in the
/// chain was taken (else the `#else` re-activates after a taken `#elif`),
/// and whether the enclosing context is active at all.
#[derive(Debug, Clone, Copy)]
struct CondFrame {
    /// Was the enclosing context active when the `#if` was seen?
    parent_active: bool,
    /// Is the branch currently being processed emitting tokens?
    active: bool,
    /// Has any branch of this chain been taken yet?
    taken: bool,
    /// Has `#else` been seen (a later `#elif` is malformed)?
    else_seen: bool,
    /// Line of the opening `#if`/`#ifdef`/`#ifndef`, for the EOF diagnostic.
    line: u32,
    /// The chain this frame is recording into, when recording is on.
    chain: Option<usize>,
}

/// One cached header being constructed.
#[derive(Debug)]
struct CacheFrame {
    /// Guard-skipped includes at the live-output offset of the `#include`.
    skips: Vec<(usize, PathBuf)>,
    /// Cached expansions this header replayed, for
    /// `IncludeExpansion::nested_variants`. Recorded on the innermost frame
    /// only: an enclosing entry pins this one, which pins these in turn, so
    /// the chain is closed by following it.
    replayed: Vec<(PathBuf, usize)>,
    /// The macro environment this header's expansion has read so far, which
    /// becomes `IncludeExpansion::deps`. Every open frame receives each read,
    /// so a nested header's dependencies reach its includers.
    deps: crate::MacroFingerprint,
    /// Names this frame has already decided: recorded in `deps`, or bound by
    /// the frame's own `#define` / `#undef` (its `ops`, which every consumer
    /// replays, so the consumer's binding cannot matter). Doubles as the
    /// short-circuit for the per-identifier read hook.
    settled: HashSet<Arc<str>>,
    /// Actual writes, distinguished from reads in `settled` when a skipped
    /// header snapshots a binding after merging its historical dependencies.
    locally_bound: HashSet<Arc<str>>,
    /// Include guards learned anywhere in this header's `#include` closure,
    /// becoming `IncludeExpansion::guards` (#56).
    guards: HashMap<PathBuf, crate::FileGuard>,
    /// Diagnostics in this header's transitive include closure. This must be
    /// independent from `PreprocessorState::diagnostics`: a report can have
    /// been emitted earlier in this run and still be required by a cache
    /// consumer that only includes this header later.
    diagnostics: Vec<Diagnostic>,
    diagnostic_keys: HashSet<(Option<PathBuf>, u32, String)>,
}

impl PreprocessorState {
    fn new(opts: PreprocessOptions, file: PathBuf) -> Self {
        let language = opts.language.unwrap_or_else(|| Language::from_path(&file));
        let mut state = Self {
            opts,
            language,
            macros: MacroTable::new(),
            fallback_macros: HashSet::new(),
            include_stack: vec![file.clone()],
            included_files: HashSet::new(),
            file_guards: HashMap::new(),
            file_expansions: HashMap::new(),
            inlined_files: HashSet::new(),
            entry_used: HashMap::new(),
            variant_used: HashMap::new(),
            direct_variants: HashSet::new(),
            conditional_stack: Vec::new(),
            cond_base: 0,
            output: String::new(),
            line_map: LineMap::new(),
            diagnostics: Vec::new(),
            diagnostic_keys: HashSet::new(),
            current_file: file,
            current_line: 1,
            emitted_bytes: HashMap::new(),
            lm_cur_file: u32::MAX,
            expansion_depth: 0,
            expansion_limit_warned: false,
            tokens_processed: 0,
            cache_frames: Vec::new(),
            macro_ops: Vec::new(),
            macro_hashes: HashMap::new(),
            expansion_incomplete: false,
            conditionals: Vec::new(),
            cond_reads: None,
            guarded_file: false,
        };
        if let Some(shared) = &state.opts.shared_macros {
            if let Ok(guard) = shared.read() {
                state.macros = guard.clone();
            }
            // The warm table is normally seeded from the predefines and the
            // CLI defines both (`macro_table_from_defines`), which is where
            // their relative precedence is usually decided; a name it never
            // accumulated must still beat the builtin fallback installed
            // below. Both seeds are first-wins, so what the warm pass picked
            // up from source outranks either. Order still matters for the
            // one case the warm table settles neither way — a caller whose
            // table binds neither name — so the CLI defines go first and
            // `-D __cplusplus=…` wins there too, as on the path below.
            state.init_cli_defines_missing_only();
            state.init_predefined_macros(true);
        } else {
            // Predefines first: a `-D __cplusplus=…` overrides the
            // language's own value.
            state.init_predefined_macros(false);
            state.init_cli_defines();
        }
        // Builtins are local to each preprocess so they apply even when
        // the shared warm table is cloned (hiview `__UNUSED` lives in .cpp
        // files, not in the header that `#ifndef`s it).
        state.install_builtin_macros();
        state
    }

    /// Install `BUILTIN_FALLBACK_MACROS`, each only when not already defined
    /// and marked in `fallback_macros` so conditionals do not see it and any
    /// real definition (CLI `-D`, source `#define`, cached include delta)
    /// replaces it.
    fn install_builtin_macros(&mut self) {
        for (name, def) in BUILTIN_FALLBACK_MACROS.iter() {
            if !self.macros.contains_key(name.as_str()) {
                self.macros.insert(name.clone(), def.clone());
                self.fallback_macros.insert(name.clone());
            }
        }
    }

    /// A name only a builtin fallback defines does not count as defined for
    /// `#ifdef` / `#ifndef` / `defined()`.
    fn is_defined_for_conditionals(&mut self, name: &str) -> bool {
        self.record_read(name);
        let bound = self.macros.contains_key(name) && !self.fallback_macros.contains(name);
        // Here the spelling is explicitly a macro operand, not an operator.
        // Ordinary expression reads omit unbound alternative operators.
        if !bound && is_alternative_token(name) {
            if let Some(reads) = self.cond_reads.as_mut() {
                reads.push(ConditionRead {
                    name: name.to_string(),
                    bound: Some(false),
                });
            }
        }
        bound
    }

    /// Content hash of what `name` is currently bound to, or `None` when it
    /// is unbound. The builtin-fallback mark is part of the binding: a
    /// fallback expands in text but reads as undefined in a conditional, so
    /// two environments that disagree about it can produce different
    /// expansions of the same header.
    fn binding_hash(&mut self, name: &str) -> Option<u64> {
        let def = self.macros.get(name)?;
        if let Some(h) = self.macro_hashes.get(name) {
            return Some(*h);
        }
        let h = hash_macro_binding(def, self.fallback_macros.contains(name));
        self.macro_hashes.insert(name.to_string(), h);
        Some(h)
    }

    /// Is `name` already settled for every open cache frame?
    ///
    /// Testing the innermost frame alone is enough, because an inner frame's
    /// `settled` set is a subset of every enclosing frame's: each of the
    /// writers (`record_read`, `note_local_binding`,
    /// `merge_recorded_deps`, `record_snapshot_read`) insert into all open
    /// frames that have not already settled the name, and a frame
    /// starts empty when it is pushed — so anything the innermost frame
    /// knows was settled while it existed, and every outer frame took the
    /// same insert. This is what keeps the per-identifier read hook at one
    /// hash lookup rather than one per level of `#include` nesting.
    fn innermost_settled(&self, name: &str) -> bool {
        self.cache_frames
            .last()
            .is_none_or(|frame| frame.settled.contains(name))
    }

    /// Record that the expansion consulted `name`, for every cached header
    /// currently being built. Free when no cache frame is open, which is the
    /// case for translation-unit text.
    ///
    /// Called on *every* macro-table consultation, the misses included: an
    /// identifier that found nothing here expanded to itself, and an entry
    /// holding that text must not be replayed into a unit where the name is
    /// a macro (#55).
    fn record_read(&mut self, name: &str) {
        if let Some(reads) = self.cond_reads.as_mut() {
            let bound = self.macros.contains_key(name) && !self.fallback_macros.contains(name);
            // `and` / `or` / `not` … spelled in a condition are operators,
            // not names the configuration could bind, unless something did.
            if bound || !is_alternative_token(name) {
                reads.push(ConditionRead {
                    name: name.to_string(),
                    bound: Some(bound),
                });
            }
        }
        if self.innermost_settled(name) {
            return;
        }
        let hash = self.binding_hash(name);
        let interned: Arc<str> = Arc::from(name);
        for frame in &mut self.cache_frames {
            if !frame.settled.insert(Arc::clone(&interned)) {
                continue;
            }
            match hash {
                Some(h) => frame.deps.defined.push((Arc::clone(&interned), h)),
                None => {
                    frame.deps.undefined.insert(Arc::clone(&interned));
                }
            }
        }
    }

    /// Mark `name` as bound by the cached headers being built, so a later
    /// read of it is not charged to the consumer's environment. Recorded
    /// even when the name is already a dependency — `settled` short-circuits
    /// that case and the earlier read stays in `deps`, which is correct: the
    /// consumer's binding did reach this expansion, before the header
    /// overwrote it.
    fn note_local_binding(&mut self, name: &str) {
        if self.cache_frames.is_empty() {
            return;
        }
        let interned: Arc<str> = Arc::from(name);
        for frame in &mut self.cache_frames {
            frame.settled.insert(Arc::clone(&interned));
            frame.locally_bound.insert(Arc::clone(&interned));
        }
    }

    /// A snapshot must retain both historical and current dependencies if
    /// they disagree. Such an entry cannot match any consumer, so that
    /// consumer expands it live. Ordinary `record_read` would discard the
    /// current dependency because the historical one already settled it.
    fn record_snapshot_read(&mut self, name: &str) {
        let hash = self.binding_hash(name);
        let interned: Arc<str> = Arc::from(name);
        for frame in &mut self.cache_frames {
            if frame.locally_bound.contains(name) {
                continue;
            }
            frame.deps.incompatible |= frame
                .deps
                .defined
                .iter()
                .any(|(n, h)| n == &interned && Some(*h) != hash)
                || (hash.is_some() && frame.deps.undefined.contains(name));
            frame.settled.insert(Arc::clone(&interned));
            match hash {
                Some(h) => {
                    if !frame
                        .deps
                        .defined
                        .iter()
                        .any(|(n, v)| n == &interned && *v == h)
                    {
                        frame.deps.defined.push((Arc::clone(&interned), h));
                    }
                }
                None => {
                    frame.deps.undefined.insert(Arc::clone(&interned));
                }
            }
        }
    }

    /// Fold a cached entry's dependencies into every open cache frame, so a
    /// header that embeds or replays a nested expansion inherits what that
    /// expansion read. Without this an entry matches its consumer's
    /// environment while a nested expansion frozen inside its text does not.
    ///
    /// The bindings are taken as *recorded*, not re-read from the current
    /// table. At a guard-skip site the nested entry was produced earlier in
    /// this run, possibly under a state this frame has since moved away
    /// from; the enclosing entry is only valid where that earlier state
    /// holds, and re-reading would claim otherwise. Names this frame binds
    /// itself are already `settled` and stay out.
    fn merge_recorded_deps(&mut self, deps: &crate::MacroFingerprint) {
        if self.cache_frames.is_empty() {
            return;
        }
        if deps.incompatible {
            for frame in &mut self.cache_frames {
                frame.deps.incompatible = true;
            }
        }
        for (name, hash) in &deps.defined {
            if self.innermost_settled(name) {
                continue;
            }
            for frame in &mut self.cache_frames {
                if frame.settled.insert(Arc::clone(name)) {
                    frame.deps.defined.push((Arc::clone(name), *hash));
                }
            }
        }
        for name in &deps.undefined {
            if self.innermost_settled(name) {
                continue;
            }
            for frame in &mut self.cache_frames {
                if frame.settled.insert(Arc::clone(name)) {
                    frame.deps.undefined.insert(Arc::clone(name));
                }
            }
        }
    }

    /// Does `deps` describe the environment this run is in right now?
    fn fingerprint_matches(&mut self, deps: &crate::MacroFingerprint) -> bool {
        if deps.incompatible {
            return false;
        }
        for (name, hash) in &deps.defined {
            if self.binding_hash(name) != Some(*hash) {
                return false;
            }
        }
        if deps.undefined.is_empty() {
            return true;
        }
        // Whichever side is smaller: an entry can depend on thousands of
        // names being unbound (every plain identifier in the header), while
        // a unit's own table is usually far smaller than that.
        if deps.undefined.len() <= self.macros.len() {
            !deps
                .undefined
                .iter()
                .any(|name| self.macros.contains_key(name.as_ref()))
        } else {
            !self
                .macros
                .keys()
                .any(|name| deps.undefined.contains(name.as_str()))
        }
    }

    /// Seed the language's own predefined macros (see
    /// [`crate::predefined_macros`]). Real definitions, not fallbacks: a
    /// C++ unit's `#ifdef __cplusplus` must be true. With `missing_only`
    /// a name the inherited warm table already binds is kept.
    fn init_predefined_macros(&mut self, missing_only: bool) {
        for (name, val) in crate::predefined_macros(self.language) {
            if missing_only && self.macros.contains_key(*name) {
                continue;
            }
            self.insert_macro(
                name.to_string(),
                MacroDef::Object {
                    replacement: lex_macro_body(val, self.language),
                },
            );
        }
    }

    fn init_cli_defines(&mut self) {
        let defines: Vec<_> = self
            .opts
            .defines
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, val) in defines {
            self.insert_macro(
                name,
                MacroDef::Object {
                    replacement: lex_macro_body(&val, self.language),
                },
            );
        }
    }

    fn init_cli_defines_missing_only(&mut self) {
        let defines: Vec<_> = self
            .opts
            .defines
            .iter()
            .filter(|(k, _)| !self.macros.contains_key(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, val) in defines {
            self.insert_macro(
                name,
                MacroDef::Object {
                    replacement: lex_macro_body(&val, self.language),
                },
            );
        }
    }

    /// Record a directive for the enclosing cached-header entries, if any.
    /// Logged unconditionally within a frame — even a `#undef` of an absent
    /// name is a no-op only locally and can still take effect in a
    /// translation unit that replays the entry.
    fn log_macro_op(&mut self, op: MacroOp) {
        if !self.cache_frames.is_empty() {
            self.macro_ops.push(op);
        }
    }

    fn insert_macro(&mut self, name: String, def: MacroDef) {
        self.log_macro_op(MacroOp::Define(name.clone(), def.clone()));
        self.note_local_binding(&name);
        self.macro_hashes.remove(&name);
        self.fallback_macros.remove(&name);
        self.macros.insert(name.clone(), def.clone());
        if self.opts.accumulate_macros {
            if let Some(shared) = &self.opts.shared_macros {
                if let Ok(mut guard) = shared.write() {
                    guard.insert(name, def);
                }
            }
        }
    }

    fn remove_macro(&mut self, name: &str) {
        self.log_macro_op(MacroOp::Undef(name.to_string()));
        self.note_local_binding(name);
        self.macro_hashes.remove(name);
        self.fallback_macros.remove(name);
        self.macros.shift_remove(name);
        if self.opts.accumulate_macros {
            if let Some(shared) = &self.opts.shared_macros {
                if let Ok(mut guard) = shared.write() {
                    guard.shift_remove(name);
                }
            }
        }
    }

    fn is_active(&self) -> bool {
        // A frame's `active` already folds in its parent's state at push /
        // re-evaluation time, so only the innermost frame needs checking.
        self.conditional_stack.last().is_none_or(|f| f.active)
    }

    /// Does the current file have an open conditional of its own for a
    /// `#elif`/`#else`/`#endif` to act on? Includer frames do not count.
    fn has_own_cond(&self) -> bool {
        self.conditional_stack.len() > self.cond_base
    }

    fn push_cond(&mut self, cond: bool, line: u32, chain: Option<usize>) {
        let parent_active = self.is_active();
        let active = parent_active && cond;
        self.conditional_stack.push(CondFrame {
            parent_active,
            active,
            taken: active,
            else_seen: false,
            line,
            chain,
        });
    }

    // ---- conditional recording (#57) ------------------------------------

    /// Start collecting the names a condition consults. A no-op unless
    /// recording is on, so `record_read` stays on its fast path.
    fn begin_reads(&mut self) {
        if self.opts.record_conditionals {
            self.cond_reads = Some(Vec::new());
        }
    }

    /// The names collected since `begin_reads`, first read first, once each.
    fn end_reads(&mut self) -> Vec<ConditionRead> {
        dedup_reads(self.cond_reads.take().unwrap_or_default())
    }

    /// Record the opening arm of a new chain; `None` when recording is off.
    /// The arm is built lazily so an ordinary run spells nothing.
    fn open_chain(&mut self, arm: impl FnOnce() -> ConditionalArm) -> Option<usize> {
        if !self.opts.record_conditionals {
            return None;
        }
        let depth = self.conditional_stack.len() - self.cond_base;
        self.conditionals.push(ConditionalChain {
            // `current_file` is the path include resolution produced, which
            // may spell the same header several ways (`src/../common/x.h`
            // from one includer, `common/x.h` from another). Consumers merge
            // chains across runs by file, so the record uses the canonical
            // path, as `included_headers` does. Canonicalization is cached.
            file: trace_ir::canonicalize(&self.current_file),
            arms: vec![arm()],
            depth,
            terminated: false,
            include_guard: false,
        });
        Some(self.conditionals.len() - 1)
    }

    /// End the chain's current arm at `line` (the line of the directive
    /// that follows it).
    fn close_arm(&mut self, chain: Option<usize>, line: u32) {
        if let Some(arm) = chain.and_then(|c| self.conditionals[c].arms.last_mut()) {
            arm.end_line = line;
        }
    }

    fn add_arm(&mut self, chain: Option<usize>, line: u32, arm: impl FnOnce() -> ConditionalArm) {
        self.close_arm(chain, line);
        if let Some(c) = chain {
            self.conditionals[c].arms.push(arm());
        }
    }

    /// Mark `chain` as this file's include guard, for the conditional
    /// record (#57). `start` indexes the opening directive's `#`.
    ///
    /// The whole shape is already decided — `guarded_file` is
    /// `detect_include_guard`'s answer for this file — so all that is left
    /// is to name the chain it belongs to: a guard's `#ifndef` is the file's
    /// first token, at its top level, so at most one chain can match.
    fn mark_include_guard(&mut self, chain: Option<usize>, tokens: &[Token], start: usize) {
        let Some(chain) = chain else { return };
        if self.guarded_file
            && self.conditional_stack.len() - 1 == self.cond_base
            && tokens[..start].iter().all(is_newline)
        {
            self.conditionals[chain].include_guard = true;
        }
    }

    // ---- include guards (#56) -------------------------------------------

    /// Learn why `path` need not be expanded again, and tell every cached
    /// header currently being built. A consumer replaying one of those
    /// entries never visits `path` itself, so the reason has to travel with
    /// the entry or the consumer re-expands a body the entry already holds.
    fn record_guard(&mut self, path: PathBuf, guard: crate::FileGuard) {
        // `#pragma once` outranks a wrapper and is never downgraded: it
        // holds for the rest of the run whatever the macro table does. It is
        // the *binding* that sticks, though, not the announcement — every
        // open frame still has to be told, including frames opened after the
        // pragma was first read. Returning early here left an entry carrying
        // such a header's body without the reason to skip it, and the
        // consumer expanded it a second time.
        let guard = match self.file_guards.get(&path) {
            Some(crate::FileGuard::Once) => crate::FileGuard::Once,
            _ => guard,
        };
        for frame in &mut self.cache_frames {
            frame.guards.insert(path.clone(), guard.clone());
        }
        self.file_guards.insert(path, guard);
    }

    /// Re-announce a guard this run already knows, for the entries opened
    /// since it was learned.
    fn propagate_guard(&mut self, path: &Path) {
        if let Some(guard) = self.file_guards.get(path).cloned() {
            self.record_guard(path.to_path_buf(), guard);
        }
    }

    /// Adopt the guards a cached expansion learned, for this run and for
    /// every entry being built around it.
    fn merge_guards(&mut self, guards: &[(PathBuf, crate::FileGuard)]) {
        for (path, guard) in guards {
            self.record_guard(path.clone(), guard.clone());
        }
    }

    /// May a repeated `#include` of `canonical` be skipped? Only on a
    /// reason the file itself stated.
    ///
    /// Note what this does NOT do: charge the guard's name to the enclosing
    /// cache entry's fingerprint. The skip does depend on the live table, so
    /// reading it there would be defensible, but a header that skips the ten
    /// its includer already pulled in would carry ten guard names and only
    /// match a consumer with the same include history — camera lost 879
    /// direct call edges to that, since past `max_expansion_variants`
    /// nothing publishes and the shared per-header units those declarations
    /// come from stop being built. The skip is kept self-contained the way
    /// the blanket suppression was instead: the site below snapshots the
    /// skipped expansion's macro effects into this entry's own `ops` and
    /// embeds its text, so a consumer reaches the same state either way.
    fn guard_suppresses(&self, canonical: &Path) -> bool {
        match self.file_guards.get(canonical) {
            Some(crate::FileGuard::Once) => true,
            Some(crate::FileGuard::Ifndef(name)) => {
                self.macros.contains_key(name.as_ref())
                    && !self.fallback_macros.contains(name.as_ref())
            }
            None => false,
        }
    }

    fn push_expansion(&mut self, line: u32) -> bool {
        if self.expansion_depth >= MAX_MACRO_EXPANSION_DEPTH {
            if !self.expansion_limit_warned {
                self.warn(
                    line,
                    format!(
                        "macro expansion depth exceeded ({MAX_MACRO_EXPANSION_DEPTH}); skipping further expansion"
                    ),
                );
                self.expansion_limit_warned = true;
            }
            return false;
        }
        self.expansion_depth += 1;
        true
    }

    fn pop_expansion(&mut self) {
        self.expansion_depth = self.expansion_depth.saturating_sub(1);
    }

    fn paint_replacement(tokens: &[Token], origin: &Token, name: &str) -> Vec<Token> {
        tokens
            .iter()
            .map(|t| t.with_macro_hide(origin, name))
            .collect()
    }

    /// Intern a path into the line-map file table (no-op if present).
    fn lm_intern(&mut self, path: &Path) -> u32 {
        self.line_map.intern_file(path)
    }

    /// Index of the current file in the line-map table, re-interned only
    /// when `current_file` changed since the last call.
    fn lm_current_file(&mut self) -> u32 {
        if self.lm_cur_file == u32::MAX
            || self.line_map.files.get(self.lm_cur_file as usize) != Some(&self.current_file)
        {
            self.lm_cur_file = self.line_map.intern_file(&self.current_file);
        }
        self.lm_cur_file
    }

    fn emit_token(&mut self, tok: &Token) {
        if matches!(tok.kind, TokenKind::Eof) {
            return;
        }
        if !matches!(tok.kind, TokenKind::Newline) && needs_leading_space(&self.output, &tok.kind) {
            self.output.push(' ');
        }
        let offset = self.output.len();
        let text = token_to_string(&tok.kind);
        self.output.push_str(&text);
        if self.opts.track_line_map {
            let fid = self.lm_current_file();
            let (line, col) = tok.expansion_site();
            self.line_map.push(offset, fid, line, col);
        }
        if matches!(tok.kind, TokenKind::Newline) {
            self.current_line += 1;
        }
    }

    fn emit_str(&mut self, s: &str, line: u32, col: u32) {
        let offset = self.output.len();
        self.output.push_str(s);
        if self.opts.track_line_map {
            let fid = self.lm_current_file();
            self.line_map.push(offset, fid, line, col);
        }
    }

    /// Emit `__FILE__` / `__LINE__` for `tok` if `name` is one of them.
    /// `__LINE__` is the line of the invocation (C11 6.10.8.1), also when
    /// the token comes from a macro body: `expansion_site` carries that
    /// through nested expansions.
    fn emit_predefined(&mut self, tok: &Token, name: &str) -> bool {
        let (line, col) = tok.expansion_site();
        match name {
            "__FILE__" => {
                let text = format!("\"{}\"", self.current_file.display());
                self.emit_str(&text, line, col);
                true
            }
            "__LINE__" => {
                self.emit_str(&line.to_string(), line, col);
                true
            }
            _ => false,
        }
    }

    fn report(&mut self, severity: DiagnosticSeverity, line: u32, message: String) {
        self.push_diagnostic(Diagnostic {
            severity,
            file: Some(self.current_file.clone()),
            line,
            message,
        });
    }

    fn push_diagnostic(&mut self, diagnostic: Diagnostic) {
        self.record_cache_diagnostic(&diagnostic);
        let key = (
            diagnostic.file.clone(),
            diagnostic.line,
            diagnostic.message.clone(),
        );
        if self.diagnostic_keys.insert(key) {
            self.diagnostics.push(diagnostic);
        }
    }

    /// Include a diagnostic in every open cache frame. A nested header's
    /// report is part of each enclosing header's cached expansion, even when
    /// result-level deduplication suppresses it for this particular run.
    fn record_cache_diagnostic(&mut self, diagnostic: &Diagnostic) {
        for frame in &mut self.cache_frames {
            let key = (
                diagnostic.file.clone(),
                diagnostic.line,
                diagnostic.message.clone(),
            );
            if frame.diagnostic_keys.insert(key) {
                frame.diagnostics.push(diagnostic.clone());
            }
        }
    }

    fn warn(&mut self, line: u32, message: impl Into<String>) {
        self.report(DiagnosticSeverity::Warning, line, message.into());
    }

    /// Records an error diagnostic and returns the matching hard error for
    /// callers that abort the current file.
    fn error(&mut self, line: u32, message: impl Into<String>) -> PreprocessError {
        let msg = message.into();
        self.report(DiagnosticSeverity::Error, line, msg.clone());
        PreprocessError::Message { message: msg }
    }

    /// Report something that cut this run's expansion short — a resource
    /// limit, or a file that stopped part-way through. Nothing composed from
    /// here on may reach the shared cache.
    fn limit_warn(&mut self, line: u32, message: impl Into<String>) {
        self.expansion_incomplete = true;
        self.warn(line, message);
    }

    /// The hard-error counterpart of `limit_warn`.
    fn limit_error(&mut self, line: u32, message: impl Into<String>) -> PreprocessError {
        self.expansion_incomplete = true;
        self.error(line, message)
    }

    fn check_resource_limits(&mut self, line: u32) -> Result<(), PreprocessError> {
        self.tokens_processed = self.tokens_processed.saturating_add(1);
        if self.tokens_processed > self.opts.max_expanded_tokens {
            return Err(self.limit_error(
                line,
                format!(
                    "preprocessed token budget exceeded ({})",
                    self.opts.max_expanded_tokens
                ),
            ));
        }
        if self.output.len() > self.opts.max_output_bytes {
            return Err(self.limit_error(
                line,
                format!(
                    "preprocessed output exceeded {} bytes",
                    self.opts.max_output_bytes
                ),
            ));
        }
        Ok(())
    }

    /// Charge `n` tokens to the expansion budget at once.
    ///
    /// `check_resource_limits` charges one token per loop iteration, which
    /// counts tokens *walked*. A function-like invocation walks O(1) — the
    /// argument list is skipped wholesale — and then materializes the whole
    /// replacement in `substitute_macro`, copying each argument once per
    /// parameter occurrence. Nothing charged for that, so peak allocation
    /// scaled with the source argument width no matter how small
    /// `max_expanded_tokens` was (issue #30). Charging the replacement
    /// before it is built makes the budget bound tokens *materialized*.
    fn charge_tokens(&mut self, n: u64, line: u32) -> Result<(), PreprocessError> {
        self.tokens_processed = self.tokens_processed.saturating_add(n);
        if self.tokens_processed > self.opts.max_expanded_tokens {
            return Err(self.limit_error(
                line,
                format!(
                    "preprocessed token budget exceeded ({})",
                    self.opts.max_expanded_tokens
                ),
            ));
        }
        Ok(())
    }

    /// Replay a cached expansion into the output. Returns false when no
    /// entry exists for `canonical`.
    fn splice_cached(&mut self, canonical: &Path) -> bool {
        // No stored expansion is valid here unless its fingerprint matches
        // this run's macro environment (#55). Returning false falls through
        // to a full expansion, which — with `inline_include_bodies` off —
        // puts the header's body in the consuming unit's own text, where it
        // is lowered under the environment that actually applies.
        let Some((at, entry)) = self.matching_variant(canonical) else {
            return false;
        };
        self.entry_used
            .insert(canonical.to_path_buf(), entry.clone());
        self.variant_used.insert(canonical.to_path_buf(), at);
        self.direct_variants.insert((canonical.to_path_buf(), at));
        if let Some(frame) = self.cache_frames.last_mut() {
            frame.replayed.push((canonical.to_path_buf(), at));
        }
        // Replaying an entry adopts its nested choices too: those headers
        // are never visited here, and without this the consumer has no
        // record for them and has to merge every stored expansion of each.
        for (path, at) in entry.nested_variants.iter() {
            self.variant_used.entry(path.clone()).or_insert(*at);
        }
        self.merge_recorded_deps(&entry.deps);
        // The consumer never visits the headers this entry covers, so the
        // reasons they may be skipped have to come from the entry (#56).
        let guards = Arc::clone(&entry.guards);
        self.merge_guards(&guards);
        for diagnostic in entry.diagnostics.iter().cloned() {
            self.push_diagnostic(diagnostic);
        }
        if !self.opts.inline_include_bodies {
            self.replay_macro_delta(&entry);
            self.included_files.insert(canonical.to_path_buf());
            self.included_files.extend(entry.files.iter().cloned());
            return true;
        }
        if self.output.len().saturating_add(entry.text.len()) > self.opts.max_output_bytes {
            self.limit_warn(
                1,
                format!(
                    "skipping cached include {} (would exceed {}-byte output cap)",
                    canonical.display(),
                    self.opts.max_output_bytes
                ),
            );
            self.included_files.insert(canonical.to_path_buf());
            return true;
        }
        let offset = self.output.len();
        self.output.push_str(&entry.text);
        // Replay the entry's macro side effects. Cached text is spliced
        // without executing the header's directives, so without this a
        // consumer sees none of the macros the header defines — later
        // warm passes then expand dependent headers against a starved
        // table and freeze unexpanded invocations into their own cache
        // entries. Replay mirrors live execution (see replay_macro_delta).
        self.replay_macro_delta(&entry);
        if self.opts.track_line_map {
            // Renumber the cached expansion's file indices into this run's
            // intern table, then splice its entries.
            let mut remap = Vec::with_capacity(entry.line_map.files.len());
            for p in &entry.line_map.files {
                remap.push(self.lm_intern(p));
            }
            let sub = &entry.line_map;
            self.line_map.splice(sub, offset, &remap);
        }
        self.included_files.extend(entry.files.iter().cloned());
        true
    }

    /// Replay a cached include's macro directives in program order, through
    /// the same mutation helpers live `#define` / `#undef` use — so a cache
    /// hit and a cache miss agree on everything a directive touches: the
    /// local table (overwrite semantics), the fallback marks, the shared
    /// table under `accumulate_macros`, and the op log feeding an enclosing
    /// cached header's own entry.
    fn replay_macro_delta(&mut self, entry: &crate::IncludeExpansion) {
        for op in entry.ops.iter() {
            match op {
                MacroOp::Undef(name) => self.remove_macro(name),
                MacroOp::Define(name, def) => self.insert_macro(name.clone(), def.clone()),
            }
        }
    }

    /// Offer `entry` to the shared cache as an expansion of `canonical`
    /// under the environment its fingerprint records.
    ///
    /// Two writers that reached the same fingerprint produce the same
    /// expansion — that is what a fingerprint means — so a race between
    /// them is not observable, and the append-only list keeps every index
    /// already handed out valid. Past the cap nothing is stored and later
    /// consumers expand the header themselves, which costs time and changes
    /// no output.
    fn publish_variant(&self, canonical: PathBuf, entry: crate::IncludeExpansion) {
        // Keep incompatible expansions only in entry_used for this run's
        // text composition; they cannot serve any consumer or use a slot.
        if entry.deps.incompatible {
            return;
        }
        let Some(cache) = self.opts.include_expansion_cache.as_ref() else {
            return;
        };
        let Ok(mut guard) = cache.write() else {
            return;
        };
        let variants = guard.entry((canonical, self.language)).or_default();
        let signature = entry.deps.signature();
        if variants.len() >= self.opts.max_expansion_variants
            || variants.iter().any(|v| v.deps.signature() == signature)
        {
            return;
        }
        variants.push(entry);
    }

    /// The stored expansion of `canonical` whose fingerprint this run's
    /// macro environment satisfies, if any.
    ///
    /// Matching needs `&mut self` (it memoizes binding hashes), so the
    /// fingerprints are lifted out under one read lock and the winner
    /// re-read under a second. That is sound because [`ExpansionVariants`]
    /// is append-only: a concurrent writer can lengthen the list, never
    /// move what an index already refers to.
    fn matching_variant(&mut self, canonical: &Path) -> Option<(usize, crate::IncludeExpansion)> {
        let key = (canonical.to_path_buf(), self.language);
        let deps: Vec<Arc<crate::MacroFingerprint>> = {
            let cache = self.opts.include_expansion_cache.as_ref()?;
            let guard = cache.read().ok()?;
            guard
                .get(&key)?
                .iter()
                .map(|e| Arc::clone(&e.deps))
                .collect()
        };
        let at = deps.iter().position(|d| self.fingerprint_matches(d))?;
        let cache = self.opts.include_expansion_cache.as_ref()?;
        let guard = cache.read().ok()?;
        Some((at, guard.get(&key)?.get(at).cloned()?))
    }

    fn is_cacheable_header(path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            matches!(e, "h" | "H" | "hpp" | "hh" | "hxx" | "inl" | "ipp")
                || e.eq_ignore_ascii_case("h")
        })
    }

    /// Self-contained cache blob: live unique text plus nested expansions
    /// inserted at each first guard-skip include site.
    fn compose_cache_text(
        &self,
        output_start: usize,
        output_end: usize,
        skips: &[(usize, PathBuf)],
    ) -> (String, LineMap, HashSet<PathBuf>) {
        if skips.is_empty() {
            return (
                self.output[output_start..output_end].to_string(),
                self.line_map.slice_from(output_start),
                HashSet::new(),
            );
        }
        let mut text = String::new();
        let mut line_map = LineMap::new();
        let mut extra_files = HashSet::new();
        let mut live_pos = output_start;
        let mut embedded: HashSet<PathBuf> = HashSet::new();
        for (at, path) in skips {
            let at = (*at).min(output_end).max(live_pos);
            Self::append_live_chunk(
                &mut text,
                &mut line_map,
                &self.output,
                &self.line_map,
                live_pos,
                at,
            );
            live_pos = at;
            if !embedded.insert(path.clone()) {
                continue;
            }
            let Some(entry) = self.entry_used.get(path) else {
                continue;
            };
            if text.len().saturating_add(entry.text.len()) > self.opts.max_output_bytes {
                continue;
            }
            extra_files.extend(entry.files.iter().cloned());
            extra_files.insert(path.clone());
            if self.opts.track_line_map {
                let mut remap = Vec::with_capacity(entry.line_map.files.len());
                for p in &entry.line_map.files {
                    remap.push(line_map.intern_file(p));
                }
                line_map.splice(&entry.line_map, text.len(), &remap);
            }
            text.push_str(&entry.text);
        }
        Self::append_live_chunk(
            &mut text,
            &mut line_map,
            &self.output,
            &self.line_map,
            live_pos,
            output_end,
        );
        (text, line_map, extra_files)
    }

    fn append_live_chunk(
        dest_text: &mut String,
        dest_map: &mut LineMap,
        src_text: &str,
        src_map: &LineMap,
        from: usize,
        to: usize,
    ) {
        if from >= to {
            return;
        }
        let dest_off = dest_text.len();
        dest_text.push_str(&src_text[from..to]);
        let chunk_len = to - from;
        let sliced = src_map.slice_from(from);
        let mut remap = Vec::with_capacity(sliced.files.len());
        for p in &sliced.files {
            remap.push(dest_map.intern_file(p));
        }
        for e in &sliced.entries {
            if (e.output_offset as usize) >= chunk_len {
                break;
            }
            dest_map.entries.push(crate::LineMapEntry {
                output_offset: e.output_offset + dest_off as u32,
                file: remap[e.file as usize],
                line: e.line,
                col: e.col,
            });
        }
    }

    fn process_file(&mut self, path: &Path) -> Result<(), PreprocessError> {
        let canonical = trace_ir::canonicalize(path);
        // A repeated `#include` is skipped only on a reason the file itself
        // stated — its `#pragma once`, or its include guard while that guard
        // is defined. Membership in `included_files` is not one: suppressing
        // on it lost every deliberate re-inclusion, X-macro tables above all
        // (#56).
        if self.guard_suppresses(&canonical) {
            // Re-splicing the cached subtree into *live* output exponentiates
            // on diamond include graphs (each skip copies a self-contained
            // blob that already contains previous copies). Record the skip on
            // the in-progress cache frame instead; `compose_cache_text`
            // embeds the nested expansion only into that frame's cache entry.
            if !self.opts.frozen_expansion_cache {
                // The reason this include was skipped belongs to every entry
                // being built around it, along with the reasons for
                // everything the skipped expansion covered.
                self.propagate_guard(&canonical);
                if let Some(guards) = self
                    .entry_used
                    .get(&canonical)
                    .map(|e| Arc::clone(&e.guards))
                {
                    self.merge_guards(&guards);
                }
                let replayed_as = self.variant_used.get(&canonical).copied();
                if let Some(frame) = self.cache_frames.last_mut() {
                    frame.skips.push((self.output.len(), canonical.clone()));
                    // The body is skipped here, but this header still needs
                    // its own lowered unit and only the entry being built
                    // knows which expansion of it applies.
                    if let Some(at) = replayed_as {
                        frame.replayed.push((canonical.clone(), at));
                    }
                }
                if let Some(entry) = self.entry_used.get(&canonical).cloned() {
                    for diagnostic in entry.diagnostics.iter() {
                        self.record_cache_diagnostic(diagnostic);
                    }
                    // `compose_cache_text` will embed this expansion's text
                    // in the frame's entry, so the frame depends on
                    // everything the expansion read.
                    self.merge_recorded_deps(&entry.deps);
                    // Keep the embedded header's macro effects self-contained,
                    // but preserve intervening changes: in B, D, B the skipped
                    // B must not undo D's definitions or undefs. Snapshot the
                    // current binding of each affected name, without executing
                    // it. External bindings also constrain this frame's cache
                    // fingerprint; locally written names are already settled.
                    let mut seen = HashSet::new();
                    // Only the final operation for each name matters here.
                    for op in entry.ops.iter().rev() {
                        let (MacroOp::Define(name, _) | MacroOp::Undef(name)) = op;
                        if !seen.insert(name) {
                            continue;
                        }
                        let original = match op {
                            MacroOp::Define(_, def) => Some(hash_macro_binding(def, false)),
                            MacroOp::Undef(_) => None,
                        };
                        // An unchanged binding is already supplied by B's
                        // own final directive. In particular, do not require
                        // an ordinary include guard to be both absent (B's
                        // input) and present (B's effect) in the consumer.
                        if self.binding_hash(name) != original {
                            self.record_snapshot_read(name);
                        }
                        let current = match self.macros.get(name) {
                            Some(def) => MacroOp::Define(name.clone(), def.clone()),
                            None => MacroOp::Undef(name.clone()),
                        };
                        self.log_macro_op(current);
                    }
                }
            }
            return Ok(());
        }

        if self.include_stack.len() >= self.opts.max_include_depth {
            self.limit_warn(
                1,
                format!(
                    "include depth exceeded ({}); skipping {}",
                    self.opts.max_include_depth,
                    path.display()
                ),
            );
            return Ok(());
        }

        // Depth does not bound repetition: an unguarded file included by
        // fifty siblings is expanded fifty times at depth two, and a pair of
        // unguarded files that include each other recurs until the depth cap
        // at every one of those sites. Cap the expansions of a single path
        // and say so, rather than losing the content silently the way the
        // blanket suppression did (#56).
        let expansions = self.file_expansions.entry(canonical.clone()).or_insert(0);
        *expansions += 1;
        if *expansions > self.opts.max_file_expansions {
            if *expansions == self.opts.max_file_expansions + 1 {
                self.limit_warn(
                    1,
                    format!(
                        "include expanded more than {} times ({}); skipping further inclusions",
                        self.opts.max_file_expansions,
                        path.display()
                    ),
                );
            }
            return Ok(());
        }

        // The root of this run is the file whose text IS the output, so it
        // is never replayed from the expansion cache: an entry for it can
        // already exist when another header pulled it in first (a
        // macro-spelled `#include` the include graph could not order), and
        // with `inline_include_bodies` off a replay would yield an empty
        // run. `include_stack` holds only the root at this point (see
        // `Preprocessor::new`); nested includes push on top of it.
        let is_root = self.include_stack.len() == 1;
        if !is_root && self.splice_cached(&canonical) {
            return Ok(());
        }

        if !is_root {
            // Past every cache path: this run is about to expand the body
            // itself, so the header's content belongs to this unit's text.
            self.inlined_files.insert(canonical.clone());
        }

        let cache_header =
            self.opts.include_expansion_cache.is_some() && Self::is_cacheable_header(&canonical);

        let guard_snapshot = if cache_header {
            self.included_files.clone()
        } else {
            HashSet::new()
        };
        // Everything this header's processing executes (`#define`/`#undef`,
        // nested replays included) from here on lands in `macro_ops`; the
        // suffix becomes the entry's `IncludeExpansion::ops`, replayed by
        // `splice_cached`.
        let ops_start = if cache_header && !self.opts.frozen_expansion_cache {
            Some(self.macro_ops.len())
        } else {
            None
        };
        self.included_files.insert(canonical.clone());
        let output_start = self.output.len();
        let pushing_frame = cache_header && !self.opts.frozen_expansion_cache;

        let content: Arc<str> = if let Some(cache) = &self.opts.source_cache {
            let key = canonical.clone();
            if let Some(s) = cache.get(&key) {
                Arc::clone(s)
            } else {
                fs::read_to_string(path)
                    .map_err(|source| PreprocessError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?
                    .into()
            }
        } else {
            fs::read_to_string(path)
                .map_err(|source| PreprocessError::Io {
                    path: path.to_path_buf(),
                    source,
                })?
                .into()
        };

        // Only now that the source is in hand: reading it can fail with `?`,
        // and a frame pushed before that would be left on the stack for the
        // enclosing header to pop as if it were its own.
        if pushing_frame {
            self.cache_frames.push(CacheFrame {
                skips: Vec::new(),
                replayed: Vec::new(),
                deps: crate::MacroFingerprint::default(),
                settled: HashSet::new(),
                locally_bound: HashSet::new(),
                guards: HashMap::new(),
                diagnostics: Vec::new(),
                diagnostic_keys: HashSet::new(),
            });
        }

        let prev_file = self.current_file.clone();
        self.current_file = path.to_path_buf();
        self.include_stack.push(path.to_path_buf());

        let tokens = Lexer::new(&content, self.language).tokenize();
        if let Err(e) = self.process_file_tokens(&tokens) {
            // Attribute the stop to the file being processed when it failed,
            // not the including TU — downstream consumers key fallback and
            // reporting decisions off this message.
            self.limit_warn(
                1,
                format!("preprocess stopped in {}: {e}", self.current_file.display()),
            );
        }

        self.include_stack.pop();
        self.current_file = prev_file;

        let emitted = self.output.len() - output_start;
        self.emitted_bytes.insert(canonical.clone(), emitted);
        let pending_skips = self.cache_frames.last().map(|f| f.skips.len()).unwrap_or(0);
        if cache_header
            && !self.opts.frozen_expansion_cache
            && emitted == 0
            && pending_skips == 0
            && content.chars().any(|c| !c.is_whitespace())
        {
            // The include resolved but its entire body was skipped — almost
            // always an include guard already defined in the shared macro
            // environment. Content silently missing from a cached expansion
            // is the failure mode that starves translation units later, so
            // make it visible during the (sequential) warm/index phases.
            self.push_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: Some(path.to_path_buf()),
                line: 1,
                message: "resolved include expanded to nothing (guard already defined?)".into(),
            });
        }

        if cache_header && !self.opts.frozen_expansion_cache {
            let frame = self
                .cache_frames
                .pop()
                .expect("cacheable header has an active cache frame");
            // An expansion composed after a run-wide limit cut this run
            // short is missing content; publishing it would hand that
            // truncation to every later consumer of the header.
            if !self.expansion_incomplete && self.opts.include_expansion_cache.is_some() {
                let output_end = self.output.len();
                let (composed, composed_map, extra_files) = if self.opts.inline_include_bodies {
                    self.compose_cache_text(output_start, output_end, &frame.skips)
                } else {
                    (
                        self.output[output_start..output_end].to_string(),
                        self.line_map.slice_from(output_start),
                        HashSet::new(),
                    )
                };
                let mut new_files: HashSet<PathBuf> = self
                    .included_files
                    .difference(&guard_snapshot)
                    .filter(|p| self.emitted_bytes.get(*p).copied().unwrap_or(0) > 0)
                    .cloned()
                    .collect();
                new_files.extend(extra_files);
                let ops: Arc<Vec<MacroOp>> = match ops_start {
                    Some(start) => Arc::new(self.macro_ops[start..].to_vec()),
                    None => Arc::default(),
                };
                let diagnostics: Arc<Vec<Diagnostic>> = Arc::new(frame.diagnostics);
                let deps = frame.deps;
                let replayed = frame.replayed;
                // Only for the files this entry actually covers. A frame
                // also learns the guards of headers it skipped because the
                // includer had already pulled them in — those contribute no
                // text here, so handing them to a consumer would suppress an
                // `#include` whose body the consumer never received. Camera
                // lost 1,064 direct call edges to exactly that.
                let mut guards: Vec<(PathBuf, crate::FileGuard)> = frame
                    .guards
                    .into_iter()
                    .filter(|(p, _)| *p == canonical || new_files.contains(p))
                    .collect();
                // The map's iteration order is not stable across runs and
                // the index must be bit-reproducible.
                guards.sort_by(|a, b| a.0.cmp(&b.0));
                // Diagnostics alone are not content. An entry holding nothing
                // else replays as an empty expansion, and `splice_cached`
                // reports the hit as a success, so every later consumer that
                // reaches this header with its guard undefined silently loses
                // the body. Leave it uncached and let those runs expand it.
                if !composed.is_empty() || !ops.is_empty() || !new_files.is_empty() {
                    // Which expansion of each header this one pulled in, so
                    // a consumer that replays this entry can lower the same
                    // ones (it never visits them itself). Taken from the
                    // frame rather than from `new_files`, which only counts
                    // files this run expanded — a nested header that was
                    // itself replayed never records emitted bytes and would
                    // drop out.
                    // Closed over nesting, so one lookup answers for the
                    // whole subtree: entries are built bottom-up, so each
                    // one this replayed is already closed, and unioning
                    // their records keeps the property.
                    let mut nested_variants: Vec<(PathBuf, usize)> = Vec::new();
                    let mut seen: HashSet<PathBuf> = HashSet::new();
                    for (nested_path, nested_at) in replayed {
                        if let Some(deeper) = self
                            .entry_used
                            .get(&nested_path)
                            .map(|e| Arc::clone(&e.nested_variants))
                        {
                            for (p, v) in deeper.iter() {
                                if seen.insert(p.clone()) {
                                    nested_variants.push((p.clone(), *v));
                                }
                            }
                        }
                        if seen.insert(nested_path.clone()) {
                            nested_variants.push((nested_path, nested_at));
                        }
                    }
                    let entry = crate::IncludeExpansion {
                        text: composed.into(),
                        files: Arc::new(new_files),
                        diagnostics,
                        line_map: Arc::new(composed_map),
                        ops,
                        deps: Arc::new(deps),
                        guards: Arc::new(guards),
                        nested_variants: Arc::new(nested_variants),
                    };
                    self.entry_used.insert(canonical.clone(), entry.clone());
                    self.publish_variant(canonical, entry);
                }
            }
            // The log only feeds open frames; once the outermost cached
            // header closes, nothing references these entries any more.
            if self.cache_frames.is_empty() {
                self.macro_ops.clear();
            }
        }

        Ok(())
    }

    /// Runs one file's token stream with `conditional_stack` fenced at the
    /// depth it had on entry. In the C11 6.10 grammar an `if-section` is a
    /// group of the `preprocessing-file` that contains it, so a conditional
    /// cannot span files in either direction: a closing or branch directive
    /// here may not act on an includer's frame (`cond_base`), and frames
    /// still open at the end belong to this file alone. Report each of
    /// those at its directive and pop them, leaving the includer in the
    /// state it had at the `#include`. Without this an unterminated `#if 0`
    /// in a header silently blanked the rest of the translation unit, and a
    /// stray `#endif` in a header consumed the includer's frame so its own
    /// `#endif` then failed (#8). A file that stopped early is rebalanced
    /// the same way but keeps only its stop diagnostic: its unprocessed
    /// remainder may still hold the `#endif`.
    fn process_file_tokens(&mut self, tokens: &[Token]) -> Result<(), PreprocessError> {
        let depth = self.conditional_stack.len();
        let outer_base = std::mem::replace(&mut self.cond_base, depth);
        // Before the token loop, not at the closing `#endif`: a guarded
        // header that reaches itself back through another header re-enters
        // while its own `#endif` is still ahead, and a guard learned on the
        // way out cannot answer for that inclusion (#56).
        let guard = detect_include_guard(tokens);
        let outer_guarded = std::mem::replace(&mut self.guarded_file, guard.is_some());
        if let Some(name) = guard {
            let path = trace_ir::canonicalize(&self.current_file);
            self.record_guard(path, crate::FileGuard::Ifndef(name));
        }
        let result = self.process_tokens(tokens);
        self.cond_base = outer_base;
        self.guarded_file = outer_guarded;
        if result.is_ok() {
            for idx in depth..self.conditional_stack.len() {
                let line = self.conditional_stack[idx].line;
                self.report(
                    DiagnosticSeverity::Error,
                    line,
                    "unterminated #if; conditional closed at end of file".into(),
                );
            }
        }
        // A chain left open (or cut short by an error) ends where the file
        // does: the line after the last one, so that line is in the arm.
        // The EOF token sits on the line after a trailing newline, but on
        // the last line itself when there is none.
        // Comments and whitespace do not produce tokens, so the token
        // before EOF cannot tell us whether the physical last line is empty.
        let last_line = tokens
            .last()
            .map_or(1, |eof| eof.line + u32::from(eof.col > 1));
        for idx in depth..self.conditional_stack.len() {
            let chain = self.conditional_stack[idx].chain;
            self.close_arm(chain, last_line);
        }
        self.conditional_stack.truncate(depth);
        result
    }

    fn process_tokens(&mut self, tokens: &[Token]) -> Result<(), PreprocessError> {
        let mut i = 0;
        while i < tokens.len() {
            self.check_resource_limits(tokens[i].line)?;
            let tok = &tokens[i];
            if matches!(tok.kind, TokenKind::Eof) {
                break;
            }

            if matches!(tok.kind, TokenKind::Hash) && at_beginning_of_line(tokens, i) {
                i = self.handle_directive(tokens, i)?;
                continue;
            }

            if self.is_active() {
                if let TokenKind::Identifier(name) = &tok.kind {
                    if self.emit_predefined(tok, name) {
                        i += 1;
                        continue;
                    }
                    if !tok.is_hidden(name) {
                        self.record_read(name);
                        if let Some(macro_def) = self.macros.get(name).cloned() {
                            match macro_def {
                                MacroDef::Function { .. } | MacroDef::GmockMethod => {
                                    if self.next_non_newline_is(tokens, i + 1, "(") {
                                        if !self.push_expansion(tok.line) {
                                            self.emit_token(tok);
                                            i += 1;
                                            continue;
                                        }
                                        i += 1;
                                        let args = match self.parse_macro_args(tokens, &mut i) {
                                            Ok(a) => a,
                                            Err(e) => {
                                                self.pop_expansion();
                                                return Err(e);
                                            }
                                        };
                                        let r = self
                                            .expand_invocation(name, tok, &macro_def, &args)
                                            .and_then(|expanded| self.process_tokens(&expanded));
                                        self.pop_expansion();
                                        r?;
                                        continue;
                                    }
                                    self.emit_token(tok);
                                }
                                MacroDef::Object { replacement } => {
                                    if !self.push_expansion(tok.line) {
                                        self.emit_token(tok);
                                        i += 1;
                                        continue;
                                    }
                                    let painted = Self::paint_replacement(&replacement, tok, name);
                                    let r = self.expand_tokens_no_directives(&painted);
                                    self.pop_expansion();
                                    r?;
                                    i += 1;
                                    continue;
                                }
                            }
                        } else {
                            self.emit_token(tok);
                        }
                    } else {
                        self.emit_token(tok);
                    }
                } else {
                    self.emit_token(tok);
                }
            }
            i += 1;
        }
        Ok(())
    }

    /// Expand macro replacement tokens: no `#` directives (a `#` here is
    /// emitted verbatim — `#param` was already stringized by
    /// `substitute_macro`); recurse into object macros.
    fn expand_tokens_no_directives(&mut self, tokens: &[Token]) -> Result<(), PreprocessError> {
        let mut i = 0;
        while i < tokens.len() {
            self.check_resource_limits(tokens[i].line)?;
            let tok = &tokens[i];
            if matches!(tok.kind, TokenKind::Eof) {
                break;
            }
            if matches!(tok.kind, TokenKind::Hash) {
                self.emit_token(tok);
                i += 1;
                continue;
            }
            if self.is_active() {
                if let TokenKind::Identifier(name) = &tok.kind {
                    if self.emit_predefined(tok, name) {
                        i += 1;
                        continue;
                    }
                    if !tok.is_hidden(name) {
                        self.record_read(name);
                        match self.macros.get(name).cloned() {
                            Some(MacroDef::Object { replacement }) => {
                                if !self.push_expansion(tok.line) {
                                    self.emit_token(tok);
                                    i += 1;
                                    continue;
                                }
                                let painted = Self::paint_replacement(&replacement, tok, name);
                                let r = self.expand_tokens_no_directives(&painted);
                                self.pop_expansion();
                                r?;
                                i += 1;
                                continue;
                            }
                            // Function-like macros appearing inside another
                            // macro's expansion must be invoked and their
                            // expansion rescanned (C11 6.10.3.4); otherwise
                            // nested definitions like
                            // `#define A SHARED_OBJ(T)` leak `SHARED_OBJ(T)`
                            // verbatim into the output.
                            Some(
                                macro_def @ (MacroDef::Function { .. } | MacroDef::GmockMethod),
                            ) if self.next_non_newline_is(tokens, i + 1, "(") => {
                                if !self.push_expansion(tok.line) {
                                    self.emit_token(tok);
                                    i += 1;
                                    continue;
                                }
                                let mut j = i + 1;
                                let args = match self.parse_macro_args(tokens, &mut j) {
                                    Ok(a) => a,
                                    Err(e) => {
                                        self.pop_expansion();
                                        return Err(e);
                                    }
                                };
                                let r = self
                                    .expand_invocation(name, tok, &macro_def, &args)
                                    .and_then(|expanded| {
                                        self.expand_tokens_no_directives(&expanded)
                                    });
                                self.pop_expansion();
                                r?;
                                i = j;
                                continue;
                            }
                            Some(MacroDef::Function { .. } | MacroDef::GmockMethod) | None => {}
                        }
                    }
                }
                self.emit_token(tok);
            }
            i += 1;
        }
        Ok(())
    }

    fn handle_directive(
        &mut self,
        tokens: &[Token],
        start: usize,
    ) -> Result<usize, PreprocessError> {
        let mut i = start + 1;
        // skip to directive name (may be on next line)
        while i < tokens.len() && matches!(tokens[i].kind, TokenKind::Newline) {
            i += 1;
        }
        if i >= tokens.len() {
            return Ok(i);
        }

        let directive = match &tokens[i].kind {
            TokenKind::Identifier(s) => s.clone(),
            // Inside a skipped group only the nesting matters (C11
            // 6.10.1p6): a line marker (`# 1 "x.c"`) or a `#!` line there
            // must not abort the file, which would also record the arm as
            // excluding everything to the end of the file.
            _ if !self.is_active() => {
                skip_directive_line(tokens, &mut i);
                return Ok(i);
            }
            _ => {
                return Err(self.error(tokens[i].line, "expected directive name after #"));
            }
        };
        let line = tokens[i].line;
        let record_line = tokens[start].line;
        i += 1;

        match directive.as_str() {
            "include" if self.is_active() => {
                i = self.handle_include(tokens, i)?;
            }
            "define" if self.is_active() => {
                i = self.handle_define(tokens, i)?;
            }
            "include" | "define" if !self.is_active() => {}
            // Inside a skipped group only the nesting matters (C11
            // 6.10.1p6): a malformed operand there must not abort the file.
            "ifdef" | "ifndef" => {
                let negate = directive == "ifndef";
                let directive = if negate {
                    ArmDirective::Ifndef
                } else {
                    ArmDirective::Ifdef
                };
                if self.is_active() {
                    let name = self.read_directive_ident(tokens, &mut i)?;
                    self.begin_reads();
                    let defined = self.is_defined_for_conditionals(&name);
                    let reads = self.end_reads();
                    let cond = defined != negate;
                    let chain = self.open_chain(|| ConditionalArm {
                        directive,
                        expression: name.clone(),
                        line: record_line,
                        end_line: record_line,
                        outcome: arm_outcome(true, cond),
                        evaluated: true,
                        reads,
                    });
                    self.push_cond(cond, line, chain);
                    if negate {
                        self.mark_include_guard(chain, tokens, start);
                    }
                } else {
                    let rest = directive_rest(tokens, i);
                    let operand = &rest[..rest.len().min(1)];
                    let chain = self.open_chain(|| ConditionalArm {
                        directive,
                        expression: operand
                            .first()
                            .and_then(ident_name)
                            .unwrap_or("")
                            .to_string(),
                        line: record_line,
                        end_line: record_line,
                        outcome: ArmOutcome::Unevaluated,
                        evaluated: false,
                        reads: operand
                            .iter()
                            .filter_map(ident_name)
                            .map(|name| ConditionRead {
                                name: name.to_string(),
                                bound: None,
                            })
                            .collect(),
                    });
                    self.push_cond(false, line, chain);
                }
            }
            "if" => {
                // Conditions in skipped groups are not evaluated (C11
                // 6.10.1p6); the frame still pushes to keep nesting balanced.
                let evaluate = self.is_active();
                let expr_start = i;
                let cond = if evaluate {
                    self.begin_reads();
                    self.expand_and_eval_condition(tokens, &mut i)
                } else {
                    skip_directive_line(tokens, &mut i);
                    false
                };
                let expr = &tokens[expr_start..i];
                let reads = self.end_reads();
                let chain = self.open_chain(|| ConditionalArm {
                    directive: ArmDirective::If,
                    expression: spell_condition(expr),
                    line: record_line,
                    end_line: record_line,
                    outcome: arm_outcome(evaluate, cond),
                    evaluated: evaluate,
                    reads: if evaluate { reads } else { spelled_reads(expr) },
                });
                self.push_cond(cond, line, chain);
                if evaluate && negated_defined_name(expr).is_some() {
                    self.mark_include_guard(chain, tokens, start);
                }
            }
            "elif" => {
                if !self.has_own_cond() {
                    return Err(self.error(line, "#elif without #if"));
                }
                let frame = *self.conditional_stack.last().unwrap();
                let evaluate = frame.parent_active && !frame.taken && !frame.else_seen;
                let expr_start = i;
                let cond = if evaluate {
                    self.begin_reads();
                    self.expand_and_eval_condition(tokens, &mut i)
                } else {
                    skip_directive_line(tokens, &mut i);
                    false
                };
                let expr = &tokens[expr_start..i];
                let reads = self.end_reads();
                if frame.else_seen {
                    self.warn(line, "#elif after #else; branch ignored");
                }
                let f = self.conditional_stack.last_mut().unwrap();
                f.active = f.parent_active && !f.taken && cond;
                f.taken |= f.active;
                let active = f.active;
                self.add_arm(frame.chain, record_line, || ConditionalArm {
                    directive: ArmDirective::Elif,
                    expression: spell_condition(expr),
                    line: record_line,
                    end_line: record_line,
                    outcome: arm_outcome(frame.parent_active, active),
                    evaluated: evaluate,
                    reads: if evaluate { reads } else { spelled_reads(expr) },
                });
            }
            "else" => {
                if !self.has_own_cond() {
                    return Err(self.error(line, "#else without #if"));
                }
                let f = self.conditional_stack.last_mut().unwrap();
                f.active = f.parent_active && !f.taken;
                f.taken = true;
                f.else_seen = true;
                let (chain, parent_active, active) = (f.chain, f.parent_active, f.active);
                self.add_arm(chain, record_line, || ConditionalArm {
                    directive: ArmDirective::Else,
                    expression: String::new(),
                    line: record_line,
                    end_line: record_line,
                    outcome: arm_outcome(parent_active, active),
                    evaluated: false,
                    reads: Vec::new(),
                });
            }
            "endif" => {
                if !self.has_own_cond() {
                    return Err(self.error(line, "#endif without #if"));
                }
                let frame = self.conditional_stack.pop().unwrap();
                self.close_arm(frame.chain, record_line);
                if let Some(chain) = frame.chain {
                    self.conditionals[chain].terminated = true;
                }
            }
            // Directives whose operands we ignore: the shared skip below
            // consumes the rest of the line. Calling skip_to_newline here as
            // well would eat the newline AND the whole following line
            // (e.g. `#pragma pack(push, 4)` swallowing the struct after it).
            "line" => {}
            "pragma" => {
                // `#pragma once` is the file's own statement that one
                // expansion per translation unit is enough; nothing else
                // about a pragma concerns the preprocessor. Reached inside a
                // skipped arm it states nothing, so it is read only while
                // active — and once read it holds for the rest of the run,
                // whatever later happens to the controlling condition.
                if self.is_active()
                    && matches!(tokens.get(i).map(|t| &t.kind), Some(TokenKind::Identifier(n)) if n.as_str() == "once")
                {
                    let path = trace_ir::canonicalize(&self.current_file);
                    self.record_guard(path, crate::FileGuard::Once);
                }
            }
            "undef" if self.is_active() => {
                let name = self.read_directive_ident(tokens, &mut i)?;
                self.remove_macro(&name);
            }
            "undef" if !self.is_active() => {}
            _ => {
                self.warn(
                    tokens[i.saturating_sub(1)].line,
                    format!("unknown directive #{directive}"),
                );
            }
        }
        i = self.skip_to_newline(tokens, i);
        Ok(i)
    }

    fn handle_include(&mut self, tokens: &[Token], mut i: usize) -> Result<usize, PreprocessError> {
        while i < tokens.len() && matches!(tokens[i].kind, TokenKind::Newline) {
            i += 1;
        }
        let line = tokens.get(i).map(|t| t.line).unwrap_or(1);
        // C11 6.10.2: a header-name (`"..."` / `<...>`) is taken as-is;
        // otherwise the rest of the line is macro-expanded and must then
        // form a header-name (`#include FOO` with `#define FOO "n.h"`).
        let path = if let Some(p) = parse_include_header(&tokens[i..]) {
            p
        } else {
            let mut end = i;
            while end < tokens.len()
                && !matches!(tokens[end].kind, TokenKind::Newline | TokenKind::Eof)
            {
                end += 1;
            }
            let expanded = self.expand_operand_tokens(&tokens[i..end])?;
            match parse_include_header(&expanded) {
                Some(p) => p,
                None => {
                    return Err(self.error(line, "expected string or <...> after #include"));
                }
            }
        };

        let include_path = match self.resolve_include(&path) {
            Ok(p) => p,
            Err(_) => {
                self.warn(line, format!("include file not found, skipping: {path}"));
                return Ok(i);
            }
        };
        let live_at = self.output.len();
        if let Err(e) = self.process_file(&include_path) {
            // The include is already in `included_files` and contributed
            // nothing, so this expansion — and every frame enclosing it — is
            // missing the header's content. Publishing any of them would keep
            // starving consumers after the underlying failure clears.
            self.limit_warn(
                line,
                format!("include preprocessing failed for {path}: {e}"),
            );
        }
        // File-local output: drop a nested cacheable header's tokens from
        // the *parent* buffer after the child has been cached. The child's
        // IR is merged at index time (PCH-style) instead of re-parsed in
        // every consumer.
        if !self.opts.inline_include_bodies
            && !self.opts.frozen_expansion_cache
            && Self::is_cacheable_header(&include_path)
        {
            self.output.truncate(live_at);
            self.line_map.truncate_at(live_at);
            if let Some(frame) = self.cache_frames.last_mut() {
                frame
                    .skips
                    .push((live_at, trace_ir::canonicalize(&include_path)));
            }
        }
        Ok(i)
    }

    /// Macro-expand a token run into a new vector, rather than into the
    /// output. Used where the result is read structurally instead of being
    /// rescanned in place: a `#include` operand assembled into a header-name,
    /// and a gMock invocation's arguments (`expand_gmock_args`).
    fn expand_operand_tokens(&mut self, tokens: &[Token]) -> Result<Vec<Token>, PreprocessError> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            // The expansion budget the emitting path charges per token, on
            // the same counter: this one builds a vector instead of writing
            // to the output, so nothing else would bound its width. Every
            // token an expansion produces is walked by an iteration here
            // (recursion included), so charging the iteration bounds the
            // whole prescan. Unbudgeted, an expansion bomb reached through a
            // gMock argument allocated gigabytes before the emitting path
            // ever saw its first token.
            self.check_resource_limits(tokens[i].line)?;
            if matches!(tokens[i].kind, TokenKind::Newline | TokenKind::Eof) {
                i += 1;
                continue;
            }
            let TokenKind::Identifier(name) = &tokens[i].kind else {
                out.push(tokens[i].clone());
                i += 1;
                continue;
            };
            if tokens[i].is_hidden(name) {
                out.push(tokens[i].clone());
                i += 1;
                continue;
            }
            self.record_read(name);
            let Some(def) = self.macros.get(name).cloned() else {
                out.push(tokens[i].clone());
                i += 1;
                continue;
            };
            match def {
                MacroDef::Object { replacement } => {
                    if !self.push_expansion(tokens[i].line) {
                        out.push(tokens[i].clone());
                        i += 1;
                        continue;
                    }
                    let painted = Self::paint_replacement(&replacement, &tokens[i], name);
                    // Every exit from an expansion pops it: an operand that
                    // stops mid-expansion is warned about and the enclosing
                    // file continues, so a leaked level would shrink the
                    // depth budget for the rest of the unit.
                    let nested = self.expand_operand_tokens(&painted);
                    self.pop_expansion();
                    out.extend(nested?);
                    i += 1;
                }
                MacroDef::Function { .. } | MacroDef::GmockMethod
                    if self.next_non_newline_is(tokens, i + 1, "(") =>
                {
                    if !self.push_expansion(tokens[i].line) {
                        out.push(tokens[i].clone());
                        i += 1;
                        continue;
                    }
                    let origin = tokens[i].clone();
                    i += 1;
                    let nested = self.parse_macro_args(tokens, &mut i).and_then(|args| {
                        let expanded = self.expand_invocation(name, &origin, &def, &args)?;
                        self.expand_operand_tokens(&expanded)
                    });
                    self.pop_expansion();
                    out.extend(nested?);
                }
                MacroDef::Function { .. } | MacroDef::GmockMethod => {
                    out.push(tokens[i].clone());
                    i += 1;
                }
            }
        }
        Ok(out)
    }

    fn resolve_include(&self, path: &str) -> Result<PathBuf, PreprocessError> {
        let candidate = if path.starts_with('/') || path.contains('\\') {
            PathBuf::from(path)
        } else {
            self.current_file
                .parent()
                .unwrap_or(Path::new("."))
                .join(path)
        };
        if candidate.exists() {
            return Ok(candidate);
        }
        for inc in &self.opts.include_paths {
            let p = inc.join(path);
            if p.is_file() {
                return Ok(p);
            }
        }
        if let Some(index) = &self.opts.basename_index {
            if let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) {
                if let Some(matches) = index.get(name) {
                    if matches.len() == 1 {
                        return Ok(matches[0].clone());
                    }
                }
            }
        }
        Err(PreprocessError::Message {
            message: format!("include file not found: {path}"),
        })
    }

    fn handle_define(&mut self, tokens: &[Token], mut i: usize) -> Result<usize, PreprocessError> {
        let name = self.read_directive_ident(tokens, &mut i)?;
        // `read_directive_ident` consumed exactly the name token.
        if let Some(open) = parameter_list_open(tokens, i - 1) {
            i = open + 1;
            let Some((params, variadic)) = self.parse_macro_param_list(tokens, &mut i) else {
                skip_directive_line(tokens, &mut i);
                return Ok(i);
            };
            let mut replacement = read_replacement_list(tokens, &mut i);
            // Normalize at define time: a named GNU variadic (`args...`)
            // whose body nevertheless spells `__VA_ARGS__` (gcc rejects the
            // mix; real corpora contain it) aliases the tail parameter, so
            // expansion needs only plain parameter lookups.
            if variadic {
                if let Some(tail) = params.last() {
                    if tail != "__VA_ARGS__" {
                        for tok in &mut replacement {
                            if matches!(&tok.kind, TokenKind::Identifier(n) if n == "__VA_ARGS__") {
                                tok.kind = TokenKind::Identifier(tail.clone());
                            }
                        }
                    }
                }
            }
            self.insert_macro(
                name,
                MacroDef::Function {
                    params,
                    replacement,
                    variadic,
                },
            );
            return Ok(i);
        }
        let replacement = read_replacement_list(tokens, &mut i);
        self.insert_macro(name, MacroDef::Object { replacement });
        Ok(i)
    }

    /// Parse the parameter list after `NAME(`. `None` means the list was
    /// unterminated or malformed: a warning has been emitted and the caller
    /// drops the definition (gcc reports an error and keeps preprocessing).
    fn parse_macro_param_list(
        &mut self,
        tokens: &[Token],
        i: &mut usize,
    ) -> Option<(Vec<String>, bool)> {
        let mut params = Vec::new();
        let mut variadic = false;
        loop {
            if self.token_is_ellipsis(tokens, *i) {
                // Anonymous `...`: register the variadic under its standard
                // name so substitution, `##` comma elision, and the
                // "last param collects the rest" rule all treat it exactly
                // like a named `args...` variadic.
                variadic = true;
                params.push("__VA_ARGS__".to_string());
                *i += 1;
                return self
                    .finish_param_list_tail(tokens, i)
                    .then_some((params, variadic));
            }
            match tokens.get(*i).map(|t| &t.kind) {
                Some(TokenKind::Punct(s)) if *s == ")" => {
                    *i += 1;
                    break;
                }
                Some(TokenKind::Identifier(name)) => {
                    params.push(name.clone());
                    *i += 1;
                }
                _ => return self.malformed_param_list(tokens, *i),
            }
            // Line splicing makes `args \`-newline-`...` equivalent to
            // `args...`; the lexer has already deleted the splice.
            if self.token_is_ellipsis(tokens, *i) {
                variadic = true;
                *i += 1;
                return self
                    .finish_param_list_tail(tokens, i)
                    .then_some((params, variadic));
            }
            match tokens.get(*i).map(|t| &t.kind) {
                Some(TokenKind::Punct(s)) if *s == ")" => {
                    *i += 1;
                    break;
                }
                Some(TokenKind::Punct(s)) if *s == "," => {
                    *i += 1;
                }
                _ => return self.malformed_param_list(tokens, *i),
            }
        }
        Some((params, variadic))
    }

    /// Warn about a parameter list that ends at a newline / EOF or contains
    /// an unexpected token, then yield `None` so the definition is dropped.
    fn malformed_param_list(&mut self, tokens: &[Token], i: usize) -> Option<(Vec<String>, bool)> {
        let line = tokens.get(i).map(|t| t.line).unwrap_or(1);
        let message = match tokens.get(i).map(|t| &t.kind) {
            None | Some(TokenKind::Eof) | Some(TokenKind::Newline) => {
                "unterminated macro parameter list; definition ignored"
            }
            _ => "expected , or ) in macro parameters; definition ignored",
        };
        self.warn(line, message);
        None
    }

    /// Consume the closing `)` after `...`. Tokens before it are dropped
    /// rather than leaked into the replacement list. Returns `false` (after
    /// warning) when the line ends first — the list must not run on to a
    /// `)` on a later line, which would swallow following code.
    fn finish_param_list_tail(&mut self, tokens: &[Token], i: &mut usize) -> bool {
        loop {
            match tokens.get(*i).map(|t| &t.kind) {
                Some(TokenKind::Punct(s)) if *s == ")" => {
                    *i += 1;
                    return true;
                }
                None | Some(TokenKind::Eof) | Some(TokenKind::Newline) => {
                    return self.malformed_param_list(tokens, *i).is_some();
                }
                Some(_) => *i += 1,
            }
        }
    }

    /// An ellipsis the lexer munched is one `...` token (#28), and the
    /// lexer splices `\`-newline before munching, so `.\`-newline-`..` is
    /// one too (#38). Dots it did not munch stay one `.` each and take the
    /// malformed-list path, which is right for `. . .` and for dots split
    /// by a comment — `invalid token in macro parameter list` in gcc and
    /// clang.
    fn token_is_ellipsis(&self, tokens: &[Token], i: usize) -> bool {
        matches!(&tokens.get(i).map(|t| &t.kind), Some(TokenKind::Punct(s)) if *s == "...")
    }

    fn next_non_newline_is(&self, tokens: &[Token], mut i: usize, punct: &str) -> bool {
        while i < tokens.len() && matches!(tokens[i].kind, TokenKind::Newline) {
            i += 1;
        }
        matches!(
            tokens.get(i).map(|t| &t.kind),
            Some(TokenKind::Punct(s)) if *s == punct
        )
    }

    fn parse_macro_args(
        &mut self,
        tokens: &[Token],
        i: &mut usize,
    ) -> Result<MacroArgs, PreprocessError> {
        while *i < tokens.len() && matches!(tokens[*i].kind, TokenKind::Newline) {
            *i += 1;
        }
        if !matches!(tokens.get(*i).map(|t| &t.kind), Some(TokenKind::Punct(s)) if *s == "(") {
            return Ok(MacroArgs::default());
        }
        *i += 1;
        let mut args = MacroArgs::default();
        let mut current: Vec<Token> = Vec::new();
        let mut depth = 0u32;
        while *i < tokens.len() {
            let tok = tokens[*i].clone();
            match &tok.kind {
                TokenKind::Punct(s) if *s == "(" => {
                    depth += 1;
                    current.push(tok);
                    *i += 1;
                }
                TokenKind::Punct(s) if *s == ")" && depth == 0 => {
                    args.args.push(current);
                    *i += 1;
                    break;
                }
                TokenKind::Punct(s) if *s == ")" => {
                    depth -= 1;
                    current.push(tok);
                    *i += 1;
                }
                TokenKind::Punct(s) if *s == "," && depth == 0 => {
                    args.args.push(current);
                    args.separators.push(tok);
                    current = Vec::new();
                    *i += 1;
                }
                TokenKind::Eof => {
                    return Err(self.error(tok.line, "unterminated macro argument list"));
                }
                _ => {
                    current.push(tok);
                    *i += 1;
                }
            }
        }
        Ok(args)
    }

    fn read_directive_ident(
        &mut self,
        tokens: &[Token],
        i: &mut usize,
    ) -> Result<String, PreprocessError> {
        while *i < tokens.len() && matches!(tokens[*i].kind, TokenKind::Newline) {
            *i += 1;
        }
        match tokens.get(*i).map(|t| &t.kind) {
            Some(TokenKind::Identifier(s)) => {
                let name = s.clone();
                *i += 1;
                Ok(name)
            }
            _ => Err(self.error(
                tokens.get(*i).map(|t| t.line).unwrap_or(1),
                "expected identifier in directive",
            )),
        }
    }

    /// Evaluate the controlling expression of `#if` / `#elif`.
    ///
    /// `defined X` / `defined(X)` is resolved as an operator over the
    /// *unexpanded* tokens (C11 6.10.1p4: the operand of `defined` is never
    /// macro-expanded), object macros are expanded recursively (hide-set
    /// painted, depth-capped), and the result is parsed with C operator
    /// precedence by `eval_pp_tokens`.
    fn expand_and_eval_condition(&mut self, tokens: &[Token], i: &mut usize) -> bool {
        // The condition runs to the end of the line; the lexer has already
        // spliced any `\`-newline continuations out of it.
        let start = *i;
        skip_directive_line(tokens, i);
        match self.expand_condition_tokens(&tokens[start..*i]) {
            Some(expanded) => eval_pp_tokens(&expanded),
            None => false,
        }
    }

    /// Macro-expand condition tokens, treating `defined` as an operator
    /// whose operand is consumed unexpanded. `defined` introduced *by* an
    /// expansion is also resolved here (common `#define HAS_X defined(X)`
    /// pattern; undefined behavior per C11, resolved like gcc/clang do).
    /// Object and function-like macros expand on a flat worklist with
    /// rescanning, so an object macro naming a function-like macro still
    /// sees the `(args)` that follow it in the condition. Hide sets stop
    /// self-reference; explicit step/size budgets stop pathological growth
    /// (this engine bypasses the text path's `check_resource_limits`), in
    /// which case `None` is returned and the condition evaluates false.
    fn warn_condition_budget(&mut self, line: u32) {
        self.warn(
            line,
            "macro expansion budget exceeded in #if condition; treating as false".to_string(),
        );
    }

    fn expand_condition_tokens(&mut self, toks: &[Token]) -> Option<Vec<Token>> {
        const MAX_TOKENS: usize = 1 << 16;
        const MAX_STEPS: u64 = 1 << 20;
        let mut work: Vec<Token> = toks.to_vec();
        let mut out: Vec<Token> = Vec::new();
        let mut i = 0usize;
        let mut steps: u64 = 0;
        while i < work.len() {
            steps += 1;
            if steps > MAX_STEPS || work.len() > MAX_TOKENS || out.len() > MAX_TOKENS {
                let line = work.get(i).map(|t| t.line).unwrap_or(1);
                self.warn_condition_budget(line);
                return None;
            }
            let tok = work[i].clone();
            if let TokenKind::Identifier(name) = &tok.kind {
                if name == "defined" {
                    let (operand, consumed) = defined_operand(&work, i);
                    let operand = operand.map(str::to_string);
                    let val = operand.is_some_and(|n| self.is_defined_for_conditionals(&n));
                    out.push(Token::new(
                        TokenKind::Number(if val { "1" } else { "0" }.into()),
                        tok.line,
                        tok.col,
                    ));
                    i += consumed;
                    continue;
                }
                if name == "__LINE__" {
                    out.push(Token::new(
                        TokenKind::Number(tok.expansion_site().0.to_string()),
                        tok.line,
                        tok.col,
                    ));
                    i += 1;
                    continue;
                }
                // A fallback must behave as undefined throughout conditional
                // evaluation: expanding it here (often to nothing) would
                // mangle the expression (`1 || __init` -> `1 ||`), while an
                // unexpanded identifier correctly evaluates to 0.
                if !tok.is_hidden(name) {
                    self.record_read(name);
                }
                if !tok.is_hidden(name) && !self.fallback_macros.contains(name.as_str()) {
                    match self.macros.get(name) {
                        Some(MacroDef::Object { replacement }) => {
                            let painted = Self::paint_replacement(replacement, &tok, name);
                            work.splice(i..i + 1, painted);
                            continue; // rescan at i
                        }
                        Some(MacroDef::Function {
                            params,
                            replacement,
                            variadic,
                        }) => {
                            if let Some((args, next)) = parse_cond_macro_args(&work, i + 1) {
                                // Same allocate-before-charge hazard as #30,
                                // in this engine: the cap is tested at the
                                // top of the loop, so one splice could take
                                // `work` far past MAX_TOKENS before the next
                                // test saw it (a 64-way macro over a 32k
                                // argument reached 329 MB against a 65,536
                                // token cap). Project what the substitution
                                // will add, against the part of `work` the
                                // splice keeps, and refuse before building.
                                let projected = projected_substitution_len(
                                    replacement,
                                    params,
                                    &args,
                                    *variadic,
                                );
                                let kept = work.len() - (next - i);
                                if kept.saturating_add(projected as usize) > MAX_TOKENS {
                                    self.warn_condition_budget(tok.line);
                                    return None;
                                }
                                let substituted = apply_concatenation(substitute_macro(
                                    name,
                                    &tok,
                                    replacement,
                                    params,
                                    &args,
                                    *variadic,
                                ));
                                work.splice(i..next, substituted);
                                continue; // rescan at i
                            }
                        }
                        Some(MacroDef::GmockMethod) | None => {}
                    }
                }
            }
            out.push(tok);
            i += 1;
        }
        Some(out)
    }

    fn skip_to_newline(&self, tokens: &[Token], mut i: usize) -> usize {
        while i < tokens.len() && !matches!(tokens[i].kind, TokenKind::Newline | TokenKind::Eof) {
            i += 1;
        }
        if i < tokens.len() && matches!(tokens[i].kind, TokenKind::Newline) {
            i += 1;
        }
        i
    }

    fn finish(self) -> PreprocessResult {
        PreprocessResult {
            output: self.output,
            line_map: self.line_map,
            diagnostics: self.diagnostics,
            language: self.language,
            included_headers: self.included_files.into_iter().collect(),
            inlined_headers: self.inlined_files.into_iter().collect(),
            replayed_variants: {
                let mut vs: HashSet<(PathBuf, usize)> = self.direct_variants;
                vs.extend(self.variant_used);
                let mut vs: Vec<(PathBuf, usize)> = vs.into_iter().collect();
                vs.sort();
                vs
            },
            conditionals: self.conditionals,
        }
    }
}

/// The tokens of the current directive line from `i` to its end.
fn directive_rest(tokens: &[Token], i: usize) -> &[Token] {
    let mut end = i;
    skip_directive_line(tokens, &mut end);
    &tokens[i.min(end)..end]
}

fn arm_outcome(evaluated: bool, taken: bool) -> ArmOutcome {
    match (evaluated, taken) {
        (false, _) => ArmOutcome::Unevaluated,
        (true, true) => ArmOutcome::Taken,
        (true, false) => ArmOutcome::Skipped,
    }
}

/// A condition as written: token spellings, with the whitespace the source
/// had between them collapsed to one space.
fn spell_condition(tokens: &[Token]) -> String {
    spell_tokens(tokens, str::to_string)
}

/// The identifiers a condition spells, for an arm that was never evaluated
/// (so nothing consulted the environment). `defined`, `__LINE__` and the
/// alternative operator spellings (`and`, `not`, …) are operators, not names,
/// except when explicitly used as the operand of `defined`.
fn spelled_reads(tokens: &[Token]) -> Vec<ConditionRead> {
    let mut reads = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let mut name = ident_name(&tokens[i]);
        if name == Some("defined") {
            let (operand, consumed) = defined_operand(tokens, i);
            name = operand;
            i += consumed;
        } else {
            name = name.filter(|n| *n != "__LINE__" && !is_alternative_token(n));
            i += 1;
        }
        if let Some(name) = name {
            reads.push(ConditionRead {
                name: name.to_string(),
                bound: None,
            });
        }
    }
    dedup_reads(reads)
}

/// The C++ alternative operator spellings (C++ [lex.digraph]; `<iso646.h>`
/// in C). The evaluator does not interpret them, so `#if A and B` reads as
/// `A` followed by an unbound identifier; for the conditional record that
/// identifier is an operator, not a name the configuration failed to bind.
fn is_alternative_token(name: &str) -> bool {
    matches!(
        name,
        "and"
            | "and_eq"
            | "bitand"
            | "bitor"
            | "compl"
            | "not"
            | "not_eq"
            | "or"
            | "or_eq"
            | "xor"
            | "xor_eq"
    )
}

fn dedup_reads(reads: Vec<ConditionRead>) -> Vec<ConditionRead> {
    let mut seen: HashSet<String> = HashSet::new();
    reads
        .into_iter()
        .filter(|r| seen.insert(r.name.clone()))
        .collect()
}

/// `NAME` from a condition that is exactly `!defined(NAME)` or
/// `!defined NAME`: the `#if` spelling of an include guard.
fn negated_defined_name(expr: &[Token]) -> Option<&str> {
    match expr {
        [bang, defined, rest @ ..] if is_punct(bang, "!") && is_ident(defined, "defined") => {
            match rest {
                [open, name, close] if is_punct(open, "(") && is_punct(close, ")") => {
                    ident_name(name)
                }
                [name] => ident_name(name),
                _ => None,
            }
        }
        _ => None,
    }
}

fn ident_name(tok: &Token) -> Option<&str> {
    match &tok.kind {
        TokenKind::Identifier(n) => Some(n.as_str()),
        _ => None,
    }
}

fn parse_include_header(tokens: &[Token]) -> Option<String> {
    let mut i = 0;
    while i < tokens.len() && matches!(tokens[i].kind, TokenKind::Newline) {
        i += 1;
    }
    match tokens.get(i).map(|t| &t.kind) {
        Some(TokenKind::String(s)) => plain_string_body(s).map(str::to_string),
        Some(TokenKind::Punct(s)) if *s == "<" => {
            let mut header = String::new();
            i += 1;
            while i < tokens.len() {
                match &tokens[i].kind {
                    TokenKind::Identifier(s) | TokenKind::Number(s) => {
                        header.push_str(s);
                    }
                    TokenKind::Punct(s) if *s != ">" => {
                        header.push_str(s);
                    }
                    TokenKind::Punct(s) if *s == ">" => return Some(header),
                    _ => return None,
                }
                i += 1;
            }
            None
        }
        _ => None,
    }
}

fn at_beginning_of_line(tokens: &[Token], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    matches!(tokens[i - 1].kind, TokenKind::Newline)
}

/// The include guard wrapping the whole file, if it has one: `#ifndef NAME`
/// (or `#if !defined(NAME)`) before any other token, `#define NAME` as the
/// very next directive, and the matching `#endif` with nothing but
/// whitespace after it. A second arm disqualifies it — a guard has nothing
/// to choose between.
///
/// Read off the token stream rather than observed while the file runs, so
/// the answer is available at the file's first inclusion and does not depend
/// on that run reaching the `#endif` (#56).
fn detect_include_guard(tokens: &[Token]) -> Option<Arc<str>> {
    let mut i = 0;
    let name = guard_opener(tokens, &mut i)?;
    // The `#define` of that same name has to be the next directive.
    if next_directive(tokens, &mut i)? != "define" || ident_name(tokens.get(i)?)? != &*name {
        return None;
    }
    // Directives only, tracking nesting, to the `#endif` that closes the
    // opener.
    let mut depth = 1usize;
    while let Some(directive) = next_directive(tokens, &mut i) {
        match directive {
            "if" | "ifdef" | "ifndef" => depth += 1,
            "elif" | "else" if depth == 1 => return None,
            "endif" => {
                depth -= 1;
                if depth == 0 {
                    skip_directive_line(tokens, &mut i);
                    return tokens[i..]
                        .iter()
                        .all(|t| matches!(t.kind, TokenKind::Newline | TokenKind::Eof))
                        .then_some(name);
                }
            }
            _ => {}
        }
    }
    None
}

/// The name an `#ifndef NAME` / `#if !defined(NAME)` opener tests, when one
/// is the file's very first token. Leaves `i` past that directive line.
///
/// Spelled out rather than routed through `next_directive`, which skips over
/// lines that are not directives: the opener has to be *first*, or the text
/// before it would escape the wrapper.
fn guard_opener(tokens: &[Token], i: &mut usize) -> Option<Arc<str>> {
    while matches!(tokens.get(*i).map(|t| &t.kind), Some(TokenKind::Newline)) {
        *i += 1;
    }
    if !matches!(tokens.get(*i)?.kind, TokenKind::Hash) {
        return None;
    }
    *i += 1;
    let keyword = ident_name(tokens.get(*i)?)?;
    *i += 1;
    let name = match keyword {
        "ifndef" => ident_name(tokens.get(*i)?)?,
        "if" => negated_defined_name(directive_rest(tokens, *i))?,
        _ => return None,
    };
    let name = Arc::from(name);
    skip_directive_line(tokens, i);
    Some(name)
}

/// The next directive's keyword, leaving `i` just past it; `None` at end of
/// input. Lines that are not directives are skipped, as is a `#` followed by
/// something other than a directive name (a `# 1 "x.c"` line marker).
fn next_directive<'a>(tokens: &'a [Token], i: &mut usize) -> Option<&'a str> {
    loop {
        while matches!(tokens.get(*i).map(|t| &t.kind), Some(TokenKind::Newline)) {
            *i += 1;
        }
        match tokens.get(*i)?.kind {
            TokenKind::Eof => return None,
            TokenKind::Hash => {
                *i += 1;
                match tokens.get(*i).and_then(ident_name) {
                    Some(keyword) => {
                        *i += 1;
                        return Some(keyword);
                    }
                    None => skip_directive_line(tokens, i),
                }
            }
            _ => skip_directive_line(tokens, i),
        }
    }
}

/// Advance to the end of the current directive line, leaving `i` on the
/// newline / EOF token. A `\`-newline continuation is spliced by the lexer,
/// so the line's tokens run up to the first bare newline.
fn skip_directive_line(tokens: &[Token], i: &mut usize) {
    while *i < tokens.len() && !matches!(tokens[*i].kind, TokenKind::Newline | TokenKind::Eof) {
        *i += 1;
    }
}

/// Whether `idx` is the variadic collector — by construction always the
/// last parameter (`parse_macro_param_list` names an anonymous `...`
/// "__VA_ARGS__"; see the invariant on `MacroDef::Function`).
fn is_variadic_tail(params: &[String], variadic: bool, idx: usize) -> bool {
    variadic && idx + 1 == params.len()
}

fn is_newline(tok: &Token) -> bool {
    matches!(tok.kind, TokenKind::Newline)
}

fn arg_is_blank(arg: &[Token]) -> bool {
    arg.iter().all(is_newline)
}

/// Whether the arguments from `idx` on carry no real tokens (absent, or
/// whitespace/newline only). Allocation-free — this runs on every `##`
/// parameter operand.
fn args_are_blank(args: &[Vec<Token>], idx: usize) -> bool {
    args.iter().skip(idx).all(|a| arg_is_blank(a))
}

/// GNU `, ## __VA_ARGS__` deletes the comma only when the variable
/// arguments are OMITTED — an explicitly supplied empty argument keeps it
/// (`F(1)` -> `g(1)` but `F(1,)` -> `g(1,)`; verified against gcc and
/// clang, whose behavior is stricter than the manual's "omitted or empty"
/// wording). A lone blank argument (`G()`, `G( )`) supplies zero
/// arguments, not one empty one.
fn varargs_omitted(args: &[Vec<Token>], idx: usize) -> bool {
    if args.len() <= idx {
        return true;
    }
    idx == 0 && args.len() == 1 && arg_is_blank(&args[0])
}

/// Whether the `##` whose right operand sits at `right` is the GNU
/// `, ## __VA_ARGS__` form: the variadic tail parameter after the operator
/// and a comma already emitted into `out`. `substitute_macro`'s `##` branch
/// owns that shape — it deletes the comma when the varargs are omitted and
/// leaves the operator inert otherwise — so an empty parameter standing
/// between the comma and the `##` must step aside instead of consuming the
/// operator as an ordinary placemarker would.
fn is_gnu_comma_paste(
    body: &[Token],
    right: usize,
    params: &[String],
    variadic: bool,
    out: &[Token],
) -> bool {
    let Some(TokenKind::Identifier(name)) = body.get(right).map(|t| &t.kind) else {
        return false;
    };
    params
        .iter()
        .position(|p| p == name)
        .is_some_and(|idx| is_variadic_tail(params, variadic, idx))
        && matches!(out.last().map(|t| &t.kind), Some(TokenKind::Punct(s)) if *s == ",")
}

/// Index of the `(` that opens a function-like macro's parameter list, if
/// the token after the macro name at `name_idx` is one. C11 6.10.3p10: the
/// definition is function-like only when `(` immediately follows the macro
/// name with no intervening whitespace, so `#define ALIAS (VALUE)` and
/// `#define HALF (.5)` are object macros whose replacement starts with `(`.
/// Tokens carry no whitespace; the lexer records whether a token touched
/// its predecessor (`Token::adjacent_before`), with a `\`-newline deleted
/// in translation phase 2 and therefore zero-width, so `F\`-newline-`(x)`
/// is function-like when `(` starts the next line.
fn parameter_list_open(tokens: &[Token], name_idx: usize) -> Option<usize> {
    if !matches!(tokens[name_idx].kind, TokenKind::Identifier(_)) {
        return None;
    }
    let i = name_idx + 1;
    let next = tokens.get(i)?;
    (next.adjacent_before && matches!(&next.kind, TokenKind::Punct(s) if *s == "(")).then_some(i)
}

/// Collect a `#define` replacement list up to the end of the line (the
/// lexer has already spliced `\`-newline continuations).
fn read_replacement_list(tokens: &[Token], i: &mut usize) -> Vec<Token> {
    let mut replacement = Vec::new();
    while *i < tokens.len() && !matches!(tokens[*i].kind, TokenKind::Newline) {
        replacement.push(tokens[*i].clone());
        *i += 1;
    }
    replacement
}

/// The arguments of one function-like macro invocation. `separators[k]` is
/// the top-level `,` token between `args[k]` and `args[k + 1]`; keeping it
/// lets the variadic collector be re-spelled exactly as written
/// (`#__VA_ARGS__` on `F(p,q)` is `"p,q"`, on `F(p , q)` it is `"p , q"`).
#[derive(Debug, Default)]
struct MacroArgs {
    args: Vec<Vec<Token>>,
    separators: Vec<Token>,
}

impl MacroArgs {
    /// The variadic collector's tokens from `idx` on, commas included, in
    /// source order — the one argument C11 6.10.3p12 says they form.
    /// How many tokens `variadic_tokens(idx)` would produce, without
    /// building them — the projection below must not allocate the very
    /// vector it exists to bound.
    fn variadic_len(&self, idx: usize) -> usize {
        let mut n = 0;
        for (ai, arg) in self.args.iter().enumerate().skip(idx) {
            if ai > idx {
                n += 1;
            }
            n += arg.len();
        }
        n
    }

    fn variadic_tokens(&self, idx: usize) -> Vec<Token> {
        let mut out = Vec::new();
        for (ai, arg) in self.args.iter().enumerate().skip(idx) {
            if ai > idx {
                out.push(self.separators[ai - 1].clone());
            }
            out.extend(arg.iter().cloned());
        }
        out
    }
}

impl PreprocessorState {
    /// One function-like invocation's replacement tokens, before rescanning.
    fn expand_invocation(
        &mut self,
        macro_name: &str,
        origin: &Token,
        def: &MacroDef,
        args: &MacroArgs,
    ) -> Result<Vec<Token>, PreprocessError> {
        Ok(match def {
            MacroDef::Function {
                params,
                replacement,
                variadic,
            } => {
                // Bound what this is about to allocate before allocating it
                // (#30). The rescan charges the result again as it walks
                // it, which is the pre-existing accounting; this charge is
                // what makes a wide argument fail before it is copied once
                // per parameter occurrence.
                self.charge_tokens(
                    projected_substitution_len(replacement, params, args, *variadic),
                    origin.line,
                )?;
                apply_concatenation(substitute_macro(
                    macro_name,
                    origin,
                    replacement,
                    params,
                    args,
                    *variadic,
                ))
            }
            MacroDef::GmockMethod => {
                let args = self.expand_gmock_args(args)?;
                let expanded = expand_gmock_method(macro_name, origin, &args);
                // The expansion promotes argument tokens into the declaration
                // itself, so a member named after any macro in the family —
                // not only the one being expanded — would be rescanned as a
                // fresh invocation and eaten. `expand_gmock_method` hides the
                // macro it expands; the rest of the family is hidden here,
                // where the table is in reach.
                for tok in &expanded {
                    if let TokenKind::Identifier(n) = &tok.kind {
                        self.record_read(n);
                    }
                }
                expanded
                    .into_iter()
                    .map(|t| {
                        let gmock_name = match &t.kind {
                            TokenKind::Identifier(n) => {
                                matches!(self.macros.get(n.as_str()), Some(MacroDef::GmockMethod))
                                    .then(|| n.clone())
                            }
                            _ => None,
                        };
                        match gmock_name {
                            Some(n) => t.with_macro_hide(origin, &n),
                            None => t,
                        }
                    })
                    .collect()
            }
            // Callers dispatch object-like macros before parsing arguments;
            // the preprocessor never panics on input, so expand it as one
            // anyway.
            MacroDef::Object { replacement } => {
                Self::paint_replacement(replacement, origin, macro_name)
            }
        })
    }

    /// A gMock invocation's arguments, macro-expanded before they are read.
    ///
    /// Every other macro substitutes its arguments and leaves them to the
    /// rescan, but a gMock invocation is read *structurally* — split at a
    /// parameter list, unwrapped of gMock's protecting parentheses — and an
    /// alias hides that structure from every one of those tests. `#define RET
    /// (std::pair<int, int>)` is one identifier until it is expanded, so its
    /// protecting parentheses would otherwise survive into the declaration;
    /// `#define PARAMS (int, int)` would look like no parameter list at all.
    fn expand_gmock_args(&mut self, args: &MacroArgs) -> Result<MacroArgs, PreprocessError> {
        let mut expanded = Vec::with_capacity(args.args.len());
        for arg in &args.args {
            expanded.push(self.expand_operand_tokens(arg)?);
        }
        Ok(MacroArgs {
            args: expanded,
            separators: args.separators.clone(),
        })
    }
}

/// Recover a gMock declaration macro as the member prototype it stands for.
///
/// Modern `MOCK_METHOD(ret, name, params[, specs])` carries the pieces
/// separately. gMock requires one extra pair of parentheses around any
/// comma-containing return or parameter type; C++ accepts neither, so both
/// are unwrapped. Of the spec list only what C++ accepts on a declaration
/// survives, spelled in C++'s order — `const`, the `ref(&)` / `ref(&&)`
/// ref-qualifier, `noexcept` with its expression, then `override` / `final`;
/// `Calltype(...)`, which has no declaration spelling, is dropped.
///
/// The arguments arrive already macro-expanded (`expand_gmock_args`): they
/// are read structurally, and an alias would hide that structure.
///
/// The legacy families — `MOCK_METHODn`, `MOCK_CONST_METHODn` and their
/// `_T` / `_WITH_CALLTYPE` spellings — carry one function-type argument,
/// split at its parameter list; the leading call-type argument is dropped.
///
/// A return type that is itself a parenthesized declarator (`void (*)(int)`,
/// `int (&)[4]`, `void (C::*)(int)`) cannot be re-spelled around a member
/// name without full declarator surgery, so it degrades to `void`, keeping
/// the member and its class; parentheses that open no declarator —
/// `decltype(...)`, or a macro spelling a comma-containing type — keep their
/// spelling and are expanded by the rescan. A malformed invocation expands to
/// nothing rather than to a broken declaration.
fn expand_gmock_method(macro_name: &str, origin: &Token, args: &MacroArgs) -> Vec<Token> {
    let synth = |text: &'static str| {
        let kind = if text.chars().all(char::is_alphabetic) {
            TokenKind::Identifier(text.into())
        } else {
            TokenKind::Punct(text)
        };
        Token::new(kind, origin.line, origin.col).with_macro_hide(origin, macro_name)
    };
    let empty_params = || vec![synth("("), synth(")")];
    let (ret, name, params, quals): (Vec<Token>, &[Token], Vec<Token>, Vec<Token>) =
        if macro_name == "MOCK_METHOD" {
            let (Some(ret), Some(name), Some(params)) = (
                args.args.first().map(|a| trim_newlines(a)),
                args.args.get(1).map(|a| trim_newlines(a)),
                args.args.get(2).map(|a| trim_newlines(a)),
            ) else {
                return Vec::new();
            };
            // gMock spells the parameters as one parenthesized group. If they
            // are not, the invocation is malformed (most likely an
            // unparenthesized comma-containing type, which gMock rejects too)
            // and any expansion of it would be a broken declaration.
            if trailing_paren_group(params) != Some(0) {
                return Vec::new();
            }
            // gMock spells the spec list as one parenthesized group, and
            // only its top-level items are specifiers: an identifier nested
            // in one of their argument lists belongs to an expression, not to
            // the declaration — the `const` of
            // `noexcept(is_nothrow<const T&>::value)` is part of the type it
            // asks about, and `Calltype(final)` names a calling convention.
            let spec = strip_outer_parens(trim_newlines(
                args.args.get(3).map(Vec::as_slice).unwrap_or_default(),
            ));
            // Of the spec list only what C++ accepts on a declaration
            // survives, spelled in the order C++ wants it: cv-qualifier,
            // ref-qualifier, exception specification, virt-specifiers.
            // `Calltype(...)` has no declaration spelling and is dropped.
            let has = |q: &str| top_level_ident(spec, q).is_some();
            let mut quals: Vec<Token> = Vec::new();
            if has("const") {
                quals.push(synth("const"));
            }
            // `ref(&)` / `ref(&&)` carry the ref-qualifier the mocked method
            // is declared with; overload and override matching need it.
            if let Some(inner) = spec_group(spec, "ref") {
                quals.extend(inner.iter().cloned());
            }
            if has("noexcept") {
                quals.push(synth("noexcept"));
                // `noexcept(expr)` is not the same declaration as `noexcept`.
                if let Some(inner) = spec_group(spec, "noexcept") {
                    quals.push(synth("("));
                    quals.extend(inner.iter().cloned());
                    quals.push(synth(")"));
                }
            }
            for q in ["override", "final"] {
                if has(q) {
                    quals.push(synth(q));
                }
            }
            let ret = trim_newlines(strip_outer_parens(ret));
            // One pair of parentheses is gMock's comma protection; a comma
            // still left outside every `<…>` means the argument never was a
            // single type, which gMock rejects too.
            if has_top_level_comma(ret) {
                return Vec::new();
            }
            let ret = if is_parenthesized_declarator(ret) {
                vec![synth("void")]
            } else {
                ret.to_vec()
            };
            (ret, name, strip_param_parens(params), quals)
        } else {
            // `MOCK_METHODn_WITH_CALLTYPE(calltype, name, signature)` puts the
            // calling convention first; it has no bearing on the declaration.
            let name_idx = usize::from(macro_name.ends_with("_WITH_CALLTYPE"));
            let Some(name) = args.args.get(name_idx).map(|a| trim_newlines(a)) else {
                return Vec::new();
            };
            if args.args.len() <= name_idx + 1 {
                return Vec::new();
            }
            // Rejoin any argument the macro call split: a signature naming a
            // comma-containing type is one argument to gMock's variadic
            // machinery but several to a fixed parameter list.
            let signature = args.variadic_tokens(name_idx + 1);
            let signature = trim_newlines(&signature).to_vec();
            let (ret, params) = match trailing_paren_group(&signature) {
                // The trailing group of `void (*())(int)` is the returned
                // pointer's parameter list, not the method's; neither the
                // type nor the arity survives, so keep just the member.
                Some(open) if is_parenthesized_declarator(&signature[..open]) => {
                    (vec![synth("void")], empty_params())
                }
                Some(open) => (
                    trim_newlines(&signature[..open]).to_vec(),
                    signature[open..].to_vec(),
                ),
                // No parameter list to split at: a bare return type keeps
                // the member, but one that is a parenthesized declarator
                // (`int (&())[4]`) cannot wrap the name any more than in the
                // modern form.
                None if is_parenthesized_declarator(&signature) => {
                    (vec![synth("void")], empty_params())
                }
                // A group that is not the trailing one is not a parameter
                // list. `int(int) const`, which gMock rejects too, would
                // otherwise be spelled whole in front of the member name.
                None if has_top_level_paren(&signature) => return Vec::new(),
                None => (signature.clone(), empty_params()),
            };
            let quals = if macro_name.starts_with("MOCK_CONST_") {
                vec![synth("const")]
            } else {
                Vec::new()
            };
            (ret, name, params, quals)
        };
    // A declaration needs both halves; `MOCK_METHOD0(Foo, )` and
    // `MOCK_METHOD(int, , ())` would otherwise emit `Foo();` and `int ();`.
    if ret.is_empty() || name.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(ret.len() + name.len() + params.len() + quals.len() + 1);
    out.extend(ret);
    out.extend(name.iter().cloned());
    out.extend(params);
    out.extend(quals);
    out.push(synth(";"));
    // Unlike a replacement list, this expansion promotes argument tokens into
    // the declaration itself — the member name is one — so every token takes
    // the macro's hide set, or `MOCK_METHOD(int, MOCK_METHOD, ())` would
    // rescan its own member name as a fresh invocation and eat it.
    out.iter()
        .map(|t| t.with_macro_hide(origin, macro_name))
        .collect()
}

/// The tokens inside `name(...)` in a gMock spec list, if it holds one:
/// `ref(&)` gives `&`, `noexcept(false)` gives `false`.
fn spec_group<'a>(spec: &'a [Token], name: &str) -> Option<&'a [Token]> {
    let at = top_level_ident(spec, name)?;
    let rest = &spec[at + 1..];
    if !rest.first().is_some_and(|t| is_punct(t, "(")) {
        return None;
    }
    let mut depth = 0_u32;
    for (idx, token) in rest.iter().enumerate() {
        if is_punct(token, "(") {
            depth += 1;
        } else if is_punct(token, ")") {
            depth -= 1;
            if depth == 0 {
                return Some(&rest[1..idx]);
            }
        }
    }
    None
}

/// Index of the identifier `name` at the top level of a gMock spec list —
/// outside every nested argument list, so the `const` of
/// `noexcept(is_nothrow<const T&>::value)` is not read as a cv-qualifier.
fn top_level_ident(spec: &[Token], name: &str) -> Option<usize> {
    let mut depth = 0_u32;
    for (idx, token) in spec.iter().enumerate() {
        match &token.kind {
            TokenKind::Punct(s) if *s == "(" => depth += 1,
            TokenKind::Punct(s) if *s == ")" => depth = depth.saturating_sub(1),
            TokenKind::Identifier(s) if depth == 0 && s == name => return Some(idx),
            _ => {}
        }
    }
    None
}

fn is_punct(token: &Token, punct: &str) -> bool {
    matches!(&token.kind, TokenKind::Punct(s) if *s == punct)
}

fn is_ident(token: &Token, name: &str) -> bool {
    matches!(&token.kind, TokenKind::Identifier(s) if s == name)
}

/// A macro argument without the newlines a multi-line invocation leaves
/// around it. They are whitespace to every structural test here — a
/// `MOCK_METHOD` whose specifier list sits on the next line still starts its
/// parameter group at the first real token.
fn trim_newlines(tokens: &[Token]) -> &[Token] {
    let is_newline = |t: &Token| matches!(t.kind, TokenKind::Newline);
    let start = tokens.iter().position(|t| !is_newline(t)).unwrap_or(0);
    let end = tokens
        .iter()
        .rposition(|t| !is_newline(t))
        .map_or(start, |i| i + 1);
    &tokens[start..end.max(start)]
}

/// True when these tokens spell a type whose own declarator is parenthesized
/// — a function pointer, a reference to an array, a pointer to member.
/// Parentheses nested in template arguments (`std::function<void(int)>`) are
/// not declarators, so only a group opened outside every `<…>` counts, and
/// only when that group holds a declarator rather than an argument list: a
/// group of plain tokens is `decltype(x)`, or a macro spelling a
/// comma-containing type, both of which keep their spelling and re-expand.
fn is_parenthesized_declarator(tokens: &[Token]) -> bool {
    let mut angle = 0_i32;
    let mut paren = 0_u32;
    for (idx, token) in tokens.iter().enumerate() {
        match &token.kind {
            TokenKind::Punct(s) if *s == "(" => {
                // A later group can still be the declarator: the first one in
                // `decltype(x) (*)(int)` is not. What `decltype` encloses is
                // an operand and never a declarator, however that expression
                // starts — `decltype(*p)` and `decltype(*(p))` alike.
                let operand_of_decltype = idx > 0 && is_ident(&tokens[idx - 1], "decltype");
                if paren == 0
                    && angle <= 0
                    && !operand_of_decltype
                    && opens_declarator(&tokens[idx + 1..])
                {
                    return true;
                }
                paren += 1;
            }
            TokenKind::Punct(s) if *s == ")" => paren = paren.saturating_sub(1),
            TokenKind::Punct(s) if *s == "<" && paren == 0 => angle += 1,
            TokenKind::Punct(s) if *s == ">" && paren == 0 => angle -= 1,
            TokenKind::Punct(s) if *s == ">>" && paren == 0 => angle -= 2,
            _ => {}
        }
    }
    false
}

/// True when a parenthesized group, given from just after its `(`, is a
/// declarator rather than an argument list or an expression.
///
/// A declarator group is a ptr-operator sequence and nothing else: `(*)`,
/// `(&)`, `(&&)`, `(*const)`, or a nested declarator behind one — `(&())` of
/// `int (&())[4]`. Only a nested-name-specifier may precede it, naming the
/// class a pointer to member points into: `(C::*)`, `(::C::*)`, `(C<T>::*)`.
/// It must end in `::` (the lexer spells that as two `:` tokens), or the
/// group is an argument list — `(int *, char)`.
///
/// What follows the ptr-operators tells a declarator from an expression that
/// merely starts with one: a declarator continues into the group's `)`, a
/// nested declarator or an array bound, never into a name, so `decltype(*p)`
/// is read as the expression it is and keeps its spelling.
fn opens_declarator(body: &[Token]) -> bool {
    let mut i = 0;
    while let Some(token) = body.get(i) {
        match &token.kind {
            TokenKind::Identifier(_) => i += 1,
            TokenKind::Punct(s) if *s == ":" => i += 1,
            TokenKind::Punct(s) if *s == "<" => match skip_angle_group(body, i) {
                Some(next) => i = next,
                None => return false,
            },
            _ => break,
        }
    }
    // A nested-name-specifier, if there is one at all, has to end in `::`.
    if i > 0 && !(i >= 2 && is_punct(&body[i - 1], ":") && is_punct(&body[i - 2], ":")) {
        return false;
    }
    let ptr_start = i;
    while body
        .get(i)
        .is_some_and(|t| is_punct(t, "*") || is_punct(t, "&") || is_punct(t, "&&"))
    {
        i += 1;
    }
    if i == ptr_start {
        return false;
    }
    while body
        .get(i)
        .is_some_and(|t| is_ident(t, "const") || is_ident(t, "volatile"))
    {
        i += 1;
    }
    body.get(i)
        .is_some_and(|t| is_punct(t, ")") || is_punct(t, "(") || is_punct(t, "["))
}

/// Index just past the `>` closing the `<…>` group that opens at `at`.
fn skip_angle_group(tokens: &[Token], at: usize) -> Option<usize> {
    let mut depth = 0_i32;
    for (idx, token) in tokens.iter().enumerate().skip(at) {
        match &token.kind {
            TokenKind::Punct(s) if *s == "<" => depth += 1,
            TokenKind::Punct(s) if *s == ">" => depth -= 1,
            TokenKind::Punct(s) if *s == ">>" => depth -= 2,
            _ => continue,
        }
        if depth <= 0 {
            return Some(idx + 1);
        }
    }
    None
}

/// True when a comma sits outside every parenthesis and every `<…>` template
/// argument list — the same structural reading as
/// `is_parenthesized_declarator`, used to tell a type that merely contains
/// commas (`std::pair<int, int>`) from a list of several (`int, char`).
fn has_top_level_comma(tokens: &[Token]) -> bool {
    let mut angle = 0_i32;
    let mut paren = 0_u32;
    for token in tokens {
        match &token.kind {
            TokenKind::Punct(s) if *s == "(" => paren += 1,
            TokenKind::Punct(s) if *s == ")" => paren = paren.saturating_sub(1),
            // Only outside a parenthesis is `>` a template closer; inside one
            // it is greater-than, and counting it would unbalance the depth
            // for everything after the group — the comma of
            // `std::conditional_t<(A > B), X, Y>` would read as top-level and
            // the whole invocation would be rejected as an argument list.
            TokenKind::Punct(s) if *s == "<" && paren == 0 => angle += 1,
            TokenKind::Punct(s) if *s == ">" && paren == 0 => angle -= 1,
            TokenKind::Punct(s) if *s == ">>" && paren == 0 => angle -= 2,
            TokenKind::Punct(s) if *s == "," && paren == 0 && angle <= 0 => return true,
            _ => {}
        }
    }
    false
}

/// True when a parenthesis opens outside every `<…>` template argument list:
/// these tokens hold a group of their own rather than spelling a plain type
/// name.
fn has_top_level_paren(tokens: &[Token]) -> bool {
    let mut angle = 0_i32;
    let mut paren = 0_u32;
    for token in tokens {
        match &token.kind {
            TokenKind::Punct(s) if *s == "(" => {
                if paren == 0 && angle <= 0 {
                    return true;
                }
                paren += 1;
            }
            TokenKind::Punct(s) if *s == ")" => paren = paren.saturating_sub(1),
            TokenKind::Punct(s) if *s == "<" && paren == 0 => angle += 1,
            TokenKind::Punct(s) if *s == ">" && paren == 0 => angle -= 1,
            TokenKind::Punct(s) if *s == ">>" && paren == 0 => angle -= 2,
            _ => {}
        }
    }
    false
}

/// A parameter group with gMock's comma-protecting parentheses removed from
/// each parameter: `((std::map<int, double>), bool)` becomes
/// `(std::map<int, double>, bool)`, which is what C++ accepts.
fn strip_param_parens(params: &[Token]) -> Vec<Token> {
    if trailing_paren_group(params) != Some(0) {
        return params.to_vec();
    }
    let inner = &params[1..params.len() - 1];
    let mut parts: Vec<&[Token]> = Vec::new();
    let mut separators: Vec<&Token> = Vec::new();
    let mut depth = 0_u32;
    let mut start = 0;
    for (idx, token) in inner.iter().enumerate() {
        if is_punct(token, "(") {
            depth += 1;
        } else if is_punct(token, ")") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && is_punct(token, ",") {
            parts.push(&inner[start..idx]);
            separators.push(token);
            start = idx + 1;
        }
    }
    parts.push(&inner[start..]);
    let mut out = vec![params[0].clone()];
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            out.push(separators[idx - 1].clone());
        }
        out.extend(strip_outer_parens(trim_newlines(part)).iter().cloned());
    }
    out.push(params[params.len() - 1].clone());
    out
}

/// Index of the `(` opening the balanced group that ends `tokens`, if the
/// last token closes one.
fn trailing_paren_group(tokens: &[Token]) -> Option<usize> {
    if !tokens.last().is_some_and(|t| is_punct(t, ")")) {
        return None;
    }
    let mut depth = 0_u32;
    for (idx, token) in tokens.iter().enumerate().rev() {
        if is_punct(token, ")") {
            depth += 1;
        } else if is_punct(token, "(") {
            depth -= 1;
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

/// `tokens` without one enclosing pair of parentheses, or unchanged when the
/// first `(` does not pair with the last `)`.
fn strip_outer_parens(tokens: &[Token]) -> &[Token] {
    match trailing_paren_group(tokens) {
        Some(0) => &tokens[1..tokens.len() - 1],
        _ => tokens,
    }
}

/// How many tokens `substitute_macro` would materialize for this body and
/// argument list, computed without building anything.
///
/// Deliberately an upper bound rather than an exact count: `##` pastes and
/// placemarkers only ever remove tokens, and `#param` yields one literal
/// where this counts the parameter's width. Over-estimating charges a
/// little extra for those shapes; under-estimating would reopen the hole
/// this exists to close.
fn projected_substitution_len(
    body: &[Token],
    params: &[String],
    args: &MacroArgs,
    variadic: bool,
) -> u64 {
    let mut n: u64 = 0;
    for tok in body {
        let width = match &tok.kind {
            TokenKind::Identifier(name) => match params.iter().position(|p| p == name) {
                Some(idx) if is_variadic_tail(params, variadic, idx) => args.variadic_len(idx),
                Some(idx) => args.args.get(idx).map_or(0, |a| a.len()),
                None => 1,
            },
            _ => 1,
        };
        n = n.saturating_add(width as u64);
    }
    n
}

fn substitute_macro(
    macro_name: &str,
    origin: &Token,
    body: &[Token],
    params: &[String],
    args: &MacroArgs,
    variadic: bool,
) -> Vec<Token> {
    debug_assert!(
        !variadic || !params.is_empty(),
        "a variadic MacroDef must name its tail parameter \
         (\"__VA_ARGS__\" for the anonymous form; see parse_macro_param_list)"
    );
    let mut out: Vec<Token> = Vec::new();
    // Whitespace owed to the next token emitted: a parameter that had
    // whitespace before it and expanded to nothing leaves that whitespace
    // behind, so `S(a x+b)` with `x` empty stringizes to "a +b" as in gcc
    // and clang. (A `##` placemarker leaves none: `a ## y+b` is "a+b".)
    let mut gap = false;
    // Adjacency owed to the token that survives a placemarker paste: an empty
    // `##` operand takes its position, so `a x ## +b` with `x` empty spaces
    // the `+` off `a` exactly as the `x` did. Every path that emits a token
    // consumes it.
    let mut paste_adjacency = None;
    let mut i = 0;
    while i < body.len() {
        let adjacency = paste_adjacency.take().unwrap_or(body[i].adjacent_before);
        let concat_width = concat_width_at(body, i);
        if concat_width > 0 && i + concat_width < body.len() {
            if let TokenKind::Identifier(name) = &body[i + concat_width].kind {
                if let Some(idx) = params.iter().position(|p| p == name) {
                    let is_va_tail = is_variadic_tail(params, variadic, idx);
                    if is_va_tail
                        && matches!(
                            out.last().map(|t| &t.kind),
                            Some(TokenKind::Punct(s)) if *s == ","
                        )
                    {
                        // GNU `, ## args`: with the varargs omitted the
                        // comma is deleted; otherwise the `##` is inert — it
                        // must NOT reach apply_concatenation, which would
                        // fuse the comma with the first vararg token
                        // (destroying string/char literals and breaking
                        // rescan). An explicitly empty argument substitutes
                        // to nothing and the comma stays, like gcc.
                        if varargs_omitted(&args.args, idx) {
                            out.pop();
                            i += concat_width + 1;
                        } else {
                            i += concat_width;
                        }
                        continue;
                    }
                    let blank = if is_va_tail {
                        args_are_blank(&args.args, idx)
                    } else {
                        args.args.get(idx).is_none_or(|a| arg_is_blank(a))
                    };
                    if blank {
                        // C99 placemarker: an empty `##` operand makes the
                        // paste a no-op; the left operand stays as-is.
                        i += concat_width + 1;
                        continue;
                    }
                }
            }
        }
        if matches!(body[i].kind, TokenKind::Hash) {
            if let Some(TokenKind::Identifier(name)) = body.get(i + 1).map(|t| &t.kind) {
                if let Some(idx) = params.iter().position(|p| p == name) {
                    // C11 6.10.3.2: `#param` becomes a string literal
                    // spelling the argument as written (not expanded). Like
                    // every replacement-list token it keeps the definition
                    // coordinates and attributes to the expansion site via
                    // `with_macro_hide`.
                    let text = if is_variadic_tail(params, variadic, idx) {
                        stringize_spelling(&args.variadic_tokens(idx))
                    } else {
                        args.args
                            .get(idx)
                            .map(|a| stringize_spelling(a))
                            .unwrap_or_default()
                    };
                    let mut literal = Token::new(
                        TokenKind::String(format!("\"{text}\"")),
                        body[i].line,
                        body[i].col,
                    )
                    .with_macro_hide(origin, macro_name);
                    // The literal stands where the `#` stood, so it touches
                    // whatever the `#` touched: `S((#x))` is "(\"a\")".
                    literal.adjacent_before = adjacency;
                    push_substituted(&mut out, &mut gap, literal);
                    i += 2;
                    continue;
                }
            }
        }
        if let TokenKind::Identifier(name) = &body[i].kind {
            // An anonymous `...` registers `__VA_ARGS__` as the last param
            // (see parse_macro_param_list), and handle_define rewrites a
            // stray `__VA_ARGS__` in a named-variadic body to the tail
            // param, so plain position lookup covers both variadic styles.
            if let Some(idx) = params.iter().position(|p| p == name) {
                let first = out.len();
                if is_variadic_tail(params, variadic, idx) {
                    out.extend(args.variadic_tokens(idx));
                } else if let Some(arg) = args.args.get(idx) {
                    out.extend(arg.iter().cloned());
                }
                // Whether the argument touches what precedes it in the
                // body is the parameter's adjacency, not what happened to
                // precede the argument at the call site: `S((x))` invoked
                // as `S( a)` stringizes to "(a)" in gcc and clang. Newline
                // tokens are whitespace the argument kept for line
                // tracking, not tokens: the flag goes on the first real
                // token, and an argument with none is empty.
                match out[first..].iter_mut().find(|t| !is_newline(t)) {
                    Some(tok) => {
                        tok.adjacent_before = adjacency && !gap;
                        gap = false;
                    }
                    None => {
                        // C99 placemarker, mirroring the `## param` case
                        // above: the empty operand must swallow the `##`
                        // itself, or the operator would reach
                        // apply_concatenation and paste whatever preceded
                        // the parameter — `S(a x ## +b)` with `x` empty
                        // fusing `a` and `+` into the non-token `a+`. The
                        // exception is GNU `, ## __VA_ARGS__`, whose comma
                        // the `##` branch above deletes; hiding the operator
                        // there would leave the unparseable `f(a,)`.
                        let width = concat_width_after(body, i);
                        if width > 0
                            && !is_gnu_comma_paste(body, i + 1 + width, params, variadic, &out)
                        {
                            // The surviving right operand takes this
                            // parameter's position, and with it its
                            // adjacency; the argument's newlines go with the
                            // placemarker, as on the `## param` side.
                            out.truncate(first);
                            paste_adjacency = Some(adjacency);
                            i += 1 + width;
                            continue;
                        }
                        gap |= !adjacency;
                    }
                }
                i += 1;
                continue;
            }
        }
        // Replacement-list tokens (not from arguments) inherit the hide set,
        // and their own adjacency unless they survived a placemarker paste.
        let mut token = body[i].with_macro_hide(origin, macro_name);
        token.adjacent_before = adjacency;
        push_substituted(&mut out, &mut gap, token);
        i += 1;
    }
    out
}

/// Append `tok` to a substitution, separating it from its predecessor when
/// an emptied parameter left whitespace owed (see `substitute_macro`).
fn push_substituted(out: &mut Vec<Token>, gap: &mut bool, mut tok: Token) {
    if std::mem::take(gap) {
        tok.adjacent_before = false;
    }
    out.push(tok);
}

/// Apply `##` token pasting after parameter substitution.
fn apply_concatenation(mut tokens: Vec<Token>) -> Vec<Token> {
    loop {
        let mut next: Vec<Token> = Vec::new();
        let mut pasted = false;
        let mut i = 0;
        while i < tokens.len() {
            let width = concat_width_at(&tokens, i);
            if width > 0 {
                // Paste the previously emitted token with the operand after
                // `##`, so chains (`a ## b ## c`) collapse left to right. A
                // dangling `##` with no operand on either side is dropped.
                if let Some(left) = next.pop() {
                    if i + width < tokens.len() {
                        next.push(paste_two_tokens(&left, &tokens[i + width]));
                        i += width + 1;
                        pasted = true;
                    } else {
                        next.push(left);
                        i += width;
                    }
                } else {
                    i += width;
                }
            } else {
                next.push(tokens[i].clone());
                i += 1;
            }
        }
        // Rescan only when a paste ran: pasting `#` with `#` can re-form a
        // `##` operator; merely dropping a dangling `##` cannot.
        if !pasted {
            return next;
        }
        tokens = next;
    }
}

/// `concat_width_at` for the token that follows `i`, 0 at the end of `tokens`.
fn concat_width_after(tokens: &[Token], i: usize) -> usize {
    if i + 1 < tokens.len() {
        concat_width_at(tokens, i + 1)
    } else {
        0
    }
}

fn concat_width_at(tokens: &[Token], i: usize) -> usize {
    if matches!(&tokens[i].kind, TokenKind::Punct(s) if *s == "##") {
        return 1;
    }
    if matches!(&tokens[i].kind, TokenKind::Hash)
        && i + 1 < tokens.len()
        && matches!(tokens[i + 1].kind, TokenKind::Hash)
    {
        return 2;
    }
    0
}

/// Fallback definitions for macros whose real definitions live in headers the
/// indexed tree does not ship (gtest, kernel headers, `<inttypes.h>`). Left
/// unexpanded they produce tree-sitter ERROR nodes and whole functions get
/// dropped from the index (docs/PARSE_FAILURES.md catalogs the impact).
/// Built once; `install_builtin_macros` clones entries per preprocess. The
/// bodies are plain C, so the C lexer serves both languages.
static BUILTIN_FALLBACK_MACROS: LazyLock<Vec<(String, MacroDef)>> = LazyLock::new(|| {
    let object = |name: &str, replacement: &str| {
        (
            name.to_string(),
            MacroDef::Object {
                replacement: lex_macro_body(replacement, Language::C),
            },
        )
    };
    let function = |name: &str, params: &[&str], replacement: &str| {
        (
            name.to_string(),
            MacroDef::Function {
                params: params.iter().map(|s| s.to_string()).collect(),
                replacement: lex_macro_body(replacement, Language::C),
                variadic: false,
            },
        )
    };
    let mut table = Vec::new();
    // GNU/MSVC unused-parameter markers. Without this, an undefined
    // `__UNUSED` after a reference declarator (`T &event __UNUSED`) is
    // parsed as a broken `declaration` and the function body is dropped.
    table.push(object("__UNUSED", ""));
    // Linux kernel address-space / section annotations: `char __user *buf`
    // and `int __init foo(void)` are syntax errors when unexpanded.
    for name in [
        "__user",
        "__iomem",
        "__percpu",
        "__rcu",
        "__force",
        "__init",
        "__exit",
        "__initdata",
        "__exitdata",
        "__read_mostly",
    ] {
        table.push(object(name, ""));
    }
    // <inttypes.h> format-specifier strings: `"%" PRIu64` must expand to
    // a string literal or the adjacent-literal concatenation mis-parses.
    for (width, prefix) in [("8", "hh"), ("16", "h"), ("32", ""), ("64", "ll")] {
        for conv in ["d", "i", "u", "x", "X", "o"] {
            table.push(object(
                &format!("PRI{conv}{width}"),
                &format!("\"{prefix}{conv}\""),
            ));
        }
    }
    // `container_of(ptr, struct T, member)` puts a type keyword in
    // expression position; keep the pointer flow and the target type. The
    // `member` argument is deliberately dropped: an offsetof-shaped body
    // yields no additional call/flow facts and routes the pointer through
    // arithmetic the flow analysis tracks less precisely.
    table.push(function(
        "container_of",
        &["ptr", "type", "member"],
        "( ( type * ) ( void * ) ( ptr ) )",
    ));
    // gtest/OpenHarmony test macros: `HWTEST_F(Suite, Name, TestSize.Level1)`
    // followed by a body is unparseable unexpanded and every test body is
    // lost. Expand to a plain function definition so bodies get indexed.
    for name in ["HWTEST", "HWTEST_F", "HWTEST_P"] {
        table.push(function(
            name,
            &["a", "b", "level"],
            "static void a ## _ ## b ()",
        ));
    }
    // gMock declarations are often the only content of a test double: left
    // unexpanded they break the enclosing mock class and its overrides
    // vanish from virtual dispatch. Recover each as the member prototype it
    // declares (see expand_gmock_method); a replacement list cannot do this
    // because the legacy forms carry the whole signature in one argument
    // and the modern form parenthesizes comma-containing return types.
    table.push(("MOCK_METHOD".to_string(), MacroDef::GmockMethod));
    for arity in 0..=10 {
        for prefix in ["MOCK_METHOD", "MOCK_CONST_METHOD"] {
            for suffix in ["", "_T", "_WITH_CALLTYPE", "_T_WITH_CALLTYPE"] {
                table.push((format!("{prefix}{arity}{suffix}"), MacroDef::GmockMethod));
            }
        }
    }
    table
});

fn paste_two_tokens(left: &Token, right: &Token) -> Token {
    let text = format!(
        "{}{}",
        token_paste_fragment(&left.kind),
        token_paste_fragment(&right.kind)
    );
    Token {
        kind: TokenKind::Identifier(text),
        line: left.line,
        col: left.col,
        hidden: Token::union_hidden(left, right),
        origin: left.origin.or(right.origin),
        // Whatever separated `left` from the token before it still
        // separates the pasted result from it.
        adjacent_before: left.adjacent_before,
    }
}

fn token_paste_fragment(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Identifier(s) => s.clone(),
        TokenKind::Number(s) => s.clone(),
        TokenKind::Punct(s) if *s != "##" => (*s).to_string(),
        _ => String::new(),
    }
}

/// Whether the text already emitted ends in a preprocessing number, i.e.
/// whether a following `.` would be absorbed into it. Decided from the
/// trailing run of pp-number characters: the run is maximal, so if it opens
/// with a digit (or `.` then a digit) the lexer read it as a number, and if
/// it opens with a letter or `_` the lexer read it as an identifier, which
/// does not absorb a `.`.
fn output_ends_in_pp_number(output: &str) -> bool {
    let run_start = output
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '.'))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let mut run = output[run_start..].chars();
    match run.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('.') => run.next().is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    }
}

fn needs_leading_space(output: &str, kind: &TokenKind) -> bool {
    if output.is_empty() {
        return false;
    }
    if output.ends_with('\n') {
        return false;
    }
    let last = output.chars().last().unwrap();
    if last == ' ' {
        return false;
    }
    match kind {
        // Closing `)` / `]` must not gain a space (`operator()`, `foo[]`).
        // After a template `>`, a space before `&` / `*` keeps
        // `shared_ptr<T> &p` from gluing into `>&` which tree-sitter
        // fails to parse as a reference parameter.
        //
        // `"::"` is unreachable today: the lexer spells a scope operator as
        // two `:` tokens and nothing else builds a `Punct("::")` (token
        // pasting always yields an `Identifier`). It is kept because it is
        // the behaviour the arm would need if `::` ever becomes one token
        // (see #37), not because it fires now.
        TokenKind::Punct(s) => match *s {
            ";" | "," | "}" | "::" | "." => true,
            "&" | "*" => last == '>',
            // A pp-number swallows `.` and alphanumerics (C11 6.4.8), so an
            // ellipsis written straight after one re-lexes *into* it: the
            // GNU case range `case 1 ... 10:` emitted as `case 1...10:`
            // comes back as the single number `1...10` instead of
            // `1` `...` `10`. Same for `[0 ... 9]` designated ranges. Only
            // a number needs this — `Args...` after an identifier re-lexes
            // correctly and must stay glued.
            "..." => output_ends_in_pp_number(output),
            _ => false,
        },
        TokenKind::Newline => false,
        _ => !matches!(last, '(' | '[' | '{' | '.' | ';'),
    }
}

/// Body of the string literal `#param` produces for one argument (C11
/// 6.10.3.2p2): each token's spelling, whitespace between tokens collapsed
/// to a single space, leading/trailing whitespace dropped, and `"` / `\`
/// inside string and character literals escaped. Tokens carry no
/// whitespace; "was there whitespace" is the lexer's
/// `Token::adjacent_before`, which sees a `\`-newline splice as nothing.
fn stringize_spelling(arg: &[Token]) -> String {
    // Literals carry their quotes, which get escaped like any other `"` in
    // the spelling (`STR(R"(a)")` is `"R\"(a)\""`, as in gcc).
    spell_tokens(arg, escape_for_stringize)
}

/// Tokens spelled back as source text: tokens that touched in the source
/// touch here, anything else is one space apart. Whitespace width never
/// moves a token boundary but adjacency does (`R"(x)"` versus `R "(x)"`),
/// so this round-trips through the lexer. `literal` maps each string or
/// character literal's spelling.
pub(crate) fn spell_tokens(arg: &[Token], literal: impl Fn(&str) -> String) -> String {
    let mut text = String::new();
    let mut first = true;
    for tok in arg {
        let spelling = match &tok.kind {
            TokenKind::Newline | TokenKind::Eof => continue,
            TokenKind::Identifier(s) | TokenKind::Number(s) => s.clone(),
            TokenKind::Punct(s) => (*s).to_string(),
            TokenKind::Hash => "#".to_string(),
            TokenKind::String(s) | TokenKind::Char(s) => literal(s),
        };
        // A newline token is skipped above but is whitespace all the same:
        // the token after it never touches the one before it, which is
        // what its flag says, since the lexer computed it against the
        // newline.
        if !first && !tok.adjacent_before {
            text.push(' ');
        }
        first = false;
        text.push_str(&spelling);
    }
    text
}

/// A literal's spelling as it appears inside the string `#` builds:
/// backslashes and quotes escaped, and a bare newline — only a raw string
/// literal can contain one — written as `\n`, as gcc does, so the result is
/// still a single-line string literal. A CRLF is one newline: translation
/// phase 1 normalizes line endings before the raw string is read, so
/// gcc/clang stringize it as `\n`, never `\r\n`.
fn escape_for_stringize(spelling: &str) -> String {
    let mut out = String::with_capacity(spelling.len());
    let mut chars = spelling.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

fn token_to_string(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Identifier(s) => s.clone(),
        TokenKind::Number(s) => s.clone(),
        TokenKind::String(s) => s.clone(),
        TokenKind::Char(s) => s.clone(),
        TokenKind::Punct(s) => (*s).to_string(),
        TokenKind::Hash => "#".to_string(),
        TokenKind::Newline => "\n".to_string(),
        TokenKind::Eof => String::new(),
    }
}

/// Collect a function-like macro invocation's arguments from condition
/// tokens. `i` points just past the macro name; returns the argument token
/// lists and the index after the closing `)`, or `None` when the next token
/// is not `(` (uninvoked name) or the list is unterminated.
fn parse_cond_macro_args(toks: &[Token], mut i: usize) -> Option<(MacroArgs, usize)> {
    if !matches!(toks.get(i).map(|t| &t.kind), Some(TokenKind::Punct(p)) if *p == "(") {
        return None;
    }
    i += 1;
    let mut args = MacroArgs::default();
    let mut current: Vec<Token> = Vec::new();
    let mut depth = 0u32;
    while i < toks.len() {
        match &toks[i].kind {
            TokenKind::Punct(p) if *p == "(" => {
                depth += 1;
                current.push(toks[i].clone());
            }
            TokenKind::Punct(p) if *p == ")" && depth == 0 => {
                args.args.push(current);
                return Some((args, i + 1));
            }
            TokenKind::Punct(p) if *p == ")" => {
                depth -= 1;
                current.push(toks[i].clone());
            }
            TokenKind::Punct(p) if *p == "," && depth == 0 => {
                args.args.push(current);
                args.separators.push(toks[i].clone());
                current = Vec::new();
            }
            _ => current.push(toks[i].clone()),
        }
        i += 1;
    }
    None
}

/// Parse one `defined X` / `defined(X)` operator at `toks[i]` (which is the
/// `defined` identifier). Returns the operand name and how many tokens the
/// operator consumed; a malformed operand has no name and conservatively
/// evaluates to false.
///
/// The truth value is not decided here: `defined(X)` is a read of the macro
/// environment even though it substitutes nothing, and a cached header's
/// fingerprint has to see it (#55), so the caller resolves the name through
/// `is_defined_for_conditionals`.
fn defined_operand(toks: &[Token], i: usize) -> (Option<&str>, usize) {
    match toks.get(i + 1).map(|t| &t.kind) {
        Some(TokenKind::Punct(p)) if *p == "(" => {
            if let (Some(TokenKind::Identifier(n)), Some(TokenKind::Punct(c))) = (
                toks.get(i + 2).map(|t| &t.kind),
                toks.get(i + 3).map(|t| &t.kind),
            ) {
                if *c == ")" {
                    return (Some(n), 4);
                }
            }
            (None, 2)
        }
        Some(TokenKind::Identifier(n)) => (Some(n), 2),
        _ => (None, 1),
    }
}

/// Content hash of a macro binding, for [`crate::MacroFingerprint`].
/// Hashes what a consumer could observe: the replacement token stream
/// (spelling and adjacency, which is all `#` and `##` depend on), the
/// parameter list, and whether the name is only a builtin fallback.
fn hash_macro_binding(def: &MacroDef, fallback: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    fallback.hash(&mut h);
    let hash_tokens = |h: &mut std::collections::hash_map::DefaultHasher, toks: &[Token]| {
        for t in toks {
            t.adjacent_before.hash(h);
            match &t.kind {
                TokenKind::Identifier(sp) | TokenKind::Number(sp) => (0u8, sp.as_str()).hash(h),
                TokenKind::String(sp) => (1u8, sp.as_str()).hash(h),
                TokenKind::Char(sp) => (2u8, sp.as_str()).hash(h),
                TokenKind::Punct(sp) => (3u8, *sp).hash(h),
                TokenKind::Hash => 4u8.hash(h),
                TokenKind::Newline => 5u8.hash(h),
                TokenKind::Eof => 6u8.hash(h),
            }
        }
    };
    match def {
        MacroDef::Object { replacement } => {
            0u8.hash(&mut h);
            hash_tokens(&mut h, replacement);
        }
        MacroDef::Function {
            params,
            replacement,
            variadic,
        } => {
            1u8.hash(&mut h);
            params.hash(&mut h);
            variadic.hash(&mut h);
            hash_tokens(&mut h, replacement);
        }
        MacroDef::GmockMethod => 2u8.hash(&mut h),
    }
    h.finish()
}

/// Evaluate a fully expanded `#if` condition with C operator precedence.
/// Identifiers that survived expansion evaluate to 0 (C11 6.10.1p4), with
/// `true`/`false` as boolean literals (C++/C23; also the previous
/// evaluator's behavior); an unexpanded function-like call form
/// `ident(...)` swallows its argument list and evaluates to 0. Errors are
/// conservative: malformed input (parse error or trailing tokens) yields
/// false (branch skipped).
fn eval_pp_tokens(toks: &[Token]) -> bool {
    let mut p = PpExprParser {
        toks,
        pos: 0,
        err: false,
    };
    let v = p.ternary();
    // Malformed input is conservative: a parse error or unconsumed trailing
    // tokens must not activate a branch.
    if p.err || p.pos != p.toks.len() {
        return false;
    }
    v.truthy()
}

/// A preprocessor arithmetic value: 64-bit two's-complement bits plus the
/// C signedness of the expression, modeling intmax_t/uintmax_t evaluation
/// (C11 6.10.1p4). Binary operators apply the usual arithmetic
/// conversions: if either operand is unsigned the operation is unsigned
/// (so `-1 < 1U` is false — the -1 converts to uintmax_t).
#[derive(Clone, Copy)]
struct PpVal {
    bits: u64,
    unsigned_: bool,
}

impl PpVal {
    fn signed(v: i64) -> Self {
        Self {
            bits: v as u64,
            unsigned_: false,
        }
    }

    fn from_bool(b: bool) -> Self {
        Self::signed(b as i64)
    }

    fn truthy(self) -> bool {
        self.bits != 0
    }

    fn as_i64(self) -> i64 {
        self.bits as i64
    }

    fn either_unsigned(self, other: Self) -> bool {
        self.unsigned_ || other.unsigned_
    }
}

struct PpExprParser<'a> {
    toks: &'a [Token],
    pos: usize,
    /// Set on any syntax error (missing `)`/`:`, dangling operator,
    /// unterminated call form, non-expression token).
    err: bool,
}

impl<'a> PpExprParser<'a> {
    fn peek_punct(&self) -> Option<&str> {
        match self.toks.get(self.pos).map(|t| &t.kind) {
            Some(TokenKind::Punct(s)) => Some(s),
            _ => None,
        }
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.peek_punct() == Some(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn ternary(&mut self) -> PpVal {
        let c = self.logical_or();
        if self.eat("?") {
            let a = self.ternary();
            let b = if self.eat(":") {
                self.ternary()
            } else {
                self.err = true;
                PpVal::signed(0)
            };
            // The result type is the common type of BOTH arms (usual
            // arithmetic conversions), regardless of which arm is taken:
            // `1 ? -1 : 1U` is unsigned.
            let chosen = if c.truthy() { a } else { b };
            return PpVal {
                bits: chosen.bits,
                unsigned_: a.either_unsigned(b),
            };
        }
        c
    }

    fn logical_or(&mut self) -> PpVal {
        let mut v = self.logical_and();
        while self.eat("||") {
            let r = self.logical_and();
            v = PpVal::from_bool(v.truthy() || r.truthy());
        }
        v
    }

    fn logical_and(&mut self) -> PpVal {
        let mut v = self.bit_or();
        while self.eat("&&") {
            let r = self.bit_or();
            v = PpVal::from_bool(v.truthy() && r.truthy());
        }
        v
    }

    fn bit_or(&mut self) -> PpVal {
        let mut v = self.bit_xor();
        while self.peek_punct() == Some("|") {
            self.pos += 1;
            let r = self.bit_xor();
            v = PpVal {
                bits: v.bits | r.bits,
                unsigned_: v.either_unsigned(r),
            };
        }
        v
    }

    fn bit_xor(&mut self) -> PpVal {
        let mut v = self.bit_and();
        while self.eat("^") {
            let r = self.bit_and();
            v = PpVal {
                bits: v.bits ^ r.bits,
                unsigned_: v.either_unsigned(r),
            };
        }
        v
    }

    fn bit_and(&mut self) -> PpVal {
        let mut v = self.equality();
        while self.peek_punct() == Some("&") {
            self.pos += 1;
            let r = self.equality();
            v = PpVal {
                bits: v.bits & r.bits,
                unsigned_: v.either_unsigned(r),
            };
        }
        v
    }

    fn equality(&mut self) -> PpVal {
        let mut v = self.relational();
        loop {
            if self.eat("==") {
                v = PpVal::from_bool(v.bits == self.relational().bits);
            } else if self.eat("!=") {
                v = PpVal::from_bool(v.bits != self.relational().bits);
            } else {
                return v;
            }
        }
    }

    fn relational(&mut self) -> PpVal {
        // Comparisons follow the converted common type: unsigned if either
        // side is unsigned, else signed.
        fn lt(a: PpVal, b: PpVal) -> bool {
            if a.either_unsigned(b) {
                a.bits < b.bits
            } else {
                a.as_i64() < b.as_i64()
            }
        }
        let mut v = self.shift();
        loop {
            if self.eat("<=") {
                let r = self.shift();
                v = PpVal::from_bool(!lt(r, v));
            } else if self.eat(">=") {
                let r = self.shift();
                v = PpVal::from_bool(!lt(v, r));
            } else if self.eat("<") {
                let r = self.shift();
                v = PpVal::from_bool(lt(v, r));
            } else if self.eat(">") {
                let r = self.shift();
                v = PpVal::from_bool(lt(r, v));
            } else {
                return v;
            }
        }
    }

    fn shift(&mut self) -> PpVal {
        // Result type follows the left operand; `>>` is logical for
        // unsigned, arithmetic for signed. Amounts are masked to 0..63
        // (out-of-range shifts are UB in C).
        let mut v = self.additive();
        loop {
            if self.eat("<<") {
                let sh = self.additive().bits as u32 & 63;
                v = PpVal {
                    bits: v.bits.wrapping_shl(sh),
                    unsigned_: v.unsigned_,
                };
            } else if self.eat(">>") {
                let sh = self.additive().bits as u32 & 63;
                v = PpVal {
                    bits: if v.unsigned_ {
                        v.bits.wrapping_shr(sh)
                    } else {
                        v.as_i64().wrapping_shr(sh) as u64
                    },
                    unsigned_: v.unsigned_,
                };
            } else {
                return v;
            }
        }
    }

    fn additive(&mut self) -> PpVal {
        let mut v = self.multiplicative();
        loop {
            if self.eat("+") {
                let r = self.multiplicative();
                v = PpVal {
                    bits: v.bits.wrapping_add(r.bits),
                    unsigned_: v.either_unsigned(r),
                };
            } else if self.eat("-") {
                let r = self.multiplicative();
                v = PpVal {
                    bits: v.bits.wrapping_sub(r.bits),
                    unsigned_: v.either_unsigned(r),
                };
            } else {
                return v;
            }
        }
    }

    fn multiplicative(&mut self) -> PpVal {
        let mut v = self.unary();
        loop {
            if self.eat("*") {
                let r = self.unary();
                v = PpVal {
                    bits: v.bits.wrapping_mul(r.bits),
                    unsigned_: v.either_unsigned(r),
                };
            } else if self.eat("/") {
                let r = self.unary();
                v = self.divide(v, r, false);
            } else if self.eat("%") {
                let r = self.unary();
                v = self.divide(v, r, true);
            } else {
                return v;
            }
        }
    }

    /// `/` and `%` under the usual arithmetic conversions; division by
    /// zero conservatively yields 0.
    fn divide(&mut self, a: PpVal, b: PpVal, rem: bool) -> PpVal {
        let unsigned_ = a.either_unsigned(b);
        if b.bits == 0 {
            return PpVal { bits: 0, unsigned_ };
        }
        let bits = if unsigned_ {
            if rem {
                a.bits % b.bits
            } else {
                a.bits / b.bits
            }
        } else if rem {
            a.as_i64().wrapping_rem(b.as_i64()) as u64
        } else {
            a.as_i64().wrapping_div(b.as_i64()) as u64
        };
        PpVal { bits, unsigned_ }
    }

    fn unary(&mut self) -> PpVal {
        if self.eat("!") {
            return PpVal::from_bool(!self.unary().truthy());
        }
        if self.eat("~") {
            let v = self.unary();
            return PpVal {
                bits: !v.bits,
                unsigned_: v.unsigned_,
            };
        }
        if self.eat("-") {
            // Negation keeps the operand's signedness (`-1U` stays
            // unsigned in C and wraps).
            let v = self.unary();
            return PpVal {
                bits: v.bits.wrapping_neg(),
                unsigned_: v.unsigned_,
            };
        }
        if self.eat("+") {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> PpVal {
        let Some(tok) = self.toks.get(self.pos) else {
            // Dangling operator with no operand.
            self.err = true;
            return PpVal::signed(0);
        };
        match &tok.kind {
            // A number or character constant that is not an integer
            // constant — a ud-suffix (`10_km`, `'a'_x`), a floating literal
            // — is an error for gcc/clang, not a value; marking the
            // expression malformed keeps the branch closed.
            TokenKind::Number(s) => {
                self.pos += 1;
                match parse_pp_int(s) {
                    Some(v) => v,
                    None => {
                        self.err = true;
                        PpVal::signed(0)
                    }
                }
            }
            TokenKind::Char(s) => {
                self.pos += 1;
                match char_literal_body(s) {
                    Some(body) => PpVal::signed(char_value(body)),
                    None => {
                        self.err = true;
                        PpVal::signed(0)
                    }
                }
            }
            TokenKind::Punct(p) if *p == "(" => {
                self.pos += 1;
                let v = self.ternary();
                if !self.eat(")") {
                    self.err = true;
                }
                v
            }
            TokenKind::Identifier(name) => {
                // C++ / C23 boolean literals (also matches the previous
                // evaluator's behavior for `#if true`).
                if name == "true" {
                    self.pos += 1;
                    return PpVal::signed(1);
                }
                if name == "false" {
                    self.pos += 1;
                    return PpVal::signed(0);
                }
                self.pos += 1;
                // Unexpanded function-like form: swallow the balanced
                // argument list so the caller's operator loop resumes
                // cleanly after it.
                if self.peek_punct() == Some("(") {
                    let mut depth = 0i32;
                    let mut closed = false;
                    while let Some(t) = self.toks.get(self.pos) {
                        match &t.kind {
                            TokenKind::Punct(p) if *p == "(" => depth += 1,
                            TokenKind::Punct(p) if *p == ")" => {
                                depth -= 1;
                                if depth == 0 {
                                    self.pos += 1;
                                    closed = true;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    if !closed {
                        self.err = true;
                    }
                }
                PpVal::signed(0)
            }
            // Non-expression token (string literal, stray punct): malformed.
            _ => {
                self.pos += 1;
                self.err = true;
                PpVal::signed(0)
            }
        }
    }
}

/// Parse a C preprocessor integer literal (decimal, hex, octal, binary,
/// with optional u/U/l/L suffixes). Anything else — a floating literal, a
/// user-defined-literal suffix — is `None`. The value is unsigned when it
/// carries a `u`/`U` suffix or does not fit in a signed 64-bit intmax_t
/// (hex/octal ladder reaching uintmax_t).
fn parse_pp_int(s: &str) -> Option<PpVal> {
    let t = s.trim_end_matches(['u', 'U', 'l', 'L']);
    let unsigned_suffix = s[t.len()..].contains(['u', 'U']);
    let (digits, radix) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (h, 16)
    } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        (b, 2)
    } else if t.len() > 1 && t.starts_with('0') {
        (&t[1..], 8)
    } else {
        (t, 10)
    };
    let v = u128::from_str_radix(digits, radix).ok()?;
    Some(PpVal {
        bits: v as u64,
        unsigned_: unsigned_suffix || v > i64::MAX as u128,
    })
}

/// Body of an ordinary, unprefixed `"…"` literal spelling — the only form
/// that names a header in `#include`.
fn plain_string_body(spelling: &str) -> Option<&str> {
    spelling.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
}

/// Body of a character literal spelling, any encoding prefix dropped
/// (`L'\n'` → `\n`). `None` when the spelling does not end at its closing
/// quote — a user-defined literal such as `'a'_x`, which is not a
/// character constant.
fn char_literal_body(spelling: &str) -> Option<&str> {
    let open = spelling.find('\'').map_or(0, |i| i + 1);
    spelling[open..].strip_suffix('\'')
}

/// Value of a character constant's body (see `char_literal_body`; escapes
/// kept verbatim).
fn char_value(s: &str) -> i64 {
    let mut chars = s.chars().peekable();
    match chars.next() {
        Some('\\') => match chars.next() {
            Some('n') => 10,
            Some('t') => 9,
            Some('r') => 13,
            Some('a') => 7,
            Some('b') => 8,
            Some('f') => 12,
            Some('v') => 11,
            Some('\\') => 92,
            Some('\'') => 39,
            Some('"') => 34,
            Some('?') => 63,
            // \x… hexadecimal escape.
            Some('x') => {
                let mut v: i64 = 0;
                while let Some(d) = chars.peek().and_then(|c| c.to_digit(16)) {
                    v = v.wrapping_mul(16).wrapping_add(d as i64);
                    chars.next();
                }
                v
            }
            // \ooo octal escape (1-3 digits, first already consumed).
            Some(d @ '0'..='7') => {
                let mut v: i64 = d as i64 - '0' as i64;
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(o) => {
                            v = v * 8 + o as i64;
                            chars.next();
                        }
                        None => break,
                    }
                }
                v
            }
            Some(c) => c as i64,
            None => 0,
        },
        Some(c) => c as i64,
        None => 0,
    }
}

pub fn preprocess_file(
    path: &Path,
    opts: &PreprocessOptions,
) -> Result<PreprocessResult, PreprocessError> {
    let mut state = PreprocessorState::new(opts.clone(), path.to_path_buf());
    state.process_file(path)?;
    Ok(state.finish())
}

pub fn preprocess_string(source: &str, file: &Path, opts: &PreprocessOptions) -> PreprocessResult {
    let mut state = PreprocessorState::new(opts.clone(), file.to_path_buf());
    let tokens = Lexer::new(source, state.language).tokenize();
    if let Err(e) = state.process_file_tokens(&tokens) {
        state.warn(1, format!("preprocess stopped: {e}"));
    }
    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macros::macro_table_from_defines;
    use crate::options::{ExpansionCache, IncludeExpansion};

    /// The one cached expansion of `path`. These tests seed a single macro
    /// environment, so a second variant would mean the fingerprinting split
    /// something it should not have (see `ExpansionVariants`).
    fn sole_variant(
        cache: &ExpansionCache,
        path: PathBuf,
        language: Language,
    ) -> Option<IncludeExpansion> {
        let guard = cache.read().unwrap();
        let variants = guard.get(&(path, language))?;
        assert!(
            variants.len() <= 1,
            "expected one cached expansion, got {}",
            variants.len()
        );
        variants.first().cloned()
    }
    use std::sync::{Arc, RwLock};

    #[test]
    fn expands_function_like_macro() {
        let src = "#define SQUARE(x) ((x) * (x))\nint y = SQUARE(n);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("((n") && result.output.contains("*"),
            "{}",
            result.output
        );
        assert!(!result.output.contains("SQUARE"));
    }

    #[test]
    fn expands_function_like_field_macro() {
        let src = "#define FIELD_P(o) ((o)->inner.p)\nFIELD_P(obj);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("inner") && result.output.contains("obj"),
            "{}",
            result.output
        );
        assert!(!result.output.contains("FIELD_P"));
    }

    #[test]
    fn expands_token_paste_concat() {
        let src = "#define CAT(a,b) a ## b\nint CAT(x, y);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("xy") || result.output.contains("x y"),
            "{}",
            result.output
        );
        assert!(!result.output.contains("CAT"));
    }

    #[test]
    fn expands_chained_token_paste() {
        let src = "#define CAT3(a,b,c) a ## b ## c\nint CAT3(x, y, z);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("xyz"), "{}", result.output);
    }

    #[test]
    fn expands_object_macro() {
        let opts = PreprocessOptions::new().with_define("NULL", "0");
        let result = preprocess_string("int *p = NULL;", Path::new("test.c"), &opts);
        assert!(result.output.contains("int") && result.output.contains("0"));
        assert!(!result.output.contains("NULL"));
    }

    /// Regression (#6): a body starting with `(.` is an object macro, not a
    /// function-like macro whose parameter list begins with `...`. The old
    /// classifier matched a bare `.` and then aborted the whole file when the
    /// parameter list failed to parse.
    #[test]
    fn object_macro_body_starting_with_dot_does_not_abort() {
        let src = "#define HALF (.5)\n#define ORIGIN (.x = 0, .y = 0)\nint x = HALF;\nstruct p o = ORIGIN;\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let flat = result.output.replace([' ', '\n'], "");
        assert!(flat.contains("intx=(.5);"), "{}", result.output);
        assert!(flat.contains("o=(.x=0,.y=0);"), "{}", result.output);
        assert!(flat.contains("intafter;"), "{}", result.output);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    /// Regression (#7): `#define ALIAS (VALUE)` is an object macro whose body
    /// is `(VALUE)`. Whitespace separates the `(` from the name, so it does
    /// not open a parameter list (C11 6.10.3p10).
    #[test]
    fn object_macro_parenthesized_identifier_expands() {
        let src = "#define VALUE 42\n#define ALIAS (VALUE)\nint x = ALIAS;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let flat = result.output.replace([' ', '\n'], "");
        assert!(flat.contains("intx=(42);"), "{}", result.output);
        assert!(!result.output.contains("ALIAS"), "{}", result.output);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    /// `#define F (x) x` is an object macro even though `(x)` would be a
    /// valid parameter list; `F(1)` therefore expands to `(x) x(1)` (gcc).
    #[test]
    fn function_like_macro_requires_paren_adjacent_to_name() {
        let src = "#define F (x) x\nint y = F(1);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let flat = result.output.replace([' ', '\n'], "");
        assert!(flat.contains("inty=(x)x(1);"), "{}", result.output);
    }

    /// `\`-newline is spliced before the `(` adjacency test (translation
    /// phase 2), so `F\` + `(x)` on the next line is function-like, while a
    /// space before the `\` still separates the `(` from the name.
    #[test]
    fn function_like_macro_name_split_by_line_splice() {
        let src = "#define F\\\n(x) x\n#define G \\\n(x) x\nint a = F(7);\nint b = G(7);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let flat = result.output.replace([' ', '\n'], "");
        assert!(flat.contains("inta=7;"), "{}", result.output);
        assert!(flat.contains("intb=(x)x(7);"), "{}", result.output);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn preproc_if0_skips_define_in_dead_branch() {
        let src = "#if 0\n#define HIDDEN 42\n#endif\nint x = 1;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("42"));
        assert!(result.output.contains("x = 1") || result.output.contains("int x"));
    }

    /// Regression: `#pragma pack(push, 4)` immediately followed by a struct
    /// definition (e.g. OpenHarmony pwm_if.h) must not swallow the next line.
    #[test]
    fn pragma_keeps_next_line_and_does_not_warn() {
        let src = "#pragma pack(push, 4)\nstruct PwmConfig {\n    int duty;\n};\n#pragma pack(pop)\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("struct PwmConfig"),
            "line after #pragma was swallowed: {}",
            result.output
        );
        assert!(result.output.contains("after"), "{}", result.output);
        assert!(
            !result.output.contains("pack"),
            "pragma text must not leak into output: {}",
            result.output
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown directive")),
            "#pragma is a standard directive, no warning expected: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn line_directive_keeps_next_line() {
        let src = "#line 100 \"orig.c\"\nint kept;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
    }

    #[test]
    fn unknown_directive_warns_but_keeps_next_line() {
        let src = "#frobnicate all the things\nint kept;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown directive")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn handles_ifdef() {
        let opts = PreprocessOptions::new().with_define("FEATURE", "1");
        let src = "#ifdef FEATURE\nint x;\n#else\nint y;\n#endif\n";
        let result = preprocess_string(src, Path::new("test.c"), &opts);
        assert!(result.output.contains("int x") || result.output.contains("int  x"));
        assert!(!result.output.contains("int y"));
    }

    #[test]
    fn handles_ifdef_file() {
        use std::path::PathBuf;
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/preproc/ifdef.c");
        let opts = PreprocessOptions::new().with_define("FEATURE", "1");
        let result = preprocess_file(&path, &opts).unwrap();
        assert!(
            result.output.contains("enabled") && result.output.contains("1"),
            "output was: {}",
            result.output
        );
    }

    #[test]
    fn if_else_selects_active_branch_only() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/if_else.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(
            result.output.contains("active"),
            "expected #if FEATURE branch, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("dead"),
            "dead branch must not appear: {}",
            result.output
        );
        assert!(
            !result.output.contains("also_dead"),
            "inverse branch must not appear: {}",
            result.output
        );
        assert!(
            result.output.contains("also_active"),
            "expected #else after !FEATURE, got: {}",
            result.output
        );
    }

    #[test]
    fn if_macro_value_expands_in_condition() {
        let src = "#define OUTER 1\n#if OUTER\nint on;\n#else\nint off;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("on"), "{}", result.output);
        assert!(!result.output.contains("off"), "{}", result.output);
    }

    #[test]
    fn nested_if_respects_inner_else() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/nested_if.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(result.output.contains("outer_on"), "{}", result.output);
        assert!(!result.output.contains("inner_on"), "{}", result.output);
        assert!(result.output.contains("inner_off"), "{}", result.output);
        assert!(!result.output.contains("outer_off"), "{}", result.output);
    }

    #[test]
    fn ifndef_and_else_inverse() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/ifndef_else.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(result.output.contains("guarded"), "{}", result.output);
        assert!(!result.output.contains("unguarded"), "{}", result.output);
        assert!(result.output.contains("present"), "{}", result.output);
        assert!(!result.output.contains("missing"), "{}", result.output);
    }

    #[test]
    fn self_referential_object_macro_is_not_reexpanded() {
        // Hiview `PRIVATE_MESSAGE_TYPE` X-macro: the replacement list starts
        // with the macro's own name (an enumerator). C11 6.10.3.4 paints
        // that token so expansion terminates; without a hide set this
        // recurses until the stack overflows.
        let src = "\
#define PRIVATE_MESSAGE_TYPE \\\n\
        PRIVATE_MESSAGE_TYPE, \\\n\
        ENGINE_UPLOAD_READY_MSG\n\
enum { PRIVATE_MESSAGE_TYPE };\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("PRIVATE_MESSAGE_TYPE")
                && result.output.contains("ENGINE_UPLOAD_READY_MSG"),
            "{}",
            result.output
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expansion depth exceeded")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn mutual_object_macros_terminate() {
        let src = "#define A B+B\n#define B A\nint x = A;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let compact: String = result
            .output
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            compact.contains("A+A") || compact.contains("x=A+A"),
            "{}",
            result.output
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expansion depth exceeded")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn nested_same_function_macro_still_expands() {
        let src = "#define MIN(a, b) ((a) < (b) ? (a) : (b))\nint x = MIN(MIN(1, 2), 3);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !result.output.contains("MIN"),
            "nested MIN must fully expand: {}",
            result.output
        );
        assert!(
            result.output.contains("1") && result.output.contains("3"),
            "{}",
            result.output
        );
    }

    #[test]
    fn cpp_operator_call_keeps_adjacent_parens() {
        let src = "struct Fn { void operator()() {} };\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            result.output.contains("operator()"),
            "operator() must not become operator( ): {}",
            result.output
        );
        assert!(!result.output.contains("operator( )"), "{}", result.output);
        let src = "void f(const std::shared_ptr<Plugin> &p);\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            result.output.contains("> &") || result.output.contains("> &p"),
            "template-id and reference must not glue: {}",
            result.output
        );
    }

    #[test]
    fn unused_macro_is_predefined_empty() {
        let src = "void f(int &x __UNUSED) { (void)x; }\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            !result.output.contains("__UNUSED"),
            "__UNUSED must expand away: {}",
            result.output
        );
        assert!(
            result.output.contains("int") && result.output.contains("&"),
            "{}",
            result.output
        );
    }

    #[test]
    fn unused_macro_applies_with_shared_table() {
        let shared = Arc::new(RwLock::new(MacroTable::new()));
        let opts = PreprocessOptions::new().with_shared_macros(Arc::clone(&shared));
        let src = "void f(int &x __UNUSED) {}\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &opts);
        assert!(
            !result.output.contains("__UNUSED"),
            "builtins must apply after cloning the shared table: {}",
            result.output
        );
    }

    #[test]
    fn kernel_annotation_macros_predefined_empty() {
        let src = "static long Read(struct file* f, char __user* buf);\n\
                   static int __init DriverInit(void) { return 0; }\n\
                   static void __exit DriverExit(void) {}\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        for name in ["__user", "__init", "__exit"] {
            assert!(
                !result.output.contains(name),
                "{name} must expand away: {}",
                result.output
            );
        }
        assert!(result.output.contains("DriverInit"), "{}", result.output);
    }

    #[test]
    fn container_of_macro_predefined() {
        let src = "void f(struct Node* p) { struct Dev* d = container_of(p, struct Dev, node); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !result.output.contains("container_of"),
            "container_of must expand away: {}",
            result.output
        );
        assert!(
            result.output.contains("struct Dev *") || result.output.contains("struct Dev*"),
            "expansion must cast to the requested type: {}",
            result.output
        );
    }

    #[test]
    fn pri_format_macros_predefined() {
        let src = "void f(unsigned long long v) { printf(\"val %\" PRIu64 \"\\n\", v); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !result.output.contains("PRIu64"),
            "PRIu64 must expand to a string literal: {}",
            result.output
        );
        assert!(result.output.contains("\"llu\""), "{}", result.output);
    }

    #[test]
    fn raw_string_literal_is_emitted_verbatim() {
        // Issue #14: `R"(a "quoted" b)"` came out as `R "(a " quoted " b)"`.
        let src =
            "const char* j = R\"(a \"quoted\" b)\";\nauto r = R\"~(=((\".*?\")|(\\S*)))~\";\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains("j= R\"(a \"quoted\" b)\" ;"), "{out}");
        assert!(out.contains("r= R\"~(=((\".*?\")|(\\S*)))~\" ;"), "{out}");
    }

    #[test]
    fn raw_string_spanning_lines_keeps_following_lines_mapped() {
        let src = "std::string s = R\"~({\n  \"k\": 1,\n  \"v\": 2})~\";\nint z;\n";
        let mut opts = PreprocessOptions::new();
        opts.track_line_map = true;
        let result = preprocess_string(src, Path::new("t.cpp"), &opts);
        assert!(
            result
                .output
                .contains("R\"~({\n  \"k\": 1,\n  \"v\": 2})~\" ;\nint z ;"),
            "{}",
            result.output
        );
        let at = result.output.find("int z").unwrap();
        let entry = result.line_map.lookup(at).unwrap();
        assert_eq!((entry.line, entry.col), (4, 1), "{entry:?}");
        // Offsets inside the literal attribute to where it starts.
        let inside = result.output.find("\"v\"").unwrap();
        let entry = result.line_map.lookup(inside).unwrap();
        assert_eq!((entry.line, entry.col), (1, 17), "{entry:?}");
    }

    #[test]
    fn raw_string_passes_through_macro_bodies_and_arguments() {
        let src = "#define ID(x) x\n#define J R\"~({\"k\":1})~\"\nauto a = ID(R\"(p, \"q\")\");\nauto b = J;\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            result.output.contains("a= R\"(p, \"q\")\" ;"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("b= R\"~({\"k\":1})~\" ;"),
            "{}",
            result.output
        );
    }

    #[test]
    fn raw_user_defined_literal_keeps_its_suffix_adjacent() {
        let src = "#define ID(x) x\n#define STR(x) #x\nauto a = R\"(json)\"_json;\nauto b = ID(R\"~(k)~\"_json);\nauto c = STR(R\"(a)\"_json);\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains("a= R\"(json)\"_json ;"), "{out}");
        assert!(out.contains("b= R\"~(k)~\"_json ;"), "{out}");
        assert!(out.contains("c= \"R\\\"(a)\\\"_json\" ;"), "{out}");
    }

    #[test]
    fn stringize_escapes_raw_string_argument_like_gcc() {
        // gcc/clang: `STR(R"(a "b")")` is `"R\"(a \"b\")\""`.
        let src = "#define STR(x) #x\nconst char* s = STR(R\"(a \"b\")\");\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            result.output.contains("s= \"R\\\"(a \\\"b\\\")\\\"\" ;"),
            "{}",
            result.output
        );
        // A newline inside the raw string is written as `\n` (gcc's
        // cpp_quote_string), so the result stays a single-line literal.
        let src = "#define STR(x) #x\nconst char* m = STR(R\"~(a\nb)~\");\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            result.output.contains("m= \"R\\\"~(a\\nb)~\\\"\" ;"),
            "{}",
            result.output
        );
    }

    #[test]
    fn encoding_prefixed_literals_stay_attached_to_their_prefix() {
        // `L'x'` used to come out as `L 'x'` (a tree-sitter ERROR site) and
        // `L"w"` as `L "w"`.
        let src = "#define STR(x) #x\nwchar_t c = L'x';\nconst char* s = u8\"s\";\nconst char* t = STR(L\"a\\n\");\n#if L'a' == 97 && u'\\n' == 10\nyes\n#else\nno\n#endif\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains("c= L'x' ;"), "{out}");
        assert!(out.contains("s= u8\"s\" ;"), "{out}");
        assert!(out.contains("t= \"L\\\"a\\\\n\\\"\" ;"), "{out}");
        assert!(out.contains("yes") && !out.contains("no"), "{out}");
    }

    #[test]
    fn c_lexes_raw_string_and_udl_shapes_as_separate_tokens() {
        // Valid C: `R` and `C` are macros, and the literals next to them are
        // separate preprocessing tokens (C11 6.4). The same text in a C++ TU
        // is one raw-string / user-defined-literal token, so the macros do
        // not expand there.
        let src = "#define R const char *s =\nR\"(x)\";\n#define C + 1\nint n = 'a'C;\n";
        let c = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new()).output;
        assert!(c.contains("s= \"(x)\" ;"), "{c}");
        assert!(c.contains("n= 'a'+ 1 ;"), "{c}");
        let cpp = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new()).output;
        assert!(cpp.contains("R\"(x)\" ;"), "{cpp}");
        assert!(cpp.contains("n= 'a'C ;"), "{cpp}");
        // The language option overrides the extension.
        let forced = preprocess_string(
            src,
            Path::new("t.c"),
            &PreprocessOptions::new().with_language(Language::Cpp),
        )
        .output;
        assert!(forced.contains("R\"(x)\" ;"), "{forced}");
    }

    #[test]
    fn expansion_cache_keeps_one_entry_per_language() {
        // One shared cache, a header reached from a C++ unit and then a C
        // unit. The C++ entry must not be replayed into the C unit: its
        // `VAL` replacement list is one Char token there, so `C` would
        // never expand.
        let dir = unique_tmp_dir("cache_language");
        fs::write(dir.join("shared.h"), "#define C + 1\n#define VAL 'a'C\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let run = |name: &str| {
            let path = dir.join(name);
            fs::write(&path, "#include \"shared.h\"\nint n = VAL;\n").unwrap();
            let opts = PreprocessOptions::new()
                .with_include_expansion_cache(Arc::clone(&cache))
                .with_include(dir.to_path_buf());
            preprocess_file(&path, &opts).unwrap().output
        };
        let cpp = run("t.cpp");
        assert!(cpp.contains("n= 'a'C ;"), "{cpp}");
        let c = run("t.c");
        assert!(c.contains("n= 'a'+ 1 ;"), "{c}");
        // Both entries live side by side; a third run of each hits its own.
        let keys: Vec<Language> = cache
            .read()
            .unwrap()
            .keys()
            .filter(|(p, _)| p.ends_with("shared.h"))
            .map(|(_, l)| *l)
            .collect();
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(keys.contains(&Language::C) && keys.contains(&Language::Cpp));
        assert!(run("u.c").contains("n= 'a'+ 1 ;"));
        assert!(run("u.cpp").contains("n= 'a'C ;"));
    }

    #[test]
    fn included_header_is_lexed_as_the_including_tu() {
        let dir = unique_tmp_dir("header_language");
        fs::write(dir.join("raw.h"), "#define R const char *s =\nR\"(x)\";\n").unwrap();
        let tu = |name: &str| {
            let path = dir.join(name);
            fs::write(&path, "#include \"raw.h\"\n").unwrap();
            preprocess_file(&path, &PreprocessOptions::new())
                .unwrap()
                .output
        };
        let c = tu("t.c");
        assert!(c.contains("s= \"(x)\" ;"), "{c}");
        let cpp = tu("t.cpp");
        assert!(cpp.contains("R\"(x)\" ;"), "{cpp}");
    }

    #[test]
    fn user_defined_literal_in_if_expression_is_malformed() {
        // gcc/clang reject a ud-suffix in a preprocessing expression; the
        // literal must not evaluate as if the suffix were not there.
        for cond in ["'a'_x == 97", "10_km", "0x10_u == 16", "1.5"] {
            let src = format!("#if {cond}\nyes\n#else\nno\n#endif\n");
            let out = preprocess_string(&src, Path::new("t.cpp"), &PreprocessOptions::new()).output;
            assert!(
                out.contains("no") && !out.contains("yes"),
                "#if {cond}: {out}"
            );
        }
        // The plain forms still evaluate.
        let src = "#if 'a' == 97 && 0x10u == 16 && 10 == 10\nyes\n#else\nno\n#endif\n";
        let out = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new()).output;
        assert!(out.contains("yes") && !out.contains("no"), "{out}");
    }

    #[test]
    fn stringize_coalesces_crlf_inside_raw_string() {
        // Translation phase 1 turns CRLF into a newline, so clang stringizes
        // a raw string spanning CRLF lines with `\n` alone.
        let src = "#define STR(x) #x\r\nconst char* m = STR(R\"~(a\r\nb)~\");\r\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            result.output.contains("m= \"R\\\"~(a\\nb)~\\\"\" ;"),
            "{}",
            result.output
        );
        assert_eq!(escape_for_stringize("a\r\nb"), "a\\nb");
        assert_eq!(escape_for_stringize("a\rb"), "a\\rb");
    }

    #[test]
    fn literal_body_helpers_strip_delimiters() {
        assert_eq!(plain_string_body("\"a.h\""), Some("a.h"));
        assert_eq!(plain_string_body("\"\""), Some(""));
        assert_eq!(plain_string_body("L\"a.h\""), None);
        assert_eq!(plain_string_body("R\"(a.h)\""), None);
        assert_eq!(char_literal_body("'a'"), Some("a"));
        assert_eq!(char_literal_body("L'\\n'"), Some("\\n"));
        assert_eq!(char_literal_body("u8'x'"), Some("x"));
        // A ud-suffix means the spelling is not a plain character constant.
        assert_eq!(char_literal_body("'a'_x"), None);
    }

    #[test]
    fn hwtest_macros_predefined_as_functions() {
        let src = "HWTEST_F(FooTest, Bar, TestSize.Level1)\n{\n    int x = 0;\n    (void)x;\n}\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(
            !result.output.contains("HWTEST_F"),
            "HWTEST_F must expand away: {}",
            result.output
        );
        assert!(
            result.output.contains("FooTest_Bar"),
            "expansion must produce a pasted function name: {}",
            result.output
        );
        assert!(
            !result.output.contains("TestSize"),
            "the level argument must be dropped: {}",
            result.output
        );
    }

    #[test]
    fn gmock_method_fallbacks_produce_declarations() {
        let src = concat!(
            "#define MAP_T(a, b) std::map<a, b>\n",
            // Aliases hide gMock's structure until they are expanded.
            "#define RET_ALIAS (std::pair<int, int>)\n",
            "#define PARAMS_ALIAS (int, int)\n",
            "#define SPEC_ALIAS (const, override)\n",
            "#define SIG_ALIAS int(int)\n",
            "class Mock {\npublic:\n",
            "    MOCK_METHOD(int, LinkNext, (int value), (override));\n",
            "    MOCK_METHOD(int, Ready, ());\n",
            "    MOCK_METHOD((std::pair<int, int>), Pair, (), ());\n",
            "    MOCK_METHOD(void, Peek, (), (const, noexcept, override));\n",
            "    MOCK_METHOD(void, Call, (), (Calltype(STDMETHODCALLTYPE)));\n",
            "    MOCK_METHOD(bool, CheckMap, ((std::map<int, double>), bool), (override));\n",
            "    MOCK_METHOD((void (*)(int)), GetHandler, (), (override));\n",
            "    MOCK_METHOD((void (C::*)(int)), GetMemberPtr, (), (override));\n",
            "    MOCK_METHOD(decltype(handle_), GetHandle, (), (const));\n",
            "    MOCK_METHOD((MAP_T(int, double)), GetMapped, (), (override));\n",
            "    MOCK_METHOD((MAP_T(int *, char)), GetPtrMapped, (), (override));\n",
            "    MOCK_METHOD((std::function<void(int)>), GetFn, (), (override));\n",
            // A `>` inside a parenthesis is greater-than, not a template
            // closer: counting it would unbalance the angle depth and make
            // the commas after it look like an argument list.
            "    MOCK_METHOD((std::conditional_t<(A > B), X, Y>), Conditional, (),\n",
            "        (override));\n",
            "    MOCK_METHOD0(Start, int());\n",
            "    MOCK_METHOD1(Attach, int(int value));\n",
            "    MOCK_CONST_METHOD2(Inspect, int(int left, int right));\n",
            "    MOCK_METHOD1_T(Push, void(const std::pair<int, int>& item));\n",
            "    MOCK_METHOD0(GetMap, std::map<int, double>());\n",
            "    MOCK_METHOD0(GetCallback, void (*())(int));\n",
            "    MOCK_METHOD0(GetArrayRef, int (&())[4]);\n",
            "    MOCK_METHOD1_WITH_CALLTYPE(STDMETHODCALLTYPE, Send, int(int value));\n",
            "    MOCK_CONST_METHOD0_WITH_CALLTYPE(STDMETHODCALLTYPE, Poll, int());\n",
            // Real mock headers wrap; the newlines a wrapped invocation
            // leaves around its arguments are whitespace, not structure.
            "    MOCK_METHOD(RET_ALIAS, GetAliased, (), (override));\n",
            "    MOCK_METHOD(void, TakeAliased, PARAMS_ALIAS, (override));\n",
            "    MOCK_METHOD(int, SpecAliased, (), SPEC_ALIAS);\n",
            "    MOCK_METHOD1(SigAliased, SIG_ALIAS);\n",
            "    MOCK_METHOD(int, RefQualified, (),\n",
            "        (const, ref(&&), noexcept(false), override));\n",
            "    MOCK_METHOD(void, OnLinked,\n",
            "        (const std::shared_ptr<Filter>& filter, StreamType out),\n",
            "        (override));\n",
            "    MOCK_METHOD3(RequestBuffer,\n",
            "        Status(std::shared_ptr<Buffer>& out, int32_t n, bool sync));\n",
            // A pointer to member is a parenthesized declarator however its
            // class is spelled, template arguments included.
            "    MOCK_METHOD((void (C<T>::*)(int)), GetTemplateMemberPtr, (), (override));\n",
            "    MOCK_METHOD((void (C<A<B>>::*)(int)), GetNestedMemberPtr, (), (override));\n",
            "    MOCK_METHOD((void (::C::*)(int)), GetRootedMemberPtr, (), (override));\n",
            // An expression that merely starts with a ptr-operator is not
            // one, so `decltype` keeps its spelling here too.
            "    MOCK_METHOD(decltype(*handle_), GetDeref, (), (const));\n",
            "    MOCK_METHOD(decltype(*(handle_)), GetParenDeref, (), (const));\n",
            // Only the top level of the spec list holds specifiers.
            "    MOCK_METHOD(int, NoexceptExpr, (),\n",
            "        (noexcept(is_nothrow<const T&>::value)));\n",
            "    MOCK_METHOD(int, CalltypeOnly, (), (Calltype(final)));\n",
            // A legacy signature naming no parameter list still recovers.
            "    MOCK_METHOD0(NoSignature, Bar);\n",
            "};\n",
        );
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        let flat = result.output.replace([' ', '\n'], "");
        assert!(
            !flat.contains("MOCK_"),
            "gMock fallback macros must expand away: {}",
            result.output
        );
        for expected in [
            "intLinkNext(intvalue)override;",
            "intReady();",
            "std::pair<int,int>Pair();",
            "voidPeek()constnoexceptoverride;",
            "voidCall();",
            // gMock's comma-protecting parentheses are the macro's own, not
            // C++ syntax: they must not reach the declaration.
            "boolCheckMap(std::map<int,double>,bool)override;",
            // A return type that is itself a parenthesized declarator cannot
            // wrap the member name, so it degrades to `void` ...
            "voidGetHandler()override;",
            // ... a pointer to member is one too, however it is qualified.
            "voidGetMemberPtr()override;",
            // ... but parentheses inside template arguments are not one, and
            // neither is a group of plain tokens: `decltype(...)`, or a macro
            // spelling a comma-containing type, keeps its spelling and is
            // expanded by the rescan like any other type.
            "std::function<void(int)>GetFn()override;",
            "std::conditional_t<(A>B),X,Y>Conditional()override;",
            "decltype(handle_)GetHandle()const;",
            "std::map<int,double>GetMapped()override;",
            // A `*` inside that macro's arguments is a parameter's, not a
            // declarator's: only a leading ptr-operator (or one behind the
            // `::` of a pointer to member) opens a declarator group.
            "std::map<int*,char>GetPtrMapped()override;",
            "intStart();",
            "intAttach(intvalue);",
            "intInspect(intleft,intright)const;",
            "voidPush(conststd::pair<int,int>&item);",
            // A signature the fixed-arity macro split at a template comma is
            // rejoined before it is parsed.
            "std::map<int,double>GetMap();",
            // The trailing group of `void (*())(int)` belongs to the returned
            // pointer, so neither type nor arity survives - the member does.
            "voidGetCallback();",
            // A signature that is nothing but a parenthesized declarator has
            // no parameter list to split at, and degrades the same way.
            "voidGetArrayRef();",
            // The `_WITH_CALLTYPE` families put the calling convention first.
            "intSend(intvalue);",
            "intPoll()const;",
            // An argument is read structurally, so it is macro-expanded
            // first: an alias for a protected type, a parameter list, a spec
            // list or a whole legacy signature is invisible until then.
            "std::pair<int,int>GetAliased()override;",
            "voidTakeAliased(int,int)override;",
            "intSpecAliased()constoverride;",
            "intSigAliased(int);",
            // The spec list keeps every qualifier C++ accepts, in C++'s
            // order, and `noexcept(expr)` is not plain `noexcept`.
            "intRefQualified()const&&noexcept(false)override;",
            "voidOnLinked(conststd::shared_ptr<Filter>&filter,StreamTypeout)override;",
            "StatusRequestBuffer(std::shared_ptr<Buffer>&out,int32_tn,boolsync);",
            // A pointer to member degrades however its class is spelled: the
            // nested-name-specifier may carry template arguments, and only
            // its closing `::` decides that the group is a declarator.
            "voidGetTemplateMemberPtr()override;",
            "voidGetNestedMemberPtr()override;",
            "voidGetRootedMemberPtr()override;",
            // `decltype(*x)` starts with a ptr-operator but continues into a
            // name, so it is the expression it looks like, not a declarator.
            "decltype(*handle_)GetDeref()const;",
            "decltype(*(handle_))GetParenDeref()const;",
            // A qualifier is one only at the top level of the spec list: the
            // `const` below belongs to the type `noexcept` asks about, and
            // `final` names a calling convention.
            "intNoexceptExpr()noexcept(is_nothrow<constT&>::value);",
            "intCalltypeOnly();",
            "BarNoSignature();",
        ] {
            assert!(
                flat.contains(expected),
                "expected `{expected}` in the gMock fallback expansion: {}",
                result.output
            );
        }

        let overridden = preprocess_string(
            "#define MOCK_METHOD(ret, name, args, spec) source_override(name)\nMOCK_METHOD(int, Kept, (), ())\n",
            Path::new("override.cpp"),
            &PreprocessOptions::new(),
        );
        assert!(
            overridden.output.contains("source_override(Kept)"),
            "a source gMock definition must override the fallback: {}",
            overridden.output
        );

        // The expansion promotes argument tokens into the declaration, so
        // they carry the macro's hide set: a member named after the macro is
        // declared, not consumed by the rescan as a fresh invocation. (Kept
        // out of the class above, whose assertion is that no `MOCK_` name
        // survives at all.)
        let self_named = preprocess_string(
            "class M { MOCK_METHOD(int, MOCK_METHOD, ()); };\n",
            Path::new("self_named.cpp"),
            &PreprocessOptions::new(),
        );
        assert_eq!(
            self_named.output.replace([' ', '\n'], ""),
            "classM{intMOCK_METHOD();;};",
            "a member named after the macro must survive: {}",
            self_named.output
        );

        // The whole family is hidden, not just the macro being expanded: a
        // member named after any of the others would otherwise be rescanned
        // as a fresh invocation and eaten the same way.
        let cross_named = preprocess_string(
            "class M { MOCK_METHOD(int, MOCK_METHOD0, ()); };\n",
            Path::new("cross_named.cpp"),
            &PreprocessOptions::new(),
        );
        assert_eq!(
            cross_named.output.replace([' ', '\n'], ""),
            "classM{intMOCK_METHOD0();;};",
            "a member named after another macro in the family must survive: {}",
            cross_named.output
        );

        // An invocation gMock itself rejects expands to nothing: it is still
        // consumed, but leaves no half-written declaration behind.
        for body in [
            // An unparenthesized comma-containing type, split into several
            // arguments by the macro call.
            "MOCK_METHOD(std::map<int, double>, Get, ());",
            // A comma still outside every `<...>` once gMock's protecting
            // parentheses are off: an argument list, not a type.
            "MOCK_METHOD((int, char), Get, ());",
            // Half a declaration is not one.
            "MOCK_METHOD0(Get, );",
            "MOCK_METHOD(int, , ());",
            // A legacy signature whose parameter list is not the last thing
            // in it: the group cannot be split off, and spelling the whole
            // signature in front of the member name is not a declaration.
            "MOCK_METHOD1(Get, int(int) const);",
            "MOCK_METHOD0(Get, int() noexcept);",
        ] {
            let malformed = preprocess_string(
                &format!("class M {{ {body} }};\n"),
                Path::new("malformed.cpp"),
                &PreprocessOptions::new(),
            );
            assert_eq!(
                malformed.output.replace([' ', '\n'], ""),
                "classM{;};",
                "`{body}` must expand to nothing: {}",
                malformed.output
            );
        }
    }

    #[test]
    fn gmock_argument_prescan_charges_the_token_budget() {
        // A gMock argument is expanded into a vector rather than into the
        // output, so only the prescan's own budget check bounds its width;
        // unbudgeted, a bomb reached through one allocated gigabytes and ran
        // for seconds before the emitting path saw a token. Both macro kinds
        // are covered, and each against the emitting path: a function-like
        // chain reaches `substitute_macro`, which materializes one whole
        // replacement per invocation, so the prescan has to charge an
        // invocation exactly as `process_tokens` does.
        let object_bomb = {
            // 4^11 tokens if fully expanded.
            let mut s = String::from("#define L0 1 1 1 1\n");
            for i in 1..=10 {
                s.push_str(&format!("#define L{i} L{p} L{p} L{p} L{p}\n", p = i - 1));
            }
            s
        };
        let function_bomb = {
            // 2^20 tokens if fully expanded, from a chain that names the
            // level below it twice and substitutes an argument each time.
            let mut s = String::from("#define F0(x) x x\n");
            for i in 1..=20 {
                s.push_str(&format!("#define F{i}(x) F{p}(x) F{p}(x)\n", p = i - 1));
            }
            s
        };
        for (kind, defs, call) in [
            ("object", object_bomb, "L10"),
            ("function-like", function_bomb, "F20(1)"),
        ] {
            for (path, body) in [
                ("emitting", format!("int x[] = {{ {call} }};\n")),
                (
                    "gMock argument",
                    format!("class M {{ MOCK_METHOD(int, F, (int y = {call})); }};\n"),
                ),
            ] {
                let opts = PreprocessOptions::new().with_max_expanded_tokens(2_000);
                let result =
                    preprocess_string(&format!("{defs}{body}"), Path::new("bomb.cpp"), &opts);
                assert!(
                    result
                        .diagnostics
                        .iter()
                        .any(|d| d.message.contains("token budget exceeded")),
                    "{kind} bomb on the {path} path must hit the budget: {:?}",
                    result.diagnostics
                );
            }
        }
    }

    #[test]
    fn ifndef_guard_defines_real_macro_over_builtin() {
        let src = "#ifndef container_of\n\
                   #define container_of(p, t, m) CUSTOM_CONTAINER(p)\n\
                   #endif\n\
                   int x = container_of(q, struct D, f);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("CUSTOM_CONTAINER"),
            "an #ifndef-guarded real definition must beat the builtin fallback: {}",
            result.output
        );
    }

    #[test]
    fn builtin_fallback_invisible_to_conditionals() {
        let src = "#ifdef __user\nint user_visible;\n#endif\n\
                   #if defined(__init)\nint init_visible;\n#endif\n\
                   int done;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !result.output.contains("user_visible") && !result.output.contains("init_visible"),
            "builtin fallbacks must not satisfy #ifdef/defined(): {}",
            result.output
        );
        assert!(result.output.contains("done"), "{}", result.output);
    }

    #[test]
    fn cli_define_overrides_builtin_with_shared_table() {
        let shared = Arc::new(RwLock::new(MacroTable::new()));
        let opts = PreprocessOptions::new()
            .with_shared_macros(shared)
            .with_define("__init", "KEEP_ME");
        let result = preprocess_string("int __init x;\n", Path::new("t.c"), &opts);
        assert!(
            result.output.contains("KEEP_ME"),
            "a -D define must override the builtin even in the shared-table path: {}",
            result.output
        );
    }

    #[test]
    fn cached_include_delta_carries_guarded_redefinition() {
        let dir = unique_tmp_dir("fallback_delta");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("c.h"),
            "#ifndef container_of\n#define container_of(p, t, m) REAL_CONTAINER(p)\n#endif\n",
        )
        .unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_include_expansion_cache(cache);
        let src = "#include \"c.h\"\nint a = container_of(x, struct D, f);\n";
        let r1 = preprocess_string(src, &dir.join("a.c"), &opts);
        assert!(
            r1.output.contains("REAL_CONTAINER"),
            "first TU must use the header's definition: {}",
            r1.output
        );
        // Second TU replays the cached include; the header's redefinition
        // must survive the delta capture and beat this TU's fallback.
        let r2 = preprocess_string(src, &dir.join("b.c"), &opts);
        assert!(
            r2.output.contains("REAL_CONTAINER"),
            "cache replay must carry the header's redefinition over the fallback: {}",
            r2.output
        );
    }

    #[test]
    fn fallback_stays_identifier_in_if_expression() {
        let src = "#if 1 || __init\nint kept;\n#endif\n\
                   #if __init\nint dropped;\n#endif\n\
                   int done;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("kept"),
            "a fallback in a #if expression must evaluate as an undefined \
             identifier (0), not expand to nothing and mangle the expression: {}",
            result.output
        );
        assert!(!result.output.contains("dropped"), "{}", result.output);
        assert!(result.output.contains("done"), "{}", result.output);
    }

    #[test]
    fn cached_header_replays_undef_of_fallback() {
        let dir = unique_tmp_dir("fallback_undef");
        fs::create_dir_all(&dir).unwrap();
        // The declaration makes the header content-bearing so a cache entry
        // is actually stored and the second TU takes the replay path.
        fs::write(dir.join("u.h"), "int u_decl;\n#undef __init\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_include_expansion_cache(cache);
        let src = "#include \"u.h\"\nint __init marker;\n";
        let r1 = preprocess_string(src, &dir.join("a.c"), &opts);
        assert!(
            r1.output.contains("__init"),
            "after the header's #undef the name must stay an identifier: {}",
            r1.output
        );
        // Cache hit must replay the #undef, not leave the fallback installed.
        let r2 = preprocess_string(src, &dir.join("b.c"), &opts);
        assert!(
            r2.output.contains("__init"),
            "cache replay must apply the header's #undef of the fallback: {}",
            r2.output
        );
    }

    #[test]
    fn cached_header_replays_noop_undef() {
        let dir = unique_tmp_dir("noop_undef");
        fs::create_dir_all(&dir).unwrap();
        // X is undefined when the entry is created, so a state diff records
        // nothing — only a log of executed directives catches this #undef.
        fs::write(dir.join("u.h"), "int u_decl;\n#undef X\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_include_expansion_cache(cache);
        let warm = preprocess_string("#include \"u.h\"\n", &dir.join("a.c"), &opts);
        assert!(warm.output.contains("u_decl"), "{}", warm.output);
        let src = "#define X 7\n#include \"u.h\"\nint arr = X;\n";
        let hit = preprocess_string(src, &dir.join("b.c"), &opts);
        assert!(
            hit.output.contains('X') && !hit.output.contains('7'),
            "cache replay must apply the header's #undef even though X was \
             absent when the entry was created: {}",
            hit.output
        );
    }

    #[test]
    fn cached_header_replays_undef_then_redefine_of_existing_macro() {
        let dir = unique_tmp_dir("undef_redef");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("r.h"), "int r_decl;\n#undef X\n#define X 9\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_include_expansion_cache(cache);
        let src = "#define X 1\n#include \"r.h\"\nint a = X;\n";
        let miss = preprocess_string(src, &dir.join("a.c"), &opts);
        assert!(
            miss.output.contains('9') && !miss.output.contains('1'),
            "{}",
            miss.output
        );
        // X existed at header entry AND exit, so a state diff records
        // neither the undef nor the redefinition.
        let hit = preprocess_string(src, &dir.join("b.c"), &opts);
        assert!(
            hit.output.contains('9') && !hit.output.contains('1'),
            "cache replay must reproduce undef-then-redefine of a macro that \
             existed when the entry was created: {}",
            hit.output
        );
    }

    #[test]
    fn cached_header_define_overwrites_like_live_execution() {
        let dir = unique_tmp_dir("replay_overwrite");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("r.h"), "int r_decl;\n#define X 9\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_include_expansion_cache(cache);
        let src = "#define X 1\n#include \"r.h\"\nint a = X;\n";
        let miss = preprocess_string(src, &dir.join("a.c"), &opts);
        assert!(
            miss.output.contains('9') && !miss.output.contains('1'),
            "{}",
            miss.output
        );
        let hit = preprocess_string(src, &dir.join("b.c"), &opts);
        assert!(
            hit.output.contains('9') && !hit.output.contains('1'),
            "a replayed #define must overwrite like live execution: {}",
            hit.output
        );
    }

    #[test]
    fn cached_replay_accumulates_to_shared_table() {
        let dir = unique_tmp_dir("replay_accum");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("m.h"), "int m_decl;\n#define FROM_HDR 5\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let src = "#include \"m.h\"\n";
        let shared1 = Arc::new(RwLock::new(MacroTable::new()));
        let opts1 = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_shared_macros(shared1)
            .with_accumulate_macros(true);
        preprocess_string(src, &dir.join("a.c"), &opts1);
        // The second run hits the cache; the replayed #define must reach the
        // shared table exactly as a live #define would.
        let shared2 = Arc::new(RwLock::new(MacroTable::new()));
        let opts2 = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_include_expansion_cache(cache)
            .with_shared_macros(Arc::clone(&shared2))
            .with_accumulate_macros(true);
        preprocess_string(src, &dir.join("b.c"), &opts2);
        assert!(
            shared2.read().unwrap().contains_key("FROM_HDR"),
            "cache replay must accumulate macros into the shared table"
        );
    }

    /// Spacing-insensitive view of preprocessed output, so assertions
    /// don't depend on the emitter's whitespace choices.
    fn flat(output: &str) -> String {
        output.replace(['\n', ' '], "")
    }

    #[test]
    fn unnamed_variadic_empty_args_elide_comma() {
        let src = "#define LOG(fmt, ...) printf(fmt, ##__VA_ARGS__)\n\
                   void f(void) { LOG(\"plain\"); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !flat(&result.output).contains(",)"),
            "GNU `, ##__VA_ARGS__` with no varargs must elide the comma: {}",
            result.output
        );
        assert!(result.output.contains("printf"), "{}", result.output);
    }

    #[test]
    fn unnamed_variadic_forwards_args_once() {
        let src = "#define LOG(fmt, ...) printf(fmt, ##__VA_ARGS__)\n\
                   void f(void) { LOG(\"num %d\", 1); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert_eq!(
            result.output.matches("num %d").count(),
            1,
            "__VA_ARGS__ must not re-substitute the named parameters: {}",
            result.output
        );
        assert_eq!(
            result.output.matches('1').count(),
            1,
            "varargs must be substituted exactly once: {}",
            result.output
        );
    }

    #[test]
    fn spaced_dots_are_not_an_ellipsis_in_a_parameter_list() {
        // `#define F(x, . . .)` is `invalid token in macro parameter list`
        // in gcc and clang. Before #28 the lexer had no `...` token, so the
        // parameter parser had to accept three `.` tokens and took this
        // spelling with them; now a real ellipsis is always one token.
        let src = "#define F(x, . . .) x\nint value = F(1, 2);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("in macro parameters")),
            "expected a malformed-parameter-list diagnostic, got {:?}",
            result.diagnostics
        );
        // The definition is dropped, so the invocation stays as written.
        assert!(result.output.contains("F(1"), "{}", result.output);
    }

    /// Phase 2 deletes `\`-newline before preprocessing tokens are
    /// recognized, so an ellipsis written across one is still `...` and
    /// this is a valid variadic macro; gcc and clang expand the call to
    /// `1` (#38). Pinned as a known false rejection until the lexer became
    /// splice-aware.
    #[test]
    fn splice_split_ellipsis_is_a_variadic_macro() {
        let src = "#define F(x, .\\\n..) x\nint value = F(1, 2);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let flat = result.output.replace([' ', '\n'], "");
        assert!(flat.contains("intvalue=1;"), "{}", result.output);
    }

    /// The same splice in ordinary code: the punctuator is re-spelled
    /// whole, and a spliced identifier is one identifier (`int cd;`, not
    /// `int c\ d;`) — #38's declaration and identifier shapes.
    #[test]
    fn splice_in_ordinary_code_is_deleted() {
        let src = "int f(int x, .\\\n..);\nint c\\\nd;\nint e = a-\\\n>b;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let out = &result.output;
        assert!(out.contains("..."), "{out}");
        assert!(
            !out.contains('\\'),
            "splice must not reach the output: {out}"
        );
        assert!(out.contains("int cd"), "{out}");
        assert!(out.contains("a->b") || out.contains("a-> b"), "{out}");
        assert_eq!(
            relex(out),
            relex("int f(int x, ...); int cd; int e = a->b;"),
            "{out}"
        );
    }

    /// Token kinds the C++ lexer reads back out of preprocessed output.
    fn relex_cpp(output: &str) -> Vec<TokenKind> {
        crate::lexer::Lexer::new(output, crate::Language::Cpp)
            .tokenize()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Newline | TokenKind::Eof))
            .collect()
    }

    /// Token kinds the C lexer reads back out of preprocessed output.
    fn relex(output: &str) -> Vec<TokenKind> {
        crate::lexer::Lexer::new(output, crate::Language::C)
            .tokenize()
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !matches!(k, TokenKind::Newline | TokenKind::Eof))
            .collect()
    }

    #[test]
    fn gnu_case_range_survives_the_round_trip() {
        // A pp-number absorbs `.` and alphanumerics, so `case 1 ... 10:`
        // emitted as `case 1...10:` re-lexes as the single number
        // `1...10`. The ellipsis keeps its leading space after a number.
        for (src, want) in [
            (
                "void f(int x) { switch (x) { case 1 ... 10: break; } }\n",
                "1 ...10",
            ),
            // A pp-number can end in a letter, so a digit test is not enough.
            (
                "void f(int x) { switch (x) { case 0x1F ... 0x2F: break; } }\n",
                "0x1F ...0x2F",
            ),
            ("int a[] = { [0 ... 9] = 1 };\n", "0 ...9"),
        ] {
            let out = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new()).output;
            assert!(out.contains(want), "want {want:?} in {out:?}");
            let kinds = relex(&out);
            assert!(
                kinds
                    .iter()
                    .any(|k| matches!(k, TokenKind::Punct(s) if *s == "...")),
                "ellipsis must survive re-lexing: {out:?} -> {kinds:?}"
            );
            assert!(
                !kinds
                    .iter()
                    .any(|k| matches!(k, TokenKind::Number(n) if n.contains('.'))),
                "no number may absorb the ellipsis: {out:?} -> {kinds:?}"
            );
        }
    }

    #[test]
    fn ellipsis_after_a_non_number_stays_glued() {
        // `Args...` must not gain a space: only a pp-number absorbs the
        // dots, and identifiers are by far the common case.
        let out = preprocess_string(
            "template <class... Args> void f(Args... a);\n",
            Path::new("t.cpp"),
            &PreprocessOptions::new(),
        )
        .output;
        assert!(out.contains("Args..."), "{out}");
        assert!(out.contains("class..."), "{out}");
    }

    #[test]
    fn pointer_to_member_call_survives_the_round_trip() {
        // #37: `-> *` made tree-sitter recover `*func` as the callee,
        // interning a function named after the operand — in the camera
        // corpus, after a template parameter.
        let src = "int g(C *c, int (C::*fp)()) { return (c->*fp)(); }\n";
        let out = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new()).output;
        assert!(out.contains("->*"), "{out}");
        assert!(!out.contains("-> *"), "{out}");
        let kinds = relex_cpp(&out);
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, TokenKind::Punct(s) if *s == "->*")),
            "{out:?} -> {kinds:?}"
        );
    }

    #[test]
    fn plain_arrow_is_unaffected_by_the_pointer_to_member_token() {
        // A real `*` after a template `>` must keep its space (`>&` would
        // not parse as a reference parameter).
        let out = preprocess_string(
            "int k(std::shared_ptr<T> &p, T *q) { return p->x + *q; }\n",
            Path::new("t.cpp"),
            &PreprocessOptions::new(),
        )
        .output;
        assert!(out.contains("p-> x"), "{out}");
        assert!(!out.contains("->*"), "{out}");
    }

    #[test]
    fn variadic_declaration_keeps_its_ellipsis() {
        // Issue #28: the ellipsis came back out as `. . .`, which
        // tree-sitter cannot parse, so every variadic declaration in the
        // corpus produced an ERROR node.
        let src = "int my_log(const char *fmt, ...) { return 0; }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("..."),
            "ellipsis must survive re-spelling: {}",
            result.output
        );
        assert!(!result.output.contains(". ."), "{}", result.output);
    }

    #[test]
    fn zero_named_param_variadic_expands() {
        let src = "#define TRACE(...) log_event(__VA_ARGS__)\n\
                   void f(void) { TRACE(); TRACE(1, 2); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains("log_event()"),
            "empty __VA_ARGS__ must expand to nothing: {}",
            result.output
        );
        assert!(
            flat(&result.output).contains("log_event(1,2)"),
            "{}",
            result.output
        );
    }

    #[test]
    fn variadic_string_vararg_survives() {
        let src = "#define LOG(fmt, ...) printf(fmt, ##__VA_ARGS__)\n\
                   void f(void) { LOG(\"%s\", \"reason\"); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("\"reason\""),
            "a string-literal vararg must not be destroyed by comma pasting: {}",
            result.output
        );
        assert!(!flat(&result.output).contains(",)"), "{}", result.output);
    }

    #[test]
    fn variadic_comma_stays_punct_for_nested_split() {
        let src = "#define INNER(a, b) use(a); use(b);\n\
                   #define WRAP(fmt, ...) INNER(fmt, ##__VA_ARGS__)\n\
                   void f(void) { WRAP(\"f\", x); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains("use(x)"),
            "the separator comma must stay a real token so nested macros split \
             their arguments: {}",
            result.output
        );
        assert!(!flat(&result.output).contains("use()"), "{}", result.output);
    }

    #[test]
    fn variadic_first_vararg_still_macro_expands() {
        let src = "#define COUNT 42\n\
                   #define LOG(fmt, ...) printf(fmt, ##__VA_ARGS__)\n\
                   void f(void) { LOG(\"%d\", COUNT); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("42"),
            "the first vararg must stay expandable on rescan: {}",
            result.output
        );
        assert!(!result.output.contains("COUNT"), "{}", result.output);
    }

    #[test]
    fn named_variadic_va_args_spelling_aliases_to_tail() {
        let src = "#define LOGE(fmt, args...) HiLogPrint(fmt, ##__VA_ARGS__)\n\
                   void f(void) { LOGE(\"oom\", n); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !result.output.contains("__VA_ARGS__"),
            "__VA_ARGS__ in a named-variadic body must alias the tail param: {}",
            result.output
        );
        assert!(result.output.contains('n'), "{}", result.output);
    }

    #[test]
    fn param_list_line_continuation() {
        let src = "#define LOG(fmt, \\\n    ...) printf(fmt, ##__VA_ARGS__)\n\
                   void f(void) { LOG(\"x\"); }\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("printf") && result.output.contains("after"),
            "a continued parameter list must not abort the file: {}",
            result.output
        );
    }

    #[test]
    fn named_variadic_continuation_before_ellipsis() {
        let src = "#define F(x, args \\\n...) g(x, ##args)\n\
                   void h(void) { F(1); F(2, 3); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains("g(1)"),
            "a continuation between a named variadic and `...` must parse: {}",
            result.output
        );
        assert!(flat(&result.output).contains("g(2,3)"), "{}", result.output);
    }

    #[test]
    fn continuation_before_close_does_not_leak_paren() {
        let src = "#define VLOG(fmt, ... \\\n) printf(fmt)\n\
                   void f(void) { VLOG(\"x\"); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("printf(\"x\")") && !result.output.contains(") printf"),
            "tokens after `...` must not leak into the replacement list: {}",
            result.output
        );
    }

    #[test]
    fn explicitly_empty_vararg_keeps_comma() {
        // gcc/clang: `, ##__VA_ARGS__` deletes the comma only when the
        // varargs are OMITTED; an explicitly supplied empty argument keeps
        // it (F(1) -> g(1) but F(1,) -> g(1,); verified against both).
        let src = "#define F(x, ...) g(x, ##__VA_ARGS__)\n\
                   void h(void) { F(1); }\nvoid k(void) { F(2,); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains("g(1)"),
            "omitted varargs must elide the comma: {}",
            result.output
        );
        assert!(
            flat(&result.output).contains("2,)"),
            "an explicitly empty vararg must keep the comma like gcc: {}",
            result.output
        );
    }

    #[test]
    fn whitespace_only_explicit_vararg_keeps_comma() {
        let src = "#define LOG(fmt, ...) printf(fmt, ##__VA_ARGS__)\n\
                   void f(void) { LOG(\"x\",\n); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains(",)"),
            "a whitespace-only explicit vararg keeps the comma like gcc: {}",
            result.output
        );
    }

    #[test]
    fn lone_blank_argument_counts_as_omitted() {
        let src = "#define G(...) f(0, ##__VA_ARGS__)\n\
                   void h(void) { G(); G( ); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !flat(&result.output).contains(",)"),
            "G() supplies zero arguments, so the comma is elided: {}",
            result.output
        );
    }

    #[test]
    fn non_variadic_hash_hash_empty_arg_keeps_comma() {
        let src = "#define M(a, b) f(a, ## b)\nvoid g(void) { M(x, ); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains("f(x,)"),
            "GNU comma deletion applies only to variadic tails; a non-variadic \
             empty ##-argument keeps the comma like gcc: {}",
            result.output
        );
    }

    #[test]
    fn truncated_param_list_in_expansion_does_not_panic() {
        // The expansion of DECL re-scans a `#define` whose parameter list is
        // truncated mid-`...`; this must degrade to a diagnostic, not panic.
        let src = "#define DECL(x) #define x(...\nDECL(FOO)\nint after(void);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains("intafter(void);"),
            "{}",
            result.output
        );
    }

    /// An unterminated parameter list ends at the newline like any other
    /// directive: warn, drop the definition, keep the following code (gcc
    /// errors and continues; it never lets the list run on to a `)` on a
    /// later line).
    #[test]
    fn unterminated_param_list_keeps_following_code() {
        let src =
            "#define PARTIAL(x, ...\nint before(void);\nint f(int a) { return (a); }\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let out = flat(&result.output);
        assert!(out.contains("intbefore(void);"), "{}", result.output);
        assert!(out.contains("intf(inta){return(a);}"), "{}", result.output);
        assert!(out.contains("intafter;"), "{}", result.output);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unterminated macro parameter list")),
            "{:?}",
            result.diagnostics
        );
        assert!(!result.output.contains("PARTIAL"), "{}", result.output);
    }

    #[test]
    fn unterminated_param_list_without_ellipsis_keeps_following_code() {
        let src = "#define PARTIAL(x,\nint f(int a) { return (a); }\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            flat(&result.output).contains("intf(inta){return(a);}"),
            "{}",
            result.output
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unterminated macro parameter list")),
            "{:?}",
            result.diagnostics
        );
    }

    /// A malformed (but terminated) list is dropped the same way instead of
    /// stopping preprocessing of the whole file.
    #[test]
    fn malformed_param_list_keeps_following_code() {
        let src = "#define BAD(x y) x\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("after"), "{}", result.output);
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("preprocess stopped")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn variadic_logging_macro_chain_expands_cleanly() {
        let src = "#define HILOG_DEBUG(label, fmt, args...) printf(fmt, ##args)\n\
                   #define DECORATOR_HILOG(op, fmt, args...) op(\"L\", fmt, ##args)\n\
                   #define MEDIA_DEBUG_LOG(fmt, ...) DECORATOR_HILOG(HILOG_DEBUG, fmt, ##__VA_ARGS__)\n\
                   void f(void)\n{\n\
                   MEDIA_DEBUG_LOG(\"plain\");\n\
                   MEDIA_DEBUG_LOG(\"num %d\", 1);\n}\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            !flat(&result.output).contains(",)"),
            "empty varargs must elide the comma through the nested chain: {}",
            result.output
        );
        assert_eq!(
            result.output.matches("num %d").count(),
            1,
            "arguments must be forwarded exactly once through the chain: {}",
            result.output
        );
    }

    #[test]
    fn builtin_fallback_yields_to_source_definition() {
        let src = "#define __init KEEP_ME\nint __init x;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("KEEP_ME"),
            "a source #define must override the builtin fallback: {}",
            result.output
        );
    }

    #[test]
    fn self_referential_function_macro_terminates() {
        let src = "#define F(x) F(x)\nint y = F(1);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("F") && result.output.contains("1"),
            "{}",
            result.output
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expansion depth exceeded")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn self_ref_macro_fixture() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/self_ref_macro.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(
            result.output.contains("PRIVATE_MESSAGE_TYPE")
                && result.output.contains("ENGINE_UPLOAD_READY_MSG"),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("MIN"),
            "nested MIN leaked: {}",
            result.output
        );
    }

    #[test]
    fn include_macro_operand_expands() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/include_macro.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(
            result.output.contains("NESTED_VAL") || result.output.contains("42"),
            "macro #include must splice nested.h: {}",
            result.output
        );
        assert!(
            result
                .included_headers
                .iter()
                .any(|p| p.ends_with("include_macro_nested.h")),
            "included_headers must record the expanded include: {:?}",
            result.included_headers
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expected string or <...>")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn function_macro_inside_object_expansion_expands() {
        // C11 6.10.3.4: after an object-like macro is replaced, the result is
        // rescanned; a function-like macro invoked there must be expanded
        // too, not emitted verbatim.
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/nested_fn_macro.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(
            !result.output.contains("WRAP") && !result.output.contains("SHARED"),
            "macro invocations leaked verbatim: {}",
            result.output
        );
        assert!(result.output.contains("status_Node"), "{}", result.output);
        assert!(result.output.contains("done"), "{}", result.output);
    }

    #[test]
    fn object_macro_with_parenthesized_value() {
        let src = "#define START (-100)\nint x = START;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("(-100)")
                || result.output.contains("-100")
                || result.output.contains("- 100"),
            "{}",
            result.output
        );
        assert!(!result.output.contains("START"));
    }

    #[test]
    fn variadic_macro_empty_args_strips_hash_hash() {
        let src = "#define WRAP(fmt, arg...) BASE(fmt, ##arg)\nWRAP(\"x\");\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("BASE") && result.output.contains("\"x\""),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains(", ,"),
            "should not leave dangling comma: {}",
            result.output
        );
    }

    #[test]
    fn enum_body_define_does_not_break_preproc() {
        let src = "typedef enum {\n    A = 1,\n#define OFF (-100)\n    B = OFF,\n} E;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result
            .diagnostics
            .iter()
            .any(|d| { d.message.contains("expected identifier in directive") }));
    }

    /// A temporary source tree, removed when dropped (assertion failures
    /// included). Derefs to its canonical path so fixture paths match cache
    /// keys, which are stored canonicalized; on macOS the temp root is
    /// behind the /var symlink.
    struct TmpTree {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl TmpTree {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl std::ops::Deref for TmpTree {
        type Target = Path;
        fn deref(&self) -> &Path {
            self.path()
        }
    }

    impl AsRef<Path> for TmpTree {
        fn as_ref(&self) -> &Path {
            self.path()
        }
    }

    fn unique_tmp_dir(tag: &str) -> TmpTree {
        let dir = tempfile::Builder::new()
            .prefix(&format!("trace_preproc_{tag}_"))
            .tempdir()
            .unwrap();
        let path = dir.path().canonicalize().unwrap();
        TmpTree { _dir: dir, path }
    }

    /// Regression: a nested include whose expansion is fully skipped by an
    /// already-defined guard must (a) warn and (b) NOT be claimed as
    /// content-bearing in the parent's cached `IncludeExpansion::files`.
    /// Claiming it made replaying translation units treat the header as
    /// already included while its content was silently absent.
    #[test]
    fn guard_skipped_include_not_claimed_and_warned() {
        let dir = unique_tmp_dir("guard_starve");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();

        // Same content, same guard, two paths: only one can be cached.
        let list_src = "#ifndef LIST_H\n#define LIST_H\nstruct Node { int v; };\n#endif\n";
        fs::write(a.join("list.h"), list_src).unwrap();
        fs::write(b.join("list.h"), list_src).unwrap();
        fs::write(
            dir.join("outer.h"),
            "#include \"list.h\"\nint outer_use(void);\n",
        )
        .unwrap();

        let shared = Arc::new(RwLock::new(MacroTable::new()));
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));

        // Warm-style pass over the first twin: defines LIST_H, caches text.
        let warm_opts = PreprocessOptions::new()
            .with_shared_macros(Arc::clone(&shared))
            .with_accumulate_macros(true)
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(a.clone());
        let r1 = preprocess_file(&a.join("list.h"), &warm_opts).unwrap();
        assert!(r1.output.contains("Node"), "{}", r1.output);

        // Second pass reaching the OTHER twin through outer.h: the guard is
        // already defined in the shared table, so b/list.h expands to
        // nothing inline.
        let index_opts = PreprocessOptions::new()
            .with_shared_macros(Arc::clone(&shared))
            .with_accumulate_macros(true)
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(b.clone());
        let r2 = preprocess_file(&dir.join("outer.h"), &index_opts).unwrap();
        // Documented consequence: content behind the leaked guard is absent.
        assert!(
            !r2.output.contains("Node"),
            "starved expansion expected here"
        );
        // (a) the starvation is visible as a diagnostic
        assert!(
            r2.diagnostics
                .iter()
                .any(|d| d.message.contains("expanded to nothing")),
            "{:?}",
            r2.diagnostics
        );
        // (b) outer.h's cached entry must not claim b/list.h as content-bearing
        let outer_entry = sole_variant(&cache, dir.join("outer.h"), Language::C);
        let claimed_b = outer_entry
            .as_ref()
            .map(|e| e.files.iter().any(|f| *f == b.join("list.h")))
            .unwrap_or(false);
        assert!(
            !claimed_b,
            "claimed starved file: {:?}",
            outer_entry.map(|e| e.files.clone())
        );
    }

    /// Frozen (TU-phase) preprocessing must stay silent about guard-skipped
    /// includes: workers expand misses inline per TU and warnings there would
    /// repeat per translation unit.
    #[test]
    fn frozen_cache_does_not_warn_on_guard_skip() {
        let dir = unique_tmp_dir("frozen_quiet");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("g.h"),
            "#ifndef G_H\n#define G_H\nint g(void);\n#endif\n",
        )
        .unwrap();

        let shared = Arc::new(RwLock::new(MacroTable::new()));
        {
            let mut t = shared.write().unwrap();
            use crate::macros::MacroDef;
            use crate::{Lexer, TokenKind};
            let toks: Vec<_> = Lexer::new("1", Language::C)
                .tokenize()
                .into_iter()
                .filter(|t| !matches!(t.kind, TokenKind::Eof))
                .collect();
            t.insert("G_H".to_string(), MacroDef::Object { replacement: toks });
        }
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_shared_macros(Arc::clone(&shared))
            .with_include_expansion_cache(cache)
            .with_frozen_expansion_cache(true)
            .with_include(dir.to_path_buf());
        let src = "#include \"g.h\"\nint main(void){return 0;}\n";
        let r = preprocess_string(src, &dir.join("m.c"), &opts);
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.message.contains("expanded to nothing")),
            "{:?}",
            r.diagnostics
        );
    }

    /// A cache hit must reproduce the diagnostics emitted while creating
    /// the cached expansion, just as it reproduces text and macro effects.
    #[test]
    fn cached_include_replays_diagnostics() {
        let dir = unique_tmp_dir("cached_diagnostics");
        fs::write(
            dir.path().join("nested.h"),
            "#frobnicate\nint from_nested_header;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("reported.h"),
            "#include \"nested.h\"\nint from_reported_header;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("first.c"),
            "#include \"reported.h\"\nint first;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("second.c"),
            "#include \"reported.h\"\nint second;\n",
        )
        .unwrap();

        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(dir.path().to_path_buf());

        let first = preprocess_file(&dir.path().join("first.c"), &opts).unwrap();
        assert_eq!(
            first
                .diagnostics
                .iter()
                .filter(|d| d.message.contains("unknown directive #frobnicate"))
                .count(),
            1,
            "{:?}",
            first.diagnostics
        );
        assert!(
            cache
                .read()
                .unwrap()
                .contains_key(&(dir.path().join("reported.h"), Language::C)),
            "first run did not populate the include cache"
        );

        let second = preprocess_file(&dir.path().join("second.c"), &opts).unwrap();
        let replayed: Vec<_> = second
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("unknown directive #frobnicate"))
            .collect();
        assert_eq!(replayed.len(), 1, "{:?}", second.diagnostics);
        assert_eq!(
            replayed[0].file.as_deref(),
            Some(dir.path().join("nested.h").as_path())
        );
        assert_eq!(replayed[0].line, 1);
    }

    /// Two cached parents can both carry a diagnostic from the same nested
    /// header. Replaying both in one run must report the source condition
    /// once, rather than once per cache path through the include graph.
    #[test]
    fn cached_parents_do_not_duplicate_nested_diagnostics() {
        let dir = unique_tmp_dir("cached_parent_diagnostics");
        fs::write(dir.join("reported.h"), "#frobnicate\nint reported;\n").unwrap();
        fs::write(dir.join("left.h"), "#include \"reported.h\"\nint left;\n").unwrap();
        fs::write(dir.join("right.h"), "#include \"reported.h\"\nint right;\n").unwrap();
        fs::write(
            dir.join("main.c"),
            "#include \"left.h\"\n#include \"right.h\"\nint main(void) { return 0; }\n",
        )
        .unwrap();

        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include_expansion_cache(cache)
            .with_include(dir.to_path_buf());

        preprocess_file(&dir.join("left.h"), &opts).unwrap();
        preprocess_file(&dir.join("right.h"), &opts).unwrap();
        let result = preprocess_file(&dir.join("main.c"), &opts).unwrap();
        let reported: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("unknown directive #frobnicate"))
            .collect();

        assert_eq!(reported.len(), 1, "{:?}", result.diagnostics);
        assert_eq!(
            reported[0].file.as_deref(),
            Some(dir.join("reported.h").as_path())
        );
        assert_eq!(reported[0].line, 1);
    }

    /// A parent cache entry must retain a nested report even if the nested
    /// header was already included earlier in the run that created it.
    #[test]
    fn cached_parent_replays_diagnostic_from_guard_skipped_child() {
        let dir = unique_tmp_dir("cached_parent_guard_skipped_diagnostic");
        fs::write(dir.join("reported.h"), "#frobnicate\nint reported;\n").unwrap();
        fs::write(
            dir.join("wrapper.h"),
            "#include \"reported.h\"\nint wrapped;\n",
        )
        .unwrap();
        fs::write(
            dir.join("first.c"),
            "#include \"reported.h\"\n#include \"wrapper.h\"\nint first;\n",
        )
        .unwrap();
        fs::write(
            dir.join("second.c"),
            "#include \"wrapper.h\"\nint second;\n",
        )
        .unwrap();

        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include_expansion_cache(cache)
            .with_include(dir.to_path_buf());

        preprocess_file(&dir.join("first.c"), &opts).unwrap();
        let second = preprocess_file(&dir.join("second.c"), &opts).unwrap();
        let reported: Vec<_> = second
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("unknown directive #frobnicate"))
            .collect();

        assert_eq!(reported.len(), 1, "{:?}", second.diagnostics);
        assert_eq!(
            reported[0].file.as_deref(),
            Some(dir.join("reported.h").as_path())
        );
        assert_eq!(reported[0].line, 1);
    }

    /// Diamond includes must not copy a header's cached body into live
    /// output on every skip — that exponentiates. Live text stays unique;
    /// the second parent's *cache entry* still embeds the nested header
    /// so a later replay of only that parent keeps the nested declaration.
    #[allow(clippy::manual_range_contains)]
    #[test]
    fn diamond_include_does_not_blow_up_and_cache_stays_self_contained() {
        let dir = unique_tmp_dir("diamond_inc");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("common.h"),
            "#ifndef COMMON_H\n#define COMMON_H\nstruct NeedThis { int x; };\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("left.h"),
            "#ifndef LEFT_H\n#define LEFT_H\n#include \"common.h\"\nvoid left(void);\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("right.h"),
            "#ifndef RIGHT_H\n#define RIGHT_H\n#include \"common.h\"\nvoid right(struct NeedThis *p);\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("top.h"),
            "#ifndef TOP_H\n#define TOP_H\n#include \"left.h\"\n#include \"right.h\"\n#endif\n",
        )
        .unwrap();

        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let warm = PreprocessOptions::new()
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(dir.to_path_buf());
        let top = preprocess_file(&dir.join("top.h"), &warm).unwrap();
        let need_count = top.output.matches("NeedThis").count();
        assert!(
            need_count >= 1 && need_count <= 2,
            "live output should mention NeedThis once (maybe twice), not explode: {need_count}\n{}",
            top.output
        );
        assert!(
            top.output.len() < 1024,
            "diamond live output too large: {}",
            top.output.len()
        );

        let right = sole_variant(&cache, dir.join("right.h"), Language::C).expect("right.h cached");
        assert!(
            right.text.contains("NeedThis"),
            "right.h cache must be self-contained, got {}",
            right.text
        );

        // Frozen consumer that only includes right.h still sees NeedThis.
        let frozen = PreprocessOptions::new()
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_frozen_expansion_cache(true)
            .with_include(dir.to_path_buf());
        let c = preprocess_file(&dir.join("right.h"), &frozen).unwrap();
        assert!(
            c.output.contains("NeedThis"),
            "frozen replay of right.h lost nested common.h: {}",
            c.output
        );
    }

    /// n headers each including all previous ones: live output is O(n), not 2^n.
    #[test]
    fn chained_includes_live_output_is_linear() {
        let dir = unique_tmp_dir("chain_inc");
        fs::create_dir_all(&dir).unwrap();
        const N: usize = 24;
        for i in 0..N {
            let mut src = format!("#ifndef H{i}\n#define H{i}\n");
            for j in 0..i {
                src.push_str(&format!("#include \"h{j}.h\"\n"));
            }
            src.push_str(&format!("int v{i};\n#endif\n"));
            fs::write(dir.join(format!("h{i}.h")), src).unwrap();
        }
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(dir.to_path_buf());
        let r = preprocess_file(&dir.join(format!("h{}.h", N - 1)), &opts).unwrap();
        for i in 0..N {
            assert!(
                r.output.contains(&format!("v{i}")),
                "missing v{i} in {}",
                r.output
            );
        }
        assert!(
            r.output.len() < 8 * 1024,
            "chained-include live output exploded: {}",
            r.output.len()
        );
    }

    #[test]
    fn include_depth_cap_skips_deeper_nests() {
        let dir = unique_tmp_dir("inc_depth");
        fs::create_dir_all(&dir).unwrap();
        const N: usize = 12;
        for i in 0..N {
            let src = if i + 1 < N {
                format!("int v{i};\n#include \"n{}.h\"\n", i + 1)
            } else {
                format!("int v{i};\n")
            };
            fs::write(dir.join(format!("n{i}.h")), src).unwrap();
        }
        let opts = PreprocessOptions::new()
            .with_include(dir.to_path_buf())
            .with_max_include_depth(6);
        let r = preprocess_file(&dir.join("n0.h"), &opts).unwrap();
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.message.contains("include depth exceeded")),
            "expected depth warning: {:?}",
            r.diagnostics
        );
        assert!(r.output.contains("v0"), "{}", r.output);
        assert!(
            !r.output.contains("v11"),
            "depth cap should not expand the whole chain: {}",
            r.output
        );
    }

    /// `W(x)` repeating its parameter 16 times, invoked with `width`
    /// argument tokens.
    fn multiplying_macro(width: usize) -> String {
        let arg = (0..width)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("#define W(x) x x x x x x x x x x x x x x x x\nint y[] = {{ W({arg}) }};\n")
    }

    #[test]
    fn wide_macro_argument_is_charged_before_it_is_materialized() {
        // Issue #30: the budget counted tokens *walked*. A function-like
        // invocation walks O(1) — `parse_macro_args` skips the argument
        // list wholesale — and `substitute_macro` then copies the argument
        // once per parameter occurrence, so peak allocation scaled with the
        // source argument width however small `max_expanded_tokens` was: an
        // 80k-token argument reached 397 MB under a 2,000-token budget.
        // The budget fired in every one of those runs and still did not
        // bound the allocation, so the diagnostic alone cannot witness this
        // fix. What changes is *when* it fires: the invocation is now
        // refused before the copy, so none of the argument is ever emitted.
        let opts = PreprocessOptions::new().with_max_expanded_tokens(2_000);
        let result = preprocess_string(&multiplying_macro(5_000), Path::new("bomb.c"), &opts);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("token budget exceeded")),
            "{:?}",
            result.diagnostics
        );
        assert!(
            !result.output.contains("a0"),
            "the replacement must be refused before it is built, so no \
             argument token reaches the output; got {} bytes starting {:?}",
            result.output.len(),
            &result.output[..result.output.len().min(80)]
        );
    }

    #[test]
    fn charging_the_replacement_still_admits_expansions_that_fit() {
        // The charge is an upper bound on what the substitution will
        // materialize, so it must not refuse an expansion the budget can
        // afford — 16 x 100 tokens against a 10,000-token budget.
        let opts = PreprocessOptions::new().with_max_expanded_tokens(10_000);
        let result = preprocess_string(&multiplying_macro(100), Path::new("ok.c"), &opts);
        assert!(
            result.diagnostics.is_empty(),
            "expansion within budget must not be refused: {:?}",
            result.diagnostics
        );
        assert!(result.output.contains("a99"), "{}", result.output);
    }

    #[test]
    fn wide_condition_argument_is_projected_before_it_is_materialized() {
        // The `#if` engine is separate from the emitting path and caps
        // `work` at 1<<16 tokens, tested at the top of its loop — so one
        // `splice` of a substitution could carry it far past the cap before
        // the next test saw it: 64 occurrences over a 32k-token argument
        // reached 329 MB peak. The projection now refuses first (13.8 MB).
        //
        // Unlike the emitting path there is NO functional tell: the
        // condition is treated as false either way, with the same warning
        // on the same line, so this test pins the contract (refused, branch
        // not taken) and not the allocation. The allocation is evidenced by
        // measurement, recorded in the commit.
        let arg = (0..2_000)
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let body = vec!["x"; 64].join(" ");
        let src = format!("#define W(x) {body}\n#if W({arg})\nint taken;\n#endif\nint after;\n");
        let result = preprocess_string(&src, Path::new("cond.c"), &PreprocessOptions::new());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("budget exceeded in #if condition")),
            "{:?}",
            result.diagnostics
        );
        assert!(!result.output.contains("taken"), "{}", result.output);
        assert!(result.output.contains("after"), "{}", result.output);
    }

    #[test]
    fn projection_still_admits_conditions_that_fit() {
        // The projection is an upper bound — it charges `#param` at the
        // argument's width and ignores that `##` only removes tokens — so
        // it must not start refusing conditions the cap can afford, which
        // would silently flip branches to false.
        let src = "#define ADD(a, b) a + b
                   #if ADD(1, 2) == 3
                   int taken;
                   #endif
";
        let result = preprocess_string(src, Path::new("ok.c"), &PreprocessOptions::new());
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        assert!(result.output.contains("taken"), "{}", result.output);
    }

    #[test]
    fn token_budget_stops_explosive_macro_expansion() {
        let src = "\
#define A B B B B B B B B
#define B C C C C C C C C
#define C D D D D D D D D
#define D E E E E E E E E
#define E 1
int x = A;
";
        let opts = PreprocessOptions::new().with_max_expanded_tokens(2_000);
        let result = preprocess_string(src, Path::new("t.c"), &opts);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("token budget exceeded")),
            "expected token-budget diagnostic: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn inline_false_keeps_parent_output_file_local() {
        let dir = unique_tmp_dir("no_inline");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("common.h"),
            "#ifndef COMMON_H\n#define COMMON_H\nstruct NeedThis { int x; };\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("top.h"),
            "#ifndef TOP_H\n#define TOP_H\n#include \"common.h\"\nint from_top;\n#endif\n",
        )
        .unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(dir.to_path_buf())
            .with_inline_include_bodies(false);
        let top = preprocess_file(&dir.join("top.h"), &opts).unwrap();
        assert!(
            top.output.contains("from_top"),
            "parent tokens must remain: {}",
            top.output
        );
        assert!(
            !top.output.contains("NeedThis"),
            "nested header body must not be copied into parent live output: {}",
            top.output
        );
        let common =
            sole_variant(&cache, dir.join("common.h"), Language::C).expect("common.h cached");
        assert!(
            common.text.contains("NeedThis"),
            "child cache still holds its own text: {}",
            common.text
        );
    }

    #[test]
    fn root_file_is_emitted_even_when_the_cache_already_holds_it() {
        // via.h includes late.h through a macro, so an indexer's raw include
        // scanner cannot order late.h before via.h; preprocessing via.h
        // first puts late.h into the expansion cache. A later run rooted at
        // late.h must still emit late.h's own text rather than replay that
        // entry (which, with `inline_include_bodies` off, emits nothing).
        let dir = unique_tmp_dir("root_cached");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("late.h"),
            "#ifndef LATE_H_
#define LATE_H_
int from_late;
#endif
",
        )
        .unwrap();
        fs::write(
            dir.join("via.h"),
            "#ifndef VIA_H
#define VIA_H
#define LATE_H \"late.h\"
#include LATE_H
#endif
",
        )
        .unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(dir.to_path_buf())
            .with_inline_include_bodies(false);
        let via = preprocess_file(&dir.join("via.h"), &opts).unwrap();
        assert!(
            !via.output.contains("from_late"),
            "nested body stays out of the parent's output: {}",
            via.output
        );
        assert!(
            cache
                .read()
                .unwrap()
                .contains_key(&(dir.join("late.h"), Language::C)),
            "the macro include put late.h into the cache"
        );
        let late = preprocess_file(&dir.join("late.h"), &opts).unwrap();
        assert!(
            late.output.contains("from_late"),
            "a run rooted at late.h must emit its text: {:?}",
            late.output
        );
    }

    #[test]
    fn if_defined_true_when_macro_defined() {
        let src = "#define FEATURE 1\n#if defined(FEATURE)\nint kept;\n#endif\n#if defined FEATURE\nint kept_noparen;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        assert!(result.output.contains("kept_noparen"), "{}", result.output);
    }

    #[test]
    fn if_defined_false_when_macro_undefined() {
        let src = "#if defined(MISSING)\nint dropped;\n#endif\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
        assert!(result.output.contains("after"), "{}", result.output);
    }

    #[test]
    fn if_not_defined_false_when_macro_defined() {
        let src = "#define FEATURE 1\n#if !defined(FEATURE)\nint dropped;\n#endif\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
        assert!(result.output.contains("after"), "{}", result.output);
    }

    #[test]
    fn if_defined_conjunction_requires_both() {
        let both = "#define A 1\n#define B 1\n#if defined(A) && defined(B)\nint kept;\n#endif\n";
        let result = preprocess_string(both, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        let one = "#define A 1\n#if defined(A) && defined(B)\nint dropped;\n#endif\n";
        let result = preprocess_string(one, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
    }

    #[test]
    fn if_not_defined_binds_tighter_than_and() {
        // (!defined A) && (defined B): A defined, B defined -> false && true = false.
        let src = "#define A 1\n#define B 1\n#if !defined(A) && defined(B)\nint dropped;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
        // A undefined, B defined -> true && true = true.
        let src = "#define B 1\n#if !defined(A) && defined(B)\nint kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
    }

    #[test]
    fn elif_not_taken_after_taken_if() {
        let src = "#if 1\nint first;\n#elif 1\nint second;\n#else\nint third;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("first"), "{}", result.output);
        assert!(!result.output.contains("second"), "{}", result.output);
        assert!(!result.output.contains("third"), "{}", result.output);
    }

    #[test]
    fn else_not_taken_after_taken_elif() {
        let src = "#if 0\nint first;\n#elif 1\nint second;\n#elif 1\nint third;\n#else\nint fourth;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("first"), "{}", result.output);
        assert!(result.output.contains("second"), "{}", result.output);
        assert!(!result.output.contains("third"), "{}", result.output);
        assert!(!result.output.contains("fourth"), "{}", result.output);
    }

    #[test]
    fn else_taken_when_no_branch_matched() {
        let src = "#if 0\nint first;\n#elif 0\nint second;\n#else\nint third;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("first"), "{}", result.output);
        assert!(!result.output.contains("second"), "{}", result.output);
        assert!(result.output.contains("third"), "{}", result.output);
    }

    #[test]
    fn if_comparisons_and_parens() {
        let src = "#define VERSION 3\n#if (VERSION >= 2) && !defined(MISSING)\nint kept;\n#endif\n#if VERSION == 2\nint dropped;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        assert!(!result.output.contains("dropped"), "{}", result.output);
    }

    #[test]
    fn if_or_precedence_over_and() {
        // C precedence: 1 || 0 && 0 == 1 || (0 && 0) -> true.
        let src = "#if 1 || 0 && 0\nint kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
    }

    #[test]
    fn if_unknown_identifier_evaluates_to_zero() {
        let src = "#if TOTALLY_UNDEFINED_NAME\nint dropped;\n#endif\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
        assert!(result.output.contains("after"), "{}", result.output);
    }

    #[test]
    fn if_defined_chained_macro_definition() {
        // Operand of defined() must not be macro-expanded (C11 6.10.1p4).
        let src = "#define ON 1\n#define FEATURE ON\n#if defined(FEATURE)\nint kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
    }

    #[test]
    fn if_function_like_macro_expands_in_condition() {
        let src =
            "#define GE(a, b) ((a) >= (b))\n#if GE(3, 2)\nint kept;\n#else\nint dropped;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        assert!(!result.output.contains("dropped"), "{}", result.output);
    }

    #[test]
    fn if_function_like_macro_uses_object_macro_args() {
        let src = "#define V 3\n#define ATLEAST(x) (V >= (x))\n#if ATLEAST(2)\nint kept;\n#else\nint dropped;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        assert!(!result.output.contains("dropped"), "{}", result.output);
    }

    #[test]
    fn if_trailing_garbage_is_false() {
        let src = "#if 1 garbage\nint dropped;\n#endif\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
        assert!(result.output.contains("after"), "{}", result.output);
    }

    #[test]
    fn if_unbalanced_paren_is_false() {
        let src = "#if (0 || 1\nint dropped;\n#endif\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
        assert!(result.output.contains("after"), "{}", result.output);
    }

    #[test]
    fn if_condition_spans_line_continuation() {
        let src = "#if 0 || \\\n    1\nint kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        assert!(
            !result.output.contains("\n1"),
            "continuation line must not leak: {}",
            result.output
        );
    }

    #[test]
    fn if_unsigned_64bit_literal_is_positive() {
        let src = "#if 0xffffffffffffffffULL > 0\nint big_ok;\n#endif\n#if 0xFFFFFFFF & 0x80000000\nint mask_ok;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("big_ok"), "{}", result.output);
        assert!(result.output.contains("mask_ok"), "{}", result.output);
    }

    #[test]
    fn if_defined_elif_fixture() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/if_defined_elif.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(result.output.contains("feature_on"), "{}", result.output);
        assert!(!result.output.contains("feature_off"), "{}", result.output);
        assert!(!result.output.contains("b1"), "{}", result.output);
        assert!(result.output.contains("b2"), "{}", result.output);
        assert!(!result.output.contains("b3"), "{}", result.output);
        assert!(!result.output.contains("b4"), "{}", result.output);
        assert!(result.output.contains("compound_ok"), "{}", result.output);
        assert!(result.output.contains("fnlike_ok"), "{}", result.output);
    }

    #[test]
    fn if_unsigned_conversion_semantics() {
        // Usual arithmetic conversions at uintmax width (C11 6.10.1p4):
        // a signed operand converts to unsigned when the other is unsigned.
        let src = "#if -1 < 1U\nint sc_dropped;\n#endif\n\
#if ~0U > 65535\nint probe_ok;\n#endif\n\
#if -1 > 0U\nint wrap_ok;\n#endif\n\
#if (0x8000000000000000 >> 63) == 1\nint shr_u_ok;\n#endif\n\
#if (-2 >> 1) == -1\nint shr_s_ok;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("sc_dropped"), "{}", result.output);
        assert!(result.output.contains("probe_ok"), "{}", result.output);
        assert!(result.output.contains("wrap_ok"), "{}", result.output);
        assert!(result.output.contains("shr_u_ok"), "{}", result.output);
        assert!(result.output.contains("shr_s_ok"), "{}", result.output);
    }

    #[test]
    fn if_true_false_keywords() {
        let src = "#if true\nint t_kept;\n#endif\n#if false\nint f_dropped;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(result.output.contains("t_kept"), "{}", result.output);
        assert!(!result.output.contains("f_dropped"), "{}", result.output);
    }

    #[test]
    fn if_ternary_combines_branch_types() {
        // The ternary result type is the common type of BOTH arms (int +
        // unsigned -> unsigned), even for the untaken arm.
        let src = "#if (1 ? -1 : 1U) < 0\nint uns_dropped;\n#endif\n#if (1 ? -1 : 1) < 0\nint sgn_kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("uns_dropped"), "{}", result.output);
        assert!(result.output.contains("sgn_kept"), "{}", result.output);
    }

    #[test]
    fn if_object_macro_aliasing_function_like_rescans() {
        let src = "#define GE(a, b) ((a) >= (b))\n#define CALL GE\n#if CALL(3, 2)\nint kept;\n#else\nint dropped;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
        assert!(!result.output.contains("dropped"), "{}", result.output);
    }

    #[test]
    fn skipped_group_tolerates_malformed_ifdef() {
        let src = "#if 0\n#ifdef 123\nint x;\n#endif\n#endif\nint after;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("after"), "{}", result.output);
        assert!(!result.output.contains("int x"), "{}", result.output);
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expected identifier")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn skipped_elif_condition_consumes_continuation() {
        let src = "#if 1\nint a;\n#elif 0 && \\\n#endif\nint x;\n#endif\nint tail;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("int a"), "{}", result.output);
        assert!(!result.output.contains("int x"), "{}", result.output);
        assert!(result.output.contains("tail"), "{}", result.output);
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("without #if")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn if_char_escapes() {
        let src = "#if '\\x41' == 65\nint hex_ok;\n#endif\n#if '\\101' == 65\nint oct_ok;\n#endif\n#if '\\012' == 10\nint oct2_ok;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("hex_ok"), "{}", result.output);
        assert!(result.output.contains("oct_ok"), "{}", result.output);
        assert!(result.output.contains("oct2_ok"), "{}", result.output);
    }

    #[test]
    fn if_line_builtin_positive() {
        let src = "#if __LINE__ > 0\nint kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
    }

    #[test]
    fn variadic_macro_definition_expands_in_condition() {
        let src = "#define ANY(...) 1\n#if ANY(x)\nint kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
    }

    #[test]
    fn cpp_digit_separators_in_condition() {
        let src = "#if 1'000'000 > 999999\nint kept;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new());
        assert!(result.output.contains("kept"), "{}", result.output);
    }

    #[test]
    fn elif_after_else_is_diagnosed() {
        let src = "#if 0\n#elif 0\n#else\nint a;\n#elif 1\nint b;\n#endif\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("int a"), "{}", result.output);
        assert!(!result.output.contains("int b"), "{}", result.output);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("#elif after #else")),
            "{:?}",
            result.diagnostics
        );
    }

    #[test]
    fn condition_expansion_bomb_hits_budget() {
        // 2^27 tokens if fully expanded; the budget must stop it quickly,
        // warn, and conservatively skip the branch. (Regression guard for
        // the budget itself: the unguarded expander OOMed on this input.)
        let mut src = String::from("#define Z0 z z\n");
        for n in 1..=27 {
            src.push_str(&format!("#define Z{n} Z{} Z{}\n", n - 1, n - 1));
        }
        src.push_str("#if Z27\nint dropped;\n#endif\nint after;\n");
        let result = preprocess_string(&src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(!result.output.contains("dropped"), "{}", result.output);
        assert!(result.output.contains("after"), "{}", result.output);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expansion budget exceeded in #if")),
            "{:?}",
            result.diagnostics
        );
    }

    /// Issue #8: a header ending inside an open `#if` must not leave its
    /// frame on the stack. The includer continues in its own (active)
    /// state, its own `#endif` still matches its own `#if`, and the
    /// truncated header is diagnosed at the offending directive.
    #[test]
    fn unterminated_if_in_header_keeps_includer_active() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/unterminated_if_include.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(
            result.output.contains("int main"),
            "includer content after the #include vanished: {}",
            result.output
        );
        assert!(
            result.output.contains("int after"),
            "includer content after its own #endif vanished: {}",
            result.output
        );
        assert!(
            !result.output.contains("int dead"),
            "skipped group leaked: {}",
            result.output
        );
        let unterminated: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("unterminated #if"))
            .collect();
        assert_eq!(unterminated.len(), 1, "{:?}", result.diagnostics);
        let d = unterminated[0];
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert_eq!(d.line, 1, "diagnostic must point at the open #if");
        assert!(
            d.file
                .as_deref()
                .is_some_and(|f| f.ends_with("unterminated_if_header.h")),
            "diagnostic must name the truncated header: {d:?}"
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("#endif without #if")),
            "includer's #endif must match its own #if: {:?}",
            result.diagnostics
        );
    }

    /// The root file gets the same EOF check, and every open frame is
    /// reported exactly once at the line of its own directive.
    #[test]
    fn unterminated_if_at_root_eof_reports_each_open_frame() {
        let src = "int before;\n#if 1\n#ifdef NOT_SET\nint hidden;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("int before"), "{}", result.output);
        assert!(!result.output.contains("int hidden"), "{}", result.output);
        let mut lines: Vec<u32> = result
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("unterminated #if"))
            .map(|d| d.line)
            .collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![2, 3], "{:?}", result.diagnostics);
    }

    /// Balanced (nested) conditionals must not trip the EOF check.
    #[test]
    fn balanced_nested_conditionals_emit_no_eof_diagnostic() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/nested_if.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unterminated #if")),
            "{:?}",
            result.diagnostics
        );
    }

    /// A closing or branch directive can only act on a frame opened in the
    /// same file. A header that starts with a stray `#endif` / `#else` /
    /// `#elif` must be diagnosed and must not pop or mutate the includer's
    /// frame, otherwise the includer's own `#endif` fails to match and the
    /// rest of the translation unit is lost.
    #[test]
    fn stray_closing_directives_in_header_do_not_touch_includer_frame() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/stray_closer_include.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert!(result.output.contains("survived"), "{}", result.output);
        assert!(result.output.contains("tail"), "{}", result.output);
        for (header, directive) in [
            ("stray_endif.h", "#endif"),
            ("stray_else.h", "#else"),
            ("stray_elif.h", "#elif"),
        ] {
            let expected = format!("{directive} without #if");
            assert!(
                result.diagnostics.iter().any(|d| {
                    d.message == expected
                        && d.line == 1
                        && d.file.as_deref().is_some_and(|f| f.ends_with(header))
                }),
                "missing `{expected}` attributed to {header}: {:?}",
                result.diagnostics
            );
        }
        assert!(
            !result.diagnostics.iter().any(|d| {
                d.message.contains("without #if")
                    && d.file
                        .as_deref()
                        .is_some_and(|f| f.ends_with("stray_closer_include.c"))
            }),
            "includer's #endif must still match its own #if: {:?}",
            result.diagnostics
        );
    }

    /// C11 6.10.3.2: `#param` becomes a string literal spelling the
    /// argument's tokens, with the original spacing collapsed to single
    /// spaces (#13). Before this the `#` was dropped together with its
    /// argument.
    #[test]
    fn stringize_quotes_argument_spelling() {
        let src = "#define STR(x) #x\n\
                   const char *a = STR(hello);\n\
                   const char *b = STR(a + b);\n\
                   const char *c = STR(a+b);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("\"hello\""), "{}", result.output);
        assert!(result.output.contains("\"a + b\""), "{}", result.output);
        assert!(result.output.contains("\"a+b\""), "{}", result.output);
        assert!(!result.output.contains('#'), "{}", result.output);
    }

    /// The operand of `#` is the argument as written, never expanded.
    /// (The two-level `XSTR(x) STR(x)` idiom that expands first needs C11
    /// argument prescan, which docs/PREPROCESSOR.md lists as unsupported.)
    #[test]
    fn stringize_uses_unexpanded_argument() {
        let src = "#define STR(x) #x\n\
                   #define VALUE 42\n\
                   const char *d = STR(VALUE);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("\"VALUE\""), "{}", result.output);
        assert!(!result.output.contains("42"), "{}", result.output);
    }

    /// `"` and `\` inside string and character literals in the argument are
    /// escaped so the result is one valid string literal.
    #[test]
    fn stringize_escapes_literals_in_argument() {
        let src = "#define STR(x) #x\n\
                   const char *c = STR(\"quoted \\\"inner\\\"\");\n\
                   const char *q = STR('\\'');\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains(r#""\"quoted \\\"inner\\\"\"""#),
            "{}",
            result.output
        );
        assert!(result.output.contains(r#""'\\''""#), "{}", result.output);
    }

    /// Newlines inside an argument count as whitespace: one space.
    #[test]
    fn stringize_collapses_newlines_to_single_space() {
        let src = "#define STR(x) #x\nconst char *m = STR(first\n    second);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("\"first second\""),
            "{}",
            result.output
        );
    }

    /// `#__VA_ARGS__` spells every variadic argument, comma-separated.
    #[test]
    fn stringize_variadic_collector_joins_arguments() {
        let src = "#define ALL(...) #__VA_ARGS__\nconst char *g = ALL(p, q);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("\"p, q\""), "{}", result.output);
    }

    /// The log/assert shapes that dominate docs/PARSE_FAILURES.md: `#cond`
    /// inside a larger body, next to `__VA_ARGS__` and `__LINE__`.
    #[test]
    fn stringize_fixture_log_and_assert_macros() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/stringize.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        // `flat` strips every space, string-literal contents included.
        let out = flat(&result.output);
        assert!(
            out.contains("log_write(\"hello%d\",1)"),
            "{}",
            result.output
        );
        assert!(
            out.contains("fail(\"x>0&&ptr!=NULL\","),
            "{}",
            result.output
        );
        assert!(!result.output.contains('#'), "{}", result.output);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn raw_string_fixture_survives_preprocessing_intact() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/raw_string.cpp");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        // The single-literal, macro and stringize shapes are asserted by the
        // unit tests above; the file adds the multi-line literal, prefixes
        // and concatenation, and the whole-file "nothing was split" check.
        let out = &result.output;
        for literal in [
            "R\"~({\n  \"file\": \"/data/log/test\",\n  \"pc\": \"0x1234\"\n})~\"",
            "LR\"(w \"x\")\"",
            "u8R\"(y \"z\")\"",
            "R\"~({\"k\":\")~\" \"v\" R\"~(\"})~\"",
            "json= R\"({\"k\":1})\"_json ;",
            "jsonViaMacro= R\"~({\"k\":2})~\"_json ;",
            "sec= \"text\"s+ 10_s ;",
            "wc= L'x' ;",
            "c16= u'y' ;",
            "int Rect= R+ 1 ;",
        ] {
            assert!(out.contains(literal), "missing {literal:?} in:\n{out}");
        }
        assert!(!out.contains("R \""), "a raw string was split:\n{out}");
        assert!(
            !out.contains("\" _json"),
            "a ud-suffix was split off:\n{out}"
        );
        assert!(
            !out.contains("L '"),
            "a prefixed char literal was split:\n{out}"
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    #[test]
    fn c_fixture_expands_macros_next_to_raw_string_and_udl_shapes() {
        use std::path::PathBuf;
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/preproc/raw_string_shapes.c");
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        let out = &result.output;
        for expected in [
            "const char* s= \"(x)\" ;",
            "int n= 'a'+ 1 ;",
            "int m[]={1 \"y\" , 2 } ;",
            "w= L\"w\" ;",
            "u= u8\"s\" ;",
            "c16= u'y' ;",
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        assert!(
            !out.contains("R\""),
            "R was lexed as a raw-string prefix:\n{out}"
        );
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    }

    /// The generated string literal is macro-expanded code, so it must map
    /// to the expansion site (the invoking macro name), not to the `#` in
    /// the definition (AGENTS.md LineMap invariant).
    #[test]
    fn stringize_maps_to_expansion_site() {
        let src = "#define STR(x) #x\n\n\nconst char *s = STR(foo);\n";
        let mut opts = PreprocessOptions::new();
        opts.track_line_map = true;
        let result = preprocess_string(src, Path::new("t.c"), &opts);
        let off = result.output.find("\"foo\"").expect("stringized literal");
        let entry = result.line_map.lookup(off).expect("line map entry");
        let invocation_col = src.lines().nth(3).unwrap().find("STR(").unwrap() as u32 + 1;
        assert_eq!(
            (entry.line, entry.col),
            (4, invocation_col),
            "{:?}",
            result.line_map
        );
    }

    /// `#__VA_ARGS__` spells the variadic arguments with the commas as
    /// written: no invented space after `,`, and a space before it only
    /// when the source had one.
    #[test]
    fn stringize_variadic_preserves_comma_spacing() {
        let src = "#define ALL(...) #__VA_ARGS__\n\
                   const char *a = ALL(p,q);\n\
                   const char *b = ALL(p, q);\n\
                   const char *c = ALL(p , q);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("\"p,q\""), "{}", result.output);
        assert!(result.output.contains("\"p, q\""), "{}", result.output);
        assert!(result.output.contains("\"p , q\""), "{}", result.output);
    }

    fn lookup_at<'a>(result: &'a PreprocessResult, needle: &str) -> &'a crate::LineMapEntry {
        let off = result
            .output
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {}", result.output));
        result.line_map.lookup(off).expect("line map entry")
    }

    fn col_of(src: &str, line: usize, needle: &str) -> u32 {
        src.lines().nth(line - 1).unwrap().find(needle).unwrap() as u32 + 1
    }

    /// A stringized literal produced through a forwarding macro maps to the
    /// outermost invocation, not to the `STR` token inside `WRAP`'s body.
    #[test]
    fn stringize_maps_to_expansion_site_through_wrapper() {
        let src = "#define STR(x) #x\n#define WRAP(x) STR(x)\n\nconst char *s = WRAP(foo);\n";
        let mut opts = PreprocessOptions::new();
        opts.track_line_map = true;
        let result = preprocess_string(src, Path::new("t.c"), &opts);
        let e = lookup_at(&result, "\"foo\"");
        assert_eq!(
            (e.line, e.col),
            (4, col_of(src, 4, "WRAP(")),
            "{:?}",
            result.line_map
        );
    }

    /// Every replacement-list token maps to the invocation (AGENTS.md
    /// LineMap invariant); an argument token keeps its own source position.
    #[test]
    fn replacement_tokens_map_to_expansion_site() {
        let src = "#define ADD(x) x + 1\n#define TWICE(x) ADD(x) * 2\n\nint a = TWICE(v);\n";
        let mut opts = PreprocessOptions::new();
        opts.track_line_map = true;
        let result = preprocess_string(src, Path::new("t.c"), &opts);
        let site = (4, col_of(src, 4, "TWICE("));
        let plus = lookup_at(&result, "+");
        assert_eq!((plus.line, plus.col), site, "{:?}", result.line_map);
        let star = lookup_at(&result, "*");
        assert_eq!((star.line, star.col), site, "{:?}", result.line_map);
        let v = lookup_at(&result, "v");
        assert_eq!(
            (v.line, v.col),
            (4, col_of(src, 4, "v)")),
            "{:?}",
            result.line_map
        );
    }

    /// `__LINE__` in a replacement list is the line of the invocation
    /// (C11 6.10.8.1), not of the `#define`.
    #[test]
    fn line_macro_in_body_expands_to_invocation_line() {
        let src = "#define HERE __LINE__\n#define VIA HERE\n\n\nint a = HERE;\nint b = VIA;\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let out = flat(&result.output);
        assert!(out.contains("inta=5;"), "{}", result.output);
        assert!(out.contains("intb=6;"), "{}", result.output);
    }

    /// Translation phase 2 (C11 5.1.1.2p1) deletes `\`-newline before
    /// tokenizing, so a tight splice inside a stringized argument is
    /// zero-width and the tokens around it are adjacent: gcc and clang both
    /// spell `STR(a\<newline>b)` as "ab", not "a b".
    #[test]
    fn stringize_tight_line_splice_is_zero_width() {
        let src = "#define STR(x) #x\nconst char *s = STR(a\\\nb);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("\"ab\""),
            "tight splice must not introduce a space: {}",
            result.output
        );
    }

    /// Only the splice itself is zero-width. Whitespace before the `\` or
    /// after the newline is real and still separates the tokens.
    #[test]
    fn stringize_keeps_whitespace_around_a_splice() {
        for src in [
            "#define STR(x) #x\nconst char *s = STR(a \\\nb);\n",
            "#define STR(x) #x\nconst char *s = STR(a\\\n b);\n",
        ] {
            let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
            assert!(
                result.output.contains("\"a b\""),
                "whitespace around a splice must survive: {}",
                result.output
            );
        }
    }

    /// A run of splices is still zero-width as long as every one of them is
    /// tight, and a splice between two punctuators behaves the same way.
    #[test]
    fn stringize_chained_and_punct_splices() {
        let chained = "#define STR(x) #x\nconst char *s = STR(a\\\n\\\nb);\n";
        let result = preprocess_string(chained, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("\"ab\""), "{}", result.output);

        let punct = "#define STR(x) #x\nconst char *s = STR(p->\\\nq);\n";
        let result = preprocess_string(punct, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains("\"p->q\""), "{}", result.output);
    }

    /// A tight splice right after a top-level `,` is still zero-width even
    /// though the separator starts a fresh argument: adjacency is measured
    /// from the last token consumed, not the last token of the argument.
    #[test]
    fn stringize_tight_splice_after_argument_separator() {
        let src = "#define ALL(...) #__VA_ARGS__\n\
                   const char *s = ALL(p,\\\nq);\n\
                   const char *t = ALL(p\\\n,q);\n\
                   const char *u = ALL(p, \\\nq);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains("s= \"p,q\" ;"), "{out}");
        assert!(out.contains("t= \"p,q\" ;"), "{out}");
        assert!(out.contains("u= \"p, q\" ;"), "{out}");
    }

    /// The splice flag rides on the token through substitution, so an
    /// argument forwarded to another macro and stringized there still
    /// spells the tight splice as zero-width.
    #[test]
    fn stringize_tight_splice_survives_forwarding() {
        let src = "#define INNER(x) #x\n\
                   #define OUTER(x) INNER(x)\n\
                   const char *s = OUTER(a\\\nb);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(
            result.output.contains("\"ab\""),
            "splice flag must survive nested argument parsing: {}",
            result.output
        );
    }

    /// An argument substituted into a body takes the parameter's adjacency,
    /// not the call site's: `S((x))` invoked as `G( a )` stringizes to
    /// "(a)" and `S( x )` invoked as `H(a)` to "a", as in gcc and clang.
    #[test]
    fn stringize_substituted_argument_takes_parameter_adjacency() {
        let src = "#define S(x) #x\n\
                   #define G(x) S((x))\n\
                   #define H(x) S( x )\n\
                   const char *a = G( a );\n\
                   const char *b = H(a);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains("a= \"(a)\" ;"), "{out}");
        assert!(out.contains("b= \"a\" ;"), "{out}");
    }

    /// A parameter that expands to nothing leaves its whitespace behind:
    /// the next token is separated when either the parameter or that token
    /// had whitespace before it, and glued only when neither did. A `##`
    /// placemarker leaves nothing. All four outputs are clang's.
    #[test]
    fn stringize_empty_argument_keeps_parameter_whitespace() {
        let src = "#define S(x) #x\n\
                   #define G1(x) S(a x+b)\n\
                   #define G2(x) S(a(x +b))\n\
                   #define G3(x) S(a(x+b))\n\
                   #define R(y) S(a ## y+b)\n\
                   const char *g1 = G1();\n\
                   const char *g2 = G2();\n\
                   const char *g3 = G3();\n\
                   const char *r = R();\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains("g1= \"a +b\" ;"), "{out}");
        assert!(out.contains("g2= \"a( +b)\" ;"), "{out}");
        assert!(out.contains("g3= \"a(+b)\" ;"), "{out}");
        assert!(out.contains("r= \"a+b\" ;"), "{out}");
    }

    /// An empty parameter on the left of `##` is a placemarker (C99
    /// 6.10.3.3p2), so the paste is a no-op and the token *before* the
    /// parameter is not an operand: `S(a x ## +b)` is "a +b", never the
    /// pasted non-token `a+`. Values checked against gcc and clang.
    #[test]
    fn empty_left_paste_preserves_operand_boundary() {
        let src = include_str!("../../../tests/fixtures/preproc/empty_left_paste.c");
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        for expected in [
            r#"b "a +b""#,
            r#"p "(y)""#,
            r#"q "a z""#,
            r#"r "a xz""#,
            r#"v "a +b""#,
            r#"n "a +b""#,
            r#"e "a +b""#,
        ] {
            assert!(
                result.output.contains(expected),
                "missing {expected}: {}",
                result.output
            );
        }
    }

    /// The same shapes outside a `#`, where the damage reaches the parser: a
    /// bad paste destroys the string literal in `a x ## "s"` and swallows the
    /// `(` in `f(x ## y)`, and tree-sitter loses the enclosing construct.
    #[test]
    fn empty_left_paste_preserves_non_stringized_tokens() {
        let src = include_str!("../../../tests/fixtures/preproc/empty_left_paste.c");
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        assert!(result.output.contains(r#"c a "s""#), "{}", result.output);
        assert!(result.output.contains("d f(y)"), "{}", result.output);
    }

    /// An empty parameter between a comma and `## __VA_ARGS__` leaves the GNU
    /// comma rule alone: it is the comma the omitted varargs delete, not a
    /// token the placemarker may shield. The compilers split here — clang
    /// collapses the placemarker first and deletes the comma (`g(o)`), while
    /// gcc applies the rule only to a comma spelled immediately before the
    /// `##` and so keeps it (`g(o, )`). We follow clang. Swallowing the `##`
    /// as an ordinary placemarker would land on gcc's output by accident,
    /// not by rule, and inconsistently — see the newline-blank case below.
    #[test]
    fn empty_left_paste_keeps_gnu_comma_deletion() {
        let src = include_str!("../../../tests/fixtures/preproc/empty_left_paste.c");
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        for expected in ["x g(o)", "y g(o ,)", "z h(t)"] {
            assert!(
                result.output.contains(expected),
                "missing {expected}: {}",
                result.output
            );
        }
    }

    /// The same GNU-comma shapes with a newline-only blank argument, pinned
    /// as they behave rather than as a rule they follow. `is_gnu_comma_paste`
    /// reads the last token already emitted, and an argument that is only
    /// newlines pushes those first, hiding the comma from the predicate — so
    /// the placemarker swallows the `##` after all and the comma survives.
    /// That lands on gcc's output where the token-only spelling above lands
    /// on clang's; both compilers are self-consistent across the two
    /// spellings and we are not. Reading the body's comma instead of
    /// `out.last()` would settle it. Pinned so the split cannot drift
    /// unnoticed while it stands.
    #[test]
    fn empty_left_paste_newline_blank_keeps_gnu_comma() {
        let src = include_str!("../../../tests/fixtures/preproc/empty_left_paste.c");
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        for expected in ["w g(o ,)", "u h(t ,)"] {
            assert!(
                result.output.contains(expected),
                "missing {expected}: {}",
                result.output
            );
        }
    }

    /// An argument that is only newlines is empty too — whitespace, not
    /// tokens — so it leaves the parameter's whitespace behind exactly as an
    /// omitted one does, and an argument that starts with a newline takes
    /// the parameter's adjacency on its first real token (clang's outputs).
    #[test]
    fn stringize_newline_only_argument_is_empty() {
        let src = "#define S(x) #x\n\
                   #define G1(x) S(a x+b)\n\
                   #define G3(x) S(a(x+b))\n\
                   const char *g1 = G1(\n);\n\
                   const char *g3 = G3(\n);\n\
                   const char *g4 = G3(\nc);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains("g1= \"a +b\" ;"), "{out}");
        assert!(out.contains("g3= \"a(+b)\" ;"), "{out}");
        assert!(out.contains("g4= \"a(c+b)\" ;"), "{out}");
    }

    /// The literal `#x` produces stands where the `#` stood, so a nested
    /// stringize spells it with the `#`'s adjacency (clang's outputs).
    #[test]
    fn stringize_literal_takes_hash_adjacency() {
        let src = "#define S(x) #x\n\
                   #define J(x) S((#x))\n\
                   #define K(x) S(( #x ))\n\
                   #define H(x) S(a#x +b)\n\
                   const char *j = J(a);\n\
                   const char *k = K(a);\n\
                   const char *h = H(z);\n";
        let result = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new());
        let out = &result.output;
        assert!(out.contains(r#"j= "(\"a\")" ;"#), "{out}");
        assert!(out.contains(r#"k= "( \"a\" )" ;"#), "{out}");
        assert!(out.contains(r#"h= "a\"z\" +b" ;"#), "{out}");
    }

    #[test]
    fn cached_guard_skip_preserves_interleaved_macro_effects() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("b.h"),
            "#ifndef B_H\n#define B_H\n#define VALUE 1\n#define REMOVED 1\nint b;\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("d.h"),
            "#undef VALUE\n#define VALUE 2\n#undef REMOVED\nint d;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("a.h"),
            "#include \"b.h\"\n#include \"d.h\"\n#include \"b.h\"\nint a;\n",
        )
        .unwrap();
        let source = "#include \"a.h\"\nint value = VALUE;\n#ifdef REMOVED\nint wrong;\n#endif\n";
        let path = dir.path().join("main.c");
        fs::write(&path, source).unwrap();
        let base = PreprocessOptions::new().with_include(dir.path().to_path_buf());
        let live = preprocess_file(&path, &base).unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = base.with_include_expansion_cache(cache);
        preprocess_file(&path, &opts).unwrap();
        let replay = preprocess_file(&path, &opts.with_frozen_expansion_cache(true)).unwrap();
        let tail = |text: &str| text[text.find("int value").unwrap()..].to_string();
        assert_eq!(tail(&live.output), "int value= 2 ;\n");
        assert_eq!(tail(&replay.output), tail(&live.output));
    }

    #[test]
    fn cached_guard_skip_does_not_replay_conflicting_macro_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("b.h"),
            "#ifndef B_H\n#define B_H\n#ifdef VALUE\nint configured;\n#endif\n#define VALUE 1\nint b;\n#endif\n",
        )
        .unwrap();
        fs::write(dir.path().join("a.h"), "#include \"b.h\"\nint a;\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.path().to_path_buf())
            .with_include_expansion_cache(cache);
        let path = dir.path().join("main.c");
        fs::write(
            &path,
            "#include \"b.h\"\n#undef VALUE\n#define VALUE 2\n#include \"a.h\"\n#include \"c.h\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("c.h"), "#include \"a.h\"\nint c;\n").unwrap();
        preprocess_file(&path, &opts).unwrap();
        fs::write(&path, "#include \"a.h\"\nint value = VALUE;\n").unwrap();
        let opts = opts.with_frozen_expansion_cache(true);
        let result = preprocess_file(&path, &opts).unwrap();
        assert!(
            result.output.contains("int value= 1 ;"),
            "{}",
            result.output
        );
        fs::write(&path, "#define VALUE 2\n#include \"c.h\"\n").unwrap();
        let nested = preprocess_file(&path, &opts).unwrap();
        assert!(
            nested.output.contains("int configured ;"),
            "{}",
            nested.output
        );
    }

    #[test]
    fn cached_guard_skip_tracks_external_macro_override() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("b.h"),
            "#ifndef B_H\n#define B_H\n#define VALUE 1\nint b;\n#endif\n",
        )
        .unwrap();
        fs::write(dir.path().join("a.h"), "#include \"b.h\"\nint a;\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.path().to_path_buf())
            .with_include_expansion_cache(cache);
        let path = dir.path().join("main.c");
        for value in [2, 3] {
            fs::write(&path, format!("#include \"b.h\"\n#undef VALUE\n#define VALUE {value}\n#include \"a.h\"\nint value = VALUE;\n")).unwrap();
            preprocess_file(&path, &opts).unwrap();
            let replay =
                preprocess_file(&path, &opts.clone().with_frozen_expansion_cache(true)).unwrap();
            assert!(
                replay.output.contains(&format!("int value= {value} ;")),
                "{}",
                replay.output
            );
        }
    }

    /// The issue's reproduction, at the level the defect lives on: one table
    /// included once per `#define` of its entry macro. Nothing about the
    /// file says "once", so every inclusion expands (#56).
    #[test]
    fn unguarded_file_re_expands_on_every_include() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("list.def"), "ENTRY(alpha)\nENTRY(beta)\n").unwrap();
        let path = dir.path().join("t.c");
        fs::write(
            &path,
            "#define ENTRY(n) void fn_##n(void) {}\n#include \"list.def\"\n#undef ENTRY\n\
             #define ENTRY(n) void use_##n(void) { fn_##n(); }\n#include \"list.def\"\n",
        )
        .unwrap();
        let out = preprocess_file(&path, &PreprocessOptions::new())
            .unwrap()
            .output;
        for name in ["fn_alpha", "fn_beta", "use_alpha", "use_beta"] {
            assert!(out.contains(name), "{name} missing from {out}");
        }
    }

    /// An include guard suppresses while its name is defined, and stops
    /// suppressing when the name goes away.
    #[test]
    fn include_guard_suppresses_only_while_its_name_is_defined() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("body.h"),
            "#ifndef BODY_H\n#define BODY_H\nint MARK;\n#endif\n",
        )
        .unwrap();
        let path = dir.path().join("t.c");
        fs::write(
            &path,
            "#define MARK a\n#include \"body.h\"\n#undef MARK\n#define MARK b\n\
             #include \"body.h\"\n#undef BODY_H\n#undef MARK\n#define MARK c\n\
             #include \"body.h\"\n",
        )
        .unwrap();
        let out = preprocess_file(&path, &PreprocessOptions::new())
            .unwrap()
            .output;
        assert!(out.contains("int a ;"), "{out}");
        assert!(!out.contains("int b ;"), "guard did not suppress: {out}");
        assert!(
            out.contains("int c ;"),
            "`#undef` of the guard ignored: {out}"
        );
    }

    /// `#pragma once` read with the enclosing conditional active holds for
    /// the rest of the run, whatever later happens to that condition.
    #[test]
    fn active_pragma_once_holds_for_the_rest_of_the_run() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("once.h"),
            "#ifdef ENABLE_ONCE\n#pragma once\n#endif\nint MARK;\n",
        )
        .unwrap();
        let path = dir.path().join("t.c");
        let source = |enable: &str| {
            format!(
                "{enable}#define MARK a\n#include \"once.h\"\n#undef ENABLE_ONCE\n\
                 #undef MARK\n#define MARK b\n#include \"once.h\"\n"
            )
        };
        fs::write(&path, source("#define ENABLE_ONCE 1\n")).unwrap();
        let out = preprocess_file(&path, &PreprocessOptions::new())
            .unwrap()
            .output;
        assert!(out.contains("int a ;"), "{out}");
        assert!(!out.contains("int b ;"), "`#pragma once` was undone: {out}");

        // The same header reached with the conditional inactive: a pragma in
        // a skipped group states nothing, so the file re-expands. This is a
        // separate run by construction — once the first run has read an
        // active `#pragma once`, it holds for that whole unit.
        fs::write(&path, source("")).unwrap();
        let out = preprocess_file(&path, &PreprocessOptions::new())
            .unwrap()
            .output;
        assert!(out.contains("int a ;") && out.contains("int b ;"), "{out}");
    }

    /// `a.h` includes `b.h` includes `a.h`, both guarded. The recursion now
    /// terminates on the guard rather than on the blanket path set, and
    /// neither body may be lost to it.
    #[test]
    fn guarded_recursive_includes_terminate_with_both_bodies() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.h"),
            "#ifndef A_H\n#define A_H\n#include \"b.h\"\nint a;\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.h"),
            "#ifndef B_H\n#define B_H\n#include \"a.h\"\nint b;\n#endif\n",
        )
        .unwrap();
        let path = dir.path().join("t.c");
        fs::write(&path, "#include \"a.h\"\n").unwrap();
        let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        assert_eq!(
            result.output.matches("int a ;").count(),
            1,
            "{}",
            result.output
        );
        assert_eq!(
            result.output.matches("int b ;").count(),
            1,
            "{}",
            result.output
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("include depth")),
            "termination came from the depth cap: {:?}",
            result.diagnostics
        );
    }

    /// Guard-driven skips still feed the cache frames, so a diamond graph
    /// costs one expansion per header rather than one per path through it.
    #[test]
    fn diamond_include_graph_does_not_re_splice_exponentially() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("h0.h"),
            "#ifndef H0\n#define H0\nint h0;\n#endif\n",
        )
        .unwrap();
        const LEVELS: usize = 12;
        for level in 1..=LEVELS {
            fs::write(
                dir.path().join(format!("h{level}.h")),
                format!(
                    "#ifndef H{level}\n#define H{level}\n#include \"h{prev}.h\"\n\
                     #include \"h{prev}.h\"\nint h{level};\n#endif\n",
                    prev = level - 1
                ),
            )
            .unwrap();
        }
        let path = dir.path().join("t.c");
        fs::write(&path, format!("#include \"h{LEVELS}.h\"\n")).unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new().with_include_expansion_cache(cache);
        let result = preprocess_file(&path, &opts).unwrap();
        for level in 0..=LEVELS {
            assert_eq!(
                result.output.matches(&format!("int h{level} ;")).count(),
                1,
                "h{level} re-spliced: {} bytes of output",
                result.output.len()
            );
        }
    }

    /// The wrapper shapes that are an include guard, and the near-misses
    /// that are not. Read structurally, so this is decided the same way for
    /// a file's first inclusion as for its tenth.
    #[test]
    fn include_guard_shape_is_read_off_the_token_stream() {
        let guard_of = |src: &str| {
            detect_include_guard(&Lexer::new(src, Language::C).tokenize())
                .map(|n| n.as_ref().to_string())
        };
        assert_eq!(
            guard_of("#ifndef G\n#define G\nint x;\n#endif\n").as_deref(),
            Some("G")
        );
        // Leading comments and blank lines are not tokens.
        assert_eq!(
            guard_of("/* c */\n\n#ifndef G\n#define G\nint x;\n#endif // G\n").as_deref(),
            Some("G")
        );
        assert_eq!(
            guard_of("#if !defined(G)\n#define G\nint x;\n#endif\n").as_deref(),
            Some("G")
        );
        // Nested conditionals do not close the wrapper early.
        assert_eq!(
            guard_of("#ifndef G\n#define G\n#ifdef A\nint a;\n#else\nint b;\n#endif\n#endif\n")
                .as_deref(),
            Some("G")
        );
        // A second arm makes it a conditional on the name, not a guard.
        assert_eq!(
            guard_of("#ifndef G\n#define G\nint x;\n#else\nint y;\n#endif\n"),
            None
        );
        // Content before the wrapper, or after it.
        assert_eq!(
            guard_of("int before;\n#ifndef G\n#define G\n#endif\n"),
            None
        );
        assert_eq!(guard_of("#ifndef G\n#define G\n#endif\nint after;\n"), None);
        // The `#define` must be the next DIRECTIVE, of that name — ordinary
        // text before it changes nothing, since it is inside the wrapper and
        // therefore skipped on a later inclusion either way.
        assert_eq!(
            guard_of("#ifndef G\nint x;\n#define G\n#endif\n").as_deref(),
            Some("G")
        );
        assert_eq!(
            guard_of("#ifndef G\n#if A\n#define G\n#endif\n#endif\n"),
            None
        );
        assert_eq!(guard_of("#ifndef G\n#define H\n#endif\n"), None);
        assert_eq!(guard_of("#ifdef G\n#define G\n#endif\n"), None);
        assert_eq!(guard_of("int x;\n"), None);
    }

    /// A `#pragma once` header embedded in a cached expansion must hand its
    /// guard to the consumer along with its text. `record_guard` returned
    /// early once `Once` was already known, which kept the guard out of
    /// every frame opened after that point — so the entry carried the body
    /// and not the reason to skip it, and the consumer expanded it twice.
    #[test]
    fn pragma_once_guard_reaches_frames_opened_after_it_was_learned() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("once.h"), "#pragma once\nint once_body;\n").unwrap();
        fs::write(dir.path().join("a.h"), "#include \"once.h\"\nint a;\n").unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = PreprocessOptions::new()
            .with_include(dir.path().to_path_buf())
            .with_include_expansion_cache(cache);
        let path = dir.path().join("main.c");

        // Producer: once.h is expanded before a.h opens its cache frame, so
        // a.h skips it and embeds its text.
        fs::write(&path, "#include \"once.h\"\n#include \"a.h\"\n").unwrap();
        let produced = preprocess_file(&path, &opts).unwrap();
        assert_eq!(produced.output.matches("int once_body ;").count(), 1);

        // Consumer: replays a.h, then reaches once.h itself.
        fs::write(&path, "#include \"a.h\"\n#include \"once.h\"\n").unwrap();
        let consumed =
            preprocess_file(&path, &opts.clone().with_frozen_expansion_cache(true)).unwrap();
        assert_eq!(
            consumed.output.matches("int once_body ;").count(),
            1,
            "`#pragma once` body expanded twice: {}",
            consumed.output
        );
    }

    /// Repetition is legitimate but not unbounded: past the cap the include
    /// is skipped and said so, rather than lost the way the blanket
    /// suppression lost it.
    #[test]
    fn repeated_expansion_of_one_path_is_capped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.h"), "int x;\n").unwrap();
        let path = dir.path().join("t.c");
        fs::write(&path, "#include \"x.h\"\n".repeat(6)).unwrap();
        let opts = PreprocessOptions::new().with_max_file_expansions(3);
        let result = preprocess_file(&path, &opts).unwrap();
        assert_eq!(
            result.output.matches("int x ;").count(),
            3,
            "{}",
            result.output
        );
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.message.contains("include expanded more than 3 times"))
                .count(),
            1,
            "expected exactly one report: {:?}",
            result.diagnostics
        );
    }

    /// A header whose body was guard-skipped contributes no text, no macro
    /// ops and no files. Caching it anyway — as happened when its own
    /// "expanded to nothing" warning was the only thing in the entry — makes
    /// `splice_cached` hand later translation units an empty expansion, so
    /// the header's declarations vanish for every TU that reaches it with the
    /// guard undefined.
    #[test]
    fn guard_skipped_header_is_not_cached_as_empty() {
        let dir = unique_tmp_dir("empty_entry");
        fs::write(
            dir.join("guarded.h"),
            "#ifndef G\n#define G\nint from_guarded;\n#endif\n",
        )
        .unwrap();
        fs::write(dir.join("first.c"), "#define G 1\n#include \"guarded.h\"\n").unwrap();
        fs::write(dir.join("second.c"), "#include \"guarded.h\"\n").unwrap();

        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = || {
            PreprocessOptions::new()
                .with_include_expansion_cache(Arc::clone(&cache))
                .with_include(dir.path.clone())
        };

        // G is already defined here, so guarded.h expands to nothing and
        // warns about it.
        let first = preprocess_file(&dir.join("first.c"), &opts()).unwrap();
        assert!(
            first
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expanded to nothing")),
            "{:?}",
            first.diagnostics
        );
        assert!(
            cache
                .read()
                .unwrap()
                .get(&(dir.join("guarded.h"), Language::C))
                .is_none(),
            "a contentless expansion must not be cached"
        );

        // A fresh macro table: G is undefined, so the body must come through.
        let second = preprocess_file(&dir.join("second.c"), &opts()).unwrap();
        assert!(
            second.output.contains("from_guarded"),
            "starved by the cache: {:?}",
            second.output
        );
    }

    /// Budget and depth limits are properties of the run that hit them, not
    /// of the header that happened to be open at the time. Publishing a
    /// cut-short expansion handed a truncated body — and the aborting run's
    /// own error — to every later consumer of that header.
    #[test]
    fn aborted_expansion_is_not_cached() {
        let dir = unique_tmp_dir("aborted_entry");
        let parent: String = (0..40).map(|i| format!("int p{i};\n")).collect();
        fs::write(dir.join("parent.h"), &parent).unwrap();
        // big.c emits ~1900 bytes of its own before reaching parent.h, so the
        // 2000-byte cap trips while parent.h is still being expanded.
        let mut big: String = (0..190).map(|i| format!("int b{i};\n")).collect();
        big.push_str("#include \"parent.h\"\n");
        fs::write(dir.join("big.c"), &big).unwrap();
        fs::write(dir.join("small.c"), "#include \"parent.h\"\n").unwrap();

        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let opts = |cache: Option<&ExpansionCache>| {
            let base = PreprocessOptions::new()
                .with_max_output_bytes(2000)
                .with_include(dir.path.clone());
            match cache {
                Some(c) => base.with_include_expansion_cache(Arc::clone(c)),
                None => base,
            }
        };

        let big_run = preprocess_file(&dir.join("big.c"), &opts(Some(&cache))).unwrap();
        assert!(
            big_run
                .diagnostics
                .iter()
                .any(|d| d.message.contains("exceeded")),
            "the cap should trip in this run: {:?}",
            big_run.diagnostics
        );

        // small.c is nowhere near the cap and must be expanded exactly as it
        // would be with no cache at all.
        let cached = preprocess_file(&dir.join("small.c"), &opts(Some(&cache))).unwrap();
        let uncached = preprocess_file(&dir.join("small.c"), &opts(None)).unwrap();
        assert_eq!(
            cached.output, uncached.output,
            "cached expansion of parent.h is truncated"
        );
        let inherited: Vec<_> = cached
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("exceeded") || d.message.contains("stopped in"))
            .collect();
        assert!(
            inherited.is_empty(),
            "inherited another run's budget diagnostics: {inherited:?}"
        );
    }

    /// A nested header that resolves but cannot be read aborts `process_file`
    /// through `?`, and `handle_include` swallows that error so the enclosing
    /// header finishes normally. Its expansion is missing the failed
    /// include's content, so it must not be published: once the file becomes
    /// readable again, every consumer routed through that entry would still
    /// be starved of it.
    #[test]
    #[cfg(unix)]
    fn unreadable_include_does_not_poison_the_enclosing_header() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_tmp_dir("leaked_frame");
        fs::write(
            dir.join("common.h"),
            "#ifndef COMMON_H\n#define COMMON_H\nint common_decl;\n#endif\n",
        )
        .unwrap();
        fs::write(dir.join("bad.h"), "int recovered_decl;\n").unwrap();
        fs::set_permissions(dir.join("bad.h"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(
            dir.join("parent.h"),
            "#include \"common.h\"\n#include \"bad.h\"\nint parent_decl;\n",
        )
        .unwrap();
        // common.h first, so parent.h's include of it is a guard skip that
        // has to be recorded on parent.h's own frame.
        fs::write(
            dir.join("main.c"),
            "#include \"common.h\"\n#include \"parent.h\"\n",
        )
        .unwrap();
        fs::write(dir.join("other.c"), "#include \"parent.h\"\n").unwrap();

        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let cached_opts = PreprocessOptions::new()
            .with_include_expansion_cache(Arc::clone(&cache))
            .with_include(dir.path.clone());

        preprocess_file(&dir.join("main.c"), &cached_opts).unwrap();

        // The condition clears. Comparing the two while bad.h is still
        // unreadable would starve both sides equally and prove nothing.
        fs::set_permissions(dir.join("bad.h"), fs::Permissions::from_mode(0o644)).unwrap();

        let cached = preprocess_file(&dir.join("other.c"), &cached_opts).unwrap();
        let uncached = preprocess_file(
            &dir.join("other.c"),
            &PreprocessOptions::new().with_include(dir.path.clone()),
        )
        .unwrap();
        assert_eq!(
            cached.output, uncached.output,
            "parent.h was published while missing the unreadable include"
        );
        assert!(
            cached.output.contains("recovered_decl"),
            "{:?}",
            cached.output
        );
        assert!(cached.output.contains("common_decl"), "{:?}", cached.output);
    }

    fn record(src: &str, opts: PreprocessOptions) -> Vec<ConditionalChain> {
        preprocess_string(
            src,
            Path::new("test.c"),
            &opts.with_record_conditionals(true),
        )
        .conditionals
    }

    fn reads_of(arm: &ConditionalArm) -> Vec<(&str, Option<bool>)> {
        arm.reads
            .iter()
            .map(|r| (r.name.as_str(), r.bound))
            .collect()
    }

    #[test]
    fn conditionals_are_not_recorded_by_default() {
        let src = "#ifdef A\nint a;\n#endif\n";
        let result = preprocess_string(src, Path::new("test.c"), &PreprocessOptions::new());
        assert!(result.conditionals.is_empty());
    }

    #[test]
    fn records_if_elif_else_chain_with_outcomes() {
        let src = "#if A\nint a;\n#elif B\nint b;\nint b2;\n#else\nint c;\n#endif\nint after;\n";
        let chains = record(src, PreprocessOptions::new().with_define("B", "1"));
        assert_eq!(chains.len(), 1);
        let chain = &chains[0];
        assert!(chain.terminated);
        assert!(!chain.include_guard);
        assert_eq!(chain.depth, 0);
        assert_eq!(chain.line(), 1);
        let arms = &chain.arms;
        assert_eq!(arms.len(), 3);

        assert_eq!(arms[0].directive, ArmDirective::If);
        assert_eq!(arms[0].expression, "A");
        assert_eq!((arms[0].line, arms[0].end_line), (1, 3));
        assert_eq!(arms[0].outcome, ArmOutcome::Skipped);
        assert!(arms[0].evaluated);
        assert_eq!(reads_of(&arms[0]), vec![("A", Some(false))]);
        assert_eq!(arms[0].body_lines(), 1);

        assert_eq!(arms[1].directive, ArmDirective::Elif);
        assert_eq!(arms[1].expression, "B");
        assert_eq!((arms[1].line, arms[1].end_line), (3, 6));
        assert_eq!(arms[1].outcome, ArmOutcome::Taken);
        assert!(arms[1].evaluated);
        assert_eq!(reads_of(&arms[1]), vec![("B", Some(true))]);
        assert_eq!(arms[1].body_lines(), 2);

        assert_eq!(arms[2].directive, ArmDirective::Else);
        assert_eq!(arms[2].expression, "");
        assert_eq!((arms[2].line, arms[2].end_line), (6, 8));
        assert_eq!(arms[2].outcome, ArmOutcome::Skipped);
        assert!(!arms[2].evaluated);
        assert!(arms[2].reads.is_empty());
    }

    /// An undefined name does not always select `#else`: `#if !X` with `X`
    /// unknown takes the FIRST arm. The record says which arm was taken
    /// rather than assuming a direction.
    #[test]
    fn negated_unknown_name_takes_the_first_arm() {
        let src = "#if !X\nint first;\n#else\nint second;\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        let arms = &chains[0].arms;
        assert_eq!(arms[0].outcome, ArmOutcome::Taken);
        assert_eq!(arms[0].expression, "!X");
        assert_eq!(reads_of(&arms[0]), vec![("X", Some(false))]);
        assert_eq!(arms[1].outcome, ArmOutcome::Skipped);
    }

    #[test]
    fn ifdef_and_ifndef_record_their_operand() {
        let src = "#ifdef A\nint a;\n#endif\n#ifndef A\nint na;\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0].arms[0].directive, ArmDirective::Ifdef);
        assert_eq!(chains[0].arms[0].expression, "A");
        assert_eq!(chains[0].arms[0].outcome, ArmOutcome::Skipped);
        assert_eq!(reads_of(&chains[0].arms[0]), vec![("A", Some(false))]);
        assert_eq!(chains[1].arms[0].directive, ArmDirective::Ifndef);
        assert_eq!(chains[1].arms[0].expression, "A");
        assert_eq!(chains[1].arms[0].outcome, ArmOutcome::Taken);
        assert_eq!(chains[1].line(), 4);
    }

    #[test]
    fn if_expression_is_kept_as_written() {
        let src = "#define ONE 1\n#if defined(A) && (ONE > 0) || defined B\nint a;\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        let arm = &chains[0].arms[0];
        assert_eq!(arm.expression, "defined(A) && (ONE > 0) || defined B");
        assert_eq!(
            reads_of(arm),
            vec![("A", Some(false)), ("ONE", Some(true)), ("B", Some(false))]
        );
    }

    /// `#if HAS_X` with `#define HAS_X defined(X)` depends on `X`, which
    /// only expansion reveals: the reads follow the evaluation, not the
    /// spelling.
    #[test]
    fn condition_reads_follow_macro_expansion() {
        let src = "#define HAS_X defined(X)\n#if HAS_X\nint a;\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(
            reads_of(&chains[0].arms[0]),
            vec![("HAS_X", Some(true)), ("X", Some(false))]
        );
    }

    /// A read is recorded once per arm, however often the expression
    /// consults it.
    #[test]
    fn repeated_reads_are_recorded_once() {
        let src = "#if defined(A) || A > 1\nint a;\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(reads_of(&chains[0].arms[0]), vec![("A", Some(false))]);
    }

    /// A chain inside an excluded arm is not evaluated (C11 6.10.1p6). Its
    /// arms are recorded as such — lines it encloses are excluded by the
    /// OUTER chain — and its names are known only by their spelling.
    #[test]
    fn chain_inside_skipped_arm_is_unevaluated() {
        let src = "#ifdef X\n#if Y > 1\nint a;\n#else\nint b;\n#endif\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(chains.len(), 2);
        let outer = &chains[0];
        assert_eq!(outer.depth, 0);
        assert_eq!(outer.arms[0].outcome, ArmOutcome::Skipped);
        assert_eq!((outer.arms[0].line, outer.arms[0].end_line), (1, 7));
        let inner = &chains[1];
        assert_eq!(inner.depth, 1);
        assert_eq!(inner.arms.len(), 2);
        assert_eq!(inner.arms[0].outcome, ArmOutcome::Unevaluated);
        assert!(!inner.arms[0].evaluated);
        assert_eq!(inner.arms[0].expression, "Y > 1");
        assert_eq!(reads_of(&inner.arms[0]), vec![("Y", None)]);
        assert_eq!(inner.arms[1].outcome, ArmOutcome::Unevaluated);
        assert_eq!((inner.arms[0].line, inner.arms[0].end_line), (2, 4));
        assert_eq!((inner.arms[1].line, inner.arms[1].end_line), (4, 6));
    }

    /// An `#elif` after a taken arm is skipped without being evaluated; its
    /// names are still reported, from the spelling.
    #[test]
    fn elif_after_taken_arm_is_skipped_without_evaluation() {
        let src = "#if 1\nint a;\n#elif defined(B) && C\nint b;\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        let arm = &chains[0].arms[1];
        assert_eq!(arm.outcome, ArmOutcome::Skipped);
        assert!(!arm.evaluated);
        assert_eq!(reads_of(arm), vec![("B", None), ("C", None)]);
        assert!(chains[0].arms[0].reads.is_empty());
    }

    #[test]
    fn unterminated_chain_is_closed_at_end_of_file() {
        let src = "int before;\n#ifdef A\nint a;\nint b;\n";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(chains.len(), 1);
        assert!(!chains[0].terminated);
        assert_eq!(chains[0].arms[0].line, 2);
        assert_eq!(chains[0].arms[0].end_line, 5);
    }

    /// Without a trailing newline the EOF token sits on the last line
    /// itself; the arm still has to span that line.
    #[test]
    fn unterminated_chain_without_trailing_newline_spans_the_last_line() {
        let src = "int before;\n#ifdef A\nint a;\nint b;";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(chains[0].arms[0].end_line, 5);
        assert_eq!(chains[0].arms[0].body_lines(), 2);
    }

    #[test]
    fn unterminated_chain_counts_trailing_comment_and_whitespace_lines() {
        let fixture =
            include_str!("../../../tests/fixtures/preproc/conditional_unterminated_comment.c");
        for src in [
            fixture,
            "#if 0\n   ",
            "#if 0\n/* comment */",
            "#if 0\n// final comment\n",
        ] {
            let chains = record(src, PreprocessOptions::new());
            assert_eq!(chains[0].arms[0].end_line, 3, "{src:?}");
            assert_eq!(chains[0].arms[0].body_lines(), 1, "{src:?}");
        }
    }

    #[test]
    fn conditional_macro_operands_keep_operator_shaped_names() {
        for directive in [
            "ifdef and",
            "ifndef and",
            "if defined(and)",
            "if defined and",
        ] {
            for outer in ["", "#if 0\n"] {
                let src = format!(
                    "{outer}#{directive}\nint x;\n#endif\n{}",
                    if outer.is_empty() { "" } else { "#endif\n" }
                );
                let chains = record(&src, PreprocessOptions::new());
                let arm = &chains.last().unwrap().arms[0];
                let bound = if outer.is_empty() { Some(false) } else { None };
                assert_eq!(reads_of(arm), vec![("and", bound)], "{src:?}");
            }
        }
    }

    #[test]
    fn conditional_directives_start_at_hash_before_splice() {
        let src = "#\\\nif 0\nint x;\n#\\\nelse\nint y;\n#\\\nendif\n";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(chains[0].line(), 1);
        assert_eq!(chains[0].arms[0].end_line, 4);
        assert_eq!(chains[0].arms[0].body_lines(), 2);
        assert_eq!(chains[0].arms[1].line, 4);
        assert_eq!(chains[0].arms[1].end_line, 7);
    }

    #[test]
    fn include_guard_is_recognized_and_other_ifndefs_are_not() {
        let dir = unique_tmp_dir("cond_guard");
        fs::write(
            dir.join("guarded.h"),
            "/* license */\n#ifndef GUARDED_H\n#define GUARDED_H\nint g;\n#endif /* GUARDED_H */\n\n",
        )
        .unwrap();
        fs::write(
            dir.join("not_defined.h"),
            "#if !defined(ND_H)\n#define ND_H\nint nd;\n#endif\n",
        )
        .unwrap();
        // The default-value idiom: same opening shape, code after the #endif.
        fs::write(
            dir.join("default.h"),
            "#ifndef LOG_TAG\n#define LOG_TAG 1\n#endif\nint d;\n",
        )
        .unwrap();
        // Something before the #ifndef, and a #define of a different name.
        fs::write(
            dir.join("late.h"),
            "int early;\n#ifndef LATE_H\n#define LATE_H\nint l;\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("other_define.h"),
            "#ifndef OD_H\n#define SOMETHING_ELSE\nint o;\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("main.c"),
            "#include \"guarded.h\"\n#include \"not_defined.h\"\n#include \"default.h\"\n#include \"late.h\"\n#include \"other_define.h\"\n#ifndef MAIN_H\n#define MAIN_H\nint m;\n#endif\n",
        )
        .unwrap();
        let opts = PreprocessOptions::new()
            .with_include(dir.path.clone())
            .with_record_conditionals(true);
        let result = preprocess_file(&dir.join("main.c"), &opts).unwrap();
        let guard_of = |name: &str| -> Vec<bool> {
            result
                .conditionals
                .iter()
                .filter(|c| c.file.ends_with(name))
                .map(|c| c.include_guard)
                .collect()
        };
        assert_eq!(guard_of("guarded.h"), vec![true]);
        assert_eq!(guard_of("not_defined.h"), vec![true]);
        assert_eq!(guard_of("default.h"), vec![false]);
        assert_eq!(guard_of("late.h"), vec![false]);
        assert_eq!(guard_of("other_define.h"), vec![false]);
        // The TU's own trailing #ifndef/#define/#endif: preceded by tokens.
        assert_eq!(guard_of("main.c"), vec![false]);
        // A guard's chain is recorded in its own file, at the header's depth.
        let guarded = result
            .conditionals
            .iter()
            .find(|c| c.file.ends_with("guarded.h"))
            .unwrap();
        assert_eq!(guarded.depth, 0);
        assert_eq!(guarded.line(), 2);
        assert_eq!(guarded.arms[0].outcome, ArmOutcome::Taken);
        assert_eq!(reads_of(&guarded.arms[0]), vec![("GUARDED_H", Some(false))]);
    }

    /// A guard candidate is per file: a header included inside the guard
    /// chain must not consume the includer's pending `#define`, and its own
    /// guard is judged on its own tokens.
    #[test]
    fn include_guard_detection_is_per_file() {
        let dir = unique_tmp_dir("cond_guard_nested");
        fs::write(
            dir.join("inner.h"),
            "#ifndef INNER_H\n#define INNER_H\nint i;\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("outer.h"),
            "#ifndef OUTER_H\n#include \"inner.h\"\n#define OUTER_H\nint o;\n#endif\n",
        )
        .unwrap();
        fs::write(dir.join("main.c"), "#include \"outer.h\"\n").unwrap();
        let opts = PreprocessOptions::new()
            .with_include(dir.path.clone())
            .with_record_conditionals(true);
        let result = preprocess_file(&dir.join("main.c"), &opts).unwrap();
        let outer = result
            .conditionals
            .iter()
            .find(|c| c.file.ends_with("outer.h"))
            .unwrap();
        let inner = result
            .conditionals
            .iter()
            .find(|c| c.file.ends_with("inner.h"))
            .unwrap();
        // outer.h's next directive after `#ifndef OUTER_H` is an #include,
        // not the #define, so it is not the canonical guard shape.
        assert!(!outer.include_guard);
        assert!(inner.include_guard);
        assert_eq!(inner.depth, 0, "includer frames do not count as depth");
    }

    #[test]
    fn chains_in_included_headers_carry_the_header_path() {
        let dir = unique_tmp_dir("cond_header_path");
        fs::write(
            dir.join("cfg.h"),
            "#ifdef USE_FAST\nint fast;\n#else\nint slow;\n#endif\n",
        )
        .unwrap();
        fs::write(
            dir.join("a.c"),
            "#define USE_FAST 1\n#include \"cfg.h\"\n#ifdef USE_FAST\nint a;\n#endif\n",
        )
        .unwrap();
        let opts = PreprocessOptions::new()
            .with_include(dir.path.clone())
            .with_record_conditionals(true);
        let result = preprocess_file(&dir.join("a.c"), &opts).unwrap();
        assert_eq!(result.conditionals.len(), 2);
        let cfg = &result.conditionals[0];
        assert!(cfg.file.ends_with("cfg.h"), "{:?}", cfg.file);
        assert_eq!(cfg.arms[0].outcome, ArmOutcome::Taken);
        assert_eq!(reads_of(&cfg.arms[0]), vec![("USE_FAST", Some(true))]);
        assert_eq!(cfg.arms[1].outcome, ArmOutcome::Skipped);
        assert!(result.conditionals[1].file.ends_with("a.c"));
    }

    /// The same header reached as `src/../common/cfg.h` from one includer
    /// and as `common/cfg.h` through an include dir is one file; its chains
    /// have to merge across runs, so the record carries the canonical path.
    #[test]
    fn chain_file_is_canonical_however_the_include_spelled_it() {
        let dir = unique_tmp_dir("cond_canonical");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("common")).unwrap();
        fs::write(dir.join("common/cfg.h"), "#ifdef A\nint a;\n#endif\n").unwrap();
        fs::write(dir.join("src/a.c"), "#include \"../common/cfg.h\"\n").unwrap();
        fs::write(dir.join("src/b.c"), "#include \"cfg.h\"\n").unwrap();
        let opts = PreprocessOptions::new()
            .with_include(dir.join("common"))
            .with_record_conditionals(true);
        let via_parent = preprocess_file(&dir.join("src/a.c"), &opts).unwrap();
        let via_dir = preprocess_file(&dir.join("src/b.c"), &opts).unwrap();
        let expected = trace_ir::canonicalize(&dir.join("common/cfg.h"));
        assert_eq!(via_parent.conditionals[0].file, expected);
        assert_eq!(via_dir.conditionals[0].file, expected);
        assert!(!via_parent.conditionals[0]
            .file
            .components()
            .any(|c| c == std::path::Component::ParentDir));
    }

    /// A line marker or `#!` line inside a skipped group is not a directive
    /// the group needs to understand; it must neither abort the file nor
    /// stretch the arm to the end of it.
    #[test]
    fn non_identifier_directive_in_skipped_group_is_ignored() {
        let src = "#if 0\n# 1 \"legacy.c\"\n#!/bin/sh\n#endif\nint kept;\nint also;\n";
        let result = preprocess_string(
            src,
            Path::new("test.c"),
            &PreprocessOptions::new().with_record_conditionals(true),
        );
        assert!(result.output.contains("kept"), "{:?}", result.output);
        assert!(result.output.contains("also"), "{:?}", result.output);
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("preprocess stopped")),
            "{:?}",
            result.diagnostics
        );
        let chain = &result.conditionals[0];
        assert!(chain.terminated);
        assert_eq!(chain.arms[0].end_line, 4);
        assert_eq!(chain.arms[0].body_lines(), 2);
        // In an active group the same line is still an error.
        let active = preprocess_string(
            "# 1 \"legacy.c\"\nint x;\n",
            Path::new("test.c"),
            &PreprocessOptions::new(),
        );
        assert!(active
            .diagnostics
            .iter()
            .any(|d| d.message.contains("expected directive name")));
    }

    /// `#ifndef X_H / #define X_H / … / #else / … / #endif` has an
    /// alternative; a guard does not.
    #[test]
    fn guard_shaped_chain_with_an_else_arm_is_not_a_guard() {
        let src = "#ifndef X_H\n#define X_H\nint x;\n#else\nint y;\n#endif\n";
        let chains = record(src, PreprocessOptions::new());
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].arms.len(), 2);
        assert!(!chains[0].include_guard);
        let plain = record(
            "#ifndef X_H\n#define X_H\nint x;\n#endif\n",
            PreprocessOptions::new(),
        );
        assert!(plain[0].include_guard);
    }

    /// `and` / `not` in a condition are operators, not names the
    /// configuration failed to bind — unless something did bind them.
    #[test]
    fn alternative_operator_spellings_are_not_reads() {
        let src = "#if defined(A) and not defined(B)\nint x;\n#elif C or D\nint y;\n#endif\n";
        let chains = record(src, PreprocessOptions::new().with_define("A", "1"));
        assert_eq!(
            reads_of(&chains[0].arms[0]),
            vec![("A", Some(true)), ("B", Some(false))]
        );
        assert!(chains[0].arms[1].evaluated);
        assert_eq!(
            reads_of(&chains[0].arms[1]),
            vec![("C", Some(false)), ("D", Some(false))]
        );
        // Unevaluated arm: the spelled identifiers, minus the operators.
        let nested = "#if 0\n#if X and Y\nint n;\n#endif\n#endif\n";
        let chains = record(nested, PreprocessOptions::new());
        assert_eq!(reads_of(&chains[1].arms[0]), vec![("X", None), ("Y", None)]);
        // A macro actually named `and` is a name.
        let bound = record(
            "#if and\nint z;\n#endif\n",
            PreprocessOptions::new().with_define("and", "1"),
        );
        assert_eq!(reads_of(&bound[0].arms[0]), vec![("and", Some(true))]);
    }

    /// `__cplusplus` is predefined for a unit lexed as C++ and unbound in a
    /// C unit (#70): `#ifdef`, `defined()`, `#if` arithmetic and text
    /// position all see it.
    #[test]
    fn cplusplus_predefined_for_cpp_units_only() {
        let src = "#ifdef __cplusplus\nint ifdef_arm;\n#endif\n\
                   #if defined(__cplusplus)\nint defined_arm;\n#endif\n\
                   #if __cplusplus >= 201103L\nint cxx11_arm;\n#endif\n\
                   long v = __cplusplus;\n";
        let cpp = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new()).output;
        assert!(cpp.contains("ifdef_arm"), "{cpp}");
        assert!(cpp.contains("defined_arm"), "{cpp}");
        assert!(cpp.contains("cxx11_arm"), "{cpp}");
        assert!(cpp.contains("v= 201703L ;"), "{cpp}");

        let c = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new()).output;
        assert!(!c.contains("ifdef_arm"), "{c}");
        assert!(!c.contains("defined_arm"), "{c}");
        assert!(!c.contains("cxx11_arm"), "{c}");
        assert!(c.contains("v= __cplusplus ;"), "{c}");

        // The language option decides, not the extension.
        let forced = preprocess_string(
            src,
            Path::new("t.c"),
            &PreprocessOptions::new().with_language(Language::Cpp),
        )
        .output;
        assert!(forced.contains("cxx11_arm"), "{forced}");
    }

    /// A command-line `-D __cplusplus=…` beats the predefined value, so a
    /// tree that pins its own standard level is honoured.
    #[test]
    fn cli_define_overrides_predefined_cplusplus() {
        let src = "#if __cplusplus >= 201103L\nint cxx11_arm;\n#else\nint cxx98_arm;\n#endif\n";
        let out = preprocess_string(
            src,
            Path::new("t.cpp"),
            &PreprocessOptions::new().with_define("__cplusplus", "199711L"),
        )
        .output;
        assert!(out.contains("cxx98_arm"), "{out}");
        assert!(!out.contains("cxx11_arm"), "{out}");
    }

    /// `__STDC__` is predefined for both languages (g++ defines it too);
    /// `__STDC_VERSION__` only for C.
    #[test]
    fn stdc_predefined_per_language() {
        let src = "#ifdef __STDC__\nint stdc_arm;\n#endif\n\
                   #if __STDC_VERSION__ >= 201112L\nint c11_arm;\n#endif\n";
        let c = preprocess_string(src, Path::new("t.c"), &PreprocessOptions::new()).output;
        assert!(c.contains("stdc_arm"), "{c}");
        assert!(c.contains("c11_arm"), "{c}");
        let cpp = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new()).output;
        assert!(cpp.contains("stdc_arm"), "{cpp}");
        assert!(!cpp.contains("c11_arm"), "{cpp}");
    }

    /// The warm pass seeds each header's table from
    /// `macro_table_from_defines`, and a translation unit that inherits a
    /// shared table takes the predefines from it: both routes must agree
    /// with the plain one.
    #[test]
    fn predefined_macros_seed_the_shared_table() {
        let none = indexmap::IndexMap::new();
        let cpp_table = macro_table_from_defines(&none, Language::Cpp);
        assert!(cpp_table.contains_key("__cplusplus"));
        assert!(cpp_table.contains_key("__STDC__"));
        assert!(!cpp_table.contains_key("__STDC_VERSION__"));
        let c_table = macro_table_from_defines(&none, Language::C);
        assert!(!c_table.contains_key("__cplusplus"));
        assert!(c_table.contains_key("__STDC_VERSION__"));

        let pinned = indexmap::IndexMap::from([("__cplusplus".to_string(), "199711L".to_string())]);
        let pinned_table = macro_table_from_defines(&pinned, Language::Cpp);
        let src = "#if __cplusplus >= 201103L\nint cxx11_arm;\n#else\nint cxx98_arm;\n#endif\n";
        let out = preprocess_string(
            src,
            Path::new("t.cpp"),
            &PreprocessOptions::new()
                .with_shared_macros(Arc::new(RwLock::new(pinned_table)))
                .with_define("__cplusplus", "199711L"),
        )
        .output;
        assert!(out.contains("cxx98_arm"), "{out}");

        let out = preprocess_string(
            src,
            Path::new("t.cpp"),
            &PreprocessOptions::new().with_shared_macros(Arc::new(RwLock::new(cpp_table))),
        )
        .output;
        assert!(out.contains("cxx11_arm"), "{out}");
    }

    /// A predefine is an ordinary definition, not a builtin fallback, so
    /// source may `#undef` it and the conditionals that follow see it gone.
    #[test]
    fn undef_of_a_predefined_macro_takes_effect() {
        let src = "#ifdef __cplusplus\nint before;\n#endif\n\
                   #undef __cplusplus\n\
                   #ifdef __cplusplus\nint after;\n#else\nint gone;\n#endif\n";
        let out = preprocess_string(src, Path::new("t.cpp"), &PreprocessOptions::new()).output;
        assert!(out.contains("before"), "{out}");
        assert!(out.contains("gone"), "{out}");
        assert!(!out.contains("after"), "{out}");
    }

    /// A `-D` beats the predefined value on the shared-table path too. The
    /// indexer seeds that table through `macro_table_from_defines`, which
    /// already applies the CLI defines over the predefines, but a caller
    /// that hands over a table those never reached must get the same
    /// precedence rather than the language's own value.
    #[test]
    fn cli_define_beats_predefined_cplusplus_with_a_shared_table() {
        let src = "#if __cplusplus >= 201103L\nint cxx11_arm;\n#else\nint cxx98_arm;\n#endif\n";
        let out = preprocess_string(
            src,
            Path::new("t.cpp"),
            &PreprocessOptions::new()
                .with_shared_macros(crate::new_shared_macro_table())
                .with_define("__cplusplus", "199711L"),
        )
        .output;
        assert!(out.contains("cxx98_arm"), "{out}");
        assert!(!out.contains("cxx11_arm"), "{out}");
    }

    /// A header's read of `__cplusplus` is a fingerprint dependency like any
    /// other macro read: bound in the C++ entry, unbound in the C entry.
    #[test]
    fn predefined_cplusplus_is_a_fingerprint_dependency() {
        let dir = unique_tmp_dir("cplusplus_fingerprint");
        fs::write(
            dir.join("lang.h"),
            "#ifdef __cplusplus\nint cxx_decl;\n#else\nint c_decl;\n#endif\n",
        )
        .unwrap();
        let cache: ExpansionCache = Arc::new(RwLock::new(HashMap::new()));
        let run = |name: &str| {
            let path = dir.join(name);
            fs::write(&path, "#include \"lang.h\"\n").unwrap();
            let opts = PreprocessOptions::new()
                .with_include_expansion_cache(Arc::clone(&cache))
                .with_include(dir.path.clone());
            preprocess_file(&path, &opts).unwrap().output
        };
        assert!(run("t.cpp").contains("cxx_decl"));
        assert!(run("t.c").contains("c_decl"));
        let cpp = sole_variant(&cache, dir.join("lang.h"), Language::Cpp).expect("C++ entry");
        assert!(
            cpp.deps.defined.iter().any(|(n, _)| &**n == "__cplusplus"),
            "{:?}",
            cpp.deps
        );
        let c = sole_variant(&cache, dir.join("lang.h"), Language::C).expect("C entry");
        assert!(c.deps.undefined.contains("__cplusplus"), "{:?}", c.deps);
    }
}
