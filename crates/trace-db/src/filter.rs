//! Display-only call-graph filtering for `trace inspect`.
//!
//! A JSON config file selects which functions should be shown in call-graph
//! output. Filtering is an adapter over already-produced results: it never
//! touches the SQLite database or the analysis pipeline, it only discards
//! edges/nodes that fail to match before they are rendered.

use crate::{GraphEdge, QueryGraph};
use anyhow::{Context, Result};
use regex::Regex;
use rustc_hash::FxHashSet;
use serde::Deserialize;
use std::path::Path;

/// Parsed `--callgraph-filter` config: a list of regex patterns applied to
/// function names.
///
/// Semantics: an edge is shown when the caller *or* the callee name matches any
/// pattern. Nodes referenced only by such edges are shown too.
#[derive(Debug, Clone)]
pub struct CallGraphFilter {
    patterns: Vec<Regex>,
}

#[derive(Deserialize)]
struct FilterConfig {
    functions: Vec<String>,
}

impl CallGraphFilter {
    /// Load and compile a filter config from a JSON file.
    ///
    /// ```json
    /// { "functions": ["malloc", "free", "calloc", "realloc"] }
    /// ```
    pub fn from_file(path: &Path) -> Result<Self> {
        let src = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read filter config {}", path.display()))?;
        Self::from_json(&src)
            .with_context(|| format!("failed to parse filter config {}", path.display()))
    }

    /// Load a filter config from a JSON string.
    pub fn from_json(src: &str) -> Result<Self> {
        let cfg: FilterConfig = serde_json::from_str(src)
            .context("filter config must be JSON: {\"functions\": [...]}")?;
        let mut patterns = Vec::with_capacity(cfg.functions.len());
        for pat in cfg.functions {
            patterns
                .push(Regex::new(&pat).with_context(|| format!("invalid function regex `{pat}`"))?);
        }
        if patterns.is_empty() {
            anyhow::bail!("filter config `functions` must not be empty");
        }
        Ok(Self { patterns })
    }

    /// True when a function name matches any pattern.
    pub fn matches(&self, name: &str) -> bool {
        self.patterns.iter().any(|re| re.is_match(name))
    }
}

impl std::fmt::Display for CallGraphFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pats: Vec<&str> = self.patterns.iter().map(|re| re.as_str()).collect();
        write!(f, "{}", pats.join(", "))
    }
}

