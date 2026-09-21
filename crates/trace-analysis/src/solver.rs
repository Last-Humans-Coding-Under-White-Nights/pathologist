use crate::constraints::{
    ArgFlowEdge, CallGraphEdge, Constraint, ConstraintKind, LocKind, ResolutionKind,
};
use crate::pag::{Pag, PagNodeKind};
use crate::summaries::{Effect, FnModelSet};
use indexmap::{IndexMap, IndexSet};
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};
use trace_ir::{CallSiteId, FnId, LocId, PagNodeId, Program, StorageClass, TargetId, VarId};

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
    /// Explicit solver pop budget (CLI `--solve-budget-pops`). `None`
    /// derives a default from the PAG constraint count; `Some(0)` means
    /// unlimited. `TRACE_SOLVE_BUDGET_POPS=<n>` overrides either (0 =
    /// unlimited).
    pub solve_budget_pops: Option<u64>,
    /// Wall-clock solver budget in seconds (CLI `--solve-budget-secs`).
    /// `None` = no time limit; `Some(0)` also disables it. The check is
    /// periodic, so a run can overshoot by at most one checkpoint. A time
    /// cap is non-deterministic by nature: two runs of the same inputs on
    /// differently loaded machines stop at different points.
    pub solve_budget_secs: Option<u64>,
    /// Emit synthetic IPC proxy→stub bridge edges (detected from class-name
    /// patterns). Disable to keep the call graph free of synthetic edges.
    pub enable_ipc: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            retain_points_to: false,
            models: std::sync::Arc::new(FnModelSet::builtin()),
            solve_budget_pops: None,
            solve_budget_secs: None,
            enable_ipc: true,
        }
    }
}

/// How a solver run ended, recorded in `analysis_run.options_json`
/// (`solver_partial`, `solver_pops`, `solve_budget_pops`, `solve_budget_secs`)
/// and as an `analyze`-stage diagnostic when the run stopped early, so a
/// consumer can tell a complete database from a budget-truncated one.
#[derive(Debug, Default, Clone)]
pub struct SolveOutcome {
    /// `true` when the worklist drained to the fixpoint: no budget
    /// interrupted the solve.
    pub converged: bool,
    /// Pops (`worklist.pop`) processed.
    pub pops: u64,
    /// The pop budget in force during the solve (`None` = unlimited pops).
    pub budget_pops: Option<u64>,
    /// The time budget in force during the solve (`None` = no time limit).
    pub budget_secs: Option<u64>,
    /// PAG constraint count the solve built from.
    pub constraints: usize,
    /// Wall-clock seconds the solve took.
    pub elapsed_secs: f64,
}

/// The derived default pop budget for a PAG with `constraints` constraints.
///
/// 800 000 pops (the old unconditional default) covers convergence on the
/// eval corpora (the largest, HDF, needs ~42k). Larger trees need far more,
/// so the budget scales a floor of 800 000 by the PAG's own constraint count.
/// Scaling linearly keeps small corpora on the old effective budget while
/// letting mid-size and large trees finish their normal convergence instead
/// of stopping at a partial result. The measurements that sized the linear
/// factor are recorded in `docs/EVAL_REPORT.md` ("Solver work budget"). The
/// override knobs (`--solve-budget-pops`, `TRACE_SOLVE_BUDGET_POPS`) still
/// apply on top.
pub fn default_pops_budget(constraints: usize) -> u64 {
    800_000 + 6 * constraints as u64
}

/// Resolve the pop budget in force for a solve.
///
/// Precedence: an `TRACE_SOLVE_BUDGET_POPS` env override first (`=0` means
/// unlimited; unparseable values fall through to the explicit/derived
/// budget), then the explicit per-run budget (CLI `--solve-budget-pops`,
/// `Some(0)` = unlimited), then the derived default that scales with the
/// PAG's constraint count. Split out for direct testing of every path.
fn pop_budget_for(
    explicit_pops_budget: Option<u64>,
    env_override: Option<&str>,
    constraints: usize,
) -> Option<u64> {
    // `Some(0)` on the explicit budget means "unlimited", `None` means
    // "no explicit budget; derive the default". They must stay distinct
    // until the last step (an explicit 0 must not fall back to the derived
    // default).
    let from_explicit = |derived: u64| -> Option<u64> {
        match explicit_pops_budget {
            Some(0) => None,
            Some(n) => Some(n),
            None => Some(derived),
        }
    };
    let derived = default_pops_budget(constraints);
    match env_override {
        Some(v) if v.trim() == "0" => None,
        Some(v) => match v.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => from_explicit(derived),
        },
        None => from_explicit(derived),
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
    /// How the solver run ended: converged or budget-truncated. A truncated
    /// run exports a `solver_partial` marker and an `analyze`-stage
    /// diagnostic so the database is distinguishable from a complete one.
    pub solve: SolveOutcome,
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
        opts.solve_budget_pops,
        opts.solve_budget_secs,
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
    memory_pts: FxHashMap<LocId, IndexSet<LocId, FxBuildHasher>>,
    loc_nodes: FxHashMap<LocId, FxHashSet<PagNodeId>>,
    worklist: Vec<PagNodeId>,
    queued: FxHashSet<PagNodeId>,
    /// Nodes whose one-time, points-to-independent constraint effects
    /// (addr-of seeding, GEP summary fallback) have already been applied.
    seen_once: FxHashSet<PagNodeId>,
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
    /// redundant merge iterations when memory hasn't grown.
    merge_sizes: FxHashMap<(PagNodeId, LocId), usize>,
}

