#[path = "it/common/mod.rs"]
mod common;
use common::{fn_name, has_edge};
use trace_analysis::{analyze, ResolutionKind};
use trace_ir::Program;
use trace_preproc::PreprocessOptions;

fn check(
    source: &str,
    standard: &str,
    test: impl FnOnce(&Program, &trace_analysis::AnalysisResult),
) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.cpp"), source).unwrap();
    let opts = PreprocessOptions::new()
        .with_include(dir.path().to_path_buf())
        .with_define("__cplusplus", standard);
    let program = trace_parse::build_program_with_jobs(dir.path(), &opts, 1).unwrap();
    let (_, result) = analyze(&program);
    test(&program, &result);
}
const CALLBACK: &str = "using Callback = void (*)(); void Handler() {} void First() {} void Second() {} struct Leaf { Leaf(Callback cb) { cb(); } };";
fn call(p: &Program, a: &trace_analysis::AnalysisResult, callee: &str) {
    assert!(
        has_edge(p, a, "Owner::Owner", callee, ResolutionKind::Direct),
        "Owner must call {callee}"
    );
}
fn callbacks(p: &Program, a: &trace_analysis::AnalysisResult, callee: &str, handlers: &[&str]) {
    call(p, a, callee);
    for handler in handlers {
        assert!(
            p.symbols
                .call_sites
                .iter()
                .any(|s| fn_name(p, s.caller) == "Owner::Owner"
                    && s.callee_name.as_ref() == callee
                    && s.fn_args()
                        .iter()
                        .any(|&(i, f)| i == 1 && fn_name(p, f) == *handler)),
            "{handler} must bind past this at {callee}"
        );
        assert!(has_edge(p, a, callee, handler, ResolutionKind::Indirect));
    }
}
macro_rules! callback_case {
    ($name:ident, $source:literal, $handlers:expr) => {
        #[test]
        fn $name() {
            check(&format!("{CALLBACK}\n{}", $source), "201703L", |p, a| {
                callbacks(p, a, "Leaf::Leaf", $handlers)
            });
        }
    };
}
#[test]
fn parenthesized_aggregate_respects_cxx_standard() {
    let src=format!("{CALLBACK} struct Derived : Leaf {{}}; struct Owner {{ Derived value; Owner() : value(Handler) {{}} }};");
    check(&src, "202002L", |p, a| {
        callbacks(p, a, "Leaf::Leaf", &["Handler"])
    });
    check(&src, "201703L", |p, a| {
        assert!(!has_edge(
            p,
            a,
            "Owner::Owner",
            "Leaf::Leaf",
            ResolutionKind::Direct
        ))
    });
}
#[test]
fn inherited_constructor_is_hidden_by_own_signature() {
    check("struct Base { Base() {} Base(int) {} }; struct Derived : Base { using Base::Base; Derived(int) : Base() {} }; struct Owner { Derived value; Owner() : value(1) {} };", "201703L", |p,a|{
        call(p,a,"Derived::Derived");
        assert!(!has_edge(p,a,"Owner::Owner","Base::Base",ResolutionKind::Direct));
        assert!(has_edge(p,a,"Derived::Derived","Base::Base",ResolutionKind::Direct));
    });
}
callback_case!(brace_elided_scalar_array,"struct Aggregate { int numbers[2]; Leaf leaf; }; struct Owner { Aggregate value; Owner(): value{1,2,Handler} {} };", &["Handler"]);
callback_case!(brace_elided_multidimensional_array,"struct Aggregate { int numbers[2][2]; Leaf leaf; }; struct Owner { Aggregate value; Owner(): value{1,2,3,4,Handler} {} };", &["Handler"]);
callback_case!(
    class_member_array,
    "struct Owner { Leaf leaves[2]; Owner(): leaves{First,Second} {} };",
    &["First", "Second"]
);
callback_case!(nested_class_array,"struct Aggregate { Leaf leaves[2]; }; struct Owner { Aggregate value; Owner(): value{{First,Second}} {} };", &["First","Second"]);
callback_case!(elided_class_array,"struct Aggregate { Leaf leaves[2]; }; struct Owner { Aggregate value; Owner(): value{First,Second} {} };", &["First","Second"]);
callback_case!(
    nested_class_array_braces,
    "struct Owner { Leaf leaves[2][1]; Owner(): leaves{{First},{Second}} {} };",
    &["First", "Second"]
);
#[test]
fn empty_nested_aggregate_constructs_members() {
    check("struct Leaf { Leaf() {} }; struct Inner { Leaf leaf; }; struct Outer { Inner inner; }; struct Owner { Outer value; Owner(): value{} {} };","201703L",|p,a|call(p,a,"Leaf::Leaf"));
}
#[test]
fn explicit_empty_aggregate_base_constructs_members() {
    check("struct Leaf { Leaf() {} }; struct Inner { Leaf leaf; }; struct Outer:Inner { int n; }; struct Owner { Outer value; Owner(): value{{},1} {} };","201703L",|p,a|call(p,a,"Leaf::Leaf"));
}
callback_case!(omitted_member_uses_default_initializer,"struct Aggregate { int n; Leaf leaf=Handler; }; struct Owner { Aggregate value; Owner(): value{1} {} };", &["Handler"]);
callback_case!(explicit_member_overrides_default_initializer,"struct Aggregate { int n; Leaf leaf=Handler; }; struct Owner { Aggregate value; Owner(): value{1,First} {} };", &["First"]);
#[test]
fn implicit_copy_constructs_nontrivial_base() {
    check("struct Base { Base(const Base&) {} }; struct Derived:Base {}; struct Owner { Derived value; Owner(const Derived& other): value(other) {} };","201703L",|p,a|call(p,a,"Base::Base"));
}
#[test]
fn implicit_move_constructs_nontrivial_base() {
    check("struct Base { Base(Base&&) {} }; struct Derived:Base {}; struct Owner { Derived value; Owner(Derived&& other): value(static_cast<Derived&&>(other)) {} };","201703L",|p,a|call(p,a,"Base::Base"));
}
#[test]
fn implicit_copy_constructs_nontrivial_member() {
    check("struct Leaf { Leaf(const Leaf&) {} }; struct Aggregate { Leaf leaf; }; struct Owner { Aggregate value; Owner(const Aggregate& other): value(other) {} };","201703L",|p,a|call(p,a,"Leaf::Leaf"));
}
#[test]
fn aggregate_callback_member_keeps_value_flow() {
    check(&format!("{CALLBACK} struct Aggregate {{ Callback cb; }}; struct Owner {{ Aggregate value; Owner(): value{{Handler}} {{ value.cb(); }} }};"),"201703L",|p,a|assert!(has_edge(p,a,"Owner::Owner","Handler",ResolutionKind::Indirect)));
}
#[test]
fn aggregate_callback_array_keeps_value_flow() {
    check(&format!("{CALLBACK} struct Aggregate {{ Callback cb[2]; }}; struct Owner {{ Aggregate value; Owner(): value{{First,Second}} {{ value.cb[0](); value.cb[1](); }} }};"),"201703L",|p,a|for f in ["First","Second"] {assert!(has_edge(p,a,"Owner::Owner",f,ResolutionKind::Indirect));});
}
#[test]
fn unknown_union_copy_survives_merge() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.cpp"),"#include \"unavailable.hpp\"\nunion External; struct Owner { External value; Owner(const External& other): value(other) {} };").unwrap();
    std::fs::write(
        dir.path().join("other.cpp"),
        "union External { int n; External(const External&) {} };",
    )
    .unwrap();
    let p = trace_parse::build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let (_, a) = analyze(&p);
    call(&p, &a, "External::External");
}

