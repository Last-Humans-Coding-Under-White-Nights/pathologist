//! Integration tests for IPC proxy/stub bridge detection.

use crate::common;

use std::collections::HashSet;
use std::path::PathBuf;
use trace_analysis::{analyze, analyze_with_options, AnalyzeOptions, ResolutionKind};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

fn build(
    name: &str,
) -> (
    trace_ir::Program,
    trace_analysis::Pag,
    trace_analysis::AnalysisResult,
) {
    let root = fixture(name);
    let include_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/include");
    let opts = PreprocessOptions::new()
        .with_include(root.clone())
        .with_include(include_dir);
    let program = build_program(&root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);
    (program, pag, analysis)
}

fn fn_name(program: &trace_ir::Program, id: trace_ir::FnId) -> String {
    program.symbols.function(id).name.clone()
}

fn has_bridge_edge(
    program: &trace_ir::Program,
    analysis: &trace_analysis::AnalysisResult,
    caller: &str,
    callee: &str,
) -> bool {
    analysis.call_edges.iter().any(|e| {
        fn_name(program, e.caller) == caller
            && fn_name(program, e.callee) == callee
            && e.resolution == ResolutionKind::IpcBridge
    })
}

#[test]
fn ipc_datashare_summary_includes_inherited_implementations() {
    check_inherited_datashare_summary("ipc_datashare_inherited");
}

#[test]
fn ipc_datashare_summary_includes_inherited_final_overrider() {
    check_inherited_datashare_summary("ipc_datashare_virtual_inherited");
}

fn check_inherited_datashare_summary(fixture: &str) {
    let (program, pag, analysis) = build(fixture);
    let sender = "OHOS::DataShare::DataShareHelper::OpenFile";
    let handler = "OHOS::DataShare::Backend::OpenFile";
    assert!(has_bridge_edge(&program, &analysis, sender, handler));
    assert_eq!(
        pag.ipc_bridges.len(),
        1,
        "inherited handlers are deduplicated"
    );
    let target = pag.ipc_bridges[0].stub_handler;
    assert_eq!(fn_name(&program, target), handler);
    assert!(program.symbols.function(target).is_defined);
    assert!(analysis.call_edges.iter().any(|edge| {
        edge.callee == target
            && edge.resolution == ResolutionKind::IpcBridge
            && edge.call_site == trace_analysis::SYNTHETIC_CALL_SITE
    }));
    assert!(analysis.execution_contexts.iter().any(|context| {
        context.entry == target
            && context.kind == trace_analysis::ContextKind::IpcHandler
            && context.self_concurrent
    }));
}

#[test]
fn ipc_summary_respects_default_test_partition() {
    for test_sender in [false, true] {
        check_summary_partition(PartitionCase {
            test_sender,
            ..Default::default()
        });
    }
}

#[test]
fn ipc_summary_respects_custom_test_partition() {
    for test_sender in [false, true] {
        check_summary_partition(PartitionCase {
            test_dir: "specs",
            policy: trace_ir::TestPartition::from_options(false, vec!["specs".into()]).unwrap(),
            test_sender,
            ..Default::default()
        });
    }
}

#[test]
fn ipc_summary_allows_test_providers_when_partition_disabled() {
    check_summary_partition(PartitionCase {
        policy: trace_ir::TestPartition::from_options(true, vec![]).unwrap(),
        production_admitted: true,
        ..Default::default()
    });
}

#[test]
fn ipc_summary_partition_keeps_authoritative_cross_image_bridges() {
    let one = check_summary_partition(PartitionCase {
        images: true,
        production_admitted: true,
        ..Default::default()
    });
    let four = check_summary_partition(PartitionCase {
        images: true,
        production_admitted: true,
        jobs: 4,
        ..Default::default()
    });
    assert_eq!(one, four, "IPC call edges must not depend on indexing jobs");
}

struct PartitionCase {
    test_dir: &'static str,
    policy: trace_ir::TestPartition,
    images: bool,
    test_sender: bool,
    production_admitted: bool,
    jobs: usize,
}

