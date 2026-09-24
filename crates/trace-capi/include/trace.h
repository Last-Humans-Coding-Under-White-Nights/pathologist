/**
 * @file trace.h
 * @brief C API for the trace static-analysis engine.
 *
 * @details
 * Exposes two main surfaces:
 *
 * 1. **Indexing**: #trace_index and #trace_index_ext run the whole analyze
 *    pipeline (discover -> preprocess -> parse/lower -> merge -> solve) against
 *    a project directory and write a SQLite database.
 * 2. **Inspection**: #trace_db_open opens an indexed database read-only to
 *    query functions, symbols, call edges, call graphs, call chains, and
 *    value-flow (dataflow) graphs.
 *
 * ## Memory-Safety Rules
 * - **Handles are opaque**: Only the matching `trace_*_free` / `trace_*_close`
 *   function releases a handle; passing any other pointer is undefined behavior.
 * - **Arena ownership**: Every string a query returns lives in the arena of the
 *   result object it belongs to. Pointers stay valid until that result is freed,
 *   and are invalid afterwards — there is no per-query arena invalidation.
 * - **Borrowed inputs**: All inputs (`const char*`, #trace_index_options, symbol
 *   arrays) are borrowed and copied during the call; they must be valid for the
 *   call's duration only.
 * - **Error and warning strings**: Error messages returned via `char **out_err`
 *   are malloc-style heap strings you own; free them with #trace_string_free.
 *   Non-fatal diagnostics returned via `char **out_warnings` (#trace_index_ext)
 *   are owned the same way.
 * - **Out-pointer discipline**: `*out_err` is cleared to NULL at the start of
 *   every call and set only on failure. On success it is NULL; if it is non-NULL
 *   it holds a heap message you must free. Never reuse a non-NULL `*out_err` across
 *   calls. `*out_warnings` is likewise cleared at the start of #trace_index_ext
 *   and set on success only.
 * - **Thread safety**: Each #trace_db handle is single-threaded; do not share
 *   across threads concurrently.
 *
 * ## Status Codes
 * #TRACE_OK (0) is success; non-zero is an error (see #trace_status). When a
 * function returns an error and @p out_err is non-NULL, `*out_err` is set to
 * a heap message.
 */

#ifndef TRACE_H
#define TRACE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * @brief Status code returned by every `trace_*` function.
 */
typedef enum trace_status {
    TRACE_OK = 0,              /**< Success. */
    TRACE_ERR_INVALID_ARG = 1, /**< Bad argument or ABI mismatch. */
    TRACE_ERR_IO = 2,          /**< Filesystem or I/O error. */
    TRACE_ERR_ANALYSIS = 3,    /**< Pipeline or query failure. */
    TRACE_ERR_NOT_FOUND = 4,   /**< Queried entity does not exist. */
    TRACE_ERR_PANIC = 5,       /**< Rust panic caught at the FFI boundary. */
} trace_status;

/**
 * @brief Traversal direction for graph queries.
 *
 * - #TRACE_DIRECTION_DOWN = downstream: follow callers->callees (call graphs) /
 *   flows-to (direction away from the root on value-flow graphs).
 * - #TRACE_DIRECTION_UP = upstream: follow callees->callers (call graphs) /
 *   flows-from (direction toward the root on value-flow graphs).
 */
typedef enum trace_direction {
    TRACE_DIRECTION_DOWN = 0, /**< Callers->callees / flows-to. */
    TRACE_DIRECTION_UP = 1,   /**< Callees->callers / flows-from. */
} trace_direction;

/**
 * @brief PAG node kind of a value-flow graph node (`flow_nodes.kind`).
 */
typedef enum trace_node_kind {
    TRACE_NODE_VAR = 0,         /**< Variable node. */
    TRACE_NODE_LOC = 1,         /**< Abstract location (storage cell). */
    TRACE_NODE_CALL_TARGET = 2, /**< Indirect-call target node. */
    TRACE_NODE_TERMINATOR = 3,  /**< Function model `clears` zeroing marker. */
    TRACE_NODE_UNKNOWN = -1,    /**< Unrecognized kind or not a PAG node. */
} trace_node_kind;