#[test]
fn subobjects_and_default_initializers_survive_cached_headers() {
    let root = common::fixture("cpp_constructor_subobjects");
    let p = trace_parse::build_program(&root, &common::default_opts(&root)).unwrap();
    let (_, a) = analyze(&p);
    for (owner, ctor) in [
        ("DefaultOwner", "Leaf"),
        ("OverrideOwner", "Leaf"),
        ("ElidedOwner", "Leaf"),
        ("EmptyOwner", "EmptyLeaf"),
        ("CopyOwner", "CopyLeaf"),
        ("MoveOwner", "MoveBase"),
        ("HiddenOwner", "Derived"),
        ("ArrayOwner", "Leaf"),
    ] {
        assert!(
            has_edge(
                &p,
                &a,
                &format!("cached::{owner}::{owner}"),
                &format!("cached::{ctor}::{ctor}"),
                ResolutionKind::Direct
            ),
            "cached {owner} -> {ctor}"
        );
    }
    assert!(!has_edge(
        &p,
        &a,
        "cached::HiddenOwner::HiddenOwner",
        "cached::Base::Base",
        ResolutionKind::Direct
    ));
    assert!(has_edge(
        &p,
        &a,
        "cached::CallbackOwner::CallbackOwner",
        "cached::Handler",
        ResolutionKind::Indirect
    ));
    for (owner, handler) in [
        ("DefaultOwner", "Handler"),
        ("OverrideOwner", "Other"),
        ("ElidedOwner", "Other"),
        ("ArrayOwner", "Handler"),
        ("ArrayOwner", "Other"),
    ] {
        assert!(
            p.symbols
                .call_sites
                .iter()
                .any(
                    |site| fn_name(&p, site.caller) == format!("cached::{owner}::{owner}")
                        && site.fn_args().iter().any(|&(index, callee)| index == 1
                            && fn_name(&p, callee) == format!("cached::{handler}"))
                ),
            "cached callback {owner} -> {handler}"
        );
    }
    assert!(p
        .symbols
        .call_sites
        .iter()
        .filter(|site| fn_name(&p, site.caller) == "cached::OverrideOwner::OverrideOwner")
        .all(|site| site
            .fn_args()
            .iter()
            .all(|&(_, callee)| fn_name(&p, callee) != "cached::Handler")));
}

