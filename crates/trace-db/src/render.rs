//! Graph rendering for `trace inspect`: one query result can be emitted as
//! the indented text view (default), a JSON document, a Graphviz DOT graph,
//! or a Mermaid flowchart.
//!
//! Rendering is pure — it only consumes the `QueryGraph` returned by
//! `call_graph` / `dataflow_graph`, so every format is testable in isolation.

use crate::inspect::{GraphEdge, QueryGraph};
use anyhow::{bail, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderFormat {
    Text,
    Json,
    Graphviz,
    Mermaid,
}

impl RenderFormat {
    /// Parse `text` (default), `json`, `graphviz`, or `mermaid`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "graphviz" => Ok(Self::Graphviz),
            "mermaid" => Ok(Self::Mermaid),
            other => bail!(
                "invalid format `{other}` (expected `text`, `json`, `graphviz`, or `mermaid`)"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Graphviz => "graphviz",
            Self::Mermaid => "mermaid",
        }
    }
}

/// CLI-side metadata folded into every render: the header line, the
/// direction word used in the title, the traversal depth, and the summary.
pub struct GraphMeta<'a> {
    /// First line of the text view, e.g. `callgraph from main (...)`.
    pub title: &'a str,
    /// Direction word, e.g. `callees` / `callers` / `flows-to` / `flows-from`.
    pub direction: &'a str,
    /// Traversal depth limit.
    pub depth: u32,
    /// Text summary line, e.g. `13 functions, 14 edges`.
    pub summary: &'a str,
}

#[derive(Serialize)]
struct JsonNode {
    id: i64,
    depth: u32,
    label: String,
    detail: String,
}

#[derive(Serialize)]
struct JsonEdge {
    from: i64,
    to: i64,
    label: String,
    site: String,
}

#[derive(Serialize)]
struct JsonGraph {
    title: String,
    direction: String,
    depth: u32,
    truncated: bool,
    summary: String,
    nodes: Vec<JsonNode>,
    edges: Vec<JsonEdge>,
}

/// Render `graph` in `format`. `label` resolves a node id to its display
/// text (the caller supplies the call-graph / dataflow naming style).
pub fn render_graph(
    graph: &QueryGraph,
    format: RenderFormat,
    meta: &GraphMeta,
    label: &mut dyn FnMut(i64, &mut String),
) -> String {
    match format {
        RenderFormat::Text => render_text(graph, meta, label),
        RenderFormat::Json => render_json(graph, meta, label),
        RenderFormat::Graphviz => render_graphviz(graph, meta, label),
        RenderFormat::Mermaid => render_mermaid(graph, meta, label),
    }
}

fn node_depth(graph: &QueryGraph) -> FxHashMap<i64, u32> {
    let mut map = FxHashMap::default();
    for &(id, d) in &graph.order {
        map.entry(id).or_insert(d);
    }
    map
}

/// `-{label}-> ({site})` edge annotation shared by the machine-readable
/// formats; mirrors the text view's `(site)` suffix.
fn edge_annotation(e: &GraphEdge) -> String {
    if e.site.is_empty() {
        e.label.clone()
    } else {
        format!("{} ({})", e.label, e.site.display())
    }
}

fn render_text(
    graph: &QueryGraph,
    meta: &GraphMeta,
    label: &mut dyn FnMut(i64, &mut String),
) -> String {
    let mut children: FxHashMap<i64, Vec<&GraphEdge>> = FxHashMap::default();
    for e in &graph.edges {
        children.entry(e.from).or_default().push(e);
    }
    let mut seen: FxHashSet<i64> = FxHashSet::default();
    let mut out = String::new();
    writeln!(out, "{}", meta.title).unwrap();
    let mut buf = String::new();

    #[allow(clippy::too_many_arguments)]
    fn walk(
        id: i64,
        level: usize,
        prefix_edge: Option<&GraphEdge>,
        children: &FxHashMap<i64, Vec<&GraphEdge>>,
        seen: &mut FxHashSet<i64>,
        label: &mut dyn FnMut(i64, &mut String),
        buf: &mut String,
        out: &mut String,
    ) {
        buf.clear();
        label(id, buf);
        let indent = "  ".repeat(level);
        match prefix_edge {
            None => writeln!(out, "{indent}* {buf}").unwrap(),
            Some(e) => {
                if seen.contains(&id) {
                    if e.site.is_empty() {
                        writeln!(out, "{indent}-{}-> {buf} (see above)", e.label).unwrap();
                    } else {
                        writeln!(
                            out,
                            "{indent}-{}-> {buf} (see above; also {})",
                            e.label,
                            e.site.display()
                        )
                        .unwrap();
                    }
                    return;
                }
                if e.site.is_empty() {
                    writeln!(out, "{indent}-{}-> {buf}", e.label).unwrap();
                } else {
                    writeln!(out, "{indent}-{}-> {buf} ({})", e.label, e.site.display()).unwrap();
                }
            }
        }
        seen.insert(id);
        if let Some(kids) = children.get(&id) {
            for kid in kids.clone() {
                walk(
                    kid.to,
                    level + 1,
                    Some(kid),
                    children,
                    seen,
                    label,
                    buf,
                    out,
                );
            }
        }
    }

    for &(root, depth) in &graph.order.clone() {
        if depth == 0 && !seen.contains(&root) {
            walk(
                root, 0, None, &children, &mut seen, label, &mut buf, &mut out,
            );
        }
    }

    if graph.truncated {
        writeln!(
            out,
            "(truncated at --depth {}; increase to see more)",
            meta.depth
        )
        .unwrap();
    }
    writeln!(out, "{}", meta.summary).unwrap();
    out
}

