use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use trace_ir::{FnId, Program, TargetId};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

fn write_json(root: &Path, name: &str, value: Value) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn target(program: &Program, name: &str) -> TargetId {
    program
        .link_targets
        .iter()
        .find(|target| target.name == name)
        .unwrap_or_else(|| panic!("missing target {name}"))
        .id
}

fn function(program: &Program, target: TargetId, name: &str) -> FnId {
    let definitions: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.target == Some(target) && f.name == name && f.is_defined)
        .map(|f| f.id)
        .collect();
    assert_eq!(definitions.len(), 1, "expected one {name} in {target:?}");
    definitions[0]
}

fn callees(program: &Program, caller: FnId) -> BTreeSet<String> {
    let expected_target = program.symbols.function(caller).target;
    program
        .symbols
        .call_sites
        .iter()
        .filter(|site| site.caller == caller)
        .flat_map(|site| {
            let callees = program.symbols.callees_of(site);
            assert_eq!(
                callees.len(),
                1,
                "unresolved or ambiguous {}",
                site.callee_name
            );
            callees
        })
        .map(|id| {
            let callee = program.symbols.function(id);
            assert_eq!(callee.target, expected_target, "call crosses target scope");
            callee.name.clone()
        })
        .collect()
}

fn configuration_fixture(root: &Path, weak_fallback: bool) {
    let weak = if weak_fallback {
        "__attribute__((weak))"
    } else {
        ""
    };
    fs::write(
        root.join("hook.c"),
        format!(
            "void full_body(void) {{}}\nvoid fallback_body(void) {{}}\n\
             #ifdef FULL\nvoid hook(void) {{ full_body(); }}\n\
             #else\n{weak} void hook(void) {{ fallback_body(); }}\n#endif\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("caller.c"),
        "void hook(void); void caller(void) { hook(); }\n",
    )
    .unwrap();
    write_json(
        root,
        "compile_commands.json",
        json!([
            {"directory":root,"file":"hook.c","output":"full/hook.o","arguments":["cc","-c","hook.c","-DFULL=1"]},
            {"directory":root,"file":"hook.c","output":"fallback/hook.o","arguments":["cc","-c","hook.c"]},
            {"directory":root,"file":"caller.c","output":"caller.o","arguments":["cc","-c","caller.c"]}
        ]),
    );
}

#[test]
fn different_objects_of_one_source_keep_target_specific_macro_configuration() {
    let dir = tempfile::tempdir().unwrap();
    configuration_fixture(dir.path(), true);
    write_json(
        dir.path(),
        "link_commands.json",
        json!([
            {"directory":dir.path(),"arguments":["cc","caller.o","full/hook.o","-o","full"]},
            {"directory":dir.path(),"arguments":["cc","caller.o","fallback/hook.o","-o","fallback"]}
        ]),
    );
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert_eq!(program.link_targets.len(), 2);
    for (name, body, weak) in [
        ("full", "full_body", false),
        ("fallback", "fallback_body", true),
    ] {
        let scope = target(&program, name);
        let caller = function(&program, scope, "caller");
        let hook = function(&program, scope, "hook");
        assert_eq!(program.symbols.function(hook).is_weak, weak);
        assert_eq!(callees(&program, caller), BTreeSet::from(["hook".into()]));
        assert_eq!(callees(&program, hook), BTreeSet::from([body.into()]));
    }
}

#[test]
fn one_target_retains_bodies_from_both_compilation_configurations() {
    let dir = tempfile::tempdir().unwrap();
    configuration_fixture(dir.path(), false);
    write_json(
        dir.path(),
        "link_commands.json",
        json!([
            {"directory":dir.path(),"arguments":["cc","caller.o","full/hook.o","fallback/hook.o","-o","combined"]}
        ]),
    );
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert_eq!(program.link_targets.len(), 1);
    let scope = target(&program, "combined");
    let caller = function(&program, scope, "caller");
    let hook = function(&program, scope, "hook");
    assert_eq!(callees(&program, caller), BTreeSet::from(["hook".into()]));
    assert_eq!(
        callees(&program, hook),
        BTreeSet::from(["full_body".into(), "fallback_body".into()])
    );
}

#[test]
fn cmake_dependency_closure_selects_override_only_for_linked_target() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("fallback.c"),
        "void weak_body(void) {} __attribute__((weak)) void hook(void) { weak_body(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("strong.c"),
        "void strong_body(void) {} void hook(void) { strong_body(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("caller.c"),
        "void hook(void); void caller(void) { hook(); }\n",
    )
    .unwrap();
    let reply = "build/.cmake/api/v1/reply";
    let definitions = [
        ("base", "STATIC_LIBRARY", vec!["fallback.c"], vec![]),
        ("override", "STATIC_LIBRARY", vec!["strong.c"], vec![]),
        ("bundle", "STATIC_LIBRARY", vec![], vec!["base", "override"]),
        ("full", "EXECUTABLE", vec!["caller.c"], vec!["bundle"]),
        ("fallback", "EXECUTABLE", vec!["caller.c"], vec!["base"]),
    ];
    let references: Vec<_> = definitions
        .iter()
        .map(|(name, _, _, _)| json!({"id":name,"jsonFile":format!("{name}.json")}))
        .collect();
    write_json(
        dir.path(),
        &format!("{reply}/index-001.json"),
        json!({"objects":[{"kind":"codemodel","version":{"major":2},"jsonFile":"codemodel.json"}]}),
    );
    write_json(
        dir.path(),
        &format!("{reply}/codemodel.json"),
        json!({"paths":{"source":dir.path(),"build":dir.path().join("build")},"configurations":[{"targets":references}]}),
    );
    for (name, kind, sources, dependencies) in definitions {
        let sources: Vec<_> = sources.iter().map(|path| json!({"path":path})).collect();
        let dependencies: Vec<_> = dependencies.iter().map(|id| json!({"id":id})).collect();
        write_json(
            dir.path(),
            &format!("{reply}/{name}.json"),
            json!({"name":name,"type":kind,"artifacts":[{"path":name}],"sources":sources,"dependencies":dependencies}),
        );
    }
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    assert_eq!(program.link_targets.len(), 5);
    for (name, body, weak, source) in [
        ("full", "strong_body", false, "strong.c"),
        ("fallback", "weak_body", true, "fallback.c"),
    ] {
        let scope = target(&program, name);
        let caller = function(&program, scope, "caller");
        let hook = function(&program, scope, "hook");
        let definition = program.symbols.function(hook);
        assert_eq!(definition.is_weak, weak);
        assert_eq!(
            program.symbols.files[definition.file.0 as usize]
                .path
                .file_name()
                .unwrap(),
            source
        );
        assert_eq!(callees(&program, caller), BTreeSet::from(["hook".into()]));
        assert_eq!(callees(&program, hook), BTreeSet::from([body.into()]));
    }
}

