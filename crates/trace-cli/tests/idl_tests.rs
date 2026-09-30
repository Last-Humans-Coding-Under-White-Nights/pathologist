//! OpenHarmony `.idl` synthesis (#123; docs/ANALYSIS.md, "IDL-generated interfaces").

mod common;

use common::{fixture, fn_name, has_any_edge, has_edge};
use std::path::Path;
use trace_analysis::{analyze, analyze_with_options, AnalyzeOptions, ResolutionKind};
use trace_ir::Program;
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

fn build(root: &Path, opts: PreprocessOptions) -> Program {
    build_program_with_jobs(root, &opts, 1).expect("build program")
}

fn missing_includes(program: &Program) -> Vec<String> {
    program
        .diagnostics
        .iter()
        .filter(|d| d.message.starts_with("include file not found"))
        .map(|d| d.message.clone())
        .collect()
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

const IATM_IDL: &str = "interface OHOS.Security.IAtm {\n  [ipccode 1] void VerifyAccessToken([in] unsigned int tokenID, [out] int state);\n}\n";
/// A unit that includes the interface header and uses nothing from it.
const MAIN_INCLUDING_IATM: &str = "#include \"iatm.h\"\nint main() { return 0; }\n";
/// `OHOS::Security::Client::V` calls `VerifyAccessToken` through `sptr<IAtm>`.
const CLIENT_CPP: &str = "#include \"iatm.h\"\nnamespace OHOS { namespace Security {\nstruct Client { sptr<IAtm> p; int V(unsigned i) { int s; return p->VerifyAccessToken(i, s); } };\n} }\n";

fn declares(program: &Program, function: &str) -> bool {
    program.symbols.functions.iter().any(|f| f.name == function)
}

fn idl_warnings(program: &Program) -> Vec<&str> {
    program
        .diagnostics
        .iter()
        .filter(|d| d.stage == "idl")
        .map(|d| d.message.as_str())
        .collect()
}

fn descriptors(program: &Program) -> Vec<&str> {
    program
        .idl_interfaces
        .iter()
        .map(|i| i.descriptor.as_str())
        .collect()
}

/// A copy of the `idl_basic` fixture, to add files to.
fn idl_basic_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture("idl_basic");
    for rel in [
        "idl/IAtm.idl",
        "service/atm_service.cpp",
        "client/client.cpp",
    ] {
        write(
            dir.path(),
            rel,
            &std::fs::read_to_string(src.join(rel)).unwrap(),
        );
    }
    dir
}

/// `p.IAtm` and `p.Atm` render different interface headers (`iatm.h`,
/// `atm.h`) but the same `atm_proxy.h` / `atm_stub.h`, so both name
/// `p::AtmProxy`. `a/IAtm.idl` sorts first and keeps them.
fn colliding_pair_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a/IAtm.idl",
        "interface p.IAtm { void First(); }\n",
    );
    write(
        dir.path(),
        "b/Atm.idl",
        "interface p.Atm { void Second(); }\n",
    );
    write(
        dir.path(),
        "main.cpp",
        "#include \"atm_proxy.h\"\nint main() { return 0; }\n",
    );
    dir
}

#[test]
fn idl_headers_resolve_as_dependency_declarations() {
    let program = build(&fixture("idl_basic"), PreprocessOptions::new());
    assert_eq!(missing_includes(&program), Vec::<String>::new());

    let proxy = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "OHOS::Security::AtmProxy::VerifyAccessToken")
        .expect("synthesized proxy method is declared");
    assert!(!proxy.is_defined);
    assert!(program.is_dep_file(proxy.file));
    let path = &program.symbols.files[proxy.file.0 as usize].path;
    assert!(
        path.ends_with(".trace-idl-generated/atm_proxy.h"),
        "{}",
        path.display()
    );

    assert_eq!(
        program.idl_interfaces,
        vec![trace_ir::IdlInterface {
            descriptor: "OHOS.Security.IAtm".into(),
            interface: "OHOS::Security::IAtm".into(),
            proxy: "OHOS::Security::AtmProxy".into(),
            stub: "OHOS::Security::AtmStub".into(),
            methods: vec![trace_ir::IdlMethod {
                name: "VerifyAccessToken".into(),
                ipccode: Some(1)
            }],
        }]
    );
}

