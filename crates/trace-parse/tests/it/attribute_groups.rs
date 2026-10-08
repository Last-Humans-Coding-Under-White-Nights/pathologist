use std::{fs, path::PathBuf};
use trace_ir::{Linkage, StorageClass, TypeDesc};
use trace_parse::{
    build_program, has_parse_errors, parse_c_source, parse_source_with_lang, SourceLang,
};
use trace_preproc::{preprocess_file, PreprocessOptions};

#[test]
fn attribute_argument_expansion_cannot_discard_a_declaration() {
    let source = include_str!("../../../../tests/fixtures/preproc/attribute_escape.c");
    let mut missing = Vec::new();
    for spelling in ["direct", "alias", "replacement"] {
        let source = match spelling {
            "alias" => format!(
                "#define BASE __attribute__\n#define ATTR BASE\n{}",
                source.replace("__attribute__", "ATTR")
            ),
            "replacement" => source.replace(
                "int x __attribute__((PAYLOAD));",
                "#define DECL int x __attribute__((PAYLOAD));\nDECL",
            ),
            _ => source.to_owned(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("escape.c");
        fs::write(&path, source).unwrap();
        let preprocessed = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
        let parsed = parse_c_source(preprocessed.output).unwrap();
        // Escaped groups are retained conservatively. Tree-sitter can diagnose
        // their GNU spelling, but both declarations must reach the IR.
        let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
        for name in ["x", "preserved"] {
            if !program.symbols.variables.iter().any(|v| v.name == name) {
                missing.push(format!("{spelling}: lost {name} from {}", parsed.source));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn noise_attributes_in_enclosing_expressions_are_elided() {
    for (extension, lang, source) in [
        (
            "c",
            SourceLang::C,
            include_str!("../../../../tests/fixtures/preproc/attribute_for.c"),
        ),
        (
            "cpp",
            SourceLang::Cpp,
            include_str!("../../../../tests/fixtures/preproc/attribute_lambda.cpp"),
        ),
    ] {
        for spelling in ["__attribute__", "ATTR", "WRAP"] {
            let source = match spelling {
                "ATTR" => format!(
                    "#define BASE __attribute__\n#define ATTR BASE\n{}",
                    source.replace("__attribute__", "ATTR")
                ),
                "WRAP" => format!("#define WRAP {}\nWRAP\n", source.replace('\n', " ")),
                _ => source.to_owned(),
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("expressions.{extension}"));
            fs::write(&path, source).unwrap();
            let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
            assert!(
                !result.output.contains("__attribute"),
                "{spelling}: {}",
                result.output
            );
            let parsed = parse_source_with_lang(result.output, lang).unwrap();
            assert!(
                !has_parse_errors(&parsed.tree),
                "{spelling}: {}",
                parsed.source
            );
        }
    }
}

#[test]
fn eliding_balanced_attribute_does_not_repair_unclosed_parameter_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("malformed.c");
    fs::write(
        &path,
        "void foo(int x __attribute__((used) ) { int y; }\nint still_here;\n",
    )
    .unwrap();
    let result = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
    let compact: String = result.output.split_whitespace().collect();
    assert!(compact.contains("{inty;}"), "{}", result.output);
    assert!(compact.contains("intstill_here;"), "{}", result.output);
    let parsed = parse_c_source(result.output).unwrap();
    assert!(has_parse_errors(&parsed.tree));
}

#[test]
fn compiler_attributes_no_longer_turn_valid_declarations_into_parse_errors() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preproc/attribute_groups.c");
    let source = fs::read_to_string(&path).unwrap();
    let raw = parse_c_source(source).unwrap();
    assert!(
        has_parse_errors(&raw.tree),
        "the fixture must retain the tree-sitter failure this regression covers"
    );

    let preprocessed = preprocess_file(&path, &PreprocessOptions::new()).unwrap();
    for attribute in [
        "constructor",
        "destructor",
        "__weak__",
        "__visibility__",
        "__alias__",
        "__cleanup__",
        "noreturn",
        "selectany",
        "dllexport",
        "dllimport",
    ] {
        assert!(
            preprocessed.output.contains(attribute),
            "missing {attribute}: {}",
            preprocessed.output
        );
    }
    let parsed = parse_c_source(preprocessed.output).unwrap();
    assert!(
        !has_parse_errors(&parsed.tree),
        "noise attributes should be elided and meaningful attributes should parse"
    );

    let dir = tempfile::tempdir().unwrap();
    fs::copy(&path, dir.path().join("attributes.c")).unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    for name in ["before_type", "after_declarator", "msvc_aligned"] {
        let variable = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .expect(name);
        assert_eq!(variable.storage, StorageClass::Global);
        assert!(matches!(
            program.types.get(variable.type_id).desc.as_ref(),
            TypeDesc::Int
        ));
        assert!((1..=3).contains(&variable.span.line));
    }
    for (name, line, defined) in [("trace_log", 6, false), ("stop_now", 8, true)] {
        let function = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == name)
            .expect(name);
        assert_eq!(function.linkage, Linkage::External);
        assert_eq!(function.span.line, line);
        assert_eq!(function.is_defined, defined);
        if name == "stop_now" {
            assert!(matches!(
                program.types.get(function.return_type).desc.as_ref(),
                TypeDesc::Void
            ));
        } else {
            assert!(matches!(
                program.types.get(function.return_type).desc.as_ref(),
                TypeDesc::Int
            ));
        }
    }
}

#[test]
fn attributed_friend_operator_declarations() {
    for spelling in [
        "[[nodiscard]]",
        "[[__nodiscard__]]",
        "[[nodiscard]] constexpr",
        "constexpr [[nodiscard]]",
        "",
    ] {
        let attr = if spelling.is_empty() {
            String::new()
        } else {
            format!("{spelling} ")
        };
        let source = format!(
            "struct Value {{\n\
             \x20   int value;\n\
             \x20   {attr}friend bool operator==(Value a, Value b) {{\n\
             \x20       return a.value == b.value;\n\
             \x20   }}\n\
             }};\n\
             \n\
             int after_friend();\n\
             int main() {{ return 0; }}\n"
        );
        let parsed = parse_source_with_lang(source.as_str(), SourceLang::Cpp).unwrap();
        assert!(
            !has_parse_errors(&parsed.tree),
            "{spelling}: parse error in tree-sitter AST"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.cpp");
        fs::write(&path, &source).unwrap();

        let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
        assert!(
            !program.diagnostics.iter().any(|d| d.stage == "parse"),
            "{spelling}: parse diagnostic present: {:?}",
            program.diagnostics
        );

        let op = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "operator==")
            .unwrap_or_else(|| panic!("{spelling}: missing operator=="));
        assert_eq!(op.span.line, 3, "{spelling}: line start");
        assert_eq!(op.end_line, 5, "{spelling}: line end");
        assert!(op.is_defined, "{spelling}: should be defined");
        assert_eq!(op.linkage, Linkage::External);
        assert_eq!(
            op.params.len(),
            2,
            "{spelling}: friend operator must have 2 parameters, no implicit this"
        );
        assert_eq!(op.explicit_arity, Some(2));
        assert!(!op.declared_in_class);

        let after = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "after_friend")
            .unwrap_or_else(|| panic!("{spelling}: missing after_friend"));
        assert_eq!(after.span.line, 8, "{spelling}: after_friend line");
        assert!(!after.is_defined);

        let main_fn = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "main")
            .unwrap_or_else(|| panic!("{spelling}: missing main"));
        assert_eq!(main_fn.span.line, 9, "{spelling}: main line");
        assert!(main_fn.is_defined);

        // Ensure no phantom symbols like Value::nodiscard, Value::operator==, etc.
        for phantom in [
            "Value::nodiscard",
            "Value::__nodiscard__",
            "Value::operator==",
            "nodiscard",
            "__nodiscard__",
        ] {
            assert!(
                !program.symbols.functions.iter().any(|f| f.name == phantom),
                "{spelling}: phantom function {phantom} appeared"
            );
        }
    }
}

#[test]
fn attributed_friend_operator_in_dependency_header() {
    let scratch = tempfile::tempdir().unwrap();
    let dep = scratch.path().join("dep");
    fs::create_dir(&dep).unwrap();
    fs::write(
        dep.join("dep.hpp"),
        "struct DepValue {\n\
         \x20   int value;\n\
         \x20   [[nodiscard]] friend bool operator==(DepValue a, DepValue b);\n\
         \x20   [[__nodiscard__]] friend bool operator!=(DepValue a, DepValue b) {\n\
         \x20       return !(a == b);\n\
         \x20   }\n\
         };\n",
    )
    .unwrap();
    fs::write(
        scratch.path().join("main.cpp"),
        "#include \"dep.hpp\"\nint main() { return 0; }\n",
    )
    .unwrap();
    let opts = PreprocessOptions::new()
        .with_include(dep.clone())
        .with_dep(&dep);
    let program = build_program(scratch.path(), &opts).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "unexpected parse diagnostics: {:?}",
        program.diagnostics
    );
    let eq_op = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "operator==")
        .expect("missing operator==");
    assert!(program.symbols.file_is_dep(eq_op.file));
    assert!(!eq_op.is_defined);
    assert_eq!(eq_op.span.line, 3);
    assert_eq!(eq_op.params.len(), 2);
    assert_eq!(eq_op.explicit_arity, Some(2));

    let ne_op = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "operator!=")
        .expect("missing operator!=");
    assert!(program.symbols.file_is_dep(ne_op.file));
    assert!(!ne_op.is_defined);
    assert_eq!(ne_op.span.line, 4);
    assert_eq!(ne_op.params.len(), 2);
    assert_eq!(ne_op.explicit_arity, Some(2));

    for phantom in [
        "DepValue::operator==",
        "DepValue::operator!=",
        "DepValue::nodiscard",
        "DepValue::__nodiscard__",
    ] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == phantom),
            "phantom function {phantom} appeared"
        );
    }
}