/**
 * @brief Abstract-location category of a `loc` node (`flow_nodes.detail`).
 */
typedef enum trace_loc_kind {
    TRACE_LOC_GLOBAL = 0,        /**< Global variable storage cell. */
    TRACE_LOC_FILE_STATIC = 1,   /**< File-static variable storage cell. */
    TRACE_LOC_FN_STATIC = 2,     /**< Function-local static variable storage cell. */
    TRACE_LOC_LOCAL = 3,         /**< Stack-allocated local variable storage cell. */
    TRACE_LOC_HEAP = 4,          /**< Dynamic heap allocation site (`new`/`malloc`). */
    TRACE_LOC_FIELD = 5,         /**< Struct/class field location. */
    TRACE_LOC_FIELD_SUMMARY = 6, /**< Instance-insensitive field summary location. */
    TRACE_LOC_ARRAY_SUMMARY = 7, /**< Array summary location for unknown index access. */
    TRACE_LOC_FUNCTION = 8,      /**< Function entry point location. */
    TRACE_LOC_STRING_LIT = 9,    /**< String literal constant interned abstract location. */
    TRACE_LOC_UNKNOWN = -1,      /**< Not a loc node or unrecognized location kind. */
} trace_loc_kind;

/**
 * @brief How a call graph edge was resolved.
 */
typedef enum trace_resolution {
    TRACE_RESOLUTION_DIRECT = 0,    /**< Direct function call. */
    TRACE_RESOLUTION_INDIRECT = 1,  /**< Indirect call resolved via points-to analysis. */
    TRACE_RESOLUTION_AMBIGUOUS = 2, /**< Ambiguous call with multiple possible targets. */
    TRACE_RESOLUTION_EXTERNAL = 3,  /**< Call to an external or unresolved library symbol. */
    /**
     * Synthetic edge injected for an IPC proxy/stub (or client/bridge) pair;
     * carries no call site. Added after 0.1: extend, never renumber.
     */
    TRACE_RESOLUTION_IPC = 4,
    TRACE_RESOLUTION_UNKNOWN = -1,  /**< Unrecognized or unknown resolution type. */
} trace_resolution;

/**
 * @brief Constraint kind carried by a value-flow (dataflow) edge.
 */
typedef enum trace_flow_kind {
    TRACE_FLOW_COPY = 0,       /**< Value or pointer copy assignment (`p = q`). */
    TRACE_FLOW_ADDR_OF = 1,    /**< Address-of operator (`p = &x`). */
    TRACE_FLOW_LOAD = 2,       /**< Dereference/load through pointer (`y = *p`). */
    TRACE_FLOW_STORE = 3,      /**< Store through pointer (`*p = y`). */
    TRACE_FLOW_GEP = 4,        /**< Field address or access (`&obj.field`, `p->field`). */
    TRACE_FLOW_POINTS_TO = 5,  /**< Points-to relationship from pointer to location. */
    TRACE_FLOW_CALL_ARG = 6,   /**< Interprocedural argument flow into a function call. */
    TRACE_FLOW_TERMINATES = 7, /**< Flow terminated by a clearing function model. */
    TRACE_FLOW_DLSYM = 8,      /**< Dynamic symbol lookup (`dlsym`, `GetProcAddress`). */
    TRACE_FLOW_UNKNOWN = -1,   /**< Unrecognized or unknown flow constraint kind. */
} trace_flow_kind;

/**
 * @brief Storage class of a symbol (`variables.kind`).
 */
typedef enum trace_symbol_kind {
    TRACE_SYM_GLOBAL = 0,      /**< Global variable with external linkage. */
    TRACE_SYM_FILE_STATIC = 1, /**< File-scope static variable (`FileStatic`). */
    TRACE_SYM_FN_STATIC = 2,   /**< Function-local static variable (`FnStatic`). */
    TRACE_SYM_PARAM = 3,       /**< Function parameter. */
    TRACE_SYM_LOCAL = 4,       /**< Stack-allocated local variable. */
    TRACE_SYM_UNKNOWN = -1,    /**< Unrecognized or unknown storage class. */
} trace_symbol_kind;

