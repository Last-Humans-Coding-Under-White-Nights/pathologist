mod common;

use common::*;
use serde_json::json;
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

#[test]
fn explicit_commands_preserve_header_variants_and_call_flows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        include_str!("../../../tests/fixtures/compile_commands/main.c"),
    )
    .unwrap();
    std::fs::write(
        root.join("config.h"),
        include_str!("../../../tests/fixtures/compile_commands/config.h"),
    )
    .unwrap();
    std::fs::write(root.join("fallback.c"), "void fallback(void) {}\n").unwrap();
    std::fs::write(root.join("compile_commands.json"), json!([
        {"directory": root, "file": "main.c", "arguments": ["cc", "-DMODE=9", "-UMODE", "-DMODE=1", "-include", "config.h", "-c", "main.c"]},
        {"directory": root, "file": "main.c", "command": "cc -DMODE=2 -include config.h -c main.c"}
    ]).to_string()).unwrap();
    for jobs in [1, 4] {
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
        let (_, analysis) = trace_analysis::analyze(&program);
        assert!(has_any_edge(&program, &analysis, "entry", "alpha"));
        assert!(has_any_edge(&program, &analysis, "entry", "beta"));
        assert!(has_edge(
            &program,
            &analysis,
            "entry",
            "header_entry",
            trace_analysis::ResolutionKind::Direct
        ));
        assert!(has_any_edge(&program, &analysis, "header_entry", "alpha"));
        assert!(has_any_edge(&program, &analysis, "header_entry", "beta"));
        assert!(program.symbols.resolve_function("fallback").is_some());
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    }
}

#[test]
fn search_classes_working_directory_language_and_ordered_forced_includes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for subdir in ["build", "src", "inc one", "inc2", "quote1", "quote2", "sys"] {
        std::fs::create_dir(root.join(subdir)).unwrap();
    }
    for (path, text) in [
        ("src/choice.h", "#define CHOICE 9\n"),
        ("quote1/choice.h", "#define CHOICE 8\n"),
        ("sys/choice.h", "#define CHOICE 7\n"),
        ("inc2/choice.h", "#define CHOICE 2\n"),
        ("inc one/choice.h", "#define CHOICE 1\n"),
        ("quote1/quoted.h", "#define QUOTED 1\n"),
        ("quote2/quoted.h", "#define QUOTED 2\n"),
        ("build/first.h", "#define FIRST 1\n"),
        (
            "build/second.h",
            "#if FIRST\nvoid forced_origin(void) {}\n#define SECOND 2\n#endif\n",
        ),
        (
            "src/main.c",
            r#"
#define HEADER <choice.h>
#include HEADER
#include "quoted.h"
#if CHOICE == 1 && QUOTED == 2 && SECOND == 2 && ADD(1) == 2 && !defined(__STDC__) && __cplusplus == 202002L && defined(__STRICT_ANSI__)
namespace Config { void selected() { forced_origin(); } }
#else
void wrong_configuration(void) {}
#endif
"#,
        ),
    ] {
        std::fs::write(root.join(path), text).unwrap();
    }
    std::fs::write(root.join("compile_commands.json"), json!([{
        "directory": "build", "file": "../src/main.c",
        "command": "cc -isystem ../sys -I'../inc one' -I../inc2 -iquote../quote2 -iquote ../quote1 -include first.h -include second.h '-DADD(x)=((x)+1)' -U__STDC__ -x c++ -std=c++20 -c ../src/main.c"
    }]).to_string()).unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 2).unwrap();
    assert!(program
        .symbols
        .resolve_function("Config::selected")
        .is_some());
    assert!(program
        .symbols
        .resolve_function("wrong_configuration")
        .is_none());
    let f = program
        .symbols
        .function(program.symbols.resolve_function("forced_origin").unwrap());
    assert_eq!(f.span.line, 2);
    assert!(program.symbols.files[f.span.file.0 as usize]
        .path
        .ends_with("build/second.h"));
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(has_any_edge(
        &program,
        &analysis,
        "Config::selected",
        "forced_origin"
    ));
}

