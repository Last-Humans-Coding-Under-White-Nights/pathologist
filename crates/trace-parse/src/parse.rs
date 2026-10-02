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
    let source = if lang == SourceLang::Cpp {
        normalize_cpp_parse_syntax(&source)
    } else {
        source
    };
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
        if let Some(end) = literal_end(bytes, i) {
            i = end;
            continue;
        }
        // Leave complete parenthesized function-pointer declarators to the
        // general pass. Forward recognition handles comments and qualified
        // owners without trying to reconstruct trivia backwards from `::*`.
        if bytes[i] == b'(' {
            if let Some(shape) = member_fn_pointer_shape(bytes, i) {
                i = shape.declarator_end;
                continue;
            }
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

/// Rewrite syntax in C++ parse input that tree-sitter cannot recognize without
/// parse errors. Replaces bytes in place with spaces so node offsets and line/column
/// numbers continue to identify the original source location
/// (docs/ANALYSIS.md, "C++ parse-input normalization").
///
/// In standard C++, attributes can precede `friend` declarations (e.g.
/// `[[nodiscard]] friend bool operator==(...)`), but upstream tree-sitter C++
/// grammar does not permit attribute specifiers before the `friend` keyword.
/// Blanking such attributes in parse input preserves the friend declaration and
/// all token positions without causing syntax errors.
///
/// A parenthesized member-function pointer `R (C::*)(Args...) const` (#170)
/// has its owner blanked into the ordinary function-pointer approximation the
/// grammar accepts, in every declaration context. A named typedef declarator
/// additionally loses the trailing function qualifiers (`const`, `&`,
/// `noexcept(...)`) that the grammar rejects on a named ordinary pointer.
///
/// Must not run while `PARSER_CPP` is borrowed: candidate validation parses a
/// scratch declaration on the same thread-local parser.
pub(crate) fn normalize_cpp_parse_syntax(source: &Arc<str>) -> Arc<str> {
    let friend_candidates = source.contains("friend") && source.contains("[[");
    let member_pointer_candidates =
        source.contains("::") && source.contains('*') && source.contains('(');
    let computed_base_candidates = source.contains("decltype");
    let typeof_candidates = source.contains("__typeof");
    if !friend_candidates
        && !member_pointer_candidates
        && !computed_base_candidates
        && !typeof_candidates
    {
        return Arc::clone(source);
    }
    let bytes = source.as_bytes();
    let mut changed: Option<Vec<u8>> = None;
    // A typedef's own declarator is at parenthesis depth zero after its
    // keyword, at the brace depth the keyword was seen at: a type body the
    // typedef introduces (`typedef struct { ... } (C::*M)() const;`) is
    // stepped over, and its members are fields. Track that token directly:
    // labels, attributes and ternaries before it need no separate
    // statement-boundary recognizer. Each entry is the brace depth of an
    // open typedef; one nested in such a body pushes its own.
    let mut typedef_depths: Vec<usize> = Vec::new();
    // The parenthesis depth outside each open brace, restored when it closes:
    // a brace inside a parameter list (`int = []{ ... }()`, `(int){1}`)
    // leaves the list's depth intact. Its length is the brace depth.
    let mut paren_stack: Vec<usize> = Vec::new();
    let mut paren_depth: usize = 0;
    let mut i = 0;
    while i < bytes.len() {
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
        if let Some(end) = literal_end(bytes, i) {
            i = end;
            continue;
        }
        if bytes[i..].starts_with(b"[[") {
            let mut curr = i;
            let mut attr_spans = Vec::new();
            while curr < bytes.len() && bytes[curr..].starts_with(b"[[") {
                if let Some(attr_end) = find_attribute_end(bytes, curr) {
                    attr_spans.push((curr, attr_end));
                    curr = skip_ws_and_comments(bytes, attr_end);
                } else {
                    break;
                }
            }
            if !attr_spans.is_empty() {
                let mut target = curr;
                if target + 9 <= bytes.len()
                    && &bytes[target..target + 9] == b"constexpr"
                    && (target + 9 == bytes.len() || !is_cpp_ident(bytes[target + 9]))
                {
                    target = skip_ws_and_comments(bytes, target + 9);
                    while target < bytes.len() && bytes[target..].starts_with(b"[[") {
                        if let Some(attr_end) = find_attribute_end(bytes, target) {
                            attr_spans.push((target, attr_end));
                            target = skip_ws_and_comments(bytes, attr_end);
                        } else {
                            break;
                        }
                    }
                }
                if target + 6 <= bytes.len()
                    && &bytes[target..target + 6] == b"friend"
                    && (target + 6 == bytes.len() || !is_cpp_ident(bytes[target + 6]))
                {
                    let output = changed.get_or_insert_with(|| bytes.to_vec());
                    for (start, end) in attr_spans {
                        for b in &mut output[start..end] {
                            if *b != b'\n' && *b != b'\r' {
                                *b = b' ';
                            }
                        }
                    }
                    i = target + 6;
                    continue;
                }
            }
        }
        let at_token = i == 0 || !is_cpp_ident(bytes[i - 1]);
        if at_token
            && paren_depth == 0
            && keyword_at(bytes, i, b"typedef")
            && typedef_depths.last() != Some(&paren_stack.len())
        {
            typedef_depths.push(paren_stack.len());
        }
        if at_token && keyword_at(bytes, i, b"enum") {
            // `enum class E : decltype(x)` names an underlying type, not a
            // base list: step over the `class`/`struct` so the class-head
            // recognizer never sees it. Attribute groups and other
            // identifiers (unexpanded attribute macros, `MACRO(...)`) before
            // it are skipped; any other token (`:`, `{`, `;`) ends the
            // search, so an unscoped enum's name never reaches a later head.
            let mut next = skip_ws_and_comments(bytes, i + 4);
            loop {
                if keyword_at(bytes, next, b"class") || keyword_at(bytes, next, b"struct") {
                    break;
                }
                if let Some(end) = skip_class_head_attribute(bytes, next) {
                    next = skip_ws_and_comments(bytes, end);
                } else if ident_starts(bytes, next) {
                    next = skip_ws_and_comments(bytes, ident_end(bytes, next));
                    if bytes.get(next) == Some(&b'(') {
                        let Some(end) = parenthesized_end(bytes, next) else {
                            break;
                        };
                        next = skip_ws_and_comments(bytes, end);
                    }
                } else {
                    break;
                }
            }
            i = if keyword_at(bytes, next, b"class") {
                next + 5
            } else if keyword_at(bytes, next, b"struct") {
                next + 6
            } else {
                i + 4
            };
            continue;
        }
        if at_token && typeof_candidates {
            let keyword_len = if keyword_at(bytes, i, b"__typeof__") {
                Some(10)
            } else if keyword_at(bytes, i, b"__typeof") {
                Some(8)
            } else {
                None
            };
            if let Some(len) = keyword_len {
                let already_blanked = changed.as_ref().is_some_and(|out| out[i] != bytes[i]);
                if already_blanked {
                    i += len;
                    continue;
                }
                let edit = typeof_edit(bytes, i, len).filter(|edit| match edit {
                    TypeofEdit::Unwrap { close, .. } => {
                        !heads_several_declarators(bytes, close + 1)
                    }
                    TypeofEdit::Decltype => true,
                });
                if let Some(edit) = edit {
                    let output = changed.get_or_insert_with(|| bytes.to_vec());
                    match edit {
                        TypeofEdit::Unwrap { open, close } => {
                            blank_preserving_lines(&mut output[i..=open]);
                            output[close] = b' ';
                        }
                        TypeofEdit::Decltype => {
                            output[i..i + 8].copy_from_slice(b"decltype");
                            output[i + 8..i + len].fill(b' ');
                        }
                    }
                }
                i += len;
                continue;
            }
        }
        if at_token
            && computed_base_candidates
            && (keyword_at(bytes, i, b"struct") || keyword_at(bytes, i, b"class"))
        {
            if let Some(head) = class_head_with_bases(bytes, i) {
                if let Some(edits) = computed_base_edits(bytes, &head) {
                    let output = changed.get_or_insert_with(|| bytes.to_vec());
                    for range in edits {
                        blank_preserving_lines(&mut output[range]);
                    }
                }
                // Only the keyword is consumed: the head's template
                // arguments and base list are scanned for the other shapes
                // (`struct traits<R (C::*)(A...)> : base`), reading original
                // bytes, so an edit computed above is never disturbed.
            }
        }
        match bytes[i] {
            b';' => {
                if typedef_depths.last() == Some(&paren_stack.len()) {
                    typedef_depths.pop();
                }
                paren_depth = 0;
            }
            b'{' => {
                paren_stack.push(paren_depth);
                paren_depth = 0;
            }
            b'}' => {
                paren_depth = paren_stack.pop().unwrap_or(0);
                // A scope closing over an unterminated typedef ends it.
                while typedef_depths
                    .last()
                    .is_some_and(|&d| d > paren_stack.len())
                {
                    typedef_depths.pop();
                }
            }
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'(' => {
                if let Some(shape) = member_fn_pointer_shape(bytes, i) {
                    let typedef_declarator = shape.named
                        && paren_depth == 0
                        && typedef_depths.last() == Some(&paren_stack.len());
                    if let Some(edit) = member_fn_pointer_edit(bytes, &shape, typedef_declarator) {
                        let output = changed.get_or_insert_with(|| bytes.to_vec());
                        blank_preserving_lines(&mut output[shape.owner.clone()]);
                        if let Some(suffix) = edit {
                            blank_preserving_lines(&mut output[suffix]);
                        }
                    }
                    // Resume inside the declarator so a member pointer nested
                    // in the parameter list is still found, at depth + 1.
                    i = shape.declarator_end;
                    continue;
                }
                paren_depth += 1;
            }
            _ => {}
        }
        i += 1;
    }
    changed.map_or_else(
        || Arc::clone(source),
        |bytes| Arc::from(String::from_utf8(bytes).expect("ASCII-only syntax edits")),
    )
}

/// Replace every byte except CR/LF with a space, so line and column numbers
/// after the range are unchanged.
fn blank_preserving_lines(bytes: &mut [u8]) {
    for b in bytes {
        if *b != b'\n' && *b != b'\r' {
            *b = b' ';
        }
    }
}

/// Whether the C++ grammar parses `type_text` as a type: a scratch alias
/// declaration is parsed directly on `PARSER_CPP`, outside normalization, and
/// never enters IR. The borrow is released before returning, so the caller's
/// own parse may follow; a caller already holding the parser would panic.
fn cpp_type_operand_parses(type_text: &str) -> bool {
    let probe = format!("using __trace_base_probe = {type_text};");
    PARSER_CPP.with(|p| {
        p.borrow_mut()
            .parse(probe.as_bytes(), None)
            .is_some_and(|tree| !has_parse_errors(&tree))
    })
}

/// `( owner :: * [cv] [name] ) ( params ) suffix` recognized at its opening
/// parenthesis. Byte ranges index the original source.
struct MemberFnPointerShape {
    /// `C::` or `ns::C::`, from the first owner byte through the final `::`.
    owner: std::ops::Range<usize>,
    /// Past the `)` closing the pointer declarator.
    declarator_end: usize,
    /// Past the `)` closing the parameter list.
    params_end: usize,
    /// Whether the declarator names something (`(C::*name)`).
    named: bool,
    suffix: MemberFnSuffix,
}

enum MemberFnSuffix {
    None,
    /// First suffix token start .. last suffix token end.
    Tokens(std::ops::Range<usize>),
    /// An unterminated `noexcept(` group: the shape is recognized so no pass
    /// degrades it to owner-only blanking, but nothing may be edited.
    Malformed,
}

fn ident_end(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && is_cpp_ident(bytes[at]) {
        at += 1;
    }
    at
}

fn ident_starts(bytes: &[u8], at: usize) -> bool {
    at < bytes.len() && (bytes[at].is_ascii_alphabetic() || bytes[at] == b'_')
}

fn keyword_at(bytes: &[u8], at: usize, keyword: &[u8]) -> bool {
    bytes[at..].starts_with(keyword)
        && bytes
            .get(at + keyword.len())
            .is_none_or(|&b| !is_cpp_ident(b))
}

/// Exclusive end of the string or character literal starting at `at`, or
/// `None` when `at` is not a literal start. A digit separator (`1'000`) is
/// not a literal start; an unterminated literal runs to the end of input.
fn literal_end(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes[at..].starts_with(b"R\"") {
        // `R` must be a raw-literal token, possibly with an encoding prefix,
        // not the tail of a user-defined suffix: `"a"_R"b"` is two strings.
        let mut start = at;
        while start > 0 && is_cpp_ident(bytes[start - 1]) {
            start -= 1;
        }
        if matches!(&bytes[start..at], b"" | b"u8" | b"u" | b"U" | b"L") {
            return Some(raw_string_end(bytes, at).unwrap_or(bytes.len()));
        }
    }
    if bytes[at] == b'\'' && is_digit_separator(bytes, at) {
        return None;
    }
    if !matches!(bytes[at], b'\'' | b'"') {
        return None;
    }
    let quote = bytes[at];
    let mut j = at + 1;
    while j < bytes.len() {
        if bytes[j] == b'\\' {
            j = (j + 2).min(bytes.len());
        } else if bytes[j] == quote {
            return Some(j + 1);
        } else {
            j += 1;
        }
    }
    Some(j)
}

/// Exclusive end of the group opened at `open`, skipping comments, literals,
/// escapes, and digit separators; `None` when it is not closed.
fn parenthesized_end(bytes: &[u8], open: usize) -> Option<usize> {
    debug_assert_eq!(bytes[open], b'(');
    let mut depth: usize = 0;
    let mut j = open;
    while j < bytes.len() {
        if bytes[j..].starts_with(b"//") {
            j = line_comment_end(bytes, j);
            continue;
        }
        if bytes[j..].starts_with(b"/*") {
            j = bytes[j + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |n| j + n + 4);
            continue;
        }
        if let Some(end) = literal_end(bytes, j) {
            j = end;
            continue;
        }
        match bytes[j] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn member_fn_pointer_shape(bytes: &[u8], open: usize) -> Option<MemberFnPointerShape> {
    debug_assert_eq!(bytes[open], b'(');
    let owner_start = skip_ws_and_comments(bytes, open + 1);
    let mut p = owner_start;
    // A globally qualified owner (`::ns::C::*`).
    if bytes[p..].starts_with(b"::") {
        p = skip_ws_and_comments(bytes, p + 2);
    }
    let star = loop {
        if !ident_starts(bytes, p) {
            return None;
        }
        p = ident_end(bytes, p);
        p = skip_ws_and_comments(bytes, p);
        // Template-qualified owners (`W<int>::C::*`) are not approximated.
        if !bytes[p..].starts_with(b"::") {
            return None;
        }
        p += 2;
        let next = skip_ws_and_comments(bytes, p);
        if bytes.get(next) == Some(&b'*') {
            break next;
        }
        p = next;
    };
    let owner = owner_start..p;
    p = skip_ws_and_comments(bytes, star + 1);
    while keyword_at(bytes, p, b"const") || keyword_at(bytes, p, b"volatile") {
        p = skip_ws_and_comments(bytes, ident_end(bytes, p));
    }
    let named = ident_starts(bytes, p);
    if named {
        p = skip_ws_and_comments(bytes, ident_end(bytes, p));
    }
    if bytes.get(p) != Some(&b')') {
        return None;
    }
    let declarator_end = p + 1;
    p = skip_ws_and_comments(bytes, declarator_end);
    if bytes.get(p) != Some(&b'(') {
        return None;
    }
    let params_end = parenthesized_end(bytes, p)?;
    let mut suffix = MemberFnSuffix::None;
    let mut suffix_start = None;
    p = params_end;
    loop {
        let token = skip_ws_and_comments(bytes, p);
        let end = if keyword_at(bytes, token, b"const") || keyword_at(bytes, token, b"volatile") {
            ident_end(bytes, token)
        } else if bytes[token..].starts_with(b"&&") {
            token + 2
        } else if bytes.get(token) == Some(&b'&') {
            token + 1
        } else if keyword_at(bytes, token, b"noexcept") {
            let after = skip_ws_and_comments(bytes, token + 8);
            if bytes.get(after) == Some(&b'(') {
                match parenthesized_end(bytes, after) {
                    Some(end) => end,
                    None => {
                        suffix = MemberFnSuffix::Malformed;
                        break;
                    }
                }
            } else {
                token + 8
            }
        } else {
            break;
        };
        let start = *suffix_start.get_or_insert(token);
        suffix = MemberFnSuffix::Tokens(start..end);
        p = end;
    }
    Some(MemberFnPointerShape {
        owner,
        declarator_end,
        params_end,
        named,
        suffix,
    })
}

/// The suffix range to blank besides the owner (`Some(None)` blanks the owner
/// alone), or `None` when the shape must stay as written. A named typedef
/// declarator drops its validated function qualifiers from the end of the
/// parameter list through the last qualifier token.
fn member_fn_pointer_edit(
    bytes: &[u8],
    shape: &MemberFnPointerShape,
    typedef_declarator: bool,
) -> Option<Option<std::ops::Range<usize>>> {
    match &shape.suffix {
        MemberFnSuffix::Malformed => None,
        MemberFnSuffix::None => Some(None),
        MemberFnSuffix::Tokens(tokens) if typedef_declarator => {
            let suffix = std::str::from_utf8(&bytes[tokens.clone()]).ok()?;
            cpp_type_operand_parses(&format!("int (*)(int) {suffix}"))
                .then_some(Some(shape.params_end..tokens.end))
        }
        MemberFnSuffix::Tokens(_) => Some(None),
    }
}

enum TypeofEdit {
    /// Blank the keyword through `(` at `open` and the `)` at `close`,
    /// leaving the simple type operand in place.
    Unwrap { open: usize, close: usize },
    /// Re-spell the keyword as `decltype`, padded to the keyword's length.
    Decltype,
}

#[derive(Debug, PartialEq, Eq)]
enum TypeofOperandKind {
    /// A complete simple type: fundamental words, or a (qualified) name
    /// with at least one pointer/reference layer or cv qualifier.
    SimpleType,
    /// An expression, or a bare name the scanner cannot classify.
    ExpressionOrName,
    /// Starts like a type but is not the simple shape (arrays, function
    /// declarators, template-ids, malformed modifier runs): left as written.
    Unsupported,
}

/// Tokens of a typeof operand outside comments: identifiers (with `::`
/// joined into one qualified token), `*`, `&`, `&&`, or anything else as a
/// one-byte token.
fn operand_tokens(bytes: &[u8]) -> Vec<&[u8]> {
    let mut tokens = Vec::new();
    let mut p = skip_ws_and_comments(bytes, 0);
    while p < bytes.len() {
        let start = p;
        if ident_starts(bytes, p) || bytes[p..].starts_with(b"::") {
            // The preprocessor may re-space a qualified name (`ns:: Type`).
            loop {
                if bytes[p..].starts_with(b"::") {
                    p = skip_ws_and_comments(bytes, p + 2);
                }
                if !ident_starts(bytes, p) {
                    break;
                }
                p = ident_end(bytes, p);
                let next = skip_ws_and_comments(bytes, p);
                if !bytes[next..].starts_with(b"::") {
                    break;
                }
                p = next;
            }
        } else if bytes[p..].starts_with(b"&&") {
            p += 2;
        } else {
            p += 1;
        }
        tokens.push(&bytes[start..p]);
        p = skip_ws_and_comments(bytes, p);
    }
    tokens
}

fn is_cv(token: &[u8]) -> bool {
    token == b"const" || token == b"volatile"
}

/// Classify the text between a typeof's parentheses. The grammar is
/// deliberately bounded: `[cv] core [cv] (* [cv])* [& | &&]`, where `core`
/// is one or more fundamental words or one qualified name. A bare name with
/// no layer or cv qualifier is ambiguous and goes to `decltype`; an
/// operator with a right operand (`x * y`) is an expression.
fn classify_typeof_operand(bytes: &[u8]) -> TypeofOperandKind {
    let tokens = operand_tokens(bytes);
    let mut p = 0;
    while tokens.get(p).is_some_and(|t| is_cv(t)) {
        p += 1;
    }
    let Some(first) = tokens.get(p) else {
        return TypeofOperandKind::ExpressionOrName;
    };
    let fundamental_word = |t: &[u8]| {
        t != b"auto"
            && std::str::from_utf8(t).is_ok_and(crate::cpp_type_names::is_fundamental_type_name)
    };
    let core_is_fundamental;
    if fundamental_word(first) {
        core_is_fundamental = true;
        while tokens
            .get(p)
            .is_some_and(|t| fundamental_word(t) || is_cv(t))
        {
            p += 1;
        }
    } else if ident_starts(first, 0) || first.starts_with(b"::") {
        core_is_fundamental = false;
        p += 1;
    } else {
        return TypeofOperandKind::ExpressionOrName;
    }
    let mut layers = 0;
    while tokens.get(p).is_some_and(|t| is_cv(t) || *t == b"*") {
        if tokens[p] == b"*" {
            layers += 1;
        }
        p += 1;
    }
    if tokens.get(p).is_some_and(|t| *t == b"&" || *t == b"&&") {
        layers += 1;
        p += 1;
    }
    if p == tokens.len() {
        if core_is_fundamental || layers > 0 || tokens.iter().any(|t| is_cv(t)) {
            TypeofOperandKind::SimpleType
        } else {
            TypeofOperandKind::ExpressionOrName
        }
    } else if core_is_fundamental {
        // `int[2]`, `void (*)(int)`, `int const int`: type-like but not the
        // simple shape.
        TypeofOperandKind::Unsupported
    } else {
        TypeofOperandKind::ExpressionOrName
    }
}

/// The edit for a `__typeof__`/`__typeof` keyword of `len` bytes at `at`, or
/// `None` to leave the spelling alone.
fn typeof_edit(bytes: &[u8], at: usize, len: usize) -> Option<TypeofEdit> {
    let open = skip_ws_and_comments(bytes, at + len);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let end = parenthesized_end(bytes, open)?;
    let operand = &bytes[open + 1..end - 1];
    match classify_typeof_operand(operand) {
        TypeofOperandKind::SimpleType => {
            // Validate the token run as a type before erasing the wrapper, so
            // a malformed modifier sequence is not turned into a different
            // declaration.
            let text = std::str::from_utf8(operand).ok()?;
            cpp_type_operand_parses(text).then_some(TypeofEdit::Unwrap {
                open,
                close: end - 1,
            })
        }
        TypeofOperandKind::ExpressionOrName => Some(TypeofEdit::Decltype),
        TypeofOperandKind::Unsupported => None,
    }
}

/// Whether the declaration a type operand ending at `after` heads goes on
/// to a second declarator. Unwrapping there would give the later
/// declarators only the operand's base type (`typedef __typeof__(int *) p,
/// q;` would make `q` an `int`). A `,` at bracket depth zero counts once the
/// declaration's `;` is reached: a file-scope, block-scope, `for`-init or
/// `if`/`switch`-init declaration. A group closing first (`)`, `]`, `}`) or a
/// `>` before any `=` ends a parameter list or an enclosing template argument
/// list, whose commas separate other entities. Template arguments after the
/// operand (`= g<int, int>()`) are skipped as a group when their `<`/`>`
/// balance; a `<` left open is a comparison, and a comma after it still
/// counts. A `,` or `>` right after the operand ends a template argument.
fn heads_several_declarators(bytes: &[u8], after: usize) -> bool {
    let mut j = skip_ws_and_comments(bytes, after);
    if matches!(bytes.get(j), Some(b',' | b'>')) {
        return false;
    }
    let mut depth: usize = 0;
    let mut angle: usize = 0;
    let mut initializer = false;
    let mut comma = false;
    let mut angle_comma = false;
    while j < bytes.len() {
        let skipped = skip_ws_and_comments(bytes, j);
        if skipped != j {
            j = skipped;
            continue;
        }
        if let Some(end) = literal_end(bytes, j) {
            j = end;
            continue;
        }
        let previous = j.checked_sub(1).map(|p| bytes[p]);
        match bytes[j] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => return false,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth == 0 => return comma || (angle > 0 && angle_comma),
            b'=' if depth == 0 => initializer = true,
            b'<' if previous.is_some_and(is_cpp_ident) => angle += 1,
            b'>' if previous == Some(b'-') => {}
            b'>' if angle > 0 => angle -= 1,
            b'>' if depth == 0 && !initializer => return false,
            b',' if depth == 0 && angle > 0 => angle_comma = true,
            b',' if depth == 0 => comma = true,
            _ => {}
        }
        j += 1;
    }
    false
}

/// A class head `struct Name<...> final : bases {` recognized at its
/// `struct`/`class` keyword, with every top-level base entry.
struct ClassHeadWithBases {
    /// The `:` opening the base list.
    colon: usize,
    entries: Vec<BaseEntry>,
}

struct BaseEntry {
    /// From the first modifier or type byte through the last byte of the
    /// entry (a pack's `...` included).
    range: std::ops::Range<usize>,
    /// `decltype(expression)` text to validate when the whole entry is a
    /// computed base; `None` for a concrete entry.
    computed: Option<std::ops::Range<usize>>,
    /// The `,` following this entry, if any.
    comma: Option<usize>,
}

/// Skip one attribute-like group after the class keyword or name:
/// `[[...]]`, `alignas(...)`, `__attribute__((...))`.
fn skip_class_head_attribute(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes[at..].starts_with(b"[[") {
        return find_attribute_end(bytes, at);
    }
    for keyword in [&b"alignas"[..], b"__attribute__"] {
        if keyword_at(bytes, at, keyword) {
            let open = skip_ws_and_comments(bytes, at + keyword.len());
            if bytes.get(open) == Some(&b'(') {
                return parenthesized_end(bytes, open);
            }
            return None;
        }
    }
    None
}

/// Exclusive end of a template argument list opened at `open` (`<`),
/// tracking nesting and skipping parenthesized groups whose comparison
/// operators do not count. `None` when unclosed.
fn template_args_end(bytes: &[u8], open: usize) -> Option<usize> {
    debug_assert_eq!(bytes[open], b'<');
    let mut depth: usize = 0;
    let mut j = open;
    while j < bytes.len() {
        let skipped = skip_ws_and_comments(bytes, j);
        if skipped != j {
            j = skipped;
            continue;
        }
        if let Some(end) = literal_end(bytes, j) {
            j = end;
            continue;
        }
        match bytes[j] {
            b'(' => {
                j = parenthesized_end(bytes, j)?;
                continue;
            }
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            b';' | b'{' | b'}' => return None,
            _ => {}
        }
        j += 1;
    }
    None
}

/// Exclusive end of a (possibly qualified, possibly templated) class name
/// starting at `at`: `Outer::Nested`, `has_foo<T, void_t<...>>`,
/// `Outer::Tmpl<T>`.
fn class_name_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut p = at;
    // A globally qualified name (`::Base`).
    if bytes[p..].starts_with(b"::") {
        p = skip_ws_and_comments(bytes, p + 2);
    }
    loop {
        if !ident_starts(bytes, p) {
            return None;
        }
        let mut end = ident_end(bytes, p);
        let next = skip_ws_and_comments(bytes, end);
        let next = if bytes.get(next) == Some(&b'<') {
            end = template_args_end(bytes, next)?;
            skip_ws_and_comments(bytes, end)
        } else {
            next
        };
        if bytes[next..].starts_with(b"::") {
            p = skip_ws_and_comments(bytes, next + 2);
            continue;
        }
        return Some(end);
    }
}

fn class_head_with_bases(bytes: &[u8], keyword: usize) -> Option<ClassHeadWithBases> {
    let mut p = keyword
        + if keyword_at(bytes, keyword, b"struct") {
            6
        } else {
            5
        };
    p = skip_ws_and_comments(bytes, p);
    while let Some(end) = skip_class_head_attribute(bytes, p) {
        p = skip_ws_and_comments(bytes, end);
    }
    p = skip_ws_and_comments(bytes, class_name_end(bytes, p)?);
    loop {
        if keyword_at(bytes, p, b"final") {
            p = skip_ws_and_comments(bytes, p + 5);
        } else if let Some(end) = skip_class_head_attribute(bytes, p) {
            p = skip_ws_and_comments(bytes, end);
        } else {
            break;
        }
    }
    if bytes.get(p) != Some(&b':') || bytes[p..].starts_with(b"::") {
        return None;
    }
    let colon = p;
    p = colon + 1;
    let mut entries = Vec::new();
    loop {
        let start = skip_ws_and_comments(bytes, p);
        let mut q = start;
        while ["public", "private", "protected", "virtual"]
            .iter()
            .any(|k| keyword_at(bytes, q, k.as_bytes()))
        {
            q = skip_ws_and_comments(bytes, ident_end(bytes, q));
        }
        let mut computed = None;
        let mut end;
        if keyword_at(bytes, q, b"decltype") {
            let open = skip_ws_and_comments(bytes, q + 8);
            if bytes.get(open) != Some(&b'(') {
                return None;
            }
            end = parenthesized_end(bytes, open)?;
            let after = skip_ws_and_comments(bytes, end);
            if bytes[after..].starts_with(b"::") {
                // `decltype(f())::base` is a qualified concrete spelling.
                end = class_name_end(bytes, skip_ws_and_comments(bytes, after + 2))?;
            } else {
                computed = Some(q..end);
            }
        } else {
            if !ident_starts(bytes, q) && !bytes[q..].starts_with(b"::") {
                return None;
            }
            end = class_name_end(bytes, q)?;
        }
        let after = skip_ws_and_comments(bytes, end);
        if bytes[after..].starts_with(b"...") {
            end = after + 3;
        }
        let after = skip_ws_and_comments(bytes, end);
        match bytes.get(after) {
            Some(b',') => {
                entries.push(BaseEntry {
                    range: start..end,
                    computed,
                    comma: Some(after),
                });
                p = after + 1;
            }
            Some(b'{') => {
                entries.push(BaseEntry {
                    range: start..end,
                    computed,
                    comma: None,
                });
                return Some(ClassHeadWithBases { colon, entries });
            }
            _ => return None,
        }
    }
}

/// Ranges to blank so only concrete base entries remain: each validated
/// computed entry, the separators it owned, and the colon when nothing is
/// left. `None` leaves the whole base list as written: no computed entry, or
/// one whose operand the grammar itself rejects (which must stay diagnosed).
fn computed_base_edits(
    bytes: &[u8],
    head: &ClassHeadWithBases,
) -> Option<Vec<std::ops::Range<usize>>> {
    if head.entries.iter().all(|e| e.computed.is_none()) {
        return None;
    }
    for entry in &head.entries {
        if let Some(computed) = &entry.computed {
            let text = std::str::from_utf8(&bytes[computed.clone()]).ok()?;
            if !cpp_type_operand_parses(text) {
                return None;
            }
        }
    }
    let mut edits = Vec::new();
    let mut previous_retained = false;
    for (index, entry) in head.entries.iter().enumerate() {
        if entry.computed.is_some() {
            edits.push(entry.range.clone());
        }
        // The comma before a retained entry survives only when a retained
        // entry precedes it; every other comma belongs to an omitted entry.
        if index > 0 {
            let comma = head.entries[index - 1]
                .comma
                .expect("inner entries end in a comma");
            if entry.computed.is_some() || !previous_retained {
                edits.push(comma..comma + 1);
            }
        }
        previous_retained |= entry.computed.is_none();
    }
    if !previous_retained {
        edits.push(head.colon..head.colon + 1);
    }
    Some(edits)
}

fn find_attribute_end(bytes: &[u8], start: usize) -> Option<usize> {
    if !bytes[start..].starts_with(b"[[") {
        return None;
    }
    let mut j = start + 2;
    let mut paren_depth: usize = 0;
    while j < bytes.len() {
        if bytes[j..].starts_with(b"//") {
            j = line_comment_end(bytes, j);
            continue;
        }
        if bytes[j..].starts_with(b"/*") {
            j = bytes[j + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |n| j + n + 4);
            continue;
        }
        if bytes[j..].starts_with(b"R\"") {
            if let Some(end) = raw_string_end(bytes, j) {
                j = end;
                continue;
            }
        }
        if bytes[j] == b'\'' && is_digit_separator(bytes, j) {
            j += 1;
            continue;
        }
        if matches!(bytes[j], b'\'' | b'"') {
            let quote = bytes[j];
            j += 1;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j = (j + 2).min(bytes.len());
                } else if bytes[j] == quote {
                    j += 1;
                    break;
                } else {
                    j += 1;
                }
            }
            continue;
        }
        if bytes[j] == b'(' {
            paren_depth += 1;
            j += 1;
            continue;
        }
        if bytes[j] == b')' {
            paren_depth = paren_depth.saturating_sub(1);
            j += 1;
            continue;
        }
        if paren_depth == 0 && bytes[j..].starts_with(b"]]") {
            return Some(j + 2);
        }
        j += 1;
    }
    None
}