fn render_json(
    graph: &QueryGraph,
    meta: &GraphMeta,
    label: &mut dyn FnMut(i64, &mut String),
) -> String {
    let depths = node_depth(graph);
    let nodes = graph
        .order
        .iter()
        .map(|&(id, _)| {
            let mut l = String::new();
            label(id, &mut l);
            JsonNode {
                id,
                depth: depths[&id],
                label: l,
                detail: graph
                    .nodes
                    .get(&id)
                    .map(|n| n.detail.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();
    let edges = graph
        .edges
        .iter()
        .map(|e| JsonEdge {
            from: e.from,
            to: e.to,
            label: e.label.clone(),
            site: e.site.display(),
        })
        .collect();
    let doc = JsonGraph {
        title: meta.title.to_string(),
        direction: meta.direction.to_string(),
        depth: meta.depth,
        truncated: graph.truncated,
        summary: meta.summary.to_string(),
        nodes,
        edges,
    };
    let mut out = serde_json::to_string_pretty(&doc).unwrap();
    out.push('\n');
    out
}

fn dot_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

fn render_graphviz(
    graph: &QueryGraph,
    meta: &GraphMeta,
    label: &mut dyn FnMut(i64, &mut String),
) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "digraph {} {{",
        quote_ident_attr(&dot_escape(meta.title))
    )
    .unwrap();
    writeln!(out, "  rankdir=\"TB\";").unwrap();
    writeln!(out, "  node [shape=box];").unwrap();
    if graph.truncated {
        writeln!(
            out,
            "  // truncated at --depth {}; increase to see more",
            meta.depth
        )
        .unwrap();
    }
    for &(id, _) in &graph.order {
        let mut l = String::new();
        label(id, &mut l);
        writeln!(out, "  n{id} [label=\"{}\"];", dot_escape(&l)).unwrap();
    }
    for e in &graph.edges {
        let ann = dot_escape(&edge_annotation(e));
        writeln!(out, "  n{} -> n{} [label=\"{}\"];", e.from, e.to, ann).unwrap();
    }
    writeln!(out, "}}").unwrap();
    out
}

/// DOT identifiers may be bare keywords `[a-zA-Z_][a-zA-Z0-9_]*` or quoted
/// strings. Quote defensively so arbitrary titles stay valid.
fn quote_ident_attr(s: &str) -> String {
    format!("\"{}\"", s)
}

fn mermaid_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '|' => out.push_str("&#124;"),
            '#' => out.push_str("#35;"),
            _ => out.push(c),
        }
    }
    out
}

