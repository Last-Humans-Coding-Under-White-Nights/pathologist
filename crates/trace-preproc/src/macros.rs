use crate::{Language, Token, TokenKind};
use indexmap::IndexMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub enum MacroDef {
    Object {
        replacement: Vec<Token>,
    },
    Function {
        params: Vec<String>,
        replacement: Vec<Token>,
        /// Invariant: when true, the LAST entry of `params` is the variadic
        /// collector — `parse_macro_param_list` pushes `"__VA_ARGS__"` for the
        /// anonymous `...` form. A hand-built variadic def (builtins, tests)
        /// must uphold this or the last named parameter will swallow every
        /// argument; `substitute_macro` debug_asserts it.
        variadic: bool,
    },
    /// Last-resort parser recovery for a gMock declaration macro
    /// (`MOCK_METHOD`, `MOCK_METHODn`, `MOCK_CONST_METHODn`, `…_T`): the
    /// invocation becomes the member prototype it declares. A replacement
    /// list cannot express this — the legacy forms carry the whole
    /// signature in one argument and the modern form parenthesizes
    /// comma-containing return types — so the preprocessor expands it in
    /// code (`expand_gmock_method`). Only the builtin fallback table
    /// creates one.
    GmockMethod,
}

/// One executed macro directive, in program order. Cached include entries
/// record these so replay reproduces the header's effects exactly — a
/// state diff cannot represent a no-op `#undef` (name absent at capture,
/// present in a later consumer) or `#undef X` + `#define X new` of a name
/// that existed at both capture boundaries.
#[derive(Debug, Clone)]
pub enum MacroOp {
    Define(String, MacroDef),
    Undef(String),
}

pub type MacroTable = IndexMap<String, MacroDef>;
pub type SharedMacroTable = Arc<RwLock<MacroTable>>;

#[must_use]
pub fn new_shared_macro_table() -> SharedMacroTable {
    Arc::new(RwLock::new(MacroTable::new()))
}

/// The macros the language itself predefines (C11 6.10.8.1, [cpp.predefined]),
/// as `(name, replacement)`. Every macro table is seeded with these, and a
/// command-line `-D` of the same name outranks them wherever both apply, so
/// a tree that pins its own standard level keeps it (#70).
///
/// Only the names the standards require and that real code tests: the
/// eval corpora read `__cplusplus` from 930 conditionals, and until this
/// existed every one of them took the C arm in a C++ unit. `__cplusplus` is
/// C++17, what the OpenHarmony clang defaults to; the corpora compare it
/// against `201103L` only. `__STDC__` is `1` in both languages (g++ defines
/// it too); `__STDC_VERSION__` is C17 and, like the real compilers, absent
/// from a C++ unit. No compiler is claimed: `__GNUC__` / `__clang__` stay
/// unbound, since either would switch on vendor extensions the parser does
/// not have.
#[must_use]
pub fn predefined_macros(language: Language) -> &'static [(&'static str, &'static str)] {
    match language {
        Language::C => &[("__STDC__", "1"), ("__STDC_VERSION__", "201710L")],
        Language::Cpp => &[("__STDC__", "1"), ("__cplusplus", "201703L")],
    }
}

/// Object-like macros for the language's [`predefined_macros`] and the
/// command-line `-D` definitions, in that order, their bodies lexed as
/// `language` (see [`Language`] for what differs).
#[must_use]
pub fn macro_table_from_defines(
    defines: &indexmap::IndexMap<String, String>,
    language: Language,
) -> MacroTable {
    let mut table = MacroTable::new();
    let predefined = predefined_macros(language)
        .iter()
        .map(|(name, val)| (name.to_string(), val.to_string()));
    let cli = defines
        .iter()
        .map(|(name, val)| (name.clone(), val.clone()));
    for (name, val) in predefined.chain(cli) {
        table.insert(
            name,
            MacroDef::Object {
                replacement: lex_macro_body(&val, language),
            },
        );
    }
    table
}

/// Tokenize a macro replacement list from source text (Eof stripped).
pub(crate) fn lex_macro_body(src: &str, language: Language) -> Vec<Token> {
    crate::Lexer::new(src, language)
        .tokenize()
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Eof))
        .collect()
}
