/*
 * ctrace.c — minimal C CLI over the trace C API.
 *
 * Demonstrates the index + inspect surfaces:
 *
 *   ctrace analyze ROOT -o DB [-I dir] [-D NAME=VALUE] [--full-export]
 *                        [--debug-points-to] [--jobs N] [--models FILE]
 *   ctrace inspect DB functions FILE LINE
 *   ctrace inspect DB symbols FILE LINE COL
 *   ctrace inspect DB calls [--from FN] [--to FN] [--file SUBSTR]
 *   ctrace inspect DB callgraph --file SUBSTR --line N [--depth N]
 *                               [--direction up|down]
 *   ctrace inspect DB dataflow --file SUBSTR --line N --col C [--depth N]
 *                              [--direction up|down]
 *   ctrace inspect DB callchain FROM_ID TO_ID [DEPTH] [DIRECTION] [LIMIT]
 *                            (or --from ID --to ID [--depth N] [--direction up|down] [--limit N])
 *
 * Build:
 *   cc ctrace.c -I ../include -L target/release -ltrace_capi -o ctrace
 *   LD_LIBRARY_PATH=target/release ./ctrace analyze tests/fixtures/direct_call -o /tmp/t.db
 */

#include <trace.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---------- small helpers ------------------------------------------------- */

static const char *res_str(trace_resolution r) {
    switch (r) {
    case TRACE_RESOLUTION_DIRECT: return "direct";
    case TRACE_RESOLUTION_INDIRECT: return "indirect";
    case TRACE_RESOLUTION_AMBIGUOUS: return "ambiguous";
    case TRACE_RESOLUTION_EXTERNAL: return "external";
    case TRACE_RESOLUTION_IPC: return "ipc";
    default: return "?";
    }
}

static const char *flow_str(trace_flow_kind k) {
    switch (k) {
    case TRACE_FLOW_COPY: return "copy";
    case TRACE_FLOW_ADDR_OF: return "addr_of";
    case TRACE_FLOW_LOAD: return "load";
    case TRACE_FLOW_STORE: return "store";
    case TRACE_FLOW_GEP: return "gep";
    case TRACE_FLOW_POINTS_TO: return "points_to";
    case TRACE_FLOW_CALL_ARG: return "call_arg";
    case TRACE_FLOW_TERMINATES: return "terminates";
    case TRACE_FLOW_DLSYM: return "dlsym";
    default: return "?";
    }
}

static const char *sym_kind_str(trace_symbol_kind k) {
    switch (k) {
    case TRACE_SYM_GLOBAL: return "global";
    case TRACE_SYM_FILE_STATIC: return "file_static";
    case TRACE_SYM_FN_STATIC: return "fn_static";
    case TRACE_SYM_PARAM: return "param";
    case TRACE_SYM_LOCAL: return "local";
    default: return "?";
    }
}

static const char *basename(const char *p) {
    const char *s = strrchr(p, '/');
    return s ? s + 1 : p;
}

static void print_err(char **err) {
    if (err && *err) {
        fprintf(stderr, "error: %s\n", *err);
        trace_string_free(*err);
        *err = NULL;
    }
}

/* ---------- commands ------------------------------------------------------ */

