use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Path, PathBuf};
use trace_ir::{
    same_param_type_or_unresolved, CallSite, CallSiteId, FlowConstraint, FnId, Function, Program,
    ReturnFlow, TemplateBase, TypeDesc, TypeId, VarId, Variable,
};

/// Per-file indexing result merged into a single [`Program`].
#[derive(Debug, Clone, Default)]
pub struct UnitIndex {
    pub path: PathBuf,
    /// Unit-local file table: index == unit-local [`trace_ir::FileId`] value.
    /// Includes the TU itself plus every `#include`d origin that produced
    /// attributed entities.
    pub files: Vec<PathBuf>,
    pub types: trace_ir::TypeTable,
    pub functions: Vec<Function>,
    pub variables: Vec<Variable>,
    pub call_sites: Vec<CallSite>,
    pub flow: Vec<FlowConstraint>,
    pub fn_returns: FxHashMap<FnId, Vec<ReturnFlow>>,
    pub diagnostics: Vec<trace_ir::Diagnostic>,
    pub anon_type_counter: u32,
    /// Per-unit `(derived, base)` class edges (C++).
    pub inheritance: Vec<(String, String)>,
    /// Per-unit templated base facts (C++).
    pub template_bases: Vec<TemplateBase>,
    /// Per-unit declared `operator->` returns (C++), merged in every mode.
    pub arrow_returns: Vec<trace_ir::ArrowReturn>,
    /// Classes declared `final` in this unit.
    pub final_classes: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeMode {
    Full,
    TypesOnly,
    SymbolsOnly,
    Variant,
}

type SiteKey = (trace_ir::FileId, u32, u32, String);
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
fn call_site_index(symbols: &trace_ir::SymbolTable, id: CallSiteId) -> Option<usize> {
    let index = id.0 as usize;
    if symbols.call_sites.get(index).is_some_and(|s| s.id == id) {
        return Some(index);
    }
    symbols.call_sites.iter().position(|s| s.id == id)
}

#[inline]
#[must_use]
fn same_call_facts(a: &CallSite, b: &CallSite) -> bool {
    a.caller == b.caller
        && a.callee_fn_id == b.callee_fn_id
        && a.callee_var == b.callee_var
        && a.var_args == b.var_args
        && a.fn_args == b.fn_args
        && a.addr_of_member_args == b.addr_of_member_args
        && a.is_direct == b.is_direct
        && a.receiver_class == b.receiver_class
        && a.return_dst == b.return_dst
}

pub fn merge_unit_index(program: &mut Program, unit: &UnitIndex) {
    merge_unit(program, unit, MergeMode::Full, None);
}

/// Merge a translation unit together with its configuration variants (#59).
///
/// Variants re-lower the whole unit, so each one repeats every flow constraint
/// of the code the configurations share. Those duplicates are dropped against
/// a set scoped to *this* unit's contribution: deduplicating against the whole
/// program would rebuild a set of every constraint merged so far, once per
/// variant.
pub fn merge_unit_variants(program: &mut Program, base: &UnitIndex, variants: &[UnitIndex]) {
    let flow_start = program.flow.len();
    let var_start = program.symbols.variables.len();
    merge_unit(program, base, MergeMode::Full, None);
    if variants.is_empty() {
        return;
    }
    // Every unit below merges with `union_aggregates`, whatever the per-source
    // variant tally ends up saying.
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
    for unit in variants {
        merge_unit(program, unit, MergeMode::Variant, Some(&mut seen));
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

/// TU preamble: types plus prototypes; header flow stays on the defining unit.
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
        program.add_template_base(&fact.derived, &fact.spelling, &fact.declaration_scope);
    }
    for fact in &unit.arrow_returns {
        if !program.arrow_returns.contains(fact) {
            program.arrow_returns.push(fact.clone());
        }
    }
    for cls in &unit.final_classes {
        program.mark_class_final(cls);
    }

    let type_map = merge_types(
        &mut program.types,
        &unit.types,
        matches!(mode, MergeMode::Variant),
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

    if matches!(mode, MergeMode::Full | MergeMode::Variant) {
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
    let mut variant_extended_fns: FxHashSet<FnId> = FxHashSet::default();
    let unit_var_names: FxHashMap<VarId, &str> = if matches!(mode, MergeMode::Variant) {
        unit.variables
            .iter()
            .map(|v| (v.id, v.name.as_str()))
            .collect()
    } else {
        FxHashMap::default()
    };
    // Every parameter's type, remapped into the program's id space, indexed by
    // its unit-local `VarId`. Both places below used to scan `unit.variables`
    // for each parameter of each function, so one unit cost
    // O(functions x params x variables) -- and a lowered TU carries tens of
    // thousands of variables (#83). Only parameters are indexed, which is all
    // either lookup asks for: locals dominate that list, and `lower_parameter`
    // is the only thing that builds a variable a function's `params` can name.
    let unit_param_types: FxHashMap<VarId, TypeId> = unit
        .variables
        .iter()
        .filter(|v| matches!(v.storage, trace_ir::StorageClass::Param))
        .map(|v| (v.id, remap_type(v.type_id, &type_map)))
        .collect();

    for func in &unit.functions {
        let old_id = func.id;
        let span_file = map_file(func.span.file);
        let mut canonical = program
            .dedup
            .existing_fn(span_file, &func.name, func.span.line);
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
            if matches!(mode, MergeMode::Variant) {
                variant_extended_fns.insert(canonical);
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
        program
            .dedup
            .insert_fn(span_file, func.name.clone(), func.span.line, merged);
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
    for &fn_id in &variant_extended_fns {
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
    // How many temporaries at each position the incoming unit has consumed.
    let mut temp_cursor: FxHashMap<TempKey, usize> = FxHashMap::default();

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

        let extended_fn = mapped_fn_id.filter(|id| variant_extended_fns.contains(id));
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
            let cursor = temp_cursor.entry(*key).or_insert(0);
            if let Some(&existing) = temps_by_site.get(key).and_then(|ids| ids.get(*cursor)) {
                *cursor += 1;
                var_map.insert(var.id, existing);
                continue;
            }
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
            *temp_cursor.entry(key).or_insert(0) += 1;
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
            let is_param = var.param_index.is_some() && !variant_extended_fns.contains(&canon_fn);
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

    if matches!(mode, MergeMode::SymbolsOnly) {
        return;
    }

    let mut call_map: FxHashMap<CallSiteId, CallSiteId> = FxHashMap::default();
    for cs in &unit.call_sites {
        if dropped_fns.contains(&cs.caller) {
            continue;
        }
        let span_file = map_file(cs.span.file);
        if program.is_dep_file(span_file) {
            continue;
        }
        let key: SiteKey = (span_file, cs.span.line, cs.span.col, cs.callee_name.clone());
        if !matches!(mode, MergeMode::Variant) {
            if let Some(&existing) = program.dedup.site_keys.get(&key) {
                call_map.insert(cs.id, existing);
                continue;
            }
        }
        let old = cs.id;
        let mut site = cs.clone();
        site.caller = fn_map.get(&site.caller).copied().unwrap_or(site.caller);
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
        if matches!(mode, MergeMode::Variant) {
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
            let existing = program
                .dedup
                .site_keys
                .get(&key)
                .copied()
                .into_iter()
                .chain(
                    program
                        .dedup
                        .variant_site_records
                        .get(&key)
                        .into_iter()
                        .flatten()
                        .copied(),
                )
                .find(|&id| {
                    call_site_index(&program.symbols, id)
                        .is_some_and(|idx| same_call_facts(&program.symbols.call_sites[idx], &site))
                });
            if let Some(existing) = existing {
                call_map.insert(old, existing);
                continue;
            }
        }
        let new_id = program.symbols.alloc_call_id();
        site.id = new_id;
        program.symbols.call_sites.push(site);
        if matches!(mode, MergeMode::Variant) {
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
                .push(new_id);
            program.dedup.site_keys.entry(key).or_insert(new_id);
        } else {
            program.dedup.site_keys.insert(key, new_id);
        }
        call_map.insert(old, new_id);
    }

    if let Some(seen) = variant_dedup {
        for flow in &unit.flow {
            if !flow_vars(flow).all(|v| var_map.contains_key(&v)) {
                continue;
            }
            let remapped = remap_flow(flow, &fn_map, &var_map);
            if seen.flow.insert(remapped.clone()) {
                program.flow.push(remapped);
            }
        }
    } else {
        for flow in &unit.flow {
            if !flow_vars(flow).all(|v| var_map.contains_key(&v)) {
                continue;
            }
            program.flow.push(remap_flow(flow, &fn_map, &var_map));
        }
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
        let remapped: Vec<ReturnFlow> = flows
            .iter()
            .filter(|f| return_flow_vars(f).all(|v| var_map.contains_key(&v)))
            .map(|f| remap_return_flow(f, &fn_map, &var_map))
            .collect();
        let target = program.fn_returns.entry(new_fn).or_default();
        if matches!(mode, MergeMode::Variant) {
            // Only a variant re-lowers a body the program already holds, so
            // only a variant can repeat a return flow. Scanning for duplicates
            // on the base path would cost a linear search per return flow in
            // every unit, to reject something that cannot arise there.
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

fn flow_vars(flow: &FlowConstraint) -> impl Iterator<Item = VarId> + '_ {
    match flow {
        FlowConstraint::Copy { dst, src }
        | FlowConstraint::Load { dst, src }
        | FlowConstraint::Store { dst, src } => vec![*dst, *src],
        FlowConstraint::AddrOfVar { dst, src } => vec![*dst, *src],
        FlowConstraint::AddrOfFn { dst, .. } => vec![*dst],
        FlowConstraint::GepField { dst, base, .. } => vec![*dst, *base],
        FlowConstraint::ArrayFnMember { array, .. } => vec![*array],
        FlowConstraint::CallReturn { dst, .. } => vec![*dst],
        FlowConstraint::CallReturnIndirect { dst, callee_var } => vec![*dst, *callee_var],
        FlowConstraint::NewHeap { dst, .. } => vec![*dst],
        FlowConstraint::StringConst { dst, .. } => vec![*dst],
    }
    .into_iter()
}

fn return_flow_vars(flow: &ReturnFlow) -> impl Iterator<Item = VarId> + '_ {
    match flow {
        ReturnFlow::AddrOfVar { src } => vec![*src],
        ReturnFlow::Copy { src } => vec![*src],
        ReturnFlow::AddrOfFn { .. } | ReturnFlow::Call { .. } => Vec::new(),
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
) -> Vec<TypeId> {
    dst.merge_struct_declarations(src);
    let mut map = Vec::with_capacity(src.all().len());
    for info in src.all() {
        let new_id = match &info.desc {
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
            dst.register_alias(alias, desc.clone());
        }
    }
    map
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
        .map(|(_, fl)| (fl.name.clone(), src.get(fl.type_id).desc.clone()))
        .collect()
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
        FlowConstraint::CallReturn { dst, callee_name } => FlowConstraint::CallReturn {
            dst: rv(*dst),
            callee_name: callee_name.clone(),
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

        let map = merge_types(&mut dst, &src, false);

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
            TypeDesc::Ptr(Box::new(full_b)),
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
                is_virtual: false,
                is_final: false,
                is_cpp: true,
            }],
            variables: vec![Variable {
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
        // A different origin bypasses source-location deduplication and
        // exercises the symbol table's exact-signature comparison.
        let repeated = unit_declaring("other.cpp", true, Vec::new());
        merge_unit_index(&mut program, &repeated);
        assert_eq!(
            program.symbols.functions.len(),
            1,
            "a prototype must not change the signature used to deduplicate a definition"
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
