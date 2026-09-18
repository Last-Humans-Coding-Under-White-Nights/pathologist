# AGENTS.md — Contributor guide for trace

This file is for AI agents and human contributors working on the trace codebase.

## Repository map

| Crate | Purpose |
|-------|---------|
| `trace-ir` | IDs, types, symbol table, flow constraints, program container |
| `trace-preproc` | Custom C/C++ preprocessor (lexer, directives, LineMap, conditionals) |
| `trace-parse` | File discovery, include graph, tree-sitter parse, AST → IR lowering, merge, compile commands |
| `trace-analysis` | PAG construction, Andersen solver, call graph, arg flow, IPC bridges, function summaries |
| `trace-db` | SQLite schema and export (minimal/full/debug) |
| `trace-capi` | C ABI wrapper library (`libtrace_capi`), C header (`crates/trace-capi/include/trace.h`), FFI indexing and inspect API |
| `trace-cli` | CLI entry point (`analyze`, `inspect`, reporting examples) |

## Pipeline (do not reorder casually)

```
discover .c/.cpp/.h → IncludeGraph → preprocess (cache) → parse/lower per TU (C or C++ grammar) → merge → build PAG → solve → export SQLite
```

- **`.c`** and **`.cpp`/`.cc`/`.cxx`** files are indexed as TUs (grammar per extension); headers enter via `#include` in preprocessed source.
- **`merge_unit_index`** combines per-TU `UnitIndex` into one `Program` (remap ids, deduplicate header entities).
- **`CallReturn`** / **`fn_returns`** expand at PAG build (after merge), not at per-TU lower time.
- **C API** (`trace-capi`) exposes the same pipeline for external embedders and queries exported databases via SQLite.

Each stage must remain independently testable.

## Invariants

1. **LineMap**: Preprocessor must preserve mappable `(original_file, line, col)` for output offsets. All exported spans use **original** file/line/col (resolved via LineMap, cached expansions included); code inside macro expansions attributes to the expansion site's origin. Header-origin entities are deduplicated across TUs at merge time.
2. **Soundness**: May-analysis — over-approximate when uncertain (unknown index → array summary; instance-insensitive **`FieldSummary`** for struct fields).
3. **Phase boundaries**: Preprocessor must not depend on analysis. IR must not depend on SQLite. Analysis must not depend on SQLite.
4. **IDs**: Use newtype IDs from `trace-ir` (`FnId`, `VarId`, `CallSiteId`, `FieldId`, etc.). Do not use raw integers in public APIs.
5. **Internal linkage**: `static` functions are not in `fn_by_name`. A `static` class member is not one: it has external linkage wherever its body is written. Solver and `pag.expand_return_flows` must resolve scope-first, not through external-only `resolve_function` — and through the image-aware forms, `resolve_function_in_scope_in_target` / `resolve_function_candidates_in_target`, passing the caller's target. The unscoped wrappers name no image and are for callers that genuinely have none (lowering, which runs before targets exist).
6. **Storage classes**: file-scope `static` → `FileStatic`; function-local `static` → `FnStatic` (see `storage_for` in `lower.rs`).
7. **IPC detection (see `docs/IPC_ROADMAP.md`)**: Proxy/stub pairs are detected from class-name suffixes (`*Proxy`/`*Client` and `*Stub`) + `SendRequest` call presence; bridges match by interface + method-name correspondence. Detection is **pure** (reads `&Program`, returns `Vec<IpcBridge>`, no `Program` mutation, no control-flow/opcode analysis) and runs during PAG build; the solver injects a synthetic `CallGraphEdge` per bridge (`call_site_id = SYNTHETIC_CALL_SITE`). Synthetic sites must never be indexed into `Program.symbols.call_sites`. IPC pairing is deliberately **not** link-target scoped: the boundary it models is a process boundary, so proxy and stub normally belong to different link images and a bridge edge may cross `target_id` (see `docs/ANALYSIS.md`, "Link targets and weak symbols").
8. **Dependency roots (`--dep <PATH>`)**: Dependency roots isolate external build dependencies from code under analysis. Sources under dependency roots never become translation units. Headers in dependency roots contribute declarations only: function bodies merge as declarations (`is_defined = false`) without local variables, call sites, flow constraints, or return flows. All symbols from dependency roots are marked `is_dep = true`.
9. **Compilation databases (`--compile-commands`)**: Automatic discovery (`compile_commands.json` at target root or `build/`, or explicit flag). Commands provide per-TU include paths, macros, forced includes (`-include`), and language standards. CLI `--include` precedes compilation database `-I`, and CLI `-D` overrides database macros.
10. **Determinism and bit-reproducibility**: The pipeline must produce identical SQLite analysis data across runs with identical inputs (excluding run metadata such as `analysis_run.created_at`). Preprocessing cache discovery publishes to the expansion cache in unit order whatever the job count (`docs/PREPROCESSOR.md`, "Parallel discovery"), so header content never depends on scheduling. Distinct strong definitions of a C++ function from separate translation units are indexed separately; within a target, a matching strong definition takes precedence over a weak one (see `docs/ANALYSIS.md`, "Link targets and weak symbols"). In translation units defining a function, direct calls resolve exclusively to that unit's definition; units without a definition treat matching definitions as equal candidates. Solver iterations and PAG traversal order must remain deterministic.
11. **PAG and Solver monotonic convergence**: The Andersen solver uses worklist-based propagation over PAG constraints. Parameter copy wiring and points-to sets must grow monotonically toward a fixpoint. No flow-sensitive or path-sensitive branching is permitted.

