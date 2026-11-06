//! #246: parser recovery and value flow are checked independently.
use crate::common::*;
use trace_analysis::{analyze_with_options, AnalyzeOptions, ResolutionKind};
use trace_parse::build_program;

#[test]
fn inline_member_addresses_share_virtual_dispatch_with_local_values() {
    let root = scratch(&[(
        "main.cpp",
        r#"
void cb();
int base_return, derived_return;
struct B {
    void (B::*cb)();
    void field() { (this->*cb)(); }
    virtual void f() {}
    virtual void g(int *) {}
    virtual void g(double *) {}
    virtual int *result() { return &base_return; }
};
struct D : B {
    void f() override {}
    void g(int *) override {}
    void g(double *) override {}
    int *result() override { return &derived_return; }
};
struct Final final : B {};
struct Foreign { virtual void f() {} };
using IntMember = void (B::*)(int *);
void local(B *b) { auto cb = &B::f; (b->*cb)(); }
void inline_address(B *b) { (b->*&B::f)(); }
void grouped(B *b) { (b->*(&B::f))(); }
void dot(B &b) { (b.*&B::f)(); }
void final_receiver(Final *b) { (b->*&B::f)(); }
void casted(B *b, int *value) { (b->*static_cast<IntMember>(&B::g))(value); }
int *inline_return(B *b) { return (b->*&B::result)(); }
D object;
Final final_object;
int value;
int *output;
void source() {
    object.cb = &B::f; object.field();
    local(&object); inline_address(&object); grouped(&object); dot(object);
    final_receiver(&final_object); casted(&object, &value);
    output = inline_return(&object);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for caller in [
        "local",
        "B::field",
        "inline_address",
        "grouped",
        "dot",
        "final_receiver",
        "casted",
        "inline_return",
    ] {
        let id = only_function(&program, caller);
        let mut targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|edge| edge.caller == id)
            .map(|edge| {
                assert_eq!(edge.resolution, ResolutionKind::Indirect, "{caller}");
                program.symbols.function(edge.callee)
            })
            .collect();
        targets.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = targets.iter().map(|f| f.name.as_str()).collect();
        let expected: &[&str] = match caller {
            "final_receiver" => &["B::f"],
            "casted" => &["B::g", "D::g"],
            "inline_return" => &["B::result", "D::result"],
            _ => &["B::f", "D::f"],
        };
        assert_eq!(names, expected, "{caller}");
        if caller == "casted" {
            assert!(targets.iter().all(|f| {
                let ty = program.symbols.explicit_params(f).unwrap().get(0).unwrap();
                matches!(
                    program.types.get(ty).desc.innermost().0,
                    trace_ir::TypeDesc::Int
                )
            }));
        }
    }
    assert_points_to(&program, &pag, &analysis, "output", "base_return");
    assert_points_to(&program, &pag, &analysis, "output", "derived_return");
}

#[test]
fn member_pointer_postfix_operands_keep_array_summary_targets() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct C { void hit(int *value) {} void other(int *value) {} };
using M = void (C::*)(int *);
M single[] = { &C::hit };
M callbacks[] = { &C::hit, &C::other };
struct Holder { M callbacks[2]; M direct; };
C *object_ptr();
void local(C *p, int *value) { M f = single[0]; (p->*f)(value); }
void array(C *p, int *value) { (p->*single[0])(value); }
void parenthesized(C *p, int *value) { (p->*(single[0]))(value); }
void dot(C &obj, int *value) { (obj.*single[0])(value); }
void summary(C *p, int index, int *value) { (p->*callbacks[index])(value); }
void field(C *p, Holder *h, int index, int *value) { (p->*h->callbacks[index])(value); }
void cast_field(C *p, Holder *h, int *value) { (p->*((M)h->direct))(value); }
void from_call(int *value) { (object_ptr()->*single[0])(value); }
#define APPLY(p, cb, value) (p->*cb)(value)
void macro(C *p, int *value) { APPLY(p, single[0], value); }
int global;
C object;
C *object_ptr() { return &object; }
Holder holder;
void source(int index) {
    holder.callbacks[0] = &C::hit;
    holder.callbacks[1] = &C::other;
    holder.direct = &C::hit;
    local(&object, &global);
    array(&object, &global);
    parenthesized(&object, &global);
    dot(object, &global);
    summary(&object, index, &global);
    field(&object, &holder, index, &global);
    cast_field(&object, &holder, &global);
    from_call(&global);
    macro(&object, &global);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for caller in [
        "local",
        "array",
        "parenthesized",
        "dot",
        "summary",
        "field",
        "cast_field",
        "macro",
    ] {
        let id = only_function(&program, caller);
        let mut targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|edge| edge.caller == id)
            .map(|edge| {
                assert_eq!(edge.resolution, ResolutionKind::Indirect, "{caller}");
                program.symbols.function(edge.callee).name.as_str()
            })
            .collect();
        targets.sort_unstable();
        let expected: &[&str] = if matches!(caller, "summary" | "field") {
            &["C::hit", "C::other"]
        } else {
            &["C::hit"]
        };
        assert_eq!(targets, expected, "{caller}");
    }
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    assert_local_points_to(&program, &pag, &analysis, "C::hit", "value", "global");
    assert!(has_edge(
        &program,
        &analysis,
        "from_call",
        "C::hit",
        ResolutionKind::Indirect
    ));
    let caller = only_function(&program, "from_call");
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|site| site.caller == caller && site.args_bound_past_this)
        .unwrap();
    let source = std::fs::read_to_string(root.path().join("main.cpp")).unwrap();
    let (line, text) = source
        .lines()
        .enumerate()
        .find(|(_, text)| text.contains("void from_call"))
        .unwrap();
    assert_eq!(site.span.line, line as u32 + 1);
    assert_eq!(site.span.col, text.find("single[0]").unwrap() as u32 + 1);
}

#[test]
fn member_pointer_fields_load_through_this_and_inherited_field_summaries() {
    let root = scratch(&[(
        "main.cpp",
        r#"
using Hook = void (*)();
struct C {
    using Member = int *(C::*)(int *, Hook);
    Member cb;
    int *hit(int *value, Hook hook) { hook(); return value; }
    int *other(int *value, Hook hook) { return nullptr; }
    int *invoke(int *value, Hook hook) { return (this->*cb)(value, hook); }
    int *local(int *value, Hook hook) { Member f = cb; return (this->*f)(value, hook); }
    int *shadow(Member cb, int *value, Hook hook) { return (this->*cb)(value, hook); }
};
struct D : C {
    int *inherited(int *value, Hook hook) { return (this->*cb)(value, hook); }
};
C::Member cb = &C::other;
int global;
C object;
D derived;
void hook() {}
void install(C *p) { p->cb = &C::hit; }
void source() {
    C *p = &object;
    D *q = &derived;
    install(p);
    p->invoke(&global, hook);
    p->local(&global, hook);
    q->inherited(&global, hook);
    p->shadow(&C::other, &global, hook);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for caller in ["C::invoke", "C::local", "D::inherited"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "C::hit",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
        assert!(
            must_not_have_edge(&program, &analysis, caller, "C::other"),
            "{caller}"
        );
    }
    assert!(has_edge(
        &program,
        &analysis,
        "C::shadow",
        "C::other",
        ResolutionKind::Indirect
    ));
    assert!(must_not_have_edge(
        &program,
        &analysis,
        "C::shadow",
        "C::hit"
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "C::hit",
        "hook",
        ResolutionKind::Indirect
    ));
    assert_local_points_to(&program, &pag, &analysis, "C::hit", "value", "global");
}

#[test]
fn defined_member_pointer_targets_keep_the_address_taking_translation_unit() {
    let root = scratch(&[
        (
            "api.hpp",
            r#"
#pragma once
struct C { int *hit(); };
using Member = int *(C::*)();
int *invoke(C *, Member);
"#,
        ),
        (
            "a.cpp",
            r#"
#include "api.hpp"
int first;
int *C::hit() { return &first; }
int *result;
void source(C *p) { result = invoke(p, &C::hit); }
"#,
        ),
        (
            "b.cpp",
            r#"
#include "api.hpp"
int second;
int *C::hit() { return &second; }
int *invoke(C *p, Member cb) { return (p->*cb)(); }
int *direct(C *p) { return p->hit(); }
"#,
        ),
    ]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    let definitions: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "C::hit" && f.is_defined)
        .collect();
    assert_eq!(definitions.len(), 2, "{definitions:?}");
    for (caller, expected_file) in [("invoke", "a.cpp"), ("direct", "b.cpp")] {
        let caller = only_function(&program, caller);
        let targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|edge| edge.caller == caller)
            .map(|edge| program.symbols.function(edge.callee))
            .filter(|f| f.name == "C::hit" && f.is_defined)
            .collect();
        assert_eq!(targets.len(), 1, "{targets:?}");
        assert_eq!(
            targets[0].file,
            file_id(&program, root.path(), expected_file)
        );
    }
    assert_points_to(&program, &pag, &analysis, "result", "first");
}

#[test]
fn unresolved_member_pointer_receivers_dispatch_from_the_bound_owner() {
    let root = scratch(&[(
        "main.cpp",
        r#"
template<class T> struct sptr { sptr(T *); T *operator->(); operator T *(); };
struct Base { virtual void hit() {} };
struct Derived : Base { void hit() override {} void hit(int) {} };
struct Final final : Base {};
struct Foreign { virtual void hit() {} };
template<class T> void invoke(sptr<T> p, void (T::*cb)()) { (p->*cb)(); }
void final_receiver(sptr<Final> p, void (Base::*cb)()) { (p->*cb)(); }
void source() {
    Derived d;
    invoke<Base>(sptr<Base>(&d), &Base::hit);
    Final f;
    final_receiver(sptr<Final>(&f), &Base::hit);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (_, analysis) = trace_analysis::analyze(&program);
    let invoke = only_function(&program, "invoke");
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|site| site.caller == invoke && site.args_bound_past_this)
        .unwrap();
    assert_eq!(site.receiver_class.as_deref(), Some("T"));
    let targets: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|edge| edge.caller == invoke)
        .map(|edge| program.symbols.function(edge.callee))
        .collect();
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert!(targets.iter().any(|f| f.name == "Base::hit"));
    assert!(targets.iter().any(|f| f.name == "Derived::hit"));
    assert!(targets
        .iter()
        .all(|f| program.symbols.explicit_params(f).unwrap().is_empty()));
    let final_receiver = only_function(&program, "final_receiver");
    let targets: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|edge| edge.caller == final_receiver)
        .map(|edge| program.symbols.function(edge.callee).name.as_str())
        .collect();
    assert_eq!(targets, ["Base::hit"]);
}

#[test]
fn member_pointer_values_bind_receivers_arguments_and_returns_across_units() {
    let root = fixture("cpp_member_pointer_calls");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, result) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for caller in ["local", "dot", "reference", "invoke", "header_call"] {
        let edges: Vec<_> = result
            .call_edges
            .iter()
            .filter(|e| program.symbols.function(e.caller).name == caller)
            .collect();
        assert!(
            edges
                .iter()
                .any(|e| program.symbols.function(e.callee).name == "Worker::run"
                    && e.resolution == ResolutionKind::Indirect),
            "{caller}: {edges:?}"
        );
    }
    for var in [
        "named_result",
        "local_result",
        "macro_result",
        "dot_result",
        "reference_result",
        "cross_result",
        "negative_result",
        "header_result",
        "virtual_result",
        "final_result",
        "override_result",
    ] {
        assert_points_to(&program, &pag, &result, var, "global");
    }
    assert_local_points_to(&program, &pag, &result, "Worker::run", "this", "object");
    let this = local_variable(&program, "Worker::run", "this");
    let object = local_variable(&program, "dot", "obj");
    assert!(result.points_to[&pag.var_node[&this]].contains(&pag.var_location[&object]));
    assert!(has_fn_arg_flow(
        &program,
        &result,
        "local",
        "Worker::run",
        2,
        "hook"
    ));
    let targets: Vec<_> = result
        .call_edges
        .iter()
        .filter(|e| program.symbols.function(e.caller).name == "negative")
        .map(|e| program.symbols.function(e.callee).name.as_str())
        .collect();
    assert_eq!(targets, ["Worker::simple"]);
    let virtuals: Vec<_> = result
        .call_edges
        .iter()
        .filter(|e| program.symbols.function(e.caller).name == "dispatch")
        .map(|e| program.symbols.function(e.callee))
        .collect();
    assert_eq!(virtuals.len(), 3, "{virtuals:?}");
    assert!(virtuals.iter().all(|f| program
        .symbols
        .explicit_params(f)
        .unwrap()
        .get(0)
        .is_some_and(|t| matches!(
            program.types.get(t).desc.as_ref(),
            trace_ir::TypeDesc::Ptr(_)
        ))));
    let final_targets: Vec<_> = result
        .call_edges
        .iter()
        .filter(|e| program.symbols.function(e.caller).name == "final_dispatch")
        .map(|e| program.symbols.function(e.callee).name.as_str())
        .collect();
    assert_eq!(final_targets, ["Base::run"]);
    let override_targets: Vec<_> = result
        .call_edges
        .iter()
        .filter(|e| program.symbols.function(e.caller).name == "final_override")
        .map(|e| program.symbols.function(e.callee).name.as_str())
        .collect();
    assert_eq!(override_targets, ["FinalOverride::run"]);
    let header_site = program
        .symbols
        .call_sites
        .iter()
        .find(|s| program.symbols.function(s.caller).name == "header_call")
        .unwrap();
    assert_eq!(header_site.span.file, file_id(&program, &root, "api.hpp"));
    assert_eq!(header_site.span.line, 24);
    assert_eq!(header_site.expansion_span.unwrap().line, 26);
    assert!(header_site
        .details
        .as_ref()
        .unwrap()
        .call_expression
        .as_deref()
        .unwrap()
        .contains("->*"));
    let run = only_function(&program, "Worker::run");
    assert!(result
        .call_edges
        .iter()
        .any(|e| e.caller == run && program.symbols.function(e.callee).name == "hook"));
}

#[test]
fn instantiations_and_sfinae_preserve_existing_members_and_defaults() {
    let root = fixture("cpp_template_instantiations");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    for name in ["ns::Box::data", "ns::Box::convert"] {
        let f = program.symbols.function(only_function(&program, name));
        assert!(!f.is_defined, "{f:?}");
    }
    assert!(
        program
            .symbols
            .function(only_function(&program, "ns::Box::run"))
            .is_defined
    );
    let run = program
        .symbols
        .function(only_function(&program, "EncodedParam::run"));
    assert!(run.is_defined);
    assert_eq!(run.default_args, 1);
    assert_eq!(run.span.line, 9);
    assert!(
        program
            .symbols
            .function(only_function(&program, "RawDataBuilder::Append"))
            .is_defined
    );
    assert!(
        program
            .symbols
            .function(only_function(&program, "SysEvent::SetEventValue"))
            .is_defined
    );
    assert_eq!(
        program.symbols.functions.len(),
        12,
        "{:?}",
        program
            .symbols
            .functions
            .iter()
            .map(|f| &f.name)
            .collect::<Vec<_>>()
    );
    let append = program
        .symbols
        .function(only_function(&program, "Builder::AppendValue"));
    assert!(append.is_defined);
    assert!(matches!(
        program.types.get(append.return_type).desc.innermost().0,
        trace_ir::TypeDesc::Struct { name, .. } if name == "Builder"
    ));
    assert!(
        program
            .symbols
            .function(only_function(&program, "Builder::Init"))
            .is_defined
    );
    let nested = program
        .symbols
        .function(only_function(&program, "NestedEnabled::run"));
    assert!(nested.is_defined);
    assert_eq!(nested.default_args, 1);
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(analysis
        .call_edges
        .iter()
        .any(|e| program.symbols.function(e.caller).name == "source" && e.callee == run.id));
}

#[test]
fn recovered_forms_export_identically_across_jobs() {
    for fixture_name in ["cpp_template_instantiations", "cpp_member_pointer_calls"] {
        let root = fixture(fixture_name);
        let one = cli_analyze(&root, &["--jobs", "1"]);
        let many = cli_analyze(&root, &["--jobs", "4"]);
        assert_eq!(analysis_rows(&one), analysis_rows(&many), "{fixture_name}");
        let conn = rusqlite::Connection::open(many.path()).unwrap();
        assert!(text_rows(
            &conn,
            "SELECT message FROM diagnostics WHERE stage = 'parse'"
        )
        .is_empty());
        let full_one = cli_analyze(&root, &["--jobs", "1", "--full-export"]);
        let full_many = cli_analyze(&root, &["--jobs", "4", "--full-export"]);
        assert_eq!(
            analysis_rows(&full_one),
            analysis_rows(&full_many),
            "full {fixture_name}"
        );
    }
}

#[test]
fn camera_adaptor_member_invocation_through_a_template_receiver() {
    let root = scratch(&[(
        "main.cpp",
        r#"
template<class T> struct sptr { sptr(T *); T *operator->(); operator T *(); };
struct Session { int Set(int *arg) { return 0; } };
// Camera camera_napi_adaptor.h:97, with SDK-specific argument extraction removed.
template<class TSession, class U>
int SetArgImpl(sptr<TSession> sessionNative, int (TSession::*setFunc)(U), U arg) {
    int retCode = (sessionNative->*setFunc)(arg);
    return retCode;
}
int global;
void source() {
    sptr<Session> session = new Session;
    SetArgImpl(session, &Session::Set, &global);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "SetArgImpl", "Session::Set"),
        "{:?}",
        callees_of(&program, &analysis, "SetArgImpl")
    );
}

#[test]
fn dependent_member_pointer_signatures_keep_parameter_pack_candidates() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct Worker { double run(int, double) { return 0; } };
template<class R, class C, class... Args>
R forward(C *p, R (C::*f)(Args...), Args... args) { return (p->*f)(args...); }
struct PointerWorker { int *echo(int *value) { return value; } };
template<class T, class C>
T *forward_pointer(C *p, T *(C::*f)(T *), T *value) { return (p->*f)(value); }
int global;
void source() {
    Worker w; forward(&w, &Worker::run, 1, 2.0);
    PointerWorker p;
    int *out = forward_pointer(&p, &PointerWorker::echo, &global);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    assert!(has_any_edge(&program, &analysis, "forward", "Worker::run"));
    assert!(has_any_edge(
        &program,
        &analysis,
        "forward_pointer",
        "PointerWorker::echo"
    ));
    assert_points_to(&program, &pag, &analysis, "out", "global");
}

#[test]
fn overloaded_arrow_star_names_survive_normalization_and_export() {
    let root = scratch(&[(
        "main.cpp",
        r#"
#define ARROW_STAR operator->*
struct C { void ARROW_STAR(int); };
void C::operator /* owner spelling */ ->*(int) {}
void explicit_calls(C &c, C *p) {
    c.operator->*(1);
    p->operator /* call spelling */ ->*(2);
}
using Member = void (C::*)(int);
void indirect(C *p, Member f) { (p->*f)(1); }
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let target = only_function(&program, "C::operator->*");
    assert!(program.symbols.function(target).is_defined);
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "C::operator->"));
    let (pag, analysis) = trace_analysis::analyze(&program);
    assert_eq!(
        analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == only_function(&program, "explicit_calls") && e.callee == target)
            .count(),
        2
    );
    let db = export_program(&program, &pag, &analysis);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM functions WHERE name='C::operator->*' AND is_defined=1",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn explicit_member_pointer_casts_select_modeled_callbacks_and_contexts() {
    for definitions in [
        "void hit(int) {} void hit(double) {}",
        "void hit(double) {} void hit(int) {}",
    ] {
        let source = format!(
            r#"
namespace std {{ struct thread {{ template<class F, class... Args> thread(F, Args...); }}; }}
struct C {{ {definitions} }};
using IntMember = void (C::*)(int);
void named(C *p) {{ std::thread t(static_cast<void (C::*)(int)>(&C::hit), p, 1); }}
void alias(C *p) {{ std::thread t(static_cast<IntMember>(&C::hit), p, 1); }}
void c_style(C *p) {{ std::thread t((void (C::*)(int))&C::hit, p, 1); }}
void local(C *p) {{ auto f = static_cast<IntMember>(&C::hit); std::thread t(f, p, 1); }}
void array(C *p) {{ IntMember callbacks[] = {{static_cast<IntMember>(&C::hit)}}; std::thread t(callbacks[0], p, 1); }}
void nested(C *p) {{ std::thread t(reinterpret_cast<IntMember>(reinterpret_cast<void (C::*)(double)>(static_cast<IntMember>(&C::hit))), p, 1); }}
void floating(C *p) {{ std::thread t(static_cast<void (C::*)(double)>(&C::hit), p, 1.0); }}
"#
        );
        let root = scratch(&[("main.cpp", &source)]);
        let program = build_program(root.path(), &default_opts(root.path())).unwrap();
        assert!(
            !program.diagnostics.iter().any(|d| d.stage == "parse"),
            "{:?}",
            program.diagnostics
        );
        let (_, analysis) = trace_analysis::analyze(&program);
        for caller in [
            "named", "alias", "c_style", "local", "array", "nested", "floating",
        ] {
            let caller_id = only_function(&program, caller);
            let edges: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| {
                    e.caller == caller_id && program.symbols.function(e.callee).name == "C::hit"
                })
                .collect();
            assert_eq!(edges.len(), 1, "{caller}, {definitions}: {edges:?}");
            let signature = program
                .symbols
                .explicit_params(program.symbols.function(edges[0].callee))
                .unwrap();
            let expected = if caller == "floating" {
                trace_ir::TypeDesc::Double
            } else {
                trace_ir::TypeDesc::Int
            };
            assert_eq!(
                program.types.get(signature.get(0).unwrap()).desc.as_ref(),
                &expected,
                "{caller}, {definitions}"
            );
            let contexts: Vec<_> = analysis
                .execution_contexts
                .iter()
                .filter(|context| {
                    context.start.is_some_and(|start| {
                        program
                            .symbols
                            .call_site_by_id(start.call_site)
                            .unwrap()
                            .caller
                            == caller_id
                    })
                })
                .collect();
            assert_eq!(contexts.len(), 1, "{caller}: {contexts:?}");
            assert_eq!(contexts[0].entry, edges[0].callee);
        }
    }
}

#[test]
fn reinterpreted_member_pointer_values_survive_round_trips() {
    let root = scratch(&[(
        "main.cpp",
        r#"
namespace std { struct thread { template<class F, class... Args> thread(F, Args...); }; }
struct C { void hit(int) {} };
using IntMember = void (C::*)(int);
using DoubleMember = void (C::*)(double);
void local_round_trip(C *p) {
    auto erased = reinterpret_cast<void (C::*)(double)>(&C::hit);
    auto restored = reinterpret_cast<void (C::*)(int)>(erased);
    std::thread t(restored, p, 1);
}
void nested_round_trip(C *p) {
    std::thread t(reinterpret_cast<IntMember>(reinterpret_cast<DoubleMember>(&C::hit)), p, 1);
}
void typed_round_trip(C *p) {
    DoubleMember erased = reinterpret_cast<DoubleMember>(&C::hit);
    IntMember restored = reinterpret_cast<IntMember>(erased);
    std::thread t(restored, p, 1);
}
void c_style_round_trip(C *p) {
    auto erased = (void (C::*)(double))&C::hit;
    std::thread t(reinterpret_cast<IntMember>(erased), p, 1);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    let hit = only_function(&program, "C::hit");
    for caller in [
        "local_round_trip",
        "nested_round_trip",
        "typed_round_trip",
        "c_style_round_trip",
    ] {
        let caller_id = only_function(&program, caller);
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "C::hit",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
        let contexts: Vec<_> = analysis
            .execution_contexts
            .iter()
            .filter(|context| {
                context.start.is_some_and(|start| {
                    program
                        .symbols
                        .call_site_by_id(start.call_site)
                        .unwrap()
                        .caller
                        == caller_id
                })
            })
            .collect();
        assert_eq!(contexts.len(), 1, "{caller}: {contexts:?}");
        assert_eq!(contexts[0].entry, hit);
    }
    for name in ["erased", "restored"] {
        let var = local_variable(&program, "local_round_trip", name);
        assert!(
            analysis
                .points_to
                .get(&pag.var_node[&var])
                .is_some_and(|points| points
                    .iter()
                    .any(|loc| pag.locations[loc.0 as usize].fn_id == Some(hit))),
            "{name}"
        );
    }
}

#[test]
fn dependent_member_pointer_arguments_keep_concrete_callback_candidates() {
    let root = scratch(&[(
        "main.cpp",
        r#"
namespace std { struct thread { template<class F, class... Args> thread(F, Args...); }; }
template<class T> struct Box { T *get(); };
template<class T, class U> struct Pair { T *first(); U *second(); };
struct C {
    void hit(Box<int> *) {}
    template<class T> void generic(Box<T> *) {}
    void pair(Pair<int, int> *) {}
    void pair(Pair<int, double> *) {}
};
template<class T> void launch(C *p, Box<T> *value) {
    std::thread direct(static_cast<void (C::*)(Box<T> *)>(&C::hit), p, value);
    using Member = void (C::*)(Box<T> *);
    Member f = &C::hit;
    std::thread local(f, p, value);
}
template<class T> void partial(C *p, Pair<T, int> *value) {
    std::thread t(static_cast<void (C::*)(Pair<T, int> *)>(&C::pair), p, value);
}
void concrete(C *p, Box<int> *value) {
    std::thread t(static_cast<void (C::*)(Box<int> *)>(&C::generic<int>), p, value);
}
void source() {
    C c; Box<int> value; launch<int>(&c, &value);
    Pair<int, int> pair; partial<int>(&c, &pair); concrete(&c, &value);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (_, analysis) = trace_analysis::analyze(&program);
    let launch = only_function(&program, "launch");
    let hit = only_function(&program, "C::hit");
    assert_eq!(
        analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == launch && e.callee == hit)
            .count(),
        2
    );
    assert_eq!(
        analysis
            .execution_contexts
            .iter()
            .filter(|c| c.entry == hit)
            .count(),
        2
    );
    assert!(has_edge(
        &program,
        &analysis,
        "concrete",
        "C::generic",
        ResolutionKind::Indirect
    ));
    let partial = only_function(&program, "partial");
    let entries: Vec<_> = analysis
        .execution_contexts
        .iter()
        .filter(|context| {
            context.start.is_some_and(|start| {
                program
                    .symbols
                    .call_site_by_id(start.call_site)
                    .unwrap()
                    .caller
                    == partial
            })
        })
        .map(|context| program.symbols.function(context.entry))
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let formal = program
        .symbols
        .explicit_params(entries[0])
        .unwrap()
        .get(0)
        .unwrap();
    assert!(matches!(program.types.get(formal).desc.innermost().0,
        trace_ir::TypeDesc::Struct { name, .. } if name.replace(' ', "") == "Pair<int,int>"));
}

#[test]
fn converted_member_pointer_accepts_base_receiver_and_rejects_unrelated_owner() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct B {};
struct D : B { void hit() {} };
struct Foreign { void hit() {} };
using Member = void (B::*)();
D object;
void invoke(B *p, Member f) { (p->*f)(); }
void source() {
    Member f = static_cast<Member>(&D::hit);
    invoke(&object, f);
}
void unrelated_source() {
    Member f = reinterpret_cast<Member>(&Foreign::hit);
    invoke(&object, f);
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    assert!(has_edge(
        &program,
        &analysis,
        "invoke",
        "D::hit",
        ResolutionKind::Indirect
    ));
    assert!(must_not_have_edge(
        &program,
        &analysis,
        "invoke",
        "Foreign::hit"
    ));
    assert_local_points_to(&program, &pag, &analysis, "D::hit", "this", "object");
}

#[test]
fn destination_member_pointer_signatures_select_callbacks_before_modeling() {
    for definitions in [
        "void hit(int) {} void hit(double) {}",
        "void hit(double) {} void hit(int) {}",
    ] {
        let source = format!(
            r#"
namespace std {{ struct thread {{ template<class F, class... Args> thread(F, Args...); }}; }}
template<class T> struct Box {{ T *get(); }};
struct C {{ {definitions} void box(Box<int> *) {{}} void box(Box<double> *) {{}} }};
using Member = void (C::*)(int);
using BoxMember = void (C::*)(Box<int> *);
void declared(C *p) {{ Member f = &C::hit; std::thread t(f, p, 1); }}
void assigned(C *p) {{ Member f; f = &C::hit; std::thread t(f, p, 1); }}
void array(C *p) {{ Member f[] = {{ &C::hit }}; std::thread t(f[0], p, 1); }}
void concrete_box(C *p, Box<int> *value) {{ BoxMember f = &C::box; std::thread t(f, p, value); }}
void cast_box(C *p, Box<int> *value) {{ std::thread t(static_cast<BoxMember>(&C::box), p, value); }}
"#
        );
        let root = scratch(&[("main.cpp", &source)]);
        let program = build_program(root.path(), &default_opts(root.path())).unwrap();
        assert!(
            !program.diagnostics.iter().any(|d| d.stage == "parse"),
            "{:?}",
            program.diagnostics
        );
        let (_, analysis) = trace_analysis::analyze(&program);
        for caller in ["declared", "assigned", "array", "concrete_box", "cast_box"] {
            let caller_id = only_function(&program, caller);
            let contexts: Vec<_> = analysis
                .execution_contexts
                .iter()
                .filter(|c| {
                    c.start.is_some_and(|s| {
                        program.symbols.call_site_by_id(s.call_site).unwrap().caller == caller_id
                    })
                })
                .collect();
            assert_eq!(contexts.len(), 1, "{caller}, {definitions}: {contexts:?}");
            let f = program.symbols.function(contexts[0].entry);
            let params = program.symbols.explicit_params(f).unwrap();
            let desc = program.types.get(params.get(0).unwrap()).desc.innermost().0;
            if caller.ends_with("box") {
                assert!(
                    matches!(desc, trace_ir::TypeDesc::Struct { name, .. } if name == "Box<int>"),
                    "{caller}: {desc:?}"
                );
            } else {
                assert_eq!(desc, &trace_ir::TypeDesc::Int, "{caller}");
            }
        }
    }
}

#[test]
fn reviewed_member_pointer_expression_values() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct C {
    int *hit(int *p) { return p; }
    void cb(void f()) { f(); }
    int *(C::*field)(int *);
    int *invoke(int *p) { return (this->*field)(p); }
};
using M = int *(C::*)(int *);
using E = void (C::*)();
using B = void (C::*)(void (*)());
M get() { return &C::hit; }
C object;
C get_obj() { return object; }
int value;
int *result;
void callback() {}
void call_result(C *p) { result = (p->*get())(&value); }
void cast_result(C *p) {
    E erased = reinterpret_cast<E>(&C::hit);
    result = (p->*reinterpret_cast<M>(erased))(&value);
}
void stored(C *p) {
    E erased;
    E *slot = &erased;
    *slot = reinterpret_cast<E>(&C::hit);
    M restored = reinterpret_cast<M>(erased);
    result = (p->*restored)(&value);
}
void adjusted(C *p) { B cb = &C::cb; (p->*cb)(callback); }
void temporary() { M m = &C::hit; result = (get_obj().*m)(&value); }
void source() {
    object.field = &C::hit; object.invoke(&value);
    call_result(&object); cast_result(&object); stored(&object); adjusted(&object); temporary();
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for caller in [
        "call_result",
        "cast_result",
        "stored",
        "C::invoke",
        "temporary",
    ] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "C::hit",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
    }
    assert!(has_edge(
        &program,
        &analysis,
        "adjusted",
        "C::cb",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "C::cb",
        "callback",
        ResolutionKind::Indirect
    ));
    assert_points_to(&program, &pag, &analysis, "result", "value");
    let caller = only_function(&program, "temporary");
    assert!(program
        .symbols
        .call_sites
        .iter()
        .any(|s| s.caller == caller && s.args_bound_past_this && s.receiver.is_some()));
}

#[test]
fn reviewed_callable_data_members_and_arrow_star_overloads() {
    let root = scratch(&[(
        "main.cpp",
        r#"
using Fn = void (*)(int *);
int value;
void alpha(int *p) {}
struct S { Fn cb; };
Fn S::* pm = &S::cb;
void data(S *s) { (s->*pm)(&value); }
struct Proxy { Fn operator->*(int) { return alpha; } };
void overloaded(Proxy p) { (p->*0)(&value); }
S object;
void source() { object.cb = alpha; data(&object); Proxy p; overloaded(p); }
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    assert!(has_edge(
        &program,
        &analysis,
        "data",
        "alpha",
        ResolutionKind::Indirect
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "overloaded",
        "alpha",
        ResolutionKind::Indirect
    ));
    assert!(analysis
        .call_edges
        .iter()
        .any(|e| program.symbols.function(e.caller).name == "overloaded"
            && program.symbols.function(e.callee).name == "Proxy::operator->*"));
    assert_local_points_to(&program, &pag, &analysis, "alpha", "p", "value");
}

#[test]
fn reviewed_member_pointer_macro_occurrences_and_unknown_size() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct C { void hit(unsigned long) {} };
using M = void (C::*)(size_t);
#define CALL(p, m) (p->*m)(1)
#define TWICE(p, m) CALL(p, m); CALL(p, m)
void caller(C *p) { M m = &C::hit; TWICE(p, m); }
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    let caller = only_function(&program, "caller");
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == caller && s.args_bound_past_this)
        .collect();
    assert_eq!(sites.len(), 2);
    assert_ne!(
        sites[0].occurrence().expansion_id,
        sites[1].occurrence().expansion_id
    );
    assert_eq!(sites[0].span, sites[1].span);
    assert!(sites.iter().all(|s| s.expansion_span.is_some()));
    let hit = only_function(&program, "C::hit");
    assert_eq!(
        analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == caller && e.callee == hit)
            .count(),
        2
    );
}

#[test]
fn reviewed_member_pointer_declarations_resolve_in_the_callers_image() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct C { void hit() {} };
void caller(C *p) { auto cb = &C::hit; (p->*cb)(); }
"#,
    )]);
    let mut program = build_program(root.path(), &default_opts(root.path())).unwrap();
    let caller = only_function(&program, "caller");
    let declaration = only_function(&program, "C::hit");
    let target = trace_ir::TargetId(0);
    let mut definition = program.symbols.function(declaration).clone();
    definition.id = program.symbols.alloc_fn_id();
    definition.target = Some(target);
    program
        .symbols
        .functions
        .iter_mut()
        .find(|f| f.id == declaration)
        .unwrap()
        .is_defined = false;
    program
        .symbols
        .functions
        .iter_mut()
        .find(|f| f.id == caller)
        .unwrap()
        .target = Some(target);
    let definition = program.symbols.add_function(definition);
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(analysis.call_edges.iter().any(|edge| edge.caller == caller
        && edge.callee == definition
        && edge.resolution == ResolutionKind::Indirect));
    assert!(!analysis
        .call_edges
        .iter()
        .any(|edge| edge.caller == caller && edge.callee == declaration));
}

#[test]
fn numeric_identifier_suffixes_preserve_member_pointer_calls() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct C { void hit() {} };
using M = void (C::*)();
void caller(C *obj1e, C *obj2E, C *obj3p, C *obj4P) {
    M m = &C::hit;
    (obj1e->*m)(); (obj2E->*m)(); (obj3p->*m)(); (obj4P->*m)();
}
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (_, analysis) = trace_analysis::analyze(&program);
    let caller = only_function(&program, "caller");
    let hit = only_function(&program, "C::hit");
    assert_eq!(
        analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == caller
                && e.callee == hit
                && e.resolution == ResolutionKind::Indirect)
            .count(),
        4
    );
}

#[test]
fn mixed_static_and_instance_overloads_select_member_pointer_values() {
    for declarations in [
        "void hit() {} static void hit(int) {}",
        "static void hit(int) {} void hit() {}",
    ] {
        let source = format!(
            r#"
struct C {{ {declarations} }};
using M = void (C::*)();
using F = void (*)(int);
M global = &C::hit;
void local(C *p) {{ M m = &C::hit; (p->*m)(); }}
void cast(C *p) {{ (p->*static_cast<M>(&C::hit))(); }}
void array(C *p) {{ M callbacks[] = {{ &C::hit }}; (p->*callbacks[0])(); }}
void ordinary() {{ F f = &C::hit; f(1); }}
void from_global(C *p) {{ (p->*global)(); }}
"#
        );
        let root = scratch(&[("main.cpp", &source)]);
        let program = build_program(root.path(), &default_opts(root.path())).unwrap();
        assert!(
            !program.diagnostics.iter().any(|d| d.stage == "parse"),
            "{:?}",
            program.diagnostics
        );
        let (_, analysis) = trace_analysis::analyze(&program);
        for name in ["local", "cast", "array", "ordinary", "from_global"] {
            let caller = only_function(&program, name);
            let targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.caller == caller)
                .map(|e| program.symbols.function(e.callee))
                .collect();
            assert_eq!(targets.len(), 1, "{name}: {targets:?}; {declarations}");
            assert_eq!(targets[0].name, "C::hit");
            assert_eq!(
                targets[0].is_static_member,
                name == "ordinary",
                "{name}; {declarations}"
            );
        }
    }
}

#[test]
fn reviewed_return_addresses_select_overloads_in_both_orders() {
    for declarations in [
        "void hit(double) {} void hit(int) {}",
        "void hit(int) {} void hit(double) {}",
    ] {
        let source = format!(
            r#"
struct C {{ {declarations} }};
using M = void (C::*)(double);
M get() {{ return &C::hit; }}
M cast_get() {{ return static_cast<M>(&C::hit); }}
void caller(C *p) {{ (p->*get())(1.0); }}
void cast_caller(C *p) {{ M m = cast_get(); (p->*m)(1.0); }}
"#
        );
        let root = scratch(&[("main.cpp", &source)]);
        let program = build_program(root.path(), &default_opts(root.path())).unwrap();
        let (_, analysis) = trace_analysis::analyze(&program);
        for caller in ["caller", "cast_caller"] {
            let caller = only_function(&program, caller);
            let targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.caller == caller && e.resolution == ResolutionKind::Indirect)
                .collect();
            assert_eq!(targets.len(), 1, "{declarations}: {targets:?}");
            let f = program.symbols.function(targets[0].callee);
            let param = program.symbols.explicit_params(f).unwrap().get(0).unwrap();
            assert!(matches!(
                program.types.get(param).desc.as_ref(),
                trace_ir::TypeDesc::Double
            ));
        }
    }
}

#[test]
fn reviewed_template_scalar_equivalence_and_dependent_pointers() {
    let root = scratch(&[(
        "main.cpp",
        r#"
template<class T> struct Box { T *get(); };
struct C {
    void scalar(Box<unsigned> *) {}
    template<class T> void dependent(Box<T> *) {}
};
using S = void (C::*)(Box<unsigned int>*);
using M = void (C::*)(Box<int*>*);
using P = void (C::*)(Box<int**>*);
S s = &C::scalar;
M m = &C::dependent<int*>;
P pp = &C::dependent<int**>;
void scalar(C *p) { (p->*s)(nullptr); }
void dependent(C *p, Box<int*> *v) { (p->*m)(v); }
void nested(C *p, Box<int**> *v) { (p->*pp)(v); }
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    for (caller, target) in [
        ("scalar", "C::scalar"),
        ("dependent", "C::dependent"),
        ("nested", "C::dependent"),
    ] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                target,
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
    }
}

#[test]
fn reviewed_callable_data_member_addresses_and_array_elements() {
    let root = scratch(&[(
        "main.cpp",
        r#"
using Fn = void (*)(int *);
int value;
void alpha(int *p) {}
struct S { Fn cb; };
void inline_address(S *p) { (p->*&S::cb)(&value); }
void array(S *p) { Fn S::*members[1]; members[0] = &S::cb; (p->*members[0])(&value); }
S object;
void source() { object.cb = alpha; inline_address(&object); array(&object); }
"#,
    )]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for caller in ["inline_address", "array"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "alpha",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
        assert!(!analysis
            .call_edges
            .iter()
            .any(|e| program.symbols.function(e.caller).name == caller
                && program.symbols.function(e.callee).name == "S::cb"));
    }
    assert_local_points_to(&program, &pag, &analysis, "alpha", "p", "value");
}

#[test]
fn reviewed_arrow_star_value_flow_and_sequenced_events() {
    let root = scratch(&[(
        "main.cpp",
        r#"
using Fn = void (*)(int *);
int value;
void alpha(int *p) {}
struct Proxy { Fn operator->*(int) { return alpha; } };
struct Global {};
Fn operator->*(Global, int) { return alpha; }
void global(Global p) { (p->*0)(&value); }
int argument() { return 0; }
void nested(Proxy p) { (p->*argument())(&value); }
void discarded(Proxy p) { p->*0; }
Fn returned(Proxy p) { return p->*0; }
void returned_caller(Proxy p) { Fn f = returned(p); f(&value); }
namespace ns {
struct Free {};
Fn operator->*(Free, int) { return alpha; }
struct Friend { friend Fn operator->*(Friend, int) { return alpha; } };
}
void immediate(Proxy p) { (p->*0)(&value); }
void assigned(Proxy p) { Fn f = p->*0; f(&value); }
void free_immediate(ns::Free p) { (p->*0)(&value); }
void free_assigned(ns::Free p) { Fn f = p->*0; f(&value); }
void friend_immediate(ns::Friend p) { (p->*0)(&value); }
#define CALL(p) (p->*0)(&value)
void macro(Proxy p) { CALL(p); }
#define TWICE(p) CALL(p); CALL(p)
void twice(Proxy p) { TWICE(p); }
"#,
    )]);
    let mut opts = default_opts(root.path());
    opts.call_sequencing = true;
    let program = build_program(root.path(), &opts).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for (caller, operator) in [
        ("global", "operator->*"),
        ("nested", "Proxy::operator->*"),
        ("returned_caller", "returned"),
        ("immediate", "Proxy::operator->*"),
        ("assigned", "Proxy::operator->*"),
        ("free_immediate", "ns::operator->*"),
        ("free_assigned", "ns::operator->*"),
        ("friend_immediate", "ns::operator->*"),
        ("macro", "Proxy::operator->*"),
        ("twice", "Proxy::operator->*"),
    ] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "alpha",
                ResolutionKind::Indirect
            ),
            "{caller}"
        );
        let id = only_function(&program, caller);
        assert!(
            analysis
                .call_edges
                .iter()
                .any(|e| e.caller == id && program.symbols.function(e.callee).name == operator),
            "{caller}"
        );
        let cfg = &program.cfgs[&id][0];
        let count = match caller {
            "twice" => 4,
            "nested" => 3,
            _ => 2,
        };
        assert_eq!(cfg.events.len(), count, "{caller}: {cfg:?}");
        assert_eq!(cfg.order.len(), count - 1, "{caller}: {cfg:?}");
        assert!(
            cfg.events.iter().all(|e| e.sites.len() == 1),
            "{caller}: {cfg:?}"
        );
    }
    for caller in ["discarded", "returned"] {
        let id = only_function(&program, caller);
        assert_eq!(program.cfgs[&id][0].events.len(), 1, "{caller}");
        assert!(analysis
            .call_edges
            .iter()
            .any(|e| e.caller == id
                && program.symbols.function(e.callee).name == "Proxy::operator->*"));
    }
    assert_local_points_to(&program, &pag, &analysis, "alpha", "p", "value");
}

#[test]
fn member_pointer_precedence_keeps_arithmetic_operator_calls() {
    let root = scratch(&[(
        "main.cpp",
        r#"
struct V { int operator->*(int) { return 2; } };
int add_left(V v) { return 1 + v->*0; }
int add_right(V v) { return v->*0 + 1; }
int multiply_left(V v) { return 2 * v->*0; }
int multiply_right(V v) { return v->*0 * 2; }
int pair(V v) { return 1 + v->*0 * v->*1; }
int grouped(V v) { return 1 + (v->*0); }
int unary(V *v) { return 1 + *v->*0; }
"#,
    )]);
    let mut opts = default_opts(root.path());
    opts.call_sequencing = true;
    let program = build_program(root.path(), &opts).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (_, analysis) = trace_analysis::analyze(&program);
    for caller in [
        "add_left",
        "add_right",
        "multiply_left",
        "multiply_right",
        "pair",
        "grouped",
        "unary",
    ] {
        let id = only_function(&program, caller);
        let targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == id)
            .collect();
        let expected = if caller == "pair" { 2 } else { 1 };
        assert_eq!(targets.len(), expected, "{caller}: {targets:?}");
        assert!(targets
            .iter()
            .all(|e| program.symbols.function(e.callee).name == "V::operator->*"));
        assert_eq!(program.cfgs[&id][0].events.len(), expected, "{caller}");
    }
}

#[test]
fn static_member_addresses_use_the_ordinary_destination_signature() {
    for declarations in [
        "static void f(int) {} static void f(double) {} void f(char) {}",
        "void f(char) {} static void f(double) {} static void f(int) {}",
        "static void f(double) {} static void f(int) {}",
    ] {
        let source = format!(
            r#"
struct C {{ {declarations} }};
using F = void (*)(int);
F global = &C::f;
void inline_type() {{ void (*cb)(int) = &C::f; cb(1); }}
void alias() {{ F cb = &C::f; cb(1); }}
void assigned() {{ F cb; cb = &C::f; cb(1); }}
void cast() {{ auto cb = static_cast<F>(&C::f); cb(1); }}
void global_call() {{ global(1); }}
F get() {{ return &C::f; }}
void returned() {{ F cb = get(); cb(1); }}
void stored_round_trip() {{
    using Erased = void (*)(int, int);
    Erased erased;
    Erased *slot = &erased;
    *slot = reinterpret_cast<Erased>(static_cast<F>(&C::f));
    F cb = reinterpret_cast<F>(erased);
    cb(1);
}}
"#
        );
        let root = scratch(&[("main.cpp", &source)]);
        let program = build_program(root.path(), &default_opts(root.path())).unwrap();
        assert!(
            !program.diagnostics.iter().any(|d| d.stage == "parse"),
            "{:?}",
            program.diagnostics
        );
        let (_, analysis) = trace_analysis::analyze(&program);
        for caller in [
            "inline_type",
            "alias",
            "assigned",
            "cast",
            "global_call",
            "returned",
            "stored_round_trip",
        ] {
            let id = only_function(&program, caller);
            let targets: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.caller == id && e.resolution == ResolutionKind::Indirect)
                .collect();
            assert_eq!(targets.len(), 1, "{caller}: {targets:?}; {declarations}");
            let f = program.symbols.function(targets[0].callee);
            assert!(f.is_static_member, "{caller}; {declarations}: {f:?}");
            let param = program.symbols.explicit_params(f).unwrap().get(0).unwrap();
            assert!(
                matches!(
                    program.types.get(param).desc.as_ref(),
                    trace_ir::TypeDesc::Int
                ),
                "{caller}: {f:?}"
            );
        }
    }
}

#[test]
fn nested_callable_declarators_refine_the_matching_signature_layer() {
    for declaration in [
        "typedef void (*(*S)(int))(double);",
        "using S = void (*(*)(int))(double);",
        "using S = Result (*)(int);",
    ] {
        let source = format!(
            r#"
using Result = void (*)(double);
void target(double) {{}}
struct C {{
    static Result s(int) {{ return target; }}
    static Result s(double) {{ return target; }}
    void s(char) {{}}
    Result method(int) {{ return target; }}
}};
{declaration}
void caller() {{ S s = &C::s; Result r = s(1); r(2.0); }}
void cast() {{ auto s = static_cast<S>(&C::s); Result r = s(1); r(2.0); }}
typedef void (*(C::*M)(int))(double);
void member(C *p) {{ M m = &C::method; Result r = (p->*m)(1); r(2.0); }}
using End = void (*)(char);
using Middle = End (*)(double);
void last(char) {{}}
End middle(double) {{ return last; }}
struct DeepSource {{ static Middle get(int) {{ return middle; }} }};
typedef void (*(*(*Deep)(int))(double))(char);
void deep() {{ Deep d = &DeepSource::get; Middle m = d(1); End e = m(2.0); e('x'); }}
"#
        );
        let root = scratch(&[("main.cpp", &source)]);
        let program = build_program(root.path(), &default_opts(root.path())).unwrap();
        assert!(
            !program.diagnostics.iter().any(|d| d.stage == "parse"),
            "{declaration}: {:?}",
            program.diagnostics
        );
        // Assert both layers: selecting the outer overload and subsequently
        // invoking its callback must use distinct parameter lists.
        let desc = program.types.resolve_alias("S").unwrap();
        let trace_ir::TypeDesc::FnPtr { params, ret } = desc.innermost().0 else {
            panic!("{desc:?}")
        };
        assert!(
            matches!(params.as_slice(), [trace_ir::TypeDesc::Int]),
            "{declaration}: {desc:?}"
        );
        let trace_ir::TypeDesc::FnPtr { params, .. } = ret.innermost().0 else {
            panic!("{desc:?}")
        };
        assert!(
            matches!(params.as_slice(), [trace_ir::TypeDesc::Double]),
            "{declaration}: {desc:?}"
        );
        let (_, analysis) = trace_analysis::analyze(&program);
        for (caller, name, count) in [
            ("caller", "C::s", 2),
            ("cast", "C::s", 2),
            ("member", "C::method", 2),
            ("deep", "DeepSource::get", 3),
        ] {
            let id = only_function(&program, caller);
            let edges: Vec<_> = analysis
                .call_edges
                .iter()
                .filter(|e| e.caller == id)
                .collect();
            assert_eq!(edges.len(), count, "{declaration}; {caller}: {edges:?}");
            let callee = edges
                .iter()
                .map(|e| program.symbols.function(e.callee))
                .find(|f| f.name == name)
                .unwrap();
            let ty = program
                .symbols
                .explicit_params(callee)
                .unwrap()
                .get(0)
                .unwrap();
            assert!(matches!(
                program.types.get(ty).desc.as_ref(),
                trace_ir::TypeDesc::Int
            ));
            assert!(has_edge(
                &program,
                &analysis,
                caller,
                if caller == "deep" { "last" } else { "target" },
                ResolutionKind::Indirect
            ));
        }
    }
}

#[test]
fn arrow_star_members_and_non_members_share_overload_selection() {
    let root = scratch(&[(
        "main.cpp",
        r#"
using F = void (*)(int *);
int value;
void from_member(int *p) {}
void from_free(int *p) {}
namespace named {
struct C { F operator->*(double) { return from_member; } };
F operator->*(C, int) { return from_free; }
C make() { return C{}; }
}
namespace adl {
struct C {
    F operator->*(double) { return from_member; }
    friend F operator->*(C, int) { return from_free; }
};
}
int argument() { return 1; }
void free_wins(named::C c) { (c->*1)(&value); }
void member_wins(named::C c) { (c->*1.0)(&value); }
void assigned(named::C c) { F f = c->*1; f(&value); }
void friend_wins(adl::C c) { (c->*1)(&value); }
void unknown() { (named::make()->*argument())(&value); }
#define CALL(c) (c->*1)(&value)
void macro(named::C c) { CALL(c); }
"#,
    )]);
    let mut opts = default_opts(root.path());
    opts.call_sequencing = true;
    let program = build_program(root.path(), &opts).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for (caller, operator, callback) in [
        ("free_wins", "named::operator->*", "from_free"),
        ("member_wins", "named::C::operator->*", "from_member"),
        ("assigned", "named::operator->*", "from_free"),
        ("friend_wins", "adl::operator->*", "from_free"),
        ("macro", "named::operator->*", "from_free"),
    ] {
        let id = only_function(&program, caller);
        let mut names: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == id)
            .map(|e| program.symbols.function(e.callee).name.as_str())
            .collect();
        names.sort();
        let mut expected = vec![operator, callback];
        expected.sort();
        assert_eq!(names, expected, "{caller}");
        let cfg = &program.cfgs[&id][0];
        assert_eq!(cfg.events.len(), 2, "{caller}: {cfg:?}");
        assert_eq!(cfg.order.len(), 1, "{caller}: {cfg:?}");
    }
    let id = only_function(&program, "unknown");
    for target in [
        "named::make",
        "argument",
        "named::operator->*",
        "named::C::operator->*",
        "from_member",
        "from_free",
    ] {
        assert!(
            analysis
                .call_edges
                .iter()
                .any(|e| e.caller == id && program.symbols.function(e.callee).name == target),
            "{target}"
        );
    }
    let sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == id)
        .collect();
    for name in ["named::make", "argument"] {
        assert_eq!(
            sites.iter().filter(|s| s.callee_name == name).count(),
            1,
            "{name}"
        );
    }
    let operators: Vec<_> = sites
        .iter()
        .filter(|s| s.callee_name.ends_with("operator->*"))
        .collect();
    assert_eq!(operators.len(), 2);
    assert_eq!(operators[0].occurrence(), operators[1].occurrence());
    assert!(operators
        .iter()
        .any(|s| s.args_bound_past_this && s.receiver.is_some()));
    assert!(operators
        .iter()
        .any(|s| !s.args_bound_past_this && s.var_args.iter().any(|(i, _)| *i == 0)));
    let cfg = &program.cfgs[&id][0];
    assert_eq!(cfg.events.len(), 4, "{cfg:?}");
    assert_eq!(cfg.order.len(), 3, "{cfg:?}");
    assert_eq!(
        cfg.events.iter().filter(|e| e.sites.len() == 2).count(),
        1,
        "{cfg:?}"
    );
    for callback in ["from_member", "from_free"] {
        assert_local_points_to(&program, &pag, &analysis, callback, "p", "value");
    }
}

