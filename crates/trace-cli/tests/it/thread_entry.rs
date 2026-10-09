//! Thread entry points (`std::thread`, `pthread_create`): callback models
//! that forward arguments into the started function.

use crate::common;

use common::*;
use trace_analysis::{analyze, AnalysisResult, ResolutionKind};
use trace_ir::Program;
use trace_parse::build_program;

analyzed_fixture!(thread_entry);

fn indirect(program: &Program, analysis: &AnalysisResult, caller: &str, callee: &str) -> bool {
    has_edge(program, analysis, caller, callee, ResolutionKind::Indirect)
}

#[test]
fn pthread_create_reaches_the_start_routine_and_forwards_its_argument() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartPthread", "Worker"));
    assert!(
        indirect(p, a, "Worker", "OnEvent"),
        "`ctx` must flow into `Worker`'s `arg`"
    );
}

#[test]
fn pthread_create_passes_its_argument_to_the_start_routine() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartCall", "Call"));
    assert!(indirect(p, a, "Call", "OnDirectEvent"));
    assert!(must_not_have_edge(p, a, "Call", "OnEvent"));
}

#[test]
fn start_routine_reads_its_context_through_a_cast() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartDrain", "Drain"));
    assert!(
        indirect(p, a, "Drain", "OnEvent"),
        "`((Ctx *)arg)->h()` is a call through `Ctx::h`"
    );
}

#[test]
fn std_thread_reaches_the_callable() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartThread", "Rotate"));
    assert!(indirect(p, a, "StartNoArgs", "Idle"));
    assert!(
        indirect(p, a, "Service::Process", "Rotate"),
        "a temporary `std::thread(fn, args)` starts `fn` too"
    );
}

#[test]
fn std_thread_forwards_a_function_argument() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartRun", "Run"));
    assert!(indirect(p, a, "Run", "OnThreadEvent"));
}

#[test]
fn std_thread_forwards_the_rest_of_its_arguments_in_order() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartLoop", "Loop"));
    assert!(
        indirect(p, a, "Loop", "OnLoopEvent"),
        "the second forwarded argument must reach `Loop`'s `h`"
    );
}

#[test]
fn std_thread_starts_a_callable_held_in_a_variable() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartEntry", "Entry"));
    assert!(indirect(p, a, "Entry", "OnEntryEvent"));
}

#[test]
fn std_thread_runs_a_lambda_with_forwarded_arguments() {
    let (p, a) = thread_entry();
    let lambda = &lambda_in(p, "StartLambda").name;
    assert!(indirect(p, a, "StartLambda", lambda));
    assert!(indirect(p, a, lambda, "OnLambdaEvent"));
}

#[test]
fn thread_entry_edge_is_attributed_to_the_starting_call_site() {
    let (p, a) = thread_entry();
    let edge = a
        .call_edges
        .iter()
        .find(|e| fn_name(p, e.caller) == "StartPthread" && fn_name(p, e.callee) == "Worker")
        .expect("thread entry edge");
    let site = p.symbols.call_site_by_id(edge.call_site).unwrap();
    assert_eq!(site.callee_name, "pthread_create");
}

#[test]
fn forwarded_arguments_stay_between_their_own_start_site_and_callback() {
    let (p, a) = thread_entry();
    for (worker, foreign) in [
        ("Run", "OnLoopEvent"),
        ("Run", "OnLambdaEvent"),
        ("Loop", "OnThreadEvent"),
        ("Entry", "OnPoolEvent"),
        ("PoolTask", "OnEntryEvent"),
    ] {
        assert!(
            must_not_have_edge(p, a, worker, foreign),
            "{worker} must not reach {foreign}"
        );
    }
}

#[test]
fn user_model_invokes_a_callback_with_listed_arguments() {
    let (p, base) = thread_entry();
    assert!(must_not_have_edge(p, base, "StartPool", "PoolTask"));

    let toml = std::fs::read_to_string(fixture("thread_entry").join("models.toml")).unwrap();
    let a = &analyze_with_models(p, &toml);
    assert!(indirect(p, a, "StartPool", "PoolTask"));
    assert!(indirect(p, a, "PoolTask", "OnPoolEvent"));
}

#[test]
fn thread_models_can_be_disabled() {
    let (p, _) = thread_entry();
    let a = &analyze_with_models(
        p,
        "[[model]]\nname = \"pthread_create\"\neffects = []\n\
         [[model]]\nname = \"std::thread::thread\"\neffects = []\n",
    );
    assert!(must_not_have_edge(p, a, "StartPthread", "Worker"));
    assert!(must_not_have_edge(p, a, "StartThread", "Rotate"));
    assert!(
        must_not_have_edge(p, a, "Service::Process", "Rotate"),
        "the constructor's model covers a temporary too"
    );
}

