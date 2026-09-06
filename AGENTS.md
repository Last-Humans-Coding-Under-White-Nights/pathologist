# AGENTS.md — Contributor guide for trace

This file is for AI agents and human contributors working on the trace codebase.

## Repository map

| Crate | Purpose |
|-------|---------|
| `trace-ir` | IDs, types, symbol table, program container |
| `trace-preproc` | Custom C preprocessor (lexer, directives, LineMap) |
| `trace-parse` | File discovery, tree-sitter parse, AST → IR lowering |
| `trace-analysis` | PAG construction, Andersen solver, call graph, arg flow |
| `trace-db` | SQLite schema and export |
| `trace-cli` | CLI entry point |

## Pipeline (do not reorder casually)

```
discover .c/.cpp/.h → IncludeGraph → preprocess (cache) → parse/lower per TU (C or C++ grammar) → merge → build PAG → solve → export SQLite
```

- **`.c`** and **`.cpp`/`.cc`/`.cxx`** files are indexed as TUs (grammar per extension); headers enter via `#include` in preprocessed source.
- **`merge_unit_index`** combines per-TU `UnitIndex` into one `Program` (remap ids).
- **`CallReturn`** / **`fn_returns`** expand at PAG build (after merge), not at per-TU lower time.

Each stage must remain independently testable.

## Invariants

1. **LineMap**: Preprocessor must preserve mappable `(original_file, line, col)` for output offsets. All exported spans use **original** file/line/col (resolved via LineMap, cached expansions included); code inside macro expansions attributes to the expansion site's origin. Header-origin entities are deduplicated across TUs at merge time.
2. **Soundness**: May-analysis — over-approximate when uncertain (unknown index → array summary; instance-insensitive **`FieldSummary`** for struct fields).
3. **Phase boundaries**: Preprocessor must not depend on analysis. IR must not depend on SQLite.
4. **IDs**: Use newtype IDs from `trace-ir` (`FnId`, `VarId`, etc.). Do not use raw integers in public APIs.
5. **Internal linkage**: `static` functions are not in `fn_by_name`. Solver and `pag.expand_return_flows` must use `resolve_function_in_scope(name, Some(file))`, not external-only `resolve_function`.
6. **Storage classes**: file-scope `static` → `FileStatic`; function-local `static` → `FnStatic` (see `storage_for` in `lower.rs`).
7. **IPC detection (see `docs/IPC_ROADMAP.md`)**: Proxy/stub pairs are detected from class-name suffixes (`*Proxy`/`*Client` and `*Stub`) + `SendRequest` call presence; bridges match by interface + method-name correspondence. Detection is **pure** (reads `&Program`, returns `Vec<IpcBridge>`, no `Program` mutation, no control-flow/opcode analysis) and runs during PAG build; the solver injects a synthetic `CallGraphEdge` per bridge. Opcode-level analysis is deferred to IDL-aware v2.

## IR flow constraints (`trace-ir/src/flow.rs`)

Lowering emits `FlowConstraint` (+ `ReturnFlow` on functions). Document new kinds in `docs/ANALYSIS.md` before adding.

Current kinds: `Copy`, `AddrOfVar`, `AddrOfFn`, `Store`, `Load`, `GepField`, `ArrayFnMember`, `CallReturn`, `CallReturnIndirect`, `NewHeap`, `StringConst`.

## Adding analysis constraints

1. Document constraint kind in `docs/ANALYSIS.md`
2. Add IR fact in `trace-ir/src/flow.rs` if needed; lower in `trace-parse/src/lower.rs`
3. Map to PAG in `trace-analysis/src/pag.rs` (`build_flow_constraints`)
4. Handle in `trace-analysis/src/solver.rs` worklist propagation
5. Add fixture C file + integration test

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

## Libc summaries

Register external function summaries in `trace-analysis/src/summaries.rs`. Document each summary's imprecision in `docs/ANALYSIS.md`.

## Tests

- Unit tests in each crate (`#[cfg(test)]`)
- Integration fixtures in `tests/fixtures/<name>/`
- Each fixture: `*.c` sources + optional `expected.json` metadata
- Run: `cargo test --workspace`
- Tests that need a scratch tree or a scratch SQLite file use `tempfile` (`tempfile::tempdir()`, or `common::TempDb` in `trace-cli` integration tests): the directory is removed when the value drops, on a failed assertion too, so no PID-suffixed paths and no manual `remove_dir_all`
- Eval corpora (OpenHarmony trees pinned by revision in `scripts/eval_expected.json`): `python3 scripts/fetch_corpora.py` once, then `python3 scripts/eval_check.py`; re-capture the expectations when a change legitimately moves the counts

## Do not

- Use Clang/gcc as the primary preprocessor (custom preproc is a project requirement)
- Commit `.db` files or `target/`
- Break workspace crate dependency direction (IR has no deps on analysis)
- Add flow-sensitive analysis without explicit design approval
- Edit the plan file in `.cursor/plans/`

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
| Static / internal call resolution | `symbol.rs` (`resolve_function_in_scope`), `solver.rs`, `pag.rs` |
| Fn-ptr arg-flow export | `solver.rs` (`extract_arg_flow`), `export.rs`, `arg_flow_edges.actual_fn_id` |
| Flow-graph export / inspect queries | `export.rs` (`export_flow_graph`), `inspect.rs` |
| Field summary / GEP fallback | `trace-analysis/src/pag.rs`, `solver.rs` |
| New SQLite column | `trace-db/src/schema.rs`, `export.rs`, `docs/SQLITE_SCHEMA.md` |
| Parse new C construct | `trace-parse/src/lower.rs` |
