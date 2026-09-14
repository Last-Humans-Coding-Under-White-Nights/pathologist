//! Array index and table dispatch expression evaluation (Phase S4).
//!
//! Provides deterministic evaluation of array subscript expressions (literals, enums,
//! defines, bitwise masks, and modulo operations) with optional SMT (Z3) bitvector
//! range solving to refine indirect call targets in table dispatch patterns.

use indexmap::IndexMap;
use rustc_hash::FxHashMap;

/// Parse integer literal from string (decimal, hex `0x...`, octal `0...`).
pub fn parse_int_literal(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches(['u', 'U', 'l', 'L']);
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else if s.len() > 1 && s.starts_with('0') && s.chars().all(|c| ('0'..='7').contains(&c)) {
        i64::from_str_radix(s, 8).ok()
    } else {
        s.parse::<i64>().ok()
    }
}

/// Evaluates a single identifier or literal to a concrete u32 index if possible.
pub fn eval_single_index(
    text: &str,
    defines: &IndexMap<String, String>,
    enums: &FxHashMap<String, u64>,
) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(val) = parse_int_literal(text) {
        if (0..=65535).contains(&val) {
            return Some(val as u32);
        }
    }
    if let Some(&val) = enums.get(text) {
        if val <= 65535 {
            return Some(val as u32);
        }
    }
    let short = text.rsplit("::").next().unwrap_or(text);
    if let Some(&val) = enums.get(short) {
        if val <= 65535 {
            return Some(val as u32);
        }
    }
    if let Some(def_val) = defines.get(text).or_else(|| defines.get(short)) {
        if let Some(val) = parse_int_literal(def_val.trim()) {
            if (0..=65535).contains(&val) {
                return Some(val as u32);
            }
        }
    }
    None
}

/// Strip enclosing parentheses: `((expr))` -> `expr`.
fn strip_parens(mut s: &str) -> &str {
    s = s.trim();
    while s.starts_with('(') && s.ends_with(')') {
        let mut depth = 0;
        let mut touches_zero_early = false;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && i < s.len() - 1 {
                        touches_zero_early = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if depth == 0 && !touches_zero_early {
            s = s[1..s.len() - 1].trim();
        } else {
            break;
        }
    }
    s
}

/// Evaluates an index expression to a bounded set of possible concrete indices.
/// Returns `Some(indices)` if bounded and known, or `None` if unbounded / unknown.
pub fn eval_index_expr(
    expr: &str,
    defines: &IndexMap<String, String>,
    enums: &FxHashMap<String, u64>,
) -> Option<Vec<u32>> {
    let expr = strip_parens(expr);
    if expr.is_empty() {
        return None;
    }

    // 1. Check for single literal / constant / enum
    if let Some(val) = eval_single_index(expr, defines, enums) {
        return Some(vec![val]);
    }

    // 2. Pure Rust fast-path for bitwise mask: `X & MASK` or `MASK & X`
    if let Some((left, right)) = split_binary_op(expr, '&') {
        let mask = eval_single_index(right, defines, enums)
            .or_else(|| eval_single_index(left, defines, enums));
        if let Some(m) = mask {
            if m <= 31 {
                let mut values = Vec::new();
                for v in 0..=m {
                    if (v & !m) == 0 {
                        values.push(v);
                    }
                }
                if !values.is_empty() && values.len() <= 16 {
                    return Some(values);
                }
            }
        }
    }

    // 3. Pure Rust fast-path for modulo: `X % N`
    if let Some((_, right)) = split_binary_op(expr, '%') {
        if let Some(n) = eval_single_index(right, defines, enums) {
            if (1..=16).contains(&n) {
                return Some((0..n).collect());
            }
        }
    }

    // 4. SMT BitVector solving (Phase S4)
    #[cfg(feature = "smt")]
    {
        if let Some(indices) = solve_index_bounds_smt(expr, defines, enums) {
            return Some(indices);
        }
    }

    None
}

/// Split by binary operator at outer nesting level (not inside parentheses).
fn split_binary_op(s: &str, op: char) -> Option<(&str, &str)> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 && c == op => {
                let left = s[..i].trim();
                let right = s[i + 1..].trim();
                if !left.is_empty() && !right.is_empty() {
                    return Some((left, right));
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(feature = "smt")]
fn solve_index_bounds_smt(
    expr_str: &str,
    defines: &IndexMap<String, String>,
    enums: &FxHashMap<String, u64>,
) -> Option<Vec<u32>> {
    use z3::ast::{Ast, BV};
    use z3::{Config, Context, SatResult, Solver};

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_c::LANGUAGE.into()).ok()?;
    let code = format!("int _eval_fn() {{ return ({expr_str}); }}");
    let tree = parser.parse(&code, None)?;
    let root = tree.root_node();
    let ret_stmt = find_first_kind(root, "return_statement")?;
    let expr_node = ret_stmt.child_by_field_name("expression").or_else(|| ret_stmt.named_child(0))?;

    let mut cfg = Config::new();
    cfg.set_param_value("timeout", "200"); // 200ms limit
    let ctx = Context::new(&cfg);
    let solver = Solver::new(&ctx);
    let mut var_map = rustc_hash::FxHashMap::default();

    let bv = node_to_bv(&ctx, expr_node, &code, defines, enums, &mut var_map)?;

    // Check if non-negative (upper bound < 1024)
    let zero = BV::from_u64(&ctx, 0, 32);
    let limit = BV::from_u64(&ctx, 1024, 32);
    solver.assert(&bv.bvuge(&zero));
    solver.assert(&bv.bvule(&limit));

    let mut results = Vec::new();
    while results.len() <= 16 {
        if solver.check() == SatResult::Sat {
            let model = solver.get_model()?;
            let val_ast = model.eval(&bv, true)?;
            let val = val_ast.as_u64()? as u32;
            results.push(val);
            let blocked = bv._eq(&BV::from_u64(&ctx, val as u64, 32));
            solver.assert(&blocked.not());
        } else {
            // UNSAT means all solutions within [0, 1024] enumerated!
            break;
        }
    }

    if results.is_empty() || results.len() > 16 {
        return None;
    }

    results.sort_unstable();
    results.dedup();
    Some(results)
}

#[cfg(feature = "smt")]
fn find_first_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(n) = find_first_kind(child, kind) {
            return Some(n);
        }
    }
    None
}

