//! Optional cumulative indexing CPU-wall scopes, summed across workers.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(crate) enum Metric {
    PreambleBuild,
    PreambleClone,
    MergeFull,
    MergeSymbols,
    MergeTypes,
    TypeImport,
    SourceLower,
    UnitFinish,
    SyntaxParse,
    AstLower,
}

struct Counter {
    calls: AtomicU64,
    nanos: AtomicU64,
}

static COUNTERS: [Counter; 10] = [const {
    Counter {
        calls: AtomicU64::new(0),
        nanos: AtomicU64::new(0),
    }
}; 10];

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TRACE_PROFILE_INDEX").is_some())
}

pub(crate) struct Scope(Metric, Option<Instant>);

impl Metric {
    pub(crate) fn enter(self) -> Scope {
        Scope(self, enabled().then(Instant::now))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(start) = self.1 {
            let counter = &COUNTERS[self.0 as usize];
            counter.calls.fetch_add(1, Ordering::Relaxed);
            counter
                .nanos
                .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

pub(crate) fn report() {
    if enabled() {
        for (name, counter) in [
            "preamble-build",
            "preamble-clone",
            "merge-full",
            "merge-symbols",
            "merge-types",
            "type-import",
            "source-lower",
            "unit-finish",
            "syntax-parse",
            "ast-lower",
        ]
        .into_iter()
        .zip(&COUNTERS)
        {
            eprintln!(
                "index-profile: {name} calls={} summed={:.3}s",
                counter.calls.load(Ordering::Relaxed),
                counter.nanos.load(Ordering::Relaxed) as f64 / 1e9
            );
        }
    }
}
