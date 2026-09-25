use crate::constraints::{
    ArgFlowEdge, CallGraphEdge, Constraint, ConstraintKind, LocKind, ResolutionKind,
};
use crate::pag::{Pag, PagNodeKind, SolverIndices};
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
    extract_arg_flow(program, &pag, &call_edges, &wired, &mut result);
    (pag, result)
}

#[derive(Default)]
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
    /// Last-seen `memory_pts[loc]` size per destination and cell, used to
    /// skip redundant merge iterations when memory hasn't grown. Keyed by
    /// destination first: a load merges many cells into one destination.
    merge_sizes: FxHashMap<PagNodeId, FxHashMap<LocId, usize>>,
    /// Locations that are the source of an `AddrOf` constraint.
    addr_taken: FxHashSet<LocId>,
    /// Variable node → the memory cell kept in step with it, and back
    /// (docs/ANALYSIS.md, "Variable cells"). Filled when the cell's address is
    /// first taken ([`register_synced_cell`]), so the per-pop hooks are one
    /// lookup each.
    synced_cell: FxHashMap<PagNodeId, LocId>,
    cell_owner: FxHashMap<LocId, PagNodeId>,
    /// `(call site, parameter)` terminator events already recorded, so each
    /// is recorded once without scanning the event list.
    terminators_seen: FxHashSet<(CallSiteId, u32)>,
    /// Membership bits mirroring the large `pts` and `memory_pts` sets
    /// ([`LocBits`]): the sets stay the storage and the iteration order,
    /// the bits only answer "already held" without hashing.
    loc_bits: LocBits,
    pts_bits: FxHashMap<PagNodeId, Vec<u64>>,
    memory_bits: FxHashMap<LocId, Vec<u64>>,
}

/// Sets of at least this many locations get a [`LocBits`] mirror.
const MIRROR_MIN: usize = 64;

/// A dense numbering of the locations large sets hold, and bit-set
/// membership over it.
///
/// Nearly every membership test the solver makes on a hub set is a hit: a
/// memory merge re-checks each location a cell gained against a destination
/// that usually holds it already, and a store re-inserts its whole source
/// into cells that usually hold it already. A bit test answers those without
/// hashing. The hub sets share a few thousand locations out of tens of
/// thousands, so bits index a dense numbering of just those, which keeps a
/// mirror a few hundred bytes instead of one bit per location.
///
/// A mirror only ever says "held": a location is numbered and its bit set
/// only once its set holds it, and sets never shrink. A clear bit falls back
/// to the set itself, so what the solver computes, and in which order, does
/// not depend on the mirrors at all.
#[derive(Default)]
struct LocBits {
    /// `LocId` → its dense number plus one; `0` for a location no mirror
    /// holds.
    number: Vec<u32>,
    next: u32,
}

impl LocBits {
    /// Whether `bits` has `loc`'s bit set.
    #[inline]
    fn test(&self, bits: &[u64], loc: LocId) -> bool {
        match self.number.get(loc.0 as usize) {
            Some(&n) if n != 0 => {
                let n = n - 1;
                bits.get(n as usize / 64)
                    .is_some_and(|word| word >> (n % 64) & 1 != 0)
            }
            _ => false,
        }
    }

    /// Set `loc`'s bit in `bits`, numbering `loc` first if it has none.
    fn set(&mut self, bits: &mut Vec<u64>, loc: LocId) {
        let i = loc.0 as usize;
        if i >= self.number.len() {
            self.number.resize(i + 1, 0);
        }
        if self.number[i] == 0 {
            self.next += 1;
            self.number[i] = self.next;
        }
        let n = (self.number[i] - 1) as usize;
        if n / 64 >= bits.len() {
            bits.resize(n / 64 + 1, 0);
        }
        bits[n / 64] |= 1u64 << (n % 64);
    }

    /// A mirror of `set`, whose elements are all held.
    fn mirror<'a>(&mut self, set: impl IntoIterator<Item = &'a LocId>) -> Vec<u64> {
        let mut bits = Vec::new();
        for &loc in set {
            self.set(&mut bits, loc);
        }
        bits
    }
}

/// One destination's side of memory merges: its points-to set and its
/// merged sizes, each looked up once and only on first need — a merge that
/// finds nothing new creates neither, as a lone merge never did.
struct MergeInto<'a> {
    dst: PagNodeId,
    /// The whole map until `held` is looked up in it.
    pts: Option<&'a mut FxHashMap<PagNodeId, FxHashSet<LocId>>>,
    held: Option<&'a mut FxHashSet<LocId>>,
    /// The whole map until `sizes` is looked up in it.
    merge_sizes: Option<&'a mut FxHashMap<PagNodeId, FxHashMap<LocId, usize>>>,
    sizes: Option<&'a mut FxHashMap<LocId, usize>>,
    /// `dst`'s mirror as it stood before this step; what the step adds is
    /// found in `held`.
    bits: Option<&'a [u64]>,
    loc_bits: &'a LocBits,
}

impl MergeInto<'_> {
    /// `pts[dst]`, created if missing.
    fn held(&mut self) -> &mut FxHashSet<LocId> {
        if let Some(pts) = self.pts.take() {
            self.held = Some(pts.entry(self.dst).or_default());
        }
        self.held.as_deref_mut().expect("looked up above")
    }

    /// The sizes of `dst`'s merged cells, created if missing.
    fn sizes(&mut self) -> &mut FxHashMap<LocId, usize> {
        if let Some(merge_sizes) = self.merge_sizes.take() {
            self.sizes = Some(merge_sizes.entry(self.dst).or_default());
        }
        self.sizes.as_deref_mut().expect("looked up above")
    }

    /// Add to `pts[dst]`, and append to `fresh`, what `memory_pts[mem_loc]`
    /// gained since its last merge into `dst`.
    fn merge(&mut self, pag: &Pag, memory: &Memory<'_>, mem_loc: LocId, fresh: &mut Vec<LocId>) {
        let Some(mem) = memory.pts.get(&mem_loc) else {
            return;
        };
        let cur_len = mem.len();
        if cur_len == 0 {
            return;
        }
        let prev_len = {
            let seen = self.sizes().entry(mem_loc).or_insert(0);
            if cur_len <= *seen {
                return;
            }
            std::mem::replace(seen, cur_len)
        };
        // The cell's own filter is the same for every element it holds, so it
        // is read once here rather than once per element.
        let filter = FnFilter::of(memory.slot_guard.get(&mem_loc));
        let (bits, loc_bits) = (self.bits, self.loc_bits);
        let held = self.held();
        let start = fresh.len();
        // Collect first, insert second. Fusing the two — `pts.insert(loc)` in
        // place of `!pts.contains(&loc)` — looks equivalent and saves a hash
        // per added location, but it measurably reorders `pts` and changes the
        // exported `points_to` rows on the HDF corpus. Whatever the mechanism,
        // this pass is not the place to find out: keep the two passes.
        for i in prev_len..cur_len {
            let loc = mem[i];
            if filter.admits(memory.fn_arity, pag, loc)
                && !bits.is_some_and(|bits| loc_bits.test(bits, loc))
                && !held.contains(&loc)
            {
                fresh.push(loc);
            }
        }
        for &loc in &fresh[start..] {
            held.insert(loc);
        }
    }
}

