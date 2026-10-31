//! Regressions for declaration recovery and modeled IPC boundaries.
use crate::common::{has_any_edge, must_not_have_edge};
use std::fs;
use trace_analysis::{AnalysisResult, ResolutionKind};
use trace_ir::Program;
use trace_preproc::PreprocessOptions;

fn build(files: &[(&str, &str)]) -> (Program, AnalysisResult) {
    let scratch = tempfile::tempdir().unwrap();
    for (file, text) in files {
        let path = scratch.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let opts = PreprocessOptions::new().with_include(scratch.path().to_path_buf());
    let p = trace_parse::build_program_with_jobs(scratch.path(), &opts, 1).unwrap();
    let (_, a) = crate::common::analyze_with_datashare_summary(&p);
    (p, a)
}

#[test]
fn recovery_keeps_distinct_overload_families_when_a_definition_moves() {
    for local in [true, false] {
        let int_def = "void app::Widget::Run(int) {}";
        let a = format!("#include \"widget.h\"\nusing namespace app;\n{}\nnamespace missing {{ using namespace app; void use(Widget *p, long x) {{ p->Run(x); }} }}", if local { int_def } else { "" });
        let c = format!(
            "#include \"widget.h\"\n{}",
            if local { "" } else { int_def }
        );
        let (p, analysis) = build(&[
            (
                "widget.h",
                "namespace app { class Widget { public: void Run(int); void Run(long); }; }",
            ),
            ("a.cpp", &a),
            (
                "b.cpp",
                "#include \"widget.h\"\nvoid app::Widget::Run(long) {}",
            ),
            ("c.cpp", &c),
        ]);
        let targets: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| {
                p.symbols.function(e.caller).name == "missing::use"
                    && p.symbols.function(e.callee).is_defined
            })
            .collect();
        assert_eq!(targets.len(), 2, "local={local}: {targets:?}");
        assert!(
            analysis
                .call_edges
                .iter()
                .any(|e| p.symbols.function(e.caller).name == "missing::use"
                    && p.symbols.function(e.callee).name == "missing::Widget::Run"
                    && e.resolution == ResolutionKind::External),
            "this case must exercise undeclared-receiver recovery"
        );
        assert!(targets.iter().any(|e| p.symbols.files
            [p.symbols.function(e.callee).file.0 as usize]
            .path
            .ends_with("b.cpp")));
    }
}

#[test]
fn recovery_prefers_local_copies_only_within_the_same_signature() {
    let (p, a) = build(&[
        ("widget.h", "namespace app { class Widget { public: void Run(int); void Run(long); }; }"),
        ("a.cpp", "#include \"widget.h\"\nusing namespace app; void app::Widget::Run(int) {} void app::Widget::Run(long) {} namespace missing { using namespace app; void use(Widget *p, long n) { p->Run(n); } }"),
        ("b.cpp", "#include \"widget.h\"\nvoid app::Widget::Run(long) {}"),
    ]);
    let targets: Vec<_> = a
        .call_edges
        .iter()
        .filter(|e| {
            p.symbols.function(e.caller).name == "missing::use"
                && p.symbols.function(e.callee).is_defined
        })
        .collect();
    assert_eq!(targets.len(), 2);
    assert!(targets.iter().all(
        |e| p.symbols.files[p.symbols.function(e.callee).file.0 as usize]
            .path
            .ends_with("a.cpp")
    ));
}

#[test]
fn recovery_rejects_standard_library_and_unreachable_namespace_namesakes() {
    let (p, a) = build(&[("main.cpp", "#include <mutex>\nnamespace mine { class mutex { public: void lock() {} void unlock() {} }; class Conn { public: void Send(int) {} }; } void f(std::mutex &m) { m.lock(); } void g(std::mutex *m) { m->unlock(); } void h(net::Conn *c) { c->Send(1); } namespace frame { class Conn { public: void Send(int) {} }; void j(net::Conn *c) { c->Send(1); } }")]);
    for (caller, callee) in [
        ("f", "mine::mutex::lock"),
        ("g", "mine::mutex::unlock"),
        ("h", "mine::Conn::Send"),
        ("frame::j", "frame::Conn::Send"),
    ] {
        assert!(p.symbols.functions.iter().any(|f| f.name == caller));
        assert!(
            must_not_have_edge(&p, &a, caller, callee),
            "{caller} -> {callee}"
        );
    }
}

