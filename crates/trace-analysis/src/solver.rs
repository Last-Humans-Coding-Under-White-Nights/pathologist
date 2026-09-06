use crate::constraints::{
    ArgFlowEdge, CallGraphEdge, Constraint, ConstraintKind, LocKind, ResolutionKind,
};
use crate::pag::{Pag, PagNodeKind};
use crate::summaries::{Effect, FnModelSet};
use indexmap::{IndexMap, IndexSet};
use rustc_hash::{FxHashMap, FxHashSet};
use trace_ir::{CallSiteId, FnId, LocId, PagNodeId, Program, StorageClass, VarId};

/// Sentinel `CallSiteId` used for synthetic IPC bridge call edges. Real call
/// sites are allocated sequentially from `0`; this value marks an edge that
/// does not correspond to any single source-level call (the proxy method has
/// only the opaque `SendRequest` call site), so exporters/consumers can
/// distinguish it and must not join it to a real `CallSite`.
pub const SYNTHETIC_CALL_SITE: CallSiteId = CallSiteId(u32::MAX);

#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    /// Retain full points-to sets on the result (for `--debug-points-to` export).
    pub retain_points_to: bool,
    /// Function models matched by callee name (built-ins plus any user
    /// configuration). See `docs/ANALYSIS.md`, "Function models".
    pub models: std::sync::Arc<FnModelSet>,
    /// Solver pop budget (None = unlimited). Override via
    /// `TRACE_SOLVE_BUDGET_POPS=<n>` env var; =0 restores unlimited.
    pub solve_budget: Option<u64>,
    /// Emit synthetic IPC proxy→stub bridge edges (detected from class-name
    /// patterns). Disable to keep the call graph free of synthetic edges.
    pub enable_ipc: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            retain_points_to: false,
            models: std::sync::Arc::new(FnModelSet::builtin()),
            solve_budget: Some(800_000),
            enable_ipc: true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct AnalysisResult {
    pub points_to: IndexMap<PagNodeId, FxHashSet<LocId>>,
    pub call_edges: Vec<CallGraphEdge>,
    pub arg_flow_edges: Vec<ArgFlowEdge>,
    pub wired_arg_flow: FxHashSet<(CallSiteId, u32, FnId)>,
    /// Applied `clears` effects: `(call site, cleared parameter index)`.
    /// Exported as terminator nodes/edges in the flow graph.
    pub terminator_events: Vec<(CallSiteId, u32)>,
}

pub fn analyze(program: &Program) -> (Pag, AnalysisResult) {
    analyze_with_options(program, AnalyzeOptions::default())
}

pub fn analyze_with_options(program: &Program, opts: AnalyzeOptions) -> (Pag, AnalysisResult) {
    let mut pag = Pag::build_with_models_and_ipc(program, &opts.models, opts.enable_ipc);
    let mut result = solve(
        &mut pag,
        program,
        opts.retain_points_to,
        &opts.models,
        opts.solve_budget,
    );
    let call_edges = result.call_edges.clone();
    let wired = result.wired_arg_flow.clone();
    extract_arg_flow(program, &call_edges, &wired, &mut result);
    (pag, result)
}

struct SolverState {
    pts: FxHashMap<PagNodeId, FxHashSet<LocId>>,
    /// Locations added to a node's points-to since it was last processed.
    /// Difference propagation: only these flow onward on pop, which keeps
    /// total work proportional to facts discovered rather than
    /// `pops × |pts|` (the old full-set re-propagation was quadratic on hub
    /// nodes and dominated solve time on large trees).
    delta: FxHashMap<PagNodeId, Vec<LocId>>,
    /// Dedup guard so repeated events (e.g. memory writes under a location
    /// many nodes hold) append a given (node, loc) pair once per pending
    /// cycle instead of unboundedly inflating delta vectors.
    delta_pending: FxHashSet<(PagNodeId, LocId)>,
    memory_pts: FxHashMap<LocId, IndexSet<LocId>>,
    loc_nodes: FxHashMap<LocId, FxHashSet<PagNodeId>>,
    worklist: Vec<PagNodeId>,
    queued: FxHashSet<PagNodeId>,
    /// Nodes whose one-time, points-to-independent constraint effects
    /// (addr-of seeding, GEP summary fallback) have already been applied.
    seen_once: FxHashSet<PagNodeId>,
    /// Summary locations that hit `SUMMARY_MEM_CAP` and stopped growing.
    saturated_summaries: FxHashSet<LocId>,
    /// Dedup for dynamically added parameter-copy constraints: the same
    /// (actual → formal) pair recurs across many call sites and re-adding it
    /// per discovered edge explodes constraint volume on large trees.
    wired_copies: FxHashSet<(PagNodeId, PagNodeId)>,
    /// Same dedup, shared with parameter copies, for model-effect stores
    /// (`content_store`) and alias/mem-copy edges.
    wired_model_edges: FxHashSet<(PagNodeId, PagNodeId)>,
    /// Signature-aware propagation guards (see `SlotGuard`).
    slot_guard: FxHashMap<LocId, SlotGuard>,
    /// Parameter count per function location (only when > 0; old-style `()`
    /// declarations stay unfiltered).
    fn_arity: FxHashMap<LocId, usize>,
    /// Last-seen `memory_pts[loc]` size per `(dst, loc)` pair, used to skip
    /// redundant `merge_memory_into` iterations when memory hasn't grown.
    merge_sizes: FxHashMap<(PagNodeId, LocId), usize>,
}

/// Declared content type of a memory cell / points-to slot, as far as it is
/// known, used to keep incompatible function values out under wrong-type
/// pointer flow.
#[derive(Clone, Copy)]
enum SlotGuard {
    /// Slot is a `FnPtr` taking `n` parameters.
    FnParams(usize),
    /// Slot is a concrete non-fn-pointer type (e.g. a struct object);
    /// storing a bare function address there cannot occur in valid C.
    NotFnPtr,
}

impl SolverState {
    /// May a function value enter the slot `slot`? Typed fn-pointer slots
    /// accept only same-arity functions; concrete non-fn slots reject all;
    /// unknown slots accept everything (conservative over-approximation).
    #[inline]
    fn arity_allows(&self, slot: LocId, fn_loc: LocId) -> bool {
        match self.slot_guard.get(&slot) {
            Some(SlotGuard::FnParams(n)) => match self.fn_arity.get(&fn_loc) {
                Some(p) => p == n,
                None => true,
            },
            Some(SlotGuard::NotFnPtr) => false,
            None => true,
        }
    }
}

