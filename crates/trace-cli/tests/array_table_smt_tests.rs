
mod common;

use common::*;
use trace_analysis::analyze;
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn test_array_table_direct_indexing_refinement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void target0(void) {}
        void target1(void) {}
        void target2(void) {}
        void target3(void) {}

        void (*g_handlers[4])(void) = {
            target0,
            target1,
            target2,
            target3,
        };

        void call_first(void) {
            g_handlers[0]();
        }

        void call_second(void) {
            g_handlers[1]();
        }

        void call_unknown(int i) {
            g_handlers[i]();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    // 1. call_first (g_handlers[0]) should reach target0, but NOT target1, target2, target3
    assert!(
        has_any_edge(&program, &analysis, "call_first", "target0"),
        "call_first must reach target0"
    );
    assert!(
        !has_any_edge(&program, &analysis, "call_first", "target1"),
        "call_first must prune target1"
    );
    assert!(
        !has_any_edge(&program, &analysis, "call_first", "target2"),
        "call_first must prune target2"
    );
    assert!(
        !has_any_edge(&program, &analysis, "call_first", "target3"),
        "call_first must prune target3"
    );

    // 2. call_second (g_handlers[1]) should reach target1, but NOT target0, target2, target3
    assert!(
        has_any_edge(&program, &analysis, "call_second", "target1"),
        "call_second must reach target1"
    );
    assert!(
        !has_any_edge(&program, &analysis, "call_second", "target0"),
        "call_second must prune target0"
    );
    assert!(
        !has_any_edge(&program, &analysis, "call_second", "target2"),
        "call_second must prune target2"
    );

    // 3. call_unknown (g_handlers[i]) with unbounded index reaches all 4 targets (soundness)
    assert!(has_any_edge(&program, &analysis, "call_unknown", "target0"));
    assert!(has_any_edge(&program, &analysis, "call_unknown", "target1"));
    assert!(has_any_edge(&program, &analysis, "call_unknown", "target2"));
    assert!(has_any_edge(&program, &analysis, "call_unknown", "target3"));
}

#[test]
fn test_array_table_struct_designated_initializers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        struct DriverOps {
            void (*dispatch)(void);
        };

        void dev0_dispatch(void) {}
        void dev1_dispatch(void) {}
        void dev2_dispatch(void) {}

        static struct DriverOps g_ops[3] = {
            [0] = { .dispatch = dev0_dispatch },
            [1] = { .dispatch = dev1_dispatch },
            [2] = { .dispatch = dev2_dispatch },
        };

        void invoke_dev0(void) {
            g_ops[0].dispatch();
        }

        void invoke_dev2(void) {
            g_ops[2].dispatch();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_any_edge(&program, &analysis, "invoke_dev0", "dev0_dispatch"),
        "invoke_dev0 reaches dev0_dispatch"
    );
    assert!(
        !has_any_edge(&program, &analysis, "invoke_dev0", "dev1_dispatch"),
        "invoke_dev0 must prune dev1_dispatch"
    );
    assert!(
        !has_any_edge(&program, &analysis, "invoke_dev0", "dev2_dispatch"),
        "invoke_dev0 must prune dev2_dispatch"
    );

    assert!(
        has_any_edge(&program, &analysis, "invoke_dev2", "dev2_dispatch"),
        "invoke_dev2 reaches dev2_dispatch"
    );
    assert!(
        !has_any_edge(&program, &analysis, "invoke_dev2", "dev0_dispatch"),
        "invoke_dev2 must prune dev0_dispatch"
    );
}

#[test]
fn test_array_table_smt_bitwise_mask_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void op0(void) {}
        void op1(void) {}
        void op2(void) {}
        void op3(void) {}

        void (*ops_table[4])(void) = {
            op0,
            op1,
            op2,
            op3,
        };

        void dispatch_masked(int id) {
            // (id & 0x01) can only evaluate to 0 or 1
            ops_table[id & 0x01]();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(
        has_any_edge(&program, &analysis, "dispatch_masked", "op0"),
        "dispatch_masked reaches op0"
    );
    assert!(
        has_any_edge(&program, &analysis, "dispatch_masked", "op1"),
        "dispatch_masked reaches op1"
    );
    assert!(
        !has_any_edge(&program, &analysis, "dispatch_masked", "op2"),
        "dispatch_masked must prune op2 (out of mask bounds)"
    );
    assert!(
        !has_any_edge(&program, &analysis, "dispatch_masked", "op3"),
        "dispatch_masked must prune op3 (out of mask bounds)"
    );
}

