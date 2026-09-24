//! C++ lowering integration tests (first-step C++ support).
#![allow(clippy::needless_borrow)]

mod common;

use std::sync::OnceLock;

use common::{default_opts, fixture, fn_name, has_any_edge, must_not_have_edge};
use trace_analysis::{analyze, AnalysisResult, ResolutionKind};
use trace_ir::{FnId, Linkage, Program};
use trace_parse::build_program;

/// `fn $name() -> &'static (Program, AnalysisResult)`: the fixture of that
/// name, built and analysed once per test binary.
macro_rules! analyzed_fixture {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        fn $name() -> &'static (Program, AnalysisResult) {
            static CACHE: OnceLock<(Program, AnalysisResult)> = OnceLock::new();
            CACHE.get_or_init(|| {
                let root = fixture(stringify!($name));
                let program = build_program(&root, &default_opts(&root)).expect("build");
                let (_pag, analysis) = analyze(&program);
                (program, analysis)
            })
        }
    };
}

fn direct_targets(program: &Program, analysis: &AnalysisResult, caller: &str) -> Vec<String> {
    analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == caller && e.resolution == ResolutionKind::Direct)
        .map(|e| fn_name(&program, e.callee))
        .collect()
}

analyzed_fixture!(cpp_issue113);

#[test]
fn cpp_relative_qualified_call_uses_enclosing_namespace() {
    let (p, a) = cpp_issue113();
    assert!(has_any_edge(p, a, "app::relative", "app::Service::Write"));
}

#[test]
fn cpp_chained_call_uses_declared_return_type() {
    let (p, a) = cpp_issue113();
    for caller in ["app::chained", "app::chained_pointer"] {
        assert!(
            has_any_edge(p, a, caller, "app::Service::Start"),
            "{caller}"
        );
    }
}

#[test]
fn cpp_hwtest_fixture_keeps_member_context() {
    let (p, a) = cpp_issue113();
    for caller in [
        "app::Fixture_CallsMember_Test::TestBody",
        "app::Fixture_ParamCallsMember_Test::TestBody",
    ] {
        assert!(
            has_any_edge(p, a, caller, "app::Service::Start"),
            "{caller}"
        );
    }
    assert!(has_any_edge(
        p,
        a,
        "app::Standalone_CallsGlobal",
        "app::Service::Write"
    ));
}

#[test]
fn cpp_template_return_preserves_receiver_and_virtual_dispatch() {
    let (p, a) = cpp_issue113();
    assert!(has_any_edge(
        p,
        a,
        "app::Adapter::Filter",
        "app::Video::Filter"
    ));
    // The member the substitution reads is itself a resolved call: a
    // receiver spelled with its template arguments must still reach the
    // class the index holds it under.
    for caller in ["app::Adapter::Filter", "app::nested_holder"] {
        assert!(has_any_edge(p, a, caller, "app::Holder::Get"), "{caller}");
    }
}

#[test]
fn cpp_external_queue_invokes_submitted_lambda() {
    let (p, a) = cpp_issue113();
    assert!(a
        .call_edges
        .iter()
        .any(|e| fn_name(p, e.caller) == "app::queued"
            && fn_name(p, e.callee).starts_with("app::queued::$lambda")));
}

#[test]
fn cpp_callback_model_is_configurable_and_reaches_a_defined_callee() {
    let (p, _) = cpp_issue113();
    let models = trace_analysis::FnModelSet::from_toml_str(
        r#"
        [[model]]
        name = "app::custom_submit"
        effects = [{ kind = "invoke", param = 0 }]
        [[model]]
        name = "app::defined_submit"
        effects = [{ kind = "invoke", param = 0 }]
    "#,
    )
    .unwrap();
    let (_, a) = trace_analysis::analyze_with_options(
        p,
        trace_analysis::AnalyzeOptions {
            models: std::sync::Arc::new(models),
            ..Default::default()
        },
    );
    // A definition in view is not the same as the callback edge being in
    // view: the shipped `ffrt::queue::submit` is a template whose body the
    // index holds once, uninstantiated. Both sites carry the edge, once each.
    let mut sites: Vec<_> = a
        .call_edges
        .iter()
        .filter(|e| fn_name(p, e.caller) == "app::configured" && fn_name(p, e.callee) == "callback")
        .map(|e| {
            p.symbols.call_sites[e.call_site.0 as usize]
                .callee_name
                .clone()
        })
        .collect();
    sites.sort();
    assert_eq!(sites, vec!["custom_submit", "defined_submit"]);
    // A callback the model does not describe stays out: `callback_arg` takes
    // an argument, and nothing says what would be passed to it.
    assert!(!has_any_edge(p, &a, "app::queued_arg", "callback_arg"));
}

#[test]
fn cpp_template_return_follows_pointers_and_inherited_bases() {
    let (p, a) = cpp_issue113();
    // A pointer receiver names the same class template its value form does.
    for caller in ["app::holder_pointer", "app::Adopted::Run", "app::Far::Run"] {
        assert!(has_any_edge(p, a, caller, "app::Holder::Get"), "{caller}");
        assert!(
            has_any_edge(p, a, caller, "app::Service::Start"),
            "{caller} return"
        );
    }
    // A receiver spelled with its template arguments still finds the members
    // the index holds under the class's own name.
    assert!(has_any_edge(p, a, "app::holder_arrow", "app::Holder::Ping"));
}

#[test]
fn cpp_using_namespace_carries_a_qualified_call() {
    let (p, a) = cpp_issue113();
    assert!(has_any_edge(p, a, "via_using", "app::Service::Write"));
    // Across translation units: the definition written under the directive
    // is indexed under the class's own namespace and merges with the header's
    // prototype, so the call reaches one defined entry rather than a stub.
    assert!(has_any_edge(p, a, "via_using_remote", "rem::Remote::Call"));
    let entries: Vec<_> = p
        .symbols
        .functions
        .iter()
        .filter(|f| f.name.ends_with("Remote::Call"))
        .map(|f| (f.name.as_str(), f.is_defined))
        .collect();
    assert_eq!(entries, vec![("rem::Remote::Call", true)]);
}

#[test]
fn cpp_callback_model_reaches_every_function_the_argument_may_hold() {
    let (p, a) = cpp_issue113();
    for callee in ["callback", "callback_alt"] {
        assert!(has_any_edge(p, a, "app::queued_either", callee), "{callee}");
    }
}

#[test]
fn cpp_issue113_receiver_and_callback_boundaries() {
    let (p, a) = cpp_issue113();
    assert!(has_any_edge(
        p,
        a,
        "app::nested_holder",
        "app::Service::Start"
    ));
    assert!(has_any_edge(
        p,
        a,
        "app::global_qualified",
        "elsewhere::Service::Write"
    ));
    assert!(!has_any_edge(
        p,
        a,
        "app::global_qualified",
        "app::Service::Write"
    ));
    assert!(!has_any_edge(
        p,
        a,
        "app::mixed_auto_return",
        "app::Service::Start"
    ));
    // Unknown cast argument types must not select the operand's overload.
    assert!(!has_any_edge(
        p,
        a,
        "app::cast_chain",
        "app::Service::Start"
    ));
    assert!(!has_any_edge(
        p,
        a,
        "app::mixed_return",
        "app::Service::Start"
    ));
    assert!(has_any_edge(p, a, "app::long_chain", "app::Chain::Finish"));
    assert!(a
        .call_edges
        .iter()
        .any(|e| fn_name(p, e.caller) == "app::queued_variable"
            && fn_name(p, e.callee).starts_with("app::queued_variable::$lambda")));
    assert!(!a
        .call_edges
        .iter()
        .any(|e| fn_name(p, e.caller) == "app::unrelated"
            && fn_name(p, e.callee).starts_with("app::unrelated::$lambda")));
}

#[test]
fn cpp_auto_return_types_match_explicit_receivers() {
    let root = fixture("cpp_auto_return");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_, analysis) = analyze(&program);
    let members = |caller| {
        let mut targets: Vec<_> = direct_targets(&program, &analysis, caller)
            .into_iter()
            .filter(|name| name.starts_with("Worker::") || name.starts_with("Sub::"))
            .collect();
        targets.sort();
        targets
    };
    // The type of each local `p` in `caller`.
    let p_types = |caller: &str| {
        let id = program.symbols.resolve_function(caller).expect("caller");
        program
            .symbols
            .variables
            .iter()
            .filter(move |v| v.fn_id == Some(id) && v.name == "p")
            .map(|v| program.types.get(v.type_id).desc.as_ref())
    };
    assert_eq!(
        program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == "factories::select")
            .count(),
        2,
        "qualified reference-return definitions must merge with their own prototypes"
    );
    let expected = vec!["Sub::go", "Worker::go", "Worker::run"];
    for caller in [
        "typed",
        "inferred",
        "chained",
        "references",
        "defined",
        "qualified",
        "member",
        "conditional",
        "init_statement",
        "returned_reference",
        "copied_reference",
        "Owner::implicit_member",
        "qualified_definition",
        "global_qualified",
        "global_free",
        "global_shared",
        "factories::body_scope",
        "value_parameter",
        "qualified_parameter_spelling",
        "agreeing_overloads",
        "agreeing_unknown",
        "uses_std::bare_shared",
        "cam::promote_in_namespace",
        "cam::shared_const",
        "cast_auto",
        "bodyns::body",
        "lock_typed",
        "lock_auto",
        "promote_typed",
        "promote_auto",
        "shared_typed",
        "shared_auto",
        "unique_typed",
        "unique_auto",
    ] {
        assert_eq!(members(caller), expected, "{caller}");
    }
    for caller in [
        "unresolved",
        "dependent_use",
        "partial_unknown",
        "dependent_shared",
        "dependent_copy",
        "dependent_lock",
        "dependent_value_use",
        "box::Box::dependent_member",
        "bare_without_using",
        "dependent_receiver",
        "dependent_static",
        "Holder::use",
        "pointer_to_value",
        "pointer_depth_mismatch",
        "dependent_alias",
        "placeholder_scalar",
        "Mixin::through_this",
        "Mixin::implicit",
        "strat_user::via_using",
    ] {
        assert!(members(caller).is_empty(), "{caller} must stay unresolved");
        for guess in [
            "Other::run",
            "T::run",
            "N::run",
            "Event::run",
            "SelfBase::self",
        ] {
            assert!(
                !has_direct(&program, &analysis, caller, guess),
                "{caller} -> {guess}"
            );
        }
        for mut desc in p_types(caller) {
            // A pointer declarator keeps its own layers over the unknown type.
            while let trace_ir::TypeDesc::Ptr(inner) = desc {
                desc = inner;
            }
            assert_eq!(
                *desc,
                trace_ir::TypeDesc::Unknown,
                "{caller}::p must stay untyped"
            );
        }
    }
    assert!(
        must_not_have_edge(
            &program,
            &analysis,
            "reference_direct_init",
            "Widget::Widget"
        ),
        "binding a reference constructs nothing"
    );
    let wrapper = |caller: &str| match p_types(caller).next().expect("p") {
        trace_ir::TypeDesc::Struct { name, .. } => name.clone(),
        other => panic!("{caller}::p is {other:?}"),
    };
    assert_eq!(wrapper("lock_auto"), "std::shared_ptr<Worker>");
    assert_eq!(wrapper("promote_auto"), "OHOS::sptr<Worker>");
    assert_eq!(wrapper("unique_auto"), "std::unique_ptr<Worker>");
    assert_eq!(wrapper("cam::promote_in_namespace"), "cam::sptr<Worker>");
    assert!(
        matches!(
            p_types("cast_auto").next(),
            Some(trace_ir::TypeDesc::Ptr(inner)) if matches!(**inner, trace_ir::TypeDesc::Struct { .. })
        ),
        "`auto p = (Worker *)v` keeps the pointer layer"
    );
    for (caller, callee) in [
        ("shared_template", "Box::run"),
        ("ui::construct_local", "ui::Gadget::run"),
        ("inherited_nested", "NodeBase::Node::run"),
        ("new_qualified", "qual::Maker::run"),
        ("shadowed_scope", "Worker::run"),
        ("shadowed_scope", "Other::run"),
    ] {
        assert!(
            has_direct(&program, &analysis, caller, callee),
            "{caller} -> {callee}"
        );
    }
    for (caller, callee) in [
        ("strat_user::via_using", "Strategy::run"),
        ("ui::construct_local", "Other::run"),
        ("inherited_nested", "Node::run"),
    ] {
        assert!(
            must_not_have_edge(&program, &analysis, caller, callee),
            "{caller} -> {callee}"
        );
    }
    assert!(
        program.symbols.functions.iter().all(|f| f.name != "cb"),
        "`cb && cb();` declares no function"
    );
}

/// An `auto&` local passes the value it names, as an explicit `T&` does: it
/// must not rank `sink(Worker *)` over `sink(Worker)`.
#[test]
fn cpp_auto_reference_ranks_overloads_as_explicit_reference() {
    let root = fixture("cpp_auto_return");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_, analysis) = analyze(&program);
    let sinks = |caller: &str| {
        let mut targets: Vec<FnId> = analysis
            .call_edges
            .iter()
            .filter(|e| {
                fn_name(&program, e.caller) == caller && fn_name(&program, e.callee) == "sink"
            })
            .map(|e| e.callee)
            .collect();
        targets.sort();
        targets
    };
    let explicit = sinks("sink_explicit");
    assert_eq!(explicit.len(), 1, "`Worker &r` picks one overload");
    for caller in ["sink_auto", "sink_const_auto"] {
        assert_eq!(
            sinks(caller),
            explicit,
            "{caller} ranks as `Worker &r` does"
        );
    }
}

/// A definition spelled `N::f` looks names up in `N` when `N` is opened only by
/// a header the unit includes.
#[test]
fn cpp_body_scope_sees_namespace_opened_in_header() {
    let root = fixture("cpp_auto_return");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_, analysis) = analyze(&program);
    let mut targets = direct_targets(&program, &analysis, "hdrns::body");
    targets.sort();
    targets.dedup();
    for callee in ["hdrns::Job::go", "hdrns::Job::run"] {
        assert!(
            targets.iter().any(|t| t == callee),
            "hdrns::body -> {callee}: {targets:?}"
        );
    }
}

/// A variable declared in a condition (`if (Worker *p = f())`) is placed where
/// its declarator starts, as one in a plain declaration is, not at its type.
#[test]
fn cpp_condition_variable_span_is_its_declarator() {
    let root = fixture("cpp_auto_return");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let source = std::fs::read_to_string(root.join("main.cpp")).expect("fixture");
    let (row, text) = source
        .lines()
        .enumerate()
        .find(|(_, line)| line.starts_with("void condition_spans()"))
        .expect("condition_spans");
    for (name, declarator) in [("typed", "*typed"), ("inferred", "inferred")] {
        let var = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("local `{name}`"));
        let col = text.find(declarator).expect("declarator") as u32 + 1;
        assert_eq!(
            (var.span.line, var.span.col),
            (row as u32 + 1, col),
            "`{name}` must start at `{declarator}`"
        );
    }
}

#[test]
fn cpp_virtual_dispatch_expands_to_overrides() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let targets = direct_targets(&program, &analysis, "main");
    assert!(
        targets.iter().any(|t| t == "gfx::Shape::area"),
        "virtual s->area should target base Shape::area, got {targets:?}"
    );
    assert!(
        targets.iter().any(|t| t == "gfx::Circle::area"),
        "virtual s->area should target override Circle::area, got {targets:?}"
    );
}

#[test]
fn repeated_macro_virtual_calls_each_gain_cross_tu_override() {
    let root = fixture("macro_virtual_cross_tu");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let derived = program
        .symbols
        .functions
        .iter()
        .find(|function| function.name == "Derived::run")
        .expect("derived override")
        .id;
    let mut sites = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| {
            fn_name(&program, site.caller) == "invoke" && site.callee_fn_id == Some(derived)
        })
        .collect::<Vec<_>>();
    sites.sort_by_key(|site| site.expansion_span.map(|span| span.line));
    assert_eq!(sites.len(), 2, "each macro invocation needs the override");
    assert_eq!(sites[0].span, sites[1].span);
    assert_ne!(sites[0].expansion_span, sites[1].expansion_span);

    let (_pag, analysis) = analyze(&program);
    assert_eq!(
        analysis
            .call_edges
            .iter()
            .filter(|edge| { fn_name(&program, edge.caller) == "invoke" && edge.callee == derived })
            .count(),
        2
    );
}

#[test]
fn header_macro_passes_all_visible_static_overloads_as_arguments() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("reg.h"),
        "void reg(void (*f)(int));\n#define REG(x) reg(x)\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.cpp"),
        "#include \"reg.h\"\nstatic void cb(int) {}\nstatic void cb(double) {}\nvoid via_macro() { REG(cb); }\nvoid direct() { reg(cb); }\n",
    )
    .unwrap();

    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let macro_site = program
        .symbols
        .call_sites
        .iter()
        .find(|site| fn_name(&program, site.caller) == "via_macro" && site.callee_name == "reg")
        .expect("macro call");
    let mut overload_lines = macro_site
        .fn_args
        .iter()
        .filter(|(index, _)| *index == 0)
        .map(|(_, function)| program.symbols.function(*function).span.line)
        .collect::<Vec<_>>();
    overload_lines.sort_unstable();
    assert_eq!(overload_lines, vec![2, 3]);
}

#[test]
fn cpp_non_virtual_member_call_exact() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let hits = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "main"
                && fn_name(&program, e.callee) == "gfx::Shape::common"
        })
        .count();
    assert_eq!(hits, 1, "s->common must resolve to exactly one site-edge");

    let common = program
        .symbols
        .resolve_function("gfx::Shape::common")
        .expect("common defined");
    assert!(
        program.symbols.function(common).is_defined,
        "out-of-class definition must be the merged entry"
    );
}

#[test]
fn cpp_header_inline_method_dedups_with_out_of_class_uses() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // radius is defined inline in util.hpp; main calls it once.
    let hits = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.callee) == "gfx::Circle::radius")
        .count();
    assert_eq!(hits, 1, "header-inline method should dedup across TUs");
}

/// A C++ class in a `.h` (not `.hpp`) must be parsed with the C++ grammar
/// under PCH-style header IR. Extension-only language would lower it as C
/// and drop CHA for out-of-line `Plugin::OnEventProxy`.
#[test]
fn cpp_dot_h_header_virtual_call_expands() {
    let root = fixture("cpp_h_header");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(
            &program,
            &analysis,
            "Plugin::OnEventProxy",
            "Plugin::OnEvent"
        ),
        "implicit this->OnEvent from .h-declared Plugin"
    );
    assert!(
        has_direct(
            &program,
            &analysis,
            "Plugin::OnEventProxy",
            "Derived::OnEvent"
        ),
        "CHA must see Derived::OnEvent declared in plugin.h"
    );
    assert!(has_direct(
        &program,
        &analysis,
        "drive",
        "Plugin::OnEventProxy"
    ));
}

#[test]
fn cpp_ctor_and_dtor_sites() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let targets = direct_targets(&program, &analysis, "main");
    assert!(
        targets.iter().any(|t| t == "gfx::Circle::Circle"),
        "new Circle() should emit ctor edge"
    );
    assert!(
        targets.iter().any(|t| t == "gfx::Shape::~Shape"),
        "delete via base ptr should emit base dtor"
    );
    assert!(
        targets.iter().any(|t| t == "gfx::Circle::~Circle"),
        "virtual dtor expansion should include derived dtor"
    );
}

#[test]
fn cpp_overload_resolution_by_arity() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let add_edges: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "main"
                && fn_name(&program, e.callee).rsplit("::").next() == Some("add")
        })
        .map(|e| e.callee)
        .collect();
    assert_eq!(add_edges.len(), 2, "each arity resolves one overload");

    for callee in add_edges {
        let params = program.symbols.function(callee).params.len();
        let body_marks = direct_targets(&program, &analysis, &fn_name(&program, callee));
        if params == 2 {
            assert!(body_marks.contains(&"mark_i".to_string()));
        } else if params == 1 {
            assert!(body_marks.contains(&"mark_d".to_string()));
        } else {
            panic!("unexpected add overload with {params} params");
        }
    }
}

#[test]
fn cpp_namespace_qualified_call() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(
        has_direct(&program, &analysis, "main", "util::tag"),
        "namespaced util::tag should be a direct callee of main"
    );
}

#[test]
fn cpp_anonymous_namespace_is_internal() {
    let root = fixture("cpp_basic");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        program.symbols.resolve_function("hidden").is_none(),
        "anon-namespace functions must not be in external lookup"
    );
    assert!(has_direct(&program, &analysis, "hidden", "util::tag"));
}

fn has_direct(program: &Program, analysis: &AnalysisResult, caller: &str, callee: &str) -> bool {
    analysis.call_edges.iter().any(|e| {
        fn_name(&program, e.caller) == caller
            && fn_name(&program, e.callee) == callee
            && e.resolution == ResolutionKind::Direct
    })
}

// --- cpp_more: overload ties, templates, multiple inheritance,
// ctor-initializer lists, static member functions ---

fn edges_to(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee_suffix: &str,
    resolution: ResolutionKind,
) -> Vec<String> {
    analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == caller
                && fn_name(&program, e.callee).ends_with(callee_suffix)
                && e.resolution == resolution
        })
        .map(|e| fn_name(&program, e.callee))
        .collect()
}

#[test]
fn cpp_overload_tie_emits_both_sites() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let hits = edges_to(&program, &analysis, "drive", "tie", ResolutionKind::Direct);
    assert_eq!(
        hits.len(),
        2,
        "same-arity overload tie must emit one site per candidate"
    );
}

#[test]
fn cpp_template_class_method_resolves_by_primary_name() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(&program, &analysis, "drive", "Box::put"),
        "Box<Widget>::put call should resolve under primary name"
    );
}

#[test]
fn cpp_virtual_call_through_base_of_multiple_inheritance() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_direct(&program, &analysis, "drive", "A::fa"));
    assert!(
        has_direct(&program, &analysis, "drive", "AB::fa"),
        "virtual expansion must include the multiple-inheritance override"
    );
    assert!(!has_direct(&program, &analysis, "drive", "B::fb"));
}

#[test]
fn cpp_ctor_initializer_list_targets() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_direct(&program, &analysis, "D::D", "Base::Base"));
    assert!(has_direct(&program, &analysis, "D::D", "Member::Member"));
    // D d2(5): constructor-declaration with argument list.
    assert!(has_direct(&program, &analysis, "drive", "D::D"));
}

#[test]
fn cpp_static_member_function_resolves() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let hits = edges_to(
        &program,
        &analysis,
        "drive",
        "S::Make",
        ResolutionKind::Direct,
    );
    assert!(hits.len() >= 2, "both S::Make calls should resolve");
}

#[test]
fn cpp_inherited_non_virtual_via_derived_receiver() {
    let root = fixture("cpp_more");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(&program, &analysis, "drive", "Base::base_value"),
        "d2.base_value() should walk up to Base"
    );
    assert!(has_direct(&program, &analysis, "sink_w", "Widget::make"));
}

// --- cpp_implicit_this: bare method calls, smart_ptr unwrap ---

#[test]
fn cpp_implicit_this_virtual_call_expands() {
    let root = fixture("cpp_implicit_this");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_direct(&program, &analysis, "drive", "Base::go"));
    let hooks = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "Base::go" && e.resolution == ResolutionKind::Direct
        })
        .map(|e| fn_name(&program, e.callee))
        .collect::<Vec<_>>();
    assert!(
        hooks.iter().any(|t| t == "Base::hook"),
        "implicit this->hook should hit Base::hook, got {hooks:?}"
    );
    assert!(
        hooks.iter().any(|t| t == "Derived::hook"),
        "virtual expansion should include Derived::hook, got {hooks:?}"
    );
}

#[test]
fn cpp_smart_ptr_member_call_unwraps_pointee() {
    let root = fixture("cpp_implicit_this");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(&program, &analysis, "call_sp", "Plugin::OnEventProxy"),
        "shared_ptr<Plugin> p; p->OnEventProxy should type as Plugin"
    );
    assert!(
        has_direct(&program, &analysis, "call_sp_ref", "Plugin::OnEventProxy"),
        "const shared_ptr<Plugin> & should unwrap to Plugin"
    );
    assert!(
        has_direct(
            &program,
            &analysis,
            "Plugin::OnEventProxy",
            "Plugin::OnEvent"
        ),
        "OnEventProxy body implicit this->OnEvent"
    );
    assert!(
        has_direct(&program, &analysis, "call_up", "Plugin::OnEventProxy"),
        "unique_ptr<Plugin> should unwrap like shared_ptr"
    );
    assert!(
        has_direct(&program, &analysis, "call_wp", "Plugin::OnEventProxy"),
        "weak_ptr<Plugin> should unwrap like shared_ptr"
    );
}

#[test]
fn cpp_smart_ptr_field_receiver_unwraps() {
    let root = fixture("cpp_implicit_this");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(&program, &analysis, "Holder::go", "Plugin::OnEvent"),
        "plugin_->OnEvent on a shared_ptr field should type as Plugin"
    );
}

// --- cpp_smart_ptr: member access through a declared `operator->` (#64) ---

analyzed_fixture!(
    /// The `cpp_smart_ptr` fixture, analysed once.
    cpp_smart_ptr
);

#[test]
fn arrow_unwraps_undeclared_single_class_argument() {
    let (program, analysis) = cpp_smart_ptr();
    for caller in [
        "AbsentLocal",
        "AbsentParameter",
        "AbsentField",
        "AbsentQualified",
        "AbsentForward",
    ] {
        for target in ["AbsentTarget::Run", "AbsentDerived::Run"] {
            assert!(
                has_direct(program, analysis, caller, target),
                "{caller} -> {target}"
            );
        }
    }
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name.ends_with("ForwardOnly::Run")));
}

#[test]
fn arrow_fallback_honours_global_scope_and_star() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "Outer::Inner::AbsentShadowed",
        "Outer::Inner::Shadow::Run"
    ));
    assert!(
        has_direct(
            program,
            analysis,
            "Outer::Inner::AbsentGlobal",
            "Shadow::Run"
        ),
        "`::Shadow` names the global class, not the enclosing namespace's"
    );
    assert!(must_not_have_edge(
        program,
        analysis,
        "Outer::Inner::AbsentGlobal",
        "Outer::Inner::Shadow::Run"
    ));
    assert!(has_direct(
        program,
        analysis,
        "Outer::Inner::AbsentGlobalQualified",
        "Outer::Scoped::Run"
    ));
    assert!(
        has_direct(program, analysis, "AbsentStar", "AbsentTarget::Run"),
        "`(*p).Run()` unwraps the same way `p->Run()` does"
    );
}

#[test]
fn arrow_fallback_skips_nested_types_and_spells_scalar_arguments_as_written() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(
        must_not_have_edge(
            program,
            analysis,
            "Outer::Inner::AbsentNestedType",
            "AbsentTarget::Run"
        ),
        "`missing<AbsentTarget>::Inner` is not a wrapper around `AbsentTarget`"
    );
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name.contains("missing")));
    let type_of = |var: &str| -> String {
        let v = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == var)
            .unwrap_or_else(|| panic!("{var} must be indexed"));
        match program.types.get(v.type_id).desc.as_ref() {
            trace_ir::TypeDesc::Struct { name, .. } => name.clone(),
            other => panic!("{var}: {other:?}"),
        }
    };
    assert_eq!(type_of("scalar_box"), "Outer::Inner::missing<int>");
    assert_eq!(
        type_of("callback_box"),
        "Outer::Inner::missing<void(int,char)>"
    );
    assert_eq!(
        type_of("fnptr_box"),
        "Outer::Inner::missing<int(*)(int,int)>"
    );
}

#[test]
fn cpp17_nested_namespace_definition_opens_each_scope() {
    let root = fixture("cpp_nested_namespace");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    for name in [
        "a::b::Deep::Go",
        "a::b::use_deep",
        "a::b::c::tri",
        "a::b::c::again",
        "a::b::inside",
        "x::y::in_y",
        "tabbed::ty::tabbed_leaf",
        "global_after",
    ] {
        let id = program
            .symbols
            .resolve_function(name)
            .unwrap_or_else(|| panic!("{name} must be indexed under its qualified name"));
        assert_eq!(
            program.symbols.function(id).linkage,
            Linkage::External,
            "{name} is not in an anonymous namespace"
        );
    }
    assert!(
        !program
            .symbols
            .functions
            .iter()
            .any(|f| f.name.contains("inline")),
        "the `inline` keyword is not part of a namespace name"
    );
    assert!(has_direct(
        &program,
        &analysis,
        "a::b::use_deep",
        "a::b::Deep::Go"
    ));
    assert!(has_direct(
        &program,
        &analysis,
        "a::b::c::tri",
        "a::b::Deep::Go"
    ));
    assert!(has_direct(
        &program,
        &analysis,
        "a::b::c::again",
        "a::b::c::tri"
    ));
    assert!(has_direct(&program, &analysis, "a::b::inside", "util::tag"));
    assert!(
        must_not_have_edge(&program, &analysis, "a::after", "util::tag"),
        "a `using namespace` inside the block must not outlive it"
    );
    assert!(has_direct(
        &program,
        &analysis,
        "global_after",
        "a::b::use_deep"
    ));
}

#[test]
fn arrow_fallback_looks_the_argument_up_like_cpp() {
    let (program, analysis) = cpp_smart_ptr();
    for caller in [
        "Outer::Inner::AbsentEnclosing",
        "Outer::Inner::AbsentAlias",
        "Outer::Inner::AbsentEastConst",
    ] {
        assert!(
            has_direct(program, analysis, caller, "Outer::Scoped::Run"),
            "{caller}"
        );
    }
    assert!(has_direct(
        program,
        analysis,
        "Outer::Other::AbsentPartial",
        "Outer::Inner::Deep::Run"
    ));
}

#[test]
fn arrow_undeclared_fallback_rejects_unsupported_arguments() {
    let (program, analysis) = cpp_smart_ptr();
    for caller in [
        "AbsentTwo",
        "AbsentScalar",
        "AbsentUnknown",
        "AbsentPointer",
        "AbsentReference",
        "AbsentMentioned",
    ] {
        let id = program.symbols.resolve_function(caller).expect("caller");
        assert!(
            !analysis.call_edges.iter().any(|e| e.caller == id),
            "{caller} must stay unresolved"
        );
    }
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "OHOS::sptr::Run"));
}

#[test]
fn arrow_fallback_preserves_dot_and_declared_wrappers() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "DrawMissingField",
        "Widget::Draw"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "DrawNoArrow",
        "Widget::Draw"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "AbsentDot",
        "AbsentTarget::promote"
    ));
    assert!(has_direct(
        program,
        analysis,
        "DeclaredWins",
        "RealTarget::Run"
    ));
    for caller in ["DeclaredWins", "DeclaredNoArrow", "DeclaredOutOfLine"] {
        assert!(must_not_have_edge(
            program,
            analysis,
            caller,
            "AbsentTarget::Run"
        ));
    }
    assert!(
        has_direct(program, analysis, "DeclaredOutOfLine", "RealTarget::Run"),
        "an out-of-line `operator->` without its class header still says what the arrow yields"
    );
}

