use std::path::PathBuf;
use trace_parse::{build_program, parse_c_source};
use trace_preproc::{preprocess_file, PreprocessOptions};

#[test]
fn in_out_preprocessed_still_has_assignment() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/in_out_ptr/main.c");
    let pre = preprocess_file(
        &path,
        &PreprocessOptions::new().with_include(path.parent().unwrap().into()),
    )
    .unwrap();
    let parsed = parse_c_source(pre.output).unwrap();
    let mut found = false;
    fn walk(node: tree_sitter::Node, found: &mut bool) {
        if node.kind() == "assignment_expression" {
            *found = true;
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            walk(ch, found);
        }
    }
    walk(parsed.tree.root_node(), &mut found);
    assert!(found, "assignment missing after preprocess");

    let program = build_program(
        path.parent().unwrap(),
        &PreprocessOptions::new().with_include(path.parent().unwrap().into()),
    )
    .unwrap();
    assert!(
        program
            .flow
            .iter()
            .any(|f| matches!(f, trace_ir::FlowConstraint::Store { .. })),
        "expected store flow after preprocess"
    );
}
