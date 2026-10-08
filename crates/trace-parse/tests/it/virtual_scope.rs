use trace_preproc::PreprocessOptions;

#[test]
fn nested_virtual_method_does_not_make_enclosing_function_virtual() {
    let tree = tempfile::tempdir().unwrap();
    std::fs::write(
        tree.path().join("scope.cpp"),
        r#"
        struct Outer {
            void Run() {
                struct Local { virtual void Inner() {} };
            }
            virtual void Actual() final {}
        };
    "#,
    )
    .unwrap();
    let program = trace_parse::build_program(tree.path(), &PreprocessOptions::default()).unwrap();
    let function = |name| {
        program
            .symbols
            .functions
            .iter()
            .find(|f| f.name == name)
            .unwrap()
    };
    assert!(!function("Outer::Run").is_virtual);
    assert!(function("Outer::Actual").is_virtual);
    assert!(function("Outer::Actual").is_final);
}
