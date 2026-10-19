use crate::common::{fixture, has_edge, only_function};
use trace_analysis::{analyze, ResolutionKind};
use trace_ir::Linkage;
use trace_parse::{build_program_with_jobs, has_parse_errors, parse_source_with_lang, SourceLang};
use trace_preproc::{preprocess_file, PreprocessOptions};

#[test]
fn cpp_visibility_linkage_keeps_unrelated_parse_diagnostics() {
    let root = fixture("cpp_visibility_linkage_negative");
    let program = build_program_with_jobs(&root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program.diagnostics.iter().any(|d| d.stage == "parse"));
}

#[test]
fn cpp_visibility_linkage_orders_preserve_identity_and_source_mapping() {
    let root = fixture("cpp_visibility_linkage").canonicalize().unwrap();
    let opts = PreprocessOptions::new().with_include(root.clone());
    for file in ["api.hpp", "factory.cpp", "caller.cpp"] {
        let preprocessed = preprocess_file(&root.join(file), &opts).unwrap();
        let parsed = parse_source_with_lang(preprocessed.output, SourceLang::Cpp).unwrap();
        assert!(!has_parse_errors(&parsed.tree), "{file}: {}", parsed.source);
    }
    for jobs in [1, 8] {
        let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert!(
            !program.diagnostics.iter().any(|d| d.stage == "parse"),
            "{:?}",
            program.diagnostics
        );
        let (_, analysis) = analyze(&program);
        for (name, file, line, defined, start) in [
            ("attribute_first", "factory.cpp", 3, true, "void"),
            ("linkage_first", "factory.cpp", 7, true, "__attribute__"),
            ("declared_only", "api.hpp", 3, false, "declared_only"),
        ] {
            let function = program.symbols.function(only_function(&program, name));
            assert_eq!(function.linkage, Linkage::External);
            assert_eq!(function.is_defined, defined);
            assert_eq!(function.span.line, line);
            assert_eq!(
                program.symbols.files[function.span.file.0 as usize].path,
                root.join(file)
            );
            let source = std::fs::read_to_string(root.join(file)).unwrap();
            let original_line = source.lines().nth(line as usize - 1).unwrap();
            assert_eq!(
                function.span.col as usize,
                original_line.find(start).unwrap() + 1
            );
            assert!(has_edge(
                &program,
                &analysis,
                "caller",
                name,
                if defined {
                    ResolutionKind::Direct
                } else {
                    ResolutionKind::External
                }
            ));
        }
        assert!(has_edge(
            &program,
            &analysis,
            "linkage_first",
            "attribute_first",
            ResolutionKind::Direct
        ));
        for call in &program.symbols.call_sites {
            let source =
                std::fs::read_to_string(&program.symbols.files[call.span.file.0 as usize].path)
                    .unwrap();
            let original_line = source.lines().nth(call.span.line as usize - 1).unwrap();
            assert_eq!(
                call.span.col as usize,
                original_line.find(call.callee_name.as_str()).unwrap() + 1
            );
        }
    }
}
