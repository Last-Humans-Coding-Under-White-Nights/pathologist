use crate::{CallSiteId, FileId, FnId, Span, TypeId, VarId};
use indexmap::IndexMap;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Linkage {
    External,
    Internal,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageClass {
    Global,
    FileStatic,
    FnStatic,
    Param,
    Local,
}

#[derive(Debug, Clone)]
pub struct Variable {
    pub id: VarId,
    pub name: String,
    pub type_id: TypeId,
    pub storage: StorageClass,
    pub fn_id: Option<FnId>,
    pub param_index: Option<u32>,
    pub span: Span,
    pub is_pointer: bool,
    /// True for a global definition, including C tentative definitions.
    pub is_defined: bool,
    /// GNU weak attribute or an active weak pragma in this translation unit.
    pub is_weak: bool,
    /// Link target identity; absent when no link metadata is available.
    pub target: Option<crate::TargetId>,
    /// Declared inside a C++ namespace, so `name` — the only name recorded for
    /// a variable — is not the name a linker resolves. Such a global neither
    /// claims its image's binding for that name nor takes part in weak
    /// override, since `a::cb` and `b::cb` are different symbols.
    pub is_namespaced: bool,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub id: FnId,
    pub name: String,
    pub linkage: Linkage,
    pub return_type: TypeId,
    pub params: Vec<VarId>,
    pub locals: Vec<VarId>,
    /// Start position (original-file coordinates via LineMap).
    pub span: Span,
    /// Last line of the definition body in original-file coordinates.
    /// Equal to `span.line` for prototypes, synthesized externals, and
    /// bodies whose end could not be mapped back (e.g. cross-header ends).
    /// On merge, overwritten together with `file`/`span` so the range always
    /// describes the surviving definition row in its own file's coordinates.
    pub end_line: u32,
    pub file: FileId,
    pub is_defined: bool,
    /// GNU weak attribute or an active weak pragma in this translation unit.
    pub is_weak: bool,
    /// Link target identity; absent when no link metadata is available.
    pub target: Option<crate::TargetId>,
    /// Overload-signature param types in the *merged* program's TypeId space,
    /// recorded by the cross-TU merge pass. During `merge_unit_index` all
    /// functions are registered before their param variables are remapped, so
    /// `param_type(VarId)` cannot resolve a same-unit predecessor's params
    /// when a later overload is compared against it. This field carries the
    /// remapped types so the gate still separates `f(int)` from `f(double)`.
    pub param_type_ids: Vec<TypeId>,
    /// Parameters the declaration lists, not counting a member's implicit
    /// `this`; `None` where no declaration was read (synthesized entries).
    /// An in-class prototype carries no parameter variables, so this is the
    /// only record of which overload it declares: without it `Get()` and
    /// `Get(Mode&)` folded into one entry, and merged into whichever same-named
    /// definition came first. It is compared only where a parameterless entry
    /// would otherwise match any arity, and only between two C++ entries.
    pub explicit_arity: Option<u32>,
    /// A definition written under a qualifier its unit does not know as a
    /// class (`void Remote::Shared(Callback cb) {}` with the class header
    /// unresolved), and so lowered without `this`. Only such a body can be
    /// the member an in-class prototype declares; a function defined inside a
    /// `namespace` block is spelled unqualified and never is one.
    pub owner_unresolved: bool,
    /// The parameter list ends in `...` or a parameter pack: a call may pass
    /// more arguments than `explicit_arity`.
    pub variadic: bool,
    /// A constructor declared `= default` or `= delete` in its class body. It
    /// is not user-provided, so the class is still an aggregate (C++17) and
    /// braces initialize its fields rather than call it.
    pub defaulted_in_class: bool,
    /// Declared in its class body: a member, whatever its parameter list
    /// shows. An in-class prototype lowers no parameter variables, so this is
    /// the only record that a parameterless entry is a member rather than a
    /// namespace function of the same qualified name.
    pub declared_in_class: bool,
    /// How many of those parameters declare a default argument, so a call
    /// may pass `explicit_arity - default_args` up to `explicit_arity`
    /// arguments. C++ puts the defaults on the first declaration, so a merge
    /// keeps the larger count.
    pub default_args: u32,
    /// Declared `virtual` (C++ methods). Virtual dispatch expansion treats a
    /// method as virtual if *any* entry with its qualified name carries this
    /// flag, so out-of-class definitions without the token still participate.
    pub is_virtual: bool,
    /// Declared `final` (C++ methods). CHA does not look for overrides in
    /// subclasses of a class that finalizes this method.
    pub is_final: bool,
    /// Entry may coexist with same-name externals of a different signature
    /// (C++ overloads). When neither side sets this, name merges behave
    /// exactly as in C (prototype + definition collapse into one entry).
    /// A C `.c` definition merging into a C++-parsed `.h` prototype clears
    /// this flag so a later TU merge does not treat the pair as overloads.
    pub is_cpp: bool,
    /// Originating translation unit, recorded by merge. Absent before merge or
    /// on synthesized entries.
    pub tu: Option<crate::FileId>,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub id: crate::CallSiteId,
    pub caller: FnId,
    pub callee_name: String,
    pub callee_var: Option<VarId>,
    /// Callee fixed up after lowering: a definition/prototype resolved at
    /// lowering time, or a synthesized external entry for a plain-identifier
    /// call that no tree-local symbol declares (libc calls, macro-emitted
    /// logging backends). `None` for indirect sites.
    pub callee_fn_id: Option<FnId>,
    pub var_args: Vec<(u32, VarId)>,
    pub fn_args: Vec<(u32, FnId)>,
    /// Argument positions recorded as `&base.member` / `&arr[i]` addresses.
    /// Lowering resolves these to the *base* variable, so function-model
    /// alias effects must not treat them as whole-object copies (copying
    /// the containing object would pollute unrelated fields).
    pub addr_of_member_args: Vec<u32>,
    /// Argument positions recorded as `&x` for a plain variable `x`: the
    /// actual there is a temporary holding x's address, and consumers that
    /// report the argument's object (arg-flow rows, terminators) name `x`.
    pub addr_of_args: Vec<u32>,
    /// The argument positions count the callee's implicit `this`: explicit
    /// arguments start at 1. Lowering sets it wherever it knows the callee is
    /// a member; after the merge, a site whose callee takes `this` without
    /// it is bound there (a callee whose class its unit never saw).
    pub args_bound_past_this: bool,
    pub span: Span,
    /// Outermost macro invocation that produced the call token. Present only
    /// when `span` points into a macro replacement list.
    pub expansion_span: Option<Span>,
    pub is_direct: bool,
    /// Static class of a C++ member-call receiver (`this`, typed pointer).
    /// Post-merge virtual expansion uses this so `final` types are not
    /// re-expanded from the declaring base.
    pub receiver_class: Option<String>,
    /// LHS of `dst = callee(...)` when the call's value is used (`CallReturn`
    /// destination). `dlsym` models write function addresses here.
    pub return_dst: Option<VarId>,
    /// Originating translation unit, recorded by merge. Absent before merge.
    pub tu: Option<crate::FileId>,
}

impl CallSite {
    /// Whether the site denotes a direct call recoverable by name: lowering
    /// recorded no callee variable and the callee text is no field or arrow
    /// expression. Cross-TU calls satisfy this: lowering marks them indirect
    /// only because the definition was not visible in the translation unit.
    pub fn resolves_by_name(&self) -> bool {
        self.callee_var.is_none()
            && !self.callee_name.contains("->")
            && !self.callee_name.contains('.')
    }

    /// What this record states about the call, borrowed from it.
    fn facts(&self) -> CallFacts<'_> {
        CallFacts {
            caller: self.caller,
            callee_fn_id: self.callee_fn_id,
            callee_var: self.callee_var,
            var_args: &self.var_args,
            fn_args: &self.fn_args,
            addr_of_member_args: &self.addr_of_member_args,
            addr_of_args: &self.addr_of_args,
            args_bound_past_this: self.args_bound_past_this,
            is_direct: self.is_direct,
            receiver_class: self.receiver_class.as_deref(),
            return_dst: self.return_dst,
        }
    }

    /// Whether two records standing at one source site state the same call.
    #[must_use]
    pub fn same_facts(&self, other: &Self) -> bool {
        self.facts() == other.facts()
    }

    /// A hash of the same facts [`CallSite::same_facts`] compares, so records
    /// at one site can be grouped and probed instead of scanned.
    ///
    /// Equal facts always give an equal fingerprint; the converse is not
    /// guaranteed, so a match found this way is still confirmed with
    /// `same_facts`, and a collision costs one comparison rather than a wrong
    /// merge. The grouping only pays while distinct facts mostly land in
    /// distinct buckets, which the crate's hasher gives at a fraction of
    /// SipHash's cost.
    #[must_use]
    pub fn fact_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        self.facts().hash(&mut hasher);
        hasher.finish()
    }
}

/// The facts a record states about a call, borrowed from it.
///
/// Identity, span and originating unit are deliberately absent: the question
/// these answer is whether a record the program already holds says what an
/// incoming one says, so the merge can coalesce them. Deriving both the
/// comparison and the hash from this one field list is what keeps them from
/// drifting — a field added to `CallSite` and weighed here reaches both.
#[derive(PartialEq, Eq, Hash)]
struct CallFacts<'a> {
    caller: FnId,
    callee_fn_id: Option<FnId>,
    callee_var: Option<VarId>,
    var_args: &'a [(u32, VarId)],
    fn_args: &'a [(u32, FnId)],
    addr_of_member_args: &'a [u32],
    addr_of_args: &'a [u32],
    args_bound_past_this: bool,
    is_direct: bool,
    receiver_class: Option<&'a str>,
    return_dst: Option<VarId>,
}

/// What a surviving entry takes from any redeclaration merged into it,
/// whatever its linkage. The cached signature moves with the parameter list
/// it describes, and a merge that hands over no list touches neither: a
/// shape-compatible prototype can carry different TypeIds, so replacing a
/// definition's cache would let a later, distinct body pass the exact-id
/// check, and `merge_unit` remaps an adopted list, which a stale cache would
/// describe with the old list's ids.
/// Whether an incoming definition replaces the one already registered.
///
/// One rule for functions and variables alike: a definition fills an entry
/// that has none, and a strong definition displaces a weak one. Equal strength
/// leaves the first winner in place.
pub fn definition_supersedes(
    existing_defined: bool,
    existing_weak: bool,
    incoming_weak: bool,
) -> bool {
    !existing_defined || (existing_weak && !incoming_weak)
}

fn absorb_redeclaration(
    existing: &mut Function,
    func: &Function,
    param_types: Option<&[TypeId]>,
    adopted_params: bool,
) {
    if existing.target.is_none() || !existing.is_defined {
        existing.is_weak |= func.is_weak;
    }
    if adopted_params {
        existing.param_type_ids = param_types
            .map(<[TypeId]>::to_vec)
            .unwrap_or_else(|| func.param_type_ids.clone());
    }
    if existing.explicit_arity.is_none() {
        existing.explicit_arity = func.explicit_arity;
    }
    existing.variadic |= func.variadic;
    // User-provided once any declaration of it is.
    existing.defaulted_in_class &= func.defaulted_in_class;
    existing.default_args = existing.default_args.max(func.default_args);
    existing.is_virtual |= func.is_virtual;
    existing.is_final |= func.is_final;
}

/// Whether merging `a` and `b` joins a member's in-class prototype with a
/// definition lowered without its `this`: a body defined under a qualifier its
/// unit did not know (`owner_unresolved`). The prototype says it is a member
/// (`declared_in_class`), with any number of parameters; a namespace function
/// defined out of line, `void util::Init() {}`, joins a prototype declared in
/// the namespace and never is.
fn joins_member_missing_this(a: &Function, b: &Function) -> bool {
    let (prototype, body) = if a.is_defined { (b, a) } else { (a, b) };
    prototype.is_cpp
        && body.is_cpp
        && !prototype.is_defined
        && prototype.declared_in_class
        && body.is_defined
        && body.owner_unresolved
}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub id: FileId,
    pub path: PathBuf,
    /// The file lies under a `--dep` dependency root: it contributes
    /// declarations only, and its entities export with `is_dep = 1` (#60).
    /// Decided once, when the path is interned.
    pub is_dep: bool,
}

/// What [`SymbolTable::register_function`] did with a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FnRegistration {
    /// The surviving entry: either the id the caller allocated, or the
    /// earlier entry this redeclaration merged into.
    pub id: FnId,
    /// The surviving entry replaced its `params` with the caller's list. It
    /// is then the caller's list that the entry holds, so a caller merging a
    /// unit owns remapping those ids out of unit-local space.
    pub adopted_params: bool,
}

/// Which link image a name lookup may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetScope {
    /// Unconstrained: every symbol, whatever image it belongs to.
    Any,
    /// One image, including the unscoped partition when the target is `None`.
    Image(Option<crate::TargetId>),
}

