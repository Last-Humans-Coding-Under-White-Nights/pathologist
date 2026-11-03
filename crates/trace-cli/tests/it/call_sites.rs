use crate::common::*;
use rusqlite::Connection;

analyzed_fixture!(cpp_call_sites);

#[test]
fn implicit_member_call_without_class_header() {
    let (program, analysis) = cpp_call_sites();
    assert!(
        has_any_edge(
            program,
            analysis,
            "app::Context::Dispatch",
            "app::Context::GetCallbacks"
        ),
        "sites: {:?}; edges: {:?}",
        program.symbols.call_sites,
        callees_of(program, analysis, "app::Context::Dispatch"),
    );
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| fn_name(program, site.caller) == "app::Context::Dispatch")
        .collect();
    let getters: Vec<_> = sites
        .iter()
        .filter_map(|site| site.callee_fn_id)
        .filter(|&id| fn_name(program, id) == "app::Context::GetCallbacks")
        .collect();
    assert_eq!(getters.len(), 2);
    assert_ne!(getters[0], getters[1], "arity selects each overload");
    assert!(has_any_edge(
        program,
        analysis,
        "app::Context::Dispatch",
        "app::Callbacks::run"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "app::Context::Dispatch",
        "app::GetCallbacks"
    ));
}

#[test]
fn callgraph_shows_unresolved_calls_as_distinct_leaves() {
    let (program, analysis) = cpp_call_sites();
    let pag = trace_analysis::Pag::build(program);
    let db = export_program(program, &pag, analysis);
    let conn = Connection::open(db.path()).unwrap();
    let caller = only_function(program, "app::untyped");
    let graph = trace_db::call_graph(&conn, caller.0.into(), trace_db::Direction::Down, 2).unwrap();
    let unresolved: Vec<_> = graph
        .edges
        .iter()
        .filter(|e| e.label == "unresolved")
        .collect();
    assert_eq!(unresolved.len(), 2);
    for edge in unresolved {
        let leaf = &graph.nodes[&edge.to];
        assert_eq!(leaf.detail, "[unresolved]");
        assert!(leaf.label.ends_with(".run"));
        assert!(!graph.edges.iter().any(|e| e.from == leaf.id));
    }
    assert!(!graph.truncated);
    let shallow =
        trace_db::call_graph(&conn, caller.0.into(), trace_db::Direction::Down, 0).unwrap();
    assert!(shallow.truncated);
    assert_eq!(shallow.nodes.len(), 1);
    let up = trace_db::call_graph(&conn, caller.0.into(), trace_db::Direction::Up, 2).unwrap();
    assert_eq!(up.nodes.len(), 1);

    let meta = trace_db::GraphMeta {
        title: "unresolved calls",
        direction: "callees",
        depth: 2,
        summary: "",
    };
    for format in [
        trace_db::RenderFormat::Text,
        trace_db::RenderFormat::Json,
        trace_db::RenderFormat::Graphviz,
        trace_db::RenderFormat::Mermaid,
    ] {
        let output = trace_db::render_graph(&graph, format, &meta, &mut |id, out| {
            out.push_str(&graph.nodes[&id].label);
        });
        assert!(output.contains("value.run"));
        assert!(output.contains("unknown().run"));
        if format == trace_db::RenderFormat::Graphviz || format == trace_db::RenderFormat::Mermaid {
            assert!(
                !output.contains("n-"),
                "leaf identifiers must be valid: {output}"
            );
        }
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .args([
            "inspect",
            db.path().to_str().unwrap(),
            "callgraph",
            "--file",
            "main.cpp",
            "--line",
            "12",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("value.run ([unresolved])"), "{text}");
    assert!(text.contains("unknown().run ([unresolved])"), "{text}");
    assert!(
        text.contains("2 functions, 2 unresolved calls, 4 edges"),
        "{text}"
    );
}

#[test]
fn chained_calls_have_distinct_method_positions() {
    let (program, _) = cpp_call_sites();
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| fn_name(program, site.caller) == "app::chained")
        .map(|site| (site.callee_name.as_str(), site.span.line, site.span.col))
        .collect();
    assert_eq!(
        sites,
        vec![
            ("app::Chain::finish", 9, 17),
            ("app::Chain::next", 9, 10),
            ("app::Chain::next", 8, 12),
            ("make", 8, 5),
        ]
    );
}

analyzed_fixture!(conditional_values);

#[test]
fn conditional_values_union_callback_targets() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for caller in [
        "initializer",
        "returned",
        "direct_callee",
        "invoke",
        "assignment",
        "nested",
        "call_arms",
        "call_callee",
        "returned_calls",
        "pointer_arms",
        "stores",
        "macro_conditional",
        "camera_stream",
        "auto_stream",
        "conditional_receiver",
        "field_arms",
        "address_arms",
        "returned_factory",
        "unknown_type_arm",
        "constant_condition",
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert_eq!(
            sites.len(),
            if caller == "stores" { 2 } else { 1 },
            "{caller}: {sites:?}"
        );
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, ["alpha", "beta"], "{caller}: {site:?}");
        }
    }
    for (caller, expected) in [
        ("control", "alpha"),
        ("unknown_arm", "alpha"),
        ("camera_session", "alpha"),
        ("camera_new", "beta"),
        ("auto_null", "alpha"),
        ("null_callback", "alpha"),
        ("zero_arm", "alpha"),
    ] {
        let targets: std::collections::BTreeSet<_> = analysis
            .call_edges
            .iter()
            .filter(|e| fn_name(program, e.caller) == caller)
            .filter(|e| e.resolution == trace_analysis::ResolutionKind::Indirect)
            .map(|e| fn_name(program, e.callee))
            .collect();
        assert_eq!(targets, [expected.to_string()].into(), "{caller}");
    }
    let targets: std::collections::BTreeSet<_> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(program, e.caller) == "cross_returned")
        .filter(|e| e.resolution == trace_analysis::ResolutionKind::Indirect)
        .map(|e| fn_name(program, e.callee))
        .collect();
    assert_eq!(targets, ["local_alpha".into(), "local_beta".into()].into());
}

#[test]
fn conditional_arm_calls_keep_sites_and_return_destinations() {
    let (program, _) = conditional_values();
    for caller in ["call_arms", "call_callee", "call_return", "call_argument"] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller)
            .collect();
        let arms: Vec<_> = sites
            .iter()
            .filter(|s| matches!(s.callee_name.as_str(), "get_alpha" | "get_beta"))
            .collect();
        assert_eq!(arms.len(), 2, "{caller}: {sites:?}");
        assert_ne!(arms[0].span, arms[1].span);
        assert!(arms[0].return_dst.is_some());
        assert_eq!(arms[0].return_dst, arms[1].return_dst);
        let condition: Vec<_> = sites
            .iter()
            .filter(|s| s.callee_name == "condition")
            .collect();
        assert_eq!(condition.len(), 1);
        assert_ne!(condition[0].span, arms[0].span);
        assert_ne!(condition[0].span, arms[1].span);
        assert_ne!(condition[0].return_dst, arms[0].return_dst);
        for arm in arms {
            let matching: Vec<_> = program
                .flow_origins
                .iter()
                .filter(|(flow, _)| {
                    matches!(flow, trace_ir::FlowConstraint::CallReturn { dst, callee_name, .. }
                    if Some(*dst) == arm.return_dst && callee_name == &arm.callee_name)
                })
                .flat_map(|(_, origins)| origins)
                .collect();
            assert!(
                matching.iter().any(|(span, _)| *span == arm.span),
                "{caller}: {matching:?}"
            );
        }
    }
}

#[test]
fn conditional_callee_is_materialized_once_when_returned() {
    let (program, analysis) = conditional_values();
    let caller = only_function(program, "conditional_factory");
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == caller)
        .collect();
    assert_eq!(sites.len(), 1);
    let site = sites[0];
    let callee = site.callee_var.unwrap();
    assert!(site.return_dst.is_some());
    let targets: std::collections::BTreeSet<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.call_site == site.id)
        .map(|e| fn_name(program, e.callee))
        .collect();
    assert_eq!(targets, ["get_alpha".into(), "get_beta".into()].into());
    let refs: Vec<_> = program
        .flow
        .iter()
        .filter_map(|flow| match flow {
            trace_ir::FlowConstraint::AddrOfFn { dst, callee }
                if program.symbols.variable(*dst).fn_id == Some(caller) =>
            {
                Some((*dst, *callee))
            }
            _ => None,
        })
        .collect();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().all(|(dst, _)| *dst == callee));
    let temps: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.fn_id == Some(caller) && v.temp.is_some())
        .collect();
    assert_eq!(
        temps.len(),
        2,
        "one conditional value and one call result: {temps:?}"
    );
}

#[test]
fn conditional_type_inference_does_not_guess_from_one_arm() {
    use trace_ir::{FlowConstraint, TypeDesc};
    let (program, _) = conditional_values();
    let caller = only_function(program, "unknown_type_arm");
    let destinations: Vec<_> = program
        .flow
        .iter()
        .filter_map(|flow| match flow {
            FlowConstraint::AddrOfVar { dst, .. }
                if program.symbols.variable(*dst).fn_id == Some(caller) =>
            {
                Some(*dst)
            }
            _ => None,
        })
        .collect();
    assert_eq!(destinations.len(), 1);
    let ty = program.symbols.variable(destinations[0]).type_id;
    assert_eq!(program.types.get(ty).desc.as_ref(), &TypeDesc::Unknown);
    for (caller, class) in [("auto_stream", "Stream"), ("auto_null", "Session")] {
        let var = local_variable(program, caller, "s");
        let ty = program.symbols.variable(var).type_id;
        assert!(
            matches!(program.types.get(ty).desc.as_ref(), TypeDesc::Ptr(inner)
            if matches!(inner.as_ref(), TypeDesc::Struct {name, ..} if name == class))
        );
    }
}