#[test]
fn weak_body_alternatives_for_one_object_retain_all_configuration_facts() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("hook.c"),
        "void full_body(void) {}\nvoid fallback_body(void) {}\n\
         __attribute__((weak)) void hook(void) {\n\
         #ifdef FULL\nfull_body();\n#else\nfallback_body();\n#endif\n}\n",
    )
    .unwrap();
    write_json(
        dir.path(),
        "compile_commands.json",
        json!([
            {"directory":dir.path(),"file":"hook.c","output":"hook.o","arguments":["cc","-c","hook.c","-DFULL=1"]},
            {"directory":dir.path(),"file":"hook.c","output":"hook.o","arguments":["cc","-c","hook.c"]}
        ]),
    );
    write_json(
        dir.path(),
        "link_commands.json",
        json!([
            {"directory":dir.path(),"arguments":["cc","hook.o","-o","combined"]}
        ]),
    );
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let hook = function(&program, target(&program, "combined"), "hook");
    assert!(program.symbols.function(hook).is_weak);
    assert_eq!(
        callees(&program, hook),
        BTreeSet::from(["full_body".into(), "fallback_body".into()])
    );
}

fn cmake_ordering_fixture(root: &Path, also_linked: bool) {
    fs::write(
        root.join("main.c"),
        "void hook(void); void caller(void) { hook(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("weak.c"),
        "void weak_body(void) {} __attribute__((weak)) void hook(void) { weak_body(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("strong.c"),
        "void strong_body(void) {} void hook(void) { strong_body(); }\n",
    )
    .unwrap();
    let reply = "build/.cmake/api/v1/reply";
    write_json(
        root,
        &format!("{reply}/index-001.json"),
        json!({"objects":[{"kind":"codemodel","version":{"major":2},"jsonFile":"model.json"}]}),
    );
    write_json(
        root,
        &format!("{reply}/model.json"),
        json!({"paths":{"source":root,"build":root.join("build")},"configurations":[{"targets":[{"id":"app","jsonFile":"app.json"},{"id":"unrelated","jsonFile":"unrelated.json"}]}]}),
    );
    let mut fragments = vec![json!({"role":"flags","fragment":"-Wl,-search_paths_first"})];
    if also_linked {
        fragments.push(json!({"role":"libraries","fragment":"libunrelated.a"}));
    }
    write_json(
        root,
        &format!("{reply}/app.json"),
        json!({
            "name":"app", "type":"EXECUTABLE", "artifacts":[{"path":"app"}],
            "sources":[{"path":"main.c"},{"path":"weak.c"}],
            "dependencies":[{"id":"unrelated","backtrace":2}],
            "backtraceGraph":{"commands":["add_executable","add_dependencies"],"nodes":[{}, {"command":0,"parent":0}, {"command":1,"parent":0}]},
            "link":{"commandFragments":fragments}
        }),
    );
    write_json(
        root,
        &format!("{reply}/unrelated.json"),
        json!({"name":"unrelated","type":"STATIC_LIBRARY","artifacts":[{"path":"libunrelated.a"}],"sources":[{"path":"strong.c"}]}),
    );
}

#[test]
fn cmake_build_order_dependency_does_not_override_weak_definition() {
    let dir = tempfile::tempdir().unwrap();
    cmake_ordering_fixture(dir.path(), false);
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let app = target(&program, "app");
    let hook = function(&program, app, "hook");
    assert!(program.symbols.function(hook).is_weak);
    assert_eq!(
        callees(&program, hook),
        BTreeSet::from(["weak_body".into()])
    );
    assert!(program
        .link_targets
        .iter()
        .find(|t| t.id == app)
        .unwrap()
        .dependencies
        .is_empty());
}

#[test]
fn cmake_link_dependency_survives_coexisting_build_order_dependency() {
    let dir = tempfile::tempdir().unwrap();
    cmake_ordering_fixture(dir.path(), true);
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let app = target(&program, "app");
    let hook = function(&program, app, "hook");
    assert!(!program.symbols.function(hook).is_weak);
    assert_eq!(
        callees(&program, hook),
        BTreeSet::from(["strong_body".into()])
    );
    assert_eq!(
        program
            .link_targets
            .iter()
            .find(|t| t.id == app)
            .unwrap()
            .dependencies,
        vec![target(&program, "unrelated")]
    );
}
