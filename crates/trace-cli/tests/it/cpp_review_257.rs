use crate::common::*;
use trace_parse::build_program;

pub(super) fn verify(
    source: &str,
    edges: &[(&str, &str)],
) -> (trace_ir::Program, trace_analysis::AnalysisResult) {
    let root = scratch(&[("main.cpp", source)]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}\n{source}",
        program.diagnostics
    );
    let (_, analysis) = trace_analysis::analyze(&program);
    for &(caller, target) in edges {
        assert!(
            analysis
                .call_edges
                .iter()
                .any(|e| program.symbols.function(e.caller).name == caller
                    && program.symbols.function(e.callee).name == target),
            "missing {caller} -> {target}\n{source}"
        );
    }
    (program, analysis)
}

#[test]
fn data_members_are_values_and_lvalues() {
    let prefix = "using F = void (*)(); void alpha() {} struct S { F cb; }; F S::*pm = &S::cb;\n";
    for (body, caller) in [
        (
            "void read(S *s) { s->cb = alpha; F f = s->*pm; f(); }",
            "read",
        ),
        ("void write(S *s) { s->*pm = alpha; s->cb(); }", "write"),
        (
            "F get(S*s) { return s->*pm; } void returned(S*s) { s->cb=alpha; F f=get(s); f(); }",
            "returned",
        ),
        (
            "void use(F f) { f(); } void argument(S*s) { s->cb=alpha; use(s->*pm); }",
            "use",
        ),
    ] {
        verify(&format!("{prefix}{body}"), &[(caller, "alpha")]);
    }
    verify("struct O { void run() {} }; struct S { O o; }; O S::*po=&S::o; void recv(S*s) { (s->*po).run(); }", &[("recv", "O::run")]);
}

#[test]
fn inferred_and_dependent_data_members() {
    verify(
        r#"
using F = void (*)(); void alpha() {} struct S { F cb; };
void automatic(S *s) { s->cb = alpha; auto pm = &S::cb; (s->*pm)(); }
void decltyped(S *s) { s->cb = alpha; decltype(&S::cb) pm = &S::cb; (s->*pm)(); }
template<class M> void templated(S *s, M pm) { (s->*pm)(); }
void caller(S *s) { s->cb = alpha; templated(s, &S::cb); }
"#,
        &[
            ("automatic", "alpha"),
            ("decltyped", "alpha"),
            ("caller", "templated"),
            ("templated", "alpha"),
        ],
    );
}

#[test]
fn inherited_member_addresses() {
    verify(
        r#"
struct A { void a() {} }; struct D : A {};
void local(D *p) { void (D::*m)() = &D::a; (p->*m)(); }
void inlined(D *p) { (p->*&D::a)(); }
void automatic(D *p) { auto m = &D::a; (p->*m)(); }
"#,
        &[
            ("local", "A::a"),
            ("inlined", "A::a"),
            ("automatic", "A::a"),
        ],
    );
}

#[test]
fn declarator_aliases_arrays_and_template_owners() {
    verify(
        r#"
using N = int; using F = void (*)(); void alpha() {};
struct C { void one(N) {} void callback(F f) { f(); } void a() {} void b() {} };
void named(C *p) { void (C::*m)(N) = &C::one; (p->*m)(1); }
void callback(C *p) { void (C::*m)(F) = &C::callback; (p->*m)(alpha); }
void local(C *p, int i) { void (C::*t[2])() = {&C::a, &C::b}; (p->*t[i])(); }
void (C::*g[2])() = {&C::a, &C::b};
void global(C *p, int i) { (p->*g[i])(); }
template<class T> struct Box { int get() { return 1; } };
using M = int (Box<int>::*)();
void owner(Box<int> *p) { int (Box<int>::*m)() = &Box<int>::get; (p->*m)(); }
struct S { F cb; }; using PM = F S::*;
void data(S *s) { s->cb = alpha; PM pm = &S::cb; (s->*pm)(); }
"#,
        &[
            ("named", "C::one"),
            ("callback", "C::callback"),
            ("C::callback", "alpha"),
            ("local", "C::a"),
            ("local", "C::b"),
            ("global", "C::a"),
            ("global", "C::b"),
            ("owner", "Box::get"),
            ("data", "alpha"),
        ],
    );
}

#[test]
fn attributed_instantiations_and_pointer_defaults() {
    verify(
        r#"
#define API __attribute__((visibility("default")))
template<class T> struct Box { void run() {} };
template class API Box<int>;
extern template class __attribute__((visibility("default"))) Box<double>;
template class [[gnu::visibility("default")]] Box<char>;
template<bool, class T = void> struct EI { using type = T; };
template<bool B, class T = void> using et = typename EI<B,T>::type;
template<class T, et<sizeof(T) == 4, int> ** = nullptr> void a(T) {}
template<class T, et<sizeof(T) == 4, int> * const = nullptr> void b(T) {}
template<class T, et<sizeof(T) == 4, int> * = nullptr, class U = T *> U c(T) { return nullptr; }
"#,
        &[],
    );
}

