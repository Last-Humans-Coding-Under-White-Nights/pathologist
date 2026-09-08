mod common;

use common::*;
use rusqlite::Connection;
use trace_analysis::{analyze, ResolutionKind};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn test_explore_mode_disabled_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("my_config") {
            defines = [
                "FEATURE_ALPHA",
                "FEATURE_BETA",
            ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        void common_fn(void) {}

        #if defined(FEATURE_ALPHA)
        void alpha_fn(void) { common_fn(); }
        #endif

        #if defined(FEATURE_BETA)
        void beta_fn(void) { common_fn(); }
        #endif

        int main(void) {
            common_fn();
            return 0;
        }
        "#,
    )
    .unwrap();

    let default_opts = PreprocessOptions::new();
    assert!(!default_opts.explore);

    let program = build_program(root, &default_opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    let fn_names: Vec<_> = program
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(fn_names.contains(&"common_fn"));
    assert!(fn_names.contains(&"main"));
    assert!(
        !fn_names.contains(&"alpha_fn"),
        "alpha_fn should be excluded without --explore"
    );
    assert!(
        !fn_names.contains(&"beta_fn"),
        "beta_fn should be excluded without --explore"
    );

    assert!(has_edge(
        &program,
        &analysis,
        "main",
        "common_fn",
        ResolutionKind::Direct
    ));
    assert!(!has_any_edge(&program, &analysis, "alpha_fn", "common_fn"));

    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "explore"),
        "no explore diagnostics when exploration is disabled"
    );
}

#[test]
fn test_explore_mode_recovers_functions_and_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("my_config") {
            defines = [
                "FEATURE_ALPHA",
                "FEATURE_BETA",
            ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        void common_fn(void) {}

        #if defined(FEATURE_ALPHA)
        void alpha_fn(void) { common_fn(); }
        #endif

        #if defined(FEATURE_BETA)
        void beta_fn(void) { common_fn(); }
        #endif

        int main(void) {
            common_fn();
            return 0;
        }
        "#,
    )
    .unwrap();

    let explore_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    assert!(explore_opts.explore);
    assert_eq!(explore_opts.explore_budget, 4);

    let program = build_program(root, &explore_opts).expect("build program");
    assert!(program.explore);
    assert_eq!(program.explore_budget, 4);

    let (_pag, analysis) = analyze(&program);

    let fn_names: Vec<_> = program
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(fn_names.contains(&"common_fn"));
    assert!(fn_names.contains(&"main"));
    assert!(
        fn_names.contains(&"alpha_fn"),
        "alpha_fn should be recovered by explore mode"
    );
    assert!(
        fn_names.contains(&"beta_fn"),
        "beta_fn should be recovered by explore mode"
    );

    assert!(has_edge(
        &program,
        &analysis,
        "main",
        "common_fn",
        ResolutionKind::Direct
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "alpha_fn",
        "common_fn",
        ResolutionKind::Direct
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "beta_fn",
        "common_fn",
        ResolutionKind::Direct
    ));
}

