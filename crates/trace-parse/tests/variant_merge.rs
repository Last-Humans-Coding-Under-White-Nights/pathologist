use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

#[test]
fn exploration_preserves_arguments_at_shared_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("BUILD.gn"),
        "config(\"x\") { defines = [ \"ALT\", \"EXTRA\" ] }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.c"),
        r#"
void base(void) {}
void alternate(void) {}
void invoke(void (*cb)(void)) { cb(); }
#ifdef ALT
#define CALLBACK alternate
#else
#define CALLBACK base
#endif
#ifdef EXTRA
void extra(void) {}
#endif
int main(void) { invoke(CALLBACK); }
"#,
    )
    .unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new().with_explore(true)).unwrap();
    assert!(program.variants_merged > 0);
    let mut callbacks: Vec<_> = program
        .symbols
        .call_sites
        .iter()
        .filter(|site| site.callee_name == "invoke")
        .flat_map(|site| site.fn_args.iter())
        .map(|(_, id)| program.symbols.function(*id).name.as_str())
        .collect();
    callbacks.sort_unstable();
    assert_eq!(callbacks, ["alternate", "base"]);
    assert_eq!(
        program
            .symbols
            .call_sites
            .iter()
            .filter(|site| site.callee_name == "cb")
            .count(),
        1,
        "identical shared call sites should stay deduplicated"
    );
}

#[test]
fn variant_does_not_duplicate_header_flow_from_prior_tu() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("BUILD.gn"),
        "config(\"x\") { defines = [ \"ALT\" ] }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("helper.h"),
        r#"
static inline void helper(int **p, int *q) {
    *p = q;
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("a.c"),
        r#"
#include "helper.h"
void fa(int **p, int *q) { helper(p, q); }
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.c"),
        r#"
#include "helper.h"
#ifdef ALT
void fb_alt(void) {}
#endif
void fb(int **p, int *q) { helper(p, q); }
"#,
    )
    .unwrap();

    let program = build_program(dir.path(), &PreprocessOptions::new().with_explore(true)).unwrap();
    assert!(program.variants_merged > 0);

    let store_count = program
        .flow
        .iter()
        .filter(|fc| matches!(fc, trace_ir::FlowConstraint::Store { .. }))
        .count();
    assert_eq!(
        store_count, 1,
        "helper's *p = *q store should not be duplicated"
    );
}

#[test]
fn variant_does_not_duplicate_file_scope_variables() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("BUILD.gn"),
        "config(\"x\") { defines = [ \"ALT\" ] }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.c"),
        r#"
static int my_global = 42;
#ifdef ALT
void f_alt(void) { (void)my_global; }
#endif
void f_base(void) { (void)my_global; }
int main(void) { return 0; }
"#,
    )
    .unwrap();

    let program = build_program(dir.path(), &PreprocessOptions::new().with_explore(true)).unwrap();
    assert!(program.variants_merged > 0);

    let my_global_vars: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "my_global")
        .collect();
    assert_eq!(
        my_global_vars.len(),
        1,
        "file-scope variable should not be duplicated by variant merge"
    );
}