#[test]
fn commands_for_different_sources_keep_shared_header_variants() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("config.h"),
        include_str!("../../../tests/fixtures/compile_commands/config.h"),
    )
    .unwrap();
    for name in ["a.c", "b.c"] {
        std::fs::write(root.join(name), "#include \"config.h\"\n").unwrap();
    }
    std::fs::write(
        root.join("compile_commands.json"),
        json!([
            {"directory": root, "file": "a.c", "arguments": ["cc", "-DMODE=1", "a.c"]},
            {"directory": root, "file": "b.c", "arguments": ["cc", "-DMODE=2", "b.c"]}
        ])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 4).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(has_any_edge(&program, &analysis, "header_entry", "alpha"));
    assert!(has_any_edge(&program, &analysis, "header_entry", "beta"));
    assert_eq!(program.variants_merged, 0);
}

#[test]
fn malformed_database_and_bad_entries_fall_back_without_losing_valid_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "#ifdef ENABLE\nvoid enabled(void) {}\n#else\nvoid fallback(void) {}\n#endif\n",
    )
    .unwrap();
    for contents in [
        "{bad json".to_string(),
        json!([{"file":"main.c"}]).to_string(),
    ] {
        std::fs::write(root.join("compile_commands.json"), contents).unwrap();
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
        assert!(program.symbols.resolve_function("fallback").is_some());
        assert!(program
            .diagnostics
            .iter()
            .any(|d| d.stage == "compile_commands"));
    }
    std::fs::write(root.join("compile_commands.json"), json!([
        {"file":"main.c"},
        {"directory":root, "file":"main.c", "arguments":["cc", "-DENABLE", "main.c"], "command":"invalid ' quoting"}
    ]).to_string()).unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program.symbols.resolve_function("enabled").is_some());
    assert!(program.symbols.resolve_function("fallback").is_none());
}

#[test]
fn explicit_language_discovers_nonstandard_extension_and_user_defines_override_database() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("source.input"),
        "#if VALUE == 3 && !defined(__STDC_VERSION__)\nvoid selected(void) {}\n#endif\n",
    )
    .unwrap();
    std::fs::write(root.join("compile_commands.json"), json!([{
        "directory":root, "file":"source.input", "arguments":["cc", "-x", "c", "-std=c90", "-DVALUE=1", "-UVALUE", "source.input"]
    }]).to_string()).unwrap();
    let program =
        build_program_with_jobs(root, &PreprocessOptions::new().with_define("VALUE", "3"), 1)
            .unwrap();
    assert!(program.symbols.resolve_function("selected").is_some());
}

#[test]
fn explicit_configurations_do_not_consume_exploration_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.c"), "#if MODE == 1\nvoid one(void) {}\n#else\nvoid two(void) {}\n#endif\n#ifdef EXTRA\nvoid extra(void) {}\n#endif\n").unwrap();
    std::fs::write(root.join("BUILD.gn"), "defines = [ \"EXTRA\" ]\n").unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([
            {"directory":root, "file":"main.c", "arguments":["cc", "-DMODE=1", "main.c"]},
            {"directory":root, "file":"main.c", "arguments":["cc", "-DMODE=2", "main.c"]}
        ])
        .to_string(),
    )
    .unwrap();
    for budget in [0, 1] {
        let opts = PreprocessOptions::new()
            .with_explore(true)
            .with_explore_budget(budget);
        let program = build_program_with_jobs(root, &opts, 2).unwrap();
        assert!(program.symbols.resolve_function("one").is_some());
        assert!(program.symbols.resolve_function("two").is_some());
        assert_eq!(
            program.symbols.resolve_function("extra").is_some(),
            budget > 0
        );
    }
}

