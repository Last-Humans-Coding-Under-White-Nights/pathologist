//! OpenHarmony task and thread primitives (#203): callback models for the
//! task, handler, pool, timer and work-queue APIs, qualified model matching,
//! virtual members a framework runs as entries, and the receiver a task is
//! queued on.

use crate::common;

use common::*;
use trace_analysis::{analyze, AnalysisResult, ContextKind, ExecutionContext, ResolutionKind};
use trace_ir::Program;

analyzed_fixture!(task_primitives);

/// A function of the fixture's `OHOS::CameraStandard` namespace.
fn cam(name: &str) -> String {
    format!("OHOS::CameraStandard::{name}")
}

/// The contexts whose entry function is `entry`.
fn contexts_of<'a>(
    program: &Program,
    analysis: &'a AnalysisResult,
    entry: &str,
) -> Vec<&'a ExecutionContext> {
    analysis
        .execution_contexts
        .iter()
        .filter(|c| fn_name(program, c.entry) == entry)
        .collect()
}

/// The one context whose entry function is `entry`.
fn context<'a>(
    program: &Program,
    analysis: &'a AnalysisResult,
    entry: &str,
) -> &'a ExecutionContext {
    let found = contexts_of(program, analysis, entry);
    assert_eq!(
        found.len(),
        1,
        "one context entered at `{entry}`: {found:?}"
    );
    found[0]
}

/// `(kind, modelled callee, invoked parameter, model)` of a submitted
/// context.
fn submitted(program: &Program, ctx: &ExecutionContext) -> (ContextKind, String, u32, String) {
    let start = ctx.start.as_ref().expect("a submitted context has a start");
    (
        ctx.kind,
        fn_name(program, start.api),
        start.param,
        ctx.model
            .clone()
            .expect("a modelled context names its model"),
    )
}

/// The name of the variable a submitted context's start site was called on.
fn receiver(program: &Program, ctx: &ExecutionContext) -> Option<String> {
    let start = ctx.start.as_ref().expect("a submitted context has a start");
    start
        .receiver
        .map(|v| program.symbols.variable(v).name.clone())
}

#[test]
fn ffrt_free_functions_start_pool_tasks() {
    let (p, a) = task_primitives();
    assert_eq!(
        submitted(p, context(p, a, &cam("FfrtTask"))),
        (
            ContextKind::PoolTask,
            "ffrt::submit".into(),
            0,
            "ffrt::submit".into()
        )
    );
    assert_eq!(
        submitted(p, context(p, a, &cam("FfrtHandleTask"))),
        (
            ContextKind::PoolTask,
            "ffrt::submit_h".into(),
            0,
            "ffrt::submit_h".into()
        )
    );
    let lambda = lambda_in(p, &cam("Session::Submit"));
    let ctx = context(p, a, &lambda.name);
    assert_eq!(submitted(p, ctx).1, "ffrt::submit");
    assert!(
        has_edge(
            p,
            a,
            &lambda.name,
            &cam("Session::Tick"),
            ResolutionKind::Direct
        ) || has_edge(
            p,
            a,
            &lambda.name,
            &cam("Session::Tick"),
            ResolutionKind::External
        ),
        "the submitted lambda is lowered with its body"
    );
}

#[test]
fn ffrt_queue_submit_h_starts_a_serial_task() {
    let (p, a) = task_primitives();
    for entry in ["QueueHandleTask", "QueuedOnParam"] {
        let (kind, api, param, model) = submitted(p, context(p, a, &cam(entry)));
        assert_eq!(kind, ContextKind::SerialTask, "{entry}");
        assert_eq!(api, "ffrt::queue::submit_h", "{entry}");
        assert_eq!(param, 0, "{entry}: the callback, past the queue's `this`");
        assert_eq!(model, "ffrt::queue::submit_h", "{entry}");
    }
}

