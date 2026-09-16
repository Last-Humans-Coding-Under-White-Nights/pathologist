use crate::{CallSiteId, FileId, FnId, Span, TypeId, VarId};
use indexmap::IndexMap;
use rustc_hash::FxHashMap;
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
    /// The argument positions count the callee's implicit `this`: explicit
    /// arguments start at 1. Lowering sets it wherever it knows the callee is
    /// a member; after the merge, a site whose callee takes `this` without
    /// it is bound there (a callee whose class its unit never saw).
    pub args_bound_past_this: bool,
    pub span: Span,
    pub is_direct: bool,
    /// Static class of a C++ member-call receiver (`this`, typed pointer).
    /// Post-merge virtual expansion uses this so `final` types are not
    /// re-expanded from the declaring base.
    pub receiver_class: Option<String>,
    /// LHS of `dst = callee(...)` when the call's value is used (`CallReturn`
    /// destination). `dlsym` models write function addresses here.
    pub return_dst: Option<VarId>,
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
}

/// What a surviving entry takes from any redeclaration merged into it,
/// whatever its linkage. The cached signature moves with the parameter list
/// it describes, and a merge that hands over no list touches neither: a
/// shape-compatible prototype can carry different TypeIds, so replacing a
/// definition's cache would let a later, distinct body pass the exact-id
/// check, and `merge_unit` remaps an adopted list, which a stale cache would
/// describe with the old list's ids.
fn absorb_redeclaration(
    existing: &mut Function,
    func: &Function,
    param_types: Option<&[TypeId]>,
    adopted_params: bool,
) {
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
#[derive(Debug, Clone, Copy)]
pub struct FnRegistration {
    /// The surviving entry: either the id the caller allocated, or the
    /// earlier entry this redeclaration merged into.
    pub id: FnId,
    /// The surviving entry replaced its `params` with the caller's list. It
    /// is then the caller's list that the entry holds, so a caller merging a
    /// unit owns remapping those ids out of unit-local space.
    pub adopted_params: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SymbolTable {
    pub files: Vec<FileInfo>,
    pub functions: Vec<Function>,
    pub variables: Vec<Variable>,
    pub call_sites: Vec<CallSite>,
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
    /// File-`static` variables per file, the first registered of a name
    /// winning.
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
    /// Headers whose entities were attributed to this TU during lowering
    /// (`#include`d code). Scope resolution consults them so a `static`
    /// inline defined in a header stays visible to its includers after
    /// cross-TU deduplication collapsed the per-TU copies.
    headers_of: FxHashMap<FileId, std::collections::BTreeSet<FileId>>,
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
        }
    }

    /// Register that multiple `headers` contribute entities lowered while indexing `tu`.
    pub fn register_included_headers(
        &mut self,
        tu: crate::FileId,
        headers: impl IntoIterator<Item = crate::FileId>,
    ) {
        let set = self.headers_of.entry(tu).or_default();
        for header in headers {
            if tu != header {
                set.insert(header);
            }
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
            let primary = self.fn_by_name.get(&func.name).copied();
            let candidates: Vec<FnId> = if func.is_cpp && func.is_defined {
                // A C++ definition tries UNDEFINED prototypes first, to reunite
                // with its declaration, and only then the primary entry (for
                // exact duplicate-definition dedup). It must not search across
                // distinct defined overloads, which are separate bodies.
                let mut list: Vec<FnId> = bucket
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|&id| self.function_by_id(id).is_some_and(|f| !f.is_defined))
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
            // C++ ones.
            let mixed_language =
                |id: FnId| !(func.is_cpp && self.function_by_id(id).is_some_and(|e| e.is_cpp));
            let matched_id = candidates
                .iter()
                .copied()
                .find(|&id| compatible(id, true))
                .or_else(|| {
                    candidates
                        .iter()
                        .copied()
                        .find(|&id| mixed_language(id) && compatible(id, false))
                });
            if let Some(existing_id) = matched_id {
                let mut missing_this = false;
                if let Some(existing) = self.function_mut_by_id(existing_id) {
                    missing_this = joins_member_missing_this(existing, &func);
                    if func.is_defined {
                        existing.is_defined = true;
                        existing.owner_unresolved = func.owner_unresolved;
                        existing.file = func.file;
                        existing.span = func.span;
                        existing.end_line = func.end_line;
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
            let own_file = entries
                .into_iter()
                .flatten()
                .find(|(file, _)| *file == func.file)
                .map(|(_, id)| *id);
            let scoped = own_file.or_else(|| {
                if !func.is_cpp {
                    return None;
                }
                self.first_in_scope(func.file, entries, |id| {
                    self.function_by_id(id).is_some_and(|e| e.is_cpp)
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
                    let entries = self.fn_by_scope.entry(func.name.clone()).or_default();
                    match entries.iter_mut().find(|(file, _)| *file == func.file) {
                        Some(entry) => entry.1 = func.id,
                        None => entries.push((func.file, func.id)),
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
            !self.fn_by_name.contains_key(&func.name),
            "synthetic function must not shadow a registered name"
        );
        self.push_indexed(func)
    }

    fn push_indexed(&mut self, func: Function) -> FnId {
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

    pub fn add_variable(&mut self, var: Variable) -> VarId {
        let id = var.id;
        match var.storage {
            StorageClass::Global => {
                self.global_by_name.insert(var.name.clone(), id);
            }
            StorageClass::FileStatic => {
                let entries = self
                    .file_statics_by_name
                    .entry(var.name.clone())
                    .or_default();
                if !entries.iter().any(|(file, _)| *file == var.span.file) {
                    entries.push((var.span.file, id));
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

    /// Resolve by C scoping rules: an internal-linkage (`static`) definition
    /// in `file` shadows any external definition of the same name for
    /// references inside that file; otherwise fall back to the external name
    /// table. `#include`d headers contributing entities to `file` are part
    /// of its scope (TU-local wins over header-defined on name collision).
    pub fn resolve_function_in_scope(
        &self,
        name: &str,
        file: Option<crate::FileId>,
    ) -> Option<FnId> {
        if let Some(file) = file {
            if let Some(id) = self.lookup_in_scopes(name, file) {
                return Some(id);
            }
        }
        self.fn_by_name.get(name).copied()
    }

    fn lookup_in_scopes(&self, name: &str, file: crate::FileId) -> Option<FnId> {
        let entries = self.fn_by_scope.get(name).map(Vec::as_slice);
        self.first_in_scope(file, entries, |_| true)
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
            .filter(move |&primary| primary != id)
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
    fn first_in_scope<T: Copy>(
        &self,
        file: FileId,
        entries: Option<&[(FileId, T)]>,
        accept: impl Fn(T) -> bool,
    ) -> Option<T> {
        let entries = entries?;
        if let Some(&(_, value)) = entries.iter().find(|(f, v)| *f == file && accept(*v)) {
            return Some(value);
        }
        let headers = self.headers_of.get(&file)?;
        entries
            .iter()
            .filter(|(f, v)| headers.contains(f) && accept(*v))
            .min_by_key(|(f, _)| *f)
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

    /// Whether a reference written in `file` can name `id`: an external
    /// entry is named everywhere, an internal one only in its own file and in
    /// the files that include the header it is in.
    pub fn function_visible_from(&self, id: FnId, file: FileId) -> bool {
        self.function_by_id(id)
            .is_some_and(|f| f.linkage != Linkage::Internal || self.file_sees(file, f.file))
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
        if let Some(fid) = cs.callee_fn_id {
            vec![fid]
        } else if cs.is_direct {
            self.resolve_function_in_scope(&cs.callee_name, Some(cs.span.file))
                .into_iter()
                .collect()
        } else if cs.resolves_by_name() {
            self.resolve_function_candidates(&cs.callee_name, Some(cs.span.file))
        } else {
            Vec::new()
        }
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
        let mut out = Vec::with_capacity(2);
        if let Some(file) = file {
            for (_, id) in self.in_scope(file, self.fn_by_scope.get(name).map(Vec::as_slice)) {
                let overloads = self.internal_overloads_seen_from(id, file);
                for id in std::iter::once(id).chain(overloads) {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
            }
        }
        if let Some(&id) = self.fn_by_name.get(name) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        // C++ overloads: additional entries under the same name that the
        // first-wins `fn_by_name` table hides.
        if let Some(bucket) = self.externals_by_name.get(name) {
            for &id in bucket {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
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

/// Last `::` segment of a function name, for base-name indexing.
fn base_name_of(name: &str) -> String {
    match name.rsplit("::").next() {
        Some(seg) if !seg.is_empty() => seg.to_string(),
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Program, TypeDesc};

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
            args_bound_past_this: false,
            span: Span::new(FileId(0), 1, 1),
            is_direct,
            receiver_class: None,
            return_dst: None,
        };
        assert!(mk("OsalMemCalloc", None, false).resolves_by_name());
        assert!(mk("f", None, true).resolves_by_name());
        assert!(!mk("ops->Dispatch", None, false).resolves_by_name());
        assert!(!mk("obj.fn", None, false).resolves_by_name());
        assert!(!mk("fp", Some(3), false).resolves_by_name());
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
        }
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
}
