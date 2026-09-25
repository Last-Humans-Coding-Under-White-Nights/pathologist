//! One-off helper: list tree-sitter ERROR nodes for files that failed to parse.
//!
//! Usage:
//!   cargo run -p trace-cli --release --example parse_failures -- \
//!     /path/to/project --from-db /tmp/out.db

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use trace_parse::{
    collect_unrecovered_parse_errors, discover_source_files, has_parse_errors, node_text,
    parse_source_with_lang, IncludeGraph, IndexSourceCache, SourceLang,
};
use trace_preproc::PreprocessOptions;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("--from-db requires ROOT")?);
    let mut from_db: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        if arg == "--from-db" {
            from_db = Some(PathBuf::from(args.next().ok_or("--from-db requires PATH")?));
        }
    }

    let failing = if let Some(db) = from_db {
        load_parse_failures_from_db(&db)?
    } else {
        Vec::new()
    };

    let opts = PreprocessOptions::default();
    let (files, headers) = discover_source_files(&root);
    let include_graph = IncludeGraph::build(&root, &files, &headers);
    let mut eff_opts = opts.clone();
    for dir in &include_graph.include_dirs {
        if !eff_opts.include_paths.iter().any(|p| p == dir) {
            eff_opts.include_paths.push(dir.clone());
        }
    }
    if eff_opts.source_cache.is_none() && !include_graph.source_cache.is_empty() {
        eff_opts.source_cache = Some(std::sync::Arc::new(trace_preproc::SourceCache::new(
            include_graph.source_cache.clone(),
        )));
    }
    let eff_opts = eff_opts.for_indexing().with_inline_include_bodies(false);
    let cpp_tus: HashSet<PathBuf> = files
        .iter()
        .filter(|p| trace_parse::is_cpp_path(p))
        .cloned()
        .collect();

    let source_cache = IndexSourceCache::new();
    let targets: Vec<PathBuf> = if failing.is_empty() {
        files
            .into_iter()
            .chain(headers)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    } else {
        failing
    };

    for path in targets {
        let canonical = include_graph.intern_path(&path);
        let pre = match source_cache.get_or_preprocess(&canonical, &include_graph, &eff_opts) {
            Ok(p) => p,
            Err(e) => {
                println!("FILE\t{}\tPREPROCESS\t{}", path.display(), e);
                continue;
            }
        };
        let lang = index_lang(&canonical, &cpp_tus, &include_graph);
        let parsed = parse_source_with_lang(Arc::clone(&pre.text), lang)?;
        if !has_parse_errors(&parsed.tree) {
            continue;
        }
        let mut error_nodes = Vec::new();
        collect_unrecovered_parse_errors(parsed.tree.root_node(), &mut error_nodes);
        if error_nodes.is_empty() {
            println!(
                "FILE\t{}\tPARSE\t{} grammar; tree-sitter reported errors but no ERROR nodes found",
                path.display(),
                if lang == SourceLang::Cpp { "C++" } else { "C" }
            );
            continue;
        }
        for node in error_nodes {
            let pos = node.start_position();
            let kind = if node.is_missing() {
                format!("missing {}", node.kind())
            } else {
                node.kind().to_string()
            };
            let text = node_text(parsed.source.as_ref(), &node);
            let snippet = if text.len() > 120 {
                format!("{}…", &text[..120])
            } else {
                text.to_string()
            };
            let snippet = snippet.replace(['\t', '\n'], " ");
            println!(
                "FILE\t{}\tERROR\tline {} col {} ({}) {}",
                path.display(),
                pos.row + 1,
                pos.column + 1,
                kind,
                snippet
            );
        }
    }
    Ok(())
}

fn load_parse_failures_from_db(db: &Path) -> Result<Vec<PathBuf>, String> {
    let conn = rusqlite::Connection::open(db).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT message FROM diagnostics WHERE stage='parse' AND message LIKE 'parse errors in %'",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let msg: String = row.map_err(|e| e.to_string())?;
        let prefix = "parse errors in ";
        let path = msg
            .strip_prefix(prefix)
            .ok_or_else(|| format!("unexpected diagnostic: {msg}"))?;
        out.push(PathBuf::from(path));
    }
    Ok(out)
}

fn index_lang(path: &Path, cpp_tus: &HashSet<PathBuf>, graph: &IncludeGraph) -> SourceLang {
    if trace_parse::is_cpp_path(path) || trace_parse::is_cpp_header_path(path) {
        return SourceLang::Cpp;
    }
    if path.extension().and_then(|e| e.to_str()) == Some("h") {
        let reachable = cpp_tus.iter().any(|tu| {
            graph
                .reachable_from(&HashSet::from([tu.clone()]))
                .contains(path)
        });
        if reachable {
            return SourceLang::Cpp;
        }
    }
    SourceLang::C
}
