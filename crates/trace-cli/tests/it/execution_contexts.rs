//! Execution contexts (#202): where a thread, a task or an IPC request starts
//! running code, with the evidence that it may run more than once at a time.

use crate::common;

use common::*;
use trace_analysis::{
    analyze, AnalysisResult, ContextKind, ExecutionContext, MultiInstance, ResolutionKind,
};
use trace_ir::Program;

analyzed_fixture!(execution_contexts);

/// The fixture analysed with its `models.toml` on top of the built-ins.
fn with_models() -> &'static (Program, AnalysisResult) {
    static CACHE: std::sync::OnceLock<(Program, AnalysisResult)> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let root = fixture("execution_contexts");
        let program = trace_parse::build_program(&root, &default_opts(&root)).expect("build");
        let toml = std::fs::read_to_string(root.join("models.toml")).unwrap();
        let analysis = analyze_with_models(&program, &toml);
        (program, analysis)
    })
}

/// The one context whose entry function is `entry`.
fn context<'a>(
    program: &Program,
    analysis: &'a AnalysisResult,
    entry: &str,
) -> &'a ExecutionContext {
    let found: Vec<_> = analysis
        .execution_contexts
        .iter()
        .filter(|c| fn_name(program, c.entry) == entry)
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one context entered at `{entry}`: {found:?}"
    );
    found[0]
}

/// `(submitting function, modelled callee, invoked parameter)` of a context.
fn started_by(program: &Program, ctx: &ExecutionContext) -> (String, String, u32) {
    let start = ctx.start.as_ref().expect("a submitted context has a start");
    let site = program.symbols.call_site_by_id(start.call_site).unwrap();
    (
        fn_name(program, site.caller),
        fn_name(program, start.api),
        start.param,
    )
}

#[test]
fn pthread_create_starts_a_thread_context() {
    let (p, a) = execution_contexts();
    let ctx = context(p, a, "Worker");
    assert_eq!(ctx.kind, ContextKind::Thread);
    assert_eq!(
        started_by(p, ctx),
        ("StartPthread".into(), "pthread_create".into(), 2)
    );
    assert_eq!(ctx.multi_instance, MultiInstance::Unknown);
    assert!(!ctx.self_concurrent, "nothing suggests a second instance");
}

#[test]
fn std_thread_starts_a_thread_context() {
    let (p, a) = execution_contexts();
    let ctx = context(p, a, "Rotate");
    assert_eq!(ctx.kind, ContextKind::Thread);
    assert_eq!(
        started_by(p, ctx),
        ("StartThread".into(), "std::thread::thread".into(), 0)
    );
}

#[test]
fn a_start_site_inside_a_loop_is_multi_instance() {
    let (p, a) = execution_contexts();
    for entry in ["LoopWorker", "Tick"] {
        let ctx = context(p, a, entry);
        assert_eq!(ctx.multi_instance, MultiInstance::Loop, "{entry}");
        assert!(ctx.self_concurrent, "{entry}");
    }
}

#[test]
fn a_start_site_in_a_call_graph_cycle_is_multi_instance() {
    let (p, a) = execution_contexts();
    let ctx = context(p, a, "Recurse");
    assert_eq!(ctx.multi_instance, MultiInstance::Cycle);
    assert!(ctx.self_concurrent);
}

#[test]
fn a_cycle_through_start_edges_is_not_recursion() {
    let (p, a) = execution_contexts();
    // `Create` starts `Nest`, which calls `Create`: a cycle only through the
    // edge from the start site to the thread it starts.
    for entry in ["Nest", "Leaf"] {
        let ctx = context(p, a, entry);
        assert_eq!(started_by(p, ctx).0, "Create", "{entry}");
        assert_eq!(ctx.multi_instance, MultiInstance::Unknown, "{entry}");
        assert!(!ctx.self_concurrent, "{entry}");
    }
}

