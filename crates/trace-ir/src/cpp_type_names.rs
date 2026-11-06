//! Lexical C++ type spellings shared by parsing, lowering and signature matching.

/// A keyword scalar, `auto`, or a standard integer / `nullptr_t` name: never
/// a class, so never qualified to the namespace it is spelled in.
pub fn is_fundamental_type_name(name: &str) -> bool {
    !name.is_empty()
        && name.split_whitespace().all(|word| {
            matches!(
                word,
                "int"
                    | "char"
                    | "void"
                    | "bool"
                    | "float"
                    | "double"
                    | "short"
                    | "long"
                    | "signed"
                    | "unsigned"
                    | "wchar_t"
                    | "char8_t"
                    | "char16_t"
                    | "char32_t"
                    | "size_t"
                    | "ssize_t"
                    | "ptrdiff_t"
                    | "intptr_t"
                    | "uintptr_t"
                    | "int8_t"
                    | "int16_t"
                    | "int32_t"
                    | "int64_t"
                    | "uint8_t"
                    | "uint16_t"
                    | "uint32_t"
                    | "uint64_t"
                    | "intmax_t"
                    | "uintmax_t"
                    | "nullptr_t"
                    | "auto"
            )
        })
}

/// A template argument split into what names the type and the pointer /
/// reference punctuation after it, every level kept and cv-qualifiers at any
/// level dropped: `T * const *` is (`T`, `**`), `T &` is (`T`, `&`).
pub fn split_pointer_suffix(arg: &str) -> (&str, String) {
    let mut rest = arg.trim();
    let mut suffix = String::new();
    loop {
        let bare = strip_trailing_cv(rest).trim_end();
        match bare.strip_suffix(['*', '&']) {
            Some(shorter) => {
                suffix.insert(0, bare.chars().next_back().unwrap_or('*'));
                rest = shorter;
            }
            None => return (bare, suffix),
        }
    }
}

/// A non-type template argument spelled as a literal (`4`, `-1`, `true`,
/// `'a'`): never a class, so never qualified to a namespace.
pub fn is_template_literal(arg: &str) -> bool {
    arg.chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, '\'' | '"' | '-'))
        || matches!(arg, "true" | "false" | "nullptr")
}

/// `T const` / `T volatile` spelled after the type, also directly after
/// pointer or reference punctuation (`T*const`).
pub fn strip_trailing_cv(mut s: &str) -> &str {
    loop {
        let t = s.trim_end();
        match t
            .strip_suffix("const")
            .or_else(|| t.strip_suffix("volatile"))
        {
            Some(rest) if rest.ends_with(|c: char| c.is_whitespace() || matches!(c, '*' | '&')) => {
                s = rest;
            }
            _ => return t,
        }
    }
}

/// Portion of a function spelling that can contain its enclosing scope.
/// A conversion operator's target type may itself contain `::`.
pub fn function_scope_spelling(name: &str) -> &str {
    let mut from = 0;
    while let Some(rel) = name[from..].find("operator") {
        let at = from + rel;
        from = at + "operator".len();
        // `operators::f` merely starts with the keyword; `operator ns::S`,
        // `operator=` and `operator new` are the real thing.
        let continues_identifier = name[from..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if (at == 0 || name[..at].ends_with("::")) && !continues_identifier {
            return &name[..at];
        }
    }
    name
}

/// Supported integral non-type arguments, independent of spelling and suffix.
/// Unsupported constant expressions remain unresolved by callers.
pub fn integral_template_value(spelling: &str) -> Option<i128> {
    let text = spelling.trim().replace('\'', "");
    match text.as_str() {
        "true" => return Some(1),
        "false" => return Some(0),
        _ => {}
    }
    let (negative, text) = text
        .strip_prefix('-')
        .map_or((false, text.as_str()), |s| (true, s));
    let text = text
        .strip_prefix('+')
        .unwrap_or(text)
        .trim_end_matches(['u', 'U', 'l', 'L']);
    let (digits, radix) =
        if let Some(s) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            (s, 16)
        } else if let Some(s) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
            (s, 2)
        } else if text.len() > 1 && text.starts_with('0') {
            (&text[1..], 8)
        } else {
            (text, 10)
        };
    let value = i128::from_str_radix(digits, radix).ok()?;
    if negative {
        value.checked_neg()
    } else {
        Some(value)
    }
}