#[test]
fn idl_client_call_reaches_proxy() {
    let program = build(&fixture("idl_basic"), PreprocessOptions::new());
    let (_, analysis) = analyze(&program);
    assert!(
        has_any_edge(
            &program,
            &analysis,
            "OHOS::Security::Client::Verify",
            "OHOS::Security::AtmProxy::VerifyAccessToken"
        ),
        "edges from Client::Verify: {:?}",
        common::callees_of(&program, &analysis, "OHOS::Security::Client::Verify"),
    );
}

#[test]
fn idl_on_disk_header_wins() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "idl/IAtm.idl", IATM_IDL);
    write(dir.path(), "include/iatm.h", "namespace OHOS { namespace Security { class IAtm { public: virtual int VerifyAccessToken(unsigned, int&) = 0; }; } }\n");
    write(dir.path(), "client.cpp", MAIN_INCLUDING_IATM);
    let program = build(dir.path(), PreprocessOptions::new());
    assert!(program
        .symbols
        .files
        .iter()
        .all(|f| !f.path.ends_with(".trace-idl-generated/iatm.h")));
    // The proxy and stub headers do not collide and are still synthesized.
    assert_eq!(program.idl_interfaces.len(), 1);
    assert_eq!(missing_includes(&program), Vec::<String>::new());
}

#[test]
fn idl_parse_error_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a/IBroken.idl",
        "interface a.IBroken {\n  void Ping(\n}\n",
    );
    write(dir.path(), "b/IAtm.idl", IATM_IDL);
    write(dir.path(), "main.cpp", MAIN_INCLUDING_IATM);
    let program = build(dir.path(), PreprocessOptions::new());
    let idl = idl_warnings(&program);
    assert_eq!(idl.len(), 1, "{idl:?}");
    assert!(idl[0].contains("IBroken.idl:3:"), "{}", idl[0]);
    assert_eq!(program.idl_interfaces.len(), 1);
}

#[test]
fn idl_header_collision_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a/IAtm.idl",
        "interface p.IAtm { void First(); }\n",
    );
    write(
        dir.path(),
        "b/IAtm.idl",
        "interface q.IAtm { void Second(); }\n",
    );
    write(dir.path(), "main.cpp", MAIN_INCLUDING_IATM);
    let names = |p: &Program| -> Vec<String> {
        let mut v: Vec<String> = p.symbols.functions.iter().map(|f| f.name.clone()).collect();
        v.sort();
        v
    };
    let first = build(dir.path(), PreprocessOptions::new());
    let second = build(dir.path(), PreprocessOptions::new());
    assert_eq!(names(&first), names(&second));
    assert!(names(&first).contains(&"p::IAtm::First".to_string()));
    assert!(!names(&first).contains(&"q::IAtm::Second".to_string()));
    assert!(idl_warnings(&first)
        .iter()
        .any(|m| m.contains("a/IAtm.idl") && m.contains("b/IAtm.idl")));
    // `q.IAtm` lost its proxy and stub headers, so it records no fact.
    assert_eq!(descriptors(&first), vec!["p.IAtm"]);
}

#[test]
fn idl_interface_collision_keeps_owner_fact() {
    let dir = colliding_pair_tree();
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(descriptors(&program), vec!["p.IAtm"]);
    assert!(declares(&program, "p::AtmProxy::First"));
    assert!(!declares(&program, "p::AtmProxy::Second"));
    assert!(idl_warnings(&program)
        .iter()
        .any(|m| m.contains("atm_proxy.h") && m.contains("a/IAtm.idl") && m.contains("b/Atm.idl")));
}

#[test]
fn idl_headers_resolve_under_compile_commands() {
    let dir = idl_basic_copy();
    // Entries for both TUs with no -I for the generated directory.
    common::write_compile_commands(
        dir.path(),
        &["service/atm_service.cpp", "client/client.cpp"],
    );
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(missing_includes(&program), Vec::<String>::new());
    // No TU includes `atm_proxy.h`; the configured path must still index it.
    let proxy = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "OHOS::Security::AtmProxy::VerifyAccessToken")
        .expect("unreferenced synthesized proxy header is indexed");
    assert!(!proxy.is_defined);
    assert!(program.is_dep_file(proxy.file));
}

