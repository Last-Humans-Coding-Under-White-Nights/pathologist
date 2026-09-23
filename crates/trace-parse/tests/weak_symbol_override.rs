use std::fs;
use std::path::Path;
use trace_ir::{FlowConstraint, FnId, Program, TargetId, VarId, Variable};
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

// --- Qualified globals (#133): a namespace-scope global links under its
// canonical name, so it unifies and overrides within an image exactly as a C
// global does under its plain name, and never under its bare leaf. ---

/// The external globals whose canonical name is `qualified`, one per image
/// that links them.
fn qualified_globals<'p>(program: &'p Program, qualified: &str) -> Vec<&'p Variable> {
    program
        .symbols
        .variables
        .iter()
        .filter(|v| v.external_symbol_name() == Some(qualified))
        .collect()
}

/// The one external global named `qualified` in `target`.
fn qualified_global_in<'p>(program: &'p Program, qualified: &str, target: &str) -> &'p Variable {
    let target = program
        .link_targets
        .iter()
        .find(|t| t.name == target)
        .unwrap_or_else(|| panic!("no target {target}"))
        .id;
    let found: Vec<_> = qualified_globals(program, qualified)
        .into_iter()
        .filter(|v| v.target == Some(target))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected one {qualified} in its image: {found:?}"
    );
    assert_eq!(
        program.symbols.target_global(target, qualified),
        Some(found[0].id),
        "the image's binding for {qualified} is its one variable"
    );
    found[0]
}

/// The functions whose address initializes or is stored into `var`.
fn function_values(program: &Program, var: VarId) -> Vec<String> {
    let mut names: Vec<_> = program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::AddrOfFn { dst, callee } if *dst == var => {
                Some(program.symbols.function(*callee).name.clone())
            }
            _ => None,
        })
        .collect();
    names.sort();
    names
}

/// What `reader`'s copy out of a global reads, by `VarId`.
fn copied_into(program: &Program, reader: &str, target: TargetId) -> Vec<VarId> {
    let reader = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == reader && v.target == Some(target))
        .unwrap_or_else(|| panic!("no {reader}"))
        .id;
    program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::Copy { dst, src } if *dst == reader => Some(*src),
            _ => None,
        })
        .collect()
}

const CALLBACKS: &str = "typedef void (*callback)(void);\n";

/// A caller's `a::cb` and the definition in another unit of its image are one
/// symbol, whichever of the two units merges first.
#[test]
fn a_qualified_global_is_one_symbol_per_image_in_both_orders() {
    for (definition, reader) in [("a_def", "b_use"), ("b_def", "a_use")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    definition,
                    &format!("{CALLBACKS}void handler(void) {{}}\nnamespace a {{ callback cb = handler; }}\n"),
                ),
                (
                    reader,
                    &format!(
                        "{CALLBACKS}namespace a {{ extern callback cb; }}\n\
                         callback seen;\n\
                         void read_cb(void) {{ seen = a::cb; }}\n"
                    ),
                ),
            ],
            &[("app", &[definition, reader])],
        );
        let cb = qualified_global_in(&program, "a::cb", "app");
        assert!(cb.is_defined, "{definition} first: {cb:?}");
        assert_eq!(function_values(&program, cb.id), ["handler"]);
        assert_eq!(
            copied_into(&program, "seen", program.link_targets[0].id),
            [cb.id],
            "{definition} first: the reader's a::cb is the defined one"
        );
    }
}

