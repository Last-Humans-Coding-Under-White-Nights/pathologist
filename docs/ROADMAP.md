# Roadmap

## Completed milestones

### M0 — Skeleton & docs ✅

- Cargo workspace (6 crates)
- README, AGENTS.md, docs/
- Fixture: `tests/fixtures/direct_call/`

### M1 — Preprocessor P0 ✅

- Lexer, `#include`, `#define`, conditionals
- `LineMap`
- Function-like macros (incl. GNU/C99 variadics), `##` pasting

### M2 — Parse to IR ✅

- tree-sitter-c integration
- Function/global/parameter extraction, call site collection
- Per-TU indexing + `merge_unit_index`
- Include graph (`IncludeGraph`), orphan-header skip

### M3 — Andersen MVP ✅

- Copy/AddrOf/Load/Store from IR flow
- Direct call graph
- SQLite export

### M4 — Field sensitivity ✅

- `GepField` → PAG `Gep`
- `Field` + **`FieldSummary`** locations
- Global/static/local location kinds
- Field-summary fallback when base `pts` is empty (vtable / param pointer patterns)

### M5 — Indirect calls + arg-flow ✅

- On-the-fly call graph for indirect calls
- Multi-hop field path callees (`p->ops->fn`)
- Designated initializer fn-ptr stores
- Param `Copy` wiring + `arg_flow_edges`
- Export unresolved indirect call sites

### M6 — Hardening ✅

- Diagnostics table
- `trace inspect` CLI
- Integration + adversarial fixtures
- Libc summary stubs (`summaries.rs`)

### M7 — Scale + export ✅

- Parallel TU indexing (`--jobs`)
- Parallel preprocess cache
- Solver adjacency index + `loc_nodes` reverse index
- Minimal SQLite export (default)
- `--full-export`, `--debug-points-to`
- Lazy abstract locations for locals/params

### M8 — Return-value flow ✅

- `ReturnFlow` / `program.fn_returns`
- `CallReturn` flow constraint (callee resolved by name at PAG build)
- Models `field = Getter()` and cross-TU getter functions
- Field RHS store via temp + `Store` (fixes `&Fn` designated init)

### M9 — Static linkage + arg-flow ✅

- Scope-aware resolution for **`static` / internal** functions (direct calls + `CallReturn`)
- Function-pointer actuals in **`arg_flow_edges`** (`actual_fn_id` column)
- Function-local **`static`** variables classified as `FnStatic` (not `Local`)
- Fixtures: `static_direct_call/`, `static_call_return/`, `fn_arg_flow/`, `fn_static_local/`

### M10 — Dependency roots (`--dep`) ✅

- Repeatable `--dep <root>` for a tree the target builds against but that is not under analysis.
- Dependency headers discovered and preprocessed; dependency sources never become translation units, and unreached dependency headers are never indexed as orphan units.
- Declarations only from a dependency body: `is_defined = false`, no locals, call sites, flow constraints or return flows.
- Receiver typing through dependency declarations — `sptr<T>` unwrapped via its declared `operator->` return.
- Schema v4: `files.is_dep`, `functions.is_dep`, `dep_roots` in `options_json`.
- `trace inspect calls --exclude-deps`.
- Fixture: `tests/fixtures/dep_root/`

## In progress / next

C++ beyond the first step is planned from the hiview corpus in
**[docs/CPP_ROADMAP.md](CPP_ROADMAP.md)** (type inference, deferred
callables, plugin factory / maps). Do not expand C++ ad hoc; land slices
C1→C11 there with fixtures.

| Item | Notes |
|------|-------|
| **C++ next slices** | See [CPP_ROADMAP.md](CPP_ROADMAP.md); eval H5 (`auto`/`lock`), H7 (`REGISTER` + map), C11 (`dlsym`) |
| **`memcpy` / `memmove` summaries** | Registered but no-op; blocks fn-ptr-through-memcpy patterns |
| **Original-source line remapping** | Done for `#include`d code: header-origin entities carry original file/line via `LineMap` and are deduplicated across TUs |
| **`compile_commands.json`** | Include paths / defines today via CLI only |
| **Heap allocation modeling** | `malloc` family stubs don't allocate fresh locs yet; C++ `new T` is `NewHeap` |
| **`__VA_OPT__`** | C23 `__VA_OPT__` (variadics, GNU `, ##args` elision and the `#` stringize operator are done) |
| **Constant array index refinement** | Avoid merging all fn-ptr table slots |
| **Points-to visualization** | Beyond `--debug-points-to` SQL dump |

## Non-goals (v1)

- Control-flow / path sensitivity
- Using gcc/clang as primary preprocessor
- Flow-sensitive analysis without explicit design approval
- Sound must-analysis (under-approximation)

## Performance targets

| Corpus | Index | Analyze | Export (minimal) |
|--------|-------|---------|------------------|
| HDF `drivers_hdf_core` | ~3.3s | ~1.5s | ~0.8s |
| Hiview `hiviewdfx_hiview` | ~3.4s | ~0.2s | ~0.4s |
| Camera `multimedia_camera_framework` | ~8s | ~0.3s | ~1.4s |

Further index-time wins: smarter header/preprocess skipping, incremental TU cache.

## Fixture coverage map

| Pattern | Fixture |
|---------|---------|
| Direct call | `direct_call/` |
| Fn-ptr init | `fn_ptr_init/`, `fn_ptr_designated/` |
| Field assign + call | `fn_ptr_field/` |
| Multi-hop vtable | `fn_ptr_vtable/` |
| Indirect param | `indirect_param/` |
| SubDevice ops + call return | `camera_subdev_ops/` |
| Static direct / call-return | `static_direct_call/`, `static_call_return/` |
| Fn-ptr arg flow | `fn_arg_flow/` |
| Function-local static | `fn_static_local/` |
| Preprocessor | `tests/fixtures/preproc/` |
| C++ first-step | `cpp_basic/`, `cpp_more/`, `cpp_flow/`, `cpp_implicit_this/`, `cpp_callable/`, `cpp_dispatch/`, `cpp_extern_c_driver/` |
| C++ next (planned) | [CPP_ROADMAP.md](CPP_ROADMAP.md) — `auto`/`lock`, `DownCastTo`, `REGISTER`+map, `std::bind`/`ffrt::submit` |
| Adversarial / limitations | `tests/fixtures/adversarial_*`, `macro_*` |

Run: `cargo test --workspace`
