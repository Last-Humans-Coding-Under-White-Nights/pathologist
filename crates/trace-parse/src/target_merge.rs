//! Build independent symbol scopes from immutable, already-lowered units.
use super::*;
use crate::link_commands::LinkDatabase;
use trace_ir::{LinkTarget, SymbolTable, TargetId};

pub(crate) fn merge_linked_units(program: &mut Program, units: &[UnitIndex], links: &LinkDatabase) {
    let mut by_source: FxHashMap<&Path, Vec<usize>> = FxHashMap::default();
    for (i, unit) in units.iter().enumerate() {
        by_source.entry(&unit.path).or_default().push(i);
    }
    let mut assigned: FxHashSet<usize> = FxHashSet::default();
    for (index, spec) in links.targets.iter().enumerate() {
        let id = TargetId(index as u32);
        let mut visited = BTreeSet::new();
        let mut pending = vec![index];
        while let Some(i) = pending.pop() {
            if visited.insert(i) {
                pending.extend(links.targets[i].dependencies.iter().copied());
            }
        }
        let mut members: BTreeSet<usize> = BTreeSet::new();
        for i in visited {
            for source in &links.targets[i].sources {
                if let Some(indices) = by_source.get(source.as_path()) {
                    let configurations = links.targets[i].configurations.get(source);
                    members.extend(indices.iter().copied().filter(|&index| {
                        configurations.is_none_or(|allowed| {
                            units[index]
                                .compilation_index
                                .is_some_and(|c| allowed.contains(&c))
                        })
                    }));
                }
            }
        }
        assigned.extend(members.iter().copied());
        program.link_targets.push(LinkTarget {
            id,
            name: spec.name.clone(),
            output: spec.output.clone(),
            sources: spec
                .sources
                .iter()
                .map(|p| program.symbols.add_file_interned(p))
                .collect(),
            dependencies: spec
                .dependencies
                .iter()
                .map(|i| TargetId(*i as u32))
                .collect(),
        });
        // Deduplication is local to a link image, including header-origin
        // entities. Diagnostic keys are program-wide and deliberately survive.
        program.dedup.clear_entities();
        let scoped: Vec<_> = members.into_iter().map(|i| &units[i]).collect();
        merge_target(program, &scoped, id);
    }
    program.dedup.clear_entities();
    // The leftovers are one configuration family like any other: merged
    // individually they would repeat every constraint their configurations
    // share, which is exactly what `VariantDedup` exists to drop.
    let unassigned: Vec<&UnitIndex> = units
        .iter()
        .enumerate()
        .filter(|(i, _)| !assigned.contains(i))
        .map(|(_, unit)| unit)
        .collect();
    if let Some((base, variants)) = unassigned.split_first() {
        let mut merge = VariantMerge::start(program, base, !variants.is_empty());
        for unit in variants {
            merge.push(program, unit);
        }
    }
}

/// Whether a unit-local file lies under a dependency root. `Program` owns that
/// classification; re-deriving the prefix test here would be a second rule.
fn in_dep_root(program: &Program, unit: &UnitIndex, file: trace_ir::FileId) -> bool {
    unit.files
        .get(file.0 as usize)
        .is_some_and(|path| program.is_dep_path(path))
}

fn merge_target(program: &mut Program, units: &[&UnitIndex], target: TargetId) {
    // A namespaced global's unqualified name is not its link-time symbol name,
    // so it neither contributes strength nor can be overridden by it: a strong
    // `b::cb` must not silence a weak `a::cb`.
    let global = |v: &Variable| v.storage == trace_ir::StorageClass::Global && !v.is_namespaced;
    // Strength only matters where a weak global exists to be overridden, and
    // most targets have none. Checking that first keeps the common case off
    // the full variable scan, which is dominated by lowering temporaries.
    let strong_globals: FxHashSet<&str> = if units
        .iter()
        .any(|u| u.variables.iter().any(|v| global(v) && v.is_weak))
    {
        units
            .iter()
            .flat_map(|unit| unit.variables.iter().map(move |v| (*unit, v)))
            .filter(|(unit, v)| {
                global(v) && v.is_defined && !v.is_weak && !in_dep_root(program, unit, v.span.file)
            })
            .map(|(_, v)| v.name.as_str())
            .collect()
    } else {
        FxHashSet::default()
    };
    let suppressed = suppressed_weak_bodies(program, units, target);
    // Scoping rewrites a full copy of every unit it merges. Materializing the
    // whole family first would hold a second copy of everything this target
    // links; merging them one at a time keeps a single copy live.
    let mut merge: Option<VariantMerge> = None;
    for (unit, removed) in units.iter().zip(&suppressed) {
        let scoped = scope_unit(unit, target, removed, &strong_globals);
        match merge.as_mut() {
            Some(merge) => merge.push(program, &scoped),
            None => merge = Some(VariantMerge::start(program, &scoped, units.len() > 1)),
        }
    }
}

