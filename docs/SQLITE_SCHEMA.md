# SQLite schema

Schema version: **v7**

Export creates secondary indexes after bulk insertion, within the same
transaction and before publishing the database. Primary keys and uniqueness
constraints remain active during insertion. The complete schema is assembled from the same table and index definitions.

See also the [README](../README.md) for CLI flags that control what is exported.

## Version and capability contract

`analysis_run.schema_version = 7` identifies an **additive compatibility
family**, not a fixed set of capabilities. New tables, indexes, and nullable
or defaulted columns may extend v7 while preserving existing columns and their
meaning. Incompatible layouts or meanings require a version bump. This retains
compatibility with earlier v7 exports without forcing re-analysis for readers
that use only the unchanged call-graph data.

Consumers must check the tables and columns needed by their query, and account
for database origin and export mode. A version number or an empty table alone
cannot establish that a producer supplied a capability. Source-level inspection
checks its required structures and rejects `analysis_run.options_json.stage =
"merge"`; an ordinary analysis with an empty `flow_origins` table remains valid.
Optional metadata has explicit fallback behavior below. Re-analysis supplies
new metadata and query indexes; there is no automatic migration from v6.

### Merger inputs and output

`trace-merge` requires v7 and structurally validates the exact columns it reads
in `files`, `link_targets`, `target_sources`, `target_dependencies`, `functions`,
`call_sites`, `call_edges`, and `diagnostics` before creating or replacing output.
Earlier v7 databases without newer flow tables or `call_sites.callee_var` /
`return_dst` can therefore still be merged. A nominal v7 database missing a
required call-graph column is rejected with a re-analysis diagnostic.

The output uses the shared v7 DDL and records `options_json.stage = "merge"`.
It preserves/remaps those call-graph tables and diagnostics, including call-site
spelling and expansion coordinates, and reconstructs external call edges.
It supports function/call lookup, call graphs, call chains, and editor call
hierarchy. It does **not** populate `variables`, `types`, `locations`,
`points_to`, `arg_flow_edges`, or any `flow_*` tables. `call_sites.callee_var`
and `return_dst` remain NULL because input variable IDs are not remapped.
Creating these structures through shared DDL does not supply their capabilities.

PAG and provenance merging is outside the merger's call-graph contract.
For dataflow inspection, use an original `trace analyze` database, or analyze
an appropriate combined source tree to produce a database with flow facts.
Neither the CLI source-level view nor the C API raw PAG query can recover
omitted flow data from merger output.

## Export modes vs tables

| Table | Minimal (default) | `--full-export` | `--debug-points-to` |
|-------|-------------------|-----------------|---------------------|
| `analysis_run` | ✓ | ✓ | ✓ |
| `files` | ✓ | ✓ | ✓ |
| `link_targets` | ✓ | ✓ | ✓ |
| `target_sources` | ✓ | ✓ | ✓ |
| `target_dependencies` | ✓ | ✓ | ✓ |
| `functions` | ✓ | ✓ | ✓ |
| `call_sites` | filtered | filtered | filtered |
| `call_edges` | ✓ | ✓ | ✓ |
| `arg_flow_edges` | ✓ | ✓ | ✓ |
| `variables` | PAG-referenced | all | all (+ arg-flow) |
| `flow_nodes` | ✓ | ✓ | ✓ |
| `flow_edges` | ✓ | ✓ | ✓ |
| `types` | | ✓ | ✓ |
| `locations` | | ✓ | ✓ |
| `points_to` | | | ✓ |
| `diagnostics` | ✓ | ✓ | ✓ |

The flow-graph tables (`flow_nodes`, `flow_edges`) and the variables they
reference are always exported because `trace inspect dataflow` works purely
off the database.

### Call site export filter

A row is written to `call_sites` when **any** of:

- the site has ≥1 row in `call_edges`
- the site has ≥1 row in `arg_flow_edges`
- `is_direct = 0` (indirect / fn-ptr syntax, **including unresolved**)

Unresolved indirect calls therefore appear in `call_sites` with zero `call_edges`.

## Entity relationships

