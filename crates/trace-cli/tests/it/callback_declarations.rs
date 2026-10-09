//! #219: named callback parameters and standalone callback declarations.
use crate::common::*;
use trace_analysis::{analyze, ResolutionKind};
use trace_ir::{Linkage, StorageClass, TypeDesc};
use trace_parse::{build_program, build_program_with_jobs};

#[test]
fn recovered_declarations_walk_array_bounds_but_not_prototype_parameters() {
    let source = r#"
int size() { return 1; }
int rows() { return 2; }
int columns() { return 3; }
int seed() { return 0; }
constexpr int prototype_size() { return 4; }
int prototype_default() { return 5; }
void simple() { int arr[size()], post(void (*cb)()); }
void nested() { int (*ptr)[size()], post(void (*cb)()); }
void multidimensional() { int arr[rows()][columns()], post(void (*cb)()); }
void callbacks() { int (*arr[size()])(), post(void (*cb)()); }
void initialized() { int arr[size()], value = seed(), post(void (*cb)()); }
void parameters() {
    int arr[size()], post(void (*cb)()),
        prototype(int values[prototype_size()], int value = prototype_default());
}
"#;
    let dir = scratch(&[("main.cpp", source)]);
    let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let (_, analysis) = analyze(&program);
    for (caller, callee) in [
        ("simple", "size"),
        ("nested", "size"),
        ("multidimensional", "rows"),
        ("multidimensional", "columns"),
        ("callbacks", "size"),
        ("initialized", "size"),
        ("initialized", "seed"),
        ("parameters", "size"),
    ] {
        let caller_id = only_function(&program, caller);
        let callee_id = only_function(&program, callee);
        let edges: Vec<_> = analysis
            .call_edges
            .iter()
            .filter(|edge| {
                edge.caller == caller_id
                    && edge.callee == callee_id
                    && edge.resolution == ResolutionKind::Direct
            })
            .collect();
        assert_eq!(edges.len(), 1, "{caller} must evaluate {callee} once");
        let site = program.symbols.call_site_by_id(edges[0].call_site).unwrap();
        let line = source.lines().nth(site.span.line as usize - 1).unwrap();
        assert!(
            line.contains(&format!("{callee}()")),
            "original position of {caller} -> {callee}"
        );
    }
    assert!(
        !program.symbols.call_sites.iter().any(|site| {
            matches!(
                site.callee_name.as_ref(),
                "prototype_size" | "prototype_default" | "post" | "prototype"
            )
        }),
        "prototype parameters and declarations do not execute in their enclosing function"
    );
    assert!(
        !program
            .symbols
            .function(only_function(&program, "prototype"))
            .is_defined
    );
}

#[test]
fn recovered_mixed_declarations_keep_bare_variable_initializers() {
    let dir = scratch(&[(
        "main.cpp",
        r#"
void job() {}
using F = void (*)();
F global_value = job, global_post(F (*cb)());
extern F external_value = job, external_post(F (*cb)());
void caller() {
    F value = job, post(F (*cb)());
    value();
    global_value();
    external_value();
}
"#,
    )]);
    let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let (_, analysis) = analyze(&program);
    let caller = only_function(&program, "caller");
    let job = only_function(&program, "job");
    for name in ["value", "global_value", "external_value"] {
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|site| site.caller == caller && site.callee_name.as_ref() == name)
            .unwrap();
        assert!(
            analysis
                .call_edges
                .iter()
                .any(|edge| edge.call_site == site.id
                    && edge.callee == job
                    && edge.resolution == ResolutionKind::Indirect),
            "{name} must retain its initializer's callback"
        );
    }
    for name in ["global_value", "external_value"] {
        assert!(
            program
                .symbols
                .variable(only_variable(&program, name))
                .is_defined
        );
    }
    for name in ["post", "global_post", "external_post"] {
        assert!(
            !program
                .symbols
                .function(only_function(&program, name))
                .is_defined
        );
    }
}

