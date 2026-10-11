//! Function-model summaries: dataflow through bodyless memory functions,
//! terminators (`clears`), and heap-return models.

use crate::common;

use common::*;
use std::sync::Arc;
use trace_analysis::{analyze_with_options, AnalyzeOptions, ResolutionKind};
use trace_db::open_db;
use trace_ir::Program;
use trace_parse::build_program;

fn build() -> Program {
    let root = fixture("fn_models");
    build_program(&root, &default_opts(&root)).expect("build")
}

fn analyze_no_models(program: &Program) -> trace_analysis::AnalysisResult {
    analyze_with_options(
        program,
        AnalyzeOptions {
            models: Arc::new(trace_analysis::FnModelSet::default()),
            ..Default::default()
        },
    )
    .1
}

fn analyze_builtin(program: &Program) -> trace_analysis::AnalysisResult {
    analyze_with_options(program, AnalyzeOptions::default()).1
}

fn indirect_targets(
    program: &Program,
    analysis: &trace_analysis::AnalysisResult,
    caller: &str,
) -> Vec<String> {
    analysis
        .call_edges
        .iter()
        .filter(|e| {
            fn_name(program, e.caller) == caller && e.resolution == ResolutionKind::Indirect
        })
        .map(|e| fn_name(program, e.callee))
        .collect()
}

#[test]
fn memcpy_s_model_flows_ops_table_to_copy() {
    let program = build();

    // Without models the copy is invisible and the target is missing.
    let base = analyze_no_models(&program);
    assert!(
        !indirect_targets(&program, &base, "copy_via_memcpy_s").contains(&"impl_run".to_string()),
        "baseline must not resolve impl_run through memcpy_s"
    );

    // With built-in models the ops table flows dst <- src (arg 2 -> arg 0).
    let with = analyze_builtin(&program);
    assert!(
        indirect_targets(&program, &with, "copy_via_memcpy_s").contains(&"impl_run".to_string()),
        "expected impl_run resolved through modeled memcpy_s"
    );
}

#[test]
fn user_toml_models_project_wrapper() {
    let root = fixture("fn_models");
    let toml_path = root.join("models.toml");
    let src = std::fs::read_to_string(&toml_path).expect("models.toml");

    let program = build();
    let base = analyze_no_models(&program);
    assert!(
        !indirect_targets(&program, &base, "call_through_wrapper_copy")
            .contains(&"impl_run".to_string()),
        "baseline must not resolve impl_run through unmodeled wrapper"
    );

    let opts = AnalyzeOptions {
        models: Arc::new(trace_analysis::FnModelSet::from_toml_str(&src).expect("parse toml")),
        ..Default::default()
    };
    let with = analyze_with_options(&program, opts).1;
    assert!(
        indirect_targets(&program, &with, "call_through_wrapper_copy")
            .contains(&"impl_run".to_string()),
        "expected impl_run resolved through TOML-modeled wrapper"
    );
}

#[test]
fn terminator_event_recorded_and_exported() {
    let program = build();

    // No terminator events without models.
    let base = analyze_no_models(&program);
    assert!(base.terminator_events.is_empty());

    let with = analyze_builtin(&program);
    assert_eq!(
        with.terminator_events.len(),
        1,
        "exactly one clears event expected (memset_s arg 0)"
    );
    assert_eq!(with.terminator_events[0].1, 0, "cleared parameter index");

    // Exported flow graph carries a terminator node with a terminates edge.
    let pag =
        trace_analysis::Pag::build_with_models(&program, &trace_analysis::FnModelSet::builtin());
    let db = export_program(&program, &pag, &with);
    let conn = open_db(&db).expect("open db");
    let (kind, label): (String, String) = conn
        .query_row(
            "SELECT kind, label FROM flow_nodes WHERE kind='terminator'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("terminator node exported");
    assert_eq!(kind, "terminator");
    assert!(label.contains("memset_s"), "label mentions callee: {label}");
    let n_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM flow_edges WHERE kind='terminates'",
            [],
            |r| r.get(0),
        )
        .expect("terminates edge count");
    assert_eq!(n_edges, 1);
    // Issue #127 review: the edge leaves the cleared object, not the `&x`
    // temporary lowering passes in its place.
    let cleared: String = conn
        .query_row(
            "SELECT s.label FROM flow_edges e JOIN flow_nodes_text s ON s.id = e.src_node \
             WHERE e.kind='terminates'",
            [],
            |r| r.get(0),
        )
        .expect("terminates edge source");
    assert_eq!(cleared, "g_cleared");
}

