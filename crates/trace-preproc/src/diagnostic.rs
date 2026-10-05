use rustc_hash::FxHashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub file: Option<std::path::PathBuf>,
    pub line: u32,
    pub message: String,
}

/// Immutable report shared by results, enclosing headers and cache replay.
pub type SharedDiagnostic = Arc<Diagnostic>;

/// Full-record interning for one indexing run. Unlike emission deduplication,
/// this distinguishes severities; sharing must never replace a report's facts.
#[derive(Default)]
pub struct DiagnosticPool {
    reports: Mutex<FxHashSet<SharedDiagnostic>>,
}

impl std::fmt::Debug for DiagnosticPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DiagnosticPool")
    }
}

impl DiagnosticPool {
    pub(crate) fn intern(&self, report: Diagnostic) -> SharedDiagnostic {
        let mut reports = self.reports.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = reports.get(&report) {
            return Arc::clone(existing);
        }
        let report = Arc::new(report);
        reports.insert(Arc::clone(&report));
        report
    }
}

/// Existing emission identity, sharing the record instead of copying its
/// path/message into every enclosing cache frame's membership set.
#[derive(Debug, Clone)]
pub(crate) struct DiagnosticKey(pub SharedDiagnostic);

impl PartialEq for DiagnosticKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
            || (self.0.file == other.0.file
                && self.0.line == other.0.line
                && self.0.message == other.0.message)
    }
}
impl Eq for DiagnosticKey {}
impl Hash for DiagnosticKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (&self.0.file, self.0.line, &self.0.message).hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_shares_full_records_but_preserves_severity_and_lifetime() {
        let pool = DiagnosticPool::default();
        let warning = Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: Some("header.h".into()),
            line: 7,
            message: "missing include".into(),
        };
        let first = pool.intern(warning.clone());
        let second = pool.intern(warning.clone());
        assert!(Arc::ptr_eq(&first, &second));
        let error = pool.intern(Diagnostic {
            severity: DiagnosticSeverity::Error,
            ..warning
        });
        assert!(!Arc::ptr_eq(&first, &error));
        assert_eq!(DiagnosticKey(first.clone()), DiagnosticKey(error));
        let weak = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(weak.upgrade().is_some());
        drop(pool);
        assert!(weak.upgrade().is_none());
    }
}
