use crate::flow::ReturnFlow;
use crate::symbol::SymbolTable;
use crate::types::TypeTable;
use crate::{CallSiteId, FileId, FnId};
use indexmap::IndexMap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

/// How to spell a C++ member function across a class hierarchy.
///
/// Constructors and destructors change spelling per class (`Derived::Derived`,
/// `Derived::~Derived`) while ordinary methods keep their name, so expansion
/// over an override set needs this distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodKind {
    Named(String),
    Ctor,
    Dtor,
}

impl MethodKind {
    /// Whether this kind participates in dynamic dispatch by default.
    pub fn is_destructor(&self) -> bool {
        matches!(self, MethodKind::Dtor)
    }

    /// The member's full name as spelled on class `cls`.
    pub fn name_on(&self, cls: &str) -> String {
        let last = cls.rsplit("::").next().unwrap_or(cls);
        match self {
            MethodKind::Named(m) => format!("{}::{}", cls, m),
            MethodKind::Ctor => format!("{}::{}", cls, last),
            MethodKind::Dtor => format!("{}::~{}", cls, last),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub file: Option<crate::FileId>,
    pub line: u32,
    pub message: String,
    pub stage: String,
}

/// A templated C++ base as written on a derived class.
///
/// `declaration_scope` is kept separately because an unqualified template
/// argument is resolved where the derived class is declared, not relative to
/// the namespace that qualifies the template itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateBase {
    pub derived: String,
    pub spelling: String,
    pub declaration_scope: String,
}

/// Cross-unit deduplication state used by the merge stage: entities whose
/// origin (header file + position) was already merged map to the first copy.
#[derive(Debug, Clone, Default)]
pub struct MergeDedup {
    /// `(file, line) → name → FnId` so a hit does not clone the function name.
    pub fn_keys: FxHashMap<(FileId, u32), FxHashMap<String, FnId>>,
    pub site_keys: FxHashMap<(FileId, u32, u32, String), CallSiteId>,
    /// Call records that configuration variants added at a site, beside the
    /// canonical one in `site_keys` (#59). Program-wide, so a fact recovered
    /// from a header by two units' variants merges instead of repeating: a
    /// header's call sites belong to every unit that includes it.
    pub variant_site_records: FxHashMap<(FileId, u32, u32, String), Vec<CallSiteId>>,
    /// Reports already merged into the whole program, keyed by stage as well as
    /// origin: two stages can report the same text at the same position, and
    /// one is not a duplicate of the other. Unit-local copies use different
    /// `FileId` spaces, so keys are inserted only after their file ids have
    /// been remapped.
    diagnostic_keys: FxHashSet<(Option<FileId>, u32, String, String)>,
}

impl MergeDedup {
    pub fn existing_fn(&self, file: FileId, name: &str, line: u32) -> Option<FnId> {
        self.fn_keys
            .get(&(file, line))
            .and_then(|by_name| by_name.get(name))
            .copied()
    }

    pub fn insert_fn(&mut self, file: FileId, name: String, line: u32, id: FnId) {
        self.fn_keys
            .entry((file, line))
            .or_default()
            .insert(name, id);
    }

