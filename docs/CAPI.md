# C API (`trace-capi`)

A C ABI over the trace engine, split into the same two stages as the CLI:

1. **Index** — `trace_index` runs the whole pipeline (discover → preprocess →
   parse/lower → merge → solve) against a project directory and writes a
   SQLite database.
2. **Inspect** — open the database and query functions/symbols by source
   position, list call edges, and traverse bounded call / value-flow graphs.

The header is `crates/trace-capi/include/trace.h`. The library builds as a
`cdylib` (`libtrace_capi.so` / `.dylib` / `.dll`) and a `staticlib`
(`libtrace_capi.a`); the Rust crate is also usable directly (`rlib`).

## Build

```bash
cargo build -p trace-capi --release
# -> target/release/libtrace_capi.{so,dylib,dll,a}
```

## Quick start (C)

```c
#include <trace.h>

int main(void) {
    trace_index_options opts = {0};
    opts.size = sizeof(opts);
    opts.root = "/path/to/project";
    opts.output_db = "/tmp/project.db";

    trace_index_result r;
    char *err = NULL;
    if (trace_index(&opts, &r, &err) != TRACE_OK) {
        fprintf(stderr, "index failed: %s\n", err);
        trace_string_free(err);
        return 1;
    }
    printf("%llu functions indexed\n", (unsigned long long)r.functions);

    trace_db *db = trace_db_open("/tmp/project.db", &err);
    trace_function_list fns = {0};
    /* find the function containing main.c:8 */
    if (trace_db_find_functions(db, "main.c", 8, &fns, &err) == TRACE_OK) {
        for (size_t i = 0; i < fns.count; i++)
            printf("fn %lld: %s (%s)\n", (long long)fns.items[i].id,
                   fns.items[i].name, fns.items[i].path);
        trace_function_list_free(&fns);
    } else if (err) {
        fprintf(stderr, "%s\n", err);
        trace_string_free(err);
    }
    trace_db_close(db);
    return 0;
}
```

Full featured example: `crates/trace-capi/examples/ctrace.c` (a small CLI with
`analyze` / `functions` / `symbols` / `calls` / `callgraph` / `dataflow`
subcommands).

## Memory-safety model

The tricky part of any C API is who owns what. The rules here:

| Item | Owner | Lifetime |
|------|-------|----------|
| `trace_db` handle | caller | until `trace_db_close` |
| `char **out_err` payloads | caller | free with `trace_string_free` |
| Strings inside a result (`name`, `path`, `label`, `detail`, …) | the result object | until the matching `trace_*_free` |
| `items` arrays in a result | the result object | freed by the same `trace_*_free` |
| Inputs (`const char *`, `trace_index_options`, `trace_symbol[]`) | caller, borrowed | copied during the call |

**One arena per query result.** Every query builds a result object that owns
an append-only arena (`crates/trace-capi/src/util.rs`). All strings of the
result are copied into that arena once; the node/edge/function/symbol arrays
reference them by pointer. Freeing the result frees the arena and therefore
every string — C never frees the strings individually, and there is no
query-to-query invalidation. A pointer you grabbed yesterday is still valid
today, as long as you have not freed the result it came from.

On the Rust side this is memory-safe by construction: each `CString` is heap
allocated once and its buffer never moves, so pointers returned by
`Arena::add` are stable for the arena's life. Reallocation of the arena's
string `Vec` moves the `CString` values but not the buffers they own.

### Cross-boundary hazard checklist

- **No panic escapes.** Every entry point runs inside
  `std::panic::catch_unwind`; a panic surfaces as
  `TRACE_ERR_PANIC` (or an `internal panic:` message). See
  `crates/trace-capi/src/util.rs` (`guard`).
- **Borrowed inputs are copied.** All C strings and option arrays are copied
  into Rust-owned memory before the pipeline/query runs, so nothing dangles
  when the caller's buffers go out of scope.
- **Handles are opaque and single-threaded.** `TraceDb` wraps a
  `rusqlite::Connection` (`!Send`) opened with
  `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`. Read-only opens do not
  create the file: opening a missing or unreadable path fails at open time
  with an error instead of silently producing an empty database that the
  first query errors on. Do not share handles across threads; each query is
  safe only on the thread that owns the handle.