#[test]
fn conditional_default_member_initializers_keep_their_own_callbacks() {
    let (program, analysis) = conditional_values();
    let caller = only_function(program, "default_callbacks");
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == caller && s.callee_var.is_some())
        .collect();
    assert_eq!(sites.len(), 20);
    for object in ["first", "second"] {
        for index in 0..10 {
            let name = format!("{object}.callbacks.cb{index}");
            let matching: Vec<_> = sites.iter().filter(|s| s.callee_name == name).collect();
            assert_eq!(matching.len(), 1, "{name}: {sites:?}");
            let targets: std::collections::BTreeSet<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == matching[0].id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            assert_eq!(
                targets,
                [
                    format!("member_alpha{index}"),
                    format!("member_beta{index}")
                ]
                .into(),
                "{name}"
            );
        }
    }
}

#[test]
fn conditional_matching_pointer_shapes_preserve_method_calls() {
    use trace_ir::TypeDesc;
    let (program, analysis) = conditional_values();
    for caller in [
        "shape_new",
        "shape_new_reversed",
        "shape_get",
        "shape_get_reversed",
    ] {
        let var = local_variable(program, caller, "p");
        let ty = program.symbols.variable(var).type_id;
        assert!(
            matches!(program.types.get(ty).desc.as_ref(), TypeDesc::Ptr(inner)
            if matches!(inner.as_ref(), TypeDesc::Struct { name, .. } if name == "ConditionalShape")),
            "{caller}"
        );
        let targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| fn_name(program, e.caller) == caller)
            .map(|e| fn_name(program, e.callee))
            .filter(|name| name.ends_with("::method"))
            .collect();
        assert_eq!(targets, ["ConditionalShape::method"], "{caller}");
    }
    for name in ["p", "q"] {
        let var = local_variable(program, "shape_mismatch", name);
        let ty = program.symbols.variable(var).type_id;
        assert_eq!(program.types.get(ty).desc.as_ref(), &TypeDesc::Unknown);
    }
}

#[test]
fn conditional_deep_unknown_types_preserve_callback_targets() {
    // Each selector's function-designator type is unknown to the receiver
    // probe. Re-entering it via argument typing used to double the work at
    // each level, making even a short chain impractical to index.
    let dir = tempfile::tempdir().unwrap();
    let mut expression = "beta".to_string();
    for _ in 0..32 {
        expression = format!("c ? alpha : ({expression})");
    }
    std::fs::write(dir.path().join("main.c"), format!(
        "typedef void (*Callback)(void);\nvoid alpha(void) {{}}\nvoid beta(void) {{}}\nvoid deep(int c) {{ Callback f = {expression}; f(); }}\n"
    )).unwrap();
    let program = trace_parse::build_program(dir.path(), &default_opts(dir.path())).unwrap();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    let (_, analysis) = trace_analysis::analyze(&program);
    let targets: std::collections::BTreeSet<_> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == "deep")
        .map(|e| fn_name(&program, e.callee))
        .collect();
    assert_eq!(targets, ["alpha".into(), "beta".into()].into());
}

#[test]
fn conditional_fixture_exports_match_across_jobs() {
    let root = fixture("conditional_values");
    let reference = analysis_rows(&cli_analyze(&root, &["--jobs", "1", "--full-export"]));
    assert_eq!(
        analysis_rows(&cli_analyze(&root, &["--jobs", "4", "--full-export"])),
        reference,
    );
}

#[test]
fn conditional_shorthand_and_compound_arm_values_preserve_callbacks() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for (caller, count) in [
        ("shorthand_initializer", 1),
        ("shorthand_callee", 1),
        ("shorthand_returned", 1),
        ("shorthand_calls", 1),
        ("assignment_arm_callee", 1),
        ("assignment_arm_calls", 2),
        ("assignment_arm_store", 2),
        ("assignment_arm_field", 2),
        ("assignment_arm_returned", 1),
        ("comma_arms", 1),
        ("comma_call_arms", 1),
        ("comma_assignment_effects", 2),
        ("invoke_ptr", 1),
        ("invoke_ptr_initializer", 1),
        ("invoke_ptr_returned", 1),
        ("invoke_ptr_fields", 1),
        ("invoke_ptr_derefs", 1),
        ("invoke_ptr_nested", 1),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert_eq!(sites.len(), count, "{caller}");
        for site in sites {
            let targets: std::collections::BTreeSet<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            assert_eq!(
                targets,
                ["alpha".into(), "beta".into()].into(),
                "{caller}: {site:?}"
            );
        }
    }
}

#[test]
fn conditional_lvalue_addresses_reach_original_cells() {
    let (program, analysis) = conditional_values();
    let caller = only_function(program, "address_conditional_writes");
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == caller)
        .collect();
    assert_eq!(sites.len(), 2);
    for site in sites {
        let targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.call_site == site.id)
            .map(|e| fn_name(program, e.callee))
            .collect();
        assert_eq!(targets, ["alpha"]);
    }
    for caller in [
        "address_conditional_argument",
        "address_conditional_fields",
        "address_conditional_derefs",
        "address_conditional_nested",
    ] {
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|s| {
                fn_name(program, s.caller) == caller && s.callee_name.starts_with("invoke_ptr")
            })
            .unwrap();
        assert_eq!(site.var_args.len(), 1, "{caller}");
        let actual = site.var_args[0].1;
        let count = program.flow.iter().filter(|flow| matches!(flow,
            trace_ir::FlowConstraint::AddrOfVar {dst, ..} | trace_ir::FlowConstraint::Copy {dst, ..} if *dst == actual)).count();
        assert!(
            count >= 2,
            "{caller}: both lvalue addresses feed the actual"
        );
    }
}

#[test]
fn conditional_compound_arms_keep_one_return_destination_per_call() {
    let (program, _) = conditional_values();
    for caller in ["shorthand_calls", "assignment_arm_calls", "comma_call_arms"] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller)
            .collect();
        for name in ["get_alpha", "get_beta"] {
            let calls: Vec<_> = sites.iter().filter(|s| s.callee_name == name).collect();
            assert_eq!(calls.len(), 1, "{caller}: {sites:?}");
            let dst = calls[0].return_dst.expect("arm's call result is consumed");
            let returns: Vec<_> = program
                .flow
                .iter()
                .filter_map(|flow| match flow {
                    trace_ir::FlowConstraint::CallReturn {
                        dst,
                        callee_name,
                        caller: owner,
                    } if *owner == Some(calls[0].caller) && callee_name == name => Some(*dst),
                    _ => None,
                })
                .collect();
            assert_eq!(returns, [dst], "{caller}: one return destination");
        }
        if caller == "comma_call_arms" {
            let calls: Vec<_> = sites.iter().filter(|s| s.callee_name == "touch").collect();
            assert_eq!(calls.len(), 2);
            assert_ne!(calls[0].span, calls[1].span);
            assert!(calls.iter().all(|s| s.return_dst.is_none()));
        }
    }
    for (caller, expressions) in [
        (
            "assignment_arm_calls",
            ["p = get_alpha()", "p = get_beta()"],
        ),
        ("assignment_arm_store", ["*out = alpha", "*out = beta"]),
        ("assignment_arm_field", ["h.cb = alpha", "h.cb = beta"]),
    ] {
        let owner = only_function(program, caller);
        let written: std::collections::BTreeSet<_> = program
            .flow_origins
            .iter()
            .filter(|(flow, _)| {
                flow.vars()
                    .any(|var| program.symbols.variable(var).fn_id == Some(owner))
            })
            .flat_map(|(_, origins)| origins.iter().map(|(_, text)| text.as_ref()))
            .collect();
        for expression in expressions {
            assert!(written.contains(expression), "{caller}: {written:?}");
        }
    }
}

#[test]
fn conditional_lvalue_roots_match_named_pointer_controls() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for (caller, expected) in [
        ("conditional_field_root", vec!["alpha", "beta"]),
        ("conditional_wrapper_field_root", vec!["alpha", "beta"]),
        ("conditional_field_control", vec!["alpha", "beta"]),
        ("compound_field_root", vec!["alpha", "beta"]),
        ("conditional_object_deref", vec!["alpha", "beta"]),
        ("conditional_field_call_roots", vec!["alpha", "beta"]),
        ("conditional_deref_read", vec!["alpha", "beta"]),
        ("conditional_deref_control", vec!["alpha", "beta"]),
        ("consume_compound_deref", vec!["alpha", "beta"]),
        ("conditional_function_address", vec!["alpha", "beta"]),
        ("conditional_array_address", vec!["alpha", "beta"]),
        ("conditional_deref_store", vec!["alpha"]),
        ("consume_comma_address", vec!["alpha"]),
        ("consume_assignment_address", vec!["alpha"]),
        ("consume_standalone_comma", vec!["alpha"]),
        ("standalone_comma_callee", vec!["alpha"]),
        ("conditional_lvalue_assignment", vec!["alpha"]),
        ("standalone_conditional_lvalue_assignment", vec!["beta"]),
        ("conditional_field_assignment", vec!["alpha"]),
        ("conditional_field_root_store", vec!["alpha"]),
        ("comma_lvalue_assignment", vec!["alpha"]),
        ("consume_reference_address", vec!["alpha", "beta"]),
        ("consume_forward_reference", vec!["alpha"]),
        ("consume_reference_member", vec!["alpha", "beta"]),
        ("write_reference_address", vec!["alpha"]),
        ("consume_indirect_reference", vec!["alpha", "beta"]),
        ("consume_remote_reference", vec!["alpha", "beta"]),
        ("ordinary_reference_value", vec!["alpha"]),
        ("consume_dereferenced_reference", vec!["alpha"]),
        ("consume_forward_member", vec!["alpha"]),
        ("forwarded_member_reference_address", vec!["alpha"]),
        ("consume_virtual_reference", vec!["alpha", "beta"]),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}: {site:?}");
        }
    }
}