/// Filter a call-graph `QueryGraph` in place (display adapter). An edge is kept
/// when the caller or callee node's label (its function name) matches the
/// filter; nodes are kept iff they are still incident to a kept edge — except
/// the roots (the depth-0 start functions) which are always kept so the query
/// anchor stays visible even when it does not match.
///
/// The `order` is recomputed on the surviving subgraph: surviving fragments are
/// re-rooted at the survivors with no incoming edge and fresh BFS depths are
/// assigned, and any survivor left unvisited (e.g. a disconnected cycle whose
/// anchor edge was pruned) is re-rooted on its own. This keeps `render_text`,
/// which only walks depth-0 entries, consistent with the other formats even
/// when the original start function was itself pruned.
///
/// `truncated` is preserved unchanged: the filter never clears the depth-limit
/// marker, because the original BFS may still have omitted reachable functions
/// beyond the depth limit (this is conservative — it may over-warn, but never
/// under-warns).
pub fn filter_query_graph(graph: &mut QueryGraph, filter: &CallGraphFilter) {
    let keeps = |node_id: i64| -> bool {
        match graph.nodes.get(&node_id) {
            Some(n) => filter.matches(&n.label),
            None => false,
        }
    };
    graph.edges.retain(|e| keeps(e.from) || keeps(e.to));

    let orig_order = std::mem::take(&mut graph.order);
    let mut incident: FxHashSet<i64> = graph
        .edges
        .iter()
        .flat_map(|e: &GraphEdge| [e.from, e.to])
        .collect();
    for &(id, depth) in &orig_order {
        if depth == 0 {
            incident.insert(id);
        }
    }
    let mut new_nodes = rustc_hash::FxHashMap::default();
    for (id, node) in graph.nodes.drain() {
        if incident.contains(&id) {
            new_nodes.insert(id, node);
        }
    }
    graph.nodes = new_nodes;

    let survivors: FxHashSet<i64> = graph.nodes.keys().copied().collect();
    let mut children: rustc_hash::FxHashMap<i64, Vec<i64>> = rustc_hash::FxHashMap::default();
    let mut has_parent: FxHashSet<i64> = FxHashSet::default();
    for e in &graph.edges {
        children.entry(e.from).or_default().push(e.to);
        has_parent.insert(e.to);
    }
    let mut roots: Vec<i64> = Vec::new();
    for &(id, depth) in &orig_order {
        // The query anchors first, in original discovery order.
        if depth == 0 && survivors.contains(&id) {
            roots.push(id);
        }
    }
    // Then re-root surviving fragments that lost their parent chain.
    for &(id, _) in &orig_order {
        if survivors.contains(&id) && !has_parent.contains(&id) && !roots.contains(&id) {
            roots.push(id);
        }
    }

    let mut order: Vec<(i64, u32)> = Vec::new();
    let mut visited: FxHashSet<i64> = FxHashSet::default();
    let mut queue = std::collections::VecDeque::new();
    for r in roots {
        if visited.insert(r) {
            queue.push_back((r, 0u32));
        }
    }
    let traverse = |queue: &mut std::collections::VecDeque<(i64, u32)>,
                    visited: &mut FxHashSet<i64>,
                    order: &mut Vec<(i64, u32)>,
                    children: &rustc_hash::FxHashMap<i64, Vec<i64>>| {
        while let Some((id, depth)) = queue.pop_front() {
            order.push((id, depth));
            if let Some(kids) = children.get(&id) {
                for &kid in kids {
                    if visited.insert(kid) {
                        queue.push_back((kid, depth + 1));
                    }
                }
            }
        }
    };
    traverse(&mut queue, &mut visited, &mut order, &children);
    // Any survivor the BFS never reached — e.g. a pure cycle whose edge list is
    // closed (every member has a parent) and which is disconnected from the
    // retained root — must still be shown, so re-root it at depth 0 in original
    // discovery order.
    for &(id, _) in &orig_order {
        if survivors.contains(&id) && visited.insert(id) {
            queue.push_back((id, 0u32));
            traverse(&mut queue, &mut visited, &mut order, &children);
        }
    }
    graph.order = order;
}