#[test]
fn arrow_wrapper_head_is_looked_up_through_enclosing_namespaces() {
    let (program, analysis) = cpp_smart_ptr();
    let caller = "Outer::Inner::DeclaredFromEnclosing";
    assert!(has_direct(program, analysis, caller, "Outer::Scoped::Run"));
    assert!(must_not_have_edge(
        program,
        analysis,
        caller,
        "AbsentTarget::Run"
    ));
    assert!(
        has_any_edge(program, analysis, caller, "Outer::OuterBox::Get"),
        "`.` stays on the wrapper, under the wrapper's own namespace"
    );
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "Outer::Inner::OuterBox::Get"));
}

#[test]
fn arrow_fallback_matches_a_typedef_by_its_whole_spelling() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "AbsentQualifiedAlias",
        "Outer::Scoped::Run"
    ));
    assert!(
        must_not_have_edge(
            program,
            analysis,
            "Outer::Inner::AbsentNoHijack",
            "AbsentTarget::Run"
        ),
        "`NoSuch::GlobalHijack` is not the global `GlobalHijack`"
    );
}

#[test]
fn field_access_through_a_wrapper_reaches_the_pointee_field() {
    // A lowering-shape check: one field step per site, no duplicates.
    // That the step reaches the pointee's summary in the solver is
    // `wrapper_fields_share_pointee_callback_summaries`.
    let (program, _analysis) = cpp_smart_ptr();
    let geps = program
        .flow
        .iter()
        .filter(|f| {
            matches!(f, trace_ir::FlowConstraint::GepField { field_name, .. }
                if field_name == "absent_payload_value")
        })
        .count();
    assert_eq!(
        geps, 3,
        "one field step for the read, the write, and the read on the wrapper variable itself"
    );
}

#[test]
fn field_step_follows_its_operator_on_a_wrapper() {
    // `b.w.own_raw` is the wrapper's own field: a dot never steps through
    // a wrapper, even one with a declared `operator->`.
    let (program, _analysis) = cpp_smart_ptr();
    let own = program
        .flow
        .iter()
        .filter(|f| {
            matches!(f, trace_ir::FlowConstraint::GepField { field_name, .. }
                if field_name == "own_raw")
        })
        .count();
    assert_eq!(own, 1, "`b.w.own_raw` must reach the wrapper's own field");
}

fn struct_tag_names(program: &Program) -> Vec<String> {
    program
        .types
        .all()
        .iter()
        .filter_map(|t| match t.desc.as_ref() {
            trace_ir::TypeDesc::Struct { name, .. } if !name.is_empty() => Some(name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn arrow_wrapper_head_spelled_global_is_tagged_without_the_prefix() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(
        has_direct(
            program,
            analysis,
            "Elsewhere::AbsentGlobalHead",
            "AbsentTarget::Run"
        ),
        "`::missing<AbsentTarget>` inside a namespace still unwraps"
    );
    let tags = struct_tag_names(program);
    assert!(
        tags.iter().any(|t| t == "missing<AbsentTarget>"),
        "the global wrapper is tagged as it is registered: {tags:?}"
    );
    assert!(
        !tags
            .iter()
            .any(|t| t.starts_with("::") || t.contains("::::")),
        "no tag keeps a leading `::` or gains an empty segment: {tags:?}"
    );
}

#[test]
fn templated_member_type_of_a_wrapper_keeps_its_separator() {
    let (program, analysis) = cpp_smart_ptr();
    let tags = struct_tag_names(program);
    assert!(
        tags.iter()
            .any(|t| t == "Outer2<AbsentTarget>::Cursor<AbsentTarget>"),
        "the member type keeps its `::` although a global `Cursor` exists: {tags:?}"
    );
    assert!(
        !tags.iter().any(|t| t.contains(">Cursor")),
        "no tag glues the member type onto its owner: {tags:?}"
    );
    for callee in ["AbsentTarget::Run", "Cursor::Run"] {
        assert!(
            must_not_have_edge(program, analysis, "AbsentTemplatedTail", callee),
            "a member type of a template is not a wrapper around its argument"
        );
    }
}

#[test]
fn arrow_fallback_never_spells_a_standard_name_or_pointer_as_a_class() {
    let (program, analysis) = cpp_smart_ptr();
    let tags = struct_tag_names(program);
    assert!(
        tags.iter().any(|t| t == "Elsewhere::missing<nullptr_t>"),
        "`nullptr_t` stays unqualified: {tags:?}"
    );
    assert!(
        tags.iter()
            .any(|t| t == "Elsewhere::missing<AbsentTarget*>"),
        "`AbsentTarget*const` is the pointer argument `AbsentTarget*`: {tags:?}"
    );
    assert!(
        !tags
            .iter()
            .any(|t| t.contains("Elsewhere::nullptr_t") || t.contains("const")),
        "no tag qualifies a standard name or keeps a trailing qualifier: {tags:?}"
    );
    for caller in [
        "Elsewhere::AbsentNullptrArg",
        "Elsewhere::AbsentPtrConstArg",
    ] {
        assert!(
            must_not_have_edge(program, analysis, caller, "AbsentTarget::Run"),
            "{caller}: neither argument is a declared class, so no guess"
        );
    }
}

#[test]
fn template_arguments_that_name_no_class_are_spelled_as_written() {
    let (program, _analysis) = cpp_smart_ptr();
    let tags = struct_tag_names(program);
    for expected in [
        "Elsewhere::missing<AbsentTarget,4>",
        "Elsewhere::missing<true>",
        "Elsewhere::missing<AbsentTarget**>",
    ] {
        assert!(
            tags.iter().any(|t| t == expected),
            "{expected} missing from {tags:?}"
        );
    }
    assert!(
        !tags
            .iter()
            .any(|t| t.contains("::4") || t.contains("::true")),
        "a literal is never qualified: {tags:?}"
    );
}

#[test]
fn defined_template_head_spelled_global_finds_the_declared_class() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(
        has_any_edge(
            program,
            analysis,
            "Outer::Deep::DefinedGlobalHead",
            "Outer::Defined::Cursor::Next"
        ),
        "`::Outer::Defined<int>::Cursor` is the same class as `Defined<int>::Cursor`"
    );
    let df = program
        .flow
        .iter()
        .filter(|f| {
            matches!(f, trace_ir::FlowConstraint::GepField { field_name, .. } if field_name == "df")
        })
        .count();
    assert_eq!(
        df, 1,
        "`::Outer::Defined<int>` is the defined class, with its layout"
    );
}

#[test]
fn member_type_of_a_defined_template_keeps_the_class_the_lookup_found() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(
        has_any_edge(
            program,
            analysis,
            "Outer::Deep::DefinedTail",
            "Outer::Defined::Cursor::Next"
        ),
        "`Defined<int>::Cursor` inside `Outer::Deep` is `Outer::Defined::Cursor`"
    );
    let tags = struct_tag_names(program);
    assert!(
        !tags.iter().any(|t| t == "Defined::Cursor"),
        "the enclosing namespace is not dropped from the member type: {tags:?}"
    );
}

#[test]
fn arrow_unwraps_by_declaration_not_by_wrapper_name() {
    // The issue's three-way reproduction: the same source, differing only in
    // the wrapper's name. `sptr` and `RefPtr` used to invent an external
    // `sptr::AddOutput` / `RefPtr::AddOutput`; only `shared_ptr` unwrapped,
    // because only `shared_ptr` was on a hardcoded list.
    let (program, analysis) = cpp_smart_ptr();
    for caller in ["UseSptr", "UseRefPtr", "UseShared"] {
        assert!(
            has_direct(program, analysis, caller, "CaptureSession::AddOutput"),
            "{caller}: `session->AddOutput` must resolve through the wrapper"
        );
    }
    for (caller, wrapper) in [("UseSptr", "sptr"), ("UseRefPtr", "RefPtr")] {
        assert!(
            must_not_have_edge(program, analysis, caller, &format!("{wrapper}::AddOutput")),
            "no member may be invented on {wrapper} itself"
        );
    }
}

#[test]
fn arrow_follows_a_chain_of_wrappers() {
    // `Outer::operator->` yields `Inner`, whose `operator->` yields the
    // pointee; neither is a template, so both are resolved at the call site.
    let (program, analysis) = cpp_smart_ptr();
    assert!(
        has_direct(program, analysis, "UseChain", "CaptureSession::AddOutput"),
        "outer->AddOutput must follow Outer -> Inner -> CaptureSession"
    );
}

#[test]
fn arrow_through_a_wrapper_field_unwraps() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(
        has_direct(program, analysis, "UseField", "CaptureSession::AddOutput"),
        "holder->session_->AddOutput must unwrap the field's wrapper too"
    );
}

#[test]
fn an_unfollowable_arrow_chain_leaves_the_site_unresolved() {
    // `Ping::operator->` yields `Pong`, whose `operator->` yields `Ping`:
    // the chain names no pointee. An unresolved indirect site is honest; an
    // invented `Ping::AddOutput` would be indistinguishable from a real call
    // to an out-of-tree function.
    let (program, analysis) = cpp_smart_ptr();
    let targets: Vec<String> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(program, e.caller) == "UseCycle")
        .map(|e| fn_name(program, e.callee))
        .collect();
    assert!(
        targets.is_empty(),
        "a cyclic operator-> chain must invent nothing, got {targets:?}"
    );
}

#[test]
fn a_wrapper_parameter_resolves_across_headers() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(
        has_direct(program, analysis, "DrawThrough", "Widget::Draw"),
        "a wrapper spelled in a .cpp signature unwraps like any other"
    );
}

#[test]
fn arrow_field_across_headers() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "WidgetBox::DrawHeld",
        "Widget::Draw"
    ));
}

#[test]
fn arrow_preserves_wrapper_dot() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(program, analysis, "UseDot", "sptr::GetRefPtr"));
    assert!(must_not_have_edge(
        program,
        analysis,
        "UseDot",
        "CaptureSession::GetRefPtr"
    ));
}

#[test]
fn arrow_raw_pointer() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "UseRaw",
        "PointerTarget::Own"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "UseRaw",
        "CaptureSession::Own"
    ));
}

#[test]
fn arrow_explicit_this() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "PointerTarget::ExplicitThis",
        "PointerTarget::Own"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "PointerTarget::ExplicitThis",
        "CaptureSession::Own"
    ));
}

#[test]
fn arrow_pointer_return_terminates() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "UsePointerReturn",
        "PointerTarget::Own"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "UsePointerReturn",
        "CaptureSession::Own"
    ));
}

#[test]
fn arrow_reference_receiver() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "UseReference",
        "CaptureSession::AddOutput"
    ));
}

#[test]
fn arrow_second_template_parameter() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "UseSecond",
        "CaptureSession::AddOutput"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "UseSecond",
        "PointerTarget::AddOutput"
    ));
}

#[test]
fn arrow_raw_pointer_field() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "UseRawField",
        "PointerTarget::Own"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "UseRawField",
        "CaptureSession::Own"
    ));
}

#[test]
fn arrow_local_reference() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(has_direct(
        program,
        analysis,
        "UseLocalReference",
        "CaptureSession::AddOutput"
    ));
}

#[test]
fn arrow_disagreeing_overloads_do_not_guess_a_template_argument() {
    let (program, analysis) = cpp_smart_ptr();
    assert!(program
        .symbols
        .call_sites
        .iter()
        .any(|site| fn_name(program, site.caller) == "UseAmbiguous"));
    assert!(!analysis
        .call_edges
        .iter()
        .any(|edge| fn_name(program, edge.caller) == "UseAmbiguous"));
}

#[test]
fn arrow_unsupported_substitutions_stay_unresolved() {
    let (program, analysis) = cpp_smart_ptr();
    for caller in ["UseInheritedTemplate", "UseNestedDependent"] {
        assert!(program
            .symbols
            .call_sites
            .iter()
            .any(|site| fn_name(program, site.caller) == caller));
        assert!(
            !analysis
                .call_edges
                .iter()
                .any(|edge| fn_name(program, edge.caller) == caller),
            "{caller} must not invent a target"
        );
    }
}

#[test]
fn raw_wrapper_pointer_preserves_callback_flow() {
    let (program, analysis) = cpp_smart_ptr();
    for caller in ["RawWrapperDirect", "RawWrapperNested"] {
        assert!(
            has_any_edge(program, analysis, caller, "RawWrapperTarget"),
            "{caller}: raw arrow must preserve the wrapper's callback flow"
        );
    }
}

#[test]
fn a_peeled_receiver_dispatches_through_every_base() {
    // A member found on the second base is as reachable as one on the first:
    // the peeled receiver goes through the ordinary hierarchy lookup.
    let (program, analysis) = cpp_smart_ptr();
    for (caller, target) in [
        ("AbsentMultiFirst", "MultiBaseA::FromA"),
        ("AbsentMultiSecond", "MultiBaseB::FromB"),
        ("AbsentMultiOwn", "MultiDerived::Own"),
    ] {
        assert!(
            has_direct(program, analysis, caller, target),
            "{caller} must reach {target}"
        );
    }
}

#[test]
fn wrapper_fields_share_pointee_callback_summaries() {
    let (program, analysis) = cpp_smart_ptr();
    for (caller, target) in [
        ("FlowReadMissing", "FlowReadTarget"),
        ("FlowReadDeclared", "FlowReadTarget"),
        ("FlowReadDerefDot", "FlowReadTarget"),
        ("FlowReadDerefDotDeclared", "FlowReadTarget"),
        ("FlowReadDerefDotRaw", "FlowReadTarget"),
        ("FlowReadDerefDotWrapperRaw", "FlowWrapperOwnTarget"),
        ("FlowReadReference", "FlowReadTarget"),
        ("FlowReadDereference", "FlowReadTarget"),
        ("FlowReadStandard", "FlowReadTarget"),
        ("FlowReadRaw", "FlowWriteTarget"),
        ("FlowReadRaw", "FlowDerefWriteTarget"),
        ("FlowReadNested", "FlowNestedTarget"),
        ("FlowReadTwoArrows", "FlowNestedTarget"),
        ("FlowReadNestedRaw", "FlowNestedTarget"),
        ("FlowReadWrapperOwn", "FlowWrapperOwnTarget"),
        ("FlowReadInherited", "FlowReadTarget"),
        ("FlowReadParen", "FlowReadTarget"),
        ("FlowReadParenWrapper", "FlowReadTarget"),
    ] {
        assert!(
            has_any_edge(program, analysis, caller, target),
            "{caller} must reach {target}"
        );
    }
    for (caller, target) in [
        ("FlowReadDeclared", "FlowWrapperOwnTarget"),
        ("FlowReadDerefDotDeclared", "FlowWrapperOwnTarget"),
        ("FlowReadDerefDotWrapperRaw", "FlowReadTarget"),
        ("FlowReadWrapperOwn", "FlowReadTarget"),
    ] {
        assert!(
            must_not_have_edge(program, analysis, caller, target),
            "{caller} must not confuse wrapper and pointee fields"
        );
    }
}

#[test]
fn wrapper_storage_stays_out_of_the_pointee() {
    // `IsoWrapper::cb` and `IsoPayload::cb` share name and position, so only
    // the unwrap's type filter keeps the address-taken wrapper's own storage
    // from reaching pointee accesses (#141).
    let (program, analysis) = cpp_smart_ptr();
    for caller in [
        "IsoReadArrow",
        "IsoReadDeref",
        "IsoReadReference",
        "IsoReadPointerArrow",
    ] {
        assert!(
            has_any_edge(program, analysis, caller, "IsoPointeeTarget"),
            "{caller} reads the pointee's cb"
        );
        assert!(
            must_not_have_edge(program, analysis, caller, "IsoWrapperTarget"),
            "{caller} must not read the wrapper's own cb"
        );
    }
    for caller in ["IsoReadWrapperDot", "IsoReadWrapperRaw"] {
        assert!(
            has_any_edge(program, analysis, caller, "IsoWrapperTarget"),
            "{caller} reads the wrapper's own cb"
        );
        assert!(
            must_not_have_edge(program, analysis, caller, "IsoPointeeTarget"),
            "{caller} must not read the pointee's cb"
        );
    }
}

#[test]
fn wrapper_reference_and_dereference_still_unwrap_fields() {
    let (program, _) = cpp_smart_ptr();
    for caller in ["RawWrapperReference", "RawWrapperDereference"] {
        assert!(
            program.flow.iter().any(|flow| {
                let trace_ir::FlowConstraint::GepField {
                    dst, field_name, ..
                } = flow
                else {
                    return false;
                };
                field_name == "payload_value"
                    && program.symbols.variable(*dst).fn_id
                        == program.symbols.resolve_function(caller)
            }),
            "{caller}: a wrapper value must still use overloaded arrow"
        );
    }
}

#[test]
fn arrow_wrapper_identity_preserves_layout_and_construction() {
    let (program, analysis) = cpp_smart_ptr();
    for caller in ["UseTemplateCallback", "UseWrapperCallback"] {
        assert!(common::has_any_edge(
            program,
            analysis,
            caller,
            "CallbackTarget"
        ));
    }
    assert!(has_direct(
        program,
        analysis,
        "UseWrapperCallback",
        "CallbackHandle::CallbackHandle"
    ));
    assert!(has_direct(
        program,
        analysis,
        "ConstructHolder::ConstructHolder",
        "CallbackHandle::CallbackHandle"
    ));
}

#[test]
fn a_dereferenced_wrapper_is_its_pointee() {
    // `(*w).m()` and `(*pw)->m()`: a smart pointer's `operator*` names the
    // pointee its `operator->` does, and a raw pointer to a wrapper is the
    // wrapper itself.
    let (program, analysis) = cpp_smart_ptr();
    for caller in ["UseDerefDot", "UseDerefDotShared", "UseDerefArrow"] {
        assert!(
            has_direct(program, analysis, caller, "CaptureSession::AddOutput"),
            "{caller}: `*w` must yield the pointee, as `w->` does"
        );
    }
}

#[test]
fn a_dot_call_on_a_wrapper_is_the_wrappers_member() {
    // The wrapper keeps its own class for `.`: `p.reset()` on a
    // `shared_ptr<Resettable>` is the wrapper's `reset`, and must not bind
    // to the pointee's member of the same name.
    let (program, analysis) = cpp_smart_ptr();
    assert!(must_not_have_edge(
        program,
        analysis,
        "UseDotShadow",
        "Resettable::reset"
    ));
}

#[test]
fn dereferencing_a_class_that_is_no_wrapper_invents_nothing() {
    // `(*it)->m()` on an iterator: its `operator*` yields something the index
    // does not follow, so the site stays unresolved instead of a member being
    // invented on the iterator itself.
    let (program, analysis) = cpp_smart_ptr();
    assert!(must_not_have_edge(
        program,
        analysis,
        "UseIterDeref",
        "Iter::AddOutput"
    ));
    assert!(!analysis
        .call_edges
        .iter()
        .any(|e| fn_name(program, e.caller) == "UseIterDeref"));
}

#[test]
fn cpp_member_virtual_overload_filters_by_arity() {
    let root = fixture("cpp_implicit_this");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let unary: Vec<(String, usize)> = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "call_unary"
                && e.resolution == ResolutionKind::Direct
                && fn_name(&program, e.callee).ends_with("::foo")
        })
        .map(|e| {
            (
                fn_name(&program, e.callee),
                program.symbols.function(e.callee).params.len(),
            )
        })
        .collect();
    assert!(
        unary.iter().any(|(t, _)| t == "Over::foo") && unary.iter().any(|(t, _)| t == "OverD::foo"),
        "p->foo(1) should CHA to unary Over::foo / OverD::foo, got {unary:?}"
    );
    for (t, n) in &unary {
        assert_eq!(*n, 2, "{t} should be this+int, params={n}, all={unary:?}");
    }

    let binary: Vec<(String, usize)> = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "call_binary"
                && e.resolution == ResolutionKind::Direct
                && fn_name(&program, e.callee).ends_with("::foo")
        })
        .map(|e| {
            (
                fn_name(&program, e.callee),
                program.symbols.function(e.callee).params.len(),
            )
        })
        .collect();
    for (t, n) in &binary {
        assert_eq!(
            *n, 3,
            "{t} should be this+int+int, params={n}, all={binary:?}"
        );
    }
}

#[test]
fn cpp_unused_attr_on_ref_param_keeps_definition() {
    let root = fixture("cpp_implicit_this");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let sink = program
        .symbols
        .resolve_function("Sink::consume")
        .expect("Sink::consume");
    assert!(
        program.symbols.function(sink).is_defined,
        "T& param __UNUSED must remain a function_definition"
    );
}

// --- cpp_callable: lambdas, std::function, functors, fn-ptr fields ---

fn has_resolution(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee: &str,
    resolution: ResolutionKind,
) -> bool {
    analysis.call_edges.iter().any(|e| {
        fn_name(&program, e.caller) == caller
            && fn_name(&program, e.callee) == callee
            && e.resolution == resolution
    })
}

#[test]
fn cpp_fn_ptr_field_and_local_resolve_indirect() {
    let root = fixture("cpp_callable");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_resolution(
        &program,
        &analysis,
        "call_field",
        "target",
        ResolutionKind::Indirect
    ));
    assert!(has_resolution(
        &program,
        &analysis,
        "call_local",
        "target",
        ResolutionKind::Indirect
    ));
}

#[test]
fn cpp_lambda_is_addr_of_fn_and_indirect_call() {
    let root = fixture("cpp_callable");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let lambda_names: Vec<String> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name.contains("$lambda"))
        .map(|f| f.name.clone())
        .collect();
    assert!(
        !lambda_names.is_empty(),
        "lambda_expression should lower to a $lambda function"
    );
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("$lambda")
                && fn_name(&program, e.callee) == "target"
        }),
        "lambda body should call target, lambdas={lambda_names:?}"
    );
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller) == "call_lambda"
                && fn_name(&program, e.callee).contains("$lambda")
                && e.resolution == ResolutionKind::Indirect
        }),
        "g() should be an indirect call to the lambda"
    );
}

#[test]
fn cpp_lambda_captures_resolve() {
    let root = fixture("cpp_lambda_captures");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // 1. Explicit by-value capture [f1] calling target1
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_explicit_val::$lambda")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }),
        "explicitly captured [f1] should call target1"
    );

    // 2. Explicit by-reference capture [&f2] calling target2
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_explicit_ref::$lambda")
                && fn_name(&program, e.callee) == "target2"
                && e.resolution == ResolutionKind::Indirect
        }),
        "explicitly captured [&f2] should call target2"
    );

    // 3. Default by-reference capture [&] calling target3
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_default_ref::$lambda")
                && fn_name(&program, e.callee) == "target3"
                && e.resolution == ResolutionKind::Indirect
        }),
        "default-captured [&] should call target3"
    );

    // 4. Default by-value capture [=] calling target4
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_default_val::$lambda")
                && fn_name(&program, e.callee) == "target4"
                && e.resolution == ResolutionKind::Indirect
        }),
        "default-captured [=] should call target4"
    );

    // 5. Mixed: default ref, except f1 by value [&, f1]
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_mixed_ref_val::$lambda")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }) && analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_mixed_ref_val::$lambda")
                && fn_name(&program, e.callee) == "target2"
                && e.resolution == ResolutionKind::Indirect
        }),
        "mixed capture [&, f1] should call target1 and target2"
    );

    // 6. Mixed: default val, except f2 by ref [=, &f2]
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_mixed_val_ref::$lambda")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }) && analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_mixed_val_ref::$lambda")
                && fn_name(&program, e.callee) == "target2"
                && e.resolution == ResolutionKind::Indirect
        }),
        "mixed capture [=, &f2] should call target1 and target2"
    );

    // 7. Init-capture [cb = f5] calling target5
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_init_capture::$lambda")
                && fn_name(&program, e.callee) == "target5"
                && e.resolution == ResolutionKind::Indirect
        }),
        "init-captured [cb = f5] should call target5"
    );

    // 8. [this] in Derived calling Derived::derivedAction and inherited Base::baseAction
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Derived::testThisCapture::$lambda")
                && fn_name(&program, e.callee) == "Derived::derivedAction"
                && e.resolution == ResolutionKind::Direct
        }),
        "[this] capture should call Derived::derivedAction"
    );
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Derived::testThisCapture::$lambda")
                && fn_name(&program, e.callee) == "Base::baseAction"
                && e.resolution == ResolutionKind::Direct
        }),
        "[this] capture should call Base::baseAction"
    );

    // 9. [*this] in Derived calling Derived::derivedAction
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Derived::testStarThisCapture::$lambda")
                && fn_name(&program, e.callee) == "Derived::derivedAction"
                && e.resolution == ResolutionKind::Direct
        }),
        "[*this] capture should call Derived::derivedAction"
    );

    // 10. [&] in Derived method capturing this
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Derived::testDefaultCapturesThis::$lambda")
                && fn_name(&program, e.callee) == "Derived::derivedAction"
                && e.resolution == ResolutionKind::Direct
        }),
        "[&] default capture in member method should capture this and call Derived::derivedAction"
    );

    // 11. [] non-capturing lambda inside member method calling global target8
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Derived::testNoCapture::$lambda")
                && fn_name(&program, e.callee) == "target8"
                && e.resolution == ResolutionKind::Direct
        }),
        "non-capturing lambda should call global target8"
    );
    assert!(
        !analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Derived::testNoCapture::$lambda")
                && fn_name(&program, e.callee).contains("Derived::")
        }),
        "non-capturing lambda must not call any Derived member"
    );

    // Parameter shadowing: lambda param f1 shadows captured f1
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_param_shadow::$lambda")
                && fn_name(&program, e.callee) == "target2"
                && e.resolution == ResolutionKind::Indirect
        }),
        "parameter shadowing captured variable should call target2"
    );
    assert!(
        !analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_param_shadow::$lambda")
                && fn_name(&program, e.callee) == "target1"
        }),
        "parameter shadowing captured variable must not call shadowed target1"
    );

    // Nested lambdas
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_nested_lambdas::$lambda")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }),
        "nested lambda should access outer captured f1"
    );

    // Lambda stored in struct field
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller) == "test_lambda_in_struct"
                && fn_name(&program, e.callee).contains("test_lambda_in_struct::$lambda")
                && e.resolution == ResolutionKind::Indirect
        }),
        "call via struct field should invoke lambda"
    );
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_lambda_in_struct::$lambda")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }),
        "lambda stored in struct field should call target1"
    );

    // 13. Init-capture by reference [&cb = f9] calling target9
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_init_capture_ref::$lambda")
                && fn_name(&program, e.callee) == "target9"
                && e.resolution == ResolutionKind::Indirect
        }),
        "reference init-capture [&cb = f9] should call target9"
    );

    // 14. Captured object calling method Worker::doWork
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_captured_object::$lambda")
                && fn_name(&program, e.callee) == "Worker::doWork"
                && e.resolution == ResolutionKind::Direct
        }),
        "captured object by ref should call Worker::doWork"
    );

    // 15. Captured pointer calling method Worker::doWork
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_captured_pointer::$lambda")
                && fn_name(&program, e.callee) == "Worker::doWork"
                && e.resolution == ResolutionKind::Direct
        }),
        "captured pointer should call Worker::doWork"
    );

    // 16. Captured variable passed as argument to helper_call
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_captured_arg_pass::$lambda")
                && fn_name(&program, e.callee) == "helper_call"
        }),
        "captured variable passed to helper_call should record call edge"
    );
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller) == "helper_call"
                && fn_name(&program, e.callee) == "target11"
                && e.resolution == ResolutionKind::Indirect
        }),
        "helper_call receiving captured function pointer should call target11"
    );

    // 17. Service class member access and calls inside lambda
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Service::testMemberAccess::$lambda")
                && fn_name(&program, e.callee) == "Service::process"
        }),
        "lambda capturing this should call Service::process"
    );
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("Service::testMemberAccess::$lambda")
                && fn_name(&program, e.callee) == "Worker::doWork"
        }),
        "lambda capturing this should call member worker.doWork"
    );

    // 18. Lambda returning a function pointer
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller) == "test_lambda_returns_fn"
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }),
        "caller of lambda returning function pointer should resolve call to target1"
    );

    // 19. Multiple captures list [f1, &f2, f3, &f4]
    for target in &["target1", "target2", "target3", "target4"] {
        assert!(
            analysis.call_edges.iter().any(|e| {
                fn_name(&program, e.caller).contains("test_multi_captures::$lambda")
                    && fn_name(&program, e.callee) == *target
                    && e.resolution == ResolutionKind::Indirect
            }),
            "multi-capture list should resolve call to {}",
            target
        );
    }

    // 20. Mutable lambda calling target1
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_mutable_lambda::$lambda")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }),
        "mutable lambda should call target1"
    );

    // 21. Multi-level nested lambdas (outer -> mid -> inner)
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_multi_nested_lambdas")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }),
        "deeply nested lambda should resolve call to target1"
    );

    // 22. Lexical class lookup for captureless lambda calling static member
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("LexicalClass::run::$lambda")
                && fn_name(&program, e.callee) == "LexicalClass::hit"
                && e.resolution == ResolutionKind::Direct
        }),
        "captureless lambda inside member method should resolve static member LexicalClass::hit"
    );

    // 23. Write through reference init-capture
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller) == "test_ref_init_capture_write"
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }) && analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller) == "test_ref_init_capture_write"
                && fn_name(&program, e.callee) == "target2"
                && e.resolution == ResolutionKind::Indirect
        }),
        "write through reference init-capture should update outer variable points-to set to target1 and target2"
    );

    // 24. Repeated callable invocations preserving distinct return destinations
    let repeated_edges: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "test_repeated_call_returns"
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        })
        .collect();
    assert_eq!(
        repeated_edges.len(),
        2,
        "both distinct invocations a() and b() should resolve to target1"
    );

    // 25. Reference init-capture of struct field [&cb = h.cb]
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(&program, e.caller).contains("test_ref_init_capture_field::$lambda")
                && fn_name(&program, e.callee) == "target1"
                && e.resolution == ResolutionKind::Indirect
        }),
        "reference init-capture of struct field [&cb = h.cb] should call target1"
    );
}

#[test]
fn cpp_functor_operator_call_resolves() {
    let root = fixture("cpp_callable");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_direct(&program, &analysis, "call_functor", "Fn::operator()"),
        "f() on a functor should target operator()"
    );
    assert!(
        has_direct(&program, &analysis, "call_functor_field", "Fn::operator()"),
        "w->cb() when cb is a functor field should target operator()"
    );
    assert!(has_direct(&program, &analysis, "Fn::operator()", "target"));
    assert!(
        has_direct(
            &program,
            &analysis,
            "call_bare_function_type",
            "function::operator()"
        ),
        "a class named function (not std::function) should still be a functor"
    );
}

