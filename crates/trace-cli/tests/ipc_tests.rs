//! Integration tests for IPC proxy/stub bridge detection.

mod common;

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
        program.template_bases.iter().any(|fact| {
            fact.derived == "svc::WrappedStub"
                && fact.spelling == "OHOS::IRemoteStub<IWrapped>"
                && fact.declaration_scope == "svc"
        }),
        "expected templated inheritance to survive lowering and merge, got: {:?}",
        program.template_bases
    );
    assert!(
        program.template_bases.iter().any(|fact| {
            fact.derived == "svc::RelativeStub"
                && fact.spelling == "OHOS::IRemoteStub<api::IRelative>"
                && fact.declaration_scope == "svc"
        }),
        "expected relative qualified template argument and scope, got: {:?}",
        program.template_bases
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
fn ipc_opcode_disparate_handler_name_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("ipc_test.cpp"),
        r#"
enum class FaultCode : unsigned int {
    REPORT = 0,
    QUERY = 1,
};

struct IRemoteObject {
    int SendRequest(unsigned int code, int data);
};

class FaultServiceStub {
public:
    int ProcessCrashLog(int data) { return 42; }
    int RetrieveRecord(int data) { return 99; }

    int OnRemoteRequest(unsigned int code, int data) {
        switch (code) {
            case static_cast<unsigned int>(FaultCode::REPORT):
                return ProcessCrashLog(data);
            case static_cast<unsigned int>(FaultCode::QUERY):
                return RetrieveRecord(data);
            default:
                return -1;
        }
    }
};

class FaultServiceProxy {
public:
    IRemoteObject *remote;

    int SubmitFaultReport(int data) {
        return remote->SendRequest(static_cast<unsigned int>(FaultCode::REPORT), data);
    }

    int FetchHistoricalLog(int data) {
        return remote->SendRequest(static_cast<unsigned int>(FaultCode::QUERY), data);
    }
};

int main() {
    return 0;
}
"#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);

    assert_eq!(program.ipc_sends.len(), 2, "must discover 2 IpcSend facts");
    assert_eq!(
        program.ipc_dispatches.len(),
        2,
        "must discover 2 IpcDispatch facts"
    );

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "FaultServiceProxy::SubmitFaultReport",
            "FaultServiceStub::ProcessCrashLog"
        ),
        "SubmitFaultReport must bridge to ProcessCrashLog via opcode REPORT"
    );

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "FaultServiceProxy::FetchHistoricalLog",
            "FaultServiceStub::RetrieveRecord"
        ),
        "FetchHistoricalLog must bridge to RetrieveRecord via opcode QUERY"
    );

    assert!(
        !has_bridge_edge(
            &program,
            &analysis,
            "FaultServiceProxy::SubmitFaultReport",
            "FaultServiceStub::RetrieveRecord"
        ),
        "SubmitFaultReport must not bridge to RetrieveRecord"
    );

    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[test]
fn ipc_opcode_arithmetic_base_offset_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("arith_ipc.cpp"),
        r#"
#define TRANS_BASE 100

struct IRemoteObject {
    int SendRequest(int code, int data);
};

class DeviceStub {
public:
    int ResetDevice(int data) { return 1; }
    int ProbeDevice(int data) { return 2; }

    int OnRemoteRequest(int code, int data) {
        switch (code) {
            case TRANS_BASE + 1:
                return ResetDevice(data);
            case TRANS_BASE + 2:
                return ProbeDevice(data);
            default:
                return 0;
        }
    }
};

class DeviceProxy {
public:
    IRemoteObject *remote;

    int TriggerReset(int x) {
        return remote->SendRequest(TRANS_BASE + 1, x);
    }

    int TriggerProbe(int x) {
        return remote->SendRequest(TRANS_BASE + 2, x);
    }
};
"#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "DeviceProxy::TriggerReset",
            "DeviceStub::ResetDevice"
        ),
        "TriggerReset must bridge to ResetDevice via TRANS_BASE + 1"
    );

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "DeviceProxy::TriggerProbe",
            "DeviceStub::ProbeDevice"
        ),
        "TriggerProbe must bridge to ProbeDevice via TRANS_BASE + 2"
    );

    assert!(
        !has_bridge_edge(
            &program,
            &analysis,
            "DeviceProxy::TriggerReset",
            "DeviceStub::ProbeDevice"
        ),
        "TriggerReset must not bridge to ProbeDevice"
    );

    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[test]
fn ipc_opcode_if_else_chain_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("if_chain_ipc.cpp"),
        r#"
enum class ThermalCode : unsigned int {
    CHANGED = 10,
    ASYNC_CHANGED = 20,
};

struct IRemoteObject {
    int SendRequest(unsigned int code, int data);
};

class ThermalStub {
public:
    int OnLevelChanged(int data) { return 0; }
    int OnAsyncLevelChanged(int data) { return 0; }