#[test]
fn namespaced_friend_operator() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "namespace my_ns {\n\
         struct Value {\n\
         \x20   int v;\n\
         \x20   [[nodiscard]] friend bool operator==(Value a, Value b) {\n\
         \x20       return a.v == b.v;\n\
         \x20   }\n\
         };\n\
         }\n\
         int main() { return 0; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));
    let op = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "my_ns::operator==")
        .expect("missing my_ns::operator==");
    assert_eq!(op.params.len(), 2);
    assert_eq!(op.explicit_arity, Some(2));
    assert!(op.is_defined);
}

#[test]
fn friend_operator_out_of_line_definition_merge() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("value.hpp"),
        "#pragma once\n\
         struct Value {\n\
         \x20   int v;\n\
         \x20   [[nodiscard]] friend bool operator==(Value a, Value b);\n\
         };\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("value.cpp"),
        "#include \"value.hpp\"\n\
         bool operator==(Value a, Value b) {\n\
         \x20   return a.v == b.v;\n\
         }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "#include \"value.hpp\"\n\
         int main() { return 0; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));
    let matching_ops: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "operator==")
        .collect();
    assert_eq!(
        matching_ops.len(),
        1,
        "friend proto and def should merge into one symbol"
    );
    assert!(matching_ops[0].is_defined);
    assert_eq!(matching_ops[0].params.len(), 2);
}