#[test]
fn a_start_under_a_multi_instance_parent_is_multi_instance() {
    let (p, a) = execution_contexts();
    // `SpawnChild` runs inside `LoopWorker`, started once per iteration;
    // `IpcChild` is started by an IPC handler, once per request.
    for entry in ["Child", "IpcChild"] {
        let ctx = context(p, a, entry);
        assert_eq!(ctx.multi_instance, MultiInstance::Parent, "{entry}");
        assert!(ctx.self_concurrent, "{entry}");
    }
}

#[test]
fn ipc_stub_handler_is_a_possibly_self_concurrent_context() {
    let (p, a) = execution_contexts();
    let ctx = context(p, a, "IFooStub::HandleGetInfo");
    assert_eq!(ctx.kind, ContextKind::IpcHandler);
    assert!(ctx.start.is_none(), "no source call site starts a request");
    assert!(ctx.self_concurrent, "the IPC worker pool may run it twice");
}

#[test]
fn ffrt_queue_submit_is_a_serial_task() {
    let (p, a) = execution_contexts();
    let ctx = context(p, a, "Serial");
    assert_eq!(ctx.kind, ContextKind::SerialTask);
    assert_eq!(started_by(p, ctx).1, "ffrt::queue::submit");
}

#[test]
fn a_user_model_states_its_context_kind_or_leaves_it_unknown() {
    let (p, base) = execution_contexts();
    assert!(
        !base
            .execution_contexts
            .iter()
            .any(|c| ["PoolJob", "Later"].contains(&fn_name(p, c.entry).as_str())),
        "no model, no context"
    );
    let (p, a) = with_models();
    assert_eq!(context(p, a, "PoolJob").kind, ContextKind::PoolTask);
    assert_eq!(context(p, a, "Later").kind, ContextKind::Unknown);
}

#[test]
fn contexts_leave_the_call_graph_unchanged() {
    let (p, a) = execution_contexts();
    let starts: Vec<_> = a
        .call_edges
        .iter()
        .filter(|e| fn_name(p, e.caller) == "StartPthread" && fn_name(p, e.callee) == "Worker")
        .collect();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].resolution, ResolutionKind::Indirect);
}

#[test]
fn contexts_are_deterministic() {
    let (p, first) = execution_contexts();
    for _ in 0..3 {
        assert_eq!(analyze(p).1.execution_contexts, first.execution_contexts);
    }
}

#[test]
fn start_sites_record_whether_they_are_lexically_in_a_loop() {
    let (p, _) = analyze_source(&[(
        "main.cpp",
        r#"
int init(); int cond(); void step(); void body(); void cond_call();
void every(); int *range(); void element(); void captured(); void invoke_lambda();
void after(); int test();
void probe(int c) {
    for (int i = init(); cond(); step()) { body(); }
    while (test()) { every(); }
    do { cond_call(); } while (c);
    for (int x : *range()) { element(); }
    for (;;) {
        auto f = [] { captured(); };
        f();
        invoke_lambda();
        break;
    }
    after();
}
"#,
    )]);
    let in_loop = |callee: &str| {
        let sites: Vec<_> = p
            .symbols
            .call_sites
            .iter()
            .filter(|cs| cs.callee_name == callee)
            .collect();
        assert_eq!(sites.len(), 1, "one call to `{callee}`: {sites:?}");
        sites[0].in_loop
    };
    for callee in [
        "cond",
        "step",
        "body",
        "test",
        "every",
        "cond_call",
        "element",
        "invoke_lambda",
    ] {
        assert!(in_loop(callee), "`{callee}` runs once per iteration");
    }
    for callee in ["init", "range", "after"] {
        assert!(!in_loop(callee), "`{callee}` runs once");
    }
    assert!(
        !in_loop("captured"),
        "a lambda's body is its own function, outside any loop of its own"
    );
}

