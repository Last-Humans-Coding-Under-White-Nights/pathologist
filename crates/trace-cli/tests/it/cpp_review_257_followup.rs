use super::cpp_review_257::verify;
use crate::common::*;

#[test]
fn inferred_static_member_addresses_are_ordinary_callbacks() {
    for declaration in [
        "struct C { static void f(int) {} static void zero() {} };",
        "struct C { static void f(int); static void zero(); }; void C::f(int) {} void C::zero() {}",
        "struct C { static void f(int); static void zero(); };",
    ] {
        verify(
            &format!(
                r#"
{declaration}
struct V {{ static void f(int, ...) {{}} }};
void caller() {{ auto p = &C::f; p(1); }}
void zero() {{ auto p = &C::zero; p(); }}
void typed() {{ void (*p)(int) = &C::f; p(1); }}
void variadic() {{ auto p = &V::f; p(1,2); }}
"#
            ),
            &[
                ("caller", "C::f"),
                ("zero", "C::zero"),
                ("typed", "C::f"),
                ("variadic", "V::f"),
            ],
        );
    }
}

#[test]
fn inferred_callable_signatures_restore_reference_returns() {
    verify(
        r#"
int value;
struct C {
    int& ref() { return value; }
    int&& move_ref() { return static_cast<int&&>(value); }
    static int& static_ref() { return value; }
    int& var_ref(int, ...) { return value; }
};
int& free_ref() { return value; }
void arrow(C *c) { auto p = &C::ref; (c->*p)(); }
void dot(C& c) { auto p = &C::ref; (c.*p)(); }
void rvalue(C *c) { auto p = &C::move_ref; (c->*p)(); }
void ordinary() { auto p = &C::static_ref; p(); }
void free_function() { auto p = &free_ref; p(); }
void variadic(C *c) { auto p = &C::var_ref; (c->*p)(1,2); }
"#,
        &[
            ("arrow", "C::ref"),
            ("dot", "C::ref"),
            ("rvalue", "C::move_ref"),
            ("ordinary", "C::static_ref"),
            ("free_function", "free_ref"),
            ("variadic", "C::var_ref"),
        ],
    );
}

#[test]
fn reference_returning_callable_signatures_keep_their_reference_layer() {
    let (program, analysis) = verify(
        r#"
int value;
struct C {
    static int& cb() { return value; }
    static int& cb(int) { return value; }
    int& member() { return value; }
    int& member(int) { return value; }
    int&& rvalue() { return static_cast<int&&>(value); }
};
using F = int&(*)(); using M = int&(C::*)();
struct Use { void call(F f) { f(); } };
void argument(Use& u) { u.call(&C::cb); }
void ordinary() { F f = &C::cb; f(); }
void member(C& c) { M f = &C::member; (c.*f)(); }
void stored() { F f; F *p = &f; *p = &C::cb; f(); }
void rvalue(C& c) { using R = int&&(C::*)(); R f = &C::rvalue; (c.*f)(); }
void cast_ordinary() { using V = int(*)(); V f = reinterpret_cast<V>(static_cast<F>(&C::cb)); reinterpret_cast<F>(f)(); }
void incompatible() { using V = int(*)(); V f = reinterpret_cast<int(*)()>(static_cast<F>(&C::cb)); f(); }
void incompatible_member(C& c) { using V = int(C::*)(); V f = reinterpret_cast<int(C::*)()>(static_cast<M>(&C::member)); (c.*f)(); }
void cast_member(C& c) { using V = int(C::*)(); V f = reinterpret_cast<int(C::*)()>(static_cast<M>(&C::member)); (c.*reinterpret_cast<M>(f))(); }
"#,
        &[
            ("ordinary", "C::cb"),
            ("member", "C::member"),
            ("stored", "C::cb"),
            ("rvalue", "C::rvalue"),
            ("cast_ordinary", "C::cb"),
            ("cast_member", "C::member"),
            ("Use::call", "C::cb"),
        ],
    );
    assert!(!has_any_edge(&program, &analysis, "incompatible", "C::cb"));
    assert!(!has_any_edge(
        &program,
        &analysis,
        "incompatible_member",
        "C::member"
    ));
    for caller in ["ordinary", "member", "stored"] {
        let targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| program.symbols.function(e.caller).name == caller)
            .collect();
        assert_eq!(targets.len(), 1, "{caller}: {targets:?}");
        assert!(program
            .symbols
            .explicit_params(program.symbols.function(targets[0].callee))
            .unwrap()
            .is_empty());
    }
}