#[test]
fn friend_operator_call_resolution_and_arg_flow() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "struct Value {\n\
         \x20   int v;\n\
         \x20   [[nodiscard]] friend bool operator==(Value a, Value b) {\n\
         \x20       return a.v == b.v;\n\
         \x20   }\n\
         };\n\
         int main() {\n\
         \x20   Value x;\n\
         \x20   Value y;\n\
         \x20   ::operator==(x, y);\n\
         \x20   return 0;\n\
         }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "diagnostics: {:?}",
        program.diagnostics
    );
    let op = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "operator==")
        .expect("missing operator==");
    assert_eq!(op.params.len(), 2);
    let main_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "main")
        .expect("missing main");
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|s| s.caller == main_fn.id && s.callee_name.ends_with("operator=="))
        .expect("missing callsite for operator==");
    assert!(!site.args_bound_past_this);
    let callees = program.symbols.callees_of(site);
    assert!(
        callees.contains(&op.id),
        "call site must resolve to operator=="
    );
}

#[test]
fn friend_declaration_preserves_namespace_function_ownership() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "namespace N {\n\
         void target() {}\n\
         struct C;\n\
         void foo(C&);\n\
         struct C {\n\
         \x20   friend void foo(C&);\n\
         \x20   void run() { foo(*this); }\n\
         };\n\
         void foo(C&) { target(); }\n\
         void entry(C& c) { c.run(); }\n\
         }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));

    // Verify foo is N::foo, NOT N::C::foo
    let foo_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "N::foo")
        .expect("missing N::foo");
    assert!(foo_fn.is_defined);
    assert_eq!(
        foo_fn.params.len(),
        1,
        "free function must have 1 param, no this"
    );
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "N::C::foo"));

    // Verify N::C::run call site resolves to N::foo with argument 0
    let run_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "N::C::run")
        .expect("missing N::C::run");
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|s| s.caller == run_fn.id && s.callee_name.ends_with("foo"))
        .expect("missing call site for foo in run");
    assert_eq!(site.callee_name, "foo");
    assert!(!site.args_bound_past_this);
    let callees = program.symbols.callees_of(site);
    assert!(
        callees.contains(&foo_fn.id),
        "call site in run must resolve to N::foo"
    );
}