#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    has_target_scopes: bool,
    /// Monotonic cache: avoids scanning imported symbols in each translation unit.
    has_weak_symbols: bool,
    pub files: Vec<FileInfo>,
    pub functions: Vec<Function>,
    pub variables: Vec<Variable>,
    pub call_sites: Vec<CallSite>,
    /// One entry per name, the one every unqualified call site resolves
    /// through. It is not first-wins: `should_take_primary` gives the slot to
    /// a defined entry over any declaration, and a later definition of the
    /// name takes it from an earlier one. Only declarations are first-wins,
    /// and only while no definition holds the slot.
    pub fn_by_name: IndexMap<String, FnId>,
    /// Every external entry per name, overloads included (C++). Unlike
    /// `fn_by_name` this never collapses to a single id.
    pub externals_by_name: FxHashMap<String, Vec<FnId>>,
    /// Functions indexed by their *base* name — the last `::` segment
    /// (`ns::f` -> `f`; leading-`::` global spellings like `::f` also index
    /// under `f`). unqualified/ADL/`using` lookup iterates the whole
    /// program for a bare callee name across every namespace, which the
    /// exact-name `fn_by_name`/`externals_by_name` tables cannot express.
    base_by_name: FxHashMap<String, Vec<FnId>>,
    pub global_by_name: IndexMap<String, VarId>,
    target_globals: FxHashMap<crate::TargetId, FxHashMap<String, VarId>>,
    /// File-`static` variables per file, the first registered of a name
    /// winning.
    /// One entry per file, kept sorted by FileId (see `insert_by_file`).
    file_statics_by_name: FxHashMap<String, Vec<(FileId, VarId)>>,
    /// Internal-linkage definitions per file: `(file, name) -> FnId`.
    /// In C, a file-`static` definition shadows any external definition of
    /// the same name for references inside that file.
    ///
    /// Keyed by name, then file: a name is looked up from a file whose scope
    /// is that file plus every header it includes, which for a large
    /// translation unit is hundreds of files, while a name is defined with
    /// internal linkage in a few. So the lookup walks the definitions of the
    /// name and asks whether each is in scope, not the other way round.
    /// Kept sorted by FileId, equal-file entries in registration order
    /// (see `insert_by_file`).
    fn_by_scope: FxHashMap<String, Vec<(FileId, FnId)>>,
    /// The further C++ overloads of an entry in `fn_by_scope`, keyed by that
    /// entry: the table holds one function per file and name.
    scope_overloads: FxHashMap<FnId, Vec<FnId>>,
    /// The `fn_by_scope` entry each further overload belongs to.
    overload_primary: FxHashMap<FnId, FnId>,
    /// Internal-linkage members declared in their class, by qualified name: a
    /// class in an anonymous namespace. Member lookup spells a member by its
    /// class (`Cls::m`) without a file, and `fn_by_scope` answers only per
    /// file, so without this such members were never found.
    internal_members_by_name: FxHashMap<String, Vec<FnId>>,
    /// Lookup contexts retained by coalesced header call records.
    shared_call_tus: FxHashMap<CallSiteId, std::collections::BTreeSet<FileId>>,
    /// Includers contributing each exact expanded header definition.
    shared_header_tus: FxHashMap<FnId, std::collections::BTreeSet<FileId>>,
    /// Headers whose entities were attributed to this TU during lowering
    /// (`#include`d code). Scope resolution consults them so a `static`
    /// inline defined in a header stays visible to its includers after
    /// cross-TU deduplication collapsed the per-TU copies.
    headers_of: FxHashMap<FileId, std::collections::BTreeSet<FileId>>,
    /// `headers_of` inverted: the TUs each header was attributed to. Visibility
    /// of a shared header body asks whether some unit holding it also includes
    /// the querying file, and that question is answered from whichever of the
    /// two sets is smaller only if both directions are indexed. Unordered,
    /// unlike its forward twin: nothing reads it but a membership test.
    includers_of: FxHashMap<FileId, FxHashSet<FileId>>,
    file_by_path: FxHashMap<PathBuf, FileId>,
    /// Canonical dependency roots (`--dep`); empty for a single-tree run.
    dep_roots: Vec<PathBuf>,
    /// `FnId -> slot in functions`. Ids are not dense (merged duplicates and
    /// superseded rows leave gaps), so lookups need this index to stay O(1).
    fn_slots: FxHashMap<FnId, u32>,
    /// Member definitions whose parameter list lacks the implicit `this`: a
    /// body lowered in a unit that never saw its class (the header include
    /// unresolved) merged with the class's in-class prototype. Recorded at
    /// that merge, the only point where both halves are in view.
    members_missing_this: std::collections::BTreeSet<FnId>,
    next_fn: u32,
    next_var: u32,
    next_call: u32,
}

impl SymbolTable {
    /// Declare the dependency roots (canonical) that classify files as they
    /// are interned. Call before any file is added, so every entry is
    /// classified under the same roots.
    pub fn set_dep_roots(&mut self, roots: Vec<PathBuf>) {
        debug_assert!(
            self.files.is_empty(),
            "dependency roots must be set before any file is interned"
        );
        self.dep_roots = roots;
    }

    pub fn dep_roots(&self) -> &[PathBuf] {
        &self.dep_roots
    }

    /// Whether `path` lies under a dependency root. This canonicalizes, so
    /// prefer [`SymbolTable::file_is_dep`] for an already-interned file.
    pub fn path_is_dep(&self, path: &Path) -> bool {
        if self.dep_roots.is_empty() {
            return false;
        }
        let canon = crate::canonicalize(path);
        self.dep_roots.iter().any(|root| canon.starts_with(root))
    }

    /// Whether an interned file lies under a dependency root. O(1).
    pub fn file_is_dep(&self, file: FileId) -> bool {
        self.files.get(file.0 as usize).is_some_and(|f| f.is_dep)
    }

    pub fn add_file(&mut self, path: PathBuf) -> FileId {
        let id = FileId(self.files.len() as u32);
        let is_dep = self.path_is_dep(&path);
        self.files.push(FileInfo { id, path, is_dep });
        id
    }

    /// Intern a file by path: repeated origins (the same header reached
    /// through many TUs) map to one [`FileId`].
    pub fn add_file_interned(&mut self, path: impl AsRef<Path>) -> FileId {
        let path = path.as_ref();
        if let Some(&id) = self.file_by_path.get(path) {
            return id;
        }
        let path = path.to_path_buf();
        let id = self.add_file(path.clone());
        self.file_by_path.insert(path, id);
        id
    }

    pub fn file_by_path(&self, path: &Path) -> Option<FileId> {
        self.file_by_path.get(path).copied()
    }

    /// Register that `header` contributes entities lowered while indexing
    /// `tu` (directly or transitively).
    pub fn register_included_header(&mut self, tu: crate::FileId, header: crate::FileId) {
        if tu != header {
            self.headers_of.entry(tu).or_default().insert(header);
            self.includers_of.entry(header).or_default().insert(tu);
        }
    }

    pub fn included_headers(
        &self,
        tu: crate::FileId,
    ) -> Option<&std::collections::BTreeSet<crate::FileId>> {
        self.headers_of.get(&tu)
    }

    pub fn add_function(&mut self, func: Function) -> FnId {
        self.add_function_with_param_types(func, None, None)
    }

    /// Variant of [`SymbolTable::add_function`] for cross-TU merges. The
    /// incoming function's params are still *unit-local* VarIds whose small
    /// ids can collide with unrelated globals already merged from earlier
    /// TUs, so resolving their types via `param_type` is meaningless and
    /// would always fail the C++ overload signature check — leaving a
    /// prototype (TU A) and its definition (TU B) as two records
    /// (`hpp_designated_dispatch` duplicated `DispatchToMessage`). The
    /// caller (merge) supplies the params' types remapped into global TypeId
    /// space; strict signature separation then also works across TUs.
    /// `merge_unit` reaches this through [`SymbolTable::register_function`],
    /// which is where that caller contract now lives; this wrapper remains for
    /// callers that do not need to know whether the entry adopted the list.
    ///
    /// `types` is the table `param_types` and the existing entries' ids both
    /// live in. It is what lets the C++ overload check compare parameter types
    /// by *shape*: one C type is interned more than once when the units that
    /// contributed it disagreed on how complete a nested tag was, so an id
    /// comparison splits a prototype from its own definition (#83). Without a
    /// table the check falls back to comparing ids, which is exact within a
    /// single unit.
    pub fn add_function_with_param_types(
        &mut self,
        func: Function,
        param_types: Option<&[TypeId]>,
        types: Option<&crate::TypeTable>,
    ) -> FnId {
        self.register_function(func, param_types, types).id
    }