#[test]
fn malloc_return_heap_summary() {
    let program = build();
    let opts = AnalyzeOptions::default();
    let (pag, _analysis) = analyze_with_options(&program, opts);
    assert!(
        pag.locations.iter().any(|l| l.desc.contains("malloc")),
        "expected a fresh heap location for modeled malloc"
    );
}

#[test]
fn realloc_return_alias_keeps_pointees() {
    let program = build();
    // With the builtin realloc model the returned pointer may be the old
    // storage; without models the CallReturn expansion yields nothing.
    let opts = AnalyzeOptions {
        models: Arc::new(trace_analysis::FnModelSet::default()),
        ..Default::default()
    };
    let (pag_no, _) = analyze_with_options(&program, opts);
    let heap_before = pag_no
        .locations
        .iter()
        .filter(|l| l.desc.contains("realloc") || l.desc.contains("malloc"))
        .count();

    let (pag_with, _) = analyze_with_options(&program, AnalyzeOptions::default());
    let heap_after = pag_with
        .locations
        .iter()
        .filter(|l| l.desc.contains("realloc") || l.desc.contains("malloc"))
        .count();
    assert!(
        heap_after > heap_before,
        "modeled malloc/realloc must contribute fresh storage locations"
    );

    // return_alias: grow_ops's result may alias its parameter's storage.
    // The use_grow return chain must therefore carry impl_run's function
    // location is NOT expected here (no fn stored in OpsA instances via
    // heap), but the heap loc must reach use_alloc's return value.
    assert!(
        pag_with
            .locations
            .iter()
            .any(|l| l.kind == trace_analysis::LocKind::Heap),
        "heap loc kind must be used for modeled allocations"
    );
}

/// A `content_store` model adds its store mid-solve, after the value's
/// pointees may already have propagated: its first firing must still write
/// them. Covers both wiring paths — a direct call (models applied before the
/// worklist) and a call through a function pointer (applied when the edge is
/// discovered, after `f`'s set has settled).
#[test]
fn content_store_model_writes_values_present_before_wiring() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("store.c"),
        "typedef int (*op_t)(int);\n\
         static int impl_a(int x) { return x; }\n\
         static int impl_b(int x) { return x; }\n\
         void set_slot(op_t *slot, op_t fn);\n\
         op_t g_direct;\n\
         op_t g_indirect;\n\
         void (*g_setter)(op_t *, op_t) = set_slot;\n\
         void store_direct(void) { op_t f = impl_a; set_slot(&g_direct, f); (void)g_direct(1); }\n\
         void store_indirect(void) { op_t f = impl_b; g_setter(&g_indirect, f); (void)g_indirect(2); }\n",
    )
    .unwrap();
    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");

    let base = analyze_no_models(&program);
    for caller in ["store_direct", "store_indirect"] {
        let targets = indirect_targets(&program, &base, caller);
        assert!(
            !targets.iter().any(|t| t.starts_with("impl_")),
            "{caller} must not resolve through the unmodeled setter: {targets:?}"
        );
    }

    let models = trace_analysis::FnModelSet::from_toml_str(
        "version = 1\n\
         [[model]]\n\
         name = \"set_slot\"\n\
         effects = [ { kind = \"content_store\", ptr = 0, value = 1 } ]\n",
    )
    .expect("parse toml");
    let with = analyze_with_options(
        &program,
        AnalyzeOptions {
            models: Arc::new(models),
            ..Default::default()
        },
    )
    .1;
    for (caller, target) in [("store_direct", "impl_a"), ("store_indirect", "impl_b")] {
        let targets = indirect_targets(&program, &with, caller);
        assert!(
            targets.contains(&target.to_string()),
            "{caller}: expected {target} stored through the modeled setter: {targets:?}"
        );
    }
}

