use rustc_hash::{FxHashMap, FxHashSet};
use trace_ir::{CallSiteId, FnId};

pub use crate::constraints::CallGraphEdge;

#[derive(Debug, Default)]
pub struct CallGraph {
    pub edges: Vec<CallGraphEdge>,
}

impl CallGraph {
    pub fn callees_of(&self, caller: FnId) -> Vec<FnId> {
        self.edges
            .iter()
            .filter(|e| e.caller == caller)
            .map(|e| e.callee)
            .collect()
    }

    pub fn callers_of(&self, callee: FnId) -> Vec<(FnId, CallSiteId)> {
        self.edges
            .iter()
            .filter(|e| e.callee == callee)
            .map(|e| (e.caller, e.call_site))
            .collect()
    }

    /// Find all simple call chains (paths) from `from` to `to` with edge count <= `max_depth`.
    /// Each chain is represented as a sequence of `CallGraphEdge`s.
    pub fn find_call_chains(
        &self,
        from: FnId,
        to: FnId,
        max_depth: usize,
    ) -> Vec<Vec<CallGraphEdge>> {
        let mut chains = Vec::new();
        if from == to {
            chains.push(Vec::new());
        }
        if max_depth == 0 {
            return chains;
        }

        let mut adj: FxHashMap<FnId, Vec<&CallGraphEdge>> = FxHashMap::default();
        for edge in &self.edges {
            adj.entry(edge.caller).or_default().push(edge);
        }

        let mut current_edges: Vec<CallGraphEdge> = Vec::new();
        let mut active_nodes: FxHashSet<FnId> = FxHashSet::default();
        active_nodes.insert(from);

        #[allow(clippy::too_many_arguments)]
        fn dfs(
            u: FnId,
            target: FnId,
            depth: usize,
            max_depth: usize,
            adj: &FxHashMap<FnId, Vec<&CallGraphEdge>>,
            current_edges: &mut Vec<CallGraphEdge>,
            active_nodes: &mut FxHashSet<FnId>,
            chains: &mut Vec<Vec<CallGraphEdge>>,
        ) {
            if depth >= max_depth {
                return;
            }
            if let Some(neighbors) = adj.get(&u) {
                for edge in neighbors {
                    let v = edge.callee;
                    if v == target {
                        let mut path = current_edges.clone();
                        path.push((*edge).clone());
                        chains.push(path);
                        continue;
                    }
                    if !active_nodes.contains(&v) {
                        active_nodes.insert(v);
                        current_edges.push((*edge).clone());

                        dfs(
                            v,
                            target,
                            depth + 1,
                            max_depth,
                            adj,
                            current_edges,
                            active_nodes,
                            chains,
                        );

                        current_edges.pop();
                        active_nodes.remove(&v);
                    }
                }
            }
        }

        dfs(
            from,
            to,
            0,
            max_depth,
            &adj,
            &mut current_edges,
            &mut active_nodes,
            &mut chains,
        );

        chains.sort_by(|a, b| {
            a.len().cmp(&b.len()).then_with(|| {
                let a_seq: Vec<u32> = a.iter().map(|e| e.callee.0).collect();
                let b_seq: Vec<u32> = b.iter().map(|e| e.callee.0).collect();
                a_seq.cmp(&b_seq)
            })
        });

        chains
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolutionKind;

    fn make_edge(caller: u32, callee: u32, site: u32) -> CallGraphEdge {
        CallGraphEdge {
            call_site: CallSiteId(site),
            caller: FnId(caller),
            callee: FnId(callee),
            resolution: ResolutionKind::Direct,
        }
    }

    #[test]
    fn test_direct_chain() {
        let cg = CallGraph {
            edges: vec![make_edge(1, 2, 10)],
        };
        let chains = cg.find_call_chains(FnId(1), FnId(2), 1);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 1);
        assert_eq!(chains[0][0].caller, FnId(1));
        assert_eq!(chains[0][0].callee, FnId(2));
    }

    #[test]
    fn test_depth_limiting() {
        // 1 -> 2 -> 3 -> 4
        let cg = CallGraph {
            edges: vec![
                make_edge(1, 2, 10),
                make_edge(2, 3, 20),
                make_edge(3, 4, 30),
            ],
        };
        // depth 2 cannot reach 4
        assert!(cg.find_call_chains(FnId(1), FnId(4), 2).is_empty());
        // depth 3 reaches 4
        let chains = cg.find_call_chains(FnId(1), FnId(4), 3);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 3);
    }

    #[test]
    fn test_branching_paths() {
        // 1 -> 2 -> 4
        // 1 -> 3 -> 4
        let cg = CallGraph {
            edges: vec![
                make_edge(1, 2, 10),
                make_edge(1, 3, 11),
                make_edge(2, 4, 20),
                make_edge(3, 4, 30),
            ],
        };
        let chains = cg.find_call_chains(FnId(1), FnId(4), 2);
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0].len(), 2);
        assert_eq!(chains[1].len(), 2);
    }

    #[test]
    fn test_cycle_avoidance() {
        // 1 -> 2 -> 1 (cycle)
        // 2 -> 3
        let cg = CallGraph {
            edges: vec![
                make_edge(1, 2, 10),
                make_edge(2, 1, 20),
                make_edge(2, 3, 21),
            ],
        };
        let chains = cg.find_call_chains(FnId(1), FnId(3), 5);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].len(), 2);
    }

    #[test]
    fn test_cycle_to_self() {
        // 1 -> 2 -> 1
        let cg = CallGraph {
            edges: vec![make_edge(1, 2, 10), make_edge(2, 1, 20)],
        };
        let chains = cg.find_call_chains(FnId(1), FnId(1), 2);
        assert_eq!(chains.len(), 2);
        assert_eq!(chains[0].len(), 0);
        assert_eq!(chains[1].len(), 2);
        assert_eq!(chains[1][0].caller, FnId(1));
        assert_eq!(chains[1][0].callee, FnId(2));
        assert_eq!(chains[1][1].caller, FnId(2));
        assert_eq!(chains[1][1].callee, FnId(1));
    }
}