/// The same qualified spelling linked into two images is two symbols: each
/// image's reader sees only its own definition, whether the reader's unit
/// comes before or after the definitions.
#[test]
fn a_qualified_global_stays_separate_in_another_image() {
    for reader in ["reader", "a_reader"] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    "one",
                    &format!(
                        "{CALLBACKS}void first(void) {{}}\nnamespace a {{ callback cb = first; }}\n"
                    ),
                ),
                (
                    "two",
                    &format!(
                        "{CALLBACKS}void second(void) {{}}\nnamespace a {{ callback cb = second; }}\n"
                    ),
                ),
                (
                    reader,
                    &format!(
                        "{CALLBACKS}namespace a {{ extern callback cb; }}\n\
                         callback seen;\n\
                         void read_cb(void) {{ seen = a::cb; }}\n"
                    ),
                ),
            ],
            &[("app_one", &["one", reader]), ("app_two", &["two", reader])],
        );
        let one = qualified_global_in(&program, "a::cb", "app_one");
        let two = qualified_global_in(&program, "a::cb", "app_two");
        assert_ne!(one.id, two.id, "{reader}");
        assert_eq!(function_values(&program, one.id), ["first"], "{reader}");
        assert_eq!(function_values(&program, two.id), ["second"], "{reader}");
        assert_eq!(
            copied_into(&program, "seen", one.target.unwrap()),
            [one.id],
            "{reader}"
        );
        assert_eq!(
            copied_into(&program, "seen", two.target.unwrap()),
            [two.id],
            "{reader}"
        );
    }
}

/// A strong `a::cb` in one unit does not override a weak `b::cb` in another:
/// they share only a leaf name, not a symbol.
#[test]
fn a_strong_qualified_global_does_not_override_another_namespaces_weak_one() {
    for (strong, weak) in [("a_strong", "b_weak"), ("b_strong", "a_weak")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    strong,
                    &format!(
                        "{CALLBACKS}void bar(void) {{}}\nnamespace a {{ callback cb = bar; }}\n"
                    ),
                ),
                (
                    weak,
                    &format!(
                        "{CALLBACKS}void foo(void) {{}}\n\
                         namespace b {{ __attribute__((weak)) callback cb = foo; }}\n"
                    ),
                ),
            ],
            &[("app", &[strong, weak])],
        );
        let a = qualified_global_in(&program, "a::cb", "app");
        let b = qualified_global_in(&program, "b::cb", "app");
        assert!(!a.is_weak, "{strong} first: weakness leaked to a::cb");
        assert!(
            b.is_weak && b.is_defined,
            "{strong} first: b::cb was overridden: {b:?}"
        );
        assert_eq!(function_values(&program, b.id), ["foo"]);
        assert_eq!(function_values(&program, a.id), ["bar"]);
    }
}

/// A strong `a::cb` supersedes a weak `a::cb` in the image linking both, and
/// only there: an image linking the weak definition alone keeps it.
#[test]
fn a_strong_qualified_global_supersedes_its_weak_definition_only_in_its_image() {
    for (weak, strong) in [("a_weak", "b_strong"), ("b_weak", "a_strong")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    weak,
                    &format!(
                        "{CALLBACKS}void fallback(void) {{}}\n\
                         namespace a {{ __attribute__((weak)) callback cb = fallback; }}\n"
                    ),
                ),
                (
                    strong,
                    &format!("{CALLBACKS}void chosen(void) {{}}\nnamespace a {{ callback cb = chosen; }}\n"),
                ),
            ],
            &[("full", &[weak, strong]), ("weak_only", &[weak])],
        );
        let full = qualified_global_in(&program, "a::cb", "full");
        assert!(!full.is_weak && full.is_defined, "{weak} first: {full:?}");
        assert_eq!(
            function_values(&program, full.id),
            ["chosen"],
            "{weak} first: the weak initializer survived the strong definition"
        );
        let weak_only = qualified_global_in(&program, "a::cb", "weak_only");
        assert!(weak_only.is_weak && weak_only.is_defined, "{weak_only:?}");
        assert_eq!(function_values(&program, weak_only.id), ["fallback"]);
    }
}