```text
analysis_run
link_targets ─┬─ target_sources → files
              └─ target_dependencies → link_targets
files ─┬─ functions ─┬─ call_sites ─ arg_flow_edges → variables
       │             │  (functions.target_id → link_targets)
       │             └─ call_edges → functions (caller and callee)
       ├─ variables ─ flow_nodes ─ flow_edges → flow_nodes
       └─ variables (type_id → types when exported,
                     target_id → link_targets)
types
locations (full export)
points_to (debug)
diagnostics
```

## Tables

### analysis_run

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Run id |
| `trace_version` | TEXT | Full binary identity: package version, source revision, dirty state, and build date |
| `schema_version` | INTEGER | Compatibility family (currently `7`; see [contract](#version-and-capability-contract)) |
| `target_root` | TEXT | Analyzed directory |
| `created_at` | TEXT | Unix timestamp (seconds) |
| `options_json` | TEXT | JSON: `test_partition`, `include_paths`, `defines`, `dep_roots`, `ignored_macros`, `include_points_to`, `full_detail`, `model_files`, `explore`, `explore_budget`, `variants_merged`, `solver_partial`, `solver_pops`, `solve_budget_pops`, `solve_budget_secs` |

`test_partition` is an object with `enabled` (boolean) and `directories`
(array of strings), recording the effective configured name list. Disabled runs
store `false` and an empty array. Defaults, validation, and precedence are defined in
[Declaring-header eligibility](ANALYSIS.md#declaring-header-eligibility).

`explore` and `explore_budget` record what the run *requested*; `variants_merged`
records how many variant units it actually merged. They come apart: a run can ask
for exploration and find no feasible variant, or be given a zero budget. A
consumer asking whether a database contains cross-variant facts must read
`variants_merged`, not `explore`. Compilation databases (#62) can contribute
additional commands for a source without exploration; those additional units
also count. `include_paths` is the union of paths observed across configurations,
while `defines` records user overrides, not every per-command macro environment.

`solver_partial` (`false` / `true`), `solver_pops`, `solve_budget_pops` (`null` =
unlimited) and `solve_budget_secs` (`null` = no time limit) describe how the
solver run ended. `solver_partial: true` means the run stopped on its work
budget before reaching the fixpoint; every table then answers a **monotone
prefix of the fixpoint** — every recorded edge and flow is real, but flows that
had not propagated when the budget hit are absent, so a may-analysis query can
under-report ("no path found" when the true fixpoint has one). The same fact is
recorded as an `analyze`-stage `warning` diagnostic (see [`diagnostics`](#diagnostics)).
Databases exported before this feature have none of the four keys.

### files

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | File id |
| `path` | TEXT UNIQUE | Absolute/normalized path |
| `sha256` | TEXT | Hash placeholder (may be empty) |
| `is_dep` | INTEGER | 1 if file resides under a dependency root (`--dep`), 0 otherwise |

### Link targets

GN inference uses these same tables; origin and precedence rules are
defined in [Link targets and weak symbols](ANALYSIS.md#link-targets-and-weak-symbols).

Link metadata is exported in every detail mode. Without link-target metadata,
these tables are empty and symbol `target_id` values are `NULL`. Incomplete GN
inference exports every target it read as an observation, complete or not,
while symbols keep `target_id = NULL`: resolution stays whole-tree. A
non-`NULL` `target_id` therefore means scoped resolution, and a populated
`link_targets` alone does not.

| Table | Columns | Meaning |
|-------|---------|---------|
| `link_targets` | `id` INTEGER PK, `name` TEXT, `output` TEXT | Link target and its output path |
| `target_sources` | `target_id` FK → `link_targets`, `file_id` FK → `files` | Source membership; the pair is the primary key |
| `target_dependencies` | `target_id` FK → `link_targets`, `dependency_id` FK → `link_targets` | Direct link dependencies; the pair is the primary key |

**Indexes:** `target_sources(file_id)`, `target_dependencies(dependency_id)`,
and partial indexes on `functions(target_id)` / `variables(target_id)` covering
only non-`NULL` rows, so a run without link metadata builds and stores nothing
for them.

Weak flags and target associations accompany functions in all export modes,
and variables whenever those variables are included in the export. These
tables and columns are what v5 adds over v4; regenerate an older database to
query them.

### functions

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Function id |
| `name` | TEXT | Linkage-visible name |
| `file_id` | INTEGER FK → `files` | Defining file (original header if include-originated). For a synthesized external (`line_start = 0`, `is_defined = 0`) this is only a resolver scope fallback — an arbitrary call-site file, not a declaration — so file filters must gate on `line_start > 0` |
| `line_start` | INTEGER | Start line (always original-file coordinates via LineMap); `0` iff the function was synthesized (never declared in-tree) |
| `line_end` | INTEGER | End line of the definition body; equals `line_start` for prototypes and synthesized externals (both `0` for a synthesized external) |
| `linkage` | TEXT | `external`, `internal`, `none` |
| `signature` | TEXT | Normalized function signature with parameter types (e.g. `name(int, char*)`). Decays top-level array parameters to pointers (`int[]` -> `int*`); drops qualifiers (`const`, `volatile`) and signedness, representing function pointers as `fn_ptr`. Used for best-effort C++ overload disambiguation during cross-repository linking |
| `is_defined` | INTEGER | 1 if a body exists under the analyzed root. 0 rows include prototype-only declarations, synthesized externals (libc/logging backends never declared in-tree), and dependency declarations |
| `is_dep` | INTEGER | 1 if function originates from a dependency root (`--dep`), 0 otherwise |
| `is_weak` | INTEGER | 1 for a weak symbol, 0 otherwise; recorded only for external linkage — a `static` has none to weaken. |
| `target_id` | INTEGER FK → `link_targets` | Owning link target; `NULL` when no target is known. |

**Index:** `functions(name)`

Header-defined functions are deduplicated across TUs within their link target
at merge time (later copies redirect), so they appear once per origin and target.

### call_sites

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Call site id |
| `caller_fn_id` | INTEGER FK → `functions` | Containing function |
| `file_id` | INTEGER FK → `files` | Call spelling file; for a macro-body call, the macro definition file |
| `line` | INTEGER | Spelling line (original-file coordinates via LineMap) |
| `col` | INTEGER | Spelling column |
| `expansion_file_id` | INTEGER FK → `files`, nullable | Outermost macro invocation file for a macro-body call |
| `expansion_line` | INTEGER, nullable | Outermost invocation line |
| `expansion_col` | INTEGER, nullable | Outermost invocation column |
| `callee_text` | TEXT | Surface syntax (`foo`, `p->handler`, …) |
| `is_direct` | INTEGER | `1` direct by name; `0` indirect |
| `callee_var` | INTEGER, nullable | IR `VarId` used as the indirect callee, when recorded; `NULL` for calls without one |
| `return_dst` | INTEGER, nullable | IR `VarId` receiving the result, when recorded; `NULL` when no result destination is recorded |

`callee_var` and `return_dst` are recorded in both minimal and full exports.
They are IR identities without foreign keys to the minimal export's filtered
`variables` table. The partial index
`call_sites(callee_var, return_dst)` covers rows with both values present;
indirect-return provenance uses it to find matching sites, then joins
`call_edges` through its `call_site_id` index. These two indexes are built after
call-site/edge insertion and before provenance export; remaining secondary
indexes are deferred. Callees are deduplicated and ordered by `FnId`, preserving
the provenance association between each return source and its root callee.
Synthetic call edges have a `NULL` call-site ID and cannot match this join.
These bindings are part of the v7 layout; older exports may omit them.

Call sites inside header-defined functions are deduplicated by
`(spelling file, line, col, expansion file, line, col, callee)` across TUs. Under `--explore`, variant calls
at that same location retain distinct IDs when their arguments, receiver, callee
binding, or return destination differ. Identical call facts are deduplicated.
Consequently, raw `call_edges` counts can increase without adding a distinct
source-location/target pair; group by caller, callee, spelling and expansion
file/line/column, and resolution when comparing invocation-level coverage.

### call_edges

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Edge id |
| `call_site_id` | INTEGER FK → `call_sites` | Call site; **`NULL` for synthetic edges** (see below) |
| `caller_fn_id` | INTEGER FK → `functions` | Resolved caller function |
| `callee_fn_id` | INTEGER FK → `functions` | Resolved target |
| `resolution` | TEXT | `direct`, `indirect`, `ambiguous`, `external` (callee statically resolved but bodyless under the analyzed root — see `functions.is_defined`), `ipc` (synthetic proxy→stub bridge edge) |

Multiple rows per call site are allowed (may-analysis indirect targets).

**Synthetic edges (IPC bridges):** edges injected for a proxy→stub bridge
carry `call_site_id = NULL` and `resolution = 'ipc'` (there is no single
source-level call site — the proxy body only has the opaque `SendRequest`
call). Their caller is given by `caller_fn_id` (the proxy method); consumers
must use `ce.caller_fn_id`, not `cs.caller_fn_id`, and treat `NULL` as a
synthetic/bridge edge with no source location. IPC detection is enabled by
default and disabled with the `--no-ipc` analyze flag.

**Indexes:** `call_edges(caller_fn_id)`, `call_edges(callee_fn_id)`,
`call_edges(call_site_id)`

### arg_flow_edges

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Edge id |
| `call_site_id` | INTEGER FK → `call_sites` | Call site |
| `arg_index` | INTEGER | 0-based parameter position the argument binds to; for a C++ member function or constructor 0 is the implicit `this`, so explicit arguments start at 1 |
| `actual_var_id` | INTEGER FK → `variables` | Actual variable (`NULL` if actual is a function) |
| `actual_fn_id` | INTEGER FK → `functions` | Actual function for fn-ptr args (`NULL` if actual is a variable) |
| `formal_var_id` | INTEGER FK → `variables` | Callee parameter var |

Exactly one of `actual_var_id` or `actual_fn_id` is set per row. A function name with several internal-linkage C++ overloads (`static` or in an anonymous namespace) is passed as each of them: one row per overload at the same `call_site_id`, `arg_index` and `formal_var_id`.

**Index:** `arg_flow_edges(call_site_id)`

### variables

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Variable id |
| `name` | TEXT | Source or synthetic name |
| `kind` | TEXT | `global`, `file_static`, `fn_static`, `param`, `local` |
| `fn_id` | INTEGER FK → `functions` | Enclosing function (nullable) |
| `type_id` | INTEGER FK → `types` | Type id |
| `file_id` | INTEGER FK → `files` | Declaration file |
| `line` | INTEGER | Declaration line |
| `col` | INTEGER | Declaration column (start of the declarator) |
| `is_weak` | INTEGER | 1 for a weak symbol, 0 otherwise; recorded only for globals — a file-scope `static`, a local or a parameter has no linkage to weaken. |
| `target_id` | INTEGER FK → `link_targets` | Owning link target; `NULL` when no target is known. |

In minimal export, variables are limited to every global and static (with
or without a flow node) and the locals and parameters the flow graph /
arg-flow edges reference; use `--full-export` for every variable. A global no
fact names is listed so `trace inspect` finds it by its own declaration;
`inspect dataflow` then reports it has no flow node.

`variables.is_synthetic` (INTEGER, default 0) marks lowering-created
intermediate values. Source selection filters this metadata, never name prefixes.

### flow_nodes

PAG value-flow nodes (`trace inspect dataflow`). Always exported. A global
or static that no flow, return or call site names has no node (see
[Program Assignment Graph](ANALYSIS.md#program-assignment-graph-pag)).

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | PAG node id (same id space as `points_to.var_node_id`) |
| `kind` | TEXT | `var`, `loc`, `constant` (immutable value with no abstract location), `call_target` (indirect-call site node), or `terminator` (function-model clears event) |
| `label` | TEXT | Human-readable label (empty for `var` nodes where label is read from `variables.name`; `loc:…`, `fn:…`, `"memset_s clears arg0"`) |
| `detail` | TEXT | Extra context (empty for `var` nodes where detail is derived from `variables.kind`, `variables.line`, and `functions.name`; location kind for `loc`, call site for `call_target`/`terminator`) |
| `var_id` | INTEGER FK → `variables` | Variable this node belongs to (`NULL` for function locations) |
| `fn_id` | INTEGER FK → `functions` | Enclosing function, when known (call targets record the caller; function locations record the function value) |
| `call_site_id` | INTEGER FK → `call_sites` | Call occurrence for call-target/clearing nodes; otherwise `NULL` |

Explicit C++ `nullptr` uses `kind='constant'`, `label='nullptr'`, and
`detail='null_pointer'`, with no variable or function owner. Its outgoing `copy`
edges retain value-flow connectivity; it is not a `locations` row and seeds no
points-to facts. This uses the existing v7 columns without a layout change.

**Index:** `flow_nodes(var_id)`

### flow_nodes_text (view)

A convenience view over `flow_nodes` joining `variables` and `functions` to reconstruct `label` and `detail` for `var` nodes (where `flow_nodes` stores empty strings to save database space, while preserving non-empty labels/details if already present). Direct queries seeking formatted node text can query this view instead of `flow_nodes`.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | PAG node id |
| `kind` | TEXT | Node kind |
| `label` | TEXT | Reconstructed label for empty `var` (`variables.name`), or stored label when present |
| `detail` | TEXT | Reconstructed detail for empty `var` (`"{kind} @{line} in {fn}"`), or stored detail when present |
| `var_id` | INTEGER FK → `variables` | Variable ID |
| `fn_id` | INTEGER FK → `functions` | Function ID |

### flow_edges

Directed value-flow edges between PAG nodes. Always exported.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Edge id |
| `src_node` | INTEGER FK → `flow_nodes` | Source node |
| `dst_node` | INTEGER FK → `flow_nodes` | Destination node (value flows src → dst) |
| `kind` | TEXT | see below |

Edge kinds:

- `copy`, `addr_of`, `load`, `store`, `gep`, `dlsym` — direct translations of the IR
  flow constraints that survived solving (including param copies wired by
  the solver). `dlsym` is the symbol-lookup model: string constants in the
  name argument become function locations on the return destination.
- `unwrap` — a smart pointer's overloaded `->`/`*` (`sp->field`): the
  wrapper value flows into the pointee-typed receiver the field access
  continues from. Only pointee-compatible locations cross it; see
  [Smart-pointer unwrap](ANALYSIS.md#smart-pointer-unwrap).
- `points_to` — implicit var → storage-location edge derived from the final
  var→location map.
- `call_arg` — actual-to-formal argument passing from `arg_flow_edges`,
  exported when no stronger constraint already connects the pair. The source
  is the value passed: for an `&x` argument that is the temporary holding
  `&x` (reached from `x`'s location through `addr_of`), even though the
  `arg_flow_edges` row names `x`.
- `terminates` — terminator visibility edge from a function-model `clears`
  effect (e.g. `memset_s(dst, …)`): the actual-argument node flows into a
  synthetic `terminator` node recording the call site. No points-to value
  is produced; the edge documents where a buffer's prior contents stop.

Edges have no position columns. Where the statement behind an edge is written
is recorded once, in [`flow_origins`](#source-level-presentation-metadata-v7),
keyed by the same `(src_node, dst_node, kind)`; edges with no statement of their
own have no origin rows. Rules:
[Where a value moves](ANALYSIS.md#where-a-value-moves); query:
[Source sites of a value move](#source-sites-of-a-value-move).

**Indexes:** `flow_edges(src_node)`, `flow_edges(dst_node)`

### types

Exported with `--full-export` only.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Type id |
| `kind` | TEXT | `void`, `int`, `short`, `long`, `long long`, `bool`, `float`, `double`, `struct`, `ptr`, `fnptr`, `array`, `func`, `unknown`, … |
| `name` | TEXT | Display name |
| `size` | INTEGER | Layout size |
| `layout_json` | TEXT | JSON field layout |

### locations

PAG abstract locations (`--full-export`).

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Location id |
| `kind` | TEXT | `global`, `file_static`, `fn_static`, `local`, `heap`, `field`, `field_summary`, `array_summary`, `function`, `string_lit`, … |
| `desc` | TEXT | Description |
| `type_id` | INTEGER FK → `types` | Optional |

### points_to

PAG node → location sets (`--debug-points-to`).

| Column | Type | Description |
|--------|------|-------------|
| `var_node_id` | INTEGER | PAG node id |
| `loc_id` | INTEGER FK → `locations` | Target location |

PK: `(var_node_id, loc_id)`

### diagnostics

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Diagnostic id |
| `severity` | TEXT | `error`, `warning`, `info` |
| `file_id` | INTEGER FK → `files` | Optional |
| `line` | INTEGER | Line |
| `message` | TEXT | Text |
| `stage` | TEXT | `preprocess`, `parse`, `analyze`, `explore`, `compile_commands`, `link_commands`, `merge` |

`preprocess` rows are the preprocessor's own diagnostics (missing includes, unknown directives,
unterminated `#if`, mid-file stops), attributed to the file and line where the condition
occurred — a nested header, not the including translation unit — and deduplicated on
`(file_id, line, message)` across translation units. `parse` rows are per unit (`parse errors
in <path>`, `file_id` NULL). `compile_commands` and `link_commands` rows report unreadable or
inconsistent build databases. See `docs/PREPROCESSOR.md`, "Error recovery".

`analyze` rows carry whole-run solver results and always have `file_id` NULL and `line` 0.
The only current one is the budget-truncation warning (`solver_partial` in
`analysis_run.options_json` is the same fact in machine-readable form).

`merge` rows report cross-repository collision warnings (multiple strong definitions with identical signatures),
unresolved external function dependencies, or weak symbol overrides generated by `trace-merge`.

## Example queries

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

### Indirect calls only (resolved)

```sql
SELECT caller.name, callee.name, cs.callee_text, cs.line
FROM call_edges ce
JOIN call_sites cs ON cs.id = ce.call_site_id
JOIN functions caller ON caller.id = ce.caller_fn_id
JOIN functions callee ON callee.id = ce.callee_fn_id
WHERE ce.resolution = 'indirect';
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

### Source sites of a value move

Every statement that moves `b`'s value or storage into `pp`, with the enclosing
function by the [edge-scope rule](ANALYSIS.md#where-a-value-moves): when
several definitions hold the line, the innermost one if the line is strictly
inside it (not its first or last line); a line held by a single definition is
that definition's, its first and last lines included; otherwise `NULL`:

```sql
SELECT o.kind, o.operation, p.path, o.line, o.col, o.expression,
       (SELECT CASE WHEN COUNT(DISTINCT f.id) = 1 THEN MIN(f.name) END
        FROM functions f
        WHERE f.file_id = o.file_id AND f.is_defined = 1
          AND o.line BETWEEN f.line_start AND f.line_end
          AND NOT EXISTS (
            SELECT 1 FROM functions g
            WHERE g.file_id = o.file_id AND g.is_defined = 1 AND g.id <> f.id
              AND o.line BETWEEN g.line_start AND g.line_end
              AND NOT (g.line_start <= f.line_start AND f.line_end <= g.line_end
                       AND o.line > f.line_start AND o.line < f.line_end)))
       AS function
FROM flow_origins o
JOIN files p ON p.id = o.file_id
JOIN flow_nodes s ON s.id = o.src_node
JOIN flow_nodes d ON d.id = o.dst_node
JOIN variables sv ON sv.id = s.var_id
JOIN variables dv ON dv.id = d.var_id
WHERE sv.name = 'b' AND dv.name = 'pp'
ORDER BY p.path, o.line, o.col;
```

`flow_origins` rows join a `flow_edges` row on `(src_node, dst_node, kind)`.
An `addr_of` origin's source is a storage-location node: a variable's carries
that variable's `var_id`, a function's carries its `fn_id` instead.

## CLI inspection

```bash
trace inspect graph.db calls [--from FN] [--to FN] [--file SUBSTR] [--exclude-deps]
trace inspect graph.db callgraph --file SUBSTR --line N [--depth N] [--direction down|up]
trace inspect graph.db dataflow --file SUBSTR --line N --col C [--depth N] [--direction down|up]
```

- `calls` lists rows from `call_edges` joined with `functions` and left-joined
  with `call_sites` (synthetic IPC edges have no call site).
  `--from` / `--to` match an exact `functions.name` or a C++ suffix (`%::FN`
  with `_`/`%` in `FN` escaped so they are not `LIKE` wildcards).
  `--file` matches ordinary edges by call-site or callee file. For synthetic
  edges it matches the caller or callee definition file.
  `--exclude-deps` hides edges whose caller or callee comes from a dependency
  root (`is_dep = 1`); a database older than v4 has no such column and reports
  an actionable re-analysis message.
  Unresolved indirect sites require SQL (query above).
- `callgraph` finds the function whose `[line_start, line_end]` contains the
  given line and prints its transitive callees (`down`) or callers (`up`),
  bounded by `--depth`. Edge labels distinguish `direct`, `indirect`,
  `external`, and `ambiguous` resolution.
- `dataflow` resolves the variable declared nearest the given position (exact
  identifier hit preferred, using a half-open character-column interval;
  declarations only — use sites are not recorded) and presents source-level
  value flow forward (`down`: where the value flows) or backward (`up`: where
  it came from). Depth counts visible transitions after technical PAG nodes
  collapse; see the authoritative
  [source-level contract](ANALYSIS.md#source-level-dataflow-presentation).
  Parameters duplicated across TUs (header prototype vs definition copies)
  are reconciled automatically when the queried copy carries no edges.

Both graph commands print a forest with `(truncated at --depth …)` markers
when the frontier was cut off.


### Source-level presentation metadata (v7)

Always exported by analysis in minimal and full modes, independently of points-to retention:

- `flow_origins(src_node, dst_node, kind, file_id, line, col, expression,
  operation)` records original source operation sites for constraint endpoint
  pairs. `kind` matches the raw constraint (`copy`, `load`, `store`, etc.);
  `operation` may additionally distinguish `return value` and `write field`. Multiple sites per
  pair are allowed. Coordinates are recorded via LineMap, not declarations.
- `flow_parameters(fn_id, arg_index, var_id, name, type_name)` records the canonical
  function parameter list for text headers in both minimal and full exports.
  `arg_index` is zero-based and follows the solver's parameter list (including
  implicit receivers). Names, IDs, and display types are retained even for
  parameters without graph nodes. `var_id` is an IR identity, without a foreign
  key to the minimal export's filtered `variables` table. This header metadata
  does not alter the dataflow JSON contract.
- `flow_field_locations(node_id, parent_node, field_name)` records concrete
  field-location parents from the PAG. `node_id` is unique; full parent chains
  distinguish nested abstract storage sharing a root variable and field label.
- `flow_field_access(base_node, dst_node, field_name)` records named PAG field
  accesses with a unique key over all three columns. It reconstructs aggregate
  initializer destinations without changing raw graph labels or operation text.
- `flow_call_origins(call_site_id, file_id, line, col, expression)` is additive
  optional v7 metadata, keyed by call ID. It records the closest source operation
  for each return-assigning call before equivalent constraints are unioned,
  preserving distinct assignments within one macro invocation. Older v7 databases
  remain readable using endpoint/source-position fallback; re-analysis supplies
  exact occurrence attribution. Both export modes use the same writer.
- `flow_call_expressions(call_site_id, expression)` is additive optional v7
  metadata, keyed by call ID. It stores the full call expression for argument
  operation presentation, separately from return-assignment provenance.
  Locations and macro invocation/spelling coordinates still come from
  `call_sites`. Both export modes use the same writer. Older databases without
  this table remain readable with empty argument expressions; re-analysis
  supplies recorded call text.
- `flow_return_calls(src_node, dst_node, call_site_id, callee_fn_id)` records
  existing return wiring by the recorded call return destination and resolved
  callee, including calls with no parameters. Each source is associated only
  with root callees whose transitive return facts contain it, including for
  multi-candidate indirect calls. It does not add analysis edges.
- `flow_calls(src_node, dst_node, call_site_id, arg_index)` records each
  actual/formal occurrence, including pairs already wired as raw `copy`.
  Sources name the passed value (the address temporary for `&x`), following
  the same export policy as raw `call_arg`. Call metadata and macro locations
  come from `call_sites`; parameter/owner identities come from `variables`.

Inspection indexes cover both endpoints of `flow_origins`, `flow_calls`, and
`flow_return_calls`, the destination of `flow_field_access`, and parameter
copies by `(fn_id, name)` under `kind='param'`. Function ownership lookup uses
`idx_functions_file_range` on `functions(file_id, is_defined, line_start, line_end)`
to seek definitions in the operation's file instead of building a temporary
index over all functions for each position batch. These indexes are exported in
minimal and full modes and built after bulk export,
keep version 7's row layout, and support selective source-level inspection.
Older v7 exports may lack these additive indexes; re-analysis supplies them.

Raw tables keep their existing meaning under the
[version and capability contract](#version-and-capability-contract).
Inspect checks required structures for older exports; the source-level view
requires its presentation metadata and an analysis origin. The CLI JSON and
raw API compatibility rules are defined in
[Source-level dataflow presentation](ANALYSIS.md#source-level-dataflow-presentation).
