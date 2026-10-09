//! Execution contexts: the places where a thread, a task or an IPC request
//! starts running code, with the evidence that one may run more than once at
//! a time (`docs/ANALYSIS.md`, "Execution contexts").
//!
//! Read once off the converged call graph, like the callback edges the
//! contexts start at: nothing here feeds the solver.

use crate::constraints::{CallGraphEdge, ResolutionKind};
use crate::summaries::ContextKind;
use rustc_hash::{FxHashMap, FxHashSet};
use trace_ir::{CallSiteId, FnId, Program};

/// One place where an execution context starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionContext {
    pub kind: ContextKind,
    /// The function the context starts running.
    pub entry: FnId,
    /// The submission that starts it; `None` for an IPC stub handler, which
    /// no call site in the program starts.
    pub start: Option<ContextStart>,
    pub multi_instance: MultiInstance,
    /// Two instances may run at the same time: every IPC handler, and a
    /// context with multi-instance evidence. `false` is the absence of that
    /// evidence, not a proof of single execution.
    pub self_concurrent: bool,
}

/// The call that hands a callback to a modelled callee (`invoke`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextStart {
    pub call_site: CallSiteId,
    /// The modelled callee that runs the callback (`pthread_create`).
    pub api: FnId,
    /// The callee parameter the callback is passed in, counting explicit
    /// arguments as the model does.
    pub param: u32,
}

/// Why a context may have more than one instance. The first that applies,
/// in this order, is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiInstance {
    /// The start site is lexically inside a loop of its function.
    Loop,
    /// The starting function is in a call-graph cycle of ordinary calls:
    /// an edge from a start site to what it starts, or an IPC bridge, does
    /// not close one.
    Cycle,
    /// The starting function is reachable from the entry of a context that
    /// may itself run more than once at a time.
    Parent,
    /// None of these: a start site reached by several ordinary calls is still
    /// possible.
    Unknown,
}

impl MultiInstance {
    /// Every value, in the order evidence takes precedence: the first that
    /// applies is recorded.
    const PRECEDENCE: [Self; 4] = [Self::Loop, Self::Cycle, Self::Parent, Self::Unknown];

    /// The value spelled `spelling` in `execution_contexts.multi_instance`.
    pub fn parse(spelling: &str) -> Option<Self> {
        Self::PRECEDENCE
            .into_iter()
            .find(|value| value.as_str() == spelling)
    }

    /// Where this evidence stands in the order it takes precedence, which is
    /// the order the variants are declared in: lower wins.
    pub fn rank(self) -> usize {
        self as usize
    }

    /// The spelling in `execution_contexts.multi_instance`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::Cycle => "cycle",
            Self::Parent => "parent",
            Self::Unknown => "unknown",
        }
    }
}

/// A callback a modelled callee runs at a call site, as the solver resolved
/// it: the start, the callback's function, and the kind the model states.
pub(crate) type Invocation = (ContextStart, FnId, ContextKind);