/// Buffers reused across propagation steps.
///
/// Propagation runs millions of times on a large tree and every step used to
/// allocate a fresh vector; on a 20-second profile of the biggest corpus the
/// allocator, not the set logic, was the solver's largest single cost. These
/// hold nothing between calls, so they live beside `SolverState` rather than
/// in it and are passed to the step that needs them — which lets the borrow
/// checker keep two steps from sharing one buffer.
#[derive(Default)]
struct Scratch {
    /// `apply_store_to_targets`: the store's source set, materialized once.
    store_src: Vec<LocId>,
    /// `apply_store_to_targets`: every location the store writes.
    store_targets: Vec<LocId>,
    /// `apply_store_to_targets`: locations whose loaders must be requeued.
    store_requeues: Vec<LocId>,
    /// `apply_store_to_targets`: signature-filtered views of the source set.
    store_views: StoreViews,
    /// `apply_store_to_targets`: summary cells this store has written.
    summaries_written: FxHashSet<LocId>,
    /// `apply_store_to_targets`: locations this store has already requeued.
    requeued: FxHashSet<LocId>,
    /// `merge_memory_into_if_grown` / `propagate_locs`: the locations one
    /// step adds.
    fresh: Vec<LocId>,
    /// `wire_params`: the actual argument's points-to, snapshotted so the
    /// formal can be written while it is read.
    wire_src: Vec<LocId>,
    /// `touch_loc_holders`: the loading nodes to requeue.
    holders: Vec<PagNodeId>,
}

/// Which function values a memory cell admits, read off its [`SlotGuard`].
/// Non-function values always pass, whatever the guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FnFilter {
    /// No guard, or a guard that constrains nothing: everything passes, so
    /// no filtered view of the source set is needed at all.
    Any,
    /// A concrete non-fn-pointer cell (struct, array, scalar pointer): a
    /// bare function address cannot occur there in valid C.
    None,
    /// A typed fn-pointer slot: same-arity functions, plus those whose
    /// arity is unknown (old-style `()` declarations stay unfiltered).
    Arity(usize),
}

impl FnFilter {
    /// The filter a memory cell's guard imposes.
    fn of(guard: Option<&SlotGuard>) -> Self {
        match guard {
            Option::None => Self::Any,
            Some(SlotGuard::NotFnPtr) => Self::None,
            Some(&SlotGuard::FnParams(n)) => Self::Arity(n),
        }
    }

    /// Does the guard admit `loc` on its own terms, with no exemption for
    /// non-function values?
    ///
    /// Note what `None` means here: a cell whose declared type is a concrete
    /// non-fn-pointer object admits *nothing*, not merely no function value.
    /// Only the memory merge asks the question this way — see
    /// [`SolverState::merge_memory_into_if_grown`].
    fn guard_admits(self, fn_arity: &FxHashMap<LocId, usize>, loc: LocId) -> bool {
        match self {
            Self::Any => true,
            Self::None => false,
            Self::Arity(n) => fn_arity.get(&loc).is_none_or(|p| *p == n),
        }
    }

    /// May `loc` be *stored* into a cell guarded by `self`? Function values
    /// are filtered; everything else flows unfiltered, which is what keeps
    /// the store path sound.
    fn admits(self, fn_arity: &FxHashMap<LocId, usize>, pag: &Pag, loc: LocId) -> bool {
        fn_for_loc(pag, loc).is_none() || self.guard_admits(fn_arity, loc)
    }
}

/// The signature-filtered views of one store's source points-to set.
///
/// Which locations may enter a target cell depends only on that cell's
/// [`FnFilter`], not on the cell itself, so a single filtered view serves
/// every target sharing a filter. Building one filtered copy per *target*
/// was the largest cost in the solver on large corpora. Views are built
/// lazily, and `Any` never needs one: the source set itself is the view.
///
/// `live` views describe the current store; the rest are past stores' and
/// keep their capacity for reuse.
#[derive(Default)]
struct StoreViews {
    views: Vec<(FnFilter, Vec<LocId>)>,
    live: usize,
}

impl StoreViews {
    /// Invalidate every view: the next store has a different source set.
    fn reset(&mut self) {
        self.live = 0;
    }

    /// The source set as `filter` sees it, built at most once per store.
    fn view<'a>(
        &'a mut self,
        filter: FnFilter,
        src: &'a [LocId],
        pag: &Pag,
        fn_arity: &FxHashMap<LocId, usize>,
    ) -> &'a [LocId] {
        if filter == FnFilter::Any {
            return src;
        }
        if let Some(at) = self.views[..self.live]
            .iter()
            .position(|(k, _)| *k == filter)
        {
            return &self.views[at].1;
        }
        if self.live == self.views.len() {
            self.views.push((filter, Vec::new()));
        }
        let at = self.live;
        self.live += 1;
        let (key, view) = &mut self.views[at];
        *key = filter;
        view.clear();
        view.extend(
            src.iter()
                .copied()
                .filter(|&l| filter.admits(fn_arity, pag, l)),
        );
        &self.views[at].1
    }
}

/// Declared content type of a memory cell / points-to slot, as far as it is
/// known, used to keep incompatible function values out under wrong-type
/// pointer flow.
#[derive(Debug, Clone, Copy)]
enum SlotGuard {
    /// Slot is a `FnPtr` taking `n` parameters.
    FnParams(usize),
    /// Slot is a concrete non-fn-pointer type (e.g. a struct object);
    /// storing a bare function address there cannot occur in valid C.
    NotFnPtr,
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
    fn touch_loc_holders(
        &mut self,
        holders: &mut Vec<PagNodeId>,
        loc: LocId,
        load_src: &FxHashMap<PagNodeId, Vec<usize>>,
    ) {
        // Collected into a scratch buffer in the set's own iteration order:
        // every write used to clone the whole holder set, which on hub
        // locations is thousands of nodes.
        holders.clear();
        if let Some(nodes) = self.loc_nodes.get(&loc) {
            holders.extend(nodes.iter().copied().filter(|n| load_src.contains_key(n)));
        }
        for &n in holders.iter() {
            self.record_delta(n, &[loc]);
            self.push(n);
        }
    }