callback_case!(
    explicit_class_array_elements,
    "struct Owner { Leaf leaves[2]; Owner(): leaves{Leaf(First),Leaf(Second)} {} };",
    &["First", "Second"]
);

#[test]
fn default_initializer_uses_declaration_scope_and_origin() {
    check("namespace callbacks { void Handler() {} } using callbacks::Handler; using Callback=void(*)(); struct Leaf { Leaf(Callback cb) { cb(); } };\n#define DEFAULT_CALLBACK Handler\nstruct Aggregate { Leaf leaf=DEFAULT_CALLBACK; };\nstruct Owner { Aggregate value; Owner(Callback Handler): value{} {} };", "201703L", |p,a| {
        callbacks(p,a,"Leaf::Leaf", &["callbacks::Handler"]);
        let site=p.symbols.call_sites.iter().find(|site| fn_name(p,site.caller)=="Owner::Owner" && site.callee_name.as_ref()=="Leaf::Leaf").unwrap();
        assert_eq!(site.span.line, 2, "macro-body spelling is the call request position");
        assert_eq!(site.expansion_span.unwrap().line, 3);
    });
}

#[test]
fn cached_header_lowering_respects_each_units_standard() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("types.hpp"), format!("{CALLBACK} struct Derived:Leaf {{}}; struct Owner {{ Derived value; Owner():value(Handler) {{}} }};" )).unwrap();
    for file in ["a.cpp", "b.cpp"] {
        std::fs::write(root.join(file), "#include \"types.hpp\"\n").unwrap();
    }
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!([
            {"directory":root,"file":"a.cpp","arguments":["c++","-std=c++17","-c","a.cpp"]},
            {"directory":root,"file":"b.cpp","arguments":["c++","-std=c++20","-c","b.cpp"]}
        ])
        .to_string(),
    )
    .unwrap();
    for jobs in [1, 8] {
        let p =
            trace_parse::build_program_with_jobs(root, &common::default_opts(root), jobs).unwrap();
        let (_, a) = analyze(&p);
        callbacks(&p, &a, "Leaf::Leaf", &["Handler"]);
    }
}

#[test]
fn constant_expression_array_bounds_preserve_following_member() {
    for bound in ["Count", "1+1", "0x2u", "(Count + 1) - 1"] {
        check(&format!("{CALLBACK} constexpr int Count=2; struct Aggregate {{ int numbers[{bound}]; Leaf leaf; }}; struct Owner {{ Aggregate value; Owner():value{{1,2,Handler}} {{}} }};"), "201703L", |p,a| {
            callbacks(p,a,"Leaf::Leaf", &["Handler"]);
            assert_eq!(p.symbols.call_sites.iter().filter(|s| fn_name(p,s.caller)=="Owner::Owner" && s.callee_name.as_ref()=="Leaf::Leaf").count(),1,"bound {bound}");
        });
    }
}
callback_case!(unknown_array_bound_retains_following_member,
    "struct Aggregate { int numbers[UnavailableCount]; Leaf leaf; }; struct Owner { Aggregate value; Owner():value{1,2,Handler} {} };", &["Handler"]);

callback_case!(inherited_constructor_initializes_derived_member,
    "struct Base { Base(int) {} }; struct Derived:Base { using Base::Base; Leaf leaf=Handler; }; struct Owner { Derived value; Owner():value(1) {} };", &["Handler"]);