/// What a memory merge reads of the solver state besides the destination.
struct Memory<'a> {
    pts: &'a FxHashMap<LocId, IndexSet<LocId, FxBuildHasher>>,
    slot_guard: &'a FxHashMap<LocId, SlotGuard>,
    fn_arity: &'a FxHashMap<LocId, usize>,
}

/// Insert `locs`, in order, into `memory_pts[cell]` (created if missing):
/// the one write path into cell memory. Returns whether the cell grew.
fn write_memory(
    memory_pts: &mut FxHashMap<LocId, IndexSet<LocId, FxBuildHasher>>,
    memory_bits: &mut FxHashMap<LocId, Vec<u64>>,
    loc_bits: &mut LocBits,
    cell: LocId,
    locs: impl IntoIterator<Item = LocId>,
) -> bool {
    let entry = memory_pts.entry(cell).or_default();
    let before = entry.len();
    match memory_bits.get_mut(&cell) {
        Some(bits) => {
            for loc in locs {
                if !loc_bits.test(bits, loc) {
                    entry.insert(loc);
                    loc_bits.set(bits, loc);
                }
            }
        }
        None => {
            for loc in locs {
                entry.insert(loc);
            }
            if entry.len() >= MIRROR_MIN {
                memory_bits.insert(cell, loc_bits.mirror(entry.iter()));
            }
        }
    }
    entry.len() > before
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
    /// `in_set_order`: a popped node's new locations, as a set.
    store_src_new: FxHashSet<LocId>,
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
    /// `register_synced_cell`: what a variable held when its cell was synced.
    held: Vec<LocId>,
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

    /// The guard's rule for a function value `loc`. Callers exempt
    /// non-function values first ([`FnFilter::admits`]).
    fn guard_admits(self, fn_arity: &FxHashMap<LocId, usize>, loc: LocId) -> bool {
        match self {
            Self::Any => true,
            Self::None => false,
            Self::Arity(n) => fn_arity.get(&loc).is_none_or(|p| *p == n),
        }
    }

    /// May `loc` be stored into, or loaded out of, a cell guarded by
    /// `self`? Function values are filtered; everything else flows
    /// unfiltered, which is what keeps both paths sound.
    fn admits(self, fn_arity: &FxHashMap<LocId, usize>, pag: &Pag, loc: LocId) -> bool {
        // `Any` answers before the `fn_for_loc` lookup: most cells are unguarded.
        self == Self::Any || fn_for_loc(pag, loc).is_none() || self.guard_admits(fn_arity, loc)
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
        indices: &SolverIndices,
    ) {
        // Collected into a scratch buffer in the set's own iteration order:
        // every write used to clone the whole holder set, which on hub
        // locations is thousands of nodes.
        holders.clear();
        if let Some(nodes) = self.loc_nodes.get(&loc) {
            holders.extend(nodes.iter().copied().filter(|&n| indices.is_load_src(n)));
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
    /// Filters exactly like the store path ([`FnFilter::admits`]): only
    /// function values are subject to the cell's guard.
    fn merge_memory_into_if_grown(
        &mut self,
        pag: &Pag,
        fresh: &mut Vec<LocId>,
        dst: PagNodeId,
        mem_loc: LocId,
    ) {
        fresh.clear();
        let (mut into, memory) = self.merge_into(dst);
        into.merge(pag, &memory, mem_loc, fresh);
        self.finish_merge(dst, fresh);
    }

    /// A load `dst = *p` stepping over `locs`, the locations `p` newly holds,
    /// in order: a function value is `dst`'s own, anything else contributes
    /// its cell's memory ([`Self::merge_memory_into_if_grown`]). One call per
    /// load rather than one per location, with the same effects in the same
    /// order: nothing a merge reads is written by the commit it defers.
    /// Returns the number of memory merges, for the `[solver]` stats line.
    fn load_into(
        &mut self,
        pag: &Pag,
        fresh: &mut Vec<LocId>,
        dst: PagNodeId,
        locs: &[LocId],
    ) -> u64 {
        fresh.clear();
        let mut merges = 0;
        let (mut into, memory) = self.merge_into(dst);
        for &loc in locs {
            if fn_for_loc(pag, loc).is_some() {
                if into.held().insert(loc) {
                    fresh.push(loc);
                }
            } else {
                merges += 1;
                into.merge(pag, &memory, loc, fresh);
            }
        }
        self.finish_merge(dst, fresh);
        merges
    }

    /// Split the state into `dst`'s side of a merge and what it reads.
    fn merge_into(&mut self, dst: PagNodeId) -> (MergeInto<'_>, Memory<'_>) {
        let Self {
            pts,
            pts_bits,
            loc_bits,
            memory_pts,
            merge_sizes,
            slot_guard,
            fn_arity,
            ..
        } = self;
        let into = MergeInto {
            dst,
            pts: Some(pts),
            held: None,
            merge_sizes: Some(merge_sizes),
            sizes: None,
            bits: pts_bits.get(&dst).map(Vec::as_slice),
            loc_bits,
        };
        let memory = Memory {
            pts: memory_pts,
            slot_guard,
            fn_arity,
        };
        (into, memory)
    }

    /// Mirror and commit what a merge added to `pts[dst]`.
    fn finish_merge(&mut self, dst: PagNodeId, fresh: &[LocId]) {
        if !fresh.is_empty() {
            self.mirror_pts(dst, fresh);
            self.commit_fresh(dst, fresh);
        }
    }

    /// Keep `pts[dst]`'s [`LocBits`] mirror in step with the `added`
    /// locations just inserted, creating it once the set is large.
    fn mirror_pts(&mut self, dst: PagNodeId, added: &[LocId]) {
        let Self {
            pts,
            pts_bits,
            loc_bits,
            ..
        } = self;
        if let Some(bits) = pts_bits.get_mut(&dst) {
            for &loc in added {
                loc_bits.set(bits, loc);
            }
        } else if let Some(set) = pts.get(&dst).filter(|set| set.len() >= MIRROR_MIN) {
            pts_bits.insert(dst, loc_bits.mirror(set));
        }
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
    let mut st = SolverState::default();
    // Lives beside the state, not in it: passing a buffer to the step that
    // needs it makes two steps sharing one a compile error.
    let mut scratch = Scratch::default();
    // A popped node's new locations in its set's order, shared by every store
    // it feeds: outside `scratch`, which each store borrows.
    let mut new_in_order: Vec<LocId> = Vec::new();
    // `UnwrapPointer` verdicts by (location type, receiver pointee type): one
    // hierarchy walk per pair instead of one per propagated location.
    let mut unwrap_memo: FxHashMap<(trace_ir::TypeId, trace_ir::TypeId), bool> =
        FxHashMap::default();
    // Per-location slot guards and per-function parameter counts for
    // signature-aware propagation.
    for loc in &pag.locations {
        if let Some(g) = slot_guard_for(program, loc.type_id) {
            st.slot_guard.insert(loc.id, g);
        }
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
        seed_addr_of(pag, program, &mut st, &mut scratch, c);
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
        if !seeds_own_location(program, var.id) {
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
            apply_fn_model(
                pag,
                program,
                &mut st,
                &mut scratch,
                cs,
                &f.name,
                models,
                &mut terminator_events,
            );
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
                                st.merge_memory_into_if_grown(
                                    pag,
                                    &mut scratch.fresh,
                                    dst,
                                    summary,
                                );
                            }
                        }
                    }
                }
            }
            continue;
        }
        sync_var_to_cell(pag, &mut st, &mut scratch.holders, node, &delta);

        if let Some(idxs) = pag.indices.copy_src.get(&node) {
            for &idx in idxs {
                let dst = pag.constraints[idx].dst;
                w_copy += delta.len() as u64;
                propagate_locs(&mut st, &mut scratch.fresh, dst, delta.iter().copied());
            }
        }

        if let Some(unwraps) = pag.indices.unwrap_src.get(&node) {
            for &(idx, pointee) in unwraps {
                let dst = pag.constraints[idx].dst;
                // Counted with copies in the `[solver]` stats line.
                w_copy += delta.len() as u64;
                let admitted = delta
                    .iter()
                    .copied()
                    .filter(|&loc| unwrap_admits(program, pag, &mut unwrap_memo, loc, pointee));
                propagate_locs(&mut st, &mut scratch.fresh, dst, admitted);
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
                w_load += st.load_into(pag, &mut scratch.fresh, dst, &delta);
            }
        }

        // Read by reference: `apply_store_to_targets` only needs `&Pag`, so
        // these index lists need no per-pop copy (the `gep` branch below
        // still does — it synthesizes locations through `&mut pag`).
        if !delta.is_empty() {
            if let Some(idxs) = pag.indices.store_dst.get(&node) {
                for &idx in idxs {
                    w_store += delta.len() as u64;
                    apply_store_to_targets(
                        pag,
                        program,
                        idx,
                        &mut st,
                        &mut scratch,
                        Some(delta.as_slice()),
                        StoreSource::Whole,
                    );
                }
            }

            if let Some(idxs) = pag.indices.store_src.get(&node) {
                // Every target already holds what the source held before
                // this pop, so only its new locations are written.
                in_set_order(
                    st.pts.get(&node),
                    &delta,
                    &mut scratch.store_src_new,
                    &mut new_in_order,
                );
                for &idx in idxs {
                    w_store += 1;
                    apply_store_to_targets(
                        pag,
                        program,
                        idx,
                        &mut st,
                        &mut scratch,
                        None,
                        StoreSource::New(&new_in_order),
                    );
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
                // A table's members pass once per step, not once per member.
                let mut table_members_passed = false;
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
                    // Function values are judged by the table-member and
                    // arity rules below, not by a struct field name.
                    if let Some(expected) = expected_name
                        .as_ref()
                        .filter(|_| fn_for_loc(pag, loc).is_none())
                    {
                        // A pointee with no struct type has no such field.
                        let Some(parent_type) = crate::pag::struct_type_for_loc(pag, program, loc)
                        else {
                            continue;
                        };
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
                                if !table_members_passed {
                                    for fl in fn_locs.iter().copied() {
                                        add_pts(&mut st, dst, fl);
                                    }
                                    table_members_passed = true;
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
                            st.merge_memory_into_if_grown(pag, &mut scratch.fresh, dst, fl);
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
                            st.merge_memory_into_if_grown(pag, &mut scratch.fresh, dst, summary);
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
                                        // `return &v` / `return fn` hand out an
                                        // address: seed it like the ones present
                                        // at solve start.
                                        for c in &pag.constraints[constraint_before..] {
                                            seed_addr_of(pag, program, &mut st, &mut scratch, c);
                                        }
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
                            program,
                            &mut st,
                            &mut scratch,
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
    pag.addressed_var(node)
        .and_then(|v| pag.var_node.get(&v).copied())
        .unwrap_or(node)
}

/// Apply a callee's function model at one resolved call site: attach
/// persistent PAG constraints between the actual-argument nodes so data
/// flows through bodyless callees, and record terminator events.
/// `ReturnAlias` / `ReturnHeap` are handled at PAG build time (they target
/// the `CallReturn` destination, not call arguments).
#[allow(clippy::too_many_arguments)]
fn apply_fn_model(
    pag: &mut Pag,
    program: &Program,
    st: &mut SolverState,
    scratch: &mut Scratch,
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
                        // Fire once with everything both sides hold now: a
                        // side that has settled pops with an empty delta and
                        // would never fire it. From here on, each side's
                        // growth fires it through the indices like any store.
                        apply_store_to_targets(
                            pag,
                            program,
                            idx,
                            st,
                            scratch,
                            None,
                            StoreSource::Whole,
                        );
                    }
                }
            }
            Effect::Clears { param } => {
                if arg_node(pag, *param).is_some() {
                    let event = (cs.id, *param);
                    if st.terminators_seen.insert(event) {
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

/// Whether an `UnwrapPointer` into a receiver whose pointee declaration is
/// `pointee` admits `loc`, by the table in `docs/ANALYSIS.md`,
/// "Smart-pointer unwrap".
fn unwrap_admits(
    program: &Program,
    pag: &Pag,
    memo: &mut FxHashMap<(trace_ir::TypeId, trace_ir::TypeId), bool>,
    loc: LocId,
    pointee: trace_ir::TypeId,
) -> bool {
    let loc = &pag.locations[loc.0 as usize];
    if matches!(loc.kind, LocKind::Function | LocKind::StringLit) {
        return false;
    }
    *memo
        .entry((loc.type_id, pointee))
        .or_insert_with(|| object_type_is_pointee(program, loc.type_id, pointee))
}

fn object_type_is_pointee(
    program: &Program,
    object: trace_ir::TypeId,
    pointee: trace_ir::TypeId,
) -> bool {
    use trace_ir::TypeDesc as TD;
    match program.types.get(object).desc.as_ref() {
        TD::Unknown | TD::Void => true,
        // Index-insensitive: an array's location stands for its elements.
        TD::Array { elem, .. } => {
            object_type_is_pointee(program, program.types.resolve_type_id(elem), pointee)
        }
        TD::Struct { name, .. } | TD::Union { name, .. } => {
            if program.types.tag_identity(object) == pointee {
                return true;
            }
            match program.types.get(pointee).desc.as_ref() {
                TD::Struct { name: base, .. } | TD::Union { name: base, .. } => {
                    program.derives_from(name, base)
                }
                // An unresolved pointee cannot rule an object out.
                _ => true,
            }
        }
        _ => false,
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
    // Two passes, as in `merge_memory_into_if_grown`, under one lookup.
    let entry = st.pts.entry(dst).or_default();
    let bits = st.pts_bits.get(&dst);
    for loc in locs {
        if !bits.is_some_and(|bits| st.loc_bits.test(bits, loc)) && !entry.contains(&loc) {
            fresh.push(loc);
        }
    }
    for &loc in fresh.iter() {
        entry.insert(loc);
    }
    st.mirror_pts(dst, fresh);
    st.commit_fresh(dst, fresh);
}

/// Which of a store's source locations [`apply_store_to_targets`] writes.
#[derive(Clone, Copy)]
enum StoreSource<'a> {
    /// The source's whole current set: for targets written for the first
    /// time, and a model store's firing when it is wired.
    Whole,
    /// Only these newly gained locations, in the source set's own order
    /// ([`in_set_order`]): every target already holds the rest (difference
    /// propagation on the value side).
    New(&'a [LocId]),
}

/// The members of `set` that `delta` names, in `set`'s iteration order: the
/// order a whole-set store writes them, so a store of only the new locations
/// fills each cell as a whole-set one would (docs/ANALYSIS.md, "Propagation
/// highlights").
fn in_set_order(
    set: Option<&FxHashSet<LocId>>,
    delta: &[LocId],
    members: &mut FxHashSet<LocId>,
    out: &mut Vec<LocId>,
) {
    out.clear();
    members.clear();
    members.extend(delta.iter().copied());
    if let Some(set) = set {
        out.extend(set.iter().copied().filter(|l| members.contains(l)));
    }
}

/// Store `*ptr = value`: write the value side's current points-to (plus its
/// own storage location for var nodes) into the memories of the given target
/// locations. `targets == None` means every location currently in the pointer
/// node's set; `Some(delta)` restricts writes to newly gained targets
/// (difference propagation — their memory is written for the first time).
/// `written` says which source locations go: the whole set, or only the
/// new ones, in the order the whole set would write them.
fn apply_store_to_targets(
    pag: &Pag,
    program: &Program,
    idx: usize,
    st: &mut SolverState,
    scratch: &mut Scratch,
    targets: Option<&[LocId]>,
    written: StoreSource,
) {
    let c = &pag.constraints[idx];
    let (src_node, dst_node) = (c.src, c.dst);
    // An object whose name denotes its storage (an aggregate, an array) is
    // stored by address, like the static seed; a pointer's name is only its
    // value, and `*out = &p` lowers to an explicit `AddrOfVar` temp
    // (docs/ANALYSIS.md, "Variable cells").
    let self_loc = match pag.nodes[src_node.0 as usize].kind {
        PagNodeKind::Var(v) if !var_is_pointer_like(program, v) => {
            pag.var_location.get(&v).copied()
        }
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
    match written {
        StoreSource::Whole => {
            if let Some(s) = st.pts.get(&src_node) {
                src.extend(s.iter().copied());
            }
        }
        StoreSource::New(locs) => src.extend_from_slice(locs),
    }
    let src_has_fns = src.iter().any(|&l| fn_for_loc(pag, l).is_some());
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
            changed |= write_memory(
                &mut st.memory_pts,
                &mut st.memory_bits,
                &mut st.loc_bits,
                loc,
                view.iter().copied().chain(self_loc),
            );
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
                changed |= write_memory(
                    &mut st.memory_pts,
                    &mut st.memory_bits,
                    &mut st.loc_bits,
                    summary,
                    view.iter().copied().chain(self_loc),
                );
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
            st.touch_loc_holders(&mut scratch.holders, loc, &pag.indices);
            sync_cell_to_var(pag, st, &mut scratch.fresh, loc);
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

/// The guard a memory cell of declared type `cell_type` imposes, if any.
fn slot_guard_for(program: &Program, cell_type: trace_ir::TypeId) -> Option<SlotGuard> {
    slot_guard_of(program.types.get(cell_type).desc.as_ref())
}

fn slot_guard_of(desc: &trace_ir::TypeDesc) -> Option<SlotGuard> {
    use trace_ir::TypeDesc as TD;
    let desc = match desc {
        // A fn-pointer *variable* lowers as `Ptr(FnPtr)`, a field as bare
        // `FnPtr`; both are fn-pointer slots (docs/ANALYSIS.md, "Variable
        // cells").
        TD::Ptr(inner) if matches!(inner.as_ref(), TD::FnPtr { .. }) => inner.as_ref(),
        // `void *` / an unknown pointer is untyped storage: it may hold any
        // address, a function's included.
        TD::Ptr(inner) if matches!(inner.as_ref(), TD::Void | TD::Unknown) => return None,
        // An array guards as its element does: a table of function pointers
        // takes their arity, a scalar array (`long v[4]`) is unguarded as a
        // scalar is, and a multi-dimensional one guards as its leaves do.
        TD::Array { elem, .. } => return slot_guard_of(elem),
        desc => desc,
    };
    match desc {
        TD::FnPtr { params, .. } if !params.is_empty() => Some(SlotGuard::FnParams(params.len())),
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
        | TD::Unknown
        | TD::FnPtr { .. } => None,
        TD::Struct { .. } | TD::Union { .. } | TD::Ptr(_) | TD::Array { .. } => {
            Some(SlotGuard::NotFnPtr)
        }
    }
}

/// Apply an `AddrOf` constraint: its destination points to the source
/// object, whose address is now taken. Other constraints are ignored.
///
/// The first time an object's address is taken it gets its slot guard, if the
/// solve-start pass did not give it one (a variable cell `expand_return_flows`
/// creates mid-solve for `return &v`), and a pointer variable's cell starts
/// being kept in step with the variable. Any location reaching this is
/// guarded; in practice `AddrOf` sources are variable and function locations
/// only (`ensure_var_loc`; member addresses lower to `Gep`), so lazily created
/// field cells, whose recorded type is not reliable enough to guard by, never
/// arrive here (docs/ANALYSIS.md, "Variable cells").
fn seed_addr_of(
    pag: &Pag,
    program: &Program,
    st: &mut SolverState,
    scratch: &mut Scratch,
    c: &Constraint,
) {
    let (ConstraintKind::AddrOf, PagNodeKind::Loc(loc)) =
        (c.kind, pag.nodes[c.src.0 as usize].kind)
    else {
        return;
    };
    if st.addr_taken.insert(loc) {
        if let std::collections::hash_map::Entry::Vacant(slot) = st.slot_guard.entry(loc) {
            if let Some(g) = slot_guard_for(program, pag.locations[loc.0 as usize].type_id) {
                slot.insert(g);
            }
        }
        register_synced_cell(pag, program, st, scratch, loc);
    }
    add_pts(st, c.dst, loc);
}

/// Does the solver start `var`'s node out holding `var`'s own location? A
/// static-storage object's name denotes its storage (an array decays to it);
/// a pointer's name denotes only the value it holds, and seeding it would
/// forge an address no `&G` produced (docs/ANALYSIS.md, "Variable cells").
fn seeds_own_location(program: &Program, var: VarId) -> bool {
    program.symbols.variable_by_id(var).is_some_and(|v| {
        matches!(
            v.storage,
            StorageClass::Global | StorageClass::FileStatic | StorageClass::FnStatic
        )
    }) && !var_is_pointer_like(program, var)
}

/// Does `var` hold a pointer? Such a variable's node and memory cell are one
/// value (docs/ANALYSIS.md, "Variable cells"). An array never does, whatever
/// its elements: its name denotes its storage. Deliberately wider than
/// [`var_may_hold_pointee`], which leaves out `int *` / `void *` to bound
/// interprocedural wiring: a buffer pointer's out-parameter still has to
/// reach its direct reads.
fn var_is_pointer_like(program: &Program, var: VarId) -> bool {
    program.symbols.variable_by_id(var).is_some_and(|v| {
        match program.types.get(v.type_id).desc.as_ref() {
            trace_ir::TypeDesc::Array { .. } => false,
            desc => v.is_pointer || desc.is_pointer_like(),
        }
    })
}

/// Start keeping `cell` in step with its variable's node, if it is a pointer
/// variable's own cell. Whatever each side already holds reaches the other:
/// the address may be taken long after the variable got its value.
fn register_synced_cell(
    pag: &Pag,
    program: &Program,
    st: &mut SolverState,
    scratch: &mut Scratch,
    cell: LocId,
) {
    let Some(v) = pag.locations[cell.0 as usize].var else {
        return;
    };
    // Field cells and summaries also carry `var`: only the variable's own
    // cell is synced.
    if pag.var_location.get(&v) != Some(&cell) || !var_is_pointer_like(program, v) {
        return;
    }
    let Some(&node) = pag.var_node.get(&v) else {
        return;
    };
    st.synced_cell.insert(node, cell);
    st.cell_owner.insert(cell, node);
    scratch.held.clear();
    if let Some(held) = st.pts.get(&node) {
        scratch.held.extend(held.iter().copied());
    }
    // Into the ordered cell in a fixed order, not the hash set's.
    scratch.held.sort_unstable();
    write_to_cell(pag, st, &mut scratch.holders, cell, &scratch.held);
    st.merge_memory_into_if_grown(pag, &mut scratch.fresh, node, cell);
}

/// Write `locs` into `cell`, filtered like a store, and requeue its loaders
/// if it grew.
fn write_to_cell(
    pag: &Pag,
    st: &mut SolverState,
    holders: &mut Vec<PagNodeId>,
    cell: LocId,
    locs: &[LocId],
) {
    let filter = FnFilter::of(st.slot_guard.get(&cell));
    let grew = {
        let SolverState {
            memory_pts,
            memory_bits,
            loc_bits,
            fn_arity,
            ..
        } = &mut *st;
        write_memory(
            memory_pts,
            memory_bits,
            loc_bits,
            cell,
            locs.iter()
                .copied()
                .filter(|&l| filter.admits(fn_arity, pag, l)),
        )
    };
    if grew {
        st.touch_loc_holders(holders, cell, &pag.indices);
    }
}

/// Node → cell half of the variable-cell rule (docs/ANALYSIS.md, "Variable
/// cells"): what a variable's node gains is written into its memory cell.
fn sync_var_to_cell(
    pag: &Pag,
    st: &mut SolverState,
    holders: &mut Vec<PagNodeId>,
    node: PagNodeId,
    delta: &[LocId],
) {
    if let Some(&cell) = st.synced_cell.get(&node) {
        write_to_cell(pag, st, holders, cell, delta);
    }
}

/// Cell → node half of the variable-cell rule: a store that grew a
/// variable's cell makes the variable itself see the new contents.
fn sync_cell_to_var(pag: &Pag, st: &mut SolverState, fresh: &mut Vec<LocId>, cell: LocId) {
    if let Some(&node) = st.cell_owner.get(&cell) {
        st.merge_memory_into_if_grown(pag, fresh, node, cell);
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
        // A pointer to a fn-pointer slot (`cb_t *out`, a table row) reaches
        // callbacks through its pointee just as a struct pointer does.
        TD::Ptr(inner) => match inner.as_ref() {
            TD::FnPtr { .. } | TD::Unknown | TD::Struct { .. } | TD::Union { .. } => true,
            TD::Ptr(slot) => matches!(slot.as_ref(), TD::FnPtr { .. }),
            _ => false,
        },
        // Pointer-flagged variable whose recorded shape is not a pointer: a
        // synthesized temp typed `int`, or a smart-pointer variable (its
        // value is its pointee's address): participate conservatively.
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
        st.mirror_pts(node, &[loc]);
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
    pag: &Pag,
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
                if let Some(actual) = pag.argument_var(cs, idx) {
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
    use trace_ir::{FlowConstraint, LocId, TypeDesc, TypeId};

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

    /// Issue #127: a load through a guarded cell lifts every data value and
    /// only the function values the guard admits — and keeps doing so when
    /// the cell grows between merges.
    #[test]
    fn merge_admits_data_and_filters_functions() {
        // store_fixture: locs 0..3 are functions (0: arity 1, 1: arity 2,
        // 2: unknown arity); 3..5 are data. Cell ids only index `memory_pts`
        // and `slot_guard`, never `pag.locations`, so they may lie past 5.
        let (pag, fn_arity, _) = store_fixture();
        let (not_fn_cell, arity2_cell) = (LocId(10), LocId(11));
        let (dst_a, dst_b) = (PagNodeId(0), PagNodeId(1));
        let mut st = SolverState {
            fn_arity,
            ..Default::default()
        };
        st.slot_guard.insert(not_fn_cell, SlotGuard::NotFnPtr);
        st.slot_guard.insert(arity2_cell, SlotGuard::FnParams(2));
        let mut fresh = Vec::new();
        let pts = |st: &SolverState, n: PagNodeId| {
            let mut v: Vec<u32> = st.pts.get(&n).into_iter().flatten().map(|l| l.0).collect();
            v.sort();
            v
        };

        // NotFnPtr cell: data passes, every function value is rejected.
        st.memory_pts
            .entry(not_fn_cell)
            .or_default()
            .extend([LocId(3), LocId(0)]);
        st.merge_memory_into_if_grown(&pag, &mut fresh, dst_a, not_fn_cell);
        assert_eq!(pts(&st, dst_a), [3]);

        // The cell grows: the second merge lifts only the new data value.
        st.memory_pts
            .entry(not_fn_cell)
            .or_default()
            .extend([LocId(1), LocId(4)]);
        st.merge_memory_into_if_grown(&pag, &mut fresh, dst_a, not_fn_cell);
        assert_eq!(pts(&st, dst_a), [3, 4]);

        // FnParams(2) cell: data, same-arity and unknown-arity functions pass;
        // the arity-1 function is rejected.
        st.memory_pts.entry(arity2_cell).or_default().extend([
            LocId(0),
            LocId(1),
            LocId(2),
            LocId(3),
        ]);
        st.merge_memory_into_if_grown(&pag, &mut fresh, dst_b, arity2_cell);
        assert_eq!(pts(&st, dst_b), [1, 2, 3]);
    }

    /// A `LocBits` mirror says "held" only for what was set in it, under a
    /// numbering shared by every mirror.
    #[test]
    fn loc_bits_answer_only_what_was_set() {
        let mut numbering = LocBits::default();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        assert!(!numbering.test(&a, LocId(7)));
        numbering.set(&mut a, LocId(7));
        numbering.set(&mut b, LocId(300));
        numbering.set(&mut b, LocId(7));
        assert!(numbering.test(&a, LocId(7)));
        assert!(!numbering.test(&a, LocId(300)));
        assert!(numbering.test(&b, LocId(300)) && numbering.test(&b, LocId(7)));
        assert!(!numbering.test(&b, LocId(8)));
        // Dense: two locations numbered, whatever their ids.
        assert_eq!(numbering.next, 2);
        let mirrored = numbering.mirror(&[LocId(8), LocId(7)]);
        assert!(numbering.test(&mirrored, LocId(8)) && numbering.test(&mirrored, LocId(7)));
        assert!(!numbering.test(&mirrored, LocId(300)));
    }

    /// A merge tests the destination's mirror as it stood before the step,
    /// so what the step itself adds has a clear bit and is found in the set:
    /// a location two cells share is added once, whether the destination is
    /// mirrored from the start or crosses [`MIRROR_MIN`] during the step.
    #[test]
    fn a_clear_bit_falls_back_to_the_set_within_a_step() {
        let mut pag = Pag::default();
        for i in 0..300u32 {
            pag.locations.push(loc(i, false));
        }
        let (cell_a, cell_b) = (LocId(0), LocId(1));
        for (held_before, mirrored_before) in [(MIRROR_MIN as u32 - 1, false), (100, true)] {
            let dst = PagNodeId(3);
            let mut st = SolverState::default();
            for l in 200..200 + held_before {
                add_pts(&mut st, dst, LocId(l));
            }
            assert_eq!(st.pts_bits.contains_key(&dst), mirrored_before);
            st.delta.clear();
            st.delta_pending.clear();
            {
                let SolverState {
                    memory_pts,
                    memory_bits,
                    loc_bits,
                    ..
                } = &mut st;
                // Both cells hold 10..20 and a location `dst` already
                // holds; `cell_b` also 20..40.
                let a = (10..20).chain([200]).map(LocId);
                let b = (10..40).chain([200]).map(LocId);
                write_memory(memory_pts, memory_bits, loc_bits, cell_a, a);
                write_memory(memory_pts, memory_bits, loc_bits, cell_b, b);
            }
            let mut fresh = Vec::new();
            st.load_into(&pag, &mut fresh, dst, &[cell_a, cell_b]);
            let added: Vec<u32> = fresh.iter().map(|l| l.0).collect();
            assert_eq!(added, (10..40).collect::<Vec<_>>(), "added once each");
            assert_eq!(st.delta[&dst], fresh);
            assert_eq!(st.pts[&dst].len(), held_before as usize + 30);
            let bits = &st.pts_bits[&dst];
            assert!(
                st.pts[&dst].iter().all(|&l| st.loc_bits.test(bits, l)),
                "the mirror holds the whole set after the step"
            );
        }
    }

    /// `load_into` steps over a load's new locations in one call and must
    /// leave exactly what one `add_pts` / `merge_memory_into_if_grown` per
    /// location leaves: the same set in the same iteration order, the same
    /// delta order, the same holders in the same order, and the same
    /// worklist. Sets large enough to be mirrored are included, the cells
    /// grow between the two steps, and `cell_b` is slot-guarded
    /// (`NotFnPtr`), so the function values it holds are not lifted.
    #[test]
    fn load_into_matches_one_step_per_location() {
        // Locations 0..3 are functions, the rest data.
        let mut pag = Pag::default();
        for i in 0..400u32 {
            pag.locations.push(loc(i, i < 3));
        }
        let (cell_a, cell_b) = (LocId(3), LocId(4));
        // Two loads share the cells, so each location has several holders.
        let dsts = [PagNodeId(9), PagNodeId(5)];
        let fill = |st: &mut SolverState, cell: LocId, range: std::ops::Range<u32>| {
            let SolverState {
                memory_pts,
                memory_bits,
                loc_bits,
                ..
            } = st;
            write_memory(memory_pts, memory_bits, loc_bits, cell, range.map(LocId));
        };
        let (mut stepped, mut batched) = (SolverState::default(), SolverState::default());
        for st in [&mut stepped, &mut batched] {
            st.slot_guard.insert(cell_b, SlotGuard::NotFnPtr);
        }
        let mut fresh = Vec::new();
        let delta = [LocId(0), cell_a, LocId(1), cell_b, LocId(0)];
        for (grow_a, grow_b) in [(10..150, 100..220), (150..260, 0..30)] {
            for st in [&mut stepped, &mut batched] {
                fill(st, cell_a, grow_a.clone());
                fill(st, cell_b, grow_b.clone());
            }
            for dst in dsts {
                for &l in &delta {
                    if fn_for_loc(&pag, l).is_some() {
                        add_pts(&mut stepped, dst, l);
                    } else {
                        stepped.merge_memory_into_if_grown(&pag, &mut fresh, dst, l);
                    }
                }
                let merges = batched.load_into(&pag, &mut fresh, dst, &delta);
                assert_eq!(merges, 2);
            }
            for dst in dsts {
                let order = |st: &SolverState| st.pts[&dst].iter().copied().collect::<Vec<_>>();
                assert_eq!(order(&stepped), order(&batched));
                assert_eq!(stepped.delta[&dst], batched.delta[&dst]);
            }
            assert_eq!(stepped.worklist, batched.worklist);
            let holders =
                |st: &SolverState, l: LocId| st.loc_nodes[&l].iter().copied().collect::<Vec<_>>();
            for l in stepped.pts[&dsts[0]].iter().copied() {
                assert_eq!(
                    holders(&stepped, l),
                    holders(&batched, l),
                    "holders of {l:?}"
                );
                assert_eq!(holders(&batched, l).len(), 2, "holders of {l:?}");
            }
        }
        for dst in dsts {
            assert!(
                batched.pts_bits.contains_key(&dst),
                "the destination is mirrored"
            );
            // Functions 0 and 1, and data 3..260; the guarded cell's
            // function 2 is not lifted.
            assert_eq!(batched.pts[&dst].len(), 2 + 257);
            assert!(!batched.pts[&dst].contains(&LocId(2)));
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

    /// An array guards as its element does: a scalar array (`long v[4]`, a
    /// struct's array field typed `Array { elem }`) is unguarded, as a scalar
    /// is, so a function value cast into it is kept; a table of function
    /// pointers keeps its arity guard and an array of structs stays
    /// `NotFnPtr`.
    #[test]
    fn array_slots_guard_as_their_elements_do() {
        use trace_ir::TypeDesc as TD;
        let array = |elem: TD| TD::Array {
            elem: Box::new(elem),
            size: Some(4),
        };
        let fn_ptr = |arity: usize| TD::FnPtr {
            ret: Box::new(TD::Void),
            params: vec![TD::Int; arity],
        };
        for scalar in [TD::Char, TD::Long, TD::SizeT] {
            let desc = array(scalar);
            assert!(slot_guard_of(&desc).is_none(), "{desc:?}");
            let nested = array(desc);
            assert!(slot_guard_of(&nested).is_none(), "{nested:?}");
        }
        assert!(matches!(
            slot_guard_of(&array(fn_ptr(2))),
            Some(SlotGuard::FnParams(2))
        ));
        assert!(matches!(
            slot_guard_of(&array(TD::Ptr(Box::new(fn_ptr(1))))),
            Some(SlotGuard::FnParams(1))
        ));
        assert!(matches!(
            slot_guard_of(&array(TD::Ptr(Box::new(TD::Int)))),
            Some(SlotGuard::NotFnPtr)
        ));
        let s = TD::Struct {
            name: "S".into(),
            fields: Vec::new(),
        };
        assert!(matches!(
            slot_guard_of(&array(s)),
            Some(SlotGuard::NotFnPtr)
        ));
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

    // --- `UnwrapPointer` (docs/ANALYSIS.md, "Smart-pointer unwrap") ---

    /// A program whose `sp` (a `shared_ptr<Payload>` value) feeds a
    /// `Payload`-typed receiver through `UnwrapPointer`.
    struct UnwrapFixture {
        program: Program,
        payload: TypeId,
        wrapper: TypeId,
        sp: VarId,
        recv: VarId,
    }

    impl UnwrapFixture {
        fn new() -> Self {
            let mut program = Program::new(".".into());
            let payload = program.types.intern(TypeDesc::Struct {
                name: "Payload".into(),
                fields: vec![("value".into(), TypeDesc::Int)],
            });
            let wrapper = program.types.intern(TypeDesc::Struct {
                name: "std::shared_ptr<Payload>".into(),
                fields: Vec::new(),
            });
            let sp = add_var(&mut program, "sp", wrapper);
            let recv = add_var(&mut program, "_recv", payload);
            UnwrapFixture {
                program,
                payload,
                wrapper,
                sp,
                recv,
            }
        }

        /// `sp` gains `&obj` for a new object typed `type_id`.
        fn point_sp_at(&mut self, name: &str, type_id: TypeId) -> VarId {
            let obj = add_var(&mut self.program, name, type_id);
            self.program.flow.push(FlowConstraint::AddrOfVar {
                dst: self.sp,
                src: obj,
            });
            obj
        }

        fn unwrap(&mut self) {
            self.program.flow.push(FlowConstraint::UnwrapPointer {
                dst: self.recv,
                src: self.sp,
            });
        }

        /// Names of the locations the receiver points to, sorted.
        fn receiver_pointees(&self) -> Vec<String> {
            pointee_names(&self.program, self.recv)
        }
    }

    fn add_var(program: &mut Program, name: &str, type_id: TypeId) -> VarId {
        let id = program.symbols.alloc_var_id();
        program.symbols.add_variable(trace_ir::Variable {
            id,
            name: name.into(),
            type_id,
            storage: trace_ir::StorageClass::Local,
            fn_id: None,
            param_index: None,
            span: trace_ir::Span::new(trace_ir::FileId(0), 1, 1),
            is_pointer: false,
            is_defined: true,
            is_weak: false,
            target: None,
            is_namespaced: false,
            qualified_name: None,
            c_linkage: false,
        });
        id
    }

    fn pointee_names(program: &Program, var: VarId) -> Vec<String> {
        let opts = AnalyzeOptions {
            retain_points_to: true,
            ..AnalyzeOptions::default()
        };
        let (pag, result) = analyze_with_options(program, opts);
        let mut names: Vec<String> = result
            .points_to
            .get(&pag.var_node[&var])
            .into_iter()
            .flatten()
            .map(|loc| pag.locations[loc.0 as usize].desc.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn unwrap_admits_pointee_derived_and_untyped_objects() {
        let mut fx = UnwrapFixture::new();
        let derived = fx.program.types.intern(TypeDesc::Struct {
            name: "Derived".into(),
            fields: vec![("value".into(), TypeDesc::Int)],
        });
        fx.program.add_inheritance("Derived", "Payload");
        let unknown = fx.program.types.unknown();
        let void = fx.program.types.intern(TypeDesc::Void);
        let payload = fx.payload;
        fx.point_sp_at("payload_obj", payload);
        fx.point_sp_at("derived_obj", derived);
        fx.point_sp_at("unknown_obj", unknown);
        fx.point_sp_at("void_obj", void);
        fx.unwrap();
        assert_eq!(
            fx.receiver_pointees(),
            ["derived_obj", "payload_obj", "unknown_obj", "void_obj"]
        );
    }

    #[test]
    fn unwrap_into_an_unresolved_pointee_admits_every_object() {
        // May-analysis: with no pointee class to compare against, an object
        // cannot be ruled out; storage that is no object still is.
        let mut program = Program::new(".".into());
        let unknown = program.types.unknown();
        let payload = program.types.intern(TypeDesc::Struct {
            name: "Payload".into(),
            fields: vec![("value".into(), TypeDesc::Int)],
        });
        let int = program.types.int();
        let sp = add_var(&mut program, "sp", unknown);
        let recv = add_var(&mut program, "_recv", unknown);
        for (name, type_id) in [("payload_obj", payload), ("int_obj", int)] {
            let obj = add_var(&mut program, name, type_id);
            program
                .flow
                .push(FlowConstraint::AddrOfVar { dst: sp, src: obj });
        }
        program
            .flow
            .push(FlowConstraint::UnwrapPointer { dst: recv, src: sp });
        assert_eq!(pointee_names(&program, recv), ["payload_obj"]);
    }

    #[test]
    fn unwrap_admits_an_array_of_the_pointee() {
        // Index-insensitive: `&pool[i]` is the array's own location.
        let mut fx = UnwrapFixture::new();
        let pool = fx.program.types.intern(TypeDesc::Array {
            elem: Box::new(TypeDesc::Struct {
                name: "Payload".into(),
                fields: vec![("value".into(), TypeDesc::Int)],
            }),
            size: Some(4),
        });
        fx.point_sp_at("pool", pool);
        fx.unwrap();
        assert_eq!(fx.receiver_pointees(), ["pool"]);
    }

    #[test]
    fn unwrap_rejects_wrapper_unrelated_pointer_and_non_object_locations() {
        let mut fx = UnwrapFixture::new();
        let other = fx.program.types.intern(TypeDesc::Struct {
            name: "Other".into(),
            // Same member name and position as `Payload`: the GEP guard
            // alone would not tell them apart.
            fields: vec![("value".into(), TypeDesc::Int)],
        });
        let payload_ptr = fx
            .program
            .types
            .intern(TypeDesc::Ptr(Box::new(TypeDesc::Struct {
                name: "Payload".into(),
                fields: vec![("value".into(), TypeDesc::Int)],
            })));
        let int = fx.program.types.int();
        let wrapper = fx.wrapper;
        fx.point_sp_at("wrapper_obj", wrapper);
        fx.point_sp_at("other_obj", other);
        fx.point_sp_at("pointer_cell", payload_ptr);
        fx.point_sp_at("int_obj", int);
        let sp = fx.sp;
        fx.program.flow.push(FlowConstraint::StringConst {
            dst: sp,
            value: "literal".into(),
        });
        let file = fx.program.symbols.add_file(".".into());
        let callee = fx.program.symbols.alloc_fn_id();
        fx.program
            .symbols
            .push_synthetic_function(trace_ir::Function {
                is_weak: false,
                target: None,
                id: callee,
                name: "Handler".into(),
                linkage: trace_ir::Linkage::External,
                return_type: TypeId(0),
                params: Vec::new(),
                locals: Vec::new(),
                span: trace_ir::Span::new(file, 1, 1),
                end_line: 1,
                file,
                is_defined: true,
                param_type_ids: Vec::new(),
                explicit_arity: None,
                default_args: 0,
                reference_params: Vec::new(),
                owner_unresolved: false,
                variadic: false,
                defaulted_in_class: false,
                declared_in_class: false,
                is_virtual: false,
                is_final: false,
                is_cpp: true,
                tu: None,
            });
        fx.program
            .flow
            .push(FlowConstraint::AddrOfFn { dst: sp, callee });
        fx.unwrap();
        let sp_pointees = pointee_names(&fx.program, sp);
        assert_eq!(sp_pointees.len(), 6, "every source location reached sp");
        assert_eq!(fx.receiver_pointees(), Vec::<String>::new());
    }

    #[test]
    fn unwrap_rejects_wrapper_typed_field_cells() {
        // `h.item` is a `shared_ptr<Payload>` member: its field cell and its
        // summary are wrapper storage, not a `Payload`.
        let mut fx = UnwrapFixture::new();
        let holder = fx.program.types.intern(TypeDesc::Struct {
            name: "Holder".into(),
            fields: vec![(
                "item".into(),
                TypeDesc::Struct {
                    name: "std::shared_ptr<Payload>".into(),
                    fields: Vec::new(),
                },
            )],
        });
        let item = fx
            .program
            .types
            .field_id_by_name(holder, "item")
            .expect("Holder.item");
        let holder_obj = add_var(&mut fx.program, "holder_obj", holder);
        let untyped = fx.program.types.unknown();
        let holder_ptr = add_var(&mut fx.program, "hp", untyped);
        fx.program.flow.push(FlowConstraint::AddrOfVar {
            dst: holder_ptr,
            src: holder_obj,
        });
        let sp = fx.sp;
        fx.program.flow.push(FlowConstraint::GepField {
            dst: sp,
            base: holder_ptr,
            field: item,
            field_name: "item".into(),
        });
        fx.unwrap();
        assert!(
            !pointee_names(&fx.program, sp).is_empty(),
            "the field cell reached sp"
        );
        assert_eq!(fx.receiver_pointees(), Vec::<String>::new());
    }

    #[test]
    fn unwrap_forwards_locations_arriving_on_later_iterations() {
        // The unwrap is built before the copies that feed its source.
        let mut fx = UnwrapFixture::new();
        fx.unwrap();
        let payload = fx.payload;
        let first = add_var(&mut fx.program, "first", fx.wrapper);
        let second = add_var(&mut fx.program, "second", fx.wrapper);
        let obj = add_var(&mut fx.program, "late_obj", payload);
        let sp = fx.sp;
        fx.program.flow.extend([
            FlowConstraint::Copy {
                dst: sp,
                src: second,
            },
            FlowConstraint::Copy {
                dst: second,
                src: first,
            },
            FlowConstraint::AddrOfVar {
                dst: first,
                src: obj,
            },
        ]);
        assert_eq!(fx.receiver_pointees(), ["late_obj"]);
    }

    #[test]
    fn unwrapped_receiver_without_pointees_keeps_the_field_summary() {
        // Regression guard (passes before `UnwrapPointer` propagates): an
        // empty `pts(sp)` still reads `Payload.value` through the summary.
        let mut fx = UnwrapFixture::new();
        fx.unwrap();
        let value = fx
            .program
            .types
            .field_id_by_name(fx.payload, "value")
            .expect("Payload.value");
        let int = fx.program.types.int();
        let gep = add_var(&mut fx.program, "gep", int);
        let recv = fx.recv;
        fx.program.flow.push(FlowConstraint::GepField {
            dst: gep,
            base: recv,
            field: value,
            field_name: "value".into(),
        });
        assert_eq!(pointee_names(&fx.program, gep), ["summary:Payload.value"]);
    }
}