/**
 * @brief Opaque handle to a read-only analysis database.
 *
 * Created via #trace_db_open and released via #trace_db_close.
 * Single-threaded; do not share across threads concurrently.
 */
typedef struct trace_db trace_db;

/**
 * @brief Options for #trace_index and #trace_index_ext.
 *
 * Set `size = sizeof(trace_index_options)` as an ABI guard.
 * Alternatively, leave `size` as 0 for lenient mode (no size check). Lenient
 * mode reads the whole struct as this header declares it, so only a caller
 * compiled against this header may use it; one built against an older,
 * shorter struct must set `size`.
 */
typedef struct trace_index_options {
    size_t                 size;            /**< sizeof(trace_index_options); ABI guard. Set 0 for lenient mode. */
    const char            *root;            /**< Path to project directory to analyze. */
    const char            *output_db;       /**< Destination SQLite database path. */
    const char *const     *includes;        /**< Array of include search directories, or NULL if none. */
    size_t                 n_includes;      /**< Number of paths in @p includes. */
    const char *const     *defines;         /**< Array of macro definitions ("NAME" or "NAME=VALUE"), or NULL if none. */
    size_t                 n_defines;       /**< Number of definitions in @p defines. */
    int32_t                jobs;            /**< Parallel worker threads (<= 0 for auto-detection). */
    int32_t                full_export;     /**< Non-zero to export types, all variables, and locations. */
    int32_t                debug_points_to; /**< Non-zero to retain and export points-to sets. */
    const char *const     *models;          /**< Array of TOML function model file paths, or NULL if none. */
    size_t                 n_models;        /**< Number of model files in @p models. */
    const char *const     *ignore_macros;   /**< Array of macro names/patterns to ignore during lowering, or NULL if none. */
    size_t                 n_ignore_macros; /**< Number of patterns in @p ignore_macros. */
} trace_index_options;

/**
 * @brief Summary counters populated by #trace_index or #trace_index_ext on success.
 */
typedef struct trace_index_result {
    uint64_t files;          /**< Number of source files indexed. */
    uint64_t functions;      /**< Number of functions discovered and indexed. */
    uint64_t call_edges;     /**< Number of call graph edges resolved. */
    uint64_t arg_flow_edges; /**< Number of argument-flow edges tracked. */
} trace_index_result;

/**
 * @brief A function record found by #trace_db_find_functions.
 *
 * String pointers (@p name, @p path) are owned by the containing #trace_function_list arena.
 */
typedef struct trace_function {
    int64_t     id;         /**< Unique function ID in the database. */
    const char *name;       /**< Function name (unmangled or demangled). */
    const char *path;       /**< Source file path where the function is declared or defined. */
    int64_t     line_start; /**< 1-based start line in the source file; 0 for a synthesized external (never declared in-tree). */
    int64_t     line_end;   /**< 1-based end line in the source file; 0 for a synthesized external. */
    int32_t     is_defined; /**< Non-zero if definition body is present; 0 if declaration only. */
} trace_function;

/**
 * @brief A list of functions returned by #trace_db_find_functions.
 *
 * Strings live in the list's arena. Free with #trace_function_list_free.
 */
typedef struct trace_function_list {
    trace_function *items; /**< Array of function items. */
    size_t          count; /**< Number of items in @p items. */
    void           *_impl; /**< Opaque arena owner — do not touch. */
} trace_function_list;

/**
 * @brief A symbol (variable) found by #trace_db_find_symbols.
 *
 * Strings are arena-owned by the containing #trace_symbol_list;
 * @p fn_name is NULL for file-scope symbols. `kind` is one of the
 * #trace_symbol_kind values or `TRACE_SYM_UNKNOWN` (-1); it crosses the
 * boundary as a plain `int32_t` so out-of-band values can be validated
 * (and rejected as #TRACE_ERR_INVALID_ARG when passed back into #trace_db_dataflow)
 * instead of being read as an invalid C enum, which would be undefined behaviour.
 */