#[test]
fn pointer_arrow_star_receivers_consider_free_operators_and_rhs_adl() {
    let root = scratch(&[
        (
            "main.cpp",
            r#"
#include "api.hpp"
using F = void (*)(int *);
int value;
void from_free(int *p) {}
void from_member(int *p) {}
void from_left(int *p) {}
void from_right(int *p) {}
void from_import(int *p) {}
struct Tag {};
struct C {
    F operator->*(Tag) { return from_member; }
    void hit(int *) {}
};
F operator->*(C *p, Tag) { return from_free; }
namespace left {
struct C { F operator->*(Tag) { return from_member; } };
F operator->*(C *, Tag) { return from_left; }
}
namespace right {
struct RequestTag { friend F operator->*(C *, RequestTag) { return from_right; } };
struct RawTag {};
F operator->*(int *, RawTag) { return from_right; }
}
void inline_rhs(C *p) { (p->*Tag{})(&value); }
void named_rhs(C *p, Tag tag) { (p->*tag)(&value); }
void assigned(C *p) { F f = p->*Tag{}; f(&value); }
void lhs_adl(left::C *p) { (p->*Tag{})(&value); }
void rhs_adl(C *p) { (p->*right::RequestTag{})(&value); }
void raw_adl(int *p) { (p->*right::RawTag{})(&value); }
void declared(imported::C *p) { (p->*imported::Tag{})(&value); }
void class_value(C c) { (c->*Tag{})(&value); }
void builtin(C *p) { auto m = &C::hit; (p->*m)(&value); }
#define CALL(p) (p->*Tag{})(&value)
void macro(C *p) { CALL(p); }
"#,
        ),
        (
            "api.hpp",
            r#"
#pragma once
namespace imported {
using Callback = void (*)(int *);
struct C {};
struct Tag {};
Callback operator->*(C *, Tag);
}
"#,
        ),
        (
            "operator.cpp",
            r#"
#include "api.hpp"
void from_import(int *);
namespace imported {
Callback operator->*(C *, Tag) { return from_import; }
}
"#,
        ),
    ]);
    let mut opts = default_opts(root.path());
    opts.call_sequencing = true;
    let program = build_program(root.path(), &opts).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for (caller, operator, callback) in [
        ("inline_rhs", "operator->*", "from_free"),
        ("named_rhs", "operator->*", "from_free"),
        ("assigned", "operator->*", "from_free"),
        ("lhs_adl", "left::operator->*", "from_left"),
        ("rhs_adl", "right::operator->*", "from_right"),
        ("raw_adl", "right::operator->*", "from_right"),
        ("declared", "imported::operator->*", "from_import"),
        ("class_value", "C::operator->*", "from_member"),
        ("macro", "operator->*", "from_free"),
    ] {
        let id = only_function(&program, caller);
        let mut names: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == id)
            .map(|e| program.symbols.function(e.callee).name.as_str())
            .collect();
        names.sort();
        let mut expected = vec![operator, callback];
        expected.sort();
        assert_eq!(names, expected, "{caller}");
        let cfg = &program.cfgs[&id][0];
        assert_eq!(cfg.events.len(), 2, "{caller}: {cfg:?}");
        assert_eq!(cfg.order.len(), 1, "{caller}: {cfg:?}");
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|s| s.caller == id && s.callee_name == operator)
            .unwrap();
        assert_eq!(
            site.args_bound_past_this,
            caller == "class_value",
            "{caller}"
        );
        if caller != "class_value" {
            let pointer = program
                .symbols
                .variables
                .iter()
                .find(|v| v.fn_id == Some(id) && v.name == "p")
                .unwrap();
            assert!(
                site.var_args.contains(&(0, pointer.id)),
                "{caller}: {site:?}"
            );
        }
    }
    let builtin = only_function(&program, "builtin");
    let targets: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| e.caller == builtin)
        .map(|e| program.symbols.function(e.callee).name.as_str())
        .collect();
    assert_eq!(targets, ["C::hit"]);
    for callback in [
        "from_free",
        "from_left",
        "from_right",
        "from_member",
        "from_import",
    ] {
        assert_local_points_to(&program, &pag, &analysis, callback, "p", "value");
    }
}