#[test]
fn discovers_database_in_build_directory_and_cpp_driver_language() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("build")).unwrap();
    std::fs::write(
        root.join("main.c"),
        "#if __cplusplus == 201103L\nnamespace Driver { void selected() {} }\n#endif\n",
    )
    .unwrap();
    std::fs::write(root.join("build/compile_commands.json"), json!([{
        "directory": root, "file":"main.c", "arguments":["clang++", "-std=c++11", "main.c", "-x", "c"]
    }]).to_string()).unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program
        .symbols
        .resolve_function("Driver::selected")
        .is_some());
}

#[test]
fn cli_accepts_an_external_database_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("main.c"),
        "#ifdef ENABLE\nvoid enabled(void) {}\n#endif\n",
    )
    .unwrap();
    let database = dir.path().join("commands.json");
    std::fs::write(
        &database,
        json!([{
            "directory":root, "file":"main.c", "arguments":["cc", "-DENABLE", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let output = dir.path().join("result.db");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(&root)
        .arg("--compile-commands")
        .arg(database)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let conn = rusqlite::Connection::open(output).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM functions WHERE name='enabled'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn database_preserves_cpp_inference_for_orphan_headers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.cpp"), "void main_cpp() {}\n").unwrap();
    std::fs::write(
        root.join("orphan.h"),
        "namespace Orphan { inline void retained() {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("orphan2.h"),
        "namespace Orphan2 { inline void retained2() {} }\n",
    )
    .unwrap();
    for database in [false, true] {
        if database {
            std::fs::write(
                root.join("compile_commands.json"),
                json!([{
                    "directory":root, "file":"main.cpp", "arguments":["c++", "main.cpp"]
                }])
                .to_string(),
            )
            .unwrap();
        }
        for jobs in [1, 4] {
            let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
            assert!(program
                .symbols
                .resolve_function("Orphan::retained")
                .is_some());
            assert!(program
                .symbols
                .resolve_function("Orphan2::retained2")
                .is_some());
        }
    }
}

#[test]
fn compiler_launcher_space_separated_std_and_xclang_operands() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        r#"
#if __cplusplus == 201703L && defined(__STRICT_ANSI__)
namespace Launcher { void selected() {} }
#endif
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": [
                "ccache",
                "clang",
                "-Xclang",
                "-fcolor-diagnostics",
                "-std",
                "c++17",
                "-c",
                "main.c"
            ]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program
        .symbols
        .resolve_function("Launcher::selected")
        .is_some());
}

#[test]
fn windows_launcher_exe_and_double_dash_spaced_std() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        r#"
#if __cplusplus == 201703L && defined(__STRICT_ANSI__)
namespace WindowsLauncher { void selected() {} }
#endif
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": [
                r"C:\tools\ccache.exe",
                "distcc.EXE",
                r"C:\LLVM\bin\clang++.exe",
                "--std",
                "c++17",
                "-c",
                "main.c"
            ]
        }])
        .to_string(),
    )
    .unwrap();
    for jobs in [1, 4] {
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
        assert!(program
            .symbols
            .resolve_function("WindowsLauncher::selected")
            .is_some());
    }
}

#[test]
fn after_chain_include_flags_follow_gcc_search_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for subdir in ["sys", "after", "pre", "pre/before"] {
        std::fs::create_dir_all(root.join(subdir)).unwrap();
    }
    for (path, text) in [
        // `-isystem` outranks `-idirafter` regardless of argument order.
        ("sys/pick.h", "#define PICK 1\n"),
        ("after/pick.h", "#define PICK 2\n"),
        // Reachable only through `-idirafter`.
        ("after/only.h", "#define ONLY 7\n"),
        // `-iwithprefixbefore` joins the `-I` chain, ahead of the after chain.
        ("pre/before/dup.h", "#define DUP 1\n"),
        ("after/dup.h", "#define DUP 2\n"),
        (
            "main.c",
            r#"
#include <pick.h>
#include <only.h>
#include <dup.h>
#if PICK == 1 && ONLY == 7 && DUP == 1
void selected(void) {}
#else
void wrong_configuration(void) {}
#endif
"#,
        ),
    ] {
        std::fs::write(root.join(path), text).unwrap();
    }
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root, "file": "main.c",
            "command": "cc -idirafter after -isystem sys -iprefix pre/ -iwithprefixbefore before -c main.c"
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 2).unwrap();
    assert!(
        program.symbols.resolve_function("selected").is_some(),
        "{:?}",
        program.diagnostics
    );
    assert!(program
        .symbols
        .resolve_function("wrong_configuration")
        .is_none());
}