#[test]
fn the_post_task_family_starts_serial_tasks_on_the_handler() {
    let (p, a) = task_primitives();
    for (entry, method) in [
        ("Posted", "PostTask"),
        ("Immediate", "PostImmediateTask"),
        ("HighPriority", "PostHighPriorityTask"),
        ("Idle", "PostIdleTask"),
        ("Sync", "PostSyncTask"),
        ("Timing", "PostTimingTask"),
        ("AtFront", "PostTaskAtFront"),
    ] {
        assert_eq!(
            submitted(p, context(p, a, &cam(entry))),
            (
                ContextKind::SerialTask,
                format!("AppExecFwk::EventHandler::{method}"),
                0,
                format!("OHOS::AppExecFwk::EventHandler::{method}"),
            ),
            "`AppExecFwk::EventHandler` written inside `OHOS` is the library's"
        );
    }
}

#[test]
fn a_subclass_inherits_the_model_of_its_base_member() {
    let (p, a) = task_primitives();
    let (kind, _, param, model) = submitted(p, context(p, a, &cam("OnSubclass")));
    assert_eq!(
        (kind, param, model.as_str()),
        (
            ContextKind::SerialTask,
            0,
            "OHOS::AppExecFwk::EventHandler::PostTask"
        )
    );
}

#[test]
fn a_thread_pool_written_in_a_nested_namespace_is_the_c_utils_pool() {
    let (p, a) = task_primitives();
    assert_eq!(
        submitted(p, context(p, a, &cam("Pooled"))),
        (
            ContextKind::PoolTask,
            "OHOS::CameraStandard::ThreadPool::AddTask".into(),
            0,
            "OHOS::ThreadPool::AddTask".into(),
        )
    );
}

#[test]
fn a_utils_timer_runs_its_callbacks_one_at_a_time() {
    let (p, a) = task_primitives();
    let (kind, _, param, model) = submitted(p, context(p, a, &cam("TimerTick")));
    assert_eq!(
        (kind, param, model.as_str()),
        (ContextKind::SerialTask, 0, "OHOS::Utils::Timer::Register")
    );
}

#[test]
fn std_async_and_std_jthread_start_threads() {
    let (p, a) = task_primitives();
    assert_eq!(
        submitted(p, context(p, a, &cam("AsyncJob"))),
        (
            ContextKind::Thread,
            "std::async".into(),
            1,
            "std::async".into()
        ),
        "after a launch policy"
    );
    assert_eq!(
        submitted(p, context(p, a, &cam("DeferredJob"))),
        (
            ContextKind::Thread,
            "std::async".into(),
            0,
            "std::async".into()
        ),
        "without one"
    );
    assert_eq!(
        submitted(p, context(p, a, &cam("JThreadJob"))),
        (
            ContextKind::Thread,
            "std::jthread::jthread".into(),
            0,
            "std::jthread::jthread".into(),
        )
    );
}

#[test]
fn hdf_work_items_and_osal_timers_are_contexts_with_their_argument() {
    let (p, a) = task_primitives();
    assert_eq!(
        submitted(p, context(p, a, "WorkFn")),
        (
            ContextKind::SerialTask,
            "HdfWorkInit".into(),
            1,
            "HdfWorkInit".into()
        )
    );
    assert_eq!(
        submitted(p, context(p, a, "DelayedFn")),
        (
            ContextKind::SerialTask,
            "HdfDelayedWorkInit".into(),
            1,
            "HdfDelayedWorkInit".into(),
        )
    );
    assert_eq!(
        submitted(p, context(p, a, "TimerFn")),
        (
            ContextKind::Thread,
            "OsalTimerCreate".into(),
            2,
            "OsalTimerCreate".into()
        )
    );
    assert!(
        has_edge(p, a, "WorkFn", "OnWork", ResolutionKind::Indirect),
        "the work item's argument reaches its parameter"
    );
}

/// Pins only which parameter of each C form is the callback and what kind of
/// context it starts. The fixture casts a function to the header pointer,
/// which real code does not do: a real header object (from
/// `ffrt_create_function_wrapper`, or one whose `exec` member the runtime
/// calls) is not looked into, so real C-form sites do not resolve yet (see
/// `docs/ANALYSIS.md`, "Documented imprecision").
#[test]
fn ffrt_c_forms_pin_their_callback_parameter() {
    let (p, a) = task_primitives();
    for (entry, api, param, kind) in [
        ("CTask", "ffrt_submit_base", 0, ContextKind::PoolTask),
        (
            "CHandleTask",
            "ffrt_submit_h_base",
            0,
            ContextKind::PoolTask,
        ),
        (
            "CQueueTask",
            "ffrt_queue_submit",
            1,
            ContextKind::SerialTask,
        ),
        (
            "CQueueHandleTask",
            "ffrt_queue_submit_h",
            1,
            ContextKind::SerialTask,
        ),
    ] {
        assert_eq!(
            submitted(p, context(p, a, entry)),
            (kind, api.into(), param, api.into()),
            "{entry}"
        );
    }
}