    /// Record freshly inserted locations for difference propagation.
    fn record_delta(&mut self, node: PagNodeId, new_locs: &[LocId]) {
        match new_locs {
            [] => {}
            // `touch_loc_holders` records one location per holder and usually
            // finds it already pending, so that path must not touch `delta`
            // unless it actually records something.
            &[loc] => {
                if self.delta_pending.insert((node, loc)) {
                    self.delta.entry(node).or_default().push(loc);
                }
            }
            _ => {
                let Self {
                    delta,
                    delta_pending,
                    ..
                } = self;
                let delta = delta.entry(node).or_default();
                for &loc in new_locs {
                    if delta_pending.insert((node, loc)) {
                        delta.push(loc);
                    }
                }
            }
        }
    }

    /// Merge whatever `memory_pts[mem_loc]` gained since the last merge for
    /// this `(dst, mem_loc)` pair into `pts[dst]`, without cloning. Skipping
    /// an ungrown memory breaks the `touch_loc_holders` → re-merge cycle that
    /// dominated budget on large trees.
    ///
    /// Note the asymmetry with the store path: this applies the cell's guard
    /// to *every* location it holds, not only to function values, so a cell
    /// declared as a concrete non-fn-pointer object contributes nothing at
    /// all to a points-to set. That is `arity_allows`'s behavior carried over
    /// unchanged — this change is required to keep exports byte-identical, so
    /// it is preserved, not endorsed.
    fn merge_memory_into_if_grown(
        &mut self,
        fresh: &mut Vec<LocId>,
        dst: PagNodeId,
        mem_loc: LocId,
    ) {
        let key = (dst, mem_loc);
        let prev_len = self.merge_sizes.get(&key).copied().unwrap_or(0);
        let Some(cur_len) = self.memory_pts.get(&mem_loc).map(IndexSet::len) else {
            return;
        };
        if cur_len <= prev_len {
            return;
        }
        // The cell's own filter is the same for every element it holds, so it
        // is read once here rather than once per element.
        let filter = FnFilter::of(self.slot_guard.get(&mem_loc));
        fresh.clear();
        // Collect first, insert second. Fusing the two — `pts.insert(loc)` in
        // place of `!pts.contains(&loc)` — looks equivalent and saves a hash
        // per added location, but it measurably reorders `pts` and changes the
        // exported `points_to` rows on the HDF corpus. Whatever the mechanism,
        // this pass is not the place to find out: keep the two passes.
        {
            let Self {
                memory_pts,
                pts,
                fn_arity,
                ..
            } = self;
            let mem = memory_pts.get(&mem_loc).expect("memory checked above");
            let pts = pts.entry(dst).or_default();
            for i in prev_len..cur_len {
                let loc = mem[i];
                if filter.guard_admits(fn_arity, loc) && !pts.contains(&loc) {
                    fresh.push(loc);
                }
            }
        }
        self.merge_sizes.insert(key, cur_len);
        if fresh.is_empty() {
            return;
        }
        {
            let pts = self.pts.get_mut(&dst).expect("pts entry exists");
            for &loc in fresh.iter() {
                pts.insert(loc);
            }
        }
        self.commit_fresh(dst, fresh);
    }

    /// Finish a step that just added `fresh` to `pts[dst]`: index the new
    /// locations, record them for difference propagation, and requeue the
    /// node. Shared by every write path so the sequence stays in one place.
    fn commit_fresh(&mut self, dst: PagNodeId, fresh: &[LocId]) {
        if fresh.is_empty() {
            return;
        }
        for &loc in fresh {
            self.loc_nodes.entry(loc).or_default().insert(dst);
        }
        self.record_delta(dst, fresh);
        self.push(dst);
    }
}

fn st_pts_stats_max(pts: &IndexMap<PagNodeId, FxHashSet<LocId>>) -> usize {
    pts.values().map(|s| s.len()).max().unwrap_or(0)
}

/// Maximum distinct locations remembered per instance-insensitive summary
/// location. Past this, further stores to it are dropped — deliberate
/// imprecision that bounds the cost of a hub summary on large trees. See the
/// cap check in `apply_store_to_targets`.
const SUMMARY_MEM_CAP: usize = 1024;

