use trace_ir::FlowConstraint;
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn expressions_share_across_units_origins_and_return_operations() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        std::fs::write(
            dir.path().join(format!("{name}.c")),
            format!(
                "char *id(char *q);\nvoid {name}(char *p, char *q) {{\n    p = id(q);\n    p = id(q);\n}}\n"
            ),
        )
        .unwrap();
    }
    let mut program =
        trace_parse::build_program_for_analysis_with_jobs(dir.path(), &PreprocessOptions::new(), 2)
            .unwrap();
    let operations: Vec<_> = program
        .flow_origins
        .values()
        .flatten()
        .filter(|(_, text)| text.as_ref() == "p = id(q)")
        .collect();
    assert_eq!(operations.len(), 4);
    for (_, text) in &operations {
        assert!(Arc::ptr_eq(text, &operations[0].1));
    }
    let calls: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| site.callee_name == "id")
        .collect();
    assert_eq!(calls.len(), 4);
    for site in &calls {
        let details = site.details.as_ref().unwrap();
        let (span, text) = details.return_operation.as_deref().unwrap();
        assert!(Arc::ptr_eq(text, &operations[0].1));
        assert!(operations.iter().any(|(origin, _)| origin == span));
        assert!(Arc::ptr_eq(
            details.call_expression.as_ref().unwrap(),
            calls[0]
                .details
                .as_ref()
                .unwrap()
                .call_expression
                .as_ref()
                .unwrap(),
        ));
    }
    let text = Arc::clone(&operations[0].1);
    // The indexing-only pool has gone; these are the eight provenance owners
    // plus this test's reference. Coordinates and occurrence records stay separate.
    assert_eq!(Arc::strong_count(&text), 9);
    program.release_flow();
    assert_eq!(Arc::strong_count(&text), 1);
}

#[test]
fn deferred_references_keep_assignment_initializer_and_macro_origins() {
    let dir = tempfile::tempdir().unwrap();
    let source = "typedef void (*Fn)(void);\nstruct Ops { Fn init; };\nstruct Ops table;\nFn direct = Late;\nFn address = &Late;\n#define BIND table.init = Late\nvoid bind(void) {\n    table.init = Late;\n    direct = Late;\n    address = &Late;\n    BIND;\n}\nvoid Late(void) {}\n";
    std::fs::write(dir.path().join("main.c"), source).unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let mut sites = Vec::new();
    for flow in &program.flow {
        if matches!(
            flow,
            FlowConstraint::AddrOfFn { .. } | FlowConstraint::Store { .. }
        ) {
            let origins = program
                .flow_origins
                .get(flow)
                .expect("deferred constraints retain origins");
            for (span, text) in origins {
                sites.push((
                    span.line,
                    span.col,
                    text.split_whitespace().collect::<String>(),
                ));
            }
        }
    }
    for (line, col, expression) in [
        (4, 13, "Late"),
        (5, 14, "&Late"),
        (8, 5, "table.init=Late"),
        (9, 5, "direct=Late"),
        (10, 5, "address=&Late"),
        (11, 5, "table.init=Late"),
    ] {
        assert!(
            sites.contains(&(line, col, expression.to_owned())),
            "missing {line}:{col} {expression}: {sites:?}"
        );
    }
}

#[test]
fn repeated_constraints_keep_distinct_origins_in_source_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = String::from("#define TWICE p = q; p = q;\nvoid f(char *p, char *q) {\n");
    for _ in 0..4096 {
        source.push_str("    p = q;\n");
    }
    source.push_str("    TWICE\n    p = (q);\n}\n");
    std::fs::write(dir.path().join("main.c"), source).unwrap();
    for _ in 0..2 {
        let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
        let var = |name| {
            program
                .symbols
                .variables
                .iter()
                .find(|v| v.name == name)
                .unwrap()
                .id
        };
        let flow = FlowConstraint::Copy {
            dst: var("p"),
            src: var("q"),
        };
        assert_eq!(program.flow.iter().filter(|f| **f == flow).count(), 4099);
        let origins = &program.flow_origins[&flow];
        assert_eq!(
            origins.len(),
            4098,
            "identical macro origins must be deduplicated"
        );
        for (index, (span, text)) in origins.iter().enumerate() {
            assert_eq!(span.line, index as u32 + 3);
            assert_eq!(span.col, 5);
            assert_eq!(
                text.split_whitespace().collect::<String>(),
                if index == 4097 { "p=(q)" } else { "p=q" }
            );
        }
    }
}