#[test]
fn cli_rejects_a_missing_explicit_database_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.c"), "void entry(void) {}\n").unwrap();
    let output = dir.path().join("result.db");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(&root)
        .arg("--compile-commands")
        .arg(dir.path().join("absent.json"))
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap();
    // A mistyped flag must not yield a plausible index built from the
    // inferred configuration it was meant to replace.
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("compilation database does not exist"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!output.exists());
}

#[test]
fn compiler_launchers_do_not_hide_the_driver_language() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A `.c` extension with a C++ driver behind two launchers: the language
    // must come from the driver, not from `sccache`.
    std::fs::write(
        root.join("main.c"),
        "namespace Launcher { void selected() {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root, "file": "main.c",
            "arguments": ["sccache", "distcc", "clang++", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 2).unwrap();
    assert!(
        program
            .symbols
            .resolve_function("Launcher::selected")
            .is_some(),
        "{:?}",
        program.diagnostics
    );
}

#[test]
fn exploration_budget_is_per_source_not_per_command() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "#if MODE == 1\nvoid one(void) {}\n#else\nvoid two(void) {}\n#endif\n\
         #ifdef EXTRA1\nvoid extra1(void) {}\n#endif\n\
         #ifdef EXTRA2\nvoid extra2(void) {}\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        root.join("BUILD.gn"),
        "defines = [ \"EXTRA1\", \"EXTRA2\" ]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([
            {"directory":root, "file":"main.c", "arguments":["cc", "-DMODE=1", "main.c"]},
            {"directory":root, "file":"main.c", "arguments":["cc", "-DMODE=2", "main.c"]}
        ])
        .to_string(),
    )
    .unwrap();
    let opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(1);
    let program = build_program_with_jobs(root, &opts, 1).unwrap();
    // Both explicit commands still land, and they do not consume the budget.
    assert!(program.symbols.resolve_function("one").is_some());
    assert!(program.symbols.resolve_function("two").is_some());
    // One source, two commands: the budget buys one exploratory variant in
    // total, not one per command. `variants_merged` counts the extra
    // configurations for the source, so it is the second command plus at most
    // one exploration.
    assert!(
        program.variants_merged <= 2,
        "budget bought {} variants",
        program.variants_merged
    );
}

#[test]
fn a_header_static_stays_one_object_per_translation_unit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("shared.h"), "static int counter;\n").unwrap();
    for name in ["a.c", "b.c"] {
        std::fs::write(
            root.join(name),
            format!(
                "#include \"shared.h\"\nvoid {}(void) {{ counter++; }}\n",
                name.trim_end_matches(".c")
            ),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("compile_commands.json"),
        json!([
            {"directory":root, "file":"a.c", "arguments":["cc", "-c", "a.c"]},
            {"directory":root, "file":"b.c", "arguments":["cc", "-c", "b.c"]}
        ])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    // Each TU that includes the header gets its own object in C. Collapsing
    // them onto one id would merge their points-to sets.
    let counters = program
        .symbols
        .variables
        .iter()
        .filter(|v| v.name == "counter")
        .count();
    assert_eq!(counters, 2, "header static shared across translation units");
}

#[test]
fn function_like_macro_define_and_undef_in_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "void target_a(void) {}\nvoid target_b(void) {}\nvoid entry(void) {\n#ifdef CALL\n    CALL();\n#else\n    target_b();\n#endif\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["cc", "-D", "CALL()=target_a()", "-U", "CALL", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(has_any_edge(&program, &analysis, "entry", "target_b"));
    assert!(!has_any_edge(&program, &analysis, "entry", "target_a"));
}

