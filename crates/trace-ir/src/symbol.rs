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

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub id: FileId,
    pub path: PathBuf,
    /// The file lies under a `--dep` dependency root: it contributes
    /// declarations only, and its entities export with `is_dep = 1` (#60).
    /// Decided once, when the path is interned.
    pub is_dep: bool,
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
    /// Internal-linkage definitions per file: `(file, name) -> FnId`.
    /// In C, a file-`static` definition shadows any external definition of
    /// the same name for references inside that file.
    fn_by_scope: FxHashMap<FileId, FxHashMap<String, FnId>>,
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

    pub fn included_headers(
        &self,
        tu: crate::FileId,
    ) -> Option<&std::collections::BTreeSet<crate::FileId>> {
        self.headers_of.get(&tu)
    }

    pub fn add_function(&mut self, func: Function) -> FnId {
        self.add_function_with_param_types(func, None)
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
    pub fn add_function_with_param_types(
        &mut self,
        func: Function,
        param_types: Option<&[TypeId]>,
    ) -> FnId {
        let mut pending_param_type_ids: Option<Vec<TypeId>> = None;
        if func.linkage == Linkage::External {
            let existing_id = self.fn_by_name.get(&func.name).copied();
            if let Some(existing_id) = existing_id {
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
                let existing_fn = self.functions.iter().find(|f| f.id == existing_id);
                let both_cpp = func.is_cpp && existing_fn.map(|e| e.is_cpp).unwrap_or(false);
                let mergeable = existing_fn
                    .map(|existing| {
                        if !func.is_cpp && !existing.is_cpp {
                            // Pure C: prototype + definition always collapse.
                            return true;
                        }
                        let arity_ok = existing.params.is_empty()
                            || func.params.is_empty()
                            || existing.params.len() == func.params.len();
                        if !both_cpp {
                            // Header parsed as C++ vs `.c` body: merge by
                            // arity and ignore param-type mismatch (typedef
                            // `GpioIrqFunc` vs decayed `Int`).
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
                                        .or_else(|| self.param_type(*func.params.get(i).unwrap()));
                                    match (existing_t(i), incoming_t) {
                                        (Some(ta), Some(tb)) => ta == tb,
                                        _ => true,
                                    }
                                });
                                ok
                            })
                    })
                    .unwrap_or(false);
                if mergeable {
                    if let Some(existing) = self.functions.iter_mut().find(|f| f.id == existing_id)
                    {
                        if func.is_defined {
                            existing.is_defined = true;
                            existing.file = func.file;
                            existing.span = func.span;
                            existing.end_line = func.end_line;
                            if !func.params.is_empty() {
                                existing.params = func.params.clone();
                            }
                        } else if existing.params.is_empty() && !func.params.is_empty() {
                            existing.params = func.params.clone();
                        }
                        // The surviving entry now carries the incoming
                        // signature's types (it may even be a different
                        // overload whose definition overwrote a prototype).
                        if let Some(ts) = param_types {
                            existing.param_type_ids = ts.to_vec();
                        }
                        if func.is_virtual {
                            existing.is_virtual = true;
                        }
                        if func.is_final {
                            existing.is_final = true;
                        }
                        // A C definition merging into a C++-parsed header
                        // prototype must drop `is_cpp`. Otherwise a later
                        // TU merge sees both_cpp and refuses the body
                        // (param TypeIds are still unit-local, so the
                        // overload type-check always fails).
                        existing.is_cpp = existing.is_cpp && func.is_cpp;
                    }
                    let bucket = self.externals_by_name.entry(func.name.clone()).or_default();
                    if !bucket.contains(&existing_id) {
                        bucket.push(existing_id);
                    }
                    return existing_id;
                }
            }
            self.fn_by_name.insert(func.name.clone(), func.id);
            self.externals_by_name
                .entry(func.name.clone())
                .or_default()
                .push(func.id);
            // The entry is not indexed yet (push_indexed runs below), so
            // carry the remapped signature types to the push.
            pending_param_type_ids = param_types.map(|ts| ts.to_vec());
        }
        if func.linkage == Linkage::Internal {
            // Merge forward declarations with definitions for internal
            // (static) functions. Without this, a forward declaration
            // lower bounds before its definition creates a separate entry;
            // call-sites resolved between the two point at the declaration
            // (is_defined=false) and the solver never expands its body.
            if let Some(scope_map) = self.fn_by_scope.get(&func.file) {
                if let Some(&existing_id) = scope_map.get(&func.name) {
                    if let Some(existing) = self.functions.iter_mut().find(|f| f.id == existing_id)
                    {
                        if func.is_defined && !existing.is_defined {
                            existing.is_defined = true;
                            existing.file = func.file;
                            existing.span = func.span;
                            existing.end_line = func.end_line;
                            if !func.params.is_empty() {
                                existing.params = func.params.clone();
                            }
                        } else if !func.is_defined
                            && existing.params.is_empty()
                            && !func.params.is_empty()
                        {
                            existing.params = func.params.clone();
                        }
                        if func.is_virtual {
                            existing.is_virtual = true;
                        }
                        if func.is_final {
                            existing.is_final = true;
                        }
                        return existing_id;
                    }
                }
            }
            // Index every internal-linkage entry, declarations included:
            // lowering resolves identifiers against this table *while the
            // file streams in*, so a designated initializer like
            // `.Read = StaticFn` must bind before the definition is lowered.
            self.fn_by_scope
                .entry(func.file)
                .or_default()
                .insert(func.name.clone(), func.id);
        }
        if let Some(ts) = pending_param_type_ids.take() {
            let mut func = func;
            func.param_type_ids = ts;
            self.push_indexed(func)
        } else {
            self.push_indexed(func)
        }
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

    /// Type of a parameter variable, for overload signature comparison.
    fn param_type(&self, var: VarId) -> Option<TypeId> {
        self.variables
            .iter()
            .find(|v| v.id == var)
            .map(|v| v.type_id)
    }

    pub fn add_variable(&mut self, var: Variable) -> VarId {
        let id = var.id;
        if var.storage == StorageClass::Global {
            self.global_by_name.insert(var.name.clone(), id);
        }
        self.variables.push(var);
        id
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
        if let Some(scope) = self.fn_by_scope.get(&file) {
            if let Some(id) = scope.get(name) {
                return Some(*id);
            }
        }
        if let Some(headers) = self.headers_of.get(&file) {
            for h in headers {
                if let Some(scope) = self.fn_by_scope.get(h) {
                    if let Some(id) = scope.get(name) {
                        return Some(*id);
                    }
                }
            }
        }
        None
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
            if let Some(scope) = self.fn_by_scope.get(&file) {
                if let Some(&id) = scope.get(name) {
                    out.push(id);
                }
            }
            if let Some(headers) = self.headers_of.get(&file) {
                for h in headers {
                    if let Some(scope) = self.fn_by_scope.get(h) {
                        if let Some(&id) = scope.get(name) {
                            if !out.contains(&id) {
                                out.push(id);
                            }
                        }
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
    /// included), in declaration order.
    pub fn functions_named(&self, name: &str) -> Vec<FnId> {
        self.externals_by_name
            .get(name)
            .cloned()
            .unwrap_or_default()
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
    use crate::Program;

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
            is_virtual: false,
            is_final: false,
            is_cpp,
        }
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
}
