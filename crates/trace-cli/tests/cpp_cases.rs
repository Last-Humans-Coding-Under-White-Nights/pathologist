//! C++ lowering integration tests (first-step C++ support).
#![allow(clippy::needless_borrow)]

mod common;

use std::sync::OnceLock;

use common::{default_opts, fixture, fn_name, must_not_have_edge};
use trace_analysis::{analyze, AnalysisResult, ResolutionKind};
use trace_ir::{FnId, Program};
use trace_parse::build_program;

fn direct_targets(program: &Program, analysis: &AnalysisResult, caller: &str) -> Vec<String> {
    analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == caller && e.resolution == ResolutionKind::Direct)
        .map(|e| fn_name(&program, e.callee))
        .collect()
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

/// The `cpp_smart_ptr` fixture, analysed once.
fn cpp_smart_ptr() -> &'static (Program, AnalysisResult) {
    static CACHE: OnceLock<(Program, AnalysisResult)> = OnceLock::new();
    CACHE.get_or_init(|| {
        let root = fixture("cpp_smart_ptr");
        let program = build_program(&root, &default_opts(&root)).expect("build");
        let (_pag, analysis) = analyze(&program);
        (program, analysis)
    })
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
                program.types.get(f.return_type).desc.clone(),
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