/// One unit as `target` sees it: every symbol bound to the target, and the
/// bodies, variables and constraints an override suppresses removed.
fn scope_unit(
    unit: &UnitIndex,
    target: TargetId,
    removed: &FxHashSet<FnId>,
    strong_globals: &FxHashSet<&str>,
) -> UnitIndex {
    let mut scoped = unit.clone();
    for f in &mut scoped.functions {
        f.target = Some(target);
        if removed.contains(&f.id) {
            f.is_defined = false;
            f.params.clear();
            f.locals.clear();
        }
    }
    scoped
        .variables
        .retain(|v| !v.fn_id.is_some_and(|id| removed.contains(&id)));
    let mut removed_globals = FxHashSet::default();
    for v in &mut scoped.variables {
        v.target = Some(target);
        if v.storage == trace_ir::StorageClass::Global
            && v.is_weak
            && !v.is_namespaced
            && strong_globals.contains(v.name.as_str())
        {
            removed_globals.insert(v.id);
            v.is_defined = false;
        }
    }
    scoped.call_sites.retain(|c| !removed.contains(&c.caller));
    scoped.fn_returns.retain(|id, _| !removed.contains(id));
    if !removed.is_empty() || !removed_globals.is_empty() {
        let mut excluded = vec![false; scoped.flow.len()];
        for id in removed {
            for range in unit.function_flow_ranges.get(id).into_iter().flatten() {
                excluded[range.clone()].fill(true);
            }
        }
        for id in &removed_globals {
            for range in unit.global_initializer_ranges.get(id).into_iter().flatten() {
                excluded[range.clone()].fill(true);
            }
        }
        let vars: FxHashSet<_> = scoped.variables.iter().map(|v| v.id).collect();
        scoped.flow = scoped
            .flow
            .into_iter()
            .enumerate()
            .filter_map(|(n, f)| {
                (!excluded[n] && flow_vars(&f).all(|v| vars.contains(&v))).then_some(f)
            })
            .collect();
    }
    // The ranges index `unit.flow`, which the filter above renumbers. Selection
    // reads them from `unit`, never from the copy, so the copy's would only be
    // a trap for a later reader — and dead weight in the meantime.
    scoped.function_flow_ranges = FxHashMap::default();
    scoped.global_initializer_ranges = FxHashMap::default();
    scoped
}

/// Per-unit merged type map plus its parameter types, for the units that offer
/// a weak-selection candidate.
type UnitTypeIndex = Option<(Vec<TypeId>, FxHashMap<VarId, TypeId>)>;

fn suppressed_weak_bodies(
    program: &mut Program,
    units: &[&UnitIndex],
    target: TargetId,
) -> Vec<FxHashSet<FnId>> {
    let mut suppressed = vec![FxHashSet::default(); units.len()];
    let weak_names: FxHashSet<_> = units
        .iter()
        .flat_map(|u| &u.functions)
        .filter(|f| f.is_defined && f.is_weak && f.linkage == trace_ir::Linkage::External)
        .map(|f| f.name.as_str())
        .collect();
    if weak_names.is_empty() {
        return suppressed;
    }
    // Registration is the single source of truth for C/C++ signature matching.
    // Only names that have weak definitions need this selection pass.
    //
    // Only the units that actually offer a candidate are indexed. The target
    // merge below merges every unit's types again, and type merging dominates
    // indexing cost, so building these for all of them would nearly double
    // that cost for any target holding a single weak definition.
    // `dep_roots` is read through `program` below, where `program.types` is
    // also borrowed mutably, so the membership test is resolved up front.
    let contributing: Vec<bool> = units
        .iter()
        .map(|unit| {
            unit.functions.iter().any(|f| {
                f.is_defined
                    && f.linkage == trace_ir::Linkage::External
                    && weak_names.contains(f.name.as_str())
                    && !in_dep_root(program, unit, f.file)
            })
        })
        .collect();
    let dep_files: Vec<FxHashSet<trace_ir::FileId>> = units
        .iter()
        .map(|unit| {
            (0..unit.files.len() as u32)
                .map(trace_ir::FileId)
                .filter(|&file| in_dep_root(program, unit, file))
                .collect()
        })
        .collect();
    // One entry per unit: the merged type map and its parameter types, or
    // `None` for a unit that offers no candidate and needs neither.
    let indexed: Vec<UnitTypeIndex> = units
        .iter()
        .zip(&contributing)
        .map(|(unit, &wanted)| {
            wanted.then(|| {
                let types = merge_types(
                    &mut program.types,
                    &unit.types,
                    false,
                    Some(unit.merge_descs.as_slice()).filter(|d| d.len() == unit.types.all().len()),
                );
                let params = unit
                    .variables
                    .iter()
                    .filter(|v| v.storage == trace_ir::StorageClass::Param)
                    .map(|v| (v.id, v.type_id))
                    .collect();
                (types, params)
            })
        })
        .collect();
    let mut selector = SymbolTable::default();
    let mut origins: FxHashMap<FnId, (&Path, bool)> = FxHashMap::default();
    for weak in [false, true] {
        for (i, unit) in units.iter().enumerate() {
            for f in unit.functions.iter().filter(|f| {
                f.is_defined
                    && f.is_weak == weak
                    && f.linkage == trace_ir::Linkage::External
                    && weak_names.contains(f.name.as_str())
            }) {
                if dep_files[i].contains(&f.file) {
                    continue;
                }
                let mut candidate = f.clone();
                candidate.id = selector.alloc_fn_id();
                candidate.target = Some(target);
                // Reached only for a contributing unit, so `indexed[i]` is set;
                // an unknown type stands in for a parameter with no recorded type.
                let unit_types = indexed[i].as_ref();
                let params: Vec<_> = f
                    .params
                    .iter()
                    .map(|p| {
                        unit_types
                            .and_then(|(types, by_id)| Some(remap_type(*by_id.get(p)?, types)))
                            .unwrap_or_else(|| program.types.unknown())
                    })
                    .collect();
                let allocated = candidate.id;
                let registration =
                    selector.register_function(candidate, Some(&params), Some(&program.types));
                let origin = unit
                    .files
                    .get(f.file.0 as usize)
                    .map(PathBuf::as_path)
                    .unwrap_or(&unit.path);
                if registration.id == allocated {
                    origins.insert(allocated, (origin, weak));
                } else if weak && origins.get(&registration.id) != Some(&(origin, true)) {
                    // Configurations of the same weak body still form a union.
                    // Only a strong body or a different weak origin overrides it.
                    suppressed[i].insert(f.id);
                }
            }
        }
    }
    suppressed
}