impl SolverState {
    fn push(&mut self, node: PagNodeId) {
        if self.queued.insert(node) {
            self.worklist.push(node);
        }
    }

    /// A memory write changed `memory_pts[loc]`: only nodes that LOAD from
    /// `loc` (i.e. are the pointer source of a Load constraint) benefit from
    /// re-merging.  Under difference propagation an empty delta would make
    /// them skip, so the location is appended to their delta explicitly
    /// (deduped).  Pushing ALL holders was the dominant budget burn on large
    /// trees — copy-only holders re-popped with no effect.
    fn touch_loc_holders(&mut self, loc: LocId, load_src: &FxHashMap<PagNodeId, Vec<usize>>) {
        if let Some(nodes) = self.loc_nodes.get(&loc).cloned() {
            for n in nodes {
                if !load_src.contains_key(&n) {
                    continue;
                }
                self.record_delta(n, &[loc]);
                self.push(n);
            }
        }
    }

    /// Record freshly inserted locations for difference propagation.
    fn record_delta(&mut self, node: PagNodeId, new_locs: &[LocId]) {
        for &loc in new_locs {
            if self.delta_pending.insert((node, loc)) {
                self.delta.entry(node).or_default().push(loc);
            }
        }
    }

    /// Skip `merge_memory_into` when `memory_pts[mem_loc]` hasn't grown
    /// since the last merge for this `(dst, mem_loc)` pair.  Breaks the
    /// touch_loc_holders → re-merge cycle that dominated budget on large
    /// trees.
    fn merge_memory_into_if_grown(&mut self, dst: PagNodeId, mem_loc: LocId) {
        let cur = self.memory_pts.get(&mem_loc).map(|m| m.len()).unwrap_or(0);
        let key = (dst, mem_loc);
        let prev = self.merge_sizes.get(&key).copied().unwrap_or(0);
        if cur > prev {
            self.merge_memory_into(dst, mem_loc);
            self.merge_sizes.insert(key, cur);
        }
    }

    /// Merge `memory_pts[mem_loc]` into `pts[dst]` without cloning.
    /// Iterates only entries added since the last merge for this pair
    /// (index-based since `memory_pts` uses `IndexSet`).
    fn merge_memory_into(&mut self, dst: PagNodeId, mem_loc: LocId) {
        let Some(mem) = self.memory_pts.get(&mem_loc) else {
            return;
        };
        let key = (dst, mem_loc);
        let prev_len = self.merge_sizes.get(&key).copied().unwrap_or(0);
        let cur_len = mem.len();
        if cur_len <= prev_len {
            return;
        }
        let new_locs: Vec<LocId> = (prev_len..cur_len)
            .filter_map(|i| {
                let loc = mem[i];
                if self.arity_allows(mem_loc, loc) {
                    Some(loc)
                } else {
                    None
                }
            })
            .collect();
        if new_locs.is_empty() {
            self.merge_sizes.insert(key, cur_len);
            return;
        }
        let mut truly_new: Vec<LocId> = Vec::new();
        {
            let entry = self.pts.entry(dst).or_default();
            for &loc in &new_locs {
                if !entry.contains(&loc) {
                    truly_new.push(loc);
                }
            }
        }
        if truly_new.is_empty() {
            self.merge_sizes.insert(key, cur_len);
            return;
        }
        {
            let entry = self.pts.get_mut(&dst).expect("pts entry exists");
            for loc in &truly_new {
                entry.insert(*loc);
            }
        }
        for loc in &truly_new {
            self.loc_nodes.entry(*loc).or_default().insert(dst);
        }
        self.record_delta(dst, &truly_new);
        self.push(dst);
        self.merge_sizes.insert(key, cur_len);
    }
}

/// A call site denotes a recoverable direct call when lowering recorded no
/// callee variable (`callee_var`) and the callee text is a plain identifier.
/// Cross-TU calls satisfy this: lowering marks them indirect only because the
/// definition was not visible in the translation unit.
fn direct_by_name(cs: &trace_ir::CallSite) -> bool {
    cs.callee_var.is_none() && !cs.callee_name.contains("->") && !cs.callee_name.contains('.')
}

fn st_pts_stats_max(pts: &IndexMap<PagNodeId, FxHashSet<LocId>>) -> usize {
    pts.values().map(|s| s.len()).max().unwrap_or(0)
}

/// Maximum distinct locations remembered per instance-insensitive summary
/// location. Past this, further stores are dropped (see saturation note in
/// `apply_store_to_targets`).
const SUMMARY_MEM_CAP: usize = 1024;

