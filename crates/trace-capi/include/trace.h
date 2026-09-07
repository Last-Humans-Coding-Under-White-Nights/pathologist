/*
 * trace.h — C API for the trace static-analysis engine.
 *
 * Two surfaces:
 *
 *   1. Indexing: `trace_index` runs the whole analyze pipeline against a
 *      project directory and writes a SQLite database.
 *   2. Inspection: open an indexed database and query functions, symbols,
 *      call edges, call graphs and value-flow (dataflow) graphs.
 *
 * Memory-safety rules
 * -------------------
 *  - Handles are opaque. Only the matching `trace_*_free` / `trace_*_close`
 *    function releases a handle; passing any other pointer is UB.
 *  - Every string a query returns lives in the arena of the result object it
 *    belongs to. Pointers stay valid until that result is freed, and are
 *    invalid afterwards — there is no per-query arena invalidation.
 *  - All inputs (`const char*`, `trace_index_options`, symbol arrays) are
 *    borrowed and copied during the call; they must be valid for the call's
 *    duration only.
 *  - Error messages returned via `char **out_err` are malloc-style heap
 *    strings you own; free them with `trace_string_free`. Non-fatal
 *    diagnostics returned via `char **out_warnings` (trace_index) are owned
 *    the same way.
 *  - `*out_err` is cleared to NULL at the start of every call and set only
 *    on failure. On success it is NULL; if it is non-NULL it holds a heap
 *    message you must free. Never reuse a non-NULL `*out_err` across calls.
 *    `*out_warnings` is likewise cleared at the start of `trace_index` and
 *    set on success only.
 *  - Each handle is single-threaded; do not share across threads.
 *
 * Status codes
 * ------------
 * 0 (TRACE_OK) is success; non-zero is an error. When a function returns an
 * error and `out_err` is non-null, `*out_err` is set to a heap message.
 */

#ifndef TRACE_H
#define TRACE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status code returned by every trace_* function. */
typedef enum trace_status {
    TRACE_OK = 0,
    TRACE_ERR_INVALID_ARG = 1, /* bad argument / ABI mismatch */
    TRACE_ERR_IO = 2,          /* filesystem error */
    TRACE_ERR_ANALYSIS = 3,    /* pipeline / query failure */
    TRACE_ERR_NOT_FOUND = 4,   /* queried entity does not exist */
    TRACE_ERR_PANIC = 5,       /* Rust panic caught at the boundary */
} trace_status;

/* Traversal direction for graph queries.
   DOWN = downstream: follow callers->callees (call graphs) / flows-to
           (moves-from-the-root direction on value-flow graphs).
   UP   = upstream:   follow callees->callers (call graphs) / flows-from. */
typedef enum trace_direction {
    TRACE_DIRECTION_DOWN = 0, /* callers->callees / flows-to */
    TRACE_DIRECTION_UP = 1,   /* callees->callers / flows-from */
} trace_direction;

/* PAG node kind of a value-flow graph node (flow_nodes.kind). */
typedef enum trace_node_kind {
    TRACE_NODE_VAR = 0,         /* variable node */
    TRACE_NODE_LOC = 1,         /* abstract location (storage cell) */
    TRACE_NODE_CALL_TARGET = 2, /* indirect-call target node */
    TRACE_NODE_TERMINATOR = 3,  /* model `clears` zeroing marker */
    TRACE_NODE_UNKNOWN = -1,    /* unrecognized / not a PAG node */
} trace_node_kind;

/* Abstract-location category of a `loc` node. */
typedef enum trace_loc_kind {
    TRACE_LOC_GLOBAL = 0,
    TRACE_LOC_FILE_STATIC = 1,
    TRACE_LOC_FN_STATIC = 2,
    TRACE_LOC_LOCAL = 3,
    TRACE_LOC_HEAP = 4,
    TRACE_LOC_FIELD = 5,
    TRACE_LOC_FIELD_SUMMARY = 6,
    TRACE_LOC_ARRAY_SUMMARY = 7,
    TRACE_LOC_FUNCTION = 8,
    TRACE_LOC_STRING_LIT = 9,
    TRACE_LOC_UNKNOWN = -1, /* not a loc node / unrecognized */
} trace_loc_kind;

/* How a call graph edge was resolved. */
typedef enum trace_resolution {
    TRACE_RESOLUTION_DIRECT = 0,
    TRACE_RESOLUTION_INDIRECT = 1,
    TRACE_RESOLUTION_AMBIGUOUS = 2,
    TRACE_RESOLUTION_EXTERNAL = 3,
    /* Synthetic edge injected for an IPC proxy/stub (or client/bridge) pair;
       it carries no call site. Added after 0.1: extend, never renumber. */
    TRACE_RESOLUTION_IPC = 4,
    TRACE_RESOLUTION_UNKNOWN = -1,
} trace_resolution;