#[test]
fn returned_reinterpreted_addresses_survive_callback_storage() {
    let (program, analysis) = verify(
        r#"
using F = void(*)(int,int); using G = void(*)(int);
struct C { static void target(int,int) {} };
void free_target(int,int) {}
G erase() { return reinterpret_cast<G>(&C::target); }
G erase_free() { return reinterpret_cast<G>(&free_target); }
G erase_cstyle() { return (void (*)(int))&C::target; }
void put(G *out) { *out = erase(); }
void put_free(G *out) { *out = erase_free(); }
void put_cstyle(G *out) { *out = erase_cstyle(); }
void restored() { G g; put(&g); F f = reinterpret_cast<F>(g); f(1,2); }
void restored_free() { G g; put_free(&g); F f = reinterpret_cast<F>(g); f(1,2); }
void restored_cstyle() { G g; put_cstyle(&g); F f = reinterpret_cast<F>(g); f(1,2); }
void incompatible() { G g; put(&g); g(1); }
"#,
        &[
            ("restored", "C::target"),
            ("restored_free", "free_target"),
            ("restored_cstyle", "C::target"),
        ],
    );
    assert!(!has_any_edge(
        &program,
        &analysis,
        "incompatible",
        "C::target"
    ));
}

#[test]
fn opaque_function_values_survive_local_and_forwarded_arguments() {
    for extension in ["c", "cpp"] {
        let source = r#"
typedef void (*F)(int); void good(int n) {}
void call(void *v) { ((F)v)(1); }
void forward(void *v) { call(v); }
void initialized(void) { void *a = (void *)good; forward(a); }
void assigned(void) { void *a; a = (void *)good; forward(a); }
void put(void **p, void *v) { *p = v; }
void stored(void) { F f; void *a = (void *)good; put((void **)&f, a); f(1); }
"#;
        let root = scratch(&[(&format!("main.{extension}"), source)]);
        let program = trace_parse::build_program(root.path(), &default_opts(root.path())).unwrap();
        let (_, analysis) = trace_analysis::analyze(&program);
        for caller in ["call", "stored"] {
            assert!(
                has_any_edge(&program, &analysis, caller, "good"),
                "{extension}: {caller}"
            );
        }
    }
}

#[test]
fn conversion_addresses_stop_at_template_argument_boundaries() {
    verify(
        r#"
struct C { explicit operator bool() const { return true; } bool ok() const { return true; } };
template<bool (C::*M)() const> struct Probe { void run() {} };
void conversion() { Probe<&C::operator bool> probe; probe.run(); }
void named() { Probe<&C::ok> probe; probe.run(); }
"#,
        &[("conversion", "Probe::run"), ("named", "Probe::run")],
    );
}

#[test]
fn static_callback_selection_covers_bare_names_stores_and_arguments() {
    for declarations in [
        "static void f() {} static void f(int) {}",
        "static void f(int) {} static void f() {}",
    ] {
        let source = format!(
            r#"
struct C {{ {declarations} }}; using F=void(*)(); struct H{{ F cb; }};
void bare(){{ F f=C::f; f(); }}
void field(H*h){{ h->cb=&C::f; h->cb(); }}
void take(F f){{ f(); }} void argument(){{ take(&C::f); }}
"#
        );
        let (p, a) = verify(
            &source,
            &[("bare", "C::f"), ("field", "C::f"), ("take", "C::f")],
        );
        for caller in ["bare", "field", "take"] {
            let edges: Vec<_> = a
                .call_edges
                .iter()
                .filter(|e| p.symbols.function(e.caller).name == caller)
                .collect();
            assert_eq!(edges.len(), 1, "{caller}: {edges:?}");
            assert_eq!(
                p.symbols
                    .explicit_params(p.symbols.function(edges[0].callee))
                    .unwrap()
                    .len(),
                0
            );
        }
    }
}