#[cfg(feature = "smt")]
fn node_to_bv<'ctx>(
    ctx: &'ctx z3::Context,
    node: tree_sitter::Node,
    source: &str,
    defines: &IndexMap<String, String>,
    enums: &FxHashMap<String, u64>,
    vars: &mut rustc_hash::FxHashMap<String, z3::ast::BV<'ctx>>,
) -> Option<z3::ast::BV<'ctx>> {
    use z3::ast::{Ast, BV};
    match node.kind() {
        "parenthesized_expression" => {
            let inner = node.named_child(0)?;
            node_to_bv(ctx, inner, source, defines, enums, vars)
        }
        "cast_expression" => {
            let inner = node.child_by_field_name("value")?;
            node_to_bv(ctx, inner, source, defines, enums, vars)
        }
        "number_literal" => {
            let text = &source[node.start_byte()..node.end_byte()];
            let val = parse_int_literal(text)?;
            Some(BV::from_u64(ctx, val as u64, 32))
        }
        "identifier" => {
            let text = &source[node.start_byte()..node.end_byte()];
            if let Some(val) = eval_single_index(text, defines, enums) {
                Some(BV::from_u64(ctx, val as u64, 32))
            } else {
                let var = vars
                    .entry(text.to_string())
                    .or_insert_with(|| BV::new_const(ctx, text, 32))
                    .clone();
                Some(var)
            }
        }
        "binary_expression" => {
            let left = node.child_by_field_name("left")?;
            let right = node.child_by_field_name("right")?;
            let op = node.child_by_field_name("operator")?;
            let op_str = &source[op.start_byte()..op.end_byte()];

            let left_bv = node_to_bv(ctx, left, source, defines, enums, vars)?;
            let right_bv = node_to_bv(ctx, right, source, defines, enums, vars)?;

            match op_str {
                "+" => Some(left_bv.bvadd(&right_bv)),
                "-" => Some(left_bv.bvsub(&right_bv)),
                "*" => Some(left_bv.bvmul(&right_bv)),
                "/" => Some(left_bv.bvudiv(&right_bv)),
                "%" => Some(left_bv.bvurem(&right_bv)),
                "&" => Some(left_bv.bvand(&right_bv)),
                "|" => Some(left_bv.bvor(&right_bv)),
                "^" => Some(left_bv.bvxor(&right_bv)),
                "<<" => Some(left_bv.bvshl(&right_bv)),
                ">>" => Some(left_bv.bvlshr(&right_bv)),
                _ => None,
            }
        }
        "conditional_expression" => {
            let cond = node.child_by_field_name("condition")?;
            let consequence = node.child_by_field_name("consequence")?;
            let alternative = node.child_by_field_name("alternative")?;

            let cond_bv = node_to_bv(ctx, cond, source, defines, enums, vars)?;
            let cons_bv = node_to_bv(ctx, consequence, source, defines, enums, vars)?;
            let alt_bv = node_to_bv(ctx, alternative, source, defines, enums, vars)?;

            let zero = BV::from_u64(ctx, 0, 32);
            let cond_bool = cond_bv._eq(&zero).not();
            Some(cond_bool.ite(&cons_bv, &alt_bv))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::eval_index_expr;
    use indexmap::IndexMap;

    #[test]
    #[cfg(feature = "smt")]
    fn test_eval_index_expr_smt() {
        let defines = IndexMap::new();
        let enums = rustc_hash::FxHashMap::default();
        let res = eval_index_expr("(x & 1) + 2", &defines, &enums);
        assert_eq!(res, Some(vec![2, 3]));
    }
}