#[test]
fn c_do_and_for_loops_mark_their_start_sites() {
    let (p, a) = analyze_source(&[(
        "main.c",
        r#"
void *Worker(void *arg) { return arg; }
void *Once(void *arg) { return arg; }
void *InInit(void *arg) { return arg; }
void *InCond(void *arg) { return arg; }
void *InBody(void *arg) { return arg; }
void Start(void *ctx, int n) {
    unsigned long tid;
    int i;
    pthread_create(&tid, 0, Once, ctx);
    do { pthread_create(&tid, 0, Worker, ctx); } while (--n);
    for (i = pthread_create(&tid, 0, InInit, ctx); i < n; i++) {
        pthread_create(&tid, 0, InBody, ctx);
    }
    for (; pthread_create(&tid, 0, InCond, ctx) != 0;) { }
}
"#,
    )]);
    for entry in ["Worker", "InCond", "InBody"] {
        assert_eq!(
            context(&p, &a, entry).multi_instance,
            MultiInstance::Loop,
            "{entry}"
        );
    }
    for entry in ["Once", "InInit"] {
        assert_eq!(
            context(&p, &a, entry).multi_instance,
            MultiInstance::Unknown,
            "{entry}: a C `for` initializer runs once"
        );
    }
}

/// A model may state that its callback is an IPC request handler; that
/// context is self-concurrent like a stub handler, and so is what it starts.
#[test]
fn a_modelled_ipc_handler_context_is_self_concurrent() {
    let dir = scratch(&[(
        "main.c",
        r#"
void *Child(void *arg) { return arg; }
void Handler(void) {
    unsigned long tid;
    pthread_create(&tid, 0, Child, 0);
}
void register_handler(void (*h)(void));
void Setup(void) { register_handler(Handler); }
"#,
    )]);
    let p = trace_parse::build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let a = analyze_with_models(
        &p,
        "[[model]]\nname = \"register_handler\"\n\
         effects = [{ kind = \"invoke\", param = 0, context = \"ipc_handler\" }]\n",
    );
    let handler = context(&p, &a, "Handler");
    assert_eq!(handler.kind, ContextKind::IpcHandler);
    assert_eq!(handler.multi_instance, MultiInstance::Unknown);
    assert!(handler.self_concurrent, "every IPC handler may run twice");
    let child = context(&p, &a, "Child");
    assert_eq!(child.multi_instance, MultiInstance::Parent);
    assert!(child.self_concurrent);
}

/// An IPC handler is self-concurrent on its kind alone, but the evidence
/// that its parent runs more than once is still recorded on it.
#[test]
fn a_modelled_ipc_handler_under_a_multi_instance_parent_keeps_the_evidence() {
    let dir = scratch(&[(
        "main.c",
        r#"
void Handler(void) {}
void register_handler(void (*h)(void));
void *LoopWorker(void *arg) {
    register_handler(Handler);
    return arg;
}
void Setup(void) {
    int i;
    for (i = 0; i < 3; i++) {
        unsigned long tid;
        pthread_create(&tid, 0, LoopWorker, 0);
    }
}
"#,
    )]);
    let p = trace_parse::build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let a = analyze_with_models(
        &p,
        "[[model]]\nname = \"register_handler\"\n\
         effects = [{ kind = \"invoke\", param = 0, context = \"ipc_handler\" }]\n",
    );
    assert_eq!(
        context(&p, &a, "LoopWorker").multi_instance,
        MultiInstance::Loop
    );
    let handler = context(&p, &a, "Handler");
    assert_eq!(handler.kind, ContextKind::IpcHandler);
    assert_eq!(handler.multi_instance, MultiInstance::Parent, "{handler:?}");
    assert!(handler.self_concurrent);
}

