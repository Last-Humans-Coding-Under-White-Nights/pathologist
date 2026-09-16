use std::fs;
use std::path::Path;
use trace_ir::{FnId, Program};
use trace_parse::build_program;
use trace_preproc::PreprocessOptions;

/// Write `units` as `<name>.<ext>` with one compile command each, link them
/// into `targets`, and index the result.
///
/// Every test here needs the same scaffold — sources, `compile_commands.json`,
/// `link_commands.json` — and differs only in the source text and the target
/// membership, so the assertions are what should be visible in each test.
fn linked_program(
    root: &Path,
    ext: &str,
    units: &[(&str, &str)],
    targets: &[(&str, &[&str])],
) -> Program {
    for (name, source) in units {
        fs::write(root.join(format!("{name}.{ext}")), source).unwrap();
    }
    let compiler = if ext == "c" { "cc" } else { "c++" };
    let commands: Vec<_> = units
        .iter()
        .map(|(name, _)| {
            serde_json::json!({
                "directory": root, "file": format!("{name}.{ext}"), "output": format!("{name}.o"),
                "arguments": [compiler, "-c", format!("{name}.{ext}"), "-o", format!("{name}.o")]
            })
        })
        .collect();
    fs::write(
        root.join("compile_commands.json"),
        serde_json::to_vec(&commands).unwrap(),
    )
    .unwrap();
    let links: Vec<_> = targets
        .iter()
        .map(|(target, members)| {
            let mut arguments = vec![compiler.to_string()];
            arguments.extend(members.iter().map(|m| format!("{m}.o")));
            arguments.extend(["-o".to_string(), (*target).to_string()]);
            serde_json::json!({"directory": root, "output": target, "arguments": arguments})
        })
        .collect();
    fs::write(
        root.join("link_commands.json"),
        serde_json::to_vec(&links).unwrap(),
    )
    .unwrap();
    build_program(root, &PreprocessOptions::new()).unwrap()
}

/// The file each of `caller`'s calls to `name` binds to, with its weak flag.
fn bound_callees(program: &Program, caller: FnId, name: &str) -> Vec<(String, bool)> {
    program
        .symbols
        .call_sites
        .iter()
        .filter(|c| c.caller == caller && c.callee_name == name)
        .flat_map(|site| program.symbols.callees_of(site))
        .map(|id| {
            let f = program.symbols.function(id);
            (
                program.symbols.files[f.file.0 as usize]
                    .path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                f.is_weak,
            )
        })
        .collect()
}

/// The single definition of `name`.
fn defined(program: &Program, name: &str) -> FnId {
    let found: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == name && f.is_defined)
        .collect();
    assert_eq!(found.len(), 1, "expected one {name}: {found:?}");
    found[0].id
}

#[test]
fn link_targets_select_strong_and_keep_separate_weak_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/weak_symbol_override");
    for name in ["fallback", "strong", "caller"] {
        fs::copy(
            fixture.join(format!("{name}.c")),
            dir.path().join(format!("{name}.c")),
        )
        .unwrap();
    }
    let commands: Vec<_> = ["fallback", "strong", "caller"]
        .iter()
        .map(|name| {
            serde_json::json!({
                "directory": dir.path(), "file": format!("{name}.c"),
                "arguments": ["cc", "-c", format!("{name}.c"), "-o", format!("{name}.o")],
                "output": format!("{name}.o")
            })
        })
        .collect();
    fs::write(
        dir.path().join("compile_commands.json"),
        serde_json::to_vec(&commands).unwrap(),
    )
    .unwrap();
    fs::write(dir.path().join("link_commands.json"), serde_json::to_vec(&serde_json::json!([
        {"directory":dir.path(), "output":"full", "arguments":["cc", "caller.o", "fallback.o", "strong.o", "-o", "full"]},
        {"directory":dir.path(), "output":"fallback", "arguments":["cc", "caller.o", "fallback.o", "-o", "fallback"]}
    ])).unwrap()).unwrap();
    let program = build_program(dir.path(), &PreprocessOptions::new()).unwrap();
    let callers: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "caller")
        .collect();
    assert_eq!(
        callers.len(),
        2,
        "shared source needs one caller per target"
    );
    let mut selected = Vec::new();
    for caller in callers {
        let call = program
            .symbols
            .call_sites
            .iter()
            .find(|c| c.caller == caller.id && c.callee_name == "hook")
            .unwrap();
        let callees = program.symbols.callees_of(call);
        assert_eq!(callees.len(), 1);
        let hook = program.symbols.function(callees[0]);
        selected.push(
            program.symbols.files[hook.file.0 as usize]
                .path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        );
    }
    selected.sort();
    assert_eq!(selected, ["fallback.c", "strong.c"]);
}