fn solve(
    pag: &mut Pag,
    program: &Program,
    retain_points_to: bool,
    models: &FnModelSet,
    explicit_pops_budget: Option<u64>,
    explicit_secs_budget: Option<u64>,
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
        wired_copies: FxHashSet::default(),
        wired_model_edges: FxHashSet::default(),
        slot_guard: FxHashMap::default(),
        fn_arity: FxHashMap::default(),
        merge_sizes: FxHashMap::default(),
    };
    // Lives beside the state, not in it: passing a buffer to the step that
    // needs it makes two steps sharing one a compile error.
    let mut scratch = Scratch::default();
    // Per-location slot guards and per-function parameter counts for
    // signature-aware propagation.
    use trace_ir::TypeDesc as TD;
    for loc in &pag.locations {
        let g = match program.types.get(loc.type_id).desc.as_ref() {
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

    // Read once: the phase timings, the periodic progress lines and the
    // summary are one switch.
    let stats_enabled = std::env::var("TRACE_SOLVER_STATS").is_ok();
    let resolve_started = std::time::Instant::now();
    let mut resolved_callees = 0usize;
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
        for callee in program.callees_of(cs) {
            resolved_callees += 1;
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
                wire_params(
                    pag,
                    program,
                    cs,
                    callee,
                    &mut st,
                    &mut scratch,
                    &mut wired_arg_flow,
                );
            }
            apply_fn_model(pag, &mut st, cs, &f.name, models, &mut terminator_events);
        }
    }
    if stats_enabled {
        eprintln!(
            "[solver] RESOLVE sites={} callees={} elapsed={:?}",
            program.symbols.call_sites.len(),
            resolved_callees,
            resolve_started.elapsed()
        );
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
    // visible warning; normal corpora converge far below it. The default
    // scales with the PAG's constraint count (see `default_pops_budget`),
    // so small corpora keep the old 800 000-pop behaviour while large trees
    // finish their normal convergence. `TRACE_SOLVE_BUDGET_POPS=<n>` as an
    // environment override; `=0` restores unlimited solving. The time budget
    // is opt-in (`--solve-budget-secs`) and checked periodically, so it can
    // overshoot by at most one checkpoint.
    let solve_budget: Option<u64> = pop_budget_for(
        explicit_pops_budget,
        std::env::var("TRACE_SOLVE_BUDGET_POPS").ok().as_deref(),
        pag.constraints.len(),
    );
    // `Some(0)` on the CLI means "no time limit", mirroring the pop flag.
    let solve_time_budget: Option<u64> = explicit_secs_budget.filter(|&n| n != 0);

    if stats_enabled {
        eprintln!(
            "[solver] START constraints={} vars={}",
            pag.constraints.len(),
            program.symbols.variables.len()
        );
    }
    let mut converged = true;
    // `Some(&str)` describes which budget stopped the solve, for the stderr
    // message and the exported diagnostic.
    let mut stopped_by: Option<&'static str> = None;
    while let Some(node) = st.worklist.pop() {
        st.queued.remove(&node);
        // A budget of N allows exactly N pops of work: the (N+1)-th node is
        // popped off the worklist and dropped unprocessed, and `pops` keeps
        // counting only processed work, so "after N pops" matches reality.
        if let Some(budget) = solve_budget {
            if pops >= budget {
                converged = false;
                stopped_by = Some("pop budget");
                eprintln!(
                    "[solver] pop budget {} exhausted after {} pops / {:?}; stopping, results are partial \
                     (raise --solve-budget-pops, set TRACE_SOLVE_BUDGET_POPS higher, or 0 for unlimited)",
                    budget,
                    pops,
                    t0.elapsed()
                );
                break;
            }
        }
        if let Some(secs) = solve_time_budget {
            // Checked every checkpoint rather than per pop: the per-pop check
            // cost a measurable fraction of solve time on large corpora. The
            // pop in flight when the clock runs out still finishes, and a run
            // that converges in under 10k pops never checks the clock, so the
            // overshoot is "up to one pop in flight plus the checkpoint
            // interval", not a hard `budget + 1`.
            if pops > 0 && pops.is_multiple_of(10_000) && t0.elapsed().as_secs() >= secs {
                converged = false;
                stopped_by = Some("time budget");
                eprintln!(
                    "[solver] time budget {secs}s exhausted after {} pops / {:?}; stopping, results are partial \
                     (raise --solve-budget-secs, or 0 for no time limit)",
                    pops,
                    t0.elapsed()
                );
                break;
            }
        }
        pops += 1;
        if stats_enabled && pops > 0 && pops.is_multiple_of(100_000) {
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
                            let summary_opt = if program.layouts_unioned {
                                // Cloned only here: `ensure_*` needs `&mut pag`,
                                // and the baseline path must not pay for it.
                                let expected = pag.constraints[idx].field_name.clone();
                                pag.ensure_field_summary_for_var_named(
                                    program,
                                    base_var,
                                    field,
                                    expected.as_deref(),
                                )
                            } else {
                                pag.ensure_field_summary_for_var(program, base_var, field)
                            };
                            if let Some(summary) = summary_opt {
                                propagate_locs(&mut st, &mut scratch.fresh, dst, [summary]);
                                st.merge_memory_into_if_grown(&mut scratch.fresh, dst, summary);
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
                propagate_locs(&mut st, &mut scratch.fresh, dst, delta.iter().copied());
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
                        st.merge_memory_into_if_grown(&mut scratch.fresh, dst, loc);
                    }
                }
            }
        }

        // Read by reference: `apply_store_to_targets` only needs `&Pag`, so
        // these index lists need no per-pop copy (the `gep` branch below
        // still does — it synthesizes locations through `&mut pag`).
        if !delta.is_empty() {
            if let Some(idxs) = pag.indices.store_dst.get(&node) {
                for &idx in idxs {
                    w_store += delta.len() as u64;
                    apply_store_to_targets(pag, idx, &mut st, &mut scratch, Some(delta.as_slice()));
                }
            }

            if let Some(idxs) = pag.indices.store_src.get(&node) {
                for &idx in idxs {
                    w_store += 1;
                    apply_store_to_targets(pag, idx, &mut st, &mut scratch, None);
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
                    let mut effective_field = field;
                    // Cross-struct FieldId guard: if the GEP carries a
                    // field name from lowering, reject pointees whose
                    // struct type has a different field at the same
                    // positional index — this prevents functions from
                    // unrelated structs leaking as indirect-call targets.
                    // Unioning a layout across configurations can move a field
                    // off its base positional index, so once variants have
                    // actually been merged the name is resolved instead of the
                    // pointee being rejected. Requesting `--explore` is not
                    // enough: with no variant merged nothing was unioned, and
                    // relaxing the guard would only let unrelated structs
                    // through.
                    if let Some(ref expected) = expected_name {
                        if let Some(parent_type) =
                            crate::pag::struct_type_for_loc(pag, program, loc)
                        {
                            match program.types.get(parent_type).layout.fields.get(&field) {
                                Some(fl) if fl.name == *expected => {}
                                _ if program.layouts_unioned => {
                                    if let Some(fid) =
                                        program.types.field_id_by_name(parent_type, expected)
                                    {
                                        effective_field = fid;
                                    } else {
                                        continue;
                                    }
                                }
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
                            if n > 0
                                && pag.field_slot_arity(program, loc, effective_field) == Some(n)
                            {
                                add_pts(&mut st, dst, loc);
                            }
                        }
                        continue;
                    }
                    if let Some(field_loc) = pag.ensure_field_loc(program, loc, effective_field) {
                        produced_cell = true;
                        // Field loc plus its instance-insensitive summary:
                        // the GEP result points AT these cells.
                        let targets = [Some(field_loc), pag.summary_for_field_loc(field_loc)];
                        propagate_locs(
                            &mut st,
                            &mut scratch.fresh,
                            dst,
                            targets.iter().filter_map(|t| t.as_ref().copied()),
                        );
                        // Cell contents reach the address node so that uses of
                        // the field lvalue (`&obj.f` passed onward, then
                        // loaded) still observe stores that lowering recorded
                        // against the cell without an intervening load temp.
                        for fl in targets.into_iter().flatten() {
                            st.merge_memory_into_if_grown(&mut scratch.fresh, dst, fl);
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
                        let summary_opt = if program.layouts_unioned {
                            pag.ensure_field_summary_for_var_named(
                                program,
                                base_var,
                                field,
                                expected_name.as_deref(),
                            )
                        } else {
                            pag.ensure_field_summary_for_var(program, base_var, field)
                        };
                        if let Some(summary) = summary_opt {
                            propagate_locs(&mut st, &mut scratch.fresh, dst, [summary]);
                            st.merge_memory_into_if_grown(&mut scratch.fresh, dst, summary);
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
                    let target = pag.node_target(program, dst);
                    // A dlsym name is a linkage name, so the global namespace
                    // bucket holds every entry it can name — including the
                    // `::name` spelling. Scanning the whole function table
                    // instead cost one pass over the program per string
                    // literal reaching a dlsym site.
                    for id in program.symbols.functions_in_namespace("", &name) {
                        if program.symbols.function(id).target == target {
                            if let Some(&fn_loc) = pag.fn_locations.get(&id) {
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
                // Loop-invariant: this is the solver's indirect-call fixpoint.
                let caller_target = program.symbols.function(cs.caller).target;
                for &loc in delta.iter() {
                    if let Some(fn_id) = fn_for_loc(pag, loc) {
                        new_callees.extend(reached_definitions(program, fn_id, caller_target));
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
                        wire_params(
                            pag,
                            program,
                            cs,
                            callee,
                            &mut st,
                            &mut scratch,
                            &mut wired_arg_flow,
                        );
                        // Expand return flows from the callee into the
                        // `CallReturnIndirect` destination so the return
                        // value reaches the assignment LHS (e.g.
                        // `sbuf->impl = constructor->obtain(capacity)`).
                        if let Some(callee_var) = cs.callee_var {
                            let dst_nodes = pag.indirect_return_dst.get(&callee_var).cloned();
                            if let Some(dst_nodes) = dst_nodes {
                                for dst_n in dst_nodes {
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

    let callbacks = callback_edges(program, pag, &st, models, &call_edges);
    call_edges.extend(callbacks);

    // Emit synthetic call edges for IPC proxy→stub bridges detected at PAG
    // build. These connect the proxy method to the stub handler it would
    // dispatch to across the (opaque) Binder boundary. The resolution is
    // marked `IpcBridge` (distinct from a source-level direct call). Edges
    // may target external interface methods when the stub delegates to an
    // inherited pure-virtual; these are still emitted for call graph
    // completeness.
    // Deliberately unfiltered by target, unlike the indirect-call loop above:
    // a Binder call is precisely one that crosses the process — and therefore
    // the link image — boundary, so a bridge's endpoints normally differ in
    // `target_id` (see `docs/ANALYSIS.md`, "Link targets and weak symbols").
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

    if stats_enabled {
        let biggest = st_pts_stats_max(&points_to);
        eprintln!(
            "[solver] DONE pops={} elapsed={:?} constraints={} max_pts={} resolved_sites={}",
            pops,
            t0.elapsed(),
            pag.constraints.len(),
            biggest,
            resolved_indirect.len()
        );
        if !converged {
            eprintln!(
                "[solver] PARTIAL stopped by {reason}: {pops} pops processed; result is a truncated \
                 fixpoint (see analysis_run.options_json.solver_partial)",
                reason = stopped_by.unwrap_or("budget")
            );
        }
    }

    let solve = SolveOutcome {
        converged,
        pops,
        budget_pops: solve_budget,
        budget_secs: solve_time_budget,
        constraints: pag.constraints.len(),
        elapsed_secs: t0.elapsed().as_secs_f64(),
    };

    AnalysisResult {
        points_to,
        call_edges,
        arg_flow_edges: Vec::new(),
        wired_arg_flow,
        terminator_events,
        solve,
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
            Effect::ReturnAlias { .. }
            | Effect::ReturnHeap
            | Effect::Dlsym { .. }
            | Effect::Invoke { .. } => {}
        }
    }
}

/// Add `locs` to `pts[dst]`, indexing and requeueing whatever is new.
///
/// The single write path: `Copy` propagation, GEP results, memory merges and
/// parameter wiring all funnel through here, so the order in which fresh
/// locations reach `delta` is defined in one place.
fn propagate_locs(
    st: &mut SolverState,
    fresh: &mut Vec<LocId>,
    dst: PagNodeId,
    locs: impl IntoIterator<Item = LocId>,
) {
    fresh.clear();
    {
        let entry = st.pts.entry(dst).or_default();
        for loc in locs {
            if !entry.contains(&loc) {
                fresh.push(loc);
            }
        }
    }
    {
        let entry = st.pts.get_mut(&dst).expect("entry just created");
        for &loc in fresh.iter() {
            entry.insert(loc);
        }
    }
    st.commit_fresh(dst, fresh);
}

/// Store `*ptr = value`: write the value side's current points-to (plus its
/// own storage location for var nodes) into the memories of the given target
/// locations. `targets == None` means every location currently in the pointer
/// node's set; `Some(delta)` restricts writes to newly gained targets
/// (difference propagation — their memory is written for the first time).
fn apply_store_to_targets(
    pag: &Pag,
    idx: usize,
    st: &mut SolverState,
    scratch: &mut Scratch,
    targets: Option<&[LocId]>,
) {
    let c = &pag.constraints[idx];
    let (src_node, dst_node) = (c.src, c.dst);
    let self_loc = match pag.nodes[src_node.0 as usize].kind {
        PagNodeKind::Var(v) => pag.var_location.get(&v).copied(),
        _ => None,
    };
    let src_empty = st.pts.get(&src_node).map(|s| s.is_empty()).unwrap_or(true);
    if src_empty && self_loc.is_none() {
        return;
    }

    let target_slice: &[LocId] = match targets {
        // `ts` borrows the caller-owned delta vector, disjoint from `st`:
        // iterate it directly, no copy needed.
        Some(ts) => ts,
        None => {
            scratch.store_targets.clear();
            if let Some(s) = st.pts.get(&dst_node) {
                scratch.store_targets.extend(s.iter().copied());
            }
            &scratch.store_targets
        }
    };
    if target_slice.is_empty() {
        return;
    }

    // Clone-free store: `pts` and `memory_pts` are disjoint fields, so the
    // source set is snapshotted once — in its own iteration order — and every
    // target writes that same snapshot. A filter can only ever reject a
    // *function* value, so a source carrying none needs no view at all.
    let src = &mut scratch.store_src;
    src.clear();
    let mut src_has_fns = false;
    if let Some(s) = st.pts.get(&src_node) {
        for &l in s.iter() {
            src_has_fns |= fn_for_loc(pag, l).is_some();
            src.push(l);
        }
    }
    let src = &scratch.store_src;
    let views = &mut scratch.store_views;
    views.reset();
    scratch.summaries_written.clear();
    scratch.requeued.clear();
    scratch.store_requeues.clear();

    for &loc in target_slice {
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
        let filter = if src_has_fns {
            FnFilter::of(st.slot_guard.get(&loc))
        } else {
            FnFilter::Any
        };
        {
            let view = views.view(filter, src, pag, &st.fn_arity);
            let entry = st.memory_pts.entry(loc).or_default();
            let before = entry.len();
            for &l in view {
                entry.insert(l);
            }
            if let Some(sl) = self_loc {
                entry.insert(sl);
            }
            changed |= entry.len() > before;
        }
        let summary_loc = pag.summary_for_field_loc(loc);
        if let Some(summary) = summary_loc {
            let entry = st.memory_pts.entry(summary).or_default();
            let before_summary = entry.len();
            // Past the cap the summary stops growing: this store's writes to
            // it are dropped, and every later one is too, since cell memory
            // never shrinks.
            if before_summary < SUMMARY_MEM_CAP && scratch.summaries_written.insert(summary) {
                // Many of a store's targets are field cells of the same
                // struct type and field, and they all share that summary.
                // Memory only grows, so once this store has written the
                // summary every later write of it in the same store is an
                // insert-by-insert no-op, and is skipped.
                //
                // The summary cell mirrors the field's declared type, but
                // only its function-vs-not distinction: a typed fn-pointer
                // summary takes function values of any arity.
                let summary_filter = match st.slot_guard.get(&summary) {
                    Some(SlotGuard::NotFnPtr) if src_has_fns => FnFilter::None,
                    _ => FnFilter::Any,
                };
                let view = views.view(summary_filter, src, pag, &st.fn_arity);
                let entry = st.memory_pts.entry(summary).or_default();
                for &l in view {
                    entry.insert(l);
                }
                if let Some(sl) = self_loc {
                    entry.insert(sl);
                }
                changed |= entry.len() > before_summary;
            }
        }
        if changed {
            scratch.store_requeues.push(loc);
            if let Some(summary) = summary_loc {
                scratch.store_requeues.push(summary);
            }
        }
    }

    // Requeuing a location twice is a no-op the second time (its delta and
    // worklist entries are already there), but re-walking its holder set is
    // not. Shared summaries make that duplicate common; first-occurrence
    // order is kept, so the surviving calls run in the order they did.
    for i in 0..scratch.store_requeues.len() {
        let loc = scratch.store_requeues[i];
        if scratch.requeued.insert(loc) {
            st.touch_loc_holders(&mut scratch.holders, loc, &pag.indices.load_src);
        }
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
    let desc = program.types.get(v.type_id).desc.as_ref();
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
    scratch: &mut Scratch,
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
            if let Some(actual_pts) = st.pts.get(&actual_node) {
                // Snapshotted rather than cloned: this used to copy the
                // actual's whole points-to set once per wired argument.
                scratch.wire_src.clear();
                scratch.wire_src.extend(actual_pts.iter().copied());
                propagate_locs(
                    st,
                    &mut scratch.fresh,
                    formal_node,
                    scratch.wire_src.iter().copied(),
                );
            }
            wired.insert((cs.id, idx, callee));
        } else if cs.fn_args.iter().any(|(j, _)| *j == idx) {
            // A name with overloads passes each one it may mean.
            let formal_node = pag.var_node.get(formal).copied().expect("formal var node");
            for &(_, fn_id) in cs.fn_args.iter().filter(|(j, _)| *j == idx) {
                if let Some(&fn_loc) = pag.fn_locations.get(&fn_id) {
                    add_pts(st, formal_node, fn_loc);
                }
            }
            wired.insert((cs.id, idx, callee));
        }
    }
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

/// The callees a model says may invoke a callback argument, with the argument
/// positions it names (`docs/ANALYSIS.md`, "Function models"). Read off the
/// symbol table rather than off the call graph: a model that invokes is rare,
/// so the index is nearly always empty or tiny, and the callback pass is
/// skipped outright when it is empty.
fn invoked_params(program: &Program, models: &FnModelSet) -> FxHashMap<FnId, Vec<u32>> {
    let mut by_callee = FxHashMap::default();
    for callee in &program.symbols.functions {
        let Some(model) = models.get_for_callee(&callee.name) else {
            continue;
        };
        let params: Vec<u32> = model
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Invoke { param } => Some(*param),
                _ => None,
            })
            .collect();
        if !params.is_empty() {
            by_callee.insert(callee.id, params);
        }
    }
    by_callee
}

/// Indirect edges for the callbacks handed to a modelled callee (`invoke`).
/// A zero-argument callback needs no parameter wiring and its return is
/// ignored, so this runs once on the converged points-to sets rather than
/// inside the fixpoint. The edge is attributed to the submitting call site,
/// which is where the caller hands the callback over, and stays inside the
/// caller's link image like every other indirect edge.
fn callback_edges(
    program: &Program,
    pag: &Pag,
    st: &SolverState,
    models: &FnModelSet,
    call_edges: &[CallGraphEdge],
) -> Vec<CallGraphEdge> {
    let invoked = invoked_params(program, models);
    let mut edges = Vec::new();
    if invoked.is_empty() {
        return edges;
    }
    // A callback's arity as `merge_unit` reads it: the explicit count when
    // the declaration gives one, else the parameters it has.
    let takes_nothing =
        |f: &trace_ir::Function| f.explicit_arity.map_or(f.params.is_empty(), |n| n == 0);
    // Seeded with what the call graph already holds: a callback the callee's
    // own body reaches keeps the edge it earned there instead of gaining a
    // second one here.
    let mut seen: FxHashSet<(CallSiteId, FnId)> = call_edges
        .iter()
        .map(|edge| (edge.call_site, edge.callee))
        .collect();
    for edge in call_edges {
        let Some(params) = invoked.get(&edge.callee) else {
            continue;
        };
        let Some(cs) = program.symbols.call_site_by_id(edge.call_site) else {
            continue;
        };
        let caller_target = program.symbols.function(cs.caller).target;
        for param in params {
            // A member's arguments are recorded past its `this`.
            let Some(index) = param.checked_add(u32::from(cs.args_bound_past_this)) else {
                continue;
            };
            let named = cs
                .fn_args
                .iter()
                .filter(|(i, _)| *i == index)
                .map(|(_, f)| *f);
            let pointed = cs
                .var_args
                .iter()
                .filter(|(i, _)| *i == index)
                .filter_map(|(_, var)| pag.var_node.get(var).and_then(|n| st.pts.get(n)))
                .flatten()
                .filter_map(|loc| fn_for_loc(pag, *loc));
            for callee in named.chain(pointed) {
                for target in reached_definitions(program, callee, caller_target) {
                    if !takes_nothing(program.symbols.function(target)) {
                        continue;
                    }
                    if seen.insert((cs.id, target)) {
                        edges.push(CallGraphEdge {
                            call_site: cs.id,
                            caller: cs.caller,
                            callee: target,
                            resolution: ResolutionKind::Indirect,
                        });
                    }
                }
            }
        }
    }
    // A points-to set is a hash set; the order its members come out in must
    // not reach the export (AGENTS.md, determinism).
    edges.sort_by_key(|edge| (edge.call_site, edge.callee));
    edges
}

/// The functions an indirect call from a caller in `caller_target` reaches
/// through `fn_id`: nothing when link images are in play and the function is
/// in another one; else, when it is a declaration (a weak forward declaration,
/// a header prototype), the definitions under its name first, so return flows
/// and parameter wiring reach the real body, then `fn_id` itself.
fn reached_definitions(
    program: &Program,
    fn_id: FnId,
    caller_target: Option<TargetId>,
) -> Vec<FnId> {
    let callee = program.symbols.function(fn_id);
    let scoped = !program.link_targets.is_empty();
    if scoped && callee.target != caller_target {
        return Vec::new();
    }
    let mut reached = Vec::new();
    if !callee.is_defined {
        reached.extend(
            program
                .symbols
                .resolve_function_candidates_in_target(
                    &callee.name,
                    Some(callee.file),
                    caller_target,
                )
                .into_iter()
                .filter(|c| program.symbols.function(*c).is_defined),
        );
    }
    reached.push(fn_id);
    reached
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
                } else {
                    for &(_, fn_id) in cs.fn_args.iter().filter(|(j, _)| *j == idx) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constraints::AbstractLocation;
    use trace_ir::{LocId, TypeId};

    fn loc(id: u32, is_fn: bool) -> AbstractLocation {
        AbstractLocation {
            id: LocId(id),
            kind: if is_fn {
                LocKind::Function
            } else {
                LocKind::Global
            },
            var: None,
            fn_id: is_fn.then_some(FnId(id)),
            field: None,
            type_id: TypeId(0),
            desc: String::new(),
        }
    }

    /// Locations 0-2 are function values of arity 1, 2 and unknown; 3-4 are
    /// ordinary objects, which no slot guard may reject.
    fn store_fixture() -> (Pag, FxHashMap<LocId, usize>, Vec<LocId>) {
        let mut pag = Pag::default();
        for i in 0..5u32 {
            pag.locations.push(loc(i, i < 3));
        }
        let mut fn_arity = FxHashMap::default();
        fn_arity.insert(LocId(0), 1);
        fn_arity.insert(LocId(1), 2);
        (pag, fn_arity, (0..5).map(LocId).collect())
    }

    /// `StoreViews` replaces the filtered copy the old solver built per
    /// target location with one view per filter. Each view must select
    /// exactly what the element-wise predicate selects.
    #[test]
    fn store_views_match_element_wise_filtering() {
        let (pag, fn_arity, src) = store_fixture();
        let mut views = StoreViews::default();
        for filter in [
            FnFilter::Any,
            FnFilter::None,
            FnFilter::Arity(1),
            FnFilter::Arity(2),
            FnFilter::Arity(3),
        ] {
            views.reset();
            let view = views.view(filter, &src, &pag, &fn_arity).to_vec();
            let expected: Vec<LocId> = src
                .iter()
                .copied()
                .filter(|&l| filter.admits(&fn_arity, &pag, l))
                .collect();
            assert_eq!(view, expected, "filter {filter:?}");
        }
    }

    /// Every `SlotGuard` maps to the filter that reproduces it, so the guard
    /// rule has one implementation rather than one per call site.
    #[test]
    fn slot_guards_map_to_the_filter_they_describe() {
        let (_, fn_arity, _) = store_fixture();
        assert_eq!(FnFilter::of(None), FnFilter::Any);
        assert_eq!(FnFilter::of(Some(&SlotGuard::NotFnPtr)), FnFilter::None);
        assert_eq!(
            FnFilter::of(Some(&SlotGuard::FnParams(2))),
            FnFilter::Arity(2)
        );
        // An unguarded slot takes anything; a non-fn-pointer slot takes no
        // function value; a typed slot takes its own arity and unknowns.
        assert!(FnFilter::Any.guard_admits(&fn_arity, LocId(0)));
        assert!(!FnFilter::None.guard_admits(&fn_arity, LocId(0)));
        assert!(FnFilter::Arity(1).guard_admits(&fn_arity, LocId(0)));
        assert!(!FnFilter::Arity(2).guard_admits(&fn_arity, LocId(0)));
        assert!(
            FnFilter::Arity(9).guard_admits(&fn_arity, LocId(2)),
            "unknown arity passes"
        );
    }

    /// Within one store a repeated filter reuses the view already built; a
    /// `reset` invalidates every view so the next store, whose source set is
    /// a different one, cannot be answered from it.
    #[test]
    fn store_views_are_reused_within_a_store_and_dropped_between_stores() {
        let (pag, fn_arity, src) = store_fixture();
        let mut views = StoreViews::default();

        views.reset();
        // `Any` is the source set itself and never occupies a view slot.
        assert_eq!(views.view(FnFilter::Any, &src, &pag, &fn_arity), src);
        assert_eq!(views.live, 0);
        assert_eq!(
            views.view(FnFilter::Arity(1), &src, &pag, &fn_arity).len(),
            4
        );
        assert_eq!(views.live, 1);
        assert_eq!(
            views.view(FnFilter::Arity(1), &src, &pag, &fn_arity).len(),
            4
        );
        assert_eq!(views.live, 1, "a repeated filter reuses its view");
        assert_eq!(
            views.view(FnFilter::Arity(2), &src, &pag, &fn_arity).len(),
            4
        );
        assert_eq!(views.live, 2);
        assert_eq!(
            views.view(FnFilter::None, &src, &pag, &fn_arity),
            [LocId(3), LocId(4)]
        );

        views.reset();
        assert!(views
            .view(FnFilter::Arity(1), &[], &pag, &fn_arity)
            .is_empty());
        assert!(views.view(FnFilter::None, &[], &pag, &fn_arity).is_empty());
    }

    #[test]
    fn pop_budget_resolution_precedence() {
        // Derived default scales with the constraint count; small corpora
        // keep the historical 800 000-pop floor.
        assert_eq!(default_pops_budget(0), 800_000);
        assert_eq!(default_pops_budget(257_318), 2_343_908);

        // No env override: explicit beats derived, Some(0) = unlimited.
        assert_eq!(pop_budget_for(None, None, 0), Some(800_000));
        assert_eq!(pop_budget_for(Some(1), None, 0), Some(1));
        assert_eq!(pop_budget_for(Some(0), None, 0), None);

        // Invalid explicit value is impossible at the CLI (u64 arg) but the
        // API normalizes Some(0) to unlimited.
        assert_eq!(pop_budget_for(Some(0), Some("999"), 0), Some(999));

        // Env override wins: `=0` is unlimited, an unparseable value falls
        // back to the explicit budget.
        assert_eq!(pop_budget_for(Some(1), Some("0"), 0), None);
        assert_eq!(pop_budget_for(Some(2), Some("garbage"), 0), Some(2));
        assert_eq!(pop_budget_for(None, Some("garbage"), 0), Some(800_000));
        assert_eq!(pop_budget_for(Some(2), Some("5000"), 0), Some(5000));
    }
}