#[test]
fn thread_entry_edges_are_deterministic() {
    let (p, first) = thread_entry();
    let edges = |a: &AnalysisResult| {
        a.call_edges
            .iter()
            .map(|e| (e.call_site, e.caller, e.callee, e.resolution))
            .collect::<Vec<_>>()
    };
    for _ in 0..3 {
        assert_eq!(edges(&analyze(p).1), edges(first));
    }
}

#[test]
fn static_member_entry_takes_the_arguments_past_its_this_slot() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartStatic", "Svc::Out"));
    assert!(indirect(p, a, "Svc::Out", "OnStaticEvent"));
    assert!(indirect(p, a, "StartStaticEntry", "Svc::Entry"));
    assert!(indirect(p, a, "Svc::Entry", "OnStaticEntryEvent"));
}

#[test]
fn static_member_entry_leaves_its_this_slot_unwired() {
    let (p, a) = thread_entry();
    let this = local_variable(p, "Svc::Out", "this");
    assert!(
        !a.arg_flow_edges.iter().any(|row| row.formal == this),
        "`Svc::Out` is static: no forwarded argument is its receiver"
    );
}

#[test]
fn member_entry_takes_its_receiver_first() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "Svc::StartRun", "Svc::Run"));
    assert!(indirect(p, a, "Svc::Run", "OnMemberEvent"));
    let h = local_variable(p, "Svc::Run", "h");
    let rows: Vec<_> = a
        .arg_flow_edges
        .iter()
        .filter(|row| row.formal == h)
        .collect();
    assert_eq!(rows.len(), 1, "the receiver is `this`, not `h`: {rows:?}");
    assert_eq!(rows[0].actual_fn, Some(only_function(p, "OnMemberEvent")));
}

#[test]
fn std_thread_does_not_start_a_callable_that_cannot_take_the_arguments() {
    let (p, a) = thread_entry();
    assert!(indirect(p, a, "StartPair", "Pair"));
    assert!(indirect(p, a, "Pair", "OnPairEvent"));
    assert!(must_not_have_edge(p, a, "StartPair", "Idle"));
}

#[test]
fn listed_arguments_may_leave_defaulted_parameters_out() {
    let (p, _) = thread_entry();
    let toml = std::fs::read_to_string(fixture("thread_entry").join("models.toml")).unwrap();
    let a = &analyze_with_models(p, &toml);
    assert!(indirect(p, a, "StartPoolDefault", "PoolDefault"));
    assert!(indirect(p, a, "PoolDefault", "OnDefaultEvent"));
}

#[test]
fn forwarded_arguments_are_arg_flow_rows() {
    let (p, a) = thread_entry();
    let formal = local_variable(p, "Run", "h");
    let handler = only_function(p, "OnThreadEvent");
    let rows: Vec<_> = a
        .arg_flow_edges
        .iter()
        .filter(|row| row.formal == formal)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].actual_fn, Some(handler));
    // The constructor's `this` is argument 0, `Run` 1, the handler 2.
    assert_eq!(rows[0].arg_index, 2);
    let site = p.symbols.call_site_by_id(rows[0].call_site).unwrap();
    assert_eq!(site.callee_name, "std::thread::thread");
}

#[test]
fn cast_receiver_spellings_resolve() {
    let (p, a) = thread_entry();
    for caller in ["ViaTypedef", "ViaConst", "ViaSameClass"] {
        assert!(indirect(p, a, caller, "OnEvent"), "{caller}");
    }
}

#[test]
fn cast_of_a_member_does_not_root_the_path_at_its_object() {
    let (p, _) = thread_entry();
    let node = local_variable(p, "ViaMember", "node");
    assert!(
        !p.flow
            .iter()
            .any(|c| matches!(c, trace_ir::FlowConstraint::Copy { src, .. } if *src == node)),
        "`(Ctx *)node->data` is not `node`"
    );
}

#[test]
fn c_start_routine_reads_its_context_through_a_cast() {
    let dir = scratch(&[(
        "main.c",
        r#"
typedef void (*Handler)(void);
struct Ctx { Handler h; };
void OnEvent(void) {}
typedef struct Ctx CtxAlias;
void *Worker(void *arg) { ((struct Ctx *)arg)->h(); return 0; }
void *AliasWorker(void *arg) { ((CtxAlias *)arg)->h(); return 0; }
void Start(struct Ctx *ctx) {
    unsigned long tid;
    ctx->h = OnEvent;
    pthread_create(&tid, 0, Worker, ctx);
}
"#,
    )]);
    let p = &build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let a = &analyze(p).1;
    assert!(indirect(p, a, "Start", "Worker"));
    assert!(indirect(p, a, "Worker", "OnEvent"));
    assert!(indirect(p, a, "AliasWorker", "OnEvent"));
}