fn solve(
    pag: &mut Pag,
    program: &Program,
    retain_points_to: bool,
    models: &FnModelSet,
    budget_override: Option<u64>,
) -> AnalysisResult {
    let mut st = SolverState {
        pts: FxHashMap::default(),
        delta: FxHashMap::default(),
        delta_pending: FxHashSet::default(),
        memory_pts: FxHashMap::default(),
        loc_nodes: FxHashMap::default(),
        worklist: Vec::new(),
        queued: FxHashSet::default(),
        seen_once: FxHashSet::default(),
        saturated_summaries: FxHashSet::default(),
        wired_copies: FxHashSet::default(),
        wired_model_edges: FxHashSet::default(),
        slot_guard: FxHashMap::default(),
        fn_arity: FxHashMap::default(),
        merge_sizes: FxHashMap::default(),
    };
    // Per-location slot guards and per-function parameter counts for
    // signature-aware propagation.
    use trace_ir::TypeDesc as TD;
    for loc in &pag.locations {
        let g = match &program.types.get(loc.type_id).desc {
            TD::FnPtr { params, .. } if !params.is_empty() => SlotGuard::FnParams(params.len()),
            TD::Void
            | TD::Char
            | TD::Bool
            | TD::Short
            | TD::Int
            | TD::Long
            | TD::LongLong
            | TD::Float
            | TD::Double
            | TD::SizeT
            | TD::Unknown => continue,
            TD::Struct { .. } | TD::Union { .. } | TD::Ptr(_) | TD::Array { .. } => {
                SlotGuard::NotFnPtr
            }
            TD::FnPtr { .. } => continue,
        };
        st.slot_guard.insert(loc.id, g);
    }
    for (&fn_id, &loc) in &pag.fn_locations {
        let f = program.symbols.function(fn_id);
        if !f.params.is_empty() {
            st.fn_arity.insert(loc, f.params.len());
        }
    }

    for c in &pag.constraints {
        st.push(c.dst);
        st.push(c.src);
    }

    // Pre-seed addr-of constraints: these are purely structural and don't
    // depend on points-to content. Seeding them upfront avoids a wasteful
    // two-pop cycle per variable (first pop for seen_once, second for
    // actual propagation) and dramatically improves worklist drain on
    // large PAGs where LIFO ordering buries early-pushed nodes.
    for c in pag.constraints.iter() {
        if matches!(c.kind, ConstraintKind::AddrOf) {
            if let PagNodeKind::Loc(loc) = pag.nodes[c.src.0 as usize].kind {
                let inserted = {
                    let entry = st.pts.entry(c.dst).or_default();
                    entry.insert(loc)
                };
                if inserted {
                    st.loc_nodes.entry(loc).or_default().insert(c.dst);
                    st.record_delta(c.dst, &[loc]);
                    st.push(c.dst);
                }
            }
        }
    }

    // Mark all nodes that received pre-seeded addr-of points as
    // already processed for addr-of so the solver's seen_once path
    // doesn't duplicate the seeding.
    for c in &pag.constraints {
        if matches!(c.kind, ConstraintKind::AddrOf) {
            st.seen_once.insert(c.dst);
        }
    }

    let mut call_edges: Vec<CallGraphEdge> = Vec::new();
    let mut resolved_indirect: FxHashMap<CallSiteId, Vec<FnId>> = FxHashMap::default();
    let mut wired_arg_flow: FxHashSet<(CallSiteId, u32, FnId)> = FxHashSet::default();
    let mut terminator_events: Vec<(CallSiteId, u32)> = Vec::new();

    for var in &program.symbols.variables {
        if !matches!(
            var.storage,
            StorageClass::Global | StorageClass::FileStatic | StorageClass::FnStatic
        ) {
            continue;
        }
        if let Some(&loc) = pag.var_location.get(&var.id) {
            let node = pag.var_node[&var.id];
            add_pts(&mut st, node, loc);
        }
    }

    for cs in &program.symbols.call_sites {
        // Direct sites: lowering saw the TU-local binding, so scope-first
        // resolution is exact per C visibility rules (file-`static` shadows
        // same-name externals inside its own TU).
        //
        // Recovered cross-TU sites: no local binding existed at lowering, and
        // merge erases which TU's view a name reflects — a name matching both
        // a `static` def and an external def is genuinely ambiguous. May-
        // approximation: consider every candidate.
        // Synthesized externals carry their FnId on the call site; other
        // sites resolve by name as before. Any callee without a definition
        // under the analyzed root (prototype-only or synthesized) yields an
        // External edge and no param wiring — there is no body to wire into.
        let callees: Vec<FnId> = if let Some(fid) = cs.callee_fn_id {
            vec![fid]
        } else if cs.is_direct {
            program
                .symbols
                .resolve_function_in_scope(&cs.callee_name, Some(cs.span.file))
                .into_iter()
                .collect()
        } else if direct_by_name(cs) {
            program
                .symbols
                .resolve_function_candidates(&cs.callee_name, Some(cs.span.file))
        } else {
            Vec::new()
        };
        for callee in callees {
            let f = program.symbols.function(callee);
            let external = !f.is_defined;
            let has_formals = !f.params.is_empty();
            call_edges.push(CallGraphEdge {
                call_site: cs.id,
                caller: cs.caller,
                callee,
                resolution: if external {
                    ResolutionKind::External
                } else {
                    ResolutionKind::Direct
                },
            });
            // Wire argument flow whenever the callee declares formals —
            // prototype-only targets still carry parameter information.
            // Synthesized externals have none, so this skips them for free.
            if has_formals {
                wire_params(pag, program, cs, callee, &mut st, &mut wired_arg_flow);
            }
            apply_fn_model(pag, &mut st, cs, &f.name, models, &mut terminator_events);
        }
    }

    let t0 = std::time::Instant::now();
    let mut pops: u64 = 0;
    let mut w_copy: u64 = 0;
    let mut w_load: u64 = 0;
    let mut w_store: u64 = 0;
    let mut w_gep: u64 = 0;
    // Work budget: on huge corpora the dynamic param-copy wiring can make
    // solving diverge in practice (points-to sets keep growing for hours).
    // A deterministic pop cap converts a hang into a partial result plus a
    // visible warning; normal corpora converge far below it. Override with
    // TRACE_SOLVE_BUDGET_POPS=<n>; =0 restores unlimited solving.
    let solve_budget: Option<u64> = match std::env::var("TRACE_SOLVE_BUDGET_POPS") {
        Ok(v) if v.trim() == "0" => None,
        Ok(v) => v.parse::<u64>().ok().or(budget_override),
        Err(_) => budget_override,
    };

    if std::env::var("TRACE_SOLVER_STATS").is_ok() {
        eprintln!(
            "[solver] START constraints={} vars={}",
            pag.constraints.len(),
            program.symbols.variables.len()
        );
    }
    let stats_enabled = std::env::var("TRACE_SOLVER_STATS").is_ok();

    while let Some(node) = st.worklist.pop() {
        st.queued.remove(&node);
        pops += 1;
        if let Some(budget) = solve_budget {
            if pops > budget {
                eprintln!(
                    "[solver] pop budget {} exhausted after {} pops / {:?}; stopping, results are partial \
                     (set TRACE_SOLVE_BUDGET_POPS to raise, or 0 for unlimited)",
                    budget,
                    pops,
                    t0.elapsed()
                );
                break;
            }
        }
        if stats_enabled && pops.is_multiple_of(100_000) {
            let biggest = st.pts.values().map(|s| s.len()).max().unwrap_or(0);
            eprintln!(
                "[solver] pops={} elapsed={:?} constraints={} queued={} max_pts={} total_pts={} locs={} copy={} load={} store={} gep={}",
                pops,
                t0.elapsed(),
                pag.constraints.len(),
                st.worklist.len(),
                biggest,
                st.pts.values().map(|s| s.len()).sum::<usize>(),
                pag.locations.len(),
                w_copy,
                w_load,
                w_store,
                w_gep
            );
        }

        // Difference propagation: process only locations added since the
        // node was last popped. Hub nodes pop thousands of times as they grow;
        // re-scanning the full set each time made solve time quadratic.
        let delta = std::mem::take(st.delta.get_mut(&node).unwrap_or(&mut Vec::new()));
        for &loc in delta.iter() {
            st.delta_pending.remove(&(node, loc));
        }
        if delta.is_empty() {
            // First touch with no pointees: apply constraints whose effect
            // does not depend on points-to content (addr-of seeding and the
            // instance-insensitive GEP field-summary fallback). Once.
            if st.seen_once.insert(node) {
                if let Some(idxs) = pag.indices.addr_of_dst.get(&node) {
                    for &idx in idxs {
                        let c = &pag.constraints[idx];
                        if let PagNodeKind::Loc(loc) = pag.nodes[c.src.0 as usize].kind {
                            add_pts(&mut st, node, loc);
                        }
                    }
                }
                if let Some(idxs) = pag.indices.gep_src.get(&node).cloned() {
                    for idx in idxs {
                        let (dst, src, field) = {
                            let c = &pag.constraints[idx];
                            (c.dst, c.src, c.field)
                        };
                        let Some(field) = field else {
                            continue;
                        };
                        if let PagNodeKind::Var(base_var) = pag.nodes[src.0 as usize].kind {
                            if let Some(summary) =
                                pag.ensure_field_summary_for_var(program, base_var, field)
                            {
                                propagate_locs(&mut st, dst, [summary]);
                                st.merge_memory_into_if_grown(dst, summary);
                            }
                        }
                    }
                }
            }
            continue;
        }

        if let Some(idxs) = pag.indices.copy_src.get(&node) {
            for &idx in idxs {
                let dst = pag.constraints[idx].dst;
                w_copy += delta.len() as u64;
                propagate_slice(&mut st, dst, &delta);
            }
        }

        if let Some(idxs) = pag.indices.addr_of_dst.get(&node) {
            for &idx in idxs {
                let c = &pag.constraints[idx];
                if let PagNodeKind::Loc(loc) = pag.nodes[c.src.0 as usize].kind {
                    add_pts(&mut st, node, loc);
                }
            }
        }

        if let Some(idxs) = pag.indices.load_src.get(&node) {
            for &idx in idxs {
                let dst = pag.constraints[idx].dst;
                for &loc in delta.iter() {
                    if fn_for_loc(pag, loc).is_some() {
                        add_pts(&mut st, dst, loc);
                    } else {
                        w_load += 1;
                        st.merge_memory_into_if_grown(dst, loc);
                    }
                }
            }
        }

        if !delta.is_empty() {
            if let Some(idxs) = pag.indices.store_dst.get(&node).cloned() {
                for idx in idxs {
                    w_store += delta.len() as u64;
                    apply_store_to_targets(pag, idx, &mut st, Some(delta.as_slice()));
                }
            }

            if let Some(idxs) = pag.indices.store_src.get(&node).cloned() {
                for idx in idxs {
                    w_store += 1;
                    apply_store_to_targets(pag, idx, &mut st, None);
                }
            }
        }

        if let Some(idxs) = pag.indices.gep_src.get(&node) {
            let idxs = idxs.clone();
            w_gep += delta.len() as u64 * idxs.len() as u64;
            'gep: for idx in idxs {
                let (dst, src, field, ref expected_name) = {
                    let c = &pag.constraints[idx];
                    (c.dst, c.src, c.field, c.field_name.clone())
                };
                let Some(field) = field else {
                    continue;
                };
                // Did any pointee yield a usable field cell? Untyped bases
                // (e.g. `void *` heap allocations) synthesize nothing, but
                // the access still needs the type-keyed summary to see
                // stores from other instances of the same struct type.
                let mut produced_cell = false;
                for &loc in delta.iter() {
                    // Cross-struct FieldId guard: if the GEP carries a
                    // field name from lowering, reject pointees whose
                    // struct type has a different field at the same
                    // positional index — this prevents functions from
                    // unrelated structs leaking as indirect-call targets.
                    if let Some(ref expected) = expected_name {
                        if let Some(parent_type) =
                            crate::pag::struct_type_for_loc(pag, program, loc)
                        {
                            match program.types.get(parent_type).layout.fields.get(&field) {
                                Some(fl) if fl.name == *expected => {}
                                _ => continue,
                            }
                        }
                    }
                    // Function values reach a base node's points-to either as
                    // `ArrayFnMember` table initializers (flow through element
                    // accesses unchanged) or via opaque/wrong-type parameter
                    // flow. Table members always pass; other fn values pass
                    // only when their declared arity matches the field slot's
                    // signature — otherwise a stray callback rides every
                    // field access of whatever pointer it polluted.
                    if let Some(fn_id) = fn_for_loc(pag, loc) {
                        let mut passed = false;
                        if let PagNodeKind::Var(base_var) = pag.nodes[src.0 as usize].kind {
                            if let Some(fn_locs) = pag.array_fn_members.get(&base_var) {
                                for fl in fn_locs.iter().copied() {
                                    add_pts(&mut st, dst, fl);
                                }
                                passed = true;
                            }
                        }
                        // Non-table function values ride a field access only
                        // when the destination slot's declared signature
                        // matches; anything else is wrong-type flow that must
                        // not surface as an indirect-call target.
                        if !passed {
                            let n = program.symbols.function(fn_id).params.len();
                            if n > 0 && pag.field_slot_arity(program, loc, field) == Some(n) {
                                add_pts(&mut st, dst, loc);
                            }
                        }
                        continue;
                    }
                    if let Some(field_loc) = pag.ensure_field_loc(program, loc, field) {
                        produced_cell = true;
                        // Field loc plus its instance-insensitive summary:
                        // the GEP result points AT these cells.
                        let targets = [Some(field_loc), pag.summary_for_field_loc(field_loc)];
                        propagate_locs(
                            &mut st,
                            dst,
                            targets.iter().filter_map(|t| t.as_ref().copied()),
                        );
                        // Cell contents reach the address node so that uses of
                        // the field lvalue (`&obj.f` passed onward, then
                        // loaded) still observe stores that lowering recorded
                        // against the cell without an intervening load temp.
                        for fl in targets.into_iter().flatten() {
                            st.merge_memory_into_if_grown(dst, fl);
                        }
                        // ArrayFnMember element fns: reachable through
                        // the array itself or any pointer to an element.
                        if let Some(owner) = pag.locations[loc.0 as usize].var {
                            if let Some(fn_locs) = pag.array_fn_members.get(&owner) {
                                for fl in fn_locs.iter().copied() {
                                    add_pts(&mut st, dst, fl);
                                }
                            }
                        }
                    }
                }
                // First-time GEP on a var with no pointees yet, or a GEP
                // whose pointees all failed to synthesize a field cell
                // (untyped `void *` heap, opaque summaries): fall back to
                // the instance-insensitive field summary so accesses
                // through this destination still observe stores made via
                // other instances of the same struct type. Without this,
                // ops tables assigned through freshly-allocated objects
                // starve every load site that reads them.
                let base_unpointed = st.pts.get(&node).map(|p| p.is_empty()).unwrap_or(true);
                if base_unpointed || !produced_cell {
                    if let PagNodeKind::Var(base_var) = pag.nodes[src.0 as usize].kind {
                        if let Some(summary) =
                            pag.ensure_field_summary_for_var(program, base_var, field)
                        {
                            propagate_locs(&mut st, dst, [summary]);
                            st.merge_memory_into_if_grown(dst, summary);
                        }
                    }
                    continue 'gep;
                }
            }
        }

        if let Some(idxs) = pag.indices.dlsym_src.get(&node) {
            for &idx in idxs {
                let dst = pag.constraints[idx].dst;
                for &loc in delta.iter() {
                    let abstract_loc = &pag.locations[loc.0 as usize];
                    if abstract_loc.kind != LocKind::StringLit {
                        continue;
                    }
                    let name = abstract_loc.desc.clone();
                    for func in &program.symbols.functions {
                        if func.name == name {
                            if let Some(&fn_loc) = pag.fn_locations.get(&func.id) {
                                add_pts(&mut st, dst, fn_loc);
                            }
                        }
                    }
                }
            }
        }

        if let Some(call_sites) = pag.indices.indirect_by_target.get(&node).cloned() {
            for cs_id in call_sites {
                let cs = program
                    .symbols
                    .call_sites
                    .get(cs_id.0 as usize)
                    .filter(|c| c.id == cs_id)
                    .expect("call site id in index");
                let mut new_callees = Vec::new();
                for &loc in delta.iter() {
                    if let Some(fn_id) = fn_for_loc(pag, loc) {
                        // If the resolved callee is undefined (e.g. a weak
                        // forward declaration), also pull in defined
                        // candidates with the same name so return flows
                        // and param wiring reach the real body.
                        if !program.symbols.function(fn_id).is_defined {
                            let name = program.symbols.function(fn_id).name.clone();
                            let file = Some(program.symbols.function(fn_id).file);
                            let extra: Vec<FnId> = program
                                .symbols
                                .resolve_function_candidates(&name, file)
                                .into_iter()
                                .filter(|c| program.symbols.function(*c).is_defined)
                                .collect();
                            if !extra.is_empty() {
                                new_callees.extend(extra);
                            }
                        }
                        new_callees.push(fn_id);
                    }
                }
                let prev = resolved_indirect.entry(cs_id).or_default();
                for callee in new_callees {
                    if !prev.contains(&callee) {
                        prev.push(callee);
                        call_edges.push(CallGraphEdge {
                            call_site: cs.id,
                            caller: cs.caller,
                            callee,
                            resolution: ResolutionKind::Indirect,
                        });
                        wire_params(pag, program, cs, callee, &mut st, &mut wired_arg_flow);
                        // Expand return flows from the callee into the
                        // `CallReturnIndirect` destination so the return
                        // value reaches the assignment LHS (e.g.
                        // `sbuf->impl = constructor->obtain(capacity)`).
                        if let Some(callee_var) = cs.callee_var {
                            if let Some(&dst_n) = pag.indirect_return_dst.get(&callee_var) {
                                let constraint_before = pag.constraints.len();
                                let mut visited = FxHashSet::default();
                                pag.expand_return_flows(
                                    program,
                                    dst_n,
                                    callee,
                                    models,
                                    &mut visited,
                                );
                                // Index any new constraints added by expand_return_flows
                                // and push their sources onto the worklist so the solver
                                // processes them.
                                if pag.constraints.len() > constraint_before {
                                    let new_srcs = pag.index_new_constraints(constraint_before);
                                    for src in new_srcs {
                                        st.push(src);
                                    }
                                }
                            }
                        }
                        apply_fn_model(
                            pag,
                            &mut st,
                            cs,
                            &program.symbols.function(callee).name,
                            models,
                            &mut terminator_events,
                        );
                    }
                }
            }
        }
    }

    // Emit synthetic call edges for IPC proxy→stub bridges detected at PAG
    // build. These connect the proxy method to the stub handler it would
    // dispatch to across the (opaque) Binder boundary. The resolution is
    // marked `IpcBridge` (distinct from a source-level direct call). Edges
    // may target external interface methods when the stub delegates to an
    // inherited pure-virtual; these are still emitted for call graph
    // completeness.
    for bridge in &pag.ipc_bridges {
        let callee = bridge.stub_handler;
        call_edges.push(CallGraphEdge {
            call_site: SYNTHETIC_CALL_SITE,
            caller: bridge.proxy_method,
            callee,
            resolution: ResolutionKind::IpcBridge,
        });
    }
    if std::env::var("TRACE_DEBUG_IPC").is_ok() {
        for bridge in &pag.ipc_bridges {
            let caller = program.symbols.function(bridge.proxy_method).name.clone();
            let callee = program.symbols.function(bridge.stub_handler).name.clone();
            eprintln!("[ipc] bridge: {caller}  -->  {callee}");
        }
        eprintln!("[ipc] total bridges: {}", pag.ipc_bridges.len());
    }

    let points_to = if retain_points_to {
        st.pts.into_iter().collect()
    } else {
        IndexMap::new()
    };

    if std::env::var("TRACE_SOLVER_STATS").is_ok() {
        let biggest = st_pts_stats_max(&points_to);
        eprintln!(
            "[solver] DONE pops={} elapsed={:?} constraints={} max_pts={} resolved_sites={}",
            pops,
            t0.elapsed(),
            pag.constraints.len(),
            biggest,
            resolved_indirect.len()
        );
    }

    AnalysisResult {
        points_to,
        call_edges,
        arg_flow_edges: Vec::new(),
        wired_arg_flow,
        terminator_events,
    }
}