/// `idl_basic` plus an unrelated C unit: an unincluded `.h` defaults to the C
/// grammar once the tree has a C unit, which synthesized headers must not.
fn mixed_idl_tree(with_compile_commands: bool) -> tempfile::TempDir {
    let dir = idl_basic_copy();
    write(dir.path(), "helper.c", "int helper(void) { return 0; }\n");
    if with_compile_commands {
        // `helper.c` has to be a C unit, so its driver is `cc`.
        common::write_compile_commands_with(
            dir.path(),
            &[
                ("c++", "service/atm_service.cpp"),
                ("c++", "client/client.cpp"),
                ("cc", "helper.c"),
            ],
        );
    }
    dir
}

#[test]
fn idl_under_dep_root_is_synthesized() {
    let dir = tempfile::tempdir().unwrap();
    let dep = dir.path().join("dep");
    write(&dep, "idl/IAtm.idl", IATM_IDL);
    let target = dir.path().join("target");
    write(&target, "client.cpp", CLIENT_CPP);
    let dep = trace_ir::canonicalize(&dep);
    let program = build(&target, PreprocessOptions::new().with_dep(dep));
    assert_eq!(missing_includes(&program), Vec::<String>::new());
    assert_eq!(program.idl_interfaces.len(), 1);
}

#[test]
fn tree_without_idl_has_no_generated_root() {
    let program = build(&fixture("ipc_basic"), PreprocessOptions::new());
    assert!(program.idl_interfaces.is_empty());
    assert!(program
        .dep_roots()
        .iter()
        .all(|d| !d.ends_with(".trace-idl-generated")));
}

#[test]
fn idl_proxy_bridges_to_service() {
    let program = build(&fixture("idl_basic"), PreprocessOptions::new());
    let (pag, analysis) = analyze(&program);
    assert!(
        has_edge(
            &program,
            &analysis,
            "OHOS::Security::AtmProxy::VerifyAccessToken",
            "OHOS::Security::AtmService::VerifyAccessToken",
            ResolutionKind::IpcBridge
        ),
        "{:?}",
        common::callees_of(
            &program,
            &analysis,
            "OHOS::Security::AtmProxy::VerifyAccessToken"
        )
    );
    assert!(!analysis
        .call_edges
        .iter()
        .any(|e| e.resolution == ResolutionKind::IpcBridge
            && fn_name(&program, e.callee) == "OHOS::Security::AtmService::LocalOnly"));
    assert!(pag
        .ipc_bridges
        .iter()
        .all(|b| b.descriptor == "OHOS.Security.IAtm"));
    assert!(!pag.ipc_bridges.is_empty());
}

#[test]
fn idl_no_ipc_keeps_declarations_drops_bridges() {
    let program = build(&fixture("idl_basic"), PreprocessOptions::new());
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            enable_ipc: false,
            ..Default::default()
        },
    );
    assert!(pag.ipc_bridges.is_empty());
    assert!(analysis
        .call_edges
        .iter()
        .all(|e| e.resolution != ResolutionKind::IpcBridge));
    assert!(has_any_edge(
        &program,
        &analysis,
        "OHOS::Security::Client::Verify",
        "OHOS::Security::AtmProxy::VerifyAccessToken"
    ));
}

#[test]
fn idl_client_only_tree_reaches_proxy() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "idl/IAtm.idl", IATM_IDL);
    write(dir.path(), "client.cpp", CLIENT_CPP);
    let program = build(dir.path(), PreprocessOptions::new());
    let (_, analysis) = analyze(&program);
    assert!(has_any_edge(
        &program,
        &analysis,
        "OHOS::Security::Client::V",
        "OHOS::Security::AtmProxy::VerifyAccessToken"
    ));
    // No service: the bridge falls back to the bodyless interface method (a leaf).
    assert!(has_edge(
        &program,
        &analysis,
        "OHOS::Security::AtmProxy::VerifyAccessToken",
        "OHOS::Security::IAtm::VerifyAccessToken",
        ResolutionKind::IpcBridge
    ));
}

#[test]
fn idl_collision_bridge_uses_owner_descriptor() {
    // `p.Atm` loses `atm_proxy.h` / `atm_stub.h`; its descriptor must not
    // label the surviving `p::AtmProxy` pair.
    let dir = colliding_pair_tree();
    let program = build(dir.path(), PreprocessOptions::new());
    let (pag, _) = analyze(&program);
    assert!(!pag.ipc_bridges.is_empty());
    assert!(
        pag.ipc_bridges.iter().all(|b| b.descriptor == "p.IAtm"),
        "{:?}",
        pag.ipc_bridges
            .iter()
            .map(|b| &b.descriptor)
            .collect::<Vec<_>>()
    );
}

