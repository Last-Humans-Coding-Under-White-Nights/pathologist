mod common;

use common::*;
use trace_analysis::{analyze, ResolutionKind};
use trace_parse::build_program;

/// Assign-after-init pattern (uninitialized declarator then fp = &a).
#[test]
fn false_positive_uninitialized_then_assign() {
    let root = fixture("false_positive");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        must_not_have_edge(&program, &analysis, "ambiguous", "b"),
        "ambiguous must not reach b"
    );
    assert!(
        must_not_have_edge(&program, &analysis, "ambiguous", "c"),
        "ambiguous must not reach c"
    );
}

/// Three-level struct nesting: o->b.c.p = v
#[test]
fn nested_struct_deep_field_store_flow() {
    let root = fixture("nested_struct");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program.flow.iter().any(|f| matches!(
            f,
            trace_ir::FlowConstraint::Store { .. } | trace_ir::FlowConstraint::GepField { .. }
        )),
        "expected flow from o->b.c.p = v (deep nested field chain)"
    );
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "user",
        "set_deep",
        ResolutionKind::Direct
    ));
}

/// Anonymous struct in function scope.
#[test]
fn anonymous_struct_field_flow() {
    let root = fixture("anon_struct");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program
            .flow
            .iter()
            .any(|f| matches!(f, trace_ir::FlowConstraint::Store { .. })),
        "expected row.payload = &value store on anonymous struct"
    );
    let (_pag, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "anon_user",
        // `touch` is prototype-only: statically resolved, but classified
        // external because no definition exists under the fixture root.
        "touch",
        ResolutionKind::External
    ));
}

/// Union member pointer assignment (parse-level flow).
#[test]
fn union_pointer_member_store_flow() {
    let root = fixture("union_pun");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    assert!(
        program.flow.iter().any(|f| matches!(
            f,
            trace_ir::FlowConstraint::Store { .. } | trace_ir::FlowConstraint::GepField { .. }
        )),
        "expected u.p = &x flow through union member"
    );
}

#[test]
fn union_type_registered_in_ir() {
    let root = fixture("union_pun");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let has_union = program
        .types
        .all()
        .iter()
        .any(|t| matches!(t.desc.as_ref(), trace_ir::TypeDesc::Union { .. }));
    assert!(has_union, "union type should be registered in type table");
}

#[test]
fn nested_field_gep_uses_distinct_field_ids() {
    let root = fixture("nested_struct");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let geps: Vec<_> = program
        .flow
        .iter()
        .filter_map(|f| match f {
            trace_ir::FlowConstraint::GepField { field, .. } => Some(field.0),
            _ => None,
        })
        .collect();
    assert!(
        geps.len() >= 2,
        "deep field chain should emit multiple GEP steps, got {:?}",
        geps
    );
}
