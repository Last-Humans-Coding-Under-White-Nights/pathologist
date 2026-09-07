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
        /// collector — parse_macro_param_list pushes `"__VA_ARGS__"` for the
        /// anonymous `...` form. A hand-built variadic def (builtins, tests)
        /// must uphold this or the last named parameter will swallow every
        /// argument; substitute_macro debug_asserts it.
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

pub fn new_shared_macro_table() -> SharedMacroTable {
    Arc::new(RwLock::new(MacroTable::new()))
}

/// Object-like macros for command-line `-D` definitions, their bodies
/// lexed as `language` (see [`Language`] for what differs).
pub fn macro_table_from_defines(
    defines: &indexmap::IndexMap<String, String>,
    language: Language,
) -> MacroTable {
    let mut table = MacroTable::new();
    for (name, val) in defines {
        table.insert(
            name.clone(),
            MacroDef::Object {
                replacement: lex_macro_body(val, language),
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
