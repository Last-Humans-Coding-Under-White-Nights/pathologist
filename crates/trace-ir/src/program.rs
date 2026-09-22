use crate::flow::ReturnFlow;
use crate::symbol::{Linkage, SymbolTable};
use crate::types::TypeTable;
use crate::{CallSiteId, FileId, FnId, Span};
use indexmap::IndexMap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

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
/// argument is resolved in the namespace or enclosing class where the derived
/// class is declared, not relative to the template itself or a later caller.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TemplateBase {
    pub derived: String,
    pub spelling: String,
    pub declaration_scope: String,
    /// The base spelling mentions an enclosing template parameter.
    pub is_dependent: bool,
}

type HeaderFunctions = FxHashMap<String, FxHashMap<Arc<str>, FnId>>;

/// Call records at one site, grouped by [`CallSite::fact_fingerprint`].
///
/// A group holds every record whose fingerprint collided, so it is one entry
/// deep except on a hash collision and the merge confirms each candidate.
pub type CallFactBuckets = FxHashMap<u64, Vec<CallSiteId>>;
/// Stable call-occurrence coordinates plus callee name. For macro member
/// calls these coordinates come from the replacement-list access operator,
/// independently of the request position exported for the call.
pub type CallSourceKey = (FileId, u32, u32, Option<(FileId, u32, u32)>, String);

/// Cross-unit deduplication state used by the merge stage: entities whose
/// origin (header file + position) was already merged map to the first copy.
#[derive(Debug, Clone, Default)]
pub struct MergeDedup {
    /// `(file, line) → name → FnId` so a hit does not clone the function name.
    pub fn_keys: FxHashMap<(FileId, u32), FxHashMap<String, FnId>>,
    /// Expanded definitions recorded by lowering or replayed from cached headers
    /// into a TU preamble; program_into_unit transfers these to UnitIndex.
    pub internal_definitions: FxHashMap<FnId, Arc<str>>,
    /// Header definitions are shared only within one link image and expansion.
    header_functions: FxHashMap<Span, HeaderFunctions>,
    pub header_flow: FxHashSet<crate::FlowConstraint>,
    pub site_keys: FxHashMap<CallSourceKey, CallSiteId>,
    /// Call records that configuration variants added at a site, beside the
    /// canonical one in `site_keys` (#59). Program-wide, so a fact recovered
    /// from a header by two units' variants merges instead of repeating: a
    /// header's call sites belong to every unit that includes it.
    ///
    /// Bucketed by a fingerprint of the call facts the merge compares, so a
    /// site whose facts differ per contributing unit costs one probe rather
    /// than a scan of every record already standing there: a shared header
    /// body reached from N units would otherwise be quadratic in N. The
    /// fingerprint only groups; the merge still confirms a candidate field by
    /// field, so a collision costs a comparison and never a wrong merge.
    pub variant_site_records: FxHashMap<CallSourceKey, CallFactBuckets>,
    /// Reports already merged into the whole program, keyed by stage as well as
    /// origin: two stages can report the same text at the same position, and
    /// one is not a duplicate of the other. Unit-local copies use different
    /// `FileId` spaces, so keys are inserted only after their file ids have
    /// been remapped.
    diagnostic_keys: FxHashSet<(Option<FileId>, u32, String, String)>,
}

impl MergeDedup {
    /// Borrow both name and definition text on the repeated-header path.
    pub fn existing_header_fn(&self, origin: Span, name: &str, text: &str) -> Option<FnId> {
        self.header_functions
            .get(&origin)?
            .get(name)?
            .get(text)
            .copied()
    }

    pub fn insert_header_fn(&mut self, origin: Span, name: String, text: Arc<str>, id: FnId) {
        self.header_functions
            .entry(origin)
            .or_default()
            .entry(name)
            .or_default()
            .insert(text, id);
    }

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

    /// Drop the entity tables while keeping `diagnostic_keys`.
    ///
    /// Entity deduplication is per link image: a header's function merged into
    /// one target must merge again into the next. A diagnostic is a
    /// program-wide fact about a source position, so re-reporting it once per
    /// target that happens to include the file is noise, not information.
    pub fn clear_entities(&mut self) {
        self.fn_keys.clear();
        self.internal_definitions.clear();
        self.header_functions.clear();
        self.header_flow.clear();
        self.site_keys.clear();
        self.variant_site_records.clear();
    }
}

