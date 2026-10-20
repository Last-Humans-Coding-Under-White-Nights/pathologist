#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use trace_ir::Program;
use trace_preproc::PreprocessOptions;

fn driver(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn fixture(root: &Path) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preproc/compiler_predefines");
    std::fs::create_dir_all(root).unwrap();
    for name in ["main.c", "section.h"] {
        std::fs::copy(fixture.join(name), root.join(name)).unwrap();
    }
}

fn names(program: &Program) -> Vec<String> {
    let mut names: Vec<_> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.is_defined)
        .map(|f| f.name.clone())
        .collect();
    names.sort();
    names
}

fn section_error(program: &Program) -> bool {
    program
        .diagnostics
        .iter()
        .any(|d| d.message.contains("No support section"))
}

#[test]
fn identities_select_hdf_and_hiview_branches_without_inventing_context() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fixture(&root);
    let defaults = PreprocessOptions::new();
    let absent = trace_parse::build_program(&root, &defaults).unwrap();
    assert!(names(&absent).contains(&"absent_identity".into()));
    assert!(section_error(&absent));
    let error = absent
        .diagnostics
        .iter()
        .find(|d| d.message.contains("No support section"))
        .unwrap();
    assert_eq!(error.line, 7);
    assert!(error.file.is_some());
    let iar = trace_parse::build_program(&root, &defaults.clone().with_define("__ICCARM__", "1"))
        .unwrap();
    assert!(names(&iar).contains(&"iar_section".into()));
    assert!(!section_error(&iar));

    for (label, dump, expected) in [
        (
            "gnu",
            "#define __GNUC__ 13\n#define __x86_64__ 1",
            vec!["gnu_identity", "gnu_section", "hiview_gnu", "x64_target"],
        ),
        (
            "clang",
            "#define __clang__ 1\n#define __GNUC__ 4\n#define __aarch64__ 1",
            vec!["clang_identity", "gnu_section", "hiview_gnu", "arm_target"],
        ),
        (
            "mingw",
            "#define __GNUC__ 13\n#define __MINGW32__ 1\n#define __MINGW64__ 1",
            vec!["gnu_identity", "gnu_section", "hiview_gnu", "mingw_target"],
        ),
        (
            "clang-msvc",
            "#define __clang__ 1\n#define _MSC_VER 1930",
            vec!["clang_identity", "gnu_section", "hiview_msvc"],
        ),
    ] {
        let compiler = driver(dir.path(), label, &format!("printf '%s\\n' '{dump}'"));
        let opts = defaults
            .clone()
            .with_compiler_predefines(true)
            .with_compiler(compiler);
        let one = trace_parse::build_program_with_jobs(&root, &opts, 1).unwrap();
        let many = trace_parse::build_program_with_jobs(&root, &opts, 4).unwrap();
        let mut expected: Vec<_> = expected.into_iter().map(str::to_string).collect();
        expected.sort();
        assert_eq!(names(&one), expected, "{label}");
        assert_eq!(names(&many), names(&one));
        assert!(!section_error(&one));
        assert!(one.diagnostics.is_empty(), "{:?}", one.diagnostics);
        let function = one
            .symbols
            .functions
            .iter()
            .find(|f| f.name == "gnu_section")
            .unwrap();
        assert_eq!(function.span.line, 3);
    }
}

#[test]
fn all_discovery_modes_are_independent_and_probe_empty_input() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fixture(&root);
    let system = dir.path().join("system");
    std::fs::create_dir(&system).unwrap();
    std::fs::write(system.join("probe.h"), "void system_header(void);\n").unwrap();
    let mut source = std::fs::read_to_string(root.join("main.c")).unwrap();
    source.push_str("\n#include <probe.h>\n");
    std::fs::write(root.join("main.c"), source).unwrap();
    let log = dir.path().join("probes");
    let compiler = driver(dir.path(), "compiler", &format!(
        "if read -r line; then exit 9; fi\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '#define __GNUC__ 12\\n#define __x86_64__ 1\\n'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2", log.display(), system.display()));
    std::fs::write(root.join("compile_commands.json"), serde_json::json!([{
        "directory": root, "file": "main.c", "arguments": [compiler, "-c", "main.c", "-o", "bad.o"]
    }]).to_string()).unwrap();
    for (macros, includes) in [(false, false), (true, false), (false, true), (true, true)] {
        let opts = PreprocessOptions::new()
            .with_compiler_predefines(macros)
            .with_system_includes(includes);
        let program = trace_parse::build_program(&root, &opts).unwrap();
        assert_eq!(!section_error(&program), macros);
        if macros {
            assert!(
                !program
                    .diagnostics
                    .iter()
                    .any(|d| d.message.contains("no compiler context")),
                "{:?}",
                program.diagnostics
            );
        }
        assert_eq!(
            program
                .symbols
                .functions
                .iter()
                .any(|f| f.name == "system_header"),
            includes
        );
        assert_eq!(
            names(&program).contains(&"x64_target".into()),
            macros || includes
        );
        if !macros && !includes {
            assert!(!log.exists());
        }
    }
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), 3);
    for line in probes.lines() {
        assert!(line.ends_with("-x c -"), "{line}");
        assert!(!line.contains("main.c") && !line.contains("bad.o"));
    }
}

