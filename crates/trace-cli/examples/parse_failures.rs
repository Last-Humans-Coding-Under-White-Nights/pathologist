//! List tree-sitter ERROR nodes for files that failed to parse.
//!
//! Usage:
//!   cargo run -p trace-cli --release --example parse_failures -- \
//!     /path/to/project --from-db /tmp/out.db

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use trace_parse::{
    collect_unrecovered_parse_errors, discover_source_files, has_parse_errors, node_text,
    parse_source_with_lang, IncludeGraph, IndexSourceCache, SourceLang,
};
use trace_preproc::PreprocessOptions;

fn main() -> Result<(), String> {
    run(std::env::args().skip(1), &mut io::stdout().lock())
}

fn run(mut args: impl Iterator<Item = String>, output: &mut impl Write) -> Result<(), String> {
    let root = PathBuf::from(args.next().ok_or("parse_failures requires ROOT")?);
    if !root.is_dir() {
        return Err(format!(
            "source root is not a directory: {}",
            root.display()
        ));
    }
    let mut from_db: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        if arg == "--from-db" {
            from_db = Some(PathBuf::from(args.next().ok_or("--from-db requires PATH")?));
        } else {
            return Err(format!("unknown argument: {arg}"));
        }
    }

    let failing = if let Some(db) = from_db {
        Some(load_parse_failures_from_db(&db)?)
    } else {
        None
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
        .map(|p| include_graph.intern_path(p))
        .collect();

    let source_cache = IndexSourceCache::new();
    let mut targets: Vec<PathBuf> =
        failing.unwrap_or_else(|| files.into_iter().chain(headers).collect());
    targets.sort();
    targets.dedup();

    let mut rows = 0;
    for path in targets {
        let canonical = include_graph.intern_path(&path);
        let pre = match source_cache.get_or_preprocess(&canonical, &include_graph, &eff_opts) {
            Ok(p) => p,
            Err(e) => {
                writeln!(output, "FILE\t{}\tPREPROCESS\t{}", path.display(), e)
                    .map_err(|e| e.to_string())?;
                rows += 1;
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
            writeln!(
                output,
                "FILE\t{}\tPARSE\t{} grammar; tree-sitter reported errors but no ERROR nodes found",
                path.display(),
                if lang == SourceLang::Cpp { "C++" } else { "C" }
            )
            .map_err(|e| e.to_string())?;
            rows += 1;
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
            let snippet = snippet(text);
            writeln!(
                output,
                "FILE\t{}\tERROR\tline {} col {} ({}) {}",
                path.display(),
                pos.row + 1,
                pos.column + 1,
                kind,
                snippet
            )
            .map_err(|e| e.to_string())?;
            rows += 1;
        }
    }
    writeln!(output, "END\t{rows}").map_err(|e| e.to_string())
}

fn snippet(text: &str) -> String {
    let mut chars = text.chars();
    let mut snippet: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        snippet.push('…');
    }
    snippet.replace(['\t', '\n', '\r'], " ")
}

fn load_parse_failures_from_db(db: &Path) -> Result<Vec<PathBuf>, String> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT message FROM diagnostics WHERE stage='parse' AND message LIKE 'parse errors in %' ORDER BY message",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_truncate_unicode_and_keep_one_tsv_line() {
        let text = format!("{}😀tail", "a".repeat(119));
        assert_eq!(snippet(&text), format!("{}😀…", "a".repeat(119)));
        assert_eq!(snippet("é😀\t\n\r"), "é😀   ");
        assert_eq!(snippet(&"é".repeat(120)), "é".repeat(120));
    }

    #[test]
    fn empty_database_selection_does_not_scan_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.c"), "int x = ;\n").unwrap();
        let db = dir.path().join("analysis.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE diagnostics (stage TEXT, message TEXT);")
            .unwrap();
        drop(conn);
        let mut output = Vec::new();
        run(
            [
                dir.path().display().to_string(),
                "--from-db".to_owned(),
                db.display().to_string(),
            ]
            .into_iter(),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"END\t0\n");
    }

    #[test]
    fn missing_database_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("missing.db");
        assert!(load_parse_failures_from_db(&db).is_err());
        assert!(!db.exists());
    }

    #[test]
    fn scan_orders_files_and_counts_completed_rows() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["z.c", "a.c"] {
            std::fs::write(dir.path().join(name), "int x = ;\n").unwrap();
        }
        let mut output = Vec::new();
        run([dir.path().display().to_string()].into_iter(), &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.find("/a.c\t").unwrap() < text.find("/z.c\t").unwrap());
        let rows: Vec<_> = text.lines().collect();
        assert_eq!(rows.last().unwrap(), &format!("END\t{}", rows.len() - 1));
    }
}