/// Copy side for model `alias` / `mem_copy` effects. An address-of actual
/// (`memcpy_s(&dst, .., &src, ..)`) is rewritten to the underlying **object
/// variable**, so later field accesses on the destination object observe the
/// source-side field cells (the address temp itself is never used for
/// loads). Pointer-typed actuals (`memcpy(d, s, n)`) stay at their own node.
fn model_copy_side(pag: &Pag, node: PagNodeId) -> PagNodeId {
    if let Some(idxs) = pag.indices.addr_of_dst.get(&node) {
        for &idx in idxs {
            let c = &pag.constraints[idx];
            if let PagNodeKind::Loc(loc) = pag.nodes[c.src.0 as usize].kind {
                if let Some(v) = pag.locations[loc.0 as usize].var {
                    if let Some(&var_node) = pag.var_node.get(&v) {
                        return var_node;
                    }
                }
            }
        }
    }
    node
}

/// Apply a callee's function model at one resolved call site: attach
/// persistent PAG constraints between the actual-argument nodes so data
/// flows through bodyless callees, and record terminator events.
/// `ReturnAlias` / `ReturnHeap` are handled at PAG build time (they target
/// the `CallReturn` destination, not call arguments).
fn apply_fn_model(
    pag: &mut Pag,
    st: &mut SolverState,
    cs: &trace_ir::CallSite,
    callee_name: &str,
    models: &FnModelSet,
    terminator_events: &mut Vec<(CallSiteId, u32)>,
) {
    let Some(model) = models.get(callee_name) else {
        return;
    };
    // `&base.member` arguments resolve to the base variable; copying the
    // whole container would pollute unrelated fields with the source's
    // pointees, so alias-style effects refuse to fire on them.
    let member_addr = |idx: u32| cs.addr_of_member_args.binary_search(&idx).is_ok();
    // Actual argument node for parameter slot `idx`, when the call passed an
    // IR variable there (literals like `0` or `sizeof(..)` do not
    // participate).
    let mut arg_node_cache: FxHashMap<u32, Option<PagNodeId>> = FxHashMap::default();
    let mut arg_node = |pag: &Pag, idx: u32| -> Option<PagNodeId> {
        *arg_node_cache.entry(idx).or_insert_with(|| {
            let v = cs.var_args.iter().find(|(j, _)| *j == idx)?.1;
            pag.var_node.get(&v).copied()
        })
    };
    for effect in &model.effects {
        match effect {
            // Alias: `pts(param[dst]) ⊇ pts(param[src])` — whole-pointer
            // alias.  Skip member-address arguments to avoid polluting
            // unrelated fields of the base container.
            Effect::Alias { dst, src } => {
                if member_addr(*dst) || member_addr(*src) {
                    continue;
                }
                let d_side = arg_node(pag, *dst).map(|n| model_copy_side(pag, n));
                let s_side = arg_node(pag, *src).map(|n| model_copy_side(pag, n));
                if let (Some(d), Some(s)) = (d_side, s_side) {
                    if st.wired_model_edges.insert((s, d)) {
                        ensure_param_copy(pag, st, s, d);
                    }
                }
            }
            // MemCopy: bulk content copy `*dst <- *src` (memcpy family).
            // Unlike pointer Alias, the copy targets a specific sub-object
            // (e.g. `memcpy_s(&dst->chipData, ..., src, ...)`), so the
            // whole-object Copy to the base variable is sound for may-
            // analysis: the GEP chain already models the field access, and
            // extra pointees on unrelated fields are over-approximated.
            Effect::MemCopy { dst, src } => {
                let d_side = arg_node(pag, *dst).map(|n| model_copy_side(pag, n));
                let s_side = arg_node(pag, *src).map(|n| model_copy_side(pag, n));
                if let (Some(d), Some(s)) = (d_side, s_side) {
                    if st.wired_model_edges.insert((s, d)) {
                        ensure_param_copy(pag, st, s, d);
                    }
                }
            }
            Effect::ContentStore { ptr, value } => {
                if let (Some(p), Some(v)) = (arg_node(pag, *ptr), arg_node(pag, *value)) {
                    if st.wired_model_edges.insert((v, p)) {
                        let idx = pag.constraints.len();
                        pag.constraints.push(Constraint {
                            kind: ConstraintKind::Store,
                            dst: p,
                            src: v,
                            field: None,
                            field_name: None,
                        });
                        pag.indices.store_dst.entry(p).or_default().push(idx);
                        pag.indices.store_src.entry(v).or_default().push(idx);
                        // Either side gaining pointees must re-fire the store;
                        // evaluate both immediately with current knowledge.
                        st.push(p);
                        st.push(v);
                    }
                }
            }
            Effect::Clears { param } => {
                if arg_node(pag, *param).is_some() {
                    let event = (cs.id, *param);
                    if !terminator_events.contains(&event) {
                        terminator_events.push(event);
                    }
                }
            }
            Effect::ReturnAlias { .. } | Effect::ReturnHeap | Effect::Dlsym { .. } => {}
        }
    }
}

