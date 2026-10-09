![](docs/images/Logo-big.png)

# trace

**trace** is a static analysis tool for C and C++ codebases. It runs a custom preprocessor, parses translation units with [tree-sitter](https://tree-sitter.github.io/), performs Andersen-style field-sensitive pointer analysis, and exports call graphs and interprocedural argument-flow facts to SQLite.

Typical uses:

- Find **direct and indirect** call targets (function pointers, vtables, struct op tables).
- Trace **argument flow** from call-site actuals to callee formals.
- Query results with **`trace inspect`**, ad-hoc SQL, or programmatic C API (**`trace-capi`**).

## Build

```bash
# Build CLI binary
cargo build --release
# binary: target/release/trace

# Or build CLI binary with mimalloc allocator for ~20% faster indexing on large workloads
# (source-builds opt-in feature; prebuilt release artifacts use the system allocator;
# measured with mimalloc 0.1 / upstream mimalloc v3):
cargo build -p trace-cli --release --features mimalloc
# binary: target/release/trace

# Or build C API library (staticlib and cdylib)
cargo build -p trace-capi --release
# library: target/release/libtrace_capi.{so,dylib,dll,a}

# Or build cross-repository merger binary
cargo build -p trace-merge --release
# binary: target/release/trace-merge
```

Ability-runtime benchmark build options and measurements are in the
[evaluation report](docs/EVAL_REPORT.md#ability-runtime-acceptance-and-performance).

Run the workspace test suite:

```bash
cargo test --workspace
```

## Quick start

```bash
trace analyze ./tests/fixtures/direct_call -o /tmp/trace.db
trace inspect /tmp/trace.db calls
trace inspect /tmp/trace.db calls --from main
trace inspect /tmp/trace.db calls --from caller --to helper
```

Analyze a large tree (parallel indexing, minimal SQLite export):

```bash
trace analyze /path/to/project -o /tmp/project.db --jobs 8
```

## Editor call hierarchy

Build the separate snapshot language server and point it at an existing database:

```bash
cargo build -p trace-lsp --release
trace analyze ./project -o analysis.db
target/release/trace-lsp --db analysis.db
```

`trace-lsp` serves standard LSP incoming/outgoing call hierarchy over stdio.
Default minimal exports work; no points-to export is needed. It opens schema v7
read-only and leaves the analyzer and database format unchanged. See
[LSP configuration and editor example](docs/LSP.md) for path mapping, range
precision, snapshot behavior, and limitations.

## CLI reference

### Conditional coverage and GN define evidence

```bash
cargo run -p trace-cli --release --example conditional_coverage -- /path/to/project > coverage.tsv
```

The reporting example records conditional branches and scans `BUILD.gn`,
`*.gni` and `*.gn` for direct string entries in `defines = [...]` and `defines += [...]`.
`GN_DEFINE` TSV rows retain the macro name, whether a value was supplied, the
value, file, entry line, confidence, and enclosing GN conditions. No inferred
define is applied. See [GN evidence](docs/GN_DEFINES.md) for ranking and limits;
`scripts/gen_conditional_coverage_report.py` renders these candidates in a local
report; see [conditional coverage reporting](docs/CONDITIONAL_COVERAGE.md).

### `trace analyze`

Analyze every C/C++ file (`.c`, `.cpp`, `.cc`, `.cxx`) under `TARGET` and write results to SQLite.

```text
trace analyze [OPTIONS] <TARGET>
```

| Option | Description |
|--------|-------------|
| `<TARGET>` | Root directory to scan recursively for `*.c` / `*.cpp` / `*.cc` / `*.cxx` files. |
| `-o`, `--output <PATH>` | Output database path. Default: `trace.db`. |
| `--include <PATH>` | Add a preprocessor `#include` search path. Repeatable. |
| `-D <NAME>` | Define preprocessor macro `NAME=1`. Repeatable. |
| `-D <NAME=VALUE>` | Define macro with explicit value. Repeatable. Overrides the language predefines (`__cplusplus`, `__STDC_VERSION__`, `__STDC__`) when the name matches. |
| `--compile-commands <PATH>` | Compilation database to read. Default: auto-discovery at the target root, then `build/`; otherwise root `compile_flags.txt`. |
| `--system-includes` | Query the compiler for its effective system header search paths. Off by default. |
| `--link-commands <PATH>` | Link commands database to read, establishing which sources each program links. Default: auto-discovery (`link_commands.json` at the target root or `build/`, link entries in the compilation database, or a CMake File API reply). Enables per-program weak-symbol resolution; see [link targets and weak symbols](docs/ANALYSIS.md#link-targets-and-weak-symbols). |
| `--jobs <N>` | Parallel jobs for indexing (parse + lower). Default: logical CPU count. |
| `--timeout-secs <N>` | Watchdog: abort the process after N seconds (exit 124). Useful when probing hang-prone trees. |
| `--full-export` | Export full IR detail: all types, all variables, PAG `locations`. Slower and produces a larger database. |
| `--debug-points-to` | Retain points-to sets during analysis and export the `points_to` debug table (requires PAG in memory). Implies keeping location data needed for export. |
| `--models <FILE>` | Load a TOML function-model file (interprocedural summaries for bodyless callees, e.g. `memcpy_s`, callback APIs and the execution context they start, and noise macro lists). Repeatable; later files override earlier entries and built-ins. See `docs/ANALYSIS.md`. |
| `--ignore-macro <NAME>` | Ignore expansions of the named macro during AST lowering (repeatable). Glob wildcards (`*`) are supported (e.g. `--ignore-macro 'LOG*'`). Suppresses boilerplate call sites, temporary variables, and flow constraints. |
| `--ignore-logging` | Preset flag to ignore common OpenHarmony and standard logging macros (`HILOG_*`, `TAG_LOG*`, `HIVIEW_LOG*`, `MEDIA_*_LOG`, `LOGD`, `LOGI`, `LOGW`, `LOGE`, `LOGF`). |
| `--no-test-partition` | Disable the [bare-tree test partition](docs/ANALYSIS.md#declaring-header-eligibility). |
| `--test-dir <NAME>` | Replace the partition’s directory names (repeatable). Defaults, validation, and precedence: [Declaring-header eligibility](docs/ANALYSIS.md#declaring-header-eligibility). |
| `--dep <PATH>` | Treat a directory as a dependency root: a tree the target builds against but that is not under analysis (repeatable). Its headers contribute declarations — types, class definitions, inheritance, prototypes, declared return types — while its sources are never translation units. Function bodies and variable initializers are skipped during lowering, so they contribute no call sites or value flow. See the note below. |
| `--explore` | Enable bounded conditional-variant exploration. Discovers candidate macro definitions from project GN files (`BUILD.gn`, `*.gni`), evaluates semantic feasibility of excluded `#if`/`#ifdef`/`#elif` arms, preprocesses and lowers feasible variants independently, and unions their facts into the merged program (preserving body and call facts plus struct fields across configurations; conditional signatures retain the base arity, see [limits](docs/ANALYSIS.md#limits-of---explore)). Off by default. |
| `--explore-budget <N>` | Maximum additional configurations per translation unit (default: 4). Budget diagnostics count omitted candidate activation goals, not proven reachable configurations. |
| `--no-ipc` | Disable IPC proxy→stub bridge edge detection (enabled by default). Bridge edges are synthetic (`resolution = 'ipc'`, `call_site_id = NULL`) and connect a `*Proxy*` method to its `*Stub*` handler across the opaque Binder boundary. See `docs/IPC_ROADMAP.md`. `.idl` interfaces are synthesized into declarations-only headers so IDL-generated services bridge too (see docs/ANALYSIS.md, "IDL-generated interfaces"). |
| `--solve-budget-pops <N>` | Solver work budget in worklist pops. Default derives from the PAG constraint count (`800000 + 6×constraints`), so normal trees converge and huge ones finish instead of stopping at a partial result; `0` = unlimited. `TRACE_SOLVE_BUDGET_POPS` overrides it for experimentation. A truncated run is recorded, not silent: `analysis_run.options_json.solver_partial`, an `analyze`-stage diagnostic, and a `trace inspect` warning. |
| `--solve-budget-secs <N>` | Solver wall-clock budget in seconds (default: off; `0` = no time limit). Checked periodically, so lead time can overshoot by up to one checkpoint, and the stop point varies with machine load. |

**Progress output** (stderr):

```text
discover: 618 TUs, 200 headers under /path
include-graph: 818 files, 1200 include edges
warm: 1/90 /path/foo.h
parse: 0 orphan headers, 618 TUs (jobs=8)
index: 24.2s (618 files, 11442 functions, 48406 flow)
analyze: 0.3s (25478 edges, 3468 indirect)
export: 0.1s
analysis complete: 11442 functions, 25478 call edges, 25803 arg-flow edges -> trace.db
```

SQLite export builds most secondary indexes after loading rows, before committing
and publishing the database, to reduce bulk insertion work.

Without build metadata, indexing spills large preprocessed source text
and LineMaps to automatically cleaned temporary files and loads them for
parsing. Discovery drops payloads that the settle pass must rebuild. After
header lowering, settled TUs can release frozen header expansions and raw
source copies before the ordered merge. Parallel parsing reserves estimated
source bytes until each unit has merged, with a byte budget and a count safety
cap. Small units can run further ahead of a slow unit; workers still wait when
either limit fills. Compilation-command indexing retains its previous source-count
limit. See
[Parsing memory limits](docs/PERFORMANCE_REVIEW.md#6-make-parsing-memory-limits-independent-of-cpu-count).
Cached header type tables
discard lowering-only indexes and layouts once their merge descriptors are
ready. Type tables share immutable descriptors across headers and TUs while
retaining local IDs. On Linux/glibc, indexing returns freed heap pages at phase
boundaries and between ordered TU merge batches. On macOS `analyze` runs
under libmalloc's space-efficient mode, which holds the peak footprint at
the live heap size for a few percent of wall time on large trees: it
re-executes itself once with `MallocSpaceEfficient=1` (same pid and
`argv[0]`). A `MallocSpaceEfficient` already set to `0` or `1` is
respected, so `MallocSpaceEfficient=0 trace analyze …` keeps the default
allocator; an empty or invalid value, which libmalloc warns about on every
run, is replaced. `inspect` never re-executes, nor does a build with the
`mimalloc` feature. The mode in effect is the `malloc:` line at the start
of the progress output. See
[memory measurements](docs/EVAL_REPORT.md#compacting-cached-header-type-tables--2026-09-28),
[macOS space-efficient malloc](docs/EVAL_REPORT.md#macos-space-efficient-malloc--2026-09-30),
the [memory investigation](docs/MEMORY_PROFILE.md), and the
[macOS measurements](docs/PERFORMANCE_REVIEW.md#incremental-per-tu-ir-cache-macos-measurements-and-decision-175)
(`scripts/profile_memory_macos.py`; on macOS compare `phys_footprint`, not RSS).

**Examples**

```bash
# HDF-style tree with extra include roots
trace analyze ~/drivers_hdf_core -o /tmp/hdf.db \
  --include ~/drivers_hdf_core/framework/core/common/include \
  -D __LITEOS__ -D CONFIG_XXX=1

# Debug pointer analysis
trace analyze ./my_app -o /tmp/debug.db --debug-points-to --full-export

# Use a compilation database outside the source tree
trace analyze ./my_app --compile-commands ./out/compile_commands.json -o /tmp/app.db

# Include installed compiler system headers as declaration-only dependencies
trace analyze ./my_app --system-includes -o /tmp/app.db
```

**Notes**

- **`.c` and `.cpp`-family files** are indexed as translation units. Headers are pulled in via `#include` during preprocessing, not analyzed as standalone TUs. A C++ unit is preprocessed with `__cplusplus` (`201703L`) and `__STDC__` predefined, a C unit with `__STDC__` and `__STDC_VERSION__` (`201710L`), so `#ifdef __cplusplus` takes the C++ arm in `.cpp` units and the C arm in `.c` units, headers included. C++ support is a pragmatic first step — see [docs/ANALYSIS.md](docs/ANALYSIS.md) for scope and imprecision.
- Line numbers in the database refer to **original** files on disk (resolved through the preprocessor's `LineMap`); call sites inside macro expansions attribute to the expansion site.
- **Compilation database** — automatically reads `compile_commands.json` at the target root, then `build/compile_commands.json`, or an explicit `--compile-commands PATH`. Each entry supplies its working directory, ordered `-I`/`-iquote`/`-isystem` paths, ordered `-D`/`-U`, `-include`, and `-x`/`-std`. MSVC `cl`/`clang-cl` preprocessing switches (`/I`, `/D`, `/U`, `/FI`, `/TC`, `/TP`, `/Tc`, `/Tp`, `/std:`) are also supported. All applicable commands for a source contribute facts, even without `--explore`; link-object membership restricts them to their target. CLI `--include` paths precede database `-I` paths and CLI `-D` values override database macros. When no JSON database is selected, root `compile_flags.txt` supplies shared flags to every discovered source (the parent directory for a single-file analysis). It contains one literal argument per line, preserving spaces; relative paths use its directory. The formats are never combined. Unreadable or malformed flags produce configuration diagnostics and fall back to inferred configuration. Files without a usable entry retain inferred configuration; a database is never required. See [compilation database support](docs/ANALYSIS.md#compilation-databases-62).
- **System headers** — `--system-includes` queries the compilation command's compiler for its effective search paths, with results cached by compiler and configuration. Sources lacking a usable compilation command share one project fallback compiler: GCC first, then Clang. Reached system headers contribute declarations and macros as dependencies. Explicit `-isystem`, `-idirafter`, and library-supplied system paths continue to work without the flag. See [compiler include discovery](docs/PREPROCESSOR.md#compilation-database-options-62).
- **GN target inference** — automatic bare-tree target discovery follows the [target inference contract](docs/ANALYSIS.md#link-targets-and-weak-symbols).
- **Link targets and weak symbols** — `--link-commands PATH`, auto-discovered `link_commands.json`, mixed compilation/link databases, and CMake File API replies establish target scopes. Strong definitions override weak fallbacks only in targets that contain them; shared sources retain separate bindings per target. See [resolution rules and limitations](docs/ANALYSIS.md#link-targets-and-weak-symbols).
- **`static` functions** (internal linkage) and **file-scope `static` variables** are resolved within the defining translation unit. **`static` locals** inside functions are tracked as `fn_static` storage.
- **Dependency roots (`--dep <PATH>`)** separate what the target *uses* from what it *is*. A dependency's headers are reached and merged for their declarations — smart-pointer wrappers such as `sptr<T>`, base classes, external interfaces — so a wrapper-typed receiver resolves on the wrapped class rather than producing an edge on the wrapper. Its sources are never translation units, its unreached headers are never indexed as standalone units, and a body written in a dependency header merges as a declaration (`is_defined = 0`) with no call sites, locals, value flow or return flow. Files and functions from a dependency root export with `is_dep = 1`; `trace inspect calls --exclude-deps` drops the edges that touch them. A dependency root nested inside the analysis root is fine; one that contains or equals it is rejected at startup, since every source would become a dependency and nothing would be left to analyze.

## `static` storage support

| Context | IR / export | Call / flow resolution |
|---------|-------------|------------------------|
| File-scope `static` function | `linkage = internal` | Direct calls and `CallReturn` via scope-aware name lookup (`file` + name) |
| File-scope `static` variable | `kind = file_static` | Persistent PAG location; name lookup scoped to its file and the files including it |
| Function-local `static` variable | `kind = fn_static` | Persistent PAG location within enclosing function |
| External (non-`static`) symbols | `linkage = external` | Global `fn_by_name` / `global_by_name` tables |

Same identifier in different `.c` files (each `static`) gets distinct IR ids; resolution uses the call site's file.

### `trace inspect`

Query an existing analysis database.

```text
trace inspect <DB> calls [--from FN] [--to FN] [--file SUBSTR] [--exclude-deps]

Edges print as `caller (file:line) -> callee [deffile] (resolution)` — the
`[deffile]` bracket distinguishes same-name (e.g. `static`) functions defined
in different files; `--file` filters ordinary edges by call-site or callee
file. A synthetic edge has no call site, so its caller definition file is used
instead.
```

| Option | Description |
|--------|-------------|
| `<DB>` | Path to SQLite file produced by `trace analyze`. |
| `--from <FN>` | Filter edges where the **caller** name equals `FN` or ends with `::FN` (C++ qualified methods). `_` and `%` in `FN` are literal, not `LIKE` wildcards. |
| `--to <FN>` | Filter edges where the **callee** name equals `FN` or ends with `::FN`. Same escaping as `--from`. |
| `--file <SUBSTR>` | Filter ordinary edges by call-site or callee file; synthetic edges by caller or callee definition file. |
| `--callgraph-filter <FILE>` | JSON file listing regex patterns over function names; edges whose caller and callee both fail to match are hidden. |
| `--exclude-deps` | Hide call edges whose caller or callee comes from a dependency root (`is_dep = 1`). Requires a v4 or later database. |

Both filters may be combined. Output format:

```text
CallerFn -> CalleeFn (direct|indirect|ambiguous) at line N
```

Only **`call_edges`** are listed. Unresolved indirect call sites appear in `call_sites` but produce no line here unless an edge exists.

**Examples**

```bash
trace inspect /tmp/hdf.db calls --from NetIfSetAddr
trace inspect /tmp/hdf.db calls --from HdfSbufReadBuffer
trace inspect /tmp/hdf.db calls --to LiteNetSetIpAddr
```

When the full call graph is too large but you only care about a handful of
functions (e.g. memory-related ones), pass a filter config and only edges whose
caller or callee matches are printed. The database and analysis stay untouched;
the filter is purely a display adapter.

```json
{ "functions": ["malloc", "free", "calloc", "realloc", "memcpy", "memset"] }
```

```bash
trace inspect /tmp/hdf.db calls --callgraph-filter mem.json
```

For unresolved indirect calls, query SQL directly (see below).

### `trace inspect callgraph`

Print the transitive callees or callers of the function containing a line.

```text
trace inspect <DB> callgraph --file SUBSTR --line N [--depth N] [--direction down|up]
```

| Option | Description |
|--------|-------------|
| `--file <SUBSTR>` | File path substring to disambiguate same-name functions. |
| `--line <N>` | A line inside the function of interest. |
| `--depth <N>` | Maximum BFS depth (default 3). |
| `--direction` | `down` = callees (default), `up` = callers. |
| `--format` | Output format: `text` (default), `json`, `graphviz`, or `mermaid`. |
| `--callgraph-filter <FILE>` | JSON file listing regex patterns over function names; edges whose caller and callee both fail to match are hidden, nodes left without any surviving edge are pruned. The start function (the root) is always kept even when it does not match, so the query anchor stays visible. |

The start function is chosen among definitions whose `[line_start, line_end]`
contains `--line`. Edges are labeled with their resolution (`direct`,
`indirect`, `external`, `ambiguous`) and call-site locations; repeated
callees print `(see above; also file:line)`.

**Examples**

```bash
trace inspect /tmp/hdf.db callgraph --file devsvc_manager.c --line 120 --depth 2
trace inspect /tmp/hdf.db callgraph --file hdf_service_record.c --line 20 --direction up
trace inspect /tmp/hdf.db callgraph --file allocator.c --line 44 --callgraph-filter mem.json
```

### `trace inspect callchain`

Find all simple call paths (chains) between two functions no longer than `--depth`.

```text
trace inspect <DB> callchain [--from FN|FILE:LINE] [--to FN|FILE:LINE] [--depth N] [--limit N]
```

| Option | Description |
|--------|-------------|
| `--from <FN>` | Start function name, C++ qualified suffix, or `FILE:LINE` (e.g. `main` or `main.c:10`). |
| `--to <FN>` | Target function name, C++ qualified suffix, or `FILE:LINE` (e.g. `target` or `worker.c:25`). |
| `--from-file <SUBSTR>`, `--from-line <N>` | File and line locating the start function. |
| `--to-file <SUBSTR>`, `--to-line <N>` | File and line locating the target function. |
| `--depth <N>` | Maximum path length in call hops (default 5). |
| `--direction` | `down` = callers -> callees (default), `up` = callees -> callers. |
| `--limit <N>` | Maximum number of chains to return (default 100, 0 for unlimited). |
| `--format` | Output format: `text` (default), `json`, `graphviz`, or `mermaid`. |
| `--callgraph-filter` | Path to JSON filter config file (`{"functions": ["regex", ...]}`). |

**Examples**

```bash
trace inspect /tmp/trace.db callchain --from main --to target --depth 3
trace inspect /tmp/trace.db callchain --from main.c:10 --to target.c:20
trace inspect /tmp/trace.db callchain --from caller --to helper --format mermaid
```

### `trace inspect dataflow`

Show possible value flows from a source variable declaration, grouped by
canonical function and global/file scope.

```text
trace inspect <DB> dataflow --file SUBSTR --line N --col C [--depth N] [--direction down|up]
```

| Option | Description |
|--------|-------------|
| `--file <SUBSTR>` | File path substring. |
| `--line <N>`, `--col <C>` | Position near a source variable **declaration**. Synthetic intermediates are excluded. |
| `--depth <N>` | Maximum visible transitions after collapsing technical nodes (default 3). |
| `--direction` | `down` = where values flow (default), `up` = where they come from. |
| `--format` | `text` (default), `json`, `graphviz`, or `mermaid`. |

All formats retain numeric identities and omit call-site IDs.
Text function headers list parameter positions, names, types, and variable IDs.
Argument edges qualify cross-function parameters as `dispatch::payload` and
identify the callee in `pass argument [fn#29, argument 2]`. Operations
identify assignments, argument positions, returns, field access, pointer reads
and writes, and indirect calls. Macro calls emphasize the invocation; text and
diagrams retain the macro-body spelling. Text uses fixed indentation; diagrams group by scope.
All formats use the same source-level graph. Text shows every transition selected
by `--depth` and lists every possible callee once per call occurrence. Inspection
loads the requested visible neighborhood and its supporting metadata on demand. Field
labels identify abstract storage paths; call targets appear in separate lists.

These are context-insensitive possible flows. Separate call sites identify
argument-to-parameter wiring; they do not represent separate executions or
preserve correlations between argument pairs. Existing default-argument facts
are not supplemented: omitted defaults currently can have no parameter flow,
while explicit C++ `nullptr` arguments appear as null pointer values without
points-to targets. Each query shows only the graph reachable from its selected
variable: use a receiver downstream or its formal upstream to inspect receiver
passing, and a nullable formal upstream to inspect null arguments. See the
authoritative
[Source-level dataflow presentation](docs/ANALYSIS.md#source-level-dataflow-presentation)
for identity, traversal, provenance, and compatibility rules. The walk stays on
the constraint edges; the memory edges that join a store to the loads of the
same cell are followed by `trace inspect slice`.

Dataflow JSON contains `title`, `direction` (`down` or `up`), `depth`,
`truncated`, `scopes`, `nodes`, and `edges`. The four scope description arrays
(`functions`, `globals`, `statics`, `values`) are always present, unique, and
sorted by numeric ID. Nodes and edges use typed scope references or `null` for
unknown ownership. Every possible callee has a function description.
Each operation has `kind`, `expression`, and `location`, plus its own `callee_id`
when known. Null callee IDs are omitted; edges omit `callee_id` entirely. Argument operations include
their original zero-based `arg_index`; null indices are omitted. Collapsed edges
preserve their distinct recorded operations and callees. This changes the JSON contract:
internal fields and the edge-level argument index are omitted. See
[Source-level dataflow presentation](docs/ANALYSIS.md#source-level-dataflow-presentation)
for field definitions, reference rules, and compatibility.

A dataflow edge in `--format json`:

```json
{
  "from": 701,
  "to": 702,
  "scope": { "kind": "function", "id": 31 },
  "expression": "dispatch(message)",
  "location": { "path": "/project/main.cpp", "line": 38, "col": 5 },
  "operations": [
    {
      "kind": "pass argument",
      "expression": "dispatch(message)",
      "location": { "path": "/project/main.cpp", "line": 38, "col": 5 },
      "callee_id": 32,
      "arg_index": 0
    }
  ]
}
```

**Examples**

```bash
trace inspect /tmp/hdf.db dataflow --file can_test.c --line 33 --col 31
trace inspect /tmp/hdf.db dataflow --file usb_raw_io.c --line 331 --col 23 --depth 4
```

### `trace inspect slice`

A bounded value slice from a variable or field access: up to where the
value comes from, then down to where it goes. Each node lists the
[execution contexts](docs/ANALYSIS.md#execution-contexts) that reach it,
and edges where the value may change hands between threads, tasks or IPC
requests are flagged as cross-context. A flag is a hint, not a proven race.
Rules, output fields and limits:
[Value slice](docs/ANALYSIS.md#value-slice-inspect-slice).

```text
trace inspect <DB> slice --file SUBSTR --line N --col C [--name IDENT]
    [--up-depth N] [--down-depth N] [--format text|json]
```

| Option | Description |
|--------|-------------|
| `--file <SUBSTR>`, `--line <N>`, `--col <C>` | Position of a variable (declaration, use, or a call's argument: `p` in `sink(p);`) or of a field access (`cb_` in `cb_ = cb;`; `p` after `->` in `s->p = p;`, the other `p` being the variable). `--file` is a literal substring of the recorded path (`%` and `_` are not wildcards). The column is inside the identifier. |
| `--name <IDENT>` | The identifier at the position, when the recorded source file cannot be read from here or the line moves several values. |
| `--up-depth <N>` | Stage 1 limit: edges followed backwards to the sources. |
| `--down-depth <N>` | Stage 2 limit: edges followed forwards from each source. |
| `--format` | `text` (default, for people) or `json` (for tools). |

**Examples**

```bash
trace analyze tests/fixtures/value_slice -o /tmp/slice.db
# `callback_` in `ICallback *cb = callback_;`: written by an IPC handler, read by a thread
trace inspect /tmp/slice.db slice --file value_slice/main.cpp --line 59 --col 25
trace inspect /tmp/slice.db slice --file value_slice/main.cpp --line 59 --col 25 --format json
```

### Graph output formats

Both `callgraph` and `dataflow` accept `--format text|json|graphviz|mermaid`
(`text` is the default). `text` is the indented view shown above; the other
formats emit machine-readable graphs of the same traversal — same nodes,
same edges, same depth limit and truncation semantics. Dataflow text shows
every transition and every possible callee in the graph selected by `--depth`.
`trace inspect dataflow`
prints its candidate/fallback `note:` hints on stderr in every format.

All examples below run the same query on `/tmp/hpp.db`
(`tests/fixtures/hpp_designated_dispatch`):

```bash
trace inspect /tmp/hpp.db callgraph --file hpp_designated_dispatch/launch.cpp --line 5 --depth 3
```

**`--format text`** (default)

```text
callgraph from launch (launch.cpp:5-5) (callees, depth 3):
* launch (launch.cpp:5)
  -indirect-> DispatchToMessage (target.cpp:1) (launch.cpp:5)
2 functions, 1 edges
```

**`--format json`** — a single JSON document with `title`, `direction`,
`depth`, `truncated`, `summary`, `nodes`, and `edges`:

```json
{
  "title": "callgraph from launch (launch.cpp:5-5) (callees, depth 3):",
  "direction": "callees",
  "depth": 3,
  "truncated": false,
  "summary": "2 functions, 1 edges",
  "nodes": [
    {
      "id": 0,
      "depth": 0,
      "label": "launch (launch.cpp:5)",
      "detail": "launch.cpp:5"
    },
    {
      "id": 1,
      "depth": 1,
      "label": "DispatchToMessage (target.cpp:1)",
      "detail": "target.cpp:1"
    }
  ],
  "edges": [
    {
      "from": 0,
      "to": 1,
      "label": "indirect",
      "site": "launch.cpp:5"
    }
  ]
}
```

**`--format graphviz`** — a DOT `digraph` renderable with `dot`:

```bash
trace inspect /tmp/hpp.db callgraph --file hpp_designated_dispatch/launch.cpp --line 5 --depth 3 --format graphviz > call.dot
dot -Tsvg call.dot -o call.svg
```

```dot
digraph "callgraph from launch (launch.cpp:5-5) (callees, depth 3):" {
  rankdir="TB";
  node [shape=box];
  n0 [label="launch (launch.cpp:5)"];
  n1 [label="DispatchToMessage (target.cpp:1)"];
  n0 -> n1 [label="indirect (launch.cpp:5)"];
}
```

**`--format mermaid`** — a Mermaid `flowchart` for GitHub/Markdown or
`mmdc`:

````markdown
```mermaid
flowchart TD
  %% callgraph from launch (launch.cpp:5-5) (callees, depth 3):
  n0["launch (launch.cpp:5)"]
  n1["DispatchToMessage (target.cpp:1)"]
  n0 -->|"indirect (launch.cpp:5)"| n1
```
````

The same flag applies to `dataflow`:

```text
trace inspect /tmp/hpp.db dataflow --file hpp_designated_dispatch/launch.cpp --line 5 --col 12 --format mermaid
```

Every format escapes special characters (quote/backslash for DOT, HTML
entities for Mermaid, JSON via `serde_json`), so arbitrary C++ names and
file paths stay valid input.

## Analysis pipeline

```
discover .c/.cpp → preprocess → parse → lower IR → build PAG → solve → export SQLite
```

| Stage | What happens |
|-------|----------------|
| **Index** | Discover `.c` / `.cpp` files, preprocess TUs (custom preprocessor), parse with tree-sitter (C or C++ grammar per TU), lower to IR (functions, variables, flow constraints, call sites). |
| **Analyze** | Build pointer assignment graph (PAG), run Andersen-style solver, resolve direct calls by name (including file-local `static` functions), indirect calls via points-to to function locations. |
| **Export** | Write SQLite (minimal by default). |

Analysis is **may-analysis** (sound over-approximation): if a call target is possible, it may appear as an edge.

Header-function identity and visibility follow the
[shared header function rules](docs/ANALYSIS.md#shared-header-functions).
Measured changes are recorded in the [evaluation report](docs/EVAL_REPORT.md).

## Export modes

| Mode | Flags | Database contents |
|------|-------|-------------------|
| **Minimal** (default) | *(none)* | `analysis_run`, `files`, `functions`, filtered `call_sites`, `call_edges`, `arg_flow_edges`, `execution_contexts`, PAG-referenced variables, flow graph (`flow_nodes` / `flow_edges` / `flow_memory_access`), `diagnostics`. |
| **Full IR** | `--full-export` | Minimal plus all `types`, all `variables`, PAG `locations`. |
| **Points-to debug** | `--debug-points-to` | Adds `points_to` table (and retains PAG during analysis). Use with `--full-export` for complete debug dumps. |

The flow-graph tables are always exported because `trace inspect dataflow`
queries them directly.

### Call site export filter

`call_sites` rows are written when any of the following holds:

- The site has at least one **`call_edge`**.
- The site has at least one **`arg_flow_edge`**.
- The site is an **indirect** call (`is_direct = 0`), including unresolved function-pointer calls.

So unresolved indirect sites (e.g. `sbuf->impl->readBuffer` before a fix) still appear in `call_sites` even with zero `call_edges`.

### `trace-merge`

Merge multiple per-repository SQLite databases into a unified database, reconstructing the cross-repository callgraph without running static analysis or dataflow solvers during the merge stage.

```text
trace-merge [OPTIONS] <INPUT_DBS>... [-o <OUTPUT_DB>]
```

| Option | Description |
|--------|-------------|
| `<INPUT_DBS>...` | Paths to per-repository SQLite databases generated by `trace analyze`. |
| `-o`, `--output <PATH>` | Target unified SQLite database path. Default: `unified.db`. |
| `-v`, `--verbose` | Print detailed diagnostics for all unresolved external functions and weak overrides. By default, collision warnings and summary counts are printed. |

**How it works**:
- Ingests and remaps files, link targets, functions, call sites, call edges, and execution contexts across input databases (an input exported before `execution_contexts` existed contributes none and is named in a `MissingExecutionContexts` diagnostic).
- Shared header paths, declarations, and definitions are deduplicated across repositories; if any merged repository defines or declares an entity as non-dependency code (`is_dep = 0`), the unified entry retains `is_dep = 0`.
- Re-links `external` call edges (`functions.is_defined = 0` or `call_edges.resolution = 'external'`) against matching exported definitions (`is_defined = 1`, `linkage = 'external'`) from other repositories, updating edge resolution to `direct` or `ambiguous`, while preserving original non-direct resolutions (`indirect` and `ipc`).
- **May-analysis over-approximation**: When multiple definitions match an external call across repositories, an ambiguous edge (`resolution = 'ambiguous'`) is emitted to *every* candidate definition so downstream reachability queries (`trace inspect callchain`) do not miss execution paths. Deduplicates call edges per call site to eliminate duplicate edges.
- Respects ELF linker semantics: internal-linkage (`static`) functions remain file-scoped; strong definitions (`is_weak = 0`) override weak definitions (`is_weak = 1`).
- Uses normalized parameter signatures in `functions.signature` (`name(type1, type2)`) to disambiguate C++ overloads and avoid false collisions; falls back to all same-name candidates if exact signature matching misses.
- Prevents accidental overwriting of any input database when specifying `-o`.
- Opens inputs read-only and publishes through a unique temporary file beside the output. Failed merges clean up staging files and preserve the existing destination.
- Merger output supports call graphs, call chains, and editor call hierarchy. It omits variables, arg-flow, points-to, and all PAG/provenance tables; call-site variable bindings remain NULL. Dataflow inspection requires an original analysis database or a new analysis of the combined source tree; see the [merger schema contract](docs/SQLITE_SCHEMA.md#merger-inputs-and-output).
- Identifies and reports merge problems:
  - **Collisions**: Multiple strong definitions of the same symbol and signature across different repositories (`MultipleDefinitions`).
  - **Unresolved externals**: External function calls that remain unresolved in all merged repositories (`UnresolvedExternal`).
  - **Weak symbol overrides**: Weak definitions superseded by strong definitions across repositories (`WeakOverride`, scoped to cross-repository overrides).
  - **Inputs without execution contexts**: Inputs that have no `execution_contexts` table (`MissingExecutionContexts`).

**Example**:

```bash
# Analyze two repositories independently
trace analyze /path/to/framework -o /tmp/framework.db
trace analyze /path/to/app -o /tmp/app.db

# Merge and reconstruct the cross-repository callgraph
trace-merge /tmp/framework.db /tmp/app.db -o /tmp/unified.db

# Query reconstructed cross-repository callchains
trace inspect /tmp/unified.db callchain --from app_main --to framework_init
```

## SQLite database schema

Minimal and full databases record indirect callee and return-destination
variable IDs in `call_sites.callee_var` and `call_sites.return_dst`. Export uses
indexed SQLite joins for indirect-return provenance, building the required
indexes before that export phase. See the
[call-site schema](docs/SQLITE_SCHEMA.md#call_sites) for bindings and index rules.

SQLite export also builds a function-range index for operation ownership lookup
during source-level dataflow inspection; see the [inspection index rules](docs/SQLITE_SCHEMA.md#source-level-presentation-metadata-v7).

Schema version: **v7**, an additive compatibility family. Readers check required structures and database origin rather than infer available capabilities from the number alone; see the [version and capability contract](docs/SQLITE_SCHEMA.md#version-and-capability-contract). Optional `flow_call_origins` and `flow_call_expressions` metadata improves exact occurrence attribution and call text; older v7 analysis exports retain fallback behavior. Re-analysis supplies new metadata and selective-query indexes. `trace-merge` accepts v7 inputs with the required call-graph columns, rejects v6 or missing required structures before writing output, and produces a call-graph database without PAG/provenance data. Foreign keys are declared in DDL; exports temporarily disable FK enforcement for bulk load speed. Macro-body calls use their definition spelling in `call_sites.file_id/line/col`; nullable `expansion_file_id/expansion_line/expansion_col` retain the outermost invocation. `flow_nodes` stores empty labels/details for variable nodes to save space; the `flow_nodes_text` view reconstructs them for direct queries. `flow_edges` rows carry no position: `flow_origins` records where each edge's operation is written (original file, line and column, one row per statement), and the enclosing function follows from that position (see [Where a value moves](docs/ANALYSIS.md#where-a-value-moves)). `flow_memory_access` records which memory cells each load and store reaches (its sites are that load's or store's `flow_origins` rows), or that none are recorded ([Memory access edges](docs/ANALYSIS.md#memory-access-edges)); an earlier v7 export has no such table and `trace-merge` output leaves it empty. `execution_contexts` lists where threads, tasks and IPC requests start running code — one row per resolved callback of an `invoke` model (`pthread_create`, `std::thread`, `ffrt::submit`, `EventHandler::PostTask`, `HdfWorkInit`, ...), with the model's name and the receiver variable it was submitted on when named, and one per entry no call site starts (an override of `Thread::Run`, `EventHandler::ProcessEvent` or `DeathRecipient::OnRemoteDied`; an IPC stub handler) — with its kind (`thread`, `pool_task`, `serial_task`, `ipc_handler`, `unknown`) and multi-instance evidence (`loop`, `cycle`, `parent`, `unknown`; see [Execution contexts](docs/ANALYSIS.md#execution-contexts)); an earlier v7 export has no such table.

### Entity relationship (overview)

```text
analysis_run
link_targets ─┬─ target_sources → files
              └─ target_dependencies → link_targets
files ─┬─ functions ─┬─ call_sites ─ arg_flow_edges → variables
       │             │  (target_id → link_targets)
       │             ├─ call_edges → functions (caller and callee)
       │             └─ execution_contexts → functions, call_sites, variables
       └─ variables ─ flow_nodes ─ flow_edges → flow_nodes
                    (fn_id → functions, type_id → types,
                     target_id → link_targets)
types
locations (full export / debug)
points_to (debug only)
diagnostics
```

### Table reference

Every table, column and index is documented once, in
[docs/SQLITE_SCHEMA.md](docs/SQLITE_SCHEMA.md) — including which tables each
export mode writes, and the `is_weak` / `target_id` columns and `link_targets`
tables added in v5. It is the canonical description; this file keeps only the
overview above so the two cannot drift.


## Example SQL queries

### Callees of a function

```sql
SELECT callee.name, ce.resolution, cs.line, cs.callee_text
FROM call_edges ce
LEFT JOIN call_sites cs ON cs.id = ce.call_site_id
JOIN functions caller ON caller.id = ce.caller_fn_id
JOIN functions callee ON callee.id = ce.callee_fn_id
WHERE caller.name = 'HdfSbufReadBuffer';
```

### Unresolved indirect call sites

```sql
SELECT caller.name, cs.line, cs.callee_text
FROM call_sites cs
JOIN functions caller ON caller.id = cs.caller_fn_id
LEFT JOIN call_edges ce ON ce.call_site_id = cs.id
WHERE cs.is_direct = 0 AND ce.id IS NULL
ORDER BY caller.name, cs.line;
```

### All indirect resolutions for a call expression pattern

```sql
SELECT caller.name, callee.name, cs.line
FROM call_edges ce
JOIN call_sites cs ON cs.id = ce.call_site_id
JOIN functions caller ON caller.id = ce.caller_fn_id
JOIN functions callee ON callee.id = ce.callee_fn_id
WHERE ce.resolution = 'indirect'
  AND cs.callee_text LIKE '%readBuffer%';
```

### Callers of a function

```sql
SELECT caller.name, ce.resolution, cs.line
FROM call_edges ce
LEFT JOIN call_sites cs ON cs.id = ce.call_site_id
JOIN functions caller ON caller.id = ce.caller_fn_id
JOIN functions callee ON callee.id = ce.callee_fn_id
WHERE callee.name = 'LiteNetSetIpAddr';
```

### Argument flow at a call site (variable actuals)

```sql
SELECT cs.line, af.arg_index, av.name AS actual, fv.name AS formal
FROM arg_flow_edges af
JOIN call_sites cs ON cs.id = af.call_site_id
JOIN variables av ON av.id = af.actual_var_id
JOIN variables fv ON fv.id = af.formal_var_id
WHERE af.actual_var_id IS NOT NULL;
```

### Argument flow (function-pointer actuals)

```sql
SELECT cs.line, af.arg_index, f.name AS actual_fn, fv.name AS formal
FROM arg_flow_edges af
JOIN call_sites cs ON cs.id = af.call_site_id
JOIN functions f ON f.id = af.actual_fn_id
JOIN variables fv ON fv.id = af.formal_var_id
WHERE af.actual_fn_id IS NOT NULL;
```

### Where threads and tasks start

```sql
SELECT e.kind, entry.name AS entry, e.model, caller.name AS started_in, cs.line,
       queue.name AS submitted_on, e.multi_instance, e.self_concurrent
FROM execution_contexts e
JOIN functions entry ON entry.id = e.entry_fn_id
LEFT JOIN call_sites cs ON cs.id = e.call_site_id
LEFT JOIN functions caller ON caller.id = cs.caller_fn_id
LEFT JOIN variables queue ON queue.id = e.receiver_var_id
ORDER BY e.kind, entry.name;
```

## Project layout

```
crates/
  trace-preproc/   Custom C preprocessor (#include, #define, conditionals)
  trace-parse/     tree-sitter parsing, IR lowering, TU merge
  trace-ir/        Shared IR (types, symbols, flow constraints)
  trace-analysis/  PAG construction, Andersen solver, call graph
  trace-db/        SQLite schema and export
  trace-capi/      C ABI wrapper library (`libtrace_capi`) and C header (`trace.h`)
  trace-cli/       `trace` binary (`analyze`, `inspect`)
  trace-merge/     Cross-repository database merger & callgraph reconstruction (`trace-merge`)
docs/              Design docs (architecture, analysis, preprocessor, schema)
tests/fixtures/    Integration test C corpora
```

## Limitations

- **C++ first step** — namespaces (qualified variables and static data members included; see [Canonical variable identity](docs/ANALYSIS.md#canonical-variable-identity)), overloads (arity), classes/virtual dispatch (including virtual bases), `final` class/method devirtualization, ctors/dtors (constructors also for objects built by `std::make_shared` / `std::make_unique` / `sptr<T>::MakeSptr`; see [Factory construction](docs/ANALYSIS.md#factory-construction); member-initializer construction retains aggregate subobjects, constant and unknown array bounds, default initialization after inherited construction, cv/ref constructor distinctions and implicit copy/move subobject calls, following constructor declarations and reference-binding rules under [Ctors / dtors](docs/ANALYSIS.md#c-support-first-step)), implicit `this->method()`, smart-pointer unwrap through a declared `operator->` (`shared_ptr`, and OHOS `sptr`/`RefPtr` or HDI `AutoPtr` alike, the wrapper keeping its own members for `.`) or, for a wrapper whose body is not in the tree, through its single class argument, `auto` locals typed from declared return types and smart-pointer factories, and callables (`std::function`, lambdas, `operator()`) are modeled; type-based overload ranking and templates beyond name-stripping are not (see [docs/ANALYSIS.md](docs/ANALYSIS.md)). Next slices from hiview: [docs/CPP_ROADMAP.md](docs/CPP_ROADMAP.md).
- **May-analysis** — indirect calls can list multiple targets; absence of an edge does not prove unreachability.
- **No path sensitivity** — all branches and paths are merged.
- **Preprocessor subset** — not gcc/clang compatible for all extensions. Vendor macros such as `__GNUC__` / `__clang__` stay undefined; `--system-includes` imports only selected target and hosted-mode macros from the compiler. Without that flag, only the language's own `__cplusplus` / `__STDC__` / `__STDC_VERSION__` are predefined; see [docs/PREPROCESSOR.md](docs/PREPROCESSOR.md).
- **Build environment** — with `--system-includes`, compiler probing locates installed standard-library headers and selected target macros, but does not supply missing SDK or generated headers. Supply additional roots with `--dep` / `--include` as needed. Other compiler-specific builtins and response files are not modeled.
- **Configuration coverage** — without database entries or `--explore`, indexing uses one inferred configuration. A name no `-D` or reached `#define` binds resolves to `0` in `#if`; use [conditional coverage reporting](docs/CONDITIONAL_COVERAGE.md) to measure exclusions in a selected configuration. Explicit database commands are all merged; exploratory configurations remain bounded by `--explore-budget`.
- **Original line attribution** — entities use original source positions through the preprocessor's `LineMap`. See [Call source locations](docs/ANALYSIS.md#call-source-locations) for macro-body spelling and invocation positions, and [Shared header functions](docs/ANALYSIS.md#shared-header-functions) for header identity and ownership.

## Further reading

- [Architecture](docs/ARCHITECTURE.md)
- [Analysis algorithm](docs/ANALYSIS.md)
- [Preprocessor spec](docs/PREPROCESSOR.md)
- [C API (`trace-capi`)](docs/CAPI.md)
- [Conditional-compilation coverage reporting](docs/CONDITIONAL_COVERAGE.md)
- [SQLite schema (detailed)](docs/SQLITE_SCHEMA.md)
- [Roadmap](docs/ROADMAP.md)
- [C++ next slices (hiview)](docs/CPP_ROADMAP.md)
- [Contributing](CONTRIBUTING.md)
- [Code of Conduct](CODE_OF_CONDUCT.md)
- [Agent guide](AGENTS.md)
- [License](LICENSE)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, testing, and pull-request guidelines, and [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) for our community standards.

## License

This project is licensed under the [MIT License](LICENSE).