typedef struct trace_symbol {
    int64_t     var_id;  /**< Unique variable ID in the database. */
    const char *name;    /**< Symbol name. */
    int32_t     kind;    /**< Storage class (#trace_symbol_kind or TRACE_SYM_UNKNOWN). */
    const char *fn_name; /**< Enclosing function name, or NULL for file-scope symbols. */
    const char *path;    /**< Source file path where the symbol is declared. */
    int64_t     line;    /**< 1-based line of declaration. */
    int64_t     col;     /**< 1-based column of declaration. */
} trace_symbol;

/**
 * @brief A list of symbols returned by #trace_db_find_symbols.
 *
 * Free with #trace_symbol_list_free.
 */
typedef struct trace_symbol_list {
    trace_symbol *items; /**< Array of symbol items. */
    size_t        count; /**< Number of items in @p items. */
    void         *_impl; /**< Opaque arena owner — do not touch. */
} trace_symbol_list;

/**
 * @brief A call graph edge returned by #trace_db_call_edges.
 *
 * Strings are owned by the containing #trace_call_edge_list arena.
 * @p caller_path is the caller's own file (not the call-site file), so it stays
 * meaningful for synthetic IPC-bridge edges (#TRACE_RESOLUTION_IPC) that carry no
 * call site. @p path, @p line, @p col describe the call site, and are NULL/0 on
 * synthetic edges.
 */
typedef struct trace_call_edge {
    int64_t            caller_id;   /**< Caller function ID. */
    const char        *caller_name; /**< Caller function name. */
    const char        *caller_path; /**< Caller's own source file path. */
    const char        *callee_name; /**< Callee function name. */
    const char        *callee_path; /**< Callee source file path, or NULL if synthesized (never declared in-tree). Own declaration path survives for prototype-only externals. */
    trace_resolution   resolution;  /**< Edge resolution kind (#trace_resolution). */
    const char        *path;        /**< Call-site source file path (NULL for synthetic IPC edges). */
    int32_t            line;        /**< 1-based call-site line (0 for synthetic IPC edges). */
    int32_t            col;         /**< 1-based call-site column (0 for synthetic IPC edges). */
} trace_call_edge;

/**
 * @brief A list of call edges returned by #trace_db_call_edges.
 *
 * Free with #trace_call_edge_list_free.
 */
typedef struct trace_call_edge_list {
    trace_call_edge *items; /**< Array of call edge items. */
    size_t           count; /**< Number of items in @p items. */
    void            *_impl; /**< Opaque arena owner — do not touch. */
} trace_call_edge_list;

/**
 * @brief A node in a call or value-flow graph.
 *
 * @p kind and @p loc_kind are meaningful only for value-flow graphs;
 * for call graphs, nodes represent functions and @p kind is #TRACE_NODE_UNKNOWN.
 */
typedef struct trace_graph_node {
    int64_t         id;       /**< Node ID (`functions.id` for call graphs, PAG node ID for dataflow). */
    int64_t         depth;    /**< BFS depth from the starting query node(s). */
    trace_node_kind kind;     /**< PAG node kind (#trace_node_kind; #TRACE_NODE_UNKNOWN for call graphs). */
    trace_loc_kind  loc_kind; /**< Location category (#trace_loc_kind; #TRACE_LOC_UNKNOWN for non-loc nodes). */
    const char     *label;    /**< Node label (e.g. function or variable name). */
    const char     *detail;   /**< Node detail text (e.g. type or location category). */
} trace_graph_node;

/**
 * @brief An edge in a call or value-flow graph.
 *
 * Exactly one of @p resolution or @p flow_kind is meaningful (depending on graph type);
 * the other is set to the corresponding `*_UNKNOWN` value.
 */
typedef struct trace_graph_edge {
    int64_t          from;       /**< Source node ID. */
    int64_t          to;         /**< Target node ID. */
    trace_resolution resolution; /**< Call resolution (for call graphs; #TRACE_RESOLUTION_UNKNOWN for dataflow). */
    trace_flow_kind  flow_kind;  /**< Flow constraint kind (for dataflow; #TRACE_FLOW_UNKNOWN for call graphs). */
    const char      *path;       /**< Source file path of call site, or "" when none. */
    int32_t          line;       /**< 1-based source line number, or 0 when none. */
    int32_t          col;        /**< 1-based source column number, or 0 when none. */
} trace_graph_edge;