#[test]
fn overrides_of_framework_entry_members_are_contexts() {
    let (p, a) = task_primitives();
    for (entry, kind, model) in [
        ("Worker::Run", ContextKind::Thread, "OHOS::Thread::Run"),
        (
            "Handler::ProcessEvent",
            ContextKind::SerialTask,
            "OHOS::AppExecFwk::EventHandler::ProcessEvent",
        ),
        (
            "Recipient::OnRemoteDied",
            ContextKind::IpcHandler,
            "OHOS::IRemoteObject::DeathRecipient::OnRemoteDied",
        ),
    ] {
        let ctx = context(p, a, &cam(entry));
        assert_eq!(ctx.kind, kind, "{entry}");
        assert!(ctx.start.is_none(), "{entry}: no call site starts it");
        assert_eq!(ctx.model.as_deref(), Some(model), "{entry}");
        assert_eq!(
            ctx.self_concurrent,
            kind == ContextKind::IpcHandler,
            "{entry}: only an IPC handler is self-concurrent without evidence"
        );
    }
}

#[test]
fn an_overload_of_a_declared_entry_member_is_no_entry() {
    // With the framework's `virtual bool Run()` declared in the tree, the
    // subclass's `Run()` overrides it and `Run(int *)` only overloads the
    // name: one thread context, entered at the override, and the overload
    // keeps its ordinary caller.
    let (p, a) = analyze_source(&[(
        "main.cpp",
        "namespace OHOS { class Thread { public: virtual bool Run(); bool Start(); }; }\n\
         class Worker : public OHOS::Thread {\npublic:\n    bool Run() override { return true; }\n    \
         void Run(int *flag) { *flag = 1; }\n};\n\
         void Caller(Worker *w, int *x) { w->Run(x); }\n\
         int main() { Worker w; int v; Caller(&w, &v); return 0; }\n",
    )]);
    let found = contexts_of(&p, &a, "Worker::Run");
    assert_eq!(found.len(), 1, "{found:?}");
    let entry = p.symbols.function(found[0].entry);
    assert_eq!(
        entry.params.len(),
        usize::from(p.symbols.has_this_param(entry.id)),
        "the override takes no parameter: {entry:?}"
    );
}

#[test]
fn a_same_arity_overload_of_a_declared_entry_member_is_no_entry() {
    // The framework declares `ProcessEvent(const InnerEvent &)`. The
    // subclass's `ProcessEvent(int *)` takes one parameter too, but of
    // another type: an overload, and no entry. The override is the one
    // context, and the overload keeps its ordinary caller.
    let (p, a) = analyze_source(&[(
        "main.cpp",
        "namespace OHOS { namespace AppExecFwk {\nclass InnerEvent {};\n\
         class EventHandler { public: virtual void ProcessEvent(const InnerEvent &event); };\n} }\n\
         class Handler : public OHOS::AppExecFwk::EventHandler {\npublic:\n    \
         void ProcessEvent(const OHOS::AppExecFwk::InnerEvent &event) override {}\n    \
         void ProcessEvent(int *flag) { *flag = 1; }\n};\n\
         void Caller(Handler *h, int *x) { h->ProcessEvent(x); }\n\
         int main() { Handler h; int v; Caller(&h, &v); return 0; }\n",
    )]);
    let found = contexts_of(&p, &a, "Handler::ProcessEvent");
    assert_eq!(found.len(), 1, "{found:?}");
    let entry = p.symbols.function(found[0].entry);
    let params = p.symbols.explicit_params(entry).unwrap();
    let param = p.types.get(params.get(0).unwrap());
    assert!(
        matches!(
            param.desc.pointee(),
            Some(trace_ir::TypeDesc::Struct { .. })
        ),
        "the override takes the event: {entry:?} {param:?}"
    );
}

