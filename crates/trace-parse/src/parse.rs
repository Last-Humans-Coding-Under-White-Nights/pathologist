use std::cell::RefCell;
use std::sync::Arc;
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
