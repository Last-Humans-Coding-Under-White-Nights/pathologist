# IPC call-graph support (within-repo Stub ↔ Proxy linking)

## Problem statement

OpenHarmony services use Binder IPC: a **proxy** method calls
`remote->SendRequest(opcode, data, reply)` and the **stub** dispatches in
`OnRemoteRequest(opcode, data, reply, option)` to the real implementation.
When both proxy and stub source live in the same repository the trace tool
could connect them, but today all IPC calls resolve as **External** to the
opaque Binder framework. The call graph therefore has a gap at every IPC
boundary — the most important communication axis in the system.

This plan adds within-repo synthetic call edges that bridge the proxy→stub
gap so existing pointer analysis and arg-flow extraction work unchanged.

## Scope

**v1 (this plan):** single-repository analysis where both the proxy and stub
implementations are indexed translation units. Cross-device / distributed
IPC (softbus transport) is explicitly out of scope.

## IPC pattern taxonomy

From the four evaluated repositories:

| Pattern | Mechanism | Example |
|---------|-----------|---------|
| **IDL-generated stub/proxy** | ZIDL tool produces `FooStub` / `FooProxy` from `.idl` files; stub dispatches by numeric opcode | Hiview, Camera, Thermal |
| **Hand-written stub/proxy** | Manual `IRemoteStub<IFoo>` / `IRemoteProxy<IFoo>` with `OnRemoteRequest` switch/if-else + per-method `SendRequest` | FaultLogger, DMS |
| **Distributed extension** | `IDExtensionStub` / `IDExtensionProxy` for cross-process lifecycle | DMS |

All share the same core: **proxy sends opcode → stub dispatches on opcode**.

### Dispatch shapes (the critical detail)

Stubs dispatch opcodes via two common patterns:

**Shape A — `switch` statement:**
```cpp
int FooStub::OnRemoteRequest(uint32_t code, MessageParcel &data, ...) {
    switch (code) {
        case TRANS_GET_INFO: return HandleGetInfo(data, reply);
        case TRANS_SET_INFO: return HandleSetInfo(data, reply);
        default: return IPCObjectStub::OnRemoteRequest(code, data, reply, option);
    }
}
```