#[test]
fn command_driver_wins_and_failure_warnings_are_deduplicated() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fixture(&root);
    std::fs::copy(root.join("main.c"), root.join("two.c")).unwrap();
    let fallback = driver(dir.path(), "fallback", "printf '#define __GNUC__ 13\\n'");
    let failed = driver(
        dir.path(),
        "nonzero",
        "printf '#define __GNUC__ 13\\n'; exit 3",
    );
    let empty = driver(dir.path(), "empty", "exit 0");
    let cl = driver(dir.path(), "cl", "exit 0");
    let iar = driver(dir.path(), "iccarm", "exit 0");
    let missing_cl = dir.path().join("missing/cl");
    for (compiler, reason) in [
        ("/missing/compiler", "not found"),
        (missing_cl.to_str().unwrap(), "not found"),
        (cl.to_str().unwrap(), "unsupported"),
        (iar.to_str().unwrap(), "unsupported"),
        (failed.to_str().unwrap(), "exited"),
        (empty.to_str().unwrap(), "empty GNU-compatible"),
    ] {
        let commands: Vec<_> = ["main.c", "two.c"]
            .into_iter()
            .map(|file| {
                serde_json::json!({
                    "directory": root, "file": file, "arguments": [compiler, "-m32", "-c", file]
                })
            })
            .collect();
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::to_vec(&commands).unwrap(),
        )
        .unwrap();
        let opts = PreprocessOptions::new()
            .with_compiler_predefines(true)
            .with_system_includes(true)
            .with_compiler(&fallback);
        let program = trace_parse::build_program(&root, &opts).unwrap();
        // Orphan fallback headers can use the explicit compiler, but each TU
        // still retains its own failed command configuration and diagnostic.
        assert!(names(&program).contains(&"absent_identity".into()));
        assert!(section_error(&program));
        let warnings: Vec<_> = program
            .diagnostics
            .iter()
            .filter(|d| d.message.contains(reason))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", program.diagnostics);
        let warning = &warnings[0].message;
        if reason == "not found" {
            assert!(!warning.contains("unsupported macro probe"), "{warning}");
        }
        for part in [compiler, "-m32", "language C", "directory", "manual -D"] {
            assert!(warning.contains(part), "{warning}");
        }
    }
}

#[test]
fn intel_driver_names_accept_successful_gnu_macro_dumps() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("main.cpp"),
        "#if __GNUC__ == 13\nvoid selected(void) {}\n#else\n#error missing compiler macros\n#endif\n",
    )
    .unwrap();
    for name in ["icc", "icc-2021.10", "icpc", "icpc-2021.10"] {
        let compiler = driver(dir.path(), name, "printf '#define __GNUC__ 13\\n'");
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([{
                "directory": root, "file": "main.cpp", "arguments": [compiler, "-c", "main.cpp"]
            }])
            .to_string(),
        )
        .unwrap();
        for includes in [false, true] {
            let opts = PreprocessOptions::new()
                .with_compiler_predefines(true)
                .with_system_includes(includes);
            let program = trace_parse::build_program(&root, &opts).unwrap();
            assert_eq!(names(&program), ["selected"], "{name}");
            assert!(
                program.diagnostics.is_empty(),
                "{name}: {:?}",
                program.diagnostics
            );
        }
    }
}

#[test]
fn no_implicit_host_compiler_and_library_option_validation() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    for includes in [false, true] {
        let opts = PreprocessOptions::new()
            .with_compiler_predefines(true)
            .with_system_includes(includes);
        let program = trace_parse::build_program(dir.path(), &opts).unwrap();
        assert!(section_error(&program));
        assert!(names(&program).contains(&"absent_identity".into()));
        assert!(program
            .diagnostics
            .iter()
            .any(|d| d.message.contains("no compiler context")));
    }
    for opts in [
        PreprocessOptions::new().with_compiler("gcc"),
        PreprocessOptions::new().with_compiler_arg("-m32"),
        PreprocessOptions::new()
            .with_compiler_predefines(true)
            .with_compiler("gcc")
            .with_compiler_arg("-DFAKE=1"),
        PreprocessOptions::new()
            .with_compiler_predefines(true)
            .with_compiler("gcc")
            .with_compiler_arg("--target"),
    ] {
        assert!(trace_parse::build_program(dir.path(), &opts).is_err());
    }
}

#[test]
fn per_command_target_language_precedence_and_cache_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("choice.h"), r#"
#if __GNUC__ == 77
void cli_override(void) {}
#elif defined(__aarch64__) && __GNUC__ == 7 && __STDC_VERSION__ == 201112L && !defined(__GNUG__)
void c_environment(void) {}
#elif defined(__x86_64__) && __GNUC__ == 8 && __cplusplus == 202002L && defined(__GNUG__) && !defined(__clang__)
void cpp_environment(void) {}
#else
#error wrong configuration
#endif
"#).unwrap();
    for file in ["one.c", "copy.c", "two.cpp"] {
        std::fs::write(root.join(file), "#include \"choice.h\"\n").unwrap();
    }
    let log = dir.path().join("probes");
    let compiler = driver(
        dir.path(),
        "compiler",
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
printf '#define __GNUC__ 13\n#define __clang__ 1\n#define __cplusplus 1\n'
case "$*" in *'--target=arm'*) printf '#define __aarch64__ 1\n' ;; *) printf '#define __x86_64__ 1\n' ;; esac
case "$*" in *'-x c++'*) printf '#define __GNUG__ 13\n' ;; esac
"#,
            log.display()
        ),
    );
    let mut commands = Vec::new();
    for file in ["one.c", "copy.c"] {
        commands.push(serde_json::json!({"directory": root, "file": file,
            "arguments": ["ccache", "--", "sccache", "--", compiler.to_str().unwrap(), "--target=arm", "-std=c11", "-U__GNUC__", "-D__GNUC__=7", "-c", file]}));
    }
    commands.push(serde_json::json!({"directory": root, "file": "two.cpp",
        "arguments": [compiler.to_str().unwrap(), "--target=x86", "-std=c++20", "-D__GNUC__=8", "-U__clang__", "-c", "two.cpp"]}));
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::to_vec(&commands).unwrap(),
    )
    .unwrap();
    let opts = PreprocessOptions::new()
        .with_compiler_predefines(true)
        .with_compiler(&compiler)
        .with_compiler_arg("--target=fallback");
    let one = trace_parse::build_program_with_jobs(&root, &opts, 1).unwrap();
    let many = trace_parse::build_program_with_jobs(&root, &opts, 4).unwrap();
    assert_eq!(names(&one), names(&many));
    assert!(names(&one).contains(&"c_environment".into()));
    assert!(names(&one).contains(&"cpp_environment".into()));
    assert!(one.diagnostics.is_empty(), "{:?}", one.diagnostics);
    let overridden =
        trace_parse::build_program(&root, &opts.with_define("__GNUC__", "77")).unwrap();
    assert!(names(&overridden).iter().all(|name| name == "cli_override"));
    assert!(
        overridden.diagnostics.is_empty(),
        "{:?}",
        overridden.diagnostics
    );
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), 2, "{probes}");
    assert_eq!(
        probes
            .lines()
            .filter(|line| line.contains("--target=fallback"))
            .count(),
        0
    );
    assert!(!probes.contains("-D__GNUC__") && !probes.contains("-U__clang__"));
}