#[test]
fn cpp_std_function_resolves_like_fn_ptr() {
    let root = fixture("cpp_callable");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_resolution(
            &program,
            &analysis,
            "call_std_function",
            "target",
            ResolutionKind::Indirect
        ),
        "std::function local assigned a function should call it indirectly"
    );
    assert!(
        has_resolution(
            &program,
            &analysis,
            "call_std_field",
            "target",
            ResolutionKind::Indirect
        ),
        "std::function field call should resolve like a fn-ptr field"
    );
}

#[test]
fn cpp_qualified_undeclared_becomes_external() {
    let root = fixture("cpp_callable");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_resolution(
            &program,
            &analysis,
            "check_exists",
            "FileUtil::Exists",
            ResolutionKind::External
        ),
        "qualified FileUtil::Exists prototype should be an external edge, not unresolved indirect"
    );
}

// --- cpp_flow: cross-language C dispatcher + C++ impl (HDF sbuf pattern) ---

#[test]
fn cpp_impl_registered_into_c_ops_table_resolves_indirect() {
    let root = fixture("cpp_flow");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for target in ["RawImplRead", "MParcelImplRead"] {
        let hits = edges_to(
            &program,
            &analysis,
            "Read",
            target,
            ResolutionKind::Indirect,
        );
        assert_eq!(
            hits.len(),
            1,
            "{target} must be an indirect target of s->impl->read exactly once"
        );
    }
}

// --- cpp_dispatch: virtual inheritance + final class/method ---

fn cpp_direct_set(program: &Program, analysis: &AnalysisResult, caller: &str) -> Vec<String> {
    let mut v = direct_targets(program, analysis, caller);
    v.sort();
    v.dedup();
    v
}

#[test]
fn cpp_virtual_inheritance_diamond_resolves_overrides() {
    let root = fixture("cpp_dispatch");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        program.bases_of("Left").iter().any(|b| b == "VBase"),
        "virtual base Left : virtual VBase must be recorded"
    );
    assert!(
        program.bases_of("Right").iter().any(|b| b == "VBase"),
        "virtual base Right : virtual VBase must be recorded"
    );
    let hits = cpp_direct_set(&program, &analysis, "diamond_drive");
    assert!(
        hits.iter().any(|t| t == "VBase::id"),
        "diamond through VBase* should include VBase::id, got {hits:?}"
    );
    assert!(
        hits.iter().any(|t| t == "Left::id"),
        "diamond through VBase* should include Left::id, got {hits:?}"
    );
    assert!(
        hits.iter().any(|t| t == "Diamond::id"),
        "diamond through VBase* should include Diamond::id, got {hits:?}"
    );
}

#[test]
fn cpp_final_class_devirtualizes_receiver() {
    let root = fixture("cpp_dispatch");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(
        program.class_is_final("Sealed"),
        "class Sealed final must be recorded"
    );
    let sealed = cpp_direct_set(&program, &analysis, "sealed_drive");
    assert_eq!(
        sealed,
        vec!["Sealed::f".to_string()],
        "Sealed* is final: only Sealed::f, not OpenSib::f"
    );
    let open = cpp_direct_set(&program, &analysis, "open_drive");
    assert!(open.iter().any(|t| t == "Open::f"), "got {open:?}");
    assert!(open.iter().any(|t| t == "Sealed::f"), "got {open:?}");
    assert!(open.iter().any(|t| t == "OpenSib::f"), "got {open:?}");
}

#[test]
fn cpp_final_method_stops_further_overrides() {
    let root = fixture("cpp_dispatch");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let mid_fn = program
        .symbols
        .resolve_function("MMid::g")
        .expect("MMid::g");
    assert!(
        program.symbols.function(mid_fn).is_final,
        "int g() final must set is_final"
    );
    let mid = cpp_direct_set(&program, &analysis, "mid_drive");
    assert_eq!(
        mid,
        vec!["MMid::g".to_string()],
        "MMid* with g() final is a unique target"
    );
    let base = cpp_direct_set(&program, &analysis, "mbase_drive");
    assert!(base.iter().any(|t| t == "MBase::g"), "got {base:?}");
    assert!(base.iter().any(|t| t == "MMid::g"), "got {base:?}");
    assert!(
        !base.iter().any(|t| t.contains("MLeaf")),
        "final method must not pick up MLeaf, got {base:?}"
    );
}

// --- cpp_extern_c_driver: C caller + C++ `extern "C"` heap/ops registration ---

#[test]
fn cpp_extern_c_driver_resolves_ipc_and_dispatch() {
    let root = fixture("cpp_extern_c_driver");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    assert!(has_direct(
        &program,
        &analysis,
        "test_ipc_read",
        "SbufObtainIpc"
    ));
    assert!(has_resolution(
        &program,
        &analysis,
        "test_ipc_read",
        "MParcelReadBuffer",
        ResolutionKind::Indirect
    ));
    assert!(has_direct(
        &program,
        &analysis,
        "test_ipc_dispatch",
        "GetServiceOps"
    ));
    assert!(has_resolution(
        &program,
        &analysis,
        "test_ipc_dispatch",
        "ServiceDispatch",
        ResolutionKind::Indirect
    ));
}

// --- cpp_templates_overloads: scalar-type overload resolution, template
// member calls with explicit arguments, in-class template methods ---

/// Param type descriptors of a function, in signature order.
fn fn_param_descs(program: &Program, id: FnId) -> Vec<String> {
    program
        .symbols
        .function(id)
        .params
        .iter()
        .map(|v| {
            let tid = program.symbols.variable(*v).type_id;
            format!("{:?}", program.types.get(tid).desc)
        })
        .collect()
}

#[test]
fn cpp_same_arity_overloads_stay_distinct_by_scalar_type() {
    let root = fixture("cpp_templates_overloads");
    let program = build_program(&root, &default_opts(&root)).expect("build");

    let candidates = program.symbols.resolve_function_candidates("f", None);
    let sigs: Vec<Vec<String>> = candidates
        .iter()
        .map(|&f| fn_param_descs(&program, f))
        .collect();
    assert!(
        sigs.contains(&vec!["Int".to_string()]),
        "f(int) must survive as its own overload, got {sigs:?}"
    );
    assert!(
        sigs.contains(&vec!["Double".to_string()]),
        "f(double) must survive as its own overload, got {sigs:?}"
    );
    assert!(
        sigs.contains(&vec!["Short".to_string()]),
        "f(short) must survive as its own overload, got {sigs:?}"
    );
    assert!(
        sigs.contains(&vec!["Int".to_string(), "Int".to_string()]),
        "f(int, int) must survive as its own overload, got {sigs:?}"
    );
    let distinct: std::collections::HashSet<_> = sigs.iter().cloned().collect();
    assert_eq!(distinct.len(), 4, "all four signatures distinct: {sigs:?}");
}

#[test]
fn cpp_call_sites_prefer_exact_scalar_match() {
    let root = fixture("cpp_templates_overloads");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for (lit_call, descs) in [
        ("f(1)", vec!["Int"]),
        ("f(1.5)", vec!["Double"]),
        ("f(s)", vec!["Short"]),
        ("f(1, 2)", vec!["Int", "Int"]),
    ] {
        let matching: Vec<FnId> = analysis
            .call_edges
            .iter()
            .filter(|e| {
                fn_name(&program, e.caller) == "main"
                    && fn_name(&program, e.callee) == "f"
                    && e.resolution == ResolutionKind::Direct
                    && fn_param_descs(&program, e.callee) == descs
            })
            .map(|e| e.callee)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "{lit_call} must pick exactly one overload with {descs:?}, got {matching:?}"
        );
    }

    // No call site may emit more than one edge: the type-resolved overload is
    // unambiguous rather than the may-tie set.
    let multi = analysis.call_edges.iter().any(|e| {
        analysis
            .call_edges
            .iter()
            .filter(|e2| e2.call_site == e.call_site)
            .count()
            > 1
    });
    assert!(
        !multi,
        "type-resolved overloads must emit one site per call"
    );
}

#[test]
fn cpp_template_member_calls_resolve_to_primary_name() {
    let root = fixture("cpp_templates_overloads");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // Template primary registrations exist.
    let candidates = program
        .symbols
        .resolve_function_candidates("FieldValue::GetNumber", None);
    assert_eq!(
        candidates.len(),
        3,
        "in-class template GetNumber must register alongside its overloads"
    );

    // `fv.GetNumber<int>()` and `b.read<short>()` resolve directly.
    assert!(
        has_direct(&program, &analysis, "main", "FieldValue::GetNumber"),
        "fv.GetNumber<int>() must resolve to FieldValue::GetNumber"
    );
    assert!(
        has_direct(&program, &analysis, "main", "Box::read"),
        "b.read<short>() must resolve to Box::read"
    );
    assert!(
        has_direct(&program, &analysis, "main", "Box::read")
            && has_direct(&program, &analysis, "main", "FieldValue::GetNumber"),
        "template member calls must be direct, not external stubs"
    );
}

#[test]
fn cpp_pointer_casts_rank_against_pointer_overloads() {
    let root = fixture("cpp_pointer_cast_overloads");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let sig_count = |name: &str, sigs: Vec<String>| {
        analysis
            .call_edges
            .iter()
            .filter(|e| {
                fn_name(&program, e.caller) == "main" && e.resolution == ResolutionKind::Direct
            })
            .filter(|e| fn_name(&program, e.callee) == name)
            .filter(|e| fn_param_descs(&program, e.callee) == sigs)
            .count()
    };
    let int = vec!["Int".to_string()];
    let ptr_int = vec!["Ptr(Int)".to_string()];
    let ch = vec!["Char".to_string()];
    let ptr_ch = vec!["Ptr(Char)".to_string()];
    let ptr_ptr_int = vec!["Ptr(Ptr(Int))".to_string()];

    assert_eq!(
        sig_count("f", int.clone()),
        1,
        "f(i) must pick exactly f(int)"
    );
    assert_eq!(
        sig_count("f", ptr_int),
        2,
        "f((int*)&i) and f(pi) must resolve to f(int*), not f(int)"
    );
    assert_eq!(
        sig_count("f", ch.clone()),
        1,
        "f(c) must pick f(char), not f(char*)"
    );
    assert_eq!(
        sig_count("f", ptr_ch),
        2,
        "f((char*)&c) and f(pc) must resolve to f(char*)"
    );
    assert_eq!(
        sig_count("f", ptr_ptr_int.clone()),
        2,
        "f((int**)&pi) and f(pp) must resolve to f(int**), not one pointer level short"
    );
    let f_direct_total = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "main"
                && fn_name(&program, e.callee) == "f"
                && e.resolution == ResolutionKind::Direct
        })
        .count();
    assert_eq!(
        f_direct_total, 8,
        "all eight f() call sites must resolve to exactly one callee each"
    );
}

#[test]
fn cpp_unresolvable_member_args_keep_full_candidate_set() {
    let root = fixture("cpp_pointer_cast_overloads");
    let program = build_program(&root, &default_opts(&root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    // `g(gh.val)` and `g(hp->val)` cannot be ranked past the receiver
    // (struct or pointer-to-struct), so BOTH the int and the Holder overload
    // stay for each member call (may-approximation) — five edges total:
    // g(42) -> g(int) only, plus two member calls each keeping both.
    let g_targets: Vec<Vec<String>> = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "main"
                && fn_name(&program, e.callee) == "g"
                && e.resolution == ResolutionKind::Direct
        })
        .map(|e| fn_param_descs(&program, e.callee))
        .collect();
    assert_eq!(
        g_targets.len(),
        5,
        "g(42) + g(gh.val) + g(hp->val) must contribute 1 + 2 + 2 edges, got {g_targets:?}"
    );
    let mut seen: Vec<Vec<String>> = g_targets.clone();
    seen.sort();
    seen.dedup();
    assert!(
        seen.contains(&vec!["Int".to_string()]),
        "g(int) must be present, got {g_targets:?}"
    );
    assert!(
        seen.iter()
            .any(|s| !s.is_empty() && s[0].starts_with("Struct")),
        "g(Holder) must be among the kept candidates (both receiver shapes), got {g_targets:?}"
    );
}

// --- cpp_name_lookup: ADL, using directives, namespace-relative lookup ---

fn cpp_name_lookup() -> (Program, trace_analysis::AnalysisResult) {
    static SHARED: OnceLock<(Program, trace_analysis::AnalysisResult)> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let root = fixture("cpp_name_lookup");
            let program = build_program(&root, &default_opts(&root)).expect("build");
            let (_pag, analysis) = analyze(&program);
            (program, analysis)
        })
        .clone()
}

#[test]
fn cpp_adl_free_function_resolves() {
    let (program, analysis) = cpp_name_lookup();
    // `swap(_a, _b)` at global scope with `kit::Widget*` args: ADL finds
    // `kit::swap`. It must be a direct in-tree edge, not an external stub.
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_drive",
            "kit::swap",
            ResolutionKind::Direct
        ),
        "ADL swap(kit::Widget*) must resolve to kit::swap"
    );
    assert!(
        !program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "swap" && !f.is_defined),
        "bare 'swap' must not survive as an undefined external stub"
    );
}

#[test]
fn cpp_using_namespace_resolves_free_functions() {
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "using_ns_drive",
            "util::helper",
            ResolutionKind::Direct
        ),
        "using namespace util; helper() must resolve"
    );
    assert!(
        has_resolution(
            &program,
            &analysis,
            "using_ns_drive",
            "util::twice",
            ResolutionKind::Direct
        ),
        "using namespace util; twice(3) must resolve"
    );
}

#[test]
fn cpp_using_member_import_resolves() {
    let (program, analysis) = cpp_name_lookup();
    // `using lib::bump;` imports the exact qualified function.
    assert!(
        has_resolution(
            &program,
            &analysis,
            "using_member_drive",
            "lib::bump",
            ResolutionKind::Direct
        ),
        "using lib::bump; bump(c) must resolve to the imported function"
    );
}

#[test]
fn cpp_using_import_of_static_resolves_internal_linkage() {
    let (program, analysis) = cpp_name_lookup();
    // `using import_static::only;` + `only(1)` must resolve to the file-local
    // static `import_static::only(int)` (internal linkage), not degrade to
    // the global external/overload or an external stub.
    assert!(
        has_resolution(
            &program,
            &analysis,
            "using_static_drive",
            "import_static::only",
            ResolutionKind::Direct
        ),
        "using import_static::only; only(1) must resolve to the static definition"
    );
}

#[test]
fn cpp_namespace_relative_call_resolves() {
    let (program, analysis) = cpp_name_lookup();
    // From inside `a::b`, bare `clamp` finds the innermost `a::b::clamp`.
    assert!(
        has_resolution(
            &program,
            &analysis,
            "a::b::go",
            "a::b::clamp",
            ResolutionKind::Direct
        ),
        "bare clamp() inside a::b must resolve to a::b::clamp"
    );
}

#[test]
fn cpp_qualified_call_unchanged() {
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "qualified_drive",
            "util::helper",
            ResolutionKind::Direct
        ),
        "util::helper() must still resolve explicitly"
    );
    assert!(has_resolution(
        &program,
        &analysis,
        "qualified_drive",
        "util::twice",
        ResolutionKind::Direct
    ));
}

#[test]
fn cpp_header_prototypes_register_qualified_names() {
    // Header-declared `void swap(Widget*, Widget*)` inside `namespace kit`
    // must register as `kit::swap` (not bare `swap`), so it folds into the
    // out-of-line definition and ADL resolves exactly once.
    let (program, _) = cpp_name_lookup();
    let proto = program.symbols.functions_named("kit::swap");
    assert!(
        proto
            .iter()
            .any(|&f| program.symbols.function(f).is_defined),
        "kit::swap must have its in-tree definition registered"
    );
    // The header must not leave a bare `swap` *external stub* — the whole
    // point of qualifying prototypes. (A deliberate global `swap`
    // definition in main.cpp is fine and expected.)
    assert!(
        !program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "swap" && !f.is_defined),
        "the header must not produce an undefined bare 'swap' external stub"
    );
}

// --- additional name-lookup edge cases ---

#[test]
fn cpp_adl_may_approx_keeps_global_overload() {
    // A global `swap(Widget*, Widget*)` and `kit::swap(Widget*, Widget*)`
    // share base name + arity. Under may-analysis the bare `swap(_a, _b)`
    // call must keep BOTH candidates (global + ADL namespace), never
    // collapse to a single wrong target.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_may_approx",
            "swap",
            ResolutionKind::Direct
        ),
        "global ::swap must remain a candidate"
    );
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_may_approx",
            "kit::swap",
            ResolutionKind::Direct
        ),
        "ADL kit::swap must remain a candidate"
    );
    assert!(
        !has_resolution(
            &program,
            &analysis,
            "adl_may_approx",
            "swap",
            ResolutionKind::External
        ),
        "both candidates are defined in-tree; neither may degrade to external"
    );
}

#[test]
fn cpp_using_nested_member_import_resolves() {
    // `using deep::inner::fold;` — a *nested* qualified import that no
    // ordinary/ADL namespace covers.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_nested_import",
            "deep::inner::fold",
            ResolutionKind::Direct
        ),
        "using deep::inner::fold must resolve the nested import"
    );
}

#[test]
fn cpp_file_static_shadows_adl() {
    // A file-scope `static void shadowed(int)` must resolve ahead of any
    // global/ADL candidate of the same base name (internal linkage wins).
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_static_shadow",
            "shadowed",
            ResolutionKind::Direct
        ),
        "file-local static shadowed() must resolve"
    );
    assert!(
        !program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "shadowed" && !f.is_defined),
        "static shadowed must not leave an external stub"
    );
}

#[test]
fn cpp_function_scoped_using_namespace_resolves() {
    // `using namespace body;` inside a function body must make `poke()`
    // resolvable only for that function.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_function_scoped_using",
            "body::poke",
            ResolutionKind::Direct
        ),
        "function-scoped using namespace body; poke() must resolve"
    );
}

#[test]
fn cpp_function_scoped_using_namespace_does_not_leak() {
    // The `using namespace body;` inside `adl_function_scoped_using` must NOT
    // make `body::poke` a candidate in `adl_using_no_leak` — a leaked
    // directive would rob the correct in-scope global `poke` edge when the
    // ranking later collapses to one candidate (under-approximation).
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_using_no_leak",
            "poke",
            ResolutionKind::Direct
        ),
        "global poke must resolve for a caller without the using directive"
    );
    assert!(
        !has_resolution(
            &program,
            &analysis,
            "adl_using_no_leak",
            "body::poke",
            ResolutionKind::Direct
        ),
        "function-body using namespace must not leak into other functions"
    );
}

#[test]
fn cpp_relative_using_namespace_target_finds_enclosing_namespace() {
    // `using namespace detail;` is written inside `relns::via_directive`
    // while an *enclosing* `relns::detail` namespace exists. C++ resolves
    // the relative first segment to the enclosing namespace, so
    // `drive_ns`'s bare `bump(1)` must reach `relns::detail::bump` (and may
    // over-approximate the global `detail::bump` too; it must not miss the
    // enclosing one).
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "relns::directive_host::user::drive_ns",
            "relns::detail::bump",
            ResolutionKind::Direct
        ),
        "relative using-namespace target must resolve against the enclosing namespace"
    );
}

#[test]
fn cpp_relative_using_member_target_finds_enclosing_namespace() {
    // `using detail::bump;` written inside `relns::via_import` names the
    // enclosing `relns::detail::bump` (first segment resolved against the
    // namespace stack), which must end up in `drive_import`'s candidate set
    // — not just the global-spelled `detail::bump`.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "relns::import_host::user::drive_import",
            "relns::detail::bump",
            ResolutionKind::Direct
        ),
        "relative using-declaration target must resolve against the enclosing namespace"
    );
}

#[test]
fn cpp_global_qualified_definition_inside_namespace_block() {
    // `void ::qualified_global() {}` written inside `namespace global_block`
    // registers at global scope under the normalized name `qualified_global`
    // (leading `::` stripped by `qualify_decl` so that merge dedup works and
    // `functions_in_namespace` needs only one comparison).  The enclosing
    // namespace prefix must NOT be prepended.
    // `global_block::caller`'s bare call must reach the global function.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "global_block::caller",
            "qualified_global",
            ResolutionKind::Direct
        ),
        "::global definition inside a namespace block must stay at global scope"
    );
}

#[test]
fn cpp_namespace_scoped_using_namespace_applies_inside_block_only() {
    // `using namespace boost_ish;` lives inside `scoped_use::inner`. It must
    // apply to `in_scope` but not leak to `scoped_use::out_of_scope` (which
    // is in the enclosing namespace, declared after the block). A TU-wide
    // leak would make `out_of_scope` bind to the better-ranking
    // `boost_ish::tick(int)` and drop the correct global `tick(double)` edge.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "scoped_use::inner::in_scope",
            "boost_ish::tick",
            ResolutionKind::Direct
        ),
        "in-scope caller must resolve through the block-scoped directive"
    );
    assert!(
        has_resolution(
            &program,
            &analysis,
            "scoped_use::out_of_scope",
            "tick",
            ResolutionKind::Direct
        ),
        "caller outside the block must fall back to the in-scope global tick"
    );
    assert!(
        !has_resolution(
            &program,
            &analysis,
            "scoped_use::out_of_scope",
            "boost_ish::tick",
            ResolutionKind::Direct
        ),
        "namespace-block using namespace must not leak into the enclosing namespace"
    );
}

#[test]
fn cpp_adl_free_function_direct_in_one_of_many_candidates() {
    // Sanity: the original ADL drive still resolves exactly through ADL with
    // the additional global overload present.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_drive",
            "kit::swap",
            ResolutionKind::Direct
        ),
        "adl_drive swap must still resolve to kit::swap"
    );
}

#[test]
fn cpp_inner_block_using_namespace_applies_inside_block_only() {
    // `using namespace innerlib;` inside the `if` body must apply only to
    // that block. Two `g()` call sites in one function: the one inside the
    // block resolves through `innerlib::g`; the sibling call after the block
    // must stay on the global `g`. A directive leaked to the whole function
    // would add `innerlib::g` to the sibling call site too (over-approx that
    // can collapse the ranking and rob the correct in-scope edge) — so
    // `innerlib::g` must appear exactly once (the in-block call).
    let (program, analysis) = cpp_name_lookup();
    let innerlib_edges = analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(&program, e.caller) == "inner_block_using_scoped"
                && fn_name(&program, e.callee) == "innerlib::g"
                && e.resolution == ResolutionKind::Direct
        })
        .count();
    assert_eq!(
        innerlib_edges, 1,
        "inner-block using namespace must not leak to the sibling call site \
         (expected exactly 1 innerlib::g edge, from the in-block call)"
    );
    assert!(
        has_resolution(
            &program,
            &analysis,
            "inner_block_using_scoped",
            "g",
            ResolutionKind::Direct
        ),
        "sibling call after the block must resolve to the global g"
    );
}

#[test]
fn cpp_adl_leading_global_scope_tag_finds_namespace() {
    // `::kit::LeadWidget` (global-scope spelling) must still derive ADL
    // namespace `kit` (the leading `::` is the global marker, not part of
    // the namespace), so the bare `lead_swap` resolves to `kit::lead_swap`.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "adl_leading_global_scope_tag",
            "kit::lead_swap",
            ResolutionKind::Direct
        ),
        "leading-:: ADL tag must resolve through ADL to kit::lead_swap"
    );
}

#[test]
fn cpp_inner_namespace_hides_global_overload() {
    // `hide::g() { f(1); }` with a global `::f(int)` and an inner
    // `hide::f(double)`. The bare name inside `hide` must resolve to
    // `hide::f` only — the global `::f` is a wrong single answer and must be
    // dropped (its presence must not be re-added by an out-of-band global
    // lookup that runs ahead of the hiding walk).
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "hide::g",
            "hide::f",
            ResolutionKind::Direct
        ),
        "inner-namespace declaration must shadow the global overload"
    );
    assert!(
        !has_resolution(&program, &analysis, "hide::g", "f", ResolutionKind::Direct),
        "global f(int) must be hidden by hide::f, not kept as a candidate"
    );
}

#[test]
fn cpp_inner_namespace_hides_global_static() {
    // `hidesf::g() { sf(1); }` with a global file-scope `static sf(int)` and
    // an inner `hidesf::sf(double)`. The nested namespace declaration must
    // shadow the file-static, resolving to `hidesf::sf` only — not the
    // wrong single global-static answer.
    let (program, analysis) = cpp_name_lookup();
    assert!(
        has_resolution(
            &program,
            &analysis,
            "hidesf::g",
            "hidesf::sf",
            ResolutionKind::Direct
        ),
        "inner-namespace declaration must shadow the global file-static"
    );
    assert!(
        !has_resolution(
            &program,
            &analysis,
            "hidesf::g",
            "sf",
            ResolutionKind::Direct
        ),
        "global static sf must be hidden by hidesf::sf, not kept as a candidate"
    );
}

/// Every `(function name, is_defined)` the index holds for `src`, lowered as C++.
fn member_entries(tag: &str, src: &str) -> Vec<(String, bool)> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("trace_{tag}_"))
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(root.join("k.cpp"), src).unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let mut entries: Vec<(String, bool)> = program
        .symbols
        .functions
        .iter()
        .map(|f| (f.name.clone(), f.is_defined))
        .collect();
    entries.sort();
    entries
}