#[test]
fn inline_friend_function_in_namespace() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "namespace demo {\n\
         struct Value {\n\
         \x20   int value;\n\
         \x20   bool compare(const Value& lhs, const Value& rhs) {\n\
         \x20       return foo(lhs, rhs);\n\
         \x20   }\n\
         \x20   [[nodiscard]] friend bool foo(const Value& lhs, const Value& rhs) {\n\
         \x20       return lhs.value == rhs.value;\n\
         \x20   }\n\
         };\n\
         }\n\
         int main() { return 0; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));

    let foo_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "demo::foo")
        .expect("missing demo::foo");
    assert!(foo_fn.is_defined);
    assert_eq!(
        foo_fn.params.len(),
        2,
        "friend function foo must have 2 params, no this"
    );
    assert_eq!(foo_fn.explicit_arity, Some(2));
    assert!(!foo_fn.declared_in_class);
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "demo::Value::foo"));

    let compare_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "demo::Value::compare")
        .expect("missing demo::Value::compare");
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|s| s.caller == compare_fn.id && s.callee_name.ends_with("foo"))
        .expect("missing call site for foo in compare");
    assert_eq!(site.callee_name, "foo");
    assert!(!site.args_bound_past_this);
    let callees = program.symbols.callees_of(site);
    assert!(
        callees.contains(&foo_fn.id),
        "call site in compare must resolve to demo::foo"
    );
}

#[test]
fn friend_function_lexical_class_lookup_for_static_member() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "void wrong() {}\n\
         void right() {}\n\
         void helper() { wrong(); }\n\
         struct C {\n\
         \x20   static void helper();\n\
         \x20   friend void run(C) { helper(); }\n\
         };\n\
         void C::helper() { right(); }\n\
         void entry(C c) { run(c); }\n\
         int main() { return 0; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));

    // Verify run is a free function (not C::run, 1 param, no this)
    let run_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "run")
        .expect("missing run");
    assert!(run_fn.is_defined);
    assert_eq!(run_fn.params.len(), 1, "run must take 1 param (C), no this");
    assert!(!program.symbols.functions.iter().any(|f| f.name == "C::run"));

    let c_helper = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "C::helper")
        .expect("missing C::helper");
    assert!(c_helper.is_defined);

    let global_helper = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "helper")
        .expect("missing global helper");

    // Call site inside run must resolve to C::helper, NOT global ::helper
    let run_sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == run_fn.id)
        .collect();
    assert_eq!(run_sites.len(), 1, "run must have exactly 1 call site");
    let site = run_sites[0];
    let callees = program.symbols.callees_of(site);
    assert!(
        callees.contains(&c_helper.id),
        "run must resolve to C::helper"
    );
    assert!(
        !callees.contains(&global_helper.id),
        "run must NOT resolve to global ::helper"
    );
}

#[test]
fn friend_function_lexical_class_lookup_for_inline_static_member() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "void wrong() {}\n\
         void right() {}\n\
         void helper() { wrong(); }\n\
         struct C {\n\
         \x20   static void helper() { right(); }\n\
         \x20   friend void run(C) { helper(); }\n\
         };\n\
         void entry(C c) { run(c); }\n\
         int main() { return 0; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));

    let run_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "run")
        .expect("missing run");
    assert!(run_fn.is_defined);
    assert_eq!(run_fn.params.len(), 1, "run must take 1 param (C), no this");
    assert!(!program.symbols.functions.iter().any(|f| f.name == "C::run"));

    let c_helper = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "C::helper")
        .expect("missing C::helper");
    assert!(c_helper.is_defined);

    let global_helper = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "helper")
        .expect("missing global helper");

    let run_sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == run_fn.id)
        .collect();
    assert_eq!(run_sites.len(), 1, "run must have exactly 1 call site");
    let site = run_sites[0];
    let callees = program.symbols.callees_of(site);
    assert!(
        callees.contains(&c_helper.id),
        "run must resolve to C::helper"
    );
    assert!(
        !callees.contains(&global_helper.id),
        "run must NOT resolve to global ::helper"
    );
}

#[test]
fn friend_function_block_scoped_import_shadows_enclosing_class() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("main.cpp"),
        "namespace N {\n\
         \x20   void target() {}\n\
         \x20   void helper() { target(); }\n\
         }\n\
         struct C {\n\
         \x20   static void helper() {}\n\
         \x20   friend void run(C) {\n\
         \x20       using N::helper;\n\
         \x20       helper();\n\
         \x20   }\n\
         };\n\
         void entry(C c) { run(c); }\n\
         int main() { return 0; }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));

    let run_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "run")
        .expect("missing run");
    assert!(run_fn.is_defined);

    let n_helper = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "N::helper")
        .expect("missing N::helper");
    assert!(n_helper.is_defined);

    let c_helper = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "C::helper")
        .expect("missing C::helper");

    let run_sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == run_fn.id)
        .collect();
    assert_eq!(run_sites.len(), 1, "run must have exactly 1 call site");
    let site = run_sites[0];
    let callees = program.symbols.callees_of(site);
    assert!(
        callees.contains(&n_helper.id),
        "run must resolve to N::helper"
    );
    assert!(
        !callees.contains(&c_helper.id),
        "run must NOT resolve to C::helper"
    );
}