#[test]
fn test_explore_mode_semantic_values_and_merged_function_body() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        source_set("test") {
            defines = [
                "BACKEND=1",
                "BACKEND=2",
            ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        void backend1_worker(void) {}
        void backend2_worker(void) {}

        void run_pipeline(void) {
        #if BACKEND == 1
            backend1_worker();
        #elif BACKEND == 2
            backend2_worker();
        #endif
        }

        int main(void) {
            run_pipeline();
            return 0;
        }
        "#,
    )
    .unwrap();

    let explore_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let program = build_program(root, &explore_opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    let fn_names: Vec<_> = program
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(fn_names.contains(&"backend1_worker"));
    assert!(fn_names.contains(&"backend2_worker"));
    assert!(fn_names.contains(&"run_pipeline"));

    // Both calls from run_pipeline should be preserved across variants!
    assert!(has_edge(
        &program,
        &analysis,
        "run_pipeline",
        "backend1_worker",
        ResolutionKind::Direct
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "run_pipeline",
        "backend2_worker",
        ResolutionKind::Direct
    ));
}

#[test]
fn test_explore_mode_struct_layout_union_and_gep_field_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("features") {
            defines = [
                "ENABLE_FOO",
                "ENABLE_BAR",
            ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        typedef void (*callback_fn)(void);

        struct DriverOps {
        #if defined(ENABLE_FOO)
            int foo_data;
            callback_fn foo_cb;
        #endif
        #if defined(ENABLE_BAR)
            int bar_data;
            callback_fn bar_cb;
        #endif
            int common_data;
        };

        void target_foo(void) {}
        void target_bar(void) {}

        #if defined(ENABLE_FOO)
        void invoke_foo(struct DriverOps *ops) {
            ops->foo_cb = target_foo;
            ops->foo_cb();
        }
        #endif

        #if defined(ENABLE_BAR)
        void invoke_bar(struct DriverOps *ops) {
            ops->bar_cb = target_bar;
            ops->bar_cb();
        }
        #endif

        int main(void) {
            return 0;
        }
        "#,
    )
    .unwrap();

    let explore_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let program = build_program(root, &explore_opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    // Verify both invoke functions are present
    let fn_names: Vec<_> = program
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(fn_names.contains(&"invoke_foo"));
    assert!(fn_names.contains(&"invoke_bar"));

    // Verify unioned struct DriverOps has fields from both variants
    let struct_id = program
        .types
        .type_id_by_tag("DriverOps", trace_ir::TypeKind::Struct)
        .expect("DriverOps struct");
    let struct_ops = program.types.get(struct_id);
    let field_names: Vec<_> = struct_ops
        .layout
        .fields
        .values()
        .map(|fl| fl.name.as_str())
        .collect();
    assert!(field_names.contains(&"foo_cb"));
    assert!(field_names.contains(&"bar_cb"));
    assert!(field_names.contains(&"common_data"));

    // Verify indirect calls through ops->foo_cb and ops->bar_cb resolve
    assert!(
        has_edge(
            &program,
            &analysis,
            "invoke_foo",
            "target_foo",
            ResolutionKind::Indirect
        ),
        "expected invoke_foo -> target_foo via indirect call"
    );
    assert!(
        has_edge(
            &program,
            &analysis,
            "invoke_bar",
            "target_bar",
            ResolutionKind::Indirect
        ),
        "expected invoke_bar -> target_bar via indirect call"
    );
}

#[test]
fn test_explore_budget_truncation_honesty_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        defines = [
            "ALT1",
            "ALT2",
            "ALT3",
            "ALT4",
            "ALT5",
        ]
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        #if defined(ALT1)
        void f1(void) {}
        #elif defined(ALT2)
        void f2(void) {}
        #elif defined(ALT3)
        void f3(void) {}
        #elif defined(ALT4)
        void f4(void) {}
        #elif defined(ALT5)
        void f5(void) {}
        #endif

        int main(void) { return 0; }
        "#,
    )
    .unwrap();

    // Set budget to 2: 1 variant activates ALT1, budget is reached, remaining arms are truncated!
    let explore_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(2);
    let program = build_program(root, &explore_opts).expect("build program");

    let explore_diags: Vec<_> = program
        .diagnostics
        .iter()
        .filter(|d| d.stage == "explore")
        .collect();

    assert!(
        !explore_diags.is_empty(),
        "expected explore diagnostic diagnosing budget truncation: {:?}",
        program.diagnostics
    );

    let budget_diag = explore_diags
        .iter()
        .find(|d| d.message.contains("budget (2)"))
        .expect("diagnostic mentioning budget (2)");

    assert_eq!(budget_diag.stage, "explore");
    assert_eq!(budget_diag.severity, trace_ir::DiagnosticSeverity::Warning);
}

#[test]
fn test_explore_sqlite_export_options() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        defines = [ "FEATURE_X" ]
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        #if defined(FEATURE_X)
        void x(void) {}
        #endif
        int main(void) { return 0; }
        "#,
    )
    .unwrap();

    let explore_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(3);
    let program = build_program(root, &explore_opts).expect("build program");
    let (pag, analysis) = analyze(&program);

    let db = export_program(&program, &pag, &analysis);
    let conn = Connection::open(db.path()).expect("open db");

    let options_json: String = conn
        .query_row("SELECT options_json FROM analysis_run LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("query analysis_run");

    let parsed: serde_json::Value = serde_json::from_str(&options_json).expect("valid json");
    assert_eq!(parsed["explore"], true);
    assert_eq!(parsed["explore_budget"], 3);
}

#[test]
fn exploration_preserves_conditions_when_grouping_defines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("BUILD.gn"), r#"defines = [ "A", "B" ]"#).unwrap();
    std::fs::write(
        dir.path().join("main.c"),
        r#"
#if defined(A) && !defined(B)
void only_a(void) {}
#endif
#ifdef B
void with_b(void) {}
#endif
"#,
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new().with_explore(true)).unwrap();
    for name in ["only_a", "with_b"] {
        assert!(
            program
                .symbols
                .functions
                .iter()
                .any(|f| f.name == name && f.is_defined),
            "missing {name}"
        );
    }
}

#[test]
fn exploration_does_not_spend_budget_on_shadowed_elif() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("BUILD.gn"), r#"defines = [ "A", "B" ]"#).unwrap();
    std::fs::write(
        dir.path().join("main.c"),
        r#"
#if 1
#elif defined(A)
void unreachable(void) {}
#endif
#ifdef B
void reachable(void) {}
#endif
"#,
    )
    .unwrap();
    let program = build_program(
        dir.path(),
        &PreprocessOptions::new()
            .with_explore(true)
            .with_explore_budget(1),
    )
    .unwrap();
    assert!(program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "reachable" && f.is_defined));
    assert!(!program.diagnostics.iter().any(|d| d.stage == "explore"));
}