**Shape B — if/else-if chain** (the user's specific question):
```cpp
int FooStub::OnRemoteRequest(uint32_t code, MessageParcel &data, ...) {
    if (code == TRANS_GET_INFO) {
        return HandleGetInfo(data, reply);
    } else if (code == TRANS_SET_INFO) {
        return HandleSetInfo(data, reply);
    }
    return IPCObjectStub::OnRemoteRequest(code, data, reply, option);
}
```

Opcodes may be `enum` members, `#define` constants, or
`IBinder::FIRST_CALL_TRANSACTION + N` expressions — not always literal
integers.

## Real-world examples (from the four evaluated repos)

These examples are concrete validation targets. Each one demonstrates a
pattern the detection must handle.

### Example 1: Switch dispatch with enum opcodes (hiviewdfx_hiview)

**Stub** — `hiviewdfx_hiview/plugins/faultlogger/service/idl/src/faultlogger_service_stub.cpp:109`:
```cpp
int FaultLoggerServiceStub::OnRemoteRequest(uint32_t code, MessageParcel &data,
    MessageParcel &reply, MessageOption &option)
{
    std::u16string descripter = FaultLoggerServiceStub::GetDescriptor();
    std::u16string remoteDescripter = data.ReadInterfaceToken();
    if (descripter != remoteDescripter) {
        HIVIEW_LOGE("read descriptor failed.");
        return -1;
    }

    switch (code) {
        case static_cast<uint32_t>(FaultLoggerServiceInterfaceCode::ADD_FAULTLOG): {
            sptr<FaultLogInfoOhos> ohosInfo = FaultLogInfoOhos::Unmarshalling(data);
            if (ohosInfo == nullptr) { return ERR_FLATTEN_OBJECT; }
            FaultLogInfoOhos info(*ohosInfo);
            AddFaultLog(info);
            return ERR_OK;
        }
        case static_cast<uint32_t>(FaultLoggerServiceInterfaceCode::QUERY_SELF_FAULTLOG): {
            int32_t type = data.ReadInt32();
            int32_t maxNum = data.ReadInt32();
            auto result = QuerySelfFaultLog(type, maxNum);
            if (!reply.WriteRemoteObject(result)) { return ERR_FLATTEN_OBJECT; }
            return ERR_OK;
        }
        case static_cast<uint32_t>(FaultLoggerServiceInterfaceCode::DESTROY): {
            Destroy();
            return ERR_OK;
        }
        default:
            return HandleOtherRemoteRequest(code, data, reply, option);
    }
}
```

**Proxy** — `hiviewdfx_hiview/plugins/faultlogger/service/idl/src/faultlogger_service_proxy.cpp:61`:
```cpp
sptr<IRemoteObject> FaultLoggerServiceProxy::QuerySelfFaultLog(int32_t faultType, int32_t maxNum)
{
    sptr<IRemoteObject> remote = Remote();
    if (remote == nullptr) { return nullptr; }
    MessageParcel data;
    if (!data.WriteInterfaceToken(FaultLoggerServiceProxy::GetDescriptor())) { return nullptr; }
    if (!data.WriteInt32(faultType)) { return nullptr; }
    if (!data.WriteInt32(maxNum)) { return nullptr; }
    MessageParcel reply;
    MessageOption option;
    if (remote->SendRequest(static_cast<uint32_t>(FaultLoggerServiceInterfaceCode::QUERY_SELF_FAULTLOG),
        data, reply, option) != ERR_OK) {
        return nullptr;
    }
    sptr<IRemoteObject> remoteObject = reply.ReadRemoteObject();
    return remoteObject;
}
```

**Opcode enum** — `hiviewdfx_hiview/plugins/faultlogger/service/idl/include/hiviewfaultlogger_ipc_interface_code.h:22`:
```cpp
enum class FaultLoggerServiceInterfaceCode {
    ADD_FAULTLOG = 0,
    QUERY_SELF_FAULTLOG,
    ENABLE_GWP_ASAN_GRAYSALE,
    DISABLE_GWP_ASAN_GRAYSALE,
    GET_GWP_ASAN_GRAYSALE,
    DESTROY,
    ENABLE_GWP_ASAN_INNER,
};
```

**Descriptor** — `hiviewdfx_hiview/plugins/faultlogger/service/idl/include/ifaultlogger_service.h:41`:
```cpp
DECLARE_INTERFACE_DESCRIPTOR(u"ohos.hiviewdfx.IFaultLoggerService");
```

**What to validate**: opcode resolution via `enum_class` member (0, 1, 5);
descriptor match between proxy and stub; handler name resolution for
`AddFaultLog`, `QuerySelfFaultLog`, `Destroy`.

### Example 2: if/else-if dispatch with enum opcodes (powermgr_thermal_manager)

**Stub** — `powermgr_thermal_manager/services/zidl/src/thermal_level_callback_stub.cpp:30`:
```cpp
int ThermalLevelCallbackStub::OnRemoteRequest(uint32_t code, MessageParcel &data,
    MessageParcel &reply, MessageOption &option)
{
    THERMAL_HILOGD(COMP_SVC,
        "ThermalLevelCallbackStub::OnRemoteRequest, cmd = %{public}d, flags= %{public}d",
        code, option.GetFlags());
    std::u16string descripter = ThermalLevelCallbackStub::GetDescriptor();
    std::u16string remoteDescripter = data.ReadInterfaceToken();
    if (descripter != remoteDescripter) {
        THERMAL_HILOGE(COMP_SVC,
            "ThermalLevelCallbackStub::OnRemoteRequest failed, descriptor is not matched!");
        return E_GET_THERMAL_SERVICE_FAILED;
    }
    const int DFX_DELAY_S = 60;
    int id = HiviewDFX::XCollie::GetInstance().SetTimer("ThermalLevelCallbackStub", DFX_DELAY_S,
        nullptr, nullptr, HiviewDFX::XCOLLIE_FLAG_LOG);
    int ret = ERR_OK;
    if (code == static_cast<uint32_t>(PowerMgr::ThermalLevelCallbackInterfaceCode::THERMAL_LEVEL_CHANGED)) {
        ret = OnThermalLevelChangedStub(data);
    } else if (code ==
        static_cast<uint32_t>(PowerMgr::ThermalLevelCallbackInterfaceCode::ASYNC_THERMAL_LEVEL_CHANGED)) {
        ret = OnAsyncThermalLevelChangedStub(data);
    } else {
        ret = IPCObjectStub::OnRemoteRequest(code, data, reply, option);
    }
    HiviewDFX::XCollie::GetInstance().CancelTimer(id);
    return ret;
}
```

**Proxy** — `powermgr_thermal_manager/services/zidl/src/thermal_level_callback_proxy.cpp:49`:
```cpp
bool ThermalLevelCallbackProxy::OnAsyncThermalLevelChanged(ThermalLevel level)
{
    sptr<IRemoteObject> remote = Remote();
    THERMAL_RETURN_IF_WITH_RET((remote == nullptr), false);
    MessageParcel data;
    MessageParcel reply;
    MessageOption option = { MessageOption::TF_ASYNC };
    if (!data.WriteInterfaceToken(ThermalLevelCallbackProxy::GetDescriptor())) { return false; }
    THERMAL_WRITE_PARCEL_WITH_RET(data, Int32, static_cast<int32_t>(level), false);
    int ret = remote->SendRequest(
        static_cast<int>(PowerMgr::ThermalLevelCallbackInterfaceCode::ASYNC_THERMAL_LEVEL_CHANGED),
        data, reply, option);
    if (ret != ERR_OK) { return false; }
    return true;
}
```

**Descriptor** — `powermgr_thermal_manager/interfaces/inner_api/native/include/ithermal_level_callback.h:32`:
```cpp
DECLARE_INTERFACE_DESCRIPTOR(u"ohos.powermgr.IThermalLevelCallback");
```

**What to validate**: if/else-if dispatch detection; handler name resolution
for `OnThermalLevelChangedStub` / `OnAsyncThermalLevelChangedStub`; enum
opcode resolution; detection of macro-wrapped SendRequest (`THERMAL_WRITE_PARCEL_WITH_RET`).

### Example 3: Callback interface (ability_dmsfwk)

**Stub holds proxy, calls back** — `ability_dmsfwk/services/dtbschedmgr/src/ability_connection_wrapper_stub.cpp:47`:
```cpp
int32_t AbilityConnectionWrapperStub::OnRemoteRequest(uint32_t code, MessageParcel& data,
    MessageParcel& reply, MessageOption& option)
{
    std::u16string descriptor = IAbilityConnection::GetDescriptor();
    std::u16string remoteDescriptor = data.ReadInterfaceToken();
    if (descriptor != remoteDescriptor) { return ERR_INVALID_STATE; }
    sptr<AppExecFwk::ElementName> element(data.ReadParcelable<AppExecFwk::ElementName>());
    if (element == nullptr) { return ERR_INVALID_VALUE; }
    int32_t resultCode = ERR_NONE;
    switch (code) {
        case IAbilityConnection::ON_ABILITY_CONNECT_DONE: {
            if (auto remoteObject = data.ReadRemoteObject()) {
                resultCode = data.ReadInt32();
                OnAbilityConnectDone(*element, remoteObject, resultCode);
                return ERR_NONE;
            }
            return ERR_INVALID_DATA;
        }
        case IAbilityConnection::ON_ABILITY_DISCONNECT_DONE: {
            resultCode = data.ReadInt32();
            OnAbilityDisconnectDone(*element, resultCode);
            return ERR_NONE;
        }
        default:
            return IPCObjectStub::OnRemoteRequest(code, data, reply, option);
    }
}

void AbilityConnectionWrapperStub::OnAbilityConnectDone(const AppExecFwk::ElementName& element,
    const sptr<IRemoteObject>& remoteObject, int32_t resultCode)
{
    if (distributedConnection_ == nullptr) { return; }
    // Wraps the stored IRemoteObject in a proxy and calls back through it
    auto proxy = std::make_unique<AbilityConnectionWrapperProxy>(distributedConnection_);
    proxy->OnAbilityConnectDone(element, remoteObject, resultCode);
}
```

**Target proxy** — `ability_dmsfwk/services/dtbschedmgr/src/ability_connection_wrapper_proxy.cpp:30`:
```cpp
// Writes interface token "ohos.abilityshell.DistributedConnection"
// Calls remote->SendRequest(IAbilityConnection::ON_ABILITY_CONNECT_DONE, ...)
```

**What to validate**: callback detection — stub stores `distributedConnection_`
(an `sptr<IRemoteObject>`), wraps in a proxy, and calls back through it;
reverse bridge: the stub's `OnAbilityConnectDone` → proxy's
`OnAbilityConnectDone` → `SendRequest(ON_ABILITY_CONNECT_DONE)`.

### Example 4: Large opcode set (ability_dmsfwk — IDistributedSched)

**Opcode enum** — `ability_dmsfwk/services/dtbschedmgr/include/distributedsched_ipc_interface_code.h:22`:
```cpp
enum class IDSchedInterfaceCode : uint32_t {
    START_REMOTE_ABILITY = 1,
    STOP_REMOTE_ABILITY = 3,
    START_EXTENSION_ABILITY = 5,
    STOP_EXTENSION_ABILITY = 7,
    // ... 30+ methods total
};
```

**What to validate**: large opcode sets (30+); `enum class` with explicit
starting value; detection at scale.

### Example 5: Macro-wrapped SendRequest (ability_dmsfwk)

**Macro** — `ability_dmsfwk/services/dtbabilitymgr/include/base/parcel_helper.h:78`:
```cpp
#define PARCEL_TRANSACT_SYNC_RET_INT(remote, code, data, reply) \
    do { \
        MessageOption option; \
        int32_t error = remote->SendRequest(code, data, reply, option); \
        if (error != ERR_NONE) { return error; } \
        int32_t result = reply.ReadInt32(); \
        return result; \
    } while (0)
```

**What to validate**: the proxy calls `SendRequest` through a macro — the
lowering pass must see through the macro expansion (preprocessor already
expands this, so the lowered IR should contain the direct `SendRequest` call).

### Example 6: IDL-generated interface (hiviewdfx_hiview)

**IDL source** — `hiviewdfx_hiview/plugins/faultlogger/framework/native/extension/zidl/IFaultLogExt.idl`:
```
callback interface IFaultLogExt {
    void OnFaultLogFault(int32_t faultType, [utf8] string log);
}
```

**What to validate**: IDL files are present in the repo; v2 can parse them
for exact method→opcode mapping. The generated stub/proxy (same as
Example 1) is the build-time output.

### Example 7: Multiple opcodes, multiple methods (multimedia_camera_framework)

**Stub** — `multimedia_camera_framework/services/camera_service/binder/server/src/hstream_capture_thumbnail_callback_stub.cpp:22`:
```cpp
int HStreamCaptureThumbnailCallbackStub::OnRemoteRequest(
    uint32_t code, MessageParcel &data, MessageParcel &reply, MessageOption &option)
{
    int errCode = -1;
    CHECK_RETURN_RET(data.ReadInterfaceToken() != GetDescriptor(), errCode);
    switch (code) {
        case static_cast<uint32_t>(
            StreamCaptureThumbnailCallbackInterfaceCode::CAMERA_STREAM_CAPTURE_ON_THUMBNAIL_AVAILABLE):
            errCode = HandleOnThumbnailAvailable(data);
            break;
        default:
            errCode = IPCObjectStub::OnRemoteRequest(code, data, reply, option);
            break;
    }
    return errCode;
}
```

**What to validate**: single-case switch with `static_cast<uint32_t>(enum)`
wrapper; handler call `HandleOnThumbnailAvailable(data)` that reads parcel
data.

## Design

### Approach: name-based matching (no control-flow analysis)

**Why not walk `switch`/`if` AST?**
- Control-flow lowering is complex, error-prone, and hard to maintain.
- AGENTS.md explicitly prohibits "Add flow-sensitive analysis without
  explicit design approval."
- Opcode resolution through `enum`/`#define`/`static_cast` chains adds
  fragile constant-evaluation logic that breaks on macros, templates,
  and build-config-dependent values.

**What we do instead:** match proxy methods to stub handlers by
**interface name + method name correspondence**. OpenHarmony's IPC
convention guarantees a predictable naming pattern:

| Proxy class | Stub class | Interface base |
|------------|------------|----------------|
| `FooProxy` | `FooStub` | `IFoo` |

Each proxy method `FooProxy::Bar(args)` corresponds to a stub handler
`FooStub::Bar(args)` (or `FooStub::HandleBar(args)`). The opcode
value is **not needed** for wiring the call edge — it's only relevant
for a future IDL-aware refinement.

### Phase 1: Proxy/stub detection (post-merge, in `trace-analysis`)

A new pass `detect_ipc_pairs(program)` runs after merge and during PAG
build (in `Pag::build_with_models`). It detects proxy and stub classes by
name patterns, then matches methods by name correspondence. It is **pure**
(reads `&Program`, returns `Vec<IpcBridge>`) so it requires no `Program`
mutation and no `&mut` churn across the ~90 `analyze(&program)` call sites.

#### Detection heuristics

**Stub detection** — a C++ class is a stub if its qualified class name
ends in `Stub`. Its handler methods are all of its defined methods except
constructors, `OnRemoteRequest`, `GetDescriptor`, and destructors. A class
with no handler methods is registered only when it defines `OnRemoteRequest`,
so a constructor-only `*Stub` class does not enable interface fallback.

**Proxy detection** — a C++ class is a proxy if its qualified class name
ends in `Proxy` or `Client`. Its methods count as IPC sends if their body
calls a function whose final callable name segment is exactly `SendRequest`;
qualified, `remote->SendRequest`, `remote.SendRequest`, and unqualified
spellings all match without admitting `SendRequest*` family names.

**Matching** — proxy methods are matched to stub handlers by:
1. **Interface name**: strip `Proxy`/`Client` suffix from proxy class →
   add `Stub` suffix → look up stub class (e.g. `FooProxy` → `FooStub`)
2. **Method name**: `FooProxy::Bar` → `FooStub::Bar` (exact name match)
3. **Fallbacks** (in order, only tried when the previous name is absent on
   the stub):
   - `Handle` prefix stripping — e.g. `FooProxy::EnableGwpAsanGrayscale` →
     `FooStub::HandleEnableGwpAsanGrayscale`
   - `Stub` suffix — e.g. proxy `OnFoo` → `FooStub::OnFooStub` (the
     marshalling-shim name some IDL generators use, e.g.
     `ThermalLevelCallbackStub::OnThermalLevelChangedStub`)

All overloads at the first matching naming tier are retained. Without opcode
or parcel-type analysis v1 cannot choose among them, so keeping the full set
preserves may-analysis soundness. Each proxy overload connects to every
retained handler overload: `m` proxy overloads and `n` handlers produce `m × n`
bridge edges.

When a stub only declares the matched interface method, detection first looks
for defined overrides in classes deriving from the stub. If none are indexed,
the bridge targets matching methods on the stub's transitive interface bases.
This includes inherited default method bodies, preserving their downstream
calls, as well as bodyless declarations that end at a leaf in call-graph
reachability. Interface bases include ordinary inheritance and the interface
argument preserved from an exact `IRemoteStub<IFoo>` base; the outer template
base remains in the ordinary inheritance graph for CHA. The argument and the
wrapper are resolved independently: in `svc::FooStub :
OHOS::IRemoteStub<IFoo>`, the argument is `svc::IFoo`, not `OHOS::IFoo`.
Relative qualified arguments use the same lexical search: in
`svc::FooStub : OHOS::IRemoteStub<api::IFoo>`, `svc::api::IFoo` is tried
before `::api::IFoo`; a leading `::` is required to force the latter.
Using recorded inheritance rather than namespace/name similarity prevents
unrelated same-namespace declarations from becoming bridge targets.

#### IR structures

Lowering preserves templated inheritance in
`Program.template_bases: Vec<TemplateBase>` (`trace-ir/src/program.rs`). Each
fact carries the derived class, base spelling, and declaration namespace so
analysis can resolve relative interface arguments in the correct lexical
scope.
The ordinary `Program.inheritance` edge continues to use the bare outer
template name for CHA.

Detected bridges use `trace-ir/src/ipc.rs`:

```rust
/// A matched proxy→stub IPC bridge (proxy method → stub handler).
#[derive(Debug, Clone)]
pub struct IpcBridge {
    pub proxy_method: FnId,
    pub stub_handler: FnId,
    pub descriptor: String, // for logging/diagnostics (v1: empty)
}
```

Bridges are stored in `Pag::ipc_bridges: Vec<IpcBridge>` (populated during
PAG build), not in `Program`, so the analysis entry point keeps its
`&Program` signature.

### Phase 2: Synthetic call edges (solver)

For each `IpcBridge`, the solver emits a synthetic `CallGraphEdge`:
```
proxy_method --[CallEdge { caller: proxy_method, callee: stub_handler,
                call_site: SYNTHETIC_CALL_SITE, resolution: IpcBridge }]--> stub_handler
```

This is done directly in the solver after the direct-call pass (before
`AnalysisResult` is built). It requires **no** new `FlowConstraint` variant
and **no** control-flow analysis. A bridge may target a defined stub handler,
a defined override in a class deriving from the stub, an inherited default
implementation, or (when no body is indexed) a bodyless interface method used
by a stub dispatcher. Argument flow
across the boundary is not wired in v1 (deferred to Phase 3).

Bridge edges use the dedicated `ResolutionKind::IpcBridge` (distinct from
`Direct`), exported as `resolution = 'ipc'`, so consumers can recognize and
optionally filter them. IPC detection is on by default; `--no-ipc` disables
it (`AnalyzeOptions.enable_ipc`).

**Synthetic call-site sentinel.** Synthetic edges carry
`call_site = SYNTHETIC_CALL_SITE = CallSiteId(u32::MAX)` (re-exported from
`solver.rs`) so they are distinguishable from every real (sequentially
allocated) call site. Consumers must not join synthetic edges to a real
`CallSite`:

- `extract_arg_flow` skips synthetic edges (no arg wiring in v1).
- The exporter stores `call_site_id = NULL` for them (and the edge's own
  `caller_fn_id` is written to the new `call_edges.caller_fn_id` column),
  so call-graph views use `ce.caller_fn_id` and render a synthetic edge with
  no source location rather than mis-attributing a real call site.

### Parameter marshalling (v1 — none)

Full `MessageParcel` read/write modeling is deferred. v1 creates the call
edge but does **not** wire parameters or return flow. This is still
valuable: the proxy→stub call edge appears in the call graph as an `ipc`
edge instead of the proxy resolving only to the opaque `SendRequest`.

For **callback interfaces** (reverse direction), the same name-based
matching works: the callback proxy class matches the callback stub class
by the same suffix rules.

### Integration points

| Stage | Change | File |
|-------|--------|------|
| IR | New `ipc.rs` module: `IpcBridge` type | `trace-ir/src/ipc.rs` (new) |
| IR | Re-export `ipc` module from `trace-ir` | `trace-ir/src/lib.rs` |
| Analysis | `Pag::ipc_bridges` field | `trace-analysis/src/pag.rs` |
| Analysis | `detect_ipc_pairs(program) -> Vec<IpcBridge>` (pure) | `trace-analysis/src/ipc.rs` (new) |
| Analysis | `Pag::build_with_models` calls `detect_ipc_pairs` | `trace-analysis/src/pag.rs` |
| Analysis | `ResolutionKind::IpcBridge` variant | `trace-analysis/src/constraints.rs` |
| Solver | Emits synthetic `CallGraphEdge` per bridge (`resolution: IpcBridge`) | `trace-analysis/src/solver.rs` |
| Solver | `AnalyzeOptions.enable_ipc` gates bridge emission (default on) | `trace-analysis/src/solver.rs` |
| Export | `resolution = 'ipc'`, `call_site_id = NULL`, `caller_fn_id` written | `trace-db/src/export.rs` |
| CLI | `--no-ipc` flag to disable detection (default on) | `trace-cli/src/main.rs` |
| Docs | This file + ANALYSIS.md + AGENTS.md update | `docs/` |

### Detection shapes handled

| Pattern | How it's detected | Notes |
|---------|-------------------|-------|
| Switch dispatch in stub | Not needed — stub detected by class name, handlers by class membership | Opcode resolution deferred to v2 |
| if/else-if dispatch in stub | Not needed — same as above | Same |
| Proxy `SendRequest` call | Proxy detected by class name + `SendRequest` presence in body | Confirms it's a proxy |
| Enum/macro opcodes | Not resolved in v1 — edge wired by name matching | Opcode resolution in v2 via IDL |
| Descriptor string | Optional: `GetDescriptor()` return or `DECLARE_INTERFACE_DESCRIPTOR` | Used for logging, not matching |
| Callback interfaces | Same name-based matching in reverse direction | Proxy class ↔ stub class |

## Fixture plan

All fixtures are synthetic. For real-world validation, run against the four
target repos (see "Real-world examples" section above and "Evaluation
targets" below).

| Fixture | Description | Mirrors real example |
|---------|-------------|---------------------|
| `ipc_basic/` | Hand-written proxy/stub, switch dispatch, one method | Example 1 (simplified) |
| `ipc_switch_multi/` | Switch with multiple opcodes, distinct handlers | Example 1 |
| `ipc_if_else/` | **if/else-if chain dispatch** | Example 2 |
| `ipc_enum_opcodes/` | Opcodes as `enum class` members, not literal ints | Examples 1, 2, 4 |
| `ipc_macro_opcodes/` | Opcodes as `#define` constants (synthetic — not found in repos) | No real match |
| `ipc_callback/` | Reverse callback: stub holds client proxy, calls back | Example 3 |
| `ipc_stub_suffix/` | Stub handlers named with a `Stub` suffix only | Example 2 (`OnThermalLevelChangedStub`) |
| `ipc_idl_generated/` | IDL-generated stub/proxy (simulated output) | Example 6 |
| `ipc_arg_flow/` | Parameter marshalling: WriteInt32/ReadInt32 positional matching | Example 1 proxy |
| `ipc_nested/` | Nested IPC: proxy calls stub which calls another proxy | — |
| `ipc_descriptor_match/` | Matching by interface descriptor string | Examples 1, 2 |

## Known imprecision (v1)

- No parameter marshalling — the call edge exists but argument flow across
  the IPC boundary is not wired (deferred to Phase 3).
- Name-based matching can produce false positives when a proxy/stub class
  name pattern collides with unrelated classes (e.g. a `*Proxy*` class that
  is not an IPC proxy). The `SendRequest` call presence check mitigates this.
- `HandleX` prefix-stripping fallback is heuristic — a `FooProxy::Bar`
  method may not map to `FooStub::HandleBar` if the stub uses a different
  naming convention.
- No support for `oneway` (async) IPC — treated as synchronous.
- Cross-process death notifications not modeled.
- No opcode-level verification — the bridge is wired purely by name. Two
  methods with the same name but different opcodes (rare) would collide.
- No IDL-aware matching in v1 — opcodes and exact stubs are in v2.
- Callback interface detection works only if the callback proxy/stub follow
  the same class-name suffix conventions.

## Evaluation targets

| Corpus | Expected improvement |
|--------|---------------------|
| `hiviewdfx_hiview` | HiviewServiceAbility, SysEventService, FaultLogger stub/proxy pairs |
| `multimedia_camera_framework` | ICameraService (128 opcodes), ICameraDeviceService, ICaptureSession |
| `powermgr_thermal_manager` | IThermalSrv (10 methods) + 3 callback interfaces |
| `ability_dmsfwk` | IDistributedSched (30+ methods), IDistributedAbilityManager, IDExtension |

Eval metric: count of `External` call edges that become resolved after
IPC bridge injection; arg-flow edges that cross the IPC boundary.

## Suggested sequence

```
Phase 1: IR infrastructure                                  [DONE]
  → IpcBridge type in trace-ir/src/ipc.rs
  → Re-export ipc module

Phase 2: Detection + matching + synthetic edges            [DONE]
  → detect_ipc_pairs() in trace-analysis/src/ipc.rs (pure, &Program)
  → Pag::ipc_bridges + Pag::build_with_models calls detect
  → Solver emits synthetic CallGraphEdge per bridge
  → SYNTHETIC_CALL_SITE sentinel (CallSiteId::MAX); extract_arg_flow skips
  → call_edges gained caller_fn_id; call_site_id nullable (NULL = synthetic)
  → inspect (crate + CLI) LEFT JOINs call_sites; renders synthetic edges
    with no source location (no mis-attribution)
  → Fixtures: ipc_basic/, ipc_if_else/, ipc_enum/, ipc_callback/, ipc_stub_suffix/
  → Integration tests: crates/trace-cli/tests/ipc_tests.rs
  → Validated on all 4 target repos (see "Status" below)

Phase 3: Parameter marshalling (optional enhancement)
  → Lowering records WriteXxx/ReadXxx order
  → PAG builds Copy constraints for positional mapping
  → Fixture: ipc_arg_flow/

Phase 4: Export + CLI                                      [PARTIAL]
  → call_edges export (`resolution = ipc`, NULL call_site_id) [DONE]
  → --no-ipc flag and docs                                  [DONE]
  → dedicated ipc_bridges SQLite table                      [DEFERRED]

Phase 5: IDL-aware matching (optional, v2)
  → Parse .idl files for exact interface definitions
  → Fixture: ipc_idl_generated/

Eval after each phase on the 4 target repositories.
```

## Status (implemented & validated)

**Implemented (Phases 1–2):** name-based proxy/stub detection and synthetic
call-edge injection. Synthetic edges use the `SYNTHETIC_CALL_SITE` sentinel;
the `call_edges` export stores them with `call_site_id = NULL` and a real
`caller_fn_id`, and both inspect paths (crate + CLI) render them with their
correct caller and no source location rather than mis-attributing a real call
site.

**Validation against real repos** (via `TRACE_DEBUG_IPC=1` on the release
binary):

| Repo | Bridges detected | Matches |
|------|-----------------|---------|
| `powermgr_thermal_manager` | 4 | `ThermalLevelCallbackProxy/Stub` (if/else), `ThermalTempCallback`, `ThermalActionCallback` |
| `hiviewdfx_hiview` | 9 | `FaultLoggerServiceProxy/Stub` — `HandleX` prefix matches (`EnableGwpAsanGrayscale` → `HandleEnableGwpAsanGrayscale`), plus 5 methods whose stubs expose interface-only handlers (defined derived overrides are preferred when indexed) |
| `ability_dmsfwk` | 2 | `AbilityConnectionWrapperProxy/Stub` (callback interface) |
| `multimedia_camera_framework` | 2 | `HStreamCaptureThumbnailCallbackProxy/Stub`, `HStreamCapturePhotoCallbackProxy/Stub` (single-case switch + `Handle` prefix) |

These counts are baselines for the checked source revisions, not expected
minimums. Generated ZIDL proxy/stub translation units are often absent from a
repository checkout (notably in the camera corpus), and detection requires
both sides to be indexed. Because v1 is name-based, an exactly paired
`MockFooProxy`/`MockFooStub` or `TestFooProxy`/`TestFooStub` can also match;
exclude test sources from the analysis root or use `--no-ipc` when that is not
desired.

**Fixture tests** (`cargo test -p trace-cli --test ipc_tests`): 10 tests, all
pass. Full `cargo test --workspace` (26 suites) remains green.

**Debugging aid:** `TRACE_DEBUG_IPC=1` prints each bridge as
`[ipc] bridge: <proxy> --> <stub>` plus a total. Debug logging is disabled by
default; IPC detection itself is enabled unless `--no-ipc` is passed.

**Remaining for full plan completion:** parameter marshalling (Phase 3), a
dedicated SQLite `ipc_bridges` table (optional Phase 4 follow-up), and
IDL-aware matching (Phase 5).

## AGENTS.md updates

Add to "Adding analysis constraints" checklist:

```
6. IPC bridge: Document in docs/IPC_ROADMAP.md
   - IpcBridge IR type (proxy_method → stub_handler, exact FnIds)
   - detect_ipc_pairs() — pure name-based detection (&Program → Vec<IpcBridge>)
   - Synthetic CallGraphEdge injected in the solver (no new FlowConstraint variant)
   - Detection by class-name + method-name correspondence — no switch/if-else
     AST walking (would require flow-sensitive analysis approval)
```

Add to "Invariants":

```
6. **IPC detection approach**: Proxy/stub pairs are detected from class name
   patterns (*Proxy*/Stub*) + SendRequest call presence. Bridge matching is
   by interface name + method name correspondence. Detection is pure (no
   control-flow analysis, no Program mutation); edges are injected in the
   solver. No opcode analysis (deferred to IDL-aware v2).
```

## Non-goals (v1)

- Cross-repository / cross-device IPC — this is why a single-tree corpus like
  `multimedia_camera_framework` reports only the in-tree callback pairs
  (`HStreamCapture*CallbackProxy/Stub`); service-face proxies (`HCameraProxy`,
  `CameraSceneSessionManagerProxy`, …) dispatch to stubs implemented by
  `HCameraService` / external `ISceneSessionManager` interfaces in another
  repo/process and cannot be paired without those roots, so they intentionally
  produce no bridge edge.
- Full `MessageParcel` read/write modeling
- Async (`oneway`) semantics
- Dynamic descriptor construction
- Distributed softbus transport