#[test]
fn conditional_and_standalone_comma_values_keep_operand_provenance() {
    use trace_ir::FlowConstraint;
    let (program, _) = conditional_values();
    for caller in ["standalone_comma_argument", "standalone_comma_callee"] {
        let owner = only_function(program, caller);
        let values: Vec<_> = program.flow.iter().filter(|flow| matches!(flow,
            FlowConstraint::AddrOfFn { dst, callee } if program.symbols.variable(*dst).fn_id == Some(owner)
                && fn_name(program, *callee) == "alpha")).collect();
        assert_eq!(values.len(), 1, "{caller}");
        let origins = program
            .flow_origins
            .get(values[0])
            .expect("right operand provenance");
        assert!(
            origins.iter().any(|(_, text)| text.as_ref() == "alpha"),
            "{caller}: {origins:?}"
        );
    }
    let pointer = local_variable(program, "conditional_array_address", "p");
    let array = local_variable(program, "conditional_array_address", "a");
    let addresses: Vec<_> = program
        .flow
        .iter()
        .filter_map(|flow| match flow {
            FlowConstraint::AddrOfVar { dst, src } if *dst == pointer => Some(*src),
            _ => None,
        })
        .collect();
    assert_eq!(
        addresses,
        [array, array],
        "array element addresses retain the ordinary base-array summary"
    );
    let owner = only_function(program, "conditional_field_call_roots");
    for name in ["get_root_alpha", "get_root_beta"] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| s.caller == owner && s.callee_name == name)
            .collect();
        assert_eq!(sites.len(), 1);
        let dst = sites[0].return_dst.expect("root call return");
        let returns: Vec<_> = program
            .flow
            .iter()
            .filter_map(|flow| match flow {
                FlowConstraint::CallReturn {
                    dst,
                    callee_name,
                    caller,
                } if *caller == Some(owner) && callee_name == name => Some(*dst),
                _ => None,
            })
            .collect();
        assert_eq!(returns, [dst]);
    }
}

#[test]
fn conditional_reference_call_addresses_retain_return_wiring() {
    use trace_ir::ReturnFlow;
    let (program, analysis) = conditional_values();
    let source = only_variable(program, "reference_a");
    let getter = only_function(program, "get_reference_a");
    assert!(program.fn_returns[&getter].iter().any(|flow| matches!(flow,
        ReturnFlow::LvalueAddress(inner) if matches!(inner.as_ref(), ReturnFlow::AddrOfVar {src} if *src == source))));
    let owner = only_function(program, "conditional_reference_call_address");
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == owner && s.callee_name.starts_with("get_reference_"))
        .collect();
    assert_eq!(sites.len(), 2);
    for site in &sites {
        assert_eq!(site.return_dst, site.address_return_dst());
        assert!(site.return_dst.is_some());
    }
    let pag = trace_analysis::Pag::build(program);
    let db = export_program(program, &pag, analysis);
    let conn = Connection::open(db.path()).unwrap();
    let count: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT s.id) FROM flow_return_calls r JOIN call_sites s ON s.id=r.call_site_id
         JOIN functions c ON c.id=s.caller_fn_id WHERE c.name='conditional_reference_call_address'",
        [], |row| row.get(0)).unwrap();
    assert_eq!(
        count, 2,
        "both address-returning call arms retain return provenance"
    );
}

#[test]
fn conditional_compound_assignments_subscripts_and_deduced_references() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for (caller, expected) in [
        ("consume_advanced", vec!["alpha", "beta"]),
        ("consume_advanced_address", vec!["alpha", "beta"]),
        ("consume_selected_element", vec!["alpha", "beta"]),
        ("selected_element_read", vec!["alpha", "beta"]),
        ("selected_element_callee", vec!["alpha", "beta"]),
        ("selected_element_store", vec!["alpha"]),
        ("selected_pointer_element", vec!["alpha", "beta"]),
        ("consume_comma_element", vec!["alpha"]),
        ("selected_array_lengths", vec!["alpha", "beta"]),
        ("returned_selected_element", vec!["alpha", "beta"]),
        ("consume_plain_element", vec!["alpha"]),
        ("consume_deduced_reference", vec!["alpha", "beta"]),
        ("consume_deduced_selector", vec!["alpha", "beta"]),
        ("consume_deduced_deref", vec!["alpha"]),
        ("ordinary_deduced_value", vec!["alpha"]),
        ("write_deduced_reference", vec!["alpha"]),
        ("selected_element_call_arms", vec!["alpha", "beta"]),
        ("selected_pointer_assignment", vec!["alpha"]),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}: {site:?}");
        }
    }
    for name in [
        "deduced_reference_a",
        "deduced_reference_b",
        "deduced_reference_selector",
        "deduced_reference_deref",
    ] {
        assert!(
            program.fn_returns[&only_function(program, name)]
                .iter()
                .any(|r| matches!(r, trace_ir::ReturnFlow::LvalueAddress(_))),
            "{name}"
        );
    }
    for name in ["deduced_value", "deduced_scalar"] {
        assert!(
            !program
                .fn_returns
                .get(&only_function(program, name))
                .into_iter()
                .flatten()
                .any(|r| matches!(r, trace_ir::ReturnFlow::LvalueAddress(_))),
            "{name}"
        );
    }
    let owner = only_function(program, "selected_element_call_arms");
    let mut returns = Vec::new();
    for name in [
        "element_condition",
        "element_alpha",
        "element_beta",
        "element_index",
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| s.caller == owner && s.callee_name == name)
            .collect();
        assert_eq!(sites.len(), 1, "{name}: {sites:?}");
        if name == "element_alpha" || name == "element_beta" {
            returns.push(sites[0].return_dst.expect("subscript arm's pointer result"));
        } else {
            assert!(sites[0].return_dst.is_none(), "{name}");
        }
    }
    assert_eq!(
        returns[0], returns[1],
        "both pointer arms feed the selected element address"
    );
}

#[test]
fn conditional_base_pointer_receivers_match_explicit_base_controls() {
    let (program, analysis) = conditional_values();
    for caller in [
        "conditional_base_receiver",
        "conditional_base_reversed",
        "conditional_base_control",
        "conditional_base_transitive",
        "conditional_base_cast",
        "conditional_base_c_cast",
    ] {
        let mut targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| {
                fn_name(program, e.caller) == caller
                    && fn_name(program, e.callee).ends_with("::run")
            })
            .map(|e| fn_name(program, e.callee))
            .collect();
        targets.sort();
        targets.dedup();
        assert_eq!(
            targets,
            ["DispatchBase::run", "DispatchDerived::run"],
            "{caller}"
        );
    }
}

#[test]
fn conditional_array_cells_keep_known_signatures_and_storage_separate() {
    use trace_ir::TypeDesc;
    let mut program = conditional_values().0.clone();
    // Source function-pointer aliases currently omit parameter descriptors.
    // Supply a known IR signature to verify the existing cell guard is kept.
    let array = local_variable(&program, "array_address_signature_guard", "a");
    let signature = TypeDesc::FnPtr {
        ret: Box::new(TypeDesc::Void),
        params: vec![TypeDesc::Int],
    };
    let array_type = program.types.intern(TypeDesc::Array {
        elem: Box::new(signature),
        size: Some(1),
    });
    program.symbols.variable_mut(array).type_id = array_type;
    let (pag, analysis) = trace_analysis::analyze_with_options(
        &program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    let caller = only_function(&program, "array_address_signature_guard");
    let targets: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller)
        .map(|e| fn_name(&program, e.callee))
        .collect();
    assert_eq!(targets, ["unary_target"]);
    let result = local_variable(&program, "returned_selected_element", "f");
    let values = &analysis.points_to[&pag.var_node[&result]];
    assert_eq!(values.len(), 2);
    assert!(
        values
            .iter()
            .all(|loc| pag.locations[loc.0 as usize].kind == trace_analysis::LocKind::Function),
        "array storage addresses must not become callback values"
    );
}

#[test]
fn conditional_pointer_subscript_addresses_reach_selected_cells() {
    let (program, analysis) = conditional_values();
    for (caller, expected) in [
        ("consume_pointer_subscript_arms", vec!["alpha", "beta"]),
        ("consume_pointer_subscript_control", vec!["alpha", "beta"]),
        ("consume_plain_pointer_subscript", vec!["alpha"]),
        ("pointer_subscript_arm_store", vec!["alpha"]),
        ("consume_returned_pointer_subscript", vec!["alpha", "beta"]),
        ("consume_pointer_subscript_calls", vec!["alpha", "beta"]),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}: {site:?}");
        }
    }
    let owner = only_function(program, "pointer_subscript_call_arms");
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == owner)
        .collect();
    let returns: Vec<_> = ["element_alpha", "element_beta"]
        .iter()
        .map(|name| {
            let calls: Vec<_> = sites.iter().filter(|s| s.callee_name == *name).collect();
            assert_eq!(calls.len(), 1, "{name}");
            calls[0]
                .return_dst
                .expect("subscript table's pointer result")
        })
        .collect();
    assert_eq!(returns[0], returns[1]);
    let indices: Vec<_> = sites
        .iter()
        .filter(|s| s.callee_name == "element_index")
        .collect();
    assert_eq!(indices.len(), 2);
    assert_ne!(indices[0].span, indices[1].span);
    assert!(indices.iter().all(|s| s.return_dst.is_none()));
}

