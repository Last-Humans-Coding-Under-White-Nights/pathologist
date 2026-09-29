use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use trace_preproc::LineMap;
use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLang {
    C,
    Cpp,
}

impl SourceLang {
    pub fn from_path(path: &std::path::Path) -> Self {
        if crate::discover::is_cpp_path(path) || crate::discover::is_cpp_header_path(path) {
            SourceLang::Cpp
        } else {
            SourceLang::C
        }
    }
}

thread_local! {
    static PARSER_C: RefCell<Parser> = RefCell::new(make_parser(SourceLang::C));
    static PARSER_CPP: RefCell<Parser> = RefCell::new(make_parser(SourceLang::Cpp));
}

fn make_parser(lang: SourceLang) -> Parser {
    let mut parser = Parser::new();
    match lang {
        SourceLang::C => parser
            .set_language(&tree_sitter_c::LANGUAGE.into())
            .expect("failed to set C language"),
        SourceLang::Cpp => parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("failed to set C++ language"),
    }
    parser
}

pub struct ParseResult {
    pub tree: Tree,
    pub source: Arc<str>,
}

/// Parse with the grammar matching `lang`. The C++ grammar is a superset of
/// the C grammar's node vocabulary for everything the lowering matches on, so
/// existing C paths are unaffected.
pub fn parse_source_with_lang(
    source: impl Into<Arc<str>>,
    lang: SourceLang,
) -> Result<ParseResult, String> {
    let source: Arc<str> = source.into();
    let tree = match lang {
        SourceLang::C => PARSER_C.with(|p| p.borrow_mut().parse(source.as_ref(), None)),
        SourceLang::Cpp => PARSER_CPP.with(|p| p.borrow_mut().parse(source.as_ref(), None)),
    };
    let tree = tree.ok_or_else(|| "tree-sitter returned no tree".to_string())?;
    Ok(ParseResult { tree, source })
}

pub fn parse_c_source(source: impl Into<Arc<str>>) -> Result<ParseResult, String> {
    parse_source_with_lang(source, SourceLang::C)
}