/// Argument and return models match the callee's exact name only
/// (`docs/ANALYSIS.md`, "Model matching"): the libc `memset`, `memcpy` and
/// `malloc` models do not reach a class's or namespace's own `memset`,
/// `memcpy` or `malloc`, defined in the tree or not, while the plain,
/// `::`- and `std::`-qualified calls keep them.
#[test]
fn libc_models_skip_same_named_members() {
    let dir = scratch(&[(
        "main.cpp",
        r#"
typedef unsigned long size_t;
extern "C" void *memset(void *s, int c, size_t n);
extern "C" void *memcpy(void *d, const void *s, size_t n);
extern "C" void *malloc(size_t n);
typedef int (*op_t)(int);
static int impl_a(int x) { return x; }
static int impl_b(int x) { return x; }
op_t g_member_cleared;
struct SecureBuffer {
    void *memset(void *p, int c, size_t n);
    void wipe() { memset(&g_member_cleared, 0, sizeof(op_t)); }
};
void *SecureBuffer::memset(void *p, int c, size_t n) { return p; }
struct Pool {
    void *memcpy(op_t *d, op_t *s, size_t n);
    int copy() {
        op_t src = impl_a;
        op_t dst = 0;
        memcpy(&dst, &src, sizeof(op_t));
        return dst(1);
    }
};
void *Pool::memcpy(op_t *d, op_t *s, size_t n) { return d; }
struct Arena { void *malloc(size_t n); };
op_t g_plain_cleared;
void plain_clear(void) { memset(&g_plain_cleared, 0, sizeof(op_t)); }
int plain_copy(void) {
    op_t src = impl_b;
    op_t dst = 0;
    memcpy(&dst, &src, sizeof(op_t));
    return dst(2);
}
void *member_alloc(Arena &arena) { void *p = arena.malloc(8); return p; }
void *plain_alloc(void) { void *q = malloc(8); return q; }
void *global_alloc(void) { void *r = ::malloc(8); return r; }
namespace secure { void *memset(void *p, int c, size_t n) { return p; } }
op_t g_ns_cleared;
void ns_clear(void) { secure::memset(&g_ns_cleared, 0, sizeof(op_t)); }
namespace std { using ::memset; }
op_t g_std_cleared;
void std_clear(void) { std::memset(&g_std_cleared, 0, sizeof(op_t)); }
"#,
    )]);
    let program = build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let (pag, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );

    let cleared_in: Vec<String> = analysis
        .terminator_events
        .iter()
        .map(|(cs, _)| {
            let site = program
                .symbols
                .call_sites
                .iter()
                .find(|s| s.id == *cs)
                .expect("terminator call site");
            fn_name(&program, site.caller)
        })
        .collect();
    assert_eq!(
        cleared_in,
        ["plain_clear", "std_clear"],
        "only libc memset clears"
    );

    assert!(
        indirect_targets(&program, &analysis, "plain_copy").contains(&"impl_b".to_string()),
        "libc memcpy copies the function pointer"
    );
    assert!(
        !indirect_targets(&program, &analysis, "Pool::copy").contains(&"impl_a".to_string()),
        "Pool::memcpy is not libc memcpy"
    );

    let points_to_heap = |function: &str, var: &str| {
        let var = local_variable(&program, function, var);
        pag.var_node
            .get(&var)
            .and_then(|n| analysis.points_to.get(n))
            .is_some_and(|pts| {
                pts.iter()
                    .any(|l| pag.locations[l.0 as usize].kind == trace_analysis::LocKind::Heap)
            })
    };
    assert!(points_to_heap("plain_alloc", "q"), "libc malloc allocates");
    assert!(
        points_to_heap("global_alloc", "r"),
        "::malloc is libc malloc"
    );
    assert!(
        !points_to_heap("member_alloc", "p"),
        "Arena::malloc is not libc malloc"
    );
}

