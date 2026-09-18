//! Callback models use the same qualified-name fallback as other models.

mod common;

use common::*;
use trace_analysis::{analyze_with_options, AnalyzeOptions, FnModelSet, ResolutionKind};
use trace_parse::build_program;

#[test]
fn callback_model_bare_name_matches_namespaced_and_member_callees() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.cpp"),
        r#"
void callback() {}
namespace api {
void dispatch(void (*)());
struct Queue { void dispatch(void (*)()); };
}
namespace disabled { void dispatch(void (*)()); }
void via_free() { api::dispatch(callback); }
void via_member(api::Queue& queue) { queue.dispatch(callback); }
void via_disabled() { disabled::dispatch(callback); }
"#,
    )
    .unwrap();
    let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let models = FnModelSet::from_toml_str(
        r#"
[[model]]
name = "dispatch"
effects = [{ kind = "invoke", param = 0 }]
[[model]]
name = "disabled::dispatch"
effects = []
"#,
    )
    .unwrap();
    let (_, analysis) = analyze_with_options(
        &program,
        AnalyzeOptions {
            models: std::sync::Arc::new(models),
            ..Default::default()
        },
    );
    for caller in ["via_free", "via_member"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "callback",
                ResolutionKind::Indirect
            ),
            "the bare dispatch model must reach the callback from {caller}"
        );
    }
    assert!(
        must_not_have_edge(&program, &analysis, "via_disabled", "callback"),
        "an exact empty model must override the bare callback model"
    );
}
