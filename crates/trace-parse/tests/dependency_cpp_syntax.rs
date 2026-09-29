use std::fs;
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

#[test]
fn dependency_member_pointer_does_not_hide_following_declarations() {
    let scratch = tempfile::tempdir().unwrap();
    let dep = scratch.path().join("dep");
    fs::create_dir(&dep).unwrap();
    fs::write(
        dep.join("api.hpp"),
        "template<class T, class C> struct Traits {\n\
           using Member = T C::*;\n\
           void useful();\n\
         };\n",
    )
    .unwrap();
    fs::write(
        scratch.path().join("main.cpp"),
        "#include \"dep/api.hpp\"\nint main() { return 0; }\n",
    )
    .unwrap();
    let program =
        build_program_with_jobs(scratch.path(), &PreprocessOptions::new().with_dep(&dep), 1)
            .unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    let useful = program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == "Traits::useful")
        .expect("member declaration after pointer-to-member type");
    assert!(program.symbols.file_is_dep(useful.file));
    assert!(!useful.is_defined);
    assert_eq!(useful.span.line, 3);
}