#[test]
fn compiler_identity_versions_follow_command_flags_and_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let log = dir.path().join("probes");
    let compiler = driver(
        dir.path(),
        "compiler",
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
printf '#define __clang__ 1\n'
case "$*" in
    *-fms-compatibility-version=19.30*) printf '#define _MSC_VER 1930\n' ;;
    *-fms-compatibility-version=19.29*) printf '#define _MSC_VER 1929\n' ;;
    *-fmsc-version=1931*) printf '#define _MSC_VER 1931\n' ;;
    *-fgnuc-version=9.3.1*) printf '#define __GNUC__ 9\n#define __GNUC_MINOR__ 3\n#define __GNUC_PATCHLEVEL__ 1\n#define __GNUG__ 9\n' ;;
    *-fgnuc-version=8.2.0*) printf '#define __GNUC__ 8\n#define __GNUC_MINOR__ 2\n#define __GNUC_PATCHLEVEL__ 0\n#define __GNUG__ 8\n' ;;
esac
"#,
            log.display()
        ),
    );
    let mut compilers = vec![compiler];
    if Path::new("/usr/bin/clang").is_file() {
        compilers.push(PathBuf::from("/usr/bin/clang"));
    }
    let cases = [
        (
            "msvc_1930",
            "x86_64-pc-windows-msvc",
            "-fms-compatibility-version=19.30",
            vec!["EXPECTED_MSC_VER=1930"],
        ),
        (
            "msvc_1930_copy",
            "x86_64-pc-windows-msvc",
            "-fms-compatibility-version=19.30",
            vec!["EXPECTED_MSC_VER=1930"],
        ),
        (
            "msvc_1929",
            "x86_64-pc-windows-msvc",
            "-fms-compatibility-version=19.29",
            vec!["EXPECTED_MSC_VER=1929"],
        ),
        (
            "msvc_1931",
            "x86_64-pc-windows-msvc",
            "-fmsc-version=1931",
            vec!["EXPECTED_MSC_VER=1931"],
        ),
        (
            "gnu_9_3_1",
            "x86_64-unknown-linux-gnu",
            "-fgnuc-version=9.3.1",
            vec![
                "EXPECTED_GNU_MAJOR=9",
                "EXPECTED_GNU_MINOR=3",
                "EXPECTED_GNU_PATCH=1",
            ],
        ),
        (
            "gnu_8_2_0",
            "x86_64-unknown-linux-gnu",
            "-fgnuc-version=8.2.0",
            vec![
                "EXPECTED_GNU_MAJOR=8",
                "EXPECTED_GNU_MINOR=2",
                "EXPECTED_GNU_PATCH=0",
            ],
        ),
    ];
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preproc/compiler_identity_versions/main.cpp");
    let mut expected: Vec<_> = cases.iter().map(|(name, ..)| name.to_string()).collect();
    expected.sort();
    for compiler in compilers {
        let mut commands = Vec::new();
        for (name, target, flag, defines) in &cases {
            let file = format!("{name}.cpp");
            std::fs::copy(&fixture, root.join(&file)).unwrap();
            let mut arguments = vec![
                compiler.to_string_lossy().into_owned(),
                format!("--target={target}"),
                flag.to_string(),
            ];
            arguments.extend(defines.iter().map(|define| format!("-D{define}")));
            arguments.extend([
                format!("-DSELECTED_FUNCTION={name}"),
                "-c".into(),
                file.clone(),
                "-o".into(),
                format!("{name}.o"),
            ]);
            commands
                .push(serde_json::json!({"directory": root, "file": file, "arguments": arguments}));
        }
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::to_vec(&commands).unwrap(),
        )
        .unwrap();
        for jobs in [1, 4] {
            let opts = PreprocessOptions::new().with_compiler_predefines(true);
            let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
            assert_eq!(names(&program), expected, "{compiler:?}, jobs={jobs}");
            assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
        }
    }
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), 5, "{probes}");
    assert!(
        !probes.contains("-D") && !probes.contains(".cpp") && !probes.contains(".o"),
        "{probes}"
    );
}

#[test]
fn fallback_standard_shared_flags_and_exploration_reuse_macro_seed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("main.cpp"),
        r#"
#if __cplusplus == 202002L && defined(__aarch64__) && __GNUC__ == 13
void selected_standard(void) {}
#if defined(FEATURE)
void explored_feature(void) {}
#endif
#else
#error wrong fallback seed
#endif
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("BUILD.gn"),
        "config(\"feature\") { defines = [ \"FEATURE\" ] }\n",
    )
    .unwrap();
    let compiler = driver(
        dir.path(),
        "compiler",
        r#"
case "$*" in *'--target=arm'*) printf '#define __aarch64__ 1\n#define __GNUC__ 13\n#define __cplusplus 1\n' ;; *) exit 4 ;; esac
"#,
    );
    let opts = PreprocessOptions::new()
        .with_compiler_predefines(true)
        .with_compiler(compiler)
        .with_compiler_arg("--target=arm")
        .with_compiler_arg("-std=c++20")
        .with_explore(true);
    for jobs in [1, 4] {
        let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert!(names(&program).contains(&"selected_standard".into()));
        assert!(names(&program).contains(&"explored_feature".into()));
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    }
    // Shared flags also configure the explicit fallback, without a driver of
    // their own. Keep their ordered -D/-U operations above the compiler seed.
    std::fs::write(root.join("compile_flags.txt"), "--target=arm\n-std=c++20\n").unwrap();
    let opts = PreprocessOptions {
        compiler_args: Vec::new(),
        ..opts
    };
    let program = trace_parse::build_program(&root, &opts).unwrap();
    assert!(names(&program).contains(&"explored_feature".into()));
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

