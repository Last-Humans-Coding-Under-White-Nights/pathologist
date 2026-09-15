use std::{fs, path::PathBuf};
use trace_ir::{Linkage, StorageClass, TypeDesc};
use trace_parse::{
    build_program, has_parse_errors, parse_c_source, parse_source_with_lang, SourceLang,
};
use trace_preproc::{preprocess_file, PreprocessOptions};

#[test]
fn attribute_argument_expansion_cannot_discard_a_declaration() {
    let source = include_str!("../../../tests/fixtures/preproc/attribute_escape.c");
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
            include_str!("../../../tests/fixtures/preproc/attribute_for.c"),
        ),
        (
            "cpp",
            SourceLang::Cpp,
            include_str!("../../../tests/fixtures/preproc/attribute_lambda.cpp"),
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