#[test]
fn compound_returns_and_unknown_standard_integers() {
    verify(
        r#"
void z() {};
struct C { void (*hit())() { return z; } int (*array())[3] { return nullptr; } void take(long) {} unsigned long get() { return 0; } };
using F = void (*)(); using M = F (C::*)();
using A = int (*)[3]; using P = A (C::*)();
using I = void (C::*)(int64_t); using G = uint64_t (C::*)();
using SI = void (C::*)(std::int64_t); using SG = std::uint64_t (C::*)();
void caller(C *p) { M m = &C::hit; F f = (p->*m)(); f(); P a = &C::array; (p->*a)(); I i = &C::take; (p->*i)(1); G g = &C::get; (p->*g)(); SI si=&C::take; (p->*si)(1); SG sg=&C::get; (p->*sg)(); }
"#,
        &[
            ("caller", "C::hit"),
            ("caller", "z"),
            ("caller", "C::array"),
            ("caller", "C::take"),
            ("caller", "C::get"),
        ],
    );
}

#[test]
fn variadic_and_conversion_members_keep_their_category() {
    let (program, analysis) = verify(
        r#"
namespace ns { struct S {}; }
struct B { virtual operator ns::S() const { return {}; } };
struct D : B { operator ns::S() const { return {}; } };
using M = ns::S (B::*)() const; M m = &B::operator ns::S;
void conversion(B *p) { (p->*m)(); }
struct C { static void hit(int) {} void hit(int, ...) {} };
using V = void (C::*)(int, ...); V v = &C::hit;
void variadic(C *p) { (p->*v)(1); }
"#,
        &[
            ("conversion", "B::operator ns::S"),
            ("conversion", "D::operator ns::S"),
            ("variadic", "C::hit"),
        ],
    );
    let targets: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|e| program.symbols.function(e.caller).name == "variadic")
        .collect();
    assert_eq!(targets.len(), 1);
    assert!(!program.symbols.function(targets[0].callee).is_static_member);
}

#[test]
fn using_directives_keep_receiver_ownership() {
    verify(
        r#"
namespace EventRaw { struct Base { void hit() {} }; }
namespace Hiview { using namespace EventRaw; struct Test : Base { void run() { hit(); } }; void use(Base *p) { p->hit(); } }
"#,
        &[
            ("Hiview::Test::run", "EventRaw::Base::hit"),
            ("Hiview::use", "EventRaw::Base::hit"),
        ],
    );
}

#[test]
fn member_calls_keep_loop_and_unevaluated_context() {
    let source = r#"
struct C { void hit() {} }; using M = void (C::*)();
void loop(C *p, M m) { while (true) { (p->*m)(); break; } }
void never(C *p, M m) { (void)sizeof((p->*m)(), 1); (void)noexcept((p->*m)()); }
"#;
    let root = scratch(&[("main.cpp", source)]);
    let program = build_program(root.path(), &default_opts(root.path())).unwrap();
    for caller in ["loop", "never"] {
        let id = only_function(&program, caller);
        let sites: Vec<_> = program
            .symbols
            .call_sites
            .iter()
            .filter(|s| s.caller == id && s.callee_var.is_some())
            .collect();
        assert!(!sites.is_empty());
        assert!(
            sites.iter().all(|s| if caller == "loop" {
                s.in_loop
            } else {
                s.unevaluated
            }),
            "{caller}: {sites:?}"
        );
    }
}

#[test]
fn data_member_pointer_array_assignment() {
    verify("using Fn=void(*)(int*); int value; void alpha(int*p){} struct S{Fn cb;}; void array(S*p){Fn S::*members[1]; members[0]=&S::cb; (p->*members[0])(&value);} S object; void source(){object.cb=alpha; array(&object);}", &[("array","alpha")]);
}

#[test]
fn ordinary_reinterpreted_values_keep_round_trip_flow() {
    let (program, analysis) = verify(
        r#"
void hit(int) {} using I = void (*)(int); using D = void (*)(int,int);
struct S { D cb; };
void erased(S *s) { s->cb(1,2); }
void store(S *s) { s->cb = reinterpret_cast<D>(&hit); }
void local(S *s) { I p = reinterpret_cast<I>(s->cb); p(1); }
void inline_cast(S *s) { (reinterpret_cast<I>(s->cb))(1); }
void consume(I f) { f(1); }
void pass(S *s) { consume(reinterpret_cast<I>(s->cb)); }
"#,
        &[("local", "hit"), ("inline_cast", "hit"), ("consume", "hit")],
    );
    assert!(!analysis
        .call_edges
        .iter()
        .any(|e| program.symbols.function(e.caller).name == "erased"
            && program.symbols.function(e.callee).name == "hit"));
}

