# Pointer analysis

trace uses inclusion-based (Andersen-style) pointer analysis to resolve indirect calls and wire interprocedural argument flow.

## Properties

| Property | Value |
|----------|-------|
| Scope | Whole-program (all indexed `.c` TUs under target root) |
| Flow sensitivity | **None** (control-flow insensitive) |
| Field handling | Field-sensitive with **instance-insensitive field summaries** |
| Pointer analysis kind | **May-analysis** (sound over-approximation) |
| Context sensitivity | **None** |

## Workflow

```mermaid
flowchart TD
  Flow[IR flow constraints]
  Ret[fn_returns summaries]
  PAG[Pag::build]
  Idx[SolverIndices]
  WL[Worklist fixpoint]
  CG[On-the-fly call edges]
  IPC[IPC proxy/stub bridges]
  OUT[AnalysisResult call edges]
  AF[arg_flow extraction]

  Flow --> PAG
  Ret --> PAG
  PAG --> Idx --> WL
  WL --> CG
  CG --> WL
  CG --> OUT
  IPC --> OUT
  CG --> AF
```

1. **`Pag::build(program)`** — materialize PAG nodes/constraints from `program.flow`, expand `CallReturn` using `program.fn_returns`, attach indirect-call `Load`/`Copy` constraints.
2. **`solve`** — worklist propagation until fixpoint; discover indirect callees when call-target points-to gains function locations.
3. **`extract_arg_flow`** — emit `arg_flow_edges` for wired parameter copies at resolved calls.

## IR flow constraints (`trace-ir`)

Lowered from C during parse. Mapped to PAG in `Pag::build_flow_constraints`.

| Constraint | Meaning | C example |
|------------|---------|-----------|
| `Copy { dst, src }` | pointer assignment | `p = q` |
| `AddrOfVar { dst, src }` | address of variable | `p = &x` |
| `AddrOfFn { dst, callee }` | address of function | `p = handler` (fn ptr) |
| `Load { dst, src }` | load through pointer | `y = *p` |
| `Store { dst, src }` | store through pointer | `*p = y`, `field = val` |
| `GepField { dst, base, field }` | field address | `&obj.field`, `p->field` |
| `ArrayFnMember { array, callee }` | fn-ptr array init member | `{ fn0, fn1 }` |
| `CallReturn { dst, callee_name }` | `dst = callee()` | `p = GetOps()` |
| `CallReturnIndirect { dst, callee_var }` | `dst = *callee_var()` | `sbuf->impl->readBuffer(...)` (indirect return) |
| `NewHeap { dst }` | heap allocation | `new T(...)` (C++ ctor result) |
| `StringConst { dst, value }` | `dst` points at a string literal | `p = "target"`; `dlsym(h, "target")` |

### Return-value flow

Functions record abstract return values in `program.fn_returns`:

