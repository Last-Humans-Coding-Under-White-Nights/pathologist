//! Narrow, diagnosed approximations; see docs/ANALYSIS.md, C++ parse-input normalization.
use super::*;
use std::ops::Range;

pub(crate) struct DeclarationMacroRecovery {
    pub offset: usize,
    pub message: &'static str,
}

struct Candidate {
    range: Range<usize>,
    class: Option<Range<usize>>,
}

/// Preprocessing has already expanded real definitions. Never edit tokens from
/// a macro expansion (including self-referential definitions). All proposals
/// are validated against the retry's AST before any edit is accepted.
pub(crate) fn recover_declaration_macros(
    original: ParseResult,
    line_map: &LineMap,
) -> Result<(ParseResult, Vec<DeclarationMacroRecovery>), String> {
    let source = &original.source;
    if !source.contains("FFI_EXPORT") && !source.contains("DECLARE_SINGLETON") {
        return Ok((original, Vec::new()));
    }
    let mut errors = Vec::new();
    collect_unrecovered_parse_errors(original.tree.root_node(), &mut errors);
    let bytes = source.as_bytes();
    let mut candidates = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        at = skip_ws_and_comments(bytes, at);
        if at == bytes.len() {
            break;
        }
        if let Some(end) = literal_end(bytes, at) {
            at = end;
            continue;
        }
        if !is_cpp_ident(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && is_cpp_ident(bytes[at]) {
            at += 1;
        }
        let name = &source[start..at];
        if !matches!(name, "FFI_EXPORT" | "DECLARE_SINGLETON") {
            continue;
        }
        if line_map
            .lookup(start)
            .is_some_and(|entry| entry.expansion_id != 0)
        {
            continue;
        }
        match name {
            "FFI_EXPORT" if !errors.is_empty() => candidates.push(Candidate {
                range: start..at,
                class: None,
            }),
            "DECLARE_SINGLETON" => {
                let open = skip_ws_and_comments(bytes, at);
                if bytes.get(open) != Some(&b'(') {
                    continue;
                }
                let name = skip_ws_and_comments(bytes, open + 1);
                let mut end = name;
                while end < bytes.len() && is_cpp_ident(bytes[end]) {
                    end += 1;
                }
                let close = skip_ws_and_comments(bytes, end);
                if end > name && bytes.get(close) == Some(&b')') {
                    candidates.push(Candidate {
                        range: start..close + 1,
                        class: Some(name..end),
                    });
                    at = close + 1;
                }
            }
            _ => {}
        }
    }
    if candidates.is_empty() {
        return Ok((original, Vec::new()));
    }
    let retry = |candidates: &[Candidate]| {
        let mut input = bytes.to_vec();
        for candidate in candidates {
            blank_preserving_lines(&mut input[candidate.range.clone()]);
        }
        // The ordinary normalization has already run. Use the same parser,
        // without normalizing again or changing any bytes outside our ranges.
        let source: Arc<str> =
            Arc::from(String::from_utf8(input).expect("ASCII-only syntax edits"));
        let tree = PARSER_CPP.with(|p| {
            let mut parser = p.borrow_mut();
            let tree = parser.parse(source.as_ref(), None);
            parser.reset();
            tree
        });
        tree.map(|tree| ParseResult { tree, source })
            .ok_or_else(|| "tree-sitter returned no tree".to_string())
    };
    let mut parsed = retry(&candidates)?;
    let count = candidates.len();
    candidates.retain(|candidate| supported(candidate, &parsed, source, &errors));
    if candidates.is_empty() {
        return Ok((original, Vec::new()));
    }
    if candidates.len() != count {
        parsed = retry(&candidates)?;
        // Rejected edits must not be prerequisites of accepted ones. Fall back
        // conservatively, with the original diagnostics, if they interact.
        if !candidates
            .iter()
            .all(|candidate| supported(candidate, &parsed, source, &errors))
        {
            return Ok((original, Vec::new()));
        }
    }
    let reports = candidates
        .iter()
        .map(|candidate| DeclarationMacroRecovery {
            offset: candidate.range.start,
            message: if candidate.class.is_some() {
                "recovered class around unresolved DECLARE_SINGLETON: macro-generated declarations are unknown and were not indexed"
            } else {
                "recovered declaration after unresolved FFI_EXPORT: annotation semantics were omitted"
            },
        })
        .collect();
    Ok((parsed, reports))
}