#[test]
fn malformed_launcher_command_keeps_warning_and_uses_explicit_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fixture(&root);
    let compiler = driver(dir.path(), "compiler", "printf '#define __GNUC__ 13\\n'");
    let opts = PreprocessOptions::new()
        .with_compiler_predefines(true)
        .with_compiler(compiler);
    for arguments in [
        vec!["ccache"],
        vec!["ccache", "-c", "main.c"],
        vec!["ccache", "--"],
        vec!["ccache", "--", "-c", "main.c"],
        vec!["-c", "main.c"],
        vec!["--target=aarch64-linux-gnu", "-c", "main.c"],
    ] {
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([{
                "directory": root, "file": "main.c", "arguments": arguments
            }])
            .to_string(),
        )
        .unwrap();
        let program = trace_parse::build_program(&root, &opts).unwrap();
        assert!(!section_error(&program));
        assert!(names(&program).contains(&"gnu_identity".into()));
        assert!(program
            .diagnostics
            .iter()
            .any(|d| d.message.contains("missing compiler driver")));
    }
}

#[test]
fn driverless_commands_warn_and_discard_flags_without_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "#ifdef FOO\nvoid foo(void) {}\n#endif\nvoid m(void) {}\n",
    )
    .unwrap();
    for (arguments, expected) in [
        (
            vec!["-c", "-DFOO", "main.c"],
            "missing compiler driver: command starts with an option",
        ),
        (
            vec!["ccache", "--", "-DFOO", "-c", "main.c"],
            "missing compiler driver after launcher",
        ),
    ] {
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([{
                "directory": root, "file": "main.c", "arguments": arguments
            }])
            .to_string(),
        )
        .unwrap();
        let program = trace_parse::build_program(root, &PreprocessOptions::new()).unwrap();
        assert_eq!(names(&program), ["m"]);
        assert_eq!(program.diagnostics.len(), 1, "{:?}", program.diagnostics);
        assert!(program.diagnostics[0].message.contains(expected));
    }
}

#[test]
fn missing_command_driver_warning_recommends_fixing_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("main.c"),
        "#ifdef __GNUC__\nvoid probed(void) {}\n#endif\n#ifdef COMMAND\nvoid configured(void) {}\n#endif\n",
    )
    .unwrap();
    let missing = dir.path().join("missing-compiler");
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!([{
            "directory": root, "file": "main.c",
            "arguments": [missing, "-DCOMMAND", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let compiler = driver(dir.path(), "compiler", "printf '#define __GNUC__ 13\\n'");
    for fallback in [None, Some(compiler)] {
        let opts = PreprocessOptions {
            compiler_predefines: true,
            compiler: fallback,
            ..PreprocessOptions::new()
        };
        let program = trace_parse::build_program(&root, &opts).unwrap();
        assert_eq!(names(&program), ["configured"]);
        assert_eq!(program.diagnostics.len(), 1, "{:?}", program.diagnostics);
        let warning = &program.diagnostics[0].message;
        assert!(
            warning.contains("compiler executable not found"),
            "{warning}"
        );
        assert!(
            warning.contains("fix the compilation command's driver"),
            "{warning}"
        );
        assert!(!warning.contains("--compiler"), "{warning}");
    }
}

#[test]
fn fallback_standards_are_language_specific_for_sources_and_orphan_headers() {
    for headers in [false, true] {
        for flags in [
            vec!["-std=c11", "-std=c++20"],
            vec!["-std=c++20", "-std=c11"],
            vec!["--std=c11", "--std=c++20"],
            vec!["--std", "c++20", "--std", "c11"],
            vec!["-std=c++20"],
            vec!["-std=c11"],
            vec!["-std=c99", "--std=c11", "-std=c++17", "--std=c++20"],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("project");
            std::fs::create_dir(&root).unwrap();
            let log = dir.path().join("probes");
            let compiler = driver(
                dir.path(),
                "compiler",
                &format!(
                    r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *'-x c -'*) case "$*" in *c++*) exit 3 ;; esac ;;
  *'-x c++ -'*) case "$*" in *c11*|*c99*) exit 4 ;; esac ;;
esac
printf '#define __GNUC__ 13\n#define __x86_64__ 1\n'
"#,
                    log.display()
                ),
            );
            if headers {
                std::fs::write(root.join("main.c"), "void configured(void) {}\n").unwrap();
                let command_driver = driver(
                    dir.path(),
                    "command-compiler",
                    "printf '#define __GNUC__ 13\\n'",
                );
                std::fs::write(root.join("compile_commands.json"), serde_json::json!([{
                    "directory": root, "file": "main.c", "arguments": [command_driver, "-c", "main.c"]
                }]).to_string()).unwrap();
            }
            for (extension, name, standard) in [
                (
                    if headers { "h" } else { "c" },
                    "c_selected",
                    "__STDC_VERSION__ == 201112L",
                ),
                (
                    if headers { "hpp" } else { "cpp" },
                    "cpp_selected",
                    "__cplusplus == 202002L",
                ),
            ] {
                let explicit_standard = if name == "c_selected" {
                    flags.iter().any(|arg| arg.contains("c11"))
                } else {
                    flags.iter().any(|arg| arg.contains("c++20"))
                };
                let condition = if explicit_standard {
                    format!(" && {standard} && defined(__STRICT_ANSI__)")
                } else {
                    String::new()
                };
                std::fs::write(root.join(format!("{name}.{extension}")), format!(
                    "#if __GNUC__ == 13 && defined(__x86_64__){condition}\nvoid {name}(void) {{}}\n#else\n#error wrong language standard or missing compiler seed\n#endif\n"
                )).unwrap();
            }
            let mut opts = PreprocessOptions::new()
                .with_compiler_predefines(true)
                .with_compiler(compiler);
            opts.compiler_args = flags.iter().map(|arg| arg.to_string()).collect();
            // Compiler probes replace earlier compiler seeds in both command
            // configurations and fallback configurations.
            opts.compiler_defines.insert("__GNUC__".into(), "0".into());
            for jobs in [1, 4] {
                let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
                assert!(names(&program).contains(&"c_selected".into()), "{flags:?}");
                assert!(
                    names(&program).contains(&"cpp_selected".into()),
                    "{flags:?}"
                );
                assert!(
                    program.diagnostics.is_empty(),
                    "{flags:?}: {:?}",
                    program.diagnostics
                );
            }
            let probes = std::fs::read_to_string(log).unwrap();
            assert_eq!(probes.lines().count(), 2, "{probes}");
        }
    }
}

