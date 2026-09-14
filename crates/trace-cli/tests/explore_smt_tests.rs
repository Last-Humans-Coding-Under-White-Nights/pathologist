#![cfg(feature = "smt")]

mod common;

use trace_analysis::analyze;
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn test_explore_smt_multi_variable_condition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("my_config") {
            defines = [
                "CONFIG_A",
                "LEVEL=2",
            ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        void common_sink(void) {}

        #if defined(CONFIG_A) && (LEVEL >= 2)
        void multi_var_worker(void) { common_sink(); }
        #endif

        int main(void) {
            return 0;
        }
        "#,
    )
    .unwrap();

    // 1. Without SMT (greedy explore): single-define test fails to activate multi-variable condition.
    let greedy_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let greedy_prog = build_program(root, &greedy_opts).expect("build greedy program");
    let fn_names_greedy: Vec<_> = greedy_prog
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        !fn_names_greedy.contains(&"multi_var_worker"),
        "greedy explore should fail on multi-variable conjunction"
    );

    // 2. With SMT explore: Z3 MaxSMT jointly satisfies both CONFIG_A and LEVEL=2.
    let smt_opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_smt(true)
        .with_explore_budget(4);
    let smt_prog = build_program(root, &smt_opts).expect("build smt program");
    let fn_names_smt: Vec<_> = smt_prog
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        fn_names_smt.contains(&"multi_var_worker"),
        "SMT explore should recover multi_var_worker"
    );

    let (_pag, analysis) = analyze(&smt_prog);
    assert!(
        !analysis.call_edges.is_empty(),
        "call from multi_var_worker to common_sink should exist"
    );
}

#[test]
fn test_explore_smt_arithmetic_kernel_version() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("driver_cfg") {
            defines = [
                "LINUX_VERSION_CODE=393216",
            ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("driver.c"),
        r#"
        #define KERNEL_VERSION(a,b,c) (((a) << 16) + ((b) << 8) + (c))

        void legacy_handler(void) {}

        #if defined(LINUX_VERSION_CODE) && LINUX_VERSION_CODE < KERNEL_VERSION(6, 6, 0)
        void legacy_driver_init(void) {
            legacy_handler();
        }
        #endif


        int main(void) {
            return 0;
        }
        "#,
    )
    .unwrap();

    let smt_opts = PreprocessOptions::new()
        .with_explore_smt(true)
        .with_explore_budget(4);
    let prog = build_program(root, &smt_opts).expect("build smt program");
    let fn_names: Vec<_> = prog
        .symbols
        .functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert!(
        fn_names.contains(&"legacy_driver_init"),
        "SMT explore should evaluate KERNEL_VERSION and recover legacy_driver_init"
    );
}

#[test]
fn test_explore_smt_determinism() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("BUILD.gn"),
        r#"
        config("cfg") {
            defines = [
                "FEATURE_1",
                "FEATURE_2",
                "FEATURE_3",
            ]
        }
        "#,
    )
    .unwrap();

    std::fs::write(
        root.join("main.c"),
        r#"
        #if defined(FEATURE_1)
        void f1(void) {}
        #endif
        #if defined(FEATURE_2)
        void f2(void) {}
        #endif
        #if defined(FEATURE_3)
        void f3(void) {}
        #endif

        int main(void) { return 0; }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new()
        .with_explore_smt(true)
        .with_explore_budget(4);

    let prog1 = build_program(root, &opts).expect("build run 1");
    let prog2 = build_program(root, &opts).expect("build run 2");

    let fns1: Vec<_> = prog1
        .symbols
        .functions
        .iter()
        .map(|f| f.name.clone())
        .collect();
    let fns2: Vec<_> = prog2
        .symbols
        .functions
        .iter()
        .map(|f| f.name.clone())
        .collect();

    assert_eq!(fns1, fns2);
    assert_eq!(prog1.variants_merged, prog2.variants_merged);
}
