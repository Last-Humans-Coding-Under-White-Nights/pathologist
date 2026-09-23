mod common;

use common::*;
use rusqlite::Connection;
use trace_analysis::analyze;
use trace_parse::build_program;

#[test]
fn fixture_ignore_macro_removes_macro_call_sites_and_preserves_surrounding_lines() {
    let root = fixture("ignore_macro");

    // 1. Baseline build (no ignored macros)
    let baseline_opts = default_opts(&root);
    let baseline_prog = build_program(&root, &baseline_opts).expect("build baseline");
    let test_fn_id = only_function(&baseline_prog, "test_fn");

    let baseline_sites: Vec<_> = baseline_prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == test_fn_id)
        .collect();
    // before_call, log_call1, log_call2, log_call3, after_call
    assert_eq!(
        baseline_sites.len(),
        5,
        "baseline should have 5 call sites in test_fn"
    );

    let before_site = baseline_sites
        .iter()
        .find(|cs| cs.callee_name == "before_call")
        .expect("before_call site");
    let after_site = baseline_sites
        .iter()
        .find(|cs| cs.callee_name == "after_call")
        .expect("after_call site");

    assert_eq!(before_site.span.line, 14);
    assert_eq!(after_site.span.line, 16);

    // 2. Build with --ignore-macro LOG
    let ignore_opts = default_opts(&root).with_ignored_macro("LOG");
    let ignore_prog = build_program(&root, &ignore_opts).expect("build with ignore");
    let test_fn_id_ignored = only_function(&ignore_prog, "test_fn");

    let surviving_sites: Vec<_> = ignore_prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == test_fn_id_ignored)
        .collect();

    // Exactly 2 call sites: before_call and after_call (0 from LOG)
    assert_eq!(
        surviving_sites.len(),
        2,
        "enclosing function must have 0 sites from LOG, only 2 surviving sites"
    );

    let surviving_before = surviving_sites
        .iter()
        .find(|cs| cs.callee_name == "before_call")
        .expect("surviving before_call");
    let surviving_after = surviving_sites
        .iter()
        .find(|cs| cs.callee_name == "after_call")
        .expect("surviving after_call");

    // Line numbers of surviving sites must be unchanged
    assert_eq!(
        surviving_before.span.line, before_site.span.line,
        "before_call line number must remain unchanged"
    );
    assert_eq!(
        surviving_after.span.line, after_site.span.line,
        "after_call line number must remain unchanged"
    );

    // Check export to SQLite contains ignored_macros in options_json
    let (pag, analysis) = analyze(&ignore_prog);
    let db = export_program(&ignore_prog, &pag, &analysis);
    let conn = Connection::open(db.path()).expect("open db");
    let options_json: String = conn
        .query_row(
            "SELECT options_json FROM analysis_run ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();

    let parsed_opts: serde_json::Value = serde_json::from_str(&options_json).unwrap();
    assert_eq!(
        parsed_opts["ignored_macros"],
        serde_json::json!(["LOG"]),
        "options_json must record ignored_macros"
    );
}

#[test]
fn fixture_ignore_macro_supports_wildcard_pattern() {
    let root = fixture("ignore_macro");

    // Wildcard: LOG*
    let opts = default_opts(&root).with_ignored_macro("LOG*");
    let prog = build_program(&root, &opts).expect("build with wildcard ignore");
    let test_fn_id = only_function(&prog, "test_fn");

    let sites: Vec<_> = prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == test_fn_id)
        .collect();

    assert_eq!(
        sites.len(),
        2,
        "wildcard LOG* must match LOG and ignore its 3 call sites"
    );
    assert!(sites.iter().any(|cs| cs.callee_name == "before_call"));
    assert!(sites.iter().any(|cs| cs.callee_name == "after_call"));
}

#[test]
fn toml_model_noise_macros_ignore_calls_and_locals() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
extern void real_call(void);
extern void noisy_call1(void);
extern void noisy_call2(void);

#define MY_LOG(x) do { \
    int local_macro_var = 42; \
    noisy_call1(); \
    noisy_call2(); \
} while (0)

void worker(void) {
    real_call();
    MY_LOG("debug");
}
"#;
    std::fs::write(dir.path().join("main.c"), src).unwrap();

    let toml_src = r#"