#[test]
fn missing_fallback_warning_names_only_configurations_actually_used() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fixture(&root);
    std::fs::write(root.join("orphan.h"), "void orphan(void);\n").unwrap();
    let compiler = driver(dir.path(), "compiler", "printf '#define __GNUC__ 13\\n'");
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!([{
            "directory": root, "file": "main.c", "arguments": [compiler, "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let opts = PreprocessOptions::new().with_compiler_predefines(true);
    for missing_source in [false, true] {
        if missing_source {
            std::fs::copy(root.join("main.c"), root.join("two.c")).unwrap();
        }
        let program = trace_parse::build_program(&root, &opts).unwrap();
        let warnings: Vec<_> = program
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("no compiler context"))
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", program.diagnostics);
        assert!(warnings[0].message.contains("language C,"));
        assert_eq!(section_error(&program), missing_source);
    }
}

#[test]
fn rejected_shared_flags_do_not_override_explicit_fallback_target() {
    for rejected in ["@args.rsp", "-DA+B=1"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("main.c"),
            r#"
#if defined(__x86_64__) && !defined(__aarch64__)
void explicit_x86_target(void) {}
#else
void rejected_arm_target(void) {}
#endif
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("compile_flags.txt"),
            format!("--target=aarch64-linux-gnu\n{rejected}\n"),
        )
        .unwrap();
        let log = dir.path().join("probes");
        let compiler = driver(
            dir.path(),
            "compiler",
            &format!(
                r#"
printf '%s\n' "$*" >> '{}'
target=
for arg; do case "$arg" in --target=*) target=$arg ;; esac; done
printf '#define __GNUC__ 13\n'
case "$target" in *aarch64*) printf '#define __aarch64__ 1\n' ;; *) printf '#define __x86_64__ 1\n' ;; esac
"#,
                log.display()
            ),
        );
        let opts = PreprocessOptions::new()
            .with_compiler_predefines(true)
            .with_compiler(compiler)
            .with_compiler_arg("--target=x86_64-linux-gnu");
        for jobs in [1, 4] {
            let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
            assert_eq!(names(&program), ["explicit_x86_target"], "{rejected}");
            assert!(program
                .diagnostics
                .iter()
                .any(|d| d.message.contains("using inferred configuration")));
        }
        let probes = std::fs::read_to_string(log).unwrap();
        assert!(probes.contains("--target=x86_64-linux-gnu"));
        assert!(!probes.contains("--target=aarch64-linux-gnu"), "{probes}");
    }
}

#[test]
fn database_only_c_header_unit_selects_c_fallback_for_orphan_headers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.h"), "void database_unit(void) {}\n").unwrap();
    std::fs::write(
        root.join("orphan.h"),
        r#"
#if defined(__GNUC__) && defined(__x86_64__) && !defined(__cplusplus)
void orphan_c_target(void) {}
#else
void orphan_missing_c_target(void) {}
#endif
"#,
    )
    .unwrap();
    let log = dir.path().join("fallback-probes");
    let fallback = driver(
        dir.path(),
        "fallback",
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
printf '#define __GNUC__ 13\n'
case "$*" in *'-x c++'*) printf '#define __aarch64__ 1\n' ;; *) printf '#define __x86_64__ 1\n' ;; esac
"#,
            log.display()
        ),
    );
    let command_driver = driver(
        dir.path(),
        "command-compiler",
        "printf '#define __GNUC__ 13\\n'",
    );
    std::fs::write(root.join("compile_commands.json"), serde_json::json!([{
        "directory": root, "file": "main.h", "arguments": [command_driver, "-x", "c-header", "-c", "main.h"]
    }]).to_string()).unwrap();
    let opts = PreprocessOptions::new()
        .with_compiler_predefines(true)
        .with_compiler(fallback);
    for jobs in [1, 4] {
        let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert_eq!(names(&program), ["database_unit", "orphan_c_target"]);
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    }
    let probes = std::fs::read_to_string(log).unwrap();
    assert!(probes.contains("-x c -"), "{probes}");
    assert!(!probes.contains("-x c++ -"), "{probes}");
}

#[test]
fn shared_probe_flags_follow_language_validation_and_exclude_linker_tail() {
    for flags in [
        "--target=aarch64-linux-gnu\n-x\nnone\n-std=c11\n",
        "--target=aarch64-linux-gnu\n/link\n--target=x86_64-linux-gnu\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("one.c"),
            r#"
#ifdef __aarch64__
void c_arm_target(void) {}
#else
void c_wrong_target(void) {}
#endif
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("two.cpp"),
            r#"
#if defined(__x86_64__)
void cpp_x86_target(void) {}
#elif defined(__aarch64__)
void cpp_arm_target(void) {}
#endif
"#,
        )
        .unwrap();
        std::fs::write(root.join("compile_flags.txt"), flags).unwrap();
        let compiler = driver(
            dir.path(),
            "compiler",
            r#"
target=
for arg; do case "$arg" in --target=*) target=$arg ;; esac; done
printf '#define __GNUC__ 13\n'
case "$target" in *aarch64*) printf '#define __aarch64__ 1\n' ;; *) printf '#define __x86_64__ 1\n' ;; esac
"#,
        );
        let opts = PreprocessOptions::new()
            .with_compiler_predefines(true)
            .with_compiler(compiler)
            .with_compiler_arg("--target=x86_64-linux-gnu");
        for jobs in [1, 4] {
            let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
            let cpp = if flags.contains("-std=c11") {
                "cpp_x86_target"
            } else {
                "cpp_arm_target"
            };
            assert_eq!(names(&program), ["c_arm_target", cpp], "{flags}");
            if flags.contains("-std=c11") {
                assert_eq!(program.diagnostics.len(), 1, "{:?}", program.diagnostics);
                assert!(program.diagnostics[0]
                    .message
                    .contains("using inferred configuration"));
            } else {
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
            }
        }
    }
}

#[test]
fn command_undef_suppresses_identity_without_leaking_between_units() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let log = dir.path().join("probes");
    let compiler = driver(
        dir.path(),
        "compiler",
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case " $* " in
  *' -undef '*) printf '#define __STDC__ 1\n' ;;
  *) printf '#define __GNUC__ 13\n#define __clang__ 1\n' ;;