#[test]
fn recovery_completes_partial_namespace_qualifiers() {
    let (p, a) = build(&[("main.cpp", "namespace lib { namespace IPC { class Receiver { public: int Run(){return 1;} }; }} using namespace lib; namespace app { int use(IPC::Receiver *p) { return p->Run(); } }")]);
    assert!(has_any_edge(&p, &a, "app::use", "lib::IPC::Receiver::Run"));
}

#[test]
fn undeclared_using_imports_keep_global_and_known_relative_qualifiers() {
    let (p, a) = build(&[(
        "main.cpp",
        r#"
#include <string>
#include <ext/remote.h>
namespace a {
namespace ext {}
namespace b {
using std::string;
using ::ext::Remote;
using ext::LocalRemote;
using remote::Unknown;
int f(string &s) { return s.empty(); }
int g(Remote *r) { return r->Ping(); }
int local(LocalRemote *r) { return r->Ping(); }
int unknown(Unknown *r) { return r->Run(); }
}}
"#,
    )]);
    for (caller, callee) in [
        ("a::b::f", "std::string::empty"),
        ("a::b::g", "ext::Remote::Ping"),
        ("a::b::local", "a::ext::LocalRemote::Ping"),
        ("a::b::unknown", "remote::Unknown::Run"),
    ] {
        assert!(
            a.call_edges.iter().any(|e| {
                p.symbols.function(e.caller).name == caller
                    && p.symbols.function(e.callee).name == callee
                    && e.resolution == ResolutionKind::External
            }),
            "missing external {caller} -> {callee}"
        );
    }
    assert!(!p
        .symbols
        .functions
        .iter()
        .any(|f| f.name.contains("::std::")));
}

#[test]
fn recovery_filters_explicit_argument_counts_including_defaults_and_variadics() {
    let (p, a) = build(&[("main.cpp", "namespace app { class Reader { public: void Read(int*) {} void Read(int*, int*) {} void Defaults(int, int = 0) {} void Variadic(int, ...) {} }; } void one(Reader *p, int *v) { p->Read(v); } void two(Reader *p, int *v) { p->Read(v,v); } void defaults(Reader *p) { p->Defaults(1); } void variadic(Reader *p) { p->Variadic(1,2,3); }")]);
    for (caller, count) in [("one", 1), ("two", 2)] {
        let targets: Vec<_> = a
            .call_edges
            .iter()
            .filter(|e| {
                p.symbols.function(e.caller).name == caller
                    && p.symbols.function(e.callee).is_defined
            })
            .collect();
        assert_eq!(targets.len(), 1, "{caller}: {targets:?}");
        assert_eq!(
            p.symbols.function(targets[0].callee).explicit_arity,
            Some(count)
        );
    }
    assert!(has_any_edge(&p, &a, "defaults", "app::Reader::Defaults"));
    assert!(has_any_edge(&p, &a, "variadic", "app::Reader::Variadic"));
}