/// Rewrite only syntax in declaration-only C++ dependencies that tree-sitter
/// does not recognize. The input is for parsing; preprocessing and its LineMap
/// retain the original text. Every edit replaces bytes in place so node offsets
/// continue to identify the original source location.
pub(crate) fn normalize_dependency_cpp_syntax(
    source: &Arc<str>,
    line_map: &LineMap,
    dep_roots: &[PathBuf],
    primary_path: &Path,
) -> Arc<str> {
    if dep_roots.is_empty() {
        return Arc::clone(source);
    }
    let bytes = source.as_bytes();
    let mut changed: Option<Vec<u8>> = None;
    let is_dep_at = |offset: usize| {
        let path = if line_map.entries.is_empty() {
            primary_path
        } else {
            let Some(entry) = line_map.lookup(offset) else {
                return false;
            };
            if line_map
                .expansion_path_of(entry)
                .is_some_and(|path| !dep_roots.iter().any(|root| path.starts_with(root)))
            {
                return false;
            }
            line_map.path_of(entry)
        };
        dep_roots.iter().any(|root| path.starts_with(root))
    };
    let mut i = 0;
    while i < bytes.len() {
        // The preprocessor normally removes comments, but raw sources and
        // literals can still contain the same byte sequences as C++ syntax.
        if bytes[i..].starts_with(b"//") {
            i = line_comment_end(bytes, i);
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i = bytes[i + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(bytes.len(), |n| i + n + 4);
            continue;
        }
        if bytes[i..].starts_with(b"R\"") {
            if let Some(end) = raw_string_end(bytes, i) {
                i = end;
                continue;
            }
        }
        if bytes[i] == b'\'' && is_digit_separator(bytes, i) {
            i += 1;
            continue;
        }
        if matches!(bytes[i], b'\'' | b'"') {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if bytes[i..].starts_with(b"::*") {
            let mut start = i;
            while start > 0 && is_cpp_ident(bytes[start - 1]) {
                start -= 1;
            }
            // Qualified/template class names need a separate treatment.
            if start < i
                && (bytes[start].is_ascii_alphabetic() || bytes[start] == b'_')
                && !bytes[..start].ends_with(b"::")
                && is_dep_at(start)
                && is_dep_at(i + 2)
            {
                let output = changed.get_or_insert_with(|| bytes.to_vec());
                output[start..i + 2].fill(b' ');
                output[i + 2] = b'*';
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    changed.map_or_else(
        || Arc::clone(source),
        |bytes| Arc::from(String::from_utf8(bytes).expect("ASCII-only syntax edits")),
    )
}

fn is_cpp_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_digit_separator(bytes: &[u8], at: usize) -> bool {
    if at == 0 || at + 1 == bytes.len() || !bytes[at + 1].is_ascii_alphanumeric() {
        return false;
    }
    let mut start = at;
    while start > 0 && (is_cpp_ident(bytes[start - 1]) || bytes[start - 1] == b'\'') {
        start -= 1;
    }
    bytes[start].is_ascii_digit()
}

fn line_comment_end(bytes: &[u8], mut at: usize) -> usize {
    while let Some(relative) = bytes[at..].iter().position(|&byte| byte == b'\n') {
        let newline = at + relative;
        let before_newline = if newline > 0 && bytes[newline - 1] == b'\r' {
            newline - 1
        } else {
            newline
        };
        if before_newline > 0 && bytes[before_newline - 1] == b'\\' {
            at = newline + 1;
        } else {
            return newline;
        }
    }
    bytes.len()
}

fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let open = bytes[start + 2..]
        .iter()
        .take(17)
        .position(|&b| b == b'(')?
        + start
        + 2;
    let delimiter = &bytes[start + 2..open];
    let mut pos = open + 1;
    while pos + delimiter.len() + 1 < bytes.len() {
        if bytes[pos] == b')'
            && bytes[pos + 1..].starts_with(delimiter)
            && bytes[pos + 1 + delimiter.len()] == b'"'
        {
            return Some(pos + delimiter.len() + 2);
        }
        pos += 1;
    }
    None
}

pub fn node_text<'a>(source: &'a str, node: &Node) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

/// Nested AST parse error walk cap, matching `MAX_AST_WALK_DEPTH` in lowering.
const MAX_PARSE_WALK_DEPTH: u32 = 512;

/// Returns true if `node` represents a benign false-positive error produced by
/// tree-sitter rather than a genuine syntax error in the source code.
///
/// Specifically: upstream tree-sitter C and C++ grammars mandate a declarator
/// before a bitfield clause in field declarations (`_field_declaration_declarator`),
/// but in standard C/C++ unnamed bitfields (e.g. `unsigned : 0;`, `int : 4;`)
/// have no declarator. Tree-sitter recovers by inserting a `(MISSING field_identifier)`
/// immediately preceding the `bitfield_clause`.
fn is_benign_parse_error(node: Node) -> bool {
    node.is_missing()
        && node.kind() == "field_identifier"
        && node
            .next_named_sibling()
            .is_some_and(|s| s.kind() == "bitfield_clause")
        && node
            .parent()
            .is_some_and(|p| p.kind() == "field_declaration")
}

/// Returns true if the parse tree contains genuine syntax errors.
///
/// False-positive error nodes caused by upstream grammar limitations (such as
/// unnamed bitfields) are ignored.
pub fn has_parse_errors(tree: &Tree) -> bool {
    let root = tree.root_node();
    if !root.has_error() {
        return false;
    }
    has_unrecovered_parse_errors(root)
}

/// Returns true if the AST subtree rooted at `node` contains genuine syntax errors.
pub fn has_unrecovered_parse_errors(node: Node) -> bool {
    has_unrecovered_parse_errors_depth(node, 0)
}

fn has_unrecovered_parse_errors_depth(node: Node, depth: u32) -> bool {
    if !node.has_error() || is_benign_parse_error(node) {
        return false;
    }
    if depth >= MAX_PARSE_WALK_DEPTH {
        return true;
    }
    let mut any_unrecovered_child = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() && has_unrecovered_parse_errors_depth(child, depth + 1) {
            any_unrecovered_child = true;
            break;
        }
    }
    if any_unrecovered_child {
        return true;
    }
    node.is_error() || node.is_missing()
}

/// Collect innermost unrecovered parse error and missing nodes in the subtree
/// rooted at `node`, ignoring benign tree-sitter artifacts (such as unnamed bitfields).
///
/// If a parent node (e.g. `ERROR`) contains children with unrecovered errors,
/// only the children are collected. If an `ERROR` node contains no child errors
/// (or only benign errors), the `ERROR` node itself is collected.
pub fn collect_unrecovered_parse_errors<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    collect_unrecovered_parse_errors_depth(node, out, 0);
}

fn collect_unrecovered_parse_errors_depth<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>, depth: u32) {
    if !node.has_error() || is_benign_parse_error(node) {
        return;
    }
    if depth >= MAX_PARSE_WALK_DEPTH {
        out.push(node);
        return;
    }
    let initial_len = out.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() {
            collect_unrecovered_parse_errors_depth(child, out, depth + 1);
        }
    }
    if out.len() == initial_len && (node.is_error() || node.is_missing()) {
        out.push(node);
    }
}