- **Zeroing on free.** Result free functions zero the whole C struct
  (`items`/`count`/`_impl` are cleared), so a stale pointer is detectable
  rather than silently reused; calling a free again on a zeroed struct is a
  no-op.
- **Status codes are typed, not text-sniffed.** Every entry point returns a
  domain error classified at its creation site (`crates/trace-capi/src/util.rs`,
  `ApiError`): bad caller input / ABI mismatch → `TRACE_ERR_INVALID_ARG`,
  filesystem problems → `TRACE_ERR_IO`, an absent queried entity →
  `TRACE_ERR_NOT_FOUND`, pipeline/query failures → `TRACE_ERR_ANALYSIS`, and a
  caught panic → `TRACE_ERR_PANIC`. `trace_index` preflights the output
  database path (`preflight_output`, `index.rs`) and reports a bad output path
  as `TRACE_ERR_IO` before running the pipeline; the probe exactly mirrors the
  exporter, so it creates a missing parent directory (like the CLI does) and
  never leaves a stale 0-byte file at `output_db` on a later failure.
- **Strict argument validation.** Everything that is a program error is
  rejected at the boundary with `TRACE_ERR_INVALID_ARG` instead of being
  coerced or deferred:
  - `trace_direction` is accepted as a raw `i32` (C enums are ints; the Rust
    enum can't represent out-of-band values) and validated: only 0 (DOWN)
    and 1 (UP) are accepted, anything else returns `TRACE_ERR_INVALID_ARG`.
    This matters because direction is a behavior switch, not data: coercing
    an unknown value to one direction would silently return wrong results as
    `TRACE_OK`. See `check_dir` in `inspect.rs`.
  - `depth == 0`, `n_roots == 0`, empty `file` filters and null handles all
    return `TRACE_ERR_INVALID_ARG`.
  - `trace_symbol.kind` is a plain `int32_t` (not the C enum), so out-of-band
    discriminants written by a caller are ordinary integers rather than
    undefined behavior when the struct is read back into `trace_db_dataflow`.
    An invalid `kind` on a dataflow root is still rejected with
    `TRACE_ERR_INVALID_ARG` (see `check_symbol_kind` in `inspect.rs`).
- **ABI guard on `trace_index_options`.** `opts->size` must be at least
  `sizeof(trace_index_options)`; smaller values are rejected with
  `TRACE_ERR_INVALID_ARG` (forward compatible: larger structs are accepted).
  `size == 0` opts out of the check for easy zero-init callers.
- **`*out_err` is cleared on entry.** Every function nulls `*out_err` before
  doing anything, and writes it only on failure. A caller that reuses one
  buffer across calls can never observe a stale (already-freed) pointer; the
  rule is: check the status, then `if (*out_err) { free it }`.

## Status codes

`TRACE_OK` (0) on success. Errors: `TRACE_ERR_INVALID_ARG`,
`TRACE_ERR_IO`, `TRACE_ERR_ANALYSIS`, `TRACE_ERR_NOT_FOUND`,
`TRACE_ERR_PANIC`. When a call fails and `out_err` is non-null, `*out_err`
holds a heap message the caller frees with `trace_string_free`.

Each code is produced by the specific path that classifies it in `ApiError`
(`crates/trace-capi/src/util.rs`), never by message inspection:

- `TRACE_ERR_INVALID_ARG` — null/overshort pointers, malformed arrays, invalid
  `trace_direction`, invalid `trace_symbol.kind`, `depth == 0`, `n_roots == 0`,
  out-of-band `trace_index_options.size`.
- `TRACE_ERR_IO` — output-path problems from the `trace_index` preflight, and
  database open failures in `trace_db_open`.
- `TRACE_ERR_NOT_FOUND` — absent queried entities, e.g. a call-graph root id
  with no matching function. The `From<anyhow::Error>` adapter classifies a
  `not found` / `no value-flow node` message from the inspect layer as
  `NOT_FOUND`; everything else from the query layer is `ANALYSIS`.
- `TRACE_ERR_ANALYSIS` — pipeline/query failures (discovery, parse, export,
  graph queries).
- `TRACE_ERR_PANIC` — a Rust panic caught at the boundary (`internal panic: …`).

## Enums vs. strings

Fields with bounded domains cross the boundary as C enums, not strings:

- edge `resolution` — `TRACE_RESOLUTION_*` (call graphs; `TRACE_RESOLUTION_IPC`
  is the no-call-site synthetic edge injected for an IPC proxy/stub pair)
- edge `flow_kind` — `TRACE_FLOW_*` (dataflow)
- node `kind` / `loc_kind` — `TRACE_NODE_*` / `TRACE_LOC_*` (dataflow)
- symbol `kind` — `TRACE_SYM_*` (crosses the boundary as a raw `int32_t`, not
  the C enum, so invalid values can be validated instead of being read as an
  out-of-band enum discriminant — see "Strict argument validation")

Every enum has a negative `_UNKNOWN` sentinel because databases may hold
values this library build does not recognize; C consumers must treat unknown
values as opaque rather than panic. Open-ended text (`label`, `detail`,
`name`, `path`) stays a string.

`TRACE_DIRECTION_*` is the one input enum, and it has no sentinel: it is
validated strictly at the boundary (see "Strict argument validation"), so an
out-of-band value is an error instead of silently running in one direction.

## Inspect details

- **Positions.** Function/symbol lookup takes a file-path *substring* plus
  line (and column), mirroring `trace inspect`. The best match is `items[0]`.
- **Call edges.** `trace_db_call_edges(db, from, to, file, …)` lists edges,
  filtered like `trace calls --from/--to/--file`. `caller_path` is the
  *caller's own file* (not the call-site file), so it stays meaningful for
  synthetic IPC-bridge edges that have no call site; `path`/`line`/`col` are
  the actual call site and are empty (0) for synthetic edges.
- **Call graphs.** `trace_db_callgraph(db, root_fn_id, direction, depth)` BFS
  over `call_edges`. Node `id`s are `functions.id`; node `kind` is
  `TRACE_NODE_UNKNOWN` (call-graph nodes are functions, not PAG nodes). Edges
  carry `resolution` plus the full call-site path/line/col.
- **Dataflow.** `trace_db_dataflow(db, symbols, n, direction, depth)` BFS over
  `flow_edges`. Pass a `trace_symbol` array obtained from
  `trace_db_find_symbols` (its `var_id` identifies the start variables). Pass
  the single best candidate (`items[0]`) to start from one variable, matching
  the CLI, whose lookup resolves position to one declaration. Node
  `kind`/`loc_kind` describe the PAG nodes; edges carry `flow_kind`;
  `path`/`line`/`col` are empty because value-flow edges have no call site.
- **`truncated`** is set when real neighbors remained beyond `depth`.

## Extending the API

1. Add the `#[repr(C)]` type in `crates/trace-capi/src/types.rs` and the
   mirroring typedefs/enums in `include/trace.h`.
2. Implement the query in `crates/trace-capi/src/inspect.rs` (result built
   via `Arena` + a leaked `Vec`; free function re-takes the `Box`).
3. Cover it with a Rust test in `src/inspect.rs` (`mod tests`) and, when it
   is consumer-facing C surface, a subcommand in `examples/ctrace.c`.
4. Rebuild the cdylib for the C example in CI (`.github/workflows/ci.yml`).

## Tests

```bash
cargo test -p trace-capi          # Rust unit tests over the extern ABI
cargo build -p trace-capi         # cdylib needed by the C example
# build the C CLI and run it against a fixture (also done in CI):
cc crates/trace-capi/examples/ctrace.c \
   -I crates/trace-capi/include \
   -L target/debug -ltrace_capi -o target/ctrace
LD_LIBRARY_PATH=target/debug ./target/ctrace analyze tests/fixtures/direct_call -o /tmp/t.db
LD_LIBRARY_PATH=target/debug ./target/ctrace inspect /tmp/t.db callgraph --file main.c --line 1
```