#[test]
fn recovered_mixed_objects_keep_storage_specifier_lifecycle_rules() {
    let dir = scratch(&[(
        "main.cpp",
        r#"
struct Result { Result() {} ~Result() {} };
void external() { extern Result obj, post(void (*cb)()); }
void ordinary() { Result obj, post(void (*cb)()); }
void static_storage() { static Result obj; }
void thread_storage() { thread_local Result obj; }
"#,
    )]);
    let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let (_, analysis) = analyze(&program);
    let external = only_function(&program, "external");
    assert!(
        !program
            .symbols
            .call_sites
            .iter()
            .any(|site| site.caller == external),
        "an extern object declaration neither constructs nor destroys an object"
    );
    for caller in ["ordinary", "static_storage", "thread_storage"] {
        assert!(
            has_edge(
                &program,
                &analysis,
                caller,
                "Result::Result",
                ResolutionKind::Direct
            ),
            "{caller} must construct its object"
        );
        assert_eq!(
            has_edge(
                &program,
                &analysis,
                caller,
                "Result::~Result",
                ResolutionKind::Direct
            ),
            caller == "ordinary",
            "{caller}: only the automatic object is destroyed at scope exit"
        );
    }
    assert_eq!(
        program
            .symbols
            .variable(local_variable(&program, "static_storage", "obj"))
            .storage,
        StorageClass::FnStatic
    );
}

#[test]
fn empty_callback_declarations_preserve_functions_parameters_and_calls() {
    let root = fixture("cpp_empty_callback");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    let (_, analysis) = analyze(&program);
    for name in [
        "a_post",
        "b_post",
        "c_post",
        "external_post",
        "c_linked_post",
        "api::post",
        "api::defaults",
        "api::first",
        "api::second",
        "api::qualified",
        "result_post",
        "block_post",
    ] {
        let function = program.symbols.function(only_function(&program, name));
        assert!(!function.is_defined, "{name} remains a prototype");
        assert_eq!(function.params.len(), 1, "{name}");
        let parameter = program.symbols.variable(function.params[0]);
        assert_eq!(parameter.name, "cb", "{name}");
        assert!(
            matches!(
                program.types.get(parameter.type_id).desc.innermost().0,
                TypeDesc::FnPtr { .. }
            ),
            "{name}: {:?}",
            program.types.get(parameter.type_id).desc
        );
        assert!(
            has_edge(
                &program,
                &analysis,
                "caller",
                name,
                ResolutionKind::External
            ),
            "{name}"
        );
        assert!(
            !program.symbols.variables.iter().any(|v| v.name == name),
            "no spurious variable {name}"
        );
    }
    assert!(
        program
            .symbols
            .function(only_function(&program, "c_linked_post"))
            .c_linkage
    );
    assert_eq!(
        program
            .symbols
            .function(only_function(&program, "local_post"))
            .linkage,
        Linkage::Internal
    );
    assert!(
        program
            .symbols
            .function(only_function(&program, "defined_post"))
            .is_defined
    );
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name.contains("__trace_callback_recovery")));
    let function = program.symbols.function(only_function(&program, "a_post"));
    assert_eq!(function.span.line, 3);
    assert_eq!(function.span.col, 6);
    assert_eq!(function.span.file, file_id(&program, &root, "api.hpp"));
    let cb = program.symbols.variable(function.params[0]);
    assert_eq!(cb.span.line, 3);
    assert_eq!(cb.span.col, 22);
    let function = program
        .symbols
        .function(only_function(&program, "result_post"));
    let cb = program.symbols.variable(function.params[0]);
    assert!(matches!(program.types.get(cb.type_id).desc.innermost().0,
        TypeDesc::FnPtr { ret, .. } if matches!(ret.as_ref(), TypeDesc::Struct { name, .. } if name == "Result")));
    let defaults = program
        .symbols
        .function(only_function(&program, "api::defaults"));
    assert_eq!(defaults.default_args, 1);
}