#[cfg(test)]
mod dependency_cpp_tests {
    use super::*;

    #[test]
    fn normalizes_only_dependency_syntax_without_moving_locations() {
        let project = PathBuf::from("/project/main.cpp");
        let dep = PathBuf::from("/external/headers/member.hpp");
        let input: Arc<str> = Arc::from(
            "struct Project { using Member = int C::*; };\n\
             struct Dependency { using Member = int C::*; [[__nodiscard__]] friend bool operator==(Dependency, Dependency) { return true; } };\n\
             const char* dep_text = \"C::*\";\n\
             const char* dep_raw = R\"tag(C::*)tag\";\n\
             const char* text = \"C::* [[__nodiscard__]]\";\n\
             const char* raw = R\"tag(C::* [[nodiscard]])tag\";\n",
        );
        let mut map = LineMap::new();
        let project_id = map.intern_file(&project);
        let dep_id = map.intern_file(&dep);
        map.push(0, project_id, 1, 1);
        let dep_start = input.find("struct Dependency").unwrap();
        map.push(dep_start, dep_id, 1, 1);
        let project_start = input.find("const char* text").unwrap();
        map.push(project_start, project_id, 5, 1);
        let output = normalize_dependency_cpp_syntax(
            &input,
            &map,
            &[PathBuf::from("/external/headers")],
            &project,
        );
        assert_eq!(input.len(), output.len());
        assert_eq!(input.matches('\n').count(), output.matches('\n').count());
        assert!(output.contains("using Member = int C::*; };"));
        assert!(output.contains("using Member = int    *;"));
        assert!(output.contains("[[__nodiscard__]] friend"));
        assert!(output.contains("dep_text = \"C::*\""));
        assert!(output.contains("dep_raw = R\"tag(C::*)tag\""));
        assert!(output.contains("\"C::* [[__nodiscard__]]\""));
        assert!(output.contains("R\"tag(C::* [[nodiscard]])tag\""));
        assert_eq!(map.lookup(dep_start).unwrap().file, dep_id);
    }

    #[test]
    fn dependency_member_pointer_approximation_recovers_declarations() {
        let source: Arc<str> =
            Arc::from("template<class T, class C> struct Traits { using Member = T C::*; };\n");
        let raw = parse_source_with_lang(Arc::clone(&source), SourceLang::Cpp).unwrap();
        assert!(has_parse_errors(&raw.tree));
        let normalized = normalize_dependency_cpp_syntax(
            &source,
            &LineMap::new(),
            &[PathBuf::from("/external")],
            Path::new("/external/member.hpp"),
        );
        let parsed = parse_source_with_lang(normalized, SourceLang::Cpp).unwrap();
        assert!(!has_parse_errors(&parsed.tree));
    }

    #[test]
    fn digit_separators_do_not_hide_later_dependency_syntax() {
        let source: Arc<str> = Arc::from(
            "constexpr int number = 1'000; using First = int C::*; \
             constexpr int hex = 0xAB'CD; using Second = int C::*; \
             char letter = 'x'; using Third = int C::*;",
        );
        let normalized = normalize_dependency_cpp_syntax(
            &source,
            &LineMap::new(),
            &[PathBuf::from("/external")],
            Path::new("/external/member.hpp"),
        );
        assert!(normalized.contains("1'000; using First = int    *;"));
        assert!(normalized.contains("0xAB'CD; using Second = int    *;"));
        assert!(normalized.contains("'x'; using Third = int    *;"));
    }

    #[test]
    fn spliced_line_comment_does_not_rewrite_its_continuation() {
        let source: Arc<str> = Arc::from(concat!(
            "// commented C::* \\\n",
            "still commented C::*\nusing Real = int C::*;\n"
        ));
        let normalized = normalize_dependency_cpp_syntax(
            &source,
            &LineMap::new(),
            &[PathBuf::from("/external")],
            Path::new("/external/member.hpp"),
        );
        assert!(normalized.contains("still commented C::*"));
        assert!(normalized.contains("using Real = int    *;"));
    }
}