impl Default for PartitionCase {
    fn default() -> Self {
        Self {
            test_dir: "tests",
            policy: Default::default(),
            images: false,
            test_sender: false,
            production_admitted: false,
            jobs: 1,
        }
    }
}

type PartitionEdge = (
    String,
    String,
    String,
    Option<trace_ir::TargetId>,
    Option<trace_ir::TargetId>,
);

fn check_summary_partition(case: PartitionCase) -> Vec<PartitionEdge> {
    let PartitionCase {
        test_dir,
        policy,
        images,
        test_sender,
        production_admitted,
        jobs,
    } = case;
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::create_dir(root.join(test_dir)).unwrap();
    std::fs::write(root.join("api.hpp"),
        "namespace OHOS { namespace DataShare { class DataShareHelper { public: int OpenFile(int); }; }}"
    ).unwrap();
    std::fs::write(root.join("src/main.cpp"),
        "#include \"api.hpp\"\nnamespace OHOS { namespace DataShare { int use(DataShareHelper *h) { return h->OpenFile(1); } }}"
    ).unwrap();
    let mut provider = String::from(
        "#include \"api.hpp\"\nnamespace OHOS { namespace DataShare {\n\
         class DataShareStub { public: virtual int OpenFile(int) = 0; };\n\
         class MockProvider : public DataShareStub { public: int OpenFile(int x) { return x; } };\n"
    );
    if test_sender {
        provider.push_str(
            "class TestHelper : public DataShareHelper { public: int OpenFile(int); };\n\
             int test_use(TestHelper *h) { return h->OpenFile(1); }\nint shared_test_use(DataShareHelper *h) { return h->OpenFile(2); }\n",
        );
    }
    provider.push_str("}}\n");
    std::fs::write(root.join(test_dir).join("provider.cpp"), provider).unwrap();
    if images {
        let commands: Vec<_> = [
            ("src/main.cpp".to_owned(), "client"),
            (format!("{test_dir}/provider.cpp"), "service"),
        ]
        .into_iter()
        .map(|(file, image)| {
            serde_json::json!({
                "directory": root, "file": file, "output": format!("{image}.o"),
                "arguments": ["c++", "-I", root, "-c", file, "-o", format!("{image}.o")]
            })
        })
        .collect();
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!(commands).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("link_commands.json"), serde_json::json!([
            {"directory": root, "output": "client", "arguments": ["c++", "client.o", "-o", "client"]},
            {"directory": root, "output": "service", "arguments": ["c++", "service.o", "-o", "service"]}
        ]).to_string()).unwrap();
    }
    let mut opts = PreprocessOptions::new().with_include(root.to_path_buf());
    opts.test_partition = policy;
    let program = trace_parse::build_program_with_jobs(root, &opts, jobs).unwrap();
    assert_eq!(program.symbols.has_target_scopes(), images);
    let (_, analysis) = analyze(&program);
    let functions = &program.symbols.functions;
    let caller = functions
        .iter()
        .find(|f| f.name == "OHOS::DataShare::use")
        .unwrap();
    let sender = functions
        .iter()
        .find(|f| {
            f.name == "OHOS::DataShare::DataShareHelper::OpenFile" && f.target == caller.target
        })
        .unwrap();
    let handler = functions
        .iter()
        .find(|f| f.name == "OHOS::DataShare::MockProvider::OpenFile" && f.is_defined)
        .unwrap();
    let bridge = |from| {
        analysis.call_edges.iter().any(|edge| {
            edge.caller == from
                && edge.callee == handler.id
                && edge.resolution == ResolutionKind::IpcBridge
                && edge.call_site == trace_analysis::SYNTHETIC_CALL_SITE
        })
    };
    assert_eq!(
        bridge(sender.id),
        production_admitted,
        "production sender must respect the partition"
    );
    if test_sender {
        let shared = functions
            .iter()
            .find(|f| f.name == "OHOS::DataShare::shared_test_use")
            .unwrap();
        let bindings: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|e| e.caller == shared.id)
            .map(|e| e.callee)
            .collect();
        assert_eq!(
            bindings,
            [sender.id],
            "the test call really shares the production-declared sender"
        );
        let test = functions
            .iter()
            .find(|f| f.name == "OHOS::DataShare::TestHelper::OpenFile")
            .unwrap();
        assert!(
            bridge(test.id),
            "test senders must retain access to their providers"
        );
    }
    assert_eq!(
        analysis
            .execution_contexts
            .iter()
            .any(|ctx| ctx.entry == handler.id
                && ctx.kind == trace_analysis::ContextKind::IpcHandler
                && ctx.self_concurrent),
        production_admitted || test_sender
    );
    if images {
        assert!(sender.target.is_some());
        assert!(handler.target.is_some());
        assert_ne!(sender.target, handler.target);
    }
    let mut edges: Vec<_> = analysis
        .call_edges
        .iter()
        .map(|edge| {
            let from = program.symbols.function(edge.caller);
            let to = program.symbols.function(edge.callee);
            (
                from.name.clone(),
                to.name.clone(),
                format!("{:?}", edge.resolution),
                from.target,
                to.target,
            )
        })
        .collect();
    edges.sort();
    edges
}