#[test]
fn idl_headers_are_cpp_in_mixed_trees_and_bridge() {
    // With and without a compilation database.
    for with_compile_commands in [false, true] {
        let dir = mixed_idl_tree(with_compile_commands);
        let program = build(dir.path(), PreprocessOptions::new());
        assert!(
            declares(&program, "OHOS::Security::AtmProxy::VerifyAccessToken"),
            "compile_commands={with_compile_commands}"
        );
        let (_, analysis) = analyze(&program);
        assert!(
            has_any_edge(
                &program,
                &analysis,
                "OHOS::Security::Client::Verify",
                "OHOS::Security::AtmProxy::VerifyAccessToken"
            ),
            "compile_commands={with_compile_commands}"
        );
        assert!(
            has_edge(
                &program,
                &analysis,
                "OHOS::Security::AtmProxy::VerifyAccessToken",
                "OHOS::Security::AtmService::VerifyAccessToken",
                ResolutionKind::IpcBridge
            ),
            "compile_commands={with_compile_commands}: {:?}",
            common::callees_of(
                &program,
                &analysis,
                "OHOS::Security::AtmProxy::VerifyAccessToken"
            )
        );
    }
}

#[test]
fn idl_cli_exports_client_and_ipc_edges() {
    let db = common::TempDb::new("idl.db");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .args(["analyze"])
        .arg(fixture("idl_basic"))
        .arg("-o")
        .arg(db.path())
        .status()
        .unwrap();
    assert!(status.success());
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let edge = |caller: &str, callee: &str| -> Vec<String> {
        let mut s = conn
            .prepare(
                "SELECT e.resolution FROM call_edges e JOIN functions c ON c.id = e.caller_fn_id \
             JOIN functions d ON d.id = e.callee_fn_id WHERE c.name = ?1 AND d.name = ?2",
            )
            .unwrap();
        s.query_map([caller, callee], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    // The proxy is declared in a dependency header and has no body.
    assert_eq!(
        edge(
            "OHOS::Security::Client::Verify",
            "OHOS::Security::AtmProxy::VerifyAccessToken"
        ),
        vec!["external".to_string()]
    );
    assert_eq!(
        edge(
            "OHOS::Security::AtmProxy::VerifyAccessToken",
            "OHOS::Security::AtmService::VerifyAccessToken"
        ),
        vec!["ipc".to_string()]
    );
    let dep: i64 = conn
        .query_row(
            "SELECT is_dep FROM files WHERE path LIKE '%/.trace-idl-generated/atm_proxy.h'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dep, 1);
}

#[test]
fn idl_test_partition_header_does_not_suppress_synthesis() {
    // A mock `iatm.h` is not a header a production includer can take
    // (docs/ANALYSIS.md, "Declaring-header eligibility"), so it does not
    // stand in for the generated one.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "idl/IAtm.idl", IATM_IDL);
    write(dir.path(), "test/mock/iatm.h", "namespace OHOS { namespace Security { class IAtm { public: virtual int Mocked() = 0; }; } }\n");
    write(dir.path(), "src/client.cpp", CLIENT_CPP);
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(missing_includes(&program), Vec::<String>::new());
    assert!(program
        .symbols
        .files
        .iter()
        .any(|f| f.path.ends_with(".trace-idl-generated/iatm.h")));
    let (_, analysis) = analyze(&program);
    assert!(has_any_edge(
        &program,
        &analysis,
        "OHOS::Security::Client::V",
        "OHOS::Security::AtmProxy::VerifyAccessToken"
    ));
}

#[test]
fn idl_extends_without_a_base_header_includes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "idl/IFoo.idl",
        "package a;\ninterface IFoo extends IBase { void A(); }\n",
    );
    write(
        dir.path(),
        "main.cpp",
        "#include \"ifoo.h\"\nint main() { return 0; }\n",
    );
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(missing_includes(&program), Vec::<String>::new());
    assert!(program.derives_from("a::IFoo", "a::IBase"));
}

#[test]
fn idl_extends_reaches_the_base_interface() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "idl/IBase.idl",
        "package a;\ninterface IBase { void Ping(); }\n",
    );
    write(
        dir.path(),
        "idl/IFoo.idl",
        "package a;\nimport IBase;\ninterface IFoo extends IBase { void A(); }\n",
    );
    write(dir.path(), "client.cpp", "#include \"ifoo.h\"\nnamespace a {\nstruct Client { sptr<IFoo> p; int V() { return p->Ping(); } };\n}\n");
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(missing_includes(&program), Vec::<String>::new());
    let (_, analysis) = analyze(&program);
    assert!(
        has_any_edge(&program, &analysis, "a::Client::V", "a::FooProxy::Ping"),
        "{:?}",
        common::callees_of(&program, &analysis, "a::Client::V")
    );
}