/// One context per resolved callback invocation, in start-site order, then
/// one per IPC stub handler in `FnId` order.
pub(crate) fn execution_contexts(
    program: &Program,
    mut invocations: Vec<Invocation>,
    ipc_handlers: impl Iterator<Item = FnId>,
    call_edges: &[CallGraphEdge],
) -> Vec<ExecutionContext> {
    // Ordered by kind spelling, as `trace-merge` orders its rows.
    invocations.sort_unstable_by(|a, b| (a.0, a.1, a.2.as_str()).cmp(&(b.0, b.1, b.2.as_str())));
    invocations.dedup();
    let mut handlers: Vec<FnId> = ipc_handlers.collect();
    handlers.sort_unstable();
    handlers.dedup();
    if invocations.is_empty() && handlers.is_empty() {
        return Vec::new();
    }

    let starts: FxHashSet<(CallSiteId, FnId)> = invocations
        .iter()
        .map(|(start, entry, _)| (start.call_site, *entry))
        .collect();
    let graph = Graph::new(call_edges, |edge| {
        edge.resolution == ResolutionKind::IpcBridge
            || starts.contains(&(edge.call_site, edge.callee))
    });
    let cyclic = graph.cyclic();
    let submitter = |start: &ContextStart| {
        program
            .symbols
            .call_site_by_id(start.call_site)
            .map(|site| (site.caller, site.in_loop))
    };
    let mut contexts: Vec<ExecutionContext> = invocations
        .into_iter()
        .map(|(start, entry, kind)| {
            let multi_instance = match submitter(&start) {
                Some((_, true)) => MultiInstance::Loop,
                Some((caller, false)) if graph.index(caller).is_some_and(|i| cyclic[i]) => {
                    MultiInstance::Cycle
                }
                _ => MultiInstance::Unknown,
            };
            ExecutionContext {
                kind,
                entry,
                start: Some(start),
                multi_instance,
                // A request handler runs on the IPC worker pool whether a
                // stub or a model says so.
                self_concurrent: kind == ContextKind::IpcHandler
                    || multi_instance != MultiInstance::Unknown,
            }
        })
        .collect();
    contexts.extend(handlers.into_iter().map(|entry| ExecutionContext {
        kind: ContextKind::IpcHandler,
        entry,
        start: None,
        multi_instance: MultiInstance::Unknown,
        self_concurrent: true,
    }));

    // Everything a self-concurrent context reaches may run more than once at
    // a time, and so may every context started from there. The call graph
    // holds the edge from each start site to its callback, so one search
    // from the self-concurrent entries reaches the nested ones too.
    let roots = contexts
        .iter()
        .filter(|c| c.self_concurrent)
        .map(|c| c.entry);
    let reached = graph.reachable_from(roots);
    for context in &mut contexts {
        if context.self_concurrent {
            continue;
        }
        let under_parent = context
            .start
            .as_ref()
            .and_then(submitter)
            .and_then(|(caller, _)| graph.index(caller))
            .is_some_and(|i| reached[i]);
        if under_parent {
            context.multi_instance = MultiInstance::Parent;
            context.self_concurrent = true;
        }
    }
    contexts
}

/// The call graph over dense node indexes, in the order functions first
/// appear in the edge list: every edge, and the ordinary calls alone.
struct Graph {
    index: FxHashMap<FnId, usize>,
    successors: Vec<Vec<usize>>,
    calls: Vec<Vec<usize>>,
}

impl Graph {
    /// `starts` tells an edge that starts an execution context from an
    /// ordinary call.
    fn new(edges: &[CallGraphEdge], starts: impl Fn(&CallGraphEdge) -> bool) -> Self {
        let mut graph = Self {
            index: FxHashMap::default(),
            successors: Vec::new(),
            calls: Vec::new(),
        };
        let mut any = FxHashSet::default();
        let mut ordinary = FxHashSet::default();
        for edge in edges {
            let from = graph.node(edge.caller);
            let to = graph.node(edge.callee);
            if any.insert((from, to)) {
                graph.successors[from].push(to);
            }
            if !starts(edge) && ordinary.insert((from, to)) {
                graph.calls[from].push(to);
            }
        }
        graph
    }

    fn node(&mut self, f: FnId) -> usize {
        let next = self.successors.len();
        let index = *self.index.entry(f).or_insert(next);
        if index == next {
            self.successors.push(Vec::new());
            self.calls.push(Vec::new());
        }
        index
    }

    fn index(&self, f: FnId) -> Option<usize> {
        self.index.get(&f).copied()
    }

    /// Per node: whether it lies on a cycle of ordinary calls (a strongly
    /// connected component of more than one node, or a node calling itself).
    fn cyclic(&self) -> Vec<bool> {
        let component = strongly_connected_components(&self.calls);
        let mut size = vec![0usize; self.calls.len()];
        for &c in &component {
            size[c] += 1;
        }
        component
            .iter()
            .enumerate()
            .map(|(v, &c)| size[c] > 1 || self.calls[v].contains(&v))
            .collect()
    }

    /// Per node: whether it is reachable from `roots` over every edge, the
    /// roots included.
    fn reachable_from(&self, roots: impl Iterator<Item = FnId>) -> Vec<bool> {
        let mut reached = vec![false; self.successors.len()];
        let mut work: Vec<usize> = roots.filter_map(|f| self.index(f)).collect();
        for &r in &work {
            reached[r] = true;
        }
        while let Some(v) = work.pop() {
            for &w in &self.successors[v] {
                if !reached[w] {
                    reached[w] = true;
                    work.push(w);
                }
            }
        }
        reached
    }
}