#[test]
fn conditional_reference_cast_addresses_preserve_referents_and_origins() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for (caller, expected) in [
        ("consume_reference_cast", vec!["alpha", "beta"]),
        ("consume_c_reference_cast", vec!["alpha", "beta"]),
        ("consume_alias_reference_cast", vec!["alpha", "beta"]),
        ("consume_nested_reference_cast", vec!["alpha", "beta"]),
        ("reference_cast_store", vec!["alpha"]),
        ("consume_reference_cast_return", vec!["alpha"]),
        ("consume_deduced_reference_cast", vec!["alpha"]),
        ("reference_cast_return_store", vec!["beta"]),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}: {site:?}");
        }
    }
    let source = local_variable(program, "reference_cast_arm", "a");
    let address = program
        .flow
        .iter()
        .find(|flow| {
            matches!(flow,
        trace_ir::FlowConstraint::AddrOfVar {src,..} if *src==source)
        })
        .expect("original referent address");
    assert!(program.flow_origins[address]
        .iter()
        .any(|(_, text)| text.as_ref() == "static_cast<Callback&>(a)"));
    let getter = only_function(program, "reference_cast_getter");
    let source = only_variable(program, "cast_reference_cell");
    assert!(program.fn_returns[&getter].iter().any(|flow|matches!(flow,
        trace_ir::ReturnFlow::LvalueAddress(inner) if matches!(inner.as_ref(),trace_ir::ReturnFlow::AddrOfVar {src} if *src==source))));
    let value_getter = only_function(program, "reference_cast_value_control");
    assert!(!program.fn_returns[&value_getter]
        .iter()
        .any(|flow| matches!(flow, trace_ir::ReturnFlow::LvalueAddress(_))));
}

#[test]
fn conditional_function_and_functional_reference_casts_preserve_targets() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for (caller, expected) in [
        ("explicit_function_reference_cast", vec!["alpha", "beta"]),
        ("aliased_function_reference_cast", vec!["alpha", "beta"]),
        ("consume_functional_reference_cast", vec!["alpha", "beta"]),
        ("consume_qualified_reference_cast", vec!["alpha", "beta"]),
        ("functional_reference_cast_store", vec!["alpha"]),
        ("consume_functional_reference_return", vec!["alpha"]),
        ("consume_functional_cast_call", vec!["alpha"]),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}");
        }
    }
    for caller in [
        "functional_reference_cast_arm",
        "qualified_reference_cast_arm",
        "functional_reference_cast_store",
        "functional_reference_getter",
        "functional_cast_call",
    ] {
        assert!(
            program
                .symbols
                .call_sites
                .iter()
                .filter(|s| fn_name(program, s.caller) == caller)
                .all(|s| !["CallbackReference", "ReferenceCastNames::R"]
                    .contains(&s.callee_name.as_str())),
            "{caller}"
        );
    }
    assert!(has_any_edge(
        program,
        analysis,
        "functional_cast_shadowed_variable",
        "reference_identity"
    ));
    assert!(has_any_edge(
        program,
        analysis,
        "ReferenceCastNames::Inner::real_call",
        "ReferenceCastNames::Inner::R"
    ));
    let calls: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| {
            fn_name(program, s.caller) == "functional_cast_call"
                && s.callee_name == "reference_cast_getter"
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].return_dst.is_some());
}

#[test]
fn conditional_cast_values_function_aliases_and_file_reference_returns() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for (caller, expected) in [
        ("consume_functional_argument", vec!["alpha", "beta"]),
        (
            "read_parenthesized_functional_return",
            vec!["alpha", "beta"],
        ),
        ("parenthesized_functional_initializer", vec!["alpha"]),
        ("functional_reference_callee", vec!["alpha"]),
        ("complete_function_alias", vec!["alpha", "beta"]),
        ("typedef_function_alias", vec!["alpha", "beta"]),
        ("functional_complete_function_alias", vec!["alpha", "beta"]),
        ("returned_function_selector", vec!["alpha", "beta"]),
        ("returned_nested_function_selector", vec!["alpha", "beta"]),
        ("file_reference_use", vec!["alpha", "beta"]),
        ("file_simple_reference_use", vec!["alpha"]),
        ("file_remote_reference_use", vec!["alpha"]),
        ("file_reference_store", vec!["beta"]),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}");
        }
    }
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "CompleteFunctionReference"));
    let getter = only_function(program, "function_selector");
    assert!(program.fn_returns[&getter]
        .iter()
        .any(|r| matches!(r, trace_ir::ReturnFlow::LvalueAddress(_))));
    let pag = trace_analysis::Pag::build(program);
    let db = export_program(program, &pag, analysis);
    let conn = Connection::open(db.path()).unwrap();
    let count:i64=conn.query_row("SELECT COUNT(*) FROM call_edges e JOIN functions f ON f.id=e.caller_fn_id WHERE f.name='file_reference_use'",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 2);
    let span = program
        .symbols
        .variable(only_variable(program, "file_selected_reference"))
        .span;
    let return_origins: i64 = conn.query_row(
        "SELECT COUNT(*) FROM flow_origins WHERE file_id=?1 AND line=?2 AND operation='return value'",
        rusqlite::params![span.file.0, span.line], |row| row.get(0)).unwrap();
    assert_eq!(
        return_origins, 2,
        "file-scope address-return wiring keeps both arm origins"
    );
    assert!(program
        .symbols
        .call_sites
        .iter()
        .all(|s| s.span.file != span.file || s.span.line != span.line));
}

#[test]
fn conditional_array_initializers_pointer_references_and_typed_cast_tables() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for (caller, expected) in [
        ("conditional_callback_array_init", vec!["alpha", "beta"]),
        (
            "conditional_callback_array_assignment_control",
            vec!["alpha", "beta"],
        ),
        (
            "conditional_callback_array_call_init",
            vec!["alpha", "beta"],
        ),
        (
            "conditional_callback_array_nested_init",
            vec!["alpha", "beta"],
        ),
        ("conditional_callback_array_null_init", vec!["alpha"]),
        ("consume_pointer_reference_cast", vec!["alpha", "beta"]),
        ("consume_pointer_reference_alias", vec!["alpha", "beta"]),
        ("consume_typedef_pointer_reference", vec!["alpha", "beta"]),
        ("pointer_reference_cast_store", vec!["alpha"]),
        ("functional_array_reference_read", vec!["alpha", "beta"]),
        ("functional_array_reference_callee", vec!["alpha", "beta"]),
        ("functional_array_reference_store", vec!["alpha"]),
        ("named_array_reference_read", vec!["alpha", "beta"]),
        ("consume_array_reference_address", vec!["alpha", "beta"]),
    ] {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}");
        }
    }
    for caller in [
        "functional_reference_receiver_arms",
        "functional_reference_receiver_selector",
        "functional_reference_receiver_control",
    ] {
        assert!(
            has_any_edge(program, analysis, caller, "FunctionalReceiver::run"),
            "{caller}"
        );
    }
    let table = local_variable(program, "conditional_callback_array_call_init", "table");
    assert!(matches!(
        program
            .types
            .get(program.symbols.variable(table).type_id)
            .desc
            .as_ref(),
        trace_ir::TypeDesc::Array { .. }
    ));
    let mut destinations = Vec::new();
    for name in ["array_init_alpha", "array_init_beta"] {
        let calls: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| {
                fn_name(program, s.caller) == "conditional_callback_array_call_init"
                    && s.callee_name == name
            })
            .collect();
        assert_eq!(calls.len(), 1);
        let dst = calls[0].return_dst.expect("initializer arm return value");
        assert_ne!(dst, table);
        destinations.push(dst);
    }
    assert_eq!(destinations[0], destinations[1]);
    for name in [
        "PointerToReferenceGetter",
        "TypedefPointerToReferenceGetter",
    ] {
        assert!(!program.types.alias_is_reference(name), "{name}");
    }
    for target in ["reference_cast_getter", "alpha"] {
        assert!(has_any_edge(
            program,
            analysis,
            "pointer_to_reference_return_control",
            target
        ));
    }
    for alias in [
        "CallbackPointerReference",
        "TypedefCallbackPointerReference",
        "CallbackArrayReference",
        "FunctionalReceiverReference",
    ] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == alias),
            "{alias}"
        );
    }
}

fn assert_conditional_callbacks(cases: &[(&str, &[&str])]) {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for &(caller, expected) in cases {
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| fn_name(program, s.caller) == caller && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty(), "{caller}");
        for site in sites {
            let mut targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.call_site == site.id)
                .map(|e| fn_name(program, e.callee))
                .collect();
            targets.sort();
            assert_eq!(targets, expected, "{caller}: {}", site.callee_name);
        }
    }
}

#[test]
fn conditional_reference_cast_lookup_stores_and_cstyle_addresses() {
    assert_conditional_callbacks(&[
        (
            "BoundaryTypes::HiddenReference::HiddenReference",
            &["alpha", "beta"],
        ),
        ("boundary_alias_control", &["alpha"]),
        ("boundary_functional_store", &["alpha", "beta"]),
        ("boundary_named_store", &["alpha", "beta"]),
        ("boundary_assignment_arm_store", &["alpha", "beta"]),
        ("boundary_cstyle_consume", &["alpha", "beta"]),
    ]);
    let (program, analysis) = conditional_values();
    assert!(has_any_edge(
        program,
        analysis,
        "BoundaryTypes::constructor",
        "BoundaryTypes::HiddenReference::HiddenReference"
    ));
    assert!(has_any_edge(
        program,
        analysis,
        "BoundaryTypes::receiver",
        "BoundaryTypes::HiddenReference::run"
    ));
    assert!(has_any_edge(
        program,
        analysis,
        "BoundaryTypes::receiver",
        "BoundaryTypes::HiddenReference::HiddenReference"
    ));
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "HiddenReference" || f.name == "BoundaryTypes::HiddenReference"));
}

#[test]
fn conditional_complete_reference_return_declarators() {
    assert_conditional_callbacks(&[
        ("boundary_pointer_return_consume", &["alpha"]),
        ("boundary_deref_pointer_value", &["alpha"]),
        ("boundary_deref_pointer_address_consume", &["alpha"]),
        ("boundary_trailing_return", &["alpha"]),
        ("boundary_alias_return_control", &["alpha"]),
        ("boundary_trailing_pointer_consume", &["alpha"]),
    ]);
    let (program, _) = conditional_values();
    for name in [
        "boundary_pointer_get",
        "boundary_deref_pointer_get",
        "boundary_trailing_get",
        "boundary_alias_get",
        "boundary_trailing_pointer_get",
    ] {
        let getter = only_function(program, name);
        assert!(
            program.fn_returns[&getter]
                .iter()
                .any(|r| matches!(r, trace_ir::ReturnFlow::LvalueAddress(_))),
            "{name}"
        );
    }
}