/// Every function name the index holds for `src`, lowered as C++.
fn member_names(tag: &str, src: &str) -> Vec<String> {
    member_entries(tag, src)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn decltype_return_type_does_not_swallow_the_member_name() {
    // Issue #29: `member_short_name` walked the whole field_declaration in
    // order and took the first `identifier` it met. In a `decltype(...)`
    // return type that identifier belongs to the *operand expression*, so
    // `decltype(*p_) Deref() const;` was indexed as the member `p_` at the
    // decltype's line and `Deref` was dropped — silently, with no
    // diagnostic, since the file parses cleanly.
    let names = member_names(
        "decltype_ret",
        "class K {\n\
         public:\n\
         \x20   decltype(*p_) Deref() const;\n\
         \x20   decltype(kSize) Sized() const;\n\
         \x20   int Plain() const;\n\
         \x20   int *p_;\n\
         };\n",
    );
    assert!(
        names.iter().any(|n| n == "K::Deref"),
        "decltype-returning member must be indexed: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "K::Sized"),
        "a decltype over a plain identifier too: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "K::p_" || n == "K::kSize"),
        "the decltype operand must not be indexed as the member: {names:?}"
    );
    assert!(names.iter().any(|n| n == "K::Plain"), "{names:?}");
}

#[test]
fn conversion_operator_is_indexed_under_its_operator_name() {
    // Issue #46: tree-sitter-cpp spells `operator T()` as an `operator_cast`
    // declarator, not an `operator_name`. Neither `member_decl_is_function`
    // nor `member_short_name` knew that kind, so the *declaration* was never
    // registered and the in-class *definition* fell through to a generic
    // walk that produced `Handle::()const` — a name no call site can match
    // and that reads like a real symbol in the `functions` table.
    let names = member_names(
        "conv_op",
        "class Handle {\n\
         public:\n\
         \x20   operator int() const;\n\
         \x20   operator bool() const { return true; }\n\
         \x20   explicit operator double() { return 0; }\n\
         \x20   operator const char *() const;\n\
         \x20   Handle &operator=(const Handle &);\n\
         \x20   int Plain() const;\n\
         };\n",
    );
    for expected in [
        "Handle::operator int",
        "Handle::operator bool",
        "Handle::operator double",
        "Handle::operator const char*",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "conversion operator must be indexed as `{expected}`: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n.contains('(')),
        "no member may be indexed under a declarator fragment: {names:?}"
    );
    assert!(names.iter().any(|n| n == "Handle::operator="), "{names:?}");
    assert!(names.iter().any(|n| n == "Handle::Plain"), "{names:?}");
}

#[test]
fn out_of_class_conversion_operator_definition_merges_with_its_declaration() {
    // The `Cls::operator T` spelling of an out-of-class definition must match
    // the in-class declaration's, or the class gains a second, undefined
    // phantom member under the same construct.
    let entries = member_entries(
        "conv_op_out_of_class",
        "class Handle {\n\
         public:\n\
         \x20   operator int() const;\n\
         \x20   int Plain() const;\n\
         };\n\
         Handle::operator int() const { return 1; }\n\
         int Handle::Plain() const { return 0; }\n",
    );
    let conv: Vec<&(String, bool)> = entries
        .iter()
        .filter(|(n, _)| n == "Handle::operator int")
        .collect();
    let plain: Vec<&(String, bool)> = entries
        .iter()
        .filter(|(n, _)| n == "Handle::Plain")
        .collect();
    assert_eq!(
        conv.len(),
        plain.len(),
        "a conversion operator must merge exactly like a plain method: {entries:?}"
    );
    assert!(
        conv.iter().any(|(_, defined)| *defined),
        "the out-of-class definition must mark the member defined: {entries:?}"
    );
}

#[test]
fn operator_names_containing_an_angle_bracket_survive() {
    // `normalize_qualified` strips balanced `<...>` argument spans, which is
    // right for `Box<int>` and wrong for `operator<`: the whole family
    // truncated to the bare keyword `operator`, so `<`, `<=` and `<<`
    // collided under one name and the two that were declarations were
    // dropped outright by the `short == "operator"` guard.
    let names = member_names(
        "angle_operators",
        "struct A {\n\
         \x20   bool operator<(const A &) const;\n\
         \x20   bool operator<=(const A &) const;\n\
         \x20   A &operator<<(int) { return *this; }\n\
         \x20   bool operator>(const A &) const { return true; }\n\
         };\n",
    );
    for expected in [
        "A::operator<",
        "A::operator<=",
        "A::operator<<",
        "A::operator>",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` must keep its spelling: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n == "A::operator"),
        "no member may be indexed under the bare keyword: {names:?}"
    );
}

#[test]
fn an_error_node_in_a_member_declaration_does_not_supply_the_name() {
    // Inside a class body the unknown attribute macro of
    // `an_unknown_attribute_macro_does_not_glue_the_return_type_onto_the_name`
    // recovers differently: not a fabricated `qualified_identifier` but a
    // real declarator preceded by an `ERROR` node holding the leftover type.
    // The member walk took the first `identifier` it met, which is inside
    // that ERROR — the same way a `decltype` operand once supplied the name
    // (#29). An ERROR node holds no declarator.
    let entries = member_entries(
        "error_node_member",
        "struct C { FFI_EXPORT CArr Get(long id); };\n\
         void caller(C &c) { c.Get(1); }\n",
    );
    assert!(
        entries.contains(&("C::Get".to_string(), false)),
        "the member is named by its declarator: {entries:?}"
    );
    assert!(
        !entries.iter().any(|(n, _)| n == "C::CArr"),
        "the leftover return type must not be indexed as a member: {entries:?}"
    );
}

#[test]
fn conversion_operator_target_type_matches_the_name_it_is_spelled_with() {
    // The name keeps the `(*)` of a function-pointer target, so the recorded
    // type must too — the pointer sits inside the `abstract_function_declarator`
    // and both walks have to descend into it, or name and type disagree about
    // the same declarator. A declarator nested inside that one means the
    // `(...)` belongs to the target, which is therefore a function type: a
    // bare `Ptr(Void)` here was indistinguishable from a pointer to `void`,
    // so nothing downstream could see the target as callable.
    let types = defined_return_types(
        "conv_op_target_agrees",
        "struct H { operator void (*)() const { return 0; } };\n",
    );
    assert_eq!(
        types,
        vec![(
            "H::operator void(*)".to_string(),
            trace_ir::TypeDesc::Ptr(Box::new(trace_ir::TypeDesc::FnPtr {
                ret: Box::new(trace_ir::TypeDesc::Void),
                params: Vec::new(),
            }))
        )],
        "name and target type must agree"
    );
}

#[test]
fn an_unknown_attribute_macro_does_not_glue_the_return_type_onto_the_name() {
    // `FFI_EXPORT CArrFloat32 FfiGetRange(...)` — an attribute macro the
    // preprocessor never saw a `#define` for. tree-sitter takes the macro as
    // the return type and has no rule left for the real one, so it recovers
    // by pairing type and name under a `qualified_identifier` whose `::` is
    // MISSING. Read as a real qualified name that spells the function
    // `CArrFloat32 FfiGetRange`, which no call site can match.
    let entries = member_entries(
        "unknown_attr_macro",
        "FFI_EXPORT CArrFloat32 FfiGetRange(long id) { return 0; }\n\
         void caller() { FfiGetRange(1); }\n",
    );
    assert!(
        entries.contains(&("FfiGetRange".to_string(), true)),
        "the function is named by its declarator, not by its return type: {entries:?}"
    );
    assert!(
        !entries.iter().any(|(n, _)| n.contains("CArrFloat32")),
        "the return type must not appear in any function name: {entries:?}"
    );
}

#[test]
fn an_unknown_attribute_macro_does_not_glue_the_return_type_onto_a_qualified_name() {
    // The out-of-line sibling of
    // `an_unknown_attribute_macro_does_not_glue_the_return_type_onto_the_name`.
    // When the definition's own name is qualified there is no MISSING `::` to
    // spot: tree-sitter keeps the real one and parks the leftover class
    // segment in an ERROR node, so `FFI_EXPORT void C::M()` reads as the
    // qualified name `void C::M` and the body hides behind a phantom external
    // `C::M` — which is what every call site resolves to instead.
    let entries = member_entries(
        "unknown_attr_macro_qualified",
        "struct C { void M(); };\n\
         FFI_EXPORT void C::M() { }\n\
         void caller(C &c) { c.M(); }\n",
    );
    assert!(
        entries.contains(&("C::M".to_string(), true)),
        "the definition must land on the declared member: {entries:?}"
    );
    assert!(
        !entries.iter().any(|(n, _)| n.contains("void")),
        "the return type must not appear in any function name: {entries:?}"
    );
}

#[test]
fn a_fabricated_qualification_keeps_every_scope_of_the_real_name() {
    // The ERROR node holds only the *first* segment the recovery split off;
    // the rest stays in the `name` field, so reading either half alone loses
    // the other.
    let entries = member_entries(
        "unknown_attr_macro_nested",
        "namespace A { struct B { void M(); }; }\n\
         FFI_EXPORT void A::B::M() { }\n",
    );
    assert_eq!(
        entries,
        vec![("A::B::M".to_string(), true)],
        "the definition keeps both scopes and merges with the declaration"
    );
}

#[test]
fn a_conversion_operators_name_does_not_depend_on_how_its_target_is_spelled() {
    // A class in a namespace has to name its target one way in the class body
    // and can name it another outside: `operator S` in class, `operator ns::S`
    // out of it. Naming the member after the spelling made those two members,
    // splitting the definition from its declaration on ordinary code.
    let entries = member_entries(
        "conv_op_target_spelling",
        "namespace ns {\n\
         struct S { int a; };\n\
         class Handle { public: operator S() const; };\n\
         }\n\
         ns::Handle::operator ns::S() const { return ns::S(); }\n",
    );
    let conv: Vec<&(String, bool)> = entries
        .iter()
        .filter(|(n, _)| n.contains("operator"))
        .collect();
    assert_eq!(
        conv,
        vec![&("ns::Handle::operator S".to_string(), true)],
        "both spellings name one member: {entries:?}"
    );
}

#[test]
fn a_trailing_attribute_macro_does_not_supply_the_member_name() {
    // The mirror of `an_error_node_in_a_member_declaration_does_not_supply_the_name`:
    // when the unknown macro *trails* the declarator, tree-sitter parks the
    // declarator itself in the ERROR node and leaves the macro outside it, so
    // skipping every ERROR names each member after its macro — and a class
    // whose members share one annotation (`OVERRIDE`, `GUARDED_BY`, a
    // `noexcept` spelling) collapses into a single symbol.
    let entries = member_entries(
        "trailing_attr_macro",
        "struct D {\n\
         \x20   int j() const NOEXCEPT_MACRO;\n\
         \x20   virtual int m() OVERRIDE;\n\
         \x20   void n() GUARDED(mu_);\n\
         \x20   void k();\n\
         };\n",
    );
    for expected in ["D::j", "D::m", "D::n", "D::k"] {
        assert!(
            entries.iter().any(|(n, _)| n == expected),
            "`{expected}` is named by its declarator: {entries:?}"
        );
    }
    for macro_name in ["D::NOEXCEPT_MACRO", "D::OVERRIDE", "D::GUARDED"] {
        assert!(
            !entries.iter().any(|(n, _)| n == macro_name),
            "no member may be named after its annotation macro: {entries:?}"
        );
    }
}

#[test]
fn a_conversion_operator_to_a_template_type_names_it_the_same_either_way() {
    // A conversion's target keeps its template arguments — they are part of
    // what tells one conversion in a class from another — so both spellings
    // have to reduce to the *same* argument list. They differ in scope, not
    // in arguments, which is why dropping only the member's own scopes is
    // enough to make them meet.
    let entries = member_entries(
        "conv_op_template_target",
        "namespace ns { template <class T> struct Vec { T a; }; }\n\
         namespace ns { class H { public: operator Vec<int>() const; }; }\n\
         ns::H::operator ns::Vec<int>() const { return ns::Vec<int>(); }\n",
    );
    let conv: Vec<&(String, bool)> = entries
        .iter()
        .filter(|(n, _)| n.contains("operator"))
        .collect();
    assert_eq!(
        conv,
        vec![&("ns::H::operator Vec<int>".to_string(), true)],
        "both spellings name one member: {entries:?}"
    );
}

/// The return type the index records for each *defined* function in `src`.
fn defined_return_types(tag: &str, src: &str) -> Vec<(String, trace_ir::TypeDesc)> {
    let dir = tempfile::Builder::new()
        .prefix(&format!("trace_{tag}_"))
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(root.join("k.cpp"), src).unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let mut types: Vec<(String, trace_ir::TypeDesc)> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.is_defined)
        .map(|f| {
            (
                f.name.clone(),
                program.types.get(f.return_type).desc.as_ref().clone(),
            )
        })
        .collect();
    types.sort_by(|a, b| a.0.cmp(&b.0));
    types
}

#[test]
fn conversion_operator_returns_the_type_it_converts_to() {
    // A conversion operator has no `type` field on its definition — the
    // converted-to type sits inside the `operator_cast`, with any pointer or
    // reference layers in the abstract declarator. Read from the wrong place
    // it defaulted to `int`, so `operator Payload *()` claimed to return an
    // integer and its callers' points-to sets lost the pointer.
    let types = defined_return_types(
        "conv_op_ret",
        "struct Payload { int v; };\n\
         class Handle {\n\
         public:\n\
         \x20   operator bool() const { return true; }\n\
         \x20   operator Payload *() const { return p_; }\n\
         \x20   operator Payload &() const { return *p_; }\n\
         \x20   Payload *p_;\n\
         };\n",
    );
    let ret = |name: &str| {
        types
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} is not defined: {types:?}"))
            .1
            .clone()
    };
    assert_eq!(ret("Handle::operator bool"), trace_ir::TypeDesc::Bool);
    // The prototype's return type wins the merge, so it is the prototype that
    // has to carry a real one. A plain method's in-class declaration does
    // (#64 reads it there, which is what lets `operator->` be followed), but a
    // conversion operator's does not: it has no `type` node, the converted-to
    // type sitting inside the `operator_cast` instead. Declared in the class
    // and defined out of line, it therefore still keeps the placeholder.
    let split = defined_return_types(
        "conv_op_ret_split",
        "struct Payload { int v; };\n\
         struct Split { operator Payload *() const; int Plain() const; };\n\
         Split::operator Payload *() const { return 0; }\n\
         int Split::Plain() const { return 0; }\n",
    );
    assert_eq!(
        split,
        vec![
            ("Split::Plain".to_string(), trace_ir::TypeDesc::Int),
            (
                "Split::operator Payload*".to_string(),
                trace_ir::TypeDesc::Void
            ),
        ],
        "a plain member prototype carries its return type; a conversion \
         operator's does not, and the merge keeps the placeholder"
    );

    // A reference lowers as a pointer here, as it does everywhere else.
    for name in ["Handle::operator Payload*", "Handle::operator Payload&"] {
        let desc = ret(name);
        assert!(
            matches!(
                desc.pointee(),
                Some(trace_ir::TypeDesc::Struct { name: tag, .. }) if tag == "Payload"
            ),
            "{name} converts to a pointer to Payload, got {desc:?}"
        );
    }
}

#[test]
fn conversion_operator_to_a_function_pointer_keeps_its_pointer() {
    // The `(*)` of a conversion to a function pointer sits *inside* the
    // `abstract_function_declarator`, before its parameter list — so cutting
    // the name at that declarator drops the target type wholesale and leaves
    // `operator void`, colliding with the real conversion to `void`. The
    // name ends where the declarator's own parameter list begins.
    let names = member_names(
        "conv_op_fnptr",
        "struct H {\n\
         \x20   operator void (*)() const;\n\
         \x20   operator void() const;\n\
         };\n",
    );
    assert!(
        names.iter().any(|n| n == "H::operator void(*)"),
        "the function-pointer target keeps its pointer: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "H::operator void"),
        "and stays distinct from the conversion to plain void: {names:?}"
    );
}

#[test]
fn conversion_operator_to_a_qualified_type_stays_inside_its_class() {
    // `operator ns::S()` names a member whose *target type* carries a `::`.
    // `qualify_decl` reads a `::` anywhere in a declared name as "this
    // spelling already names its own scope, leave it alone" — right for the
    // out-of-class `Cls::m()`, wrong here: the in-class definition would be
    // registered as a free function at global scope, leaving the declaration
    // it should have merged with stranded and undefined. `Handle` sits at
    // global scope, so `ns::` is none of its own and the target keeps it —
    // the member still has to end up inside its class.
    let entries = member_entries(
        "conv_op_qualified",
        "namespace ns { struct S { int a; }; }\n\
         class Handle {\n\
         public:\n\
         \x20   operator ns::S() const;\n\
         \x20   operator ns::S() { return ns::S(); }\n\
         };\n",
    );
    assert!(
        entries.contains(&("Handle::operator ns::S".to_string(), true)),
        "the in-class definition belongs to Handle and merges with the \
         declaration: {entries:?}"
    );
    assert!(
        !entries.iter().any(|(n, _)| n.starts_with("operator")),
        "no member may escape to global scope: {entries:?}"
    );
}

#[test]
fn macro_declared_conversion_operator_spells_the_same_name_as_its_definition() {
    // The lowering sees preprocessor output, where an expansion joins its
    // tokens with whitespace (`operator int ( ) const`). The name has to
    // survive that intact, or the macro-declared prototype and the
    // hand-written definition land under two different members.
    let entries = member_entries(
        "conv_op_macro",
        "#define CONVERTS_TO(T) operator T() const\n\
         class Handle {\n\
         public:\n\
         \x20   CONVERTS_TO(int);\n\
         };\n\
         Handle::operator int() const { return 0; }\n",
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(n, _)| n == "Handle::operator int")
            .collect::<Vec<_>>()
            .len(),
        1,
        "the macro-declared prototype and the definition are one member: {entries:?}"
    );
    assert!(
        entries.contains(&("Handle::operator int".to_string(), true)),
        "{entries:?}"
    );
}

#[test]
fn keyword_operator_names_keep_the_space_that_separates_their_words() {
    // `normalize_qualified` deleted *all* whitespace, which is right between
    // a name and punctuation (`~ Cls`, `A :: b` out of a macro expansion) but
    // wrong between two words: `operator new` was indexed as `operatornew`.
    let names = member_names(
        "operator_new",
        "class Pool {\n\
         public:\n\
         \x20   static void *operator new(unsigned long);\n\
         \x20   static void operator delete(void *);\n\
         };\n",
    );
    assert!(names.iter().any(|n| n == "Pool::operator new"), "{names:?}");
    assert!(
        names.iter().any(|n| n == "Pool::operator delete"),
        "{names:?}"
    );
}

#[test]
fn an_out_of_class_conversion_operator_behind_a_macro_stays_in_its_class() {
    // `EXPORT C::operator int() const {}` recovers as
    // `scope:(C) :: (ERROR "operator") name:(int)` — the same three parts as
    // the fabricated `FFI_EXPORT void C::M()`, in a different order: there
    // the ERROR holds the real class and precedes the `::`, here it holds the
    // stranded keyword and follows it, and the scope is the real class.
    // Reading them alike cut `C::` off the front, and `qualify_decl` — with
    // no `::` left to see — registered the body as a free function at global
    // scope, stranding the declaration it should have merged with.
    let entries = member_entries(
        "conv_op_out_of_class_macro",
        "struct C { operator int() const; };\n\
         EXPORT C::operator int() const { return 0; }\n",
    );
    assert_eq!(
        entries,
        vec![("C::operator int".to_string(), true)],
        "the definition belongs to C and merges with its declaration: {entries:?}"
    );
}

#[test]
fn a_conversion_operator_behind_a_macro_is_named_the_same_wherever_it_sits() {
    // The target's own qualification is dropped on this path too, or the
    // out-of-class definition and the in-class declaration are two members
    // for the reason `strip_scope_qualifiers` exists. A qualified *class*
    // nests the stranded keyword one `qualified_identifier` deeper per scope
    // it carries, out of reach of a direct-children scan.
    for (tag, src, want) in [
        (
            "conv_op_macro_qualified_target",
            "namespace ns { struct S { int a; }; }\n\
             struct C { operator ns::S() const; };\n\
             EXPORT C::operator ns::S() const { return ns::S(); }\n",
            "C::operator ns::S",
        ),
        (
            "conv_op_macro_qualified_class",
            "namespace ns { struct S { int a; };\n\
             struct C { operator S() const; }; }\n\
             EXPORT ns::C::operator ns::S() const { return ns::S(); }\n",
            "ns::C::operator S",
        ),
    ] {
        let entries = member_entries(tag, src);
        assert!(
            entries.contains(&(want.to_string(), true)),
            "`{want}` is one member, defined: {entries:?}"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|(n, _)| n.contains("operator"))
                .count(),
            1,
            "and only one: {entries:?}"
        );
    }
}

#[test]
fn a_leading_attribute_macro_does_not_name_a_conversion_operator_after_its_target() {
    // Inside a class body the macro takes the `type` field and the keyword is
    // stranded in an `ERROR` of its own, leaving the target type standing in
    // declarator position: `MACRO operator ns::S() const;` was indexed as the
    // member `C::S` — a name that collides with the class `S` itself and
    // matches no declaration of the real member. The pointer spelling needs
    // none of this, keeping a real `operator_name`, and must not regress.
    let names = member_names(
        "conv_op_leading_macro",
        "struct C {\n\
         \x20   MACRO operator int() const;\n\
         \x20   MACRO operator ns::S() const;\n\
         \x20   MACRO operator char *() const;\n\
         \x20   MACRO int Plain() const;\n\
         };\n",
    );
    for expected in [
        "C::operator int",
        // `C` sits at global scope, so `ns::` is none of its own and stays —
        // the same spelling the macro-free and out-of-class paths produce.
        "C::operator ns::S",
        "C::operator char*",
        "C::Plain",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` must be indexed: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n == "C::int" || n == "C::S"),
        "no member may be named after the type it converts to, and none may \
         lose the target's own scope on the way: {names:?}"
    );
}

#[test]
fn a_trailing_attribute_macro_does_not_supply_a_definitions_name() {
    // The mirror of `a_trailing_attribute_macro_does_not_supply_the_member_name`
    // for *definitions*. A nullary declarator is as good a call as it is a
    // declarator, so `void C::M() OVERRIDE {}` parks `C::M()` in an `ERROR`
    // and hands the `declarator` field to the macro. The definition then
    // landed on the macro: a *defined* function named `OVERRIDE` — one per
    // class that annotates a nullary member, all merging into a single
    // symbol — while `C::M` stayed undefined and its body unreachable.
    // A declarator with parameters parses fine and must not regress.
    let entries = member_entries(
        "trailing_macro_definition",
        "struct C { void M(); void N(); void P(int a); };\n\
         void C::M() OVERRIDE { }\n\
         void C::N() ACQUIRE(mu_) { }\n\
         void C::P(int a) OVERRIDE { a; }\n\
         struct D { void Q() OVERRIDE { } void R() ACQUIRE(mu_) { } };\n\
         void g() OVERRIDE { }\n",
    );
    for expected in ["C::M", "C::N", "C::P", "D::Q", "D::R", "g"] {
        assert!(
            entries.contains(&(expected.to_string(), true)),
            "`{expected}` is defined under its own name: {entries:?}"
        );
    }
    for macro_name in ["OVERRIDE", "ACQUIRE", "C::OVERRIDE", "D::OVERRIDE"] {
        assert!(
            !entries.iter().any(|(n, _)| n == macro_name),
            "no function may be named after its annotation macro: {entries:?}"
        );
    }
}

#[test]
fn a_fabricated_qualification_is_found_under_the_scopes_the_real_name_carries() {
    // `FFI_EXPORT n::S C::M() {}` — the leftover return type is itself
    // qualified, so the `ERROR` holding the real class `C` sits in the
    // *nested* `qualified_identifier`, one level down per scope either half
    // spells. Scanning only direct children missed it and indexed the
    // definition as `n::S C::M`, leaving `C::M` undefined and its body
    // unreachable — the very failure the unqualified spelling fixed.
    for (tag, src, want) in [
        (
            "fabricated_qualified_ret",
            "namespace n { struct S { int a; }; }\n\
             struct C { n::S M(); };\n\
             FFI_EXPORT n::S C::M() { return n::S(); }\n",
            "C::M",
        ),
        (
            "fabricated_qualified_both",
            "namespace n { namespace q { struct S { int a; }; } }\n\
             namespace A { struct B { n::q::S M(); }; }\n\
             FFI_EXPORT n::q::S A::B::M() { return n::q::S(); }\n",
            "A::B::M",
        ),
    ] {
        let entries = member_entries(tag, src);
        assert!(
            entries.contains(&(want.to_string(), true)),
            "`{want}` is defined under its own name: {entries:?}"
        );
        assert!(
            !entries.iter().any(|(n, _)| n.contains(' ')),
            "no name may keep the return type glued to it: {entries:?}"
        );
    }
}

#[test]
fn a_member_wearing_both_macros_is_still_named_by_its_declarator() {
    // With an unknown macro on *both* sides, tree-sitter puts the leftover
    // return type and the real declarator in the same `ERROR`
    // (`ERROR [int Get(long)]`) rather than one in it and one beside it. The
    // "does this ERROR hold a declarator?" test then said yes and the walk
    // read the whole node, taking the leftover type first: every member
    // sharing a return type collapsed into `C::int` / `C::void`, and the real
    // members survived only as externals synthesized by their call sites.
    let entries = member_entries(
        "both_attr_macros",
        "struct C {\n\
         \x20   EXPORT_API int Get(long) GUARDED_BY(mu_);\n\
         \x20   EXPORT_API void Set(int) GUARDED_BY(mu_);\n\
         };\n\
         void u(C &c) { c.Get(1); c.Set(2); }\n",
    );
    for expected in ["C::Get", "C::Set"] {
        assert!(
            entries.iter().any(|(n, _)| n == expected),
            "`{expected}` is named by its declarator: {entries:?}"
        );
    }
    assert!(
        !entries.iter().any(|(n, _)| n == "C::int" || n == "C::void"),
        "no member may be named after its return type: {entries:?}"
    );
}

#[test]
fn a_standard_attribute_does_not_supply_the_member_name() {
    // `[[nodiscard]]`, `[[gnu::pure]]` and `__attribute__((pure))` parse
    // cleanly — no ERROR anywhere — but each holds an identifier of its own
    // in front of the declaration, and the member walk took it: every
    // annotated member of a class collapsed into `H::nodiscard`. Conversion
    // operators made this reachable for the first time, since their
    // declarations only began registering with #46.
    let entries = member_entries(
        "attributed_members",
        "struct H {\n\
         \x20   [[nodiscard]] operator bool() const;\n\
         \x20   [[gnu::pure]] int Plain() const;\n\
         \x20   __attribute__((pure)) int Gnu() const;\n\
         \x20   [[maybe_unused]] int data_;\n\
         };\n\
         H::operator bool() const { return true; }\n",
    );
    assert!(
        entries.contains(&("H::operator bool".to_string(), true)),
        "the declaration merges with its out-of-line definition: {entries:?}"
    );
    for expected in ["H::Plain", "H::Gnu"] {
        assert!(
            entries.iter().any(|(n, _)| n == expected),
            "`{expected}` keeps its own name: {entries:?}"
        );
    }
    for attr in ["H::nodiscard", "H::gnu", "H::pure", "H::maybe_unused"] {
        assert!(
            !entries.iter().any(|(n, _)| n == attr),
            "no member may be named after an attribute: {entries:?}"
        );
    }
}

#[test]
fn a_conversion_operator_wearing_both_macros_is_named_by_its_target() {
    // With a macro on both sides the `ERROR` swallows the target too
    // (`ERROR [operator int() const]`), and the trailing macro is what
    // follows it — so reading the first thing after the `ERROR` named every
    // such member `C::operator GUARDED_BY`, collapsing a class's conversions
    // into one symbol. A declarator inside the `ERROR` is the target whenever
    // there is one; only its absence means the target is still to come.
    let names = member_names(
        "conv_op_both_macros",
        "struct C {\n\
         \x20   EXPORT_API operator int() const GUARDED_BY(m);\n\
         \x20   EXPORT_API operator bool() const GUARDED_BY(m);\n\
         };\n",
    );
    for expected in ["C::operator int", "C::operator bool"] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` must be indexed: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n.contains("GUARDED_BY")),
        "no member may be named after its annotation macro: {names:?}"
    );
}

#[test]
fn conversions_to_same_named_types_in_different_namespaces_stay_apart() {
    // The target type is the only thing telling one conversion in a class
    // from another, so dropping *every* scope from it put two members — and
    // two bodies — under one `C::operator S`. Only the scopes the member
    // itself sits in may go; `a` and `b` are none of `C`'s, and no spelling
    // of these declarations anywhere could have elided them.
    let entries = member_entries(
        "conv_op_rival_namespaces",
        "namespace a { struct S { int x; }; }\n\
         namespace b { struct S { int y; }; }\n\
         struct C {\n\
         \x20   operator a::S() const { return a::S(); }\n\
         \x20   operator b::S() const { return b::S(); }\n\
         };\n",
    );
    for expected in ["C::operator a::S", "C::operator b::S"] {
        assert!(
            entries.contains(&(expected.to_string(), true)),
            "`{expected}` is its own member: {entries:?}"
        );
    }
}

#[test]
fn conversions_to_one_template_with_different_arguments_stay_apart() {
    // Same reasoning for the other half of the target's spelling: stripping
    // `<...>` made `operator Vec<int>` and `operator Vec<double>` one symbol.
    // Keeping the arguments costs no merge, since a declaration and its
    // out-of-class definition differ in scope rather than in arguments.
    let entries = member_entries(
        "conv_op_rival_template_args",
        "template <class T> struct Vec { T v; };\n\
         struct C {\n\
         \x20   operator Vec<int>() const { return Vec<int>(); }\n\
         \x20   operator Vec<double>() const { return Vec<double>(); }\n\
         };\n",
    );
    for expected in ["C::operator Vec<int>", "C::operator Vec<double>"] {
        assert!(
            entries.contains(&(expected.to_string(), true)),
            "`{expected}` is its own member: {entries:?}"
        );
    }
}

#[test]
fn a_function_pointer_target_lowers_the_same_however_it_is_spelled() {
    // `operator void (*)()` and the `typedef`ed `operator FP()` name the same
    // C++ type, so they must intern the same descriptor — and it has to be a
    // function type, or nothing downstream can tell the target is callable.
    let direct = defined_return_types(
        "conv_fnptr_direct",
        "struct H { operator void (*)() { return 0; } };\n",
    );
    let aliased = defined_return_types(
        "conv_fnptr_typedef",
        "typedef void (*FP)();\nstruct H { operator FP() { return 0; } };\n",
    );
    let want = trace_ir::TypeDesc::Ptr(Box::new(trace_ir::TypeDesc::FnPtr {
        ret: Box::new(trace_ir::TypeDesc::Void),
        params: Vec::new(),
    }));
    assert_eq!(
        direct,
        vec![("H::operator void(*)".to_string(), want.clone())]
    );
    assert_eq!(aliased, vec![("H::operator FP".to_string(), want)]);
}

#[test]
fn a_conversion_target_drops_exactly_the_scopes_its_member_sits_in() {
    // The member's own scopes are what the author could have elided at the
    // in-class spelling, so those and only those come off — at whatever depth
    // they sit, and inside template arguments as well as at the top.
    for (tag, src, want) in [
        // `a::b::` — the whole enclosing chain, longest prefix first.
        (
            "conv_scope_nested",
            "namespace a { namespace b { struct S { int x; };\n\
             \x20   struct H { operator S() const; }; } }\n\
             a::b::H::operator a::b::S() const { return a::b::S(); }\n",
            "a::b::H::operator S",
        ),
        // `a::` alone — an outer scope of the member, still elidable in class.
        (
            "conv_scope_outer",
            "namespace a { struct S { int x; };\n\
             \x20   namespace b { struct H { operator S() const; }; } }\n\
             a::b::H::operator a::S() const { return a::S(); }\n",
            "a::b::H::operator S",
        ),
        // A template argument carries the same scope and loses it the same way.
        (
            "conv_scope_in_template_arg",
            "namespace ns { template <class T> struct Vec { T a; };\n\
             \x20   struct T1 { int q; };\n\
             \x20   struct H { operator Vec<T1>() const; }; }\n\
             ns::H::operator ns::Vec<ns::T1>() const { return ns::Vec<ns::T1>(); }\n",
            "ns::H::operator Vec<T1>",
        ),
    ] {
        let entries = member_entries(tag, src);
        assert!(
            entries.contains(&(want.to_string(), true)),
            "`{want}` is one member, defined: {entries:?}"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|(n, _)| n.contains("operator"))
                .count(),
            1,
            "and only one: {entries:?}"
        );
    }
}

#[test]
fn a_global_operator_new_call_still_resolves_to_a_synthesized_external() {
    // The guard deciding which unresolved callee becomes a synthesized
    // `external` rejects names containing a space — calibrated to the old
    // invariant that no name had one, which giving `operator new` its space
    // broke. Both sites lost their callee and their edge.
    let names = member_names(
        "global_operator_new",
        "typedef unsigned long size_t;\n\
         void *f(size_t n) { return ::operator new(n); }\n\
         void g(void *p) { ::operator delete(p); }\n",
    );
    for expected in ["::operator new", "::operator delete"] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` must be synthesized as an external callee: {names:?}"
        );
    }
}

#[test]
fn a_macro_annotated_destructor_is_not_filed_under_the_constructor() {
    // `MACRO ~D();` strands the `~` alone in an `ERROR` and leaves `D`
    // standing as the declarator, so the destructor was indexed as `D::D` —
    // classified a ctor, and so dropped from the override set `delete p`
    // expands over.
    let names = member_names(
        "macro_destructor",
        "struct B { MACRO virtual ~B(); virtual void f(); };\n\
         struct D : B { MACRO ~D() override; void f() override; };\n\
         void kill(B *b) { delete b; }\n",
    );
    for expected in ["B::~B", "D::~D"] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` must keep its destructor spelling: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n == "D::D"),
        "a destructor may not be filed under the constructor: {names:?}"
    );
}