/// A weak declaration of `a::cb` weakens the definition of `a::cb` it
/// redeclares, and not `b::cb`.
#[test]
fn a_weak_qualified_declaration_weakens_only_its_own_definition() {
    let dir = tempfile::tempdir().unwrap();
    let program = linked_program(
        dir.path(),
        "cpp",
        &[(
            "mod",
            &format!(
                "{CALLBACKS}void foo(void) {{}}\n\
                 namespace a {{ extern callback cb __attribute__((weak)); }}\n\
                 namespace a {{ callback cb = foo; }}\n\
                 namespace b {{ callback cb = foo; }}\n"
            ),
        )],
        &[("app", &["mod"])],
    );
    assert!(qualified_global_in(&program, "a::cb", "app").is_weak);
    assert!(!qualified_global_in(&program, "b::cb", "app").is_weak);
}

/// Every `VarId` `&object` is stored into, `object` being `name` in `target`.
fn addresses_of(program: &Program, name: &str, target: TargetId) -> Vec<VarId> {
    let object = program
        .symbols
        .variables
        .iter()
        .find(|v| v.name == name && v.target == Some(target))
        .unwrap_or_else(|| panic!("no {name}"))
        .id;
    program
        .flow
        .iter()
        .filter_map(|f| match f {
            FlowConstraint::AddrOfVar { dst, src } if *src == object => Some(*dst),
            _ => None,
        })
        .collect()
}

/// A header's qualified external declaration is the one symbol its image
/// defines, whichever unit includes it; its internal-linkage variables — an
/// anonymous namespace's, a namespace `static`, a file `static` — are each
/// including unit's own storage, never unified by the image.
#[test]
fn shared_header_variables_link_by_external_symbol_only() {
    for (writer, reader) in [("a_write", "b_read"), ("b_write", "a_read")] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("h.h"),
            "struct X { int v; };\n\
             namespace ns { extern X *ptr; }\n\
             namespace { X *hidden; }\n\
             namespace ns { static X *internal; }\n\
             static X *file_local;\n",
        )
        .unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    writer,
                    "#include \"h.h\"\n\
                     X written;\n\
                     namespace ns { X *ptr = &written; }\n\
                     void write() { hidden = &written; ns::internal = &written; file_local = &written; }\n",
                ),
                (
                    reader,
                    "#include \"h.h\"\n\
                     X own;\n\
                     X *seen_ptr, *seen_hidden, *seen_internal, *seen_local;\n\
                     void read() {\n\
                         hidden = &own; ns::internal = &own; file_local = &own;\n\
                         seen_ptr = ns::ptr; seen_hidden = hidden;\n\
                         seen_internal = ns::internal; seen_local = file_local;\n\
                     }\n",
                ),
            ],
            &[("app", &[writer, reader])],
        );
        let target = program.link_targets[0].id;
        let ptr = qualified_global_in(&program, "ns::ptr", "app");
        let written = addresses_of(&program, "written", target);
        let own = addresses_of(&program, "own", target);
        assert_eq!(written.iter().filter(|&&v| v == ptr.id).count(), 1);
        assert_eq!(copied_into(&program, "seen_ptr", target), [ptr.id]);
        for reader in ["seen_hidden", "seen_internal", "seen_local"] {
            let read = copied_into(&program, reader, target);
            assert_eq!(read.len(), 1, "{writer} first: {reader} reads {read:?}");
            assert!(
                own.contains(&read[0]),
                "{writer} first: {reader} lost its unit's storage"
            );
            assert!(
                !written.contains(&read[0]),
                "{writer} first: {reader} reads the other unit's storage"
            );
            assert_eq!(
                program.symbols.variable(read[0]).external_symbol_name(),
                None
            );
        }
    }
}