#[test]
fn a_function_like_command_line_define_settles_a_candidate() {
    // `-D CALL()=...` records the name with its parameter list, while
    // exploration candidates are bare identifiers. The command line already
    // decides CALL, so the GN candidate must buy no variant for it.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "void target_a(void) {}\nvoid entry(void) {\n#if CALL == 2\n    explored();\n#else\n    target_a();\n#endif\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("BUILD.gn"), "defines = [ \"CALL=2\" ]\n").unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["cc", "-D", "CALL()=target_a()", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let opts = PreprocessOptions::new()
        .with_explore(true)
        .with_explore_budget(4);
    let program = build_program_with_jobs(root, &opts, 1).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(has_any_edge(&program, &analysis, "entry", "target_a"));
    assert_eq!(
        program.variants_merged, 0,
        "the command line already defines CALL"
    );
}

#[test]
fn build_program_with_jobs_rejects_a_missing_explicit_database_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.c"), "void entry(void) {}\n").unwrap();
    let mut opts = PreprocessOptions::new();
    opts.compilation_database = Some(root.join("absent.json"));
    let err = build_program_with_jobs(root, &opts, 1).unwrap_err();
    assert!(
        err.contains("compilation database does not exist or is not a file"),
        "{err}"
    );
}

#[test]
fn versioned_launcher_and_mixed_case_windows_extension() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A `.c` path: only the `.Exe`-stripped `CLANG++` driver stem selects C++,
    // and only if the versioned launcher ahead of it is skipped. A namespace
    // does not parse as C, so the symbol pins both.
    std::fs::write(
        root.join("main.c"),
        "namespace Driver { void cpp_callee(void) {} }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["/usr/bin/ccache-4.10", "C:\\bin\\CLANG++.Exe", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program
        .symbols
        .resolve_function("Driver::cpp_callee")
        .is_some());
}

#[test]
fn an_uppercase_launcher_on_windows_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "#if defined(__cplusplus)\nnamespace Driver { void win_launcher() {} }\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["C:\\bin\\CCACHE.EXE", "C:\\bin\\CLANG++.Exe", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program
        .symbols
        .resolve_function("Driver::win_launcher")
        .is_some());
}

#[test]
fn a_launcher_prefixed_compiler_wrapper_is_the_driver() {
    // `ccache-clang++` / `ccache-gcc` wrappers are a real naming convention.
    // The name starts with a launcher's, but it *is* the driver: skipping it
    // would eat the next argument, dropping the flag it lands on.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "#if __cplusplus == 201103L\nnamespace Driver { void selected() {} }\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["ccache-clang++", "-std=c++11", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program
        .symbols
        .resolve_function("Driver::selected")
        .is_some());
}

#[test]
fn a_non_ascii_compiler_path_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.c"), "void entry(void) {}\n").unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["/opt/\u{7f16}\u{8bd1}\u{5668}/\u{65e5}\u{672c}", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program.symbols.resolve_function("entry").is_some());
}

#[test]
fn an_unknown_standard_keeps_the_rest_of_the_command() {
    // The standards table ages out. Losing the version macros is acceptable;
    // losing the entry's includes and defines is not.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("inc")).unwrap();
    std::fs::write(root.join("inc/dep.h"), "#define DEP_OK 1\n").unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include \"dep.h\"\nvoid helper(void) {}\nvoid entry(void) {\n#if defined(FROM_DB) && DEP_OK\n    helper();\n#endif\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["cc", "-std=gnu2y", "-Iinc", "-DFROM_DB=1", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(has_any_edge(&program, &analysis, "entry", "helper"));
}