#[test]
fn empty_callback_variables_keep_storage_types_and_initialization_flow() {
    let root = fixture("cpp_empty_callback");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    let (_, analysis) = analyze(&program);
    for (name, storage) in [
        ("callback", StorageClass::Global),
        ("initialized", StorageClass::Global),
        ("file_callback", StorageClass::FileStatic),
        ("local_callback", StorageClass::Local),
        ("static_callback", StorageClass::FnStatic),
        ("first_callback", StorageClass::Global),
        ("second_callback", StorageClass::Global),
    ] {
        let variable = program.symbols.variable(only_variable(&program, name));
        assert_eq!(variable.storage, storage, "{name}");
        assert!(
            matches!(
                program.types.get(variable.type_id).desc.innermost().0,
                TypeDesc::FnPtr { .. }
            ),
            "{name}: {:?}",
            program.types.get(variable.type_id).desc
        );
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == name),
            "{name} is a variable"
        );
    }
    assert!(
        !program
            .symbols
            .variable(only_variable(&program, "declared_callback"))
            .is_defined
    );
    assert!(!program.symbols.variables.iter().any(|v| v.name == "job"));
    let caller = only_function(&program, "caller");
    let job = only_function(&program, "job");
    for name in [
        "callback",
        "initialized",
        "local_callback",
        "static_callback",
        "first_callback",
        "second_callback",
        "alias_callback",
        "using_callback",
    ] {
        let site = program
            .symbols
            .call_sites
            .iter()
            .find(|site| site.caller == caller && site.callee_name.as_ref() == name)
            .unwrap();
        assert!(
            analysis
                .call_edges
                .iter()
                .any(|edge| edge.call_site == site.id
                    && edge.callee == job
                    && edge.resolution == ResolutionKind::Indirect),
            "{name}"
        );
    }
    for name in ["Callback", "CallbackAlias"] {
        assert!(
            matches!(
                program.types.resolve_alias(name).unwrap().innermost().0,
                TypeDesc::FnPtr { .. }
            ),
            "{name}"
        );
    }
}

#[test]
fn empty_callback_api_activates_invoke_model() {
    let root = fixture("cpp_empty_callback");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    let analysis = analyze_with_models(
        &program,
        r#"
[[model]]
name = "a_post"
effects = [{ kind = "invoke", param = 0 }]
"#,
    );
    let caller = only_function(&program, "caller");
    let job = only_function(&program, "job");
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|site| site.caller == caller && site.callee_name.as_ref() == "a_post")
        .unwrap();
    assert!(analysis.call_edges.iter().any(|edge| edge.caller == caller
        && edge.callee == job
        && edge.call_site == site.id
        && edge.resolution == ResolutionKind::Indirect));
}

#[test]
fn empty_callback_issue_repro_works_in_c_and_cpp() {
    let source = "void job() {}\nvoid a_post(void (*cb)());\nvoid b_post(void (*cb)(void));\nvoid c_post(int (*cb)(int));\nvoid caller() { a_post(job); b_post(job); c_post(0); }\n";
    for file in ["main.c", "main.cpp"] {
        let dir = scratch(&[(file, source)]);
        let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
        let (_, analysis) = analyze(&program);
        assert_eq!(program.symbols.functions.len(), 5, "{file}");
        for name in ["a_post", "b_post", "c_post"] {
            assert!(
                has_edge(
                    &program,
                    &analysis,
                    "caller",
                    name,
                    ResolutionKind::External
                ),
                "{file}: {name}"
            );
        }
    }
}