#[test]
fn a_declspec_modifier_does_not_supply_the_member_name() {
    // MSVC's spelling of the attribute collapse: `__declspec(...)` parses to
    // `ms_declspec_modifier`, which the standard/GNU attribute guard missed,
    // so every annotated member of a class became one `H::dllexport`.
    let names = member_names(
        "declspec_members",
        "struct H {\n\
         \x20   __declspec(dllexport) int Alpha() const;\n\
         \x20   __declspec(dllexport) int Beta() const;\n\
         \x20   __declspec(dllexport) operator bool() const;\n\
         \x20   int Plain() const;\n\
         };\n",
    );
    for expected in ["H::Alpha", "H::Beta", "H::operator bool", "H::Plain"] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` must keep its own name: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n == "H::dllexport"),
        "no member may be named after a `__declspec`: {names:?}"
    );
}

#[test]
fn a_pointer_returning_definition_survives_a_trailing_macro() {
    // The return type's pointer wraps the declarator, so the `ERROR` holding
    // the real one sits a level below the definition — out of reach of a scan
    // over its own children, which left the body under the macro's name.
    // `Foo *GetInstance() OVERRIDE {}` is a very ordinary singleton shape.
    let entries = member_entries(
        "ptr_return_trailing_macro",
        "struct C { void *P(); char *N(); void M(); };\n\
         void *C::P() OVERRIDE { return 0; }\n\
         char *C::N() OVERRIDE { return 0; }\n\
         void C::M() OVERRIDE { }\n",
    );
    for expected in ["C::P", "C::N", "C::M"] {
        assert!(
            entries.contains(&(expected.to_string(), true)),
            "`{expected}` is defined under its own name: {entries:?}"
        );
    }
    assert!(
        !entries.iter().any(|(n, _)| n == "OVERRIDE"),
        "no definition may land on its annotation macro: {entries:?}"
    );
}

#[test]
fn a_macro_annotated_conversion_operator_keeps_the_targets_own_scope() {
    // The `ERROR` swallows the target's scope along with the keyword
    // (`ERROR [operator ns::]`), leaving only `S` on the declarator — so
    // reading the declarator alone spelled the member `D1::operator S` where
    // every other path spells it `D1::operator ns::S`, and the two never met.
    // The in-class *definition* takes the same repair, and a globally
    // qualified target reduces to the same member.
    let entries = member_entries(
        "macro_conv_keeps_scope",
        "namespace ns { struct S { int a; }; }\n\
         struct D1 { MACRO operator ns::S() const; };\n\
         struct D2 { operator ns::S() const; };\n\
         struct D3 { MACRO operator ns::S() const { return ns::S(); } };\n\
         D1::operator ns::S() const { return ns::S(); }\n",
    );
    assert!(
        entries.contains(&("D1::operator ns::S".to_string(), true)),
        "the declaration and its out-of-class definition are one member: {entries:?}"
    );
    assert!(
        entries.contains(&("D2::operator ns::S".to_string(), false)),
        "the macro-free spelling agrees: {entries:?}"
    );
    assert!(
        entries.contains(&("D3::operator ns::S".to_string(), true)),
        "and so does the in-class definition: {entries:?}"
    );
    for wrong in ["D1::operator S", "D3::S", "D3::operator S"] {
        assert!(
            !entries.iter().any(|(n, _)| n == wrong),
            "`{wrong}` loses the target's scope: {entries:?}"
        );
    }
}

#[test]
fn a_globally_qualified_conversion_target_names_the_same_member() {
    // `operator ::ns::S` is the defensive spelling of `operator ns::S`. The
    // space after the keyword is dropped before punctuation, which every
    // later step keys on, and the leading `::` matched no scope prefix — so
    // the definition stranded its declaration as an undefined phantom.
    let entries = member_entries(
        "conv_global_qualified_target",
        "namespace ns { struct S { int a; }; struct H2 { operator S() const; }; }\n\
         ns::H2::operator ::ns::S() const { return ns::S(); }\n",
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(n, _)| n.contains("operator"))
            .collect::<Vec<_>>(),
        vec![&("ns::H2::operator S".to_string(), true)],
        "one member, defined: {entries:?}"
    );
}

#[test]
fn a_conversion_to_a_template_type_survives_a_leading_macro() {
    // Recovery leaves the target's argument list on the declarator, making it
    // a `template_method` — a kind the member-vs-data test did not know, so
    // the member was read as a data field and left out of the index entirely.
    let names = member_names(
        "macro_conv_template_target",
        "template <class T> struct Vec {};\n\
         struct C { MACRO operator Vec<int>() const; void Plain(); };\n",
    );
    assert!(
        names.iter().any(|n| n.starts_with("C::operator Vec")),
        "the conversion must reach the index at all: {names:?}"
    );
    assert!(names.iter().any(|n| n == "C::Plain"), "{names:?}");
}

#[test]
fn a_global_qualifier_survives_when_the_members_namespace_shadows_the_target() {
    // A leading `::` is redundant only when what follows re-spells a scope
    // the member sits in. Dropping it unconditionally merged the conversion
    // to the *global* `S` with the one to the namespace's own `S` — two
    // types, two bodies, one symbol, which is the over-merge this whole
    // canonicalization exists to avoid.
    let entries = member_entries(
        "conv_global_shadowed",
        "struct S { int g; };\n\
         namespace n {\n\
         struct S { int i; };\n\
         struct H { operator ::S() const; operator S() const; };\n\
         }\n\
         n::H::operator ::S() const { return ::S(); }\n\
         n::H::operator n::S() const { return n::S(); }\n",
    );
    for expected in ["n::H::operator ::S", "n::H::operator S"] {
        assert!(
            entries.contains(&(expected.to_string(), true)),
            "`{expected}` is its own member, defined: {entries:?}"
        );
    }
}

#[test]
fn a_template_argument_does_not_decide_whether_the_global_qualifier_is_redundant() {
    // Whether a leading `::` is redundant is a question about the top-level
    // target alone. Deciding it from "did canonicalization change anything"
    // let a *template argument* answer it: `operator ::Vec<n::T>` lost its
    // `::` merely because `n::T` shed its scope, so the conversion to the
    // global `Vec` merged into the namespace's own `Vec` — leaving the
    // global-target declaration stranded and its body on the wrong member.
    let entries = member_entries(
        "conv_global_tmpl_arg",
        "template <class> struct Vec {};\n\
         namespace n {\n\
         template <class> struct Vec {};\n\
         struct T {};\n\
         struct H { operator ::Vec<T>(); operator Vec<T>(); };\n\
         }\n\
         n::H::operator ::Vec<n::T>() { return ::Vec<n::T>(); }\n\
         n::H::operator n::Vec<n::T>() { return n::Vec<n::T>(); }\n",
    );
    for expected in ["n::H::operator ::Vec<T>", "n::H::operator Vec<T>"] {
        assert!(
            entries.contains(&(expected.to_string(), true)),
            "`{expected}` is its own member, defined by its own definition: {entries:?}"
        );
    }
}

#[test]
fn a_nested_scope_does_not_preempt_canonicalizing_the_target_head() {
    // The member's enclosing scopes nest, so one spelling can match a longer
    // prefix in a template argument than at its head: for a member of
    // `a::b`, the argument of `a::Vec<a::b::T>` begins with `a::b::` while
    // the head begins only with `a::`. Choosing a single prefix for the
    // whole target let the argument's longer match win and stop there,
    // leaving `a::Vec<T>` — which never met the `Vec<T>` its class declares.
    let entries = member_entries(
        "conv_nested_prefix_head",
        "namespace a {\n\
         template <class> struct Vec {};\n\
         namespace b {\n\
         struct T {};\n\
         struct H { operator Vec<T>(); };\n\
         } }\n\
         a::b::H::operator a::Vec<a::b::T>() { return a::Vec<a::b::T>(); }\n",
    );
    assert!(
        entries.contains(&("a::b::H::operator Vec<T>".to_string(), true)),
        "the definition merges with its declaration: {entries:?}"
    );
    assert_eq!(
        entries
            .iter()
            .filter(|(n, _)| n.contains("operator"))
            .count(),
        1,
        "and is one member: {entries:?}"
    );
}

#[test]
fn a_macro_annotated_conversion_keeps_its_qualified_target() {
    // The macro shapes name the member from the declarator the `ERROR` parks
    // the target in, and that declarator was *walked* rather than spelled —
    // so the name came out of the target's last segment alone. A qualified
    // target lost its own scope: the annotated declaration was
    // `C::operator S` while every unannotated spelling of the same member is
    // `C::operator ns::S`, so the two never met and `S` collided with the
    // class of that name.
    let entries = member_entries(
        "conv_op_macro_qualified_target",
        "namespace ns { struct S { int x; }; }\n\
         struct C {\n\
         \x20   EXPORT_API operator ns::S() const GUARDED_BY(m);\n\
         };\n\
         ns::S C::operator ns::S() const { return ns::S(); }\n",
    );
    assert!(
        entries.contains(&("C::operator ns::S".to_string(), true)),
        "the annotated declaration must meet its definition: {entries:?}"
    );
    assert!(
        !entries.iter().any(|(n, _)| n == "C::operator S"),
        "the target's own scope may not be dropped: {entries:?}"
    );
}

#[test]
fn a_macro_annotated_conversion_keeps_its_template_arguments() {
    // Same walk, other half of the spelling: `operator Vec<int>` behind both
    // macros was named `C::operator Vec`, which merges `Vec<int>` with
    // `Vec<double>` and meets neither the plain declaration nor the
    // out-of-class definition of either.
    let entries = member_entries(
        "conv_op_macro_template_target",
        "template <class T> struct Vec { T v; };\n\
         struct C {\n\
         \x20   EXPORT_API operator Vec<int>() const GUARDED_BY(m);\n\
         \x20   EXPORT_API operator Vec<double>() const GUARDED_BY(m);\n\
         };\n\
         Vec<int> C::operator Vec<int>() const { return Vec<int>(); }\n",
    );
    assert!(
        entries.contains(&("C::operator Vec<int>".to_string(), true)),
        "the annotated declaration must meet its definition: {entries:?}"
    );
    assert!(
        entries.contains(&("C::operator Vec<double>".to_string(), false)),
        "and stay apart from the class's other conversion: {entries:?}"
    );
    assert!(
        !entries.iter().any(|(n, _)| n == "C::operator Vec"),
        "template arguments may not be dropped: {entries:?}"
    );
}

#[test]
fn a_macro_annotated_conversion_to_a_function_pointer_keeps_its_pointer() {
    // The `(*)` that makes a function-pointer target nameable sits in the
    // declarator too, and dropping it named the conversion `C::operator int`
    // — the name of the class's conversion *to* `int`, so one symbol held
    // two unrelated members. Both macro shapes must spell it the way the
    // unannotated declaration does.
    let names = member_names(
        "conv_op_macro_fn_ptr_target",
        "struct C {\n\
         \x20   EXPORT_API operator int (*)() const GUARDED_BY(m);\n\
         \x20   EXPORT_API operator int (*)(char)() const;\n\
         \x20   operator int() const;\n\
         };\n",
    );
    for expected in [
        "C::operator int(*)",
        "C::operator int(*)(char)",
        "C::operator int",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "`{expected}` is its own member: {names:?}"
        );
    }
}

#[test]
fn a_macro_trailing_a_pointer_conversion_declares_no_member() {
    // A pointer or reference target recovers differently from every other
    // kind: the operator keeps a whole `function_declarator`, the member's
    // `;` goes *missing*, and the trailing macro is parked after it as a
    // `declaration` of its own — which registered the phantom
    // `C::GUARDED_BY` that call sites on any annotated member resolve to
    // instead of the real one. A member closed by a missing `;` is one the
    // author wrote no `;` after, so what follows is the rest of it.
    for (tag, target) in [("ptr", "Payload *"), ("ref", "Payload &")] {
        let names = member_names(
            &format!("conv_op_macro_{tag}_target"),
            &format!(
                "struct Payload {{ int x; }};\n\
                 struct C {{\n\
                 \x20   EXPORT_API operator {target}() const GUARDED_BY(m);\n\
                 }};\n"
            ),
        );
        assert!(
            !names.iter().any(|n| n.contains("GUARDED_BY")),
            "no member may be named after its annotation macro: {names:?}"
        );
        assert_eq!(
            names.len(),
            1,
            "the conversion is the class's only member: {names:?}"
        );
    }
}

#[test]
fn every_conversion_target_kind_spells_one_member_under_any_macro() {
    // The four macro shapes are four separate recoveries, and the committed
    // case for the pair only ever used `int` — which is why review found the
    // other target kinds each losing a different part of the target's
    // spelling. What every shape owes is the same: a member's name must not
    // depend on how it is annotated, since the annotated declaration and the
    // plain one are the same member and have to merge.
    //
    // Excluded: a *globally* qualified target (`operator ::ns::S`) behind a
    // leading macro. That one recovery puts its `ERROR` at class-body level
    // rather than inside the member — the macro and the keyword land there
    // together and the target becomes a `declaration` beside them — so the
    // member walk never sees it and no repair here can reach it. Recorded in
    // `docs/ANALYSIS.md`.
    let prelude = "namespace ns { struct S { int x; }; template <class T> struct Vec { T v; }; }\n\
                   struct S { int y; };\n\
                   template <class T> struct Vec { T v; };\n\
                   struct P { int z; };\n";
    for target in [
        "int",
        "bool",
        "unsigned long",
        "S",
        "ns::S",
        "Vec<int>",
        "ns::Vec<int>",
        "Vec<Vec<int>>",
        "P *",
        "P &",
        "const P *",
        "const char *",
        "int (*)()",
        "int (*)(char)",
    ] {
        let mut spelled: Vec<(&str, Vec<String>)> = Vec::new();
        for (shape, decl) in [
            ("plain", format!("operator {target}() const;")),
            ("leading", format!("EXPORT_API operator {target}() const;")),
            (
                "trailing",
                format!("operator {target}() const GUARDED_BY(m);"),
            ),
            (
                "both",
                format!("EXPORT_API operator {target}() const GUARDED_BY(m);"),
            ),
        ] {
            let names = member_names(
                "conv_op_target_matrix",
                &format!("{prelude}struct C {{\n    {decl}\n}};\n"),
            );
            assert!(
                !names.iter().any(|n| n.contains("GUARDED_BY")),
                "`operator {target}` ({shape}): no member may be named after \
                 its annotation macro: {names:?}"
            );
            spelled.push((shape, names));
        }
        let (_, plain) = &spelled[0];
        assert_eq!(
            plain.len(),
            1,
            "`operator {target}` is the class's only member: {plain:?}"
        );
        for (shape, names) in &spelled[1..] {
            assert_eq!(
                names, plain,
                "`operator {target}` must spell the same member with a {shape} \
                 macro as without one"
            );
        }
    }
}

#[test]
fn cpp_default_parameters_in_prototype_retained_and_merge() {
    let dir = tempfile::Builder::new()
        .prefix("trace_default_param_")
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("api.h"),
        "int compute(int base, int multiplier = 1);\n",
    )
    .unwrap();
    std::fs::write(
        root.join("api.cpp"),
        "#include \"api.h\"\nint compute(int base, int multiplier) {\n    return base * multiplier;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.cpp"),
        "#include \"api.h\"\nint main() {\n    return compute(5);\n}\n",
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let compute = program
        .symbols
        .resolve_function("compute")
        .expect("compute resolved");
    let func = program.symbols.function(compute);
    assert!(
        func.is_defined,
        "compute must be defined (prototype and definition merged)"
    );
    assert_eq!(
        func.params.len(),
        2,
        "optional parameter must be retained as second param"
    );
    let (_pag, analysis) = analyze(&program);
    assert_eq!(
        direct_targets(&program, &analysis, "main"),
        vec!["compute"],
        "main must call compute"
    );
}

#[test]
fn a_pointer_typedef_to_a_struct_keeps_its_pointer() {
    // `typedef struct Session *SessionPtr` names a POINTER. The typedef's
    // struct branch registered the alias as the bare tag and never walked the
    // declarator, so every `SessionPtr s` was a struct VALUE: `s->fd`
    // decomposed against a non-pointer and the field edge was lost. Only the
    // one-statement form is affected -- `typedef struct S S;` followed by
    // `typedef S *P;` takes the other branch, which walks the declarator.
    let dir = tempfile::Builder::new()
        .prefix("trace_ptr_typedef_")
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("a.c"),
        "struct Session { int fd; };\n\
         typedef struct Session *SessionPtr;\n\
         int take(SessionPtr s) { return s->fd; }\n",
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");

    let take = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "take")
        .expect("take is indexed");
    let param = take.params.first().copied().expect("take has a parameter");
    let ty = program
        .symbols
        .variable_by_id(param)
        .map(|v| v.type_id)
        .expect("the parameter has a type");
    assert!(
        matches!(
            program.types.get(ty).desc.as_ref(),
            trace_ir::TypeDesc::Ptr(_)
        ),
        "SessionPtr is a pointer, got {:?}",
        program.types.get(ty).desc
    );
}

#[test]
fn c_caller_reaches_a_cpp_extern_c_definition_across_units() {
    // The shape reported in #83, end to end: a `.c` caller and a `.cpp`
    // definition meet at one `extern "C"` prototype, and the tag in that
    // prototype's parameter is complete in the C unit and opaque in the C++
    // one. Each unit interns its own `TypeId` for it, so comparing the cached
    // signature by id refused the merge, `dispatch` stayed split into a
    // prototype and a body, and the caller kept an `external` edge to the
    // prototype -- exactly `hdf_remote_service.c:68` failing to reach
    // `hdf_remote_adapter.cpp:469`. Only the corpora and the `merge.rs` unit
    // tests covered this; `cargo test` alone did not.
    let dir = tempfile::Builder::new()
        .prefix("trace_extern_c_e2e_")
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("api.h"),
        "struct Session;\n\
         #ifdef __cplusplus\n\
         extern \"C\" {\n\
         #endif\n\
         int dispatch(struct Session *s);\n\
         #ifdef __cplusplus\n\
         }\n\
         #endif\n",
    )
    .unwrap();
    // The C unit sees the tag complete.
    std::fs::write(
        root.join("caller.c"),
        "#include \"api.h\"\n\
         struct Session { int fd; };\n\
         int run(struct Session *s) { return dispatch(s); }\n",
    )
    .unwrap();
    // The C++ unit only ever sees it declared.
    std::fs::write(
        root.join("impl.cpp"),
        "#include \"api.h\"\n\
         extern \"C\" int dispatch(struct Session *s) { return s ? 1 : 0; }\n",
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");

    let dispatches: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "dispatch")
        .collect();
    assert_eq!(
        dispatches.len(),
        1,
        "the prototype and the definition are one function, got {:?}",
        dispatches
            .iter()
            .map(|f| (f.id, f.is_defined))
            .collect::<Vec<_>>()
    );
    assert!(
        dispatches[0].is_defined,
        "the surviving entry must carry the C++ body"
    );

    let (_pag, analysis) = analyze(&program);
    assert_eq!(
        direct_targets(&program, &analysis, "run"),
        vec!["dispatch"],
        "the C caller must reach the C++ definition directly, not as an external"
    );
}

#[test]
fn cpp_array_parameter_prototype_merges_with_pointer_definition() {
    // `int a[]` and `int *a` declare the same parameter, so this is one
    // function. Before the top-level decay in `same_param_type` the two read
    // as C++ overloads: `sum` stayed split and `go` kept an `external` edge to
    // the undefined prototype -- the #83 symptom from a different cause. Pure
    // C never showed it, because C prototypes and definitions collapse without
    // consulting parameter types at all.
    let dir = tempfile::Builder::new()
        .prefix("trace_arr_param_")
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(root.join("api.h"), "int sum(int a[]);\n").unwrap();
    std::fs::write(
        root.join("impl.cpp"),
        "#include \"api.h\"\nint sum(int *a) { return a[0]; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.cpp"),
        "#include \"api.h\"\nint go(int *p) { return sum(p); }\n",
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");

    let sums: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "sum")
        .collect();
    assert_eq!(
        sums.len(),
        1,
        "the array prototype and the pointer definition are one function"
    );
    assert!(
        sums[0].is_defined,
        "the surviving entry must carry the body"
    );

    let (_pag, analysis) = analyze(&program);
    assert_eq!(
        direct_targets(&program, &analysis, "go"),
        vec!["sum"],
        "go must reach the definition directly"
    );
}

#[test]
fn c_typedef_self_alias_preserves_struct_pointer_type() {
    let dir = tempfile::Builder::new()
        .prefix("trace_typedef_self_")
        .tempdir()
        .unwrap();
    let root = dir.path();
    // Header only has forward typedef struct Session Session;
    std::fs::write(
        root.join("session.h"),
        "typedef struct Session Session;\nint SessionStart(Session **s);\n",
    )
    .unwrap();
    // Caller TU uses the header
    std::fs::write(
        root.join("caller.c"),
        "#include \"session.h\"\nint run(Session **s) {\n    return SessionStart(s);\n}\n",
    )
    .unwrap();
    // Definition TU defines struct Session and the function
    std::fs::write(
        root.join("session.c"),
        "#include \"session.h\"\nstruct Session { int id; };\nint SessionStart(Session **s) {\n    return 0;\n}\n",
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let start = program
        .symbols
        .resolve_function("SessionStart")
        .expect("SessionStart resolved");
    let func = program.symbols.function(start);
    assert!(func.is_defined, "SessionStart must be defined");

    // `Session **` must lower as a pointer to the struct, not degrade to
    // `Ptr(Ptr(Int))`. Check `run`, not `SessionStart`: `run` lives in
    // caller.c, the TU that sees ONLY `typedef struct Session Session;`, so it
    // is the side that degraded. `SessionStart` is defined in session.c
    // alongside `struct Session { int id; }`, so its parameter resolved
    // through the tag either way and asserting on it proves nothing.
    let assert_ptr_ptr_session = |fn_name: &str, id| {
        let f = program.symbols.function(id);
        let param_var = program.symbols.variable(f.params[0]);
        let param_ty = program.types.get(param_var.type_id);
        match param_ty.desc.as_ref() {
            trace_ir::TypeDesc::Ptr(inner) => match &**inner {
                trace_ir::TypeDesc::Ptr(elem) => match &**elem {
                    trace_ir::TypeDesc::Struct { name, .. } => assert_eq!(name, "Session"),
                    other => panic!("{fn_name}: expected Struct Session, got {other:?}"),
                },
                other => panic!("{fn_name}: expected Ptr, got {other:?}"),
            },
            other => panic!("{fn_name}: expected Ptr, got {other:?}"),
        }
    };
    let run = program
        .symbols
        .resolve_function("run")
        .expect("run resolved");
    assert_ptr_ptr_session("run", run);
    assert_ptr_ptr_session("SessionStart", start);
    let (_pag, analysis) = analyze(&program);
    assert_eq!(
        direct_targets(&program, &analysis, "run"),
        vec!["SessionStart"],
        "run must call SessionStart directly"
    );
}

/// The constructor-call check in lowering read the `anon_` prefix without
/// the digits `is_anonymous_tag` requires, so a tree's own `struct anon_vma`
/// (a real tag in Linux) lost its constructor calls. A synthesized
/// `anon_<n>` cannot be spelled as a declared type from source, so the
/// named side is the only one a test can reach.
#[test]
fn named_tag_with_anon_prefix_emits_its_constructor_call() {
    let dir = tempfile::Builder::new()
        .prefix("trace_anon_ctor_")
        .tempdir()
        .unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("test.cpp"),
        r#"
struct anon_vma {
    anon_vma(int x);
};
anon_vma::anon_vma(int x) {}

void test() {
    anon_vma v(42);
}
"#,
    )
    .unwrap();

    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let targets = direct_targets(&program, &analysis, "test");
    assert!(
        targets.iter().any(|t| t == "anon_vma::anon_vma"),
        "a named tag starting with `anon_` must emit its constructor call: {targets:?}"
    );
}

analyzed_fixture!(cpp_type_lookup);

#[test]
fn bare_type_name_resolves_through_enclosing_namespaces() {
    let (program, analysis) = cpp_type_lookup();
    for caller in [
        "a::b::c::TriLocal",
        "a::b::c::TriParam",
        "a::b::c::TriField",
        "a::x::Partial",
    ] {
        assert!(
            has_direct(program, analysis, caller, "a::b::Deep::Go"),
            "{caller}"
        );
    }
    assert!(has_direct(
        program,
        analysis,
        "a::b::c::TriGlobal",
        "GlobalTarget::Run"
    ));
    assert!(has_direct(
        program,
        analysis,
        "Outer::ArrowReturn",
        "RealTarget::Run"
    ));
    let make = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "a::b::c::MakeDeep")
        .expect("MakeDeep");
    assert!(
        matches!(program.types.get(make.return_type).desc.as_ref(),
            trace_ir::TypeDesc::Ptr(inner)
                if matches!(&**inner, trace_ir::TypeDesc::Struct { name, .. } if name == "a::b::Deep")),
        "a return type is looked up like any other type"
    );
}

#[test]
fn innermost_type_declaration_shadows_outer_ones() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "n::m::Shadowed",
        "n::Shadow::Hit"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "n::m::Shadowed",
        "Shadow::Hit"
    ));
    assert!(has_direct(
        program,
        analysis,
        "n::m::GlobalShadow",
        "Shadow::Hit"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "n::m::GlobalShadow",
        "n::Shadow::Hit"
    ));
}

#[test]
fn same_named_typedefs_in_two_namespaces_stay_apart() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "p::inner::ScopedTypedef",
        "p::PT::Run"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "p::inner::ScopedTypedef",
        "q::QT::Run"
    ));
    assert!(has_direct(
        program,
        analysis,
        "q::OtherTypedef",
        "q::QT::Run"
    ));
}

#[test]
fn using_alias_declaration_is_a_typedef() {
    let (program, analysis) = cpp_type_lookup();
    for caller in ["UsingLocal", "UsingPointer", "UsingWrapper"] {
        assert!(
            has_direct(program, analysis, caller, "AliasTarget::Run"),
            "{caller}"
        );
    }
    for caller in ["ua::deeper::UsingEnclosing", "UsingQualified"] {
        assert!(
            has_direct(program, analysis, caller, "ua::Scoped::Run"),
            "{caller}"
        );
    }
    assert!(has_any_edge(program, analysis, "UsingFnPtr", "CbTarget"));
}

#[test]
fn class_member_alias_is_scoped_to_its_class() {
    let (program, analysis) = cpp_type_lookup();
    for caller in [
        "WithMemberAlias::ViaMember",
        "WithMemberAlias::ViaParam",
        "MemberAliasOutside",
    ] {
        assert!(
            has_direct(program, analysis, caller, "AliasTarget::Run"),
            "{caller}"
        );
    }
    assert!(must_not_have_edge(
        program,
        analysis,
        "MemberAliasNoLeak",
        "AliasTarget::Run"
    ));
}

#[test]
fn nested_class_is_registered_under_its_outer_class() {
    let (program, analysis) = cpp_type_lookup();
    let tags = struct_tag_names(program);
    for tag in [
        "CS::Defined::Iterator",
        "CS::Plain::It",
        "GMethods::GIn2",
        "GIn",
    ] {
        assert!(tags.iter().any(|t| t == tag), "{tag}: {tags:?}");
    }
    for tag in ["CS::Iterator", "CS::It", "GIn2"] {
        assert!(!tags.iter().any(|t| t == tag), "no {tag}: {tags:?}");
    }
    for phantom in ["CS::Defined::it", "CS::Plain::pit", "CS::Plain::cb"] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == phantom),
            "{phantom} is a field of the nested class"
        );
    }
    let field_of = |cls: &str, field: &str| {
        program
            .types
            .type_id_by_tag(cls, trace_ir::TypeKind::Struct)
            .map(|id| match program.types.get(id).desc.as_ref() {
                trace_ir::TypeDesc::Struct { fields, .. } => {
                    fields.iter().any(|(name, _)| name == field)
                }
                _ => false,
            })
            .unwrap_or(false)
    };
    assert!(field_of("CS::Defined::Iterator", "it"));
    assert!(field_of("CS::Plain::It", "pit"));
    assert!(field_of("CS::Defined", "df"));
    assert!(!field_of("CS::Defined", "it"));
    for (caller, callee) in [
        ("CS::Sub::P7", "CS::Defined::Iterator::Next"),
        ("CS::Plain::Use", "CS::Plain::It::Step"),
        ("CS::OpenBox", "CS::Plain::Box::Open"),
        ("Built::Builder::Build", "Built::Built"),
        ("MakeBuilt", "Built::Built"),
    ] {
        assert!(has_direct(program, analysis, caller, callee), "{caller}");
    }
    for caller in ["CallHook", "CallHook2", "CallNestedCb"] {
        assert!(
            has_any_edge(program, analysis, caller, "HookTarget"),
            "{caller} calls through the nested class's field"
        );
    }
    let it_reads = program
        .flow
        .iter()
        .filter(|f| {
            matches!(f, trace_ir::FlowConstraint::GepField { field_name, .. }
                if field_name == "it")
        })
        .count();
    assert_eq!(it_reads, 1, "`i.it` reaches the nested class's layout");
}

#[test]
fn constructor_call_arguments_bind_past_this() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "ConstructTakes",
        "Takes::Takes"
    ));
    assert!(
        has_any_edge(program, analysis, "Takes::Takes", "CtorCbTarget"),
        "the callback reaches the constructor's own parameter"
    );
}

#[test]
fn member_class_sees_later_members_of_its_enclosing_class() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "Later::Builder::Build",
        "Later::Later"
    ));
}

#[test]
fn function_local_aliases_are_block_scoped() {
    let (program, analysis) = cpp_type_lookup();
    for caller in ["LocalUsing", "LocalTypedef"] {
        assert!(
            has_direct(program, analysis, caller, "LocalTarget::Run"),
            "{caller}"
        );
    }
    assert!(must_not_have_edge(
        program,
        analysis,
        "LocalOutOfBlock",
        "LocalTarget::Run"
    ));
    assert!(has_direct(
        program,
        analysis,
        "LocalAliasCtor",
        "Built2::Built2"
    ));
}

#[test]
fn array_alias_keeps_its_shape() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_any_edge(
        program,
        analysis,
        "CallTable",
        "TableCbTarget"
    ));
    let table = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "cb_table")
        .expect("cb_table");
    assert!(matches!(
        program.types.get(table.type_id).desc.as_ref(),
        trace_ir::TypeDesc::Array { .. }
    ));
}

#[test]
fn qualified_spellings_reach_aliases_and_constructors() {
    let (program, analysis) = cpp_type_lookup();
    for caller in [
        "TemplateMemberAlias",
        "ArrowTemplateMemberAlias",
        "rv::GlobalAliasUse",
        "CastReceiver",
    ] {
        assert!(
            has_direct(program, analysis, caller, "LocalTarget::Run"),
            "{caller}"
        );
    }
    for callee in ["rv::Made::Made", "rv::Made::In::In"] {
        assert!(
            has_direct(program, analysis, "QualifiedCtor", callee),
            "{callee}"
        );
    }
}

#[test]
fn cpp_class_nested_in_c_structs_keeps_its_whole_path() {
    let (program, _analysis) = cpp_type_lookup();
    let tags = struct_tag_names(program);
    assert!(tags.iter().any(|t| t == "CPlain::CMid::CDeep"), "{tags:?}");
    assert!(program
        .symbols
        .resolve_function("CPlain::CMid::CDeep::Go")
        .is_some());
}

#[test]
fn bodyless_class_specifier_inside_a_class_names_the_member_class() {
    let (program, _analysis) = cpp_type_lookup();
    let tags = struct_tag_names(program);
    for tag in ["ListNode", "PimplImpl"] {
        assert!(!tags.iter().any(|t| t == tag), "no bare {tag}: {tags:?}");
    }
    let field_points_to = |cls: &str, field: &str, target: &str| {
        let id = program
            .types
            .type_id_by_tag(cls, trace_ir::TypeKind::Struct)
            .unwrap_or_else(|| panic!("{cls}: {tags:?}"));
        matches!(program.types.get(id).desc.as_ref(), trace_ir::TypeDesc::Struct { fields, .. }
            if fields.iter().any(|(name, desc)| name == field
                && matches!(desc, trace_ir::TypeDesc::Ptr(inner)
                    if matches!(&**inner, trace_ir::TypeDesc::Struct { name, .. } if name == target))))
    };
    assert!(field_points_to("List::ListNode", "next", "List::ListNode"));
    assert!(field_points_to("Pimpl", "p", "Pimpl::PimplImpl"));
}

#[test]
fn out_of_line_member_class_is_the_declared_class() {
    let (program, analysis) = cpp_type_lookup();
    for callee in ["hm::Manager::Info::Info", "hm::Manager::Info::Get"] {
        assert!(
            has_direct(program, analysis, "hm::Manager::Add", callee),
            "{callee}"
        );
    }
}

#[test]
fn nearer_function_hides_a_class_of_the_same_name() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "hide::CallHidden",
        "hide::HiddenCtor"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "hide::CallHidden",
        "HiddenCtor::HiddenCtor"
    ));
}

#[test]
fn alias_in_a_function_local_class_stays_in_the_class() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "LocalClassAlias",
        "OuterAliasTarget::Run"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "LocalClassAlias",
        "InnerAliasTarget::Run"
    ));
}

#[test]
fn constructor_call_reaches_an_internal_linkage_class() {
    let (program, analysis) = cpp_type_lookup();
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(program, e.caller) == "UseLocalHelper"
                && fn_name(program, e.callee).ends_with("LocalHelper::LocalHelper")
        }),
        "{:?}",
        common::callees_of(program, analysis, "UseLocalHelper")
    );
}

#[test]
fn function_passed_to_new_reaches_the_constructor_parameter() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_any_edge(
        program,
        analysis,
        "Consumer::Consumer",
        "OnEvent"
    ));
}

#[test]
fn alias_of_an_alias_names_the_class() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "AliasChain",
        "ChainTarget::Run"
    ));
}

#[test]
fn local_class_in_a_member_function_sees_the_enclosing_class() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "Enclosing::Method",
        "Enclosing::Nested::Run"
    ));
}

#[test]
fn local_class_prefers_the_function_class_over_the_namespace() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "lcns::Enclosing2::Method",
        "lcns::Enclosing2::Nested::Run"
    ));
    assert!(must_not_have_edge(
        program,
        analysis,
        "lcns::Enclosing2::Method",
        "lcns::Nested::Run"
    ));
}

#[test]
fn out_of_line_member_of_a_data_only_struct_keeps_its_methods() {
    let (program, analysis) = cpp_type_lookup();
    assert!(has_direct(
        program,
        analysis,
        "CConfig::CParser::Parse",
        "HitTarget::Hit"
    ));
}

analyzed_fixture!(cpp_member_args);

/// `(arg_index, actual, formal)` for every argument `caller` hands `callee`.
fn arg_bindings(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    callee: &str,
) -> Vec<(u32, String, String)> {
    let sites: std::collections::HashSet<_> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(program, e.caller) == caller && fn_name(program, e.callee) == callee)
        .map(|e| e.call_site)
        .collect();
    analysis
        .arg_flow_edges
        .iter()
        .filter(|e| sites.contains(&e.call_site))
        .map(|e| {
            let actual = match (e.actual_var, e.actual_fn) {
                (Some(v), _) => program.symbols.variable(v).name.clone(),
                (None, Some(f)) => fn_name(program, f),
                (None, None) => String::new(),
            };
            let formal = program.symbols.variable(e.formal).name.clone();
            (e.arg_index, actual, formal)
        })
        .collect()
}

#[test]
fn method_call_arguments_bind_past_this() {
    let (program, analysis) = cpp_member_args();
    for (callee, actual) in [
        ("Button::SetHandler", "OnDot"),
        ("Button::SetHandler", "OnArrow"),
        ("Button::operator()", "OnFunctor"),
        ("Slot::operator()", "OnFieldFunctor"),
        ("Remote::Later", "OnRemote"),
        ("Remote::Shared", "OnStaticDot"),
        ("Remote::Shared", "OnStaticQualified"),
    ] {
        let bindings = arg_bindings(program, analysis, "Wire", callee);
        assert!(
            bindings.contains(&(1, actual.to_owned(), "cb".to_owned())),
            "Wire -> {callee}: {actual} must reach `cb`, got {bindings:?}"
        );
        assert!(
            has_any_edge(program, analysis, callee, actual),
            "{callee} calls what it was handed: {actual}"
        );
    }
    let bindings = arg_bindings(program, analysis, "Wire", "Button::SetHandler");
    assert!(
        bindings.contains(&(1, "f".to_owned(), "cb".to_owned())),
        "a variable argument reaches `cb` too: {bindings:?}"
    );
    assert!(has_any_edge(
        program,
        analysis,
        "Button::SetHandler",
        "OnVar"
    ));
}

#[test]
fn member_defined_without_its_class_in_view_takes_this() {
    let (program, analysis) = cpp_member_args();
    for (callee, actual) in [
        ("Detached::Later", "OnDetachedLater"),
        ("Detached::Shared", "OnDetachedShared"),
    ] {
        assert_eq!(
            arg_bindings(program, analysis, "UseDetached", callee),
            [(1, actual.to_owned(), "cb".to_owned())],
            "{callee}: the definition's unit never saw `Detached`"
        );
        assert!(has_any_edge(program, analysis, callee, actual));
        let definition = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == callee && f.is_defined)
            .expect("defined");
        assert_eq!(
            program.symbols.variable(definition.params[0]).name,
            "this",
            "{callee} has its implicit `this`"
        );
    }
    let this_of = |name: &str| {
        program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == name && f.is_defined)
            .and_then(|f| f.params.first())
            .map(|&p| program.symbols.variable(p).name.clone())
    };
    assert_eq!(
        this_of("Detached::Reset").as_deref(),
        Some("this"),
        "a parameterless member defined without its class takes `this` too"
    );
    assert_eq!(
        this_of("util::Init"),
        None,
        "a parameterless namespace function defined out of line takes no `this`"
    );
    assert_eq!(
        arg_bindings(program, analysis, "CallInImplUnit", "Detached::Shared"),
        [(1, "OnSameUnit".to_owned(), "cb".to_owned())],
        "a call beside the definition is bound past the `this` it gains"
    );
}

#[test]
fn namespace_function_named_like_a_class_takes_no_this() {
    let (program, analysis) = cpp_member_args();
    assert_eq!(
        arg_bindings(program, analysis, "UseClockNamespace", "ns::Clock::Format"),
        [(0, "OnFormat".to_owned(), "cb".to_owned())],
        "`ns::Clock` is a namespace here, whatever `clock_class.hpp` declares"
    );
}

#[test]
fn call_to_a_member_declared_only_binds_past_this() {
    let (program, _) = cpp_member_args();
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|cs| {
            fn_name(program, cs.caller) == "CallWithoutHeader"
                && cs.callee_name == "Remote::Declared"
        })
        .expect("site");
    assert_eq!(site.fn_args.first().map(|&(index, _)| index), Some(1));
}

#[test]
fn qualified_member_call_resolved_after_the_merge_binds_past_this() {
    let (program, analysis) = cpp_member_args();
    assert_eq!(
        arg_bindings(program, analysis, "CallWithoutHeader", "Remote::Shared"),
        [(1, "OnUnseen".to_owned(), "cb".to_owned())],
        "the unit never saw `Remote`, yet `Remote::Shared` is a member"
    );
    assert!(has_any_edge(
        program,
        analysis,
        "Remote::Shared",
        "OnUnseen"
    ));
    assert_eq!(
        arg_bindings(program, analysis, "CallWithoutHeader", "util::Free"),
        [(0, "OnUnseenFree".to_owned(), "cb".to_owned())],
        "a namespace function has no `this`"
    );
}

#[test]
fn implicit_this_and_qualified_method_calls_bind_past_this() {
    let (program, analysis) = cpp_member_args();
    for (caller, actual) in [
        ("Button::Relay", "OnImplicit"),
        ("Button::RelayArrow", "OnThisArrow"),
        ("Fancy::Qualified", "OnQualified"),
    ] {
        let bindings = arg_bindings(program, analysis, caller, "Button::SetHandler");
        assert!(
            bindings.contains(&(1, "cb".to_owned(), "cb".to_owned())),
            "{caller} -> Button::SetHandler: `cb` must reach `cb`, got {bindings:?}"
        );
        assert!(
            has_any_edge(program, analysis, "Button::SetHandler", actual),
            "{actual} flows through {caller} into Button::SetHandler"
        );
    }
}

#[test]
fn qualified_method_call_picks_overload_by_explicit_arity() {
    let (program, analysis) = cpp_member_args();
    let targets: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(program, e.caller) == "PickerUser::Use")
        .map(|e| e.callee)
        .collect();
    assert_eq!(targets.len(), 1, "one overload takes two arguments");
    let params = &program.symbols.function(targets[0]).params;
    assert_eq!(params.len(), 3, "`this`, `cb` and `n`");
    assert!(has_any_edge(
        program,
        analysis,
        &fn_name(program, targets[0]),
        "OnOverload"
    ));
}

#[test]
fn no_explicit_argument_binds_to_this() {
    let (program, analysis) = cpp_member_args();
    for e in &analysis.arg_flow_edges {
        if program.symbols.variable(e.formal).name != "this" {
            continue;
        }
        assert!(e.actual_fn.is_none(), "a function reached `this`: {e:?}");
        let actual = program
            .symbols
            .variable(e.actual_var.expect("actual"))
            .name
            .clone();
        assert!(
            actual.starts_with("_ret"),
            "only a `new` allocation is bound to `this`, got `{actual}`"
        );
    }
}

#[test]
fn new_expression_arguments_bind_past_this() {
    let (program, analysis) = cpp_member_args();
    for (caller, actual) in [
        ("MakeWithVar", "f"),
        ("MakeByName", "OnNewName"),
        ("MakeStatement", "OnNewStmt"),
    ] {
        let bindings = arg_bindings(program, analysis, caller, "Worker::Worker");
        assert!(
            bindings.contains(&(1, actual.to_owned(), "cb".to_owned())),
            "{caller}: {actual} must reach `cb`, got {bindings:?}"
        );
    }
    for actual in ["OnNewVar", "OnNewName", "OnNewStmt"] {
        assert!(has_any_edge(program, analysis, "Worker::Worker", actual));
    }
}

#[test]
fn member_initializer_arguments_bind_past_this() {
    let (program, analysis) = cpp_member_args();
    for (caller, callee, actual) in [
        ("ByName::ByName", "Base::Base", "OnBaseName"),
        ("ByVar::ByVar", "Base::Base", "f"),
        ("BraceBase::BraceBase", "Base::Base", "OnBraceBase"),
        ("Holder::Holder", "Member::Member", "f"),
        (
            "BraceHolder::BraceHolder",
            "Member::Member",
            "OnBraceMember",
        ),
    ] {
        let bindings = arg_bindings(program, analysis, caller, callee);
        assert!(
            bindings.contains(&(1, actual.to_owned(), "cb".to_owned())),
            "{caller} -> {callee}: {actual} must reach `cb`, got {bindings:?}"
        );
    }
    for (callee, actual) in [
        ("Base::Base", "OnBaseName"),
        ("Base::Base", "OnBaseVar"),
        ("Base::Base", "OnBraceBase"),
        ("Member::Member", "OnMember"),
        ("Member::Member", "OnBraceMember"),
    ] {
        assert!(
            has_any_edge(program, analysis, callee, actual),
            "{callee} calls {actual}"
        );
    }
}

analyzed_fixture!(cpp_overload_prototypes);

/// `callee@file:line` for every edge out of `caller`, sorted; a callee with
/// no body in the tree is marked `(declared)`.
fn callee_definitions(program: &Program, analysis: &AnalysisResult, caller: &str) -> Vec<String> {
    let mut out: Vec<String> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(program, e.caller) == caller)
        .map(|e| {
            let f = program.symbols.function(e.callee);
            let file = program.symbols.files[f.span.file.0 as usize]
                .path
                .file_name();
            format!(
                "{}@{}:{}{}",
                f.name,
                file.map(|n| n.to_string_lossy()).unwrap_or_default(),
                f.span.line,
                if f.is_defined { "" } else { " (declared)" }
            )
        })
        .collect();
    out.sort();
    out
}

/// The file name `f` is defined (or declared) in.
fn file_name_of(program: &Program, f: &trace_ir::Function) -> String {
    let path = &program.symbols.files[f.span.file.0 as usize].path;
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// The callee of every edge out of `caller`, or only out of its overload
/// taking `arity` explicit parameters.
fn overload_callees(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    arity: Option<u32>,
) -> Vec<FnId> {
    analysis
        .call_edges
        .iter()
        .filter(|e| {
            let from = program.symbols.function(e.caller);
            from.name == caller && arity.is_none_or(|a| from.explicit_arity == Some(a))
        })
        .map(|e| e.callee)
        .collect()
}

/// The explicit arities of [`overload_callees`], sorted, one per edge.
fn overload_callee_arities(
    program: &Program,
    analysis: &AnalysisResult,
    caller: &str,
    arity: Option<u32>,
) -> Vec<Option<u32>> {
    let mut arities: Vec<_> = overload_callees(program, analysis, caller, arity)
        .into_iter()
        .map(|id| program.symbols.function(id).explicit_arity)
        .collect();
    arities.sort();
    arities
}

#[test]
fn prototype_overloads_bind_calls_by_declared_arity() {
    let (program, analysis) = cpp_overload_prototypes();
    assert_eq!(
        callee_definitions(program, analysis, "GetWithOut"),
        ["Session::Get@session.cpp:4"],
        "`s->Get(mode)` is `Get(Mode&)`, not `Get()`"
    );
    assert_eq!(
        callee_definitions(program, analysis, "GetPlain"),
        ["Session::Get@session.cpp:3"]
    );
    assert_eq!(
        callee_definitions(program, analysis, "RunTwice"),
        ["Session::Run@session.cpp:6"]
    );
    assert!(has_any_edge(program, analysis, "Session::Run", "OnRun"));
}

#[test]
fn prototype_default_arguments_accept_shorter_calls() {
    let (program, analysis) = cpp_overload_prototypes();
    let targets = callee_definitions(program, analysis, "LogWithDefault");
    assert!(
        targets.iter().any(|t| t == "Session::Log@session.cpp:7"),
        "`s->Log(OnLog)` fits `Log(Callback, int = 0)`: {targets:?}"
    );
    assert!(has_any_edge(program, analysis, "Session::Log", "OnLog"));
}

#[test]
fn variadic_overload_stays_a_candidate_for_more_arguments() {
    let (program, analysis) = cpp_overload_prototypes();
    assert_eq!(
        callee_definitions(program, analysis, "LogPointers"),
        ["Log@variadic.cpp:2"],
        "`Log(p, p)` fits `Log(void*, ...)`, not `Log(int, int)`"
    );
    assert_eq!(
        callee_definitions(program, analysis, "NotifyOnce"),
        ["Notify@variadic.cpp:9"],
        "`Notify(\"ready\", 1)` takes the fixed overload before the variadic one"
    );
    assert_eq!(
        callee_definitions(program, analysis, "TraceUnknown")
            .into_iter()
            .filter(|t| t.starts_with("Trace@"))
            .collect::<Vec<_>>(),
        ["Trace@variadic.cpp:15", "Trace@variadic.cpp:16"],
        "an argument of unknown type keeps both overloads"
    );
    assert_eq!(
        callee_definitions(program, analysis, "ShowNumber"),
        ["Show@variadic.cpp:22"],
        "`Show(2.5)` cannot take `Show(const char*)`"
    );
    assert_eq!(
        callee_definitions(program, analysis, "EmitMore"),
        ["Session::Emit@session.cpp:9"],
        "in-class `Emit(Callback)` and `Emit(Callback, ...)` stay two overloads"
    );
    assert!(has_any_edge(program, analysis, "Session::Emit", "OnEmit"));
}