/// A class-template member returning a bare type parameter. Kept with
/// types across header-unit merges; looked up by class and member name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateReturn {
    pub arity: u32,
    /// None blocks inference for an unsupported same-arity return.
    pub parameter: Option<usize>,
    pub pointer_depth: usize,
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

#[derive(Debug, Clone)]
pub struct LinkTarget {
    pub id: crate::TargetId,
    pub name: String,
    pub output: PathBuf,
    pub sources: Vec<FileId>,
    pub dependencies: Vec<crate::TargetId>,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub root: PathBuf,
    pub link_targets: Vec<LinkTarget>,
    /// Per-unit ranges used to remove an overridden weak body's constraints.
    pub function_flow_ranges: FxHashMap<FnId, Vec<std::ops::Range<usize>>>,
    /// Initializer constraints, including temporaries and deferred references.
    pub global_initializer_ranges: FxHashMap<crate::VarId, Vec<std::ops::Range<usize>>>,
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
    inheritance: Vec<(String, String)>,
    bases_by_class: FxHashMap<String, Vec<usize>>,
    derived_by_class: FxHashMap<String, Vec<usize>>,
    /// C++ template-base facts, including their declaration namespace.
    ///
    /// The ordinary inheritance graph stores the bare template name for CHA
    /// (for example, `IRemoteStub`), while consumers that understand a
    /// particular template can inspect the preserved spelling
    /// (`IRemoteStub<IFoo>`).
    pub template_bases: Vec<TemplateBase>,
    /// `template_bases` as a set: a unit's facts are re-added by every unit
    /// merging it, and a scan of the list per fact was quadratic.
    template_base_set: rustc_hash::FxHashSet<TemplateBase>,
    /// Declared C++ `operator->` returns, merged with a unit's types so a
    /// header's wrappers are followable from every unit that includes it.
    pub arrow_returns: Vec<ArrowReturn>,
    /// Class -> full member name -> return substitutions. Ordered keys keep
    /// header merges deterministic; lookups never scan unrelated functions.
    pub template_returns: BTreeMap<String, BTreeMap<String, Vec<TemplateReturn>>>,
    /// Classes declared `final` — CHA does not walk into their subclasses.
    pub final_classes: Vec<String>,
    /// Classes defined in an anonymous namespace, with the files their
    /// definitions are in. Every file's anonymous namespace spells a class the
    /// same, so the name alone does not say which of them a receiver is.
    pub anonymous_classes: BTreeMap<String, BTreeSet<FileId>>,
    /// The classes of `anonymous_classes` declared `final`, with the files
    /// defining them so. `final_classes` holds only the other classes: a
    /// name alone would cut dispatch for every class of it.
    pub anonymous_final_classes: BTreeMap<String, BTreeSet<FileId>>,
    /// The direct bases of each class of `anonymous_classes`, with the file
    /// that derives it so: `inheritance` joins every class of a name, and
    /// another file's class of the name derives from other bases.
    pub anonymous_bases: BTreeMap<String, BTreeSet<(FileId, String)>>,
    /// Qualified names of the C++ namespaces opened (`ns::inner`), merged with
    /// a header's types so a unit knows the namespaces its headers open.
    pub namespaces: BTreeSet<String>,
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

