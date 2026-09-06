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
    /// Preprocessor reports already merged into the whole program. Unit-local
    /// copies use different `FileId` spaces, so keys are inserted only after
    /// their file ids have been remapped.
    preprocess_diagnostic_keys: FxHashSet<(Option<FileId>, u32, String)>,
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

    pub fn insert_preprocess_diagnostic(
        &mut self,
        file: Option<FileId>,
        line: u32,
        message: &str,
    ) -> bool {
        self.preprocess_diagnostic_keys
            .insert((file, line, message.to_owned()))
    }
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
    /// Classes declared `final` — CHA does not walk into their subclasses.
    pub final_classes: Vec<String>,
}

impl Program {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            ..Default::default()
        }
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