/**
 * @brief A call or value-flow graph result.
 *
 * Strings and arrays live in the graph's arena. Free with #trace_graph_free.
 */
typedef struct trace_graph {
    trace_graph_node *nodes;     /**< Array of graph nodes. */
    size_t            n_nodes;   /**< Number of nodes in @p nodes. */
    trace_graph_edge *edges;     /**< Array of graph edges. */
    size_t            n_edges;   /**< Number of edges in @p edges. */
    int32_t           truncated; /**< Non-zero if deeper nodes exist beyond the depth limit. */
    void             *_impl;     /**< Opaque arena owner — do not touch. */
} trace_graph;

/**
 * @brief Free a heap string previously returned via @p out_err or @p out_warnings.
 *
 * Safe to call on NULL.
 *
 * @param[in,out] s Heap string to free, or NULL.
 */
void trace_string_free(char *s);

/**
 * @brief Index a project directory into a SQLite database.
 *
 * `opts->root` must reference a directory containing .c/.cpp sources; the
 * database is written to `opts->output_db` (created/replaced atomically).
 * On success @p out receives summary counters.
 *
 * This is the 0.1 entry point; its signature is frozen. Use #trace_index_ext
 * to also receive the run's non-fatal warnings.
 *
 * @param[in]  opts    Indexing options (root directory, output DB, includes, defines, etc.). Must not be NULL.
 * @param[out] out     Pointer to receive summary counters on success. Must not be NULL.
 * @param[out] out_err Optional pointer to receive a heap error string on failure.
 *                     Cleared to NULL on entry. If non-NULL on failure, caller must free with #trace_string_free.
 * @return #TRACE_OK on success, or an error status code (#TRACE_ERR_INVALID_ARG,
 *         #TRACE_ERR_IO, #TRACE_ERR_ANALYSIS, #TRACE_ERR_PANIC).
 * @see trace_index_ext, trace_index_options, trace_index_result, trace_string_free
 */
trace_status trace_index(const trace_index_options *opts,
                         trace_index_result *out,
                         char **out_err);

/**
 * @brief Index a project directory into a SQLite database, with a warning channel.
 *
 * Behaves exactly like #trace_index, but on success delivers non-fatal
 * diagnostics (e.g. include paths lying outside the analyzed tree) through
 * @p out_warnings when non-NULL: it is set to a heap string (free with
 * #trace_string_free) if the run produced any, else NULL; pass NULL to
 * ignore warnings.
 *
 * @param[in]  opts         Indexing options. Must not be NULL.
 * @param[out] out          Pointer to receive summary counters on success. Must not be NULL.
 * @param[out] out_warnings Optional pointer to receive non-fatal warnings on success.
 *                          Cleared to NULL on entry. If non-NULL, caller must free with #trace_string_free.
 *                          Pass NULL to ignore warnings.
 * @param[out] out_err      Optional pointer to receive an error message on failure.
 *                          Cleared to NULL on entry. If non-NULL, caller must free with #trace_string_free.
 * @return #TRACE_OK on success, or an error status code on failure.
 * @see trace_index, trace_string_free
 */
trace_status trace_index_ext(const trace_index_options *opts,
                             trace_index_result *out,
                             char **out_warnings,
                             char **out_err);

/**
 * @brief Open an indexed database read-only.
 *
 * Returns an owned handle or NULL plus a heap error message in `*out_err`.
 * Close with #trace_db_close.
 *
 * @param[in]  path    Path to the SQLite database file. Must not be NULL.
 * @param[out] out_err Optional pointer to receive a heap error string on failure.
 *                     Cleared to NULL on entry. If non-NULL on failure, caller must free with #trace_string_free.
 * @return Owned #trace_db handle on success, or NULL on failure.
 * @note Handles are single-threaded; do not share across threads concurrently.
 * @see trace_db_close
 */
trace_db *trace_db_open(const char *path, char **out_err);

/**
 * @brief Close a database handle.
 *
 * Safe to call on NULL. The handle is invalid after this call.
 *
 * @param[in,out] db Database handle to close, or NULL.
 * @see trace_db_open
 */