esac
"#,
            log.display()
        ),
    );
    let mut commands = Vec::new();
    for (name, flags) in [
        ("normal", vec![]),
        ("suppressed", vec!["-undef"]),
        ("restored", vec!["-undef", "-D__GNUC__=99"]),
    ] {
        let file = format!("{name}.c");
        std::fs::write(root.join(&file), format!(
            "#if defined(__GNUC__) || defined(__clang__)\nvoid {name}_identity(void) {{}}\n#else\nvoid {name}_no_identity(void) {{}}\n#endif\n"
        )).unwrap();
        let arguments: Vec<_> = std::iter::once(compiler.to_string_lossy().into_owned())
            .chain(flags.into_iter().map(str::to_string))
            .chain(["-c".into(), file.clone()])
            .collect();
        commands.push(serde_json::json!({"directory": root, "file": file, "arguments": arguments}));
    }
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::to_vec(&commands).unwrap(),
    )
    .unwrap();
    let opts = PreprocessOptions::new().with_compiler_predefines(true);
    for jobs in [1, 4] {
        let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert_eq!(
            names(&program),
            [
                "normal_identity",
                "restored_identity",
                "suppressed_no_identity"
            ]
        );
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    }
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), 2, "{probes}");
    assert_eq!(
        probes
            .lines()
            .filter(|line| line.contains("-undef"))
            .count(),
        1
    );
}

#[test]
fn header_only_shared_language_selects_the_matching_fallback_probe() {
    for (selector, language, expected) in [
        ("c", "c", "c_compiler_header"),
        ("c-header", "c", "c_compiler_header"),
        ("c++", "c++", "cpp_compiler_header"),
        ("c++-header", "c++", "cpp_compiler_header"),
    ] {
        for header in ["main.h", "main.hpp"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("project");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(
                root.join(header),
                r#"
#if defined(__GNUC__) && defined(__x86_64__) && !defined(__cplusplus)
void c_compiler_header(void) {}
#elif defined(__GNUC__) && defined(__aarch64__) && defined(__cplusplus)
void cpp_compiler_header(void) {}
#else
void missing_compiler_header(void) {}
#endif
"#,
            )
            .unwrap();
            std::fs::write(root.join("compile_flags.txt"), format!("-x\n{selector}\n")).unwrap();
            let log = dir.path().join("probes");
            let compiler = driver(
                dir.path(),
                "compiler",
                &format!(
                    r#"
printf '%s\n' "$*" >> '{}'
printf '#define __GNUC__ 13\n'
case "$*" in *'-x c++'*) printf '#define __aarch64__ 1\n' ;; *) printf '#define __x86_64__ 1\n' ;; esac
"#,
                    log.display()
                ),
            );
            let opts = PreprocessOptions::new()
                .with_compiler_predefines(true)
                .with_compiler(compiler);
            for jobs in [1, 4] {
                let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
                assert_eq!(names(&program), [expected], "{header}: -x {selector}");
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
            }
            let probes = std::fs::read_to_string(log).unwrap();
            assert_eq!(probes.lines().count(), 1, "{probes}");
            assert!(
                probes.trim_end().ends_with(&format!("-x {language} -")),
                "{probes}"
            );
        }
    }
}

#[test]
fn accepted_shared_flags_override_fallback_target_and_standard() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("main.cpp"),
        r#"
#if defined(__aarch64__) && !defined(__x86_64__) && __cplusplus == 202002L
void shared_configuration(void) {}
#else
#error wrong shared target or standard
#endif
"#,
    )
    .unwrap();
    std::fs::write(root.join("compile_flags.txt"), "--target=arm\n-std=c++20\n").unwrap();
    let compiler = driver(
        dir.path(),
        "compiler",
        r#"
target=
standard=
for arg; do
    case "$arg" in --target=*) target=$arg ;; -std=*) standard=$arg ;; esac
done
[ "$target" = --target=arm ] && [ "$standard" = -std=c++20 ] || exit 4
printf '#define __GNUC__ 13\n#define __aarch64__ 1\n'
"#,
    );
    let opts = PreprocessOptions::new()
        .with_compiler_predefines(true)
        .with_compiler(compiler)
        .with_compiler_arg("--target=x86")
        .with_compiler_arg("-std=c++17");
    let program = trace_parse::build_program(&root, &opts).unwrap();
    assert_eq!(names(&program), ["shared_configuration"]);
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

#[test]
fn cli_standard_override_survives_warming_nested_headers() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preproc/compiler_standard_override");
    let compiler = driver(dir.path(), "compiler", "printf '#define __GNUC__ 13\\n'");
    let mut compilers = vec![compiler];
    // Always exercise the fixture driver; also cover the reported native
    // clang reproduction when that executable is installed.
    if Path::new("/usr/bin/clang").is_file() {
        compilers.push(PathBuf::from("/usr/bin/clang"));
    }
    for (source, standard, macro_name, value) in [
        ("main.cpp", "c++20", "__cplusplus", "199711L"),
        ("main.c", "c11", "__STDC_VERSION__", "199901L"),
        ("main.c", "c89", "__STDC_VERSION__", "199901L"),
        ("main.cpp", "c++20", "__STRICT_ANSI__", "0"),
        ("main.cpp", "gnu++20", "__STRICT_ANSI__", "1"),
    ] {
        let root = dir.path().join(format!("{standard}-{macro_name}"));
        std::fs::create_dir(&root).unwrap();
        std::fs::copy(fixture.join("main.cpp"), root.join(source)).unwrap();
        for file in ["outer.h", "nested.h"] {
            std::fs::copy(fixture.join(file), root.join(file)).unwrap();
        }
        for compiler in &compilers {
            let opts = PreprocessOptions::new()
                .with_compiler_predefines(true)
                .with_compiler(compiler)
                .with_compiler_arg(format!("-std={standard}"))
                .with_define(macro_name, value)
                .with_define("TEST_STANDARD", macro_name)
                .with_define("TEST_EXPECTED_STANDARD", value);
            for jobs in [1, 4] {
                let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
                // A nested expansion captured with the fallback standard must
                // not introduce definitions excluded by the CLI override.
                assert_eq!(
                    names(&program),
                    ["header_cli_standard", "tu_cli_standard"],
                    "{compiler:?}, {standard}, {macro_name}={value}, jobs={jobs}"
                );
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
            }
        }
    }
}