#[test]
fn a_command_string_keeps_backslashes_that_are_not_shell_escapes() {
    // A database written on Windows spells its paths `C:\proj\include`. A
    // shell-faithful split swallows those separators silently, leaving an
    // include directory that resolves nothing. A directory whose name really
    // does contain a backslash pins the behaviour on any host. The `\"` in the
    // define is a genuine shell escape and must still be honoured.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("in\\c")).unwrap();
    std::fs::write(root.join("in\\c/dep.h"), "#define DEP_OK 1\n").unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include \"dep.h\"\nvoid helper(void) {}\nvoid entry(void) {\n#if DEP_OK && defined(NAMED)\n    helper();\n#endif\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "command": "clang.exe -Iin\\c -DNAMED=\\\"x\\\" -c main.c"
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(has_any_edge(&program, &analysis, "entry", "helper"));
}

#[test]
fn a_missing_forced_include_is_reported_against_the_source() {
    // The synthetic `<command-line>` search path must not reach a diagnostic:
    // a diagnostic's file is interned into the program's file table.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.c"), "void entry(void) {}\n").unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "arguments": ["cc", "-include", "generated/config.h", "-c", "main.c"]
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program
        .diagnostics
        .iter()
        .any(|d| d.message.contains("forced include file not found")));
    assert!(
        !program
            .symbols
            .files
            .iter()
            .any(|f| f.path.to_string_lossy().contains("<command-line>")),
        "synthetic search path interned as a file"
    );
}

#[test]
fn split_command_escaped_backslash_inside_double_quotes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.c"),
        "void helper(void) {}\nvoid entry(void) {\n#if defined(FOO)\nhelper();\n#endif\n}\n",
    )
    .unwrap();
    // The value ends in an escaped backslash, so the `\\` sits directly against
    // the closing quote. Pairing it is what stops the second backslash from
    // eating that quote and running the scan off the end of the string; a value
    // with anything after the `\\` closes either way and proves nothing.
    std::fs::write(
        root.join("compile_commands.json"),
        json!([{
            "directory": root,
            "file": "main.c",
            "command": "clang.exe \"-DFOO=x\\\\\" -c main.c"
        }])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    let (_, analysis) = trace_analysis::analyze(&program);
    assert!(has_any_edge(&program, &analysis, "entry", "helper"));
}

/// A configuration that *adds* an overload must not be folded into the one the
/// first translation unit already defined. Both orders are checked because the
/// merge treats the first source's units as the base family: when the extra
/// overload arrives from a later source it takes the name-only fallback, and
/// only the signature check keeps it apart from an alternative implementation.
///
/// Both a differing arity and a same-arity, differing-type overload are covered:
/// a parameter count alone cannot tell `pick(int)` from `pick(double)`.
#[test]
fn a_conditionally_added_overload_stays_distinct_in_either_order() {
    let extra_overload = [
        "inline int pick(int a, int b) { return a + b; }\n",
        "inline int pick(double a) { return (int)a; }\n",
    ];
    for extra in extra_overload {
        for extra_in in ["a.cpp", "b.cpp"] {
            check_overload_survives(extra, extra_in);
        }
    }
}

fn check_overload_survives(extra: &str, extra_in: &str) {
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("shared.h"),
            format!(
                "#ifndef SHARED_H\n#define SHARED_H\n\
                 inline int pick(int a) {{ return a; }}\n\
                 #ifdef EXTRA\n{extra}#endif\n#endif\n"
            ),
        )
        .unwrap();
        for name in ["a.cpp", "b.cpp"] {
            std::fs::write(
                root.join(name),
                format!(
                    "#include \"shared.h\"\nint {}_use(void) {{ return pick(1); }}\n",
                    &name[..1]
                ),
            )
            .unwrap();
        }
        let entry = |name: &str| {
            let mut args = vec!["clang++".to_string()];
            if name == extra_in {
                args.push("-DEXTRA=1".to_string());
            }
            args.push("-c".to_string());
            args.push(name.to_string());
            json!({"directory": root, "file": name, "arguments": args})
        };
        std::fs::write(
            root.join("compile_commands.json"),
            json!([entry("a.cpp"), entry("b.cpp")]).to_string(),
        )
        .unwrap();
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
        let picks = program
            .symbols
            .functions
            .iter()
            .filter(|f| f.name == "pick" && f.is_defined)
            .count();
        assert_eq!(
            picks, 2,
            "both `pick` overloads must survive with -DEXTRA on {extra_in}, \
             extra overload {extra:?}"
        );
    }
}