#[test]
fn idl_client_reaches_the_brokers_members() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "idl/IAtm.idl", IATM_IDL);
    write(dir.path(), "ipc/iremote_broker.h", "namespace OHOS {\nclass IRemoteObject;\nclass IRemoteBroker { public: virtual IRemoteObject* AsObject() = 0; };\n}\n");
    write(dir.path(), "client.cpp", "#include \"iremote_broker.h\"\n#include \"iatm.h\"\nnamespace OHOS { namespace Security {\nstruct Client { sptr<IAtm> p; void* V() { return p->AsObject(); } };\n} }\n");
    let program = build(dir.path(), PreprocessOptions::new());
    let (_, analysis) = analyze(&program);
    assert!(
        has_any_edge(
            &program,
            &analysis,
            "OHOS::Security::Client::V",
            "OHOS::IRemoteBroker::AsObject"
        ),
        "{:?}",
        common::callees_of(&program, &analysis, "OHOS::Security::Client::V")
    );
}

#[test]
fn idl_production_interface_wins_over_a_test_one() {
    // `mock/IAtm.idl` sorts before `src/idl/IAtm.idl`.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "mock/IAtm.idl",
        "interface p.IAtm { void Mocked(); }\n",
    );
    write(
        dir.path(),
        "src/idl/IAtm.idl",
        "interface p.IAtm { void Real(); }\n",
    );
    write(
        dir.path(),
        "src/main.cpp",
        "#include \"atm_proxy.h\"\nint main() { return 0; }\n",
    );
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(missing_includes(&program), Vec::<String>::new());
    assert_eq!(program.idl_interfaces.len(), 1);
    assert_eq!(program.idl_interfaces[0].methods[0].name, "Real");
    let paths: Vec<String> = program
        .symbols
        .files
        .iter()
        .filter(|f| f.path.ends_with("atm_proxy.h"))
        .map(|f| {
            let p = f.path.to_string_lossy();
            p[p.find(".trace-idl-generated").unwrap()..].to_string()
        })
        .collect();
    assert!(
        paths.contains(&".trace-idl-generated/atm_proxy.h".to_string())
            && paths.contains(&".trace-idl-generated/mock/atm_proxy.h".to_string()),
        "{paths:?}"
    );
    // Both are different headers: no collision to report.
    assert!(program.diagnostics.iter().all(|d| d.stage != "idl"));
}

#[test]
fn idl_header_in_an_include_directory_wins() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let sdk = trace_ir::canonicalize(dir.path()).join("sdk");
    write(&root, "idl/IAtm.idl", IATM_IDL);
    write(&sdk, "iatm.h", "namespace OHOS { namespace Security { class IAtm { public: virtual int VerifyAccessToken(unsigned, int&) = 0; }; } }\n");
    write(&root, "client.cpp", MAIN_INCLUDING_IATM);
    let program = build(&root, PreprocessOptions::new().with_include(sdk));
    assert!(program
        .symbols
        .files
        .iter()
        .all(|f| !f.path.ends_with(".trace-idl-generated/iatm.h")));
    assert_eq!(missing_includes(&program), Vec::<String>::new());
}

#[test]
fn idl_generated_directory_on_disk_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "idl/IAtm.idl", IATM_IDL);
    write(dir.path(), ".trace-idl-generated/notes.h", "int notes;\n");
    write(dir.path(), "main.cpp", MAIN_INCLUDING_IATM);
    let program = build(dir.path(), PreprocessOptions::new());
    let idl = idl_warnings(&program);
    assert_eq!(idl.len(), 1, "{idl:?}");
    assert!(
        idl[0].contains(".trace-idl-generated") && idl[0].contains("on disk"),
        "{}",
        idl[0]
    );
}