#[test]
fn fallback_macro_probes_wait_for_actual_orphans_and_their_language() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("main.c"),
        "#define HEADER \"included.h\"\n#include HEADER\n",
    )
    .unwrap();
    std::fs::write(root.join("included.h"), "void included(void) {}\n").unwrap();
    let command = driver(dir.path(), "command", "printf '#define __GNUC__ 13\\n'");
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!([{
            "directory": root, "file": "main.c", "arguments": [command, "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let log = dir.path().join("fallback-probes");
    let fallback = driver(
        dir.path(),
        "fallback",
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\nprintf '#define __GNUC__ 13\\n'",
            log.display()
        ),
    );
    let opts = PreprocessOptions::new()
        .with_compiler_predefines(true)
        .with_compiler(fallback);
    for jobs in [1, 4] {
        let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert_eq!(names(&program), ["included"]);
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
        assert!(
            !log.exists(),
            "consumed headers must not trigger fallback probes"
        );
    }
    std::fs::write(
        root.join("orphan.hpp"),
        "#ifdef __GNUC__\nvoid orphan(void) {}\n#endif\n",
    )
    .unwrap();
    for jobs in [1, 4] {
        let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
        assert_eq!(names(&program), ["included", "orphan"]);
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    }
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), 1, "{probes}");
    assert!(probes.trim_end().ends_with("-x c++ -"), "{probes}");

    // An empty database does not route a header-only tree through configured
    // source indexing; its headers still need the ordinary fallback seed.
    let headers_only = dir.path().join("headers-only");
    std::fs::create_dir(&headers_only).unwrap();
    std::fs::copy(root.join("orphan.hpp"), headers_only.join("orphan.hpp")).unwrap();
    std::fs::write(headers_only.join("compile_commands.json"), "[]").unwrap();
    let program = trace_parse::build_program(&headers_only, &opts).unwrap();
    assert_eq!(names(&program), ["orphan"]);
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

#[test]
fn unsupported_macro_drivers_keep_their_include_only_discovery() {
    for name in ["cl", "iccarm"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let system = dir.path().join("system");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&system).unwrap();
        std::fs::write(system.join("probe.h"), "void system_header(void);\n").unwrap();
        std::fs::write(
            root.join("main.c"),
            r#"
#include <probe.h>
#if defined(__GNUC__) || defined(__x86_64__)
#error unsupported macro driver must not supply a target or identity
#endif
void entry(void) {}
"#,
        )
        .unwrap();
        let compiler = driver(dir.path(), name, &format!(
            "printf '#define __GNUC__ 13\\n#define __x86_64__ 1\\n'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2", system.display()));
        std::fs::write(
            root.join("compile_commands.json"),
            serde_json::json!([{
                "directory": root, "file": "main.c", "arguments": [compiler, "-c", "main.c"]
            }])
            .to_string(),
        )
        .unwrap();
        for includes in [false, true] {
            let opts = PreprocessOptions::new()
                .with_compiler_predefines(true)
                .with_system_includes(includes);
            let program = trace_parse::build_program(&root, &opts).unwrap();
            assert_eq!(names(&program), ["entry"]);
            assert_eq!(
                program
                    .symbols
                    .functions
                    .iter()
                    .any(|f| f.name == "system_header" && !f.is_defined),
                includes
            );
            assert_eq!(
                program
                    .diagnostics
                    .iter()
                    .filter(|d| d.message.contains("unsupported macro probe"))
                    .count(),
                1
            );
            assert!(
                !program
                    .diagnostics
                    .iter()
                    .any(|d| d.message.contains("must not supply")),
                "{:?}",
                program.diagnostics
            );
            if includes {
                assert!(
                    !program
                        .diagnostics
                        .iter()
                        .any(|d| d.message.contains("include file not found")),
                    "{:?}",
                    program.diagnostics
                );
                assert!(program
                    .symbols
                    .files
                    .iter()
                    .any(|f| f.path.ends_with("probe.h") && f.is_dep));
            }
        }
    }
}

#[test]
fn configured_source_languages_do_not_trigger_fallback_probes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.c"), "void configured(void) {}\n").unwrap();
    std::fs::write(root.join("orphan.hpp"), "#if __GNUC__ == 13 && __cplusplus == 202002L\nvoid orphan(void) {}\n#else\n#error missing fallback macros\n#endif\n").unwrap();
    let command_driver = driver(
        dir.path(),
        "command-compiler",
        "printf '#define __GNUC__ 13\\n'",
    );
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::json!([{
            "directory": root, "file": "main.c", "arguments": [command_driver, "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let log = dir.path().join("probes");
    let fallback = driver(
        dir.path(),
        "fallback",
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'\nprintf '#define __GNUC__ 13\\n'\n",
            log.display()
        ),
    );
    for includes in [false, true] {
        let opts = PreprocessOptions::new()
            .with_compiler_predefines(true)
            .with_system_includes(includes)
            .with_compiler(&fallback)
            .with_compiler_arg("-std=c++20");
        let program = trace_parse::build_program(&root, &opts).unwrap();
        assert_eq!(names(&program), ["configured", "orphan"]);
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    }
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), 2, "{probes}");
    assert!(
        probes.lines().all(|line| line.ends_with("-x c++ -")),
        "{probes}"
    );
}

#[test]
fn compatibility_toggles_preserve_identities_and_probe_cache_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preproc/compiler_compatibility/main.cpp");
    std::fs::copy(fixture, root.join("main.cpp")).unwrap();
    let log = dir.path().join("probes");
    let fake = driver(
        dir.path(),
        "compiler",
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
gnu=1
for arg; do
    case "$arg" in
        -fms-compatibility) gnu=0 ;;
        -fno-ms-compatibility) gnu=1 ;;
    esac
