//! Deferred publishing to the include-expansion cache. Why the indexer's
//! discovery pass needs it, and the rule [`ExpansionJournal::commit`]
//! applies, are in `docs/PREPROCESSOR.md` ("Parallel discovery").

use crate::macros::{MacroDef, MacroTable};
use crate::options::admits_variant;
use crate::{
    ExpansionCache, ExpansionKey, ExpansionVariants, IncludeExpansion, Language, MacroFingerprint,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// Tag bit of a provisional variant handle, which names an expansion by
/// [`IncludeExpansion::id`] until it has an index.
const PROVISIONAL: usize = 1 << (usize::BITS - 1);

static NEXT_EXPANSION_ID: AtomicUsize = AtomicUsize::new(1);

/// A fresh [`IncludeExpansion::id`].
pub(crate) fn next_expansion_id() -> usize {
    NEXT_EXPANSION_ID.fetch_add(1, Ordering::Relaxed) & !PROVISIONAL
}

fn provisional(id: usize) -> usize {
    PROVISIONAL | id
}

fn provisional_id(handle: usize) -> Option<usize> {
    (handle & PROVISIONAL != 0).then_some(handle & !PROVISIONAL)
}

/// A macro's definition and whether it is a builtin fallback.
pub(crate) type Binding = (Arc<MacroDef>, bool);

type StoredVariants = FxHashMap<ExpansionKey, ExpansionVariants>;

/// What a run under `PreprocessOptions::defer_expansion_publish` read from
/// the shared [`ExpansionCache`] and would have published to it: per header,
/// the variants it was shown and each lookup it made (at which point of its
/// macro history, and what it took), its kept-back entries, and that
/// history. A variant that was not stored when the run took it is recorded
/// by a provisional handle instead of an index.
#[derive(Debug, Clone)]
pub struct ExpansionJournal {
    seen: FxHashMap<ExpansionKey, Seen>,
    pending: FxHashMap<ExpansionKey, ExpansionVariants>,
    cap: usize,
    language: Language,
    history: MacroHistory,
}

/// One header's variant list as the run was shown it, and its lookups there.
#[derive(Debug, Clone)]
struct Seen {
    view: Vec<Shown>,
    lookups: Vec<Lookup>,
}

#[derive(Debug, Clone)]
struct Shown {
    id: usize,
    signature: u64,
    deps: Arc<MacroFingerprint>,
    source: Source,
}

impl Shown {
    fn of(entry: &IncludeExpansion, source: Source) -> Self {
        Self {
            id: entry.id,
            signature: entry.signature,
            deps: Arc::clone(&entry.deps),
            source,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Source {
    /// In the shared cache, at this index.
    Stored(usize),
    /// In the pending entries of `expansion_journals_ahead[journal]`.
    Ahead { journal: usize, at: usize },
}

#[derive(Debug, Clone, Copy)]
struct Lookup {
    /// The run's macro history point when it looked.
    point: usize,
    /// The position in the view it took; `None` for its own entry or none.
    took: Option<usize>,
}

impl Seen {
    /// The run's view of `key`, built on its first look: the stored list,
    /// then what the journals ahead would append that it does not already
    /// hold (one may have committed since).
    fn of<'m>(
        seen: &'m mut FxHashMap<ExpansionKey, Seen>,
        key: &ExpansionKey,
        cache: &ExpansionCache,
        ahead: &[Arc<ExpansionJournal>],
    ) -> &'m mut Seen {
        seen.entry(key.clone()).or_insert_with(|| {
            let mut view: Vec<Shown> = Vec::new();
            if let Some(list) = cache
                .read()
                .ok()
                .as_deref()
                .and_then(|stored| stored.get(key))
            {
                view.extend(
                    list.iter()
                        .enumerate()
                        .map(|(at, entry)| Shown::of(entry, Source::Stored(at))),
                );
            }
            for (journal, entries) in ahead
                .iter()
                .enumerate()
                .filter_map(|(i, j)| Some((i, j.pending.get(key)?)))
            {
                for (at, entry) in entries.iter().enumerate() {
                    if !view.iter().any(|v| v.id == entry.id) {
                        view.push(Shown::of(entry, Source::Ahead { journal, at }));
                    }
                }
            }
            Seen {
                view,
                lookups: Vec::new(),
            }
        })
    }

    fn shows(&self, id: usize) -> bool {
        self.view.iter().any(|v| v.id == id)
    }
}

/// A run's macro table at every point of its history: the table it ended
/// with, and before each change the binding the name had.
#[derive(Debug, Clone, Default)]
struct MacroHistory {
    undo: Vec<(Arc<str>, Option<Binding>)>,
    last: MacroTable,
    last_fallbacks: FxHashSet<String>,
    /// Name → its positions in `undo`, built on first need.
    changes: OnceLock<FxHashMap<Arc<str>, Vec<usize>>>,
}

impl MacroHistory {
    fn binding_at(&self, name: &str, point: usize) -> Option<(&MacroDef, bool)> {
        let changes = self.changes.get_or_init(|| {
            let mut by_name: FxHashMap<Arc<str>, Vec<usize>> = FxHashMap::default();
            for (at, (name, _)) in self.undo.iter().enumerate() {
                by_name.entry(Arc::clone(name)).or_default().push(at);
            }
            by_name
        });
        // The first change at or after `point` saw the binding `point` had.
        match changes
            .get(name)
            .and_then(|ats| ats.get(ats.partition_point(|at| *at < point)))
        {
            Some(at) => self.undo[*at]
                .1
                .as_ref()
                .map(|(def, fallback)| (def.as_ref(), *fallback)),
            None => self
                .last
                .get(name)
                .map(|def| (def.as_ref(), self.last_fallbacks.contains(name))),
        }
    }

    /// Whether the environment at `point` satisfies `deps`.
    fn satisfies(&self, deps: &MacroFingerprint, point: usize) -> bool {
        deps.satisfied_by(
            |name| {
                self.binding_at(name, point)
                    .map(|(def, fallback)| crate::preprocessor::hash_macro_binding(def, fallback))
            },
            |names| {
                names
                    .iter()
                    .any(|name| self.binding_at(name, point).is_some())
            },
        )
    }
}

impl ExpansionJournal {
    pub(crate) fn new(cap: usize, language: Language) -> Self {
        Self {
            seen: FxHashMap::default(),
            pending: FxHashMap::default(),
            cap,
            language,
            history: MacroHistory::default(),
        }
    }

    /// Record that `name` changed, having been bound to `before`.
    pub(crate) fn record_macro_change(&mut self, name: &str, before: Option<Binding>) {
        self.history.undo.push((Arc::from(name), before));
    }

    /// Record the table the run ended with.
    pub(crate) fn finish(&mut self, table: MacroTable, fallbacks: FxHashSet<String>) {
        self.history.last = table;
        self.history.last_fallbacks = fallbacks;
    }

    /// The first variant of `key` — shown, then kept back — that `matches`
    /// accepts, with the handle to record for it.
    pub(crate) fn lookup(
        &mut self,
        key: &ExpansionKey,
        cache: &ExpansionCache,
        ahead: &[Arc<ExpansionJournal>],
        matches: impl FnMut(&MacroFingerprint) -> bool,
    ) -> Option<(usize, IncludeExpansion)> {
        let point = self.history.undo.len();
        let seen = Seen::of(&mut self.seen, key, cache, ahead);
        let own = self.pending.get(key).map_or(&[][..], Vec::as_slice);
        let shown = seen.view.len();
        let at = seen
            .view
            .iter()
            .map(|v| v.deps.as_ref())
            .chain(own.iter().map(|e| e.deps.as_ref()))
            .position(matches);
        seen.lookups.push(Lookup {
            point,
            took: at.filter(|at| *at < shown),
        });
        let at = at?;
        let Some(variant) = seen.view.get(at) else {
            let entry = own[at - shown].clone();
            return Some((provisional(entry.id), entry));
        };
        match variant.source {
            Source::Stored(index) => {
                Some((index, cache.read().ok()?.get(key)?.get(index)?.clone()))
            }
            Source::Ahead { journal, at: kept } => Some((
                provisional(variant.id),
                ahead.get(journal)?.pending.get(key)?.get(kept)?.clone(),
            )),
        }
    }

    /// Keep `entry` back as a variant of `key`, if the list the run sees
    /// admits it.
    /// Returns the handle a record of `entry` resolves through at commit:
    /// its own, or that of the variant already holding its signature; `None`
    /// when the list is full.
    pub(crate) fn publish(
        &mut self,
        key: ExpansionKey,
        cache: &ExpansionCache,
        ahead: &[Arc<ExpansionJournal>],
        entry: IncludeExpansion,
    ) -> Option<usize> {
        let seen = Seen::of(&mut self.seen, &key, cache, ahead);
        let own = self.pending.entry(key).or_default();
        let signatures = seen
            .view
            .iter()
            .map(|v| v.signature)
            .chain(own.iter().map(|e| e.signature));
        if admits_variant(signatures, self.cap, entry.signature) {
            let handle = provisional(entry.id);
            own.push(entry);
            return Some(handle);
        }
        if let Some(variant) = seen.view.iter().find(|v| v.signature == entry.signature) {
            return Some(match variant.source {
                Source::Stored(index) => index,
                Source::Ahead { .. } => provisional(variant.id),
            });
        }
        own.iter()
            .find(|e| e.signature == entry.signature)
            .map(|e| provisional(e.id))
    }

    /// Whether the cache already rules this run out. A run not ruled out can
    /// still fail to commit.
    #[must_use]
    pub fn is_contradicted(&self, cache: &ExpansionCache) -> bool {
        cache
            .read()
            .map_or(true, |stored| !self.agrees(&stored, false))
    }

    /// Whether this run, committed before `later`, rules `later` out. Errs
    /// towards yes.
    #[must_use]
    pub fn rules_out(&self, later: &ExpansionJournal) -> bool {
        self.clashes(later, |_, _| false)
    }

    /// [`Self::rules_out`] for this run taken as a guess at its unit's next
    /// run: an entry whose signature is stored by now is one the next run
    /// would take rather than publish, and is left out.
    #[must_use]
    pub fn likely_rules_out(&self, later: &ExpansionJournal, cache: &ExpansionCache) -> bool {
        let Ok(stored) = cache.read() else {
            return self.rules_out(later);
        };
        self.clashes(later, |key, entry| {
            stored
                .get(key)
                .is_some_and(|list| list.iter().any(|e| e.signature == entry.signature))
        })
    }

    /// Whether `later` was shown an entry this run would publish.
    #[must_use]
    pub fn feeds(&self, later: &ExpansionJournal) -> bool {
        self.pending.iter().any(|(key, entries)| {
            later
                .seen
                .get(key)
                .is_some_and(|seen| entries.iter().any(|e| seen.shows(e.id)))
        })
    }

    /// Check the run against `cache` and, when it is the run a publishing
    /// preprocess against the cache would be, publish its entries and turn
    /// the handles in `replayed` (the variants its result records) into
    /// indices. Returns false, changing nothing, otherwise.
    #[must_use]
    pub fn commit(&self, cache: &ExpansionCache, replayed: &mut [(PathBuf, usize)]) -> bool {
        let Ok(mut stored) = cache.write() else {
            return false;
        };
        if !self.agrees(&stored, true) {
            return false;
        }
        let mut indices: FxHashMap<usize, usize> = FxHashMap::default();
        for (key, entries) in &self.pending {
            let base = stored.get(key).map_or(0, Vec::len);
            indices.extend(entries.iter().enumerate().map(|(at, e)| (e.id, base + at)));
        }
        let mut resolve = |path: &PathBuf, handle: usize| -> Option<usize> {
            let Some(id) = provisional_id(handle) else {
                return Some(handle);
            };
            if let Some(index) = indices.get(&id) {
                return Some(*index);
            }
            let index = stored
                .get(&(path.clone(), self.language))?
                .iter()
                .position(|e| e.id == id)?;
            indices.insert(id, index);
            Some(index)
        };
        let Some(resolved) = replayed
            .iter()
            .map(|(path, handle)| resolve(path, *handle))
            .collect::<Option<Vec<usize>>>()
        else {
            return false;
        };
        let mut published = Vec::with_capacity(self.pending.len());
        for (key, entries) in &self.pending {
            let Some(entries) = entries
                .iter()
                .map(|entry| with_indices(entry, &mut resolve))
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            published.push((key.clone(), entries));
        }
        for ((_, handle), index) in replayed.iter_mut().zip(resolved) {
            *handle = index;
        }
        for (key, entries) in published {
            stored.entry(key).or_default().extend(entries);
        }
        true
    }

    /// Whether this run clashes with `later`, committed after it, leaving out
    /// its entries for which `skip` holds: one of them is one `later` was not
    /// shown and would have taken, duplicates an entry `later` publishes, or
    /// overflows the cap for it. Its entries land after every stored variant,
    /// so a lookup that took a stored one is safe.
    fn clashes(
        &self,
        later: &ExpansionJournal,
        skip: impl Fn(&ExpansionKey, &IncludeExpansion) -> bool,
    ) -> bool {
        self.pending.iter().any(|(key, entries)| {
            let Some(seen) = later.seen.get(key) else {
                return false;
            };
            let theirs = later.pending.get(key).map_or(&[][..], Vec::as_slice);
            let unseen: Vec<&IncludeExpansion> =
                entries.iter().filter(|e| !seen.shows(e.id)).collect();
            if !theirs.is_empty() && seen.view.len() + unseen.len() + theirs.len() > later.cap {
                return true;
            }
            let exposed: Vec<usize> = seen
                .lookups
                .iter()
                .filter(|l| {
                    l.took
                        .is_none_or(|at| matches!(seen.view[at].source, Source::Ahead { .. }))
                })
                .map(|l| l.point)
                .collect();
            unseen.into_iter().filter(|e| !skip(key, e)).any(|e| {
                exposed
                    .iter()
                    .any(|point| later.history.satisfies(&e.deps, *point))
                    || theirs.iter().any(|t| t.signature == e.signature)
            })
        })
    }

    /// Does `stored` leave this run as the run a publishing preprocess
    /// against it would be? With `complete` unset, a shown variant that is
    /// not stored yet is given the benefit of the doubt.
    fn agrees(&self, stored: &StoredVariants, complete: bool) -> bool {
        self.seen.iter().all(|(key, seen)| {
            let list = stored.get(key).map_or(&[][..], Vec::as_slice);
            let own = self.pending.get(key).map_or(&[][..], Vec::as_slice);
            if list.len() + own.len() > self.cap {
                return false;
            }
            if list.len() == seen.view.len()
                && list.iter().zip(&seen.view).all(|(e, v)| e.id == v.id)
            {
                return true;
            }
            // Where each shown variant is stored, in the order it was shown.
            let mut positions = Vec::with_capacity(seen.view.len());
            let mut last = None;
            for shown in &seen.view {
                let at = list.iter().position(|e| e.id == shown.id);
                match at {
                    Some(p) if last.is_some_and(|last| p <= last) => return false,
                    Some(p) => last = Some(p),
                    None if complete => return false,
                    None => {}
                }
                positions.push(at);
            }
            let unshown = || list.iter().enumerate().filter(|(_, e)| !seen.shows(e.id));
            let taken_instead = seen.lookups.iter().any(|lookup| {
                let before = match lookup.took {
                    None => list.len(),
                    Some(at) => match positions[at] {
                        Some(p) => p,
                        None => return false,
                    },
                };
                unshown()
                    .take_while(|(p, _)| *p < before)
                    .any(|(_, e)| self.history.satisfies(&e.deps, lookup.point))
            });
            !taken_instead
                && !unshown().any(|(_, e)| own.iter().any(|o| o.signature == e.signature))
        })
    }
}

/// `entry` with the handles among its nested variants turned into indices.
fn with_indices(
    entry: &IncludeExpansion,
    resolve: &mut impl FnMut(&PathBuf, usize) -> Option<usize>,
) -> Option<IncludeExpansion> {
    let mut entry = entry.clone();
    if entry
        .nested_variants
        .iter()
        .any(|(_, handle)| provisional_id(*handle).is_some())
    {
        let nested = entry
            .nested_variants
            .iter()
            .map(|(path, handle)| Some((path.clone(), resolve(path, *handle)?)))
            .collect::<Option<Vec<_>>>()?;
        entry.nested_variants = nested.into();
    }
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{preprocess_file, PreprocessOptions, PreprocessResult};
    use std::path::Path;
    use std::sync::RwLock;

    /// A header whose expansion reads `A`, so a unit defining it and one that
    /// does not need two variants.
    const HEADER: &str = "#ifdef A\nint with_a;\n#else\nint without_a;\n#endif\n";

    struct Tree {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Tree {
        fn new(units: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = trace_ir::canonicalize(dir.path());
            std::fs::write(root.join("h.h"), HEADER).unwrap();
            for (name, text) in units {
                std::fs::write(root.join(name), text).unwrap();
            }
            Self { _dir: dir, root }
        }

        fn header(&self) -> PathBuf {
            self.root.join("h.h")
        }

        fn run(&self, unit: &str, cache: &ExpansionCache, deferred: bool) -> PreprocessResult {
            self.run_after(unit, cache, deferred, Vec::new())
        }

        fn run_after(
            &self,
            unit: &str,
            cache: &ExpansionCache,
            deferred: bool,
            ahead: Vec<Arc<ExpansionJournal>>,
        ) -> PreprocessResult {
            let opts = PreprocessOptions::new()
                .with_include(self.root.clone())
                .with_include_expansion_cache(Arc::clone(cache))
                .with_inline_include_bodies(false)
                .with_deferred_expansion_publish(deferred)
                .with_expansion_journals_ahead(ahead);
            preprocess_file(&self.root.join(unit), &opts).unwrap()
        }
    }

    fn new_cache() -> ExpansionCache {
        Arc::new(RwLock::new(FxHashMap::default()))
    }

    /// The stored texts of `path`, in index order.
    fn stored(cache: &ExpansionCache, path: &Path) -> Vec<String> {
        cache
            .read()
            .unwrap()
            .get(&(path.to_path_buf(), Language::C))
            .map(|list| list.iter().map(|e| e.text.to_string()).collect())
            .unwrap_or_default()
    }

    /// Commit `result`'s journal, returning the variants its result records
    /// once they are indices.
    fn commit(result: &PreprocessResult, cache: &ExpansionCache) -> Option<Vec<(PathBuf, usize)>> {
        let journal = result.expansion_journal.as_ref().expect("a deferred run");
        let mut replayed = result.replayed_variants.clone();
        if !journal.commit(cache, &mut replayed) {
            return None;
        }
        replayed.sort();
        Some(replayed)
    }

    #[test]
    fn a_committed_run_leaves_what_the_writable_run_leaves() {
        // Included twice without a guard, so the second inclusion replays the
        // entry the first one published and the result records it.
        let tree = Tree::new(&[("u.c", "#include \"h.h\"\n#include \"h.h\"\n")]);
        let writable = new_cache();
        let serial = tree.run("u.c", &writable, false);
        assert!(serial.expansion_journal.is_none());

        let deferred = new_cache();
        let run = tree.run("u.c", &deferred, true);
        assert!(
            stored(&deferred, &tree.header()).is_empty(),
            "publishing is deferred"
        );
        assert_eq!(run.output, serial.output);
        assert_eq!(
            commit(&run, &deferred),
            Some(serial.replayed_variants.clone())
        );
        assert_eq!(
            stored(&deferred, &tree.header()),
            stored(&writable, &tree.header())
        );
    }

    #[test]
    fn a_variant_stored_ahead_that_the_run_would_take_blocks_its_commit() {
        let tree = Tree::new(&[
            ("first.c", "#include \"h.h\"\n"),
            ("second.c", "#include \"h.h\"\n"),
        ]);
        let cache = new_cache();
        let second = tree.run("second.c", &cache, true);
        // `first.c` commits ahead of it and publishes the environment
        // `second.c` presents, which a writable `second.c` would replay.
        let first = tree.run("first.c", &cache, true);
        assert!(commit(&first, &cache).is_some());
        assert_eq!(commit(&second, &cache), None);
        assert_eq!(
            stored(&cache, &tree.header()).len(),
            1,
            "nothing was written"
        );
    }

    #[test]
    fn a_variant_stored_ahead_that_the_run_would_not_take_moves_its_indices() {
        let tree = Tree::new(&[
            ("with_a.c", "#define A 1\n#include \"h.h\"\n"),
            ("plain.c", "#include \"h.h\"\n#include \"h.h\"\n"),
        ]);
        let serial = new_cache();
        tree.run("with_a.c", &serial, false);
        let serial_plain = tree.run("plain.c", &serial, false);
        assert_eq!(serial_plain.replayed_variants, vec![(tree.header(), 1)]);

        let cache = new_cache();
        let plain = tree.run("plain.c", &cache, true);
        let with_a = tree.run("with_a.c", &cache, true);
        assert!(commit(&with_a, &cache).is_some());
        assert_eq!(commit(&plain, &cache), Some(serial_plain.replayed_variants));
        assert_eq!(
            stored(&cache, &tree.header()),
            stored(&serial, &tree.header())
        );
    }

    #[test]
    fn each_lookup_is_judged_by_its_macro_state_and_spelling_provenance() {
        let tree = Tree::new(&[
            ("with_a.c", "#define A 1\n#include \"h.h\"\n"),
            // `A` is unbound at the include and bound when the run ends.
            ("defines_after.c", "#include \"h.h\"\n#define A 1\n"),
            // `A` is bound at the include and unbound when the run ends.
            (
                "undefines_after.c",
                "#define A 1\n#include \"h.h\"\n#undef A\n",
            ),
        ]);
        // Both runs commit. The first was unbound at lookup; the second had
        // the same macro text from another source file, whose observable
        // spelling provenance cannot reuse `with_a.c`'s cached expansion.
        for unit in ["defines_after.c", "undefines_after.c"] {
            let cache = new_cache();
            let run = tree.run(unit, &cache, true);
            let with_a = tree.run("with_a.c", &cache, true);
            assert!(commit(&with_a, &cache).is_some());
            assert!(commit(&run, &cache).is_some(), "{unit}");
        }
    }

    #[test]
    fn a_shared_binding_undefined_after_the_lookup_still_blocks() {
        let tree = Tree::new(&[
            ("a.h", "#define A 1\n"),
            ("with_a.c", "#include \"a.h\"\n#include \"h.h\"\n"),
            (
                "undefines_after.c",
                "#include \"a.h\"\n#include \"h.h\"\n#undef A\n",
            ),
        ]);
        let cache = new_cache();
        let run = tree.run("undefines_after.c", &cache, true);
        let with_a = tree.run("with_a.c", &cache, true);
        assert!(commit(&with_a, &cache).is_some());
        assert!(commit(&run, &cache).is_none());
    }

    #[test]
    fn entries_shown_from_a_journal_ahead_have_to_be_stored_first() {
        let tree = Tree::new(&[
            ("first.c", "#include \"h.h\"\n"),
            ("second.c", "#include \"h.h\"\n"),
        ]);
        let cache = new_cache();
        let first = tree.run("first.c", &cache, true);
        let first_journal = Arc::new(first.expansion_journal.clone().unwrap());
        let second = tree.run_after("second.c", &cache, true, vec![first_journal]);
        // It replayed `first.c`'s entry instead of publishing its own.
        assert!(second
            .expansion_journal
            .as_ref()
            .unwrap()
            .pending
            .is_empty());
        assert_eq!(second.replayed_variants.len(), 1);
        assert!(second.replayed_variants[0].1 & PROVISIONAL != 0);

        assert_eq!(commit(&second, &cache), None, "not stored yet");
        assert!(commit(&first, &cache).is_some());
        assert_eq!(commit(&second, &cache), Some(vec![(tree.header(), 0)]));
        assert_eq!(stored(&cache, &tree.header()).len(), 1);
    }

    #[test]
    fn a_later_replay_keeps_the_variant_first_adopted() {
        // `outer.h` records the full `b.h` as its nested variant, and its
        // replay defines `B_H` without covering `b.h`, so a later
        // `#include "b.h"` matches the stored guard-defined (empty) variant.
        // The unit reached both: the full one first, which alone holds
        // `b.h`'s declarations.
        let tree = Tree::new(&[
            (
                "b.h",
                "// b\n#ifndef B_H\n#define B_H\nint brief;\n#endif\n",
            ),
            (
                "outer.h",
                "#ifndef OUTER_H\n#define OUTER_H\n#include \"b.h\"\nint outer;\n#endif\n",
            ),
            ("w.c", "#include \"b.h\"\n"),
            ("p.c", "#include \"outer.h\"\n"),
            ("e.c", "#define B_H\n#include \"b.h\"\n"),
            ("c.c", "#include \"outer.h\"\n#include \"b.h\"\n"),
        ]);
        let cache = new_cache();
        for unit in ["w.c", "p.c", "e.c"] {
            tree.run(unit, &cache, false);
        }
        let brief = tree.root.join("b.h");
        let variants = stored(&cache, &brief);
        let full = variants
            .iter()
            .position(|text| text.contains("brief"))
            .expect("the full variant is stored");
        assert!(
            variants.iter().any(|text| !text.contains("brief")),
            "a guard-defined variant is stored: {variants:?}"
        );
        let consumer = tree.run("c.c", &cache, false);
        assert!(
            consumer.replayed_variants.contains(&(brief.clone(), full)),
            "{:?} lacks ({brief:?}, {full})",
            consumer.replayed_variants
        );
    }

    #[test]
    fn an_entry_records_the_variant_its_publisher_expanded_itself() {
        // `p.c` expands the guarded `b.h` live, publishing its full variant,
        // then builds `outer.h`, whose `#include "b.h"` the guard skips. The
        // entry takes `b.h`'s macro effects (its guard) and must also say
        // which `b.h` it stands for, or a consumer replaying `outer.h` gets
        // the guard without the declarations.
        let tree = Tree::new(&[
            ("b.h", "#ifndef B_H\n#define B_H\nint brief;\n#endif\n"),
            (
                "outer.h",
                "#ifndef OUTER_H\n#define OUTER_H\n#include \"b.h\"\nint outer;\n#endif\n",
            ),
            ("p.c", "#include \"b.h\"\n#include \"outer.h\"\n"),
            ("c.c", "#include \"outer.h\"\n#include \"b.h\"\n"),
        ]);
        let cache = new_cache();
        tree.run("p.c", &cache, false);
        let brief = tree.root.join("b.h");
        let full = stored(&cache, &brief)
            .iter()
            .position(|text| text.contains("brief"))
            .expect("the full variant is stored");
        let consumer = tree.run("c.c", &cache, false);
        assert!(
            consumer.replayed_variants.contains(&(brief.clone(), full)),
            "{:?} lacks ({brief:?}, {full})",
            consumer.replayed_variants
        );
    }

    #[test]
    fn an_entry_records_the_nested_headers_it_expanded_live() {
        // `outer.h` includes `mid.h`, which includes `b.h`; one run expands
        // all three. `outer.h`'s entry holds the other two as nested
        // includes and must record them as it records replayed ones, or a
        // consumer replaying `outer.h` gets `b.h`'s guard without it.
        let tree = Tree::new(&[
            ("b.h", "#ifndef B_H\n#define B_H\nint brief;\n#endif\n"),
            (
                "mid.h",
                "#ifndef MID_H\n#define MID_H\n#include \"b.h\"\n#endif\n",
            ),
            (
                "outer.h",
                "#ifndef OUTER_H\n#define OUTER_H\n#include \"mid.h\"\n#endif\n",
            ),
            ("p.c", "#include \"outer.h\"\n"),
        ]);
        let cache = new_cache();
        tree.run("p.c", &cache, false);
        let nested: Vec<PathBuf> = cache
            .read()
            .unwrap()
            .get(&(tree.root.join("outer.h"), Language::C))
            .and_then(|variants| variants.first())
            .map(|entry| {
                entry
                    .nested_variants
                    .iter()
                    .map(|(p, _)| p.clone())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            nested.contains(&tree.root.join("mid.h")) && nested.contains(&tree.root.join("b.h")),
            "{nested:?}"
        );
    }

    #[test]
    fn a_replayed_empty_variant_does_not_stand_for_the_header_later() {
        // `q.c` adopts the full `b.h` through `outer.h`, then replays the
        // guard-defined (empty) `b.h` directly, then builds `later.h`, which
        // includes `b.h` too. `later.h`'s entry must name the full `b.h`:
        // naming the empty one hands consumers the guard without the
        // declarations.
        let tree = Tree::new(&[
            (
                "b.h",
                "// b\n#ifndef B_H\n#define B_H\nint brief;\n#endif\n",
            ),
            (
                "outer.h",
                "#ifndef OUTER_H\n#define OUTER_H\n#include \"b.h\"\nint outer;\n#endif\n",
            ),
            (
                "later.h",
                "#ifndef LATER_H\n#define LATER_H\n#include \"b.h\"\nint later;\n#endif\n",
            ),
            ("w.c", "#include \"b.h\"\n"),
            ("p.c", "#include \"outer.h\"\n"),
            ("e.c", "#define B_H\n#include \"b.h\"\n"),
            (
                "q.c",
                "#include \"outer.h\"\n#include \"b.h\"\n#include \"later.h\"\n",
            ),
        ]);
        let cache = new_cache();
        for unit in ["w.c", "p.c", "e.c", "q.c"] {
            tree.run(unit, &cache, false);
        }
        let brief = tree.root.join("b.h");
        let full = stored(&cache, &brief)
            .iter()
            .position(|text| text.contains("brief"))
            .expect("the full variant is stored");
        let recorded: Vec<(PathBuf, usize)> = cache
            .read()
            .unwrap()
            .get(&(tree.root.join("later.h"), Language::C))
            .and_then(|variants| variants.first())
            .map(|entry| entry.nested_variants.iter().cloned().collect())
            .unwrap_or_default();
        assert_eq!(recorded, vec![(brief, full)]);
    }

    #[test]
    fn an_entry_records_each_expansion_of_a_header_it_includes_twice() {
        // `decl.h` has no guard and reads `NAME`; `both.h` includes it once
        // per name. A consumer replaying `both.h` needs both expansions.
        let tree = Tree::new(&[
            ("decl.h", "int NAME;\n"),
            (
                "both.h",
                "#ifndef BOTH_H\n#define BOTH_H\n#define NAME first\n#include \"decl.h\"\n\
                 #undef NAME\n#define NAME second\n#include \"decl.h\"\n#undef NAME\n#endif\n",
            ),
            ("w.c", "#include \"both.h\"\n"),
        ]);
        let cache = new_cache();
        tree.run("w.c", &cache, false);
        let decl = tree.root.join("decl.h");
        let variants = stored(&cache, &decl);
        let at = |name: &str| {
            variants
                .iter()
                .position(|text| text.contains(name))
                .expect("each expansion is stored")
        };
        let recorded: Vec<(PathBuf, usize)> = cache
            .read()
            .unwrap()
            .get(&(tree.root.join("both.h"), Language::C))
            .and_then(|variants| variants.first())
            .map(|entry| entry.nested_variants.iter().cloned().collect())
            .unwrap_or_default();
        assert_eq!(
            recorded,
            vec![(decl.clone(), at("first")), (decl, at("second"))]
        );
    }

    #[test]
    fn an_adopted_empty_variant_does_not_displace_the_one_expanded_here() {
        // `f.c` stores an `x.h` that replayed the guard-defined (empty)
        // `b.h`. `q.c` expands `b.h` itself, then replays that `x.h`, then
        // builds `later.h`: `later.h` must name the `b.h` `q.c` expanded,
        // not the empty one `x.h`'s record carried in.
        let tree = Tree::new(&[
            (
                "b.h",
                "// b\n#ifndef B_H\n#define B_H\nint brief;\n#endif\n",
            ),
            (
                "x.h",
                "#ifndef X_H\n#define X_H\n#include \"b.h\"\nint x;\n#endif\n",
            ),
            (
                "later.h",
                "#ifndef LATER_H\n#define LATER_H\n#include \"b.h\"\nint later;\n#endif\n",
            ),
            ("e.c", "#define B_H\n#include \"b.h\"\n"),
            ("f.c", "#define B_H\n#include \"x.h\"\n"),
            (
                "q.c",
                "#include \"b.h\"\n#include \"x.h\"\n#include \"later.h\"\n",
            ),
        ]);
        let cache = new_cache();
        for unit in ["e.c", "f.c", "q.c"] {
            tree.run(unit, &cache, false);
        }
        let brief = tree.root.join("b.h");
        let full = stored(&cache, &brief)
            .iter()
            .position(|text| text.contains("brief"))
            .expect("the full variant is stored");
        let recorded: Vec<(PathBuf, usize)> = cache
            .read()
            .unwrap()
            .get(&(tree.root.join("later.h"), Language::C))
            .and_then(|variants| variants.first())
            .map(|entry| entry.nested_variants.iter().cloned().collect())
            .unwrap_or_default();
        assert_eq!(recorded, vec![(brief, full)]);
    }
}
