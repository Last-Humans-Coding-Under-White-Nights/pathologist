use std::{fs, path::PathBuf};
use trace_ir::{Linkage, StorageClass, TypeDesc};
use trace_parse::{build_program, has_parse_errors, parse_c_source};
use trace_preproc::{preprocess_file, PreprocessOptions};

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
            program.types.get(variable.type_id).desc,
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
                program.types.get(function.return_type).desc,
                TypeDesc::Void
            ));
        } else {
            assert!(matches!(
                program.types.get(function.return_type).desc,
                TypeDesc::Int
            ));
        }
    }
}