/// The strongly connected component of each node of the graph whose
/// successor lists are `succ`, as a dense id per node. Tarjan's algorithm,
/// iterative so a deep chain cannot overflow the stack: nodes are visited in
/// index order and successors in list order, so the ids are deterministic,
/// and a component's id is smaller than that of every component reaching it
/// (reverse topological order).
pub fn strongly_connected_components(succ: &[Vec<usize>]) -> Vec<usize> {
    const UNSEEN: usize = usize::MAX;
    let n = succ.len();
    let mut order = vec![UNSEEN; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut component = vec![UNSEEN; n];
    let mut stack = Vec::new();
    let mut next = 0;
    let mut components = 0;
    for root in 0..n {
        if order[root] != UNSEEN {
            continue;
        }
        // (node, position of the next successor to visit)
        let mut frames = vec![(root, 0)];
        order[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut pos)) = frames.last_mut() {
            if let Some(&w) = succ[v].get(*pos) {
                *pos += 1;
                if order[w] == UNSEEN {
                    order[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    frames.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(order[w]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == order[v] {
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component[w] = components;
                    if w == v {
                        break;
                    }
                }
                components += 1;
            }
        }
    }
    component
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Edges `(caller, callee)`; those into `started` start a context.
    fn graph_with_starts(edges: &[(u32, u32)], started: &[u32]) -> Graph {
        let edges: Vec<CallGraphEdge> = edges
            .iter()
            .map(|&(a, b)| CallGraphEdge {
                call_site: CallSiteId(0),
                caller: FnId(a),
                callee: FnId(b),
                resolution: ResolutionKind::Direct,
            })
            .collect();
        Graph::new(&edges, |edge| started.contains(&edge.callee.0))
    }

    fn graph(edges: &[(u32, u32)]) -> Graph {
        graph_with_starts(edges, &[])
    }

    fn on_cycle(g: &Graph, f: u32) -> bool {
        g.cyclic()[g.index(FnId(f)).unwrap()]
    }

    #[test]
    fn cycles_are_components_of_several_nodes_or_self_calls() {
        // 1 -> 2 -> 3 -> 2, 3 -> 4, 5 -> 5, 6 -> 4
        let g = graph(&[(1, 2), (2, 3), (3, 2), (3, 4), (5, 5), (6, 4)]);
        assert!(!on_cycle(&g, 1));
        assert!(on_cycle(&g, 2));
        assert!(on_cycle(&g, 3));
        assert!(!on_cycle(&g, 4));
        assert!(on_cycle(&g, 5));
        assert!(!on_cycle(&g, 6));
    }

    #[test]
    fn a_start_edge_closes_no_cycle_but_is_followed_for_reach() {
        // 1 calls 2, which starts 3, which calls 2 again.
        let g = graph_with_starts(&[(1, 2), (2, 3), (3, 2)], &[3]);
        assert!(!on_cycle(&g, 2));
        assert!(!on_cycle(&g, 3));
        let reached = g.reachable_from([FnId(1)].into_iter());
        assert!(reached[g.index(FnId(3)).unwrap()]);
    }

    #[test]
    fn components_are_numbered_callees_first() {
        // 0 -> 1 -> 2 -> 1, 3 alone
        let comp = strongly_connected_components(&[vec![1], vec![2], vec![1], vec![]]);
        assert_eq!(comp[1], comp[2]);
        assert_ne!(comp[0], comp[1]);
        assert!(comp[1] < comp[0]);
        assert_ne!(comp[3], comp[0]);
    }

    #[test]
    fn reachability_includes_the_roots() {
        let g = graph(&[(1, 2), (2, 3), (4, 5)]);
        let reached = g.reachable_from([FnId(2)].into_iter());
        let at = |f: u32| reached[g.index(FnId(f)).unwrap()];
        assert!(at(2) && at(3));
        assert!(!at(1) && !at(4) && !at(5));
    }
}