#[test]
fn a_pointer_overload_of_a_reference_entry_member_is_no_entry() {
    // `ProcessEvent(InnerEvent *)` beside the framework's
    // `ProcessEvent(const InnerEvent &)`: both lower to a pointer to the
    // class, but only the reference binding overrides. The pointer overload
    // keeps its ordinary caller.
    let (p, a) = analyze_source(&[(
        "main.cpp",
        "namespace OHOS { namespace AppExecFwk {\nclass InnerEvent {};\n\
         class EventHandler { public: virtual void ProcessEvent(const InnerEvent &event); };\n} }\n\
         class Handler : public OHOS::AppExecFwk::EventHandler {\npublic:\n    \
         void ProcessEvent(const OHOS::AppExecFwk::InnerEvent &event) override {}\n    \
         void ProcessEvent(OHOS::AppExecFwk::InnerEvent *event) {}\n};\n\
         void Caller(Handler *h, OHOS::AppExecFwk::InnerEvent *e) { h->ProcessEvent(e); }\n\
         int main() { Handler h; OHOS::AppExecFwk::InnerEvent e; Caller(&h, &e); return 0; }\n",
    )]);
    let found = contexts_of(&p, &a, "Handler::ProcessEvent");
    assert_eq!(found.len(), 1, "{found:?}");
    let entry = p.symbols.function(found[0].entry);
    assert_eq!(
        entry.reference_params,
        [true],
        "the override binds the event by reference: {entry:?}"
    );
}

#[test]
fn an_override_of_a_template_base_entry_member_is_an_entry() {
    // The framework's class is a template: its `Run(std::unique_ptr<T>)`
    // spells the parameter where the override spells the argument. The
    // override is still the entry: a template argument the index does not
    // know matches any, as it does for dispatch.
    let (p, a) = analyze_source(&[(
        "main.cpp",
        "namespace std { template <typename T> class unique_ptr {}; }\n\
         namespace OHOS { template <typename T> class Thread {\npublic:\n    \
         virtual bool Run(std::unique_ptr<T> job);\n    bool Start();\n}; }\n\
         struct Payload {};\n\
         class Worker : public OHOS::Thread<Payload> {\npublic:\n    \
         bool Run(std::unique_ptr<Payload> job) override { return true; }\n};\n\
         int main() { Worker w; w.Start(); return 0; }\n",
    )]);
    let found = contexts_of(&p, &a, "Worker::Run");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, ContextKind::Thread);
}

#[test]
fn classes_the_tree_declares_are_only_what_they_are_named() {
    let (p, a) = task_primitives();
    for entry in [
        "OHOS::HiviewDFX::Plugin::ProcessEvent",
        "OHOS::HiviewDFX::Loop::Run",
        "OHOS::HiviewDFX::NotATask",
        "OHOS::HiviewDFX::NotMail",
    ] {
        assert!(
            contexts_of(p, a, entry).is_empty(),
            "{entry}: {:?}",
            contexts_of(p, a, entry)
        );
    }
}

#[test]
fn a_named_receiver_is_recorded_with_the_start() {
    let (p, a) = task_primitives();
    let named = |entry: &str| receiver(p, context(p, a, &cam(entry)));
    assert_eq!(named("QueuedOnParam").as_deref(), Some("queue"));
    assert_eq!(named("PostedOnParam").as_deref(), Some("handler"));
    assert_eq!(named("Kicked").as_deref(), Some("this"));
    assert_eq!(named("Pooled").as_deref(), Some("pool"));
    assert_eq!(named("TimerTick").as_deref(), Some("timer"));
    assert_eq!(
        named("Posted"),
        None,
        "a field receiver (`handler_->PostTask`) is not a variable"
    );
    assert_eq!(named("FfrtTask"), None, "a free function has no receiver");
}