#[test]
fn conditional_cast_array_elements_struct_members_and_file_effects() {
    assert_conditional_callbacks(&[
        ("boundary_cast_array_init", &["alpha", "beta"]),
        ("boundary_cstyle_array_init", &["alpha", "beta"]),
        ("boundary_single_struct_array", &["alpha", "beta"]),
        ("boundary_struct_array_init", &["alpha", "beta"]),
        ("boundary_struct_array_other", &["unrelated"]),
        ("boundary_struct_array_control", &["alpha", "beta"]),
        ("boundary_struct_array_elided", &["alpha", "beta"]),
        ("boundary_struct_array_nested", &["alpha", "beta"]),
        ("boundary_global_struct_array", &["alpha", "beta"]),
        ("boundary_global_arm_effect", &["alpha", "unrelated"]),
        ("boundary_global_arm_result", &["beta"]),
        ("boundary_global_comma_effect", &["alpha", "unrelated"]),
        ("boundary_global_comma_result", &["beta"]),
        ("boundary_global_condition_effect", &["alpha", "unrelated"]),
        ("boundary_global_condition_result", &["beta"]),
        ("boundary_global_shorthand_effect", &["alpha", "unrelated"]),
        ("boundary_global_shorthand_result", &["alpha", "beta"]),
        ("boundary_global_unevaluated_effect", &["unrelated"]),
        ("boundary_global_noexcept_effect", &["unrelated"]),
    ]);
    let (program, _) = conditional_values();
    for name in [
        "boundary_global_arm_cell",
        "boundary_global_comma_cell",
        "boundary_global_condition_cell",
        "boundary_global_shorthand_cell",
    ] {
        let var = only_variable(program, name);
        let writes: Vec<_> = program
            .flow
            .iter()
            .filter(
                |flow| matches!(flow, trace_ir::FlowConstraint::Copy { dst, .. } if *dst == var),
            )
            .collect();
        assert_eq!(writes.len(), 1, "{name}: evaluate the assignment once");
        assert!(
            program.flow_origins[writes[0]]
                .iter()
                .any(|(_, expr)| { expr.contains(&format!("{name} = alpha")) }),
            "{name}: retain assignment provenance"
        );
    }
}

#[test]
fn conditional_prototype_aliases_cast_tables_and_aggregate_elements() {
    assert_conditional_callbacks(&[
        ("conversion_prototype_alias", &["alpha"]),
        ("conversion_prototype_control", &["alpha"]),
        ("conversion_pointer_cast_read", &["alpha", "beta"]),
        ("conversion_pointer_cast_callee", &["alpha", "beta"]),
        ("conversion_pointer_cast_consume", &["alpha", "beta"]),
        ("conversion_pointer_cast_store", &["alpha"]),
        ("conversion_aggregate_element", &["alpha", "beta"]),
        ("conversion_aggregate_control", &["alpha", "beta"]),
        ("conversion_aggregate_precise", &["alpha", "beta"]),
        ("conversion_designated_array", &["alpha", "beta"]),
        ("conversion_designated_other", &["unrelated"]),
        ("conversion_rvalue_function_reference", &["alpha", "beta"]),
        ("conversion_rvalue_function_alias", &["alpha", "beta"]),
    ]);
    let (program, analysis) = conditional_values();
    for name in [
        "conversion_alias_wrap",
        "conversion_explicit_wrap",
        "conversion_base_get",
    ] {
        let id = only_function(program, name);
        assert!(
            program.fn_returns[&id]
                .iter()
                .any(|r| matches!(r, trace_ir::ReturnFlow::LvalueAddress(_))),
            "{name}"
        );
    }
    let id = only_function(program, "conversion_object_xvalue");
    assert!(program.fn_returns[&id]
        .iter()
        .all(|r| !matches!(r, trace_ir::ReturnFlow::LvalueAddress(_))));
    for target in ["alpha", "beta"] {
        assert!(
            must_not_have_edge(program, analysis, "conversion_object_cast_table", target),
            "a class conversion must not expose its input as a built-in table"
        );
    }
}

#[test]
fn conditional_array_decay_and_base_object_receiver_types() {
    let (program, analysis) = conditional_values();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    for caller in [
        "conversion_null_array",
        "conversion_null_array_auto",
        "conversion_null_array_reverse",
        "conversion_null_array_control",
    ] {
        assert!(
            has_any_edge(program, analysis, caller, "ConversionRun::run"),
            "{caller}"
        );
    }
    for caller in ["conversion_base_lvalue", "conversion_base_lvalue_control"] {
        assert!(
            has_any_edge(program, analysis, caller, "ConversionBase::run"),
            "{caller}"
        );
    }
    let p = local_variable(program, "conversion_null_array_auto", "p");
    assert!(
        matches!(program.types.get(program.symbols.variable(p).type_id).desc.as_ref(),
        trace_ir::TypeDesc::Ptr(inner) if matches!(inner.as_ref(),trace_ir::TypeDesc::Struct{name,..} if name=="ConversionRun"))
    );
}

#[test]
fn conditional_file_effects_cover_addresses_indices_and_lambda_captures() {
    assert_conditional_callbacks(&[
        ("conversion_address_effect", &["alpha", "unrelated"]),
        ("conversion_address_result", &["beta"]),
        (
            "conversion_address_condition_effect",
            &["alpha", "unrelated"],
        ),
        ("conversion_address_condition_result", &["beta"]),
        ("conversion_decay_effect", &["alpha", "unrelated"]),
        ("conversion_decay_result", &["beta"]),
        ("conversion_index_effect", &["alpha", "unrelated"]),
        ("conversion_index_result", &["beta"]),
        ("conversion_named_index_effect", &["alpha", "unrelated"]),
        ("conversion_lambda_effect", &["alpha", "beta"]),
        ("conversion_lambda_body_control", &["beta"]),
        ("conversion_nested_capture_effect", &["alpha", "beta"]),
    ]);
    let (program, _) = conditional_values();
    for name in [
        "conversion_address_cell",
        "conversion_condition_cell",
        "conversion_decay_cell",
        "conversion_index_cell",
        "conversion_named_index_cell",
        "conversion_lambda_cell",
        "conversion_nested_capture_cell",
    ] {
        let var = only_variable(program, name);
        let writes: Vec<_> = program
            .flow
            .iter()
            .filter(
                |flow| matches!(flow, trace_ir::FlowConstraint::Copy { dst, .. } if *dst == var),
            )
            .collect();
        assert_eq!(writes.len(), 1, "{name}: preserve exactly one assignment");
        assert!(
            program.flow_origins[writes[0]]
                .iter()
                .any(|(_, expr)| { expr.contains(&format!("{name} = alpha")) }),
            "{name}: assignment provenance"
        );
    }
}

#[test]
fn conditional_nested_common_class_probes_do_not_repeat_category_probes() {
    let mut selected = "b".to_owned();
    for _ in 0..64 {
        selected = format!("(c ? d : {selected})");
    }
    let source = format!("struct B {{ void run() {{}} }}; struct D:B {{}}; void caller(bool c) {{ D d; B b; {selected}.run(); }}");
    let (program, analysis) = analyze_source(&[("main.cpp", &source)]);
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    assert!(has_any_edge(&program, &analysis, "caller", "B::run"));
}

#[test]
fn conditional_ranges_receivers_and_pointer_offsets_reach_callbacks() {
    assert_conditional_callbacks(&[
        ("consumer_range", &["alpha", "beta"]),
        ("consumer_range_auto", &["alpha", "beta"]),
        ("consumer_range_reference", &["alpha", "beta"]),
        ("consumer_range_comma", &["alpha"]),
        ("consumer_range_control", &["alpha"]),
        ("consumer_mixed_field", &["alpha", "beta"]),
        ("consumer_offset_invoke", &["alpha"]),
        ("consumer_offset_reverse_invoke", &["alpha"]),
        ("consumer_offset_control_invoke", &["alpha"]),
    ]);
    let (program, analysis) = conditional_values();
    for caller in [
        "consumer_callable",
        "consumer_callable_refs",
        "consumer_callable_comma",
        "consumer_callable_assignment",
        "consumer_callable_control",
    ] {
        assert!(
            has_any_edge(program, analysis, caller, "ConsumerFun::operator()"),
            "{caller}"
        );
    }
    for caller in [
        "consumer_comma_receiver",
        "consumer_assignment_receiver",
        "consumer_comma_object",
        "consumer_assignment_object",
        "consumer_mixed_array",
        "consumer_mixed_array_reverse",
    ] {
        assert!(
            has_any_edge(program, analysis, caller, "ConsumerRun::run"),
            "{caller}"
        );
    }
}

#[test]
fn conditional_file_member_reference_getters_keep_overloads_and_provenance() {
    assert_conditional_callbacks(&[
        ("consumer_member_call", &["alpha"]),
        ("consumer_member_selected_call", &["alpha", "beta"]),
        ("consumer_member_typed_call", &["alpha"]),
        ("consumer_member_free_control", &["alpha"]),
        ("consumer_remote_call", &["alpha"]),
        ("consumer_remote_typed_call", &["alpha"]),
    ]);
    let (program, analysis) = conditional_values();
    let pag = trace_analysis::Pag::build(program);
    let db = export_program(program, &pag, analysis);
    let c = Connection::open(db.path()).unwrap();
    let span = program
        .symbols
        .variable(only_variable(program, "consumer_member_address"))
        .span;
    let count: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM flow_origins
             WHERE file_id=?1 AND line=?2 AND operation='return value'",
            rusqlite::params![span.file.0, span.line],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        count > 0,
        "file-scope member address return has operation provenance"
    );
    assert!(program
        .symbols
        .call_sites
        .iter()
        .all(|s| s.span.file != span.file || s.span.line != span.line));
}

