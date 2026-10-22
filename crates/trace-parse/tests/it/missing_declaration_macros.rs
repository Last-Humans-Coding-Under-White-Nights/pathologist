use std::fs;
use std::path::Path;
use trace_ir::{DiagnosticSeverity, Linkage, Program, TypeDesc};
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/missing_declaration_macros")
}

fn function<'a>(program: &'a Program, name: &str) -> &'a trace_ir::Function {
    program
        .symbols
        .functions
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| {
            panic!(
                "missing {name}; functions: {:?}",
                program
                    .symbols
                    .functions
                    .iter()
                    .map(|f| &f.name)
                    .collect::<Vec<_>>()
            )
        })
}

fn assert_signature(
    program: &Program,
    name: &str,
    ret: TypeDesc,
    params: Vec<TypeDesc>,
    defined: bool,
    c_linkage: bool,
) {
    let f = function(program, name);
    assert_eq!(*program.types.get(f.return_type).desc, ret, "{name}");
    let types: Vec<_> = f
        .param_type_ids
        .iter()
        .map(|&ty| (*program.types.get(ty).desc).clone())
        .collect();
    assert_eq!(types, params, "{name}");
    assert_eq!(f.is_defined, defined, "{name}");
    assert_eq!(f.c_linkage, c_linkage, "{name}");
    assert_eq!(f.linkage, Linkage::External, "{name}");
}

#[test]
fn missing_declaration_macros_preserve_explicit_declarations_and_original_positions() {
    let program = build_program_with_jobs(&fixture(), &PreprocessOptions::new(), 1).unwrap();
    let ptr = |ty| TypeDesc::Ptr(Box::new(ty));
    assert_signature(
        &program,
        "ffi::Declared",
        TypeDesc::Int,
        vec![ptr(TypeDesc::Char), ptr(TypeDesc::Int)],
        false,
        false,
    );
    assert_signature(
        &program,
        "ffi::CDeclared",
        TypeDesc::Int,
        vec![TypeDesc::Double],
        false,
        true,
    );
    assert_signature(
        &program,
        "ffi::Defined",
        TypeDesc::Int,
        vec![TypeDesc::Int],
        true,
        false,
    );
    assert_signature(
        &program,
        "ffi::CBefore",
        ptr(TypeDesc::Char),
        vec![ptr(TypeDesc::Int)],
        true,
        true,
    );
    assert_signature(
        &program,
        "ffi::CAfter",
        TypeDesc::Bool,
        vec![TypeDesc::Long],
        true,
        true,
    );
    assert_signature(
        &program,
        "ffi::CBody",
        TypeDesc::Int,
        vec![TypeDesc::Double],
        true,
        true,
    );
    let result = TypeDesc::Struct {
        name: "ffi::Result".into(),
        fields: vec![("value".into(), TypeDesc::Int)],
    };
    assert_signature(
        &program,
        "ffi::Custom",
        result.clone(),
        vec![result],
        false,
        false,
    );
    // Prototypes use their declarators; definitions use their declaration starts.
    for (name, file, line, col) in [
        ("ffi::Declared", "api.h", 3, 16),
        ("ffi::CDeclared", "api.h", 5, 16),
        ("ffi::Defined", "one.cpp", 4, 12),
        ("ffi::CBefore", "one.cpp", 5, 23),
        ("ffi::CAfter", "one.cpp", 6, 23),
        ("ffi::CBody", "one.cpp", 8, 12),
        ("ffi::Factory::Before", "api.h", 10, 5),
        ("ffi::Factory::After", "api.h", 14, 5),
    ] {
        let f = function(&program, name);
        assert_eq!((f.span.line, f.span.col), (line, col), "{name}");
        assert!(program.symbols.files[f.span.file.0 as usize]
            .path
            .ends_with(file));
    }
    let members: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name.starts_with("ffi::Factory::"))
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(members, ["ffi::Factory::Before", "ffi::Factory::After"]);
    let class = program.types.class_type_id("ffi::Factory").unwrap();
    let TypeDesc::Struct { fields, .. } = &*program.types.get(class).desc else {
        panic!("class type")
    };
    assert_eq!(
        fields,
        &[
            ("before_field".into(), TypeDesc::Int),
            ("after_field".into(), TypeDesc::Long)
        ]
    );
    let reports: Vec<_> = program
        .diagnostics
        .iter()
        .filter(|d| d.stage == "parse")
        .collect();
    assert_eq!(reports.len(), 8, "{reports:?}");
    for d in reports {
        assert_eq!(d.severity, DiagnosticSeverity::Warning);
        let file =
            &program.symbols.files[d.file.expect("original diagnostic file").0 as usize].path;
        if d.message.contains("DECLARE_SINGLETON") {
            assert!(file.ends_with("api.h"));
            assert_eq!(d.line, 11);
            assert!(d.message.contains("unknown and were not indexed"));
        } else {
            assert!(d.message.contains("annotation semantics were omitted"));
            assert!(if file.ends_with("api.h") {
                [3, 5, 17].contains(&d.line)
            } else {
                file.ends_with("one.cpp") && [4, 5, 6, 8].contains(&d.line)
            });
        }
    }
    for caller in ["ffi::Defined", "ffi::CBody"] {
        assert!(program.symbols.call_sites.iter().any(|site| program
            .symbols
            .function(site.caller)
            .name
            == caller
            && site.callee_name == "helper"));
    }
}

