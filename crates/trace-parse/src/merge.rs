#[path = "target_merge.rs"]
mod target_merge;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
pub(crate) use target_merge::merge_linked_units;
use trace_ir::{
    same_param_type_or_unresolved, CallSite, CallSiteId, FlowConstraint, FnId, Function, Program,
    ReturnFlow, TemplateBase, TypeDesc, TypeId, VarId, Variable,
};

/// Per-file indexing result merged into a single [`Program`].
#[derive(Debug, Clone, Default)]
pub struct UnitIndex {
    pub path: PathBuf,
    /// Successful command ordinal for this source in the compilation database.
    pub compilation_index: Option<usize>,
    /// Unit-local file table: index == unit-local [`trace_ir::FileId`] value.
    /// Includes the TU itself plus every `#include`d origin that produced
    /// attributed entities.
    pub files: Vec<PathBuf>,
    pub types: trace_ir::TypeTable,
    pub functions: Vec<Function>,
    /// Exact expanded C++ internal definitions, independent of TU-local IDs.
    pub internal_definitions: FxHashMap<FnId, Arc<str>>,
    pub variables: Vec<Variable>,
    pub call_sites: Vec<CallSite>,
    pub flow: Vec<FlowConstraint>,
    pub function_flow_ranges: FxHashMap<FnId, Vec<std::ops::Range<usize>>>,
    /// Initializer constraints, including temporaries and deferred references.
    pub global_initializer_ranges: FxHashMap<VarId, Vec<std::ops::Range<usize>>>,
    pub fn_returns: FxHashMap<FnId, Vec<ReturnFlow>>,
    pub diagnostics: Vec<trace_ir::Diagnostic>,
    pub anon_type_counter: u32,
    /// Per-unit `(derived, base)` class edges (C++).
    pub inheritance: Vec<(String, String)>,
    /// Per-unit templated base facts (C++).
    pub template_bases: Vec<TemplateBase>,
    /// Per-unit declared `operator->` returns (C++), merged in every mode.
    pub arrow_returns: Vec<trace_ir::ArrowReturn>,
    pub template_returns: BTreeMap<String, BTreeMap<String, Vec<trace_ir::TemplateReturn>>>,
    /// Classes declared `final` in this unit.
    pub final_classes: Vec<String>,
    /// Classes this unit defines in an anonymous namespace, by unit-local file.
    pub anonymous_classes: BTreeMap<String, BTreeSet<trace_ir::FileId>>,
    /// Those of them declared `final`, likewise.
    pub anonymous_final_classes: BTreeMap<String, BTreeSet<trace_ir::FileId>>,
    /// Their direct bases, likewise.
    pub anonymous_bases: BTreeMap<String, BTreeSet<(trace_ir::FileId, String)>>,
    /// Namespaces this unit opens, merged in every mode.
    pub namespaces: BTreeSet<String>,
    /// Per type of `types`, the descriptor `merge_types` interns into the
    /// receiving table on the ordinary path: the type's own for a type whose
    /// layout spells it, otherwise the aggregate rebuilt from its layout.
    /// Computed once per unit rather than once per consumer, and shared, so
    /// a consumer that already holds it answers by address. Empty when the
    /// unit was built without it; the merge then rebuilds as it goes.
    pub merge_descs: Vec<Arc<TypeDesc>>,
}