| `ReturnFlow` | Source |
|--------------|--------|
| `AddrOfVar { src }` | `return &global` / `return &file_static` |
| `AddrOfFn { callee }` | `return &Fn` / fn identifier in `&` expression |
| `Copy { src }` | `return local` or `return param` |
| `Call { callee_name }` | `return Other()` (transitive; `Other` resolved in callee's file) |

`return &local` is recorded as `AddrOfVar` but is **unsound** for stack locals (may-analysis may report escaped addresses). Prefer treating this as a known imprecision.

At PAG build time, `CallReturn` resolves `callee_name` with **`resolve_function_candidates(name, file)`** — every function the merged name may refer to: the query file's internal-linkage entries (`fn_by_scope`, declarations included) plus the canonical external definition. Name-based facts lose the calling TU's visibility context at merge time, so a name matching both a file-`static` def and an external def is genuinely ambiguous; per may-analysis semantics all candidates are expanded. Callee ids that survived lowering + merge (e.g. `AddrOfFn`) are used directly instead — they are exact.

This models patterns like:

```c
subDev->subDevOps = GetSensorDeviceOps();  // return &g_sensorDeviceOps
subDev.subDevOps->setConfig(subDev);
```

**`CallReturnIndirect`** is the indirect-call analogue of `CallReturn`. The callee is resolved by the solver when indirect call targets are known (via function-pointer analysis). The `callee_var` is a synthetic load variable that holds the resolved function pointer; the solver wires return flows from each resolved target into `dst`.

**`NewHeap`** represents C++ `new T(...)` allocations. The PAG allocates a heap location typed to the allocated struct and adds an `AddrOf` edge from `dst` to the heap location. The solver then propagates into the struct's fields, enabling resolution of function pointers stored by constructors (e.g., `MParcelImplInterfaceAssign` writing into `HdfSBufImpl.readBuffer`).

**`StringConst`** intern a C string literal as an abstract location (`LocKind::StringLit`). Assignments (`const char *n = "foo"`), copies, and call arguments intern the same way, so a later `dlsym(h, n)` still sees `"foo"`. Concatenated literals (`"ta" "rget"`) are folded. No `sprintf` / buffer writes.

### `dlsym` / `GetProcAddress`

Built-in models treat `dlsym` / `dlvsym` / `GetProcAddress` as **symbol lookup**: the return destination of a call (the `CallSite.return_dst` of `f = dlsym(...)`, including `return dlsym(...)` via a temp) may point to every **in-tree** function whose exact name matches a string constant in the name argument (parameter 1). Out-of-tree names add no pointees (true external). The handle / DSO path is ignored (whole-program search). Non-literal names that never receive a string constant stay unresolved — they do **not** fan out to every exported function.

## Program Assignment Graph (PAG)

### Node kinds

| `PagNodeKind` | Role |
|---------------|------|
| `Var(VarId)` | IR variable (local, param, global, synthetic temps) |
| `Loc(LocId)` | Abstract memory / function location |
| `CallTarget(CallSiteId)` | Synthetic node for indirect call resolution |

### PAG constraint kinds

| Kind | Semantics |
|------|-----------|
| `Copy` | `pts(dst) ⊇ pts(src)` |
| `AddrOf` | `pts(dst) ⊇ { loc }` |
| `Load` | for each `o ∈ pts(src)`: merge `memory_pts(o)` into `pts(dst)`; function locs copied directly |
| `Store` | for each `o ∈ pts(dst)`: merge `pts(src)` into `memory_pts(o)` and field summaries |
| `Gep` | field projection from base object locations (+ summary fallback) |

### Abstract location kinds

| `LocKind` | Description |
|-----------|-------------|
| `Global` | External/global variable |
| `FileStatic` | File-scope `static` |
| `FnStatic` | Function-local `static` |
| `Local` | Parameter or stack local storage |
| `Heap` | Reserved for allocator summaries (stub) |
| `Field` | Specific field at a known parent object location |
| `FieldSummary` | Instance-insensitive merge of struct field `T.f` across all instances |
| `ArraySummary` | Unknown-index array element summary |
| `Function` | Function entry address for indirect call targets |

### Lazy locations

**Global**, **file-scope `static`**, and **function-local `static`** variables receive `Loc` nodes eagerly at PAG build. Ordinary **locals** and **parameters** get locations **on demand** when referenced by `AddrOf`/`ensure_var_loc`.

## Solver

Worklist algorithm with **constraint adjacency index** (`SolverIndices`) for O(1) lookup of affected constraints per node.

### Work budget

Solving is capped at a deterministic **800 000 pops** by default. Normal corpora converge far below the cap (the HDF framework corpus needs ~42k). The cap trades late-stage target recall for bounded runtime. Override via `TRACE_SOLVE_BUDGET_POPS=<n>`; `=0` restores unlimited solving. The budget is deterministic, so repeated runs produce identical databases.

### State

| Map | Role |
|-----|------|
| `pts` | PAG node → set of abstract locations |
| `memory_pts` | Object location → set of stored pointer values |
| `loc_nodes` | Reverse index: location → PAG nodes that must be requeued on store |

### Propagation highlights

**`Gep` with empty base points-to**

When `pts(base)` is empty (typical for pointer parameters with no incoming flow), fall back to **`FieldSummary`** for `(struct_type(base), field)` via `ensure_field_summary_for_var`. This connects field stores through parameters to later field loads on unrelated instances (may-analysis).

The same fallback also fires when the base *has* pointees but none of them yielded a field cell — e.g. `void *` heap allocations or opaque summaries, where per-pointee `ensure_field_loc` synthesizes nothing. Without this, ops fields assigned through freshly-allocated objects starve every load site that reads them (observed as missing indirect-call edges for shared-obj style code).

**Stores to field summaries**

`apply_store` propagates into both concrete field locs and their `FieldSummary`, keeping summary memory in sync with instance stores.

**Signature-guarded function-value propagation**

Wrong-type pointer casts put unrelated objects into a pointer's points-to; a store through such a pointer would otherwise write callback addresses into alien layouts, where later field loads surface them as bogus indirect-call targets. The solver therefore filters **function values only** (all non-function flow stays unfiltered, preserving soundness):

- A fn value may enter `memory_pts[cell]` / a summary cell only when the cell's declared type accepts it: `FnPtr` slots require the same parameter count; concrete non-fn-pointer cells (`struct`, array, scalar-pointer, union) reject all fn values; unknown/untyped cells stay writable.
- The same guard applies when `merge_memory_into` lifts cell contents into points-to sets, and when a `Gep` passes fn values from the base node's set into the field node — except registered `array_fn_members` table members, which always pass (see "Arrays and function-pointer tables").

Consequence: callbacks stored through correctly-typed ops assignments resolve exactly as before, while cross-signature leaks (e.g. a 2-param `AddService` callback surfacing at 4-param `Dispatch` sites) are cut. Documented imprecision: old-style casts that stash fn pointers in `void *`-typed cells then call them through typed loads still work (unknown cells accept everything), but calls through cells whose declared type is structurally wrong for the stored fn are no longer reported.

**Indirect calls**

1. Each indirect call site gets a `CallTarget` node.
2. For field-path callees (`p->ops->fn`), lowering emits `Load`/`Copy` chain into a temp var; PAG connects `CallTarget` via `Copy` or `Load`.
3. When `pts(CallTarget)` gains a `Function` location, emit `CallGraphEdge` (resolution `indirect`), wire parameter `Copy` constraints, call `apply_call_summary`.

**Direct calls**

Sites lowering marked `is_direct = true` saw the TU-local binding, so scope-first **`resolve_function_in_scope(callee_name, call_site.file)`** is exact per C visibility rules: a file-`static` definition shadows same-name external functions inside its own TU (backed by the `fn_by_scope` index, which includes internal *declarations* — lowering streams a file top-down and initializers like `.Read = StaticFn` must bind before the definition is lowered).

Because header-defined functions are deduplicated to their header origin at merge time, `fn_by_scope` entries for them live under the header's `FileId`. Scope resolution therefore also consults **`headers_of(file)`** — the set of headers that contributed entities to a TU — so an includer still sees the header's internal-linkage definitions; TU-local definitions keep precedence on name collision.

**Cross-TU direct-call recovery**

A plain call whose definition lives in another TU is lowered with `is_direct = false` (the callee symbol is not visible in the calling TU). At solve time, sites that are *not* direct, have no `callee_var`, and whose callee text is a bare identifier are recovered as direct-by-name calls via `direct_by_name`, expanding **all** `resolve_function_candidates` (may-approximation — see `CallReturn` above). Without this, every cross-TU call to a function declared through a pointer-returning prototype (e.g. `T *f(void);`) would be dropped, because such prototypes previously also produced phantom variables — lowering now registers functions for pointer-wrapped declarators instead.

### Analyze options

```rust
pub struct AnalyzeOptions {
    pub retain_points_to: bool,  // CLI: --debug-points-to
    pub enable_ipc: bool,        // CLI: enabled unless --no-ipc
    // function models and solve budget omitted
}
```

When `retain_points_to` is false (default), points-to sets are discarded after solving to reduce memory.

### OpenHarmony IPC bridges

For an in-tree OpenHarmony proxy/stub pair, analysis adds a synthetic call
edge from a proxy method that calls `SendRequest` to the corresponding stub
handler. Classes are paired by their qualified `*Proxy`/`*Client` and `*Stub`
names; methods match exactly, with `HandleX` and `XStub` handler fallbacks.
For a stub with interface-only declarations, defined overrides in classes
deriving from that stub are preferred; otherwise matching methods on the
stub's transitive interface bases are retained, including default method
bodies and bodyless declarations. Interface bases are recovered from both
ordinary inheritance and the interface argument of an exact
`IRemoteStub<IFoo>` base. An unqualified interface argument is resolved in the
stub declaration namespace, independently of a qualified wrapper namespace;
a relative qualified argument such as `api::IFoo` searches the declaration
namespace and its enclosing namespaces, while only `::api::IFoo` forces global
lookup. Bodyless methods are intentionally leaf targets when no implementation
is indexed.
The edge has resolution `ipc` and no source call site, and can be disabled
with `--no-ipc`.
Because v1 has no opcode or parcel-type information, overloaded handlers at
the selected naming tier are all retained as a may-analysis result. With `m`
proxy overloads and `n` handler overloads this deliberately emits the full
`m × n` cross-product, so IPC edge counts can grow for overloaded interfaces.

This is deliberately name-based and does not interpret transaction opcodes or
control flow in `OnRemoteRequest`. Synthetic IPC edges are appended after the
points-to fixpoint and do not feed the solver worklist. The analysis also does
not model parcel argument or return-value flow. See
[IPC_ROADMAP.md](IPC_ROADMAP.md) for the pattern study, validation targets, and
limitations.

## Field sensitivity

- Struct fields have distinct `FieldId` entries in `TypeTable`.
- `GepField` in IR becomes PAG `Gep` with field id.
- **`FieldSummary`** locations unify all instances of `struct T.field` for sound may-analysis (e.g. vtable writes through a parameter pointer visible at unrelated call sites).
- Unknown or non-struct base → GEP may no-op.
- **Struct identity is per-TypeDesc**: types intern by full `(tag, fields)` equality. Field summaries and layouts of divergent copies diverge too (stores through one copy are invisible to loads through another), so lowering must produce *identical* descs for the same logical struct in every TU. In particular, a typedef'd anonymous struct (`typedef struct { .. } Alias;`) takes `Alias` as its tag — per-unit `anon_N` counters would otherwise split one shared-header type into several TypeIds after merge.

## Arrays and function-pointer tables

- **Constant index**: treated conservatively (element refinement is future work).
- **Unknown subscript**: `ArraySummary` — all elements merged.
- **`ArrayFnMember`**: each initializer function is merged into the array var's points-to; any subscript call may target **any** listed function.
- **Nested initializer lists** (`{ {TYPE, Fn}, ... }`): element expressions are visited recursively, so arrays of structs with fn-ptr members feed `ArrayFnMember` facts into the table var. Element fn values flow through field loads on the array itself *and* through pointers to elements (`m = &arr[i]; m->fn()`), regardless of worklist order.
- **Field-designated members** (`[i] = { .fn = Fn }`): lowered as precise
  `GepField`+`Store` chains against the array var (index-insensitive, like
  runtime element stores), so a member only feeds loads of the field it was
  written to. Purely positional nested lists still use the merged
  `ArrayFnMember` blob. Mixed forms where positional and designated members
  coexist in one element list keep the designated precision; bare positional
  members of such lists are not separately parked (rare; sound direction).
- **Initializer-less array declarations** (tentative definitions such as
  `static struct Ops g_tbl[4];`) register the variable like any other global;
  runtime stores into elements then resolve normally.
- **Positional struct initializers** (`static struct Ops o = { Fn, ... };`):
  each bare value is mapped to its declared field by position and lowered as
  the same precise `GepField`+`Store` chain designated members use — function
  addresses included. Position counting treats designated and bare members
  uniformly (C's reset-after-designator subtlety is not modeled; rare).

## Member subobject addressing

`&outer.member` lowers to a gep-temp chain targeting the member's own abstract
location, typed by the member's declared struct — not to a flattened address of
the outer instance. Field loads through such pointers resolve fields against
the member's type (`dev->service = &inst.service; ... service->Dispatch`
resolves `Dispatch`, not same-index members of the outer struct). Arrays of
structs peel to their element type for field resolution (`arr[i].field`).

## Indirect call resolution patterns

Supported lowering patterns include:

| Pattern | Example |
|---------|---------|
| Direct fn ptr var | `fp()` |
| Single field | `obj.handler()` |
| Multi-hop field | `p->ops->setIpAddr()` |
| Mixed `.` / `->` | `subDev.subDevOps->setConfig()` |
| Designated init | `.handler = &Fn` |

### External callees

Plain-identifier calls that resolve to no definition under the analyzed root
are classified as `external`, not left as unresolved indirect sites. Two
sources feed this class: prototype-only declarations (the callee resolves
statically but has no body here), and synthesized entries for names that are
never declared in the tree at all (libc without tree headers, logging
backends referenced only inside macros — `finalize_extern_callees`). Edges to
bodyless functions never carry param wiring unless the prototype declares
formals; unresolved fn-pointer sites (`ptr_expr` shapes) remain the only
occupants of the "no target" indirect bucket.
| Static ops struct | `g_ops = { .fn = Fn }` + `memcpy`-style assign via `SbufInterfaceAssign` (field store from global init) |
| Call return | `p->field = Getter()` |

## Argument flow

When a call edge is created (direct or indirect), actuals are connected to callee formals:

- **Pointer variables** → PAG `Copy` from actual var node to formal var node
- **Function identifiers** passed as fn-ptr args → `add_pts(formal, fn_loc)`

After fixpoint, `extract_arg_flow` records:

```
(call_site, arg_index, actual_var?, actual_fn?, formal_var)
```

Exactly one of `actual_var` or `actual_fn` is set per row. Only arguments that resolve to IR variables or function refs at the call site participate.

Return-value flow affects **points-to** (what a call expression assigns), not arg-flow formals.

### Flow-graph export

`export_flow_graph` (`trace-db/src/export.rs`) serializes the post-solve PAG
as `flow_nodes` / `flow_edges` for the `inspect dataflow` command:

- Constraint kinds map 1:1 to edge kinds `copy` / `addr_of` / `load` /
  `store` / `gep`.
- `points_to` edges are derived from the final var→location map.
- `call_arg` edges come from `extract_arg_flow` and are exported only when
  no stronger constraint already connects the actual/formal pair — this
  covers scalar (non-pointer) arguments that the solver does not persist as
  PAG constraints.

## Function models (configurable summaries)

Bodyless functions (libc, `_s`-family secure variants, vendor externs) contribute no
IR: calls to them produce call edges but no data flow. **Function models** close this
gap with declarative per-function summaries that relate parameters to each other.

Models are matched by function name at every resolved call site — direct, recovered
cross-TU, indirect, and external. A model applies regardless of whether the callee is
defined in-tree, so project-specific wrappers can be described too.

### Effect kinds

| Effect | Semantics (may) | PAG realization |
|--------|-----------------|-----------------|
| `alias { dst, src }` | `pts(param[dst]) ⊇ pts(param[src])` after the call | persistent `Copy(actual[dst] ← actual[src])` |
| `mem_copy { dst, src }` | contents of `*src` copied into `*dst` (memcpy family) | modeled as `alias` (see imprecision note) |
| `content_store { ptr, value }` | `*param[ptr] = param[value]` | persistent `Store(actual[ptr] ← actual[value])` |
| `return_alias { param }` | returned pointer may be `param[param]` | addr/copy edges into the `CallReturn` destination |
| `return_heap` | returns a fresh storage location | fresh `Heap` loc per call site into the destination |
| `clears { param }` | **terminator**: memory reachable via `param[param]` is zeroed by this call | no value introduction; terminator event exported |
| `dlsym { param }` | return value may be the address of the in-tree function named by string constants in `param[param]` | `Dlsym` PAG constraint; unknown names add nothing |

Effects attach to parameter positions (0-based) of the *actual arguments* recorded at
the call site. Arguments that are not IR variables or functions (literals like
`sizeof(...)` or `0`) simply do not participate.

### Terminators (`clears`)

A terminator states that the call **writes zeros** through a pointer parameter
(`memset(p, 0, n)` family). Semantics under may-analysis:

- The call introduces **no pointer values** through any modeled argument; data never
  flows *out of* the terminator's parameters into memory or return values.
- Kills are **not** modeled: the solver is flow-insensitive and inclusion-based
  (monotone); values stored before a memset still reach later loads. This mirrors the
  documented no-path-sensitivity stance — adding kills would be flow-sensitive
  refinement requiring explicit design approval.
- Every applied `clears` event is exported as a `terminator` flow node with a
  `terminates` edge from the cleared argument, so `trace inspect dataflow` shows where
  value chains are zeroed instead of silently stopping.

### Built-in models

Shipped in `trace-analysis/src/summaries.rs`; user configuration overrides same-name
entries:

| Function(s) | Effects |
|-------------|---------|
| `memcpy`, `memmove`, `strcpy`, `strncpy` | `mem_copy dst=0 src=1` |
| `memcpy_s`, `memmove_s`, `strcpy_s`, `strncpy_s` | `mem_copy dst=0 src=2` |
| `memset`, `memset_s` | `clears param=0` |
| `malloc`, `calloc`, `zalloc`, `kmalloc` | `return_heap` |
| `realloc` | `return_alias param=0`, `return_heap` |
| `dlsym`, `dlvsym`, `GetProcAddress` | `dlsym param=1` (symbol-name argument) |

### Configuration format

TOML, one `[model]` table per function; loaded via `--models <FILE>` (repeatable;
later files override earlier entries and built-ins):

```toml
version = 1

[[model]]
name = "SbufImplAssign"
effects = [
    { kind = "mem_copy", dst = 0, src = 1 },
]

[[model]]
name = "MyInit"
effects = [
    { kind = "content_store", ptr = 0, value = 1 },
]

[[model]]
name = "MyPoolAlloc"
effects = [ { kind = "return_heap" } ]

[[model]]
name = "MyDlsym"
effects = [ { kind = "dlsym", param = 1 } ]

# An explicitly empty effect list overrides (disables) a same-name built-in.
[[model]]
name = "memcpy"
effects = []
```

### Documented imprecision

- **`mem_copy` as aliasing**: true memcpy makes contents equal; modeling it as
  `pts(dst) ⊇ pts(src)` reproduces every field/value read through the destination
  (field cells and summaries of the source objects become visible), but stores through
  the destination may also land in source-side field cells (over-approximation, sound
  for may-analysis).
- **Member-address arguments are skipped**: lowering resolves `&base.member` to the
  base variable, so alias effects (`mem_copy`, `alias`) refuse to fire when either
  side is a member/array-element address — copying the whole container would pollute
  unrelated fields with the source's pointees. Such copies contribute no flow.
- `return_alias`/`return_heap` fire only when the callee has **no body** under the
  analyzed root; defined functions keep their exact return flow.
- Terminators kill nothing (see above).

## C++ support (first step)

`.cpp/.cc/.cxx/C++` files are indexed as TUs and parsed with tree-sitter-cpp
(`SourceLang` per TU; headers inherit the including TU's grammar). Lowering is
C++-aware only where it must be — everything else reuses the C machinery.

- **Namespaces**: `ns_stack` qualifies declarations (`ns::f`). Anonymous
  namespaces get internal linkage. Unqualified C++ calls resolve through
  **namespace-aware ordinary lookup**: the global namespace, enclosing
  namespaces (innermost to outermost), plus every namespace brought in by
  `using namespace X;`. **ADL** (argument-dependent / Koenig lookup):
  namespaces are derived from the qualified tag of each argument's
  `Struct`/`Union` type (pointer/array layers peeled) and merged into the
  candidate set, so `swap(a, b)` where `a` is `kit::Widget*` finds
  `kit::swap`. **`using X::f;`** imports the exact qualified function name
  into the candidate set of the bare base name. Relative `using`
  targets are expanded against the enclosing namespace stack:
  `using namespace detail;` inside `namespace a` records `a::detail`
  **and** the literal `detail` (`expand_using_target`), matching C++'s
  first-segment resolution; exact global lookups still win when the
  enclosing spelling does not exist. Header prototypes
  (`lower_function_decl`) are now namespace-qualified like definitions
  so they register under their correct `ns::f` spelling. `using`
  directives are **block-scoped**: file-scope directives apply TU-wide,
  but a directive written inside a function body, a namespace block, or a
  nested statement block (`if`/`for`/`while` body — itself a
  `compound_statement`) applies only to that block
  (length-snapshot/restore in `lower_function`, `lower_namespace`, and
  around each `compound_statement` in `walk_function_body`). Leaking them
  block-wide could let the overload ranking collapse away the correct
  in-scope edge, and is avoided.
- **Overloads**: same-name entries are kept apart when **both** sides are C++
  and arity (or same-arity param types) differ (`add_function`;
  `externals_by_name` bucket). Signature comparison uses real types: at TU
  merge the incoming params are remapped into global `TypeId` space by
  `merge_unit_index` and passed via `add_function_with_param_types`, so a
  cross-TU prototype + definition of the same function collapse into one
  record instead of duplicating (two `functions` rows / two callgraph edges),
  while distinct same-arity overloads still separate across TUs. The merge
  gate resolves the surviving entry's types through `Function::param_type_ids`
  (remapped into global type space at merge; unit-local VarIds are not
  queryable mid-merge). A C `.c`
  body still merges with a C++-parsed `.h` prototype of the same arity —
  otherwise callers bind to the undefined prototype (HDF `GpioSetIrq` /
  `gpio->func`). Calls resolve over the candidate set filtered by argument
  count; an empty arity-filtered set falls back to all candidates (varargs).
  Ties emit one direct site per candidate. Same-arity C++ overloads
  additionally rank by **static argument type**: `CallArgs.arg_desc`
  carries each argument's `TypeDesc` (casts unwrapped, numeric literal
  width, `char`, `true`/`false`, string, `nullptr`, plus var/field/subscript
  declared types); a unique exact param-type match wins, otherwise the
  full arity set is kept (may-approx).
- **Template member calls**: `obj.GetNumber<int>()` parses its method slot
  as `template_method`; both it and `template_type` route to the primary
  name, so `fv.GetNumber<int>()` / `b.read<short>()` resolve directly.
  In-class template methods (`template_declaration` members in a class
  body) register as prototypes and lower their inner `function_definition`.
- **Classes**: layouts intern under the fully qualified tag
  (`gfx::Shape`). Inheritance facts (`Program.inheritance`) drive member
  resolution: a call walks upward to the nearest declaring base. **Non-virtual**
  methods resolve exactly to that declaring function; **`virtual` methods and
  destructors** additionally expand downward through the subclass closure
  (one site per target — delete-through-base is the dominant dtor pattern).
  Expansion runs again **after TU merge** so overrides declared later in the
  same file or in other TUs are included. Downward expansion is rooted at the
  **static receiver type**. Targets are filtered by **explicit arity**
  (`params` minus implicit `this`; empty parameter lists stay, and an empty
  filtered set falls back to all candidates so varargs still resolve). A
  `final` class, or a method declared `final`,
  cuts off further subclasses (devirtualization). C++ `struct` inheritance
  and **virtual bases** (`class D : virtual B`) are recorded the same way as
  ordinary bases for CHA (diamond override sets include the most-derived
  override). When no ancestor declares the member, the call falls back to
  the receiver's static-type subclass closure.
- **Implicit `this->method()`**: a bare identifier call inside a method
  (`OnEvent()` from `OnEventProxy`) is rewritten as a member call on the
  enclosing class when that class (or a base) declares the method. This
  runs before free-function name lookup so it does not synthesize an
  unqualified external stub.
- **Field receivers**: a bare identifier used as a member-call receiver
  (`plugin_->OnEvent()` inside a method) is looked up as a data member of
  the enclosing class (and bases) when it is not a local/param, so
  `shared_ptr<Plugin>` fields unwrap like parameters.
- **Smart pointers**: `std::shared_ptr<T>` / `unique_ptr` / `weak_ptr`
  intern as `Ptr(Struct{T})`, so `p->method` types as `T`. Nested pointer
  layers (`T &`, `T *`) are peeled for the same reason.
- **Callables**: only `std::function<Sig>` / `::std::function<Sig>` intern
  as `FnPtr` so assignment and field stores of function addresses
  participate in indirect-call resolution. Other types whose last segment
  is `function` stay ordinary classes. Lambdas lower to synthetic
  `$lambda` functions with `AddrOfFn` on init/assign/arg (captures
  unmodeled). Functors (`operator()`) are member calls, including `obj()`
  and `h->field()` when `field`'s type declares `operator()`. Callable
  data members that are not methods fall through to the C fn-ptr
  field-load path.
- **Methods**: out-of-class definitions (`Ret Cls::m()`) merge with their
  in-class prototypes. An implicit `this` parameter (`Ptr(Struct{Cls})`,
  param index 0) is prepended. `virtual` flags survive merges.
- **Conversion operators**: `operator T()` is a member named
  `Cls::operator T`, spelled from its `operator_cast` declarator: the name
  runs to the declarator's own parameter list, so the target type keeps its
  pointer and reference layers (`operator const char*`, `operator T*`,
  `operator void(*)`) and its template arguments (`operator Vec<int>`) while
  the `()` and cv-qualifiers are dropped. Of the target's own scopes, only
  those the *member itself* sits in are dropped — any contiguous run of the
  enclosing segments, so `T`, `H::T`, `b::H::T` and `a::b::H::T` are one
  member for a class `a::b::H`, and each qualified name in the target takes
  the longest run that applies to it, a template argument's differing from
  the head's: `ns::Handle::operator ns::S`
  and the in-class `operator S` written inside `namespace ns` are one member,
  since how far the author had to qualify the type is an accident of where
  the declaration sits — but a scope the member does not sit in could not
  have been elided at either spelling, so it is kept and
  `C::operator a::S` stays distinct from `C::operator b::S`. A leading `::`
  follows the same rule: dropped when what follows re-spells a scope the
  member sits in (`operator ::ns::S` for a member of `ns`), kept otherwise,
  since it is all that separates a global type from one the member's own
  namespace shadows (`operator ::S` beside `operator S` inside a
  `namespace n` that declares its own `S`). That question is settled by the
  top-level target alone — a template argument shedding its own scope says
  nothing about whether the `::` in front was redundant. A `::` in the
  target therefore never qualifies the member out of its class either.
  Declaration, in-class definition and out-of-class definition
  (`Cls::operator T()`) all merge, and a definition returns the type it
  converts to — a function-pointer target lowering to `Ptr(FnPtr{..})`, the
  same descriptor the `typedef`ed spelling of it produces — though see the
  prototype note below for when that survives the merge. One limit follows
  from naming the member after the *spelling* of its target type: two
  spellings of one type are two members (`operator const char*` from the
  declaration and `operator char const*` from an out-of-class definition do
  not merge). The recorded *type* is the coarser of the two in one place: a
  function-pointer target's `FnPtr` carries no parameters, so
  `operator int(*)(char)` and `operator int(*)(long)` — neither valid C++
  without a typedef — are two members by name, since the name runs to the
  *member's* own parameter list and so keeps the target's, but both record
  `Ptr(FnPtr{Int, params: []})`. A conversion to a reference to an
  array (`operator int (&())[3]`) is the one target the name cannot spell: it
  is the only one whose declarator wraps *around* the parameter list rather
  than ending at it, so the name is cut mid-spelling as
  `operator int(&`. Self-consistent, so declaration and definition still
  merge, but not a spelling anyone would recognise.
- **Attribute macros around a conversion operator**: an unknown macro
  (no `#define` in the include path) takes the `type` field and leaves the
  `operator` keyword stranded in an `ERROR`, so the target type stands where
  the declared name belongs. Both shapes are read back to the ordinary
  spelling: in a class body `MACRO operator ns::S() const;` is
  `Cls::operator ns::S`, not the member `Cls::S` it used to be; out of one,
  `EXPORT Cls::operator int() {}` keeps its `Cls::` instead of losing it to
  the fabricated-qualification repair below and escaping to global scope.
  The target is read from the *source text*, not by walking the declarator
  the `ERROR` parks it in: a declarator yields the one identifier it is named
  by, which is a target's last segment only, and every other part of the
  spelling — `ns::`, `<int>`, `(*)` — decides which member it is. The
  spelling runs from the keyword to the end of the declarator the member's
  own parameter list hangs off, which is contiguous with whatever scope the
  `ERROR` swallowed however it split the two. So `MACRO operator Vec<int>()
  const GUARDED_BY(m);` is `Cls::operator Vec<int>` and
  `MACRO operator int (*)() const;` is `Cls::operator int(*)`, each meeting
  the unannotated declaration and the out-of-class definition of the same
  member; every target kind spells the same under a leading macro, a trailing
  one, or both.
  A pointer or reference target recovers differently from the rest — the
  operator keeps a whole `function_declarator`, the member's `;` goes
  *missing*, and a trailing macro is parked after it as a `declaration` of its
  own. A class-body `declaration` following a member closed by a missing `;`
  is the tail of that member (the author wrote no `;` between them), so it
  declares nothing; registering it gave the class an undefined
  `Cls::GUARDED_BY` that call sites on any annotated member resolved to. A
  genuinely separate member after a missing `;` recovers as a
  `field_declaration`, not a `declaration`, so the rule does not reach it; the
  one shape it does swallow is a *ctor* declaration after a member whose `;` the
  author actually forgot (`void f()` then `C(int);`), which is not valid C++
  either way.
  What tells the two repairs apart is where the `ERROR` sits relative to the
  `::`: before it for a fabricated qualification (the leftover type is the
  scope), after it for a real one. Both are looked for at every level of the
  `qualified_identifier` chain, not just the top: a qualified name nests one
  level per scope it carries, so each scope either half spells pushes the
  recovery's mark one level deeper (`FFI_EXPORT n::q::S A::B::M()` parks its
  `A` three levels down).
