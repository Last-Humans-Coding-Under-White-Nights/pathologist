//! Recovery for the C++ grammar's expression/declaration ambiguity in #219.
//! Rules and source-location contract: docs/ANALYSIS.md, C++ parse-input normalization.

use super::*;
use crate::parse::{parse_source_with_lang, SourceLang};
use tree_sitter::{InputEdit, Point, Tree};

/// Scratch nodes are lowered synchronously; their trees remain alive for the
/// unit's caches keyed by node ID.
pub(super) fn try_lower(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
) -> bool {
    if !ctx.is_cpp
        || !matches!(node.cached_kind(), "declaration" | "expression_statement")
        || node_is_from_ignored_macro(ctx, node)
        || !node_text(source, &node).contains('*')
        || !ambiguous_callback(program, ctx, source, node)
    {
        return false;
    }
    let Some((tree, class_wrapper)) = recover(
        node_text(source, &node),
        node.start_byte(),
        node.start_position(),
    ) else {
        return false;
    };
    let tree = Arc::new(tree);
    let root = tree.root_node();
    let declaration = if class_wrapper {
        root.named_child(0)
            .and_then(|class| class.cached_field("body"))
            .and_then(|body| body.named_child(0))
    } else {
        root.named_child(0)
    };
    let Some(declaration) = declaration else {
        return false;
    };
    if !has_empty_callback(declaration) {
        return false;
    }
    ctx.recovered_trees.push(Arc::clone(&tree));
    let outer_origin = ctx
        .recovery_origin
        .replace((node.start_byte(), node.end_byte()));
    // The recovered declaration's children retain original byte positions.
    // The synthetic class never enters the semantic scope, and the synthetic
    // body is never visited: a recovered function remains a prototype.
    lower_declaration(program, ctx, source, declaration, None);
    if let Some(caller) = ctx.current_fn {
        for declarator in declaration.children_by_field_name("declarator", &mut declaration.walk())
        {
            walk_declarator_expressions(program, ctx, source, declarator, caller, 0);
            if let Some(value) = field_initializer(declaration, declarator) {
                walk_function_body(program, ctx, source, value, caller);
            }
        }
    }
    ctx.recovery_origin = outer_origin;
    true
}

/// Walk evaluated array bounds along the declarator chain, in source order.
/// A function declarator can wrap an array of callbacks, so follow its
/// declarator too, but never descend into prototype parameters or type syntax.
fn walk_declarator_expressions(
    program: &mut Program,
    ctx: &mut LowerContext,
    source: &str,
    node: Node,
    caller: FnId,
    depth: u32,
) {
    if depth >= MAX_AST_WALK_DEPTH {
        return;
    }
    if let Some(inner) = node.cached_field("declarator") {
        walk_declarator_expressions(program, ctx, source, inner, caller, depth + 1);
    } else if matches!(
        node.cached_kind(),
        "parenthesized_declarator" | "reference_declarator" | "attributed_declarator"
    ) {
        for inner in node.named_children(&mut node.walk()) {
            walk_declarator_expressions(program, ctx, source, inner, caller, depth + 1);
        }
    }
    if node.cached_kind() == "array_declarator" {
        if let Some(size) = node.cached_field("size") {
            walk_function_body(program, ctx, source, size, caller);
        }
    }
}