static int cmd_analyze(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: ctrace analyze ROOT [-o db] [-I dir] [-D NAME=VALUE] "
                        "[--full-export] [--debug-points-to] [--jobs N] [--models FILE]\n");
        return 2;
    }
    const char *root = argv[1];
    const char *output = "trace.db";
    const char *includes[64];
    const char *defines[64];
    const char *models[16];
    size_t n_inc = 0, n_def = 0, n_mod = 0;
    int32_t jobs = 0;
    int32_t full_export = 0, debug_points_to = 0;

    for (int i = 2; i < argc; i++) {
        if (!strcmp(argv[i], "-o") && i + 1 < argc) output = argv[++i];
        else if (!strcmp(argv[i], "-I") && i + 1 < argc && n_inc < 64) includes[n_inc++] = argv[++i];
        else if (!strcmp(argv[i], "-D") && i + 1 < argc && n_def < 64) defines[n_def++] = argv[++i];
        else if (!strcmp(argv[i], "--full-export")) full_export = 1;
        else if (!strcmp(argv[i], "--debug-points-to")) debug_points_to = 1;
        else if (!strcmp(argv[i], "--jobs") && i + 1 < argc) jobs = (int32_t)atoi(argv[++i]);
        else if (!strcmp(argv[i], "--models") && i + 1 < argc && n_mod < 16) models[n_mod++] = argv[++i];
        else {
            fprintf(stderr, "unknown argument: %s\n", argv[i]);
            return 2;
        }
    }

    trace_index_options opts;
    memset(&opts, 0, sizeof(opts));
    opts.size = sizeof(opts);
    opts.root = root;
    opts.output_db = output;
    opts.includes = n_inc ? includes : NULL;
    opts.n_includes = n_inc;
    opts.defines = n_def ? defines : NULL;
    opts.n_defines = n_def;
    opts.jobs = jobs;
    opts.full_export = full_export;
    opts.debug_points_to = debug_points_to;
    opts.models = n_mod ? models : NULL;
    opts.n_models = n_mod;

    trace_index_result r;
    char *err = NULL;
    char *warnings = NULL;
    trace_status st = trace_index_ext(&opts, &r, &warnings, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        printf("index failed (status %d)\n", (int)st);
        return 1;
    }
    if (warnings) {
        fprintf(stderr, "warning: %s\n", warnings);
        trace_string_free(warnings);
    }
    printf("indexed: %llu files, %llu functions, %llu call edges, %llu arg-flow edges -> %s\n",
           (unsigned long long)r.files, (unsigned long long)r.functions,
           (unsigned long long)r.call_edges, (unsigned long long)r.arg_flow_edges, output);
    return 0;
}

static trace_db *open_db(const char *path, char **err) {
    return trace_db_open(path, err);
}