#[test]
fn ipc_datashare_framework_summary_with_declared_sptr_reaches_extension() {
    let root = fixture("ipc_datashare");
    let opts = PreprocessOptions::new()
        .with_include(root.clone())
        .with_define("DECLARED_SPTR", "1");
    let program = build_program(&root, &opts).unwrap();
    let (_, analysis) = analyze(&program);
    let stub = "OHOS::DataShare::MediaDataShareStubImpl::OpenFile";
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "DataShare::DataShareHelper::OpenFile",
        stub
    ));
    assert!(analysis
        .call_edges
        .iter()
        .any(|e| fn_name(&program, e.caller) == stub
            && fn_name(&program, e.callee)
                == "OHOS::AbilityRuntime::MediaDataShareExtAbility::OpenFile"));
}

#[test]
fn ipc_datashare_framework_summary_reaches_extension_ability() {
    let (program, pag, analysis) = build("ipc_datashare");
    let stub = "OHOS::DataShare::MediaDataShareStubImpl::OpenFile";
    let edges: Vec<_> = analysis
        .call_edges
        .iter()
        .map(|e| {
            (
                fn_name(&program, e.caller),
                fn_name(&program, e.callee),
                e.resolution,
            )
        })
        .collect();
    let has = |caller: &str, callee: &str| edges.iter().any(|(a, b, _)| a == caller && b == callee);
    assert!(
        has("shadow::DeclaredFactory", "shadow::Result::Run"),
        "{edges:?}"
    );
    assert!(
        has(
            "OHOS::Media::MediaLibraryManager::OpenAsset",
            "DataShare::DataShareHelper::OpenFile"
        ),
        "{edges:?}"
    );
    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "DataShare::DataShareHelper::OpenFile",
            stub
        ),
        "{edges:?}"
    );
    assert!(
        has(stub, "OHOS::DataShare::MediaDataShareStubImpl::GetOwner"),
        "{edges:?}"
    );
    assert!(
        has(
            stub,
            "OHOS::AbilityRuntime::MediaDataShareExtAbility::OpenFile"
        ),
        "{edges:?}"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|(caller, callee, resolution)| {
                caller == stub
                    && callee == "OHOS::alternate::MediaDataShareExtAbility::OpenFile"
                    && *resolution == ResolutionKind::Direct
            })
            .count(),
        1,
        "unresolved imports keep the overload compatible with two arguments"
    );
    let declared_edges: Vec<_> = edges
        .iter()
        .filter(|(caller, _, _)| caller == "declared::MissingMember")
        .collect();
    assert!(!declared_edges.is_empty());
    assert!(declared_edges.iter().all(|(_, callee, resolution)| {
        callee == "declared::MediaDataShareExtAbility::OpenFile"
            && *resolution == ResolutionKind::External
    }));
    assert_eq!(
        pag.ipc_bridges.len(),
        3,
        "both provider overloads must be kept"
    );
    for bridge in &pag.ipc_bridges {
        assert!(analysis.execution_contexts.iter().any(|context| {
            context.entry == bridge.stub_handler
                && context.kind == trace_analysis::ContextKind::IpcHandler
                && context.self_concurrent
        }));
    }
    assert!(analysis
        .call_edges
        .iter()
        .filter(|e| e.resolution == ResolutionKind::IpcBridge)
        .all(|e| {
            e.call_site == trace_analysis::SYNTHETIC_CALL_SITE
                && !fn_name(&program, e.caller).contains("unrelated")
                && !fn_name(&program, e.caller).contains("LocalHelper")
                && !fn_name(&program, e.callee).contains("unrelated")
        }));
}