fn render_mermaid(
    graph: &QueryGraph,
    meta: &GraphMeta,
    label: &mut dyn FnMut(i64, &mut String),
) -> String {
    let mut out = String::new();
    writeln!(out, "flowchart TD").unwrap();
    writeln!(out, "  %% {}", mermaid_escape(meta.title)).unwrap();
    for &(id, _) in &graph.order {
        let mut l = String::new();
        label(id, &mut l);
        writeln!(out, "  n{id}[\"{}\"]", mermaid_escape(&l)).unwrap();
    }
    for e in &graph.edges {
        writeln!(
            out,
            "  n{} -->|\"{}\"| n{}",
            e.from,
            mermaid_escape(&edge_annotation(e)),
            e.to
        )
        .unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect::{EdgeSite, GraphNode, QueryGraph};

    fn graph() -> QueryGraph {
        let mut g = QueryGraph::default();
        g.nodes.insert(
            1,
            GraphNode {
                id: 1,
                label: "main".into(),
                detail: "main.c:1".into(),
                kind: None,
                loc_kind: None,
            },
        );
        g.nodes.insert(
            2,
            GraphNode {
                id: 2,
                label: "target".into(),
                detail: "target.c:5".into(),
                kind: None,
                loc_kind: None,
            },
        );
        g.order.push((1, 0));
        g.order.push((2, 1));
        g.edges.push(GraphEdge {
            from: 1,
            to: 2,
            label: "indirect".into(),
            site: EdgeSite {
                path: "/proj/target.c".into(),
                line: 5,
                col: 2,
            },
        });
        g
    }

    fn meta<'a>() -> GraphMeta<'a> {
        GraphMeta {
            title: "callgraph from main (callees, depth 2):",
            direction: "callees",
            depth: 2,
            summary: "2 functions, 1 edges",
        }
    }

    fn label(id: i64, out: &mut String) {
        out.push_str("fn");
        out.push_str(&id.to_string());
    }

    #[test]
    fn text_renders_indented_forest() {
        let out = render_graph(&graph(), RenderFormat::Text, &meta(), &mut label);
        assert_eq!(
            out,
            "callgraph from main (callees, depth 2):\n* fn1\n  -indirect-> fn2 (target.c:5)\n2 functions, 1 edges\n"
        );
    }

    #[test]
    fn text_renders_see_above_for_revisits() {
        let mut g = graph();
        g.edges.push(GraphEdge {
            from: 2,
            to: 1,
            label: "direct".into(),
            site: EdgeSite::default(),
        });
        let out = render_graph(&g, RenderFormat::Text, &meta(), &mut label);
        assert!(out.contains("  -direct-> fn1 (see above)"), "{out}");
    }

    #[test]
    fn text_renders_truncation_marker() {
        let mut g = graph();
        g.truncated = true;
        let out = render_graph(&g, RenderFormat::Text, &meta(), &mut label);
        assert!(
            out.contains("(truncated at --depth 2; increase to see more)"),
            "{out}"
        );
    }

    #[test]
    fn json_round_trips_graph() {
        let out = render_graph(&graph(), RenderFormat::Json, &meta(), &mut label);
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["title"], "callgraph from main (callees, depth 2):");
        assert_eq!(doc["direction"], "callees");
        assert_eq!(doc["depth"], 2);
        assert_eq!(doc["truncated"], false);
        assert_eq!(doc["summary"], "2 functions, 1 edges");
        assert_eq!(doc["nodes"][0]["id"], 1);
        assert_eq!(doc["nodes"][0]["depth"], 0);
        assert_eq!(doc["nodes"][0]["label"], "fn1");
        assert_eq!(doc["nodes"][0]["detail"], "main.c:1");
        assert_eq!(doc["edges"][0]["from"], 1);
        assert_eq!(doc["edges"][0]["to"], 2);
        assert_eq!(doc["edges"][0]["label"], "indirect");
        assert_eq!(doc["edges"][0]["site"], "target.c:5");
    }

    #[test]
    fn graphviz_emits_digraph_with_edges() {
        let out = render_graph(&graph(), RenderFormat::Graphviz, &meta(), &mut label);
        assert!(out.starts_with("digraph \""), "{out}");
        assert!(out.contains("n1 [label=\"fn1\"];"), "{out}");
        assert!(
            out.contains("n1 -> n2 [label=\"indirect (target.c:5)\"];"),
            "{out}"
        );
        assert!(out.trim_end().ends_with('}'), "{out}");
    }

    #[test]
    fn mermaid_emits_flowchart_with_labels() {
        let out = render_graph(&graph(), RenderFormat::Mermaid, &meta(), &mut label);
        assert!(out.starts_with("flowchart TD"), "{out}");
        assert!(out.contains("n1[\"fn1\"]"), "{out}");
        assert!(
            out.contains("n1 -->|\"indirect (target.c:5)\"| n2"),
            "{out}"
        );
    }

    #[test]
    fn escaping_keeps_labels_valid() {
        let mut g = graph();
        g.nodes.insert(
            3,
            GraphNode {
                id: 3,
                label: "a\"b\\c&d|e".into(),
                detail: String::new(),
                kind: Some(crate::inspect::FlowNodeKind::Loc),
                loc_kind: Some(trace_analysis::LocKind::Heap),
            },
        );
        g.order.push((3, 2));
        g.edges.push(GraphEdge {
            from: 2,
            to: 3,
            label: "copy".into(),
            site: EdgeSite {
                path: "x\"y".into(),
                line: 0,
                col: 0,
            },
        });
        let mut node_label = |id: i64, out: &mut String| {
            out.push_str(&g.nodes.get(&id).unwrap().label.clone());
        };
        let dot = render_graph(&g, RenderFormat::Graphviz, &meta(), &mut node_label);
        assert!(dot.contains("n3 [label=\"a\\\"b\\\\c&d|e\"];"), "{dot}");
        let mmd = render_graph(&g, RenderFormat::Mermaid, &meta(), &mut node_label);
        assert!(mmd.contains("n3[\"a&quot;b\\c&amp;d&#124;e\"]"), "{mmd}");
        assert!(mmd.contains("copy (x&quot;y:0)"), "{mmd}");
    }

    #[test]
    fn format_parse_accepts_all_and_rejects_garbage() {
        for (s, want) in [
            ("text", RenderFormat::Text),
            ("json", RenderFormat::Json),
            ("graphviz", RenderFormat::Graphviz),
            ("mermaid", RenderFormat::Mermaid),
        ] {
            assert_eq!(RenderFormat::parse(s).unwrap(), want);
        }
        assert!(RenderFormat::parse("dot").is_err());
        assert_eq!(RenderFormat::Text.as_str(), "text");
    }
}