fn skip_ws_and_comments(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if bytes[at..].starts_with(b"//") {
            at = line_comment_end(bytes, at);
            continue;
        }
        if bytes[at..].starts_with(b"/*") {
            at = bytes[at + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |n| at + n + 4);
            continue;
        }
        break;
    }
    at
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
    (node.is_missing()
        && node.kind() == "field_identifier"
        && node
            .next_named_sibling()
            .is_some_and(|s| s.kind() == "bitfield_clause")
        && node
            .parent()
            .is_some_and(|p| p.kind() == "field_declaration"))
        || (node.is_error()
            && node.parent().is_some_and(|p| p.kind() == "call_expression")
            && is_explicit_operator_error(node))
}

fn is_explicit_operator_error(node: Node) -> bool {
    let mut cursor = node.walk();
    let mut children = node.children(&mut cursor).filter(|c| c.kind() != "comment");
    let Some(first) = children.next() else {
        return false;
    };
    if first.kind() != "." && first.kind() != "->" {
        return false;
    }
    let mut next = children.next();
    if next.as_ref().is_some_and(|c| c.kind() == "template") {
        next = children.next();
    }
    next.is_some_and(|c| c.kind() == "operator_name" && !c.has_error()) && children.next().is_none()
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

    #[test]
    fn normalize_cpp_parse_syntax_blanks_attributes_before_friend() {
        let input: Arc<str> = Arc::from(
            "struct Value {\n\
             \x20   [[nodiscard]] friend bool operator==(Value a, Value b);\n\
             \x20   [[__nodiscard__]] friend bool operator!=(Value a, Value b);\n\
             \x20   [[nodiscard]] /* note */ friend bool operator<(Value a, Value b);\n\
             \x20   [[nodiscard]]\n\x20   friend bool operator<=(Value a, Value b);\n\
             \x20   [[deprecated(\"old (use new)\")]] [[nodiscard]] friend bool operator>(Value a, Value b);\n\
             };\n\
             [[nodiscard]] bool regular_fn();\n\
             void friend_function();\n\
             const char* str = \"[[nodiscard]] friend\";\n\
             const char* raw = R\"tag([[nodiscard]] friend)tag\";\n\
             // [[nodiscard]] friend comment\n\
             /* [[nodiscard]] friend block comment */\n",
        );
        let output = normalize_cpp_parse_syntax(&input);
        assert_eq!(input.len(), output.len());
        assert_eq!(input.matches('\n').count(), output.matches('\n').count());

        // Attributes before friend should be blanked with spaces
        let single_attr = " ".repeat("[[nodiscard]]".len());
        assert!(output.contains(&format!(
            "    {single_attr} friend bool operator==(Value a, Value b);"
        )));
        let gnu_attr = " ".repeat("[[__nodiscard__]]".len());
        assert!(output.contains(&format!(
            "    {gnu_attr} friend bool operator!=(Value a, Value b);"
        )));
        assert!(output.contains(&format!(
            "    {single_attr} /* note */ friend bool operator<(Value a, Value b);"
        )));
        assert!(output.contains(&format!(
            "    {single_attr}\n    friend bool operator<=(Value a, Value b);"
        )));
        let multi_attr = " ".repeat("[[deprecated(\"old (use new)\")]] [[nodiscard]]".len());
        assert!(output.contains(&format!(
            "    {multi_attr} friend bool operator>(Value a, Value b);"
        )));

        // Syntax not before friend or inside literals/comments must remain intact
        assert!(output.contains("[[nodiscard]] bool regular_fn();"));
        assert!(output.contains("void friend_function();"));
        assert!(output.contains("\"[[nodiscard]] friend\""));
        assert!(output.contains("R\"tag([[nodiscard]] friend)tag\""));
        assert!(output.contains("// [[nodiscard]] friend comment"));
        assert!(output.contains("/* [[nodiscard]] friend block comment */"));

        // Parsed tree has no unrecovered errors
        let parsed = parse_source_with_lang(output, SourceLang::Cpp).unwrap();
        assert!(!has_parse_errors(&parsed.tree));

        let test_src = "struct S {\n\
             \x20   [[nodiscard]] constexpr friend bool operator==(S, S);\n\
             \x20   constexpr [[nodiscard]] friend bool operator!=(S, S);\n\
             \x20   [[deprecated(\"msg\", 1'000)]] friend bool operator<(S, S);\n\
             \x20   [[nodiscard]] constexpr [[deprecated(\"old\", 2'000)]] friend bool operator<=(S, S);\n\
             };\n";
        let parsed_test = parse_source_with_lang(test_src, SourceLang::Cpp).unwrap();
        assert!(!has_parse_errors(&parsed_test.tree));
    }
    /// The scratch validation parse is a prerequisite of the computed-base
    /// and typedef-suffix rewrites: valid dependent operands must pass and
    /// malformed ones must fail, and the parser borrow must be released
    /// between calls so the real parse that follows does not panic.
    #[test]
    fn computed_base_validation_probe_parses_dependent_call() {
        for valid in [
            "decltype(make<T>())",
            "decltype(T::template fn<U>())",
            "decltype(this->template fn<U>())",
            "decltype(f<T, U>(0) + U())",
            "decltype(ns::value)",
            "int (*)(int) const & noexcept(false)",
        ] {
            assert!(cpp_type_operand_parses(valid), "{valid}");
        }
        for malformed in [
            "decltype(f() +)",
            "decltype()",
            "decltype(make<T>()",
            "int (*)(int) noexcept(1 +)",
        ] {
            assert!(!cpp_type_operand_parses(malformed), "{malformed}");
        }
        // Validation inside normalization, then the unit parse on the same
        // thread-local parser.
        let source: Arc<str> =
            "template<class T> T make();\nstruct A {};\ntemplate<class T> struct D : decltype(make<T>()), A {};\n".into();
        for _ in 0..3 {
            let parsed = parse_source_with_lang(Arc::clone(&source), SourceLang::Cpp).unwrap();
            assert!(!has_parse_errors(&parsed.tree));
            assert!(
                parsed
                    .source
                    .contains("struct D :                      A {}"),
                "{}",
                parsed.source
            );
        }
    }

    /// The dependency pass must leave a parenthesized member-function pointer
    /// to the general pass even when a comment separates `(` from the owner;
    /// owner-only blanking there would strand the typedef's `const`.
    #[test]
    fn dependency_pass_skips_commented_member_function_pointer_shape() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("api.hpp");
        let source: Arc<str> =
            "struct C {};\ntypedef void (/* owner */ C::*F)(int) const;\nusing DM = int C::*;\n"
                .into();
        let dep = normalize_dependency_cpp_syntax(
            &source,
            &LineMap::default(),
            std::slice::from_ref(&dir.path().to_path_buf()),
            &primary,
        );
        assert!(dep.contains("(/* owner */ C::*F)"), "{dep}");
        assert!(dep.contains("using DM = int    *;"), "{dep}");
        let general = normalize_cpp_parse_syntax(&dep);
        assert!(
            general.contains("(/* owner */    *F)(int)      ;"),
            "{general}"
        );
        let parsed = parse_source_with_lang(general, SourceLang::Cpp).unwrap();
        assert!(!has_parse_errors(&parsed.tree));
    }

    #[test]
    fn dependency_pass_leaves_commented_and_global_function_owners() {
        let dir = tempfile::tempdir().unwrap();
        for trivia in [
            "/* comment */ ",
            "// comment\n",
            "/* // */\n// (\n",
            "// continued \\\ncomment\n",
        ] {
            for owner in ["C::", "::C::", ":: /* scope */ C::", "ns::C::"] {
                let source: Arc<str> = format!(
                    "struct C {{}}; namespace ns {{ struct C {{}}; }}\ntypedef void ({trivia}{owner}*F)(int) const;\nusing DM = int C::*;\n"
                ).into();
                let dep = normalize_dependency_cpp_syntax(
                    &source,
                    &LineMap::default(),
                    &[dir.path().to_path_buf()],
                    &dir.path().join("api.hpp"),
                );
                assert!(dep.contains(&format!("({trivia}{owner}*F)")), "{dep}");
                assert!(dep.contains("using DM = int    *;"), "{dep}");
                let parsed = parse_source_with_lang(dep, SourceLang::Cpp).unwrap();
                assert!(!has_parse_errors(&parsed.tree), "{}", parsed.source);
                assert!(!parsed.source.contains(" const;"), "{}", parsed.source);
            }
        }
    }

    #[test]
    fn unclosed_raw_literals_do_not_close_syntax_groups() {
        for source in ["(R\"tag(body )", "<R\"tag(body >"] {
            assert_eq!(literal_end(source.as_bytes(), 1), Some(source.len()));
            let end = if source.starts_with('(') {
                parenthesized_end(source.as_bytes(), 0)
            } else {
                template_args_end(source.as_bytes(), 0)
            };
            assert_eq!(end, None, "{source}");
        }
        let source: Arc<str> =
            "using T = __typeof__(R\"tag(body ) ; \" typedef __typeof__(int) X;".into();
        assert_eq!(normalize_cpp_parse_syntax(&source), source);
    }

    /// A typedef's own declarator can follow a brace-delimited type body the
    /// typedef itself introduces; members inside that body are fields, and
    /// typedefs nested in class or function bodies are still declarators.
    #[test]
    fn typedef_declarator_after_its_own_type_body_drops_suffix() {
        let source: Arc<str> = "struct C {};\n\
             typedef struct { int x; } (C::*M)() const;\n\
             typedef struct { struct { int y; } in; void (C::*f)() const; } (C::*N)() const &;\n\
             typedef struct { typedef void (C::*G)() const; } (C::*P)() const;\n\
             struct H { typedef void (C::*K)() const; void (C::*field)() const; };\n\
             void body() { typedef void (C::*L)() const; }\n"
            .into();
        let normalized = normalize_cpp_parse_syntax(&source);
        assert_eq!(normalized.len(), source.len());
        for expected in [
            "typedef struct { int x; } (   *M)()      ;",
            "(   *f)() const; } (   *N)()        ;",
            "{ typedef void (   *G)()      ; } (   *P)()      ;",
            "struct H { typedef void (   *K)()      ; void (   *field)() const; };",
            "void body() { typedef void (   *L)()      ; }",
        ] {
            assert!(normalized.contains(expected), "{expected}\n{normalized}");
        }
        // A brace inside the typedef's parameter list (a lambda or compound
        // literal in a default argument) does not reset the parenthesis
        // depth: a member-pointer parameter after it keeps its qualifiers.
        for (input, expected) in [
            (
                "struct C {};\ntypedef void (*F)(int = []{ return 0; }(), void (C::*q)() const);",
                "struct C {};\ntypedef void (*F)(int = []{ return 0; }(), void (   *q)() const);",
            ),
            (
                "struct C {};\ntypedef void (*F)(int = (int){1}, void (C::*q)() const &);",
                "struct C {};\ntypedef void (*F)(int = (int){1}, void (   *q)() const &);",
            ),
        ] {
            let source: Arc<str> = input.into();
            assert_eq!(&*normalize_cpp_parse_syntax(&source), expected, "{input}");
        }
        let parsed = parse_source_with_lang(
            "struct C {};\ntypedef struct { int x; } (*M)();\n",
            SourceLang::Cpp,
        )
        .unwrap();
        assert!(!has_parse_errors(&parsed.tree), "native control");
        let parsed = parse_source_with_lang(
            "struct C {};\ntypedef struct { int x; } (C::*M)() const;\n",
            SourceLang::Cpp,
        )
        .unwrap();
        assert!(!has_parse_errors(&parsed.tree), "{}", parsed.source);
    }

    /// Unwrapping a typeof that heads several declarators would give the
    /// later ones the operand's base type only (`q` as `int`): that spelling
    /// stays as written. Parameter lists and template arguments, whose
    /// commas separate other entities, still unwrap.
    #[test]
    fn typeof_heading_several_declarators_is_not_unwrapped() {
        for kept in [
            "typedef __typeof__(int *) p, q;",
            "__typeof__(int *) a = f(1, 2), b;",
            "void g() { __typeof__(int *) c = {}, d; }",
            "void f() { for (__typeof__(int *) p = 0, q = 0;;) {} }",
            "void f(int *a) { if (__typeof__(int *) p = a, q = a; p) {} }",
            "__typeof__(int *) a = b < c, d;",
        ] {
            let source: Arc<str> = kept.into();
            assert_eq!(normalize_cpp_parse_syntax(&source), source, "{kept}");
        }
        for (input, expected) in [
            (
                "typedef __typeof__(int *) p;",
                "typedef            int *  p;",
            ),
            (
                "__typeof__(int *) a = f(1, 2);",
                "           int *  a = f(1, 2);",
            ),
            (
                "void h(__typeof__(int *) a, int b);",
                "void h(           int *  a, int b);",
            ),
            (
                "std::pair<__typeof__(int *), int> pr, ps;",
                "std::pair<           int * , int> pr, ps;",
            ),
            (
                "std::vector<__typeof__(int *)> v, w;",
                "std::vector<           int * > v, w;",
            ),
            (
                "__typeof__(int *) a = g<int, int>();",
                "           int *  a = g<int, int>();",
            ),
            (
                "std::map<__typeof__(int *) *, int> m;",
                "std::map<           int *  *, int> m;",
            ),
            (
                "std::map<__typeof__(int *) const, int> m, n;",
                "std::map<           int *  const, int> m, n;",
            ),
            (
                "void k(__typeof__(int *) a, int b = []{ return 0; }());",
                "void k(           int *  a, int b = []{ return 0; }());",
            ),
        ] {
            let source: Arc<str> = input.into();
            assert_eq!(&*normalize_cpp_parse_syntax(&source), expected, "{input}");
        }
    }

    /// A scoped enum's underlying type is not a base list, whatever
    /// attribute-like tokens sit between `enum` and `class`.
    #[test]
    fn scoped_enum_with_macro_attribute_keeps_underlying_decltype() {
        let source: Arc<str> = "int x;\n\
             enum MYATTR class E : decltype(x) { a };\n\
             enum [[deprecated]] MYATTR __attribute__((packed)) struct F : decltype(x) { b };\n"
            .into();
        assert_eq!(normalize_cpp_parse_syntax(&source), source);
        // An unscoped enum's identifiers are skipped only up to its own
        // `:`/`{`/`;`; a later class head is still rewritten.
        let source: Arc<str> = "template<class T> T make();\n\
             enum E : int { e };\n\
             enum G { g };\n\
             enum H;\n\
             template<class T> struct D : decltype(make<T>()) {};\n"
            .into();
        let normalized = normalize_cpp_parse_syntax(&source);
        assert!(
            normalized.contains(&format!(
                "struct D {} {{}};",
                " ".repeat(": decltype(make<T>())".len())
            )),
            "{normalized}"
        );
    }

    #[test]
    fn test_explicit_operator_parse() {
        let src = r#"
struct Wrapper { int* operator->(); };
template<class T>
auto pointer_of(T& value) -> decltype(value.operator->());

struct Comparable { bool operator>(const Comparable&) const; };
template<class T>
auto compare(T a, T b) -> decltype(a.operator>(b));

int after_operator_call();
void test_body(Wrapper& w, Comparable& c) {
    w.operator->();
    c.operator>(c);
    c.operator<(c);
    c.operator==(c);
    c.operator!=(c);
    c.operator<=(c);
    c.operator>=(c);
    w.operator*();
    w.operator++();
    w.operator[] (0);
    w.operator() (0);
    w.template operator->();
    Wrapper::operator->();
    w.operator ->();
    c.operator >(c);
    w . operator -> ();
    Wrapper* pw = &w;
    pw -> operator -> ();
    w./*comment*/operator->();
    w. /*comment*/ operator->();
    w.operator->/*comment*/();
    pw->/*comment*/operator->();
    c.operator<=>(c);
    c./*comment*/operator<=>(c);
    w.operator=(w);
    w.operator+(1);
    w.operator-(1);
    w.operator*(1);
    w.operator/(1);
    w.operator%(1);
    w.operator+=(1);
    w.operator-=(1);
    w.operator*=(1);
    w.operator/=(1);
    w.operator%=(1);
    w.operator&=(1);
    w.operator|=(1);
    w.operator^=(1);
    w.operator<<=(1);
    w.operator>>=(1);
    w.operator& (1);
    w.operator| (1);
    w.operator^ (1);
    w.operator~ ();
    w.operator! ();
    w.operator, (w);
    w.operator->* (1);
    w.operator++(0);
    w.operator--(0);
    w.operator--();
    w.operator()(1, 2);
    w.template operator->();
    w.template operator()<int>(1);
    w.operator()<int>(1);
}
int main() { return 0; }
"#;
        let parsed = parse_source_with_lang(src, SourceLang::Cpp).unwrap();
        assert!(!has_parse_errors(&parsed.tree));

        let complex =
            parse_source_with_lang("void f() { get_wrapper().operator->(); }", SourceLang::Cpp)
                .unwrap();
        assert!(!has_parse_errors(&complex.tree));

        let malformed_dot =
            parse_source_with_lang("void f(Wrapper& w) { w.operator.->(); }", SourceLang::Cpp)
                .unwrap();
        assert!(has_parse_errors(&malformed_dot.tree));

        let malformed_q =
            parse_source_with_lang("void f(Wrapper& w) { w.operator?->(); }", SourceLang::Cpp)
                .unwrap();
        assert!(has_parse_errors(&malformed_q.tree));
    }
}
