//! Query-time path feasibility verification using SMT (Z3).
//!
//! Verifies whether interprocedural call chains and value-flow paths are
//! dynamically feasible by extracting AST branch conditions, early-return
//! guards, and parameter bindings along the path, and solving their conjunction
//! with Z3.
//!
//! Follows Invariant 10 (determinism) and Invariant 2 (soundness:
//! unknown / timeout degrades gracefully to Feasible).

use rusqlite::Connection;

#[cfg(feature = "smt")]
use rustc_hash::FxHashMap;
#[cfg(feature = "smt")]
use std::sync::Arc;
#[cfg(feature = "smt")]
use z3::ast::Ast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFeasibility {
    Feasible,
    Infeasible,
    Unknown,
}

#[cfg(not(feature = "smt"))]
pub fn verify_call_chain(_conn: &Connection, _chain: &crate::inspect::CallChain) -> PathFeasibility {
    PathFeasibility::Feasible
}

#[cfg(not(feature = "smt"))]
pub fn verify_flow_path(_conn: &Connection, _flow_node_ids: &[i64]) -> PathFeasibility {
    PathFeasibility::Feasible
}

#[cfg(feature = "smt")]
pub fn verify_call_chain(conn: &Connection, chain: &crate::inspect::CallChain) -> PathFeasibility {
    if chain.edges.is_empty() {
        return PathFeasibility::Feasible;
    }

    let mut cache = SourceCache::default();
    let mut cfg = z3::Config::new();
    cfg.set_param_value("timeout", "1000"); // 1s per query
    let ctx = z3::Context::new(&cfg);
    let solver = z3::Solver::new(&ctx);
    let mut vars = FxHashMap::default();

    for (i, edge) in chain.edges.iter().enumerate() {
        if edge.site.path.is_empty() || edge.site.line <= 0 {
            continue;
        }

        let Some(cur_file) = cache.get(&edge.site.path) else {
            continue;
        };
        let (source, tree) = (&cur_file.0, &cur_file.1);

        // 1. Intraprocedural constraints for this call site (guards & preceding assignments)
        let constraints = extract_path_constraints(source, tree, edge.site.line);
        let step_prefix = format!("step_{i}");

        for c in constraints {
            match c {
                PathConstraint::Guard(cond_node, polarity) => {
                    if let Some(b) = node_to_bool(&ctx, cond_node, source, &step_prefix, &mut vars) {
                        if polarity {
                            solver.assert(&b);
                        } else {
                            solver.assert(&b.not());
                        }
                    }
                }
                PathConstraint::VarInit(var_name, val_node) => {
                    if let Some(val_bv) = node_to_bv(&ctx, val_node, source, &step_prefix, &mut vars) {
                        let key = format!("{step_prefix}::{var_name}");
                        let var_bv = vars
                            .entry(key.clone())
                            .or_insert_with(|| z3::ast::BV::new_const(&ctx, key.as_str(), 64))
                            .clone();
                        solver.assert(&var_bv._eq(&val_bv));
                    }
                }
            }
        }

        // 2. Interprocedural parameter binding: edge[i-1] call arguments -> edge[i] callee parameters
        if i > 0 {
            let prev_edge = &chain.edges[i - 1];
            if !prev_edge.site.path.is_empty() && prev_edge.site.line > 0 {
                if let Some(prev_file) = cache.get(&prev_edge.site.path) {
                    let (prev_src, prev_tree) = (&prev_file.0, &prev_file.1);
                    if let Some(call_expr) = find_call_node_at(prev_tree, prev_edge.site.line) {
                        if let Some(args_node) = call_expr.child_by_field_name("arguments") {
                            if let Ok((fn_path, line_start)) = find_function_location(conn, edge.caller_id) {
                                if let Some(target_file) = cache.get(&fn_path) {
                                    let (cur_src, cur_tree) = (&target_file.0, &target_file.1);
                                    if let Some(fn_def) = find_fn_def_at(cur_tree, line_start) {
                                        bind_call_arguments_to_parameters(
                                            &ctx,
                                            &solver,
                                            prev_src,
                                            args_node,
                                            &format!("step_{}", i - 1),
                                            cur_src,
                                            fn_def,
                                            &step_prefix,
                                            &mut vars,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    match solver.check() {
        z3::SatResult::Sat => PathFeasibility::Feasible,
        z3::SatResult::Unsat => PathFeasibility::Infeasible,
        z3::SatResult::Unknown => PathFeasibility::Unknown,
    }
}

#[cfg(feature = "smt")]
pub fn verify_flow_path(conn: &Connection, flow_node_ids: &[i64]) -> PathFeasibility {
    if flow_node_ids.len() < 2 {
        return PathFeasibility::Feasible;
    }

    let mut cache = SourceCache::default();
    let mut cfg = z3::Config::new();
    cfg.set_param_value("timeout", "1000");
    let ctx = z3::Context::new(&cfg);
    let solver = z3::Solver::new(&ctx);
    let mut vars = FxHashMap::default();

    for (i, &nid) in flow_node_ids.iter().enumerate() {
        let mut stmt = match conn.prepare(
            "SELECT f.path, v.line, v.col FROM flow_nodes fn \
             JOIN variables v ON v.id = fn.var_id \
             JOIN files f ON f.id = v.file_id \
             WHERE fn.id = ?1",
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let row = stmt.query_row([nid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        });

        let (path, line, _) = match row {
            Ok(r) => r,
            Err(_) => continue,
        };

        if path.is_empty() || line <= 0 {
            continue;
        }

        let Some(file_data) = cache.get(&path) else {
            continue;
        };
        let (source, tree) = (&file_data.0, &file_data.1);

        let constraints = extract_path_constraints(source, tree, line);
        let step_prefix = format!("flow_{i}");

        for c in constraints {
            match c {
                PathConstraint::Guard(cond_node, polarity) => {
                    if let Some(b) = node_to_bool(&ctx, cond_node, source, &step_prefix, &mut vars) {
                        if polarity {
                            solver.assert(&b);
                        } else {
                            solver.assert(&b.not());
                        }
                    }
                }
                PathConstraint::VarInit(var_name, val_node) => {
                    if let Some(val_bv) = node_to_bv(&ctx, val_node, source, &step_prefix, &mut vars) {
                        let key = format!("{step_prefix}::{var_name}");
                        let var_bv = vars
                            .entry(key.clone())
                            .or_insert_with(|| z3::ast::BV::new_const(&ctx, key.as_str(), 64))
                            .clone();
                        solver.assert(&var_bv._eq(&val_bv));
                    }
                }
            }
        }
    }

    match solver.check() {
        z3::SatResult::Sat => PathFeasibility::Feasible,
        z3::SatResult::Unsat => PathFeasibility::Infeasible,
        z3::SatResult::Unknown => PathFeasibility::Unknown,
    }
}

#[cfg(feature = "smt")]
type ParsedFile = Arc<(String, tree_sitter::Tree, bool)>;

#[cfg(feature = "smt")]
#[derive(Default)]
struct SourceCache {
    files: FxHashMap<String, Option<ParsedFile>>,
}

#[cfg(feature = "smt")]
impl SourceCache {
    fn get(&mut self, path: &str) -> Option<Arc<(String, tree_sitter::Tree, bool)>> {
        if !self.files.contains_key(path) {
            let parsed = (|| {
                let source = std::fs::read_to_string(path).ok()?;
                let is_cpp = path.ends_with(".cpp")
                    || path.ends_with(".cc")
                    || path.ends_with(".cxx")
                    || path.ends_with(".hpp")
                    || path.ends_with(".hxx");
                let mut parser = tree_sitter::Parser::new();
                let lang = if is_cpp {
                    tree_sitter_cpp::LANGUAGE.into()
                } else {
                    tree_sitter_c::LANGUAGE.into()
                };
                parser.set_language(&lang).ok()?;
                let tree = parser.parse(&source, None)?;
                Some(Arc::new((source, tree, is_cpp)))
            })();
            self.files.insert(path.to_string(), parsed);
        }

        self.files.get(path)?.clone()
    }
}

#[cfg(feature = "smt")]
fn find_function_location(conn: &Connection, fn_id: i64) -> Result<(String, i64), rusqlite::Error> {
    conn.query_row(
        "SELECT f.path, fn.line_start FROM functions fn \
         JOIN files f ON f.id = fn.file_id \
         WHERE fn.id = ?1",
        [fn_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

#[cfg(feature = "smt")]
fn find_fn_def_at(tree: &tree_sitter::Tree, line: i64) -> Option<tree_sitter::Node<'_>> {
    let target_row = (line.saturating_sub(1)) as usize;
    let mut stack = vec![tree.root_node()];
    while let Some(curr) = stack.pop() {
        if curr.start_position().row <= target_row && curr.end_position().row >= target_row {
            if curr.kind() == "function_definition" && curr.start_position().row == target_row {
                return Some(curr);
            }
            let mut cursor = curr.walk();
            for child in curr.children(&mut cursor) {
                stack.push(child);
            }
        }
    }
    None
}

#[cfg(feature = "smt")]
fn find_call_node_at(tree: &tree_sitter::Tree, line: i64) -> Option<tree_sitter::Node<'_>> {
    let target_row = (line.saturating_sub(1)) as usize;
    let mut stack = vec![tree.root_node()];
    while let Some(curr) = stack.pop() {
        if curr.start_position().row <= target_row && curr.end_position().row >= target_row {
            if curr.kind() == "call_expression" && curr.start_position().row == target_row {
                return Some(curr);
            }
            let mut cursor = curr.walk();
            for child in curr.children(&mut cursor) {
                stack.push(child);
            }
        }
    }
    None
}

#[cfg(feature = "smt")]
enum PathConstraint<'a> {
    Guard(tree_sitter::Node<'a>, bool),
    VarInit(&'a str, tree_sitter::Node<'a>),
}

#[cfg(feature = "smt")]
fn extract_path_constraints<'a>(
    source: &'a str,
    tree: &'a tree_sitter::Tree,
    line: i64,
) -> Vec<PathConstraint<'a>> {
    let target_row = (line.saturating_sub(1)) as usize;
    let mut constraints = Vec::new();

    // Find the innermost node on this line (preferring call_expression or statement)
    let mut best: Option<tree_sitter::Node<'a>> = None;
    let mut stack = vec![tree.root_node()];
    while let Some(curr) = stack.pop() {
        if curr.start_position().row <= target_row && curr.end_position().row >= target_row {
            if curr.kind() == "call_expression" && curr.start_position().row == target_row {
                best = Some(curr);
                break;
            }
            if curr.start_position().row == target_row {
                best = Some(curr);
            }
            let mut cursor = curr.walk();
            for child in curr.children(&mut cursor) {
                stack.push(child);
            }
        }
    }

    let Some(leaf) = best else {
        return constraints;
    };

    let mut curr = leaf;
    while let Some(parent) = curr.parent() {
        if parent.kind() == "if_statement" {
            let in_consequence = parent
                .child_by_field_name("consequence")
                .map(|c| curr.start_byte() >= c.start_byte() && curr.end_byte() <= c.end_byte())
                .unwrap_or(false);

            let in_alternative = parent
                .child_by_field_name("alternative")
                .map(|a| curr.start_byte() >= a.start_byte() && curr.end_byte() <= a.end_byte())
                .unwrap_or(false);

            if let Some(cond) = parent.child_by_field_name("condition") {
                if in_consequence {
                    constraints.push(PathConstraint::Guard(cond, true));
                } else if in_alternative {
                    constraints.push(PathConstraint::Guard(cond, false));
                }
            }
        } else if parent.kind() == "compound_statement" {
            // Check preceding statements in the same compound statement:
            // 1) early return guards
            // 2) local variable initializations / assignments
            let mut cursor = parent.walk();
            for sibling in parent.children(&mut cursor) {
                if sibling.end_byte() > curr.start_byte() {
                    break;
                }
                if sibling.kind() == "if_statement" {
                    if sibling.child_by_field_name("alternative").is_none() {
                        if let Some(csq) = sibling.child_by_field_name("consequence") {
                            if consequence_always_returns(csq) {
                                if let Some(cond) = sibling.child_by_field_name("condition") {
                                    constraints.push(PathConstraint::Guard(cond, false));
                                }
                            }
                        }
                    }
                } else if sibling.kind() == "declaration" {
                    let mut d_cursor = sibling.walk();
                    for child in sibling.children(&mut d_cursor) {
                        if child.kind() == "init_declarator" {
                            if let (Some(decl), Some(val)) = (
                                child.child_by_field_name("declarator"),
                                child.child_by_field_name("value"),
                            ) {
                                if let Some(name) = extract_decl_name(source, decl) {
                                    constraints.push(PathConstraint::VarInit(name, val));
                                }
                            }
                        }
                    }
                } else if sibling.kind() == "expression_statement" {
                    if let Some(assign) = sibling
                        .child_by_field_name("expression")
                        .or_else(|| sibling.named_child(0))
                    {
                        if assign.kind() == "assignment_expression" {
                            if let (Some(left), Some(right)) = (
                                assign.child_by_field_name("left"),
                                assign.child_by_field_name("right"),
                            ) {
                                if let Some(name) = extract_decl_name(source, left) {
                                    constraints.push(PathConstraint::VarInit(name, right));
                                }
                            }
                        }
                    }
                }
            }
        } else if parent.kind() == "function_definition" {
            break;
        }

        curr = parent;
    }

    constraints
}

#[cfg(feature = "smt")]
fn consequence_always_returns(node: tree_sitter::Node) -> bool {
    if node.kind() == "return_statement" {
        return true;
    }
    if node.kind() == "compound_statement" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "return_statement" {
                return true;
            }
        }
    }
    false
}

#[cfg(feature = "smt")]
fn extract_decl_name<'a>(source: &'a str, node: tree_sitter::Node<'a>) -> Option<&'a str> {
    let mut cur = node;
    while let Some(inner) = cur.child_by_field_name("declarator") {
        cur = inner;
    }
    let text = source[cur.start_byte()..cur.end_byte()].trim();
    if !text.is_empty() && text.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(text)
    } else {
        None
    }
}

#[cfg(feature = "smt")]
#[allow(clippy::too_many_arguments)]
fn bind_call_arguments_to_parameters<'ctx>(
    ctx: &'ctx z3::Context,
    solver: &z3::Solver<'ctx>,
    arg_src: &str,
    args_node: tree_sitter::Node,
    arg_prefix: &str,
    param_src: &str,
    fn_def_node: tree_sitter::Node,
    param_prefix: &str,
    vars: &mut FxHashMap<String, z3::ast::BV<'ctx>>,
) {
    let Some(decl) = fn_def_node.child_by_field_name("declarator") else {
        return;
    };
    let params_node = decl
        .child_by_field_name("parameters")
        .or_else(|| {
            // declarator could be function_declarator whose child is parameters
            decl.named_children(&mut decl.walk())
                .find(|c| c.kind() == "parameter_list")
        });
    let Some(params) = params_node else {
        return;
    };

    let mut cursor = args_node.walk();
    for (arg_idx, arg_child) in args_node.named_children(&mut cursor).enumerate() {
        if let Some(param_node) = params.named_child(arg_idx) {
            if let Some(param_name) = extract_decl_name(param_src, param_node) {
                if let Some(arg_bv) = node_to_bv(ctx, arg_child, arg_src, arg_prefix, vars) {
                    let p_key = format!("{param_prefix}::{param_name}");
                    let param_bv = vars
                        .entry(p_key.clone())
                        .or_insert_with(|| z3::ast::BV::new_const(ctx, p_key.as_str(), 64))
                        .clone();
                    solver.assert(&param_bv._eq(&arg_bv));
                }
            }
        }
    }
}

#[cfg(feature = "smt")]
fn node_to_bv<'ctx>(
    ctx: &'ctx z3::Context,
    node: tree_sitter::Node,
    source: &str,
    prefix: &str,
    vars: &mut FxHashMap<String, z3::ast::BV<'ctx>>,
) -> Option<z3::ast::BV<'ctx>> {
    let mut curr = node;
    while curr.kind() == "parenthesized_expression" {
        if let Some(inner) = curr.named_child(0) {
            curr = inner;
        } else {
            break;
        }
    }

    match curr.kind() {
        "number_literal" => {
            let text = &source[curr.start_byte()..curr.end_byte()];
            let val = trace_preproc::parse_int_literal(text)?;
            Some(z3::ast::BV::from_i64(ctx, val, 64))
        }
        "identifier" => {
            let text = source[curr.start_byte()..curr.end_byte()].trim();
            if text == "NULL" || text == "nullptr" || text == "false" {
                return Some(z3::ast::BV::from_i64(ctx, 0, 64));
            }
            if text == "true" {
                return Some(z3::ast::BV::from_i64(ctx, 1, 64));
            }
            let key = format!("{prefix}::{text}");
            let bv = vars
                .entry(key.clone())
                .or_insert_with(|| z3::ast::BV::new_const(ctx, key.as_str(), 64))
                .clone();
            Some(bv)
        }
        "binary_expression" => {
            let op_node = curr.children(&mut curr.walk()).find(|c| {
                matches!(
                    &source[c.start_byte()..c.end_byte()],
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"
                )
            })?;
            let op = &source[op_node.start_byte()..op_node.end_byte()];
            let left_node = curr.child_by_field_name("left")?;
            let right_node = curr.child_by_field_name("right")?;
            let left_bv = node_to_bv(ctx, left_node, source, prefix, vars)?;
            let right_bv = node_to_bv(ctx, right_node, source, prefix, vars)?;
            match op {
                "+" => Some(left_bv.bvadd(&right_bv)),
                "-" => Some(left_bv.bvsub(&right_bv)),
                "*" => Some(left_bv.bvmul(&right_bv)),
                "/" => Some(left_bv.bvsdiv(&right_bv)),
                "%" => Some(left_bv.bvsrem(&right_bv)),
                "&" => Some(left_bv.bvand(&right_bv)),
                "|" => Some(left_bv.bvor(&right_bv)),
                "^" => Some(left_bv.bvxor(&right_bv)),
                "<<" => Some(left_bv.bvshl(&right_bv)),
                ">>" => Some(left_bv.bvashr(&right_bv)),
                _ => None,
            }
        }
        "unary_expression" => {
            let arg = curr.child_by_field_name("argument")?;
            let op_node = curr.children(&mut curr.walk()).find(|c| {
                matches!(&source[c.start_byte()..c.end_byte()], "-" | "~")
            })?;
            let op = &source[op_node.start_byte()..op_node.end_byte()];
            let bv = node_to_bv(ctx, arg, source, prefix, vars)?;
            match op {
                "-" => Some(bv.bvneg()),
                "~" => Some(bv.bvnot()),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(feature = "smt")]
fn node_to_bool<'ctx>(
    ctx: &'ctx z3::Context,
    node: tree_sitter::Node,
    source: &str,
    prefix: &str,
    vars: &mut FxHashMap<String, z3::ast::BV<'ctx>>,
) -> Option<z3::ast::Bool<'ctx>> {
    let mut curr = node;
    while curr.kind() == "parenthesized_expression" {
        if let Some(inner) = curr.named_child(0) {
            curr = inner;
        } else {
            break;
        }
    }

    if curr.kind() == "binary_expression" {
        let op_node = curr.children(&mut curr.walk()).find(|c| {
            matches!(
                &source[c.start_byte()..c.end_byte()],
                "==" | "!=" | "<" | "<=" | ">" | ">=" | "&&" | "||"
            )
        })?;
        let op = &source[op_node.start_byte()..op_node.end_byte()];
        let left_node = curr.child_by_field_name("left")?;
        let right_node = curr.child_by_field_name("right")?;

        match op {
            "&&" => {
                let l = node_to_bool(ctx, left_node, source, prefix, vars)?;
                let r = node_to_bool(ctx, right_node, source, prefix, vars)?;
                return Some(z3::ast::Bool::and(ctx, &[&l, &r]));
            }
            "||" => {
                let l = node_to_bool(ctx, left_node, source, prefix, vars)?;
                let r = node_to_bool(ctx, right_node, source, prefix, vars)?;
                return Some(z3::ast::Bool::or(ctx, &[&l, &r]));
            }
            "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                let left_bv = node_to_bv(ctx, left_node, source, prefix, vars)?;
                let right_bv = node_to_bv(ctx, right_node, source, prefix, vars)?;
                let cmp = match op {
                    "==" => left_bv._eq(&right_bv),
                    "!=" => left_bv._eq(&right_bv).not(),
                    "<" => left_bv.bvslt(&right_bv),
                    "<=" => left_bv.bvsle(&right_bv),
                    ">" => left_bv.bvsgt(&right_bv),
                    ">=" => left_bv.bvsge(&right_bv),
                    _ => unreachable!(),
                };
                return Some(cmp);
            }
            _ => {}
        }
    } else if curr.kind() == "unary_expression"
        && curr
            .children(&mut curr.walk())
            .any(|c| &source[c.start_byte()..c.end_byte()] == "!")
    {
        if let Some(arg) = curr.child_by_field_name("argument") {
            let b = node_to_bool(ctx, arg, source, prefix, vars)?;
            return Some(b.not());
        }
    }

    // Fallback: evaluate as BV and compare != 0
    let bv = node_to_bv(ctx, curr, source, prefix, vars)?;
    let zero = z3::ast::BV::from_i64(ctx, 0, 64);
    Some(bv._eq(&zero).not())
}