/// Reopening a namespace to redeclare its variable, before and after the
/// definition and in another unit, links one symbol with one initializer.
#[test]
fn qualified_redeclarations_link_one_variable_initialized_once() {
    let dir = tempfile::tempdir().unwrap();
    let program = linked_program(
        dir.path(),
        "cpp",
        &[
            (
                "def",
                "struct X { int v; };\n\
                 X object;\n\
                 namespace ns { extern X *ptr; }\n\
                 namespace ns { X *ptr = &object; }\n\
                 namespace ns { extern X *ptr; }\n\
                 X *seen_here;\n\
                 void read_here() { seen_here = ns::ptr; }\n",
            ),
            (
                "use",
                "struct X { int v; };\n\
                 namespace ns { extern X *ptr; }\n\
                 namespace ns { extern X *ptr; }\n\
                 X *seen_there;\n\
                 void read_there() { seen_there = ns::ptr; }\n",
            ),
        ],
        &[("app", &["def", "use"])],
    );
    let target = program.link_targets[0].id;
    let ptr = qualified_global_in(&program, "ns::ptr", "app");
    assert!(ptr.is_defined);
    assert_eq!(addresses_of(&program, "object", target), [ptr.id]);
    assert_eq!(copied_into(&program, "seen_here", target), [ptr.id]);
    assert_eq!(copied_into(&program, "seen_there", target), [ptr.id]);
}

/// #133 review: a weak out-of-class static member definition is weak, so a
/// strong definition of the member in the same image supersedes it.
#[test]
fn a_strong_static_member_definition_supersedes_a_weak_one() {
    let holder = "struct H { static callback cb; };\n";
    for (weak, strong) in [("a_weak", "b_strong"), ("b_weak", "a_strong")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    weak,
                    &format!(
                        "{CALLBACKS}{holder}void fallback(void) {{}}\n\
                         __attribute__((weak)) callback H::cb = fallback;\n"
                    ),
                ),
                (
                    strong,
                    &format!(
                        "{CALLBACKS}{holder}void chosen(void) {{}}\ncallback H::cb = chosen;\n"
                    ),
                ),
            ],
            &[("full", &[weak, strong]), ("weak_only", &[weak])],
        );
        let full = qualified_global_in(&program, "H::cb", "full");
        assert!(!full.is_weak && full.is_defined, "{weak} first: {full:?}");
        assert_eq!(
            function_values(&program, full.id),
            ["chosen"],
            "{weak} first"
        );
        let weak_only = qualified_global_in(&program, "H::cb", "weak_only");
        assert!(weak_only.is_weak && weak_only.is_defined, "{weak_only:?}");
        assert_eq!(function_values(&program, weak_only.id), ["fallback"]);
    }
}

/// #133 review: a static member defined under `using namespace` in a unit
/// that only forward-declares its class is the member another unit of the
/// image reads through the class body, whichever unit merges first.
#[test]
fn a_static_member_defined_without_its_class_body_links_to_its_class() {
    for (definition, reader) in [("a_def", "b_use"), ("b_def", "a_use")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    definition,
                    &format!(
                        "{CALLBACKS}void handler(void) {{}}\n\
                         namespace OHOS {{ struct FooTest; }}\n\
                         using namespace OHOS;\n\
                         callback FooTest::proxy_ = handler;\n"
                    ),
                ),
                (
                    reader,
                    &format!(
                        "{CALLBACKS}namespace OHOS {{ struct FooTest {{ static callback proxy_; }}; }}\n\
                         callback seen;\n\
                         void read_proxy(void) {{ seen = OHOS::FooTest::proxy_; }}\n"
                    ),
                ),
            ],
            &[("app", &[definition, reader])],
        );
        assert!(
            qualified_globals(&program, "FooTest::proxy_").is_empty(),
            "{definition} first: the definition lost its namespace"
        );
        let proxy = qualified_global_in(&program, "OHOS::FooTest::proxy_", "app");
        assert!(proxy.is_defined, "{definition} first: {proxy:?}");
        assert_eq!(function_values(&program, proxy.id), ["handler"]);
        assert_eq!(
            copied_into(&program, "seen", program.link_targets[0].id),
            [proxy.id],
            "{definition} first"
        );
    }
}