#[test]
fn empty_callback_recovery_preserves_expressions_and_type_shadowing() {
    let dir = scratch(&[(
        "main.cpp",
        r#"
struct Callable { void operator()(); };
Callable make(int);
Callable Callable(int);
void run(int *p) {
    make(*p)();
    Callable(*p)();
    auto make_local = make;
    make_local(*p)();
    int value(42);
    int casted = int(*p);
    const char *literal = R"(void (*fake)(); void fake_api(void (*cb)());)";
    // void (*comment_callback)();
}
void local_shadow(int *p) {
    auto Callable = make;
    Callable(*p)();
}
"#,
    )]);
    let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let (_, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "run",
        "make",
        ResolutionKind::External
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "run",
        "Callable",
        ResolutionKind::External
    ));
    for name in [
        "p",
        "fake",
        "fake_api",
        "comment_callback",
        "value",
        "casted",
        "make_local",
    ] {
        assert!(
            !program.symbols.functions.iter().any(|f| f.name == name),
            "{name}"
        );
    }
    assert_eq!(
        program
            .symbols
            .variables
            .iter()
            .filter(|v| v.name == "p")
            .count(),
        2
    );
    assert!(!program
        .symbols
        .variables
        .iter()
        .any(|v| v.name == "comment_callback" || v.name == "fake"));
}

#[test]
fn empty_callback_recovery_respects_dependency_headers() {
    let dir = scratch(&[
        ("dep/api.hpp", "struct Result {};\nvoid dep_post(Result (*cb)());\nextern void (*dep_callback)();\nvoid dep_body(void (*cb)()) { void (*local)() = cb; local(); }\n"),
        ("main.cpp", "#include \"dep/api.hpp\"\nResult job();\nvoid caller() { dep_post(job); }\n"),
    ]);
    let program = build_program(
        dir.path(),
        &default_opts(dir.path()).with_dep(dir.path().join("dep")),
    )
    .unwrap();
    let (_, analysis) = analyze(&program);
    for name in ["dep_post", "dep_body"] {
        let function = program.symbols.function(only_function(&program, name));
        assert!(!function.is_defined);
        assert!(program.is_dep_file(function.span.file));
        assert!(!program
            .symbols
            .call_sites
            .iter()
            .any(|site| site.caller == function.id));
    }
    let callbacks: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "dep_callback")
        .collect();
    assert!(!callbacks.is_empty());
    for callback in callbacks {
        assert!(program.is_dep_file(callback.span.file));
        assert!(!callback.is_defined);
    }
    assert!(!program.symbols.variables.iter().any(|v| v.name == "local"));
    assert!(has_edge(
        &program,
        &analysis,
        "caller",
        "dep_post",
        ResolutionKind::External
    ));
}

#[test]
fn empty_callback_recovery_preserves_multiline_original_positions() {
    let dir = scratch(&[("main.cpp", "// π\nvoid job() {}\nvoid post(\n    void (*cb)(\n    )\n);\nvoid\n(*callback)(\n);\nvoid caller() { post(job); callback = job; callback(); }\n")]);
    let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let post = program.symbols.function(only_function(&program, "post"));
    let cb = program.symbols.variable(post.params[0]);
    assert_eq!((post.span.line, post.span.col), (3, 6));
    assert_eq!((cb.span.line, cb.span.col), (4, 12));
    let callback = program
        .symbols
        .variable(only_variable(&program, "callback"));
    assert_eq!((callback.span.line, callback.span.col), (8, 1));
}

#[test]
fn empty_callback_recovery_keeps_template_and_alias_types() {
    let dir = scratch(&[(
        "main.cpp",
        r#"
struct Result {};
using ResultAlias = Result;
Result job();
template<class T> void post(T (*cb)());
void alias_post(ResultAlias (*cb)());
namespace api { void qualified_post(::Result (*cb)()); }
void caller() { post<Result>(job); alias_post(job); api::qualified_post(job); }
"#,
    )]);
    let program = build_program(dir.path(), &default_opts(dir.path())).unwrap();
    let (_, analysis) = analyze(&program);
    for name in ["post", "alias_post", "api::qualified_post"] {
        let function = program.symbols.function(only_function(&program, name));
        assert!(!function.is_defined);
        assert_eq!(function.params.len(), 1);
        assert!(
            has_edge(
                &program,
                &analysis,
                "caller",
                name,
                ResolutionKind::External
            ),
            "{name}"
        );
    }
}