    int OnRemoteRequest(unsigned int code, int data) {
        if (code == static_cast<unsigned int>(ThermalCode::CHANGED)) {
            return OnLevelChanged(data);
        } else if (code == static_cast<unsigned int>(ThermalCode::ASYNC_CHANGED)) {
            return OnAsyncLevelChanged(data);
        }
        return -1;
    }
};

class ThermalProxy {
public:
    IRemoteObject *remote;

    int NotifyLevel(int x) {
        return remote->SendRequest(static_cast<unsigned int>(ThermalCode::CHANGED), x);
    }

    int NotifyAsyncLevel(int x) {
        return remote->SendRequest(static_cast<unsigned int>(ThermalCode::ASYNC_CHANGED), x);
    }
};
"#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "ThermalProxy::NotifyLevel",
            "ThermalStub::OnLevelChanged"
        ),
        "NotifyLevel must bridge to OnLevelChanged"
    );

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "ThermalProxy::NotifyAsyncLevel",
            "ThermalStub::OnAsyncLevelChanged"
        ),
        "NotifyAsyncLevel must bridge to OnAsyncLevelChanged"
    );

    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[cfg(feature = "smt")]
#[test]
fn ipc_opcode_symbolic_ternary_smt_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("smt_ipc_test.cpp"),
        r#"
#define OP_SYNC 100
#define OP_ASYNC 200
#define OP_OTHER 300

struct IRemoteObject {
    int SendRequest(int code, int data);
};

class DispatchStub {
public:
    int HandleSync(int data) { return 1; }
    int HandleAsync(int data) { return 2; }
    int HandleOther(int data) { return 3; }

    int OnRemoteRequest(int code, int data) {
        switch (code) {
            case OP_SYNC:
                return HandleSync(data);
            case OP_ASYNC:
                return HandleAsync(data);
            case OP_OTHER:
                return HandleOther(data);
            default:
                return 0;
        }
    }
};

class DispatchProxy {
public:
    IRemoteObject *remote;

    int SendDynamic(int data, int is_async) {
        return remote->SendRequest(is_async ? OP_ASYNC : OP_SYNC, data);
    }
};
"#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (pag, analysis) = analyze(&program);

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "DispatchProxy::SendDynamic",
            "DispatchStub::HandleAsync"
        ),
        "SendDynamic must bridge to HandleAsync"
    );

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "DispatchProxy::SendDynamic",
            "DispatchStub::HandleSync"
        ),
        "SendDynamic must bridge to HandleSync"
    );

    assert!(
        !has_bridge_edge(
            &program,
            &analysis,
            "DispatchProxy::SendDynamic",
            "DispatchStub::HandleOther"
        ),
        "SendDynamic must NOT bridge to HandleOther (infeasible opcode)"
    );

    assert_eq!(pag.ipc_bridges.len(), 2);
}

#[cfg(feature = "smt")]
#[test]
fn test_ipc_symbolic_opcode_enum_offset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.cpp"),
        r#"
        class IRemoteBroker {
        public:
            virtual ~IRemoteBroker() {}
        };

        class IRemoteObject {
        public:
            virtual int SendRequest(unsigned int code, void* data, void* reply, void* option) = 0;
        };

        class MyInterface : public IRemoteBroker {
        public:
            enum {
                BASE_CODE = 50,
                CODE_OP1 = BASE_CODE + 1,
                CODE_OP2 = BASE_CODE + 2,
            };
            virtual void Execute1() = 0;
            virtual void Execute2() = 0;
        };

        class MyProxy : public MyInterface {
        public:
            IRemoteObject* remote;
            void Execute1() override {
                remote->SendRequest(CODE_OP1, nullptr, nullptr, nullptr);
            }
            void Execute2() override {
                // Symbolic expression: BASE_CODE + 2
                remote->SendRequest(CODE_OP2, nullptr, nullptr, nullptr);
            }
        };

        class MyStub : public MyInterface {
        public:
            void TargetHandler1() {}
            void TargetHandler2() {}

            int OnRemoteRequest(unsigned int code, void* data, void* reply, void* option) {
                switch (code) {
                    case CODE_OP1:
                        TargetHandler1();
                        return 0;
                    case CODE_OP2:
                        TargetHandler2();
                        return 0;
                    default:
                        return -1;
                }
            }
        };
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "MyProxy::Execute1",
            "MyStub::TargetHandler1"
        ),
        "Execute1 should bridge to TargetHandler1 via CODE_OP1"
    );

    assert!(
        has_bridge_edge(
            &program,
            &analysis,
            "MyProxy::Execute2",
            "MyStub::TargetHandler2"
        ),
        "Execute2 should bridge to TargetHandler2 via CODE_OP2"
    );

    assert!(
        !has_bridge_edge(
            &program,
            &analysis,
            "MyProxy::Execute1",
            "MyStub::TargetHandler2"
        ),
        "Execute1 should not bridge to TargetHandler2"
    );
}