fn propagate_locs(st: &mut SolverState, dst: PagNodeId, locs: impl IntoIterator<Item = LocId>) {
    let mut new_locs: Vec<LocId> = Vec::new();
    {
        let entry = st.pts.entry(dst).or_default();
        for loc in locs {
            if !entry.contains(&loc) {
                new_locs.push(loc);
            }
        }
    }
    if new_locs.is_empty() {
        return;
    }
    {
        let entry = st.pts.get_mut(&dst).expect("entry just created");
        for loc in &new_locs {
            entry.insert(*loc);
            st.loc_nodes.entry(*loc).or_default().insert(dst);
        }
    }
    st.record_delta(dst, &new_locs);
    st.push(dst);
}

fn propagate_slice(st: &mut SolverState, dst: PagNodeId, src: &[LocId]) {
    let mut new_locs: Vec<LocId> = Vec::new();
    {
        let entry = st.pts.entry(dst).or_default();
        for &loc in src {
            if !entry.contains(&loc) {
                new_locs.push(loc);
            }
        }
    }
    if new_locs.is_empty() {
        return;
    }
    {
        let entry = st.pts.get_mut(&dst).expect("entry just created");
        for loc in &new_locs {
            entry.insert(*loc);
        }
    }
    for loc in &new_locs {
        st.loc_nodes.entry(*loc).or_default().insert(dst);
    }
    st.record_delta(dst, &new_locs);
    st.push(dst);
}