#[test]
fn member_call_sites_record_a_named_receiver() {
    let (p, _) = task_primitives();
    let site = |callee: &str, caller: &str| {
        let sites: Vec<_> = p
            .symbols
            .call_sites
            .iter()
            .filter(|cs| cs.callee_name.as_str() == callee && fn_name(p, cs.caller) == caller)
            .collect();
        assert_eq!(sites.len(), 1, "{callee} in {caller}: {sites:?}");
        sites[0]
            .receiver
            .map(|v| p.symbols.variable(v).name.clone())
    };
    assert_eq!(
        site(
            "OHOS::HiviewDFX::Mailbox::AddTask",
            "OHOS::HiviewDFX::UsePlugin"
        )
        .as_deref(),
        Some("mailbox")
    );
    assert_eq!(
        site("ffrt::submit_h", &cam("Session::Submit")),
        None,
        "a free function"
    );
}

#[test]
fn contexts_are_deterministic() {
    let (p, first) = task_primitives();
    for _ in 0..3 {
        assert_eq!(analyze(p).1.execution_contexts, first.execution_contexts);
    }
}

#[test]
fn exported_contexts_name_their_model_and_receiver() {
    let root = fixture("task_primitives");
    let db = cli_analyze(&root, &[]);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    let rows = text_rows(
        &conn,
        "SELECT f.name, e.kind, e.model, v.name \
         FROM execution_contexts e \
         JOIN functions f ON f.id = e.entry_fn_id \
         LEFT JOIN variables v ON v.id = e.receiver_var_id \
         ORDER BY f.name",
    );
    let row = |entry: &str| {
        rows.iter()
            .find(|r| r[0] == format!("Text({:?})", cam(entry)))
            .unwrap_or_else(|| panic!("no row for {entry}: {rows:?}"))
            .clone()
    };
    assert_eq!(
        row("QueuedOnParam")[1..],
        [
            "Text(\"serial_task\")",
            "Text(\"ffrt::queue::submit_h\")",
            "Text(\"queue\")",
        ]
    );
    assert_eq!(
        row("Recipient::OnRemoteDied")[1..],
        [
            "Text(\"ipc_handler\")",
            "Text(\"OHOS::IRemoteObject::DeathRecipient::OnRemoteDied\")",
            "Null",
        ]
    );
}

#[test]
fn exported_contexts_do_not_depend_on_the_job_count() {
    let root = fixture("task_primitives");
    let one = cli_analyze(&root, &["--jobs", "1"]);
    let eight = cli_analyze(&root, &["--jobs", "8"]);
    assert_eq!(analysis_rows(&one), analysis_rows(&eight));
}

/// A start site in a header body two units include names its receiver in
/// the merged program's variables, whichever unit's copy is kept.
#[test]
fn a_header_start_site_keeps_its_receiver_through_the_merge() {
    let header = "void Job();\n\
                  inline void Enqueue(ffrt::queue &queue) { queue.submit_h(Job); }\n";
    let (p, a) = analyze_source(&[
        ("queue.h", header),
        (
            "a.cpp",
            "#include \"queue.h\"\nvoid Job() {}\nvoid A(ffrt::queue &q) { Enqueue(q); }\n",
        ),
        (
            "b.cpp",
            "#include \"queue.h\"\nvoid B(ffrt::queue &q) { Enqueue(q); }\n",
        ),
    ]);
    let ctx = context(&p, &a, "Job");
    assert_eq!(submitted(&p, ctx).3, "ffrt::queue::submit_h");
    let var = ctx
        .start
        .as_ref()
        .unwrap()
        .receiver
        .expect("a named receiver");
    let var = p.symbols.variable(var);
    assert_eq!(var.name, "queue");
    assert_eq!(
        var.fn_id.map(|f| fn_name(&p, f)).as_deref(),
        Some("Enqueue")
    );
}

/// A start site in a unit merged after another unit that declares
/// variables: the merge renumbers the site's receiver, so the receiver must
/// be remapped with the unit's other variables or it names an unrelated one
/// of the first unit.
#[test]
fn a_start_site_in_a_later_unit_keeps_its_receiver_through_the_merge() {
    let (p, a) = analyze_source(&[
        (
            "a.cpp",
            "int g0, g1, g2, g3;\nvoid Z(int a, int b, int c) { g0 = a; g1 = b; g2 = c; }\n",
        ),
        (
            "b.cpp",
            "void Job() {}\nvoid B(ffrt::queue &queue) { queue.submit_h(Job); }\n",
        ),
    ]);
    let ctx = context(&p, &a, "Job");
    assert_eq!(submitted(&p, ctx).3, "ffrt::queue::submit_h");
    let var = ctx
        .start
        .as_ref()
        .unwrap()
        .receiver
        .expect("a named receiver");
    let var = p.symbols.variable(var);
    assert_eq!(var.name, "queue");
    assert_eq!(var.fn_id.map(|f| fn_name(&p, f)).as_deref(), Some("B"));
}