#[test]
fn ipc_datashare_summary_respects_options_and_model_override() {
    let (program, _, _) = build("ipc_datashare");
    let opts = AnalyzeOptions {
        enable_ipc: false,
        ..Default::default()
    };
    let (pag, analysis) = analyze_with_options(&program, opts);
    assert!(pag.ipc_bridges.is_empty());
    assert!(analysis
        .call_edges
        .iter()
        .all(|e| e.resolution != ResolutionKind::IpcBridge));

    let mut opts = AnalyzeOptions::default();
    std::sync::Arc::make_mut(&mut opts.models)
        .merge_toml_str(
            r#"
        [[model]]
        name = "OHOS::DataShare::DataShareHelper::OpenFile"
        effects = []
    "#,
        )
        .unwrap();
    let (pag, _) = analyze_with_options(&program, opts.clone());
    assert!(pag.ipc_bridges.is_empty());

    std::sync::Arc::make_mut(&mut opts.models)
        .merge_toml_str(
            r#"
        [[model]]
        name = "OHOS::DataShare::DataShareHelper::OpenFile"
        effects = [{ kind = "ipc", target = "OHOS::DataShare::MediaDataShareStubImpl::OpenFile" }]
    "#,
        )
        .unwrap();
    let (pag, analysis) = analyze_with_options(&program, opts);
    assert_eq!(pag.ipc_bridges.len(), 1);
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "DataShare::DataShareHelper::OpenFile",
        "OHOS::DataShare::MediaDataShareStubImpl::OpenFile"
    ));
}

#[test]
fn ipc_basic_bridges_proxy_to_stub() {
    let (program, pag, analysis) = build("ipc_basic");

    // Sanity: both classes are indexed.
    assert!(program
        .symbols
        .functions
        .iter()
        .any(|f| f.name.contains("IFooProxy")));
    assert!(program
        .symbols
        .functions
        .iter()
        .any(|f| f.name.contains("IFooStub")));

    // The bridge proxy→stub handlers must appear as IPC call edges.
    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "IFooProxy::GetInfo",
            "IFooStub::HandleGetInfo"
        ),
        "expected GetInfo → HandleGetInfo bridge edge, got: {:?}",
        analysis
            .call_edges
            .iter()
            .filter(|e| fn_name(&program, e.caller).contains("IFoo"))
            .map(|e| (fn_name(&program, e.caller), fn_name(&program, e.callee)))
            .collect::<Vec<_>>()
    );
    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "IFooProxy::SetInfo",
            "IFooStub::HandleSetInfo"
        ),
        "expected SetInfo → HandleSetInfo bridge edge"
    );
    assert!(
        !analysis.call_edges.iter().any(|e| {
            e.resolution == ResolutionKind::IpcBridge
                && fn_name(&program, e.caller) == "IFooProxy::LocalOnly"
        }),
        "a proxy method without SendRequest must not produce an IPC bridge"
    );

    // Bridges are recorded on the Pag.
    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[test]
fn ipc_if_else_bridges_proxy_to_stub() {
    let (program, pag, analysis) = build("ipc_if_else");

    assert!(has_bridge_edge(
        &program,
        &analysis,
        "IThermalProxy::OnTemperatureChanged",
        "IThermalStub::OnTemperatureChanged"
    ));
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "IThermalProxy::OnLevelChanged",
        "IThermalStub::OnLevelChanged"
    ));
    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[test]