/// Store `*ptr = value`: write the value side's current points-to (plus its
/// own storage location for var nodes) into the memories of the given target
/// locations. `targets == None` means every location currently in the pointer
/// node's set; `Some(delta)` restricts writes to newly gained targets
/// (difference propagation — their memory is written for the first time).
fn apply_store_to_targets(pag: &Pag, idx: usize, st: &mut SolverState, targets: Option<&[LocId]>) {
    let c = &pag.constraints[idx];
    // Clone-free store: `pts` and `memory_pts` are disjoint fields, so the
    // destination set can be iterated by reference while memory is mutated.
    let src_set = st.pts.get(&c.src);
    let self_loc = match pag.nodes[c.src.0 as usize].kind {
        PagNodeKind::Var(v) => pag.var_location.get(&v).copied(),
        _ => None,
    };
    if src_set.map(|s| s.is_empty()).unwrap_or(true) && self_loc.is_none() {
        return;
    }
    let owned_targets: Vec<LocId>;
    let target_iter: &[LocId] = match targets {
        // `ts` borrows the caller-owned delta vector, disjoint from `st`:
        // iterate it directly, no copy needed.
        Some(ts) => ts,
        None => {
            owned_targets = st
                .pts
                .get(&c.dst)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            &owned_targets
        }
    };
    let mut requeues: Vec<LocId> = Vec::new();
    for &loc in target_iter.iter() {
        if fn_for_loc(pag, loc).is_some() {
            continue;
        }

        // Signature guard: never plant a function value into a cell whose
        // declared type is an incompatible fn pointer (or a concrete
        // non-fn-pointer object). Wrong-type casts put unrelated objects into
        // a pointer's points-to; without this guard a store through such a
        // pointer writes callbacks into alien layouts, where later field
        // loads surface them as bogus indirect-call targets. Untyped cells
        // (`void *`, unknown layouts) stay writable — conservative.
        let mut changed = false;
        // Signature-guarded view of the source set (see comment above).
        let filtered_src: Vec<LocId> = match src_set {
            Some(s) => s
                .iter()
                .copied()
                .filter(|&l| fn_for_loc(pag, l).is_none() || st.arity_allows(loc, l))
                .collect(),
            None => Vec::new(),
        };
        let filtered_ref: Option<&[LocId]> = src_set.map(|_| filtered_src.as_slice());
        {
            let entry = st.memory_pts.entry(loc).or_default();
            let before = entry.len();
            if let Some(s) = filtered_ref {
                for &l in s.iter() {
                    entry.insert(l);
                }
            }
            if let Some(sl) = self_loc {
                entry.insert(sl);
            }
            changed |= entry.len() > before;
        }
        let mut summary_loc = None;
        if let Some(summary) = pag.summary_for_field_loc(loc) {
            summary_loc = Some(summary);
            let summary_entry = st.memory_pts.entry(summary).or_default();
            let before_summary = summary_entry.len();
            if summary_entry.len() < SUMMARY_MEM_CAP {
                let accepts_fns = matches!(
                    st.slot_guard.get(&summary),
                    None | Some(SlotGuard::FnParams(_))
                );
                // Same signature guard for the instance-insensitive summary
                // cell (its declared type mirrors the field's).
                if let Some(s) = src_set {
                    for &l in s.iter() {
                        if !accepts_fns && fn_for_loc(pag, l).is_some() {
                            continue;
                        }
                        summary_entry.insert(l);
                    }
                }
                if let Some(sl) = self_loc {
                    summary_entry.insert(sl);
                }
            } else if !st.saturated_summaries.contains(&summary) {
                st.saturated_summaries.insert(summary);
            }
            changed |= summary_entry.len() > before_summary;
        }
        if changed {
            requeues.push(loc);
            if let Some(summary) = summary_loc {
                requeues.push(summary);
            }
        }
    }
    for loc in requeues {
        st.touch_loc_holders(loc, &pag.indices.load_src);
    }
}