## IR flow constraints (`trace-ir/src/flow.rs`)

Lowering emits `FlowConstraint` (+ `ReturnFlow` on functions). Document new kinds in `docs/ANALYSIS.md` before adding.

Current kinds:
- `Copy { dst, src }`: Pointer assignment (`p = q`).
- `AddrOfVar { dst, src }`: Address-of variable (`p = &x`).
- `AddrOfFn { dst, callee }`: Address-of function (`p = handler`).
- `Load { dst, src }`: Load through pointer (`y = *p`).
- `Store { dst, src }`: Store through pointer (`*p = y`, `field = val`).
- `GepField { dst, base, field, field_name }`: Field address or access (`&obj.field`, `p->field`). `field_name` disambiguates structurally distinct types sharing positional field offsets.
- `ArrayFnMember { array, callee }`: Function-pointer array initializer member (`{ fn0, fn1 }`).
- `CallReturn { dst, callee_name }`: Direct call assignment (`dst = callee()`), expanded during PAG construction using `fn_returns`.
- `CallReturnIndirect { dst, callee_var }`: Indirect/virtual call assignment (`dst = callee_var()`), callee resolved at analysis time from points-to sets.
- `NewHeap { dst }`: Heap allocation (`new T(...)`), allocates fresh heap location so constructor's implicit `this` has concrete pointees.
- `StringConst { dst, value }`: String literal constant interned as abstract location, enabling dynamic symbol lookup (`dlsym`, `GetProcAddress`).

Return-value flow (`ReturnFlow` in `program.fn_returns`):
- `AddrOfVar { src }`: `return &var`
- `AddrOfFn { callee }`: `return &fn`
- `Copy { src }`: `return local` or `return param`
- `Call { callee_name }`: `return other()` (transitive call return)

## Adding analysis constraints

1. Document constraint kind in `docs/ANALYSIS.md`
2. Add IR fact in `trace-ir/src/flow.rs` if needed; lower in `trace-parse/src/lower.rs`
3. Map to PAG in `trace-analysis/src/pag.rs` (`build_flow_constraints`)
4. Handle in `trace-analysis/src/solver.rs` worklist propagation
5. Add fixture C/C++ file + integration test

## Export / CLI

- Default export is **minimal** (call graph + arg-flow + PAG flow graph; see `trace-db/src/export.rs`).
- `--full-export`: types, all variables, `locations`
- `--debug-points-to`: retain/export points-to
- Document schema changes in `docs/SQLITE_SCHEMA.md` and `README.md`

## Adding preprocessor features

1. Document in `docs/PREPROCESSOR.md` first
2. Add lexer tests if new token kinds are needed
3. Add fixture under `tests/fixtures/preproc/` (builtin fallback macros go
   under `tests/fixtures/builtin_macros/`)
4. Update phase table (P0/P1/P2) in docs

## Libc and external summaries

Register external function summaries and function models (`FnModelSet`, loaded via `--models <FILE>`) in `trace-analysis/src/summaries.rs`. Document each summary's imprecision in `docs/ANALYSIS.md`.

## Tests

- Unit tests in each crate (`#[cfg(test)]`)
- Integration fixtures in `tests/fixtures/<name>/`
- Each fixture: `*.c` / `*.cpp` sources + optional `expected.json` metadata
- Run: `cargo test --workspace`
- Tests that need a scratch tree or a scratch SQLite file use `tempfile` (`tempfile::tempdir()`, or `common::TempDb` in `trace-cli` integration tests): the directory is removed when the value drops, on a failed assertion too, so no PID-suffixed paths and no manual `remove_dir_all`
- Eval corpora (OpenHarmony trees pinned by revision in `scripts/eval_expected.json`): `python3 scripts/fetch_corpora.py` once, then `python3 scripts/eval_check.py`; re-capture the expectations when a change legitimately moves the counts

## Do not