/// `program` analysed with the built-in models plus `toml`, with its PAG.
fn analyze_with_models_and_pag(
    program: &Program,
    toml: &str,
) -> (trace_analysis::Pag, AnalysisResult) {
    let models = trace_analysis::FnModelSet::from_toml_str(toml).expect("models");
    let opts = trace_analysis::AnalyzeOptions {
        models: std::sync::Arc::new(models),
        ..Default::default()
    };
    trace_analysis::analyze_with_options(program, opts)
}

/// One model is matched under one rule whatever its effect: a member model
/// with `invoke` and `return_heap` applies both to a call recorded under a
/// requalified class name and to one on a subclass.
#[test]
fn every_effect_of_a_member_model_applies_where_its_invoke_does() {
    let dir = scratch(&[(
        "main.cpp",
        "namespace OHOS { namespace AppExecFwk { } }\n\
         using namespace OHOS;\n\
         void Job() {}\n\
         void Other() {}\n\
         class Mine : public AppExecFwk::EventHandler {};\n\
         void Use(AppExecFwk::EventHandler &h) { int *r = h.PostTask(Job); }\n\
         void Sub(Mine &m) { int *s = m.PostTask(Other); }\n",
    )]);
    let program = trace_parse::build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let model = "OHOS::AppExecFwk::EventHandler::PostTask";
    let (pag, a) = analyze_with_models_and_pag(
        &program,
        &format!(
            "[[model]]\nname = \"{model}\"\neffects = [\n  \
             {{ kind = \"invoke\", param = 0, context = \"serial_task\" }},\n  \
             {{ kind = \"return_heap\" }},\n]\n"
        ),
    );
    for entry in ["Job", "Other"] {
        assert_eq!(
            submitted(&program, context(&program, &a, entry)).3,
            model,
            "{entry}: the callback"
        );
    }
    let heaps = pag
        .locations
        .iter()
        .filter(|l| l.desc == format!("{model}() storage"))
        .count();
    assert_eq!(heaps, 2, "both calls return the model's fresh storage");
}

/// The model a member is matched to is chosen per effect: a nearer base's
/// `entry` model of the same member does not hide a farther base's `invoke`
/// model.
#[test]
fn a_nearer_model_without_the_effect_does_not_hide_a_farther_one() {
    let dir = scratch(&[(
        "main.cpp",
        "struct Base { virtual void Post(void (*f)()); };\n\
         struct Mid : Base { void Post(void (*f)()) override; };\n\
         struct Leaf : Mid { void Post(void (*f)()) override {} };\n\
         void Job() {}\n\
         void Use(Leaf &leaf) { leaf.Post(Job); }\n",
    )]);
    let program = trace_parse::build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let a = analyze_with_models(
        &program,
        "[[model]]\nname = \"Mid::Post\"\neffects = [{ kind = \"entry\", context = \"thread\" }]\n\
         [[model]]\nname = \"Base::Post\"\n\
         effects = [{ kind = \"invoke\", param = 0, context = \"serial_task\" }]\n",
    );
    let (kind, _, param, model) = submitted(&program, context(&program, &a, "Job"));
    assert_eq!(
        (kind, param, model.as_str()),
        (ContextKind::SerialTask, 0, "Base::Post")
    );
    let entry = context(&program, &a, "Leaf::Post");
    assert_eq!(
        (entry.kind, entry.model.as_deref()),
        (ContextKind::Thread, Some("Mid::Post")),
        "the nearer model still makes the override an entry"
    );
}