#[test]
fn idl_headers_under_a_forced_c_language_do_not_fail_the_run() {
    let opts = PreprocessOptions::new().with_language(trace_preproc::Language::C);
    let program = build(&fixture("idl_basic"), opts);
    assert_eq!(program.idl_interfaces.len(), 1);
    assert_eq!(missing_includes(&program), Vec::<String>::new());
}

#[test]
fn idl_that_does_not_parse_changes_nothing_but_its_warning() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture("ipc_basic");
    write(
        dir.path(),
        "main.cpp",
        &std::fs::read_to_string(src.join("main.cpp")).unwrap(),
    );
    let facts = |p: &Program| {
        let (_, analysis) = analyze(p);
        let mut functions: Vec<(String, bool)> = p
            .symbols
            .functions
            .iter()
            .map(|f| (f.name.clone(), f.is_defined))
            .collect();
        functions.sort();
        let mut edges: Vec<(String, String, String)> = analysis
            .call_edges
            .iter()
            .map(|e| {
                (
                    fn_name(p, e.caller),
                    fn_name(p, e.callee),
                    format!("{:?}", e.resolution),
                )
            })
            .collect();
        edges.sort();
        (functions, edges, p.symbols.files.len(), p.dep_roots().len())
    };
    let plain = build(dir.path(), PreprocessOptions::new());
    write(dir.path(), "idl/IBroken.idl", "interface a.IBroken {\n");
    let with_idl = build(dir.path(), PreprocessOptions::new());
    assert_eq!(facts(&plain), facts(&with_idl));
    assert_eq!(with_idl.diagnostics.len(), plain.diagnostics.len() + 1);
}

/// A tree with a production unit and a test unit that both include a header
/// only an IDL file in the test partition generates.
fn test_only_idl_tree(with_compile_commands: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "test/idl/ITestOnly.idl",
        "interface t.ITestOnly { void A(); }\n",
    );
    write(
        dir.path(),
        "src/prod.cpp",
        "#include \"itest_only.h\"\nnamespace t {\nstruct Prod { sptr<ITestOnly> p; int V() { return p->A(); } };\n}\n",
    );
    write(
        dir.path(),
        "test/unit.cpp",
        "#include \"itest_only.h\"\nnamespace t {\nstruct Unit { sptr<ITestOnly> p; int V() { return p->A(); } };\n}\n",
    );
    if with_compile_commands {
        common::write_compile_commands(dir.path(), &["src/prod.cpp", "test/unit.cpp"]);
    }
    dir
}

#[test]
fn idl_in_the_test_partition_stays_in_it() {
    // With and without a compilation database.
    for with_compile_commands in [false, true] {
        let dir = test_only_idl_tree(with_compile_commands);
        let program = build(dir.path(), PreprocessOptions::new());
        let declared = program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "t::ITestOnly::A")
            .expect("the test unit sees the interface");
        let path = &program.symbols.files[declared.file.0 as usize].path;
        assert!(
            path.ends_with(".trace-idl-generated/test/itest_only.h"),
            "{}",
            path.display()
        );
        let (_, analysis) = analyze(&program);
        assert!(
            has_any_edge(&program, &analysis, "t::Unit::V", "t::TestOnlyProxy::A"),
            "compile_commands={with_compile_commands}: {:?}",
            common::callees_of(&program, &analysis, "t::Unit::V")
        );
        // The production unit cannot take a header of the test partition.
        assert!(
            !has_any_edge(&program, &analysis, "t::Prod::V", "t::TestOnlyProxy::A"),
            "compile_commands={with_compile_commands}: {:?}",
            common::callees_of(&program, &analysis, "t::Prod::V")
        );
        let missing = missing_includes(&program);
        assert!(!missing.is_empty());
        assert!(
            missing.iter().all(|m| m.ends_with("itest_only.h")),
            "{missing:?}"
        );
    }
}