[noise]
macros = ["MY_LOG"]
"#;
    let model_set = trace_analysis::FnModelSet::from_toml_str(toml_src).unwrap();
    assert_eq!(model_set.noise_macros(), &["MY_LOG"]);

    let opts = default_opts(dir.path()).with_ignored_macros(model_set.noise_macros().to_vec());
    let prog = build_program(dir.path(), &opts).unwrap();
    let worker_fn = only_function(&prog, "worker");

    let worker_sites: Vec<_> = prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == worker_fn)
        .collect();

    assert_eq!(worker_sites.len(), 1);
    assert_eq!(worker_sites[0].callee_name, "real_call");

    // Local variable inside macro should also not be in symbols
    let vars: Vec<_> = prog
        .symbols
        .variables
        .iter()
        .filter(|v| v.fn_id == Some(worker_fn))
        .collect();
    assert!(
        !vars.iter().any(|v| v.name == "local_macro_var"),
        "macro-internal local variable should not be lowered"
    );
}

#[test]
fn openharmony_logging_macros_pattern_match() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
extern void do_work(void);
extern void HILOG_IMPL(int level, const char* msg);

#define HILOG_INFO(msg) ((void)HILOG_IMPL(3, msg))
#define TAG_LOGI(tag, fmt, ...) do { \
    const char* abbr = "file.cpp"; \
    HILOG_IMPL(3, fmt); \
} while (0)

void service_handler(void) {
    do_work();
    HILOG_INFO("info message");
    TAG_LOGI("MyTag", "test format %d", 123);
}
"#;
    std::fs::write(dir.path().join("main.c"), src).unwrap();

    // Use OpenHarmony default patterns
    let patterns = vec!["HILOG_*".to_string(), "TAG_LOG*".to_string()];
    let opts = default_opts(dir.path()).with_ignored_macros(patterns);
    let prog = build_program(dir.path(), &opts).unwrap();
    let handler = only_function(&prog, "service_handler");

    let sites: Vec<_> = prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == handler)
        .collect();

    assert_eq!(
        sites.len(),
        1,
        "both HILOG_INFO and TAG_LOGI expansions should be ignored"
    );
    assert_eq!(sites[0].callee_name, "do_work");
    assert_eq!(sites[0].span.line, 12);
}

#[test]
fn comma_expression_mixed_origin_preserves_surrounding_calls() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
extern void noisy(void);
extern void real_call(void);
extern void after_call(void);

#define LOG() noisy()

void test_fn(void) {
    LOG(), real_call();
    after_call();
}
"#;
    std::fs::write(dir.path().join("main.c"), src).unwrap();

    let opts = default_opts(dir.path()).with_ignored_macro("LOG");
    let prog = build_program(dir.path(), &opts).unwrap();
    let test_fn = only_function(&prog, "test_fn");

    let sites: Vec<_> = prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == test_fn)
        .collect();

    assert_eq!(
        sites.len(),
        2,
        "LOG() in comma expression should be filtered, preserving real_call and after_call"
    );
    let callee_names: Vec<_> = sites.iter().map(|cs| cs.callee_name.as_str()).collect();
    assert_eq!(callee_names, vec!["real_call", "after_call"]);
}

#[test]
fn cli_define_macro_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
extern void noisy(void);
extern void real_call(void);

void test_fn(void) {
    LOG();
    real_call();
}
"#;
    std::fs::write(dir.path().join("main.c"), src).unwrap();

    let opts = default_opts(dir.path())
        .with_define("LOG", "noisy")
        .with_ignored_macro("LOG");
    let prog = build_program(dir.path(), &opts).unwrap();
    let test_fn = only_function(&prog, "test_fn");

    let sites: Vec<_> = prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == test_fn)
        .collect();

    assert_eq!(
        sites.len(),
        1,
        "CLI -D defined macro LOG should be suppressed, retaining real_call"
    );
    assert_eq!(sites[0].callee_name, "real_call");
}

#[test]
fn argument_forwarding_macros_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
extern void real_call(void);
extern void noisy1(void);
extern void noisy2(void);

#define FORWARD(x) x
#define NESTED(x) FORWARD(x)