fn supported(candidate: &Candidate, parsed: &ParseResult, original: &str, errors: &[Node]) -> bool {
    let Some(mut node) = parsed
        .tree
        .root_node()
        .descendant_for_byte_range(candidate.range.start, candidate.range.end)
    else {
        return false;
    };
    if let Some(class) = &candidate.class {
        // The blank invocation must be a complete item directly in a named
        // class body, never part of a method, initializer, or nested expression.
        if node.kind() != "field_declaration_list" || node.has_error() {
            return false;
        }
        let Some(owner) = node.parent() else {
            return false;
        };
        if !matches!(owner.kind(), "class_specifier" | "struct_specifier")
            || !owner
                .child_by_field_name("name")
                .is_some_and(|name| node_text(&parsed.source, &name) == &original[class.clone()])
        {
            return false;
        }
        // No non-trivia between the preceding item and the invocation. This
        // also excludes qualified and nested uses of the exact macro name.
        let previous = node
            .children(&mut node.walk())
            .filter(|child| child.end_byte() <= candidate.range.start)
            .last();
        return previous.is_some_and(|previous| {
            skip_ws_and_comments(original.as_bytes(), previous.end_byte()) == candidate.range.start
        });
    }
    // With the prefix erased, require a complete, explicitly typed
    // function declaration/definition. Pointer/reference return declarators
    // are supported; function-pointer variables and operators are not.
    let after = skip_ws_and_comments(parsed.source.as_bytes(), candidate.range.end);
    if skip_ws_and_comments(original.as_bytes(), candidate.range.end) != after {
        return false;
    }
    if node.kind() == "translation_unit" || node.kind() == "declaration_list" {
        let Some(next) = node
            .named_children(&mut node.walk())
            .find(|n| n.start_byte() == after)
        else {
            return false;
        };
        node = next;
    }
    let statement_start = node.start_byte();
    if node.kind() == "linkage_specification" {
        let Some(body) = node.child_by_field_name("body") else {
            return false;
        };
        node = body;
    }
    if !matches!(node.kind(), "declaration" | "function_definition") || node.has_error() {
        return false;
    }
    let Some(ty) = node.child_by_field_name("type") else {
        return false;
    };
    if after != statement_start && after != ty.start_byte() {
        return false;
    }
    let Some(mut declarator) = node.child_by_field_name("declarator") else {
        return false;
    };
    if node
        .children_by_field_name("declarator", &mut node.walk())
        .count()
        != 1
    {
        return false;
    }
    for _ in 0..MAX_PARSE_WALK_DEPTH {
        if !matches!(
            declarator.kind(),
            "pointer_declarator" | "reference_declarator"
        ) {
            break;
        }
        let inner = if declarator.kind() == "reference_declarator" {
            declarator.named_child(0)
        } else {
            declarator.child_by_field_name("declarator")
        };
        let Some(inner) = inner else {
            return false;
        };
        declarator = inner;
    }
    if declarator.kind() != "function_declarator" {
        return false;
    }
    let Some(mut name) = declarator.child_by_field_name("declarator") else {
        return false;
    };
    while name.kind() == "qualified_identifier" {
        let Some(inner) = name.child_by_field_name("name") else {
            return false;
        };
        name = inner;
    }
    if name.kind() != "identifier" {
        return false;
    }
    let mut scope = node.parent();
    while let Some(parent) = scope {
        if !matches!(
            parent.kind(),
            "translation_unit"
                | "declaration_list"
                | "namespace_definition"
                | "linkage_specification"
        ) {
            return false;
        }
        scope = parent.parent();
    }
    // An error in the prefix/type, not merely elsewhere in the unit/body,
    // must justify this approximation. Unrelated errors remain in the retry.
    errors.iter().any(|error| {
        error.start_byte() < declarator.start_byte() && error.end_byte() >= candidate.range.start
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recover(source: &str) -> (ParseResult, Vec<DeclarationMacroRecovery>) {
        let parsed = parse_source_with_lang(source, SourceLang::Cpp).unwrap();
        recover_declaration_macros(parsed, &LineMap::new()).unwrap()
    }

    #[test]
    fn supported_forms_preserve_every_byte_outside_the_macro() {
        for source in [
            "FFI_EXPORT int callback();",
            "extern \"C\" { FFI_EXPORT int callback(); }",
            "FFI_EXPORT extern \"C\" int callback();",
            "extern \"C\" FFI_EXPORT int callback() { return 1; }",
            "FFI_EXPORT const char *callback(int n) { return nullptr; }",
            "FFI_EXPORT Result &callback(const Arg &arg);",
            "namespace n { FFI_EXPORT Result callback(const Arg &arg); }",
            "class Factory { int before(); DECLARE_SINGLETON(Factory) public: void after(); };",
            "struct Factory { DECLARE_SINGLETON /* hi */ (\r\n Factory\n); protected: int after; };",
        ] {
            let (parsed, reports) = recover(source);
            assert_eq!(reports.len(), 1, "{source}: {}", parsed.tree.root_node().to_sexp());
            assert!(!has_parse_errors(&parsed.tree), "{source}: {}", parsed.tree.root_node().to_sexp());
            let start = reports[0].offset;
            let end = if source[start..].starts_with("FFI_EXPORT") {
                start + "FFI_EXPORT".len()
            } else {
                start + source[start..].find(')').unwrap() + 1
            };
            let mut expected = source.as_bytes().to_vec();
            blank_preserving_lines(&mut expected[start..end]);
            assert_eq!(parsed.source.as_bytes(), expected, "{source}");
        }
    }

    #[test]
    fn unsupported_contexts_and_unknown_macros_are_unchanged() {
        for source in [
            "OTHER_EXPORT int callback();",
            "FFI_EXPORT int global;",
            "FFI_EXPORT int (*callback)(int);",
            "FFI_EXPORT int callback(), variable;",
            "FFI_EXPORT int C::operator()() { return 0; }",
            "FFI_EXPORT long callback();",
            "FFI_EXPORT FFI_EXPORT int callback();",
            "class X { FFI_EXPORT int member(); };",
            "void body() { FFI_EXPORT int local(); }",
            "FFI_EXPORT callback(); int broken = ;",
            "FFI_EXPORT int broken( ;",
            "class X { DECLARE_SINGLETON(Y) public: int after; };",
            "class X { DECLARE_SINGLETON(X, Y) public: int after; };",
            "class X { void method() { DECLARE_SINGLETON(X); } };",
            "class X { void DECLARE_SINGLETON(X); };",
            "DECLARE_SINGLETON(X) int after;",
            "class X { UNKNOWN_SINGLETON(X) public: int after; };",
            "const char *s = R\"tag(FFI_EXPORT int f(); DECLARE_SINGLETON(X))tag\";",
            "// FFI_EXPORT int f();\n/* DECLARE_SINGLETON(X) */ int broken = ;",
        ] {
            let (parsed, reports) = recover(source);
            assert!(reports.is_empty(), "{source}");
            assert_eq!(&*parsed.source, source);
        }
    }

    #[test]
    fn unrelated_errors_survive_and_rejected_candidates_are_restored() {
        let source = "FFI_EXPORT int f(); FFI_EXPORT int value; int broken = ;";
        let (parsed, reports) = recover(source);
        assert_eq!(reports.len(), 1);
        assert!(parsed.source.contains("FFI_EXPORT int value;"));
        assert!(has_parse_errors(&parsed.tree));
    }

    #[test]
    fn cold_and_replayed_expansions_keep_recovery_and_line_map_positions() {
        let scratch = tempfile::tempdir().unwrap();
        let header = scratch.path().join("api.h");
        std::fs::write(&header, "#pragma once\nextern \"C\" {\n  FFI_EXPORT int callback();\n}\nclass Factory {\n  DECLARE_SINGLETON(Factory)\npublic: void after();\n};\n").unwrap();
        let main = scratch.path().join("main.cpp");
        std::fs::write(&main, "#include \"api.h\"\n").unwrap();
        let cache: trace_preproc::ExpansionCache = Default::default();
        let opts = trace_preproc::PreprocessOptions::new()
            .for_indexing()
            .with_language(trace_preproc::Language::Cpp)
            .with_include_expansion_cache(Arc::clone(&cache));
        let mut expected = None;
        for _ in 0..2 {
            let pre = trace_preproc::preprocess_file(&main, &opts).unwrap();
            assert!(!cache.read().unwrap().is_empty());
            let parsed = parse_source_with_lang(pre.output, SourceLang::Cpp).unwrap();
            let (parsed, reports) = recover_declaration_macros(parsed, &pre.line_map).unwrap();
            assert!(!has_parse_errors(&parsed.tree));
            let positions: Vec<_> = reports
                .iter()
                .map(|report| {
                    let entry = pre.line_map.lookup(report.offset).unwrap();
                    (
                        pre.line_map.path_of(entry).to_path_buf(),
                        entry.line,
                        entry.col,
                        report.message,
                    )
                })
                .collect();
            assert_eq!(positions.len(), 2);
            assert_eq!((positions[0].1, positions[0].2), (3, 3));
            assert_eq!((positions[1].1, positions[1].2), (6, 3));
            assert_eq!(positions[0].0, header.canonicalize().unwrap());
            let snapshot = (parsed.source, parsed.tree.root_node().to_sexp(), positions);
            if let Some(expected) = &expected {
                assert_eq!(&snapshot, expected);
            } else {
                expected = Some(snapshot);
            }
        }
    }
}