/* Constraint kind carried by a value-flow (dataflow) edge. */
typedef enum trace_flow_kind {
    TRACE_FLOW_COPY = 0,
    TRACE_FLOW_ADDR_OF = 1,
    TRACE_FLOW_LOAD = 2,
    TRACE_FLOW_STORE = 3,
    TRACE_FLOW_GEP = 4,
    TRACE_FLOW_POINTS_TO = 5,
    TRACE_FLOW_CALL_ARG = 6,
    TRACE_FLOW_TERMINATES = 7,
    TRACE_FLOW_DLSYM = 8,
    TRACE_FLOW_UNKNOWN = -1,
} trace_flow_kind;

/* Storage class of a symbol (variables.kind). */
typedef enum trace_symbol_kind {
    TRACE_SYM_GLOBAL = 0,
    TRACE_SYM_FILE_STATIC = 1,
    TRACE_SYM_FN_STATIC = 2,
    TRACE_SYM_PARAM = 3,
    TRACE_SYM_LOCAL = 4,
    TRACE_SYM_UNKNOWN = -1,
} trace_symbol_kind;

/* Opaque handle to a read-only analysis database. */
typedef struct trace_db trace_db;

/* Options for trace_index. Set size = sizeof(trace_index_options). */
typedef struct trace_index_options {
    size_t                 size;              /* sizeof(self); ABI guard */
    const char            *root;              /* project directory */
    const char            *output_db;         /* destination SQLite path */
    const char *const     *includes;          /* include search dirs; NULL if none */
    size_t                 n_includes;
    const char *const     *defines;           /* "NAME" or "NAME=VALUE"; NULL if none */
    size_t                 n_defines;
    int32_t                jobs;              /* <=0: auto */
    int32_t                full_export;       /* types/all vars/locations */
    int32_t                debug_points_to;   /* retain + export points-to */
    const char *const     *models;            /* TOML model files; NULL if none */
    size_t                 n_models;
} trace_index_options;

/* Summary counters filled on success. */
typedef struct trace_index_result {
    uint64_t files;
    uint64_t functions;
    uint64_t call_edges;
    uint64_t arg_flow_edges;
} trace_index_result;

/* A function found by trace_db_find_functions. Strings are arena-owned. */
typedef struct trace_function {
    int64_t     id;
    const char *name;
    const char *path;
    int64_t     line_start;
    int64_t     line_end;
    int32_t     is_defined;
} trace_function;

/* A list of functions; strings live in the list's arena. */
typedef struct trace_function_list {
    trace_function *items;
    size_t          count;
    void           *_impl; /* opaque arena owner — do not touch */
} trace_function_list;

/* A symbol found by trace_db_find_symbols. Strings are arena-owned;
   fn_name is NULL for file-scope symbols. `kind` is one of the
   TRACE_SYM_* values or TRACE_SYM_UNKNOWN (-1); it crosses the boundary as a
   plain int32_t so out-of-band values can be validated (and rejected as
   TRACE_ERR_INVALID_ARG when passed back into trace_db_dataflow) instead of
   being read as an invalid C enum, which would be undefined behaviour. */
typedef struct trace_symbol {
    int64_t     var_id;
    const char *name;
    int32_t     kind;
    const char *fn_name;
    const char *path;
    int64_t     line;
    int64_t     col;
} trace_symbol;

typedef struct trace_symbol_list {
    trace_symbol *items;
    size_t        count;
    void         *_impl;
} trace_symbol_list;

/* A call graph edge from trace_db_call_edges. */
typedef struct trace_call_edge {
    int64_t            caller_id;
    const char        *caller_name;
    const char        *caller_path;
    const char        *callee_name;
    const char        *callee_path;
    trace_resolution   resolution;
    const char        *path; /* call-site file */
    int32_t            line; /* 1-based; 0 when none */
    int32_t            col;  /* 1-based; 0 when none */
} trace_call_edge;

typedef struct trace_call_edge_list {
    trace_call_edge *items;
    size_t           count;
    void            *_impl;
} trace_call_edge_list;

/* A call or value-flow graph node. `kind`/`loc_kind` are meaningful only for
   value-flow graphs (call-graph nodes are functions -> TRACE_NODE_UNKNOWN). */
typedef struct trace_graph_node {
    int64_t         id;
    int64_t         depth;      /* BFS depth from the start nodes */
    trace_node_kind kind;
    trace_loc_kind  loc_kind;
    const char     *label;
    const char     *detail;
} trace_graph_node;

/* A call or value-flow graph edge. Exactly one of `resolution`/`flow_kind`
   is meaningful (by graph type); the other is the *_UNKNOWN value. */
typedef struct trace_graph_edge {
    int64_t          from;
    int64_t          to;
    trace_resolution resolution; /* call graphs */
    trace_flow_kind  flow_kind;  /* dataflow   */
    const char      *path;       /* source path; "" when none */
    int32_t          line;       /* 1-based; 0 when none */
    int32_t          col;        /* 1-based; 0 when none */
} trace_graph_edge;