- **Ctors / dtors**: emitted for `new Cls(...)`, destructor calls on
  `delete p`, explicit qualified dtor calls, constructor-declarations with
  an argument list, ctor-initializer lists (base + member targets).
- **References** lower as pointers (aliasing stores land on caller memory).
- **Templates**: lowered once per primary name; `<...>` arguments stripped.

Known C++ imprecision (in addition to the general list below):

- Lambda **captures** are unmodeled (including `[this]`); the lambda body
  still participates in the call graph as a nested function.
- `auto` from a call (`auto p = wp.lock()`) stays `Unknown`; there is no
  return-type inference, so member calls on such pointers do not unwrap.
- `std::bind` / generic functors without a visible `operator()` stay
  unresolved-indirect unless a function address flows into them.
- Default construction without parens (`Cls o;`) emits no ctor site.
- **No call site resolves to a conversion operator**: `Cls::operator T` is
  indexed, but neither the compiler-inserted call of an implicit conversion
  nor an explicit `h.operator T()` reaches it — the explicit spelling parses
  as a field access whose `operator` keyword lands in an `ERROR` node, so it
  interns a phantom `Cls::T` instead. Conversion operators therefore have no
  incoming edges.
- **A prototype's return type wins the merge**: a member declared in a class
  and defined out of line records the prototype's placeholder `void`, not the
  definition's real type. This is general to all member functions (`int
  Cls::f()` defined out of line records `void` too), so a conversion
  operator's target type survives as its return type only when the class has
  no separate declaration.
- **A data member annotated by a macro that takes arguments is indexed as a
  function**: `int a_ GUARDED_BY(mu_);` parses as a member named `a_` in an
  `ERROR` beside a `function_declarator` spelling `GUARDED_BY(mu_)`, and only
  the latter looks like a declarator, so the class gains one undefined
  `Cls::GUARDED_BY` and the fields themselves are not indexed. A class that
  annotates several fields alike contributes one such phantom, not one per
  field, and a real method of that name would merge into it. Not separable
  from a *function* behind a leading macro (`MACRO int Plain() const;`), which
  is the same shape — type field, `ERROR`, `function_declarator` — with the
  halves meaning the opposite things; suppressing one drops the other, and
  dropping real methods is the worse trade.
- **A globally-qualified conversion target behind a leading macro escapes the
  repair**: `MACRO operator ::ns::S() const;` is the one shape whose `ERROR`
  lands at *class-body* level rather than inside the member — the macro and the
  `operator` keyword go into it together and the target becomes a
  `declaration` beside it — so the member walk, which runs per member, never
  sees the keyword. The declaration is indexed as `C::S`, and with a trailing
  macro as well the `ERROR` swallows the whole operator and the macro is left
  as the only `declaration`, giving the phantom `C::GUARDED_BY`. Every other
  target kind spells the same under a leading macro, a trailing one, both, or
  none; this one needs a repair at the class-body level, where nothing today
  reads recovery marks. Pinned as the one exclusion in
  `every_conversion_target_kind_spells_one_member_under_any_macro`.
- **A target type shadowed by an inner scope collapses onto the outer one**:
  the target drops the scopes the member sits in, which is right until two
  types of that name exist at different depths of those scopes. `operator S`
  and `operator N::S` in a class in `N` that also contains an inner `N::N::S`,
  or `operator ns::S` beside `operator ns::Handle::S`, come out one member.
  Telling them apart needs real name lookup, not a spelling rule.
- **A pointer-returning member behind a leading macro is still named after
  its return type**: `MACRO int (*Get(long))(int);` leaves a declarator-shaped
  leftover type in the `ERROR`, so the rule that reads only the `ERROR`'s
  declarators picks it rather than `Get`. The plain
  `MACRO int Get(long);` shape is handled.
- **A definition wearing a macro on both sides is lost**:
  `EXPORT void C::M() GUARDED_BY(m) { }` splits at the top level into a
  `declaration` holding `C::M` and a *separate* `function_definition` whose
  declarator is the trailing macro and which owns the body. `C::M` is
  therefore indexed undefined and its body attributed to a defined global
  `GUARDED_BY`. Either macro alone is handled; only the pair defeats it,
  because the repair the two need lives in different nodes.
- Objects at namespace scope emit no ctor/dtor sites (no enclosing function).
- Anonymous-namespace overload ties degrade to first-wins.
- **ADL namespace derivation is spelling-based**: only arguments whose
  `Struct`/`Union` tag carries an explicit `::` in the source contribute
  their namespace; enum-typed arguments and types referenced by a *bare*
  spelling (`using kit::Widget;` then `Widget a`) contribute nothing, so a
  call that relies purely on such types' ADL can degrade to an external
  stub (sound — never a wrong binding). A leading global-scope `::` in a
  tag (`::kit::Widget`) is stripped before deriving the namespace, matching
  how `functions_in_namespace` treats `kit` and `::kit` as interchangeable.
- Relative `using`/`using namespace` targets expand to every enclosing
  namespace the spelling could denote (over-approximation); genuine C++
  picks the innermost *declared* namespace only.
- Namespace **hiding/ordering is not modeled** for overload ranking:
  `rank_overloads` collapses a same-arity set to the best static-type
  match without preferring a candidate from the innermost enclosing
  namespace over an exact global match. May-analysis keeps all *tied*
  candidates (one direct edge per candidate), but a strictly-better-
  ranking leaked candidate would be preferred even where C++ scoping
  would hide it.
- Overload resolution is arity + static-type ranked; a non-exact argument
  (conversion, `auto`, unknown type) still keeps the whole arity set, and
  0-arg member-call overloads resolve through the primary-name entry only.
- Scalar type identity is coarse: `unsigned`/`signed` collapse to `Int`,
  `signed long long`→`LongLong`, `long double`→`Double`; distinct
  same-arity overloads relying on those distinctions collapse.
  Unnamed parameters (`void foo(int)`) still occupy a slot so
  `foo(int)` and `foo(int, int)` stay distinct; `void f(void)` does not.
- Template specializations collapse into the primary entry; no
  dependent-type modeling.
- Virtual expansion is CHA from the static receiver type (not points-to).
  Multiple bases resolve; nearest declarer wins when walking up.
  `override` implies virtual. `final` on a class or method stops further
  subclass targets. Virtual inheritance is recorded as a normal base edge.
- Headers shared between `.c` and `.cpp` TUs parse under whichever
  grammar reaches them first at merge time.
- **`using namespace` in headers**: ANALYSIS.md says file-scope directives
  apply TU-wide, but with header IR merged symbols-only, a `using namespace
  std;` written in a header is never seen while lowering the TU that includes
  it, so unqualified calls in the TU that depend on it degrade to external
  stubs. Sound but imprecise.
- **`using namespace` across namespace reopenings**: a using-directive at
  namespace scope stays in effect in later reopenings of the same namespace
  (`namespace A { using namespace B; }` then `namespace A { … }` finds
  `B::f`). Lowering truncates directives at namespace block exit, so
  reopenings lose the candidate. OHOS code reopens `OHOS::X` blocks often;
  this is an under-approximation. Sound for may-analysis (candidates may be
  missed but never wrongly added).

Next slices (hiview-grounded): [docs/CPP_ROADMAP.md](CPP_ROADMAP.md).

## Known imprecision

- All paths merged; no null-check refinement.
- `free` does not invalidate pointers.
- `FieldSummary` may connect unrelated struct instances.
- Multiple vtable/ops targets reported for one indirect site (may-analysis).
- **Casts of struct instances to another ops type** (`svc = (IOps *)&inst`):
  the whole instance flows into the target-typed slot, so field loads on it
  resolve against the *outer* layout plus its type-matched summary — sibling
  fields at colliding positional indexes can cross into such loads (observed
  as ~2% of Dispatch-site edges on HDF test drivers).
- **Signature-guarded propagation drops cross-signature fn values** (see
  "Signature-guarded function-value propagation"): calls through cells whose
  declared fn-pointer arity mismatches the stored function are not reported.
  Sites whose only reachable "targets" arrived via such wrong-type flow now
  report none (e.g. stub-side `super->X` calls behind an opaque IPC
  boundary in HDF — their baseline targets were cross-object pollution, not
  real resolutions).
- **IPC / process boundaries are only partly modeled**: OpenHarmony
  `SendRequest` proxy methods connect to same-tree, conventionally named stub
  handlers through synthetic `ipc` call edges. Opcode dispatch, parcel flow,
  cross-repository services, and `HdfRemoteServiceObtain`-style registrations
  remain unmodeled.
- **`memcpy` / `memmove`**: modeled through function models (see above);
  unmodeled copier names remain invisible.
- Macro-generated identifiers may be skipped when classified as macro-like callees.
- Function pointer resolution is name/linkage based. **`dlsym` / `GetProcAddress`**: a string constant in the name argument (literal or a variable that receives one) is looked up among indexed functions of that exact name; missing symbols stay unresolved. `dlopen` that only runs `REGISTER` static constructors is the C3 factory path, not modeled here. The DSO handle is not used to restrict candidates. `sprintf` into a name buffer is unmodeled.

## Performance notes

Whole-program HDF-scale runs (~600 TUs, ~11k functions) target roughly:

| Phase | Typical |
|-------|---------|
| Index | ~25s (parallel preprocess + parse) |
| Analyze | ~0.3s |
| Export (minimal) | ~0.1s |

Key optimizations: solver adjacency index, `loc_nodes` reverse index, worklist dedup, lazy abstract locations, minimal SQLite export, skipped redundant header indexing.
