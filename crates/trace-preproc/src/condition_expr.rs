//! Preprocessor condition expression AST and SMT lowering (#Phase S1).
//!
//! Parses `#if` and `#elif` conditions into an expression AST, extracts macro
//! dependencies, and (under the `smt` feature) lowers expressions to Z3 ASTs
//! (`Bool` and `BitVec<64>`).

use crate::lexer::{Token, TokenKind};
use crate::preprocessor::{char_literal_body, char_value, parse_pp_int};
use crate::Language;
use std::collections::HashSet;

#[cfg(feature = "smt")]
use z3::ast::Ast;

/// Abstract syntax tree of a C preprocessor condition expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionExpr {
    /// Integer literal.
    Int(i64),
    /// Macro definedness check: `defined(M)` or `defined M`.
    Defined(String),
    /// Identifier value: `M` (evaluates to its integer value if defined, 0 if undefined).
    Var(String),
    /// Logical NOT: `!expr`.
    Not(Box<ConditionExpr>),
    /// Bitwise NOT: `~expr`.
    BitNot(Box<ConditionExpr>),
    /// Arithmetic negation: `-expr`.
    Neg(Box<ConditionExpr>),
    /// Equality: `lhs == rhs`.
    Eq(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Inequality: `lhs != rhs`.
    Ne(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Less than: `lhs < rhs`.
    Lt(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Less than or equal: `lhs <= rhs`.
    Le(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Greater than: `lhs > rhs`.
    Gt(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Greater than or equal: `lhs >= rhs`.
    Ge(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Logical AND: `lhs && rhs`.
    And(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Logical OR: `lhs || rhs`.
    Or(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Bitwise AND: `lhs & rhs`.
    BitAnd(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Bitwise OR: `lhs | rhs`.
    BitOr(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Bitwise XOR: `lhs ^ rhs`.
    BitXor(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Shift left: `lhs << rhs`.
    Shl(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Shift right: `lhs >> rhs`.
    Shr(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Addition: `lhs + rhs`.
    Add(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Subtraction: `lhs - rhs`.
    Sub(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Multiplication: `lhs * rhs`.
    Mul(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Division: `lhs / rhs`.
    Div(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Remainder: `lhs % rhs`.
    Rem(Box<ConditionExpr>, Box<ConditionExpr>),
    /// Conditional ternary: `cond ? then_expr : else_expr`.
    Ternary(Box<ConditionExpr>, Box<ConditionExpr>, Box<ConditionExpr>),
    /// Function-like macro invocation: `name(arg0, arg1, ...)`.
    Call(String, Vec<ConditionExpr>),
    /// Unparseable or malformed expression fallback.
    Unknown,
}

/// Parse an integer literal according to C preprocessor rules (hex, octal, binary, decimal, suffixes, optional sign).
#[must_use]
pub fn parse_int_literal(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('-') {
        parse_pp_int(rest.trim_start()).map(|p| -(p.as_i64()))
    } else if let Some(rest) = s.strip_prefix('+') {
        parse_pp_int(rest.trim_start()).map(|p| p.as_i64())
    } else {
        parse_pp_int(s).map(|p| p.as_i64())
    }
}

impl ConditionExpr {
    /// Parse a preprocessor condition string into an AST.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let toks: Vec<Token> = crate::Lexer::new(text, Language::C)
            .tokenize()
            .into_iter()
            .filter(|t| !matches!(t.kind, TokenKind::Eof | TokenKind::Newline))
            .collect();
        let mut p = ConditionExprParser {
            toks: &toks,
            pos: 0,
            err: false,
        };
        let expr = p.ternary();
        if p.err || p.pos != p.toks.len() {
            ConditionExpr::Unknown
        } else {
            expr
        }
    }

    /// Collect all macro identifiers referenced in this condition expression.
    pub fn collect_macro_names(&self, names: &mut HashSet<String>) {
        match self {
            ConditionExpr::Int(_) | ConditionExpr::Unknown => {}
            ConditionExpr::Defined(name) | ConditionExpr::Var(name) => {
                names.insert(name.clone());
            }
            ConditionExpr::Not(inner)
            | ConditionExpr::BitNot(inner)
            | ConditionExpr::Neg(inner) => {
                inner.collect_macro_names(names);
            }
            ConditionExpr::Eq(l, r)
            | ConditionExpr::Ne(l, r)
            | ConditionExpr::Lt(l, r)
            | ConditionExpr::Le(l, r)
            | ConditionExpr::Gt(l, r)
            | ConditionExpr::Ge(l, r)
            | ConditionExpr::And(l, r)
            | ConditionExpr::Or(l, r)
            | ConditionExpr::BitAnd(l, r)
            | ConditionExpr::BitOr(l, r)
            | ConditionExpr::BitXor(l, r)
            | ConditionExpr::Shl(l, r)
            | ConditionExpr::Shr(l, r)
            | ConditionExpr::Add(l, r)
            | ConditionExpr::Sub(l, r)
            | ConditionExpr::Mul(l, r)
            | ConditionExpr::Div(l, r)
            | ConditionExpr::Rem(l, r) => {
                l.collect_macro_names(names);
                r.collect_macro_names(names);
            }
            ConditionExpr::Ternary(c, t, e) => {
                c.collect_macro_names(names);
                t.collect_macro_names(names);
                e.collect_macro_names(names);
            }
            ConditionExpr::Call(name, args) => {
                names.insert(name.clone());
                for arg in args {
                    arg.collect_macro_names(names);
                }
            }
        }
    }
}

struct ConditionExprParser<'a> {
    toks: &'a [Token],
    pos: usize,
    err: bool,
}

impl ConditionExprParser<'_> {
    fn peek_punct(&self) -> Option<&str> {
        match self.toks.get(self.pos) {
            Some(Token {
                kind: TokenKind::Punct(s),
                ..
            }) => Some(s),
            _ => None,
        }
    }

    fn peek_ident(&self) -> Option<&str> {
        match self.toks.get(self.pos) {
            Some(Token {
                kind: TokenKind::Identifier(s),
                ..
            }) => Some(s.as_str()),
            _ => None,
        }
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.peek_punct() == Some(p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn ternary(&mut self) -> ConditionExpr {
        let c = self.logical_or();
        if self.eat("?") {
            let a = self.ternary();
            if self.eat(":") {
                let b = self.ternary();
                ConditionExpr::Ternary(Box::new(c), Box::new(a), Box::new(b))
            } else {
                self.err = true;
                ConditionExpr::Unknown
            }
        } else {
            c
        }
    }

    fn logical_or(&mut self) -> ConditionExpr {
        let mut v = self.logical_and();
        while self.eat("||") || self.eat_ident("or") {
            let r = self.logical_and();
            v = ConditionExpr::Or(Box::new(v), Box::new(r));
        }
        v
    }

    fn logical_and(&mut self) -> ConditionExpr {
        let mut v = self.bit_or();
        while self.eat("&&") || self.eat_ident("and") {
            let r = self.bit_or();
            v = ConditionExpr::And(Box::new(v), Box::new(r));
        }
        v
    }

    fn bit_or(&mut self) -> ConditionExpr {
        let mut v = self.bit_xor();
        while self.eat("|") || self.eat_ident("bitor") {
            let r = self.bit_xor();
            v = ConditionExpr::BitOr(Box::new(v), Box::new(r));
        }
        v
    }

    fn bit_xor(&mut self) -> ConditionExpr {
        let mut v = self.bit_and();
        while self.eat("^") || self.eat_ident("xor") {
            let r = self.bit_and();
            v = ConditionExpr::BitXor(Box::new(v), Box::new(r));
        }
        v
    }

    fn bit_and(&mut self) -> ConditionExpr {
        let mut v = self.equality();
        while self.eat("&") || self.eat_ident("bitand") {
            let r = self.equality();
            v = ConditionExpr::BitAnd(Box::new(v), Box::new(r));
        }
        v
    }

    fn equality(&mut self) -> ConditionExpr {
        let mut v = self.relational();
        loop {
            if self.eat("==") {
                let r = self.relational();
                v = ConditionExpr::Eq(Box::new(v), Box::new(r));
            } else if self.eat("!=") || self.eat_ident("not_eq") {
                let r = self.relational();
                v = ConditionExpr::Ne(Box::new(v), Box::new(r));
            } else {
                return v;
            }
        }
    }

    fn relational(&mut self) -> ConditionExpr {
        let mut v = self.shift();
        loop {
            if self.eat("<=") {
                let r = self.shift();
                v = ConditionExpr::Le(Box::new(v), Box::new(r));
            } else if self.eat(">=") {
                let r = self.shift();
                v = ConditionExpr::Ge(Box::new(v), Box::new(r));
            } else if self.eat("<") {
                let r = self.shift();
                v = ConditionExpr::Lt(Box::new(v), Box::new(r));
            } else if self.eat(">") {
                let r = self.shift();
                v = ConditionExpr::Gt(Box::new(v), Box::new(r));
            } else {
                return v;
            }
        }
    }

    fn shift(&mut self) -> ConditionExpr {
        let mut v = self.additive();
        loop {
            if self.eat("<<") {
                let r = self.additive();
                v = ConditionExpr::Shl(Box::new(v), Box::new(r));
            } else if self.eat(">>") {
                let r = self.additive();
                v = ConditionExpr::Shr(Box::new(v), Box::new(r));
            } else {
                return v;
            }
        }
    }

    fn additive(&mut self) -> ConditionExpr {
        let mut v = self.multiplicative();
        loop {
            if self.eat("+") {
                let r = self.multiplicative();
                v = ConditionExpr::Add(Box::new(v), Box::new(r));
            } else if self.eat("-") {
                let r = self.multiplicative();
                v = ConditionExpr::Sub(Box::new(v), Box::new(r));
            } else {
                return v;
            }
        }
    }

    fn multiplicative(&mut self) -> ConditionExpr {
        let mut v = self.unary();
        loop {
            if self.eat("*") {
                let r = self.unary();
                v = ConditionExpr::Mul(Box::new(v), Box::new(r));
            } else if self.eat("/") {
                let r = self.unary();
                v = ConditionExpr::Div(Box::new(v), Box::new(r));
            } else if self.eat("%") {
                let r = self.unary();
                v = ConditionExpr::Rem(Box::new(v), Box::new(r));
            } else {
                return v;
            }
        }
    }

    fn unary(&mut self) -> ConditionExpr {
        if self.eat("!") || self.eat_ident("not") {
            return ConditionExpr::Not(Box::new(self.unary()));
        }
        if self.eat("~") || self.eat_ident("compl") {
            return ConditionExpr::BitNot(Box::new(self.unary()));
        }
        if self.eat("-") {
            return ConditionExpr::Neg(Box::new(self.unary()));
        }
        if self.eat("+") {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> ConditionExpr {
        let Some(tok) = self.toks.get(self.pos) else {
            self.err = true;
            return ConditionExpr::Unknown;
        };
        match &tok.kind {
            TokenKind::Number(s) => {
                self.pos += 1;
                if let Some(v) = parse_pp_int(s) {
                    ConditionExpr::Int(v.as_i64())
                } else {
                    self.err = true;
                    ConditionExpr::Unknown
                }
            }
            TokenKind::Char(s) => {
                self.pos += 1;
                if let Some(body) = char_literal_body(s) {
                    ConditionExpr::Int(char_value(body))
                } else {
                    self.err = true;
                    ConditionExpr::Unknown
                }
            }
            TokenKind::Punct(p) if *p == "(" => {
                self.pos += 1;
                let v = self.ternary();
                if !self.eat(")") {
                    self.err = true;
                }
                v
            }
            TokenKind::Identifier(name) => {
                if name == "defined" {
                    self.pos += 1;
                    if self.eat("(") {
                        if let Some(TokenKind::Identifier(target)) =
                            self.toks.get(self.pos).map(|t| &t.kind)
                        {
                            let target = target.clone();
                            self.pos += 1;
                            if !self.eat(")") {
                                self.err = true;
                            }
                            ConditionExpr::Defined(target)
                        } else {
                            self.err = true;
                            ConditionExpr::Unknown
                        }
                    } else if let Some(TokenKind::Identifier(target)) =
                        self.toks.get(self.pos).map(|t| &t.kind)
                    {
                        let target = target.clone();
                        self.pos += 1;
                        ConditionExpr::Defined(target)
                    } else {
                        self.err = true;
                        ConditionExpr::Unknown
                    }
                } else if name == "true" {
                    self.pos += 1;
                    ConditionExpr::Int(1)
                } else if name == "false" {
                    self.pos += 1;
                    ConditionExpr::Int(0)
                } else {
                    let ident = name.clone();
                    self.pos += 1;
                    if self.peek_punct() == Some("(") {
                        self.pos += 1;
                        let mut args = Vec::new();
                        if self.eat(")") {
                            ConditionExpr::Call(ident, args)
                        } else {
                            loop {
                                args.push(self.ternary());
                                if self.eat(",") {
                                    continue;
                                }
                                if self.eat(")") {
                                    break;
                                }
                                self.err = true;
                                break;
                            }
                            ConditionExpr::Call(ident, args)
                        }
                    } else {
                        ConditionExpr::Var(ident)
                    }
                }
            }
            _ => {
                self.pos += 1;
                self.err = true;
                ConditionExpr::Unknown
            }
        }
    }

    fn eat_ident(&mut self, s: &str) -> bool {
        if self.peek_ident() == Some(s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// SMT / Z3 Lowering
// ---------------------------------------------------------------------------

#[cfg(feature = "smt")]
pub trait MacroSmtEnv<'ctx> {
    /// Whether macro `name` is defined in this environment.
    fn is_defined(&self, name: &str) -> z3::ast::Bool<'ctx>;
    /// The integer value of macro `name` (evaluates to 0 if undefined).
    fn value(&self, name: &str) -> z3::ast::BV<'ctx>;
}

#[cfg(feature = "smt")]
impl ConditionExpr {
    /// Lower this condition expression to a 64-bit bitvector expression in Z3.
    pub fn to_smt_bv<'ctx>(
        &self,
        ctx: &'ctx z3::Context,
        env: &dyn MacroSmtEnv<'ctx>,
    ) -> z3::ast::BV<'ctx> {
        match self {
            ConditionExpr::Int(val) => z3::ast::BV::from_i64(ctx, *val, 64),
            ConditionExpr::Defined(name) => {
                let def = env.is_defined(name);
                def.ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::Var(name) => env.value(name),
            ConditionExpr::Not(inner) => {
                let b = inner.to_smt_bool(ctx, env);
                b.not().ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::BitNot(inner) => inner.to_smt_bv(ctx, env).bvnot(),
            ConditionExpr::Neg(inner) => inner.to_smt_bv(ctx, env).bvneg(),
            ConditionExpr::Eq(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                l._eq(&r).ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::Ne(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                l._eq(&r).not().ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::Lt(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                l.bvslt(&r).ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::Le(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                l.bvsle(&r).ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::Gt(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                l.bvsgt(&r).ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::Ge(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                l.bvsge(&r).ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::And(lhs, rhs) => {
                let l = lhs.to_smt_bool(ctx, env);
                let r = rhs.to_smt_bool(ctx, env);
                z3::ast::Bool::and(ctx, &[&l, &r]).ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::Or(lhs, rhs) => {
                let l = lhs.to_smt_bool(ctx, env);
                let r = rhs.to_smt_bool(ctx, env);
                z3::ast::Bool::or(ctx, &[&l, &r]).ite(
                    &z3::ast::BV::from_i64(ctx, 1, 64),
                    &z3::ast::BV::from_i64(ctx, 0, 64),
                )
            }
            ConditionExpr::BitAnd(lhs, rhs) => {
                lhs.to_smt_bv(ctx, env).bvand(&rhs.to_smt_bv(ctx, env))
            }
            ConditionExpr::BitOr(lhs, rhs) => {
                lhs.to_smt_bv(ctx, env).bvor(&rhs.to_smt_bv(ctx, env))
            }
            ConditionExpr::BitXor(lhs, rhs) => {
                lhs.to_smt_bv(ctx, env).bvxor(&rhs.to_smt_bv(ctx, env))
            }
            ConditionExpr::Shl(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvshl(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Shr(lhs, rhs) => {
                lhs.to_smt_bv(ctx, env).bvashr(&rhs.to_smt_bv(ctx, env))
            }
            ConditionExpr::Add(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvadd(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Sub(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvsub(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Mul(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvmul(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Div(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                let zero = z3::ast::BV::from_i64(ctx, 0, 64);
                r._eq(&zero).ite(&zero, &l.bvsdiv(&r))
            }
            ConditionExpr::Rem(lhs, rhs) => {
                let l = lhs.to_smt_bv(ctx, env);
                let r = rhs.to_smt_bv(ctx, env);
                let zero = z3::ast::BV::from_i64(ctx, 0, 64);
                r._eq(&zero).ite(&zero, &l.bvsrem(&r))
            }
            ConditionExpr::Ternary(cond, then_e, else_e) => {
                let c = cond.to_smt_bool(ctx, env);
                c.ite(&then_e.to_smt_bv(ctx, env), &else_e.to_smt_bv(ctx, env))
            }
            ConditionExpr::Call(name, args) => {
                if name == "KERNEL_VERSION" && args.len() == 3 {
                    let a = args[0].to_smt_bv(ctx, env);
                    let b = args[1].to_smt_bv(ctx, env);
                    let c = args[2].to_smt_bv(ctx, env);
                    let c16 = z3::ast::BV::from_i64(ctx, 16, 64);
                    let c8 = z3::ast::BV::from_i64(ctx, 8, 64);
                    a.bvshl(&c16).bvadd(&b.bvshl(&c8)).bvadd(&c)
                } else {
                    z3::ast::BV::from_i64(ctx, 0, 64)
                }
            }
            ConditionExpr::Unknown => z3::ast::BV::from_i64(ctx, 0, 64),
        }
    }

    /// Lower this condition expression to a boolean formula in Z3.
    pub fn to_smt_bool<'ctx>(
        &self,
        ctx: &'ctx z3::Context,
        env: &dyn MacroSmtEnv<'ctx>,
    ) -> z3::ast::Bool<'ctx> {
        let zero = z3::ast::BV::from_i64(ctx, 0, 64);
        match self {
            ConditionExpr::Int(val) => z3::ast::Bool::from_bool(ctx, *val != 0),
            ConditionExpr::Defined(name) => env.is_defined(name),
            ConditionExpr::Var(name) => env.value(name)._eq(&zero).not(),
            ConditionExpr::Not(inner) => inner.to_smt_bool(ctx, env).not(),
            ConditionExpr::And(lhs, rhs) => {
                let l = lhs.to_smt_bool(ctx, env);
                let r = rhs.to_smt_bool(ctx, env);
                z3::ast::Bool::and(ctx, &[&l, &r])
            }
            ConditionExpr::Or(lhs, rhs) => {
                let l = lhs.to_smt_bool(ctx, env);
                let r = rhs.to_smt_bool(ctx, env);
                z3::ast::Bool::or(ctx, &[&l, &r])
            }
            ConditionExpr::Eq(lhs, rhs) => lhs.to_smt_bv(ctx, env)._eq(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Ne(lhs, rhs) => {
                lhs.to_smt_bv(ctx, env)._eq(&rhs.to_smt_bv(ctx, env)).not()
            }
            ConditionExpr::Lt(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvslt(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Le(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvsle(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Gt(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvsgt(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Ge(lhs, rhs) => lhs.to_smt_bv(ctx, env).bvsge(&rhs.to_smt_bv(ctx, env)),
            ConditionExpr::Ternary(cond, then_e, else_e) => {
                let c = cond.to_smt_bool(ctx, env);
                let t = then_e.to_smt_bool(ctx, env);
                let e = else_e.to_smt_bool(ctx, env);
                c.ite(&t, &e)
            }
            _ => self.to_smt_bv(ctx, env)._eq(&zero).not(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defined_variants() {
        assert_eq!(
            ConditionExpr::parse("defined(FOO)"),
            ConditionExpr::Defined("FOO".into())
        );
        assert_eq!(
            ConditionExpr::parse("defined FOO"),
            ConditionExpr::Defined("FOO".into())
        );
        assert_eq!(
            ConditionExpr::parse("!defined(BAR)"),
            ConditionExpr::Not(Box::new(ConditionExpr::Defined("BAR".into())))
        );
    }

    #[test]
    fn parses_conjunction_and_disjunction() {
        assert_eq!(
            ConditionExpr::parse("defined(A) || defined(B)"),
            ConditionExpr::Or(
                Box::new(ConditionExpr::Defined("A".into())),
                Box::new(ConditionExpr::Defined("B".into()))
            )
        );
        assert_eq!(
            ConditionExpr::parse("defined(A) && defined(B)"),
            ConditionExpr::And(
                Box::new(ConditionExpr::Defined("A".into())),
                Box::new(ConditionExpr::Defined("B".into()))
            )
        );
    }

    #[test]
    fn parses_arithmetic_and_comparisons() {
        assert_eq!(
            ConditionExpr::parse("LEVEL >= 2"),
            ConditionExpr::Ge(
                Box::new(ConditionExpr::Var("LEVEL".into())),
                Box::new(ConditionExpr::Int(2))
            )
        );
        assert_eq!(
            ConditionExpr::parse("FOO == 0x10"),
            ConditionExpr::Eq(
                Box::new(ConditionExpr::Var("FOO".into())),
                Box::new(ConditionExpr::Int(16))
            )
        );
    }

    #[test]
    fn parses_nested_multi_variable_condition() {
        let expr = ConditionExpr::parse("defined(CONFIG_A) && (LEVEL >= 2 || !defined(MINIMAL))");
        let mut names = HashSet::new();
        expr.collect_macro_names(&mut names);
        assert!(names.contains("CONFIG_A"));
        assert!(names.contains("LEVEL"));
        assert!(names.contains("MINIMAL"));
    }

    #[test]
    fn parses_kernel_version_call() {
        assert_eq!(
            ConditionExpr::parse("LINUX_VERSION_CODE < KERNEL_VERSION(6, 6, 0)"),
            ConditionExpr::Lt(
                Box::new(ConditionExpr::Var("LINUX_VERSION_CODE".into())),
                Box::new(ConditionExpr::Call(
                    "KERNEL_VERSION".into(),
                    vec![
                        ConditionExpr::Int(6),
                        ConditionExpr::Int(6),
                        ConditionExpr::Int(0),
                    ]
                ))
            )
        );
    }

    #[cfg(feature = "smt")]
    #[test]
    fn smt_lowering_evaluation() {
        use std::collections::HashMap;

        struct TestEnv<'ctx> {
            ctx: &'ctx z3::Context,
            defs: HashMap<&'static str, bool>,
            vals: HashMap<&'static str, i64>,
        }

        impl<'ctx> MacroSmtEnv<'ctx> for TestEnv<'ctx> {
            fn is_defined(&self, name: &str) -> z3::ast::Bool<'ctx> {
                let d = self.defs.get(name).copied().unwrap_or(false);
                z3::ast::Bool::from_bool(self.ctx, d)
            }
            fn value(&self, name: &str) -> z3::ast::BV<'ctx> {
                let v = self.vals.get(name).copied().unwrap_or(0);
                z3::ast::BV::from_i64(self.ctx, v, 64)
            }
        }

        let cfg = z3::Config::new();
        let ctx = z3::Context::new(&cfg);

        let mut defs = HashMap::new();
        defs.insert("CONFIG_A", true);
        defs.insert("LEVEL", true);
        let mut vals = HashMap::new();
        vals.insert("LEVEL", 2);

        let env = TestEnv {
            ctx: &ctx,
            defs,
            vals,
        };

        let expr = ConditionExpr::parse("defined(CONFIG_A) && (LEVEL >= 2 || !defined(MINIMAL))");
        let solver = z3::Solver::new(&ctx);
        let smt_b = expr.to_smt_bool(&ctx, &env);
        solver.assert(&smt_b);
        assert_eq!(solver.check(), z3::SatResult::Sat);
    }

    #[test]
    fn parse_int_literal_handles_signs_and_bases() {
        assert_eq!(parse_int_literal("0"), Some(0));
        assert_eq!(parse_int_literal("  42  "), Some(42));
        assert_eq!(parse_int_literal("-1"), Some(-1));
        assert_eq!(parse_int_literal("+100"), Some(100));
        assert_eq!(parse_int_literal("0x10"), Some(16));
        assert_eq!(parse_int_literal("-0x20"), Some(-32));
        assert_eq!(parse_int_literal("0b101"), Some(5));
        assert_eq!(parse_int_literal("invalid"), None);
    }

    #[test]
    fn parses_alternative_tokens() {
        assert_eq!(
            ConditionExpr::parse("A and B"),
            ConditionExpr::And(
                Box::new(ConditionExpr::Var("A".into())),
                Box::new(ConditionExpr::Var("B".into()))
            )
        );
        assert_eq!(
            ConditionExpr::parse("A or B"),
            ConditionExpr::Or(
                Box::new(ConditionExpr::Var("A".into())),
                Box::new(ConditionExpr::Var("B".into()))
            )
        );
        assert_eq!(
            ConditionExpr::parse("not A"),
            ConditionExpr::Not(Box::new(ConditionExpr::Var("A".into())))
        );
        assert_eq!(
            ConditionExpr::parse("A not_eq B"),
            ConditionExpr::Ne(
                Box::new(ConditionExpr::Var("A".into())),
                Box::new(ConditionExpr::Var("B".into()))
            )
        );
        assert_eq!(
            ConditionExpr::parse("A bitand B"),
            ConditionExpr::BitAnd(
                Box::new(ConditionExpr::Var("A".into())),
                Box::new(ConditionExpr::Var("B".into()))
            )
        );
        assert_eq!(
            ConditionExpr::parse("A bitor B"),
            ConditionExpr::BitOr(
                Box::new(ConditionExpr::Var("A".into())),
                Box::new(ConditionExpr::Var("B".into()))
            )
        );
        assert_eq!(
            ConditionExpr::parse("A xor B"),
            ConditionExpr::BitXor(
                Box::new(ConditionExpr::Var("A".into())),
                Box::new(ConditionExpr::Var("B".into()))
            )
        );
        assert_eq!(
            ConditionExpr::parse("compl A"),
            ConditionExpr::BitNot(Box::new(ConditionExpr::Var("A".into())))
        );
    }
}