#[test]
fn missing_declaration_macros_real_header_definitions_take_precedence() {
    let scratch = tempfile::tempdir().unwrap();
    for file in ["one.cpp", "two.cpp", "api.h", "definitions.h"] {
        let mut source = fs::read_to_string(fixture().join(file)).unwrap();
        if file == "api.h" {
            source.insert_str(0, "#include \"definitions.h\"\n");
        }
        fs::write(scratch.path().join(file), source).unwrap();
    }
    let program = build_program_with_jobs(scratch.path(), &PreprocessOptions::new(), 2).unwrap();
    assert!(
        !program.diagnostics.iter().any(|d| d.stage == "parse"),
        "{:?}",
        program.diagnostics
    );
    for member in ["Factory", "~Factory", "GetInstance"] {
        function(&program, &format!("ffi::Factory::{member}"));
    }
    assert!(function(&program, "ffi::CBefore").c_linkage);
}

#[test]
fn missing_declaration_macros_configuration_and_expansion_provenance_are_respected() {
    let scratch = tempfile::tempdir().unwrap();
    fs::write(
        scratch.path().join("main.cpp"),
        "FFI_EXPORT int configured(int n) { return n; }\n",
    )
    .unwrap();
    let opts = PreprocessOptions::new().with_define("FFI_EXPORT", "__attribute__((weak))");
    let program = build_program_with_jobs(scratch.path(), &opts, 1).unwrap();
    assert!(function(&program, "configured").is_weak);
    assert!(!program.diagnostics.iter().any(|d| d.stage == "parse"));
    fs::write(
        scratch.path().join("main.cpp"),
        "#define FFI_EXPORT FFI_EXPORT\nFFI_EXPORT int unresolved();\n",
    )
    .unwrap();
    let program = build_program_with_jobs(scratch.path(), &PreprocessOptions::new(), 1).unwrap();
    assert!(program
        .diagnostics
        .iter()
        .any(|d| d.message.starts_with("parse errors in")));
    assert!(!program
        .diagnostics
        .iter()
        .any(|d| d.message.starts_with("recovered")));
}

#[test]
fn missing_declaration_macros_keep_unrelated_errors_and_unknown_macros() {
    let scratch = tempfile::tempdir().unwrap();
    fs::write(scratch.path().join("main.cpp"), "FFI_EXPORT int recovered();\nint broken = ;\nUNKNOWN_EXPORT int untouched();\nclass X { UNKNOWN_SINGLETON(X) public: int neighbor; };\n").unwrap();
    let program = build_program_with_jobs(scratch.path(), &PreprocessOptions::new(), 1).unwrap();
    function(&program, "recovered");
    assert!(program
        .diagnostics
        .iter()
        .any(|d| d.message.starts_with("parse errors in")));
    let recovered: Vec<_> = program
        .diagnostics
        .iter()
        .filter(|d| d.message.starts_with("recovered"))
        .collect();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].line, 1);
}

#[test]
fn missing_declaration_macros_are_deterministic_with_cached_headers_and_jobs() {
    let mut expected = None;
    for jobs in [1, 4, 1] {
        let program = build_program_with_jobs(&fixture(), &PreprocessOptions::new(), jobs).unwrap();
        let functions: Vec<_> = program
            .symbols
            .functions
            .iter()
            .map(|f| format!("{f:?}"))
            .collect();
        let diagnostics: Vec<_> = program
            .diagnostics
            .iter()
            .map(|d| format!("{d:?}"))
            .collect();
        let snapshot = (functions, diagnostics);
        if let Some(expected) = &expected {
            assert_eq!(&snapshot, expected);
        } else {
            expected = Some(snapshot);
        }
    }
}