#[test]
fn default_arguments_pool_only_within_one_signature() {
    let (program, analysis) = cpp_overload_prototypes();
    assert_eq!(
        callee_definitions(program, analysis, "text::TrimOne"),
        ["text::Trim@defaults.cpp:6"],
        "the prototype's default reaches a definition spelled `string`"
    );
    assert_eq!(
        callee_definitions(program, analysis, "FormatUnknown")
            .into_iter()
            .filter(|t| t.starts_with("Format@"))
            .collect::<Vec<_>>(),
        ["Format@format.cpp:3"],
        "`Format(cb, cb)` does not borrow `Format(int, int = 0)`'s default"
    );
}

#[test]
fn prototype_does_not_merge_into_a_same_named_body_of_another_arity() {
    let (program, analysis) = cpp_overload_prototypes();
    assert_eq!(
        callee_definitions(program, analysis, "FireNamed"),
        ["Listener::Fire@listener.cpp:3"],
        "the unrelated variadic template body must not take the call"
    );
    assert!(has_any_edge(program, analysis, "Listener::Fire", "OnFire"));
}

#[test]
fn static_member_template_keeps_its_in_class_body() {
    let (program, analysis) = cpp_overload_prototypes();
    let walks: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Walker::Walk")
        .map(|f| (f.span.line, f.is_defined, f.linkage))
        .collect();
    assert!(
        walks.contains(&(7, true, Linkage::External)),
        "a static member is not internal linkage, and its body survives: {walks:?}"
    );
    assert!(
        callee_definitions(program, analysis, "Walker::Walk")
            .iter()
            .any(|t| t == "Walker::Walk@walker.hpp:7"),
        "`Walk(root_, callback)` reaches the two-parameter static overload"
    );
}

analyzed_fixture!(cpp_direct_init);

#[test]
fn direct_initialization_spelled_like_a_function_declaration_constructs() {
    let (program, analysis) = cpp_direct_init();
    for (caller, callee, actual) in [
        ("ByName", "Worker::Worker", "OnName"),
        ("ByVariable", "Worker::Worker", "f"),
        ("ByTwo", "Pair::Pair", "OnFirst"),
        ("ByBraces", "Worker::Worker", "OnBrace"),
    ] {
        let bindings = arg_bindings(program, analysis, caller, callee);
        assert!(
            bindings.iter().any(|(i, a, _)| *i == 1 && a == actual),
            "{caller} -> {callee}: {actual} is argument 1, got {bindings:?}"
        );
        assert!(
            bindings.iter().any(|(i, _, f)| *i == 0 && f == "this"),
            "{caller}: the object is `this`, got {bindings:?}"
        );
    }
    assert!(
        arg_bindings(program, analysis, "ByTwo", "Pair::Pair").contains(&(
            2,
            "OnSecond".to_owned(),
            "second".to_owned()
        ))
    );
    for (callee, actual) in [
        ("Worker::Worker", "OnName"),
        ("Worker::Worker", "OnVar"),
        ("Pair::Pair", "OnSecond"),
        ("Worker::Worker", "OnBrace"),
        ("PointerDirectInit", "OnPointer"),
        ("AggregateBraces", "OnAggregate"),
    ] {
        assert!(
            has_any_edge(program, analysis, callee, actual),
            "{callee} -> {actual}"
        );
    }
    let names: Vec<&str> = program
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    for object in ["w", "p", "c"] {
        assert!(
            !names.contains(&object),
            "`{object}` is an object: {names:?}"
        );
    }
    assert!(
        has_any_edge(program, analysis, "Counter::Tick", "Lock::Lock"),
        "a data member in the parentheses is a constructor argument"
    );
    assert!(
        has_any_edge(program, analysis, "PointerFromLocal", "OnTable"),
        "`Callback *slot(table);` initializes a pointer from the local"
    );
    for object in ["guard", "slot"] {
        assert!(
            !names.contains(&object),
            "`{object}` is an object, not a function: {names:?}"
        );
    }
    assert!(
        has_any_edge(program, analysis, "DefaultedAggregate", "OnDefaulted"),
        "a class whose only constructor is defaulted is an aggregate"
    );
    assert!(
        has_any_edge(program, analysis, "LocalAliasHidesVariable", "build"),
        "`Worker build(Value);` declares a function when `Value` names a type"
    );
    for declaration in ["make", "nothing", "make_global"] {
        assert!(
            names.contains(&declaration),
            "`{declaration}` stays a declaration"
        );
    }
    assert!(
        arg_bindings(program, analysis, "Parenthesized", "Worker::Worker").contains(&(
            1,
            "OnParenthesized".to_owned(),
            "cb".to_owned()
        )),
        "a parenthesized function name is the argument"
    );
    for object in [
        "global_worker",
        "global_callback",
        "static_worker",
        "header_worker",
    ] {
        assert!(
            !names.contains(&object),
            "`{object}` is a file-scope object: {names:?}"
        );
    }
    assert!(
        has_direct(program, analysis, "HeaderLocal", "Worker::Worker"),
        "`Worker w(header_callback);` constructs with a header's file `static`"
    );
    assert!(
        has_any_edge(program, analysis, "CallGlobalCallback", "OnGlobalPointer"),
        "`Callback global_callback(OnGlobalPointer);` initializes the pointer"
    );
}

analyzed_fixture!(cpp_later_members);

#[test]
fn a_method_calls_a_method_its_class_defines_later() {
    let (program, analysis) = cpp_later_members();
    for (caller, callee) in [
        ("Parser::Parse", "Parser::ReadHeader"),
        ("Parser2::Parse", "Parser2::ReadHeader"),
        ("Runner::Run", "Runner::Invoke"),
        ("Shadow::Go", "Shadow::Helper"),
        ("Derived::Call", "Derived::Name"),
        ("Box::Open", "Box::Unpack"),
        ("Outer::Inner::Start", "Outer::Inner::Step"),
    ] {
        assert_eq!(
            direct_targets(program, analysis, caller),
            [callee],
            "{caller} calls {callee}"
        );
    }
    assert!(
        analysis.call_edges.iter().any(|e| {
            fn_name(program, e.caller).starts_with("Outer::Spawn::$lambda")
                && fn_name(program, e.callee) == "Outer::Work"
                && e.resolution == ResolutionKind::Direct
        }),
        "a lambda in a method body calls the later method"
    );
    assert!(
        arg_bindings(program, analysis, "Runner::Run", "Runner::Invoke").contains(&(
            1,
            "OnLater".to_owned(),
            "cb".to_owned()
        )),
        "the argument binds past `this`"
    );
    assert!(has_any_edge(program, analysis, "Runner::Invoke", "OnLater"));
    assert_eq!(
        overload_callee_arities(program, analysis, "Overloads::Use", None),
        [Some(1), Some(2)],
        "one call per overload, by arity"
    );
    for (caller, wrong) in [
        ("Shadow::Go", "Helper"),
        ("Derived::Call", "Base::Name"),
        ("Local::Apply", "Local::Later"),
    ] {
        assert!(
            must_not_have_edge(program, analysis, caller, wrong),
            "{caller} does not call {wrong}"
        );
    }
    // An overload above the calling body does not stand in for the one below.
    for (caller, caller_arity, callee_arities) in [
        ("Forward::Process", 1, [Some(0)]),
        ("Forward::Use", 0, [Some(2)]),
        ("Built::Make", 0, [Some(0)]),
        ("Built::Copy", 0, [Some(1)]),
    ] {
        let mut arities = overload_callee_arities(program, analysis, caller, Some(caller_arity));
        arities.dedup();
        assert_eq!(
            arities, callee_arities,
            "{caller} calls the overload its arguments fit"
        );
    }
    assert!(has_any_edge(
        program,
        analysis,
        "Forward::Process",
        "OnNoArgs"
    ));
    assert!(has_any_edge(program, analysis, "Forward::Get", "OnTwoArgs"));
}

analyzed_fixture!(cpp_anonymous_members);

#[test]
fn members_of_a_class_in_an_anonymous_namespace_resolve() {
    let (program, analysis) = cpp_anonymous_members();
    for (caller, callee) in [
        ("Profile::Use", "Profile::Ratio"),
        ("Profile::Use", "Profile::Later"),
        ("Drive", "Profile::Use"),
        ("Drive", "Profile::Take"),
        ("Drive", "Holder::Holder"),
        ("Drive", "Out::Run"),
        ("Dispatch", "Impl::Fire"),
    ] {
        assert!(
            has_direct(program, analysis, caller, callee),
            "{caller} -> {callee}: {:?}",
            direct_targets(program, analysis, caller)
        );
    }
    for (caller, callee) in [
        ("Profile::Take", "OnTake"),
        ("Holder::Holder", "OnHolder"),
        ("Out::Run", "OnOut"),
        ("Impl::Fire", "OnFired"),
    ] {
        assert!(
            has_any_edge(program, analysis, caller, callee),
            "{caller} -> {callee}"
        );
    }
    let undefined: Vec<&str> = program
        .symbols
        .functions
        .iter()
        .filter(|f| !f.is_defined && f.name.starts_with("Profile::"))
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        undefined.is_empty(),
        "no external stands in for a defined member: {undefined:?}"
    );
    for ctor in ["FromPlain::FromPlain", "AnonFromPlain::AnonFromPlain"] {
        assert!(
            must_not_have_edge(program, analysis, ctor, ctor),
            "`: Plain()` does not construct through {ctor}"
        );
    }
    let callee_files = |caller: &str, callee: &str| -> Vec<String> {
        let prefix = format!("{callee}@");
        let mut files: Vec<String> = callee_definitions(program, analysis, caller)
            .iter()
            .filter_map(|d| Some(d.strip_prefix(&prefix)?.split(':').next()?.to_owned()))
            .collect();
        files.dedup();
        files
    };
    // Each file's class of a name is its own, virtual members included.
    for (caller, callee, file) in [
        ("Profile::Use", "Profile::Ratio", "anonymous.cpp"),
        ("DriveOther", "Profile::Ratio", "other.cpp"),
        ("FireHere", "Impl::Fire", "anonymous.cpp"),
        ("FireOther", "Impl::Fire", "other.cpp"),
    ] {
        assert_eq!(
            callee_files(caller, callee),
            [file],
            "{caller} calls its own file's {callee}"
        );
    }
    for (caller, other_files) in [
        ("FireQuiet", "Quiet::Fire"),
        ("FireInherited", "InheritsSub::Fire"),
        ("RunPlainly", "Hides::Run"),
    ] {
        assert!(
            must_not_have_edge(program, analysis, caller, other_files),
            "{caller} does not reach {other_files}: other.cpp's class of the name is another class"
        );
    }
    assert_eq!(
        callee_files("FireOwnFinished", "Finished::Fire"),
        ["anonymous.cpp"],
        "an anonymous receiver does not reach an external class of its name"
    );
    assert!(
        must_not_have_edge(program, analysis, "FireOwnFinished", "FinishedSub::Fire"),
        "nor that class's subclasses"
    );
    assert!(
        has_any_edge(program, analysis, "FireFinished", "FinishedSub::Fire"),
        "another file's anonymous `final` does not stop dispatch on an external class of its name"
    );
    assert!(
        has_any_edge(program, analysis, "Dispatch", "InheritsSub::Fire"),
        "a call through the base reaches the override in another file's anonymous namespace"
    );
    // Each overload's callees, in declaration order.
    let bodies = |name: &str| -> Vec<Vec<String>> {
        program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == name)
            .map(|f| {
                analysis
                    .call_edges
                    .iter()
                    .filter(|e| e.caller == f.id)
                    .map(|e| fn_name(program, e.callee))
                    .collect()
            })
            .collect()
    };
    assert_eq!(
        bodies("Typed::Work"),
        [["OnInt"], ["OnDouble"]],
        "Typed::Work(int) and Typed::Work(double) keep their own bodies"
    );
    for name in ["StaticWork", "FreeWork"] {
        assert_eq!(
            bodies(name),
            [["OnFreeInt"], ["OnFreeDouble"]],
            "{name}(int) and {name}(double) keep their own bodies"
        );
    }
    let mut free_calls = overload_callees(program, analysis, "DriveFreeOverloads", None);
    free_calls.sort();
    free_calls.dedup();
    assert_eq!(
        free_calls.len(),
        4,
        "each call reaches the overload its argument fits: {:?}",
        callee_definitions(program, analysis, "DriveFreeOverloads")
    );
    for (caller, callee) in [("CallFreeWorkPtr", "FreeWork"), ("TakeWork", "StaticWork")] {
        let reached: Vec<u32> = analysis
            .call_edges
            .iter()
            .filter(|e| {
                fn_name(program, e.caller) == caller && fn_name(program, e.callee) == callee
            })
            .map(|e| program.symbols.function(e.callee).span.line)
            .collect();
        assert_eq!(
            reached.len(),
            2,
            "a pointer taken from `{callee}` may hold either overload: {reached:?}"
        );
    }
    let outside: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Outside::Run")
        .collect();
    assert!(
        outside.len() == 2
            && outside
                .iter()
                .all(|f| f.is_defined && f.linkage == Linkage::Internal),
        "members defined after the namespace closes are its class's internal members: {:?}",
        outside
            .iter()
            .map(|f| (f.span.line, f.is_defined, f.linkage))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        program.symbols.functions_named("Outside::Run").len(),
        2,
        "member lookup finds both overloads"
    );
    for (caller, callee) in [
        ("CallExternalRoot", "Shared::Run"),
        ("CallNamed", "NamedSub::Run"),
    ] {
        assert!(
            must_not_have_edge(program, analysis, caller, callee),
            "{caller} does not reach {callee}: other.cpp's class of the name derives from another class"
        );
    }
    assert!(
        has_any_edge(program, analysis, "CallOtherRoot", "Shared::Run"),
        "a call through the base the anonymous class derives from still reaches it"
    );
    let respelled: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Respelled")
        .map(|f| f.is_defined)
        .collect();
    assert_eq!(
        respelled,
        [true],
        "a declaration and a definition spelling a type two ways are one function"
    );
    assert!(has_direct(program, analysis, "CallRespelled", "Respelled"));
    let early: Vec<Vec<String>> = overload_callees(program, analysis, "EarlyPick", None)
        .into_iter()
        .map(|id| {
            analysis
                .call_edges
                .iter()
                .filter(|e| e.caller == id)
                .map(|e| fn_name(program, e.callee))
                .collect()
        })
        .collect();
    assert_eq!(
        early,
        [["OnPickArg"]],
        "a declaration of Pick(Arg) reaches Pick(Arg)'s body, not Pick(int)'s"
    );
    assert!(
        has_any_edge(
            program,
            analysis,
            "FireThroughHeaderImpl",
            "HeaderLeaf::Fire"
        ),
        "a header class's member defined in the .cpp keeps its prototype's `virtual`"
    );
    let mut logs: Vec<String> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(program, e.caller) == "UseHeaderLog")
        .map(|e| file_name_of(program, program.symbols.function(e.callee)))
        .collect();
    logs.sort();
    assert_eq!(
        logs,
        ["anon_header.h", "other.cpp"],
        "a pointer to `HeaderLog` may hold the header's overload and the .cpp's"
    );
    // Each unit's `static` functions under a shared header stay its own.
    for (caller, own, file) in [
        ("UseSharedHere", "OnSharedHere", "anonymous.cpp"),
        ("UseSharedThere", "OnSharedThere", "other.cpp"),
    ] {
        for callee in ["SharedTag", "SharedDeclared"] {
            let reached: Vec<(String, Vec<String>)> = analysis
                .call_edges
                .iter()
                .filter(|e| {
                    fn_name(program, e.caller) == caller && fn_name(program, e.callee) == callee
                })
                .map(|e| {
                    let body = analysis
                        .call_edges
                        .iter()
                        .filter(|b| b.caller == e.callee)
                        .map(|b| fn_name(program, b.callee))
                        .collect();
                    (
                        file_name_of(program, program.symbols.function(e.callee)),
                        body,
                    )
                })
                .collect();
            assert!(
                reached
                    .iter()
                    .any(|(f, body)| f == file && body.iter().any(|c| c == own))
                    && reached.iter().all(|(_, body)| !body
                        .iter()
                        .any(|c| c.starts_with("OnShared") && c != own)),
                "{caller} reaches its own file's {callee}, not the other unit's: {reached:?}"
            );
        }
    }
    for (caller, other_base) in [
        ("CallMixed", "OtherRoot::Run"),
        ("CallMixedThere", "ExternalRoot::Run"),
    ] {
        assert!(
            must_not_have_edge(program, analysis, caller, other_base),
            "{caller} does not reach {other_base}: each file's anonymous Mixed has its own base"
        );
    }
    let loads: Vec<bool> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Load")
        .map(|f| f.is_defined)
        .collect();
    assert_eq!(
        loads,
        [false, true],
        "Load(cfg1::Config *) and Load(cfg2::Config *) are two functions"
    );
    for caller in ["FireHeaderImpl", "Dispatch"] {
        let targets: Vec<(bool, String)> = analysis
            .call_edges
            .iter()
            .filter(|e| {
                fn_name(program, e.caller) == caller
                    && fn_name(program, e.callee) == "HeaderImpl::Fire"
            })
            .map(|e| {
                let f = program.symbols.function(e.callee);
                (f.is_defined, file_name_of(program, f))
            })
            .collect();
        assert_eq!(
            targets,
            [(true, "other.cpp".to_owned())],
            "{caller} reaches the body other.cpp defines for a header's anonymous class"
        );
    }
    assert!(has_any_edge(program, analysis, "Respelled", "OnRespelled"));
    assert_eq!(
        callee_files("FireOwnOpen", "Open::Fire"),
        ["anonymous.cpp"],
        "an anonymous receiver reaches its own class"
    );
    assert!(
        must_not_have_edge(program, analysis, "FireOwnOpen", "OpenSub::Fire"),
        "not a subclass of an external class of its name"
    );
    for (caller, callee) in [
        ("FireOpen", "OpenSub::Fire"),
        ("FireSealed", "SealedSub::Fire"),
    ] {
        assert!(
            has_any_edge(program, analysis, caller, callee),
            "{caller} -> {callee}: another file's anonymous class of the name does not stop dispatch"
        );
    }
    assert_eq!(
        callee_files("Dispatch", "Impl::Fire"),
        ["anonymous.cpp", "other.cpp"],
        "a call through the base reaches every file's override"
    );
    // Overloads stay two functions, each with its own body.
    for (arity, callee, other) in [(1, "OnOne", "OnTwo"), (2, "OnTwo", "OnOne")] {
        let callees: Vec<String> =
            overload_callees(program, analysis, "Overloaded::Work", Some(arity))
                .into_iter()
                .map(|id| fn_name(program, id))
                .collect();
        assert!(
            callees.iter().any(|c| c == callee) && !callees.iter().any(|c| c == other),
            "Overloaded::Work of arity {arity} calls {callee} only: {callees:?}"
        );
    }
    assert_eq!(
        overload_callee_arities(program, analysis, "DriveOverloads", None),
        [Some(1), Some(2)],
        "one call per overload"
    );
}

