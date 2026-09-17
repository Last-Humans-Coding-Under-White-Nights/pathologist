use std::fs;
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn weak_attributes_macros_and_pragmas_reach_ir() {
    for extension in ["c", "cpp"] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(format!("weak.{extension}")),
            include_str!("../../../tests/fixtures/weak_annotations/weak.c"),
        )
        .unwrap();
        let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
        for name in ["leading", "trailing", "pragma_fn", "alias_name", "late"] {
            let f = program
                .symbols
                .functions
                .iter()
                .find(|f| f.name == name)
                .unwrap();
            assert!(f.is_weak, "{name}: {f:?}");
        }
        for name in ["value", "pragma_value", "leading_value"] {
            let v = program
                .symbols
                .variables
                .iter()
                .find(|v| v.name == name)
                .unwrap();
            assert!(v.is_weak, "{name}: {v:?}");
        }
        let strong = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "strong")
            .unwrap();
        assert!(!strong.is_weak);
        assert!(
            !program
                .symbols
                .functions
                .iter()
                .find(|f| f.name == "string_attribute")
                .unwrap()
                .is_weak
        );
    }
}

#[test]
fn external_variable_declarations_are_not_strong_definitions() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("globals.c"), "extern int *declared;\nint *tentative;\nextern int initialized = 1;\nint plain;\nextern int extern_plain;\n").unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    for (name, expected) in [
        ("declared", false),
        ("tentative", true),
        ("initialized", true),
        ("plain", true),
        ("extern_plain", false),
    ] {
        let variable = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .unwrap();
        assert_eq!(variable.is_defined, expected, "{name}: {variable:?}");
    }
}

#[test]
fn weak_global_redeclarations_share_binding_in_both_orders() {
    for declaration_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let declaration = "extern void (*configured)(void) __attribute__((weak));\n";
        let definition = "void (*configured)(void) = fallback;\n";
        let source = if declaration_first {
            format!("void fallback(void) {{}}\n{declaration}{definition}")
        } else {
            format!("void fallback(void) {{}}\n{definition}{declaration}")
        };
        fs::write(dir.path().join("globals.c"), source).unwrap();
        let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
        let globals: Vec<_> = program
            .symbols
            .variables
            .iter()
            .filter(|v| v.name == "configured")
            .collect();
        assert!(!globals.is_empty());
        assert!(globals.iter().all(|v| v.is_weak), "{globals:?}");
    }
}

#[test]
fn cached_header_weak_declaration_applies_to_source_definition() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("weak.h"),
        "extern void (*configured)(void) __attribute__((weak));\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.c"),
        "#include \"weak.h\"\nvoid fallback(void) {}\nvoid (*configured)(void) = fallback;\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let global = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "configured" && v.is_defined)
        .unwrap();
    assert!(global.is_weak, "{global:?}");
}

#[test]
fn a_declarators_own_attribute_does_not_weaken_its_siblings() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("siblings.c"),
        "void a(void) __attribute__((weak)), b(void);\n\
         __attribute__((weak)) void c(void), d(void);\n\
         static __attribute__((weak)) void e(void) {}\n\
         __attribute__((weak)) int f;\n\
         void h(void) { int local __attribute__((weak)) = 0; (void)local; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    // The attribute sits inside `a`'s declarator, so it weakens `a` alone;
    // a leading one is a declaration specifier and reaches every declarator.
    for (name, expected) in [("a", true), ("b", false), ("c", true), ("d", true)] {
        let f = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(f.is_weak, expected, "{name}: {f:?}");
    }
    // Internal linkage and block scope have no linkage to weaken.
    let e = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "e")
        .unwrap();
    assert!(!e.is_weak, "static function: {e:?}");
    for (name, expected) in [("f", true), ("local", false)] {
        let v = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(v.is_weak, expected, "{name}: {v:?}");
    }
}

#[test]
fn a_pragma_does_not_weaken_a_namespaced_global_of_the_same_name() {
    // A pragma names a linkage symbol. `app::cb` is mangled, so its
    // unqualified spelling is not the name the pragma weakens -- the same
    // invariant the weak/strong global selection enforces on both sides.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("weak_ns.cpp"),
        "#pragma weak cb\nint cb;\nnamespace app { int cb; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let weak_of = |namespaced: bool| {
        program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == "cb" && v.is_namespaced == namespaced)
            .unwrap_or_else(|| panic!("missing cb (namespaced: {namespaced})"))
            .is_weak
    };
    assert!(weak_of(false), "the global the pragma names is weak");
    assert!(!weak_of(true), "app::cb keeps its own binding");
}
