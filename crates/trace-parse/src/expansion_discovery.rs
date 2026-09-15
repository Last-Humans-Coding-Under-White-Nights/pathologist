//! The discovery pass on the worker pool: every translation unit preprocessed
//! against the writable expansion cache, with the cache and each unit's text
//! left exactly as running the units one at a time in order would leave them.
//! The design is in `docs/PREPROCESSOR.md` ("Parallel discovery").

use crate::deps::IncludeGraph;
use crate::index_cache::{IndexSourceCache, PreprocessedSource};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use trace_preproc::{ExpansionCache, ExpansionJournal, Language, PreprocessOptions};

/// How far past the commit point, per worker, units are run.
const WINDOW_PER_WORKER: usize = 4;

/// What one discovery pass did, for the `preprocess-done` line.
#[derive(Default)]
pub(crate) struct DiscoveryStats {
    /// Runs discarded because a run ahead of them rules them out.
    pub(crate) discarded: usize,
    /// Units the committing thread ran itself.
    pub(crate) in_order: usize,
}

/// The inputs of one discovery pass.
pub(crate) struct Discovery<'a> {
    /// The units, in the order they commit.
    pub(crate) units: &'a [PathBuf],
    pub(crate) graph: &'a IncludeGraph,
    pub(crate) sources: &'a IndexSourceCache,
    pub(crate) expansions: &'a ExpansionCache,
    /// Writable options per language.
    pub(crate) opts: &'a HashMap<Language, PreprocessOptions>,
    pub(crate) language: &'a (dyn Fn(&Path) -> Language + Sync),
}

type Journal = Arc<ExpansionJournal>;

/// Names one run of a unit, so that a run shown another's journal can tell
/// whether that run still stands.
type RunId = usize;

/// A finished run's journal, as shown to a later run.
struct Shown {
    unit: usize,
    run: RunId,
    journal: Journal,
}

/// A run that finished and has been neither committed nor discarded.
struct Finished {
    run: RunId,
    /// Taken when the unit reaches the commit point.
    src: Option<PreprocessedSource>,
    /// `None` for a source read as-is, which reads and publishes nothing.
    journal: Option<Journal>,
}

struct Board<'a> {
    cache: &'a ExpansionCache,
    n: usize,
    window: usize,
    /// Lowest unit never handed out.
    next: usize,
    /// Units whose run was discarded, to run again.
    requeued: BTreeSet<usize>,
    /// Units a worker could not run; the committing thread runs them.
    failed: BTreeSet<usize>,
    /// Units committed, all in order.
    committed: usize,
    /// The run each committed unit committed, when a worker ran it.
    committed_runs: Vec<Option<RunId>>,
    next_run: RunId,
    /// Finished runs of uncommitted units, and of the unit being committed.
    finished: BTreeMap<usize, Finished>,
    /// Each queued unit's discarded journal: a guess at its next run.
    discarded: HashMap<usize, Journal>,
    /// Per queued unit, the unfinished units ahead it waits for.
    blockers: HashMap<usize, HashSet<usize>>,
    stats: DiscoveryStats,
}

/// A worker's finished run, as handed to the board.
struct Run {
    unit: usize,
    id: RunId,
    src: PreprocessedSource,
    journal: Option<Journal>,
    shown: Vec<Shown>,
}

/// One pass in progress: its inputs, the options its runs use, and the
/// board the workers and the committing thread share.
struct Pass<'a> {
    discovery: &'a Discovery<'a>,
    /// `discovery.opts` with publishing deferred.
    deferred: HashMap<Language, PreprocessOptions>,
    board: Mutex<Board<'a>>,
    changed: Condvar,
}

impl<'a> Pass<'a> {
    fn lock(&self) -> MutexGuard<'_, Board<'a>> {
        self.board.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'g>(&self, board: MutexGuard<'g, Board<'a>>) -> MutexGuard<'g, Board<'a>> {
        self.changed.wait(board).unwrap_or_else(|e| e.into_inner())
    }