fn ipc_enum_bridges_proxy_to_stub() {
    let (program, pag, analysis) = build("ipc_enum");

    assert!(has_bridge_edge(
        &program,
        &analysis,
        "FooProxy::Add",
        "FooStub::Add"
    ));
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "FooProxy::Query",
        "FooStub::Query"
    ));
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "FooProxy::Destroy",
        "FooStub::Destroy"
    ));
    assert!(
        !has_bridge_edge(&program, &analysis, "FooProxy::Add", "FooStub::Add1"),
        "no spurious edge"
    );
    assert_eq!(pag.ipc_bridges.len(), 3);
}

#[test]
fn ipc_callback_bridges_callback_proxy_to_stub() {
    let (program, pag, analysis) = build("ipc_callback");

    assert!(has_bridge_edge(
        &program,
        &analysis,
        "ConnectionProxy::OnConnect",
        "ConnectionStub::OnConnect"
    ));
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "ConnectionProxy::OnDisconnect",
        "ConnectionStub::OnDisconnect"
    ));
    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[test]
fn ipc_stub_suffix_handler_fallback() {
    // A stub whose handlers are named only with a `Stub` suffix (no plain
    // interface-method name) is matched via the `{name}Stub` fallback.
    let (program, pag, analysis) = build("ipc_stub_suffix");

    assert!(has_bridge_edge(
        &program,
        &analysis,
        "FooProxy::OnFoo",
        "FooStub::OnFooStub"
    ));
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "FooProxy::OnBar",
        "FooStub::OnBarStub"
    ));
    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[test]
fn ipc_overloads_retain_every_possible_handler() {
    let (program, pag, analysis) = build("ipc_overloads");
    let proxy_methods: HashSet<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.is_defined && f.name == "OverloadProxy::Run")
        .map(|f| f.id)
        .collect();
    let stub_handlers: HashSet<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.is_defined && f.name == "OverloadStub::Run")
        .map(|f| f.id)
        .collect();

    assert_eq!(
        proxy_methods.len(),
        2,
        "both proxy overloads must be indexed"
    );
    assert_eq!(
        stub_handlers.len(),
        2,
        "both stub overloads must be indexed"
    );

    let bridge_pairs: HashSet<_> = pag
        .ipc_bridges
        .iter()
        .filter(|bridge| proxy_methods.contains(&bridge.proxy_method))
        .map(|bridge| (bridge.proxy_method, bridge.stub_handler))
        .collect();
    let expected_pairs: HashSet<_> = proxy_methods
        .iter()
        .flat_map(|proxy| stub_handlers.iter().map(move |handler| (*proxy, *handler)))
        .collect();
    assert_eq!(bridge_pairs, expected_pairs);

    let ipc_edge_pairs: HashSet<_> = analysis
        .call_edges
        .iter()
        .filter(|edge| {
            edge.resolution == ResolutionKind::IpcBridge && proxy_methods.contains(&edge.caller)
        })
        .map(|edge| (edge.caller, edge.callee))
        .collect();
    assert_eq!(ipc_edge_pairs, expected_pairs);

    let downstream: HashSet<_> = analysis
        .call_edges
        .iter()
        .filter(|edge| stub_handlers.contains(&edge.caller))
        .map(|edge| fn_name(&program, edge.callee))
        .collect();
    assert_eq!(
        downstream,
        HashSet::from(["HandleInt".to_string(), "HandleDouble".to_string()])
    );
}

#[test]
fn no_ipc_bridges_without_proxy_stub_pair() {
    // A fixture with no *Proxy/*Stub classes should produce no bridges.
    let (_program, pag, _analysis) = build("direct_call");
    assert_eq!(pag.ipc_bridges.len(), 0);
}