/// #133 review: a qualified `extern` redeclaration declares the variable and
/// defines nothing, so a weak definition linked beside it keeps its value,
/// and the redeclaring unit reads that definition, in either unit order.
#[test]
fn a_qualified_extern_declaration_defines_nothing() {
    for (weak_def, user) in [("a_weak_def", "b_user"), ("b_weak_def", "a_user")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    weak_def,
                    &format!(
                        "{CALLBACKS}void fallback(void) {{}}\n\
                         namespace n {{ __attribute__((weak)) callback cb = fallback; }}\n"
                    ),
                ),
                (
                    user,
                    &format!(
                        "{CALLBACKS}namespace n {{ extern callback cb; }}\n\
                         extern callback n::cb;\n\
                         callback seen;\n\
                         void read_cb(void) {{ seen = n::cb; }}\n"
                    ),
                ),
            ],
            &[("app", &[weak_def, user])],
        );
        let cb = qualified_global_in(&program, "n::cb", "app");
        assert_eq!(
            function_values(&program, cb.id),
            ["fallback"],
            "{weak_def} first: {cb:?}"
        );
        assert_eq!(
            copied_into(&program, "seen", program.link_targets[0].id),
            [cb.id],
            "{weak_def} first"
        );
    }
}

/// #133 review: `__attribute__((weak))` on an in-class static member
/// declaration makes the member weak, so a strong out-of-class definition
/// in another unit of the image supersedes the weak one.
#[test]
fn an_in_class_weak_static_member_is_weak() {
    let holder = "struct H { __attribute__((weak)) static callback cb; };\n";
    for (weak, strong) in [("a_weak", "b_strong"), ("b_weak", "a_strong")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    weak,
                    &format!(
                        "{CALLBACKS}{holder}void fallback(void) {{}}\ncallback H::cb = fallback;\n"
                    ),
                ),
                (
                    strong,
                    &format!(
                        "{CALLBACKS}struct H {{ static callback cb; }};\n\
                         void chosen(void) {{}}\ncallback H::cb = chosen;\n"
                    ),
                ),
            ],
            &[("full", &[weak, strong])],
        );
        let cb = qualified_global_in(&program, "H::cb", "full");
        assert_eq!(
            function_values(&program, cb.id),
            ["chosen"],
            "{weak} first: {cb:?}"
        );
    }
}

/// #133 review: a weak in-class initializer (`inline static`) is the weak
/// definition's own, so a strong definition in the same image drops it.
#[test]
fn a_strong_definition_supersedes_a_weak_inline_member_initializer() {
    for (weak, strong) in [("a_weak", "b_strong"), ("b_weak", "a_strong")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    weak,
                    &format!(
                        "{CALLBACKS}void fallback(void) {{}}\n\
                         struct H {{ __attribute__((weak)) inline static callback cb = fallback; }};\n"
                    ),
                ),
                (
                    strong,
                    &format!(
                        "{CALLBACKS}struct H {{ static callback cb; }};\n\
                         void chosen(void) {{}}\ncallback H::cb = chosen;\n"
                    ),
                ),
            ],
            &[("full", &[weak, strong])],
        );
        let cb = qualified_global_in(&program, "H::cb", "full");
        assert!(!cb.is_weak && cb.is_defined, "{weak} first: {cb:?}");
        assert_eq!(function_values(&program, cb.id), ["chosen"], "{weak} first");
    }
}

/// #133 review: `inline static callback cb;` defines the member (zero
/// initialized) without an initializer, so it supersedes a weak definition
/// as `= nullptr` would.
#[test]
fn an_uninitialized_inline_member_is_a_strong_definition() {
    for (weak, strong) in [("a_weak", "b_strong"), ("b_weak", "a_strong")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    weak,
                    &format!(
                        "{CALLBACKS}struct H {{ static callback cb; }};\n\
                         void fallback(void) {{}}\n\
                         __attribute__((weak)) callback H::cb = fallback;\n"
                    ),
                ),
                (
                    strong,
                    &format!("{CALLBACKS}struct H {{ inline static callback cb; }};\n"),
                ),
            ],
            &[("full", &[weak, strong])],
        );
        let cb = qualified_global_in(&program, "H::cb", "full");
        assert!(!cb.is_weak && cb.is_defined, "{weak} first: {cb:?}");
        assert!(
            function_values(&program, cb.id).is_empty(),
            "{weak} first: {cb:?}"
        );
    }
}