    /// Record a diagnostic's origin, returning whether it is the first of its
    /// `(file, line, message, stage)`.
    pub fn insert_diagnostic(
        &mut self,
        file: Option<FileId>,
        line: u32,
        message: &str,
        stage: &str,
    ) -> bool {
        self.diagnostic_keys
            .insert((file, line, message.to_owned(), stage.to_owned()))
    }
}

/// What a C++ class's declared `operator->` returns, kept apart from the
/// function's own return type so that it reaches the units that include the
/// declaring header: those merge a header's *types* only, and a wrapper-typed
/// field declared in another header is lowered without the wrapper's members
/// in scope. Call sites follow these facts to the class `x->m` looks `m` up on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrowReturn {
    /// The declaring class, spelled without template arguments.
    pub class_name: String,
    /// The returned type, with the pointer layer recorded in `pointer`.
    /// `Unknown` for a dependent type that is not a bare parameter
    /// (`sptr<T>`), which no call site can name.
    pub target: crate::TypeDesc,
    /// When the returned type is a bare template parameter (`T *`), its
    /// position in the class template's parameter list, for the call site to
    /// substitute from the instantiation's arguments.
    pub parameter: Option<usize>,
    /// Whether a pointer is returned. A pointer ends the arrow chain at its
    /// pointee; a class value continues it through that class's own arrow.
    pub pointer: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub root: PathBuf,
    pub types: TypeTable,
    pub symbols: SymbolTable,
    pub flow: Vec<crate::FlowConstraint>,
    /// Per-function return-value summaries collected during lowering.
    pub fn_returns: IndexMap<FnId, Vec<ReturnFlow>>,
    pub diagnostics: Vec<Diagnostic>,
    pub include_paths: Vec<PathBuf>,
    /// `#include` dependency edges (dependent → included), project-local only.
    pub include_deps: Vec<(PathBuf, PathBuf)>,
    pub defines: IndexMap<String, String>,
    pub anon_type_counter: u32,
    pub dedup: MergeDedup,
    /// C++ class-inheritance facts: `(derived, base)` qualified names.
    /// Names are the fully qualified spellings used for functions/types
    /// (`ns::Cls`). Populated at lowering, consumed post-merge by virtual
    /// dispatch expansion.
    pub inheritance: Vec<(String, String)>,
    /// C++ template-base facts, including their declaration namespace.
    ///
    /// The ordinary inheritance graph stores the bare template name for CHA
    /// (for example, `IRemoteStub`), while consumers that understand a
    /// particular template can inspect the preserved spelling
    /// (`IRemoteStub<IFoo>`).
    pub template_bases: Vec<TemplateBase>,
    /// Declared C++ `operator->` returns, merged with a unit's types so a
    /// header's wrappers are followable from every unit that includes it.
    pub arrow_returns: Vec<ArrowReturn>,
    /// Classes declared `final` — CHA does not walk into their subclasses.
    pub final_classes: Vec<String>,
    /// Whether configuration-variant exploration was enabled (#59).
    pub explore: bool,
    /// Maximum configuration-variant exploration budget per translation unit
    /// (#59), as configured for the run that built this program. The default
    /// lives with the CLI flag and `PreprocessOptions`, not here.
    pub explore_budget: usize,
    /// Variant units actually merged (#59). `--explore` only *offers* to
    /// explore: a unit with no feasible variant, or a zero budget, merges
    /// none. Analyses that compensate for cross-variant layout unioning must
    /// key on this rather than on `explore`, so that requesting exploration
    /// and getting none is indistinguishable from not requesting it.
    pub variants_merged: usize,
    /// Whether any unit was merged with cross-configuration layout unioning
    /// (`merge_unit_variants`). Distinct from [`Program::variants_merged`],
    /// which counts *exploration* variants per source and is 0 for an ordinary
    /// compilation database even though every unit past the first still merges
    /// as a variant and unions its aggregate layouts. Analyses that compensate
    /// for a unioned layout — a field moved off the index the configuration
    /// that lowered the access gave it — must key on this.
    pub layouts_unioned: bool,
}