#[test]
fn ipc_interface_fallback_prefers_defined_overrides() {
    // Stub with no handler method bodies — OnRemoteRequest calls inherited
    // interface methods directly. When a derived concrete server is indexed,
    // the fallback should bridge to its method bodies rather than dead-end at
    // the external interface declarations.
    let (program, pag, analysis) = build("ipc_interface_fallback");

    assert!(
        program.template_bases().iter().any(|fact| {
            fact.derived == "svc::WrappedStub"
                && fact.spelling == "OHOS::IRemoteStub<IWrapped>"
                && fact.declaration_scope == "svc"
        }),
        "expected templated inheritance to survive lowering and merge, got: {:?}",
        program.template_bases()
    );
    assert!(
        program.template_bases().iter().any(|fact| {
            fact.derived == "svc::RelativeStub"
                && fact.spelling == "OHOS::IRemoteStub<api::IRelative>"
                && fact.declaration_scope == "svc"
        }),
        "expected relative qualified template argument and scope, got: {:?}",
        program.template_bases()
    );

    let bridge_names: Vec<_> = pag
        .ipc_bridges
        .iter()
        .map(|b| {
            (
                fn_name(&program, b.proxy_method),
                fn_name(&program, b.stub_handler),
            )
        })
        .collect();

    assert!(
        bridge_names
            .iter()
            .any(|(p, s)| p == "QueryResultProxy::HasNext" && s == "QueryResultService::HasNext"),
        "expected HasNext → QueryResultService::HasNext bridge, got: {:?}",
        bridge_names
    );
    assert!(
        bridge_names
            .iter()
            .any(|(p, s)| p == "QueryResultProxy::GetNext" && s == "QueryResultService::GetNext"),
        "expected GetNext → QueryResultService::GetNext bridge, got: {:?}",
        bridge_names
    );
    assert!(
        bridge_names
            .iter()
            .any(|(p, s)| p == "svc::WrappedProxy::Fetch" && s == "svc::IWrapped::Fetch"),
        "expected template-base interface fallback, got: {:?}",
        bridge_names
    );
    assert!(
        !bridge_names
            .iter()
            .any(|(_, s)| s == "OHOS::IWrapped::Fetch"),
        "template argument must resolve in the stub declaration scope: {bridge_names:?}"
    );
    assert!(
        bridge_names
            .iter()
            .any(|(p, s)| { p == "svc::RelativeProxy::Run" && s == "svc::api::IRelative::Run" }),
        "expected relative qualified interface fallback, got: {bridge_names:?}"
    );
    assert!(
        !bridge_names.iter().any(|(_, s)| s == "api::IRelative::Run"),
        "relative qualified argument must prefer the nearest declaration scope: {bridge_names:?}"
    );
    assert!(
        bridge_names
            .iter()
            .any(|(p, s)| p == "DefaultProxy::Run" && s == "IDefault::Run"),
        "expected defined ancestor fallback, got: {:?}",
        bridge_names
    );
    assert_eq!(pag.ipc_bridges.len(), 5);
    assert!(
        !bridge_names.iter().any(|(_, s)| s.starts_with("Other::")),
        "interface fallback must stay in the stub namespace: {bridge_names:?}"
    );
    assert!(
        !bridge_names
            .iter()
            .any(|(_, s)| s.starts_with("QueryResult::")),
        "interface fallback must follow inheritance, not name similarity: {bridge_names:?}"
    );
    assert!(
        !bridge_names
            .iter()
            .any(|(p, _)| p == "ConstructorOnlyProxy::Ping"),
        "constructor-only classes must not register as IPC stubs: {bridge_names:?}"
    );

    let downstream: HashSet<_> = analysis
        .call_edges
        .iter()
        .filter(|edge| {
            matches!(
                fn_name(&program, edge.caller).as_str(),
                "QueryResultService::HasNext" | "QueryResultService::GetNext" | "IDefault::Run"
            )
        })
        .map(|edge| fn_name(&program, edge.callee))
        .collect();
    assert_eq!(
        downstream,
        HashSet::from([
            "HasNextImpl".to_string(),
            "GetNextImpl".to_string(),
            "DefaultRunImpl".to_string()
        ])
    );
}