/* A call or value-flow graph result. Strings live in the graph's arena. */
typedef struct trace_graph {
    trace_graph_node *nodes;
    size_t            n_nodes;
    trace_graph_edge *edges;
    size_t            n_edges;
    int32_t           truncated; /* deeper nodes exist beyond the depth limit */
    void             *_impl;
} trace_graph;

/*
 * Free a heap string previously returned via `out_err`. Safe on NULL.
 */
void trace_string_free(char *s);

/*
 * Index a project directory into a SQLite database.
 *
 * `opts->root` must reference a directory containing .c/.cpp sources; the
 * database is written to `opts->output_db` (created/replaced atomically).
 * On success `*out` receives summary counters. Return values: TRACE_OK,
 * TRACE_ERR_INVALID_ARG (incl. ABI mismatch), TRACE_ERR_IO (unusable output
 * path / export I/O), TRACE_ERR_ANALYSIS, TRACE_ERR_PANIC.
 *
 * This is the 0.1 entry point; its signature is frozen. Use
 * `trace_index_ext` to also receive the run's non-fatal warnings.
 */
trace_status trace_index(const trace_index_options *opts,
                         trace_index_result *out,
                         char **out_err);

/*
 * Index a project directory into a SQLite database, with a warning channel.
 *
 * Behaves exactly like `trace_index`, but on success delivers non-fatal
 * diagnostics (e.g. include paths lying outside the analyzed tree) through
 * `out_warnings` when non-null: it is set to a heap string (free with
 * `trace_string_free`) if the run produced any, else NULL; pass NULL to
 * ignore warnings.
 */
trace_status trace_index_ext(const trace_index_options *opts,
                             trace_index_result *out,
                             char **out_warnings,
                             char **out_err);

/*
 * Open an indexed database read-only. Returns an owned handle or NULL plus a
 * heap error message in `*out_err`. Close with `trace_db_close`.
 */
trace_db *trace_db_open(const char *path, char **out_err);

/*
 * Close a database handle. Safe on NULL; invalid after call.
 */
void trace_db_close(trace_db *db);

/*
 * Enumerate functions containing `line` in files whose path contains `file`,
 * best match first. Fill the arena-backed list into `*out`; the list and its
 * strings are released by `trace_function_list_free`.
 */
trace_status trace_db_find_functions(trace_db *db,
                                     const char *file,
                                     int64_t line,
                                     trace_function_list *out,
                                     char **out_err);
void trace_function_list_free(trace_function_list *list);

/*
 * Enumerate variables declared on/near `line:col` in files whose path
 * contains `file`, best candidate first.
 */
trace_status trace_db_find_symbols(trace_db *db,
                                   const char *file,
                                   int64_t line,
                                   int64_t col,
                                   trace_symbol_list *out,
                                   char **out_err);
void trace_symbol_list_free(trace_symbol_list *list);

/*
 * List call edges, optionally filtered by caller (`from`), callee (`to`) and
 * a path substring (`file`). `from`/`to` match an exact name or a
 * C++ `::`-qualified suffix (e.g. "Plugin" matches "ns::Plugin::foo").
 * NULL filters are skipped.
 *
 * `caller_path` is the caller's OWN file (not the call-site file), so it stays
 * meaningful for synthetic IPC-bridge edges (resolution IPC) that carry no
 * call site. `path`/`line`/`col` are the actual call site, and are NULL/0 on
 * synthetic edges.
 */
trace_status trace_db_call_edges(trace_db *db,
                                 const char *from,
                                 const char *to,
                                 const char *file,
                                 trace_call_edge_list *out,
                                 char **out_err);
void trace_call_edge_list_free(trace_call_edge_list *list);

/*
 * Bounded BFS over the call graph rooted at `root_fn_id`. `depth >= 1`.
 * Down follows callers->callees; up follows callees->callers. `direction`
 * must be TRACE_DIRECTION_DOWN or TRACE_DIRECTION_UP; other values are
 * rejected with TRACE_ERR_INVALID_ARG.
 */
trace_status trace_db_callgraph(trace_db *db,
                                int64_t root_fn_id,
                                trace_direction direction,
                                uint32_t depth,
                                trace_graph *out,
                                char **out_err);

/*
 * Bounded BFS over the value-flow graph starting at the variables described
 * by `roots` (typically output of `trace_db_find_symbols`). `depth >= 1` and
 * `n_roots >= 1`; `direction` must be DOWN or UP (other values are rejected
 * with TRACE_ERR_INVALID_ARG). Pass the single best candidate (`items[0]`) to
 * start from one variable, matching the CLI. Each `roots[i].kind` is validated
 * (`TRACE_ERR_INVALID_ARG` on an out-of-band value).
 */
trace_status trace_db_dataflow(trace_db *db,
                               const trace_symbol *roots,
                               size_t n_roots,
                               trace_direction direction,
                               uint32_t depth,
                               trace_graph *out,
                               char **out_err);

/*
 * Release a graph result and every string it owns. Safe on NULL.
 */
void trace_graph_free(trace_graph *graph);

#ifdef __cplusplus
}
#endif

#endif /* TRACE_H */