#[test]
fn variadic_callbacks_survive_indirect_storage() {
    verify(
        r#"
using F = void (*)(int, int, ...);
void target(int, int, ...) {}
void set(F *p) { *p = target; }
void test() { F f; set(&f); f(1, 2, 3); }
struct Slots { F callback; F table[2]; };
void field(Slots *s) { set(&s->callback); s->callback(1, 2, 3); }
void array(Slots *s) { s->table[0] = target; s->table[0](1, 2, 3); }
"#,
        &[("test", "target"), ("field", "target"), ("array", "target")],
    );
}

#[test]
fn callable_data_member_declarators_keep_the_data_pointer_layer() {
    let (program, _) = verify(
        r#"
void alpha() {}
struct S { void (*cb)(); };
void (*S::*pm)() = &S::cb;
void direct(S *s) { s->cb = alpha; (s->*pm)(); }
"#,
        &[("direct", "alpha")],
    );
    let pm = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "pm")
        .unwrap();
    let desc = program.types.get(pm.type_id).desc.as_ref();
    assert!(
        matches!(desc, trace_ir::TypeDesc::MemberPointer { is_function: false, member, .. }
        if matches!(member.as_ref(), trace_ir::TypeDesc::Ptr(inner) if matches!(inner.as_ref(), trace_ir::TypeDesc::FnPtr { .. })))
    );
}

#[test]
fn pointers_to_data_member_pointers_keep_the_outer_pointer() {
    let (program, _) = verify(
        r#"
using F = void (*)(); void alpha() {} struct S { F cb; };
F S::*pm = &S::cb;
F S::**p = &pm;
void indirect(S *s) { s->cb = alpha; (s->**p)(); }
"#,
        &[("indirect", "alpha")],
    );
    let p = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == "p")
        .unwrap();
    let desc = program.types.get(p.type_id).desc.as_ref();
    assert!(matches!(desc, trace_ir::TypeDesc::Ptr(inner)
        if matches!(inner.as_ref(), trace_ir::TypeDesc::MemberPointer { is_function: false, .. })));
}

#[test]
fn chained_member_bindings_keep_each_operator_invocation() {
    let (program, analysis) = verify(
        r#"
struct V { V operator->*(int) { return {}; } };
V operator+(int, V) { return {}; }
V chain(V v) { return 1 + v->*0->*1; }
"#,
        &[("chain", "V::operator->*")],
    );
    let sites: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|edge| {
            program.symbols.function(edge.caller).name == "chain"
                && program.symbols.function(edge.callee).name == "V::operator->*"
        })
        .map(|e| e.call_site)
        .collect();
    assert_eq!(sites.len(), 2, "{sites:?}");
    assert_ne!(sites[0], sites[1]);
}

#[test]
fn known_empty_callable_signatures_select_arity_and_return_type() {
    let (program, analysis) = verify(
        r#"
struct C { static void f() {} static void f(int) {} static int value() { return 0; } };
void (*cb)() = &C::f;
using Z = void(); Z *cb2 = &C::f;
void test() { cb(); }
void alias_call() { cb2(); }
"#,
        &[("test", "C::f"), ("alias_call", "C::f")],
    );
    let targets: Vec<_> = analysis
        .call_edges
        .iter()
        .filter(|edge| program.symbols.function(edge.caller).name == "test")
        .collect();
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert_eq!(
        program
            .symbols
            .explicit_params(program.symbols.function(targets[0].callee))
            .unwrap()
            .len(),
        0
    );
    let known = trace_ir::TypeDesc::FnPtr {
        ret: Box::new(trace_ir::TypeDesc::Void),
        params: Vec::new(),
    };
    let unknown = trace_ir::TypeDesc::FnPtr {
        ret: Box::new(trace_ir::TypeDesc::Unknown),
        params: vec![trace_ir::TypeDesc::Unknown],
    };
    for f in program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "C::f" || f.name == "C::value")
    {
        assert_eq!(
            program.symbols.function_pointer_signature_admits(
                f.id,
                &known,
                &program.types,
                program.reference_returns.contains(&f.id)
            ),
            f.name == "C::f" && program.symbols.explicit_params(f).unwrap().len() == 0,
            "{}: params {:?}, ret {:?}",
            f.name,
            f.params,
            program.types.get(f.return_type).desc
        );
        assert!(program.symbols.function_pointer_signature_admits(
            f.id,
            &unknown,
            &program.types,
            program.reference_returns.contains(&f.id)
        ));
    }
}
