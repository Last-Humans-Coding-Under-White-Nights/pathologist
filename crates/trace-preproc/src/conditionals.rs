//! Records of the conditional-compilation chains a preprocess run met, for
//! measuring what the current configuration excludes (#57).
//!
//! The primary record is a *chain* — one `#if`/`#ifdef`/`#ifndef` with its
//! `#elif`/`#else` arms up to the `#endif` — not a macro name. The lines an
//! arm excludes belong to the whole expression that controls the chain; a
//! per-name view is a derived aggregate and has to say how it apportions
//! (see `examples/conditional_coverage.rs` in `trace-cli`).
//!
//! Recording is off by default (`PreprocessOptions::record_conditionals`);
//! the hooks cost nothing when it is off.

use std::path::PathBuf;

/// The directive that opens an arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmDirective {
    If,
    Ifdef,
    Ifndef,
    Elif,
    Else,
}

impl ArmDirective {
    pub fn as_str(self) -> &'static str {
        match self {
            ArmDirective::If => "if",
            ArmDirective::Ifdef => "ifdef",
            ArmDirective::Ifndef => "ifndef",
            ArmDirective::Elif => "elif",
            ArmDirective::Else => "else",
        }
    }
}

/// What happened to an arm's lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmOutcome {
    /// The arm's lines were emitted: its condition held, or it is the
    /// `#else` of a chain no earlier arm of which was taken.
    Taken,
    /// The arm's lines were excluded: its condition was false, or an
    /// earlier arm of the chain had already been taken.
    Skipped,
    /// The chain sits inside an excluded region, so nothing in it was
    /// evaluated (C11 6.10.1p6). Its lines are excluded by the enclosing
    /// chain, not by this one.
    Unevaluated,
}

impl ArmOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            ArmOutcome::Taken => "taken",
            ArmOutcome::Skipped => "skipped",
            ArmOutcome::Unevaluated => "unevaluated",
        }
    }
}

/// One name a condition depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionRead {
    pub name: String,
    /// Whether a macro (other than a builtin fallback, which conditionals
    /// do not see) was bound to the name when the condition consulted it.
    /// `None` when the condition was not evaluated, so nothing consulted
    /// the environment: the name is spelled in the expression, and that is
    /// all that is known.
    pub bound: Option<bool>,
}

/// One arm of a chain: the directive that opens it and the lines up to the
/// next directive of the same chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalArm {
    pub directive: ArmDirective,
    /// The controlling condition as written (macro names unexpanded;
    /// `defined` kept). The operand alone for `#ifdef` / `#ifndef`, empty
    /// for `#else`.
    pub expression: String,
    /// Physical line of the directive's opening `#`, before any splice.
    pub line: u32,
    /// Line of the directive that ends this arm — the chain's next
    /// `#elif` / `#else` / `#endif` — or, for a chain left open, the
    /// line after the file's last line.
    pub end_line: u32,
    pub outcome: ArmOutcome,
    /// Whether the condition was evaluated. False for `#else`, for an arm
    /// after one already taken, and for every arm of an unevaluated chain.
    pub evaluated: bool,
    /// Names the condition depends on, first read first, without repeats.
    /// For an evaluated arm these are every name the evaluation consulted,
    /// macro expansion included — `#if HAS_X` with `#define HAS_X
    /// defined(X)` reads both `HAS_X` and `X`. For an arm that was not
    /// evaluated they are the identifiers spelled in the expression, with
    /// `bound` unknown. `defined`, `__LINE__` and the alternative operator
    /// spellings (`and`, `not`, …) are operators, not reads, except when
    /// used as explicit macro operands of `defined`, `#ifdef` or `#ifndef`.
    pub reads: Vec<ConditionRead>,
}

impl ConditionalArm {
    /// Source lines strictly between this arm's directive and the directive
    /// that ends it. Opening directive lines are not counted, but their
    /// continuation lines and nested directives are. This is a physical
    /// line count, not code reachability.
    pub fn body_lines(&self) -> u32 {
        self.end_line.saturating_sub(self.line).saturating_sub(1)
    }
}

/// One `#if` … `#endif` chain in one file, in the run that met it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionalChain {
    /// The file the chain is in (canonical), which is the file the
    /// preprocessor was processing when it met the opening directive.
    pub file: PathBuf,
    pub arms: Vec<ConditionalArm>,
    /// Nesting depth within its file: 0 for a chain not enclosed by any
    /// other chain of the same file. Includer frames do not count.
    pub depth: usize,
    /// Whether an `#endif` closed the chain. An unterminated chain is closed
    /// at its file's end (with an error diagnostic) and its last arm ends on
    /// the line after the file's last line.
    pub terminated: bool,
    /// The chain is the file's include guard: `#ifndef X` (or
    /// `#if !defined(X)`) before any other token of the file, `#define X` as
    /// the very next directive, and its `#endif` followed by nothing but
    /// whitespace. A guard is a conditional that always takes its first arm
    /// on the file's first inclusion, and the name it tests is not
    /// configuration.
    pub include_guard: bool,
}

impl ConditionalChain {
    /// Line of the opening directive.
    pub fn line(&self) -> u32 {
        self.arms.first().map_or(0, |arm| arm.line)
    }
}
