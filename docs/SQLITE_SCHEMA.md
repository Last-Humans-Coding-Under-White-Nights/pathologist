# SQLite schema

Schema version: **v3**

See also the [README](../README.md) for CLI flags that control what is exported.

## Export modes vs tables

| Table | Minimal (default) | `--full-export` | `--debug-points-to` |
|-------|-------------------|-----------------|---------------------|
| `analysis_run` | ✓ | ✓ | ✓ |
| `files` | ✓ | ✓ | ✓ |
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
files ─┬─ functions ─┬─ call_sites ─ arg_flow_edges → variables
       │             └─ call_edges → functions (caller and callee)
       ├─ variables ─ flow_nodes ─ flow_edges → flow_nodes
       └─ variables (type_id → types when exported)
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
| `schema_version` | INTEGER | Database layout version (currently `3`) |
| `target_root` | TEXT | Analyzed directory |
| `created_at` | TEXT | Unix timestamp (seconds) |
| `options_json` | TEXT | JSON: `include_paths`, `defines`, `include_points_to`, `full_detail` |

### files

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | File id |
| `path` | TEXT UNIQUE | Absolute/normalized path |
| `sha256` | TEXT | Hash placeholder (may be empty) |

### functions

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Function id |
| `name` | TEXT | Linkage-visible name |
| `file_id` | INTEGER FK → `files` | Defining file (original header if include-originated) |
| `line_start` | INTEGER | Start line (always original-file coordinates via LineMap) |
| `line_end` | INTEGER | End line of the definition body; equals `line_start` for prototypes and synthesized externals |
| `linkage` | TEXT | `external`, `internal`, `none` |
| `signature` | TEXT | Placeholder `fn_<name>` |
| `is_defined` | INTEGER | 1 if a body exists under the analyzed root. 0 rows include prototype-only declarations and synthesized externals (libc/logging backends never declared in-tree) |

**Index:** `functions(name)`

Header-defined functions are deduplicated across TUs at merge time (first copy
wins, later copies redirect), so they appear once per origin.

### call_sites

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | Call site id |
| `caller_fn_id` | INTEGER FK → `functions` | Containing function |
| `file_id` | INTEGER FK → `files` | Call location file |
| `line` | INTEGER | Line (always original-file coordinates via LineMap; macro-expansion sites map to the expansion origin) |
| `col` | INTEGER | Column |
| `callee_text` | TEXT | Surface syntax (`foo`, `p->handler`, …) |
| `is_direct` | INTEGER | `1` direct by name; `0` indirect |

Call sites inside header-defined functions are deduplicated by
`(origin file, line, col, callee)` across TUs.

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
| `arg_index` | INTEGER | 0-based argument index |
| `actual_var_id` | INTEGER FK → `variables` | Actual variable (`NULL` if actual is a function) |
| `actual_fn_id` | INTEGER FK → `functions` | Actual function for fn-ptr args (`NULL` if actual is a variable) |
| `formal_var_id` | INTEGER FK → `variables` | Callee parameter var |

Exactly one of `actual_var_id` or `actual_fn_id` is set per row.

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

In minimal export, variables are limited to those referenced by the flow
graph / arg-flow edges; use `--full-export` for every variable.

### flow_nodes

PAG value-flow nodes (`trace inspect dataflow`). Always exported.

| Column | Type | Description |
|--------|------|-------------|
| `id` | INTEGER PK | PAG node id (same id space as `points_to.var_node_id`) |
| `kind` | TEXT | `var`, `loc`, `call_target` (indirect-call site node), or `terminator` (function-model clears event) |
| `label` | TEXT | Human-readable label (variable name, `loc:…`, `fn:…`, `"memset_s clears arg0"`) |
| `detail` | TEXT | Extra context (variable kind, enclosing function, …) |
| `var_id` | INTEGER FK → `variables` | Variable this node belongs to (`NULL` for function locations) |
| `fn_id` | INTEGER FK → `functions` | Enclosing function, when known |

**Index:** `flow_nodes(var_id)`

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
- `points_to` — implicit var → storage-location edge derived from the final
  var→location map.
- `call_arg` — actual-to-formal argument passing from `arg_flow_edges`,
  exported when no stronger constraint already connects the pair.
- `terminates` — terminator visibility edge from a function-model `clears`
  effect (e.g. `memset_s(dst, …)`): the actual-argument node flows into a
  synthetic `terminator` node recording the call site. No points-to value
  is produced; the edge documents where a buffer's prior contents stop.

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
| `stage` | TEXT | `preprocess`, `parse`, `analysis` |

`preprocess` rows are the preprocessor's own diagnostics (missing includes, unknown directives,
unterminated `#if`, mid-file stops), attributed to the file and line where the condition
occurred — a nested header, not the including translation unit — and deduplicated on
`(file_id, line, message)` across translation units. `parse` rows are per unit (`parse errors
in <path>`, `file_id` NULL). See `docs/PREPROCESSOR.md`, "Error recovery".

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

## CLI inspection

```bash
trace inspect graph.db calls [--from FN] [--to FN]
trace inspect graph.db callgraph --file SUBSTR --line N [--depth N] [--direction down|up]
trace inspect graph.db dataflow --file SUBSTR --line N --col C [--depth N] [--direction down|up]
```

- `calls` lists rows from `call_edges` joined with `functions` and left-joined
  with `call_sites` (synthetic IPC edges have no call site).
  `--from` / `--to` match an exact `functions.name` or a C++ suffix (`%::FN`
  with `_`/`%` in `FN` escaped so they are not `LIKE` wildcards).
  `--file` matches ordinary edges by call-site or callee file. For synthetic
  edges it matches the caller or callee definition file.
  Unresolved indirect sites require SQL (query above).
- `callgraph` finds the function whose `[line_start, line_end]` contains the
  given line and prints its transitive callees (`down`) or callers (`up`),
  bounded by `--depth`. Edge labels distinguish `direct`, `indirect`,
  `external`, and `ambiguous` resolution.
- `dataflow` resolves the variable declared nearest the given position (exact
  identifier hit preferred; declarations only — use sites are not recorded)
  and walks the PAG value-flow graph forward (`down`: where the value flows)
  or backward (`up`: where it came from). Edge kinds match `flow_edges.kind`.
  Parameters duplicated across TUs (header prototype vs definition copies)
  are reconciled automatically when the queried copy carries no edges.

Both graph commands print a forest with `(truncated at --depth …)` markers
when the frontier was cut off.