#[test]
fn idl_production_proxy_inherits_nothing_from_the_test_partition() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "test/IBase.idl",
        "package a;\ninterface IBase { void TestOnly(); }\n",
    );
    write(
        dir.path(),
        "src/IFoo.idl",
        "package a;\ninterface IFoo extends IBase { void A(); }\n",
    );
    write(
        dir.path(),
        "src/main.cpp",
        "#include \"foo_proxy.h\"\nint main() { return 0; }\n",
    );
    let program = build(dir.path(), PreprocessOptions::new());
    let foo = program
        .idl_interfaces
        .iter()
        .find(|i| i.descriptor == "a.IFoo")
        .expect("IFoo is recorded");
    let methods: Vec<&str> = foo.methods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(methods, vec!["A"]);
    assert!(program
        .symbols
        .functions
        .iter()
        .all(|f| f.name != "a::FooProxy::TestOnly"));
    assert_eq!(missing_includes(&program), Vec::<String>::new());
}

#[test]
fn idl_interface_token_is_the_bridge_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "idl/IAtm.idl",
        "interface_token ohos.security.atm;\ninterface OHOS.Security.IAtm { void A(); }\n",
    );
    write(
        dir.path(),
        "main.cpp",
        "#include \"atm_proxy.h\"\nint main() { return 0; }\n",
    );
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(program.idl_interfaces.len(), 1);
    assert_eq!(program.idl_interfaces[0].descriptor, "ohos.security.atm");
    let (pag, _) = analyze(&program);
    assert!(!pag.ipc_bridges.is_empty());
    assert!(pag
        .ipc_bridges
        .iter()
        .all(|b| b.descriptor == "ohos.security.atm"));
}

#[test]
fn idl_interface_extending_itself_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "idl/ISelf.idl",
        "package c;\ninterface ISelf extends ISelf { void A(); }\n",
    );
    write(
        dir.path(),
        "main.cpp",
        "#include \"iself.h\"\nint main() { return 0; }\n",
    );
    let program = build(dir.path(), PreprocessOptions::new());
    let idl = idl_warnings(&program);
    assert_eq!(idl.len(), 1, "{idl:?}");
    assert!(
        idl[0].contains("ISelf.idl") && idl[0].contains("extends itself"),
        "{}",
        idl[0]
    );
    // It is synthesized as the interface it would be without the clause.
    assert!(declares(&program, "c::ISelf::A"));
    assert!(!program.derives_from("c::ISelf", "c::ISelf"));
}

/// A directory of the generated root's name that is on disk is a
/// dependency root like the generated one, and its headers are indexed.
#[test]
fn idl_generated_directory_on_disk_is_indexed() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "idl/IAtm.idl", IATM_IDL);
    write(
        dir.path(),
        ".trace-idl-generated/notes.h",
        "int Notes(int level);\n",
    );
    write(
        dir.path(),
        "main.cpp",
        "#include \"iatm.h\"\n#include \"notes.h\"\nint main() { return Notes(1); }\n",
    );
    let program = build(dir.path(), PreprocessOptions::new());
    assert_eq!(missing_includes(&program), Vec::<String>::new());
    let notes = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "Notes")
        .expect("notes.h is indexed");
    let file = &program.symbols.files[notes.file.0 as usize];
    assert!(file.path.ends_with(".trace-idl-generated/notes.h"));
    assert!(file.is_dep);
}

/// A hand-written `AtmClient` pairs with the stub `IAtm.idl` names, as an
/// `AtmProxy` does, and its bridge carries the interface's descriptor.
#[test]
fn idl_client_class_bridges_under_the_interfaces_descriptor() {
    let dir = idl_basic_copy();
    write(
        dir.path(),
        "client/atm_client.cpp",
        "#include \"iatm.h\"\nnamespace OHOS { namespace Security {\nclass AtmClient {\npublic:\n    int VerifyAccessToken(unsigned id, int &s);\n    sptr<IRemoteObject> remote_;\n};\nint AtmClient::VerifyAccessToken(unsigned id, int &s) { return remote_->SendRequest(1, id, s); }\n} }\n",
    );
    let program = build(dir.path(), PreprocessOptions::new());
    let (pag, _) = analyze(&program);
    let descriptors: Vec<&str> = pag
        .ipc_bridges
        .iter()
        .filter(|b| {
            fn_name(&program, b.proxy_method) == "OHOS::Security::AtmClient::VerifyAccessToken"
        })
        .map(|b| b.descriptor.as_str())
        .collect();
    assert_eq!(descriptors, vec!["OHOS.Security.IAtm"]);
}