/// A strong global in one namespace must not silence a weak global that merely
/// shares its unqualified name: they are different link-time symbols.
#[test]
fn a_strong_global_does_not_override_a_weak_one_in_another_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let program = linked_program(
        dir.path(),
        "cpp",
        &[(
            "mod",
            "typedef void (*callback)(void);\n\
             void foo(void) {}\n\
             void bar(void) {}\n\
             namespace a { __attribute__((weak)) callback cb = foo; }\n\
             namespace b { callback cb = bar; }\n",
        )],
        &[("app", &["mod"])],
    );
    let cbs: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "cb")
        .collect();
    assert_eq!(cbs.len(), 2, "namespaced globals stay distinct: {cbs:?}");
    // Exactly one is weak: the annotation must not propagate by unqualified
    // name to the unrelated `b::cb`.
    assert_eq!(
        cbs.iter().filter(|v| v.is_weak).count(),
        1,
        "weakness leaked across namespaces: {cbs:?}"
    );
    // And the weak one keeps its definition: `b::cb` is a different symbol,
    // so it cannot override it.
    let weak = cbs.iter().find(|v| v.is_weak).expect("weak a::cb");
    assert!(
        weak.is_defined,
        "a strong b::cb must not suppress a::cb: {weak:?}"
    );
}

/// A strong C++ definition must find the weak definition of its own signature,
/// even when a *different* overload of the name was registered first — that
/// other overload is what the scoped lookup lands on as the primary entry.
#[test]
fn a_strong_cpp_overload_overrides_the_weak_definition_of_its_signature() {
    let dir = tempfile::tempdir().unwrap();
    // Names order the units, so `hook(int)` registers before either
    // `hook(double)`.
    let program = linked_program(
        dir.path(),
        "cpp",
        &[
            (
                "a_other",
                "void other_body(void) {}\nvoid hook(int) { other_body(); }\n",
            ),
            (
                "b_weak",
                "void weak_body(void) {}\n\
                 __attribute__((weak)) void hook(double) { weak_body(); }\n",
            ),
            (
                "c_strong",
                "void strong_body(void) {}\nvoid hook(double) { strong_body(); }\n",
            ),
            (
                "d_caller",
                "void hook(double);\nvoid caller(void) { hook(1.0); }\n",
            ),
        ],
        &[("app", &["a_other", "b_weak", "c_strong", "d_caller"])],
    );
    // The weak body must have been superseded, not merely out-ranked later.
    let weak_defs: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "hook" && f.is_weak && f.is_defined)
        .collect();
    assert!(
        weak_defs.is_empty(),
        "weak hook(double) survived the strong definition: {weak_defs:?}"
    );
    let selected: Vec<_> = bound_callees(&program, defined(&program, "caller"), "hook")
        .into_iter()
        .filter(|(file, _)| file.ends_with("_weak.cpp") || file.ends_with("_strong.cpp"))
        .collect();
    assert!(
        selected.iter().all(|(_, weak)| !weak),
        "call bound to the weak body: {selected:?}"
    );
}