- Use Clang/gcc as the primary preprocessor (custom preproc is a project requirement)
- Commit `.db` files or `target/`
- Break workspace crate dependency direction (IR has no deps on analysis; analysis has no deps on DB)
- Add flow-sensitive analysis without explicit design approval
- Edit the plan file in `.cursor/plans/`
- Use raw integers for entity IDs in public APIs (use `FnId`, `VarId`, etc.)
- Use non-deterministic container iteration (e.g. iterating raw `HashSet` or unseeded `HashMap` when output order matters)

## Code quality

- Keep one source of truth for symbol identity, link-target ownership, and resolution precedence. Reuse the shared resolver and metadata instead of duplicating policies in lowering, analysis, or export.
- Keep shared SQL and export behavior in one place across minimal and full modes; extend existing helpers rather than maintaining parallel implementations.
- Preserve simple phase boundaries and explicit invariants. Avoid speculative abstractions and unnecessary scans or allocations in per-symbol and solver hot paths.

## Build commands

```bash
cargo build --workspace
cargo test --workspace
cargo run -p trace-cli --release -- analyze tests/fixtures/direct_call -o /tmp/out.db
```

Use `cargo run -p trace-cli --release -- …` (or rebuild `target/release/trace` after every compile) so benchmarks run the current binary.

## Common tasks

| Task | Where to look |
|------|---------------|
| Fix include resolution / graph | `trace-parse/src/deps.rs`, `trace-preproc/src/preprocessor.rs` |
| Return-value / call assignment flow | `trace-parse/src/lower.rs`, `pag.expand_return_flows` |
| Static / internal call resolution | `symbol.rs` (`resolve_function_in_scope_in_target`), `solver.rs`, `pag.rs` |
| Link targets / weak symbols | `link_commands.rs` (reading build metadata), `target_merge.rs` (per-image scopes and weak selection), `symbol.rs` (`TargetScope`) |
| Fn-ptr arg-flow export | `solver.rs` (`extract_arg_flow`), `export.rs`, `arg_flow_edges.actual_fn_id` |
| Flow-graph export / inspect queries | `export.rs` (`export_flow_graph`), `inspect.rs` |
| Field summary / GEP fallback | `trace-analysis/src/pag.rs`, `solver.rs` |
| New SQLite column | `trace-db/src/schema.rs`, `export.rs`, `docs/SQLITE_SCHEMA.md` |
| Parse new C/C++ construct | `trace-parse/src/lower.rs` |
| C++ virtual dispatch & hierarchy | `trace-ir/src/program.rs` (`inheritance`), `trace-parse/src/lower.rs` (`expand_virtual_overrides`) |
| C++ smart pointer unwrapping | `trace-parse/src/lower.rs` (`ArrowReturn`), `symbol.rs` |
| C++ class-template member return substitution | `trace-ir/src/program.rs` (`TemplateReturn`), `trace-parse/src/lower.rs` (`register_template_return`, `substituted_template_return`), `merge.rs` → `docs/ANALYSIS.md` |
| Type of a call-expression receiver (`S::Get().Open()`) | `trace-parse/src/lower.rs` (`CallResult`, `call_result_shape`, `receiver_desc`) → `docs/ANALYSIS.md` |
| C API functions / FFI bindings | `crates/trace-capi/src/`, `crates/trace-capi/include/trace.h`, `docs/CAPI.md`, `Doxyfile` |
| Dependency roots (`--dep`) | `trace-parse/src/configured.rs`, `merge.rs`, `trace-db/src/inspect.rs`, `trace-cli/src/main.rs` |
| Measure what the configuration excludes (`#if` arms not taken) | `PreprocessOptions::record_conditionals` + `PreprocessResult::conditionals` (`trace-preproc/src/conditionals.rs`), `trace-cli/examples/conditional_coverage.rs`, `scripts/gen_conditional_coverage_report.py` → `docs/CONDITIONAL_COVERAGE.md` |
| Bounded conditional-variant exploration (`--explore`) | `PreprocessOptions::explore` + `explore_budget`, `trace-parse/src/explore.rs`, `gn_defines.rs`, `merge_unit_variants`, `lower.rs` → `docs/ANALYSIS.md` |
| Parallel discovery pass (expansion-cache writes) | `trace-parse/src/expansion_discovery.rs`, `trace-preproc/src/journal.rs` → `docs/PREPROCESSOR.md` |
| Compilation database (`--compile-commands`) | `trace-parse/src/compile_commands.rs`, `configured.rs`, `merge_unit_variants` → `docs/ANALYSIS.md` |
| Function models / summaries | `trace-analysis/src/summaries.rs` (`FnModelSet`, `--models`) → `docs/ANALYSIS.md` |
| Builtin fallback macros | `trace-preproc/src/preprocessor.rs`, `tests/fixtures/builtin_macros/` → `docs/PREPROCESSOR.md` |