void trace_db_close(trace_db *db);

/**
 * @brief Enumerate functions containing @p line in files whose path contains @p file.
 *
 * Best match first. Fills the arena-backed list into @p out; the list and its
 * strings are released by #trace_function_list_free.
 *
 * @param[in]  db      Database handle. Must not be NULL.
 * @param[in]  file    File path substring to match. Must not be NULL.
 * @param[in]  line    1-based line number.
 * @param[out] out     Function list to populate on success.
 * @param[out] out_err Optional pointer to receive a heap error string on failure.
 * @return #TRACE_OK on success, or an error status code on failure.
 * @see trace_function_list_free
 */
trace_status trace_db_find_functions(trace_db *db,
                                     const char *file,
                                     int64_t line,
                                     trace_function_list *out,
                                     char **out_err);

/**
 * @brief Release a function list and all arena-owned strings inside it.
 *
 * Safe on NULL or zeroed structs. Clears the struct to zeroes on free.
 *
 * @param[in,out] list Function list to free.
 * @see trace_db_find_functions
 */
void trace_function_list_free(trace_function_list *list);

/**
 * @brief Enumerate variables declared on or near `line:col` in files whose path contains @p file.
 *
 * Best candidate first. Fills the arena-backed list into @p out; the list and its
 * strings are released by #trace_symbol_list_free.
 *
 * @param[in]  db      Database handle. Must not be NULL.
 * @param[in]  file    File path substring to match. Must not be NULL.
 * @param[in]  line    1-based line number.
 * @param[in]  col     1-based column number.
 * @param[out] out     Symbol list to populate on success.
 * @param[out] out_err Optional pointer to receive a heap error string on failure.
 * @return #TRACE_OK on success, or an error status code on failure.
 * @see trace_symbol_list_free
 */
trace_status trace_db_find_symbols(trace_db *db,
                                   const char *file,
                                   int64_t line,
                                   int64_t col,
                                   trace_symbol_list *out,
                                   char **out_err);

/**
 * @brief Release a symbol list and all arena-owned strings inside it.
 *
 * Safe on NULL or zeroed structs. Clears the struct to zeroes on free.
 *
 * @param[in,out] list Symbol list to free.
 * @see trace_db_find_symbols
 */
void trace_symbol_list_free(trace_symbol_list *list);

/**
 * @brief List call edges, optionally filtered by caller, callee, and file path.
 *
 * Filters:
 * - @p from matches caller name (exact or C++ `::`-qualified suffix). NULL to skip.
 * - @p to matches callee name (exact or C++ `::`-qualified suffix). NULL to skip.
 * - @p file matches path substring. NULL to skip.
 *
 * `caller_path` is the caller's OWN file (not the call-site file), so it stays
 * meaningful for synthetic IPC-bridge edges (#TRACE_RESOLUTION_IPC) that carry no
 * call site. `path`/`line`/`col` are the actual call site, and are NULL/0 on
 * synthetic edges.
 *
 * @param[in]  db      Database handle. Must not be NULL.
 * @param[in]  from    Caller filter, or NULL to match all callers.
 * @param[in]  to      Callee filter, or NULL to match all callees.
 * @param[in]  file    File substring filter, or NULL to match all files.
 * @param[out] out     Call edge list to populate on success.
 * @param[out] out_err Optional pointer to receive a heap error string on failure.
 * @return #TRACE_OK on success, or an error status code on failure.
 * @see trace_call_edge_list_free
 */
trace_status trace_db_call_edges(trace_db *db,
                                 const char *from,
                                 const char *to,
                                 const char *file,
                                 trace_call_edge_list *out,
                                 char **out_err);

/**
 * @brief Release a call edge list and all arena-owned strings inside it.
 *
 * Safe on NULL or zeroed structs. Clears the struct to zeroes on free.
 *
 * @param[in,out] list Call edge list to free.
 * @see trace_db_call_edges
 */
void trace_call_edge_list_free(trace_call_edge_list *list);