#[test]
fn free_arrow_star_uses_ordinary_scope_and_imports() {
    verify(
        r#"
using F=void(*)(); void alpha(){} namespace ns { struct P{}; }
namespace ops { F operator->*(ns::P,int){return alpha;} void local(ns::P p){(p->*0)();} }
void declared(ns::P p){using ops::operator->*; (p->*0)();}
void directive(ns::P p){using namespace ops; (p->*0)();}
"#,
        &[
            ("ops::local", "ops::operator->*"),
            ("ops::local", "alpha"),
            ("declared", "alpha"),
            ("directive", "alpha"),
        ],
    );
}

#[test]
fn non_type_template_arguments_compare_integral_values() {
    for (left, right) in [("2", "02"), ("2", "0x2u"), ("1", "true"), ("0", "false")] {
        verify(
            &format!(
                r#"
template<int N,class T=int> struct Box{{T*get();}};
struct C{{void hit(Box<{left}>*){{}}}}; using M=void(C::*)(Box<{right}>*);
M m=&C::hit; void caller(C*p){{(p->*m)(nullptr);}}
"#
            ),
            &[("caller", "C::hit")],
        );
    }
}

#[test]
fn returned_functors_resolve_their_call_operator() {
    verify(
        r#"
struct Fun { void operator()() {} }; struct P { Fun operator->*(int) { return {}; } };
void caller(P p){(p->*0)();} void control(P p){Fun f=p->*0; f();}
"#,
        &[
            ("caller", "P::operator->*"),
            ("caller", "Fun::operator()"),
            ("control", "Fun::operator()"),
        ],
    );
}

#[test]
fn returned_data_member_pointer_keeps_its_category() {
    verify(
        r#"
using F=void(*)(); void alpha(){} struct C{F f;}; F C::*get(){return &C::f;}
void caller(C*p){p->f=alpha;(p->*get())();}
"#,
        &[("caller", "get"), ("caller", "alpha")],
    );
}

#[test]
fn enum_operands_participate_in_arrow_star_lookup() {
    verify(
        r#"
using F=void(*)(); void alpha(){} struct C{}; enum class Tag{go};
F operator->*(C*,Tag){return alpha;} void caller(C*p){(p->*Tag::go)();}
"#,
        &[("caller", "operator->*"), ("caller", "alpha")],
    );
}

#[test]
fn lone_free_operator_must_accept_its_right_operand() {
    let (p, a) = verify(
        r#"
using F=void(*)(); void alpha(){} struct C{void a(){}}; using M=void(C::*)(); struct Tag{};
F operator->*(C*,Tag){return alpha;} struct Holder{operator M()const{return &C::a;}};
void tagged(C*p){(p->*Tag{})();} void held(C*p,Holder h){(p->*h)();}
"#,
        &[("tagged", "alpha")],
    );
    assert!(!has_any_edge(&p, &a, "held", "operator->*"));
    assert!(!has_any_edge(&p, &a, "held", "alpha"));
}

#[test]
fn member_pointer_slots_keep_persistent_forwarding_flow() {
    verify(
        r#"
struct C{void hit(){}}; using M=void(C::*)();
void set(M*out){*out=&C::hit;} void helper(M*out){set(out);}
void caller(C*p){M m;helper(&m);(p->*m)();}
"#,
        &[("caller", "C::hit")],
    );
}

#[test]
fn overloads_take_precedence_over_data_member_binding() {
    verify(
        r#"
using F=void(*)(); void alpha(){} void beta(){} struct S{F cb;}; F S::*pm=&S::cb;
struct Proxy{F operator->*(F S::*){return beta;}};
void init(Proxy p){F f=p->*pm;f();} void use(F f){f();} void argument(Proxy p){use(p->*pm);}
"#,
        &[("init", "beta"), ("use", "beta")],
    );
}

#[test]
fn implicit_arrow_star_in_sizeof_is_unevaluated() {
    let (p, a) = verify(
        r#"
struct C{int operator->*(int){return 0;}};
void never(C c){(void)sizeof(c->*1);} void runs(C c){(void)(c->*1);}
"#,
        &[("runs", "C::operator->*")],
    );
    assert!(!has_any_edge(&p, &a, "never", "C::operator->*"));
    let id = only_function(&p, "never");
    assert!(p
        .symbols
        .call_sites
        .iter()
        .filter(|s| s.caller == id)
        .all(|s| s.unevaluated));
}