/// A call through a virtual member reaches an override another unit
/// declares through a site the merge adds; that site keeps the loop evidence
/// of the call.
#[test]
fn a_virtual_start_site_keeps_its_loop_evidence() {
    let dir = scratch(&[
        (
            "exec.h",
            "struct Executor { virtual void Post(void (*job)(void)); };\n",
        ),
        (
            "run.cpp",
            r#"#include "exec.h"
void Job() {}
void Run(Executor *e, int n) {
    for (int i = 0; i < n; i++) { e->Post(Job); }
}
"#,
        ),
        (
            "pool.cpp",
            r#"#include "exec.h"
struct Pool : Executor { void Post(void (*job)(void)) override; };
"#,
        ),
    ]);
    let p = trace_parse::build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let a = analyze_with_models(
        &p,
        "[[model]]\nname = \"Pool::Post\"\n\
         effects = [{ kind = \"invoke\", param = 0, context = \"pool_task\" }]\n",
    );
    let ctx = context(&p, &a, "Job");
    assert_eq!(started_by(&p, ctx).1, "Pool::Post");
    assert_eq!(ctx.kind, ContextKind::PoolTask);
    assert_eq!(ctx.multi_instance, MultiInstance::Loop);
}

/// With `--explore`, a start site that only a variant puts in a loop merges
/// into the base configuration's record, which keeps the loop evidence.
#[test]
fn a_loop_only_a_variant_compiles_marks_the_merged_start_site() {
    let dir = scratch(&[
        (
            "main.c",
            "void *Worker(void *arg) { return arg; }\n\
             void Start(void *ctx, int n)\n{\n    unsigned long tid;\n\
             #ifdef MANY\n    while (n--)\n#endif\n\
             \x20   pthread_create(&tid, 0, Worker, ctx);\n}\n",
        ),
        ("BUILD.gn", "defines = [ \"MANY\" ]\n"),
    ]);
    let base = trace_parse::build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let a = analyze(&base).1;
    assert_eq!(
        context(&base, &a, "Worker").multi_instance,
        MultiInstance::Unknown,
        "the base configuration has no loop"
    );
    let opts = default_opts(dir.path())
        .with_explore(true)
        .with_explore_budget(4);
    let explored = trace_parse::build_program(dir.path(), &opts).unwrap();
    assert_eq!(explored.variants_merged, 1);
    let a = analyze(&explored).1;
    assert_eq!(
        context(&explored, &a, "Worker").multi_instance,
        MultiInstance::Loop
    );
}

#[test]
fn execution_contexts_are_exported() {
    let root = fixture("execution_contexts");
    let db = cli_analyze(&root, &[]);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let rows = text_rows(
        &conn,
        "SELECT e.kind, f.name, s.callee_text, c.name, e.param_index, \
                e.multi_instance, e.self_concurrent \
         FROM execution_contexts e \
         JOIN functions f ON f.id = e.entry_fn_id \
         LEFT JOIN call_sites s ON s.id = e.call_site_id \
         LEFT JOIN functions c ON c.id = e.api_fn_id \
         ORDER BY f.name",
    );
    let row = |entry: &str| {
        rows.iter()
            .find(|r| r[1] == format!("Text({entry:?})"))
            .unwrap_or_else(|| panic!("no row for {entry}: {rows:?}"))
            .clone()
    };
    assert_eq!(
        row("Worker"),
        [
            "Text(\"thread\")",
            "Text(\"Worker\")",
            "Text(\"pthread_create\")",
            "Text(\"pthread_create\")",
            "Integer(2)",
            "Text(\"unknown\")",
            "Integer(0)",
        ]
    );
    assert_eq!(row("LoopWorker")[5], "Text(\"loop\")");
    assert_eq!(row("Recurse")[5], "Text(\"cycle\")");
    assert_eq!(row("Child")[5], "Text(\"parent\")");
    assert_eq!(row("Serial")[0], "Text(\"serial_task\")");
    assert_eq!(
        row("IFooStub::HandleGetInfo"),
        [
            "Text(\"ipc_handler\")",
            "Text(\"IFooStub::HandleGetInfo\")",
            "Null",
            "Null",
            "Null",
            "Text(\"unknown\")",
            "Integer(1)",
        ]
    );
}

#[test]
fn exported_contexts_do_not_depend_on_the_job_count() {
    let root = fixture("execution_contexts");
    let one = cli_analyze(&root, &["--jobs", "1"]);
    let eight = cli_analyze(&root, &["--jobs", "8"]);
    assert_eq!(analysis_rows(&one), analysis_rows(&eight));
}