#[test]
fn cast_pointer_levels_come_from_the_declarator() {
    let dir = scratch(&[(
        "main.cpp",
        r#"
typedef void (*Handler)(void);
template <class T> struct Box { T item; Handler h; };
void OnEvent(void) {}
void *Worker(void *arg) { static_cast<Box<int *> *>(arg)->h(); return nullptr; }
void Start(Box<int *> *box) {
    unsigned long tid;
    box->h = OnEvent;
    pthread_create(&tid, nullptr, Worker, box);
}
"#,
    )]);
    let p = &build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let a = &analyze(p).1;
    assert!(indirect(p, a, "Start", "Worker"));
    assert!(
        indirect(p, a, "Worker", "OnEvent"),
        "the `*` in `Box<int *>` is the template argument's, not the cast's"
    );
}

analyzed_fixture!(thread_launch);

#[test]
fn jthread_hands_a_stop_token_taking_callable_its_own_token_first() {
    let (p, a) = thread_launch();
    assert!(indirect(p, a, "StartStoppable", "Work"));
    assert!(
        indirect(p, a, "Work", "Hit"),
        "`Hit` is `cb`, past the token `std::jthread` supplies"
    );
    let token = local_variable(p, "Work", "token");
    assert!(
        !a.arg_flow_edges.iter().any(|row| row.formal == token),
        "no argument of the call is the token"
    );
    let cb = local_variable(p, "Work", "cb");
    let rows: Vec<_> = a
        .arg_flow_edges
        .iter()
        .filter(|row| row.formal == cb)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].actual_fn, Some(only_function(p, "Hit")));
}

#[test]
fn jthread_hands_its_token_to_a_callable_taking_it_by_reference() {
    let (p, a) = thread_launch();
    assert!(indirect(p, a, "StartRefStoppable", "RefWork"));
    assert!(
        indirect(p, a, "RefWork", "RefHit"),
        "`RefHit` is `cb`, past the token a `const std::stop_token &` takes"
    );
    let token = local_variable(p, "RefWork", "token");
    assert!(!a.arg_flow_edges.iter().any(|row| row.formal == token));
}

#[test]
fn jthread_hands_its_token_to_a_callable_taking_it_by_rvalue_reference() {
    let (p, a) = thread_launch();
    assert!(indirect(p, a, "StartRvalStoppable", "RvalWork"));
    assert!(
        indirect(p, a, "RvalWork", "RvalHit"),
        "`RvalHit` is `cb`, past the token a `std::stop_token &&` takes"
    );
    let token = local_variable(p, "RvalWork", "token");
    assert!(!a.arg_flow_edges.iter().any(|row| row.formal == token));
}

/// The reviewers' spellings: an unnamed reference token, a callback
/// parameter written as a function pointer, a by-reference policy and a
/// `std::async` whose future is discarded.
#[test]
fn reference_tokens_and_policies_as_reviewed() {
    let dir = scratch(&[(
        "main.cpp",
        "#include <future>\n\
         #include <stop_token>\n\
         #include <thread>\n\
         void Hit() {}\n\
         void Work(const std::stop_token &, void (*cb)()) { cb(); }\n\
         void Start() { std::jthread t(Work, Hit); }\n\
         void hit() {}\n\
         void asyncWork(void (*cb)()) { cb(); }\n\
         void start(const std::launch &policy) { std::async(policy, asyncWork, hit); }\n",
    )]);
    let p = &build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let a = &analyze(p).1;
    assert!(indirect(p, a, "Start", "Work"));
    assert!(indirect(p, a, "Work", "Hit"));
    assert!(indirect(p, a, "start", "asyncWork"));
    assert!(indirect(p, a, "asyncWork", "hit"));
}

#[test]
fn jthread_forwards_an_argument_to_a_pointer_to_a_token() {
    // A pointer is not a reference: `std::jthread` supplies no token to it.
    let (p, a) = thread_launch();
    assert!(indirect(p, a, "StartPtr", "PtrWork"));
    assert!(indirect(p, a, "PtrWork", "PtrHit"));
}

#[test]
fn jthread_forwards_a_token_the_caller_passes_when_it_can_supply_none() {
    // `TokenOnly(std::stop_token)` has no room for a supplied token ahead
    // of the caller's: `std::jthread` forwards the arguments as given.
    let (p, a) = thread_launch();
    assert!(indirect(p, a, "StartTokenOnly", "TokenOnly"));
    let token = local_variable(p, "TokenOnly", "token");
    let rows: Vec<_> = a
        .arg_flow_edges
        .iter()
        .filter(|row| row.formal == token)
        .collect();
    assert_eq!(rows.len(), 1, "the caller's token, forwarded: {rows:?}");
    assert_eq!(
        rows[0].actual_var,
        Some(local_variable(p, "StartTokenOnly", "token"))
    );
}

#[test]
fn jthread_forwards_as_std_thread_to_a_callable_without_a_stop_token() {
    let (p, a) = thread_launch();
    assert!(indirect(p, a, "StartPlain", "Plain"));
    assert!(indirect(p, a, "Plain", "PlainHit"));
}