void test_fn(void) {
    real_call();
    FORWARD(noisy1());
    NESTED(noisy2());
}
"#;
    std::fs::write(dir.path().join("main.c"), src).unwrap();

    let opts = default_opts(dir.path())
        .with_ignored_macro("FORWARD")
        .with_ignored_macro("NESTED");
    let prog = build_program(dir.path(), &opts).unwrap();
    let test_fn = only_function(&prog, "test_fn");

    let sites: Vec<_> = prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == test_fn)
        .collect();

    assert_eq!(
        sites.len(),
        1,
        "argument forwarding macros FORWARD and NESTED should suppress forwarded calls"
    );
    assert_eq!(sites[0].callee_name, "real_call");
}

#[test]
fn rhs_assignment_flow_suppressed_for_ignored_macro() {
    let dir = tempfile::tempdir().unwrap();
    let src = r#"
typedef void (*CB)(void);
void target(void) {}
CB get_handler(void) {
    return target;
}

#define LOG() get_handler()

void test_fn(CB *out_ptr) {
    CB p1 = 0;
    p1 = (CB)LOG();
    p1();

    CB p2 = 0;
    p2 = (LOG());
    p2();

    CB p3 = (CB)LOG();
    p3();

    *out_ptr = (CB)LOG();
}
"#;
    std::fs::write(dir.path().join("main.c"), src).unwrap();

    let baseline_opts = default_opts(dir.path());
    let baseline_prog = build_program(dir.path(), &baseline_opts).unwrap();
    let base_test_fn = only_function(&baseline_prog, "test_fn");
    let base_target = only_function(&baseline_prog, "target");
    let (_base_pag, base_analysis) = analyze(&baseline_prog);

    // In baseline, p1, p2, p3 calls resolve to target
    let base_p1_site = baseline_prog
        .symbols
        .call_sites
        .iter()
        .find(|cs| cs.caller == base_test_fn && cs.callee_name == "p1")
        .expect("p1 site");
    let base_p1_edges: Vec<_> = base_analysis
        .call_edges
        .iter()
        .filter(|e| e.call_site == base_p1_site.id && e.callee == base_target)
        .collect();
    assert!(
        !base_p1_edges.is_empty(),
        "baseline p1() should reach target callback"
    );

    let ignore_opts = default_opts(dir.path()).with_ignored_macro("LOG");
    let ignore_prog = build_program(dir.path(), &ignore_opts).unwrap();
    let test_fn = only_function(&ignore_prog, "test_fn");
    let target = only_function(&ignore_prog, "target");

    // 1. No CallReturn or Store constraint for get_handler or LOG
    let has_flow = ignore_prog.flow.iter().any(|fc| match fc {
        trace_ir::FlowConstraint::CallReturn { callee_name, .. } => callee_name == "get_handler",
        trace_ir::FlowConstraint::Store { dst: _, src } => ignore_prog
            .symbols
            .variable(*src)
            .name
            .starts_with("__ret_"),
        _ => false,
    });
    assert!(
        !has_flow,
        "ignored macro on RHS must not emit CallReturn or Store constraints"
    );

    // 2. No CallSite for get_handler in test_fn
    let sites: Vec<_> = ignore_prog
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.caller == test_fn)
        .collect();
    assert!(
        !sites.iter().any(|cs| cs.callee_name == "get_handler"),
        "get_handler call sites must be suppressed"
    );

    // 3. No temporary variables allocated for get_handler call in test_fn
    let vars: Vec<_> = ignore_prog
        .symbols
        .variables
        .iter()
        .filter(|v| v.fn_id == Some(test_fn))
        .collect();
    assert!(
        !vars.iter().any(|v| v.name.starts_with("__ret_")),
        "no __ret_ temporary variables should be allocated for ignored call on RHS"
    );

    // 4. Indirect calls p1(), p2(), p3() must NOT resolve to target
    let (_pag, analysis) = analyze(&ignore_prog);
    for name in ["p1", "p2", "p3"] {
        let p_site = sites
            .iter()
            .find(|cs| cs.callee_name == name)
            .unwrap_or_else(|| panic!("{name}() call site"));
        let edges_to_target: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|edge| edge.call_site == p_site.id && edge.callee == target)
            .collect();
        assert!(
            edges_to_target.is_empty(),
            "indirect call {name}() must not reach suppressed target callback"
        );
    }
}