#[test]
fn inherited_constructor_reference_signatures_remain_distinct() {
    check("struct Base { Base(const int&) {} }; struct Derived:Base { using Base::Base; Derived(int& x):Base(x) {} }; struct Owner { Derived value; Owner(const int& x):value(x) {} };", "201703L", |p,a| {
        call(p,a,"Base::Base");
        assert!(!has_edge(p,a,"Owner::Owner","Derived::Derived",ResolutionKind::Direct));
    });
    check(&format!("{CALLBACK} struct Base {{ Base(Callback& cb) {{ cb(); }} }}; struct Derived:Base {{ using Base::Base; Derived(Callback&& cb):Base(cb) {{}} }}; struct Owner {{ Derived value; Owner(Callback& cb):value(cb) {{}} }};"), "201703L", |p,a| {
        call(p,a,"Base::Base");
        assert!(!has_edge(p,a,"Owner::Owner","Derived::Derived",ResolutionKind::Direct));
    });
}

#[test]
fn parenthesized_reference_member_binds_without_copying() {
    for declarator in ["(&ref)", "(( &ref ))", "(&&ref)", "(*&ref)"] {
        let argument = if declarator.contains("&&") {
            "static_cast<Value&&>(arg)"
        } else {
            "arg"
        };
        let ty = if declarator.contains('*') {
            "Value*&"
        } else {
            "Value&"
        };
        check(&format!("struct Value {{ Value(const Value&) {{}} }}; struct Owner {{ Value {declarator}; Owner({ty} arg):ref({argument}) {{}} }};"),"201703L",|p,a| {
            assert!(!has_edge(p,a,"Owner::Owner","Value::Value",ResolutionKind::Direct),"{declarator}");
            assert!(p.symbols.call_sites.iter().all(|s|fn_name(p,s.caller)!="Owner::Owner" || s.callee_name.as_ref()!="Value::Value"));
        });
    }
}

#[test]
fn new_constructor_facts_survive_cached_header_imports() {
    let root = common::fixture("cpp_constructor_subobjects");
    let p = trace_parse::build_program(&root, &common::default_opts(&root)).unwrap();
    let (_, a) = analyze(&p);
    for (owner, callee) in [
        ("BoundOwner", "Leaf"),
        ("InheritedOwner", "InheritedBase"),
        ("InheritedOwner", "OtherBase"),
        ("InheritedOwner", "Leaf"),
        ("ConstOwner", "ConstBase"),
    ] {
        assert!(
            has_edge(
                &p,
                &a,
                &format!("cached::{owner}::{owner}"),
                &format!("cached::{callee}::{callee}"),
                ResolutionKind::Direct
            ),
            "{owner} -> {callee}"
        );
    }
    assert!(!has_edge(
        &p,
        &a,
        "cached::ConstOwner::ConstOwner",
        "cached::ConstDerived::ConstDerived",
        ResolutionKind::Direct
    ));
    assert!(!has_edge(
        &p,
        &a,
        "cached::GroupedReferenceOwner::GroupedReferenceOwner",
        "cached::Value::Value",
        ResolutionKind::Direct
    ));
    for name in ["Handler", "Other"] {
        assert!(p
            .symbols
            .call_sites
            .iter()
            .any(
                |site| fn_name(&p, site.caller) == "cached::InheritedOwner::InheritedOwner"
                    && site.fn_args().iter().any(|&(index, callee)| index == 1
                        && fn_name(&p, callee) == format!("cached::{name}"))
            ));
    }
    assert_eq!(
        p.symbols
            .call_sites
            .iter()
            .filter(
                |site| fn_name(&p, site.caller) == "cached::BoundOwner::BoundOwner"
                    && site.callee_name.as_ref() == "cached::Leaf::Leaf"
            )
            .count(),
        1
    );
}

#[test]
fn reference_binding_filter_retains_converting_temporaries() {
    check("struct Base { Base(int&&) {} }; struct Derived:Base { using Base::Base; }; struct Owner { Derived value; Owner(short x):value(x) {} };", "201703L", |p,a|call(p,a,"Base::Base"));
}

#[test]
fn unknown_extent_empty_array_retains_element_construction() {
    check("struct Leaf { Leaf() {} }; struct Owner { Leaf leaves[UnavailableCount]; Owner():leaves{} {} };", "201703L", |p,a|call(p,a,"Leaf::Leaf"));
}

#[test]
fn member_constant_array_bound_uses_class_scope() {
    check(&format!("{CALLBACK} constexpr int Count=1; struct Aggregate {{ static constexpr int Count=2; int numbers[Count]; Leaf leaf; }}; struct Owner {{ Aggregate value; Owner():value{{1,2,Handler}} {{}} }};"),"201703L",|p,a| {
        callbacks(p,a,"Leaf::Leaf", &["Handler"]);
        assert_eq!(p.symbols.call_sites.iter().filter(|s|fn_name(p,s.caller)=="Owner::Owner" && s.callee_name.as_ref()=="Leaf::Leaf").count(),1);
    });
}