    /// Change the board and wake everyone waiting on it.
    fn update<R>(&self, change: impl FnOnce(&mut Board<'a>) -> R) -> R {
        let result = change(&mut self.lock());
        self.changed.notify_all();
        result
    }

    /// `unit`'s path and the deferred options for its language.
    fn unit(&self, unit: usize) -> (&PathBuf, &PreprocessOptions) {
        let path = &self.discovery.units[unit];
        (path, &self.deferred[&(self.discovery.language)(path)])
    }
}

/// Lets the workers go when the committing thread leaves, however it leaves:
/// if it unwinds, no further commit comes and the scope would wait forever.
struct ReleaseWorkers<'p, 'a>(&'p Pass<'a>);

impl Drop for ReleaseWorkers<'_, '_> {
    fn drop(&mut self) {
        self.0.update(|board| board.committed = board.n);
    }
}

impl Board<'_> {
    /// The next unit to run, a fresh id for the run, and the finished runs
    /// ahead of it to show it; `None` when nothing is worth starting.
    fn claim(&mut self) -> Option<(usize, RunId, Vec<Shown>)> {
        let queued = self
            .requeued
            .iter()
            .copied()
            .find(|unit| self.blockers.get(unit).is_none_or(HashSet::is_empty));
        let unit = match queued {
            Some(unit) => {
                self.requeued.remove(&unit);
                unit
            }
            None if self.next < self.n && self.next < self.committed + self.window => {
                self.next += 1;
                self.next - 1
            }
            None => return None,
        };
        self.next_run += 1;
        let shown = self
            .finished
            .range(self.committed..unit)
            .filter_map(|(u, f)| {
                Some(Shown {
                    unit: *u,
                    run: f.run,
                    journal: Arc::clone(f.journal.as_ref()?),
                })
            })
            .collect();
        Some((unit, self.next_run, shown))
    }

    /// Whether the run `shown` came from is still finished, or committed.
    fn stands(&self, shown: &Shown) -> bool {
        self.finished
            .get(&shown.unit)
            .is_some_and(|f| f.run == shown.run)
            || self.committed_runs[shown.unit] == Some(shown.run)
    }

    /// Take a worker's run, unless something already rules it out: the cache
    /// (`contradicted`), a run it was shown that has since been discarded, or
    /// a finished run ahead of it.
    fn finish(&mut self, run: Run, contradicted: bool) {
        let Run {
            unit,
            id,
            src,
            journal,
            shown,
        } = run;
        // Past the commit point only when the committing thread left early
        // (`ReleaseWorkers`); nothing is waiting for this run.
        if unit < self.committed {
            return;
        }
        let ruled_out = contradicted
            || journal.as_ref().is_some_and(|j| {
                shown.iter().any(|s| !self.stands(s) && s.journal.feeds(j))
                    || self
                        .finished
                        .range(self.committed..unit)
                        .any(|(_, ahead)| ahead.journal.as_ref().is_some_and(|a| a.rules_out(j)))
            });
        if ruled_out {
            self.requeue(unit, journal);
            return;
        }
        if let Some(journal) = &journal {
            self.discard_behind(unit, journal);
        }
        self.unblock(unit);
        self.finished.insert(
            unit,
            Finished {
                run: id,
                src: Some(src),
                journal,
            },
        );
    }

    /// Discard the finished runs behind `unit` that `journal` rules out.
    fn discard_behind(&mut self, unit: usize, journal: &ExpansionJournal) {
        let behind: Vec<usize> = self
            .finished
            .range(unit + 1..)
            .filter(|(_, later)| later.journal.as_ref().is_some_and(|l| journal.rules_out(l)))
            .map(|(u, _)| *u)
            .collect();
        for u in behind {
            self.discard(u);
        }
    }

