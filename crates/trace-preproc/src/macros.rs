use crate::{Language, Token, TokenKind};
use indexmap::IndexMap;
use rustc_hash::FxHashSet;
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, RwLock};

const NUM_MACRO_NAME_SHARDS: usize = 128;
const SHARD_MASK: usize = NUM_MACRO_NAME_SHARDS - 1;
const LOCAL_CACHE_SIZE: usize = 512;
const LOCAL_CACHE_MASK: usize = LOCAL_CACHE_SIZE - 1;

type LocalCacheSlot = Option<(u64, Arc<str>)>;
type LocalCache = [LocalCacheSlot; LOCAL_CACHE_SIZE];

thread_local! {
    static LOCAL_CACHE: RefCell<LocalCache> =
        RefCell::new(std::array::from_fn(|_| None));
}

/// Process-global deduplication table for macro names.
///
/// Preprocessing thousands of translation units repeatedly allocates identical
/// macro names (`Arc::from(name)`) for include guards, definitions, and dependencies.
/// This interner ensures macro names share a single canonical `Arc<str>` allocation
/// across threads.
pub struct MacroNameInterner {
    shards: [RwLock<FxHashSet<Arc<str>>>; NUM_MACRO_NAME_SHARDS],
}

impl Default for MacroNameInterner {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| {
                RwLock::new(FxHashSet::with_capacity_and_hasher(16, Default::default()))
            }),
        }
    }
}

impl MacroNameInterner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Process-global macro name interner instance.
    #[must_use]
    pub fn global() -> &'static Self {
        static GLOBAL: LazyLock<MacroNameInterner> = LazyLock::new(MacroNameInterner::new);
        &GLOBAL
    }

    /// Intern a macro name, returning a shared deduplicated `Arc<str>`.
    pub fn intern(&self, name: &str) -> Arc<str> {
        let mut hasher = rustc_hash::FxHasher::default();
        name.hash(&mut hasher);
        self.intern_with_hash(name, hasher.finish())
    }

    /// Intern a macro name with a precomputed hash, returning a shared deduplicated `Arc<str>`.
    pub fn intern_with_hash(&self, name: &str, hash: u64) -> Arc<str> {
        let shard_idx = (hash as usize) & SHARD_MASK;
        let shard = &self.shards[shard_idx];

        if let Ok(guard) = shard.read() {
            if let Some(existing) = guard.get(name) {
                return Arc::clone(existing);
            }
        }
        let mut guard = shard.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.get(name) {
            return Arc::clone(existing);
        }
        let interned: Arc<str> = Arc::from(name);
        guard.insert(Arc::clone(&interned));
        interned
    }

    /// Number of distinct macro names interned across all shards.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.read().map_or(0, |g| g.len()))
            .sum()
    }

    /// Whether any macro names are interned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for MacroNameInterner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacroNameInterner")
            .field("len", &self.len())
            .finish()
    }
}

/// Intern a macro name into the process-global dedup table.
///
/// Lookups first query a thread-local direct-mapped L1 cache (512 slots) to avoid
/// cross-thread lock contention and cache-line invalidation on frequent macro names.
/// Misses fall back to the 128-shard global deduplication table.
pub fn intern_macro_name(name: &str) -> Arc<str> {
    let mut hasher = rustc_hash::FxHasher::default();
    name.hash(&mut hasher);
    let hash = hasher.finish();

    LOCAL_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        let idx = (hash as usize) & LOCAL_CACHE_MASK;
        if let Some((entry_hash, entry_arc)) = &cache[idx] {
            if *entry_hash == hash && entry_arc.as_ref() == name {
                return Arc::clone(entry_arc);
            }
        }
        let interned = MacroNameInterner::global().intern_with_hash(name, hash);
        cache[idx] = Some((hash, Arc::clone(&interned)));
        interned
    })
}

/// Immutable definition storage is shared by macro tables and cached directive logs.
#[derive(Debug, Clone)]
pub enum MacroDef {
    Object {
        replacement: Arc<[Token]>,
    },
    Function {
        params: Arc<[String]>,
        replacement: Arc<[Token]>,
        /// Invariant: when true, the LAST entry of `params` is the variadic
        /// collector — `parse_macro_param_list` pushes `"__VA_ARGS__"` for the
        /// anonymous `...` form. A hand-built variadic def (builtins, tests)
        /// must uphold this or the last named parameter will swallow every
        /// argument; `substitute_macro` debug_asserts it.
        variadic: bool,
    },
    /// Last-resort parser recovery for a gMock declaration macro
    /// (`MOCK_METHOD`, `MOCK_METHODn`, `MOCK_CONST_METHODn`, `…_T`): the
    /// invocation becomes the member prototype it declares. A replacement
    /// list cannot express this — the legacy forms carry the whole
    /// signature in one argument and the modern form parenthesizes
    /// comma-containing return types — so the preprocessor expands it in
    /// code (`expand_gmock_method`). Only the builtin fallback table
    /// creates one.
    GmockMethod,
}

/// One executed macro directive, in program order. Cached include entries
/// record these so replay reproduces the header's effects exactly — a
/// state diff cannot represent a no-op `#undef` (name absent at capture,
/// present in a later consumer) or `#undef X` + `#define X new` of a name
/// that existed at both capture boundaries.
#[derive(Debug, Clone)]
pub enum MacroOp {
    Define(Arc<str>, Arc<MacroDef>),
    Undef(Arc<str>),
}

pub type MacroTable = IndexMap<Arc<str>, Arc<MacroDef>>;
pub type SharedMacroTable = Arc<RwLock<MacroTable>>;

#[must_use]
pub fn new_shared_macro_table() -> SharedMacroTable {
    Arc::new(RwLock::new(MacroTable::new()))
}