/// Of member models on equally near bases, the first model name wins,
/// whatever order the class lists its bases in.
#[test]
fn equally_near_base_models_are_chosen_by_model_name_not_base_order() {
    for bases in ["Z, public A", "A, public Z"] {
        let dir = scratch(&[(
            "main.cpp",
            &format!(
                "struct A {{ virtual void Post(void (*f)()); }};\n\
                 struct Z {{ virtual void Post(void (*f)()); }};\n\
                 struct Leaf : public {bases} {{ void Post(void (*f)()) override {{}} }};\n\
                 void Job() {{}}\n\
                 void Use(Leaf &leaf) {{ leaf.Post(Job); }}\n"
            ),
        )]);
        let program =
            trace_parse::build_program(dir.path(), &default_opts(dir.path())).expect("build");
        let a = analyze_with_models(
            &program,
            "[[model]]\nname = \"Z::Post\"\n\
             effects = [{ kind = \"invoke\", param = 0, context = \"serial_task\" }]\n\
             [[model]]\nname = \"A::Post\"\n\
             effects = [{ kind = \"invoke\", param = 0, context = \"thread\" }]\n",
        );
        let (kind, _, param, model) = submitted(&program, context(&program, &a, "Job"));
        assert_eq!(
            (kind, param, model.as_str()),
            (ContextKind::Thread, 0, "A::Post"),
            "Leaf : {bases}"
        );
    }
}

/// A nearer base's model wins over a farther base's, even when the farther
/// one's model name sorts first.
#[test]
fn a_nearer_base_model_wins_over_a_first_named_farther_one() {
    let dir = scratch(&[(
        "main.cpp",
        "struct A { virtual void Post(void (*f)()); };\n\
         struct Mid : A { void Post(void (*f)()) override; };\n\
         struct Z { virtual void Post(void (*f)()); };\n\
         struct Leaf : Mid, Z { void Post(void (*f)()) override {} };\n\
         void Job() {}\n\
         void Use(Leaf &leaf) { leaf.Post(Job); }\n",
    )]);
    let program = trace_parse::build_program(dir.path(), &default_opts(dir.path())).expect("build");
    let a = analyze_with_models(
        &program,
        "[[model]]\nname = \"A::Post\"\n\
         effects = [{ kind = \"invoke\", param = 0, context = \"thread\" }]\n\
         [[model]]\nname = \"Z::Post\"\n\
         effects = [{ kind = \"invoke\", param = 0, context = \"serial_task\" }]\n",
    );
    let (kind, _, param, model) = submitted(&program, context(&program, &a, "Job"));
    assert_eq!(
        (kind, param, model.as_str()),
        (ContextKind::SerialTask, 0, "Z::Post")
    );
}

analyzed_fixture!(thread_launch);

#[test]
fn std_async_without_a_policy_starts_only_its_first_argument() {
    let (p, a) = thread_launch();
    assert_eq!(
        submitted(p, context(p, a, "AsyncWork")),
        (
            ContextKind::Thread,
            "std::async".into(),
            0,
            "std::async".into()
        )
    );
    assert!(
        contexts_of(p, a, "Unused").is_empty(),
        "`Unused` is an argument of `AsyncWork`, not a callable `std::async` runs"
    );
    assert!(!has_any_edge(p, a, "StartAsync", "Unused"));
    assert!(!has_any_edge(p, a, "AsyncWork", "Unused"));
}

#[test]
fn std_async_after_a_policy_starts_its_second_argument() {
    let (p, a) = thread_launch();
    for (start, work, hit) in [
        ("StartPolicy", "PolicyWork", "PolicyHit"),
        ("StartHeld", "HeldWork", "HeldHit"),
        ("StartRefHeld", "RefHeldWork", "RefHeldHit"),
        ("StartLocalRef", "LocalRefWork", "LocalRefHit"),
    ] {
        assert_eq!(
            submitted(p, context(p, a, work)),
            (
                ContextKind::Thread,
                "std::async".into(),
                1,
                "std::async".into()
            ),
            "{work}"
        );
        assert!(has_edge(p, a, start, work, ResolutionKind::Indirect));
        assert!(has_edge(p, a, work, hit, ResolutionKind::Indirect));
        assert!(contexts_of(p, a, hit).is_empty(), "{hit}");
    }
}