impl Program {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            ..Default::default()
        }
    }

    /// Dependency roots whose headers contribute declarations but whose
    /// sources are never indexed as translation units (`--dep`, #60).
    pub fn dep_roots(&self) -> &[PathBuf] {
        self.symbols.dep_roots()
    }

    /// Whether a path lies under a dependency root. Canonicalizes; prefer
    /// [`Program::is_dep_file`] once the path has been interned.
    pub fn is_dep_path(&self, path: &std::path::Path) -> bool {
        self.symbols.path_is_dep(path)
    }

    /// Whether an interned file lies under a dependency root. O(1).
    pub fn is_dep_file(&self, file: FileId) -> bool {
        self.symbols.file_is_dep(file)
    }

    /// Record a `(derived, base)` edge once.
    pub fn add_inheritance(&mut self, derived: &str, base: &str) {
        if derived.is_empty() || base.is_empty() {
            return;
        }
        let edge = (derived.to_string(), base.to_string());
        if !self.inheritance.contains(&edge) {
            self.inheritance.push(edge);
        }
    }

    /// Preserve a templated base-class spelling once.
    pub fn add_template_base(&mut self, derived: &str, base: &str, declaration_scope: &str) {
        if derived.is_empty() || base.is_empty() {
            return;
        }
        let fact = TemplateBase {
            derived: derived.to_string(),
            spelling: base.to_string(),
            declaration_scope: declaration_scope.to_string(),
        };
        if !self.template_bases.contains(&fact) {
            self.template_bases.push(fact);
        }
    }

    /// Templated base-class facts declared directly on `cls`.
    pub fn template_bases_of(&self, cls: &str) -> Vec<&TemplateBase> {
        self.template_bases
            .iter()
            .filter(|fact| fact.derived == cls)
            .collect()
    }

    /// Record that `cls` is a `final` class.
    pub fn mark_class_final(&mut self, cls: &str) {
        if cls.is_empty() {
            return;
        }
        if !self.final_classes.iter().any(|c| c == cls) {
            self.final_classes.push(cls.to_string());
        }
    }

    pub fn class_is_final(&self, cls: &str) -> bool {
        self.final_classes.iter().any(|c| c == cls)
    }

    fn class_method_is_final(&self, cls: &str, kind: &MethodKind) -> bool {
        self.symbols
            .functions_named(&kind.name_on(cls))
            .iter()
            .any(|&id| self.symbols.function(id).is_final)
    }

    /// Direct base classes of `cls`.
    pub fn bases_of(&self, cls: &str) -> Vec<String> {
        self.inheritance
            .iter()
            .filter(|(d, _)| d == cls)
            .map(|(_, b)| b.clone())
            .collect()
    }

    /// `root` plus every class transitively deriving from it (BFS).
    pub fn subclass_closure(&self, root: &str) -> Vec<String> {
        let mut out = vec![root.to_string()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            for (derived, base) in &self.inheritance {
                if base == &cur && !out.iter().any(|c| c == derived) {
                    out.push(derived.clone());
                }
            }
            i += 1;
        }
        out
    }

    /// Subclass closure used for virtual dispatch: stop at `final` classes
    /// and at classes that declare this method `final`.
    pub fn dispatch_subclass_closure(&self, root: &str, kind: &MethodKind) -> Vec<String> {
        let mut out = vec![root.to_string()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            i += 1;
            if self.class_is_final(&cur) || self.class_method_is_final(&cur, kind) {
                continue;
            }
            for (derived, base) in &self.inheritance {
                if base == &cur && !out.iter().any(|c| c == derived) {
                    out.push(derived.clone());
                }
            }
        }
        out
    }

    /// Every method that CHA may select for a virtual call whose static
    /// receiver type is `cls`. For each class in the (final-cut) subclass
    /// closure, the nearest declaration walking toward bases is a target —
    /// so a `final` class that does not override still resolves to the
    /// inherited implementation, not to sibling overrides.
    pub fn method_targets(&self, cls: &str, kind: &MethodKind) -> Vec<FnId> {
        let mut out = Vec::new();
        for c in self.dispatch_subclass_closure(cls, kind) {
            let mut queue = std::collections::VecDeque::new();
            let mut seen = std::collections::BTreeSet::new();
            queue.push_back(c);
            while let Some(cur) = queue.pop_front() {
                if !seen.insert(cur.clone()) {
                    continue;
                }
                let ids = self.symbols.functions_named(&kind.name_on(&cur));
                if !ids.is_empty() {
                    for id in ids {
                        if !out.contains(&id) {
                            out.push(id);
                        }
                    }
                    break;
                }
                for base in self.bases_of(&cur) {
                    queue.push_back(base);
                }
            }
        }
        out
    }

    pub fn add_diagnostic(&mut self, diag: Diagnostic) {
        self.diagnostics.push(diag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two stages can report the same text at the same position — a variant's
    /// `parse` report must not stand in for a later unit's `preprocess` one
    /// (#59 review).
    #[test]
    fn diagnostic_dedup_separates_stages() {
        let mut dedup = MergeDedup::default();
        let file = Some(FileId(3));

        assert!(dedup.insert_diagnostic(file, 12, "unknown type name", "parse"));
        assert!(
            !dedup.insert_diagnostic(file, 12, "unknown type name", "parse"),
            "the same report from the same stage is a duplicate"
        );
        assert!(
            dedup.insert_diagnostic(file, 12, "unknown type name", "preprocess"),
            "a different stage reporting the same text is its own finding"
        );
    }
}