/// Add a persistent `Copy { dst: formal, src: actual }` constraint and wire it
/// into the solver adjacency index so later growth of `pts(actual)` still
/// reaches the formal during this solve.
fn ensure_param_copy(
    pag: &mut Pag,
    st: &mut SolverState,
    actual_node: PagNodeId,
    formal_node: PagNodeId,
) {
    let constraint_idx = pag.constraints.len();
    pag.constraints.push(crate::constraints::Constraint {
        kind: crate::constraints::ConstraintKind::Copy,
        dst: formal_node,
        src: actual_node,
        field: None,
        field_name: None,
    });
    pag.indices
        .copy_src
        .entry(actual_node)
        .or_default()
        .push(constraint_idx);
    // Only enqueue when the actual already carries pointees; future growth
    // re-fires this copy through the `copy_src` index automatically.
    if st.pts.get(&actual_node).is_some_and(|p| !p.is_empty()) {
        st.push(actual_node);
    }
}

/// Only parameters whose pointees can influence call-target resolution get
/// persistent interprocedural copy constraints: function pointers directly,
/// opaque/unknown pointees that may hide them, and aggregates (op tables,
/// entry structures) whose fields carry callbacks. Opaque *buffer* pointers
/// (`char *`, `int *`, sized-value pointers) are excluded: their flow is
/// over-approximated by FieldSummary fallbacks and wiring them eagerly makes
/// solve time explode on large trees.
fn var_may_hold_pointee(program: &Program, var: VarId) -> bool {
    use trace_ir::TypeDesc as TD;
    let Some(v) = program.symbols.variable_by_id(var) else {
        return false;
    };
    let desc = &program.types.get(v.type_id).desc;
    match desc {
        TD::FnPtr { .. } => true,
        TD::Ptr(inner) => matches!(
            inner.as_ref(),
            TD::FnPtr { .. } | TD::Unknown | TD::Struct { .. } | TD::Union { .. }
        ),
        // Pointer-flagged variable whose recorded shape degraded to a scalar
        // (e.g. synthesized load temps typed `int`): participate
        // conservatively.
        _ if v.is_pointer => true,
        _ => false,
    }
}

