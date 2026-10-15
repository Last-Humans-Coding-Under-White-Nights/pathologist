#[test]
fn reused_system_syntax_keeps_unit_ownership_and_macro_locations() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("project");
    let system = scratch.path().join("system");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&system).unwrap();
    // An extensionless installed header stays inline, making each unit large
    // enough for syntax reuse without requiring an installed C++ toolchain.
    let mut header: String = (0..800)
        .map(|i| format!("struct Header{i} {{ int field; }};\n"))
        .collect();
    header.push_str(
        "void sink();\n#define FORWARD() sink()\ninline void dependency_body() { sink(); }\n",
    );
    std::fs::write(system.join("library"), header).unwrap();
    for i in 0..4 {
        std::fs::write(
            root.join(format!("unit{i}.cpp")),
            format!(
                "struct Prefix{i} {{ int value; }};\n#include <library>\n{}static void entry() {{ FORWARD(); }}\n",
                "\n".repeat(i)
            ),
        )
        .unwrap();
    }
    let opts = trace_preproc::PreprocessOptions::new().with_system_include(&system);
    let mut snapshots = Vec::new();
    for jobs in [1, 3] {
        let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert_eq!(program.symbols.call_sites.len(), 4);
        for call in &program.symbols.call_sites {
            assert_eq!(call.callee_name.as_str(), "sink");
            assert_eq!(
                program.symbols.files[call.span.file.0 as usize].path,
                system.join("library")
            );
            assert_eq!(call.span.line, 802);
            let caller = program.symbols.function(call.caller);
            let invocation = call.expansion_span.unwrap();
            assert_eq!(invocation.file, caller.span.file);
            assert_eq!(invocation.line, caller.span.line);
            assert!(!program.is_dep_file(caller.span.file));
        }
        assert!(program
            .symbols
            .functions
            .iter()
            .any(|f| f.name == "dependency_body"
                && !f.is_defined
                && program.is_dep_file(f.span.file)));
        snapshots.push(format!(
            "{:?}\n{:?}\n{:?}",
            program.symbols.functions, program.symbols.call_sites, program.flow
        ));
    }
    assert_eq!(snapshots[0], snapshots[1]);
}

#[cfg(unix)]
#[test]
fn compilation_commands_probe_each_compiler_configuration_once() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&system).unwrap();
    std::fs::write(
        system.join("probe.h"),
        "#if defined(__x86_64__) && __STDC_HOSTED__ == 1 && !defined(__GNUC__)\n\
         void from_system(void);\n#else\nvoid wrong_target(void);\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        root.join("one.c"),
        "#include <probe.h>\nvoid one(void) { from_system(); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("two.c"),
        "#include <probe.h>\nvoid two(void) { from_system(); }\n",
    )
    .unwrap();

    let driver = dir.path().join("compiler");
    let log = dir.path().join("calls");
    std::fs::write(
        &driver,
        format!(
            "#!/bin/sh\nprintf x >> '{}'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\nprintf '#define __x86_64__ 1\\n#define __STDC_HOSTED__ 1\\n#define __GNUC__ 13\\n'\n",
            log.display(),
            system.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    let commands: Vec<_> = ["one.c", "two.c"]
        .into_iter()
        .map(|file| {
            serde_json::json!({
                "directory": root,
                "file": file,
                "arguments": [driver, "-c", file]
            })
        })
        .collect();
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::to_vec(&commands).unwrap(),
    )
    .unwrap();

    let default_program =
        trace_parse::build_program(&root, &trace_preproc::PreprocessOptions::new()).unwrap();
    assert!(!log.exists(), "the default must not invoke the compiler");
    assert!(!default_program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "from_system" && default_program.is_dep_file(f.span.file)));

    let program = trace_parse::build_program(
        &root,
        &trace_preproc::PreprocessOptions::new().with_system_includes(true),
    )
    .unwrap();
    assert!(program
        .symbols
        .functions
        .iter()
        .any(|f| { f.name == "from_system" && program.is_dep_file(f.span.file) }));
    assert!(!program
        .symbols
        .functions
        .iter()
        .any(|f| f.name == "wrong_target"));
    assert_eq!(std::fs::read(&log).unwrap(), b"x");
}

#[test]
fn bare_tree_explicit_system_and_after_headers_are_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let system = dir.path().join("system");
    let after = dir.path().join("after");
    for path in [&root, &system, &after] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(system.join("system.h"), "void dep_system(void) {}\n").unwrap();
    std::fs::write(after.join("after.h"), "void dep_after(void) {}\n").unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <system.h>\n#include <after.h>\n",
    )
    .unwrap();
    let mut opts = trace_preproc::PreprocessOptions::new();
    opts.system_include_paths.push(system);
    opts.after_include_paths.push(after);
    let program = trace_parse::build_program(&root, &opts).unwrap();
    for name in ["dep_system", "dep_after"] {
        assert!(
            program.symbols.functions.iter().any(|function| {
                function.name == name
                    && program.is_dep_file(function.span.file)
                    && !function.is_defined
            }),
            "{name} was not a dependency declaration"
        );
    }
}

#[test]
fn compilation_command_after_header_is_a_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let after = dir.path().join("after");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&after).unwrap();
    std::fs::write(after.join("after.h"), "void dep_after(void) {}\n").unwrap();
    std::fs::write(root.join("main.c"), "#include <after.h>\n").unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["cc", "-idirafter", after, "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program =
        trace_parse::build_program(&root, &trace_preproc::PreprocessOptions::new()).unwrap();
    assert!(program.symbols.functions.iter().any(|function| {
        function.name == "dep_after"
            && program.is_dep_file(function.span.file)
            && !function.is_defined
    }));
}