/**
 * @brief Bounded BFS over the call graph rooted at @p root_fn_id.
 *
 * Down follows callers->callees; up follows callees->callers.
 * @p direction must be #TRACE_DIRECTION_DOWN or #TRACE_DIRECTION_UP;
 * other values are rejected with #TRACE_ERR_INVALID_ARG.
 *
 * @param[in]  db          Database handle. Must not be NULL.
 * @param[in]  root_fn_id  Function ID of the root function.
 * @param[in]  direction   Traversal direction (#TRACE_DIRECTION_DOWN or #TRACE_DIRECTION_UP).
 * @param[in]  depth       Maximum search depth (must be >= 1).
 * @param[out] out         Graph result to populate on success. Free with #trace_graph_free.
 * @param[out] out_err     Optional pointer to receive a heap error string on failure.
 * @return #TRACE_OK on success, or an error status code on failure.
 * @see trace_graph_free
 */
trace_status trace_db_callgraph(trace_db *db,
                                int64_t root_fn_id,
                                trace_direction direction,
                                uint32_t depth,
                                trace_graph *out,
                                char **out_err);

/**
 * @brief Find all call chains connecting @p from_fn_id and @p to_fn_id bounded by @p depth steps.
 *
 * Direction DOWN searches callers->callees; UP searches callees->callers.
 * @p limit caps the number of discovered chains (0 for unlimited).
 * `depth >= 1` (unless `from_fn_id == to_fn_id` where depth 0 yields a trivial chain).
 *
 * @param[in]  db          Database handle. Must not be NULL.
 * @param[in]  from_fn_id  Source function ID.
 * @param[in]  to_fn_id    Destination function ID.
 * @param[in]  direction   Traversal direction (#TRACE_DIRECTION_DOWN or #TRACE_DIRECTION_UP).
 * @param[in]  depth       Maximum chain length in steps.
 * @param[in]  limit       Maximum number of chains to discover (0 for unlimited).
 * @param[out] out         Graph result to populate on success. Free with #trace_graph_free.
 * @param[out] out_err     Optional pointer to receive a heap error string on failure.
 * @return #TRACE_OK on success, or an error status code on failure.
 * @see trace_graph_free
 */
trace_status trace_db_call_chains(trace_db *db,
                                  int64_t from_fn_id,
                                  int64_t to_fn_id,
                                  trace_direction direction,
                                  uint32_t depth,
                                  size_t limit,
                                  trace_graph *out,
                                  char **out_err);

/**
 * @brief Bounded BFS over the value-flow graph starting at the variables described by @p roots.
 *
 * Roots are typically output of #trace_db_find_symbols. `depth >= 1` and `n_roots >= 1`;
 * @p direction must be DOWN or UP (other values are rejected with #TRACE_ERR_INVALID_ARG).
 * Pass the single best candidate (`items[0]`) to start from one variable, matching the CLI.
 * Each `roots[i].kind` is validated (#TRACE_ERR_INVALID_ARG on an out-of-band value).
 *
 * @param[in]  db          Database handle. Must not be NULL.
 * @param[in]  roots       Array of starting symbols (must not be NULL).
 * @param[in]  n_roots     Number of symbols in @p roots (must be >= 1).
 * @param[in]  direction   Traversal direction (#TRACE_DIRECTION_DOWN or #TRACE_DIRECTION_UP).
 * @param[in]  depth       Maximum search depth (must be >= 1).
 * @param[out] out         Graph result to populate on success. Free with #trace_graph_free.
 * @param[out] out_err     Optional pointer to receive a heap error string on failure.
 * @return #TRACE_OK on success, or an error status code on failure.
 * @see trace_graph_free
 */
trace_status trace_db_dataflow(trace_db *db,
                               const trace_symbol *roots,
                               size_t n_roots,
                               trace_direction direction,
                               uint32_t depth,
                               trace_graph *out,
                               char **out_err);

/**
 * @brief Release a graph result and every string it owns.
 *
 * Safe to call on NULL or zeroed structs. Clears the struct to zeroes on free.
 *
 * @param[in,out] graph Graph result to free.
 * @see trace_db_callgraph, trace_db_call_chains, trace_db_dataflow
 */
void trace_graph_free(trace_graph *graph);

#ifdef __cplusplus
}
#endif

#endif /* TRACE_H */