#[test]
fn free_arrow_star_receivers_admit_base_conversions_and_inherited_adl() {
    let root = scratch(&[(
        "main.cpp",
        r#"
using Fn = void (*)(int *);
int value;
void from_base(int *p) {}
void from_adl(int *p) {}
struct Base {};
struct Middle : Base {};
struct Derived : Middle {};
Fn operator->*(Base &, int) { return from_base; }
Fn operator->*(const Base &, long) { return from_base; }
void base(Base &b) { (b->*0)(&value); }
void derived(Derived &d) { (d->*0)(&value); }
void by_value(Derived d) { (d->*0)(&value); }
void const_derived(const Derived &d) { (d->*0L)(&value); }
void assigned(Derived &d) { Fn f = d->*0; f(&value); }
#define CALL(d) (d->*0)(&value)
void macro(Derived &d) { CALL(d); }
namespace api {
struct Root {};
Fn operator->*(Root &, int) { return from_adl; }
void visit(Root &) {}
struct FriendRoot { friend Fn operator->*(FriendRoot &, int) { return from_adl; } };
}
namespace impl {
struct Derived : api::Root {};
struct FriendDerived : api::FriendRoot {};
}
void inherited_adl(impl::Derived &d) { (d->*0)(&value); }
void friend_adl(impl::FriendDerived &d) { (d->*0)(&value); }
void ordinary_adl(impl::Derived &d) { visit(d); }
namespace copied {
struct Base {};
struct Derived : Base {};
Fn operator->*(Base, int) { return from_base; }
}
void copied_base(copied::Derived &d) { (d->*0)(&value); }
"#,
    )]);
    let mut opts = default_opts(root.path());
    opts.call_sequencing = true;
    let program = build_program(root.path(), &opts).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    for (caller, operator, callback) in [
        ("base", "operator->*", "from_base"),
        ("derived", "operator->*", "from_base"),
        ("by_value", "operator->*", "from_base"),
        ("const_derived", "operator->*", "from_base"),
        ("assigned", "operator->*", "from_base"),
        ("macro", "operator->*", "from_base"),
        ("inherited_adl", "api::operator->*", "from_adl"),
        ("friend_adl", "api::operator->*", "from_adl"),
        ("copied_base", "copied::operator->*", "from_base"),
    ] {
        let id = only_function(&program, caller);
        let mut names: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == id)
            .map(|e| program.symbols.function(e.callee).name.as_str())
            .collect();
        names.sort();
        let mut expected = vec![operator, callback];
        expected.sort();
        assert_eq!(names, expected, "{caller}");
        let cfg = &program.cfgs[&id][0];
        assert_eq!(cfg.events.len(), 2, "{caller}: {cfg:?}");
        assert_eq!(cfg.order.len(), 1, "{caller}: {cfg:?}");
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|s| s.caller == id && s.callee_name == operator)
            .unwrap();
        assert!(!site.args_bound_past_this, "{caller}: {site:?}");
    }
    assert!(has_edge(
        &program,
        &analysis,
        "ordinary_adl",
        "api::visit",
        ResolutionKind::Direct
    ));
    for callback in ["from_base", "from_adl"] {
        assert_local_points_to(&program, &pag, &analysis, callback, "p", "value");
    }
}