#[test]
fn empty_callback_export_is_identical_across_jobs_and_export_modes() {
    let root = fixture("cpp_empty_callback");
    let mut minimal = None;
    for jobs in [1, 2] {
        let program = build_program_with_jobs(&root, &default_opts(&root), jobs).unwrap();
        let (pag, analysis) = analyze(&program);
        let db = export_program(&program, &pag, &analysis);
        let rows = analysis_rows(&db);
        if let Some(expected) = &minimal {
            assert_eq!(&rows, expected);
        } else {
            minimal = Some(rows);
        }
        let full = export_program_full(&program, &pag, &analysis);
        for db in [&db, &full] {
            let conn = rusqlite::Connection::open(db.path()).unwrap();
            assert_eq!(
                text_rows(
                    &conn,
                    "SELECT name, is_defined FROM functions WHERE name = 'a_post'"
                ),
                vec![vec!["Text(\"a_post\")".to_owned(), "Integer(0)".to_owned()]]
            );
            assert_eq!(text_rows(&conn, "SELECT count(*) FROM call_edges e JOIN functions f ON f.id = e.callee_fn_id WHERE f.name = 'a_post'"), vec![vec!["Integer(1)".to_owned()]]);
        }
    }
}

#[test]
fn recovered_declarations_preserve_member_and_template_value_calls() {
    let root = fixture("cpp_callback_recovery_context");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    assert!(program
        .symbols
        .variables
        .iter()
        .any(|v| v.name == "box_callback"));
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "box_callback"));
    for caller in ["MemberShadow::run", "value_template"] {
        let id = only_function(&program, caller);
        assert_eq!(
            program
                .symbols
                .call_sites
                .iter()
                .filter(|site| site.caller == id)
                .count(),
            2,
            "{caller}"
        );
        assert_eq!(
            program
                .symbols
                .variables
                .iter()
                .filter(|v| v.name == "p" && v.fn_id == Some(id))
                .count(),
            1,
            "{caller}"
        );
    }
}

#[test]
fn recovered_declarations_preserve_linkage_and_template_return_context() {
    let root = fixture("cpp_callback_recovery_context");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    assert!(
        !program
            .symbols
            .variable(only_variable(&program, "external_variable"))
            .is_defined
    );
    assert!(
        program
            .symbols
            .variable(only_variable(&program, "defined_variable"))
            .is_defined
    );
    let dependent = program
        .symbols
        .function(only_function(&program, "dependent_post"));
    assert!(matches!(
        *program.types.get(dependent.return_type).desc,
        TypeDesc::Unknown
    ));
    let (_, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "dependent_caller",
        "S::run",
        ResolutionKind::Direct
    ));
}

#[test]
fn recovered_declarations_preserve_direct_initialization() {
    let root = fixture("cpp_callback_recovery_context");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    let (_, analysis) = analyze(&program);
    assert!(has_edge(
        &program,
        &analysis,
        "objects",
        "S::S",
        ResolutionKind::Direct
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "objects",
        "S::~S",
        ResolutionKind::Direct
    ));
    assert!(has_edge(
        &program,
        &analysis,
        "callbacks",
        "job",
        ResolutionKind::Indirect
    ));
    for name in ["obj", "cb"] {
        assert!(!program.symbols.functions.iter().any(|f| f.name == name));
        assert!(program.symbols.variables.iter().any(|v| v.name == name));
    }
}

#[test]
fn recovered_initializer_return_operation_stays_inside_its_declarator() {
    let root = fixture("cpp_callback_recovery_context");
    let program = build_program(&root, &default_opts(&root)).unwrap();
    let caller = only_function(&program, "initialized");
    let site = program
        .symbols
        .call_sites
        .iter()
        .find(|site| site.caller == caller && site.callee_name.as_ref() == "factory")
        .unwrap();
    let (span, expression) = site
        .details
        .as_ref()
        .unwrap()
        .return_operation
        .as_ref()
        .unwrap()
        .as_ref();
    assert_eq!(span.line, 19);
    assert_eq!(expression.as_ref(), "value = factory()");
}
