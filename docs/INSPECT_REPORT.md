# `trace inspect` Report (callgraph + dataflow)

**Date:** 2026-08-28  
**Binary:** `target/release/trace` (current tree; `cargo build --release` before the run, `cargo test --workspace` green)  
**Re-verified 2026-08-28 (after the C++ overload/template slice):** every probe below was re-run on the new binary and is byte-identical; `cargo test --workspace` is green (25 suites, incl. 32 C++ cases + 3 new `cpp_templates_overloads` regression tests). New in this revision: Case 20 (scalar-type overload ranking at call sites) and Case 21 (template member calls) in §1.2.

**Re-verified 2026-09-02 (variadic-macro preprocessor slice):** every probe below was re-run on the current binary; outputs are identical to master's, so the variadic changes alter no probe. Three blocks were refreshed for drift already present on master since 2026-08-28: the two `tie` overload records plus `Base::base_value` in Case 16, the depth-1 truncation note in Case 18, and the dependent `T` callee under `Box::read` in Case 20.

**Re-verified 2026-09-02 (object-macro `(` classification fix, #6/#7):** every fixture directory under `tests/fixtures/` (67 directories) was analyzed with the pre-fix and post-fix binaries and the two SQLite exports compared as full `.dump`s; all 67 are byte-identical, so every probe below is unchanged. No fixture used by this report defines a macro whose body starts with `(`; the new `preproc/object_macro_paren.c` fixture is covered by `cargo test`, not by a probe here.

**Re-verified 2026-09-02 (file-scoped conditional fence, #8):** every fixture directory under `tests/fixtures/` (69 directories) was analyzed with the master (`24e093f`) and post-fix binaries and the two SQLite exports compared as full `.dump`s. **68 are byte-identical**, so every probe below is unchanged. The one that differs is `preproc`, which now carries this change's fixtures — `unterminated_if_include.c` + `unterminated_if_header.h` and `stray_closer_include.c` + `stray_{endif,else,elif}.h`: the master binary blanks the includer after the header's open `#if 0` (no `main`) and loses `tail` once a header's stray `#endif` has consumed the includer's frame, while the fixed binary keeps `main`, `survived` and `tail`. The `unterminated #if` error itself is a preprocessor diagnostic, and `trace analyze` did not export those at the time (the index path kept only the preprocessed text, the LineMap and the included headers; fixed by #20, see the 2026-09-03 block below), so it was asserted by `cargo test` rather than visible in the `.dump`. No probe here uses these fixtures.

**Re-verified 2026-09-02 (`#` stringize and expansion-site attribution, #13):** the same 69 fixture directories were analyzed with the `#8` binary and this one and the `.dump`s compared. **60 are byte-identical.** Nine differ:

- `preproc` gains `stringize.c`, whose `f()` is now indexed at all — its stringized log/assert macros no longer produce `#(` parse errors.
- eight macro fixtures (`builtin_macros`, `macro_field`, `macro_fnlike`, `macro_indirect`, `macro_nested_field`, `macro_paste`, `macro_xmacro`, `union_macro`) differ **only in line/column values**, in `functions`, `variables`, `call_sites` and `flow_nodes`: macro-expanded entities now attribute to the invocation instead of the `#define`. For example `builtin_macros`' `FooTest_Bar` moves from the macro definition at line 1 to its `HWTEST` invocation at line 8, and `macro_xmacro`'s `alpha_handler` / `beta_handler` from 9 to 12. No row is added, removed or renamed in any of the eight.

The one probe that prints such a position, Case 7 below, was re-run against both binaries — `main.c:1` → `main.c:11` — and the case text below shows the new output; every other probe is unchanged.

**Re-verified 2026-09-03 (preprocessor diagnostics exported, #20):** every fixture directory under `tests/fixtures/` (72 directories) was analyzed with the master (`9d15520`) and post-fix binaries and the two SQLite exports compared as full `.dump`s with the `analysis_run` timestamp row excluded. **62 are identical**; the other 10 differ only in the `diagnostics` table, so every probe below is unchanged. The `preprocess`-stage rows that `trace analyze` used to drop are now exported: `direct_call`, `memcpy_*` / `memmove_fnptr`, `cpp_flow`, `cpp_ctor_callback`, `cpp_extern_c_driver` and `indirect_cpp_return` gain one `include file not found, skipping: <system header>` warning per `#include <stdio.h>` / `<string.h>` / `<stdint.h>` / `<map>` … line (attributed to the including fixture file and line), and `preproc` gains the eleven rows the #8 fixtures were written for — `unterminated #if` on `unterminated_if_header.h:1`, the three `#elif` / `#else` / `#endif without #if` errors on `stray_{elif,else,endif}.h:1` with their `preprocess stopped in …` warnings, and the `resolved include expanded to nothing` warning on each of those four headers — next to the pre-existing `parse errors in …` rows. Those rows are now also asserted by `cargo test` (`unterminated_if_fixture_exports_preprocess_error`). `mixed_lang`, the #14 fixture whose `shared.h` is warmed under both lexers, stays identical: its header reports nothing under either, and the review fix that forwards the second lexer's diagnostics (a `#` line inside a C++ raw string is an unknown directive in C) is covered by a temp-tree regression test rather than a fixture row.

**Re-verified 2026-09-04 (gMock declaration fallbacks, #15):** every fixture directory under `tests/fixtures/` (72 directories) was analyzed with the master (`168e643`) and post-fix binaries and the two SQLite exports compared as full `.dump`s with the `analysis_run` timestamp row excluded. **71 are identical.** The one that differs is `builtin_macros`, which now carries this change's `gmock.cpp` — modern, legacy, `_WITH_CALLTYPE`, wrapped-across-lines, comma-protected and pointer-to-member forms. The master binary reports a parse error there and indexes eight phantom members named after the macros themselves (`MockInterface::MOCK_METHOD`, `MockInterface::std`, `MockInterface::void`, `MockInterface::MOCK_METHOD0`, …); the fixed binary parses the file cleanly and indexes the fourteen members the class declares. No probe below uses that fixture, so every case is unchanged.

**Re-verified 2026-09-08 (conditional `#include` suppression, #56):** every fixture directory under `tests/fixtures/` (80 directories) was analyzed with the master (`0a35409`) and post-fix binaries and the two SQLite exports compared as full `.dump`s with the `analysis_run` timestamp row excluded. **79 are identical**, so every probe below is unchanged. The one that differs is `preproc`, which now carries this change's fixtures. Under the master binary the second `#include` of `xmacro_table/list.def` is suppressed, so `use_alpha` and `use_beta` are absent along with their edges to `fn_alpha` / `fn_beta`; the fixed binary defines both at `list.def:1` and `:2` and carries both edges. `pragma_once_inactive/once.h` likewise expands twice, so `second_body` is a definition at `once.h:4` with an edge to `second_target` rather than the undefined reference master leaves at `guard_undef_reinclude/t.c:18`, and `guard_undef_reinclude` contributes its own `second_body -> second_target`. The `diagnostics` count is unchanged (13 rows either way): nothing about the new suppression rule reports where the old one did not. No probe here uses these fixtures; they are asserted by `cargo test` (`preproc_reinclude_cases.rs`).

**Method:** every fixture under `tests/fixtures/` that exercises indirect calls or value flow was analyzed fresh into `/tmp/*.db`, then probed with both tools. Every result was cross-checked three ways: (1) against the source files, (2) against `call_sites`/`call_edges` and `flow_nodes`/`flow_edges` in the export, (3) via recursive-CTE BFS closure queries on those tables.

| Command | Data source | `--direction` meaning |
|---------|-------------|-----------------------|
| `trace inspect <db> callgraph --file <substr> --line <N> [--depth N] [--direction up\|down]` | `call_sites` + `call_edges` | `down` = callees, `up` = callers |
| `trace inspect <db> dataflow --file <substr> --line <N> --col <N> [--depth N] [--direction up\|down]` | `flow_nodes` + `flow_edges` | `down` = where the value flows, `up` = where it comes from |

Defaults: `--direction down`; `dataflow` depth default 3. Both are bounded BFS traversals over one symbol lookup — sub-millisecond on every fixture.

## Output conventions

- Symbol lookup: `--file` is a **path substring** (basename or full path); function names match exactly or by C++ qualified suffix; the line must lie inside a function body (header line prints `name (file.c:S-E)`).
- Edges: `-direct->` / `-indirect->` / `-external->`, followed by `(callee.c:N)` and the call site `(caller.c:N)`; `[external]` marks external callees.
- Dedup: a callee re-reached at a different call site renders `(see above; also main.c:49)`.
- Truncation: a live frontier at the depth limit prints `(truncated at --depth N; increase to see more)`, exit 0.
- Ambiguity is a note, not an error: `note: 3 candidates on this line; using pa (others: pb, x)` (nearest column). Dataflow falls back to the nearest declaration listed in the line: `note: no declaration exactly at main.cpp:26:17; using w (local) in drive main.cpp:26:12`.
- Errors exit 1: `depth must be >= 1`; ``invalid direction `sideways` (expected `up` or `down`)``; `no function contains <file>:<line>; nearby definitions: …`; `no variable declared near <file>:<line>:<col>`.

---

# Part 1 — `trace inspect callgraph`

## 1.1 Indirect calls (C)

### Case 1 — dispatch table, array summary

Fixture `fn_ptr_table`.

| Field | Value |
|-------|-------|
| File | `tests/fixtures/fn_ptr_table/main.c` |
| Function | `dispatch_table` (main.c:7-10) |
| Site | `table[0]()` (main.c:9), `void (*table[2])(void) = {row0, row1}` |
| Resolved targets | `row0` (main.c:1), `row1` (main.c:4) — both `-indirect->` |

```text
callgraph from dispatch_table (main.c:7-10) (callees, depth 2):
* dispatch_table (main.c:7)
  -indirect-> row0 (main.c:1) (main.c:9)
  -indirect-> row1 (main.c:4) (main.c:9)
3 functions, 2 edges
```

Even though the subscript is the constant `0`, the array summary keeps every initializer element, so both `row0` and `row1` resolve (documented array-summary behavior). depth 1 and depth 2 are identical.

### Case 2 — global ops struct, two-field indirect chain

Fixture `fn_ptr_vtable`.

| Field | Value |
|-------|-------|
| File | `tests/fixtures/fn_ptr_vtable/main.c` |
| Function | `dispatch` (main.c:19-22) |
| Site | `g_ops.interFace->handler(v)` (main.c:21) |
| Resolved targets | `target` (main.c:9) — `-indirect->` |

```text
callgraph from dispatch (main.c:19-22) (callees, depth 1):
* dispatch (main.c:19)
  -indirect-> target (main.c:9) (main.c:21)
2 functions, 1 edges
```

Confirmed in the flow graph: `addr_of fn:target → store into field → load` through `g_sub.handler` and `g_ops.interFace` summaries; single target, no false positives.

### Case 3 — two distinct ops structs; no cross-struct bleed

Fixture `fn_ptr_cross_struct` — `OpsA`/`OpsB` are distinct `struct` types; `a->callback` must not resolve to `b->handler`.

| Field | Value |
|-------|-------|
| File | `tests/fixtures/fn_ptr_cross_struct/caller.c` |
| Function | `CallBoth` (caller.c:22-29) |

**depth 1** (all `-direct->`): `RegisterOpsA` (ops_a.c:16), `RegisterOpsB` (ops_b.c:16), `CallWithOpsA` (caller.c:12), `CallWithOpsB` (caller.c:17). Truncation note shown.

**depth 2** adds the two indirect fixups:

```text
callgraph from CallBoth (caller.c:22-29) (callees, depth 2):
* CallBoth (caller.c:22)
  -direct-> RegisterOpsA (ops_a.c:16) (caller.c:24)
    -direct-> InitOpsA (ops_a.c:9) (ops_a.c:17)
  -direct-> RegisterOpsB (ops_b.c:16) (caller.c:25)
    -direct-> InitOpsB (ops_b.c:9) (ops_b.c:17)
  -direct-> CallWithOpsA (caller.c:12) (caller.c:26)
    -indirect-> CallbackImplA (ops_a.c:4) (caller.c:14)
  -direct-> CallWithOpsB (caller.c:17) (caller.c:27)
    -indirect-> HandlerImplB (ops_b.c:4) (caller.c:19)
9 functions, 8 edges
```

- depth 3 = depth 2 (saturated).
- **No cross-struct bleed**: the DB holds exactly two indirect edges — `(caller.c:14 → CallbackImplA)` and `(caller.c:19 → HandlerImplB)`.

**UP direction** depth 2 from `CallbackImplA` (ops_a.c:4): `CallbackImplA ← CallWithOpsA ← CallBoth`, 3 funcs/2 edges — matches the reverse closure.

### Case 4 — designated init + helper-returned element pointer

Fixture `camera_subdev_ops` — `fill_subdev` initializes `g_sensorDeviceOps[0].setConfig = CameraCmdSensorSetConfig` (designated fields), fetched via `GetSensorDeviceOps`.

| Field | Value |
|-------|-------|
| File | `tests/fixtures/camera_subdev_ops/common.c` |
| Function | `CommonDeviceSetConfig` (common.c:8-13) |

**depth 1:** `fill_subdev` (`-direct->`, common.c:11) + `CameraCmdSensorSetConfig` (`-indirect->`, via `subDev->subDevOps->setConfig`, common.c:12); truncated.

**depth 2 / 3 (identical — saturated):**

```text
callgraph from CommonDeviceSetConfig (common.c:8-13) (callees, depth 2):
* CommonDeviceSetConfig (common.c:8)
  -direct-> fill_subdev (common.c:3) (common.c:11)
    -direct-> GetSensorDeviceOps (sensor.c:7) (common.c:5)
  -indirect-> CameraCmdSensorSetConfig (sensor.c:12) (common.c:12)
4 functions, 3 edges
```

### Case 5 — designated init across two header levels

Fixture `nested_designated_dispatch`.

| Field | Value |
|-------|-------|
| File | `tests/fixtures/nested_designated_dispatch/launch.c` |
| Function | `launch` (launch.c:5) |
| Resolved targets | `DispatchToMessage` (sidecar.c:3) — `-indirect->` |

```text
callgraph from launch (launch.c:5-5) (callees, depth 3):
* launch (launch.c:5)
  -indirect-> DispatchToMessage (sidecar.c:3) (launch.c:5)
2 functions, 1 edges
```

The `.Dispatch = DispatchToMessage` member is reached through two nested header `NAMED_INIT` layers; single exact target.

### Case 6 — designated init + C++ multi-TU prototype record

Fixture `hpp_designated_dispatch` (launch.cpp + target.cpp + store.cpp + layout.hpp):

```text
callgraph from launch (launch.cpp:5-5) (callees, depth 3):
* launch (launch.cpp:5)
  -indirect-> DispatchToMessage (target.cpp:1) (launch.cpp:5)
2 functions, 1 edges
```

The instrumented callback resolves to the single definition `DispatchToMessage (target.cpp:1)`. (Previously the definition's *prototype mirror* in `store.cpp:3` also surfaced as a separate `[external]` record + a second edge; fixed — see Observations 1.)

### Case 7 — macro-wrapped call

Fixture `macro_indirect` — `#define INVOKE(f) ((f)())`:

```text
callgraph from via_macro_indirect (main.c:9-12) (callees, depth 2):
* via_macro_indirect (main.c:9)
  -indirect-> target (main.c:3) (main.c:11)
2 functions, 1 edges
```

`INVOKE(fp)` (source line 11) resolves through the function-like macro to `target`; the reported call site `main.c:11` is the invocation (expansion-site attribution, per the LineMap invariant — until the #13 follow-up it was reported at the `#define` on line 1). `decoy` is never reached — only the stored `&target` cell feeds the call.

### Case 8 — `dlsym` / `GetProcAddress` (all positive + negative patterns)

Fixture `dlsym` — `callgraph` output shows exactly the expected edges:

```text
call_literal    (main.c:31) -> target (indirect)   # dlsym(h, "target")
call_var        (main.c:39) -> target (indirect)   # const char *n = "target"
call_copy       (main.c:48) -> target (indirect)   # char m[]; strcpy(m, "target")
call_concat     (main.c:55) -> target (indirect)   # "targ" "et" adjacent literals
call_wrap       (main.c:62) -> target (indirect)   # wrap(): dlsym result re-invoked
call_global     (main.c:69) -> target (indirect)   # g_name[] = {"target"}
call_getproc    (main.c:76) -> target (indirect)   # GetProcAddress(module, "target")
call_cast_invoke(main.c:83) -> target (indirect)   # ((fn_t)dlsym(h, "target"))()
call_missing    (main.c:88/89) -> dlopen, dlsym    # "not_a_symbol": no target
call_unknown    (main.c:97/99) -> dlopen, dlsym    # computed-name: no target
```

Eight of ten helpers resolve to `target` as `-indirect->`; the two negative cases have zero callee edges. The three string cells in the flow graph are `loc:string:target`, `loc:string:x.so`, `loc:string:not_a_symbol` — only `target` feeds any callable cell.

### Case 9 — static/internal linkage resolution

Fixtures `static_call_return` (`GetOps` main.c:3-5 → `user` main.c:7) and `fn_static_local` (`target` main.c:1, `user` main.c:3-7, call `-indirect->` into a function-local `static` fn-ptr) resolve via scope-aware lookup; no external-only alias is emitted for the internal callees.

### Case 10 — function-model bridging

Fixture `fn_models` (with `--models models.toml`):

```text
copy_via_memcpy_s       (main.c:37) -> impl_run (main.c) (indirect)  # memcpy_s mem_copy model
call_through_wrapper_copy (main.c:45) -> impl_run (main.c) (indirect) # OpsA→OpsB TOML model
use_alloc (main.c:65) -> alloc_ops (direct); use_alloc (main.c:66) -> grow_ops (direct)
```

The `OpsA`→`OpsB` copy bridged by the model makes `g_dst.run` and `g_fourth.run` resolve even though the two structs are distinct types (the baseline field summary cannot cross types).

### Case 11 — array-of-struct designated tables (known over-approximation)

Fixture `array_table_designated` (static functions `raw_obtain` main.c:11, `ipc_obtain` main.c:12, designated `g_map[3]`, tentative `g_tbl[2]`, local `tbl[2]`):

```text
caller_helper_ptr (main.c:32) -> loc_b, loc_a, ipc_obtain, raw_obtain  (indirect)  # c->obtain via map_get()
caller_direct     (main.c:37) -> loc_b, loc_a, ipc_obtain, raw_obtain  (indirect)  # g_map[t].obtain
caller_local      (main.c:79) -> loc_b, loc_a, ipc_obtain, raw_obtain  (indirect)  # tbl[t].obtain (local array)
run               (main.c:62) -> impl_a, impl_b                        (indirect)  # g_tbl[i].fn runtime stores
```

`g_map[].obtain` (global) and the **local** `tbl[].obtain` share the single `struct Ctor.obtain` field summary (instance-insensitive), so every `->obtain` load sees `raw_obtain`, `ipc_obtain`, `loc_a`, `loc_b` — expected FieldSummary merge, and exactly what the fixture's integration test asserts (`array_table_designated_init_resolves_targets` requires `raw_obtain` + `ipc_obtain` present). The tentative-definition + runtime `g_tbl[i].fn = impl_a/impl_b` path resolves precisely to those two.

## 1.2 C++ cases

### Case 12 — CHA virtual dispatch, qualified names, dtor fan-out

Fixture `cpp_basic` (`main` at main.cpp:38-54):

```text
callgraph from main (main.cpp:38-54) (callees, depth 2):
* main (main.cpp:38)
  -direct-> gfx::Circle::Circle (main.cpp:32) (main.cpp:39)
  -direct-> gfx::Shape::area (main.cpp:28) (main.cpp:42)
  -direct-> gfx::Circle::area (main.cpp:36) (main.cpp:42)
  -direct-> gfx::Circle::radius (util.hpp:21) (main.cpp:43)
  -direct-> gfx::Shape::common (main.cpp:30) (main.cpp:44)
  -direct-> gfx::Shape::~Shape (main.cpp:26) (main.cpp:46)
  -direct-> gfx::Circle::~Circle (main.cpp:34) (main.cpp:46)
  -direct-> add (main.cpp:6) (main.cpp:48)
    -direct-> mark_i (main.cpp:3) (main.cpp:7)
    -direct-> mark_d (main.cpp:4) (main.cpp:12)
  -direct-> add (main.cpp:6) (see above; also main.cpp:49)
  -direct-> util::tag (main.cpp:17) (main.cpp:50)
  -direct-> hidden (main.cpp:21) (main.cpp:51)
    -direct-> util::tag (main.cpp:17) (see above; also main.cpp:21)
13 functions, 14 edges
```

- `s->area()` (main.cpp:42) → `gfx::Shape::area` **and** `gfx::Circle::area`: CHA over the class hierarchy (`s` is typed `Shape*`, object is a `Circle`). Correct over-approximation.
- Virtual dtor (main.cpp:46) → `~Shape` + `~Circle`.
- `add` overloads (`add(int,int)` main.cpp:6-9, `add(double)` main.cpp:11-13) collapse in the exported `functions` table to the record at main.cpp:6, and both call sites (48, 49) bind to it; the `mark_d` edge (`add :12 → mark_d`) proves the second overload body's edges are in the graph. The tree **can** disambiguate overloads by defining line — see Case 14.
- `hidden` (main.cpp:21) is called directly at main.cpp:51 (`int h = hidden();`) — exactly one `-direct->` edge, no fan-out.

### Case 13 — implicit `this`, virtual fan-out, smart-pointer unwrap, arity filtering

Fixture `cpp_implicit_this` (everything in main.cpp):

```text
callgraph from Base::go (main.cpp:3-3) (callees, depth 1):
* Base::go (main.cpp:3)
  -direct-> Base::hook (main.cpp:10) (main.cpp:3)
  -direct-> Derived::hook (main.cpp:11) (main.cpp:3)
3 functions, 2 edges

callgraph from drive (main.cpp:13-13) (callees, depth 2):   # drive(Base*) -> go
* drive (main.cpp:13)
  -direct-> Base::go (main.cpp:3) (main.cpp:13)
    -direct-> Base::hook (main.cpp:10) (main.cpp:3)
    -direct-> Derived::hook (main.cpp:11) (main.cpp:3)
4 functions, 3 edges
```

- `go` calls the virtual `hook()` through the implicit `this`; whole-class CHA yields both overrides.
- Smart-pointer calls (`call_sp`/`call_sp_ref`/`call_up`/`call_wp`) all resolve `Plugin::OnEvent`/`Plugin::OnEventProxy` targets through the unwrap.
- **Arity filtering** (verified by fn_id, not just name):
  - `call_unary` (main.cpp:60) → `Over::foo (main.cpp:55)` + `OverD::foo (main.cpp:57)` — the `foo(int)` overrides only.
  - `call_binary` (main.cpp:61) → `Over::foo (main.cpp:56)` + `OverD::foo (main.cpp:58)` — the `foo(int,int)` overrides only.

### Case 14 — overload disambiguation by defining line

The two `Over::foo` overloads share a display name but are distinct fn records (55 vs 56); the callgraph tree surfaces the **defining line** as the disambiguator:

```text
callgraph from call_unary (main.cpp:60-60):
* call_unary (main.cpp:60)
  -direct-> Over::foo (main.cpp:55) (main.cpp:60)   # foo(int)
  -direct-> OverD::foo (main.cpp:57) (main.cpp:60)
```

### Case 15 — callable objects: lambda, functor, `std::function`

Fixture `cpp_callable` (main.cpp) — `calls` confirms:

```text
call_lambda (main.cpp:20) -> call_lambda::$lambda19:14 (indirect)
    call_lambda::$lambda19:14 (main.cpp:19) -> target (direct)
call_field (main.cpp:9)    -> target (indirect)     # h->cb via setup_field/field
call_local (main.cpp:15)   -> target (indirect)     # c({ .cb=target }) designated
call_functor (main.cpp:29)      -> Fn::operator() (direct)   # w; _w.f(6)
call_functor_field (main.cpp:36)-> Fn::operator() (direct)   # w->callableField
call_std_function (main.cpp:45) -> target (indirect)         # std::function f=switch_fn
call_std_field (main.cpp:54)    -> target (indirect)         # w->getPluginObject
call_bare_function_type (main.cpp:68) -> function::operator() (direct)
```

Lambda, functor struct, function-pointer fields, and `std::function` all resolve; no spurious unrelated `operator()` targets.

### Case 16 — multi-inheritance, template, ctor-init chains

Fixture `cpp_more` (main.cpp + cpp_more.hpp `drive` at main.cpp:18-33):

```text
callgraph from drive (main.cpp:18-33) (callees, depth 2):
* drive (main.cpp:18)
  -direct-> tie (main.cpp:4) (main.cpp:19)
  -direct-> tie (main.cpp:5) (main.cpp:20)
  -direct-> D::D (cpp_more.hpp:46) (main.cpp:23)
    -direct-> Base::Base (main.cpp:9) (cpp_more.hpp:46)
    -direct-> Member::Member (main.cpp:11) (cpp_more.hpp:46)
  -direct-> Box::put (cpp_more.hpp:18) (main.cpp:25)
  -direct-> Box::get (cpp_more.hpp:19) (main.cpp:26)
  -direct-> A::fa (cpp_more.hpp:26) (main.cpp:29)
  -direct-> AB::fa (cpp_more.hpp:36) (main.cpp:29)
  -direct-> S::Make (main.cpp:13) (main.cpp:31)
  -direct-> sink_int (main.cpp:15) (main.cpp:32)
  -direct-> S::Make (main.cpp:13) (see above; also main.cpp:32)
  -direct-> sink_w (main.cpp:16) (main.cpp:32)
    -direct-> Widget::make (main.cpp:7) (main.cpp:16)
  -direct-> Base::base_value (cpp_more.hpp:7) (main.cpp:32)
15 functions, 15 edges
```

- `pa->fa()` (main.cpp:29) → `A::fa` **and** `AB::fa` — mult-inheritance CHA (both meanings of `fa` are reachable through an `A*`).
- `D::D`'s ctor-init list inlines `Base::Base` + `Member::Member`.
- Template `Box::put/get`, `S::Make`, and `sink_w → Widget::make` through the value-fn chain; `w.value()` also surfaces `Base::base_value`.
- The two `tie` calls resolve to the two distinct overload records (main.cpp:4 and main.cpp:5) instead of collapsing onto one.

### Case 17 — C/C++ interop: one call site, two implementations

Fixture `cpp_flow` — `main.c:9-13` calls `RegisterOps` (impl.cpp:13) and `Read` (ops.c:11-16):

```text
callgraph from Read (ops.c:11-16) (callees, depth 2):
* Read (ops.c:11)
  -indirect-> RawImplRead (ops.c:3) (ops.c:15)
  -indirect-> MParcelImplRead (impl.cpp:5) (ops.c:15)
3 functions, 2 edges
```

The C fixture calls through `s->impl->read(...)`, and both the C (`RawImplRead`) and C++ (`MParcelImplRead`) implementations resolved. `main → RegisterOps, Read` depth 2 flattens the whole fixture (5 funcs/4 edges).

### Case 18 — sbuf readBuffer (reproduces eval-report Case 32)

Fixture `indirect_cpp_return` — `HdfSbufReadBuffer` (hdf_sbuf.c:48-54, call hdf_sbuf.c:53):

```text
callgraph from HdfSbufReadBuffer (hdf_sbuf.c:48-54) (callees, depth 1):
* HdfSbufReadBuffer (hdf_sbuf.c:48)
  -indirect-> SbufRawImplReadBuffer (hdf_sbuf_impl_raw.c:12) (hdf_sbuf.c:53)
  -indirect-> SbufMParcelImplReadBuffer (hdf_sbuf_impl_hipc.cpp:49) (hdf_sbuf.c:53)
(truncated at --depth 1; increase to see more)
3 functions, 2 edges
```

**Exactly two targets**, matching the eval report. Source grep confirms exactly two `readBuffer =` stores in the fixture (`hdf_sbuf_impl_raw.c:57`, `hdf_sbuf_impl_hipc.cpp:79`). depth 2 adds the C++ impl's own calls (`MParcelCast` impl_hipc.cpp:32, `MessageParcel::ReadUint32` :8, `MessageParcel::ReadUnpadBuffer` :9).

### Case 19 — constructor registry + service callback chain

Fixture `cpp_ctor_callback` — `test_callback_dispatch` (main.c:11-26):

```text
callgraph from test_callback_dispatch (main.c:11-26) (callees, depth 2):
* test_callback_dispatch (main.c:11)
  -direct-> RegisterConstructor (impl.cpp:7) (main.c:12)
    -external-> std::string (impl.cpp:8 [external]) (impl.cpp:8)
  -direct-> CreateService (impl.cpp:12) (main.c:13)
    -external-> std::map::find (impl.cpp:13 [external]) (impl.cpp:13)
    -external-> std::string (impl.cpp:8 [external]) (see above; also impl.cpp:13)
    -external-> std::map::end (impl.cpp:14 [external]) (impl.cpp:14)
  -indirect-> SampleDriverInit (impl.cpp:35) (main.c:21)   # entry.Init (registry entry)
  -indirect-> SampleDispatch (impl.cpp:21) (main.c:23)     # dev.service->Dispatch
8 functions, 8 edges
```

Note: `entry.Init` resolves to `SampleDriverInit` even though `entry.Init = nullptr` is stored first — instance-insensitive `FieldSummary` reads the whole field history (documented may-analaracter over-approximation); the tool renders it faithfully.

### Case 20 — scalar-type overload ranking (fixture `cpp_templates_overloads`)

Same-arity overloads separated by parameter type are distinct records, and a call
site picks the exact match instead of fanning out over every candidate:

```text
callgraph from main (main.cpp:18-31) (callees, depth 2):
* main (main.cpp:18)
  -direct-> FieldValue::GetNumber (main.cpp:4) (main.cpp:20)
  -direct-> FieldValue::GetNumber (main.cpp:5) (main.cpp:21)
  -direct-> FieldValue::GetNumber (main.cpp:4) (see above; also main.cpp:22)
  -direct-> f (main.cpp:13) (main.cpp:23)   # f(1) -> f(int)
  -direct-> f (main.cpp:14) (main.cpp:24)   # f(1.5) -> f(double)
  -direct-> f (main.cpp:15) (main.cpp:26)   # f(s) -> f(short)
  -direct-> f (main.cpp:16) (main.cpp:27)   # f(1, 2) -> f(int, int)
  -direct-> Box::read (main.cpp:10) (main.cpp:29)
    -external-> T (main.cpp:6 [external]) (main.cpp:10)
9 functions, 9 edges
```

`f(1)`, `f(1.5)`, `f(s)` each emit **one** direct edge to the distinct defined
overload — `FieldValue::GetNumber` has 3 records (int / long / template primary).
Candidates of the same name resolve independently; export `functions` keeps one
row per overload, call sites one direct edge per exact match (ties still emit all
candidates). The `-external-> T` edge under `Box::read` is the dependent call
on the template parameter inside the primary (`T::read(...)`-style); with no
concrete instantiation record it is kept as an external callee rather than
dropped. Validate: `SELECT COUNT(*) ...` (see `scripts/eval_check.py`).

### Case 21 — template member calls (fixture `cpp_templates_overloads`)

`fv.GetNumber<int>()` and `b.read<short>()` resolve to the primary registered
method (template `<…>` stripped from callee text; in-class template methods lower
from their `template_declaration` body):

```text
inspect calls --from GetNumber :
FieldValue::GetNumber (main.cpp:6) -> T [main.cpp] (external)   # template body `return T();`
```

The call sites themselves are direct (`8/8` edges, 0 indirect, no unresolved
`GetNumber`/`read` stubs). The single `external` edge is the template body's `T()`
temporary — the un-instantiated placeholder stays unresolvable by design
(document in `docs/CPP_ROADMAP.md` C9).

## 1.3 Depth, direction, dedup

| Probe | Result |
|-------|--------|
| depth 1 with a live frontier | prints `(truncated at --depth 1; increase to see more)`, exit 0 |
| depth > saturated | no truncation, output identical (camera d2=d3; cross_struct d2=d3; cpp_basic d3=d4) |
| `--direction up` (`CallbackImplA` ops_a.c:4) | reverse BFS: `CallbackImplA -indirect-> CallWithOpsA -direct-> CallBoth`, callers labeled like callees |
| same callee at a second call site | `(see above; also main.c:49)` |
| external C++ proto record vs definition, same name | distinct nodes keyed by source file |

## 1.4 callgraph error handling

- `--depth 0` → `Error: depth must be >= 1` (exit 1)
- `--direction sideways` → ``Error: invalid direction `sideways` (expected `up` or `down`)``
- `--file nosuch.c --line 7` → `Error: no function contains nosuch.c:7; nearby definitions: ` (empty list)
- line between functions (fn_ptr_table main.c:3) → `Error: no function contains fn_ptr_table/main.c:3; nearby definitions: row0 (main.c:1-2), row1 (main.c:4-5), dispatch_table (main.c:7-10)…`
- exit 1

---

# Part 2 — `trace inspect dataflow`

Dataflow walks the exported PAG value-flow graph. Node label grammar: variables `name (kind @line in fn)`; locations `loc:<name>`; field locations `loc:<field> of <var>`; field summaries `loc:summary:<Type>.<field>`; call targets `target:<expr> (call @N)`; heap `loc:new heap`; strings `loc:string:name (string_lit)`; terminators `terminator:<label> (call @N in fn)`. Edge labels: `copy`, `addr_of`, `load`, `store`, `gep`, `points_to`, `call_arg`, `terminates`, plus generic `flow` (see 2.11).

## 2.1 Value → param → param — `arg_flow`

```text
dataflow for value (local) in entry main.c:8:9 (flows-to, depth 3):
* value (local @8 in entry)
  -call_arg-> q (param @3 in provider)
    -call_arg-> p (param @1 in consume)
```
depth ≥ 2 identical (saturates). Up from `p` and `q` traces back to `value` symmetric to the above.

## 2.2 Param → ops struct field → target — `fn_ptr_cross_struct`

- `pa` (caller.c:23:10, down): `pa -> call_arg-> ops (CallWithOpsA:12) -> copy-> a (:13) -> gep-> _gep5`; and `pa -> call_arg-> out (RegisterOpsA:16)`.
- `ops` (CallWithOpsA:12:18, up): `ops -> call_arg-> pa`.
- `a` (caller.c:13:17, down) — the fn-ptr load chain into the actual call target:

```text
* a (local @13 in CallWithOpsA)
  -gep-> _gep5 (local @14 in CallWithOpsA)
    -load-> _load6 (local @14 in CallWithOpsA)
      -copy-> target:a->callback (call @14)
```

- `x` (caller.c:22:14, down, depth 4): two independent chains (`→ x CallWithOpsA → x CallbackImplA` and `→ x CallWithOpsB → x HandlerImplB`) with **no cross-contamination** — 5 nodes/4 edges.

## 2.3 Global ops chain — `fn_ptr_vtable`

`g_ops` (main.c:17:19, down, depth 6):

```text
* g_ops (file_static @17)
  -points_to-> loc:g_ops of g_ops (file_static)
  -gep-> _gep6 (local @20 in dispatch)
  -gep-> _gep7 (local @21 in dispatch)
    -load-> _load8 (local @21 in dispatch)
      -gep-> _gep9 (local @21 in dispatch)
        -load-> _load10 (local @21 in dispatch)
          -copy-> target:g_ops->interFace->handler (call @21)
9 flow nodes, 7 flow edges
```

`v` (dispatch:19:15, down) → `p` (target:9:20) via one `call_arg`; `p` up mirrors it. The `loc:summary:Sub.handler` / `loc:summary:Ops.interFace` field-summary cells are present as isolated detail nodes.

## 2.4 Camera designated table — `camera_subdev_ops`

`subDev` (common.c:10:22, down, depth 5):

```text
* subDev (local @10 in CommonDeviceSetConfig)
  -call_arg-> subDev (param @3 in fill_subdev)
    -gep-> _gep2 (local @5 in fill_subdev)
  -gep-> _gep5 (local @12 in CommonDeviceSetConfig)
    -load-> _load6 (local @12 in CommonDeviceSetConfig)
      -gep-> _gep7 (local @12 in CommonDeviceSetConfig)
        -load-> _load8 (local @12 in CommonDeviceSetConfig)
          -copy-> target:subDev->subDevOps->setConfig (call @12)
  -call_arg-> subDev (param @12 in CameraCmdSensorSetConfig)
9 flow nodes, 8 flow edges
```

`subDev` (fill_subdev:3:25) up → `subDev` (CommonDeviceSetConfig:10). Global `g_sensorDeviceOps` (sensor.c:3:31, down): `loc:g_sensorDeviceOps → addr_of → _ret3 → store → _gep2` in `fill_subdev`, plus `loc:setConfig of g_sensorDeviceOps` — the designated-init chain feeding the table.

## 2.5 C/C++ interop impl lift — `cpp_flow`

`s` (main.c:7:10, down, depth 4) selects the C-side `Read` record and widens to the twin:

```text
* s (param @7 in Read)
* s (param @11 in Read)
  -gep-> _gep9 -> -load-> _load10 -> -gep-> _gep11 -> -load-> _load12   # impl-pointer checks
  -gep-> _gep14 -> -load-> _ret13
    -call_arg-> self (param @5 in MParcelImplRead)
    -call_arg-> self (param @3 in RawImplRead)
10 flow nodes, 8 flow edges
```

Both impls' distinct `self` params each receive the argument. Up from `s` → `g_s` (main.c:4:16, global). Down from `g_s` fans into `RegisterOps`' and both the C/C++ `Read` lanes.

## 2.6 Heap-backed object — `cpp_basic`

```text
dataflow for s (local) in main main.cpp:40:16 (flows-from, depth 3):
* s (local @40 in main)
  -copy-> c (local @39 in main)
    -copy-> _ret12 (local @39 in main)
      -addr_of-> loc:new heap (heap)
```
`c` (main.cpp:39:17, down) → `s`. The remote `new` allocation cell is the source.

## 2.7 Ctor registry service chain — `cpp_ctor_callback`

| Probe | Chain |
|-------|-------|
| `entry` (main.c:16:28, down) | `_gep10/_gep12/_gep14/_gep16 -> load -> _load17` (the `entry.Init` field, call context for main.c:21) |
| `dev` (main.c:15:32, down, depth 5) | `call_arg-> device (SampleDriverInit:35)` plus `gep(_gep18)->load(_load19)->gep(_gep20)->load(_load21)->copy-> target:dev->service->Dispatch (call @23)` |
| `svc` (main.c:13:10, down) | `call_arg-> service (param @21 in SampleDispatch)` |

## 2.8 Callable objects and fields — `cpp_callable`

- `h` (main.cpp:9:24, down): `gep(_gep4) -> load(_load5) -> copy -> target:h->cb (call @9)`.
- `w` (main.cpp:54:28 = `call_std_field`, down): `gep(_gep16) -> load(_load17) -> copy -> target:w->getPluginObject (call @54)`.
- `w` (main.cpp:36:32 = `call_functor_field`, down): `1 flow node, 0 edges` — the callable stores into the `w->callableField` location (reachable in the PAG as a field cell), so the bare param has no named chain; the callgraph still resolves `Fn::operator()` at that site.
- `p` (main.cpp:14:10, up) and `f` (main.cpp:44:26, up): `-addr_of-> loc:fn:target (function)` — the function-value source seeding the callable / `std::function`.
- `fn_ptr_table` `table` (main.c:8:10, down): `-copy-> target:table[...] (call @9)`.

## 2.9 StringConst / dlsym — `dlsym`

- `n` (main.c:37:16, up): `n -> addr_of -> loc:string:target (string_lit)`.
- `f` (main.c:38:10, up, depth 3): `f -> dlsym -> n -> addr_of -> loc:string:target`. The underlying edge is `flow_edges` id 16, `kind='dlsym'`, src node 20 (`n`), dst 21 (`f`).
- helper `wrap`'s `_ret12` (main.c:24:12, up, depth 3): `_ret12 -> dlsym -> name (param @22) -> call_arg -> _ret33 -> addr_of -> loc:string:target` — call-surviving dlsym re-invoked through the wrapped fn.<br>
  (`terminates` maps the same way for the `fn_models` case in §2.10.)

## 2.10 Terminators / models — `fn_models`

- `g_cleared` (main.c:27:13, down): `-points_to-> loc:g_cleared`, `-terminates-> terminator:memset_s clears arg0 (call @50 in clear_only)` — the memcpy/memset model's terminator node renders with the `terminator:` tag and call-site detail (flow_nodes kind `terminator`).
- `g_dst` (main.c:25:13, down, depth 3): `-gep-> _gep9 -> -load-> _load10 -> -copy-> target:g_dst->run (call @37)` — the `memcpy_s` mem_copy model bridges the OpsA→OpsB bytes so `g_dst.run` resolves.

## 2.11 Edge-kind rendering (fixed)

`flow_edges.kind` stores `dlsym` and `terminates`; the dataflow labeler maps `copy/addr_of/load/store/gep/points_to/call_arg/terminates` and used to let everything else (only `dlsym`) fall through to the generic `"flow"` label (`crates/trace-db/src/inspect.rs:492`). Fixed with a `"dlsym" => "dlsym"` arm; dlsym edges now print `-dlsym->`.

## 2.12 Widening, ambiguity, truncation

- Same-function param twins widen: `add`'s `a` (main.cpp:6:16) prints `note: 2 candidates on this line; using a (others: b)` and lists **both** `a (param @6 in add)` and `a (param @11 in add)` (2 nodes, 0 edges). A different function's same-named param is not widened into (unit tests cover this).
- Ambiguous candidates: `n` (dlsym main.c:37:16, up) prints `note: 4 candidates on this line; using n (others: h, f, _ret18)`; long lists elide with `(+N more)`.
- Column fallback: `note: no declaration exactly at cpp_more/main.cpp:26:17; using w (local) in drive main.cpp:26:12`.
- Truncation appears exactly at the live frontier: `pa` (caller.c:23:10) depth 3 prints `(truncated at --depth 3; increase to see more)` (5 nodes, 4 edges); depth 5 completes the chain (`-copy-> target:a->callback`), 7 nodes/6 edges, marker removed. The shortest chain saturates early: `a` (caller.c:13:17) depth 2 truncates (3 nodes, 2 edges), depth 3 completes with no marker.

## 2.13 dataflow error handling

- `--file nosuch.c --line 8 --col 9` → `Error: no variable declared near nosuch.c:8:9` (exit 1).
- depth 0 and bad direction behave like callgraph (identical messages, exit 1).

---

# Part 3 — Validation methodology

Every claim was checked against the **source code**, not just against the export. For each case I read every file that stores into or calls the target callable and enumerated the store sites by hand (and with `grep`), then compared the tool's resolved set to that source-derived ground truth.

Source-derived ground truth:

| Case | Store/define sites found by reading the source | Tool result | Match |
|------|-----------------------------------------------|-------------|-------|
| `fn_ptr_vtable` | `g_sub.handler = target` (main.c:10) — the only store; call `g_ops.interFace->handler` (main.c:21) | `target` only | ✓ |
| `fn_ptr_cross_struct` | `g_opsA.callback = CallbackImplA` (ops_a.c:17), `g_opsB.handler = HandlerImplB` (ops_b.c:17); `pa→&g_opsA`, `pb→&g_opsB` (caller.c) | `CallWithOpsA→CallbackImplA`, `CallWithOpsB→HandlerImplB` only | ✓ |
| `camera_subdev_ops` | `g_sensorDeviceOps = { .setConfig = &CameraCmdSensorSetConfig }` (sensor.c:4); `subDevOps = GetSensorDeviceOps()` (common.c:5) | `CommonDeviceSetConfig→fill_subdev→GetSensorDeviceOps`, `setConfig → CameraCmdSensorSetConfig` | ✓ |
| `nested_designated_dispatch` | `g_svc = { .object.objectId = 1, .Dispatch = DispatchToMessage }` (store.c:7) with pre-prefix member; call `g_svc.Dispatch(0)` (launch.c:5) | `launch → DispatchToMessage` | ✓ |
| `hpp_designated_dispatch` | `int DispatchToMessage(int);` in store.cpp:3 is a **declaration only**; definition at target.cpp:1; `.Dispatch = DispatchToMessage` (store.cpp:5); call `g_svc.Dispatch(0)` (launch.cpp:5) | `launch → DispatchToMessage (target.cpp:1)` — single edge, no proto mirror | ✓ (was 2; fixed) |
| `dlsym` | literal `"target"` present only in the 8 positive calls/`g_name`; `other` never referenced; `"not_a_symbol"`/uninit `n` in negatives | 8 × `→ target`, 2 × none | ✓ |
| `indirect_cpp_return` | `readBuffer =` stored exactly twice: `<SbufRawImplReadBuffer>` (raw:57), `<SbufMParcelImplReadBuffer>` (hipc:79); also `obtain`/`recycle` twice each | 3 sites × exactly the 2 impls (6 indirect edges) | ✓ |
| `cpp_ctor_callback` | `.Init` stored in `g_sampleDriverEntry` (impl.cpp:44 = SampleDriverInit) and nulled in local `entry` (main.c:17); `.Dispatch = SampleDispatch` (impl.cpp:23); `RegisterConstructor`/`CreateService` direct (main.c:12/13) | `entry.Init→SampleDriverInit` (field-summary, see below), `dev.service->Dispatch→SampleDispatch`, 2 direct | ✓ (with documented over-approx) |
| `cpp_flow` | `raw_ops = { RawImplRead }` (ops.c:12) and `parcel_ops = { MParcelImplRead }` (impl.cpp:9) — both `struct Ops` instances; `g_s.impl = &parcel_ops` | `Read → RawImplRead + MParcelImplRead` | ✓ type-level summary |
| `array_table_designated` | `g_map[0].obtain=raw_obtain`, `g_map[1].obtain=ipc_obtain`, local `tbl[0].obtain=loc_a`, `tbl[1].obtain=loc_b` — all `struct Ctor` | all handlers appear at every `.obtain` load | ✓ FieldSummary merge |
| `cpp_basic` | `c = new Circle`, `s = c` (Shape*); virtual `area` overridden in both classes; `add(int,int)` body calls `mark_i`, `add(double)` body calls `mark_d` | both `area` overrides, both `dtor`s, `add` merged record with both `mark_*` edges | ✓ |
| `cpp_implicit_this` | `go()` calls virtual `hook()`; overrides `Base::hook`, `Derived::hook`; `Over::foo(int)@55`, `foo(int,int)@56`, `OverD@57/58` | `Base::go→{Base,Derived}::hook`; `call_unary→foo@55/57`, `call_binary→foo@56/58` (arity split by defining line) | ✓ |
| `cpp_more` | `pa->fa()` with `pa` = `&ab` (AB : A,B); `D::D` init `Base(v), m()`; `b.put/get`; `sink_w->w->make()` | `A::fa`+`AB::fa`, `Base::Base`+`Member::Member`, `Box::put/get`, `Widget::make` | ✓ |
| `cpp_callable` | `h->cb=target`, `p=target`, `[](){return target();}`, `Fn::operator()`→target, `f=target` (std::function), `w->getPluginObject=target` | each callable site resolves `target`/its operator | ✓ |

Only two observations fall out of this source reading that pure SQL checks would not have surfaced:

1. `cpp_ctor_callback` — the local `entry.Init` is initialized to `nullptr` in source, so `entry.Init(&dev)` is statically a null call; the tool still lists `SampleDriverInit` because the global `g_sampleDriverEntry.Init` store and the local share the per-type `DriverEntry.Init` summary. Expected FieldSummary over-approximation.
2. `array_table_designated` similarly: reading the fixture, the local `tbl` (loc_a/loc_b) is what "pollutes" the global `g_map` loads — both are the same `struct Ctor`, so the instance-insensitive summary merges them by design.

(The earlier draft listed a third: `hpp_designated_dispatch`'s prototype+definition producing two edges. That was a real duplicate-record bug, now fixed — see Observations 1 and 2.)

Cross-checks beyond the source: (a) recursive-CTE closures over `call_sites`/`call_edges` and `flow_nodes`/`flow_edges` reproduce every emitted set edge-for-edge (they confirm the tool reads the export correctly, not that the analysis is right — hence the source pass above); (b) underlying edge kinds (`indirect`, `dlsym`, `terminates`) confirmed verbatim in the export; (c) `cargo test --workspace` green, including the `trace-db` inspect unit tests (depth/truncation/error/widening) and `trace-ir` add_function tests (C++ proto+def collapse, same-arity overload separation).

# Observations

1. **`dlsym` edge label (fixed):** inspect.rs mapped only quasi-direct edge kinds and let `dlsym` fall through to the generic `-flow->` (data was correct). A `"dlsym" => "dlsym"` arm was added; edges now render `-dlsym->`.
2. **C++ prototype+definition duplicate (fixed):** a cross-TU prototype (`store.cpp:3`) and its definition (`target.cpp:1`) used to surface as two `functions` records and two callgraph edges. Cause: the overload signature check compared the *incoming* function's unit-local param ids against the global table (id collisions made them never match). `merge_unit_index` now passes the params' remapped global types into `add_function_with_param_types`, so prototype+definition collapse by name/arity and distinct same-arity overloads still separate — across TUs too. `hpp_designated_dispatch` now emits a single `DispatchToMessage (target.cpp:1)` edge. Unit tests cover both halves.
3. **Overload display merge:** the exported `functions` table dedups C++ overloads by name (single record `add` at main.cpp:6); the callgraph tree restores precision by showing the defining **line** (Case 14), and the underlying edges are arity-correct (Case 13). The plain `calls` listing cannot distinguish overloads of equal display name.
4. **Analyzer over-approximations surface as extra targets,** exactly as documented: instance-insensitive FieldSummary (array_table_designated Case 11 `loc_a/loc_b` merge; cpp_ctor_callback `entry.Init=nullptr` still resolving), whole-class CHA (Case 12 `Shape::area`+`Circle::area`, Case 13 `Base::hook`+`Derived::hook`, Case 16 `A::fa`+`AB::fa`), array summary across subscripts (Case 1). None of these are inspect defects; the tools rendered the solver's exact result.