#[test]
fn cpp_inherited_template_return_uses_base_declaration_scope() {
    let (p, a) = cpp_issue113();
    for caller in [
        "inherited_outside",
        "caller_scope::inherited_elsewhere",
        "inherited_nested",
    ] {
        assert!(
            has_any_edge(p, a, caller, "base_scope::Widget::Start"),
            "{caller}"
        );
        assert!(!has_any_edge(p, a, caller, "Widget::Start"));
        assert!(!has_any_edge(p, a, caller, "caller_scope::Widget::Start"));
    }
}

#[test]
fn cpp_inherited_template_return_keeps_dependent_arguments_unknown() {
    let (p, a) = cpp_issue113();
    assert!(has_any_edge(
        p,
        a,
        "DependentDerived::Run",
        "base_scope::ScopedHolder::Get"
    ));
    assert!(!has_any_edge(
        p,
        a,
        "DependentDerived::Run",
        "DependentType::Start"
    ));
}

#[test]
fn cpp_inherited_template_return_uses_enclosing_class_scope() {
    let (p, a) = cpp_issue113();
    assert!(has_any_edge(
        p,
        a,
        "inherited_class_scope",
        "OuterScope::Widget::Start"
    ));
    assert!(!has_any_edge(
        p,
        a,
        "inherited_class_scope",
        "Widget::Start"
    ));
}

#[test]
fn cpp_template_return_resolves_partially_qualified_bases() {
    let (p, a) = cpp_issue113();
    for caller in ["partial_base::scoped", "imported_base::imported"] {
        assert!(
            has_any_edge(p, a, caller, "partial_base::hardware::Holder::Get"),
            "{caller} base member"
        );
        assert!(
            has_any_edge(p, a, caller, "partial_base::hardware::Widget::Start"),
            "{caller} return"
        );
    }
}

#[test]
fn relative_qualified_callee_in_enclosing_namespace_and_using_directive() {
    let dir = tempfile::Builder::new()
        .prefix("trace_relative_qual_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("connection.h"),
        r#"
namespace OHOS {
namespace Runtime {
namespace Utils {
void RemoveConn(long id);
}
}
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a_conn.cpp"),
        r#"
#include "connection.h"
namespace OHOS {
namespace Runtime {
namespace Utils {
void RemoveConn(long id) {}
}
void CallerA() {
    Utils::RemoveConn(1);
}
}
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b_manager.cpp"),
        r#"
#include "connection.h"
using namespace OHOS::Runtime;
namespace OHOS {
namespace Manager {
void CallerB() {
    Utils::RemoveConn(2);
}
}
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let target_fn = program
        .symbols
        .resolve_function("OHOS::Runtime::Utils::RemoveConn")
        .expect("resolved RemoveConn");
    let func = program.symbols.function(target_fn);
    assert!(func.is_defined);
    assert_eq!(
        &program.symbols.files[func.file.0 as usize].path,
        root.join("a_conn.cpp").as_path()
    );

    let (_pag, analysis) = analyze(&program);
    let callers = ["OHOS::Runtime::CallerA", "OHOS::Manager::CallerB"];
    for caller in callers {
        let caller_id = program
            .symbols
            .resolve_function(caller)
            .unwrap_or_else(|| panic!("missing {caller}"));
        let callees: Vec<FnId> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == caller_id && e.resolution == ResolutionKind::Direct)
            .map(|e| e.callee)
            .collect();
        assert_eq!(
            callees,
            vec![target_fn],
            "{caller} must resolve directly to OHOS::Runtime::Utils::RemoveConn"
        );
    }
}

#[test]
fn cpp_template_base_keeps_arguments_on_their_own_nested_class() {
    let (p, _) = cpp_issue113();
    let bases = p.template_bases_of("NestedDerived");
    assert_eq!(bases.len(), 1);
    assert_eq!(bases[0].spelling, "NestedBase<int>::Inner<double>");
}

#[test]
fn cpp_template_return_uses_smart_pointer_pointee_arguments() {
    let (p, a) = cpp_issue113();
    for caller in ["arrow_return::custom", "standard_arrow"] {
        assert!(
            has_any_edge(p, a, caller, "arrow_return::Holder::Get"),
            "{caller} member"
        );
        assert!(
            has_any_edge(p, a, caller, "arrow_return::Service::Start"),
            "{caller} return"
        );
    }
}

#[test]
fn distinct_definitions_indexed_separately_and_context_aware_call_resolution() {
    let dir = tempfile::Builder::new()
        .prefix("trace_distinct_defs_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
namespace Ns {
void Common(int x);
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a_first.cpp"),
        r#"
#include "common.h"
namespace Ns {
void Common(int x) {}
void CallInA() { Common(1); }
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b_second.cpp"),
        r#"
#include "common.h"
namespace Ns {
void Common(int x) {}
void CallInB() { Common(2); }
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("c_manager.cpp"),
        r#"
#include "common.h"
namespace Ns {
void CallInC() { Common(3); }
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let defs: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Ns::Common" && f.is_defined)
        .collect();
    assert_eq!(defs.len(), 2, "both definitions must be indexed separately");
    let fn_a = defs
        .iter()
        .find(|f| program.symbols.files[f.file.0 as usize].path == root.join("a_first.cpp"))
        .unwrap()
        .id;
    let fn_b = defs
        .iter()
        .find(|f| program.symbols.files[f.file.0 as usize].path == root.join("b_second.cpp"))
        .unwrap()
        .id;

    let (_pag, analysis) = analyze(&program);

    let get_callees = |caller_name: &str| -> Vec<FnId> {
        let caller_id = program
            .symbols
            .resolve_function(caller_name)
            .unwrap_or_else(|| panic!("missing {caller_name}"));
        analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == caller_id && e.resolution == ResolutionKind::Direct)
            .map(|e| e.callee)
            .collect()
    };

    // Caller in unit A resolves exclusively to definition in A:
    assert_eq!(get_callees("Ns::CallInA"), vec![fn_a]);
    // Caller in unit B resolves exclusively to definition in B:
    assert_eq!(get_callees("Ns::CallInB"), vec![fn_b]);
    // Caller in unit C (which saw only the header declaration) treats definitions as equal and resolves to both:
    let mut callees_c = get_callees("Ns::CallInC");
    callees_c.sort();
    let mut expected_c = vec![fn_a, fn_b];
    expected_c.sort();
    assert_eq!(callees_c, expected_c);
}

#[test]
fn regression_declaration_only_overload_ranking() {
    // Comment 1: Ensure declaration-only exact-match overloads are not filtered out
    // before overload ranking in favor of viable but worse-matching definitions.
    let dir = tempfile::Builder::new()
        .prefix("trace_overload_decl_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("a.cpp"),
        r#"
void f(int x);
void f(double x) {}
void caller() {
    f(1);
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        r#"
void f(int x) {}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    let caller_id = program.symbols.resolve_function("caller").expect("caller");
    let f_int_defs: Vec<FnId> = program
        .symbols
        .functions_named("f")
        .into_iter()
        .filter(|&id| {
            let func = program.symbols.function(id);
            func.is_defined
                && func.params.len() == 1
                && program.symbols.variable(func.params[0]).type_id == program.types.int()
        })
        .collect();
    assert_eq!(f_int_defs.len(), 1, "f(int) definition must exist");
    let f_double_defs: Vec<FnId> = program
        .symbols
        .functions_named("f")
        .into_iter()
        .filter(|&id| {
            let func = program.symbols.function(id);
            func.is_defined
                && func.params.len() == 1
                && program.symbols.variable(func.params[0]).type_id == program.types.double()
        })
        .collect();
    assert_eq!(f_double_defs.len(), 1, "f(double) definition must exist");

    let callees: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller_id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert_eq!(
        callees, f_int_defs,
        "caller must resolve to f(int), not f(double)"
    );
}

#[test]
fn regression_relative_qualified_candidates_shadowing() {
    // Comment 4: Stop candidate search at the first enclosing scope that resolves the qualifier.
    let dir = tempfile::Builder::new()
        .prefix("trace_shadow_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("test.cpp"),
        r#"
namespace Outer {
namespace Utils {
void Run(int x) {}
}
namespace Inner {
namespace Utils {
void Run(int x) {}
}
void Caller() {
    Utils::Run(1);
}
}
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    let caller_id = program
        .symbols
        .resolve_function("Outer::Inner::Caller")
        .expect("caller");
    let inner_run = program
        .symbols
        .resolve_function("Outer::Inner::Utils::Run")
        .expect("inner Run");
    let outer_run = program
        .symbols
        .resolve_function("Outer::Utils::Run")
        .expect("outer Run");

    let callees: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller_id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert!(
        callees.contains(&inner_run),
        "caller must resolve to Outer::Inner::Utils::Run"
    );
    assert!(
        !callees.contains(&outer_run),
        "caller must NOT resolve to shadowed Outer::Utils::Run"
    );
}

fn check_header_helper(with_compile_commands: bool) {
    let dir = tempfile::Builder::new()
        .prefix("trace_header_helper_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
void Target();
static void Helper() {
    Target();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        r#"
#include "common.h"
void Target() {}
void CallerA() {
    Helper();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        r#"
#include "common.h"
void Target() {}
void CallerB() {
    Helper();
}
"#,
    )
    .unwrap();
    if with_compile_commands {
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([
                {"directory": root, "file": "a.cpp", "arguments": ["c++", "-c", "a.cpp"]},
                {"directory": root, "file": "b.cpp", "arguments": ["c++", "-c", "b.cpp"]}
            ])
            .to_string(),
        )
        .unwrap();
    }
    let program = build_program(root, &default_opts(root)).expect("build");
    let defs: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Target" && f.is_defined)
        .collect();
    assert_eq!(defs.len(), 2, "both Target definitions must exist");
    let fn_a = defs
        .iter()
        .find(|f| program.symbols.files[f.file.0 as usize].path == root.join("a.cpp"))
        .unwrap()
        .id;
    let fn_b = defs
        .iter()
        .find(|f| program.symbols.files[f.file.0 as usize].path == root.join("b.cpp"))
        .unwrap()
        .id;

    let (_pag, analysis) = analyze(&program);

    for edge in &analysis.call_edges {
        assert!(
            program.symbols.function_by_id(edge.caller).is_some(),
            "edge caller {:?} must exist in program.symbols",
            edge.caller
        );
        assert!(
            program.symbols.function_by_id(edge.callee).is_some(),
            "edge callee {:?} must exist in program.symbols",
            edge.callee
        );
    }

    let a_file_id = program
        .symbols
        .files
        .iter()
        .position(|f| f.path == root.join("a.cpp"))
        .map(|i| trace_ir::FileId(i as u32))
        .unwrap();
    let b_file_id = program
        .symbols
        .files
        .iter()
        .position(|f| f.path == root.join("b.cpp"))
        .map(|i| trace_ir::FileId(i as u32))
        .unwrap();

    // Call site from Helper in TU a.cpp has span.file pointing to common.h via LineMap, but tu = a.cpp:
    let cs_target_a = program
        .symbols
        .call_sites
        .iter()
        .find(|cs| cs.tu == Some(a_file_id) && cs.callee_name == "Target")
        .unwrap();
    assert_eq!(
        program.symbols.files[cs_target_a.span.file.0 as usize].path,
        root.join("common.h"),
        "call site inside Helper must have span.file pointing to common.h via LineMap"
    );
    assert_eq!(
        program.symbols.callees_of(cs_target_a),
        vec![fn_a],
        "Helper call site in TU a.cpp must resolve exclusively to Target in a.cpp"
    );

    let edge_a: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.call_site == cs_target_a.id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert_eq!(edge_a, vec![fn_a]);

    // Call site from Helper in TU b.cpp has span.file pointing to common.h via LineMap, but tu = b.cpp:
    let cs_target_b = program
        .symbols
        .call_sites
        .iter()
        .find(|cs| cs.tu == Some(b_file_id) && cs.callee_name == "Target")
        .unwrap();
    assert_eq!(
        program.symbols.files[cs_target_b.span.file.0 as usize].path,
        root.join("common.h"),
        "call site inside Helper must have span.file pointing to common.h via LineMap"
    );
    assert_eq!(
        program.symbols.callees_of(cs_target_b),
        vec![fn_b],
        "Helper call site in TU b.cpp must resolve exclusively to Target in b.cpp"
    );

    let edge_b: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.call_site == cs_target_b.id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert_eq!(edge_b, vec![fn_b]);

    let helpers: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "Helper")
        .collect();
    assert_eq!(helpers.len(), 1, "identical header definitions are shared");
    let helper = helpers[0].id;
    for name in ["CallerA", "CallerB"] {
        let caller = program.symbols.resolve_function(name).unwrap();
        let edges: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == caller)
            .map(|e| e.callee)
            .collect();
        assert_eq!(edges, vec![helper]);
    }
    let mut targets: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == helper)
        .map(|e| e.callee)
        .collect();
    targets.sort();
    let mut expected = vec![fn_a, fn_b];
    expected.sort();
    assert_eq!(
        targets, expected,
        "sharing retains both TU-specific bindings"
    );
}

#[test]
fn regression_call_inside_header_helper_resolves_to_local_def() {
    // Comment 2: Header-static function calling external function resolves exclusively
    // to the local definition in the translation unit containing the call.
    check_header_helper(true);
    check_header_helper(false);
}

fn check_header_callback_initializer(with_compile_commands: bool) {
    let dir = tempfile::Builder::new()
        .prefix("trace_header_cb_init_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
static void callback() {}
static void (*handler)() = callback;
static void helper() {
    handler();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        r#"
#include "common.h"
void caller_a() {
    helper();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        r#"
#include "common.h"
void caller_b() {
    helper();
}
"#,
    )
    .unwrap();
    if with_compile_commands {
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([
                {"directory": root, "file": "a.cpp", "arguments": ["c++", "-c", "a.cpp"]},
                {"directory": root, "file": "b.cpp", "arguments": ["c++", "-c", "b.cpp"]}
            ])
            .to_string(),
        )
        .unwrap();
    }
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for edge in &analysis.call_edges {
        assert!(
            program.symbols.function_by_id(edge.caller).is_some(),
            "caller {:?} must exist in symbols",
            edge.caller
        );
        assert!(
            program.symbols.function_by_id(edge.callee).is_some(),
            "callee {:?} must exist in symbols",
            edge.callee
        );
    }

    let helpers: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "helper" && f.is_defined)
        .collect();
    assert_eq!(helpers.len(), 1);
    let helper = helpers[0].id;
    let callbacks: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "callback" && f.is_defined)
        .collect();
    assert_eq!(callbacks.len(), 1);
    let expected = vec![callbacks[0].id];
    assert_each_site_of_targets(&program, &analysis, helper, &expected);
}

/// Every call site in `helper` resolves to exactly `expected` (sorted). Each
/// TU retains its own mutable callback storage, and both stores must remain
/// initialized even though their function targets are shared.
fn assert_each_site_of_targets(
    program: &Program,
    analysis: &AnalysisResult,
    helper: FnId,
    expected: &[FnId],
) {
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == helper)
        .collect();
    assert!(!sites.is_empty());
    for site in sites {
        let mut targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.call_site == site.id)
            .map(|e| e.callee)
            .collect();
        targets.sort();
        assert_eq!(targets, expected);
    }
}

fn check_header_array_callback_initializer(with_compile_commands: bool) {
    let dir = tempfile::Builder::new()
        .prefix("trace_header_arr_cb_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
static void cb0() {}
static void cb1() {}
static void (*handlers[])() = { cb0, cb1 };
static void helper(int idx) {
    handlers[idx]();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        r#"
#include "common.h"
void caller_a() {
    helper(0);
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        r#"
#include "common.h"
void caller_b() {
    helper(1);
}
"#,
    )
    .unwrap();
    if with_compile_commands {
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([
                {"directory": root, "file": "a.cpp", "arguments": ["c++", "-c", "a.cpp"]},
                {"directory": root, "file": "b.cpp", "arguments": ["c++", "-c", "b.cpp"]}
            ])
            .to_string(),
        )
        .unwrap();
    }
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    for edge in &analysis.call_edges {
        assert!(
            program.symbols.function_by_id(edge.caller).is_some(),
            "caller {:?} must exist in symbols",
            edge.caller
        );
        assert!(
            program.symbols.function_by_id(edge.callee).is_some(),
            "callee {:?} must exist in symbols",
            edge.callee
        );
    }

    let helpers: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "helper" && f.is_defined)
        .collect();
    assert_eq!(helpers.len(), 1);
    let helper = helpers[0].id;
    let mut expected = Vec::new();
    for name in ["cb0", "cb1"] {
        let callbacks: Vec<_> = program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == name && f.is_defined)
            .collect();
        assert_eq!(callbacks.len(), 1);
        expected.push(callbacks[0].id);
    }
    expected.sort();
    assert_each_site_of_targets(&program, &analysis, helper, &expected);
}

fn check_header_struct_callback_initializer(with_compile_commands: bool) {
    let dir = tempfile::Builder::new()
        .prefix("trace_header_struct_cb_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
struct Ops {
    void (*fn)();
};
static void callback() {}
static Ops ops = { callback };
static void helper() {
    ops.fn();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        r#"
#include "common.h"
void caller_a() {
    helper();
}
"#,
    )
    .unwrap();
    if with_compile_commands {
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([
                {"directory": root, "file": "a.cpp", "arguments": ["c++", "-c", "a.cpp"]}
            ])
            .to_string(),
        )
        .unwrap();
    }
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let helper = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "helper" && f.is_defined)
        .unwrap()
        .id;
    let callback = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "callback" && f.is_defined)
        .unwrap()
        .id;

    let edges: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == helper)
        .map(|e| e.callee)
        .collect();
    assert_eq!(
        edges,
        vec![callback],
        "helper must resolve struct member indirect call to callback"
    );
}

#[test]
fn regression_header_callback_initializer_scalar() {
    check_header_callback_initializer(true);
    check_header_callback_initializer(false);
}

#[test]
fn regression_header_callback_initializer_array() {
    check_header_array_callback_initializer(true);
    check_header_array_callback_initializer(false);
}

#[test]
fn regression_header_callback_initializer_struct() {
    check_header_struct_callback_initializer(true);
    check_header_struct_callback_initializer(false);
}

#[test]
fn regression_call_return_dataflow_isolation_across_tus() {
    // Comment 7: Align CallReturn dataflow expansion with translation-unit call resolution.
    let dir = tempfile::Builder::new()
        .prefix("trace_call_return_iso_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
int* GetPtr();
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        r#"
#include "common.h"
static int val_a = 10;
int* GetPtr() {
    return &val_a;
}
void CallerA() {
    int* p = GetPtr();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        r#"
#include "common.h"
static int val_b = 20;
int* GetPtr() {
    return &val_b;
}
void CallerB() {
    int* q = GetPtr();
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (pag, analysis) = trace_analysis::analyze_with_options(
        &program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );

    let val_a_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "val_a")
        .unwrap()
        .id;
    let val_b_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "val_b")
        .unwrap()
        .id;
    let p_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "p")
        .unwrap()
        .id;
    let q_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "q")
        .unwrap()
        .id;

    let val_a_loc = pag.var_location[&val_a_var];
    let val_b_loc = pag.var_location[&val_b_var];
    let p_node = pag.var_node[&p_var];
    let q_node = pag.var_node[&q_var];

    let p_pts = analysis.points_to.get(&p_node);
    let q_pts = analysis.points_to.get(&q_node);

    assert!(
        p_pts.is_some_and(|pts| pts.contains(&val_a_loc)),
        "p must point to val_a"
    );
    assert!(
        !p_pts.is_some_and(|pts| pts.contains(&val_b_loc)),
        "p must NOT receive return flow from b.cpp (val_b)"
    );

    assert!(
        q_pts.is_some_and(|pts| pts.contains(&val_b_loc)),
        "q must point to val_b"
    );
    assert!(
        !q_pts.is_some_and(|pts| pts.contains(&val_a_loc)),
        "q must NOT receive return flow from a.cpp (val_a)"
    );
}

#[test]
fn regression_shape_equivalence_param_types_across_tus() {
    // Comment 6: Compare parameter types by shape equivalence rather than raw TypeId equality.
    let dir = tempfile::Builder::new()
        .prefix("trace_shape_eq_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
typedef int MyInt;
void Process(MyInt x);
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        r#"
#include "common.h"
void Process(int x) {}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        r#"
#include "common.h"
void Caller() {
    Process(42);
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    let caller_id = program.symbols.resolve_function("Caller").unwrap();
    let process_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "Process" && f.is_defined)
        .unwrap()
        .id;

    let callees: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller_id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert_eq!(
        callees,
        vec![process_fn],
        "Caller in b.cpp must resolve to Process in a.cpp via shape equivalence"
    );
}

#[test]
fn regression_transitive_return_flow_call_tu_isolation() {
    // Review Comment 1: Align ReturnFlow::Call dataflow expansion with translation-unit call resolution.
    let dir = tempfile::Builder::new()
        .prefix("trace_transitive_return_iso_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
int* GetPtr();
int* Forward();
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.cpp"),
        r#"
#include "common.h"
static int val_a = 10;
int* GetPtr() {
    return &val_a;
}
int* Forward() {
    return GetPtr();
}
void CallerA() {
    int* p = Forward();
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b.cpp"),
        r#"
#include "common.h"
static int val_b = 20;
int* GetPtr() {
    return &val_b;
}
int* Forward() {
    return GetPtr();
}
void CallerB() {
    int* q = Forward();
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (pag, analysis) = trace_analysis::analyze_with_options(
        &program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );

    let val_a_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "val_a")
        .unwrap()
        .id;
    let val_b_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "val_b")
        .unwrap()
        .id;
    let p_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "p")
        .unwrap()
        .id;
    let q_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "q")
        .unwrap()
        .id;

    let val_a_loc = pag.var_location[&val_a_var];
    let val_b_loc = pag.var_location[&val_b_var];
    let p_node = pag.var_node[&p_var];
    let q_node = pag.var_node[&q_var];

    let p_pts = analysis.points_to.get(&p_node);
    let q_pts = analysis.points_to.get(&q_node);

    assert!(
        p_pts.is_some_and(|pts| pts.contains(&val_a_loc)),
        "p must point to val_a via transitive ReturnFlow::Call"
    );
    assert!(
        !p_pts.is_some_and(|pts| pts.contains(&val_b_loc)),
        "p must NOT receive points-to from b.cpp (val_b) via ReturnFlow::Call"
    );

    assert!(
        q_pts.is_some_and(|pts| pts.contains(&val_b_loc)),
        "q must point to val_b via transitive ReturnFlow::Call"
    );
    assert!(
        !q_pts.is_some_and(|pts| pts.contains(&val_a_loc)),
        "q must NOT receive points-to from a.cpp (val_a) via ReturnFlow::Call"
    );
}

#[test]
fn regression_relative_qualified_template_callee() {
    // Review Comment 2: Avoid treating relative qualified template calls like Utils::Run<int>(x) as bare callee nodes.
    let dir = tempfile::Builder::new()
        .prefix("trace_rel_qual_tmpl_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("test.cpp"),
        r#"
namespace Ns {
    struct Utils {
        template <typename T>
        static void Run(T x);
    };
    template <typename T>
    void Utils::Run(T x) {}

    void Caller() {
        Utils::Run<int>(42);
    }
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let caller_id = program.symbols.resolve_function("Ns::Caller").unwrap();
    let run_id = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "Ns::Utils::Run" && f.is_defined)
        .expect("Ns::Utils::Run definition must exist")
        .id;

    let callees: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == caller_id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert_eq!(
        callees,
        vec![run_id],
        "Utils::Run<int>(42) inside namespace Ns must resolve to Ns::Utils::Run"
    );
}

#[test]
fn regression_in_class_member_decl_dedup_against_definition() {
    // Review Comment 3: Account for explicit_arity in has_same_signature for in-class member declarations.
    let dir = tempfile::Builder::new()
        .prefix("trace_in_class_dedup_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("worker.h"),
        r#"
#pragma once
struct Worker {
    static void Work(int a, int b);
};
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("worker.cpp"),
        r#"
#include "worker.h"
void Worker::Work(int a, int b) {}
void Trigger() {
    Worker::Work(1, 2);
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let trigger_id = program.symbols.resolve_function("Trigger").unwrap();
    let work_def = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "Worker::Work" && f.is_defined)
        .expect("Worker::Work definition must exist")
        .id;

    let callees: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == trigger_id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert_eq!(
        callees,
        vec![work_def],
        "Call to Worker::Work must resolve solely to definition, filtering out prototype declaration"
    );
}

#[test]
fn regression_cross_tu_member_fn_implicit_this_and_explicit_arity() {
    // Review Comment 4: Account for implicit this and parameterless prototypes in resolve_equal_defs.
    let dir = tempfile::Builder::new()
        .prefix("trace_member_cross_tu_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("calc.h"),
        r#"
#pragma once
struct Calculator {
    int Compute(int x);
    void Reset();
};
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("calc.cpp"),
        r#"
#include "calc.h"
int Calculator::Compute(int x) { return x * 2; }
void Calculator::Reset() {}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("user.cpp"),
        r#"
#include "calc.h"
void User(Calculator* c) {
    c->Compute(10);
    c->Reset();
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);

    let user_id = program.symbols.resolve_function("User").unwrap();
    let compute_def = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "Calculator::Compute" && f.is_defined)
        .expect("Calculator::Compute definition")
        .id;
    let reset_def = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "Calculator::Reset" && f.is_defined)
        .expect("Calculator::Reset definition")
        .id;

    let callees: Vec<FnId> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == user_id && e.resolution == ResolutionKind::Direct)
        .map(|e| e.callee)
        .collect();
    assert!(
        callees.contains(&compute_def),
        "User call to c->Compute must resolve across TUs to Calculator::Compute definition"
    );
    assert!(
        callees.contains(&reset_def),
        "User call to c->Reset must resolve across TUs to Calculator::Reset definition"
    );
}

#[test]
fn regression_merge_c_and_cpp_def_order_symmetric() {
    // Review Comment 5: Symmetrize is_incompatible_def check across C and C++ definitions to avoid merge-order non-determinism.
    let dir = tempfile::Builder::new()
        .prefix("trace_c_cpp_order_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("impl_cpp.cpp"),
        r#"
extern "C" void SharedTask() {}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("impl_c.c"),
        r#"
void SharedTask() {}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    assert!(
        program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "SharedTask"),
        "SharedTask should be indexed without error"
    );
}

#[test]
fn regression_bind_calls_past_this_shape_aware() {
    // Review Comment 6: Update bind_calls_past_this to call program.callees_of(cs) to preserve shape-aware type equivalence.
    let dir = tempfile::Builder::new()
        .prefix("trace_bind_past_this_shape_")
        .tempdir()
        .unwrap();
    let root_buf = dir.path().canonicalize().unwrap();
    let root = root_buf.as_path();
    std::fs::write(
        root.join("common.h"),
        r#"
#pragma once
struct Service {
    typedef int RequestId;
    void Handle(RequestId req, int flags);
};
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("service.cpp"),
        r#"
#include "common.h"
void Service::Handle(int req, int flags) {}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("caller.cpp"),
        r#"
#include "common.h"
void Dispatch(Service* s) {
    s->Handle(101, 1);
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let cs = program
        .symbols
        .call_sites
        .iter()
        .find(|c| c.callee_name.contains("Handle"))
        .expect("Handle call site must exist");
    assert!(
        cs.args_bound_past_this,
        "Call site to Service::Handle must have args_bound_past_this set via shape-aware callees_of"
    );
}

// --- cpp_smart_pointer_value_flow: value flow through wrappers (#141) ---

analyzed_fixture!(
    /// The `cpp_smart_pointer_value_flow` fixture, analysed once.
    cpp_smart_pointer_value_flow
);

/// Facts lowered into function `func`, by their destination's owner.
fn flows_in<'a>(
    program: &'a Program,
    func: &str,
) -> impl Iterator<Item = &'a trace_ir::FlowConstraint> {
    let fn_id = common::only_function(program, func);
    program.flow.iter().filter(move |flow| {
        let dst = flow.vars().next().expect("every fact names a destination");
        program.symbols.variable(dst).fn_id == Some(fn_id)
    })
}

/// `(src, dst)` of every `UnwrapPointer` in `func`.
fn unwraps_in(program: &Program, func: &str) -> Vec<(trace_ir::VarId, trace_ir::VarId)> {
    flows_in(program, func)
        .filter_map(|flow| match *flow {
            trace_ir::FlowConstraint::UnwrapPointer { dst, src } => Some((src, dst)),
            _ => None,
        })
        .collect()
}

/// Whether the lowered facts carry a value from `from` to `to`, following
/// copies, unwraps, field projections and loads forward.
fn value_reaches(program: &Program, from: trace_ir::VarId, to: trace_ir::VarId) -> bool {
    use trace_ir::FlowConstraint as F;
    let mut seen = std::collections::HashSet::from([from]);
    let mut stack = vec![from];
    while let Some(cur) = stack.pop() {
        if cur == to {
            return true;
        }
        for flow in &program.flow {
            let next = match *flow {
                F::Copy { dst, src } | F::Load { dst, src } | F::UnwrapPointer { dst, src }
                    if src == cur =>
                {
                    dst
                }
                F::GepField { dst, base, .. } if base == cur => dst,
                _ => continue,
            };
            if seen.insert(next) {
                stack.push(next);
            }
        }
    }
    false
}

#[test]
fn smart_pointer_flow_reaches_fields_through_the_wrapper_value() {
    let (program, _) = cpp_smart_pointer_value_flow();
    let input = common::local_variable(program, "read", "input");
    let sp = common::local_variable(program, "read", "sp");
    let value = common::local_variable(program, "read", "value");
    assert!(
        unwraps_in(program, "read")
            .iter()
            .any(|&(src, _)| src == sp),
        "sp->field unwraps sp itself"
    );
    assert!(value_reaches(program, input, value), "input -> sp -> value");
}

#[test]
fn smart_pointer_flow_keeps_the_pointee_callback_summary() {
    // Regression guard: passes before #141 through the type-keyed summary.
    let (program, analysis) = cpp_smart_pointer_value_flow();
    assert!(has_any_edge(program, analysis, "read", "PayloadTarget"));
}

#[test]
fn smart_pointer_flow_loads_a_wrapper_member_before_unwrapping_it() {
    let (program, _) = cpp_smart_pointer_value_flow();
    for (func, result, unwraps) in [
        ("nested", "nested_value", 1),
        ("nested_dot", "dot_value", 1),
        ("twice", "twice_value", 2),
        ("deref_dot", "deref_dot_value", 1),
        ("deref_twice", "deref_twice_value", 2),
    ] {
        let h = common::local_variable(program, func, "h");
        let found = unwraps_in(program, func);
        assert_eq!(
            found.len(),
            unwraps,
            "{func}: one unwrap per overloaded arrow"
        );
        // The unwrap into the `Payload` receiver reads the loaded `item`
        // value, never the holder.
        let (item_value, _) = *found.last().expect("an unwrap");
        assert_ne!(item_value, h, "{func}: the holder is not the wrapper");
        assert!(
            flows_in(program, func).any(|flow| matches!(
                flow,
                trace_ir::FlowConstraint::GepField { field_name, .. } if field_name == "item"
            )),
            "{func}: the path keeps its item prefix"
        );
        assert!(
            flows_in(program, func).any(|flow| matches!(
                *flow,
                trace_ir::FlowConstraint::Load { dst, .. } if dst == item_value
            )),
            "{func}: the unwrapped item is a loaded value"
        );
        let result = common::local_variable(program, func, result);
        assert!(value_reaches(program, h, result), "{func}: h -> {result:?}");
    }
}

#[test]
fn smart_pointer_flow_method_call_leaves_no_receiver() {
    let (program, _) = cpp_smart_pointer_value_flow();
    let fn_id = common::only_function(program, "method_probe");
    assert!(unwraps_in(program, "method_probe").is_empty());
    assert!(
        !program
            .symbols
            .variables
            .iter()
            .any(|v| v.fn_id == Some(fn_id) && v.name.starts_with('_')),
        "no temporaries for a method call"
    );
}

/// Write `files` under a fresh temporary root and build it.
fn build_tree(
    files: &[(&str, &str)],
    opts: impl Fn(&std::path::Path) -> trace_preproc::PreprocessOptions,
) -> (tempfile::TempDir, Program) {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let program = build_program(dir.path(), &opts(dir.path())).expect("build");
    (dir, program)
}

/// Every unwrap names live variables of one function; returns that function
/// per unwrap.
fn unwrap_owners(program: &Program) -> Vec<FnId> {
    program
        .flow
        .iter()
        .filter_map(|flow| match *flow {
            trace_ir::FlowConstraint::UnwrapPointer { dst, src } => {
                let dst = program.symbols.variable_by_id(dst).expect("live receiver");
                let src = program.symbols.variable_by_id(src).expect("live source");
                assert_eq!(dst.fn_id, src.fn_id, "an unwrap stays in one function");
                assert!(dst.name.starts_with("_recv"), "{}", dst.name);
                dst.fn_id
            }
            _ => None,
        })
        .collect()
}

const SMART_PAYLOAD: &str = "struct Payload { void (*cb)(); };\n";

#[test]
fn smart_pointer_flow_shared_header_unwrap_merges_once() {
    let header =
        format!("{SMART_PAYLOAD}inline void Touch(std::shared_ptr<Payload> sp) {{ sp->cb(); }}\n");
    let (_dir, program) = build_tree(
        &[
            ("payload.h", &header),
            ("a.cpp", "#include \"payload.h\"\nvoid UseA() {}\n"),
            ("b.cpp", "#include \"payload.h\"\nvoid UseB() {}\n"),
        ],
        default_opts,
    );
    let touch = common::only_function(&program, "Touch");
    assert_eq!(
        unwrap_owners(&program),
        [touch],
        "one remapped unwrap for the shared body"
    );
}

#[test]
fn smart_pointer_flow_explored_variants_keep_their_own_receivers() {
    let source = format!(
        "{SMART_PAYLOAD}#if defined(FEATURE_ALPHA)\nvoid Alpha(std::shared_ptr<Payload> sp) {{ sp->cb(); }}\n#else\nvoid Beta(std::shared_ptr<Payload> sp) {{ sp->cb(); }}\n#endif\n"
    );
    let (_dir, program) = build_tree(
        &[
            (
                "BUILD.gn",
                "config(\"c\") { defines = [ \"FEATURE_ALPHA\" ] }\n",
            ),
            ("main.cpp", &source),
        ],
        |_| {
            trace_preproc::PreprocessOptions::new()
                .with_explore(true)
                .with_explore_budget(4)
        },
    );
    let mut owners = unwrap_owners(&program);
    owners.sort();
    let mut expected = vec![
        common::only_function(&program, "Alpha"),
        common::only_function(&program, "Beta"),
    ];
    expected.sort();
    assert_eq!(owners, expected, "one receiver per variant's body");
}

#[test]
fn smart_pointer_flow_summaries_stay_in_their_link_target() {
    // Regression guard: each image's receivers read its own `Payload.cb`
    // summary, so `A` and `B` never see each other's callback.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("payload.h"), SMART_PAYLOAD).unwrap();
    let mut compiles = Vec::new();
    let mut links = Vec::new();
    for image in ["A", "B"] {
        let name = image.to_lowercase();
        std::fs::write(
            root.join(format!("{name}.cpp")),
            format!(
                "#include \"payload.h\"\nvoid Target{image}() {{}}\nvoid Set{image}(Payload *p) {{ p->cb = Target{image}; }}\nvoid Read{image}(std::shared_ptr<Payload> sp) {{ sp->cb(); }}\n"
            ),
        )
        .unwrap();
        compiles.push(serde_json::json!({"directory": root, "file": format!("{name}.cpp"), "output": format!("{name}.o"), "arguments": ["c++", "-c", format!("{name}.cpp"), "-o", format!("{name}.o")]}));
        links.push(serde_json::json!({"directory": root, "output": name, "arguments": ["c++", format!("{name}.o"), "-o", name]}));
    }
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!(compiles).to_string(),
    )
    .unwrap();
    std::fs::write(
        root.join("link_commands.json"),
        serde_json::json!(links).to_string(),
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = analyze(&program);
    assert!(has_any_edge(&program, &analysis, "ReadA", "TargetA"));
    assert!(has_any_edge(&program, &analysis, "ReadB", "TargetB"));
    assert!(must_not_have_edge(&program, &analysis, "ReadA", "TargetB"));
    assert!(must_not_have_edge(&program, &analysis, "ReadB", "TargetA"));
}

#[test]
fn smart_pointer_promotion_result_aliases_the_receiver() {
    let (program, analysis) = cpp_smart_pointer_value_flow();
    // The call itself is recorded as before.
    assert!(has_any_edge(
        program,
        analysis,
        "promote_local",
        "std::weak_ptr::lock"
    ));
    assert!(has_any_edge(
        program,
        analysis,
        "promote_ohos",
        "OHOS::wptr::promote"
    ));
    for (func, results) in [
        ("promote_local", &["promoted", "promoted_value"][..]),
        ("promote_assign", &["assigned", "assigned_value"]),
        ("promote_ohos", &["strong"]),
        ("promote_nested_ns", &["nested_strong"]),
        ("promote_unnamed", &["unnamed"]),
    ] {
        let wp = common::local_variable(program, func, "wp");
        for result in results {
            let result_var = common::local_variable(program, func, result);
            assert!(
                value_reaches(program, wp, result_var),
                "{func}: wp -> {result}"
            );
        }
    }
}

#[test]
fn smart_pointer_promotion_of_a_field_reads_the_loaded_weak_value() {
    let (program, _) = cpp_smart_pointer_value_flow();
    for (func, result) in [
        ("promote_field", "field_promoted"),
        ("promote_paren", "paren_promoted"),
    ] {
        let msg = common::local_variable(program, func, "msg");
        let promoted = common::local_variable(program, func, result);
        let weak_value = promotion_source(program, func, result);
        assert_ne!(
            weak_value, msg,
            "{func}: the receiver is msg->weak, not msg"
        );
        assert!(
            flows_in(program, func).any(|flow| matches!(
                *flow,
                trace_ir::FlowConstraint::Load { dst, .. } if dst == weak_value
            )),
            "{func}: the weak value is loaded from the field"
        );
        assert!(
            value_reaches(program, msg, promoted),
            "{func}: msg -> {result}"
        );
    }
    let msg = common::local_variable(program, "promote_field", "msg");
    let field_value = common::local_variable(program, "promote_field", "field_value");
    assert!(value_reaches(program, msg, field_value));
}

#[test]
fn smart_pointer_promotion_ignores_non_promotions() {
    let (program, analysis) = cpp_smart_pointer_value_flow();
    for (func, receiver, result) in [
        ("mutex_lock", "m", "locked"),
        ("promote_custom", "wp", "custom_strong"),
        ("promote_with_arg", "wp", "with_arg"),
    ] {
        let receiver = common::local_variable(program, func, receiver);
        let result_var = common::local_variable(program, func, result);
        assert!(
            !value_reaches(program, receiver, result_var),
            "{func}: {result} must not alias the receiver"
        );
    }
    // The defined wrapper's own member is still the call's target.
    assert!(has_any_edge(
        program,
        analysis,
        "promote_custom",
        "custom::wptr::promote"
    ));
}

/// The source of the one receiver copy into `result` in `func`.
fn promotion_source(program: &Program, func: &str, result: &str) -> trace_ir::VarId {
    let result = common::local_variable(program, func, result);
    let sources: Vec<_> = flows_in(program, func)
        .filter_map(|flow| match *flow {
            trace_ir::FlowConstraint::Copy { dst, src } if dst == result => Some(src),
            _ => None,
        })
        .collect();
    assert_eq!(sources.len(), 1, "{func}: one receiver copy");
    sources[0]
}

#[test]
fn smart_pointer_promotion_of_a_call_result_reads_the_returned_value() {
    let (program, _) = cpp_smart_pointer_value_flow();
    let src = promotion_source(program, "promote_call_result", "call_promoted");
    assert!(
        flows_in(program, "promote_call_result").any(|flow| matches!(
            flow,
            trace_ir::FlowConstraint::CallReturn { dst, callee_name, .. }
                if *dst == src && callee_name == "get_weak"
        )),
        "the receiver value is get_weak()'s result"
    );
}

#[test]
fn smart_pointer_promotion_of_a_dereferenced_member_reads_the_member() {
    let (program, _) = cpp_smart_pointer_value_flow();
    let func = "promote_deref_field";
    let h = common::local_variable(program, func, "h");
    let promoted = common::local_variable(program, func, "deref_promoted");
    assert!(
        !flows_in(program, func)
            .any(|flow| matches!(*flow, trace_ir::FlowConstraint::Load { src, .. } if src == h)),
        "the holder is not loaded through"
    );
    assert!(
        flows_in(program, func).any(|flow| matches!(
            flow,
            trace_ir::FlowConstraint::GepField { field_name, .. } if field_name == "weak"
        )),
        "the weak member is read"
    );
    assert!(value_reaches(program, h, promoted));
}

#[test]
fn smart_pointer_promotion_of_a_reference_reads_through_it() {
    let (program, _) = cpp_smart_pointer_value_flow();
    let weak = common::local_variable(program, "promote_ref", "weak");
    let src = promotion_source(program, "promote_ref", "ref_strong");
    assert!(
        flows_in(program, "promote_ref").any(|flow| matches!(
            *flow,
            trace_ir::FlowConstraint::Load { dst, src: from } if dst == src && from == weak
        )),
        "the wptr the reference names is loaded, not the reference copied"
    );
}

#[test]
fn smart_pointer_promotion_stores_the_receiver_value_directly() {
    let (program, _) = cpp_smart_pointer_value_flow();
    for func in [
        "promote_into_field",
        "promote_into_field_paren",
        "promote_into_deref",
        "promote_into_deref_paren",
    ] {
        let wp = common::local_variable(program, func, "wp");
        assert!(
            flows_in(program, func).any(
                |flow| matches!(*flow, trace_ir::FlowConstraint::Store { src, .. } if src == wp)
            ),
            "{func}: no temporary between the receiver and the store"
        );
    }
}

#[test]
fn smart_pointer_flow_dereferenced_member_value_has_the_wrapper_type() {
    let (program, _) = cpp_smart_pointer_value_flow();
    for func in ["deref_dot", "deref_twice"] {
        let (member_value, _) = *unwraps_in(program, func).last().expect("an unwrap");
        let type_id = program.symbols.variable(member_value).type_id;
        let desc = program.types.get(type_id).desc.as_ref().clone();
        assert!(
            !matches!(&desc, trace_ir::TypeDesc::Struct { name, .. } if name == "Holder"),
            "{func}: the loaded member is not typed as its holder: {desc:?}"
        );
    }
}

/// The `cpp_smart_pointer_value_flow` fixture's points-to sets, solved once
/// with `retain_points_to`.
fn smart_pointer_points_to() -> &'static (trace_analysis::Pag, AnalysisResult) {
    static CACHE: OnceLock<(trace_analysis::Pag, AnalysisResult)> = OnceLock::new();
    CACHE.get_or_init(|| {
        let (program, _) = cpp_smart_pointer_value_flow();
        trace_analysis::analyze_with_options(
            program,
            trace_analysis::AnalyzeOptions {
                retain_points_to: true,
                ..Default::default()
            },
        )
    })
}