    /// Release merge-only lookup tables after the last merge and finalization.
    /// Callers that intend to merge more units must retain this state.
    pub fn release_merge_state(&mut self) {
        self.dedup = MergeDedup::default();
        self.template_base_set = rustc_hash::FxHashSet::default();
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

    /// Resolve callee functions for a call site, consulting `types` for shape-aware
    /// overload and definition matching.
    pub fn callees_of(&self, cs: &crate::CallSite) -> Vec<crate::FnId> {
        self.symbols.callees_of_with_types(cs, Some(&self.types))
    }

    /// Record a `(derived, base)` edge once.
    ///
    /// Only a direct self-loop is rejected. A cycle through two or more
    /// classes (`D: B` merged with `B: D` from another configuration of the
    /// same header) is representable, so this is not guaranteed to be a DAG:
    /// every walk over it — [`Self::subclass_closure`], [`Self::method_targets`],
    /// [`Self::bases_of`] chains — must carry its own visited set.
    pub fn add_inheritance(&mut self, derived: &str, base: &str) {
        // A class is not its own base; a malformed `class A : public A`
        // would otherwise list `A` among `bases_of("A")`.
        if derived.is_empty() || base.is_empty() || derived == base {
            return;
        }
        if self
            .bases_by_class
            .get(derived)
            .is_some_and(|edges| edges.iter().any(|&i| self.inheritance[i].1 == base))
        {
            return;
        }
        let index = self.inheritance.len();
        self.inheritance.push((derived.to_owned(), base.to_owned()));
        push_edge_index(&mut self.bases_by_class, derived, index);
        push_edge_index(&mut self.derived_by_class, base, index);
    }

    /// Whether `cls` has a recorded base class.
    pub fn has_bases(&self, cls: &str) -> bool {
        self.bases_by_class.contains_key(cls)
    }

    /// Whether `cls` is a base or a derived class of any recorded edge.
    pub fn has_inheritance_edges(&self, cls: &str) -> bool {
        self.bases_by_class.contains_key(cls) || self.derived_by_class.contains_key(cls)
    }

    pub fn inheritance(&self) -> &[(String, String)] {
        &self.inheritance
    }

    pub fn take_inheritance(&mut self) -> Vec<(String, String)> {
        self.bases_by_class.clear();
        self.derived_by_class.clear();
        std::mem::take(&mut self.inheritance)
    }

    /// Preserve a templated base-class spelling once.
    pub fn add_template_base(
        &mut self,
        derived: &str,
        base: &str,
        declaration_scope: &str,
        is_dependent: bool,
    ) {
        if derived.is_empty() || base.is_empty() {
            return;
        }
        self.add_template_base_fact(&TemplateBase {
            derived: derived.to_string(),
            spelling: base.to_string(),
            declaration_scope: declaration_scope.to_string(),
            is_dependent,
        });
    }

    /// [`add_template_base`](Self::add_template_base) for a fact another
    /// unit already holds: every unit merging a header re-adds the header's
    /// facts, so the common case is a hit that allocates nothing.
    pub fn add_template_base_fact(&mut self, fact: &TemplateBase) {
        if fact.derived.is_empty() || fact.spelling.is_empty() {
            return;
        }
        if !self.template_base_set.contains(fact) {
            self.template_base_set.insert(fact.clone());
            self.template_bases.push(fact.clone());
        }
    }

    /// Templated base-class facts declared directly on `cls`.
    pub fn template_bases_of(&self, cls: &str) -> Vec<&TemplateBase> {
        self.template_bases
            .iter()
            .filter(|fact| fact.derived == cls)
            .collect()
    }

    /// Record that `member` of class template `owner` substitutes `fact`
    /// for a concrete receiver. Every unit merging the declaring header
    /// re-adds the header's facts, so the common case is a hit.
    pub fn add_template_return(&mut self, owner: &str, member: &str, fact: &TemplateReturn) {
        let facts = self
            .template_returns
            .entry(owner.to_string())
            .or_default()
            .entry(member.to_string())
            .or_default();
        if !facts.contains(fact) {
            facts.push(fact.clone());
        }
    }

    /// The substitution facts recorded for `member` of class `cls`.
    pub fn template_returns_of(&self, cls: &str, member: &str) -> Option<&[TemplateReturn]> {
        Some(self.template_returns.get(cls)?.get(member)?.as_slice())
    }

    /// Whether `cls` is a class template any of whose members substitutes a
    /// return type — so a receiver spelled with arguments must keep them.
    pub fn has_template_returns(&self, cls: &str) -> bool {
        self.template_returns.contains_key(cls)
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

    /// Record that `file` defines the class `cls` in an anonymous namespace.
    pub fn mark_class_anonymous(&mut self, cls: &str, file: FileId) {
        self.anonymous_classes
            .entry(cls.to_string())
            .or_default()
            .insert(file);
    }

    /// Record that `file` defines the class `cls` `final` in an anonymous
    /// namespace.
    pub fn mark_anonymous_class_final(&mut self, cls: &str, file: FileId) {
        self.anonymous_final_classes
            .entry(cls.to_string())
            .or_default()
            .insert(file);
    }

    /// Record that `file` derives the class `cls`, defined there in an
    /// anonymous namespace, from `base`.
    pub fn add_anonymous_base(&mut self, cls: &str, file: FileId, base: &str) {
        self.anonymous_bases
            .entry(cls.to_string())
            .or_default()
            .insert((file, base.to_string()));
    }

    /// Whether the class `cls` an anonymous namespace defines in a file
    /// `sees` admits derives, through bases those files see, from a class
    /// `outside` admits that is not one of them.
    pub fn anonymous_class_derives_from(
        &self,
        cls: &str,
        sees: &dyn Fn(FileId) -> bool,
        outside: &dyn Fn(&str) -> bool,
    ) -> bool {
        let mut pending = vec![cls];
        let mut visited = BTreeSet::new();
        while let Some(cur) = pending.pop() {
            if !visited.insert(cur) {
                continue;
            }
            let bases = self.anonymous_bases.get(cur).into_iter().flatten();
            for (_, base) in bases.filter(|(file, _)| sees(*file)) {
                if self.class_is_anonymous_in(base, sees) {
                    pending.push(base);
                } else if outside(base) {
                    return true;
                }
            }
        }
        false
    }

    /// Whether `cls` names a class an anonymous namespace defines in a file
    /// `sees` admits.
    pub fn class_is_anonymous_in(&self, cls: &str, sees: impl Fn(FileId) -> bool) -> bool {
        Self::defined_in(&self.anonymous_classes, cls, sees)
    }

    fn defined_in(
        classes: &BTreeMap<String, BTreeSet<FileId>>,
        cls: &str,
        sees: impl Fn(FileId) -> bool,
    ) -> bool {
        classes
            .get(cls)
            .is_some_and(|files| files.iter().any(|&f| sees(f)))
    }

    /// The entries under `name` that `keep` admits.
    fn members_among(&self, name: &str, keep: &dyn Fn(FnId) -> bool) -> Vec<FnId> {
        let mut ids = self.symbols.functions_named(name);
        ids.retain(|&id| keep(id));
        ids
    }

    /// Whether dispatch stops at `cls`, seen from the files `sees` admits: it
    /// is `final`, or declares the method `final`. Another file's class in an
    /// anonymous namespace is another class, and does not stop it.
    fn dispatch_stops_at(
        &self,
        cls: &str,
        kind: &MethodKind,
        sees: &dyn Fn(FileId) -> bool,
    ) -> bool {
        self.class_is_final(cls)
            || Self::defined_in(&self.anonymous_final_classes, cls, sees)
            || self
                .symbols
                .functions_named(&kind.name_on(cls))
                .iter()
                .any(|&id| {
                    let f = self.symbols.function(id);
                    f.is_final && (f.linkage != Linkage::Internal || sees(f.file))
                })
    }

    /// Direct base classes of `cls` as the files `sees` admits see it: an
    /// anonymous-namespace class there derives from its own bases, not from
    /// those every file's class of its name declares.
    fn bases_seen(&self, cls: &str, sees: &dyn Fn(FileId) -> bool) -> Vec<String> {
        if !self.class_is_anonymous_in(cls, sees) {
            return self.bases_of(cls);
        }
        let bases = self.anonymous_bases.get(cls).into_iter().flatten();
        bases
            .filter(|(file, _)| sees(*file))
            .map(|(_, base)| base.clone())
            .collect()
    }

    /// Direct base classes of `cls`.
    pub fn bases_of(&self, cls: &str) -> Vec<String> {
        self.base_names(cls).map(str::to_owned).collect()
    }

    /// [`Self::bases_of`], borrowed.
    pub fn base_names(&self, cls: &str) -> impl Iterator<Item = &str> {
        self.bases_by_class
            .get(cls)
            .into_iter()
            .flatten()
            .map(|&i| self.inheritance[i].1.as_str())
    }

    /// `root` plus every class transitively deriving from it (BFS).
    pub fn subclass_closure(&self, root: &str) -> Vec<String> {
        let mut out = vec![root.to_string()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            for &index in self.derived_by_class.get(&cur).into_iter().flatten() {
                let derived = &self.inheritance[index].0;
                if !out.iter().any(|c| c == derived) {
                    out.push(derived.clone());
                }
            }
            i += 1;
        }
        out
    }

    /// Subclass closure used for virtual dispatch: stop at `final` classes
    /// and at classes that declare this method `final`.
    fn dispatch_subclass_closure(
        &self,
        root: &str,
        kind: &MethodKind,
        sees: &dyn Fn(FileId) -> bool,
    ) -> Vec<String> {
        let mut out = vec![root.to_string()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            i += 1;
            if self.dispatch_stops_at(&cur, kind, sees) {
                continue;
            }
            for &index in self.derived_by_class.get(&cur).into_iter().flatten() {
                let derived = &self.inheritance[index].0;
                if !out.iter().any(|c| c == derived) {
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
        self.method_targets_among(cls, kind, &|_| true, &|_| true)
    }

    /// [`method_targets`](Self::method_targets) for a receiver whose class
    /// shares its name with others, seen from the files `sees` admits: only
    /// classes and members seen there stop dispatch as `final`, and a member
    /// `targets` rejects is no target.
    pub fn method_targets_among(
        &self,
        cls: &str,
        kind: &MethodKind,
        sees: &dyn Fn(FileId) -> bool,
        targets: &dyn Fn(FnId) -> bool,
    ) -> Vec<FnId> {
        let mut out = Vec::new();
        for c in self.dispatch_subclass_closure(cls, kind, sees) {
            let own = self.members_among(&kind.name_on(&c), targets);
            if !own.is_empty() {
                for id in own {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
                continue;
            }
            // `c`'s own lookup just missed, so start the walk at its bases
            // rather than letting the queue repeat that same query.
            let mut queue = std::collections::VecDeque::new();
            let mut seen = std::collections::BTreeSet::new();
            queue.extend(self.bases_seen(&c, sees));
            seen.insert(c);
            while let Some(cur) = queue.pop_front() {
                if !seen.insert(cur.clone()) {
                    continue;
                }
                let ids = self.members_among(&kind.name_on(&cur), targets);
                if !ids.is_empty() {
                    for id in ids {
                        if !out.contains(&id) {
                            out.push(id);
                        }
                    }
                    break;
                }
                for base in self.bases_seen(&cur, sees) {
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

/// Append `index` under `key`, allocating the key only when it is new: the
/// ubiquitous bases (`RefBase`) collect thousands of edges across a merge.
fn push_edge_index(map: &mut FxHashMap<String, Vec<usize>>, key: &str, index: usize) {
    if let Some(edges) = map.get_mut(key) {
        edges.push(index);
    } else {
        map.insert(key.to_owned(), vec![index]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inheritance_queries_preserve_order_deduplication_and_reset() {
        let mut program = Program::default();
        program.add_inheritance("D", "B");
        program.add_inheritance("D", "A");
        program.add_inheritance("E", "B");
        program.add_inheritance("D", "B");
        program.add_inheritance("B", "D");
        assert_eq!(program.bases_of("D"), ["B", "A"]);
        assert_eq!(program.subclass_closure("B"), ["B", "D", "E"]);
        assert!(program.has_bases("D") && !program.has_bases("A"));
        assert!(program.has_inheritance_edges("A") && !program.has_inheritance_edges("Z"));
        assert_eq!(program.take_inheritance().len(), 4);
        assert!(program.bases_of("D").is_empty());
        assert!(!program.has_inheritance_edges("A"));
        assert_eq!(program.subclass_closure("B"), ["B"]);
        program.add_inheritance("New", "B");
        assert_eq!(program.subclass_closure("B"), ["B", "New"]);
    }

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