    /// Throw away `unit`'s finished run and queue the unit again, along with
    /// every finished run behind it that was shown what it would publish.
    fn discard(&mut self, unit: usize) {
        let mut queue = vec![unit];
        while let Some(u) = queue.pop() {
            let Some(run) = self.finished.remove(&u) else {
                continue;
            };
            if let Some(journal) = &run.journal {
                queue.extend(
                    self.finished
                        .range(u + 1..)
                        .filter(|(_, later)| {
                            later.journal.as_ref().is_some_and(|l| journal.feeds(l))
                        })
                        .map(|(later, _)| *later),
                );
            }
            self.requeue(u, run.journal);
        }
    }

    /// Queue `unit` to run again, waiting for the unfinished units ahead of
    /// it whose own discarded run likely rules its last run out. That is only
    /// a guess: a wrong one costs a run, never a wrong commit.
    fn requeue(&mut self, unit: usize, journal: Option<Journal>) {
        self.stats.discarded += 1;
        self.requeued.insert(unit);
        let Some(journal) = journal else {
            return;
        };
        let blockers = (self.committed..unit)
            .filter(|u| {
                !self.finished.contains_key(u)
                    && self
                        .discarded
                        .get(u)
                        .is_some_and(|ahead| ahead.likely_rules_out(&journal, self.cache))
            })
            .collect();
        self.blockers.insert(unit, blockers);
        self.discarded.insert(unit, journal);
    }

    /// Nothing waits for `unit` any more: it finished, failed or committed.
    fn unblock(&mut self, unit: usize) {
        for waiting in self.blockers.values_mut() {
            waiting.remove(&unit);
        }
    }

    /// Record `unit` as committed: by the worker run `run`, or by a run of
    /// the committing thread's own with `journal`. That run was never among
    /// the finished, so the finished runs behind it are checked against it
    /// here, in the same step that moves the commit count a worker's check
    /// against the cache relies on.
    fn committed(&mut self, unit: usize, run: Option<RunId>, journal: Option<&ExpansionJournal>) {
        if let Some(journal) = journal {
            self.discard_behind(unit, journal);
        }
        self.committed = unit + 1;
        self.committed_runs[unit] = run;
        self.finished.remove(&unit);
        self.discarded.remove(&unit);
        self.blockers.remove(&unit);
        self.unblock(unit);
    }
}

impl Discovery<'_> {
    pub(crate) fn run(&self, pool: &rayon::ThreadPool, jobs: usize) -> DiscoveryStats {
        let n = self.units.len();
        if jobs == 1 || n <= 1 {
            // The serial pass itself: there is nothing to schedule.
            for path in self.units {
                let opts = &self.opts[&(self.language)(path)];
                let _ = self.sources.get_or_preprocess(path, self.graph, opts);
            }
            return DiscoveryStats {
                discarded: 0,
                in_order: n,
            };
        }
        let pass = Pass {
            discovery: self,
            deferred: self
                .opts
                .iter()
                .map(|(l, o)| (*l, o.clone().with_deferred_expansion_publish(true)))
                .collect(),
            board: Mutex::new(Board {
                cache: self.expansions,
                n,
                window: jobs * WINDOW_PER_WORKER,
                next: 0,
                requeued: BTreeSet::new(),
                failed: BTreeSet::new(),
                committed: 0,
                committed_runs: vec![None; n],
                next_run: 0,
                finished: BTreeMap::new(),
                discarded: HashMap::default(),
                blockers: HashMap::default(),
                stats: DiscoveryStats::default(),
            }),
            changed: Condvar::new(),
        };
        pool.in_place_scope(|scope| {
            for _ in 0..jobs {
                scope.spawn(|_| pass.work());
            }
            let _release = ReleaseWorkers(&pass);
            for unit in 0..n {
                pass.commit_unit(unit);
            }
        });
        pass.board
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .stats
    }
}

