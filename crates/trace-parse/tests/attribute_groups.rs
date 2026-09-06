use std::{fs, path::PathBuf};
use trace_parse::{has_parse_errors, parse_c_source};
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
    let parsed = parse_c_source(preprocessed.output).unwrap();
    assert!(
        !has_parse_errors(&parsed.tree),
        "balanced compiler attributes should be gone before parsing"
    );
}