#[test]
fn synthesized_summary_sender_is_independent_of_directory_order() {
    let mut previous = None;
    for prefix in ["aa", "zz"] {
        let path = format!("{prefix}/tests/mock.cpp");
        let (p, a) = build(&[
            ("src/main.cpp", "namespace OHOS { namespace DataShare { int prod_use(DataShareHelper *h) { return h->OpenFile(1); } }}"),
            (&path, "namespace OHOS { namespace DataShare { class DataShareStub { public: virtual int OpenFile(int) = 0; }; class MockProvider : public DataShareStub { public: int OpenFile(int x) override { return x; } }; int test_use(DataShareHelper *h) { return h->OpenFile(2); } }}"),
        ]);
        assert!(!a
            .call_edges
            .iter()
            .any(|e| e.resolution == ResolutionKind::IpcBridge));
        assert!(
            !a.execution_contexts
                .iter()
                .any(|c| p.symbols.function(c.entry).name
                    == "OHOS::DataShare::MockProvider::OpenFile")
        );
        let mut projection: Vec<_> = a
            .call_edges
            .iter()
            .map(|e| {
                (
                    p.symbols.function(e.caller).name.clone(),
                    p.symbols.function(e.callee).name.clone(),
                    format!("{:?}", e.resolution),
                )
            })
            .collect();
        projection.sort();
        if let Some(old) = previous {
            assert_eq!(old, projection);
        }
        previous = Some(projection);
    }
}

#[test]
fn virtual_summary_defers_to_indexed_implementation_but_retains_missing_framework_fallback() {
    for implementation in [false, true] {
        let source = format!("namespace OHOS {{ namespace DataShare {{ class DataShareHelper {{ public: virtual int OpenFile(int) = 0; }}; {} class DataShareStub {{ public: virtual int OpenFile(int) = 0; }}; class Provider : public DataShareStub {{ public: int OpenFile(int x) override {{ return x; }} }}; int use(DataShareHelper *h) {{ return h->OpenFile(1); }} }}}}", if implementation { "class DataShareHelperImpl : public DataShareHelper { public: int OpenFile(int x) override { return x+1; } };" } else { "" });
        let (p, a) = build(&[("main.cpp", &source)]);
        assert_eq!(
            a.call_edges
                .iter()
                .any(|e| e.resolution == ResolutionKind::IpcBridge),
            !implementation
        );
        assert_eq!(
            a.execution_contexts
                .iter()
                .any(|context| p.symbols.function(context.entry).name
                    == "OHOS::DataShare::Provider::OpenFile"
                    && context.kind == trace_analysis::ContextKind::IpcHandler),
            !implementation
        );
        if implementation {
            assert!(has_any_edge(
                &p,
                &a,
                "OHOS::DataShare::use",
                "OHOS::DataShare::DataShareHelperImpl::OpenFile"
            ));
        }
    }
}

#[test]
fn dependency_declared_wrapper_keeps_its_pointee() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("project");
    let sdk = scratch.path().join("sdk");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&sdk).unwrap();
    fs::write(sdk.join("refbase.h"), "namespace OHOS { template<class T> class sptr { T *p; public: sptr(T *q):p(q){} T *operator->() const { return p; } T *GetRefPtr() const { return p; } }; }").unwrap();
    fs::write(root.join("main.cpp"), "#include <refbase.h>\nnamespace OHOS { class Item { public: int Run(){return 1;} }; int use(Item *p){ auto q=sptr<Item>(p); return q->Run(); } int chain(Item *p){return sptr<Item>(p)->Run();} }").unwrap();
    let opts = PreprocessOptions::new()
        .with_include(sdk.clone())
        .with_dep(&sdk);
    let p = trace_parse::build_program(&root, &opts).unwrap();
    let (_, a) = crate::common::analyze_with_datashare_summary(&p);
    for caller in ["OHOS::use", "OHOS::chain"] {
        assert!(has_any_edge(&p, &a, caller, "OHOS::Item::Run"), "{caller}");
    }
}

