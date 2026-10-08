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