#[test]
fn test_explore_preserves_file_scope_variable_flow() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        defines = [ "ALT" ]
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        typedef void (*fn_t)(void);
        static fn_t g_cb;
        void target_cb(void) {}

        void init_base(void) {
            g_cb = target_cb;
        }

        #ifdef ALT
        void call_in_variant(void) {
            g_cb();
        }
        #endif

        int main(void) {
            init_base();
            return 0;
        }
        "#,
    )
    .unwrap();

    let explore_opts = PreprocessOptions::new().with_explore(true);
    let program = build_program(root, &explore_opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(program.variants_merged > 0);
    assert!(
        has_edge(
            &program,
            &analysis,
            "call_in_variant",
            "target_cb",
            ResolutionKind::Indirect
        ),
        "expected call_in_variant -> target_cb via indirect call through shared file-scope g_cb"
    );
}

/// A caller outside every conditional contributes the *same* facts in the base
/// configuration and in each variant. Its call sites must therefore merge, not
/// accumulate one duplicate record per variant (#59 review).
///
/// The call has to reference locals to be a real test: variant locals only pair
/// with the base's through the merge's local index, and a call with no operands
/// compares equal regardless of whether that index works.
#[test]
fn test_explore_merges_configuration_independent_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("my_config") {
            defines = [ "FEATURE_ALPHA" ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        int compute(int v) { return v; }

        #ifdef FEATURE_ALPHA
        void alpha_fn(void) {}
        #endif

        int main(void) {
            int x = 5;
            int y = compute(x);
            return y;
        }
        "#,
    )
    .unwrap();

    let explore_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let program = build_program(root, &explore_opts).expect("build program");

    assert!(
        program.variants_merged > 0,
        "the fixture must actually explore a variant for this test to mean anything"
    );

    let compute_sites: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.callee_name == "compute")
        .collect();
    assert_eq!(
        compute_sites.len(),
        1,
        "a configuration-independent call must merge across variants, got {} records at {:?}",
        compute_sites.len(),
        compute_sites
            .iter()
            .map(|cs| (cs.span.line, cs.span.col))
            .collect::<Vec<_>>()
    );

    // The same holds for the locals the call binds: `x` and `y` exist once.
    for name in ["x", "y"] {
        let count = program
            .symbols
            .variables
            .iter()
            .filter(|v| v.name == name && v.fn_id.is_some())
            .count();
        assert_eq!(count, 1, "local `{name}` duplicated across variants");
    }
}

/// The `#ifdef X / #else` pair spells one function's two implementations on
/// *different lines*. Merging the variant must not evict the base definition's
/// span or its facts: a may-analysis only ever adds (#59 review).
#[test]
fn test_explore_keeps_base_definition_of_alternative_implementation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("my_config") {
            defines = [ "USE_HW" ]
        }
        "#,
    )
    .unwrap();

    // Written without leading indentation so the line numbers are exact.
    let src = "typedef int (*cb_t)(int);\n\
               int real_target(int v) { return v; }\n\
               #ifdef USE_HW\n\
               void install(cb_t cb) { cb(1); }\n\
               #else\n\
               void install(cb_t cb) { cb(2); }\n\
               #endif\n\
               void top(void) { install(real_target); }\n";
    std::fs::write(root.join("main.c"), src).unwrap();

    let base_arm_line = 6; // the `#else` body: what the default configuration compiles
    let variant_arm_line = 4; // the `#ifdef USE_HW` body

    let base_program = build_program(root, &default_opts(root)).expect("build base program");
    let base_install = base_program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "install" && f.is_defined)
        .expect("base defines install");
    assert_eq!(
        base_install.span.line, base_arm_line,
        "sanity: the default configuration compiles the `#else` arm"
    );

    let explore_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let program = build_program(root, &explore_opts).expect("build explore program");

    let install: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "install" && f.is_defined)
        .collect();
    assert_eq!(install.len(), 1, "one canonical `install` entry");
    assert_eq!(
        install[0].span.line, base_arm_line,
        "the base configuration's definition stays canonical; a variant must not \
         overwrite its span"
    );

    let cb_lines: Vec<u32> = program
        .symbols
        .call_sites
        .iter()
        .filter(|cs| cs.callee_name == "cb")
        .map(|cs| cs.span.line)
        .collect();
    assert!(
        cb_lines.contains(&base_arm_line),
        "the baseline call site at line {base_arm_line} must survive exploration, got {cb_lines:?}"
    );
    assert!(
        cb_lines.contains(&variant_arm_line),
        "the variant arm's call site at line {variant_arm_line} is the fact exploration \
         exists to recover, got {cb_lines:?}"
    );
}