fn wire_params(
    pag: &mut Pag,
    program: &Program,
    cs: &trace_ir::CallSite,
    callee: FnId,
    st: &mut SolverState,
    wired: &mut FxHashSet<(CallSiteId, u32, FnId)>,
) {
    let callee_fn = program.symbols.function(callee);
    for (i, formal) in callee_fn.params.iter().enumerate() {
        let idx = i as u32;
        if let Some(actual) = cs.var_args.iter().find(|(j, _)| *j == idx).map(|(_, v)| *v) {
            let formal_node = pag.var_node.get(formal).copied().expect("formal var node");
            let actual_node = pag.var_node.get(&actual).copied().expect("actual var node");
            // Persistent copy constraint: the actual's points-to may still be
            // growing when the edge is first discovered (direct sites are
            // wired before the first propagation round). A one-shot snapshot
            // here loses interprocedural flow (observed as missed indirect
            // targets passed through parameters). Scalars cannot contribute
            // pointees and are skipped to bound constraint volume.
            if var_may_hold_pointee(program, actual)
                && var_may_hold_pointee(program, *formal)
                && st.wired_copies.insert((actual_node, formal_node))
            {
                ensure_param_copy(pag, st, actual_node, formal_node);
            }
            if let Some(actual_pts) = st.pts.get(&actual_node).cloned() {
                propagate_pts(st, formal_node, &actual_pts);
            }
            wired.insert((cs.id, idx, callee));
        } else if let Some(fn_id) = cs.fn_args.iter().find(|(j, _)| *j == idx).map(|(_, f)| *f) {
            let formal_node = pag.var_node.get(formal).copied().expect("formal var node");
            if let Some(&fn_loc) = pag.fn_locations.get(&fn_id) {
                add_pts(st, formal_node, fn_loc);
            }
            wired.insert((cs.id, idx, callee));
        }
    }
}

fn propagate_pts(st: &mut SolverState, dst: PagNodeId, src_pts: &FxHashSet<LocId>) {
    let mut new_locs: Vec<LocId> = Vec::new();
    {
        let entry = st.pts.entry(dst).or_default();
        for &loc in src_pts {
            if !entry.contains(&loc) {
                new_locs.push(loc);
            }
        }
    }
    if new_locs.is_empty() {
        return;
    }
    {
        let entry = st.pts.get_mut(&dst).expect("entry just created");
        for loc in &new_locs {
            entry.insert(*loc);
        }
    }
    for loc in &new_locs {
        st.loc_nodes.entry(*loc).or_default().insert(dst);
    }
    st.record_delta(dst, &new_locs);
    st.push(dst);
}

fn add_pts(st: &mut SolverState, node: PagNodeId, loc: LocId) {
    let inserted = {
        let entry = st.pts.entry(node).or_default();
        entry.insert(loc)
    };
    if inserted {
        st.loc_nodes.entry(loc).or_default().insert(node);
        st.record_delta(node, &[loc]);
        st.push(node);
    }
}

fn fn_for_loc(pag: &Pag, loc: LocId) -> Option<FnId> {
    let abstract_loc = &pag.locations[loc.0 as usize];
    if abstract_loc.kind == LocKind::Function {
        abstract_loc.fn_id
    } else {
        None
    }
}

fn extract_arg_flow(
    program: &Program,
    call_edges: &[CallGraphEdge],
    wired: &FxHashSet<(CallSiteId, u32, FnId)>,
    result: &mut AnalysisResult,
) {
    for edge in call_edges {
        // Synthetic edges (IPC bridges) have no source-level call site and no
        // argument wiring in v1; skip them here.
        if edge.call_site == SYNTHETIC_CALL_SITE {
            continue;
        }
        let cs = program
            .symbols
            .call_sites
            .get(edge.call_site.0 as usize)
            .filter(|c| c.id == edge.call_site)
            .expect("call site for edge");
        let callee = program.symbols.function(edge.callee);
        for (i, formal) in callee.params.iter().enumerate() {
            let idx = i as u32;
            if wired.contains(&(edge.call_site, idx, edge.callee)) {
                if let Some(actual) = cs.var_args.iter().find(|(j, _)| *j == idx).map(|(_, v)| *v) {
                    result.arg_flow_edges.push(ArgFlowEdge {
                        call_site: edge.call_site,
                        arg_index: idx,
                        actual_var: Some(actual),
                        actual_fn: None,
                        formal: *formal,
                    });
                } else if let Some(fn_id) =
                    cs.fn_args.iter().find(|(j, _)| *j == idx).map(|(_, f)| *f)
                {
                    result.arg_flow_edges.push(ArgFlowEdge {
                        call_site: edge.call_site,
                        arg_index: idx,
                        actual_var: None,
                        actual_fn: Some(fn_id),
                        formal: *formal,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_by_name_classifies_plain_identifiers() {
        let mk = |callee_name: &str, callee_var: Option<u32>, is_direct: bool| trace_ir::CallSite {
            id: trace_ir::CallSiteId(0),
            caller: trace_ir::FnId(0),
            callee_name: callee_name.into(),
            callee_var: callee_var.map(trace_ir::VarId),
            callee_fn_id: None,
            var_args: Vec::new(),
            fn_args: Vec::new(),
            addr_of_member_args: Vec::new(),
            span: trace_ir::Span {
                file: trace_ir::FileId(0),
                line: 1,
                col: 1,
            },
            is_direct,
            receiver_class: None,
            return_dst: None,
        };
        assert!(direct_by_name(&mk("OsalMemCalloc", None, false)));
        assert!(direct_by_name(&mk("f", None, true)));
        assert!(!direct_by_name(&mk("ops->Dispatch", None, false)));
        assert!(!direct_by_name(&mk("obj.fn", None, false)));
        assert!(!direct_by_name(&mk("fp", Some(3), false)));
    }
}