/// #133 review: an `extern "C"` variable declared in a namespace is the
/// C-linkage symbol of its bare name, so it links with that definition in
/// another unit, whichever merges first, and `ns::cb` reads it.
#[test]
fn an_extern_c_namespace_variable_links_by_its_bare_name() {
    for (definition, reader) in [("a_def", "b_use"), ("b_def", "a_use")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    definition,
                    &format!("{CALLBACKS}void handler(void) {{}}\nextern \"C\" {{ callback cb = handler; }}\n"),
                ),
                (
                    reader,
                    &format!(
                        "{CALLBACKS}namespace ns {{ extern \"C\" callback cb; }}\n\
                         callback seen;\n\
                         void read_cb(void) {{ seen = ns::cb; }}\n"
                    ),
                ),
            ],
            &[("app", &[definition, reader])],
        );
        let target = program.link_targets[0].id;
        let cb = program
            .symbols
            .target_global(target, "cb")
            .unwrap_or_else(|| panic!("{definition} first: no image symbol cb"));
        let cb = program.symbols.variable(cb);
        assert!(cb.is_defined, "{definition} first: {cb:?}");
        assert_eq!(
            function_values(&program, cb.id),
            ["handler"],
            "{definition} first"
        );
        assert_eq!(
            copied_into(&program, "seen", target),
            [cb.id],
            "{definition} first"
        );
    }
}

/// `extern "C" T x;` without braces declares `x`, as `extern T x;` does:
/// a weak definition linked beside it keeps its value.
#[test]
fn a_braceless_extern_c_declaration_defines_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let program = linked_program(
        dir.path(),
        "cpp",
        &[
            (
                "weak_def",
                &format!(
                    "{CALLBACKS}void fallback(void) {{}}\n\
                     extern \"C\" {{ __attribute__((weak)) callback cb = fallback; }}\n"
                ),
            ),
            ("user", &format!("{CALLBACKS}extern \"C\" callback cb;\n")),
        ],
        &[("app", &["weak_def", "user"])],
    );
    let target = program.link_targets[0].id;
    let cb = program
        .symbols
        .variable(program.symbols.target_global(target, "cb").unwrap());
    assert!(cb.is_weak, "{cb:?}");
    assert_eq!(function_values(&program, cb.id), ["fallback"], "{cb:?}");
}

/// #133 review: a definition after an `extern "C"` declaration of the same
/// namespace variable inherits its C linkage, so both are the bare symbol
/// another unit's definition or reader links with.
#[test]
fn a_later_definition_inherits_c_linkage() {
    for (definition, reader) in [("a_def", "b_use"), ("b_def", "a_use")] {
        let dir = tempfile::tempdir().unwrap();
        let program = linked_program(
            dir.path(),
            "cpp",
            &[
                (
                    definition,
                    &format!(
                        "{CALLBACKS}void good(void) {{}}\n\
                         namespace N {{ extern \"C\" callback cb; callback cb = good; }}\n"
                    ),
                ),
                (
                    reader,
                    &format!(
                        "{CALLBACKS}extern \"C\" callback cb;\n\
                         callback seen;\n\
                         void read_cb(void) {{ seen = cb; }}\n"
                    ),
                ),
            ],
            &[("app", &[definition, reader])],
        );
        let target = program.link_targets[0].id;
        let cb = program
            .symbols
            .target_global(target, "cb")
            .unwrap_or_else(|| panic!("{definition} first: no image symbol cb"));
        assert_eq!(
            function_values(&program, cb),
            ["good"],
            "{definition} first"
        );
        assert_eq!(
            copied_into(&program, "seen", target),
            [cb],
            "{definition} first"
        );
        assert!(
            program.symbols.target_global(target, "N::cb").is_none(),
            "{definition} first: the definition split off as N::cb"
        );
    }
}