#[test]
fn minified_unicode_lines_recover_exact_expressions_and_columns() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = "void f(char *p, char *q) { /*é中🙂*/ ";
    let operation = "p /* unchanged */ = q;";
    let count = 4096;
    let source = format!("{prefix}{}\np = q;\n}}", operation.repeat(count));
    std::fs::write(dir.path().join("main.c"), source).unwrap();
    for _ in 0..2 {
        let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
        let var = |name| {
            program
                .symbols
                .variables
                .iter()
                .find(|v| v.name == name)
                .unwrap()
                .id
        };
        let flow = FlowConstraint::Copy {
            dst: var("p"),
            src: var("q"),
        };
        let origins = &program.flow_origins[&flow];
        assert_eq!(origins.len(), count + 1);
        for (i, (span, expression)) in origins[..count].iter().enumerate() {
            assert_eq!(span.line, 1);
            assert_eq!(
                span.col as usize,
                prefix.chars().count() + i * operation.len() + 1
            );
            assert_eq!(expression.as_ref(), "p /* unchanged */ = q");
        }
        let (span, expression) = &origins[count];
        assert_eq!((span.line, span.col), (2, 1));
        assert_eq!(expression.as_ref(), "p = q");
    }
}

#[test]
fn aggregate_elements_keep_closest_origins_including_deferred_references() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/dataflow_initializer_origins");
    let program = build_program(&root, &PreprocessOptions::new()).unwrap();
    let origins_for = |predicate: fn(&FlowConstraint) -> bool| {
        program
            .flow_origins
            .iter()
            .filter(|(flow, _)| predicate(flow))
            .flat_map(|(_, origins)| origins.iter())
            .map(|(span, text)| (span.line, text.split_whitespace().collect::<String>()))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let expected = std::collections::BTreeSet::from([
        (6, "{Later}".to_owned()),
        (7, "{&value}".to_owned()),
        (10, "{.callback=Later}".to_owned()),
        (11, "{.pointer=&value}".to_owned()),
        (14, "{.inner.callback=Later}".to_owned()),
        (15, "{.inner.pointer=&value}".to_owned()),
    ]);
    assert_eq!(
        origins_for(|flow| matches!(flow, FlowConstraint::Store { .. })),
        expected
    );
    let functions = origins_for(|flow| matches!(flow, FlowConstraint::AddrOfFn { .. }));
    assert_eq!(
        functions,
        expected
            .into_iter()
            .filter(|(_, text)| text.contains("Later"))
            .collect()
    );
    let containers = origins_for(|flow| {
        matches!(flow,
        FlowConstraint::GepField { field_name, .. } if field_name == "inner")
    });
    assert_eq!(
        containers,
        std::collections::BTreeSet::from([
            (14, "{.inner.callback=Later}".to_owned()),
            (15, "{.inner.pointer=&value}".to_owned()),
            (18, ".inner".to_owned()),
        ])
    );
}

#[test]
fn large_aggregate_provenance_text_is_bounded_per_element() {
    for fields in [128, 1000] {
        for designated in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut source = String::from("char payload;\nstruct Many {\n");
            for i in 0..fields {
                source.push_str(&format!("    char *f{i};\n"));
            }
            source.push_str("};\nstruct Many table = {\n");
            for i in 0..fields {
                if designated {
                    source.push_str(&format!("    .f{i} = &payload,\n"));
                } else {
                    source.push_str("    &payload,\n");
                }
            }
            source.push_str("};\n");
            std::fs::write(dir.path().join("main.c"), source).unwrap();
            let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
            let origins: Vec<_> = program.flow_origins.values().flatten().collect();
            assert_eq!(origins.len(), fields * 3);
            let bytes: usize = origins.iter().map(|(_, text)| text.len()).sum();
            assert!(
                bytes <= fields * 128,
                "{fields} fields retained {bytes} expression bytes"
            );
            for (span, text) in origins {
                assert!(text.len() <= 64, "an origin copied sibling elements");
                assert!(span.line >= fields as u32 + 5 && span.line <= fields as u32 * 2 + 4);
            }
        }
    }
}