/// C has no overloading, so `#ifdef` arms that differ in arity are still one
/// function. The signature check that keeps C++ overloads apart must not reach
/// them: rejecting the merge lets the second arm register as a redeclaration,
/// which overwrites the base definition's span and parameters.
#[test]
fn c_alternative_arms_of_different_arity_stay_one_function() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("shared.h"),
        "#ifndef SHARED_H\n#define SHARED_H\n\
         #ifdef DEBUG_LOG\n\
         void log_msg(const char *m, const char *f, int l) { (void)m; (void)f; (void)l; }\n\
         #else\n\
         void log_msg(const char *m) { (void)m; }\n\
         #endif\n#endif\n",
    )
    .unwrap();
    std::fs::write(
        root.join("a.c"),
        "#include \"shared.h\"\nvoid a_use(void) { log_msg(\"x\"); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.c"),
        "#include \"shared.h\"\nvoid b_use(void) { log_msg(\"x\", \"f\", 1); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_commands.json"),
        json!([
            {"directory": root, "file": "a.c", "arguments": ["cc", "-c", "a.c"]},
            {"directory": root, "file": "b.c", "arguments": ["cc", "-DDEBUG_LOG=1", "-c", "b.c"]}
        ])
        .to_string(),
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    let defs: Vec<u32> = program
        .symbols
        .functions
        .iter()
        .filter(|f| f.name == "log_msg" && f.is_defined)
        .map(|f| f.span.line)
        .collect();
    // The `#else` arm is what the base configuration compiles; the variant must
    // extend it rather than replace it.
    assert_eq!(defs, vec![6], "one `log_msg`, still spanning the base arm");
}

/// Every unit past the first merges with cross-configuration layout unioning,
/// so a field can sit at an index the configuration that lowered the access
/// never gave it. The solver's name-based recovery has to be switched on for an
/// ordinary database too, where no *exploration* variant was merged at all.
#[test]
fn field_flow_survives_a_layout_unioned_across_commands() {
    for extra_in in ["a.c", "b.c"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("shared.h"),
            "#ifndef SHARED_H\n#define SHARED_H\n\
             struct S {\n#ifdef EXTRA\n    int extra_a;\n    int extra_b;\n#endif\n\
             void (*cb)(void);\n};\nextern struct S g;\n#endif\n",
        )
        .unwrap();
        std::fs::write(
            root.join("a.c"),
            "#include \"shared.h\"\nstruct S g;\nvoid real_target(void) {}\n\
             void a_install(void) { g.cb = real_target; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.c"),
            "#include \"shared.h\"\nvoid b_fire(void) { g.cb(); }\n",
        )
        .unwrap();
        let entry = |name: &str| {
            let mut args = vec!["cc".to_string()];
            if name == extra_in {
                args.push("-DEXTRA=1".to_string());
            }
            args.push("-c".to_string());
            args.push(name.to_string());
            json!({"directory": root, "file": name, "arguments": args})
        };
        std::fs::write(
            root.join("compile_commands.json"),
            json!([entry("a.c"), entry("b.c")]).to_string(),
        )
        .unwrap();
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
        let (_, analysis) = trace_analysis::analyze(&program);
        assert!(
            has_any_edge(&program, &analysis, "b_fire", "real_target"),
            "the callback stored under one configuration must reach the call \
             made under the other, with -DEXTRA on {extra_in}"
        );
        // The per-source exploration tally keeps its own meaning.
        assert_eq!(program.variants_merged, 0);
    }
}