impl Pass<'_> {
    /// A worker: claim a unit, run it with publishing deferred, hand the run
    /// to the board.
    fn work(&self) {
        let discovery = self.discovery;
        loop {
            let (unit, id, shown) = {
                let mut board = self.lock();
                loop {
                    if board.committed >= board.n {
                        return;
                    }
                    if let Some(claimed) = board.claim() {
                        break claimed;
                    }
                    board = self.wait(board);
                }
            };
            let (path, opts) = self.unit(unit);
            let opts = opts.clone().with_expansion_journals_ahead(
                shown.iter().map(|s| Arc::clone(&s.journal)).collect(),
            );
            // A failed or panicking run goes to the committing thread, which
            // fails or panics the same way the serial pass would.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                discovery
                    .sources
                    .preprocess_uncached(path, discovery.graph, &opts)
            }));
            let Ok(Ok((src, journal))) = result else {
                // No worker run of it will finish, so nothing should wait for one.
                self.update(|board| {
                    board.failed.insert(unit);
                    board.unblock(unit);
                });
                continue;
            };
            let run = Run {
                unit,
                id,
                src,
                journal: journal.map(Arc::new),
                shown,
            };
            // Checked without the board, so a commit can land in between;
            // the commit count says whether one did, and `finish` covers the
            // unit being committed, whose run is still among the finished. A
            // contradiction stands whatever commits later: the cache only grows.
            let (mut board, contradicted) = loop {
                let commits = self.lock().committed;
                let contradicted = run
                    .journal
                    .as_ref()
                    .is_some_and(|j| j.is_contradicted(discovery.expansions));
                let board = self.lock();
                if contradicted || board.committed == commits {
                    break (board, contradicted);
                }
            };
            board.finish(run, contradicted);
            drop(board);
            self.changed.notify_all();
        }
    }

    /// Commit `unit`'s finished run, or run the unit here when it has none
    /// that stands.
    fn commit_unit(&self, unit: usize) {
        let discovery = self.discovery;
        let taken = {
            let mut board = self.lock();
            loop {
                if let Some(finished) = board.finished.get_mut(&unit) {
                    if let Some(src) = finished.src.take() {
                        break Some((finished.run, src, finished.journal.clone()));
                    }
                }
                // Nobody is running it: every unit before it has committed,
                // so running it here is the serial pass.
                if board.failed.remove(&unit) || board.requeued.remove(&unit) || board.next == unit
                {
                    board.next = board.next.max(unit + 1);
                    break None;
                }
                board = self.wait(board);
            }
        };
        let (run, own) = match taken {
            Some((run, mut src, journal)) => {
                if journal
                    .as_ref()
                    .is_none_or(|j| src.commit(j, discovery.expansions))
                {
                    discovery
                        .sources
                        .insert(&discovery.units[unit], discovery.graph, src);
                    (Some(run), None)
                } else {
                    self.update(|board| {
                        board.discard(unit);
                        board.requeued.remove(&unit);
                    });
                    (None, self.run_here(unit))
                }
            }
            None => (None, self.run_here(unit)),
        };
        self.update(|board| board.committed(unit, run, own.as_ref()));
    }

    /// Run `unit` on the committing thread and commit it, returning its
    /// journal. Nothing else writes the cache, and the run is shown nothing
    /// ahead, so it always stands.
    fn run_here(&self, unit: usize) -> Option<ExpansionJournal> {
        let discovery = self.discovery;
        self.lock().stats.in_order += 1;
        let (path, opts) = self.unit(unit);
        // An error publishes nothing and stores nothing, as in the serial pass.
        let Ok((mut src, journal)) =
            discovery
                .sources
                .preprocess_uncached(path, discovery.graph, opts)
        else {
            return None;
        };
        if let Some(journal) = &journal {
            assert!(
                src.commit(journal, discovery.expansions),
                "a run shown nothing ahead commits against a cache only this thread writes"
            );
        }
        discovery.sources.insert(path, discovery.graph, src);
        journal
    }
}