done
printf '#define __clang__ 1\n'
if [ "$gnu" = 1 ]; then printf '#define __GNUC__ 4\n#define __GNUG__ 4\n'; fi
"#,
            log.display()
        ),
    );
    let mut compilers = vec![fake];
    if Path::new("/usr/bin/clang").is_file() {
        compilers.push(PathBuf::from("/usr/bin/clang"));
    }
    let cases = [
        (vec![], "gnu_compatible"),
        (vec!["-fms-compatibility"], "ms_compatible"),
        (
            vec!["-fms-compatibility", "-fno-ms-compatibility"],
            "gnu_compatible",
        ),
        (
            vec!["-fno-ms-compatibility", "-fms-compatibility"],
            "ms_compatible",
        ),
        (
            vec!["-fms-extensions", "-fno-ms-extensions"],
            "gnu_compatible",
        ),
        (
            vec!["-fno-ms-extensions", "-fms-extensions"],
            "gnu_compatible",
        ),
    ];
    for compiler in compilers {
        for fallback in [false, true] {
            for (flags, expected) in &cases {
                let mut opts = PreprocessOptions::new().with_compiler_predefines(true);
                let commands = if fallback {
                    opts = opts.with_compiler(&compiler);
                    opts.compiler_args = flags.iter().map(|arg| arg.to_string()).collect();
                    serde_json::json!([])
                } else {
                    let mut arguments = vec![compiler.to_string_lossy().into_owned()];
                    arguments.extend(flags.iter().map(|arg| arg.to_string()));
                    arguments.extend(["-c".into(), "main.cpp".into()]);
                    serde_json::json!([{"directory": root, "file": "main.cpp", "arguments": arguments}])
                };
                std::fs::write(root.join("compile_commands.json"), commands.to_string()).unwrap();
                for jobs in [1, 4] {
                    let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
                    assert_eq!(
                        names(&program),
                        [*expected],
                        "{compiler:?}, fallback={fallback}, {flags:?}"
                    );
                    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
                }
            }
        }
    }
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), cases.len(), "{probes}");
    for (flags, _) in &cases {
        let mut args = flags.clone();
        args.extend(["-E", "-dM", "-x", "c++", "-"]);
        assert!(
            probes.lines().any(|line| line == args.join(" ")),
            "{flags:?}: {probes}"
        );
    }
}

#[test]
fn include_only_fallback_standards_apply_to_units_and_warmed_headers() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preproc/compiler_standard_override");
    let system = dir.path().join("system");
    std::fs::create_dir(&system).unwrap();
    let fake = driver(dir.path(), "compiler", &format!(
        "printf '#define __GNUC__ 13\\n'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2", system.display()
    ));
    let mut compilers = vec![fake];
    if Path::new("/usr/bin/clang").is_file() {
        compilers.push(PathBuf::from("/usr/bin/clang"));
    }
    for (source, standard, macro_name, value) in [
        ("main.cpp", "c++20", "__cplusplus", "202002L"),
        ("main.c", "c11", "__STDC_VERSION__", "201112L"),
        ("main.cpp", "c++20", "__STRICT_ANSI__", "1"),
        ("main.cpp", "gnu++20", "__STRICT_ANSI__", "0"),
    ] {
        let root = dir.path().join(format!("{standard}-{macro_name}"));
        std::fs::create_dir(&root).unwrap();
        std::fs::copy(fixture.join("main.cpp"), root.join(source)).unwrap();
        for file in ["outer.h", "nested.h"] {
            std::fs::copy(fixture.join(file), root.join(file)).unwrap();
        }
        for compiler in &compilers {
            let opts = PreprocessOptions::new()
                .with_system_includes(true)
                .with_compiler(compiler)
                .with_compiler_arg(format!("-std={standard}"))
                .with_define("TEST_STANDARD", macro_name)
                .with_define("TEST_EXPECTED_STANDARD", value);
            for jobs in [1, 4] {
                let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
                assert_eq!(
                    names(&program),
                    ["header_cli_standard", "tu_cli_standard"],
                    "{compiler:?}, {standard}, {macro_name}, jobs={jobs}"
                );
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
            }
        }
    }
}

#[test]
fn split_fallback_standards_probe_like_joined_standards() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/preproc/compiler_standard_forms/main.cpp");
    let log = dir.path().join("probes");
    let fake = driver(
        dir.path(),
        "compiler",
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
for arg; do case "$arg" in -std|--std) exit 4 ;; esac; done
printf '#define __GNUC__ 13\n#define __clang__ 1\n'
"#,
            log.display()
        ),
    );
    let mut compilers = vec![fake];
    if Path::new("/usr/bin/clang").is_file() {
        compilers.push(PathBuf::from("/usr/bin/clang"));
    }
    for (source, standard, macro_name, value) in [
        ("main.cpp", "c++20", "__cplusplus", "202002L"),
        ("main.c", "c11", "__STDC_VERSION__", "201112L"),
    ] {
        let root = dir.path().join(standard);
        std::fs::create_dir(&root).unwrap();
        std::fs::copy(&fixture, root.join(source)).unwrap();
        for compiler in &compilers {
            for flags in [
                vec![format!("-std={standard}")],
                vec!["-std".into(), standard.into()],
                vec!["--std".into(), standard.into()],
            ] {
                let mut opts = PreprocessOptions::new()
                    .with_compiler_predefines(true)
                    .with_compiler(compiler)
                    .with_define("TEST_STANDARD", macro_name)
                    .with_define("TEST_EXPECTED_STANDARD", value);
                opts.compiler_args = flags;
                for jobs in [1, 4] {
                    let program = trace_parse::build_program_with_jobs(&root, &opts, jobs).unwrap();
                    assert_eq!(
                        names(&program),
                        ["selected_standard"],
                        "{compiler:?}, {:?}, jobs={jobs}",
                        opts.compiler_args
                    );
                    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
                }
            }
        }
    }
    // Equivalent standard spellings share the same normalized probe key.
    let probes = std::fs::read_to_string(log).unwrap();
    assert_eq!(probes.lines().count(), 2, "{probes}");
    assert!(
        probes.lines().any(|line| line.starts_with("-std=c++20 ")),
        "{probes}"
    );
    assert!(
        probes.lines().any(|line| line.starts_with("-std=c11 ")),
        "{probes}"
    );
}