#[test]
fn conditional_aggregate_layouts_keep_field_targets() {
    assert_conditional_callbacks(&[
        ("aggregate_anonymous", &["alpha", "beta"]),
        ("aggregate_anonymous_alias", &["alpha", "beta"]),
        ("aggregate_union", &["alpha", "beta"]),
        ("aggregate_union_designated", &["alpha", "beta"]),
        ("aggregate_void_pointer", &["alpha"]),
        ("aggregate_const_void_pointer", &["alpha"]),
        ("aggregate_multidimensional", &["alpha", "beta"]),
        ("aggregate_designated_array_field", &["alpha", "beta"]),
        ("aggregate_nested_designated", &["alpha", "beta"]),
        ("aggregate_constant_table_copy", &["alpha", "beta"]),
        ("aggregate_constant_tables_separate", &["alpha"]),
        ("aggregate_implicit_conversion", &["alpha", "beta"]),
        ("aggregate_braced_element", &["alpha", "beta"]),
        ("c_aggregate_positional", &["alpha", "beta"]),
        ("c_aggregate_designated", &["alpha", "beta"]),
        ("c_aggregate_mixed", &["alpha", "beta"]),
        ("c_aggregate_mixed_later", &["beta", "unrelated"]),
        ("c_aggregate_index_designated", &["alpha", "beta"]),
        ("c_aggregate_index_continuation", &["alpha", "beta"]),
        ("c_aggregate_index_continuation_other", &["unrelated"]),
        ("c_aggregate_anonymous", &["alpha", "beta"]),
        ("c_aggregate_enum_prefix", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_alias_casts_and_array_arguments_keep_values() {
    assert_conditional_callbacks(&[
        ("aggregate_pointer_alias", &["alpha", "beta"]),
        ("aggregate_cast_alias", &["alpha", "beta"]),
        ("aggregate_cast_callee", &["alpha", "beta"]),
        ("aggregate_cast_control", &["alpha"]),
        ("c_aggregate_cast", &["alpha", "beta"]),
        ("aggregate_multidimensional_use", &["alpha", "beta"]),
        ("aggregate_ref_indirect", &["alpha"]),
        ("aggregate_value_indirect", &["alpha"]),
        (
            "AggregateCastShadow::AggregatePointer::AggregatePointer",
            &["alpha", "beta"],
        ),
    ]);
    let (program, _) = conditional_values();
    let wrapper = only_function(program, "aggregate_value_wrap");
    assert!(program.fn_returns[&wrapper]
        .iter()
        .all(|flow| !matches!(flow, trace_ir::ReturnFlow::LvalueAddress(_))));
    assert!(program.symbols.call_sites.iter().all(|site| !matches!(
        site.callee_name.as_ref(),
        "AggregatePointer" | "AggregateCallback" | "CAggregateCallback"
    )));
}

#[test]
fn conditional_file_effects_skip_generic_controls_and_include_construction() {
    assert_conditional_callbacks(&[
        ("c_aggregate_generic_call", &["beta"]),
        ("aggregate_ctor_effect_call", &["alpha", "beta"]),
        ("aggregate_default_effect_call", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_array_decay_retains_dlsym_strings() {
    assert_conditional_callbacks(&[
        ("aggregate_string_null", &["aggregate_string_beta"]),
        (
            "aggregate_string_choice",
            &["aggregate_string_alpha", "aggregate_string_beta"],
        ),
    ]);
    let (program, analysis) = conditional_values();
    assert!(has_any_edge(
        program,
        analysis,
        "aggregate_string_null",
        "aggregate_string_beta"
    ));
    for target in ["aggregate_string_alpha", "aggregate_string_beta"] {
        assert!(
            has_any_edge(program, analysis, "aggregate_string_choice", target),
            "{target}"
        );
    }
}

#[test]
fn conditional_file_effect_walk_bounds_deep_initializers() {
    let source = format!("int value = {};", vec!["1"; 40_000].join(" + "));
    let (program, _) = analyze_source(&[("deep.c", &source)]);
    assert!(program
        .diagnostics
        .iter()
        .any(|d| d.message.contains("AST walk depth exceeded")));
}

#[test]
fn conditional_indirect_reference_aliases_survive_shared_headers() {
    let header =
        "using F = void(*)(); using Getter = F&(*)(); F &get(); decltype(auto) wrap(Getter g);";
    let a = "#include \"shared.h\"\nvoid alpha() {} F cell=alpha; F &get() { return cell; }";
    let b = "#include \"shared.h\"\ndecltype(auto) wrap(Getter g) { return g(); } void caller() { F *p=&wrap(get); (*p)(); }";
    let (program, analysis) = analyze_source(&[("shared.h", header), ("a.cpp", a), ("b.cpp", b)]);
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    assert!(has_any_edge(&program, &analysis, "caller", "alpha"));
}

#[test]
fn conditional_nested_member_arrays_walk_the_type_path_once() {
    let mut source =
        String::from("using F=void(*)(); void alpha() {} void beta() {} struct L0 { F f; };\n");
    for level in 1..=32 {
        source.push_str(&format!("struct L{level} {{ L{} a[1]; }};\n", level - 1));
    }
    let path = format!("root.{}f", "a[0].".repeat(32));
    source.push_str(&format!(
        "void caller(bool c) {{ L32 root; {path}=c?alpha:beta; {path}(); }}"
    ));
    let (program, analysis) = analyze_source(&[("main.cpp", &source)]);
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    assert!(has_any_edge(&program, &analysis, "caller", "alpha"));
    assert!(has_any_edge(&program, &analysis, "caller", "beta"));
}

#[test]
fn conditional_designated_cursors_preserve_elements_and_defaults() {
    assert_conditional_callbacks(&[
        (
            "nine_positional_before_designator",
            &["alpha", "beta", "nine_delta", "nine_gamma"],
        ),
        ("nine_designated_defaults", &["beta"]),
        ("nine_nested_defaults", &["beta"]),
        ("nine_designated_override", &["alpha"]),
        ("nine_partial_global", &["alpha", "beta"]),
        ("nine_partial_designated_global", &["alpha", "beta"]),
        ("nine_partial_override_global", &["beta"]),
    ]);
}

#[test]
fn conditional_indexed_pointers_and_nested_offsets_retain_callbacks() {
    assert_conditional_callbacks(&[
        ("nine_pointer_index", &["alpha"]),
        ("nine_pointer_array_index", &["alpha"]),
        ("nine_nested_offset", &["alpha"]),
    ]);
    let (program, analysis) = conditional_values();
    assert!(has_any_edge(
        program,
        analysis,
        "nine_deref_callable",
        "NineCallable::operator()"
    ));
}

#[test]
fn conditional_callable_categories_and_global_getter_arguments_survive() {
    assert_conditional_callbacks(&[
        ("nine_selected_getters", &["alpha", "beta"]),
        ("nine_field_getter", &["alpha"]),
        ("nine_member_argument", &["alpha"]),
        ("nine_free_overload", &["alpha"]),
        ("nine_store_through_argument", &["alpha", "beta"]),
    ]);
    let (program, analysis) = conditional_values();
    let pag = trace_analysis::Pag::build(program);
    let db = export_program(program, &pag, analysis);
    let c = Connection::open(db.path()).unwrap();
    let span = program
        .symbols
        .variable(only_variable(program, "nine_global_argument"))
        .span;
    let count: i64 = c.query_row("SELECT COUNT(*) FROM flow_origins WHERE file_id=?1 AND line=?2 AND operation='argument'", rusqlite::params![span.file.0, span.line], |r| r.get(0)).unwrap();
    assert!(
        count > 0,
        "initializer parameter copy has source provenance"
    );
    assert!(program
        .symbols
        .call_sites
        .iter()
        .all(|site| site.span.file != span.file || site.span.line != span.line));
}

#[test]
fn conditional_reference_forwarding_retains_selected_calls_and_callable_bindings() {
    assert_conditional_callbacks(&[
        ("ten_forward_caller", &["alpha"]),
        ("ten_forward_value", &["alpha"]),
        ("ten_auto_cast", &["alpha"]),
        ("ten_auto_address", &["alpha"]),
        ("ten_subscript_getter", &["alpha"]),
        ("ten_auto_value", &["alpha"]),
    ]);
    let (program, _) = conditional_values();
    let value_wrap = only_function(program, "ten_auto_value_wrap");
    assert!(program.fn_returns[&value_wrap]
        .iter()
        .all(|flow| !matches!(flow, trace_ir::ReturnFlow::LvalueAddress(_))));
}

#[test]
fn conditional_offset_values_and_alias_casts_preserve_callbacks() {
    assert_conditional_callbacks(&[
        ("ten_offset_arms", &["alpha", "beta"]),
        ("ten_offset_value", &["alpha"]),
        ("ten_offset_invoke", &["alpha"]),
        ("ten_offset_callee", &["alpha", "beta"]),
        ("ten_enum_offset", &["alpha"]),
        ("ten_comma_cast", &["beta"]),
        ("ten_comma_cast_callee", &["beta"]),
        ("ten_comma_global_effect", &["alpha", "beta"]),
        ("ten_c_tag_cast", &["alpha", "beta"]),
        ("ten_c_comma_cast", &["beta"]),
        ("ten_c_shared_tag", &["alpha", "beta"]),
    ]);
    let (program, analysis) = conditional_values();
    for caller in [
        "ten_comma_cast",
        "ten_comma_cast_callee",
        "ten_c_comma_cast",
    ] {
        assert!(has_any_edge(
            program,
            analysis,
            caller,
            if caller == "ten_c_comma_cast" {
                "ten_c_get"
            } else {
                "ten_comma_get"
            }
        ));
        assert!(analysis
            .call_edges
            .iter()
            .filter(|e| fn_name(program, e.caller) == caller)
            .all(|e| !matches!(fn_name(program, e.callee).as_str(), "TenCast" | "TenF")));
    }
}

#[test]
fn conditional_global_defaults_respect_union_activity_and_anonymous_layouts() {
    assert_conditional_callbacks(&[
        ("ten_inactive_union", &["beta"]),
        ("ten_anonymous_default", &["alpha", "beta"]),
        ("ten_union_default", &["alpha", "beta"]),
        ("ten_second_union_default", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_parenthesized_global_aggregates_complete_defaults_in_cpp20() {
    let source =
        include_str!("../../../../tests/fixtures/conditional_values/review_ten_parenthesized.cpp");
    let dir = scratch(&[
        ("main.cpp", source),
        (
            "callbacks.cpp",
            "void alpha() {} void beta() {} void unrelated() {}",
        ),
    ]);
    let root = dir.path();
    std::fs::write(root.join("compile_commands.json"),serde_json::json!([{"directory":root,"file":"main.cpp","arguments":["c++","-std=c++20","-c","main.cpp"]}]).to_string()).unwrap();
    let program =
        trace_parse::build_program_with_jobs(root, &trace_preproc::PreprocessOptions::new(), 1)
            .unwrap();
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    let (_, analysis) = trace_analysis::analyze(&program);
    for caller in ["ten_parenthesized_default", "ten_parenthesized_override"] {
        assert!(has_any_edge(&program, &analysis, caller, "beta"));
        assert_eq!(
            has_any_edge(&program, &analysis, caller, "alpha"),
            caller == "ten_parenthesized_default"
        );
        must_not_have_edge(&program, &analysis, caller, "unrelated");
    }
}

#[test]
fn conditional_declared_reference_getters_survive_header_function_dependency_filtering() {
    let header =
        include_str!("../../../../tests/fixtures/conditional_values/review_thirteen_static.h");
    for jobs in [1, 4] {
        let dir = scratch(&[
            ("h.h", header),
            (
                "main.cpp",
                "#include \"h.h\"\nvoid alpha() {} void caller() { (*thirteen_header_p)(); }",
            ),
        ]);
        let p = trace_parse::build_program_with_jobs(dir.path(), &default_opts(dir.path()), jobs)
            .unwrap();
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        let (_, a) = trace_analysis::analyze(&p);
        assert!(has_any_edge(&p, &a, "caller", "alpha"));
    }
}

#[test]
fn conditional_nested_pointer_layers_and_offset_values_preserve_reads_and_writes() {
    assert_conditional_callbacks(&[
        ("thirteen_nested_store", &["alpha"]),
        ("thirteen_nested_callee", &["alpha", "beta"]),
        ("thirteen_offset_table", &["alpha", "beta"]),
        ("thirteen_offset_read", &["alpha", "beta"]),
        ("thirteen_returned_offset", &["alpha", "beta"]),
        ("thirteen_enumerator", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_return_categories_preserve_all_viable_values_and_callable_bindings() {
    assert_conditional_callbacks(&[
        ("thirteen_uncertain_returns", &["alpha", "beta"]),
        ("thirteen_callable_offset", &["alpha"]),
        ("thirteen_cast_table", &["alpha"]),
        ("thirteen_deferred_callable", &["alpha"]),
    ]);
}

#[test]
fn conditional_comma_reference_addresses_and_omitted_array_defaults_preserve_cells() {
    assert_conditional_callbacks(&[
        ("thirteen_use", &["alpha", "beta"]),
        ("thirteen_comma_return", &["alpha"]),
        ("thirteen_omitted_array_default", &["alpha", "beta"]),
        ("thirteen_full_array_override", &["beta"]),
    ]);
}

#[test]
fn conditional_file_getter_defaults_use_declaration_scope_and_explicit_overrides() {
    assert_conditional_callbacks(&[
        ("thirteen_free_default", &["alpha"]),
        ("thirteen_member_default", &["beta"]),
        ("thirteen_explicit_default_override", &["beta"]),
        ("thirteen_header_caller", &["alpha"]),
    ]);
}

#[test]
fn conditional_default_getter_expressions_survive_shared_headers_without_eager_effects() {
    assert_conditional_callbacks(&[
        ("thirteen_skipped_default_effect", &["beta"]),
        ("thirteen_evaluated_default_effect", &["alpha", "beta"]),
    ]);
    for jobs in [1, 4] {
        let dir=scratch(&[
            ("h.h","using F=void(*)(); void alpha(); namespace Defaults { static F a=alpha; inline F &get(F &v=a) { return v; } }"),
            ("main.cpp","#include \"h.h\"\nvoid alpha() {} F *p=&Defaults::get(); void caller() { (*p)(); }"),
            ("second.cpp","#include \"h.h\"\nF *q=&Defaults::get(); void second() { (*q)(); }"),
        ]);
        let p = trace_parse::build_program_with_jobs(dir.path(), &default_opts(dir.path()), jobs)
            .unwrap();
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        let (_, a) = trace_analysis::analyze(&p);
        for caller in ["caller", "second"] {
            assert!(has_any_edge(&p, &a, caller, "alpha"), "{caller}");
        }
    }
}

#[test]
fn conditional_callable_objects_and_lambdas_deduce_reference_returns() {
    assert_conditional_callbacks(&[
        ("four_callable_reference", &["alpha"]),
        ("four_lambda_reference", &["alpha"]),
    ]);
}

#[test]
fn conditional_union_storage_views_preserve_only_compatible_prefixes() {
    assert_conditional_callbacks(&[
        ("four_union_first", &["alpha"]),
        ("four_union_second", &["beta"]),
        ("four_union_write", &["alpha", "beta"]),
        ("four_union_prefix", &["alpha"]),
        ("four_union_incompatible_tail", &[]),
        ("four_c_union_read", &["alpha"]),
        ("four_c_union_write", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_first_member_casts_preserve_the_member_cell() {
    assert_conditional_callbacks(&[
        ("four_first_member_cast", &["alpha"]),
        ("four_single_member_cast", &["alpha"]),
    ]);
}

#[test]
fn conditional_selector_materialization_honors_the_ast_depth_bound() {
    for depth in [128, 256, 4096] {
        let expression = "c ? a : ".repeat(depth) + "b";
        let source = format!(
            "using F=void(*)(); void alpha(){{}} void beta(){{}} \
             void caller(bool c){{ F a=alpha,b=beta; F f={expression}; f(); }}"
        );
        let dir = scratch(&[("main.cpp", &source)]);
        let p =
            trace_parse::build_program_with_jobs(dir.path(), &default_opts(dir.path()), 1).unwrap();
        let (_, a) = trace_analysis::analyze(&p);
        assert!(has_any_edge(&p, &a, "caller", "alpha"), "depth {depth}");
        if depth < 512 {
            assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
            assert!(has_any_edge(&p, &a, "caller", "beta"), "depth {depth}");
        } else {
            assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
            assert!(p.diagnostics[0].message.contains("AST walk depth exceeded"));
            // Skipped suffixes must not allocate selector temporaries anyway.
            assert!(
                p.symbols.variables.len() < 600,
                "{}",
                p.symbols.variables.len()
            );
        }
    }
}

#[test]
fn conditional_unknown_array_extents_complete_only_missing_designated_elements() {
    assert_conditional_callbacks(&[
        ("defaults_full_check", &["beta"]),
        ("defaults_gap_check", &["alpha", "beta"]),
        ("defaults_reordered_check", &["beta"]),
        ("defaults_unresolved_check", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_file_member_getters_bind_explicit_reference_argument_positions() {
    assert_conditional_callbacks(&[
        ("defaults_member_explicit_check", &["alpha"]),
        ("defaults_member_second_check", &["beta"]),
    ]);
}

#[test]
fn conditional_offsets_accept_class_and_imported_unscoped_enums() {
    assert_conditional_callbacks(&[
        ("OffsetReviewHost::member", &["alpha", "beta"]),
        ("OffsetReviewHost::constant", &["alpha", "beta"]),
        ("offset_review_class_parameter", &["alpha", "beta"]),
        ("offset_review_class_constant", &["alpha", "beta"]),
        ("offset_review_imported", &["alpha", "beta"]),
        ("offset_review_imported_constant", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_offsets_accept_builtin_integer_expressions() {
    assert_conditional_callbacks(&[
        ("offset_review_arithmetic", &["alpha", "beta"]),
        ("offset_review_c_arithmetic", &["alpha", "beta"]),
    ]);
}

#[test]
fn conditional_ordinary_calls_bind_omitted_defaults_in_declaration_scope() {
    assert_conditional_callbacks(&[
        ("normal_default_callp", &["alpha"]),
        ("normal_default_take", &["alpha"]),
        ("NormalDefaultMethod::run", &["alpha"]),
        ("NormalDefaultCtor::run", &["alpha"]),
        ("NormalDefaultHeapCtor::NormalDefaultHeapCtor", &["alpha"]),
        ("normal_default_skip", &["alpha"]),
        ("normal_default_skipped_check", &["beta"]),
        ("normal_default_override", &["beta"]),
        ("normal_default_nested_take", &["alpha"]),
        ("NormalDefaultScope::take", &["alpha"]),
        ("normal_default_member_ref_caller", &["alpha"]),
        ("normal_default_reference_take", &["alpha"]),
        ("NormalDefaultVirtualBase::run", &["alpha"]),
        ("NormalDefaultVirtualDerived::run", &["alpha"]),
        ("NormalDefaultConcrete::run", &["alpha"]),
    ]);
    let (p, _) = conditional_values();
    let nested: Vec<_> = p
        .symbols
        .call_sites
        .iter()
        .filter(|site| {
            fn_name(p, site.caller) == "normal_default_nested_caller"
                && site
                    .callee_fn_id
                    .is_some_and(|f| fn_name(p, f) == "normal_default_make")
        })
        .collect();
    assert_eq!(nested.len(), 1);
    let value = nested[0]
        .return_dst
        .expect("default call has a return destination");
    let hosts: Vec<_> = p
        .symbols
        .call_sites
        .iter()
        .filter(|site| {
            fn_name(p, site.caller) == "normal_default_nested_caller"
                && site
                    .callee_fn_id
                    .is_some_and(|f| fn_name(p, f) == "normal_default_nested_take")
        })
        .collect();
    assert_eq!(hosts.len(), 2);
    for host in hosts {
        assert_eq!(host.var_args, [(0, value)]);
    }
    assert!(p.symbols.call_sites.iter().all(|site| site
        .details
        .as_ref()
        .is_none_or(|d| d.pending_defaults.is_none())));
    let source =
        std::fs::read_to_string(fixture("conditional_values").join("review_offsets_defaults.cpp"))
            .unwrap();
    let line = source
        .lines()
        .position(|line| line.starts_with("void normal_default_nested_take"))
        .unwrap()
        + 1;
    assert_eq!(nested[0].span.line as usize, line);
}

#[test]
fn conditional_ordinary_defaults_survive_shared_header_and_definition_merges() {
    for jobs in [1, 4] {
        let dir = scratch(&[
            (
                "h.h",
                "using F=void(*)(); void alpha(); void take(F f=alpha); struct Member { void run(F f=alpha); }; struct Constructed { Constructed(F f=alpha); };",
            ),
            (
                "main.cpp",
                "#include \"h.h\"\nvoid alpha(){} void caller(){take(); Member m; m.run(); Constructed c;}",
            ),
            ("take.cpp", "#include \"h.h\"\nvoid take(F f){f();} void Member::run(F f){f();} Constructed::Constructed(F f){f();}"),
        ]);
        let p = trace_parse::build_program_with_jobs(dir.path(), &default_opts(dir.path()), jobs)
            .unwrap();
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        let (_, a) = trace_analysis::analyze(&p);
        assert!(has_any_edge(&p, &a, "take", "alpha"));
        assert!(has_any_edge(&p, &a, "Member::run", "alpha"));
        assert!(has_any_edge(&p, &a, "Constructed::Constructed", "alpha"));
    }
}

#[test]
fn conditional_nested_default_calls_keep_each_caller_and_dependency_ownership() {
    for dependency in [false, true] {
        for jobs in [1, 4] {
            let dir = scratch(&[
                (
                    "dep/h.h",
                    "using F=void(*)(); F make(); void take(F f=make()); inline void excluded(){make();}",
                ),
                (
                    "main.cpp",
                    "#include \"dep/h.h\"\nvoid alpha(){} F make(){return alpha;} void first(){take();} void second(){take();}",
                ),
                ("take.cpp", "#include \"dep/h.h\"\nvoid take(F f){f();}"),
            ]);
            let mut opts = default_opts(dir.path());
            if dependency {
                opts.dep_roots.push(dir.path().join("dep"));
            }
            let p = trace_parse::build_program_with_jobs(dir.path(), &opts, jobs).unwrap();
            assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
            let (_, a) = trace_analysis::analyze(&p);
            assert!(has_any_edge(&p, &a, "take", "alpha"));
            let mut destinations = Vec::new();
            for caller in ["first", "second"] {
                assert!(has_any_edge(&p, &a, caller, "make"));
                let sites: Vec<_> = p
                    .symbols
                    .call_sites
                    .iter()
                    .filter(|site| fn_name(&p, site.caller) == caller && site.callee_name == "make")
                    .collect();
                assert_eq!(
                    sites.len(),
                    1,
                    "{caller}; dependency={dependency}; jobs={jobs}"
                );
                let site = sites[0];
                assert!(p
                    .symbols
                    .files
                    .iter()
                    .find(|file| file.id == site.span.file)
                    .unwrap()
                    .path
                    .ends_with("dep/h.h"));
                assert!(p
                    .symbols
                    .files
                    .iter()
                    .find(|file| file.id == site.scope_file())
                    .unwrap()
                    .path
                    .ends_with("main.cpp"));
                destinations.push(site.return_dst.expect("nested default return"));
            }
            assert_ne!(destinations[0], destinations[1]);
            if dependency {
                assert!(!p
                    .symbols
                    .call_sites
                    .iter()
                    .any(|site| fn_name(&p, site.caller) == "excluded"));
            }
        }
    }
}

#[test]
fn conditional_first_member_casts_keep_projections_in_all_value_consumers() {
    assert_conditional_callbacks(&[
        ("six_first_returned", &["alpha"]),
        ("six_first_use", &["alpha"]),
        ("six_first_callee", &["alpha"]),
        ("six_first_read", &["alpha"]),
        ("six_first_nested_returned", &["alpha"]),
        ("six_first_other", &["beta"]),
    ]);
}

#[test]
fn conditional_reference_defaults_bind_value_temporaries() {
    assert_conditional_callbacks(&[
        ("six_default_take", &["alpha"]),
        ("SixDefaultMember::take", &["alpha"]),
        ("SixDefaultConstruct::SixDefaultConstruct", &["alpha"]),
    ]);
}

#[test]
fn conditional_default_calls_union_loop_evidence_across_cached_hosts() {
    let (p, a) = conditional_values();
    for caller in [
        "six_default_loop",
        "six_default_chain_loop",
        "six_default_once",
    ] {
        let contexts: Vec<_> = a
            .execution_contexts
            .iter()
            .filter(|context| {
                fn_name(p, context.entry) == "six_default_entry"
                    && context.start.is_some_and(|start| {
                        p.symbols
                            .call_site_by_id(start.call_site)
                            .is_some_and(|site| fn_name(p, site.caller) == caller)
                    })
            })
            .collect();
        assert_eq!(contexts.len(), 1, "{caller}");
        let repeated = caller != "six_default_once";
        assert_eq!(contexts[0].self_concurrent, repeated, "{caller}");
        assert_eq!(
            contexts[0].multi_instance == trace_analysis::MultiInstance::Loop,
            repeated,
            "{caller}"
        );
    }
}

#[test]
fn conditional_default_calls_mark_unmodeled_sequencing_at_the_host() {
    let source = include_str!("../../../../tests/fixtures/conditional_values/review_six.cpp");
    let dir = scratch(&[
        ("main.cpp", source),
        ("callbacks.cpp", "void alpha() {} void beta() {}"),
    ]);
    let opts = default_opts(dir.path()).with_call_sequencing(true);
    let p = trace_parse::build_program_with_jobs(dir.path(), &opts, 1).unwrap();
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    let caller = p.symbols.resolve_function("six_default_sequence").unwrap();
    let host = p
        .symbols
        .call_sites
        .iter()
        .find(|site| site.caller == caller && site.callee_name == "six_default_take")
        .unwrap();
    assert!(p
        .symbols
        .call_sites
        .iter()
        .any(|site| site.caller == caller && site.callee_name == "six_default_make"));
    let variants = &p.cfgs[&caller];
    assert!(!variants.is_empty());
    for cfg in variants {
        let event = cfg
            .events
            .iter()
            .position(|event| event.sites.contains(&host.id))
            .unwrap();
        assert!(cfg
            .gaps
            .iter()
            .any(|gap| gap.kind == trace_ir::GapKind::UnsupportedConstruct
                && gap.span == Some(host.span)
                && gap.events
                    == Some((
                        trace_ir::EventId(event as u32),
                        trace_ir::EventId(event as u32)
                    ))));
    }
}

#[test]
fn conditional_address_and_decay_walks_honor_the_shared_depth_bound() {
    for depth in [128, 256, 4096] {
        for arrays in [false, true] {
            let expression = "c ? a : ".repeat(depth) + "b";
            let declarations = if arrays {
                "F a[1]={alpha},b[1]={beta};"
            } else {
                "F a=alpha,b=beta;"
            };
            let value = if arrays {
                format!("&({expression})[0]")
            } else {
                format!("&({expression})")
            };
            let source=format!("using F=void(*)();void alpha(){{}}void beta(){{}}void caller(bool c){{{declarations} F*p={value};(*p)();}}");
            let dir = scratch(&[("main.cpp", &source)]);
            let p = trace_parse::build_program_with_jobs(dir.path(), &default_opts(dir.path()), 1)
                .unwrap();
            let (_, a) = trace_analysis::analyze(&p);
            assert!(
                has_any_edge(&p, &a, "caller", "alpha"),
                "depth={depth}; arrays={arrays}"
            );
            if depth < 512 {
                assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
                assert!(has_any_edge(&p, &a, "caller", "beta"));
            } else {
                assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
                assert!(p.diagnostics[0].message.contains("AST walk depth exceeded"));
                assert!(p.flow.len() < 600, "{} flow facts", p.flow.len());
                assert!(
                    p.flow_origins.len() < 1_600,
                    "{} origins",
                    p.flow_origins.len()
                );
                assert!(
                    !has_any_edge(&p, &a, "caller", "beta"),
                    "the skipped suffix must not be expanded"
                );
            }
        }
    }
}

#[test]
fn conditional_function_designator_categories_honor_the_depth_bound() {
    for depth in [128, 256, 4096] {
        let expression = "c ? alpha : ".repeat(depth) + "beta";
        let source=format!("void alpha(){{}}void beta(){{}}decltype(auto) get(bool c){{return {expression};}} using F=void(*)(); void caller(bool c){{F f=&get(c);f();}}");
        let dir = scratch(&[("main.cpp", &source)]);
        let p =
            trace_parse::build_program_with_jobs(dir.path(), &default_opts(dir.path()), 1).unwrap();
        if depth < 512 {
            assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
            let (_, a) = trace_analysis::analyze(&p);
            assert!(has_any_edge(&p, &a, "caller", "alpha"));
            assert!(has_any_edge(&p, &a, "caller", "beta"));
        } else {
            assert_eq!(p.diagnostics.len(), 1, "{:?}", p.diagnostics);
            assert!(p.diagnostics[0].message.contains("AST walk depth exceeded"));
            assert!(p.symbols.variables.len() < 600);
            let get = p.symbols.resolve_function("get").unwrap();
            assert!(
                p.fn_returns[&get]
                    .iter()
                    .all(|flow| !matches!(flow, trace_ir::ReturnFlow::LvalueAddress(_))),
                "category inference must not prove a reference through the skipped suffix"
            );
        }
    }
}