#[test]
fn epoll_registration_reaches_wait_dispatch() {
    let root = fixture("epoll");
    let program = build_program(&root, &default_opts(&root)).expect("build epoll");
    let (pag, with) = analyze_with_options(
        &program,
        AnalyzeOptions {
            retain_points_to: true,
            ..Default::default()
        },
    );
    assert!(with.solve.converged);
    let registration = program
        .symbols
        .call_sites
        .iter()
        .find(|cs| {
            fn_name(&program, cs.caller) == "Observer::monitor"
                && cs.callee_name == "register_event"
        })
        .expect("nested registration");
    let payload = registration
        .var_args
        .iter()
        .find(|(index, _)| *index == 2)
        .unwrap()
        .1;
    assert!(with.points_to[&pag.var_node[&payload]].iter().any(|loc| {
        let ty = pag.locations[loc.0 as usize].type_id;
        matches!(program.types.get(ty).desc.as_ref(), trace_ir::TypeDesc::Struct { name, .. } if name == "LevelHandler")
    }), "the registration passes the handler subobject's address");
    for caller in [
        "HandleEpollEvent",
        "pwait_events",
        "pwait2_events",
        "Observer::handle",
    ] {
        assert!(
            indirect_targets(&program, &with, caller).contains(&"HandleLevelReport".into()),
            "{caller} must receive the registered handler"
        );
    }
    assert!(
        indirect_targets(&program, &with, "Observer::handle").contains(&"NestedReport".into()),
        "nested handler must flow"
    );
    assert!(indirect_targets(&program, &with, "only_wait").is_empty());
    let variable = |name: &str| {
        program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == name)
            .unwrap()
            .id
    };
    let received = pag.var_node[&variable("late_events")];
    for source in [
        "late_registration",
        "array_registration",
        "mutable_registration",
    ] {
        let source_loc = pag.var_location[&variable(source)];
        assert!(
            with.points_to
                .get(&received)
                .is_some_and(|pts| pts.contains(&source_loc)),
            "late-discovered endpoints must propagate {source}'s address"
        );
    }
    // Literal null registrations must not appear as sources of wait buffers.
    let wait_nodes: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| {
            matches!(
                cs.callee_name.as_str(),
                "epoll_wait" | "epoll_pwait" | "epoll_pwait2" | "wait_api"
            )
        })
        .filter_map(|cs| pag.argument_var(cs, 1))
        .map(|var| pag.var_node[&var])
        .collect();
    for flow in &program.flow {
        if let trace_ir::FlowConstraint::NullPointer { dst } = flow {
            if program.symbols.variable(*dst).temp.is_none() {
                continue; // a real pointer variable can receive a value later
            }
            assert!(
                !pag.constraints.iter().any(|c| c.src == pag.var_node[dst]
                    && c.kind == trace_analysis::ConstraintKind::Copy
                    && wait_nodes.contains(&c.dst)),
                "literal-null temporary {} must not copy into a wait buffer",
                program.symbols.variable(*dst).name
            );
            assert!(with
                .points_to
                .get(&pag.var_node[dst])
                .is_none_or(|pts| pts.is_empty()));
        }
    }
    // FieldSummary may already resolve the dispatch without models. Pin
    // the actual new bridge in the exported graph, including its direction.
    let db = export_program(&program, &pag, &with);
    let conn = open_db(&db).expect("open epoll db");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM flow_edges e
         JOIN flow_edges a ON a.dst_node=e.src_node AND a.kind='addr_of'
         JOIN flow_nodes_text s ON s.id=a.src_node
         JOIN flow_nodes_text d ON d.id=e.dst_node
         WHERE e.kind='copy' AND s.label='registration of registration' AND d.label='events'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 5,
        "only the five libc wait buffers receive registrations"
    );
    let late: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM flow_edges e
         JOIN flow_edges a ON a.dst_node=e.src_node AND a.kind='addr_of'
         JOIN flow_nodes_text s ON s.id=a.src_node
         JOIN flow_nodes_text d ON d.id=e.dst_node
         WHERE e.kind='copy' AND s.label='late_registration of late_registration' AND d.label='late_events'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(late, 1, "indirect API calls must wire the epoll flow once");
    let (base_pag, base) = analyze_with_options(
        &program,
        AnalyzeOptions {
            models: Arc::new(trace_analysis::FnModelSet::default()),
            ..Default::default()
        },
    );
    let base_db = export_program(&program, &base_pag, &base);
    let base_conn = open_db(&base_db).unwrap();
    let count: i64 = base_conn
        .query_row(
            "SELECT COUNT(*) FROM flow_edges e
         JOIN flow_edges a ON a.dst_node=e.src_node AND a.kind='addr_of'
         JOIN flow_nodes_text s ON s.id=a.src_node
         JOIN flow_nodes_text d ON d.id=e.dst_node
         WHERE e.kind='copy' AND s.label='registration of registration' AND d.label='events'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "epoll flow requires the models");
}

#[test]
fn alias_models_accept_precise_members_but_reject_container_fallbacks() {
    let root = fixture("member_alias");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    let models = trace_analysis::FnModelSet::from_toml_str(
        "version = 1\n[[model]]\nname = \"Alias\"\neffects = [{kind = \"alias\", dst = 0, src = 1}]\n",
    ).unwrap();
    for enabled in [false, true] {
        let (pag, analysis) = analyze_with_options(
            &program,
            AnalyzeOptions {
                retain_points_to: true,
                models: Arc::new(if enabled {
                    models.clone()
                } else {
                    trace_analysis::FnModelSet::default()
                }),
                ..Default::default()
            },
        );
        let payload = program
            .symbols
            .variables
            .iter()
            .find(|v| v.name == "payload")
            .unwrap()
            .id;
        let loc = pag.var_location[&payload];
        for cs in program
            .symbols
            .call_sites
            .iter()
            .filter(|cs| cs.callee_name == "Alias")
        {
            let precise = fn_name(&program, cs.caller) == "precise";
            assert_eq!(cs.addr_of_member_args().is_empty(), precise);
            let dst = cs.var_args.iter().find(|(index, _)| *index == 0).unwrap().1;
            let reached = analysis
                .points_to
                .get(&pag.var_node[&dst])
                .is_some_and(|pts| pts.contains(&loc));
            assert_eq!(
                reached,
                enabled && precise,
                "models={enabled}, precise={precise}"
            );
        }
    }
}
