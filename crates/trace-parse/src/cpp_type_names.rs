//! Lexical vocabulary of C++ type spellings shared by lowering and the
//! parse-input normalizer (`parse.rs`), which must agree on what names a
//! fundamental type, independently of any symbol table.

/// A keyword scalar, `auto`, or a standard integer / `nullptr_t` name: never
/// a class, so never qualified to the namespace it is spelled in.
pub(crate) fn is_fundamental_type_name(name: &str) -> bool {
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