/// Names of the variables whose storage each unwrap receiver in `func`
/// points to, one sorted list per receiver, in lowering order.
fn receiver_pointees(func: &str) -> Vec<Vec<String>> {
    let (program, _) = cpp_smart_pointer_value_flow();
    let (pag, analysis) = smart_pointer_points_to();
    unwraps_in(program, func)
        .into_iter()
        .map(|(_, receiver)| common::points_to_names_of_var(program, pag, analysis, receiver))
        .collect()
}

#[test]
fn smart_pointer_flow_storage_and_value_stay_in_step() {
    // Stored through `&filled`, read as `filled`; copied into `copied`, read
    // through `*w`.
    assert_eq!(receiver_pointees("ReadFilled"), [["g_filled"]]);
    assert_eq!(receiver_pointees("ReadThrough"), [["g_copied"]]);
}

#[test]
fn smart_pointer_flow_argument_reaches_the_parameter() {
    assert_eq!(receiver_pointees("UseArg"), [["g_passed"]]);
    assert_eq!(receiver_pointees("UseLate"), [["g_late"]]);
}

#[test]
fn smart_pointer_flow_path_rooted_at_a_call_result() {
    let (program, _) = cpp_smart_pointer_value_flow();
    let unwraps = unwraps_in(program, "ReadReturned");
    assert_eq!(unwraps.len(), 2, "GetSp()->cb and GetSp()->value");
    for (wrapper_value, _) in &unwraps {
        assert!(
            flows_in(program, "ReadReturned").any(|flow| matches!(
                flow,
                trace_ir::FlowConstraint::CallReturn { dst, callee_name, .. }
                    if dst == wrapper_value && callee_name == "GetSp"
            )),
            "each unwrap reads GetSp()'s result"
        );
        let type_id = program.symbols.variable(*wrapper_value).type_id;
        assert!(
            matches!(
                program.types.get(type_id).desc.as_ref(),
                trace_ir::TypeDesc::Struct { name, .. } if name == "OHOS::sptr<Payload>"
            ),
            "the call's result is typed as the wrapper it returns"
        );
    }
    assert_eq!(
        receiver_pointees("ReadReturned"),
        [["g_returned"], ["g_returned"]]
    );
}

#[test]
fn implicit_this_member_assignment_flow_and_points_to() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
struct Target {
    int val;
};

class Observer {
    Target *target_ = nullptr;
public:
    void SetTarget(Target *t) {
        target_ = t;
    }
    Target* GetTarget() {
        return target_;
    }
};

static Target g_target;

void run() {
    Observer obs;
    obs.SetTarget(&g_target);
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");

    // Acceptance criterion 1: Running trace analyze on the reproducer emits a
    // FlowConstraint::Store constraint storing `t` into `this->target_`.
    let gep_target = program
        .flow
        .iter()
        .find_map(|f| match f {
            trace_ir::FlowConstraint::GepField {
                dst,
                base,
                field_name,
                ..
            } if field_name == "target_" && program.symbols.variable(*base).name == "this" => {
                Some(*dst)
            }
            _ => None,
        })
        .expect("must emit GepField for this->target_");

    let has_store = program.flow.iter().any(|f| match f {
        trace_ir::FlowConstraint::Store { dst, src } => {
            *dst == gep_target && program.symbols.variable(*src).name == "t"
        }
        _ => false,
    });
    assert!(
        has_store,
        "must emit FlowConstraint::Store storing t into this->target_, got {:?}",
        program.flow
    );

    // Acceptance criterion 2: Inspecting points-to sets demonstrates that
    // `target_` contains `g_target`.
    let (pag, analysis) = trace_analysis::analyze_with_options(
        &program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );

    let gep_node = pag.var_node.get(&gep_target).expect("GEP PAG node");
    let gep_pts = analysis.points_to.get(gep_node).expect("GEP points-to");
    assert!(
        !gep_pts.is_empty(),
        "GEP node must point to the field summary location"
    );

    let summary_loc = gep_pts.iter().copied().next().unwrap();
    assert_eq!(
        pag.locations[summary_loc.0 as usize].desc,
        "summary:Observer.target_"
    );

    let g_target_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "g_target")
        .expect("g_target variable")
        .id;
    let g_target_loc = pag
        .var_location
        .get(&g_target_var)
        .copied()
        .expect("g_target loc");

    let t_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "t" && v.fn_id.is_some())
        .expect("t variable")
        .id;
    let t_node = pag.var_node.get(&t_var).expect("t PAG node");
    let t_pts = analysis.points_to.get(t_node).expect("t points-to");
    assert!(
        t_pts.contains(&g_target_loc),
        "parameter t must point to g_target; pts = {t_pts:?}"
    );
}

#[test]
fn memory_pressure_observer_implicit_this_store() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
struct epoll_data {
    void *ptr;
};
struct epoll_event {
    epoll_data data;
};
struct LevelHandler {};

class MemoryPressureObserver {
    struct LevelHandler *handlerInfo_ = nullptr;
public:
    void HandleEpollEvent(struct epoll_event *curEpollEvent) {
        handlerInfo_ = (struct LevelHandler*)curEpollEvent->data.ptr;
    }
};
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");

    // Acceptance criterion 3: In memory_pressure_observer.cpp,
    // handlerInfo_ = (struct LevelHandler*)curEpollEvent->data.ptr; emits a Store to this->handlerInfo_.
    let gep_handler = program
        .flow
        .iter()
        .find_map(|f| match f {
            trace_ir::FlowConstraint::GepField {
                dst,
                base,
                field_name,
                ..
            } if field_name == "handlerInfo_" && program.symbols.variable(*base).name == "this" => {
                Some(*dst)
            }
            _ => None,
        })
        .expect("must emit GepField for this->handlerInfo_");

    let has_store = program.flow.iter().any(|f| match f {
        trace_ir::FlowConstraint::Store { dst, src } => {
            *dst == gep_handler && program.symbols.variable(*src).name == "curEpollEvent"
        }
        _ => false,
    });
    assert!(
        has_store,
        "must emit FlowConstraint::Store to this->handlerInfo_, got {:?}",
        program.flow
    );
}

#[test]
fn inherited_and_nested_implicit_this_member_points_to() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
struct Target {
    int val;
};

struct Inner {
    Target *target_ptr = nullptr;
};

class Base {
protected:
    Inner inner_;
};

class Derived : public Base {
public:
    void SetTarget(Target *t) {
        inner_.target_ptr = t;
    }
};

static Target g_target;

void run() {
    Derived d;
    d.SetTarget(&g_target);
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (pag, analysis) = trace_analysis::analyze_with_options(
        &program,
        trace_analysis::AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );

    assert!(
        has_direct(&program, &analysis, "run", "Derived::SetTarget"),
        "direct call run -> Derived::SetTarget must be present"
    );

    let gep_target_ptr = program
        .flow
        .iter()
        .find_map(|f| match f {
            trace_ir::FlowConstraint::GepField {
                dst, field_name, ..
            } if field_name == "target_ptr" => Some(*dst),
            _ => None,
        })
        .expect("must emit GEP for target_ptr");

    let has_store = program.flow.iter().any(|f| match f {
        trace_ir::FlowConstraint::Store { dst, src } => {
            *dst == gep_target_ptr && program.symbols.variable(*src).name == "t"
        }
        _ => false,
    });
    assert!(has_store, "must emit Store of t into inner_.target_ptr");

    let gep_node = pag.var_node.get(&gep_target_ptr).expect("GEP PAG node");
    let gep_pts = analysis.points_to.get(gep_node).expect("GEP points-to");
    assert!(
        !gep_pts.is_empty(),
        "GEP node must point to the field summary location"
    );

    let summary_loc = gep_pts
        .iter()
        .copied()
        .find(|l| pag.locations[l.0 as usize].desc == "summary:Inner.target_ptr")
        .expect("gep_target_ptr must point to summary:Inner.target_ptr");
    assert_eq!(
        pag.locations[summary_loc.0 as usize].desc,
        "summary:Inner.target_ptr"
    );

    let g_target_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "g_target")
        .expect("g_target variable")
        .id;
    let g_target_loc = pag
        .var_location
        .get(&g_target_var)
        .copied()
        .expect("g_target loc");

    let t_var = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "t" && v.fn_id.is_some())
        .expect("t variable")
        .id;
    let t_node = pag.var_node.get(&t_var).expect("t PAG node");
    let t_pts = analysis.points_to.get(t_node).expect("t points-to");
    assert!(
        t_pts.contains(&g_target_loc),
        "parameter t must point to g_target; pts = {t_pts:?}"
    );
}

#[test]
fn implicit_this_member_callback_flow_and_call_edge() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
typedef void (*HandlerFn)();

void my_handler() {}

class Controller {
    HandlerFn callback_ = nullptr;
public:
    void Init() {
        callback_ = my_handler;
    }
    void Trigger() {
        callback_();
    }
};

void run() {
    Controller c;
    c.Init();
    c.Trigger();
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = trace_analysis::analyze(&program);

    let has_call = analysis.call_edges.iter().any(|e| {
        fn_name(&program, e.caller).ends_with("Trigger")
            && fn_name(&program, e.callee) == "my_handler"
    });
    assert!(
        has_call,
        "Controller::Trigger must resolve call to my_handler; call edges: {:?}",
        analysis
            .call_edges
            .iter()
            .map(|e| (fn_name(&program, e.caller), fn_name(&program, e.callee)))
            .collect::<Vec<_>>()
    );
}

#[test]
fn local_array_shadows_implicit_this_member_subscript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
typedef void (*Callback)();
void expected() {}
void wrong() {}
struct C {
    Callback callbacks[1];
    void run() {
        callbacks[0] = wrong;
        Callback callbacks[1] = { expected };
        Callback f = callbacks[0];
        f();
    }
};
struct C2 {
    Callback callbacks[1];
    Callback get_member() {
        return callbacks[0];
    }
    Callback get_shadowed() {
        Callback callbacks[1] = { expected };
        return callbacks[0];
    }
};
void entry() {
    C c;
    c.run();
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = trace_analysis::analyze(&program);

    let has_expected = analysis.call_edges.iter().any(|e| {
        fn_name(&program, e.caller).ends_with("run") && fn_name(&program, e.callee) == "expected"
    });
    let has_wrong = analysis.call_edges.iter().any(|e| {
        fn_name(&program, e.caller).ends_with("run") && fn_name(&program, e.callee) == "wrong"
    });
    assert!(
        has_expected,
        "C::run must call expected via shadowed local array; call edges: {:?}",
        analysis
            .call_edges
            .iter()
            .map(|e| (fn_name(&program, e.caller), fn_name(&program, e.callee)))
            .collect::<Vec<_>>()
    );
    assert!(
        !has_wrong,
        "C::run must not call wrong when local array shadows member"
    );

    // Verify return_flow_from_expr shadowing:
    // C2::get_member should return a load from the implicit this member GEP.
    let get_member_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name.ends_with("get_member"))
        .expect("get_member fn")
        .id;
    let get_member_returns = program
        .fn_returns
        .get(&get_member_fn)
        .expect("get_member returns");
    assert_eq!(get_member_returns.len(), 1);
    let member_ret_src = match &get_member_returns[0] {
        trace_ir::ReturnFlow::Copy { src } => *src,
        other => panic!("expected ReturnFlow::Copy, got {other:?}"),
    };
    // The member load temp is loaded from a GEP on this
    let loads_from_gep = program.flow.iter().any(|f| match f {
        trace_ir::FlowConstraint::Load { dst, .. } => *dst == member_ret_src,
        _ => false,
    });
    assert!(
        loads_from_gep,
        "get_member must load return value from implicit this member GEP"
    );

    // C2::get_shadowed should return the local array variable directly, not loading from GEP.
    let get_shadowed_fn = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name.ends_with("get_shadowed"))
        .expect("get_shadowed fn")
        .id;
    let get_shadowed_returns = program
        .fn_returns
        .get(&get_shadowed_fn)
        .expect("get_shadowed returns");
    assert_eq!(get_shadowed_returns.len(), 1);
    let shadowed_ret_src = match &get_shadowed_returns[0] {
        trace_ir::ReturnFlow::Copy { src } => *src,
        other => panic!("expected ReturnFlow::Copy, got {other:?}"),
    };
    let shadowed_var = program.symbols.variable(shadowed_ret_src);
    assert_eq!(shadowed_var.name, "callbacks");
    assert_eq!(
        shadowed_var.fn_id,
        Some(get_shadowed_fn),
        "shadowed return must reference local variable"
    );
}

#[test]
fn derived_static_function_hides_base_field_designator() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
typedef void (*Callback)();
struct Base { Callback foo; };
struct Derived : Base {
    static void foo() {}
    void run() { Callback p = foo; p(); }
};
void entry() { Derived d; d.run(); }
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, analysis) = trace_analysis::analyze(&program);

    let has_derived_foo = analysis.call_edges.iter().any(|e| {
        fn_name(&program, e.caller).ends_with("run") && fn_name(&program, e.callee).ends_with("foo")
    });
    assert!(
        has_derived_foo,
        "Derived::run must call Derived::foo; call edges: {:?}",
        analysis
            .call_edges
            .iter()
            .map(|e| (fn_name(&program, e.caller), fn_name(&program, e.callee)))
            .collect::<Vec<_>>()
    );
}

#[test]
fn implicit_this_member_address_of() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
struct Target { int val; };
class Container {
    Target target_;
public:
    Target* get_ptr() {
        return &target_;
    }
    void run() {
        Target* p = &target_;
        p->val = 42;
    }
};
void entry() {
    Container c;
    c.run();
    Target* ptr = c.get_ptr();
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, _analysis) = trace_analysis::analyze(&program);

    // Verify &target_ emits GepField on this->target_ and not an unresolved AddrOfFn
    let gep_targets = program
        .flow
        .iter()
        .filter_map(|f| match f {
            trace_ir::FlowConstraint::GepField {
                dst,
                base,
                field_name,
                ..
            } if field_name == "target_" && program.symbols.variable(*base).name == "this" => {
                Some(*dst)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !gep_targets.is_empty(),
        "must emit GepField for this->target_ in address-of expressions"
    );

    // Ensure pending function references did not treat target_ as a function
    assert!(
        !program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "target_"),
        "target_ member variable must not be registered as a function"
    );
}

#[test]
fn implicit_this_member_casted_and_parenthesized_call_store() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("main.cpp"),
        r#"
struct Target { int x; };
Target* make_target() { return nullptr; }
typedef Target* (*FactoryFn)();

class Holder {
    Target* member_ = nullptr;
    Target* member2_ = nullptr;
public:
    void test_direct() {
        member_ = (Target*)make_target();
        member2_ = (make_target());
    }
    void test_indirect(FactoryFn fn) {
        member_ = (Target*)fn();
        member2_ = (fn());
    }
};
void run(FactoryFn fn) {
    Holder h;
    h.test_direct();
    h.test_indirect(fn);
}
"#,
    )
    .unwrap();
    let program = build_program(root, &default_opts(root)).expect("build");
    let (_pag, _analysis) = trace_analysis::analyze(&program);

    // Direct and indirect call stores
    let stores = program
        .flow
        .iter()
        .filter(|f| matches!(f, trace_ir::FlowConstraint::Store { .. }))
        .count();
    assert!(
        stores >= 4,
        "must emit Store constraints for member_ and member2_ across direct and indirect calls, found {stores}"
    );

    // Check that CallReturn constraints exist for make_target
    let has_call_return = program.flow.iter().any(|f| match f {
        trace_ir::FlowConstraint::CallReturn { callee_name, .. } => callee_name == "make_target",
        _ => false,
    });
    assert!(
        has_call_return,
        "must emit CallReturn for make_target even when casted or parenthesized"
    );

    // Check that CallReturnIndirect constraints exist
    let has_call_return_indirect = program
        .flow
        .iter()
        .any(|f| matches!(f, trace_ir::FlowConstraint::CallReturnIndirect { .. }));
    assert!(
        has_call_return_indirect,
        "must emit CallReturnIndirect for indirect call even when casted or parenthesized"
    );
}