#[test]
fn ipc_bridge_export_has_no_call_site_and_keeps_its_caller() {
    type IpcRow = (Option<i64>, Option<i64>, String, String, String);

    let (program, pag, analysis) = build("ipc_basic");
    let db = common::export_program(&program, &pag, &analysis);
    let conn = trace_db::open_db(db.path()).expect("open exported database");

    let rows: Vec<IpcRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT ce.call_site_id, cs.line, caller.name, callee.name, ce.resolution \
                 FROM call_edges ce \
                 LEFT JOIN call_sites cs ON cs.id = ce.call_site_id \
                 JOIN functions caller ON caller.id = ce.caller_fn_id \
                 JOIN functions callee ON callee.id = ce.callee_fn_id \
                 WHERE ce.resolution = 'ipc' ORDER BY caller.name, callee.name",
            )
            .expect("prepare IPC edge query");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("query IPC edges")
        .collect::<Result<_, _>>()
        .expect("read IPC edges")
    };

    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(site, line, _, _, resolution)| {
        site.is_none() && line.is_none() && resolution == "ipc"
    }));
    assert!(rows.iter().any(|(_, _, caller, callee, _)| {
        caller == "IFooProxy::GetInfo" && callee == "IFooStub::HandleGetInfo"
    }));
}

#[test]
fn ipc_disabled_via_options() {
    // With enable_ipc = false, no bridge edges are emitted even when the
    // source contains a proxy/stub pair.
    let root = fixture("ipc_basic");
    let include_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/include");
    let opts = PreprocessOptions::new()
        .with_include(root.clone())
        .with_include(include_dir);
    let program = build_program(&root, &opts).expect("build program");

    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            enable_ipc: false,
            ..Default::default()
        },
    );
    assert!(pag.ipc_bridges.is_empty());
    let has_bridge = analysis.call_edges.iter().any(|e| {
        e.resolution == ResolutionKind::IpcBridge
            && fn_name(&program, e.caller) == "IFooProxy::GetInfo"
    });
    assert!(!has_bridge, "expected no bridge edges when IPC is disabled");
}

#[test]
fn ipc_smart_ptr_reaches_the_stub_through_an_undeclared_wrapper() {
    // The two mechanisms are independent — `->` on an out-of-tree wrapper
    // resolves the proxy class, and the proxy then bridges to its handler —
    // so nothing pins the chain unless a test walks both links.
    let (program, pag, analysis) = build("ipc_smart_ptr");

    for (caller, callee) in [
        ("CallThroughWrapper", "IFooProxy::GetInfo"),
        ("CallThroughWrapperField", "IFooProxy::SetInfo"),
    ] {
        assert!(
            analysis.call_edges.iter().any(|e| {
                fn_name(&program, e.caller) == caller && fn_name(&program, e.callee) == callee
            }),
            "{caller} must reach {callee} through the wrapper"
        );
    }

    for (proxy, handler) in [
        ("IFooProxy::GetInfo", "IFooStub::HandleGetInfo"),
        ("IFooProxy::SetInfo", "IFooStub::HandleSetInfo"),
    ] {
        assert!(
            has_bridge_edge(&program, &analysis, proxy, handler),
            "{proxy} must still bridge to {handler}"
        );
    }

    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[test]
fn cpp_singleton_ipc_chain_reaches_service() {
    let (program, _pag, analysis) = build("cpp_singleton_ipc");
    let has = |caller: &str, callee: &str| {
        analysis
            .call_edges
            .iter()
            .any(|e| fn_name(&program, e.caller) == caller && fn_name(&program, e.callee) == callee)
    };
    assert!(has("app", "DelayedSingleton::GetInstance"));
    assert!(has("app", "FooClient::Launch"));
    assert!(
        has("FooClient::Launch", "FooProxy::Start"),
        "the client reaches the proxy through IFoo"
    );
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "FooProxy::Start",
        "FooService::Start"
    ));
}