/// Match the misparsed `Type(*name)()` call, admitting type names only in the
/// original scope. In particular, `factory(*p)()` must remain an expression.
fn ambiguous_callback(program: &Program, ctx: &LowerContext, source: &str, node: Node) -> bool {
    let mut pending = vec![(node, 0)];
    while let Some((node, depth)) = pending.pop() {
        if node.cached_kind() == "call_expression" {
            let empty = node.cached_field("arguments").is_some_and(|args| {
                args.named_children(&mut args.walk())
                    .all(|n| n.cached_kind() == "comment")
            });
            if empty {
                if let Some(inner) = node
                    .cached_field("function")
                    .filter(|n| n.cached_kind() == "call_expression")
                {
                    if let (Some(ty), Some(args)) = (
                        inner.cached_field("function"),
                        inner.cached_field("arguments"),
                    ) {
                        let arguments: Vec<_> = args
                            .named_children(&mut args.walk())
                            .filter(|n| n.cached_kind() != "comment")
                            .collect();
                        if arguments.len() == 1
                            && arguments[0].cached_kind() == "pointer_expression"
                            && pointer_name(arguments[0])
                        {
                            let name = normalize_qualified(node_text(source, &ty));
                            let template_type = template_type_name(ctx, source, ty, &name);
                            if ty.cached_kind() == "primitive_type"
                                || (lookup_var(ctx, program, &name).is_none()
                                    && find_in_enclosing_scopes(
                                        program,
                                        ctx,
                                        &name,
                                        &mut |candidate, _| {
                                            scoped_variable_unless_hidden(program, ctx, candidate)
                                        },
                                    )
                                    .is_none()
                                    && template_type != Some(false)
                                    && resolve_function_named(program, ctx, &name).is_none()
                                    && (is_fundamental_type_name(&name)
                                        || names_type_in_scope(program, ctx, &name)
                                        || scoped_type_desc(program, ctx, &name).is_some()
                                        || template_type == Some(true)))
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        if depth < MAX_AST_WALK_DEPTH {
            pending.extend(
                node.named_children(&mut node.walk())
                    .map(|n| (n, depth + 1)),
            );
        }
    }
    false
}

/// The nearest template binding decides whether an unqualified name can be a type.
fn template_type_name(ctx: &LowerContext, source: &str, node: Node, name: &str) -> Option<bool> {
    if !ctx.has_templates {
        return None;
    }
    // A value parameter used as a template argument does not make the
    // containing type a value (`Box<N>` still introduces a declaration).
    let name = strip_template_args(name);
    let path: Vec<_> = ancestors(ctx, node).collect();
    for params in path
        .into_iter()
        .rev()
        .filter(|n| n.cached_kind() == "template_declaration")
        .filter_map(|n| n.cached_field("parameters"))
    {
        let names = template_parameter_names(source, params);
        for (param, binding) in params
            .named_children(&mut params.walk())
            .filter(|p| p.cached_kind() != "comment")
            .zip(names)
        {
            if binding.is_some_and(|binding| spelling_mentions(&name, Spelling::Source, binding)) {
                return Some(matches!(
                    param.cached_kind(),
                    "type_parameter_declaration"
                        | "variadic_type_parameter_declaration"
                        | "optional_type_parameter_declaration"
                        | "template_template_parameter_declaration"
                ));
            }
        }
    }
    None
}

fn pointer_name(node: Node) -> bool {
    match node.cached_kind() {
        "identifier" => true,
        "pointer_expression" => {
            node.cached_field("operator")
                .is_some_and(|op| op.cached_kind() == "*")
                && node.cached_field("argument").is_some_and(pointer_name)
        }
        _ => false,
    }
}

fn has_empty_callback(node: Node) -> bool {
    let mut pending = vec![(node, 0)];
    while let Some((node, depth)) = pending.pop() {
        if is_function_pointer_declarator(node)
            && node.cached_field("parameters").is_some_and(|params| {
                params
                    .named_children(&mut params.walk())
                    .all(|n| n.cached_kind() == "comment")
            })
        {
            return true;
        }
        if depth < MAX_AST_WALK_DEPTH {
            pending.extend(
                node.named_children(&mut node.walk())
                    .map(|n| (n, depth + 1)),
            );
        }
    }
    false
}

fn recover(statement: &str, start_byte: usize, start_position: Point) -> Option<(Tree, bool)> {
    let statement = statement.trim_end();
    let header = statement.strip_suffix(';')?;
    const PREFIX: &str = "struct __trace_callback_recovery { ";
    let input = format!("{PREFIX}{statement}\n}};");
    let mut parsed = parse_source_with_lang(input, SourceLang::Cpp).ok()?;
    let root = parsed.tree.root_node();
    let member = root.named_child(0)?.cached_field("body")?;
    if !root.has_error()
        && root.named_child_count() == 1
        && member.named_child_count() == 1
        && member.named_child(0)?.cached_kind() == "field_declaration"
    {
        relocate(&mut parsed.tree, PREFIX.len(), start_byte, start_position);
        return Some((parsed.tree, true));
    }
    // Class grammar rejects a qualified free-function name. A body anchors
    // the function interpretation without changing any header token offset.
    // A leading space provides a prefix to replace even at byte zero.
    let mut parsed = parse_source_with_lang(format!(" {header} {{}}"), SourceLang::Cpp).ok()?;
    let root = parsed.tree.root_node();
    if root.has_error()
        || root.named_child_count() != 1
        || root.named_child(0)?.cached_kind() != "function_definition"
    {
        return None;
    }
    relocate(&mut parsed.tree, 1, start_byte, start_position);
    Some((parsed.tree, false))
}

/// Replace the scratch prefix with the original source's coordinate extent,
/// without reparsing. Editing the tree itself keeps parent traversal in the
/// same coordinate space as the relocated children; an offset root view does
/// not relocate the tree used by `Node::parent`.
fn relocate(tree: &mut Tree, prefix_len: usize, start_byte: usize, start_position: Point) {
    tree.edit(&InputEdit {
        start_byte: 0,
        old_end_byte: prefix_len,
        new_end_byte: start_byte,
        start_position: Point::new(0, 0),
        old_end_position: Point::new(0, prefix_len),
        new_end_position: start_position,
    });
}

/// Class grammar stores a variable initializer beside its declarator as
/// `default_value`; ordinary grammar stores it inside an `init_declarator`.
pub(super) fn field_initializer<'t>(
    declaration: Node<'t>,
    declarator: Node<'t>,
) -> Option<Node<'t>> {
    if declaration.cached_kind() != "field_declaration" {
        return None;
    }
    let mut after = false;
    for (index, child) in declaration.children(&mut declaration.walk()).enumerate() {
        if child.id() == declarator.id() {
            after = true;
        } else if after {
            match declaration.field_name_for_child(index as u32) {
                Some("declarator") => return None,
                Some("default_value") => return Some(child),
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(tree: &Tree, class_wrapper: bool) -> Node<'_> {
        let node = tree.root_node().named_child(0).unwrap();
        if class_wrapper {
            node.cached_field("body").unwrap().named_child(0).unwrap()
        } else {
            node
        }
    }

    #[test]
    fn scratch_context_recovers_both_binding_kinds() {
        for (source, pointer) in [
            ("void post(void (*cb)());", false),
            ("void (*cb)();", true),
            ("Result post(Result (*cb)() = nullptr);", false),
        ] {
            let (tree, class_wrapper) = recover(source, 0, Point::new(0, 0)).unwrap();
            assert!(class_wrapper);
            let decl = declaration(&tree, class_wrapper);
            assert!(has_empty_callback(decl));
            let declarator = decl.cached_field("declarator").unwrap();
            assert_eq!(is_function_pointer_declarator(declarator), pointer);
        }
    }

    #[test]
    fn qualified_function_fallback_remains_a_declaration_to_lowering() {
        let source = "void api::post(void (*cb)());";
        let (tree, class_wrapper) = recover(source, 0, Point::new(0, 0)).unwrap();
        assert!(!class_wrapper);
        let declarator = declaration(&tree, class_wrapper)
            .cached_field("declarator")
            .unwrap();
        assert_eq!(parse_declarator_name(source, declarator).0, "api::post");
        assert!(has_empty_callback(declarator));
        assert!(declarator.end_byte() < source.len());
    }

    #[test]
    fn scratch_nodes_retain_original_bytes_and_points() {
        let source = "// π\n  void post(\n    void (*cb)()\n  );\n";
        let start = source.find("void post").unwrap();
        let statement = source[start..].trim_end();
        let (tree, _) = recover(statement, start, Point::new(1, 2)).unwrap();
        let root = tree.root_node();
        let declaration = root
            .named_child(0)
            .unwrap()
            .cached_field("body")
            .unwrap()
            .named_child(0)
            .unwrap();
        let declarator = declaration.cached_field("declarator").unwrap();
        let name = declarator.cached_field("declarator").unwrap();
        assert_eq!(node_text(source, &name), "post");
        assert_eq!(name.start_position(), Point::new(1, 7));
        assert_eq!(name.parent().unwrap(), declarator);
        assert_eq!(declarator.parent().unwrap(), declaration);
        let parameter = declarator
            .cached_field("parameters")
            .unwrap()
            .named_child(0)
            .unwrap();
        let parameter = parameter.cached_field("declarator").unwrap();
        assert_eq!(parse_declarator_name(source, parameter).0, "cb");
        assert_eq!(node_text(source, &parameter), "(*cb)()");
        assert_eq!(parameter.start_position(), Point::new(2, 9));
    }

    #[test]
    fn incomplete_or_multiple_statements_are_not_recovered() {
        for source in [
            "void post(void (*cb)())",
            "void post(void (*cb)();",
            "void post(void (*cb)()); int another;",
            "void post(void (*cb)() + 1);",
        ] {
            assert!(recover(source, 0, Point::new(0, 0)).is_none(), "{source}");
        }
    }
}