static int cmd_functions(trace_db *db, const char *file, long long line) {
    trace_function_list fns;
    memset(&fns, 0, sizeof(fns));
    char *err = NULL;
    trace_status st = trace_db_find_functions(db, file, line, &fns, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    for (size_t i = 0; i < fns.count; i++) {
        const trace_function *f = &fns.items[i];
        printf("%s (%s:%lld-%lld)%s\n", f->name, basename(f->path),
               (long long)f->line_start, (long long)f->line_end,
               f->is_defined ? "" : " [external]");
    }
    if (fns.count == 0) printf("no function contains %s:%lld\n", file, line);
    trace_function_list_free(&fns);
    return 0;
}

static int cmd_symbols(trace_db *db, const char *file, long long line, long long col) {
    trace_symbol_list syms;
    memset(&syms, 0, sizeof(syms));
    char *err = NULL;
    trace_status st = trace_db_find_symbols(db, file, line, col, &syms, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    for (size_t i = 0; i < syms.count; i++) {
        const trace_symbol *s = &syms.items[i];
        printf("%s (%s) %s:%lld:%lld",
               s->name, sym_kind_str(s->kind), basename(s->path),
               (long long)s->line, (long long)s->col);
        if (s->fn_name) printf(" in %s", s->fn_name);
        printf("\n");
    }
    if (syms.count == 0) printf("no symbol near %s:%lld:%lld\n", file, line, col);
    trace_symbol_list_free(&syms);
    return 0;
}

static int cmd_calls(trace_db *db, const char *from, const char *to, const char *file) {
    trace_call_edge_list edges;
    memset(&edges, 0, sizeof(edges));
    char *err = NULL;
    trace_status st = trace_db_call_edges(db, from, to, file, &edges, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    for (size_t i = 0; i < edges.count; i++) {
        const trace_call_edge *e = &edges.items[i];
        /* Synthetic IPC-bridge edges carry no call site (path == NULL). */
        if (e->path) {
            printf("%s (%s:%d) -> %s (%s) [%s] (%s)\n",
                   e->caller_name, basename(e->caller_path), (int)e->line,
                   e->callee_name, basename(e->callee_path),
                   res_str(e->resolution), basename(e->path));
        } else {
            printf("%s -> %s [%s] (%s)\n",
                   e->caller_name, e->callee_name,
                   res_str(e->resolution), basename(e->callee_path));
        }
    }
    if (edges.count == 0) printf("no call edges\n");
    trace_call_edge_list_free(&edges);
    return 0;
}

static void print_graph(const trace_graph *g) {
    /* discovery order */
    for (size_t i = 0; i < g->n_nodes; i++) {
        const trace_graph_node *n = &g->nodes[i];
        printf("%*s* %s", (int)n->depth * 2, "", n->label);
        if (n->detail && n->detail[0]) printf(" (%s)", n->detail);
        printf("\n");
    }
    for (size_t i = 0; i < g->n_edges; i++) {
        const trace_graph_edge *e = &g->edges[i];
        const char *kind = e->resolution != TRACE_RESOLUTION_UNKNOWN
                               ? res_str(e->resolution)
                               : flow_str(e->flow_kind);
        printf("  %lld -%s-> %lld", (long long)e->from, kind, (long long)e->to);
        if (e->path && e->path[0]) printf(" (%s:%d)", basename(e->path), (int)e->line);
        printf("\n");
    }
    printf("%zu nodes, %zu edges%s\n", g->n_nodes, g->n_edges,
           g->truncated ? " (truncated)" : "");
}

/* Returns TRACE_DIRECTION_DOWN/UP, or -1 for anything else. The library
 * rejects out-of-band values with TRACE_ERR_INVALID_ARG, so the example must
 * not silently coerce "sideways" to one direction. */
static trace_direction parse_dir(const char *s) {
    if (!strcmp(s, "up")) return TRACE_DIRECTION_UP;
    if (!strcmp(s, "down")) return TRACE_DIRECTION_DOWN;
    return (trace_direction)-1;
}

/* Bounded BFS depth; rejects values the library would reject (0 or negative). */
static long parse_depth(const char *s) {
    char *end = NULL;
    long d = strtol(s, &end, 10);
    if (end == s || *end != '\0' || d < 1) return -1;
    return d;
}

static int cmd_callgraph(trace_db *db, const char *file, long long line,
                         long depth, trace_direction dir) {
    trace_function_list fns;
    memset(&fns, 0, sizeof(fns));
    char *err = NULL;
    trace_status st = trace_db_find_functions(db, file, line, &fns, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    if (fns.count == 0) {
        printf("no function contains %s:%lld\n", file, line);
        trace_function_list_free(&fns);
        return 1;
    }
    int64_t root = fns.items[0].id;
    trace_function_list_free(&fns);

    trace_graph g;
    memset(&g, 0, sizeof(g));
    st = trace_db_callgraph(db, root, dir, (uint32_t)depth, &g, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    print_graph(&g);
    trace_graph_free(&g);
    return 0;
}

static int cmd_dataflow(trace_db *db, const char *file, long long line,
                        long long col, long depth, trace_direction dir) {
    trace_symbol_list syms;
    memset(&syms, 0, sizeof(syms));
    char *err = NULL;
    trace_status st = trace_db_find_symbols(db, file, line, col, &syms, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    if (syms.count == 0) {
        printf("no symbol near %s:%lld:%lld\n", file, line, col);
        trace_symbol_list_free(&syms);
        return 1;
    }

    trace_graph g;
    memset(&g, 0, sizeof(g));
    /* Use only the best (first) candidate, matching the CLI: it is the one
       covering the line/col or the nearest column, not every declaration. */
    st = trace_db_dataflow(db, syms.items, 1, dir, (uint32_t)depth, &g, &err);
    trace_symbol_list_free(&syms);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    print_graph(&g);
    trace_graph_free(&g);
    return 0;
}

static int cmd_chains(trace_db *db, int64_t from_id, int64_t to_id,
                      long depth, trace_direction dir, size_t limit) {
    trace_graph g;
    memset(&g, 0, sizeof(g));
    char *err = NULL;
    trace_status st = trace_db_call_chains(db, from_id, to_id, dir, (uint32_t)depth, limit, &g, &err);
    if (st != TRACE_OK) {
        print_err(&err);
        return 1;
    }
    print_graph(&g);
    trace_graph_free(&g);
    return 0;
}

/* argument parser for `inspect` subcommands is inline in cmd_inspect. */
static int cmd_inspect(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: ctrace inspect DB {functions|symbols|calls|callgraph|dataflow|callchain} ...\n");
        return 2;
    }
    const char *db_path = argv[1];
    const char *cmd = argv[2];
    char *err = NULL;
    trace_db *db = open_db(db_path, &err);
    if (!db) {
        print_err(&err);
        return 1;
    }

    /* Collect `--key value` pairs and positional args separately. */
    int nflags = 0, npos = 0;
    for (int i = 3; i < argc; i++) {
        if (argv[i][0] == '-' && argv[i][1]) {
            nflags += (i + 1 < argc) ? 2 : 1;
            if (i + 1 < argc) i++;
        } else {
            npos++;
        }
    }
    char **flags = nflags ? malloc((size_t)nflags * sizeof(*flags)) : NULL;
    char **positionals = npos ? malloc((size_t)npos * sizeof(*positionals)) : NULL;
    int nf = 0, np = 0;
    {
        for (int i = 3; i < argc; i++) {
            if (argv[i][0] == '-' && argv[i][1]) {
                if (i + 1 < argc && nf + 1 < nflags) {
                    flags[nf++] = argv[i];
                    flags[nf++] = argv[++i];
                }
            } else if (np < npos) {
                positionals[np++] = argv[i];
            }
        }
    }

    const char *file = NULL, *line_s = NULL, *col_s = NULL;
    const char *from = NULL, *to = NULL, *sub = NULL;
    const char *depth_s = NULL, *dir_s = NULL, *limit_s = NULL;
    for (int i = 0; i + 1 < nf; i += 2) {
        const char *k = flags[i];
        const char *v = flags[i + 1];
        if (!strcmp(k, "--file")) file = v;
        else if (!strcmp(k, "--line")) line_s = v;
        else if (!strcmp(k, "--col")) col_s = v;
        else if (!strcmp(k, "--from")) from = v;
        else if (!strcmp(k, "--to")) to = v;
        else if (!strcmp(k, "--depth")) depth_s = v;
        else if (!strcmp(k, "--direction")) dir_s = v;
        else if (!strcmp(k, "--limit")) limit_s = v;
        else if (!strcmp(k, "--file-substr")) sub = v;
    }
    /* Positional forms fall back to file/line/col when flags are absent. */
    if (!file && npos > 0) file = positionals[0];
    if (!line_s) line_s = npos > 1 ? positionals[1] : NULL;
    if (!col_s) col_s = npos > 2 ? positionals[2] : NULL;

    int rc = 0;
    if (!strcmp(cmd, "functions")) {
        if (!file || !line_s) {
            fprintf(stderr, "functions requires --file and --line\n");
            rc = 2;
        } else {
            rc = cmd_functions(db, file, atoll(line_s));
        }
    } else if (!strcmp(cmd, "symbols")) {
        if (!file || !line_s || !col_s) {
            fprintf(stderr, "symbols requires --file --line --col\n");
            rc = 2;
        } else {
            rc = cmd_symbols(db, file, atoll(line_s), atoll(col_s));
        }
    } else if (!strcmp(cmd, "calls")) {
        /* `--file` (positional/via flag) is the call-edge path filter;
           `--file-substr` is kept as an alias. */
        rc = cmd_calls(db, from, to, file ? file : sub);
    } else if (!strcmp(cmd, "callgraph")) {
        if (!file || !line_s) {
            fprintf(stderr, "callgraph requires --file and --line\n");
            rc = 2;
        } else {
            trace_direction dir = parse_dir(dir_s ? dir_s : "down");
            long depth = depth_s ? parse_depth(depth_s) : 3;
            if (dir == (trace_direction)-1 || depth < 1) {
                fprintf(stderr, "callgraph: bad --direction or --depth "
                                "(direction: up|down, depth: >= 1)\n");
                rc = 2;
            } else {
                rc = cmd_callgraph(db, file, atoll(line_s), depth, dir);
            }
        }
    } else if (!strcmp(cmd, "dataflow")) {
        if (!file || !line_s || !col_s) {
            fprintf(stderr, "dataflow requires --file --line --col\n");
            rc = 2;
        } else {
            trace_direction dir = parse_dir(dir_s ? dir_s : "down");
            long depth = depth_s ? parse_depth(depth_s) : 3;
            if (dir == (trace_direction)-1 || depth < 1) {
                fprintf(stderr, "dataflow: bad --direction or --depth "
                                "(direction: up|down, depth: >= 1)\n");
                rc = 2;
            } else {
                rc = cmd_dataflow(db, file, atoll(line_s), atoll(col_s), depth, dir);
            }
        }
    } else if (!strcmp(cmd, "callchain") || !strcmp(cmd, "chains")) {
        const char *from_arg = from ? from : (npos > 0 ? positionals[0] : NULL);
        const char *to_arg = to ? to : (npos > 1 ? positionals[1] : NULL);
        const char *d_arg = depth_s ? depth_s : (npos > 2 ? positionals[2] : NULL);
        const char *dir_arg = dir_s ? dir_s : (npos > 3 ? positionals[3] : NULL);
        const char *lim_arg = limit_s ? limit_s : (npos > 4 ? positionals[4] : NULL);

        if (!from_arg || !to_arg) {
            fprintf(stderr, "callchain requires --from and --to (or positional FROM_ID TO_ID [DEPTH] [DIRECTION] [LIMIT])\n");
            rc = 2;
        } else {
            int64_t from_id = atoll(from_arg);
            int64_t to_id = atoll(to_arg);
            trace_direction dir = parse_dir(dir_arg ? dir_arg : "down");
            long depth = 5;
            if (d_arg) {
                char *end = NULL;
                long d = strtol(d_arg, &end, 10);
                if (end == d_arg || *end != '\0' || (from_id == to_id ? d < 0 : d < 1)) {
                    depth = -1;
                } else {
                    depth = d;
                }
            }
            long lim = lim_arg ? atol(lim_arg) : 0;
            size_t limit = lim > 0 ? (size_t)lim : 0;
            if (dir == (trace_direction)-1 || depth < 0) {
                fprintf(stderr, "callchain: bad --direction or --depth "
                                "(direction: up|down, depth: >= 1)\n");
                rc = 2;
            } else {
                rc = cmd_chains(db, from_id, to_id, depth, dir, limit);
            }
        }
    } else {
        fprintf(stderr, "unknown inspect command: %s\n", cmd);
        rc = 2;
    }

    if (flags) free(flags);
    if (positionals) free(positionals);
    trace_db_close(db);
    return rc;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: ctrace {analyze|inspect} ...\n");
        return 2;
    }
    if (!strcmp(argv[1], "analyze")) {
        return cmd_analyze(argc - 1, argv + 1);
    }
    if (!strcmp(argv[1], "inspect")) {
        return cmd_inspect(argc - 1, argv + 1);
    }
    fprintf(stderr, "unknown command: %s\n", argv[1]);
    return 2;
}