#[test]
fn ipc_rejects_known_interface_mismatch() {
    let (program, _pag, analysis) = build("ipc_interface_mismatch");
    assert!(
        !has_bridge_edge(&program, &analysis, "WidgetProxy::Ping", "WidgetStub::Ping"),
        "IFoo's proxy is not IBar's stub"
    );
    assert!(has_bridge_edge(
        &program,
        &analysis,
        "api::GoodProxy::Ping",
        "api::GoodStub::Ping"
    ));
    assert!(
        has_bridge_edge(&program, &analysis, "LegacyProxy::Ping", "LegacyStub::Ping"),
        "unknown interfaces keep name-based pairing"
    );
    assert!(
        !has_bridge_edge(&program, &analysis, "RelayProxy::Ping", "RelayStub::Ping"),
        "RelayStub serves IBar through its middle base, so it is not IFoo's stub"
    );
    assert!(
        has_bridge_edge(&program, &analysis, "ExtProxy::Ping", "ExtStub::Ping"),
        "a derived interface serves its base's stub"
    );
    assert!(
        has_bridge_edge(&program, &analysis, "OrphanProxy::Ping", "OrphanStub::Ping"),
        "dropping a bridge takes a declared interface on both sides"
    );
    assert!(
        has_bridge_edge(&program, &analysis, "TplProxy::Ping", "TplStub::Ping"),
        "a template parameter is not an interface"
    );
    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "alias_ns::AliasProxy::Ping",
            "alias_ns::AliasStub::Ping"
        ),
        "an alias names the class it aliases"
    );
    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "sh::cam::ShadowProxy::Ping",
            "sh::cam::ShadowStub::Ping"
        ),
        "a shadowing reading does not drop the bridge"
    );
    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "client::UsedProxy::Ping",
            "client::UsedStub::Ping"
        ),
        "a using-directive may import the interface a bare name spells"
    );
    assert!(
        !has_bridge_edge(&program, &analysis, "GlobProxy::Ping", "GlobStub::Ping"),
        "`::IGlob` is the global class, not gapi's namesake"
    );
    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "client2::AlProxy::Ping",
            "client2::AlStub::Ping"
        ),
        "a bare spelling may name an imported alias"
    );
    assert!(
        has_bridge_edge(&program, &analysis, "DeepProxy::Ping", "IDeep::Ping"),
        "a recovered interface's own IRemoteStub base is followed"
    );
    assert!(
        !has_bridge_edge(&program, &analysis, "DepProxy::Ping", "Iface2::Ping"),
        "a template parameter is no fallback ancestor"
    );
    let reached: Vec<String> = analysis
        .call_edges
        .iter()
        .filter(|e| fn_name(&program, e.caller) == "ping")
        .map(|e| fn_name(&program, e.callee))
        .collect();
    for target in ["WidgetMock::Ping", "WidgetProxy::Ping"] {
        assert!(reached.iter().any(|c| c == target), "{target}: {reached:?}");
    }
}

#[test]
fn no_ipc_flag_drops_only_the_bridges() {
    type Edge = (String, String, String);
    let edges = |extra: &[&str]| -> Vec<Edge> {
        let db = common::TempDb::new("ipc.db");
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("analyze")
            .arg(fixture("ipc_basic"))
            .args(["--include"])
            .arg(fixture("include"))
            .args(extra)
            .arg("-o")
            .arg(db.path())
            .output()
            .expect("run trace analyze");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let conn = trace_db::open_db(db.path()).expect("open exported database");
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM call_edges ce \
                 WHERE ce.call_site_id IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM call_sites cs WHERE cs.id = ce.call_site_id)",
                [],
                |row| row.get(0),
            )
            .expect("count edges without a call site row");
        assert_eq!(orphans, 0, "a synthetic site was indexed as a call site");
        let mut stmt = conn
            .prepare(
                "SELECT caller.name, callee.name, ce.resolution FROM call_edges ce \
                 JOIN functions caller ON caller.id = ce.caller_fn_id \
                 JOIN functions callee ON callee.id = ce.callee_fn_id \
                 ORDER BY 1, 2, 3",
            )
            .expect("prepare edge query");
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query edges")
            .collect::<Result<_, _>>()
            .expect("read edges");
        rows
    };
    let with = edges(&[]);
    let without = edges(&["--no-ipc"]);
    assert!(with.iter().any(|(_, _, resolution)| resolution == "ipc"));
    let ordinary: Vec<&Edge> = with.iter().filter(|(_, _, r)| r != "ipc").collect();
    assert_eq!(without.iter().collect::<Vec<_>>(), ordinary);
}
