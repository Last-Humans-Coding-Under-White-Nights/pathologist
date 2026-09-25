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

/// Returns true if `node` represents a benign false-positive error produced by
/// tree-sitter rather than a genuine syntax error in the source code.
///
/// Specifically: upstream tree-sitter C and C++ grammars mandate a declarator
/// before a bitfield clause in field declarations (`_field_declaration_declarator`),
/// but in standard C/C++ unnamed bitfields (e.g. `unsigned : 0;`, `int : 4;`)
/// have no declarator. Tree-sitter recovers by inserting a `(MISSING field_identifier)`
/// immediately preceding the `bitfield_clause`.
pub fn is_benign_parse_error(node: Node) -> bool {
    if node.is_missing()
        && node.kind() == "field_identifier"
        && node
            .next_sibling()
            .is_some_and(|s| s.kind() == "bitfield_clause")
        && node
            .parent()
            .is_some_and(|p| p.kind() == "field_declaration")
    {
        return true;
    }
    false
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

fn has_unrecovered_parse_errors(node: Node) -> bool {
    if !node.has_error() {
        return false;
    }
    if is_benign_parse_error(node) {
        return false;
    }
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() && has_unrecovered_parse_errors(child) {
            return true;
        }
    }
    false
}
