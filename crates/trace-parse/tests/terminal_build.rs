use trace_parse::{build_program_for_analysis_with_jobs, build_program_with_jobs};
use trace_preproc::PreprocessOptions;

#[test]
fn terminal_build_preserves_finalized_calls_across_indexing_paths() {
    for configured in [false, true] {
        for explore in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("BUILD.gn"),
                "config(\"x\") { defines = [ \"ALT\" ] }\n",
            )
            .unwrap();
            std::fs::write(
                dir.path().join("main.cpp"),
                r#"
struct Base { virtual void run(int *) {} };
struct Derived : Base { void run(int *) override {} };
void invoke(Base *b, int *p) { b->run(p); }
#ifdef ALT
void extra(Base *b, int *p) { invoke(b, p); }
#endif
void entry(Base *b, int *p) { invoke(b, p); external(p); }
"#,
            )
            .unwrap();
            if configured {
                let command = serde_json::json!([{"directory": dir.path(), "file": "main.cpp", "arguments": ["c++", "-c", "main.cpp"]}]);
                std::fs::write(
                    dir.path().join("compile_commands.json"),
                    command.to_string(),
                )
                .unwrap();
            }
            let opts = PreprocessOptions::new().with_explore(explore);
            let retained = build_program_with_jobs(dir.path(), &opts, 2).unwrap();
            let terminal = build_program_for_analysis_with_jobs(dir.path(), &opts, 2).unwrap();
            assert!(!retained.dedup.site_keys.is_empty());
            assert!(terminal.dedup.site_keys.is_empty());
            assert_eq!(
                format!("{:?}", retained.symbols.call_sites),
                format!("{:?}", terminal.symbols.call_sites)
            );
            assert_eq!(
                format!("{:?}", retained.flow),
                format!("{:?}", terminal.flow)
            );
            assert_eq!(
                format!("{:?}", retained.symbols.functions),
                format!("{:?}", terminal.symbols.functions)
            );
        }
    }
}