#[test]
fn test_array_table_modulo_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void mod0(void) {}
        void mod1(void) {}
        void mod2(void) {}

        void (*mod_table[3])(void) = {
            mod0,
            mod1,
            mod2,
        };

        void dispatch_mod(int id) {
            // (id % 2) can only produce 0 or 1
            mod_table[id % 2]();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(has_any_edge(&program, &analysis, "dispatch_mod", "mod0"));
    assert!(has_any_edge(&program, &analysis, "dispatch_mod", "mod1"));
    assert!(
        !has_any_edge(&program, &analysis, "dispatch_mod", "mod2"),
        "mod2 must be pruned because id % 2 cannot equal 2"
    );
}

#[cfg(feature = "smt")]
#[test]
fn test_array_table_smt_compound_and_ternary_refinement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void h0(void) {}
        void h1(void) {}
        void h2(void) {}
        void h3(void) {}

        void (*fn_table[4])(void) = {
            h0,
            h1,
            h2,
            h3,
        };

        void dispatch_compound(int x) {
            // ((x & 1) + 2) can only evaluate to 2 or 3
            fn_table[(x & 1) + 2]();
        }

        void dispatch_ternary(int flag) {
            // (flag ? 1 : 3) can only evaluate to 1 or 3
            fn_table[flag ? 1 : 3]();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    // dispatch_compound -> only h2, h3
    assert!(!has_any_edge(&program, &analysis, "dispatch_compound", "h0"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_compound", "h1"));
    assert!(has_any_edge(&program, &analysis, "dispatch_compound", "h2"));
    assert!(has_any_edge(&program, &analysis, "dispatch_compound", "h3"));

    // dispatch_ternary -> only h1, h3
    assert!(!has_any_edge(&program, &analysis, "dispatch_ternary", "h0"));
    assert!(has_any_edge(&program, &analysis, "dispatch_ternary", "h1"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_ternary", "h2"));
    assert!(has_any_edge(&program, &analysis, "dispatch_ternary", "h3"));
}

#[test]
fn test_array_table_macro_and_hex_mask() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        #define SLOT_MASK 0x02

        void s0(void) {}
        void s1(void) {}
        void s2(void) {}
        void s3(void) {}

        void (*slot_table[4])(void) = {
            s0,
            s1,
            s2,
            s3,
        };

        void dispatch_mask(int id) {
            // id & SLOT_MASK (0x02) can only be 0 or 2
            slot_table[id & SLOT_MASK]();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(has_any_edge(&program, &analysis, "dispatch_mask", "s0"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_mask", "s1"));
    assert!(has_any_edge(&program, &analysis, "dispatch_mask", "s2"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_mask", "s3"));
}

#[test]
fn test_array_table_out_of_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void f0(void) {}
        void f1(void) {}

        void (*small_table[2])(void) = {
            f0,
            f1,
        };

        void dispatch_oob(void) {
            // Index 99 does not exist in small_table
            small_table[99]();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(!has_any_edge(&program, &analysis, "dispatch_oob", "f0"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_oob", "f1"));
}

#[cfg(feature = "smt")]
#[test]
fn test_array_table_smt_shift_and_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("main.c"),
        r#"
        void e0(void) {}
        void e1(void) {}
        void e2(void) {}
        void e3(void) {}
        void e4(void) {}

        void (*even_table[5])(void) = {
            e0,
            e1,
            e2,
            e3,
            e4,
        };

        void dispatch_shift(int x) {
            // ((x & 0x01) << 1) can only be 0 or 2
            even_table[(x & 0x01) << 1]();
        }
        "#,
    )
    .unwrap();

    let opts = PreprocessOptions::new().with_include(root.to_path_buf());
    let program = build_program(root, &opts).expect("build program");
    let (_pag, analysis) = analyze(&program);

    assert!(has_any_edge(&program, &analysis, "dispatch_shift", "e0"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_shift", "e1"));
    assert!(has_any_edge(&program, &analysis, "dispatch_shift", "e2"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_shift", "e3"));
    assert!(!has_any_edge(&program, &analysis, "dispatch_shift", "e4"));
}