/// Filter a call-chains result in place. A chain is kept if any of its nodes
/// matches the filter.
pub fn filter_call_chains(
    result: &mut crate::inspect::CallChainsResult,
    filter: &CallGraphFilter,
    labels: &rustc_hash::FxHashMap<i64, crate::inspect::GraphNode>,
) {
    result.chains.retain(|chain| {
        chain.nodes.iter().any(|&id| {
            labels
                .get(&id)
                .map(|n| filter.matches(&n.label))
                .unwrap_or(false)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches_patterns() {
        let f =
            CallGraphFilter::from_json(r#"{ "functions": ["malloc", "free", "^cb_"] }"#).unwrap();
        assert!(f.matches("malloc"));
        assert!(f.matches("__libc_malloc"));
        assert!(f.matches("free"));
        assert!(f.matches("cb_foo"));
        assert!(!f.matches("memcpy"));
        assert!(!f.matches("calloc"));
    }

    #[test]
    fn rejects_invalid_regex() {
        let err = CallGraphFilter::from_json(r#"{ "functions": ["("] }"#).unwrap_err();
        assert!(err.to_string().contains("invalid function regex"), "{err}");
    }

    #[test]
    fn rejects_missing_functions() {
        let err = CallGraphFilter::from_json(r#"{"functions": []}"#).unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn rejects_non_json() {
        assert!(CallGraphFilter::from_json("not json").is_err());
    }

    fn label_node(id: i64, name: &str) -> crate::GraphNode {
        crate::GraphNode {
            id,
            label: name.to_string(),
            detail: String::new(),
            kind: None,
            loc_kind: None,
        }
    }

    #[test]
    fn query_graph_keeps_only_edges_with_matching_endpoint() {
        let f = CallGraphFilter::from_json(r#"{ "functions": ["malloc"] }"#).unwrap();
        // Nodes: 1=a_caller -> 2=malloc -> 3=other_callee
        let mut graph = QueryGraph {
            nodes: [
                (1, label_node(1, "a_caller")),
                (2, label_node(2, "malloc")),
                (3, label_node(3, "other_callee")),
                (4, label_node(4, "unrelated")),
            ]
            .into_iter()
            .collect(),
            order: vec![(1, 0), (2, 1), (3, 2), (4, 3)],
            edges: vec![
                crate::GraphEdge {
                    from: 1,
                    to: 2,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
                crate::GraphEdge {
                    from: 2,
                    to: 3,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
                crate::GraphEdge {
                    from: 3,
                    to: 4,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
            ],
            truncated: false,
        };

        filter_query_graph(&mut graph, &f);

        // Edges touching `malloc` (1->2 and 2->3) survive; 3->4 does not touch
        // malloc, so node 4 (and that edge) are pruned.
        let surviving: Vec<(i64, i64)> = graph.edges.iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(surviving, vec![(1, 2), (2, 3)], "edges: {surviving:?}");
        assert!(graph.nodes.contains_key(&1));
        assert!(graph.nodes.contains_key(&2));
        assert!(graph.nodes.contains_key(&3));
        assert!(!graph.nodes.contains_key(&4), "node 4 should be pruned");
        assert_eq!(
            graph.order.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn query_graph_empty_keeps_root_when_nothing_matches() {
        let f = CallGraphFilter::from_json(r#"{ "functions": ["free"] }"#).unwrap();
        let mut graph = QueryGraph {
            nodes: [(1, label_node(1, "a")), (2, label_node(2, "b"))]
                .into_iter()
                .collect(),
            order: vec![(1, 0), (2, 1)],
            edges: vec![crate::GraphEdge {
                from: 1,
                to: 2,
                label: "direct".into(),
                site: crate::EdgeSite::default(),
            }],
            truncated: false,
        };
        filter_query_graph(&mut graph, &f);
        assert!(graph.edges.is_empty());
        assert_eq!(graph.nodes.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(graph.order, vec![(1, 0)]);
    }

    #[test]
    fn query_graph_reroots_when_start_function_is_pruned() {
        // BFS root 1=a does not match; only the deeper 2=b -> 3=malloc fragment
        // (plus its parent b) survives. The query anchor (root 1) stays visible
        // and the surviving fragment is re-rooted at depth 0 so `render_text`
        // (which walks depth-0 entries) emits it.
        let f = CallGraphFilter::from_json(r#"{ "functions": ["malloc"] }"#).unwrap();
        let mut graph = QueryGraph {
            nodes: [
                (1, label_node(1, "a")),
                (2, label_node(2, "b")),
                (3, label_node(3, "malloc")),
                (4, label_node(4, "c")),
            ]
            .into_iter()
            .collect(),
            order: vec![(1, 0), (2, 1), (3, 2), (4, 3)],
            edges: vec![
                crate::GraphEdge {
                    from: 1,
                    to: 2,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
                crate::GraphEdge {
                    from: 2,
                    to: 3,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
                // Reachable, disconnected, unrelated: 1->2->4.
                crate::GraphEdge {
                    from: 2,
                    to: 4,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
            ],
            truncated: true,
        };

        filter_query_graph(&mut graph, &f);

        let edges: Vec<(i64, i64)> = graph.edges.iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(edges, vec![(2, 3)], "only b->malloc survives: {edges:?}");
        assert!(graph.nodes.contains_key(&1), "query anchor always kept");
        assert!(
            !graph.nodes.contains_key(&4),
            "unmatched non-root endpoint pruned"
        );
        // Root stays at depth 0; b (no surviving parent) is also a root.
        assert_eq!(
            graph.order,
            vec![(1, 0), (2, 0), (3, 1)],
            "root kept, fragment re-rooted at depth 0"
        );
        assert!(
            graph.truncated,
            "depth-limit marker is preserved (not cleared) after pruning"
        );
    }

    #[test]
    fn query_graph_disconnected_cycle_is_rerooted_and_rendered() {
        // root -> wrapper -> malloc -> wrapper, filter matching only `malloc`.
        // The retained root loses its edge, leaving a cycle (wrapper <-> malloc)
        // disconnected from it. Every survivor must still reach `order` so the
        // text renderer (walking depth-0 entries) and JSON (emitting per-order
        // nodes) stay consistent.
        let f = CallGraphFilter::from_json(r#"{ "functions": ["^malloc$"] }"#).unwrap();
        let mut graph = QueryGraph {
            nodes: [
                (1, label_node(1, "root")),
                (2, label_node(2, "wrapper")),
                (3, label_node(3, "malloc")),
            ]
            .into_iter()
            .collect(),
            order: vec![(1, 0), (2, 1), (3, 2)],
            edges: vec![
                crate::GraphEdge {
                    from: 1,
                    to: 2,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
                crate::GraphEdge {
                    from: 2,
                    to: 3,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
                crate::GraphEdge {
                    from: 3,
                    to: 2,
                    label: "direct".into(),
                    site: crate::EdgeSite::default(),
                },
            ],
            truncated: false,
        };
        filter_query_graph(&mut graph, &f);

        // Only the wrapper <-> malloc cycle survives.
        let edges: Vec<(i64, i64)> = graph.edges.iter().map(|e| (e.from, e.to)).collect();
        assert_eq!(edges, vec![(2, 3), (3, 2)], "edges: {edges:?}");
        // Root is kept as the anchor; the disconnected cycle is re-rooted at
        // wrapper (depth 0) and reachable, not silently dropped.
        let order_ids: Vec<i64> = graph.order.iter().map(|&(id, _)| id).collect();
        assert_eq!(
            order_ids,
            vec![1, 2, 3],
            "all survivors in order: {order_ids:?}"
        );
        for e in &graph.edges {
            assert!(graph.nodes.contains_key(&e.from), "edge from missing node");
            assert!(graph.nodes.contains_key(&e.to), "edge to missing node");
        }
    }

    #[test]
    fn query_graph_cycle_connected_to_anchor() {
        // b (anchor, depth 0) <-> malloc with `malloc` matching: the cycle is
        // reachable from the anchor, so no extra re-rooting is needed.
        let f = CallGraphFilter::from_json(r#"{ "functions": ["malloc"] }"#).unwrap();
        let mut graph = QueryGraph {
            nodes: [(1, label_node(1, "b")), (2, label_node(2, "malloc"))]
                .into_iter()
                .collect(),
            order: vec![(1, 0), (2, 1)],
            edges: vec![
                crate::GraphEdge {
                    from: 1,
                    to: 2,
                    label: "call".into(),
                    site: crate::EdgeSite::default(),
                },
                crate::GraphEdge {
                    from: 2,
                    to: 1,
                    label: "call".into(),
                    site: crate::EdgeSite::default(),
                },
            ],
            truncated: false,
        };
        filter_query_graph(&mut graph, &f);
        assert_eq!(
            graph.order.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
            vec![1, 2],
            "cycle rooted at earliest survivor"
        );
    }

    #[test]
    fn query_graph_preserves_depth_limit_warning() {
        // Even a match-all filter must not clear `truncated`: the original BFS
        // still cut off reachable functions at the depth limit.
        let f = CallGraphFilter::from_json(r#"{ "functions": [".*"] }"#).unwrap();
        let mut graph = QueryGraph {
            nodes: [(1, label_node(1, "a")), (2, label_node(2, "b"))]
                .into_iter()
                .collect(),
            order: vec![(1, 0), (2, 1)],
            edges: vec![crate::GraphEdge {
                from: 1,
                to: 2,
                label: "direct".into(),
                site: crate::EdgeSite::default(),
            }],
            truncated: true,
        };
        filter_query_graph(&mut graph, &f);
        assert!(
            graph.truncated,
            "match-all filter keeps every edge, so the depth-limit marker must persist"
        );
        assert_eq!(
            graph.order.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}
