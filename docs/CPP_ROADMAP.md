# Future C++ support (hiview-driven)

First-step C++ is enough for **typed** virtual dispatch
(`Plugin::OnEventProxy` → plugin `OnEvent` overrides) and same-class
unqualified calls. The remaining holes on `hiviewdfx_hiview` are not
missing keywords (`virtual` / `final`); they are **type recovery**,
**deferred callables**, **string/map factories**, and **`dlopen`/`dlsym`**.
This plan is ordered by recovered in-tree call-graph edges, not by C++
standard chapter.

Corpus: `~/hiviewdfx_hiview`. Re-eval command:

```
cargo run -p trace-cli --release -- analyze ~/hiviewdfx_hiview -o /tmp/hiview.db --jobs 8
```

Do not chase STL noise (`std::string::c_str`, `parcel->WriteString`,
`resultSet->Close`) or OHOS SDK (`FileUtil`, `TimeUtil`, binder
`remote->SendRequest`) until the hubs below close.

## Already working (do not re-plan)

| Capability | Hiview evidence |
|------------|-----------------|
| CHA from static receiver + implicit `this` | `Plugin::OnEventProxy` → 22 plugin `OnEvent` bodies |
| Smart-pointer / `operator->` unwrap, by declaration not by name (C12); an undeclared wrapper guessed from its one class argument (#86) | `call_sp` fixture; typed `Plugin*` paths; hdf `AutoPtr<T>` (+4,551 direct edges); camera `sptr<T>` (+16,587 direct edges) |
| `final` / virtual bases | fixtures only (`cpp_dispatch`); hiview barely uses them |
| Direct `std::function` field store | `cpp_callable`; **not** the factory path (C3) |
| `$lambda` as a nested function | 357 interned; almost none have **incoming** edges |
| ADL (Koenig lookup) | `swap(a, b)` with `kit::Widget*` args → `kit::swap`; namespaces derived from `Struct` `TypeDesc` tag |
| `using namespace X;` in free-function resolution | unqualified calls now search every namespace brought in by a `using namespace` directive |
| `using X::f;` member import | `using lib::bump;` introduces `bump(c)` into the candidate set of the bare name `bump` |
| Header prototypes namespace-qualified | `lower_function_decl` now applies `qualify_decl` so header prototypes register under `ns::f`, not a bare name |
| Namespace-relative bare call | bare `clamp()` inside `namespace a::b` finds `a::b::clamp` via enclosing-namespace walk |
| Conversion operators (`operator T()`) | indexed as `Cls::operator T` — declaration, in-class definition and out-of-class definition all merge, and the definition returns the type it converts to (#46) |
| Members behind unknown attribute macros | a macro with no `#define` in the include path no longer supplies the name, whether it leads (`FFI_EXPORT T f()`, `MACRO operator ns::S()`), trails (`void C::M() OVERRIDE {}`) or flanks (`EXPORT_API int Get(long) GUARDED_BY(mu_)`) the declarator (#46) |
| Members behind standard attributes | `[[deprecated]]`, `[[gnu::pure]]`, `__attribute__((pure))` no longer supply the name — camera's `CameraInput` had collapsed all 26 annotated members into one `CameraInput::deprecated` (#46) |

---

## C1 — Return-type inference (`auto`, `lock()`, `make_shared`)

**Why first:** Unblocks the pipeline pump and most remaining `ptr->…`
sites. CHA already works once the receiver is `Plugin` / `EventLoop`.

**Hiview:** `base/pipeline.cpp:47-64`

```cpp
std::weak_ptr<Plugin> plugin = processors_.front();
if (auto pluginPtr = plugin.lock()) {
    if (auto workLoop = pluginPtr->GetWorkLoop()) {
        workLoop->AddEvent(pluginPtr, shared_from_this());
    } else {
        pluginPtr->OnEventProxy(shared_from_this());
    }
}
```

`GetWorkLoop()` is declared `std::shared_ptr<EventLoop>` in `plugin.h`;
`lock()` is `shared_ptr<Plugin>`. Both are hidden behind `auto`, so the
receiver stays `Unknown` and `OnEventProxy` / `AddEvent` get **0**
targets (eval H5).

Same shape:

- `SysEventDispatcher::DispatchEvent` — `auto ptr = dispatcher.lock(); ptr->OnEventListeningCallback(...)` (`plugins/sys_dispatcher/sys_dispatcher.cpp:45-48`)
- `if (auto workLoop = pluginPtr->GetWorkLoop())` — if-init from a typed method, still `auto`

**Design sketch:** On `Var x = call`, copy the callee’s interned return
type onto `x` (`CallReturn` already exists for pointer flow; this is
the **type** analogue). Special-case `shared_ptr`/`weak_ptr`/`unique_ptr`
methods `lock` / `get` / `release` as identity unwraps to `T`.
`std::make_shared<T>(…)` / `make_unique<T>` intern as `Ptr(Struct{T})`.

**Eval when done:** `pluginPtr->OnEventProxy` in `OnContinue` has the
same CHA fan-out as typed `Plugin::OnEventProxy`. `workLoop->AddEvent`
resolves to `EventLoop::AddEvent`.

**Fixture:** `auto p = wp.lock(); p->OnEventProxy();` next to today’s
explicit `shared_ptr<Plugin> p`.

---

## C2 — Casts and template down-casts

**Hiview:** `base/include/event.h:201-205` and every plugin that handles
sys-events:

```cpp
template <typename Derived>
static std::shared_ptr<Derived> DownCastTo(std::shared_ptr<Event> event)
{
    return std::static_pointer_cast<Derived>(event);
};
```

Callers: `Event::DownCastTo<SysEvent>(event)` then `sysEvent->SetEventValue`
(`sys_dispatcher.cpp:63`, `event_logger`, `usage_fold_event_report.cpp:73`).
Eval still exports `Event::DownCastTo` as **external** and leaves
`sysEvent->SetEventValue` unresolved.

Also: `std::dynamic_pointer_cast<Event>(sysEvent)` before named posting
(`service/hiview_service.cpp:411-413`);
`std::static_pointer_cast<PluginProxy>(plugin)` after `make_shared`
(`core/hiview_platform.cpp:384`).

`Event::Repack<Base, Derived>` (`event.h:207-218`) does `new Derived(*base)`
and `event.reset(derived)` — a typed heap replace the PAG should treat as
“this `Event*` may be `Derived`”.

**Design sketch:** Instantiate `DownCastTo<T>` / `static_pointer_cast<T>` /
`dynamic_pointer_cast<T>` as copy + result type `Ptr(Struct{T})` (may-cast:
keep the source type in pts as well). Do **not** require RTTI.

**Eval when done:** `SysEventDispatcher::Convert2SysEvent` → `SysEvent`
methods; `sysEvent->SetEventValue` is a direct in-tree edge where the
method exists.

---

## C3 — Plugin factory (`std::function` + `unordered_map` + `REGISTER`)

Eval **H7**. Three stacked gaps; fixing only intern-as-`FnPtr` is not enough.

1. **Ctor-init store.** `PluginRegistInfo(GetObject, …)` copies a factory
   into `getPluginObject` (`base/include/plugin_factory.h:28-33`). Lower
   `std::function` construction / assignment from a function designator as
   `AddrOfFn`.
2. **Map summary.** `RegisterPlugin` inserts into
   `unordered_map<string, shared_ptr<PluginRegistInfo>>`;
   `GetGlobalPluginInfo` does `find` + `it->second`. Model map values as
   an **array/field summary** (same may-analysis as C fn-ptr tables): every
   inserted `PluginRegistInfo` is a possible `find` result.
3. **Static `REGISTER` macro.** Expands to a namespace-scope
   `PluginRegister` whose ctor calls `RegisterPlugin`
   (`plugin_factory.h:59-71`). Namespace-scope ctors currently emit **no**
   sites. Either emit a synthetic `$static_init` function per TU or treat
   `REGISTER(ClassName)` as “`ClassName::GetObject` may flow to every
   `getPluginObject()` call”.

**Eval when done:** `PluginFactory::GetPlugin` →
`RegisterEventLogger::GetObject`, `RegisterSysEventStore::GetObject`, …
(over-approx all registered plugins). `CreatePlugin` in
`hiview_platform.cpp:386` likewise.

**Fixture:** map or `RegisterPlugin(name, PluginRegistInfo(MakeFoo))` +
`info->getPluginObject()` — not a bare `w->cb = target`.

---

## C4 — Deferred callables (`std::bind`, `packaged_task`, `ffrt::submit`)

Hiview’s second dispatch axis: **do this later on a work loop / ffrt
queue**. Lambdas exist as `$lambda` but creators have **0 incoming
edges**; `std::bind` is opaque.

| Site | Pattern |
|------|---------|
| `base/plugin.cpp:93` | `std::bind(&Plugin::OnEventProxy, this, event)` then `AddTimerEvent` |
| `base/event_loop.cpp:191-199` | `bind` → `packaged_task<bool()>` stored on `LoopEvent` |
| `base/event_loop.cpp:502-504` | `event.packagedTask->operator()()` |
| `base/event_loop.cpp:494-498` | `event.task()` **or** `handler->OnEventProxy` (already CHA-able if `handler` is typed) |
| `passthrough_monitor.cpp:80` | `ffrt::submit([bundleName=…] { LoadCompleteReporter::ReportAudioStart(...); })` |
| `uc_telemetry_callback.cpp:187` | `[callback = shared_from_this()] { callback->RunTraceOnTimeTask(); }` |
| `platform_monitor.cpp:451` | `std::bind(&PlatformMonitor::CollectPerfProfiler, this)` |

**Design sketch:**

- `std::bind(&Cls::m, recv, args…)` → `AddrOfFn(Cls::m)` into the
  destination callable (ignore bound args for v1; may over-approx).
- `ffrt::submit(F)` / `std::thread(F)` summaries: if `F` is a `$lambda`
  or `FnPtr` slot, emit a **direct/indirect** edge from the submit site
  to that function (async, same as a call).
- Lambda **captures**: `[this]`, `[callback = shared_from_this()]` copy
  the receiver into the lambda’s implicit `this` / first synthetic
  field so body member calls type.
- `std::packaged_task<Sig>` intern as `FnPtr` like `std::function`.

**Eval when done:** `Plugin::DelayProcessEvent` reaches `OnEventProxy`;
`EventLoop::ProcessEvent` packaged path reaches `EventHandler::OnEventProxy`;
a sample `ffrt::submit` lambda (`FaultLogDatabase::SaveFaultLogInfo` or
passthrough_monitor) has an edge **from the submitter**.

---

## C5 — Named plugin lookup (`pluginMap_["XPower"]`)

**Hiview:** `HiviewPlatform::PostAsyncEventToTarget`
(`core/hiview_platform.cpp:655-677`):

```cpp
auto it = pluginMap_.find(calleeName);
auto callee = it->second;
auto workLoop = callee->GetWorkLoop();
workLoop->AddEvent(callee, event);
```

Callers pass a **string literal** (`"XPower"` in
`service/hiview_service.cpp:413`). `pluginMap_` is filled in
`CreatePlugin` (`hiview_platform.cpp:406`). Same map: `GetPluginInfo`,
`InstancePluginByProxy`.

This is C fn-ptr table dispatch with a `std::string` key. May-analysis:
every `shared_ptr<Plugin>` stored in `pluginMap_` is a possible
`find` result (CHA on `Plugin` already fans out `OnEventProxy`).
Optional later: constant-string key sensitivity.

**Eval when done:** `PostAsyncEventToTarget` → `AddEvent` /
`OnEventProxy` on in-tree plugins (over-approx). Literal `"XPower"`
narrowing is extra credit.

---

## C6 — `PluginProxy` lazy load

`REGISTER_PROXY` plugins are a `PluginProxy` shell.
`LoadPluginIfNeed` (`base/plugin_proxy.cpp:85-103`):

```cpp
plugin_ = GetHiviewContext()->InstancePluginByProxy(shared_from_this());
// then plugin_->OnEvent / OnEventListeningCallback
```

`InstancePluginByProxy` (`hiview_platform.cpp:873`) calls
`registInfo->getPluginObject()` (depends on C3). Until the real class
flows into `plugin_`, proxy methods stop at the shell.

**Eval when done:** a `REGISTER_PROXY` plugin (e.g. freeze detector)
`PluginProxy::OnEvent` reaches the concrete `::OnEvent` as well as CHA
from `Plugin`.

---

## C7 — Map / vector of callables (handlers, parsers)

Not the plugin factory: **ad-hoc dispatch tables**.

- `it->second(data)` after `TEST_CONTENT.find(action)`
  (`test/plugins/test_plugin/test_plugin.cpp:90`)
- `OhosXperfEvent* event = parser(msg)` from
  `map<int32_t, ParserXperfFunc>` (`xperf_dispatcher.cpp:54`)
- Fault dump / formatter command maps (`faultlog_dump.cpp`,
  `faultlog_formatter.cpp`)

Same PAG pattern as C fn-ptr arrays + C3 map summary. Prefer one
**associative-container value summary** used by C3, C5, and C7.

**Eval when done:** xperf `parser(msg)` resolves to registered
`ParserXperfFunc` entries (over-approx).

---

## C8 — Singleton / fluent `GetInstance()`

`TraceStateMachine::GetInstance().OpenTrace(...)`,
`HiviewPlatform::GetInstance().PostAsyncEventToTarget(...)`,
`XperfRegisterManager::GetInstance().PostEvent(...)`.

Often the chain is typed (`HiviewPlatform::GetInstance()` returns
`HiviewPlatform&`). Failures are `auto &platform = …GetInstance()`
(C1) or SDK singletons outside the tree. In-tree: intern
`GetInstance` return as `Ptr(Struct{Cls})` from the method’s return
type (C1 covers this if returns are tracked).

**Eval when done:** `hiview_service.cpp` `GetInstance().OpenTrace` is a
direct edge to `TraceStateMachine::OpenTrace` when that class is in-tree.

---

## C9 — Template member calls (`GetNumber<T>`)

**Status: implemented.** `fieldNum->GetNumber<uint64_t>()` now resolves.
`tree-sitter-cpp` parses the method slot of a call like
`obj.GetNumber<int>()` as `template_method` (not `field_identifier`), so
the member-call matcher had to accept it; `strip_template_args` /
`normalize_qualified` already produce the primary name `GetNumber`.
In-class template methods parse as `template_declaration` members inside
the class body and are now registered as prototypes and lowered (their
`function_definition` is unwrapped). Fixture
`tests/fixtures/cpp_templates_overloads`.

**Eval result:** those sites are direct `FieldValue::GetNumber` /
`Box::read`, not unresolved-indirect.

### Overload resolution by scalar type (part of this slice)

Same-arity overloads with distinct scalar parameter types stay distinct
end-to-end:

- `TypeDesc` gained `Bool`/`Short`/`LongLong`/`Float`/`Double` (previously
  everything collapsed to `Int`/`Long`), with `ScalarKind` and full
  layout/export support. Imprecisions: `unsigned`/`signed` collapse to
  `Int`, `signed long long`→`LongLong`, `long double`→`Double`.
- Cross-TU merge (`merge_unit_index` → `add_function_with_param_types`)
  re-adds every function with *remapped* param TypeIds, but the incoming
  `Function.params` still hold unit-local VarIds while merging, so a
  same-unit predecessor's type probe came back `None` and the gate fell
  back to arity-only — collapsing `f(double)` into `f(int)` during the
  merge pass. Fixed with `Function::param_type_ids: Vec<TypeId>`: the
  gate resolves the existing side via `param_type_ids` first, new entries
  carry the remapped signature via `pending_param_type_ids` into
  `push_indexed`. All `Function { … }` literals set it to `Vec::new()`.
- Call sites rank same-arity candidates: `CallArgs.arg_desc` carries the
  static `TypeDesc` of each argument (casts unwrapped, numeric literal
  width, char, `true`/`false`, string, `nullptr`, and var/field/subscript
  types resolved); a single exact match (score 0) is preferred, ties /
  unknowns fall back to the full arity set (may-approx).

**Known limitation:** 0-arg member overload calls (e.g. the 1-arg vs
0-arg `GetNumber`) resolve through the `functions_named` primary only, so
they emit one edge; 1-arg member calls resolve exactly via the generic
path (`GetNumber(7L)` → the `long` overload). Not a regression.

---

## C10 — Header grammar: first TU wins

Headers included from both `.c` and `.cpp` keep the grammar of whichever
TU is merged first. A C parse of `plugin.h` / `event.h` drops
`template`, `std::function`, and `class` bodies; later C++ TUs then
merge against a hollow type.

Hiview is almost all C++; HDF is mixed. Fix: parse a header with the
**C++ grammar if any including TU is C++**, or keep per-language views
and merge layouts.

**Eval when done:** `Plugin` / `Event` layouts interned even when a
stray `.c` TU includes the same header (HDF interop). Fixture: `.c` +
`.cpp` both include a header with `class Plugin { virtual void OnEvent(); }`.

---

## C11 — `dlopen` / `dlsym` (and `GetProcAddress`)

**Landed** as a POSIX function model (`Effect::Dlsym`) next to `memcpy`
summaries. String literals (and variables that receive them) in the name
argument become `AddrOf` of matching in-tree functions; unknown names add
nothing. Fixture: `tests/fixtures/dlsym/`. Handle / DSO path is ignored.
ELF parsing of prebuilt `.so` files remains out of scope.

Today function-pointer resolution is name/linkage inside the indexed
tree. `dlsym` is explicitly unmodeled ([ANALYSIS.md](ANALYSIS.md)
imprecision). Both corpora use it as a **second factory**, next to
`REGISTER` / ops tables.

**Two hiview shapes:**

1. **`dlopen` only, symbols via static ctors.** `LoadModule` is
   `dlopen(..., RTLD_GLOBAL)` (`base/utility/dynamic_module.cpp:27-33`).
   `HiviewPlatform::LoadDynamicPlugin` / `LoadPluginBundle` then rely on
   `REGISTER` constructors inside the `.so` to fill
   `PluginFactory` (`core/hiview_platform.cpp:291-324`). If those `.cpp`
   files are already TUs, this is **C3** (namespace-scope init) plus
   “`dlopen` may run that DSO’s `$static_init`”. No `dlsym` of the
   plugin class itself.

2. **`dlsym` of a named export, then call through the pointer.**

```cpp
// graphic_memory_collector_impl.cpp:47-59
void *handler = dlopen("libucollection_graphic.z.so", RTLD_LAZY);
auto getInterface = reinterpret_cast<GraphicMemoryCollector *(*)()>(
    dlsym(handler, "GetInstance"));
graphCollectorInstance = getInterface();
graphCollectorInstance->GetGraphicUsage(...);
```

Same idea: `CallDllFunc(module, "Init")` via `dlsym(module, funcName)`
(`hiretrieval_dynamic_loader.cpp:63-75`);
`DynamicLibraryHandle::GetSymbol` (`dynamic_library_handle.cpp:39-44`).

**HDF (same feature, C):** `dlsym(g_libHandle, "SbufObtainIpc")` in
`hdf_sbuf.c` is how the C++ IPC backend is wired; also
`dlsym(handle, DRIVER_DESC)` / `hdfVdiDesc` in the host loader, and
HDI `dlsym(..., constructor)` in `hdi_support.cpp`. The in-tree
`HdfSbufReadBuffer` eval (2 targets) only works when that store is a
**compile-time** `&SbufObtainIpc`. The production `dlsym` path is
invisible.

**Design sketch (may-analysis):**

- Summary `dlsym(handle, name)`: if `name` is a **string literal** (or
  `const char*` folded to one), resolve like an external lookup of that
  symbol among indexed TUs (`GetInstance`, `SbufObtainIpc`, `Init`, …)
  and `AddrOfFn` into the destination. Non-literal names over-approx
  **exported** functions of matching arity, or stay unresolved.
- `dlopen("libfoo.so")` optionally restricts the candidate set to
  files that would link into that DSO (needs a later mapping; v1 can
  ignore the handle and search the whole program).
- Out-of-tree `.so` (no matching TU) → true **external**, same as
  today’s libc stubs. Do not invent callees.
- Windows: `GetProcAddress` as the same summary.

This is **not** C++-specific; land it as a libc/POSIX model next to
`memcpy` summaries, with C++ casts/`auto` (C1) so
`getInterface()`’s return type can be `GraphicMemoryCollector*`.

**Eval when done:**

- Hiview: `GraphicMemoryCollectorImpl::GetGraphicUsage` indirect-calls
  in-tree `GetInstance` if that symbol is indexed; `CallDllFunc(..., "Init")`
  → `Init` when present.
- HDF: `dlsym(..., "SbufObtainIpc")` reaches `SbufObtainIpc` the same
  way the fixture `cpp_extern_c_driver` does via a direct call.

**Fixture:** `void *h = dlopen("x.so", RTLD_LAZY); auto f =
dlsym(h, "target"); ((int(*)())f)();` with `extern "C" int target();`
defined in the same tree.

**Out of scope for v1:** parsing ELF/dynamic-export tables of
prebuilt `.so` files that have no source under the target root.

---

## C12 — Member access through `operator->` (#64)

**Status: implemented.** `x->m()` used to unwrap the receiver through a
hardcoded list of three names (`shared_ptr` / `unique_ptr` / `weak_ptr`).
A wrapper called anything else did not unwrap: the receiver kept the
*wrapper's* class and a phantom member was invented on it. That is worse
than a miss — `sptr::AddOutput` is an edge to an undefined external
function, and nothing downstream can tell it from a real call out of tree.
A listed wrapper had the opposite problem: it was unwrapped on the *type*,
so `sp.reset()` bound to the pointee's `reset` and `sp.get()` became a
phantom `Pointee::get`.

Hiview is `std::shared_ptr`-based, so this never surfaced there. Camera is
`sptr`-based: **14,338** `sptr<...>` occurrences, led by
`sptr<CaptureOutput>` (1,514), `sptr<CameraInput>` (1,073),
`sptr<CameraDevice>` (1,022), `sptr<CaptureSession>` (523).

The receiver is now resolved through the **declared** `operator->`:

- A wrapper's instantiation keeps the wrapper as its class, with the
  arguments in its tag (`sptr<CaptureSession>`). `.` is the wrapper's own
  member; `->` and `*` are the pointee's.
- What an `operator->` returns is recorded as a fact when it is declared.
  For a class template returning a parameter (`T *`) the fact is the
  parameter's position, and the call site substitutes the instantiation's
  argument at it — `sptr<T>`, `RefPtr<T>` and HDI `AutoPtr<T>` need no
  list entry. A named class is followed on through a chain up to eight
  links deep, cycles cut; a raw pointer ends the chain at its pointee.
- The facts travel with a header's *types*, which is what a header unit
  contributes to the units that include it, so a wrapper-typed field
  declared in a different header from the wrapper resolves too.
- For a template wrapper whose **body is not in the tree**, `->` may also
  use its sole argument when that argument is a declared class (#86).
  This covers `sptr<T>`, `wptr<T>` and namespace-qualified spellings on
  locals, parameters and fields, with the ordinary class-hierarchy
  dispatch rules. Multiple arguments, scalar/pointer/reference arguments
  and unknown classes do not qualify; rejected arrows stay unresolved
  instead of inventing a member on the wrapper. The argument is looked up
  through the enclosing namespaces innermost first and then the global
  scope, on bare and partially qualified spellings, a typedef standing for
  the class it names and a trailing `const` ignored, and a leading `::`
  naming the global class only. `(*sp).m()` on such a wrapper is the same
  guess as `sp->m()`. A nested type of an out-of-tree template
  (`Outer<A>::Inner`, an iterator) is not a wrapper around `A`: its arrow
  stays unresolved. Scalar and function-type arguments are spelled as
  written, never qualified to a namespace; the wrapper's own name is looked
  up through the enclosing namespaces like an argument. A typedef is
  matched by its whole spelling. An out-of-line `operator->` without its
  class header still records what the arrow returns. A field step follows
  its operator: `sp->f` reaches the pointee's field through a wrapper, on
  the wrapper variable itself as well as along a field chain, while `w.f`
  stays the wrapper's own field, and a raw `Wrapper<T>*` uses the built-in
  arrow, so `p->f` stays on the wrapper's own layout. At an overloaded
  arrow the remaining path restarts on a receiver typed as the pointee, so
  its field step lands on the pointee's instance-insensitive summary --
  the one a raw `T*` read or write already uses -- rather than on a
  subobject of the wrapper the solver would never resolve. A member type
  keeps its `::` and its own name when it carries arguments
  (`Outer<A>::Inner<B>`), and a member type of a defined template keeps
  the class the lookup found as its prefix. A
  wrapper spelled `::W<T>` is the global `W`, tagged without the prefix,
  wherever a head is looked up. `nullptr_t`, `intmax_t`, `uintmax_t`,
  `auto` and a literal argument are never qualified; `T*const` is the
  pointer argument `T*`, and every pointer level survives a qualifier
  between levels. A C++11 `using Alias = T;` is not
  lowered as a typedef yet; an argument spelled through one stays
  unresolved.
- Class declarations and definitions are tracked separately from inferred
  type tags and travel through header merges. Merely mentioning an
  external type cannot enable the guess; a forward declaration of the
  wrapper says nothing about its `operator->` and does not disable it. A
  wrapper with a body continues through its `operator->`; the fallback
  never overrides one. Dot calls retain the wrapper's identity. The
  standard-pointer name fallback remains, including for minimal
  standard-library stubs and out-of-tree pointees.
- This is a may-analysis approximation for partial source trees, not proof
  that an absent wrapper implements `operator->`. Requiring exactly one
  declared class argument bounds the guess; declaration lookup uses hash
  sets rather than scanning symbols at each call. Explicit dereferencing
  (`*p`) follows the same rule as `->`: a declared operator, the
  standard-name fallback, or the one-argument guess on an out-of-tree
  wrapper.
- Where nothing names a class — overloads that disagree, a dependent return
  the index cannot name (`sptr<T>` inside a template, a parameter of a base
  template), a cycle, an iterator's `operator*` — the site is left
  unresolved. No member is invented on the wrapper.

Not modelled: the implicit call to each `operator->` (only the terminal
member call is emitted), an `operator*` other than a smart pointer's, and a
reference member holding a wrapper (it lowers as a pointer and is read as
one).

**Relationship to C1:** complementary, not a duplicate. C1 infers the type
of a *variable* (`auto`, `lock()`, `make_shared`); this resolves *member
lookup* through a wrapper whose type is already known. Camera needs this
one, hiview needs C1.

**Precondition for supplying `refbase.h`.** Camera does not declare `sptr`
anywhere in the tree today, so its 27,474 unresolved indirect sites were
honest misses until #86 started guessing through the undeclared wrapper.
The moment the missing include is supplied they would all have become
phantom `sptr::*` edges; with C12 in first, they resolve — and the
declared `operator->` takes over from the guess the moment it appears.

**Eval result (#86, `master` 769f2e8 → this change, same machine, `--jobs
8`):** camera direct edges **21,059 → 37,646**, external functions
**6,373 → 5,353**, the **254** invented `OHOS::sptr::*` members gone (a
probe pins them at 0). Compared by site and callee, 17,739 direct edges
are gained and 1,152 lost, 777 of the 808 losing sites carrying the same
call under a proper qualification or the real member: an unqualified
`sptr<T>` lowered to an unknown type before, so `cameraInput_->Open()`
bound by bare name to a free `Open` some fuzzer defined. The argument is
looked up through the enclosing namespaces and the global scope, on bare
and partially qualified spellings, through typedefs and past a trailing
`const`; `using namespace` directives are not searched (see the report
for why). hdf moves inside its bands with every exact metric unchanged —
its last `OHOS::sptr::*` phantoms become real edges. hiview moves for
that and for a second fault the review exposed: a C++17
`namespace A::B {` was lowered as an anonymous namespace, so its classes
registered under bare names; fixed, and hiview's external functions fall
**3,623 → 3,191** as bare prototypes fold into their definitions.
Type-table growth from a tag per instantiation is paid for by a
clone-free tag lookup in `TypeTable::intern` and a dense id remap in the
unit merge; all three corpora are level with or faster than `master`.
Residue: 31 camera sites call through a wrapper whose argument class is
not in the tree and stay unresolved. See
[EVAL_REPORT.md](EVAL_REPORT.md).

**Eval result (`master` 52fd920 → this change, same machine, `--jobs 8`):**
HDI's in-tree `OHOS::HDI::AutoPtr<T>` is exactly this shape, so hdf moves —
direct edges **37,632 → 42,183** (4,551 distinct edges added, none lost
when compared by file, line, column, caller and callee), external edges
**29,853 → 28,854**, external functions **2,499 → 2,403**, the **118**
invented `AutoPtr::*` members among them gone while all **36** direct calls
to the real `AutoPtr::Get` stay. Defined functions, indirect edges and
diagnostics do not move. Spot-checked against the sources:
`method->GetName()` on the `const AutoPtr<ASTMethod> &` parameter of
`CClientProxyCodeEmitter::EmitProxyMethodImpl`
(`codegen/c_client_proxy_code_emitter.cpp:271,275`) reaches the in-tree
`ASTMethod::GetName` where it was an external `AutoPtr::GetName`.

hiview and camera declare no wrapper in the tree, so every `->` takes the
path it took before: direct edges are unchanged in hiview and gain two in
camera, indirect edges and diagnostics are unchanged. What moves is the
`.` side of the name fallback — `sp.get()`, `wp.lock()`, `up.release()`
are external calls on `std::shared_ptr::get`, `std::weak_ptr::lock`,
`std::unique_ptr::release` now, not phantoms on the pointee (`Event::get`,
`Plugin::lock`), and a wrapper's construction is a constructor call on the
wrapper. See [EVAL_REPORT.md](EVAL_REPORT.md) for the full comparison.
Fixture `tests/fixtures/cpp_smart_ptr`.


## Later / skip for this corpus

| Item | Why deferred |
|------|----------------|
| Type-based overload ranking | Hiview dispatch is virtual / fn-ptr, not `add(int)` vs `add(double)` |
| Default arguments | `Event::Repack(..., replace=true)` — arity fallback already fires |
| Exceptions | No CG recovery |
| `std::variant` / `optional` | Return-type (C1) covers most `optional` uses |
| `std::call_once` | One-shot init (`usage_fold_event_report.cpp:60`); body is a lambda (C4) |
| Placement `new` | Rare; raw-pointer alias |
| Binder `remote->SendRequest` | Out-of-tree IPC; opcode tables if we ever analyze IDL stubs |
| Coroutines / concepts | Absent |

---

## Suggested sequence

```
C1 type-of-call / lock / make_shared
  → C2 DownCastTo / pointer_cast
    → C4 bind + ffrt::submit + lambda captures
      → C3 REGISTER / map / std::function ctor   (needs C4-style AddrOfFn)
        → C5 pluginMap_ named lookup
          → C6 PluginProxy
            → C7 other maps (share C3 summary)
C8 GetInstance          (mostly falls out of C1)
C9 template-method names (cheap, independent)
C10 header grammar      (foundational; do when HDF mixed headers bit)
C11 dlopen/dlsym        (POSIX summary; also HDF SbufObtainIpc / driver_loader)
```

C1+C2 close eval **H5** and the `sysEvent->` rain. C3+C5+C6 close **H7**
and named `PostAsyncEventToTarget`. C4 wires the event loop and ffrt
so recovered plugins are actually *invoked* from timers and queues.
C11 is the remaining factory when the constructor lives in a DSO
(`dlsym("GetInstance")`, HDF `SbufObtainIpc`). `dlopen` of in-tree
plugins without `dlsym` is C3 (static `REGISTER` ctors).

## Fixture checklist (add with each slice)

| Slice | Fixture sketch |
|-------|----------------|
| C1 | `auto p = wp.lock(); p->OnEventProxy();` + `auto l = p->GetWorkLoop(); l->AddEvent(...)` |
| C2 | `Event::DownCastTo<SysEvent>(e); sys->SetEventValue(...)` |
| C3 | `REGISTER`-like static `PluginRegistInfo(MakeFoo)` + `info->getPluginObject()` |
| C4 | `std::bind(&Plugin::OnEventProxy, this, e)` stored and invoked; `ffrt::submit` stub summary |
| C5 | `map<string, shared_ptr<Plugin>>` + `find("Foo")->second->OnEventProxy()` |
| C6 | Proxy field filled by factory, then `proxy->OnEvent()` |
| C7 | `map<int, int(*)(Msg*)>` + `parser(msg)` |
| C9 | `obj.GetNumber<uint64_t>()` → `GetNumber`; overlay done — see `tests/fixtures/cpp_templates_overloads` |
| C10 | `.c` includes `class` header, `.cpp` defines methods; virtual call still CHA |
| C11 | `dlsym(h, "target")` then call; `extern "C" int target()` in-tree |
| C12 | `sptr<T>` / `RefPtr<T>` / absent `shared_ptr<T>`, a wrapper chain, a cyclic one, `(*sp).m()`, `sp.reset()` — see `tests/fixtures/cpp_smart_ptr` |
| ADL | `swap(a, b)` with `kit::Widget*` args → `kit::swap`; `using namespace/util::helper`; `using lib::bump`; `a::b::go()` → `a::b::clamp` — see `tests/fixtures/cpp_name_lookup` |