/// A namespaced global must not occupy the image's binding for its unqualified
/// name, or an unrelated C global of that name unifies with it.
#[test]
fn a_namespaced_global_does_not_claim_the_unqualified_binding() {
    let dir = tempfile::tempdir().unwrap();
    let program = linked_program(
        dir.path(),
        "cpp",
        &[
            ("a_ns", "namespace ns { int counter = 1; }\n"),
            ("b_plain", "int counter = 2;\n"),
        ],
        &[("app", &["a_ns", "b_plain"])],
    );
    let counters: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "counter" && v.storage == trace_ir::StorageClass::Global)
        .collect();
    assert_eq!(
        counters.len(),
        2,
        "ns::counter and ::counter are separate symbols: {counters:?}"
    );
    // The unqualified binding belongs to the plain global, not the namespaced one.
    let target = program.link_targets[0].id;
    let bound = program
        .symbols
        .target_global(target, "counter")
        .expect("binding for counter");
    assert!(
        !program.symbols.variable(bound).is_namespaced,
        "a namespaced global claimed the unqualified binding"
    );
}

/// The `--link-commands` flag names a database outside the auto-discovered
/// locations. Every other test here relies on discovery, so the explicit path —
/// CLI flag through to `LinkDatabase::load` — would otherwise go unexercised.
#[test]
fn an_explicit_link_commands_path_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Build the project with discovery in place, then move the link database
    // somewhere discovery will not look.
    let program = linked_program(
        root,
        "c",
        &[
            ("fallback", "__attribute__((weak)) void hook(void) {}\n"),
            ("strong", "void hook(void) {}\n"),
            (
                "caller",
                "void hook(void);\nvoid caller(void) { hook(); }\n",
            ),
        ],
        &[("app", &["fallback", "strong", "caller"])],
    );
    assert_eq!(program.link_targets.len(), 1, "discovery precondition");

    let moved = root.join("elsewhere").join("links.json");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(root.join("link_commands.json"), &moved).unwrap();

    // Without the flag there is no link metadata at all.
    let discovered = build_program(root, &PreprocessOptions::new()).unwrap();
    assert!(
        discovered.link_targets.is_empty(),
        "moving the database should defeat discovery"
    );

    let mut opts = PreprocessOptions::new();
    opts.link_commands = Some(moved);
    let explicit = build_program(root, &opts).unwrap();
    assert_eq!(explicit.link_targets.len(), 1, "flag not honoured");
    assert_eq!(explicit.link_targets[0].name, "app");
    // And the metadata is actually used: the strong definition wins.
    let weak_defs: Vec<_> = explicit
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "hook" && f.is_weak && f.is_defined)
        .collect();
    assert!(weak_defs.is_empty(), "weak hook survived: {weak_defs:?}");
}

/// A C tentative definition (`int x;`) counts as a strong definition and
/// overrides a weak one, matching `-fno-common`. Documented in ANALYSIS.md;
/// every other override test uses an *initialized* strong global.
#[test]
fn a_tentative_definition_overrides_a_weak_global() {
    let dir = tempfile::tempdir().unwrap();
    let program = linked_program(
        dir.path(),
        "c",
        &[
            (
                "a_weak",
                "void fallback(void) {}\n\
                 typedef void (*callback)(void);\n\
                 __attribute__((weak)) callback hook = fallback;\n",
            ),
            // No initializer: a tentative definition.
            (
                "b_tentative",
                "typedef void (*callback)(void);\ncallback hook;\n",
            ),
        ],
        &[("app", &["a_weak", "b_tentative"])],
    );
    let hooks: Vec<_> = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "hook" && v.storage == trace_ir::StorageClass::Global)
        .collect();
    assert_eq!(hooks.len(), 1, "one symbol per image: {hooks:?}");
    assert!(
        !hooks[0].is_weak,
        "the tentative definition should have superseded the weak one: {hooks:?}"
    );
}