    /// As [`SymbolTable::add_function_with_param_types`], but also reporting
    /// whether the surviving entry took `func`'s parameter list as its own.
    ///
    /// Only this function knows that: which entry a redeclaration merges into
    /// and whether that entry adopts the incoming params are decided by the
    /// branches below, and a caller cannot recover the answer afterwards.
    /// Comparing the survivor's `params` to the list just passed in is not
    /// proof — at merge time the incoming ids are unit-local and can equal an
    /// unrelated list of already-global ids by coincidence, which is how a
    /// later prototype came to disconnect a definition's body from its own
    /// parameters (#83). `merge_unit` needs the answer because an adopted list
    /// is still unit-local and has to be remapped.
    pub fn register_function(
        &mut self,
        func: Function,
        param_types: Option<&[TypeId]>,
        types: Option<&crate::TypeTable>,
    ) -> FnRegistration {
        self.has_weak_symbols |= func.is_weak;
        let mut adopted_params = false;
        if func.linkage == Linkage::External {
            // Find existing candidates under func.name: all entries in
            // externals_by_name (which tracks overloads), falling back to fn_by_name.
            // Candidates under `func.name`, in the order they may be merged
            // into. `externals_by_name` holds every entry under the name
            // (overloads included); `fn_by_name` holds only the primary, so
            // consulting it alone made a definition miss its own declaration
            // whenever another overload had been registered after it (#83).
            let bucket = self.externals_by_name.get(&func.name);
            let primary = if func.target.is_some() {
                bucket
                    .into_iter()
                    .flatten()
                    .copied()
                    .find(|&id| self.function(id).target == func.target)
            } else {
                self.fn_by_name.get(&func.name).copied()
            };
            let candidates: Vec<FnId> = if func.is_cpp && func.is_defined {
                // A C++ definition tries UNDEFINED prototypes first, to reunite
                // with its declaration, and only then the primary entry (for
                // exact duplicate-definition dedup). It must not search across
                // distinct defined overloads, which are separate bodies — nor,
                // under link metadata, across entries from another image.
                //
                // Inside an image a *defined* entry is a candidate too: one
                // symbol per signature is linked there, so a strong definition
                // has to reach the weak definition it supersedes. Restricting
                // this to undefined entries let the primary — which under
                // scoping is merely the first entry of the name in the image,
                // often an unrelated overload — be the only defined candidate,
                // and a strong `hook(double)` registered behind a `hook(int)`
                // never found the weak `hook(double)` to override. Signature
                // matching still separates genuine overloads.
                let scoped = func.target.is_some();
                let mut list: Vec<FnId> = bucket
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&id| {
                        self.function_by_id(id)
                            .is_some_and(|f| f.target == func.target && (scoped || !f.is_defined))
                    })
                    .collect();
                if let Some(primary) = primary {
                    if !list.contains(&primary) {
                        list.push(primary);
                    }
                }
                list
            } else {
                // Everything else — a C++ declaration, or any C entry — may
                // merge into any existing entry under the name.
                match bucket {
                    Some(ids) => ids.clone(),
                    None => primary.into_iter().collect(),
                }
            };
            // Run twice. The first pass demands that a mixed-language pair
            // agree on their resolvable parameter types as well as their
            // arity; the second is the historical arity-only rule. Two passes
            // rather than one stricter rule because this only decides WHICH
            // arity-compatible candidate is taken, never whether any is: an
            // incoming pure-C definition scans the whole overload bucket now,
            // so with two same-arity C++ prototypes under one name it used to
            // take whichever was inserted first regardless of signature.
            let compatible = |existing_id: FnId, require_types: bool| {
                self.redeclaration_compatible(&func, param_types, types, existing_id, require_types)
            };
            // Only a mixed-language pair can answer the two passes
            // differently -- it is the sole case that returns before
            // consulting types -- so the fallback scan is restricted to those
            // candidates rather than repeating identical work for a bucket of
            let mixed_language =
                |id: FnId| !(func.is_cpp && self.function_by_id(id).is_some_and(|e| e.is_cpp));
            let is_incompatible_def = |existing_id: FnId| -> bool {
                if let Some(existing) = self.function_by_id(existing_id) {
                    if func.is_cpp && existing.is_cpp && func.is_defined && existing.is_defined {
                        let is_same_def =
                            existing.file == func.file && existing.span.line == func.span.line;
                        let has_weak = existing.is_weak || func.is_weak;
                        if !is_same_def && !has_weak {
                            return true;
                        }
                    }
                }
                false
            };
            let matched_id = candidates
                .iter()
                .copied()
                .find(|&id| compatible(id, true) && !is_incompatible_def(id))
                .or_else(|| {
                    candidates.iter().copied().find(|&id| {
                        mixed_language(id) && compatible(id, false) && !is_incompatible_def(id)
                    })
                });
            if let Some(existing_id) = matched_id {
                let mut missing_this = false;
                if let Some(existing) = self.function_mut_by_id(existing_id) {
                    missing_this = joins_member_missing_this(existing, &func);
                    // Deduplication selects the first-encountered definition for C++;
                    // C keeps its historical overwrite for unscoped programs.
                    // Within an image, precedence decides.
                    let overwrite = if func.is_cpp && existing.is_cpp {
                        definition_supersedes(existing.is_defined, existing.is_weak, func.is_weak)
                    } else {
                        func.target.is_none()
                            || definition_supersedes(
                                existing.is_defined,
                                existing.is_weak,
                                func.is_weak,
                            )
                    };
                    if func.is_defined && overwrite {
                        existing.is_weak =
                            func.is_weak || (func.target.is_none() && existing.is_weak);
                        existing.is_defined = true;
                        existing.owner_unresolved = func.owner_unresolved;
                        existing.file = func.file;
                        existing.span = func.span;
                        existing.end_line = func.end_line;
                        existing.tu = func.tu;
                        if !func.params.is_empty() {
                            existing.params = func.params.clone();
                            adopted_params = true;
                        }
                    } else if existing.params.is_empty() && !func.params.is_empty() {
                        existing.params = func.params.clone();
                        adopted_params = true;
                    }
                    absorb_redeclaration(existing, &func, param_types, adopted_params);
                    existing.declared_in_class |= func.declared_in_class;
                    // A C definition merging into a C++-parsed header
                    // prototype must drop `is_cpp`. Otherwise a later
                    // TU merge sees both_cpp and refuses the body
                    // (param TypeIds are still unit-local, so the
                    // overload type-check always fails). Only a DEFINITION
                    // may do this: a mere C declaration reaching a C++ body
                    // would otherwise strip that body's overload identity.
                    if func.is_defined {
                        existing.is_cpp = existing.is_cpp && func.is_cpp;
                    }
                }
                if missing_this {
                    self.members_missing_this.insert(existing_id);
                }
                let bucket = self.externals_by_name.entry(func.name.clone()).or_default();
                if !bucket.contains(&existing_id) {
                    bucket.push(existing_id);
                }
                // Same rule as the fresh-entry branch below: a defined entry
                // takes the primary slot, and an undefined one only claims it
                // while no defined entry holds it. Testing `contains_key` here
                // instead left `fn_by_name` on an undefined overload even after
                // a declaration merged into a defined entry. The definedness
                // passed is the incoming registration's, deliberately -- see
                // `should_take_primary` and
                // `a_declaration_does_not_move_the_primary_slot_between_two_bodies`.
                if Self::should_take_primary(
                    self.fn_by_name
                        .get(&func.name)
                        .and_then(|&id| self.function_by_id(id)),
                    func.is_defined,
                ) {
                    self.fn_by_name.insert(func.name.clone(), existing_id);
                }
                return FnRegistration {
                    id: existing_id,
                    adopted_params,
                };
            }
            if Self::should_take_primary(
                self.fn_by_name
                    .get(&func.name)
                    .and_then(|&id| self.function_by_id(id)),
                func.is_defined,
            ) {
                self.fn_by_name.insert(func.name.clone(), func.id);
            }
            self.externals_by_name
                .entry(func.name.clone())
                .or_default()
                .push(func.id);
        }
        if func.linkage == Linkage::Internal {
            // Merge forward declarations with definitions for internal
            // (static) functions. Without this, a forward declaration
            // lower bounds before its definition creates a separate entry;
            // call-sites resolved between the two point at the declaration
            // (is_defined=false) and the solver never expands its body.
            //
            // A C++ function with internal linkage can be overloaded
            // (`static void f(int)` beside `f(double)`, or a member of a class
            // in an anonymous namespace), and merging on file and name alone
            // gave the overloads one entry holding every body. A C++ entry
            // merges only into a redeclaration of it, found as an external
            // one is; C keeps one function per file and name.
            //
            // A C++ declaration, its definition and its overloads share the
            // translation unit, headers included: a header's
            // `static void f(int);` beside the `.cpp`'s `f(double)`, or a
            // header's anonymous-namespace class member defined in the `.cpp`.
            let entries = self.fn_by_scope.get(&func.name).map(Vec::as_slice);
            // Same run, same rule as the replacement search below: the
            // bucket is sorted, so the entries for this file are found
            // rather than scanned for.
            let own_file = entries
                .map(|entries| file_run(entries, func.file).1)
                .unwrap_or_default()
                .iter()
                .find(|(_, id)| {
                    self.function(*id).target == func.target
                        && (!func.is_cpp
                            || self.function(*id).tu == func.tu
                            || (!self.function(*id).is_defined && func.is_defined))
                })
                .map(|(_, id)| *id);
            let scoped = own_file.or_else(|| {
                if !func.is_cpp {
                    return None;
                }
                self.first_in_scope(func.file, entries, |id| {
                    self.function_by_id(id).is_some_and(|e| {
                        e.is_cpp
                            && e.target == func.target
                            && (e.tu == func.tu || (!e.is_defined && func.is_defined))
                    })
                })
            });
            let overloads = func.is_cpp
                && scoped.is_some_and(|id| self.function_by_id(id).is_some_and(|e| e.is_cpp));
            let compatible = match scoped {
                Some(primary) if overloads => std::iter::once(primary)
                    .chain(
                        self.scope_overloads
                            .get(&primary)
                            .into_iter()
                            .flatten()
                            .copied(),
                    )
                    .find(|&id| self.redeclaration_compatible(&func, param_types, types, id, true)),
                _ => scoped,
            };
            // A header's entry is every including unit's, and so is each
            // overload a unit's `.cpp` adds to it: a free function found
            // through a header is not folded into one, whether it is the
            // header's declaration or another unit's `static void f(double)`.
            // It stays this unit's function, and the unit merge joins the
            // unit's calls to it (`respelled_declarations`). A class member's
            // prototype does take its definition, as an external member's
            // does, and keeps its `virtual`.
            let shared_declaration = own_file.is_none()
                && compatible.is_some_and(|id| {
                    self.function_by_id(id).is_some_and(|e| {
                        // The same header text lowered once more for the unit
                        // is that entry, not another function.
                        let same_text =
                            e.span.file == func.span.file && e.span.line == func.span.line;
                        !e.declared_in_class && !same_text
                    })
                });
            let redeclared = compatible.filter(|_| !shared_declaration);
            if let Some(existing_id) = redeclared {
                if let Some(existing) = self.function_mut_by_id(existing_id) {
                    if func.is_defined && !existing.is_defined {
                        existing.is_defined = true;
                        existing.file = func.file;
                        existing.span = func.span;
                        existing.end_line = func.end_line;
                        existing.tu = func.tu;
                        if !func.params.is_empty() {
                            existing.params = func.params.clone();
                            adopted_params = true;
                        }
                    } else if !func.is_defined
                        && existing.params.is_empty()
                        && !func.params.is_empty()
                    {
                        existing.params = func.params.clone();
                        adopted_params = true;
                    }
                    absorb_redeclaration(existing, &func, param_types, adopted_params);
                    let becomes_member = func.declared_in_class && !existing.declared_in_class;
                    existing.declared_in_class |= func.declared_in_class;
                    if becomes_member {
                        self.internal_members_by_name
                            .entry(func.name.clone())
                            .or_default()
                            .push(existing_id);
                    }
                    return FnRegistration {
                        id: existing_id,
                        adopted_params,
                    };
                }
            }
            // Index every internal-linkage entry, declarations included:
            // lowering resolves identifiers against this table *while the
            // file streams in*, so a designated initializer like
            // `.Read = StaticFn` must bind before the definition is lowered.
            // A further overload leaves the name on the first one and joins
            // its overloads.
            // An overload of a member is a member, though its definition,
            // written outside the class, does not say so.
            let joins = scoped.filter(|_| overloads && !shared_declaration);
            let member_overload = joins
                .and_then(|primary| self.function_by_id(primary))
                .is_some_and(|primary| primary.declared_in_class);
            match joins {
                Some(primary) => {
                    self.scope_overloads
                        .entry(primary)
                        .or_default()
                        .push(func.id);
                    self.overload_primary.insert(func.id, primary);
                }
                None => {
                    let replace = self.fn_by_scope.get(&func.name).and_then(|entries| {
                        let (start, run) = file_run(entries, func.file);
                        run.iter()
                            .position(|(_, id)| {
                                self.function(*id).target == func.target
                                    && (!func.is_cpp
                                        || self.function(*id).tu == func.tu
                                        || (!self.function(*id).is_defined && func.is_defined))
                            })
                            .map(|offset| start + offset)
                    });
                    let entries = self.fn_by_scope.entry(func.name.clone()).or_default();
                    match replace.map(|i| &mut entries[i]) {
                        Some(entry) => entry.1 = func.id,
                        None => insert_by_file(entries, func.file, func.id),
                    }
                }
            }
            if func.declared_in_class || member_overload {
                self.internal_members_by_name
                    .entry(func.name.clone())
                    .or_default()
                    .push(func.id);
            }
        }
        // A fresh entry owns the list it arrived with rather than adopting
        // another entry's, so `adopted_params` stays false; the caller
        // recognises this case by the returned id being the one it allocated.
        //
        // A named entry carries the remapped signature types: a unit merge
        // registers functions before remapping their parameter variables, so a
        // later redeclaration or overload compares against this cache.
        let mut func = func;
        if func.linkage != Linkage::None {
            if let Some(ts) = param_types {
                func.param_type_ids = ts.to_vec();
            }
        }
        let id = self.push_indexed(func);
        debug_assert!(
            !adopted_params,
            "a fresh entry owns its parameter list; nothing adopted one"
        );
        FnRegistration { id, adopted_params }
    }

    /// Push a synthesized `Function` (e.g. extern-callee stubs) without
    /// registering it in the name/scope resolution maps, so it cannot shadow
    /// real definitions. Only the id index is maintained.
    pub fn push_synthetic_function(&mut self, func: Function) -> FnId {
        debug_assert!(
            self.externals_by_name.get(&func.name).is_none_or(|ids| ids
                .iter()
                .all(|id| self.function(*id).target != func.target)),
            "synthetic function must not shadow a registered name in its target"
        );
        self.push_indexed(func)
    }

    fn push_indexed(&mut self, func: Function) -> FnId {
        self.has_weak_symbols |= func.is_weak;
        self.has_target_scopes |= func.target.is_some();
        let id = func.id;
        self.fn_slots.insert(id, self.functions.len() as u32);
        self.base_by_name
            .entry(base_name_of(&func.name))
            .or_default()
            .push(id);
        self.functions.push(func);
        id
    }

    /// Slot of `id` in [`SymbolTable::functions`], O(1).
    pub fn function_index(&self, id: FnId) -> Option<usize> {
        self.fn_slots.get(&id).map(|&s| s as usize)
    }

    /// Whether an entry should become the primary `fn_by_name` entry for its
    /// name. Every unqualified call site resolves through that one slot, so a
    /// defined entry always claims it and an undefined one only takes it while
    /// no defined entry holds it -- otherwise a later declaration shadows a
    /// body and the solver never expands it (#83).
    ///
    /// `incoming_is_defined` describes the REGISTRATION, not the entry the
    /// caller would install. The two differ on the merge path, where a
    /// declaration folds into an already-defined entry: passing the merged
    /// entry's definedness there would let that declaration pull the slot off
    /// whichever body currently holds it, making every unqualified call site
    /// depend on declaration placement and unit merge order. A declaration
    /// asserts no body, so it claims the slot only when no body has it.
    fn should_take_primary(current: Option<&Function>, incoming_is_defined: bool) -> bool {
        incoming_is_defined || !current.map(|f| f.is_defined).unwrap_or(false)
    }

    /// Mutable view of `id`, O(1) like [`SymbolTable::function_by_id`].
    /// `push_indexed` is the only writer of `functions`, and it records the
    /// slot, so the map never lags the vector.
    fn function_mut_by_id(&mut self, id: FnId) -> Option<&mut Function> {
        let slot = *self.fn_slots.get(&id)? as usize;
        self.functions.get_mut(slot).filter(|f| f.id == id)
    }

    /// Whether `func` redeclares the entry `existing_id` rather than
    /// overloading it: see the two passes in
    /// [`register_function`](Self::register_function).
    fn redeclaration_compatible(
        &self,
        func: &Function,
        param_types: Option<&[TypeId]>,
        types: Option<&crate::TypeTable>,
        existing_id: FnId,
        require_types: bool,
    ) -> bool {
        // Merge only compatible redeclarations (prototype + definition).
        // Distinct arities mean C++ overloads — and only then: keep
        // both entries so call-site resolution can pick between them.
        //
        // Overload splitting requires *both* sides to be C++. A `.h`
        // reached from a C++ TU is parsed as C++ (`is_cpp`), but the
        // `.c` definition is not. Treating that as an overload (the
        // old `||`) left callers bound to the undefined prototype —
        // HDF `GpioSetIrq` never reached `GpioRegListener`, so
        // `gpio->func` stayed empty. Mixed-language same-name entries
        // still require matching arity when both sides have params,
        // so a coincidental C++ overload is not swallowed.
        let existing_fn = self.function_by_id(existing_id);
        let both_cpp = func.is_cpp && existing_fn.map(|e| e.is_cpp).unwrap_or(false);
        existing_fn
            .map(|existing| {
                if existing.target != func.target {
                    return false;
                }
                if !func.is_cpp && !existing.is_cpp {
                    // Pure C: prototype + definition always collapse.
                    return true;
                }
                // `f(T*&)` and `f(T*&, Args&...)` declare the same
                // explicit arity and are still two overloads.
                let variadic_ok = !both_cpp || existing.variadic == func.variadic;
                let arity_ok = variadic_ok
                    && if existing.params.is_empty() || func.params.is_empty() {
                        // A parameterless side matches any list, except
                        // that two C++ declarations of different arity
                        // are overloads: an in-class prototype lowers no
                        // parameter variables, and `Get()` swallowed
                        // `Get(Mode&)` and its definition's callers. The
                        // counts exclude `this`, so a body lowered
                        // without its class in view still merges.
                        !both_cpp
                            || existing
                                .explicit_arity
                                .zip(func.explicit_arity)
                                .is_none_or(|(a, b)| a == b)
                    } else {
                        existing.params.len() == func.params.len()
                    };
                if !both_cpp && !require_types {
                    // Header parsed as C++ vs `.c` body: merge by
                    // arity and ignore param-type mismatch (typedef
                    // `GpioIrqFunc` vs decayed `Int`). That tolerance
                    // is why this pair needs the first pass in `register_function` --
                    // it is the reason arity alone can be ambiguous.
                    return arity_ok;
                }
                // C++: prototypes and definitions of the *same*
                // function merge; distinct same-arity overloads
                // must stay apart. Parameter types disambiguate.
                // When `param_types` is supplied (cross-TU merge) it
                // holds the remapped global types of `func.params`;
                // otherwise (per-TU) they resolve via `param_type`.
                // The existing side resolves through `param_type_ids`
                // first: during a merge pass all functions register
                // before their param variables are remapped, so the
                // VarId probe comes back `None` for a same-unit
                // predecessor with a *different* signature. A side
                // whose type is unresolvable falls back to arity-only,
                // like the C-vs-C++ path.
                //
                // Resolved pairs compare by SHAPE, not by id: a `.h`
                // prototype and the `.cpp` definition of one function
                // reach this table from two different units, and one C
                // type is interned twice whenever those units
                // disagreed on how complete a nested tag was --
                // `struct HdfRemoteService *` split in HDF because the
                // defining unit had not seen `struct HdfObject`'s
                // fields. An id comparison read that as an overload,
                // left the prototype undefined, and every C caller of
                // the interface stopped at it (#83).
                //
                // Shape comparison reunites a declaration with its
                // definition across units. It must NOT join two
                // DEFINITIONS: camera declares the same class in
                // unrelated fuzzer targets (two headers each define
                // `IStreamOperatorMock::Capture`) and puts a test mock
                // beside the production implementation
                // (`DeferredVideoProcessingSessionCallback::OnError`).
                // Those are distinct bodies that merely share a
                // qualified name, and folding them together lets the
                // second overwrite the survivor's span and parameters
                // and evict the first body's facts -- the same hazard
                // `base_definitions` guards in trace-parse's merge. A
                // pair of definitions therefore keeps the exact id
                // comparison.
                let by_shape = types.filter(|_| !(existing.is_defined && func.is_defined));
                arity_ok
                    && (existing.params.is_empty() || func.params.is_empty() || {
                        let existing_t = |i: usize| {
                            existing
                                .param_type_ids
                                .get(i)
                                .copied()
                                .or_else(|| self.param_type(existing.params[i]))
                        };
                        let ok = existing.params.iter().enumerate().all(|(i, _)| {
                            let incoming_t = param_types
                                .and_then(|ts| ts.get(i))
                                .copied()
                                .or_else(|| func.params.get(i).and_then(|&p| self.param_type(p)));
                            match (existing_t(i), incoming_t) {
                                (Some(ta), Some(tb)) => match by_shape {
                                    Some(types) => crate::same_param_type(types, ta, tb),
                                    None => ta == tb,
                                },
                                _ => true,
                            }
                        });
                        ok
                    })
            })
            .unwrap_or(false)
    }

    /// Type of a parameter variable, for overload signature comparison.
    fn param_type(&self, var: VarId) -> Option<TypeId> {
        // Variables usually sit at their id's slot; a unit can push them in
        // another order than it allocated their ids.
        self.variable_by_id(var)
            .or_else(|| self.variables.iter().find(|v| v.id == var))
            .map(|v| v.type_id)
    }

    pub fn has_weak_symbols(&self) -> bool {
        self.has_weak_symbols
    }

    /// Record a weak pragma applied after the declaration was registered.
    pub fn mark_has_weak_symbols(&mut self) {
        self.has_weak_symbols = true;
    }

    pub fn target_global(&self, target: crate::TargetId, name: &str) -> Option<VarId> {
        self.target_globals.get(&target)?.get(name).copied()
    }

    pub fn add_variable(&mut self, var: Variable) -> VarId {
        self.has_weak_symbols |= var.is_weak;
        let id = var.id;
        match var.storage {
            StorageClass::Global => match var.target {
                // A namespaced global does not claim the image's binding for
                // its unqualified name; an unrelated `::counter` would
                // otherwise unify with `ns::counter`.
                Some(target) if !var.is_namespaced => {
                    self.target_globals
                        .entry(target)
                        .or_default()
                        .entry(var.name.clone())
                        .or_insert(id);
                }
                Some(_) => {}
                // `global_by_name` is the unscoped index, and every image holds
                // its own copy of a shared global. Letting a scoped copy
                // overwrite the entry left it pointing at whichever target
                // merged last, so an unscoped caller resolving through it
                // inherited an arbitrary image's variable.
                None => {
                    self.global_by_name.insert(var.name.clone(), id);
                }
            },
            StorageClass::FileStatic => {
                let entries = self
                    .file_statics_by_name
                    .entry(var.name.clone())
                    .or_default();
                // First registration of a name in a file wins.
                if file_run(entries, var.span.file).1.is_empty() {
                    insert_by_file(entries, var.span.file, id);
                }
            }
            _ => {}
        }
        self.variables.push(var);
        id
    }

    /// The file-`static` variable `name` code in `file` sees: defined in
    /// `file`, else in a header it includes.
    pub fn file_static_named(&self, file: FileId, name: &str) -> Option<VarId> {
        let entries = self.file_statics_by_name.get(name).map(Vec::as_slice);
        self.first_in_scope(file, entries, |_| true)
    }

    pub fn alloc_fn_id(&mut self) -> FnId {
        let id = FnId(self.next_fn);
        self.next_fn += 1;
        id
    }

    pub fn alloc_var_id(&mut self) -> VarId {
        let id = VarId(self.next_var);
        self.next_var += 1;
        id
    }

    pub fn alloc_call_id(&mut self) -> CallSiteId {
        let id = CallSiteId(self.next_call);
        self.next_call += 1;
        id
    }

    pub fn resolve_function(&self, name: &str) -> Option<FnId> {
        self.fn_by_name.get(name).copied()
    }

    /// The further overloads of the internal-linkage entry `id` that code in
    /// `file` sees, which a name resolving to `id` there may also mean. A
    /// header's entry is every including unit's, and an overload one unit's
    /// `.cpp` adds to it is not another unit's.
    pub fn internal_overloads_seen_from(
        &self,
        id: FnId,
        file: FileId,
    ) -> impl Iterator<Item = FnId> + '_ {
        // `id` may be any member of its overload set.
        let primary = self.overload_primary.get(&id).copied().unwrap_or(id);
        let further = self
            .scope_overloads
            .get(&primary)
            .into_iter()
            .flatten()
            .copied();
        std::iter::once(primary)
            .filter(move |&primary| primary != id && self.function_visible_from(primary, file))
            .chain(further.filter(move |&overload| {
                overload != id && self.function_visible_from(overload, file)
            }))
    }

    /// Pass each function argument's further internal-linkage overloads, as
    /// the call's file sees them, at its position too: the name may mean any
    /// of them.
    pub fn pass_internal_overloads_as_args(&mut self) {
        let added: Vec<(usize, Vec<(u32, FnId)>)> = self
            .call_sites
            .iter()
            .enumerate()
            .filter(|(_, site)| {
                site.fn_args.iter().any(|(_, callee)| {
                    self.scope_overloads.contains_key(callee)
                        || self.overload_primary.contains_key(callee)
                })
            })
            .map(|(i, site)| {
                let mut more: Vec<(u32, FnId)> = Vec::new();
                for &(index, callee) in &site.fn_args {
                    for overload in self.internal_overloads_seen_from(callee, site.span.file) {
                        let arg = (index, overload);
                        if !site.fn_args.contains(&arg) && !more.contains(&arg) {
                            more.push(arg);
                        }
                    }
                }
                (i, more)
            })
            .collect();
        for (i, more) in added {
            self.call_sites[i].fn_args.extend(more);
        }
    }

    /// Whether any internal-linkage entry has further overloads.
    pub fn has_internal_overloads(&self) -> bool {
        !self.scope_overloads.is_empty()
    }

    /// The nearest entry of a name that code in `file` sees and `accept`
    /// takes: the one in `file` itself, else the one in the lowest-numbered
    /// header `file` includes. What [`in_scope`](Self::in_scope) would list
    /// first, without building the list: this runs per call site.
    /// `entries` must be sorted by FileId, with stable equal-file ordering.
    fn first_in_scope<T: Copy>(
        &self,
        file: FileId,
        entries: Option<&[(FileId, T)]>,
        accept: impl Fn(T) -> bool,
    ) -> Option<T> {
        let entries = entries?;
        // Own-file entries are one contiguous run, and the header entries
        // ascend, so the first accepted header entry is the lowest-numbered
        // one: neither branch has to look at the whole bucket.
        if let Some(&(_, value)) = file_run(entries, file).1.iter().find(|(_, v)| accept(*v)) {
            return Some(value);
        }
        let headers = self.headers_of.get(&file)?;
        entries
            .iter()
            .find(|(f, v)| headers.contains(f) && accept(*v))
            .map(|(_, v)| *v)
    }

    /// The entries of a name that code in `file` sees, nearest first: the
    /// one in `file` itself, then those in the headers it includes in
    /// ascending file order.
    fn in_scope<T: Copy>(&self, file: FileId, entries: Option<&[(FileId, T)]>) -> Vec<(FileId, T)> {
        let Some(entries) = entries else {
            return Vec::new();
        };
        let headers = self.headers_of.get(&file);
        let mut seen: Vec<(FileId, T)> = entries
            .iter()
            .filter(|(f, _)| *f == file || headers.is_some_and(|h| h.contains(f)))
            .copied()
            .collect();
        seen.sort_by_key(|(f, _)| (*f != file, *f));
        seen
    }

    /// Record the TUs contributing an identical expanded header definition.
    pub fn share_header_function(&mut self, id: FnId, tu: FileId) {
        self.shared_header_tus.entry(id).or_default().insert(tu);
    }

    /// Retain lookup contexts when identical header call records coalesce.
    pub fn share_header_call(&mut self, id: CallSiteId, tu: FileId) {
        self.shared_call_tus.entry(id).or_default().insert(tu);
    }

    pub fn is_shared_header_function(&self, id: FnId) -> bool {
        self.shared_header_tus.contains_key(&id)
    }

    /// Whether a reference in this file can see the function, respecting the
    /// contributing TU set of shared definitions.
    pub fn function_visible_from(&self, id: FnId, file: FileId) -> bool {
        self.function_by_id(id).is_some_and(|f| {
            if f.linkage != Linkage::Internal {
                return true;
            }
            if f.is_cpp {
                // A shared body is visible in every contributing unit and the
                // files those units include (docs/ANALYSIS.md).
                if let Some(tus) = self.shared_header_tus.get(&id) {
                    return f.file == file || self.any_unit_sees(tus, file);
                }
                if f.tu.is_some_and(|tu| !self.file_sees(tu, file)) {
                    return false;
                }
            }
            self.file_sees(file, f.file)
        })
    }

    /// [`SymbolTable::file_sees`] lifted over a set of units: whether any of
    /// `tus` is `file` or includes it.
    ///
    /// Walking `tus` would cost one probe per unit, and a header carrying a
    /// shared body can have thousands. `file`'s own includers answer the same
    /// question, so the shorter side is walked: a body shared by the whole
    /// tree is tested against the few units that include the querying file
    /// rather than the other way round.
    fn any_unit_sees(&self, tus: &std::collections::BTreeSet<FileId>, file: FileId) -> bool {
        if tus.contains(&file) {
            return true;
        }
        let Some(includers) = self.includers_of.get(&file) else {
            return false;
        };
        // The two sets are different types, so each direction spells its own
        // walk; both ask exactly `includers ∩ tus ≠ ∅`.
        if includers.len() <= tus.len() {
            includers.iter().any(|tu| tus.contains(tu))
        } else {
            tus.iter().any(|tu| includers.contains(tu))
        }
    }

    /// Whether code in `file` sees what `other` defines: `other` is `file`, or
    /// a header `file` includes.
    pub fn file_sees(&self, file: FileId, other: FileId) -> bool {
        file == other
            || self
                .headers_of
                .get(&file)
                .is_some_and(|headers| headers.contains(&other))
    }

    /// The TUs a function stands for: every includer of a shared header body,
    /// otherwise the one unit that lowered it.
    fn contributing_tus(&self, id: FnId) -> impl Iterator<Item = FileId> + '_ {
        let shared = self.shared_header_tus.get(&id);
        let own = shared.is_none().then(|| {
            let f = self.function(id);
            f.tu.unwrap_or(f.file)
        });
        shared.into_iter().flatten().copied().chain(own)
    }

    /// Whether this definition represents the given TU, including shared bodies.
    fn defined_in_tu(&self, id: FnId, tu: FileId) -> bool {
        self.function(id).is_defined && self.contributing_tus(id).any(|t| t == tu)
    }

    /// Resolve a name-based return fact in every context represented by its
    /// caller. A shared header body unions its includers' bindings, while an
    /// ordinary body keeps the TU-local definition precedence.
    pub fn return_flow_candidates(&self, caller: FnId, name: &str) -> Vec<FnId> {
        let function = self.function(caller);
        // `(tu, lookup file)` per context: a shared body looks up from each
        // includer, an ordinary one from its own file.
        let shared = self.is_shared_header_function(caller);
        let contexts: Vec<(FileId, FileId)> = self
            .contributing_tus(caller)
            .map(|tu| (tu, if shared { tu } else { function.file }))
            .collect();
        let mut result = Vec::new();
        for (tu, file) in contexts {
            let mut candidates =
                self.resolve_function_candidates_in_target(name, Some(file), function.target);
            if candidates.iter().any(|&c| self.defined_in_tu(c, tu)) {
                candidates.retain(|&c| self.defined_in_tu(c, tu));
            }
            push_unique(&mut result, candidates);
        }
        result
    }

    /// Take the member definitions whose parameters lack the implicit `this`
    /// (see `members_missing_this`), leaving none recorded.
    pub fn take_members_missing_this(&mut self) -> std::collections::BTreeSet<FnId> {
        std::mem::take(&mut self.members_missing_this)
    }

    /// The functions a call site reaches without points-to, in the merged
    /// program: the callee lowering bound, else its name resolved scope-first
    /// for a direct site, or over every candidate for a site recovered by
    /// name. The solver wires exactly these, so anything that has to agree
    /// with its wiring asks here.
    pub fn callees_of(&self, cs: &CallSite) -> Vec<FnId> {
        self.callees_of_with_types(cs, None)
    }

    pub fn callees_of_with_types(
        &self,
        cs: &CallSite,
        types: Option<&crate::TypeTable>,
    ) -> Vec<FnId> {
        if let Some(tus) = self.shared_call_tus.get(&cs.id) {
            let mut result = Vec::new();
            for &tu in tus {
                push_unique(&mut result, self.callees_in_tu(cs, types, tu));
            }
            return result;
        }
        let caller_tu = cs
            .tu
            .or_else(|| self.function_by_id(cs.caller).and_then(|f| f.tu))
            .unwrap_or(cs.span.file);
        self.callees_in_tu(cs, types, caller_tu)
    }

    fn callees_in_tu(
        &self,
        cs: &CallSite,
        types: Option<&crate::TypeTable>,
        caller_tu: FileId,
    ) -> Vec<FnId> {
        // Both resolvers fall back to whole-program interpretation when
        // nothing is scoped, so one tail serves either mode.
        let target = self.caller_target(cs);
        let resolve_equal_defs = |fid: FnId| -> Vec<FnId> {
            let f = self.function(fid);
            if f.linkage != Linkage::External || self.defined_in_tu(fid, caller_tu) {
                return vec![fid];
            }
            // Nothing else can be named, so nothing else can be an equal
            // definition: skip the candidate walk entirely.
            if self.is_sole_binding_of_name(fid, &f.name) {
                return vec![fid];
            }
            let f_skip = usize::from(self.has_this_param(fid));
            let f_explicit_len = f.params.len().saturating_sub(f_skip);
            let defs: Vec<FnId> = self
                .resolve_function_candidates_in_target(&f.name, Some(caller_tu), target)
                .into_iter()
                .filter(|&id| {
                    let cand = self.function(id);
                    if !cand.is_defined || cand.variadic != f.variadic {
                        return false;
                    }
                    let cand_skip = usize::from(self.has_this_param(id));
                    let cand_explicit_len = cand.params.len().saturating_sub(cand_skip);
                    let both_cpp = f.is_cpp && cand.is_cpp;
                    if f.params.is_empty() || cand.params.is_empty() {
                        if !both_cpp {
                            return true;
                        }
                        let f_arity = f.explicit_arity.or(Some(f_explicit_len as u32));
                        let cand_arity = cand.explicit_arity.or(Some(cand_explicit_len as u32));
                        return f_arity.zip(cand_arity).is_none_or(|(a, b)| a == b);
                    }
                    if cand_explicit_len != f_explicit_len {
                        return false;
                    }
                    (0..cand_explicit_len).all(|i| {
                        let p1 = cand.params[cand_skip + i];
                        let p2 = f.params[f_skip + i];
                        let t1 = self
                            .param_type(p1)
                            .or_else(|| cand.param_type_ids.get(cand_skip + i).copied());
                        let t2 = self
                            .param_type(p2)
                            .or_else(|| f.param_type_ids.get(f_skip + i).copied());
                        match (t1, t2) {
                            (Some(a), Some(b)) => match types {
                                Some(ty) => crate::same_param_type(ty, a, b),
                                None => a == b,
                            },
                            _ => true,
                        }
                    })
                })
                .collect();
            if defs.len() > 1 {
                if let Some(&local_def) = defs.iter().find(|&&id| self.defined_in_tu(id, caller_tu))
                {
                    return vec![local_def];
                }
                return defs;
            }
            if !defs.is_empty() {
                return defs;
            }
            vec![fid]
        };
        if let Some(fid) = cs.callee_fn_id {
            if !self.has_target_scopes || self.function(fid).target == target {
                if !cs.is_direct {
                    return vec![fid];
                }
                return resolve_equal_defs(fid);
            }
        }
        if cs.is_direct {
            let direct =
                self.resolve_function_in_scope_in_target(&cs.callee_name, Some(caller_tu), target);
            if let Some(fid) = direct {
                return resolve_equal_defs(fid);
            }
            Vec::new()
        } else if cs.resolves_by_name() {
            self.resolve_function_candidates_in_target(&cs.callee_name, Some(caller_tu), target)
        } else {
            Vec::new()
        }
    }

    /// The image a call site resolves in: its caller's, or none at all when
    /// no link metadata scoped the program.
    fn caller_target(&self, cs: &CallSite) -> Option<crate::TargetId> {
        self.has_target_scopes
            .then(|| self.function(cs.caller).target)
            .flatten()
    }

    /// Whether `id` is visible to a lookup confined to `scope`.
    fn in_scope_of(&self, id: FnId, scope: TargetScope) -> bool {
        match scope {
            TargetScope::Any => true,
            TargetScope::Image(target) => {
                self.function_by_id(id).is_some_and(|f| f.target == target)
            }
        }
    }

    /// The image a lookup is confined to, or `Any` when it is unconstrained.
    ///
    /// `Image(None)` is not the same question as `Any`: it names the partition
    /// of sources no link target claimed, and matches only symbols in it.
    /// Collapsing the two makes every unconstrained query miss every
    /// target-scoped symbol as soon as any link metadata exists.
    fn image_of(&self, target: Option<crate::TargetId>) -> TargetScope {
        if self.has_target_scopes {
            TargetScope::Image(target)
        } else {
            TargetScope::Any
        }
    }

    /// The one entry a direct call written in `file` binds to. A direct site
    /// stays a single edge whether or not link metadata exists, so it never
    /// widens to the candidate set below.
    pub fn resolve_function_in_scope(
        &self,
        name: &str,
        file: Option<crate::FileId>,
    ) -> Option<FnId> {
        self.first_in_image(name, file, TargetScope::Any)
    }

    /// [`resolve_function_in_scope`](Self::resolve_function_in_scope) confined
    /// to one link image.
    pub fn resolve_function_in_scope_in_target(
        &self,
        name: &str,
        file: Option<FileId>,
        target: Option<crate::TargetId>,
    ) -> Option<FnId> {
        self.first_in_image(name, file, self.image_of(target))
    }

    fn first_in_image(&self, name: &str, file: Option<FileId>, scope: TargetScope) -> Option<FnId> {
        if let Some(file) = file {
            let entries = self.fn_by_scope.get(name).map(Vec::as_slice);
            if let Some(id) = self.first_in_scope(file, entries, |id| {
                self.in_scope_of(id, scope) && self.function_visible_from(id, file)
            }) {
                return Some(id);
            }
        }
        if let Some(id) = self
            .fn_by_name
            .get(name)
            .copied()
            .filter(|&id| self.in_scope_of(id, scope))
        {
            return Some(id);
        }
        // `fn_by_name` holds one entry per name across the whole program --
        // the last definition of it, whichever image that came from -- so
        // under scoping it can hold another image's entry and hide this one.
        // The per-name bucket is the complete list.
        let candidates = matches!(scope, TargetScope::Image(_))
            .then(|| self.externals_by_name.get(name))
            .flatten()?;
        let in_scope = || {
            candidates
                .iter()
                .copied()
                .filter(|&id| self.in_scope_of(id, scope))
        };
        // The bucket is in registration order, which the primary slot
        // deliberately is not: `should_take_primary` keeps a body ahead of the
        // declarations of its name. Taking the bucket's first entry dropped
        // that rule inside an image, so a prototype registered before the body
        // -- another overload, or a header the image did not merge -- answered
        // for every call site and the solver never expanded the body.
        in_scope()
            .find(|&id| self.function(id).is_defined)
            .or_else(|| in_scope().next())
    }

    /// All functions a post-merge name lookup may refer to.
    ///
    /// Name-based facts (`CallReturn`, `ReturnFlow::Call`, recovered direct
    /// calls) lose the calling TU's visibility context at merge time, so a
    /// name that matches both a file-`static` definition and an external
    /// definition is genuinely ambiguous there. Per may-analysis semantics
    /// (over-approximate when uncertain) callers must consider every
    /// candidate. Paths that preserved callee ids through lowering + merge
    /// should use those ids directly instead — they are exact.
    pub fn resolve_function_candidates(
        &self,
        name: &str,
        file: Option<crate::FileId>,
    ) -> Vec<FnId> {
        self.candidates_in_image(name, file, TargetScope::Any)
    }

    /// [`resolve_function_candidates`](Self::resolve_function_candidates)
    /// confined to one link image.
    pub fn resolve_function_candidates_in_target(
        &self,
        name: &str,
        file: Option<FileId>,
        target: Option<crate::TargetId>,
    ) -> Vec<FnId> {
        self.candidates_in_image(name, file, self.image_of(target))
    }

    /// Is `fid` the only function any lookup of `name` can reach — no
    /// internal-linkage entry anywhere and the one external binding? Then a
    /// name lookup cannot produce an alternative definition of it. True for
    /// prototype-only bindings too, which have no body to choose between.
    fn is_sole_binding_of_name(&self, fid: FnId, name: &str) -> bool {
        !self.fn_by_scope.contains_key(name)
            && self
                .externals_by_name
                .get(name)
                .is_some_and(|ids| ids.as_slice() == [fid])
    }

    fn candidates_in_image(
        &self,
        name: &str,
        file: Option<FileId>,
        scope: TargetScope,
    ) -> Vec<FnId> {
        // Most probes miss — a scope walk asks about every enclosing scope —
        // so the vector stays unallocated until something is found.
        let mut out = CandidateSet::default();
        if let Some(file) = file {
            for (_, id) in self.in_scope(file, self.fn_by_scope.get(name).map(Vec::as_slice)) {
                if !self.in_scope_of(id, scope) || !self.function_visible_from(id, file) {
                    continue;
                }
                // Two scope entries of one overload set share a primary, so
                // they surface the same further overloads. Each id is listed
                // once: a repeat would wire the same callee edge twice.
                let overloads = self.internal_overloads_seen_from(id, file);
                out.push(id);
                for id in overloads {
                    if self.in_scope_of(id, scope) {
                        out.push(id);
                    }
                }
            }
        }
        if let Some(&id) = self.fn_by_name.get(name) {
            if self.in_scope_of(id, scope) {
                out.push(id);
            }
        }
        // C++ overloads: additional entries under the same name that the
        // one-slot-per-name `fn_by_name` table hides. Unioned with the scope entries
        // rather than used as a fallback — a name matching both a
        // file-`static` definition and the image's external definition is
        // genuinely ambiguous, and a may-analysis expands both.
        if let Some(bucket) = self.externals_by_name.get(name) {
            for &id in bucket {
                if self.in_scope_of(id, scope) {
                    out.push(id);
                }
            }
        }
        out.into_vec()
    }

    /// Every external entry declared or defined under `name` (overloads
    /// included), in declaration order, then every internal member of a class
    /// under it. Other internal-linkage functions live in the per-file scope
    /// table and are not consulted.
    pub fn functions_named(&self, name: &str) -> Vec<FnId> {
        let internal = self
            .internal_members_by_name
            .get(name)
            .into_iter()
            .flatten();
        self.externals_by_name
            .get(name)
            .into_iter()
            .flatten()
            .chain(internal)
            .copied()
            .collect()
    }

    /// Whether [`functions_named`](Self::functions_named) finds any entry,
    /// without cloning the overload set.
    pub fn has_function_named(&self, name: &str) -> bool {
        self.externals_by_name
            .get(name)
            .is_some_and(|ids| !ids.is_empty())
            || self
                .internal_members_by_name
                .get(name)
                .is_some_and(|ids| !ids.is_empty())
    }

    /// Functions whose fully-qualified name is exactly `namespace::name`.
    /// A declaration may spell the qualification with or without a leading
    /// `::` (`::ns::swap` and `ns::swap` both match namespace `ns`); the
    /// empty namespace covers global functions, spelled `name` or `::name`.
    pub fn functions_in_namespace(&self, namespace: &str, name: &str) -> Vec<FnId> {
        self.base_by_name
            .get(name)
            .map(|bucket| {
                let qualified = if namespace.is_empty() {
                    name.to_string()
                } else {
                    format!("{namespace}::{name}")
                };
                let qualified_leading = format!("::{qualified}");
                bucket
                    .iter()
                    .copied()
                    .filter(|id| {
                        self.function_by_id(*id)
                            .is_some_and(|f| f.name == qualified || f.name == qualified_leading)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn function_by_id(&self, id: FnId) -> Option<&Function> {
        let slot = *self.fn_slots.get(&id)?;
        self.functions.get(slot as usize).filter(|f| f.id == id)
    }

    pub fn function(&self, id: FnId) -> &Function {
        self.function_by_id(id)
            .unwrap_or_else(|| panic!("unknown function id {}", id.0))
    }

    pub fn has_this_param(&self, fid: FnId) -> bool {
        let f = self.function(fid);
        f.is_cpp
            && f.params
                .first()
                .is_some_and(|&p| self.variable_by_id(p).map(|v| v.name.as_str()) == Some("this"))
    }

    pub fn variable_by_id(&self, id: VarId) -> Option<&Variable> {
        self.variables.get(id.0 as usize).filter(|v| v.id == id)
    }

    pub fn variable(&self, id: VarId) -> &Variable {
        self.variable_by_id(id)
            .unwrap_or_else(|| panic!("unknown variable id {}", id.0))
    }

    pub fn variable_mut(&mut self, id: VarId) -> &mut Variable {
        self.variables
            .get_mut(id.0 as usize)
            .filter(|v| v.id == id)
            .unwrap_or_else(|| panic!("unknown variable id {}", id.0))
    }

    #[must_use]
    pub fn call_site_by_id(&self, id: CallSiteId) -> Option<&CallSite> {
        self.call_sites
            .get(id.0 as usize)
            .filter(|c| c.id == id)
            .or_else(|| self.call_sites.iter().find(|c| c.id == id))
    }

    pub fn function_ids_unique(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        self.functions.iter().all(|f| seen.insert(f.id))
    }
}

/// The entries of a name-keyed bucket that belong to `file`.
///
/// Buckets are kept sorted by [`FileId`] (see [`insert_by_file`]), so the
/// entries for one file are a contiguous run that binary search finds and
/// everything after it is in ascending file order. Both lookup shapes the
/// scope rules need — "the one in this file" and "the one in the
/// lowest-numbered header" — read off that.
fn file_run<T>(entries: &[(FileId, T)], file: FileId) -> (usize, &[(FileId, T)]) {
    let start = entries.partition_point(|(f, _)| *f < file);
    let len = entries[start..].partition_point(|(f, _)| *f == file);
    (start, &entries[start..start + len])
}

/// Insert into a bucket, keeping it sorted by [`FileId`] and keeping equal-file
/// entries in registration order. The single writer of that invariant.
fn insert_by_file<T>(entries: &mut Vec<(FileId, T)>, file: FileId, value: T) {
    let at = entries.partition_point(|(f, _)| *f <= file);
    debug_assert!(at == 0 || entries[at - 1].0 <= file);
    debug_assert!(at == entries.len() || entries[at].0 > file);
    entries.insert(at, (file, value));
}

/// A candidate list that never lists an id twice.
///
/// The ordered vector stays authoritative — it is what callers get back, and
/// its order is the resolution order. A membership set appears only once the
/// list is long enough for the linear scan to cost more than it saves, since
/// nearly every candidate list is a handful of entries.
#[derive(Default)]
struct CandidateSet {
    out: Vec<FnId>,
    seen: Option<FxHashSet<FnId>>,
}

impl CandidateSet {
    /// Index size past which a membership set beats scanning the vector.
    const INDEX_AT: usize = 16;

    fn push(&mut self, id: FnId) {
        if self.seen.is_none() && self.out.len() >= Self::INDEX_AT {
            self.seen = Some(self.out.iter().copied().collect());
        }
        let fresh = match &mut self.seen {
            Some(seen) => seen.insert(id),
            None => !self.out.contains(&id),
        };
        if fresh {
            self.out.push(id);
        }
    }

    fn into_vec(self) -> Vec<FnId> {
        self.out
    }
}

/// Last `::` segment of a function name, for base-name indexing.
fn base_name_of(name: &str) -> String {
    match name.rsplit("::").next() {
        Some(seg) if !seg.is_empty() => seg.to_string(),
        _ => name.to_string(),
    }
}

/// Append the ids not already present, keeping first-seen order.
fn push_unique(out: &mut Vec<FnId>, ids: impl IntoIterator<Item = FnId>) {
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Program, TypeDesc};

    /// `CallFacts` ties the comparison and the fingerprint to one field list,
    /// so they cannot drift. What a derive cannot state is which fields belong
    /// on that list: a record's identity, position and originating unit are
    /// not facts about the call, and letting one in would split records the
    /// merge has to coalesce, leaving a duplicate per contributing unit.
    #[test]
    fn call_facts_ignore_identity_position_and_unit() {
        let base = CallSite {
            id: crate::CallSiteId(7),
            caller: FnId(3),
            callee_name: "handler".into(),
            callee_var: Some(VarId(1)),
            callee_fn_id: Some(FnId(9)),
            var_args: vec![(1, VarId(4))],
            fn_args: vec![(2, FnId(5))],
            addr_of_member_args: vec![1],
            addr_of_args: Vec::new(),
            args_bound_past_this: false,
            span: Span::new(FileId(2), 10, 3),
            expansion_span: None,
            is_direct: true,
            receiver_class: Some("Cls".into()),
            return_dst: Some(VarId(6)),
            tu: Some(FileId(2)),
        };
        let elsewhere = CallSite {
            id: crate::CallSiteId(11),
            callee_name: "other".into(),
            span: Span::new(FileId(5), 99, 1),
            tu: Some(FileId(5)),
            ..base.clone()
        };
        assert!(base.same_facts(&elsewhere));
        assert_eq!(base.fact_fingerprint(), elsewhere.fact_fingerprint());

        // And a fact that does differ still separates them, both ways.
        let rebound = CallSite {
            var_args: vec![(1, VarId(8))],
            ..base.clone()
        };
        assert!(!base.same_facts(&rebound));
        assert_ne!(base.fact_fingerprint(), rebound.fact_fingerprint());
    }

    #[test]
    fn resolves_by_name_classifies_plain_identifiers() {
        let mk = |callee_name: &str, callee_var: Option<u32>, is_direct: bool| CallSite {
            id: crate::CallSiteId(0),
            caller: FnId(0),
            callee_name: callee_name.into(),
            callee_var: callee_var.map(VarId),
            callee_fn_id: None,
            var_args: Vec::new(),
            fn_args: Vec::new(),
            addr_of_member_args: Vec::new(),
            addr_of_args: Vec::new(),
            args_bound_past_this: false,
            span: Span::new(FileId(0), 1, 1),
            expansion_span: None,
            is_direct,
            receiver_class: None,
            return_dst: None,
            tu: None,
        };
        assert!(mk("OsalMemCalloc", None, false).resolves_by_name());
        assert!(mk("f", None, true).resolves_by_name());
        assert!(!mk("ops->Dispatch", None, false).resolves_by_name());
        assert!(!mk("obj.fn", None, false).resolves_by_name());
        assert!(!mk("fp", Some(3), false).resolves_by_name());
    }

    #[test]
    fn scope_lookup_preserves_file_priority_and_equal_file_order() {
        let mut s = SymbolTable::default();
        let mut ids = Vec::new();
        for (file, target) in [(9, 0), (2, 0), (2, 1), (6, 0)] {
            let mut f = fake_function(
                s.alloc_fn_id(),
                "local",
                vec![],
                true,
                false,
                FileId(file),
                1,
            );
            f.linkage = Linkage::Internal;
            f.target = Some(crate::TargetId(target));
            ids.push(s.add_function(f));
        }
        for header in [9, 2, 6] {
            s.register_included_header(FileId(20), FileId(header));
        }
        assert_eq!(
            s.resolve_function_in_scope("local", Some(FileId(20))),
            Some(ids[1])
        );
        assert_eq!(
            s.resolve_function_in_scope("local", Some(FileId(9))),
            Some(ids[0])
        );
        assert_eq!(
            s.resolve_function_candidates("local", Some(FileId(20))),
            vec![ids[1], ids[2], ids[3], ids[0]]
        );
    }

    #[test]
    fn file_static_lookup_preserves_header_priority_and_late_own_file() {
        let mut s = SymbolTable::default();
        let mut ids = Vec::new();
        for file in [9, 2, 6, 4] {
            let id = s.alloc_var_id();
            ids.push(s.add_variable(Variable {
                id,
                name: "local".into(),
                type_id: TypeId(0),
                storage: StorageClass::FileStatic,
                fn_id: None,
                param_index: None,
                span: Span::new(FileId(file), 1, 1),
                is_pointer: false,
                is_defined: true,
                is_weak: false,
                target: None,
                is_namespaced: false,
            }));
        }
        for header in [9, 2, 6] {
            s.register_included_header(FileId(4), FileId(header));
            s.register_included_header(FileId(20), FileId(header));
        }
        assert_eq!(s.file_static_named(FileId(20), "local"), Some(ids[1]));
        assert_eq!(s.file_static_named(FileId(4), "local"), Some(ids[3]));
    }

    #[test]
    fn sole_binding_shortcut_agrees_with_the_candidate_walk() {
        let mut p = Program::new(PathBuf::from("/t"));
        let caller_file = p.symbols.add_file(PathBuf::from("/t/c.c"));
        let callee_file = p.symbols.add_file(PathBuf::from("/t/d.c"));
        let entry = fake_function(
            p.symbols.alloc_fn_id(),
            "entry",
            vec![],
            true,
            false,
            caller_file,
            1,
        );
        let caller = p.symbols.add_function(entry);
        let f = fake_function(
            p.symbols.alloc_fn_id(),
            "f",
            vec![],
            true,
            false,
            callee_file,
            1,
        );
        let callee = p.symbols.add_function(f);
        let cs = CallSite {
            id: CallSiteId(0),
            caller,
            callee_name: "f".into(),
            callee_var: None,
            callee_fn_id: Some(callee),
            var_args: vec![],
            fn_args: vec![],
            addr_of_member_args: vec![],
            addr_of_args: vec![],
            args_bound_past_this: false,
            span: Span::new(caller_file, 1, 1),
            expansion_span: None,
            is_direct: true,
            receiver_class: None,
            return_dst: None,
            tu: Some(caller_file),
        };
        // The shortcut applies, and the walk it replaces yields the same edge.
        assert!(p.symbols.is_sole_binding_of_name(callee, "f"));
        assert_eq!(p.callees_of(&cs), vec![callee]);
        assert_eq!(
            p.symbols
                .resolve_function_candidates("f", Some(caller_file)),
            vec![callee]
        );
    }

    #[test]
    fn an_internal_entry_under_the_name_defeats_the_sole_binding_shortcut() {
        // A file-`static` definition shadows a same-name external inside its
        // own TU, so the shortcut must never skip the candidate walk while
        // any internal-linkage entry is registered under the name.
        let mut p = Program::new(PathBuf::from("/t"));
        let source = p.symbols.add_file(PathBuf::from("/t/d.c"));
        let header = p.symbols.add_file(PathBuf::from("/t/h.h"));
        let ext = fake_function(p.symbols.alloc_fn_id(), "f", vec![], true, false, source, 1);
        let ext = p.symbols.add_function(ext);
        assert!(p.symbols.is_sole_binding_of_name(ext, "f"));
        let mut shadow =
            fake_function(p.symbols.alloc_fn_id(), "f", vec![], true, false, header, 5);
        shadow.linkage = Linkage::Internal;
        p.symbols.add_function(shadow);
        assert!(
            !p.symbols.is_sole_binding_of_name(ext, "f"),
            "an internal-linkage entry under the name must still be walked"
        );
    }

    fn fake_function(
        id: FnId,
        name: &str,
        params: Vec<VarId>,
        is_defined: bool,
        is_cpp: bool,
        file: FileId,
        line: u32,
    ) -> Function {
        Function {
            is_weak: false,
            target: None,
            id,
            name: name.to_string(),
            linkage: Linkage::External,
            return_type: TypeId(0),
            params,
            locals: Vec::new(),
            span: Span::new(file, line, 1),
            end_line: line,
            file,
            is_defined,
            param_type_ids: Vec::new(),
            explicit_arity: None,
            default_args: 0,
            owner_unresolved: false,
            variadic: false,
            defaulted_in_class: false,
            declared_in_class: false,
            is_virtual: false,
            is_final: false,
            is_cpp,
            tu: None,
        }
    }

    /// A shared body reaches the headers its contributors include. The search
    /// walks whichever of the two sides is shorter, so both orders are put to
    /// the same questions here and must answer alike.
    #[test]
    fn shared_body_is_visible_through_a_contributor_s_other_headers() {
        let mut p = Program::new(PathBuf::from("/t"));
        let shared = p.symbols.add_file(PathBuf::from("/t/shared.h"));
        // Narrow: one includer, against a body many units contribute.
        let narrow = p.symbols.add_file(PathBuf::from("/t/narrow.h"));
        // Wide: many includers, only the first of which holds the body.
        let wide = p.symbols.add_file(PathBuf::from("/t/wide.h"));
        let outsider = p.symbols.add_file(PathBuf::from("/t/outsider.h"));

        let units: Vec<FileId> = (0..8)
            .map(|i| p.symbols.add_file(PathBuf::from(format!("/t/u{i}.cpp"))))
            .collect();
        let stranger = p.symbols.add_file(PathBuf::from("/t/stranger.cpp"));
        for &unit in &units {
            p.symbols.register_included_header(unit, shared);
        }
        p.symbols.register_included_header(units[0], narrow);
        p.symbols.register_included_header(units[0], wide);
        for &unit in &units[1..] {
            p.symbols.register_included_header(unit, wide);
        }
        p.symbols.register_included_header(stranger, outsider);
        p.symbols.register_included_header(stranger, wide);

        let mut f = fake_function(
            p.symbols.alloc_fn_id(),
            "helper",
            Vec::new(),
            true,
            true,
            shared,
            1,
        );
        f.linkage = Linkage::Internal;
        let id = p.symbols.add_function(f);
        // Only the first unit contributes, so `wide` has more includers than
        // the body has contributors and the walk goes the other way.
        p.symbols.share_header_function(id, units[0]);
        assert!(p.symbols.function_visible_from(id, narrow));
        assert!(p.symbols.function_visible_from(id, wide));
        assert!(!p.symbols.function_visible_from(id, outsider));

        // Every unit contributes: now the contributor set is the longer side.
        for &unit in &units[1..] {
            p.symbols.share_header_function(id, unit);
        }
        assert!(p.symbols.function_visible_from(id, narrow));
        assert!(p.symbols.function_visible_from(id, wide));
        assert!(
            !p.symbols.function_visible_from(id, outsider),
            "a unit that never included the header must not open the body up"
        );
        assert!(!p.symbols.function_visible_from(id, stranger));
    }

    #[test]
    fn scoped_lookup_prefers_the_file_then_its_headers_in_order() {
        let mut p = Program::new(PathBuf::from("/t"));
        let unit = p.symbols.add_file(PathBuf::from("/t/unit.c"));
        let late = p.symbols.add_file(PathBuf::from("/t/late.h"));
        let early = p.symbols.add_file(PathBuf::from("/t/early.h"));
        let unrelated = p.symbols.add_file(PathBuf::from("/t/other.h"));
        p.symbols.register_included_header(unit, late);
        p.symbols.register_included_header(unit, early);
        fn add(p: &mut Program, file: FileId, line: u32) -> FnId {
            let mut f = fake_function(
                p.symbols.alloc_fn_id(),
                "helper",
                Vec::new(),
                true,
                false,
                file,
                line,
            );
            f.linkage = Linkage::Internal;
            p.symbols.add_function(f)
        }
        let in_unrelated = add(&mut p, unrelated, 1);
        let in_late = add(&mut p, late, 2);
        let in_early = add(&mut p, early, 3);
        // Only headers the unit includes are in scope, lowest file id first.
        assert_eq!(
            p.symbols.resolve_function_in_scope("helper", Some(unit)),
            Some(in_late)
        );
        assert_eq!(
            p.symbols.resolve_function_candidates("helper", Some(unit)),
            vec![in_late, in_early]
        );
        // The unit's own definition shadows every header's.
        let own = add(&mut p, unit, 4);
        assert_eq!(
            p.symbols.resolve_function_in_scope("helper", Some(unit)),
            Some(own)
        );
        assert_eq!(
            p.symbols.resolve_function_candidates("helper", Some(unit)),
            vec![own, in_late, in_early]
        );
        assert_eq!(
            p.symbols
                .resolve_function_in_scope("helper", Some(unrelated)),
            Some(in_unrelated)
        );
    }

    #[test]
    fn cpp_prototype_merges_with_cross_tu_definition() {
        // TU 1 (store.cpp) declares `int DispatchToMessage(int);`. Its param
        // var has been merged into the global table (resolvable type).
        let mut p = Program::new(PathBuf::from("/t"));
        let file = p.symbols.add_file(PathBuf::from("/t/store.cpp"));
        let proto = fake_function(
            p.symbols.alloc_fn_id(),
            "DispatchToMessage",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            file,
            3,
        );
        let param = Variable {
            is_defined: false,
            is_weak: false,
            target: None,
            is_namespaced: false,
            id: proto.params[0],
            name: "$arg0".into(),
            type_id: TypeId(4),
            storage: StorageClass::Param,
            fn_id: Some(proto.id),
            param_index: Some(0),
            span: Span::new(file, 3, 23),
            is_pointer: false,
        };
        p.symbols.add_variable(param);
        let proto_id = p.symbols.add_function(proto);

        // TU 2 (target.cpp) defines the same function. Its param var is still
        // a unit-local id NOT in the global table (None type). The pair must
        // collapse into the prototype's entry.
        let target_file = p.symbols.add_file(PathBuf::from("/t/target.cpp"));
        let def = fake_function(
            p.symbols.alloc_fn_id(),
            "DispatchToMessage",
            vec![VarId(999)],
            true,
            true,
            target_file,
            1,
        );
        let def_id = p.symbols.add_function(def);
        assert_eq!(def_id, proto_id, "proto+def must collapse to one record");
        let merged = p.symbols.function(proto_id);
        assert!(merged.is_defined);
        assert_eq!(merged.file, target_file);
        assert_eq!(p.symbols.fn_by_name.len(), 1);
    }

    #[test]
    fn cpp_prototype_merges_with_a_definition_whose_param_type_interned_twice() {
        // `hdf_remote_adapter_if.h` declares `void HdfRemoteAdapterRecycle(
        // struct HdfRemoteService *)`; `hdf_remote_adapter.cpp` defines it.
        // Both sides are C++ (a `.h` reached from a C++ TU is parsed as C++),
        // so the overload check runs -- and the two units interned
        // `struct HdfRemoteService *` under DIFFERENT ids, because the unit
        // that supplied the prototype had already seen the nested
        // `struct HdfObject` complete and the unit that supplied the
        // definition had not. Comparing the ids splits the prototype from its
        // definition, and every C caller of the interface stays bound to the
        // undefined prototype (#83).
        let mut p = Program::new(PathBuf::from("/t"));
        let svc = |obj_fields: Vec<(String, TypeDesc)>| {
            TypeDesc::Ptr(Box::new(TypeDesc::Struct {
                name: "HdfRemoteService".into(),
                fields: vec![(
                    "object".into(),
                    TypeDesc::Struct {
                        name: "HdfObject".into(),
                        fields: obj_fields,
                    },
                )],
            }))
        };
        let complete = p
            .types
            .intern(svc(vec![("objectId".into(), TypeDesc::Int)]));
        let incomplete = p.types.intern(svc(Vec::new()));
        assert_ne!(
            complete, incomplete,
            "the two units must disagree on the id for the test to mean anything"
        );

        let file = p.symbols.add_file(PathBuf::from("/t/adapter_if.h"));
        let mut proto = fake_function(
            p.symbols.alloc_fn_id(),
            "HdfRemoteAdapterRecycle",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            file,
            28,
        );
        proto.param_type_ids = vec![complete];
        let proto_id = p.symbols.add_function(proto);

        let def_file = p.symbols.add_file(PathBuf::from("/t/adapter.cpp"));
        let def = fake_function(
            p.symbols.alloc_fn_id(),
            "HdfRemoteAdapterRecycle",
            vec![VarId(999)],
            true,
            true,
            def_file,
            253,
        );
        let def_id =
            p.symbols
                .add_function_with_param_types(def, Some(&[incomplete]), Some(&p.types));
        assert_eq!(
            def_id, proto_id,
            "one tag interned twice must not split a prototype from its definition"
        );
        let merged = p.symbols.function(proto_id);
        assert!(merged.is_defined, "the caller must reach the body");
        assert_eq!(merged.file, def_file);
    }

    #[test]
    fn two_definitions_sharing_a_name_and_shape_stay_apart() {
        // Camera has the same class in unrelated fuzzer targets, and a test
        // mock beside the production implementation, so one qualified name
        // carries two real bodies with identical signatures. Shape comparison
        // is for reuniting a declaration with its definition; applied to a
        // pair of definitions it would fold the two bodies together and evict
        // the survivor's facts (#83 review).
        let mut p = Program::new(PathBuf::from("/t"));
        let arg = p.types.intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: "CaptureInfo".into(),
            fields: vec![("id".into(), TypeDesc::Int)],
        })));
        // The same shape interned a second time, as another unit's copy.
        let other_arg = p.types.intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: "CaptureInfo".into(),
            fields: Vec::new(),
        })));

        let mock = p.symbols.add_file(PathBuf::from("/t/mock_fuzzer.h"));
        let mut in_mock = fake_function(
            p.symbols.alloc_fn_id(),
            "Mock::Capture",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            mock,
            94,
        );
        in_mock.param_type_ids = vec![arg];
        let mock_id = p.symbols.add_function(in_mock);

        let real = p.symbols.add_file(PathBuf::from("/t/capture.cpp"));
        let in_real = fake_function(
            p.symbols.alloc_fn_id(),
            "Mock::Capture",
            vec![VarId(999)],
            true,
            true,
            real,
            37,
        );
        let real_id =
            p.symbols
                .add_function_with_param_types(in_real, Some(&[other_arg]), Some(&p.types));
        assert_ne!(
            real_id, mock_id,
            "two definitions of one name are two bodies, not a redeclaration"
        );
        let kept = p.symbols.function(mock_id);
        assert_eq!(kept.file, mock, "the first body keeps its own span");
        assert_eq!(kept.span.line, 94);
    }

    #[test]
    fn cpp_overloads_on_distinct_tags_stay_apart_across_units() {
        // The shape comparison must still separate real overloads: the tag
        // NAME is what distinguishes `send(struct Data *)` from
        // `send(struct Control *)`, and neither may swallow the other.
        // `on_control` is a PROTOTYPE on purpose: a pair of definitions takes
        // the exact-id path, which would never reach `same_param_type` and
        // would pass even if tag names were ignored entirely.
        let mut p = Program::new(PathBuf::from("/t"));
        let ptr_to = |name: &str| {
            TypeDesc::Ptr(Box::new(TypeDesc::Struct {
                name: name.into(),
                fields: vec![("x".into(), TypeDesc::Int)],
            }))
        };
        let data = p.types.intern(ptr_to("Data"));
        let control = p.types.intern(ptr_to("Control"));

        let file = p.symbols.add_file(PathBuf::from("/t/a.cpp"));
        let mut on_data = fake_function(
            p.symbols.alloc_fn_id(),
            "send",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            file,
            1,
        );
        on_data.param_type_ids = vec![data];
        let data_id = p.symbols.add_function(on_data);

        let on_control = fake_function(
            p.symbols.alloc_fn_id(),
            "send",
            vec![VarId(999)],
            false,
            true,
            file,
            9,
        );
        let control_id =
            p.symbols
                .add_function_with_param_types(on_control, Some(&[control]), Some(&p.types));
        assert_ne!(control_id, data_id, "distinct tags are distinct overloads");
        assert_eq!(p.symbols.externals_by_name["send"].len(), 2);
    }

    #[test]
    fn cpp_same_arity_distinct_overloads_stay_apart() {
        let mut p = Program::new(PathBuf::from("/t"));
        let file = p.symbols.add_file(PathBuf::from("/t/store.cpp"));
        let fint = fake_function(
            p.symbols.alloc_fn_id(),
            "f",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            file,
            1,
        );
        let var_int = Variable {
            is_defined: false,
            is_weak: false,
            target: None,
            is_namespaced: false,
            id: fint.params[0],
            name: "a".into(),
            type_id: TypeId(4),
            storage: StorageClass::Param,
            fn_id: Some(fint.id),
            param_index: Some(0),
            span: Span::new(file, 1, 1),
            is_pointer: false,
        };
        p.symbols.add_variable(var_int);
        let fint_id = p.symbols.add_function(fint);

        // `double` overload, same arity, both params resolvable -> separate.
        let fdouble = fake_function(
            p.symbols.alloc_fn_id(),
            "f",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            file,
            2,
        );
        let var_double = Variable {
            is_defined: false,
            is_weak: false,
            target: None,
            is_namespaced: false,
            id: fdouble.params[0],
            name: "b".into(),
            type_id: TypeId(8),
            storage: StorageClass::Param,
            fn_id: Some(fdouble.id),
            param_index: Some(0),
            span: Span::new(file, 2, 1),
            is_pointer: false,
        };
        p.symbols.add_variable(var_double);
        let fdouble_id = p.symbols.add_function(fdouble);
        assert_ne!(
            fdouble_id, fint_id,
            "same-arity distinct overloads keep both"
        );
        assert_eq!(p.symbols.externals_by_name["f"].len(), 2);
    }

    #[test]
    fn functions_in_namespace_finds_global_and_qualified_spellings() {
        let mut p = Program::new(PathBuf::from("/t"));
        let file = p.symbols.add_file(PathBuf::from("/t/a.cpp"));

        let global = fake_function(
            p.symbols.alloc_fn_id(),
            "swap",
            Vec::new(),
            true,
            true,
            file,
            1,
        );
        let global_id = p.symbols.add_function(global);

        // `void ::swap(...)` at global scope registers the leading-`::`
        // spelling; the empty-namespace query must still find it.
        let global_explicit = fake_function(
            p.symbols.alloc_fn_id(),
            "::swap",
            Vec::new(),
            true,
            true,
            file,
            2,
        );
        let explicit_id = p.symbols.add_function(global_explicit);

        let kit = fake_function(
            p.symbols.alloc_fn_id(),
            "kit::swap",
            Vec::new(),
            true,
            true,
            file,
            3,
        );
        let kit_id = p.symbols.add_function(kit);

        // Fully-qualified spelling with leading `::` must match namespace `kit`
        // just like `kit::swap` does (and must NOT match the global namespace
        // lookup for bare `swap`).
        let kit_explicit = fake_function(
            p.symbols.alloc_fn_id(),
            "::kit::swap",
            Vec::new(),
            true,
            true,
            file,
            4,
        );
        let kit_explicit_id = p.symbols.add_function(kit_explicit);

        let ns = p.symbols.functions_in_namespace("", "swap");
        assert_eq!(ns.len(), 2, "global + ::-spelled globals: {ns:?}");
        assert!(ns.contains(&explicit_id));
        assert!(
            !ns.contains(&kit_id) && !ns.contains(&kit_explicit_id),
            "namespaced swaps must not leak into the global-namespace set"
        );
        let kit_ns = p.symbols.functions_in_namespace("kit", "swap");
        assert_eq!(kit_ns.len(), 2, "kit::swap + ::kit::swap: {kit_ns:?}");
        assert!(kit_ns.contains(&kit_id));
        assert!(kit_ns.contains(&kit_explicit_id));
        assert!(
            !kit_ns.contains(&global_id),
            "global swap must not leak into kit"
        );
        assert!(p.symbols.functions_in_namespace("other", "swap").is_empty());
        assert!(p.symbols.functions_in_namespace("", "missing").is_empty());
    }

    #[test]
    fn cpp_overload_prototype_reunited_when_another_overload_precedes() {
        // A header declares two overloads:
        //   void process(int);
        //   void process(double);
        // Then a .cpp file defines void process(int) { ... }.
        // The definition must reunite with the first prototype, even though
        // fn_by_name was overwritten by the second prototype.
        let mut p = Program::new(PathBuf::from("/t"));
        let header = p.symbols.add_file(PathBuf::from("/t/api.h"));
        let int_ty = p.types.intern(TypeDesc::Int);
        let double_ty = p.types.intern(TypeDesc::Double);

        let mut proto1 = fake_function(
            p.symbols.alloc_fn_id(),
            "process",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            10,
        );
        proto1.param_type_ids = vec![int_ty];
        let proto1_id = p.symbols.add_function(proto1);

        let mut proto2 = fake_function(
            p.symbols.alloc_fn_id(),
            "process",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            20,
        );
        proto2.param_type_ids = vec![double_ty];
        // Registered WITH its types: `add_function` resolves the incoming side
        // through `param_types` and then the params' `VarId`s, never through
        // `func.param_type_ids`, so a bare `add_function` here would leave the
        // incoming type unresolved, take the permissive fallback and merge the
        // two prototypes into one entry — making every assertion below compare
        // an id with itself.
        let proto2_id =
            p.symbols
                .add_function_with_param_types(proto2, Some(&[double_ty]), Some(&p.types));

        // Premise: two prototypes, two entries, and `fn_by_name` holds the
        // LATER one, so a definition that consulted only `fn_by_name` would
        // never see proto1.
        assert_ne!(proto1_id, proto2_id, "int and double are two overloads");
        assert_eq!(p.symbols.functions.len(), 2);
        assert_eq!(p.symbols.fn_by_name.get("process"), Some(&proto2_id));

        // Now .cpp defines process(int).
        let cpp_file = p.symbols.add_file(PathBuf::from("/t/api.cpp"));
        let def1 = fake_function(
            p.symbols.alloc_fn_id(),
            "process",
            vec![VarId(999)],
            true,
            true,
            cpp_file,
            100,
        );
        let def1_id =
            p.symbols
                .add_function_with_param_types(def1, Some(&[int_ty]), Some(&p.types));

        assert_eq!(
            def1_id, proto1_id,
            "process(int) definition must reunite with proto1"
        );
        assert_eq!(
            p.symbols.functions.len(),
            2,
            "reuniting must not add a third entry"
        );
        assert!(p.symbols.function(proto1_id).is_defined);
        // And `fn_by_name` must now prefer the defined entry over proto2.
        assert_eq!(p.symbols.fn_by_name.get("process"), Some(&proto1_id));
    }

    #[test]
    fn a_later_declaration_does_not_shadow_a_defined_overload() {
        // `fn_by_name` holds one entry per name and every call site that
        // resolves unqualified goes through it. A declaration of a DIFFERENT
        // overload is a new entry, not a merge, and it must not take the
        // primary slot away from a body that already has it — otherwise the
        // solver resolves callers to an undefined entry and never expands it.
        let mut p = Program::new(PathBuf::from("/t"));
        let int_ty = p.types.intern(TypeDesc::Int);
        let double_ty = p.types.intern(TypeDesc::Double);

        let cpp_file = p.symbols.add_file(PathBuf::from("/t/store.cpp"));
        let mut def = fake_function(
            p.symbols.alloc_fn_id(),
            "store",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            cpp_file,
            10,
        );
        def.param_type_ids = vec![int_ty];
        let def_id = p
            .symbols
            .add_function_with_param_types(def, Some(&[int_ty]), Some(&p.types));
        assert_eq!(p.symbols.fn_by_name.get("store"), Some(&def_id));

        let header = p.symbols.add_file(PathBuf::from("/t/store.h"));
        let mut later_decl = fake_function(
            p.symbols.alloc_fn_id(),
            "store",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            3,
        );
        later_decl.param_type_ids = vec![double_ty];
        let decl_id =
            p.symbols
                .add_function_with_param_types(later_decl, Some(&[double_ty]), Some(&p.types));

        assert_ne!(decl_id, def_id, "store(double) is a separate overload");
        assert_eq!(
            p.symbols.fn_by_name.get("store"),
            Some(&def_id),
            "the defined entry must keep the primary slot"
        );
    }

    #[test]
    fn a_declaration_does_not_move_the_primary_slot_between_two_bodies() {
        // Two defined overloads under one name: the primary slot holds one of
        // them, and which one is already arbitrary. What must not happen is a
        // mere DECLARATION moving it. A declaration asserts no body, so if it
        // could hand the slot to whichever overload it happens to match, every
        // unqualified call site's answer would turn on where declarations sit
        // across the tree and on the order units merge -- and the index has to
        // be bit-reproducible. `register_function` therefore hands
        // `should_take_primary` the INCOMING entry's definedness, not the
        // merged entry's; the merged entry is defined either way here, so
        // passing that instead lets a declaration reroute callers.
        let mut p = Program::new(PathBuf::from("/t"));
        let int_ty = p.types.intern(TypeDesc::Int);
        let double_ty = p.types.intern(TypeDesc::Double);
        let cpp_file = p.symbols.add_file(PathBuf::from("/t/store.cpp"));

        let mut int_def = fake_function(
            p.symbols.alloc_fn_id(),
            "store",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            cpp_file,
            10,
        );
        int_def.param_type_ids = vec![int_ty];
        let int_id =
            p.symbols
                .add_function_with_param_types(int_def, Some(&[int_ty]), Some(&p.types));

        let mut double_def = fake_function(
            p.symbols.alloc_fn_id(),
            "store",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            cpp_file,
            20,
        );
        double_def.param_type_ids = vec![double_ty];
        let double_id =
            p.symbols
                .add_function_with_param_types(double_def, Some(&[double_ty]), Some(&p.types));
        assert_ne!(int_id, double_id, "two distinct bodies");
        let holder = *p.symbols.fn_by_name.get("store").expect("a primary slot");
        assert!(holder == int_id || holder == double_id);

        // A declaration of the `int` overload, arriving from a header.
        let header = p.symbols.add_file(PathBuf::from("/t/store.h"));
        let mut int_decl = fake_function(
            p.symbols.alloc_fn_id(),
            "store",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            3,
        );
        int_decl.param_type_ids = vec![int_ty];
        let merged =
            p.symbols
                .add_function_with_param_types(int_decl, Some(&[int_ty]), Some(&p.types));
        assert_eq!(merged, int_id, "the declaration merges into its own body");
        assert_eq!(
            p.symbols.fn_by_name.get("store"),
            Some(&holder),
            "a declaration must not move the primary slot off the body holding it"
        );
    }

    #[test]
    fn an_unresolved_parameter_type_is_not_a_wildcard_across_overloads() {
        // `same_type_shape` treats `TypeDesc::Unknown` as matching anything.
        // That rule belongs to the variant merge, which takes a candidate only
        // when exactly one matches; this search takes the FIRST compatible
        // candidate, so a wildcard here folds distinct overloads together --
        // and an unresolvable parameter type is ordinary (a name no header in
        // the include path declares). Exact-`TypeId` comparison, which is what
        // this check did before shape comparison arrived, kept them apart.
        let mut p = Program::new(PathBuf::from("/t"));
        let unknown = p.types.unknown();
        let int_ty = p.types.intern(TypeDesc::Int);
        let double_ty = p.types.intern(TypeDesc::Double);
        let header = p.symbols.add_file(PathBuf::from("/t/emit.h"));

        let mut proto_u = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            1,
        );
        proto_u.param_type_ids = vec![unknown];
        let proto_u_id =
            p.symbols
                .add_function_with_param_types(proto_u, Some(&[unknown]), Some(&p.types));

        let mut proto_i = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            2,
        );
        proto_i.param_type_ids = vec![int_ty];
        let proto_i_id =
            p.symbols
                .add_function_with_param_types(proto_i, Some(&[int_ty]), Some(&p.types));
        assert_ne!(
            proto_u_id, proto_i_id,
            "an unresolved parameter type is a type, not every type"
        );

        let mut def_d = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            header,
            9,
        );
        def_d.param_type_ids = vec![double_ty];
        let def_d_id =
            p.symbols
                .add_function_with_param_types(def_d, Some(&[double_ty]), Some(&p.types));
        assert_ne!(
            def_d_id, proto_u_id,
            "the `double` body must not land on the unresolved prototype"
        );
        assert_ne!(def_d_id, proto_i_id, "nor on the `int` one");

        // Two sides that BOTH failed to resolve still describe one function:
        // that is the pair exact-id comparison accepted.
        let mut def_u = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            header,
            12,
        );
        def_u.param_type_ids = vec![unknown];
        let merged =
            p.symbols
                .add_function_with_param_types(def_u, Some(&[unknown]), Some(&p.types));
        assert_eq!(
            merged, proto_u_id,
            "the unresolved prototype gets its own definition"
        );
    }

    #[test]
    fn a_c_definition_picks_the_matching_overload_out_of_a_cpp_bucket() {
        // A pure-C definition may merge into a C++-parsed prototype, and that
        // pair is matched on arity alone -- a `.c` body's parameter types are
        // decayed and would not compare equal to the header's. Since the
        // candidate search scans the whole `externals_by_name` bucket rather
        // than only the primary slot, two same-arity C++ prototypes under one
        // name left arity unable to tell them apart, and the definition merged
        // into whichever was registered first. It now tries the candidates
        // demanding the signature agree too, and only falls back to arity when
        // none does.
        let mut p = Program::new(PathBuf::from("/t"));
        let int_ty = p.types.intern(TypeDesc::Int);
        let double_ty = p.types.intern(TypeDesc::Double);
        let header = p.symbols.add_file(PathBuf::from("/t/emit.h"));

        let mut proto_i = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            1,
        );
        proto_i.param_type_ids = vec![int_ty];
        let proto_i_id =
            p.symbols
                .add_function_with_param_types(proto_i, Some(&[int_ty]), Some(&p.types));

        let mut proto_d = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            2,
        );
        proto_d.param_type_ids = vec![double_ty];
        let proto_d_id =
            p.symbols
                .add_function_with_param_types(proto_d, Some(&[double_ty]), Some(&p.types));
        assert_ne!(proto_i_id, proto_d_id, "two same-arity C++ prototypes");

        // The `.c` definition of the `double` overload, parsed as C.
        let c_file = p.symbols.add_file(PathBuf::from("/t/emit.c"));
        let mut c_def = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            true,
            false,
            c_file,
            9,
        );
        c_def.param_type_ids = vec![double_ty];
        let merged =
            p.symbols
                .add_function_with_param_types(c_def, Some(&[double_ty]), Some(&p.types));
        assert_eq!(
            merged, proto_d_id,
            "the body must land on the prototype it shares a signature with, \
             not on whichever prototype was registered first"
        );
        assert!(
            !p.symbols
                .function_by_id(proto_i_id)
                .expect("emit(int) survives")
                .is_defined,
            "the other overload must stay undefined"
        );
    }

    /// Pins the invariant both promotion sites now share: once a body exists
    /// under a name, `fn_by_name` points at it and no later declaration takes
    /// the slot back. This is a characterization test, not a regression guard
    /// -- restoring the matched branch's old `contains_key` test still passes
    /// it, because a primary can only be undefined while no defined entry
    /// exists at all, so the asymmetry between the two sites was unreachable.
    /// The shared `should_take_primary` is a consistency fix, not a bug fix.
    #[test]
    fn fn_by_name_points_at_a_body_once_one_exists() {
        let mut p = Program::new(PathBuf::from("/t"));
        let int_ty = p.types.intern(TypeDesc::Int);
        let double_ty = p.types.intern(TypeDesc::Double);
        let header = p.symbols.add_file(PathBuf::from("/t/api.h"));

        // An undefined overload takes the primary slot first.
        let mut proto_d = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            1,
        );
        proto_d.param_type_ids = vec![double_ty];
        let proto_d_id =
            p.symbols
                .add_function_with_param_types(proto_d, Some(&[double_ty]), Some(&p.types));

        // A body for the other overload arrives, and claims the slot.
        let cpp = p.symbols.add_file(PathBuf::from("/t/api.cpp"));
        let mut def_i = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            cpp,
            10,
        );
        def_i.param_type_ids = vec![int_ty];
        let def_i_id =
            p.symbols
                .add_function_with_param_types(def_i, Some(&[int_ty]), Some(&p.types));
        assert_ne!(def_i_id, proto_d_id);
        assert_eq!(p.symbols.fn_by_name.get("emit"), Some(&def_i_id));

        // Now a redeclaration of the DOUBLE overload merges into its own
        // undefined entry. That must not hand the slot back to it.
        let mut redecl_d = fake_function(
            p.symbols.alloc_fn_id(),
            "emit",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            1,
        );
        redecl_d.param_type_ids = vec![double_ty];
        let merged =
            p.symbols
                .add_function_with_param_types(redecl_d, Some(&[double_ty]), Some(&p.types));
        assert_eq!(merged, proto_d_id, "the redeclaration merges into proto_d");
        assert_eq!(
            p.symbols.fn_by_name.get("emit"),
            Some(&def_i_id),
            "the defined entry must keep the primary slot"
        );
    }

    /// Review follow-up: `is_cpp` was cleared by any C-parsed redeclaration.
    /// Only a C DEFINITION may do that; a mere declaration would otherwise
    /// strip a C++ body's overload identity and let a later unrelated overload
    /// merge into it.
    #[test]
    fn a_c_declaration_does_not_strip_a_cpp_definitions_overload_identity() {
        let mut p = Program::new(PathBuf::from("/t"));
        let int_ty = p.types.intern(TypeDesc::Int);
        let cpp = p.symbols.add_file(PathBuf::from("/t/impl.cpp"));
        let mut def = fake_function(
            p.symbols.alloc_fn_id(),
            "handle",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            cpp,
            5,
        );
        def.param_type_ids = vec![int_ty];
        let def_id = p
            .symbols
            .add_function_with_param_types(def, Some(&[int_ty]), Some(&p.types));
        assert!(p.symbols.function(def_id).is_cpp);

        let c_header = p.symbols.add_file(PathBuf::from("/t/legacy.h"));
        let mut c_decl = fake_function(
            p.symbols.alloc_fn_id(),
            "handle",
            vec![p.symbols.alloc_var_id()],
            false,
            false,
            c_header,
            2,
        );
        c_decl.param_type_ids = vec![int_ty];
        let merged =
            p.symbols
                .add_function_with_param_types(c_decl, Some(&[int_ty]), Some(&p.types));
        assert_eq!(merged, def_id, "the C declaration merges into the body");
        assert!(
            p.symbols.function(def_id).is_cpp,
            "a declaration must not clear the body's is_cpp"
        );
    }

    #[test]
    fn cpp_type_shape_normalizes_leading_colons() {
        let mut p = Program::new(PathBuf::from("/t"));
        let ptr_with_colons = p.types.intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: "::ns::Device".into(),
            fields: vec![("id".into(), TypeDesc::Int)],
        })));
        let ptr_without_colons = p.types.intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: "ns::Device".into(),
            fields: vec![("id".into(), TypeDesc::Int)],
        })));

        let h_file = p.symbols.add_file(PathBuf::from("/t/dev.h"));
        let mut proto = fake_function(
            p.symbols.alloc_fn_id(),
            "get_device",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            h_file,
            5,
        );
        proto.param_type_ids = vec![ptr_with_colons];
        let proto_id = p.symbols.add_function(proto);

        let cpp_file = p.symbols.add_file(PathBuf::from("/t/dev.cpp"));
        let def = fake_function(
            p.symbols.alloc_fn_id(),
            "get_device",
            vec![VarId(999)],
            true,
            true,
            cpp_file,
            50,
        );
        let def_id = p.symbols.add_function_with_param_types(
            def,
            Some(&[ptr_without_colons]),
            Some(&p.types),
        );

        assert_eq!(
            def_id, proto_id,
            "::ns::Device and ns::Device must match type shape"
        );
        assert!(p.symbols.function(proto_id).is_defined);
    }

    #[test]
    fn a_scoped_lookup_prefers_a_body_over_a_prototype_in_the_same_image() {
        // Under link scoping `fn_by_name` can hold another image's entry, and
        // the lookup then falls through to the per-name bucket. That bucket is
        // in registration order, so a prototype registered ahead of the body
        // answered for every call site in the image and the solver never
        // expanded the body behind it.
        let mut p = Program::new(PathBuf::from("/t"));
        let int_ty = p.types.intern(TypeDesc::Int);
        let double_ty = p.types.intern(TypeDesc::Double);
        let image = crate::TargetId(1);
        let elsewhere = crate::TargetId(2);

        let header = p.symbols.add_file(PathBuf::from("/t/api.h"));
        let mut proto = fake_function(
            p.symbols.alloc_fn_id(),
            "process",
            vec![p.symbols.alloc_var_id()],
            false,
            true,
            header,
            3,
        );
        proto.param_type_ids = vec![double_ty];
        proto.target = Some(image);
        let proto_id =
            p.symbols
                .add_function_with_param_types(proto, Some(&[double_ty]), Some(&p.types));

        let cpp = p.symbols.add_file(PathBuf::from("/t/api.cpp"));
        let mut def = fake_function(
            p.symbols.alloc_fn_id(),
            "process",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            cpp,
            10,
        );
        def.param_type_ids = vec![int_ty];
        def.target = Some(image);
        let def_id = p
            .symbols
            .add_function_with_param_types(def, Some(&[int_ty]), Some(&p.types));
        assert_ne!(proto_id, def_id, "process(double) is a separate overload");

        // A definition in ANOTHER image registers last and takes the primary
        // slot, which is what pushes this image's lookup into the bucket.
        let other = p.symbols.add_file(PathBuf::from("/t/other.cpp"));
        let mut foreign = fake_function(
            p.symbols.alloc_fn_id(),
            "process",
            vec![p.symbols.alloc_var_id()],
            true,
            true,
            other,
            4,
        );
        foreign.param_type_ids = vec![int_ty];
        foreign.target = Some(elsewhere);
        let foreign_id =
            p.symbols
                .add_function_with_param_types(foreign, Some(&[int_ty]), Some(&p.types));
        assert_eq!(p.symbols.fn_by_name.get("process"), Some(&foreign_id));

        assert_eq!(
            p.symbols
                .resolve_function_in_scope_in_target("process", None, Some(image)),
            Some(def_id),
            "the body in this image, not the prototype registered before it"
        );
    }

    #[test]
    fn test_c_and_cpp_definition_merge_order_symmetry() {
        let mut p1 = Program::default();
        let f1 = p1.symbols.add_file(PathBuf::from("/t/a.cpp"));
        let f2 = p1.symbols.add_file(PathBuf::from("/t/b.c"));
        let v1 = p1.symbols.alloc_var_id();
        let v2 = p1.symbols.alloc_var_id();
        let id_cpp1 = p1.symbols.alloc_fn_id();
        let id_c1 = p1.symbols.alloc_fn_id();
        let fn_cpp1 = fake_function(id_cpp1, "collide", vec![v1], true, true, f1, 10);
        let fn_c1 = fake_function(id_c1, "collide", vec![v2], true, false, f2, 20);

        // Order 1: C++ first, C second
        let r_cpp1 = p1.symbols.register_function(fn_cpp1, None, None);
        let r_c1 = p1.symbols.register_function(fn_c1, None, None);

        let mut p2 = Program::default();
        let f1_2 = p2.symbols.add_file(PathBuf::from("/t/a.cpp"));
        let f2_2 = p2.symbols.add_file(PathBuf::from("/t/b.c"));
        let v1_2 = p2.symbols.alloc_var_id();
        let v2_2 = p2.symbols.alloc_var_id();
        let id_c2 = p2.symbols.alloc_fn_id();
        let id_cpp2 = p2.symbols.alloc_fn_id();
        let fn_c2 = fake_function(id_c2, "collide", vec![v2_2], true, false, f2_2, 20);
        let fn_cpp2 = fake_function(id_cpp2, "collide", vec![v1_2], true, true, f1_2, 10);

        // Order 2: C first, C++ second
        let r_c2 = p2.symbols.register_function(fn_c2, None, None);
        let r_cpp2 = p2.symbols.register_function(fn_cpp2, None, None);

        assert_eq!(
            r_cpp1.id == r_c1.id,
            r_c2.id == r_cpp2.id,
            "C and C++ definitions must merge (or stay separate) symmetrically regardless of order"
        );
    }
}