#[test]
fn ipc_summary_final_metadata_does_not_cross_provider_images() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    let sources = [
        ("client", "namespace OHOS { namespace DataShare { class DataShareHelper { public: int OpenFile(int); }; int use(DataShareHelper *h) { return h->OpenFile(1); } }}"),
        ("final_server", "namespace OHOS { namespace DataShare { class DataShareStub { public: virtual int OpenFile(int x) final { return x; } }; }}"),
        ("provider_server", "namespace OHOS { namespace DataShare { class DataShareStub { public: virtual int OpenFile(int) = 0; }; class Provider : public DataShareStub { public: int OpenFile(int x) override { return x; } }; }}"),
    ];
    let mut compile = Vec::new();
    let mut link = Vec::new();
    for (name, source) in sources {
        fs::write(root.join(format!("{name}.cpp")), source).unwrap();
        compile.push(serde_json::json!({"directory":root,"file":format!("{name}.cpp"),"output":format!("{name}.o"),"arguments":["c++","-c",format!("{name}.cpp"),"-o",format!("{name}.o")]}));
        link.push(serde_json::json!({"directory":root,"output":name,"arguments":["c++",format!("{name}.o"),"-o",name]}));
    }
    fs::write(
        root.join("compile_commands.json"),
        serde_json::to_string(&compile).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("link_commands.json"),
        serde_json::to_string(&link).unwrap(),
    )
    .unwrap();
    let mut previous = None;
    for jobs in [1, 4] {
        let p =
            trace_parse::build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
        let (_, a) = crate::common::analyze_with_datashare_summary(&p);
        let mut bridges: Vec<_> = a
            .call_edges
            .iter()
            .filter(|e| e.resolution == ResolutionKind::IpcBridge)
            .map(|e| {
                let caller = p.symbols.function(e.caller);
                let callee = p.symbols.function(e.callee);
                assert_ne!(caller.target, callee.target);
                assert_eq!(e.call_site, trace_analysis::SYNTHETIC_CALL_SITE);
                callee.name.clone()
            })
            .collect();
        bridges.sort();
        assert_eq!(
            bridges,
            [
                "OHOS::DataShare::DataShareStub::OpenFile",
                "OHOS::DataShare::Provider::OpenFile"
            ]
        );
        if let Some(old) = previous {
            assert_eq!(old, bridges);
        }
        previous = Some(bridges);
    }
}

#[test]
fn nested_constructor_recovery_uses_its_own_argument_count() {
    let (p, a) = build(&[
        (
            "a.cpp",
            "using namespace app; void use() { auto p = sptr<Thing>(new Thing(1, 2)); }",
        ),
        (
            "b.cpp",
            "namespace app { class Thing { public: Thing(int) {} Thing(int,int) {} }; }",
        ),
    ]);
    let constructors: Vec<_> = a
        .call_edges
        .iter()
        .filter(|e| {
            p.symbols.function(e.caller).name == "use"
                && p.symbols.function(e.callee).name == "app::Thing::Thing"
        })
        .collect();
    assert_eq!(constructors.len(), 1);
    assert_eq!(
        p.symbols.function(constructors[0].callee).explicit_arity,
        Some(2)
    );
}

#[test]
fn sdk_photo_asset_proxy_never_becomes_the_camera_wrapper_or_closes_a_cycle() {
    let (p, a) = build(&[("main.cpp", "#include \"media_photo_asset_proxy.h\"\nnamespace OHOS { namespace CameraStandard { class PhotoAssetIntf { public: virtual void AddPhotoProxy(int) = 0; }; class PhotoAssetProxy { public: void AddPhotoProxy(int x) { intf_->AddPhotoProxy(x); } PhotoAssetIntf *intf_; }; class PhotoAssetAdapter : public PhotoAssetIntf { public: void AddPhotoProxy(int x) override { proxy_->AddPhotoProxy(x); } Media::PhotoAssetProxy *proxy_; }; }}")]);
    let adapter = "OHOS::CameraStandard::PhotoAssetAdapter::AddPhotoProxy";
    let wrapper = "OHOS::CameraStandard::PhotoAssetProxy::AddPhotoProxy";
    assert!(
        has_any_edge(&p, &a, wrapper, adapter),
        "the legitimate wrapper-to-adapter edge is present"
    );
    assert!(
        must_not_have_edge(&p, &a, adapter, wrapper),
        "the SDK receiver must not close a false adapter/wrapper cycle"
    );
}