/// See [`UnitIndex::merge_descs`].
pub(crate) fn merge_descs_of(types: &trace_ir::TypeTable) -> Vec<Arc<TypeDesc>> {
    types
        .all()
        .iter()
        .map(|info| match info.desc.as_ref() {
            TypeDesc::Struct { name, fields } | TypeDesc::Union { name, fields }
                if !fields.is_empty() && !layout_spells_desc(types, info) =>
            {
                let fields = fields_from_layout(types, info);
                let rebuilt = match info.desc.as_ref() {
                    TypeDesc::Struct { .. } => TypeDesc::Struct {
                        name: name.clone(),
                        fields,
                    },
                    _ => TypeDesc::Union {
                        name: name.clone(),
                        fields,
                    },
                };
                types.share_desc(&rebuilt)
            }
            _ => Arc::clone(&info.desc),
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MergeMode {
    Full,
    TypesOnly,
    SymbolsOnly,
    HeaderPreamble,
    Variant,
}

type SiteKey = trace_ir::CallSourceKey;
type LocalKey = (FnId, trace_ir::FileId, u32, u32, String);
type TempKey = (FnId, trace_ir::FileId, u32, u32, &'static str);
type FileVarKey = (u32, trace_ir::FileId, u32, u32, String);

/// The kind of a synthesized temporary, or `None` for a declared variable.
///
/// Lowering names these after the unit-local [`VarId`] it just allocated
/// (`_gep41`, `_load42`, `_ret43`, `_recv44`), so the name identifies an
/// allocation order rather than the expression, and two configurations of the
/// same code never agree on it. Each kind keeps its own ordinal space: a temp
/// one configuration emits and another does not must not shift the pairing of
/// an unrelated kind at the same position.
fn temp_prefix(name: &str) -> Option<&'static str> {
    ["_gep", "_load", "_ret", "_recv"]
        .into_iter()
        .find(|prefix| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        })
}

#[derive(Default)]
struct VariantDedup {
    flow: FxHashSet<FlowConstraint>,
    file_vars: FxHashMap<FileVarKey, VarId>,
    /// One scope per originating translation unit. Variables dedup within a
    /// source's own configurations, never between two sources that happen to
    /// include the same header.
    source_scopes: FxHashMap<PathBuf, u32>,
    /// Definitions the base configuration merged for this unit, as
    /// `file → name → candidates`, so alternative arms can match an existing
    /// overload by signature even when their source lines differ.
    base_defs: FxHashMap<trace_ir::FileId, FxHashMap<String, Vec<FnId>>>,
}

impl VariantDedup {
    fn source_scope(&mut self, path: &Path) -> u32 {
        if let Some(&id) = self.source_scopes.get(path) {
            return id;
        }
        let id = self.source_scopes.len() as u32;
        self.source_scopes.insert(path.to_path_buf(), id);
        id
    }
}

/// Position of a call site in the merged table.
///
/// Merged programs allocate call ids in append order, so the id is the index.
/// The scan is a fallback for a caller that built a sparse call table.
#[inline]
pub fn merge_unit_index(program: &mut Program, unit: &UnitIndex) {
    merge_unit(program, unit, MergeMode::Full, None);
}

pub fn merge_unit_header_preamble(program: &mut Program, unit: &UnitIndex) {
    merge_unit(program, unit, MergeMode::HeaderPreamble, None);
}

/// Merge a translation unit together with its configuration variants (#59).
///
/// Variants re-lower the whole unit, so each one repeats every flow constraint
/// of the code the configurations share. Those duplicates are dropped against
/// a set scoped to *this* unit's contribution: deduplicating against the whole
/// program would rebuild a set of every constraint merged so far, once per
/// variant.
pub fn merge_unit_variants(program: &mut Program, base: &UnitIndex, variants: &[UnitIndex]) {
    let mut merge = VariantMerge::start(program, base, !variants.is_empty());
    for unit in variants {
        merge.push(program, unit);
    }
}

/// [`merge_unit_variants`] with the family supplied one unit at a time.
///
/// Units are consumed strictly in order, so a caller that has to *build* each
/// one — target scoping clones and rewrites every unit it merges — can hold a
/// single unit at a time instead of the whole family.
pub(crate) struct VariantMerge {
    /// `None` once the base is merged and no variant follows, which is the
    /// common case and skips building the dedup index entirely.
    seen: Option<VariantDedup>,
}

impl VariantMerge {
    pub(crate) fn start(program: &mut Program, base: &UnitIndex, has_variants: bool) -> Self {
        let flow_start = program.flow.len();
        let var_start = program.symbols.variables.len();
        merge_unit(program, base, MergeMode::Full, None);
        if !has_variants {
            return Self { seen: None };
        }
        // Every unit below merges with `union_aggregates`, whatever the
        // per-source variant tally ends up saying.
        program.layouts_unioned = true;
        let mut seen = VariantDedup {
            flow: program.flow[flow_start..].iter().cloned().collect(),
            file_vars: FxHashMap::default(),
            source_scopes: FxHashMap::default(),
            base_defs: base_definitions(program, base),
        };
        let base_scope = seen.source_scope(&base.path);
        for v in &program.symbols.variables[var_start..] {
            if v.fn_id.is_none() {
                seen.file_vars.insert(
                    (
                        base_scope,
                        v.span.file,
                        v.span.line,
                        v.span.col,
                        v.name.clone(),
                    ),
                    v.id,
                );
            }
        }
        Self { seen: Some(seen) }
    }

    pub(crate) fn push(&mut self, program: &mut Program, unit: &UnitIndex) {
        let Some(seen) = self.seen.as_mut() else {
            debug_assert!(false, "variant pushed after start declared none");
            return;
        };
        merge_unit(program, unit, MergeMode::Variant, Some(seen));
        // A later configuration can introduce a definition absent from the
        // first one. Subsequent alternatives must extend that definition too.
        for (file, definitions) in base_definitions(program, unit) {
            let known = seen.base_defs.entry(file).or_default();
            for (name, ids) in definitions {
                let candidates = known.entry(name).or_default();
                for id in ids {
                    if !candidates.contains(&id) {
                        candidates.push(id);
                    }
                }
            }
        }
        program.variants_merged += 1;
    }
}

/// Index the definitions the base configuration just merged for this unit, by
/// file and name.
///
/// A variant that spells the same function on a different line — the
/// `#ifdef X / #else` alternative-implementation shape this feature exists to
/// recover — must extend that entry rather than register a second definition:
/// `add_function_with_param_types` treats a second definition as a
/// redeclaration and overwrites the survivor's span and parameters with the
/// variant's, which evicts facts the baseline had.
///
/// Keep every overload as a candidate; the caller compares signatures.
fn base_definitions(
    program: &mut Program,
    base: &UnitIndex,
) -> FxHashMap<trace_ir::FileId, FxHashMap<String, Vec<FnId>>> {
    let file_map: Vec<trace_ir::FileId> = base
        .files
        .iter()
        .map(|path| program.symbols.add_file_interned(path))
        .collect();
    let primary_file_id = program.symbols.add_file_interned(&base.path);

    let mut by_file: FxHashMap<trace_ir::FileId, FxHashMap<String, Vec<FnId>>> =
        FxHashMap::default();
    for func in base.functions.iter().filter(|f| f.is_defined) {
        let span_file = file_map
            .get(func.span.file.0 as usize)
            .copied()
            .unwrap_or(primary_file_id);
        let Some(id) = program
            .dedup
            .existing_fn(span_file, &func.name, func.span.line)
        else {
            continue;
        };
        // The unit's own entry can have merged into a declaration that another
        // unit contributed; only a surviving *definition* is a base body.
        if !program
            .symbols
            .function_by_id(id)
            .is_some_and(|f| f.is_defined)
        {
            continue;
        }
        let candidates = by_file
            .entry(span_file)
            .or_default()
            .entry(func.name.clone())
            .or_default();
        if !candidates.contains(&id) {
            candidates.push(id);
        }
    }
    by_file
}

/// Nested PCH: types, typedefs, and inheritance only.
pub fn merge_unit_types(program: &mut Program, unit: &UnitIndex) {
    merge_unit(program, unit, MergeMode::TypesOnly, None);
}

/// TU preamble: types and symbols, with internal C++ body facts replayed for
/// the receiving unit. Header sharing is defined in docs/ANALYSIS.md,
/// "Shared header functions"; the full merge coalesces those contributions.
///
/// Also the mode for a dependency-root header's own unit: its bodies are not
/// the target's code, so they contribute prototypes and stop there (#60).
pub fn merge_unit_symbols(program: &mut Program, unit: &UnitIndex) {
    merge_unit(program, unit, MergeMode::SymbolsOnly, None);
}

fn merge_unit(
    program: &mut Program,
    unit: &UnitIndex,
    mode: MergeMode,
    mut variant_dedup: Option<&mut VariantDedup>,
) {
    let source_scope = variant_dedup
        .as_deref_mut()
        .map(|seen| seen.source_scope(&unit.path))
        .unwrap_or(0);
    program.anon_type_counter = program.anon_type_counter.max(unit.anon_type_counter);
    for (derived, base) in &unit.inheritance {
        program.add_inheritance(derived, base);
    }
    for fact in &unit.template_bases {
        program.add_template_base_fact(fact);
    }
    for fact in &unit.arrow_returns {
        if !program.arrow_returns.contains(fact) {
            program.arrow_returns.push(fact.clone());
        }
    }
    for (class, methods) in &unit.template_returns {
        for (name, facts) in methods {
            for fact in facts {
                program.add_template_return(class, name, fact);
            }
        }
    }
    for cls in &unit.final_classes {
        program.mark_class_final(cls);
    }
    program.namespaces.extend(unit.namespaces.iter().cloned());

    let type_map = merge_types(
        &mut program.types,
        &unit.types,
        matches!(mode, MergeMode::Variant),
        Some(unit.merge_descs.as_slice()).filter(|d| d.len() == unit.types.all().len()),
    );

    let mut file_map: Vec<trace_ir::FileId> = Vec::with_capacity(unit.files.len());
    for path in &unit.files {
        file_map.push(program.symbols.add_file_interned(path));
    }
    let primary_file_id = program.symbols.add_file_interned(&unit.path);
    for &mapped in &file_map {
        if mapped != primary_file_id {
            program
                .symbols
                .register_included_header(primary_file_id, mapped);
        }
    }
    let map_file = |id: trace_ir::FileId| -> trace_ir::FileId {
        file_map
            .get(id.0 as usize)
            .copied()
            .unwrap_or(primary_file_id)
    };
    for (cls, files) in &unit.anonymous_classes {
        for &file in files {
            program.mark_class_anonymous(cls, map_file(file));
        }
    }
    for (cls, files) in &unit.anonymous_final_classes {
        for &file in files {
            program.mark_anonymous_class_final(cls, map_file(file));
        }
    }
    for (cls, bases) in &unit.anonymous_bases {
        for (file, base) in bases {
            program.add_anonymous_base(cls, map_file(*file), base);
        }
    }

    if matches!(
        mode,
        MergeMode::Full | MergeMode::HeaderPreamble | MergeMode::Variant
    ) {
        for diagnostic in &unit.diagnostics {
            let diagnostic = trace_ir::Diagnostic {
                file: diagnostic.file.map(map_file),
                ..diagnostic.clone()
            };
            // A variant re-lowers the whole unit, so every diagnostic the base
            // configuration already reported comes back once per variant. Route
            // every stage through the dedup set for those, not just
            // `preprocess`: a diagnostic that only the variant's own code
            // produces is still new, and is still kept.
            let dedup_every_stage = matches!(mode, MergeMode::Variant);
            // Register unconditionally, and decide whether to keep it after.
            // Short-circuiting past this left a base-configuration `parse`
            // report unregistered, so every variant that re-lowered the same
            // code reported it again as if it were new (#59 cloud review).
            let first_of_its_kind = program.dedup.insert_diagnostic(
                diagnostic.file,
                diagnostic.line,
                &diagnostic.message,
                &diagnostic.stage,
            );
            if first_of_its_kind || (!dedup_every_stage && diagnostic.stage != "preprocess") {
                program.diagnostics.push(diagnostic);
            }
        }
    }

    if matches!(mode, MergeMode::TypesOnly) {
        return;
    }

    let mut fn_map: FxHashMap<FnId, FnId> = FxHashMap::default();
    let mut dropped_fns: FxHashSet<FnId> = FxHashSet::default();
    let mut remap_params: FxHashSet<FnId> = FxHashSet::default();
    let mut variant_param_map: FxHashMap<VarId, VarId> = FxHashMap::default();
    // Functions this unit merged into an entry the program already held: a
    // variant's re-lowered bodies, or shared header bodies (#116).
    let mut remerged_fns: FxHashSet<FnId> = FxHashSet::default();
    // Whether any function of this unit is a shared header body, so the flow
    // pass can skip the per-flow owner lookup for the common case.
    let mut unit_shares_headers = false;
    // Every parameter's name and type (remapped into the program's id space),
    // indexed by its unit-local `VarId`. Both places below used to scan
    // `unit.variables` for each parameter of each function, so one unit cost
    // O(functions x params x variables) -- and a lowered TU carries tens of
    // thousands of variables (#83). Only parameters are indexed, which is all
    // either lookup asks for: locals dominate that list, and `lower_parameter`
    // is the only thing that builds a variable a function's `params` can name.
    let mut unit_var_names: FxHashMap<VarId, &str> = FxHashMap::default();
    let mut unit_param_types: FxHashMap<VarId, TypeId> = FxHashMap::default();
    for v in &unit.variables {
        if v.storage == trace_ir::StorageClass::Param {
            unit_var_names.insert(v.id, v.name.as_str());
            unit_param_types.insert(v.id, remap_type(v.type_id, &type_map));
        }
    }

    let respelled = respelled_declarations(unit, &unit_param_types, &program.types);
    for func in &unit.functions {
        let old_id = func.id;
        if respelled.contains_key(&old_id) {
            dropped_fns.insert(old_id);
            continue;
        }
        if matches!(mode, MergeMode::HeaderPreamble)
            && func.linkage == trace_ir::Linkage::Internal
            && func.is_defined
            && func.is_cpp
        {
            dropped_fns.insert(old_id);
            continue;
        }
        let span_file = map_file(func.span.file);
        let internal_cpp = func.linkage == trace_ir::Linkage::Internal && func.is_cpp;
        // Only a body that originates in an included header can be shared.
        let header_definition = unit
            .internal_definitions
            .get(&old_id)
            .filter(|_| span_file != primary_file_id);
        let origin = trace_ir::Span::new(span_file, func.span.line, func.span.col);
        let shared = header_definition
            .and_then(|text| program.dedup.existing_header_fn(origin, &func.name, text));
        let mut canonical = shared;
        if canonical.is_none() && !internal_cpp {
            canonical = program
                .dedup
                .existing_fn(span_file, &func.name, func.span.line);
        }
        if canonical.is_none() && matches!(mode, MergeMode::Variant) && func.is_defined {
            // The two arms of an `#ifdef X / #else` pair put one function's
            // implementations on different lines, so the line-keyed dedup misses
            // and the variant would otherwise register a second definition —
            // which overwrites the base definition's span and parameters and
            // drops the call sites bound to them (#59 review).
            let candidate = variant_dedup
                .as_deref()
                .and_then(|seen| seen.base_defs.get(&span_file))
                .and_then(|by_name| by_name.get(&func.name))
                .into_iter()
                .flatten()
                .copied();
            // Only an alternative *implementation* extends the base definition.
            // In C++ a configuration that adds an overload — `pick(int)` always,
            // plus `pick(double)` under a define — also lands on a line the base
            // never had, and merging it would fold two functions into one and
            // conflate their parameters and call targets. The signature
            // separates the two: alternative arms of one function agree on it.
            // The variant that *widens* a signature in place (`log(msg)`
            // gaining a `file, line` pair inside the parameter list) keeps the
            // function's own line, so it matches above and never reaches this
            // fallback.
            //
            // Types are compared the way `add_function_with_param_types` does
            // on the ordinary path: through this unit's `type_map`, and a pair
            // either side cannot resolve counts as matching, so an unknown type
            // never splits a function that a conditional typedef merely respells.
            // Both sides share the same comparison; the rule AROUND it is
            // intentionally not shared, because this path demands exact arity
            // equality while the symbol table treats an empty parameter list as
            // a wildcard so a params-less prototype still merges. This path
            // also asks for the `_or_unresolved` spelling, which the symbol
            // table must not: it is safe here only because an ambiguous answer
            // falls through to the ordinary merge below rather than picking a
            // candidate. Keep the two in view of each other -- see
            // `SymbolTable::register_function`.
            let mut matching = candidate.filter(|&id| {
                let Some(base) = program.symbols.function_by_id(id) else {
                    return false;
                };
                // C has no overloading, so a same-name definition in the same
                // file is the other arm of one function whatever its signature:
                // `#ifdef DEBUG` arms routinely differ in arity because the
                // directive wraps the whole declaration. `symbol.rs` draws this
                // same line before its own overload check, and merging is what
                // stops the arm from overwriting the base's span and parameters.
                if !(func.is_cpp && base.is_cpp) {
                    return true;
                }
                base.params.len() == func.params.len()
                    && func.params.iter().enumerate().all(|(i, old)| {
                        let incoming = unit_param_types.get(old).copied();
                        let existing = base.param_type_ids.get(i).copied().or_else(|| {
                            program
                                .symbols
                                .variable_by_id(base.params[i])
                                .map(|v| v.type_id)
                        });
                        match (existing, incoming) {
                            (Some(a), Some(b)) => {
                                same_param_type_or_unresolved(&program.types, a, b)
                            }
                            _ => true,
                        }
                    })
            });
            // Unknown types can match several overloads. Keep the ordinary
            // merge path in that case instead of choosing by insertion order.
            canonical = matching.next().filter(|_| matching.next().is_none());
        }
        if let Some(canonical) = canonical {
            fn_map.insert(old_id, canonical);
            if shared.is_some() {
                program
                    .symbols
                    .share_header_function(canonical, primary_file_id);
                unit_shares_headers = true;
            }
            if matches!(mode, MergeMode::Variant) || shared.is_some() {
                remerged_fns.insert(canonical);
                if let Some(idx) = program.symbols.function_index(canonical) {
                    // Pair parameters by NAME, never by position. A variant
                    // routinely inserts a parameter *ahead* of the ones the
                    // base configuration has:
                    //
                    //     void log(
                    //     #ifdef DEBUG_LOG
                    //         const char *file, int line,
                    //     #endif
                    //         const char *msg);
                    //
                    // Positional pairing would map the variant's `file` onto
                    // the base's `msg` and hand `msg` every value `file` holds.
                    let canon_by_name: FxHashMap<&str, VarId> = program.symbols.functions[idx]
                        .params
                        .iter()
                        .filter_map(|&pid| {
                            program
                                .symbols
                                .variable_by_id(pid)
                                .map(|v| (v.name.as_str(), pid))
                        })
                        .collect();
                    for &old_param in &func.params {
                        let Some(name) = unit_var_names.get(&old_param) else {
                            continue;
                        };
                        if let Some(&canon_param) = canon_by_name.get(name) {
                            variant_param_map.insert(old_param, canon_param);
                        }
                    }
                }
            } else {
                dropped_fns.insert(old_id);
            }
            continue;
        }
        let new_id = program.symbols.alloc_fn_id();
        let mut f = func.clone();
        f.id = new_id;
        f.span.file = span_file;
        f.file = span_file;
        f.tu = Some(primary_file_id);
        f.return_type = remap_type(f.return_type, &type_map);
        // A body written in a dependency header is not the target's code:
        // keep the signature, drop everything the body would contribute (#60).
        if program.is_dep_file(span_file) {
            f.is_defined = false;
            f.end_line = f.span.line;
            f.locals.clear();
        }
        // The incoming params are unit-local VarIds: resolving their types
        // against the global table in `add_function` hits unrelated globals
        // whose ids collide, breaking C++ prototype + definition merges. Map
        // them through the unit's own variables + this unit's type_map so the
        // overload signature check sees real, remapped types.
        let incoming_param_types: Vec<trace_ir::TypeId> = f
            .params
            .iter()
            .map(|old| {
                // `Unknown`, not `TypeId(0)`. Zero is the first descriptor the
                // prelude interns, `Void`, and no parameter has that type, so
                // a parameter whose variable did not lower used to guarantee a
                // signature mismatch and block a merge the rest of the
                // signature agreed on. `Unknown` is what the comparison
                // already documents for a type neither side can resolve: it
                // matches anything, leaving arity to decide.
                unit_param_types
                    .get(old)
                    .copied()
                    .unwrap_or_else(|| program.types.unknown())
            })
            .collect();
        let registered =
            program
                .symbols
                .register_function(f, Some(&incoming_param_types), Some(&program.types));
        let merged = registered.id;
        // A fresh entry owns its parameter list; an entry this unit merged into
        // has just adopted one. Either way the ids are still unit-local and the
        // variable pass has to remap them. Only the symbol table can say which
        // happened: comparing the survivor's params to the list handed in is
        // not proof, because unit-local ids can equal an unrelated list of
        // already-global ids and a later prototype then disconnects a
        // definition's body from its own parameters (#83).
        if merged == new_id || registered.adopted_params {
            remap_params.insert(merged);
        }
        fn_map.insert(old_id, merged);
        // Cached header bodies enter a temporary TU Program through this
        // mode. program_into_unit transfers their text into UnitIndex for
        // the later full merge; final programs need no second text index.
        if matches!(mode, MergeMode::SymbolsOnly) {
            if let Some(text) = unit.internal_definitions.get(&old_id) {
                program
                    .dedup
                    .internal_definitions
                    .insert(merged, Arc::clone(text));
            }
        }
        if let Some(text) = header_definition {
            program
                .dedup
                .insert_header_fn(origin, func.name.clone(), Arc::clone(text), merged);
            program
                .symbols
                .share_header_function(merged, primary_file_id);
            unit_shares_headers = true;
        }
        if !internal_cpp {
            program
                .dedup
                .insert_fn(span_file, func.name.clone(), func.span.line, merged);
        }
    }

    for (&declaration, &definition) in &respelled {
        let Some(&merged) = fn_map.get(&definition) else {
            continue;
        };
        fn_map.insert(declaration, merged);
        // A declaration shared with the units including its header joins a
        // definition of that header only; one in this unit's own file is this
        // unit's, and another unit decides for its own copy.
        let file_of = |id: FnId| {
            unit.functions
                .iter()
                .find(|f| f.id == id)
                .map(|f| f.span.file)
        };
        if let (Some(func), true) = (
            unit.functions.iter().find(|f| f.id == declaration),
            file_of(declaration) == file_of(definition),
        ) {
            let span_file = map_file(func.span.file);
            program
                .dedup
                .insert_fn(span_file, func.name.clone(), func.span.line, merged);
        }
    }

    let mut var_map: FxHashMap<VarId, VarId> = FxHashMap::default();
    for (&old_param, &canon_param) in &variant_param_map {
        var_map.insert(old_param, canon_param);
    }

    // Index only the functions extended by this variant. Scanning a function's
    // entire local list for every variable makes large bodies quadratic.
    //
    // Declared locals are matched by name at their position. Synthesized
    // temporaries cannot be: lowering names them after the unit-local `VarId`
    // it just allocated, so the same expression is `_gep6` in the base and
    // `_gep772` in a variant that lowered a different amount of code before
    // reaching it. They are matched positionally instead — the k-th temporary
    // of a given kind at a source position — which is the order lowering emits
    // them in on both sides.
    let mut local_by_site: FxHashMap<LocalKey, VarId> = FxHashMap::default();
    let mut temps_by_site: FxHashMap<TempKey, Vec<VarId>> = FxHashMap::default();
    for &fn_id in &remerged_fns {
        for &var_id in &program.symbols.function(fn_id).locals {
            if let Some(var) = program.symbols.variable_by_id(var_id) {
                let coords = (fn_id, var.span.file, var.span.line, var.span.col);
                if let Some(prefix) = temp_prefix(&var.name) {
                    temps_by_site
                        .entry((coords.0, coords.1, coords.2, coords.3, prefix))
                        .or_default()
                        .push(var_id);
                } else {
                    local_by_site
                        .entry((coords.0, coords.1, coords.2, coords.3, var.name.clone()))
                        .or_insert(var_id);
                }
            }
        }
    }
    // How many temporaries at each position each incoming body has consumed.
    // Keyed by the unit-local function as well: a unit can carry the same
    // header body twice (a cached header expansion and its own copy), and the
    // second copy's k-th temporary is the first copy's k-th, not the one
    // after it.
    let mut temp_cursor: FxHashMap<(Option<FnId>, TempKey), usize> = FxHashMap::default();

    for var in &unit.variables {
        if var
            .fn_id
            .map(|id| dropped_fns.contains(&id))
            .unwrap_or(false)
        {
            continue;
        }
        if var_map.contains_key(&var.id) {
            continue;
        }
        let span_file = map_file(var.span.file);
        if var.storage == trace_ir::StorageClass::Local && program.is_dep_file(span_file) {
            continue;
        }

        let mapped_fn_id = var.fn_id.and_then(|id| fn_map.get(&id).copied());

        if var.fn_id.is_none() {
            if let Some(seen) = variant_dedup.as_deref_mut() {
                let file_key: FileVarKey = (
                    source_scope,
                    span_file,
                    var.span.line,
                    var.span.col,
                    var.name.clone(),
                );
                if let Some(&existing) = seen.file_vars.get(&file_key) {
                    var_map.insert(var.id, existing);
                    continue;
                }
            }
        }

        let extended_fn = mapped_fn_id.filter(|id| remerged_fns.contains(id));
        let temp_key = extended_fn
            .zip(temp_prefix(&var.name))
            .map(|(id, prefix)| (id, span_file, var.span.line, var.span.col, prefix) as TempKey);
        let local_key = extended_fn
            .filter(|_| temp_key.is_none())
            .map(|id| (id, span_file, var.span.line, var.span.col, var.name.clone()));
        if let Some(existing) = local_key.as_ref().and_then(|key| local_by_site.get(key)) {
            var_map.insert(var.id, *existing);
            continue;
        }
        if let Some(key) = &temp_key {
            let cursor = temp_cursor.entry((var.fn_id, *key)).or_insert(0);
            if let Some(&existing) = temps_by_site.get(key).and_then(|ids| ids.get(*cursor)) {
                *cursor += 1;
                var_map.insert(var.id, existing);
                continue;
            }
        }

        // One external symbol is one variable per image, keyed by its
        // canonical name: `a::counter` and `b::counter` stay two, and an
        // internal-linkage variable has no symbol to unify under.
        if let (Some(target), Some(symbol)) = (var.target, var.external_symbol_name()) {
            if let Some(existing) = program.symbols.target_global(target, symbol) {
                let current = program.symbols.variable(existing);
                if var.is_defined
                    && trace_ir::definition_supersedes(
                        current.is_defined,
                        current.is_weak,
                        var.is_weak,
                    )
                {
                    let mut replacement = var.clone();
                    replacement.id = existing;
                    replacement.type_id = remap_type(var.type_id, &type_map);
                    replacement.span.file = span_file;
                    *program.symbols.variable_mut(existing) = replacement;
                }
                var_map.insert(var.id, existing);
                continue;
            }
        }

        if matches!(mode, MergeMode::SymbolsOnly)
            && var.storage == trace_ir::StorageClass::Local
            && var.fn_id.is_some()
            && !mapped_fn_id.is_some_and(|fid| {
                program
                    .symbols
                    .function_by_id(fid)
                    .is_some_and(|f| f.linkage == trace_ir::Linkage::Internal && f.is_cpp)
            })
        {
            continue;
        }

        let new_id = program.symbols.alloc_var_id();
        let mut v = var.clone();
        let old = v.id;
        v.id = new_id;
        v.type_id = remap_type(v.type_id, &type_map);
        v.fn_id = mapped_fn_id;
        v.span.file = span_file;
        program.symbols.add_variable(v);
        var_map.insert(old, new_id);
        if let Some(key) = local_key {
            local_by_site.insert(key, new_id);
        }
        if let Some(key) = temp_key {
            // A temporary the base has no counterpart for: record it so a later
            // variant matches it rather than allocating a third copy.
            temps_by_site.entry(key).or_default().push(new_id);
            *temp_cursor.entry((var.fn_id, key)).or_insert(0) += 1;
        }
        if var.fn_id.is_none() {
            if let Some(seen) = variant_dedup.as_deref_mut() {
                seen.file_vars.insert(
                    (
                        source_scope,
                        span_file,
                        var.span.line,
                        var.span.col,
                        var.name.clone(),
                    ),
                    new_id,
                );
            }
        }

        // Record the variable on its function. Lowering leaves `Function::locals`
        // empty — it tracks scope in its own name→id map — so the merge is the
        // stage that fills it, and it must do so in *every* body-merging mode.
        // Filling it only for variants would leave the base configuration's
        // locals invisible to `local_by_site` above, and every variant would
        // then re-allocate a `VarId` for a local the base already has: calls
        // that bind those locals stop comparing equal, and identical facts are
        // recorded once per variant instead of merging (#59 review).
        //
        // In a variant, a parameter that paired with the base by name never
        // reaches here (`var_map` already holds it). One that did not pair is a
        // parameter the variant adds, and it is deliberately recorded as a
        // local: the canonical arity belongs to the base configuration, and
        // `solver` reads `params.len()` as the arity of an indirect-call
        // target, so growing it would stop base call edges from resolving.
        if let Some(canon_fn) = mapped_fn_id {
            let is_param = var.param_index.is_some() && !remerged_fns.contains(&canon_fn);
            if !is_param {
                if let Some(idx) = program.symbols.function_index(canon_fn) {
                    program.symbols.functions[idx].locals.push(new_id);
                }
            }
        }
    }
    for merged_id in &remap_params {
        if let Some(idx) = program.symbols.function_index(*merged_id) {
            let func = &mut program.symbols.functions[idx];
            func.params = func
                .params
                .iter()
                .filter_map(|v| var_map.get(v).copied())
                .collect();
        }
    }

    if program.is_dep_file(primary_file_id) {
        return;
    }

    let mut call_map: FxHashMap<CallSiteId, CallSiteId> = FxHashMap::default();
    for cs in &unit.call_sites {
        if dropped_fns.contains(&cs.caller) {
            continue;
        }
        let Some(&mapped_caller) = fn_map.get(&cs.caller) else {
            continue;
        };
        let span_file = map_file(cs.span.file);
        let occurrence = cs.occurrence();
        let occurrence_file = map_file(occurrence.span.file);
        let occurrence_expansion = occurrence
            .expansion_span
            .map(|span| (map_file(span.file), span.line, span.col));
        // Dependency roots suppress bodies, not project code emitted by a
        // macro declared in a dependency header. For a macro-body call the
        // expansion is its semantic ownership location; the spelling remains
        // the exported request position.
        let ownership_file = map_file(cs.scope_file());
        if program.is_dep_file(ownership_file) {
            continue;
        }
        let key: SiteKey = (
            occurrence_file,
            occurrence.span.line,
            occurrence.span.col,
            occurrence_expansion,
            occurrence.expansion_id,
            cs.callee_name.clone(),
        );
        let is_internal_caller = program
            .symbols
            .function_by_id(mapped_caller)
            .is_some_and(|f| f.linkage == trace_ir::Linkage::Internal && f.is_cpp);
        let shared_caller = program.symbols.is_shared_header_function(mapped_caller);
        // The site's body is being merged into an entry the program already
        // holds: a variant configuration or a shared header body.
        let remerge = matches!(mode, MergeMode::Variant) || shared_caller;
        if matches!(mode, MergeMode::SymbolsOnly) && !is_internal_caller {
            continue;
        }
        if !matches!(mode, MergeMode::Variant) && !is_internal_caller {
            if let Some(&existing) = program.dedup.site_keys.get(&key) {
                call_map.insert(cs.id, existing);
                continue;
            }
        }
        let old = cs.id;
        let mut site = cs.clone();
        site.caller = mapped_caller;
        site.callee_fn_id = site.callee_fn_id.and_then(|f| fn_map.get(&f).copied());
        site.callee_var = site.callee_var.and_then(|v| var_map.get(&v).copied());
        site.var_args = site
            .var_args
            .iter()
            .filter_map(|(i, v)| var_map.get(v).map(|nv| (*i, *nv)))
            .collect();
        site.fn_args = site
            .fn_args
            .iter()
            .filter_map(|(i, f)| fn_map.get(f).map(|nf| (*i, *nf)))
            .collect();
        site.return_dst = site.return_dst.and_then(|v| var_map.get(&v).copied());
        site.span.file = span_file;
        site.expansion_span = site.expansion_span.map(|mut span| {
            span.file = map_file(span.file);
            span
        });
        site.occurrence = site.occurrence.map(|mut occurrence| {
            occurrence.span.file = map_file(occurrence.span.file);
            occurrence.expansion_span = occurrence.expansion_span.map(|mut span| {
                span.file = map_file(span.file);
                span
            });
            occurrence
        });
        // Grouping records by their facts is a remerge concern only, so an
        // ordinary site never pays for the fingerprint — and being the one
        // spelling of "this is a remerge" from here on, it cannot disagree
        // with the registration below.
        let facts = remerge.then(|| site.fact_fingerprint());
        if let Some(facts) = facts {
            // Several configurations can call the same spelled callee at the
            // same source site with different callbacks, receivers or outputs.
            // Keep separate records: solver argument binding uses one actual
            // per position, so appending alternatives to var_args loses facts.
            //
            // Candidates are every record already standing at this site — the
            // base configuration's, plus the ones earlier variants added. That
            // list is program-wide because a site in a header belongs to every
            // unit that includes it: scoped to one unit's variants, a fact two
            // units both recover would be recorded once per unit (#59 review).
            let primary = program.dedup.site_keys.get(&key).copied();
            let states_site = |id: CallSiteId| {
                program
                    .symbols
                    .call_site_by_id(id)
                    .is_some_and(|held| held.same_facts(&site))
            };
            // The base record need not be in the variant buckets, so it is
            // asked for separately; the buckets answer the rest with one
            // probe, which keeps a site whose facts differ in each of N
            // contributing units linear in N rather than quadratic.
            let existing = primary.filter(|&id| states_site(id)).or_else(|| {
                let bucket = program.dedup.variant_site_records.get(&key)?.get(&facts)?;
                bucket
                    .iter()
                    .copied()
                    // `primary` was tested just above; skip the repeat probe.
                    .find(|&id| Some(id) != primary && states_site(id))
            });
            if let Some(existing) = existing {
                if shared_caller {
                    program.symbols.share_header_call(existing, primary_file_id);
                }
                call_map.insert(old, existing);
                continue;
            }
        }
        let new_id = program.symbols.alloc_call_id();
        site.id = new_id;
        site.tu = Some(primary_file_id);
        program.symbols.call_sites.push(site);
        if shared_caller {
            program.symbols.share_header_call(new_id, primary_file_id);
        }
        if !is_internal_caller || shared_caller {
            if let Some(facts) = facts {
                // A variant adds records at a site the base configuration may
                // already own. The key stands for the site across the whole
                // program, and a later unit that reaches it is in the base
                // configuration, not this variant's — so the base record stays
                // canonical and only a site no configuration has claimed yet is
                // registered here. The record is remembered separately either way,
                // so a later variant can merge into it.
                program
                    .dedup
                    .variant_site_records
                    .entry(key.clone())
                    .or_default()
                    .entry(facts)
                    .or_default()
                    .push(new_id);
                program.dedup.site_keys.entry(key).or_insert(new_id);
            } else {
                program.dedup.site_keys.insert(key, new_id);
            }
        }
        call_map.insert(old, new_id);
    }

    let valid_flow = |flow: &FlowConstraint| {
        flow.vars().all(|v| var_map.contains_key(&v))
            && flow_fns(flow).all(|f| fn_map.contains_key(&f))
    };

    let symbols = &program.symbols;
    let is_internal_fn = |fid: FnId| {
        symbols
            .function_by_id(fid)
            .is_some_and(|f| f.linkage == trace_ir::Linkage::Internal && f.is_cpp)
    };

    let is_internal_flow = |flow: &FlowConstraint| {
        flow.vars().any(|v| {
            var_map
                .get(&v)
                .and_then(|&nv| symbols.variable_by_id(nv))
                .and_then(|var| var.fn_id)
                .is_some_and(is_internal_fn)
        })
    };
    // Unit variables owned by a shared header body, so the flow pass below
    // asks one set instead of three lookups per variable of every flow.
    let shared_vars: FxHashSet<VarId> = if unit_shares_headers {
        unit.variables
            .iter()
            .filter(|v| {
                v.fn_id
                    .and_then(|f| fn_map.get(&f))
                    .is_some_and(|&f| symbols.is_shared_header_function(f))
            })
            .map(|v| v.id)
            .collect()
    } else {
        FxHashSet::default()
    };

    let references_this_tus_internal_function = |flow: &FlowConstraint| {
        flow_fns(flow).any(|f| fn_map.get(&f).copied().is_some_and(is_internal_fn))
    };

    let mut internal_init_vars: FxHashSet<VarId> = FxHashSet::default();
    if matches!(mode, MergeMode::SymbolsOnly) {
        let file_scope_vars: FxHashSet<VarId> = unit
            .variables
            .iter()
            .filter(|v| v.fn_id.is_none())
            .map(|v| v.id)
            .collect();
        for flow in &unit.flow {
            if references_this_tus_internal_function(flow) {
                for v in flow.vars() {
                    if file_scope_vars.contains(&v) {
                        internal_init_vars.insert(v);
                    }
                }
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for flow in &unit.flow {
                match flow {
                    FlowConstraint::Store { dst, src }
                    | FlowConstraint::Copy { dst, src }
                    | FlowConstraint::Load { dst, src }
                    | FlowConstraint::AddrOfVar { dst, src }
                    | FlowConstraint::UnwrapPointer { dst, src }
                        if file_scope_vars.contains(dst) && file_scope_vars.contains(src) =>
                    {
                        let has_dst = internal_init_vars.contains(dst);
                        let has_src = internal_init_vars.contains(src);
                        if (has_dst || has_src) && (!has_dst || !has_src) {
                            internal_init_vars.insert(*dst);
                            internal_init_vars.insert(*src);
                            changed = true;
                        }
                    }
                    FlowConstraint::GepField { dst, base, .. }
                        if file_scope_vars.contains(dst) && file_scope_vars.contains(base) =>
                    {
                        let has_dst = internal_init_vars.contains(dst);
                        let has_base = internal_init_vars.contains(base);
                        if (has_dst || has_base) && (!has_dst || !has_base) {
                            internal_init_vars.insert(*dst);
                            internal_init_vars.insert(*base);
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let is_supporting_flow = |flow: &FlowConstraint| {
        if internal_init_vars.is_empty() {
            return false;
        }
        match flow {
            FlowConstraint::Store { dst, src }
            | FlowConstraint::Copy { dst, src }
            | FlowConstraint::Load { dst, src }
            | FlowConstraint::AddrOfVar { dst, src }
            | FlowConstraint::UnwrapPointer { dst, src } => {
                internal_init_vars.contains(dst) && internal_init_vars.contains(src)
            }
            FlowConstraint::GepField { dst, base, .. } => {
                internal_init_vars.contains(dst) && internal_init_vars.contains(base)
            }
            _ => false,
        }
    };

    let replay_in_tu = |flow: &FlowConstraint| {
        is_internal_flow(flow)
            || references_this_tus_internal_function(flow)
            || is_supporting_flow(flow)
    };

    // Shared header facts span TU families, whereas variant facts are local
    // to one family. Every flow of a shared body passes through the
    // program-wide header index, which therefore subsumes the family's own.
    for flow in &unit.flow {
        if matches!(mode, MergeMode::SymbolsOnly) && !replay_in_tu(flow) {
            continue;
        }
        if !valid_flow(flow) {
            continue;
        }
        let remapped = remap_flow(flow, &fn_map, &var_map);
        let shared_body = !shared_vars.is_empty() && flow.vars().any(|v| shared_vars.contains(&v));
        let seen = if shared_body {
            Some(&mut program.dedup.header_flow)
        } else {
            variant_dedup.as_deref_mut().map(|seen| &mut seen.flow)
        };
        if seen.is_some_and(|seen| !seen.insert(remapped.clone())) {
            continue;
        }
        program.flow.push(remapped);
    }

    for (old_fn, flows) in &unit.fn_returns {
        if dropped_fns.contains(old_fn) {
            continue;
        }
        let Some(&new_fn) = fn_map.get(old_fn) else {
            continue;
        };
        if program
            .symbols
            .function_by_id(new_fn)
            .is_some_and(|f| program.is_dep_file(f.file))
        {
            continue;
        }
        if matches!(mode, MergeMode::SymbolsOnly) {
            let is_internal = program
                .symbols
                .function_by_id(new_fn)
                .is_some_and(|f| f.linkage == trace_ir::Linkage::Internal && f.is_cpp);
            if !is_internal {
                continue;
            }
        }
        let remapped: Vec<ReturnFlow> = flows
            .iter()
            .filter(|f| {
                f.var().into_iter().all(|v| var_map.contains_key(&v))
                    && return_flow_fns(f).all(|callee| fn_map.contains_key(&callee))
            })
            .map(|f| remap_return_flow(f, &fn_map, &var_map))
            .collect();
        let target = program.fn_returns.entry(new_fn).or_default();
        if matches!(mode, MergeMode::Variant) || symbols.is_shared_header_function(new_fn) {
            // Variants and shared header bodies may repeat return facts.
            // Ordinary, unshared bodies need no duplicate search.
            for rf in remapped {
                if !target.contains(&rf) {
                    target.push(rf);
                }
            }
        } else {
            target.extend(remapped);
        }
    }
}

fn flow_fns(flow: &FlowConstraint) -> impl Iterator<Item = FnId> {
    match flow {
        FlowConstraint::AddrOfFn { callee, .. } | FlowConstraint::ArrayFnMember { callee, .. } => {
            Some(*callee)
        }
        // A call follows the function it is written in: dropped with it in
        // `valid_flow`, replayed with it in `MergeMode::SymbolsOnly`.
        FlowConstraint::CallReturn { caller, .. } => *caller,
        _ => None,
    }
    .into_iter()
}

fn return_flow_fns(flow: &ReturnFlow) -> impl Iterator<Item = FnId> {
    match flow {
        ReturnFlow::AddrOfFn { callee } => Some(*callee),
        _ => None,
    }
    .into_iter()
}

/// `map` is indexed by the source table's id: those are dense and
/// [`TypeTable::all`](trace_ir::TypeTable::all) yields them in order, so the
/// per-type remap a merge does for every variable and return type is an
/// index rather than a hash of the id.
fn remap_type(id: TypeId, map: &[TypeId]) -> TypeId {
    map.get(id.0 as usize).copied().unwrap_or(id)
}

fn merge_types(
    dst: &mut trace_ir::TypeTable,
    src: &trace_ir::TypeTable,
    union_aggregates: bool,
    merge_descs: Option<&[Arc<TypeDesc>]>,
) -> Vec<TypeId> {
    dst.merge_struct_declarations(src);
    let mut map = Vec::with_capacity(src.all().len());
    for (i, info) in src.all().iter().enumerate() {
        // The precomputed descriptor is what the arms below would build;
        // interning it by address skips both the rebuild and the hashing.
        if let Some(desc) = merge_descs.filter(|_| !union_aggregates).map(|d| &d[i]) {
            let new_id = dst.intern_arc(desc);
            debug_assert_eq!(info.id.0 as usize, map.len());
            map.push(new_id);
            continue;
        }
        let new_id = match info.desc.as_ref() {
            // The layout usually still spells the descriptor's own fields, and
            // then interning the descriptor by reference is the same as
            // interning the rebuilt one, without cloning every field deep. A
            // unit re-merges each header's aggregates, so this is most of them.
            TypeDesc::Struct { fields, .. } | TypeDesc::Union { fields, .. }
                if !fields.is_empty() && !union_aggregates && layout_spells_desc(src, info) =>
            {
                dst.intern_ref(&info.desc)
            }
            TypeDesc::Struct { name, fields } if !fields.is_empty() => {
                if union_aggregates {
                    dst.union_struct_layout(name.clone(), fields_from_layout(src, info))
                } else {
                    dst.compute_struct_layout(name.clone(), fields_from_layout(src, info))
                }
            }
            TypeDesc::Union { name, fields } if !fields.is_empty() => {
                if union_aggregates {
                    dst.union_union_layout(name.clone(), fields_from_layout(src, info))
                } else {
                    dst.compute_union_layout(name.clone(), fields_from_layout(src, info))
                }
            }
            other => dst.intern_ref(other),
        };
        debug_assert_eq!(info.id.0 as usize, map.len());
        map.push(new_id);
    }
    for (alias, desc) in src.all_aliases() {
        if dst.resolve_alias(alias).is_none() {
            dst.register_alias_ref(alias, desc);
        }
    }
    map
}

/// Whether [`fields_from_layout`] would rebuild `info`'s descriptor as it is.
fn layout_spells_desc(src: &trace_ir::TypeTable, info: &trace_ir::TypeInfo) -> bool {
    let (TypeDesc::Struct { fields, .. } | TypeDesc::Union { fields, .. }) = info.desc.as_ref()
    else {
        return false;
    };
    fields.len() == info.layout.fields.len()
        && fields
            .iter()
            .zip(info.layout.fields.values())
            .all(|((name, desc), fl)| *name == fl.name && src.get(fl.type_id).desc.as_ref() == desc)
}

/// Prefer layout field types over the interned `TypeDesc` field list: PCH
/// intern may have rewritten nested empty tags (`struct IDeviceIoService`)
/// in the layout while the desc still stores the incomplete tag.
fn fields_from_layout(
    src: &trace_ir::TypeTable,
    info: &trace_ir::TypeInfo,
) -> Vec<(String, TypeDesc)> {
    info.layout
        .fields
        .iter()
        .map(|(_, fl)| (fl.name.clone(), src.get(fl.type_id).desc.as_ref().clone()))
        .collect()
}

/// The unit's C++ internal-linkage declarations that another definition of
/// the unit defines, each with that definition. A `static` function or a
/// member of a class in an anonymous namespace that is called but never
/// defined does not link, so a declaration no definition joined when it was
/// registered is defined under another spelling of a parameter type
/// (`static void f(ns::Obj *);` and `static void f(Obj *) {}` under
/// `using namespace ns`, since a type name lowering could not resolve reads as
/// `int`). The definition is the one of the same name and arity whose
/// parameter types are the declaration's, else the one whose parameter types
/// may name them; several candidates leave the declaration alone.
fn respelled_declarations(
    unit: &UnitIndex,
    param_types: &FxHashMap<VarId, TypeId>,
    types: &trace_ir::TypeTable,
) -> BTreeMap<FnId, FnId> {
    let undefined: FxHashSet<&str> = unit
        .functions
        .iter()
        .filter(|f| f.is_cpp && !f.is_defined && f.linkage == trace_ir::Linkage::Internal)
        .map(|f| f.name.as_str())
        .collect();
    if undefined.is_empty() {
        return BTreeMap::new();
    }
    let mut by_name: BTreeMap<&str, Vec<&Function>> = BTreeMap::new();
    for func in unit
        .functions
        .iter()
        .filter(|f| f.is_cpp && undefined.contains(f.name.as_str()))
    {
        by_name.entry(func.name.as_str()).or_default().push(func);
    }
    let arity = |f: &Function| f.explicit_arity.map_or(f.params.len(), |n| n as usize);
    // Whether every parameter pair satisfies `same`; a side without parameter
    // variables (an in-class prototype) says nothing against it.
    let params_agree = |a: &Function, b: &Function, same: &dyn Fn(TypeId, TypeId) -> bool| {
        a.params.is_empty()
            || b.params.is_empty()
            || (a.params.len() == b.params.len()
                && a.params.iter().zip(&b.params).all(|(x, y)| {
                    match (param_types.get(x), param_types.get(y)) {
                        (Some(&x), Some(&y)) => same(x, y),
                        _ => true,
                    }
                }))
    };
    let mut out = BTreeMap::new();
    for group in by_name.values().filter(|group| group.len() > 1) {
        let declarations = group
            .iter()
            .filter(|f| !f.is_defined && f.linkage == trace_ir::Linkage::Internal);
        for declaration in declarations {
            let definitions: Vec<&Function> = group
                .iter()
                .copied()
                .filter(|f| {
                    f.is_defined
                        && f.linkage == trace_ir::Linkage::Internal
                        && f.variadic == declaration.variadic
                        && arity(f) == arity(declaration)
                })
                .collect();
            let only = |same: &dyn Fn(TypeId, TypeId) -> bool| {
                let mut fitting = definitions
                    .iter()
                    .filter(|f| params_agree(declaration, f, same));
                match (fitting.next(), fitting.next()) {
                    (Some(f), None) => Some(f.id),
                    _ => None,
                }
            };
            let exact = |x: TypeId, y: TypeId| x == y;
            let loose = |x: TypeId, y: TypeId| {
                trace_ir::may_name_same_type(types.get(x).desc.as_ref(), types.get(y).desc.as_ref())
            };
            if let Some(definition) = only(&exact).or_else(|| only(&loose)) {
                out.insert(declaration.id, definition);
            }
        }
    }
    out
}

fn remap_flow(
    flow: &FlowConstraint,
    fn_map: &FxHashMap<FnId, FnId>,
    var_map: &FxHashMap<VarId, VarId>,
) -> FlowConstraint {
    let rv = |v: VarId| var_map.get(&v).copied().unwrap_or(v);
    let rf = |f: FnId| fn_map.get(&f).copied().unwrap_or(f);
    match flow {
        FlowConstraint::Copy { dst, src } => FlowConstraint::Copy {
            dst: rv(*dst),
            src: rv(*src),
        },
        FlowConstraint::AddrOfVar { dst, src } => FlowConstraint::AddrOfVar {
            dst: rv(*dst),
            src: rv(*src),
        },
        FlowConstraint::AddrOfFn { dst, callee } => FlowConstraint::AddrOfFn {
            dst: rv(*dst),
            callee: rf(*callee),
        },
        FlowConstraint::Load { dst, src } => FlowConstraint::Load {
            dst: rv(*dst),
            src: rv(*src),
        },
        FlowConstraint::Store { dst, src } => FlowConstraint::Store {
            dst: rv(*dst),
            src: rv(*src),
        },
        FlowConstraint::GepField {
            dst,
            base,
            field,
            field_name,
        } => FlowConstraint::GepField {
            dst: rv(*dst),
            base: rv(*base),
            field: *field,
            field_name: field_name.clone(),
        },
        FlowConstraint::ArrayFnMember { array, callee } => FlowConstraint::ArrayFnMember {
            array: rv(*array),
            callee: rf(*callee),
        },
        FlowConstraint::CallReturn {
            dst,
            callee_name,
            caller,
        } => FlowConstraint::CallReturn {
            dst: rv(*dst),
            callee_name: callee_name.clone(),
            caller: caller.map(rf),
        },
        FlowConstraint::CallReturnIndirect { dst, callee_var } => {
            FlowConstraint::CallReturnIndirect {
                dst: rv(*dst),
                callee_var: rv(*callee_var),
            }
        }
        FlowConstraint::NewHeap { dst } => FlowConstraint::NewHeap { dst: rv(*dst) },
        FlowConstraint::StringConst { dst, value } => FlowConstraint::StringConst {
            dst: rv(*dst),
            value: value.clone(),
        },
        FlowConstraint::UnwrapPointer { dst, src } => FlowConstraint::UnwrapPointer {
            dst: rv(*dst),
            src: rv(*src),
        },
    }
}

fn remap_return_flow(
    flow: &ReturnFlow,
    fn_map: &FxHashMap<FnId, FnId>,
    var_map: &FxHashMap<VarId, VarId>,
) -> ReturnFlow {
    let rv = |v: VarId| var_map.get(&v).copied().unwrap_or(v);
    let rf = |f: FnId| fn_map.get(&f).copied().unwrap_or(f);
    match flow {
        ReturnFlow::AddrOfVar { src } => ReturnFlow::AddrOfVar { src: rv(*src) },
        ReturnFlow::AddrOfFn { callee } => ReturnFlow::AddrOfFn {
            callee: rf(*callee),
        },
        ReturnFlow::Copy { src } => ReturnFlow::Copy { src: rv(*src) },
        ReturnFlow::Call { callee_name } => ReturnFlow::Call {
            callee_name: callee_name.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_ir::{Diagnostic, DiagnosticSeverity};

    fn unit_reporting(path: &str, stage: &str) -> UnitIndex {
        UnitIndex {
            path: PathBuf::from(path),
            files: vec![PathBuf::from(path)],
            diagnostics: vec![Diagnostic {
                severity: DiagnosticSeverity::Error,
                file: Some(trace_ir::FileId(0)),
                line: 12,
                message: "unknown type name".into(),
                stage: stage.into(),
            }],
            ..Default::default()
        }
    }

    /// Both ends of an unwrap are variables the merge renumbers.
    #[test]
    fn remap_flow_renumbers_both_unwrap_endpoints() {
        let var_map: FxHashMap<VarId, VarId> = [(VarId(1), VarId(11)), (VarId(2), VarId(12))]
            .into_iter()
            .collect();
        let flow = FlowConstraint::UnwrapPointer {
            dst: VarId(1),
            src: VarId(2),
        };
        assert_eq!(
            remap_flow(&flow, &FxHashMap::default(), &var_map),
            FlowConstraint::UnwrapPointer {
                dst: VarId(11),
                src: VarId(12),
            }
        );
    }

    /// The remap is indexed by the source id (#86), so it must line up with
    /// `TypeTable::all` for every id the source hands out: one the
    /// destination already holds under another number, one it has to add,
    /// and one it canonicalizes to a richer layout on the way in.
    #[test]
    fn merge_types_remaps_every_source_id_by_index() {
        let full_b = TypeDesc::Struct {
            name: "B".into(),
            fields: vec![("y".into(), TypeDesc::Int)],
        };
        let mut src = trace_ir::TypeTable::new();
        let a = src.intern(TypeDesc::Struct {
            name: "A".into(),
            fields: vec![("x".into(), TypeDesc::Int)],
        });
        let p = src.intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: "B".into(),
            fields: Vec::new(),
        })));
        let mut dst = trace_ir::TypeTable::new();
        let b = dst.intern(full_b.clone());

        let map = merge_types(&mut dst, &src, false, None);

        assert_eq!(map.len(), src.all().len(), "one slot per source id");
        assert_eq!(
            remap_type(TypeId(0), &map),
            TypeId(0),
            "the prelude is shared"
        );
        let a_dst = remap_type(a, &map);
        assert_ne!(a_dst, a, "`A` lands after `B`, so its number moves");
        assert_eq!(dst.get(a_dst).desc, src.get(a).desc);
        assert_eq!(
            dst.get(remap_type(p, &map)).desc,
            TypeDesc::Ptr(Box::new(full_b)).into(),
            "the empty tag canonicalizes to the layout the destination holds"
        );
        assert_eq!(dst.type_id_by_tag("B", trace_ir::TypeKind::Struct), Some(b));
        let beyond = TypeId(map.len() as u32 + 7);
        assert_eq!(
            remap_type(beyond, &map),
            beyond,
            "an id outside the map is its own"
        );
    }

    fn int_field(name: &str) -> (String, TypeDesc) {
        (name.into(), TypeDesc::Int)
    }

    fn tag(name: &str, fields: Vec<(String, TypeDesc)>) -> TypeDesc {
        TypeDesc::Struct {
            name: name.into(),
            fields,
        }
    }

    /// Interning an aggregate by reference when its layout still spells its
    /// descriptor is a shortcut: it must leave the destination exactly as
    /// rebuilding the field list from the layout does.
    #[test]
    fn merging_an_unchanged_aggregate_matches_rebuilding_it() {
        let mut src = trace_ir::TypeTable::new();
        let inner = tag("Inner", vec![int_field("x")]);
        src.intern(tag(
            "Outer",
            vec![
                ("inner".into(), inner.clone()),
                ("next".into(), TypeDesc::Ptr(Box::new(inner))),
            ],
        ));
        src.intern(TypeDesc::Union {
            name: "U".into(),
            fields: vec![int_field("i"), ("l".into(), TypeDesc::Long)],
        });
        for info in src.all() {
            if matches!(info.desc.as_ref(), TypeDesc::Struct { fields, .. } | TypeDesc::Union { fields, .. } if !fields.is_empty())
            {
                assert!(layout_spells_desc(&src, info), "{:?}", info.desc);
            }
        }

        let mut fast = trace_ir::TypeTable::new();
        let fast_map = merge_types(&mut fast, &src, false, None);
        let mut rebuilt = trace_ir::TypeTable::new();
        let rebuilt_map: Vec<TypeId> = src
            .all()
            .iter()
            .map(|info| match info.desc.as_ref() {
                TypeDesc::Struct { name, fields } if !fields.is_empty() => {
                    rebuilt.compute_struct_layout(name.clone(), fields_from_layout(&src, info))
                }
                TypeDesc::Union { name, fields } if !fields.is_empty() => {
                    rebuilt.compute_union_layout(name.clone(), fields_from_layout(&src, info))
                }
                other => rebuilt.intern_ref(other),
            })
            .collect();
        assert_eq!(fast_map, rebuilt_map);
        assert_eq!(format!("{:?}", fast.all()), format!("{:?}", rebuilt.all()));
    }

    /// Completing a nested tag rewrites the layout but not the descriptor, so
    /// the merge has to take the layout's completed field, not the shortcut.
    #[test]
    fn a_completed_nested_tag_is_merged_from_the_layout() {
        let mut src = trace_ir::TypeTable::new();
        let outer = src.intern(tag(
            "Outer",
            vec![("inner".into(), tag("Inner", Vec::new()))],
        ));
        src.intern(tag("Inner", vec![int_field("x")]));
        src.complete_nested_tags();
        assert!(!layout_spells_desc(&src, src.get(outer)));

        let mut dst = trace_ir::TypeTable::new();
        let map = merge_types(&mut dst, &src, false, None);
        assert_eq!(
            dst.get(remap_type(outer, &map)).desc.as_ref(),
            &tag(
                "Outer",
                vec![("inner".into(), tag("Inner", vec![int_field("x")]))]
            )
        );
    }

    /// A struct spelled with other fields under the same name is a different
    /// type, shortcut or not.
    #[test]
    fn a_same_named_struct_with_other_fields_stays_apart() {
        let mut src = trace_ir::TypeTable::new();
        let narrow = src.intern(tag("S", vec![int_field("a")]));
        let mut dst = trace_ir::TypeTable::new();
        let wide = dst.intern(tag("S", vec![("a".into(), TypeDesc::Long)]));

        let map = merge_types(&mut dst, &src, false, None);
        let merged = remap_type(narrow, &map);
        assert_ne!(merged, wide);
        assert_eq!(dst.get(merged).desc, src.get(narrow).desc);
        assert_eq!(
            dst.get(wide).desc.as_ref(),
            &tag("S", vec![("a".into(), TypeDesc::Long)])
        );
    }

    /// A variant re-lowers the whole unit, so it re-reports everything the base
    /// already did. That holds for every stage, not only `preprocess` (#59).
    #[test]
    fn a_variant_does_not_repeat_a_diagnostic_the_base_reported() {
        for stage in ["parse", "preprocess"] {
            let mut program = Program::new(PathBuf::from("/tmp/root"));
            let base = unit_reporting("a.c", stage);
            let variant = unit_reporting("a.c", stage);
            merge_unit_variants(&mut program, &base, std::slice::from_ref(&variant));
            assert_eq!(
                program.diagnostics.len(),
                1,
                "`{stage}` reported once by the base and once per variant: {:?}",
                program.diagnostics
            );
        }
    }

    /// One translation unit contributing `name(T *)`, with `T`'s nested tag
    /// complete or not, as `merge_unit_index` sees it.
    fn unit_declaring(
        path: &str,
        defined: bool,
        nested_fields: Vec<(String, TypeDesc)>,
    ) -> UnitIndex {
        unit_declaring_param(path, defined, nested_fields, "object")
    }

    /// As [`unit_declaring`], but naming the parameter, so a test can tell
    /// which unit's variable a merged function's `params` point at.
    fn unit_declaring_param(
        path: &str,
        defined: bool,
        nested_fields: Vec<(String, TypeDesc)>,
        param_name: &str,
    ) -> UnitIndex {
        let mut types = trace_ir::TypeTable::new();
        let param_type = types.intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
            name: "Svc".into(),
            fields: vec![(
                "object".into(),
                TypeDesc::Struct {
                    name: "Obj".into(),
                    fields: nested_fields,
                },
            )],
        })));
        let fn_id = FnId(0);
        let param = VarId(0);
        UnitIndex {
            path: PathBuf::from(path),
            files: vec![PathBuf::from(path)],
            types,
            functions: vec![Function {
                is_weak: false,
                target: None,
                id: fn_id,
                name: "recycle".into(),
                linkage: trace_ir::Linkage::External,
                return_type: TypeId(0),
                params: vec![param],
                locals: Vec::new(),
                span: trace_ir::Span::new(trace_ir::FileId(0), if defined { 40 } else { 8 }, 1),
                end_line: if defined { 44 } else { 8 },
                file: trace_ir::FileId(0),
                is_defined: defined,
                param_type_ids: Vec::new(),
                explicit_arity: Some(1),
                default_args: 0,
                owner_unresolved: false,
                variadic: false,
                defaulted_in_class: false,
                declared_in_class: false,
                is_virtual: false,
                is_final: false,
                is_cpp: true,
                tu: None,
            }],
            variables: vec![Variable {
                is_defined: false,
                is_weak: false,
                target: None,
                is_namespaced: false,
                qualified_name: None,
                c_linkage: false,
                id: param,
                name: param_name.into(),
                type_id: param_type,
                storage: trace_ir::StorageClass::Param,
                fn_id: Some(fn_id),
                param_index: Some(0),
                span: trace_ir::Span::new(trace_ir::FileId(0), 8, 1),
                is_pointer: true,
            }],
            ..Default::default()
        }
    }

    /// Issue #127 review: one unit can carry the same header body twice (a
    /// cached header expansion plus the translation unit's own copy). Both
    /// merge into one function; the second copy's temporaries must match the
    /// first's, or each call in the body is recorded twice.
    #[test]
    fn a_header_body_twice_in_one_unit_keeps_one_record_per_call() {
        let header = trace_ir::FileId(1);
        let body = |id: u32| Function {
            is_weak: false,
            target: None,
            id: FnId(id),
            name: "Unmarshal".into(),
            linkage: trace_ir::Linkage::Internal,
            return_type: TypeId(0),
            params: vec![VarId(id * 10)],
            locals: Vec::new(),
            span: trace_ir::Span::new(header, 81, 1),
            end_line: 90,
            file: header,
            is_defined: true,
            param_type_ids: Vec::new(),
            explicit_arity: Some(1),
            default_args: 0,
            owner_unresolved: false,
            variadic: false,
            defaulted_in_class: false,
            declared_in_class: false,
            is_virtual: false,
            is_final: false,
            is_cpp: true,
            tu: None,
        };
        let var = |id: u32, name: String, owner: u32, param: bool| Variable {
            is_defined: false,
            is_weak: false,
            target: None,
            is_namespaced: false,
            qualified_name: None,
            c_linkage: false,
            id: VarId(id),
            name,
            type_id: TypeId(0),
            storage: if param {
                trace_ir::StorageClass::Param
            } else {
                trace_ir::StorageClass::Local
            },
            fn_id: Some(FnId(owner)),
            param_index: param.then_some(0),
            span: trace_ir::Span::new(
                header,
                if param { 81 } else { 86 },
                if param { 30 } else { 75 },
            ),
            is_pointer: true,
        };
        let site = |id: u32, owner: u32| CallSite {
            id: trace_ir::CallSiteId(id),
            caller: FnId(owner),
            callee_name: "ReadBuffer".into(),
            callee_var: None,
            callee_fn_id: None,
            var_args: vec![(0, VarId(owner * 10)), (2, VarId(owner * 10 + 1))],
            fn_args: Vec::new(),
            addr_of_member_args: Vec::new(),
            addr_of_args: vec![2],
            args_bound_past_this: false,
            span: trace_ir::Span::new(header, 86, 10),
            expansion_span: None,
            occurrence: None,
            is_direct: true,
            receiver_class: None,
            return_dst: None,
            tu: None,
        };
        let text: Arc<str> = Arc::from("static inline int Unmarshal(struct Buf *data) { .. }");
        let mut unit = UnitIndex {
            path: PathBuf::from("main.cpp"),
            files: vec![PathBuf::from("main.cpp"), PathBuf::from("sample.h")],
            functions: vec![body(1), body(2)],
            variables: vec![
                var(10, "data".into(), 1, true),
                var(11, "_ret11".into(), 1, false),
                var(20, "data".into(), 2, true),
                var(21, "_ret21".into(), 2, false),
            ],
            call_sites: vec![site(0, 1), site(1, 2)],
            ..Default::default()
        };
        unit.internal_definitions.insert(FnId(1), Arc::clone(&text));
        unit.internal_definitions.insert(FnId(2), text);
        let mut program = Program::new(PathBuf::from("root"));
        merge_unit_index(&mut program, &unit);
        let records = program
            .symbols
            .call_sites
            .iter()
            .filter(|cs| cs.callee_name == "ReadBuffer")
            .count();
        assert_eq!(program.symbols.functions.len(), 1, "one function");
        assert_eq!(records, 1, "one record for the one call");
    }

    #[test]
    fn internal_definition_text_is_retained_only_for_tu_preambles() {
        let mut unit = unit_declaring("util.h", true, Vec::new());
        unit.functions[0].linkage = trace_ir::Linkage::Internal;
        let text: Arc<str> = Arc::from("static void recycle(Svc *object) {}");
        unit.internal_definitions
            .insert(unit.functions[0].id, Arc::clone(&text));
        let mut preamble = Program::new(PathBuf::from("root"));
        merge_unit_symbols(&mut preamble, &unit);
        let id = preamble.symbols.functions[0].id;
        assert_eq!(preamble.dedup.internal_definitions.get(&id), Some(&text));
        let mut merged = Program::new(PathBuf::from("root"));
        merge_unit_index(&mut merged, &unit);
        assert!(
            merged.dedup.internal_definitions.is_empty(),
            "the final merge must not retain a second definition-text index"
        );
    }

    /// The merge has to hand the symbol table the type table its remapped
    /// parameter ids live in, or the C++ overload check falls back to
    /// comparing ids — and the prototype's unit and the defining unit intern
    /// their own `struct Svc *` whenever they disagree on how complete the
    /// nested `struct Obj` is, which left every C caller of hdf's IPC
    /// interface bound to an undefined prototype (#83).
    #[test]
    fn a_prototype_and_its_definition_collapse_across_units() {
        let mut program = Program::new(PathBuf::from("/tmp/root"));
        // The prototype's unit had seen `struct Obj`'s field; the defining
        // unit had not.
        let proto = unit_declaring("if.h", false, vec![("objectId".into(), TypeDesc::Int)]);
        let def = unit_declaring("impl.cpp", true, Vec::new());
        merge_unit_index(&mut program, &proto);
        merge_unit_index(&mut program, &def);
        let recycle: Vec<_> = program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == "recycle")
            .collect();
        assert_eq!(
            recycle.len(),
            1,
            "prototype and definition must be one record: {recycle:?}"
        );
        assert!(recycle[0].is_defined, "the caller must reach the body");
    }

    /// The other direction of the same signal. When a *definition* merges
    /// into an existing prototype it hands over its own parameter list, and
    /// those ids are still unit-local: they must be remapped, or the entry
    /// points at whichever global variable happens to share the number —
    /// here the prototype's, so the body's flow facts would reference a
    /// parameter the function does not list.
    #[test]
    fn a_definition_merging_into_a_prototype_remaps_the_params_it_hands_over() {
        let mut program = Program::new(PathBuf::from("/tmp/root"));
        let proto = unit_declaring_param("if.h", false, Vec::new(), "declared_object");
        merge_unit_index(&mut program, &proto);
        let def = unit_declaring_param("impl.cpp", true, Vec::new(), "defined_object");
        merge_unit_index(&mut program, &def);

        let id = program.symbols.resolve_function("recycle").unwrap();
        let f = program.symbols.function(id);
        assert!(f.is_defined, "the definition must win the entry");
        let param = f.params[0];
        let var = program
            .symbols
            .variables
            .iter()
            .find(|v| v.id == param)
            .expect("the listed parameter must be a variable this program owns");
        assert_eq!(
            var.name, "defined_object",
            "the entry must point at the defining unit's parameter, not the \
             prototype's variable that shares its unit-local number"
        );
    }

    #[test]
    fn later_prototype_does_not_split_a_repeated_definition() {
        let mut program = Program::new(PathBuf::from("/tmp/root"));
        let def = unit_declaring("impl.cpp", true, Vec::new());
        let proto = unit_declaring("if.h", false, vec![("objectId".into(), TypeDesc::Int)]);
        merge_unit_index(&mut program, &def);
        merge_unit_index(&mut program, &proto);
        // Distinct definitions from separate compile units are indexed separately.
        let repeated = unit_declaring("other.cpp", true, Vec::new());
        merge_unit_index(&mut program, &repeated);
        assert_eq!(
            program.symbols.functions.len(),
            2,
            "distinct definitions from separate compile units are indexed separately"
        );
    }

    #[test]
    fn later_prototype_does_not_collapse_distinct_definitions() {
        let mut program = Program::new(PathBuf::from("/tmp/root"));
        let def = unit_declaring("impl.cpp", true, Vec::new());
        let fields = vec![("objectId".into(), TypeDesc::Int)];
        let proto = unit_declaring("if.h", false, fields.clone());
        let other = unit_declaring("mock.cpp", true, fields);
        merge_unit_index(&mut program, &def);
        merge_unit_index(&mut program, &proto);
        merge_unit_index(&mut program, &other);
        assert_eq!(
            program.symbols.functions.len(),
            2,
            "a shape-compatible prototype must not erase the distinction between bodies"
        );
        assert!(program.symbols.functions.iter().all(|f| f.is_defined));
    }

    #[test]
    fn later_prototype_preserves_definition_parameter_identity() {
        let mut program = Program::new(PathBuf::from("/tmp/root"));
        let def = unit_declaring("impl.cpp", true, Vec::new());
        merge_unit_index(&mut program, &def);
        let id = program.symbols.resolve_function("recycle").unwrap();
        let original_params = program.symbols.function(id).params.clone();
        // Both units allocate VarId(0), which also happens to be the first
        // definition's global parameter ID. Equality is not proof of adoption.
        let proto = unit_declaring("if.h", false, Vec::new());
        merge_unit_index(&mut program, &proto);
        assert_eq!(program.symbols.function(id).params, original_params);
    }

    /// Deduplication must not silence a *different* stage reporting the same
    /// text at the same position — those are two findings, not one.
    #[test]
    fn a_variant_keeps_a_report_from_a_stage_the_base_did_not_report() {
        let mut program = Program::new(PathBuf::from("/tmp/root"));
        let base = unit_reporting("a.c", "preprocess");
        let variant = unit_reporting("a.c", "parse");
        merge_unit_variants(&mut program, &base, std::slice::from_ref(&variant));
        assert_eq!(program.diagnostics.len(), 2, "{:?}", program.diagnostics);
    }
}