/// The macros the language itself predefines (C11 6.10.8.1, [cpp.predefined]),
/// as `(name, replacement)`. Every macro table is seeded with these, and a
/// command-line `-D` of the same name outranks them wherever both apply, so
/// a tree that pins its own standard level keeps it (#70).
///
/// Only the names the standards require and that real code tests: the
/// eval corpora read `__cplusplus` from 930 conditionals, and until this
/// existed every one of them took the C arm in a C++ unit. `__cplusplus` is
/// C++17, what the OpenHarmony clang defaults to; the corpora compare it
/// against `201103L` only. `__STDC__` is `1` in both languages (g++ defines
/// it too); `__STDC_VERSION__` is C17 and, like the real compilers, absent
/// from a C++ unit. No compiler is claimed: `__GNUC__` / `__clang__` stay
/// unbound, since either would switch on vendor extensions the parser does
/// not have.
#[must_use]
pub fn predefined_macros(language: Language) -> &'static [(&'static str, &'static str)] {
    match language {
        Language::C => &[("__STDC__", "1"), ("__STDC_VERSION__", "201710L")],
        Language::Cpp => &[("__STDC__", "1"), ("__cplusplus", "201703L")],
    }
}

/// Object-like macros for the language's [`predefined_macros`] and the
/// command-line `-D` definitions, in that order, their bodies lexed as
/// `language` (see [`Language`] for what differs).
#[must_use]
pub fn macro_table_from_defines(
    defines: &indexmap::IndexMap<String, String>,
    language: Language,
) -> MacroTable {
    let mut table = MacroTable::new();
    let predefined = predefined_macros(language)
        .iter()
        .map(|(name, val)| (name.to_string(), val.to_string()));
    let cli = defines
        .iter()
        .map(|(name, val)| (name.clone(), val.clone()));
    for (name, val) in predefined.chain(cli) {
        table.insert(
            intern_macro_name(&name),
            Arc::new(MacroDef::Object {
                replacement: lex_macro_body(&val, language).into(),
            }),
        );
    }
    table
}

/// Tokenize a macro replacement list from source text (Eof stripped).
pub(crate) fn lex_macro_body(src: &str, language: Language) -> Vec<Token> {
    crate::Lexer::new(src, language)
        .tokenize()
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Eof))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_definition_clones_share_token_storage() {
        let def = MacroDef::Function {
            params: vec!["x".to_string()].into(),
            replacement: lex_macro_body("x + x", Language::C).into(),
            variadic: false,
        };
        let MacroDef::Function {
            params,
            replacement,
            ..
        } = &def
        else {
            unreachable!()
        };
        let MacroDef::Function {
            params: cloned_params,
            replacement: cloned_tokens,
            ..
        } = def.clone()
        else {
            unreachable!()
        };
        assert!(Arc::ptr_eq(params, &cloned_params));
        assert!(Arc::ptr_eq(replacement, &cloned_tokens));
    }

    #[test]
    fn macro_op_size() {
        assert_eq!(std::mem::size_of::<MacroOp>(), 24);
    }

    #[test]
    fn macro_name_interner_deduplicates_strings() {
        let interner = MacroNameInterner::new();
        let a = interner.intern("MY_MACRO");
        let b = interner.intern("MY_MACRO");
        assert_eq!(a.as_ref(), "MY_MACRO");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(interner.len(), 1);

        let c = interner.intern("OTHER_MACRO");
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn global_macro_name_interner_shares_pointers() {
        let a = intern_macro_name("__TEST_MACRO__");
        let b = intern_macro_name("__TEST_MACRO__");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn interner_multi_threaded_pointer_consistency() {
        use std::sync::Barrier;
        use std::thread;

        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let b = Arc::clone(&barrier);
                thread::spawn(move || {
                    b.wait();
                    let mut pointers = Vec::new();
                    for i in 0..100 {
                        let name = format!("CONCURRENT_MACRO_{}", i % 20);
                        pointers.push((i % 20, intern_macro_name(&name)));
                    }
                    pointers
                })
            })
            .collect();

        let all_results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let reference = &all_results[0];
        for other in &all_results[1..] {
            for (ref_entry, other_entry) in reference.iter().zip(other.iter()) {
                assert_eq!(ref_entry.0, other_entry.0);
                assert_eq!(ref_entry.1, other_entry.1);
                assert!(Arc::ptr_eq(&ref_entry.1, &other_entry.1));
            }
        }
    }

    #[test]
    fn interner_l1_cache_eviction_and_fallback() {
        // Intern more than 512 names to force L1 direct-mapped cache collisions/evictions.
        let mut first_pass = Vec::with_capacity(1000);
        for i in 0..1000 {
            let name = format!("EV_MACRO_{i}");
            first_pass.push(intern_macro_name(&name));
        }

        // Re-query: evicted entries must still return the exact same canonical Arc pointer.
        for (i, original) in first_pass.iter().enumerate() {
            let name = format!("EV_MACRO_{i}");
            let re_queried = intern_macro_name(&name);
            assert!(Arc::ptr_eq(original, &re_queried));
        }
    }

    #[test]
    fn interner_throughput() {
        let names = [
            "FOO",
            "BAR",
            "BAZ",
            "QUX",
            "_STDIO_H",
            "__cplusplus",
            "NULL",
            "NDEBUG",
        ];
        let start = std::time::Instant::now();
        for i in 0..100_000 {
            let _ = intern_macro_name(names[i % names.len()]);
        }
        let elapsed = start.elapsed();
        // 100,000 lookups through L1 cache should complete in well under 50ms (typically ~1-3ms)
        assert!(elapsed.as_millis() < 500, "100k lookups took {:?}", elapsed);
    }
}
