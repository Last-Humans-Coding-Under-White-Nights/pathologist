mod common;

use common::*;
use serde_json::json;
use trace_parse::build_program_with_jobs;
use trace_preproc::PreprocessOptions;

#[test]
fn whitespace_only_lines_are_ignored_without_trimming_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(" include space ")).unwrap();
    std::fs::write(
        root.join(" include space /config.h"),
        "#define HEADER_OK 1\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <config.h>\n#if HEADER_OK && FROM_FLAGS == 7\nvoid selected(void) {}\n#endif\n",
    )
    .unwrap();
    for ending in ["\n", "\r\n"] {
        let flags = [
            "   ",
            "\t",
            "-I",
            " \t ",
            " include space ",
            "-DFROM_FLAGS=7",
            "\t  ",
            "",
        ]
        .join(ending);
        std::fs::write(root.join("compile_flags.txt"), flags).unwrap();
        for jobs in [1, 4] {
            let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
            assert!(program.symbols.resolve_function("selected").is_some());
            assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
        }
    }
}

#[test]
fn invalid_caller_macros_do_not_discard_valid_shared_flags() {
    use trace_preproc::CommandMacro;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let include = dir.path().join("flags include");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&include).unwrap();
    std::fs::write(include.join("config.h"), "#define HEADER_OK 1\n").unwrap();
    std::fs::write(
        root.join("compile_flags.txt"),
        "-I../flags include\n-DFROM_FLAGS=7\n-DVALUE=1\n",
    )
    .unwrap();
    for (file, name) in [("main.c", "source"), ("orphan.hpp", "orphan")] {
        std::fs::write(root.join(file), format!(
            "#include <config.h>\n#if HEADER_OK && FROM_FLAGS == 7 && VALUE == 9 && FROM_CALLER == 3\nvoid {name}(void) {{}}\n#endif\n"
        )).unwrap();
    }
    for bad in [
        CommandMacro::Define("123BAD".into(), "1".into()),
        CommandMacro::Undef("BAD NAME".into()),
    ] {
        let mut opts = PreprocessOptions::new().with_define("VALUE", "9");
        opts.command_macros = vec![CommandMacro::Define("FROM_CALLER".into(), "3".into()), bad];
        for jobs in [1, 4] {
            let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
            for name in ["source", "orphan"] {
                assert!(program.symbols.resolve_function(name).is_some(), "{name}");
            }
            assert!(
                program
                    .diagnostics
                    .iter()
                    .any(|d| d.message.contains("invalid command-line")),
                "{:?}",
                program.diagnostics
            );
            assert!(
                program
                    .diagnostics
                    .iter()
                    .all(|d| d.stage != "compile_commands"),
                "{:?}",
                program.diagnostics
            );
        }
    }
}

#[test]
fn shared_flags_recover_a_conditional_call() {
    for jobs in [1, 4] {
        let program =
            build_program_with_jobs(&fixture("compile_flags"), &PreprocessOptions::new(), jobs)
                .unwrap();
        let (_, analysis) = trace_analysis::analyze(&program);
        assert!(has_any_edge(&program, &analysis, "entry", "target"));
        assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
    }
}

#[test]
fn shared_quote_paths_preserve_search_classes_order_and_cli_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    for (path, value) in [
        ("quote one", 1),
        ("quote two", 2),
        ("include", 3),
        ("cli", 4),
    ] {
        std::fs::create_dir(dir.path().join(path)).unwrap();
        std::fs::write(
            dir.path().join(path).join("config.h"),
            format!("#define HEADER {value}\n"),
        )
        .unwrap();
        std::fs::write(dir.path().join(path).join("local.h"), "#define LOCAL 0\n").unwrap();
    }
    std::fs::write(root.join("src/local.h"), "#define LOCAL 1\n").unwrap();
    for flags in [
        "-iquote../quote one\n-iquote../quote two\n-I../include\n",
        "-iquote\n../quote one\n-iquote\n../quote two\n-I\n../include\n",
    ] {
        std::fs::write(root.join("compile_flags.txt"), flags).unwrap();
        for cli_override in [false, true] {
            let mut opts = PreprocessOptions::new();
            if cli_override {
                opts.include_paths.push(dir.path().join("cli"));
            }
            let angle_value = if cli_override { 4 } else { 3 };
            std::fs::write(
                root.join("src/main.c"),
                format!(
                    "#include \"local.h\"\n#include \"config.h\"\n\
                     #if LOCAL == 1 && HEADER == 1\nvoid quoted(void) {{}}\n#endif\n\
                     #undef HEADER\n#include <config.h>\n\
                     #if HEADER == {angle_value}\nvoid angled(void) {{}}\n#endif\n\
                     void entry(void) {{ quoted(); angled(); }}\n"
                ),
            )
            .unwrap();
            for jobs in [1, 4] {
                let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
                let (_, analysis) = trace_analysis::analyze(&program);
                for name in ["quoted", "angled"] {
                    assert!(program.symbols.resolve_function(name).is_some(), "{name}");
                    assert!(has_any_edge(&program, &analysis, "entry", name));
                }
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn shared_builtin_i_duplicates_match_json_and_preserve_cli_and_cpath() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let builtin = dir.path().join("builtin");
    let user = dir.path().join("user");
    let bin = dir.path().join("bin");
    for path in [&builtin, &user, &bin] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(builtin.join("config.h"), "#define BUILTIN_CHOICE 1\n").unwrap();
    std::fs::write(user.join("config.h"), "#define BUILTIN_CHOICE 0\n").unwrap();
    let driver = bin.join("gcc");
    std::fs::write(&driver,
        "#!/bin/sh\nprintf '#include <...> search starts here:\\n %s\\nEnd of search list.\\n' \"$BUILTIN_DIR\" >&2\n"
    ).unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();

    for mode in ["flags", "json", "headers"] {
        let root = dir.path().join(mode);
        std::fs::create_dir(&root).unwrap();
        for (file, name) in [
            ("main.c", "source"),
            ("orphan.h", "orphan"),
            ("other.hpp", "other"),
        ] {
            if mode == "headers" && file == "main.c" {
                continue;
            }
            std::fs::write(root.join(file), format!(
                "#include <config.h>\n#if BUILTIN_CHOICE\nvoid {name}_builtin(void) {{}}\n#else\nvoid {name}_user(void) {{}}\n#endif\n"
            )).unwrap();
        }
        if mode == "json" {
            std::fs::write(
                root.join("compile_commands.json"),
                json!([{
                    "directory": root,
                    "file": "main.c",
                    "arguments": ["gcc", "-I../builtin", "-I../user", "-c", "main.c"]
                }])
                .to_string(),
            )
            .unwrap();
        } else {
            std::fs::write(root.join("compile_flags.txt"), "-I../builtin\n-I../user\n").unwrap();
        }
        for (label, enabled, cli, cpath, expected) in [
            ("defaults", true, false, false, "user"),
            ("cli", true, true, false, "builtin"),
            ("cpath", true, false, true, "builtin"),
            ("unprobed", false, false, false, "builtin"),
        ] {
            for jobs in [1, 4] {
                let output = dir.path().join(format!("{mode}-{label}-{jobs}.db"));
                let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_trace"));
                command
                    .arg("analyze")
                    .arg(&root)
                    .arg("--jobs")
                    .arg(jobs.to_string())
                    .arg("-o")
                    .arg(&output)
                    .env("PATH", &bin)
                    .env("BUILTIN_DIR", &builtin)
                    .env_remove("CPATH");
                if enabled {
                    command.arg("--system-includes");
                }
                if cli {
                    command.arg("--include").arg(&builtin);
                }
                if cpath {
                    command.env("CPATH", "../builtin");
                }
                let result = command.output().unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let conn = rusqlite::Connection::open(output).unwrap();
                let names: &[&str] = match mode {
                    "json" => &["source"],
                    "headers" => &["orphan", "other"],
                    _ => &["source", "orphan", "other"],
                };
                for name in names {
                    let selected = format!("{name}_{expected}");
                    let count: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM functions WHERE name=?1",
                            [&selected],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(count, 1, "{mode}, {label}, jobs={jobs}: missing {selected}");
                    let total: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM functions WHERE name IN (?1, ?2)",
                            [format!("{name}_builtin"), format!("{name}_user")],
                            |row| row.get(0),
                        )
                        .unwrap();
                    assert_eq!(total, 1);
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn shared_system_paths_reach_project_probe_and_enable_external_header_call() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let bin = dir.path().join("bin");
    let builtin = dir.path().join("builtin");
    for path in [&root, &bin, &builtin] {
        std::fs::create_dir(path).unwrap();
    }
    for (path, value) in [
        ("system one", 1),
        ("system two", 2),
        ("cli", 3),
        ("builtin", 4),
    ] {
        std::fs::create_dir_all(dir.path().join(path)).unwrap();
        std::fs::write(
            dir.path().join(path).join("config.h"),
            format!("#define HEADER {value}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        dir.path().join("system one/external.h"),
        "#define ENABLE_EXTERNAL 1\nvoid external(void) {}\n",
    )
    .unwrap();
    let driver = bin.join("gcc");
    std::fs::write(
        &driver,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$PROBE_LOG\"\n\
         printf '#include <...> search starts here:\\n' >&2\n\
         while [ \"$#\" -gt 0 ]; do\n\
         if [ \"$1\" = -isystem ]; then printf ' %s\\n' \"$2\" >&2; shift 2; else shift; fi\n\
         done\n\
         printf ' %s\\nEnd of search list.\\n' \"$BUILTIN_DIR\" >&2\n",
    )
    .unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();

    for (index, flags) in [
        "-isystem../system one\n-isystem../system two\n-DVALUE=1\n",
        "-isystem\n../system one\n-isystem\n../system two\n-DVALUE=1\n",
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::write(root.join("compile_flags.txt"), flags).unwrap();
        for cli_override in [false, true] {
            let expected_header = if cli_override { 3 } else { 1 };
            std::fs::write(
                root.join("main.c"),
                format!(
                    "#include <config.h>\n#include <external.h>\nvoid entry(void) {{\n\
                     #if ENABLE_EXTERNAL && HEADER == {expected_header} && VALUE == 9\n\
                     external();\n#endif\n}}\n"
                ),
            )
            .unwrap();
            let output = dir.path().join(format!("result-{index}-{cli_override}.db"));
            let log = dir.path().join(format!("probe-{index}-{cli_override}.log"));
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_trace"));
            command
                .arg("analyze")
                .arg(&root)
                .args(["--system-includes", "--jobs", "1", "-D", "VALUE=9", "-o"])
                .arg(&output)
                .env("PATH", &bin)
                .env("PROBE_LOG", &log)
                .env("BUILTIN_DIR", &builtin);
            if cli_override {
                command.arg("--include").arg(dir.path().join("cli"));
            }
            let result = command.output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let probe_args = std::fs::read_to_string(log).unwrap();
            let paths: Vec<_> = probe_args.lines().collect();
            let one = trace_ir::canonicalize(&dir.path().join("system one"));
            let two = trace_ir::canonicalize(&dir.path().join("system two"));
            assert!(paths.windows(4).any(|args| args
                == [
                    "-isystem",
                    one.to_str().unwrap(),
                    "-isystem",
                    two.to_str().unwrap()
                ]));
            let conn = rusqlite::Connection::open(output).unwrap();
            let edges: i64 = conn.query_row(
                "SELECT count(*) FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id WHERE caller.name='entry' AND callee.name='external' AND callee.is_dep=1 AND callee.is_defined=0",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                edges, 1,
                "external header did not enable the conditional call"
            );
            let missing: i64 = conn.query_row(
                "SELECT count(*) FROM diagnostics WHERE message LIKE '%include file not found%'",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(missing, 0);
        }
    }
}

#[test]
fn orphan_headers_with_shared_flags_resolve_generated_idl_interfaces() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let source = fixture("idl_basic");
    for rel in [
        "idl/IAtm.idl",
        "client/client.cpp",
        "service/atm_service.cpp",
    ] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(source.join(rel), path).unwrap();
    }
    std::fs::write(root.join("compile_flags.txt"), "-DSHARED_FLAG=1\n").unwrap();
    // Two orphan headers exercise the parallel header pass when jobs = 4.
    for name in ["OrphanClient", "OtherOrphanClient"] {
        std::fs::write(
            root.join(format!("client/{name}.h")),
            format!(
                "#include \"iatm.h\"\n\
                 #if SHARED_FLAG\nnamespace OHOS {{ namespace Security {{\n\
                 struct {name} {{\n    sptr<IAtm> proxy_;\n\
                 int Check(unsigned id) {{ int s; return proxy_->VerifyAccessToken(id, s); }}\n\
                 }};\n}} }}\n#endif\n"
            ),
        )
        .unwrap();
    }
    for jobs in [1, 4] {
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
        assert!(
            program.diagnostics.iter().all(|d| !(d.stage == "preprocess"
                && d.message.contains("include file not found")
                && d.message.contains("iatm.h"))),
            "jobs={jobs}: {:?}",
            program.diagnostics
        );
        let (_, analysis) = trace_analysis::analyze(&program);
        for caller in [
            "Client::Verify",
            "OrphanClient::Check",
            "OtherOrphanClient::Check",
        ] {
            for callee in ["IAtm::VerifyAccessToken", "AtmProxy::VerifyAccessToken"] {
                assert!(
                    has_any_edge(
                        &program,
                        &analysis,
                        &format!("OHOS::Security::{caller}"),
                        &format!("OHOS::Security::{callee}"),
                    ),
                    "jobs={jobs}: {caller} -> {callee}"
                );
            }
        }
        assert!(has_edge(
            &program,
            &analysis,
            "OHOS::Security::AtmProxy::VerifyAccessToken",
            "OHOS::Security::AtmService::VerifyAccessToken",
            trace_analysis::ResolutionKind::IpcBridge,
        ));
    }
}

#[test]
fn orphan_headers_use_shared_flags_include_paths_and_cli_overrides() {
    for source in [Some("main.c"), Some("main.cpp"), None] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        for (path, value) in [("flags include", 1), ("cli include", 2)] {
            std::fs::create_dir(dir.path().join(path)).unwrap();
            std::fs::write(
                dir.path().join(path).join("selection.h"),
                format!("#define HEADER_VALUE {value}\n"),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("compile_flags.txt"),
            "-DFLAG\n-DVALUE=1\n-I\n../flags include\n",
        )
        .unwrap();
        if let Some(file) = source {
            std::fs::write(root.join(file), "void entry(void) {}\n").unwrap();
        }
        for (header, name) in [("one.h", "one"), ("two.hpp", "two")] {
            std::fs::write(
                root.join(header),
                format!(
                    "#if defined(FLAG) && VALUE == 9\n#include <selection.h>\n\
                 #ifdef __cplusplus\nnamespace N {{\n#endif\n\
                 #if HEADER_VALUE == 1\nvoid {name}_flags(void) {{}}\n\
                 #elif HEADER_VALUE == 2\nvoid {name}_cli(void) {{}}\n#endif\n\
                 #ifdef __cplusplus\n}}\n#endif\n#endif\n"
                ),
            )
            .unwrap();
        }
        for cli_include in [false, true] {
            let mut opts = PreprocessOptions::new().with_define("VALUE", "9");
            if cli_include {
                opts.include_paths.push(dir.path().join("cli include"));
            }
            for jobs in [1, 4] {
                let program = build_program_with_jobs(&root, &opts, jobs).unwrap();
                assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
                let suffix = if cli_include { "cli" } else { "flags" };
                for name in ["one", "two"] {
                    let prefix = if name == "two" || source != Some("main.c") {
                        "N::"
                    } else {
                        ""
                    };
                    let expected = format!("{prefix}{name}_{suffix}");
                    assert!(
                        program.symbols.resolve_function(&expected).is_some(),
                        "{source:?}, jobs={jobs}: {expected}"
                    );
                }
            }
        }
    }
}

#[test]
fn orphan_header_language_errors_are_reported_once_after_source_validation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("compile_flags.txt"),
        "-DFLAG\n-x\nnone\n-std=c11\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.c"),
        "#ifdef FLAG\nvoid configured_source(void) {}\n#endif\n",
    )
    .unwrap();
    for name in ["one", "two"] {
        std::fs::write(root.join(format!("{name}.hpp")), format!(
            "void fallback_{name}(void) {{}}\n#ifdef FLAG\nvoid configured_{name}(void) {{}}\n#endif\n"
        )).unwrap();
    }
    for jobs in [1, 4] {
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
        assert!(program
            .symbols
            .resolve_function("configured_source")
            .is_some());
        for name in ["one", "two"] {
            assert!(program
                .symbols
                .resolve_function(&format!("fallback_{name}"))
                .is_some());
            assert!(program
                .symbols
                .resolve_function(&format!("configured_{name}"))
                .is_none());
        }
        let diagnostics: Vec<_> = program
            .diagnostics
            .iter()
            .filter(|d| d.stage == "compile_commands")
            .collect();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0]
            .message
            .contains("does not match source language"));
    }
}

#[test]
fn invalid_shared_arguments_report_once_for_multiple_languages_and_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (file, name) in [
        ("one.c", "one"),
        ("two.c", "two"),
        ("three.cpp", "three"),
        ("four.cpp", "four"),
        ("five.c", "five"),
    ] {
        std::fs::write(
            root.join(file),
            format!(
                "void {name}(void) {{}}\n#ifdef FROM_FLAGS\nvoid flags_{name}(void) {{}}\n#endif\n"
            ),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("orphan.hpp"),
        "void orphan_fallback(void) {}\n#ifdef FROM_FLAGS\nvoid orphan_flags(void) {}\n#endif\n",
    )
    .unwrap();
    for bad in [
        "-I",
        "-U123BAD",
        "-U=",
        "-UFOO=1",
        "-UFOO BAR",
        "-UF(x)",
        "-DA+B=1",
    ] {
        std::fs::write(
            root.join("compile_flags.txt"),
            format!("-DFROM_FLAGS\n{bad}\n"),
        )
        .unwrap();
        for jobs in [1, 4] {
            let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
            for name in ["one", "two", "three", "four", "five"] {
                assert!(
                    program.symbols.resolve_function(name).is_some(),
                    "{bad}: {name}"
                );
                assert!(program
                    .symbols
                    .resolve_function(&format!("flags_{name}"))
                    .is_none());
            }
            assert!(program
                .symbols
                .resolve_function("orphan_fallback")
                .is_some());
            assert!(program.symbols.resolve_function("orphan_flags").is_none());
            let diagnostics: Vec<_> = program
                .diagnostics
                .iter()
                .filter(|d| d.stage == "compile_commands")
                .collect();
            assert_eq!(diagnostics.len(), 1, "{bad}: {diagnostics:?}");
            assert!(diagnostics[0].message.contains("compile_flags.txt"));
            assert!(diagnostics[0]
                .message
                .contains("using inferred configuration"));
        }
    }
}

#[test]
fn shared_undefines_and_cached_configs_preserve_mixed_languages_and_cli_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let sources = [
        ("one.c", "one"),
        ("two.c", "two"),
        ("three.cpp", "three"),
        ("four.cpp", "four"),
    ];
    for (file, name) in sources {
        std::fs::write(
            root.join(file),
            format!(
                "#if SHARED == 9 && !defined(REMOVED) && !defined(NONEXISTENT)\n\
             #ifdef __cplusplus\nnamespace N {{ void {name}() {{}} }}\n\
             #else\nvoid {name}(void) {{}}\n#endif\n#endif\n"
            ),
        )
        .unwrap();
    }
    std::fs::write(root.join("compile_flags.txt"),
        "-DSHARED=1\n-USHARED\n-DREMOVED\n-UREMOVED\n-UNONEXISTENT\n-U\n_ANOTHER_MISSING2\n-x\nnone\n"
    ).unwrap();
    for language in [
        None,
        Some(trace_preproc::Language::C),
        Some(trace_preproc::Language::Cpp),
    ] {
        let mut opts = PreprocessOptions::new().with_define("SHARED", "9");
        opts.language = language;
        for jobs in [1, 4] {
            let program = build_program_with_jobs(root, &opts, jobs).unwrap();
            assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
            for (file, name) in sources {
                let effective = language.unwrap_or_else(|| {
                    trace_preproc::Language::from_path(std::path::Path::new(file))
                });
                let expected = if effective == trace_preproc::Language::Cpp {
                    format!("N::{name}")
                } else {
                    name.to_string()
                };
                assert!(
                    program.symbols.resolve_function(&expected).is_some(),
                    "{language:?}: {expected}"
                );
            }
        }
    }
}

#[test]
fn shared_language_specific_errors_keep_the_other_languages_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (file, name) in [
        ("one.c", "one"),
        ("two.c", "two"),
        ("three.cpp", "three"),
        ("four.cpp", "four"),
    ] {
        std::fs::write(
            root.join(file),
            format!(
                "void {name}(void) {{}}\n#if defined(SHARED) && __cplusplus == 201703L\n\
             namespace N {{ void configured_{name}() {{}} }}\n#endif\n"
            ),
        )
        .unwrap();
    }
    // -x none keeps each source's language; only C++ agrees with this standard.
    std::fs::write(
        root.join("compile_flags.txt"),
        "-DSHARED\n-x\nnone\n-std=c++17\n",
    )
    .unwrap();
    for jobs in [1, 4] {
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), jobs).unwrap();
        for name in ["one", "two", "three", "four"] {
            assert!(program.symbols.resolve_function(name).is_some());
            assert_eq!(
                program
                    .symbols
                    .resolve_function(&format!("N::configured_{name}"))
                    .is_some(),
                matches!(name, "three" | "four")
            );
        }
        let diagnostics: Vec<_> = program
            .diagnostics
            .iter()
            .filter(|d| d.stage == "compile_commands")
            .collect();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert!(diagnostics[0]
            .message
            .contains("does not match source language"));
    }
}

#[test]
fn shared_flags_cover_multiple_sources_with_per_source_language_and_exclusions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("dep")).unwrap();
    std::fs::write(root.join("compile_flags.txt"), "-DSHARED\r\n\r\n").unwrap();
    std::fs::write(
        root.join("a.c"),
        "#if defined(SHARED) && !defined(__cplusplus)\nvoid c_source(void) {}\n#endif\n",
    )
    .unwrap();
    std::fs::write(root.join("b.cpp"), "#if defined(SHARED) && defined(__cplusplus)\nnamespace N { void cpp_source() {} }\n#endif\n").unwrap();
    for name in ["dep/excluded.c", "excluded.txt"] {
        std::fs::write(root.join(name), "void excluded(void) {}\n").unwrap();
    }
    let mut opts = PreprocessOptions::new();
    opts.dep_roots.push(root.join("dep"));
    let program = build_program_with_jobs(root, &opts, 2).unwrap();
    for name in ["c_source", "N::cpp_source"] {
        assert!(program.symbols.resolve_function(name).is_some(), "{name}");
    }
    assert!(program.symbols.resolve_function("excluded").is_none());
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

#[test]
fn cli_overrides_and_literal_space_paths_survive() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (path, value) in [("flags include", 1), ("cli include", 2)] {
        std::fs::create_dir(root.join(path)).unwrap();
        std::fs::write(
            root.join(path).join("config.h"),
            format!("#define HEADER {value}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("compile_flags.txt"),
        "-Iflags include\r\n-DVALUE=1\r\n-UVALUE\r\n-DVALUE=3\r\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <config.h>\n#if HEADER == 2 && VALUE == 4\nvoid selected(void) {}\n#endif\n",
    )
    .unwrap();
    let mut opts = PreprocessOptions::new().with_define("VALUE", "4");
    opts.include_paths.push(root.join("cli include"));
    let program = build_program_with_jobs(root, &opts, 1).unwrap();
    assert!(program.symbols.resolve_function("selected").is_some());
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

#[test]
fn selected_json_never_falls_through_to_flags() {
    for location in [
        "compile_commands.json",
        "build/compile_commands.json",
        "explicit.json",
    ] {
        let valid = json!([{
            "directory": if location.starts_with("build/") { ".." } else { "." },
            "file": "main.c",
            "arguments": ["cc", "-DFROM_JSON"]
        }])
        .to_string();
        for content in ["[]".to_string(), "invalid json".to_string(), valid.clone()] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir(root.join("build")).unwrap();
            std::fs::write(root.join("main.c"), "#ifdef FROM_FLAGS\nvoid flags(void) {}\n#endif\n#ifdef FROM_JSON\nvoid database(void) {}\n#endif\nvoid inferred(void) {}\n").unwrap();
            std::fs::write(root.join("compile_flags.txt"), "-DFROM_FLAGS\n").unwrap();
            std::fs::write(root.join(location), &content).unwrap();
            let mut opts = PreprocessOptions::new();
            if location == "explicit.json" {
                opts.compilation_database = Some(root.join(location));
            }
            let program = build_program_with_jobs(root, &opts, 1).unwrap();
            assert!(program.symbols.resolve_function("flags").is_none());
            assert!(program.symbols.resolve_function("inferred").is_some());
            assert_eq!(
                program.symbols.resolve_function("database").is_some(),
                content == valid
            );
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.c"), "void entry(void) {}\n").unwrap();
    std::fs::write(dir.path().join("compile_flags.txt"), "-DFROM_FLAGS\n").unwrap();
    let mut opts = PreprocessOptions::new();
    opts.compilation_database = Some(dir.path().join("missing.json"));
    assert!(build_program_with_jobs(dir.path(), &opts, 1).is_err());
}

#[test]
fn flags_search_is_root_only_or_single_file_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("compile_flags.txt"), "-DPARENT\n").unwrap();
    std::fs::write(root.join("src/compile_flags.txt"), "-DLOCAL\n").unwrap();
    std::fs::write(
        root.join("src/main.c"),
        "#ifdef PARENT\nvoid parent(void) {}\n#endif\n#ifdef LOCAL\nvoid local(void) {}\n#endif\n",
    )
    .unwrap();
    let opts = PreprocessOptions::new();
    let program = build_program_with_jobs(root, &opts, 1).unwrap();
    assert!(program.symbols.resolve_function("parent").is_some());
    assert!(program.symbols.resolve_function("local").is_none());
    let program = build_program_with_jobs(&root.join("src/main.c"), &opts, 1).unwrap();
    assert!(program.symbols.resolve_function("parent").is_none());
    assert!(program.symbols.resolve_function("local").is_some());
    std::fs::remove_file(root.join("compile_flags.txt")).unwrap();
    let program = build_program_with_jobs(root, &opts, 1).unwrap();
    assert!(program.symbols.resolve_function("local").is_none());
    std::fs::write(root.join("compile_flags.txt"), "-DPARENT\n").unwrap();
    std::fs::remove_file(root.join("src/compile_flags.txt")).unwrap();
    let program = build_program_with_jobs(&root.join("src"), &opts, 1).unwrap();
    assert!(program.symbols.resolve_function("parent").is_none());
}

#[test]
fn empty_and_malformed_flags_degrade_without_aborting() {
    for flags in [
        "",
        "\r\n\n",
        "   \r\n\t\n \t \r\n",
        "-DLOST\n-I\n",
        "-DLOST\n@args.rsp\n",
        "-DLOST\n-x\ninvalid\n",
        "-DLOST\n\0",
        "-DLOST\n-include\n",
        "-DLOST\n-D=1\n",
        "-DLOST\n-D=identifier\n",
        "-DLOST\n-D123BAD=1\n",
        "-DLOST\n-DF(x=1\n",
        "-DLOST\n-DA+B=1\n",
        "-DLOST\n-DFOO BAR=1\n",
        "-DLOST\n-DA-B=1\n",
        "-DLOST\n-DF(x)extra=1\n",
        "-DLOST\n-DF (x)=1\n",
        "-DLOST\n-DF(x,)=1\n",
        "-DLOST\n-DF(x,x)=1\n",
        "-DLOST\n-DF(...,x)=1\n",
        "-DLOST\n-DF(args...extra)=1\n",
        "-I.\n-std=c99\n-DLOST=1\ncshkjnclksmcklsjcklnsjcnbsjb\n",
        "-DLOST\nmain.c\n",
        "-DLOST\ncc\n",
        "-DLOST\n--\nmain.c\n",
        "-DLOST\n-\n",
        "-DLOST\naaaaaaa\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("main.c"),
            "void inferred(void) {}\n#ifdef LOST\nvoid lost(void) {}\n#endif\n",
        )
        .unwrap();
        std::fs::write(root.join("compile_flags.txt"), flags).unwrap();
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
        assert!(program.symbols.resolve_function("inferred").is_some());
        assert!(program.symbols.resolve_function("lost").is_none());
        assert_eq!(
            program
                .diagnostics
                .iter()
                .any(|d| d.stage == "compile_commands" && d.message.contains("compile_flags.txt")),
            flags.contains("LOST"),
            "{flags:?}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.c"), "void inferred(void) {}\n").unwrap();
    // A directory is reliably unreadable as text, including when run as root.
    std::fs::create_dir(dir.path().join("compile_flags.txt")).unwrap();
    let program = build_program_with_jobs(dir.path(), &PreprocessOptions::new(), 1).unwrap();
    assert!(program.symbols.resolve_function("inferred").is_some());
    assert!(program
        .diagnostics
        .iter()
        .any(|d| d.message.contains("compile_flags.txt")));
}

#[test]
fn explicit_language_and_standard_apply_to_shared_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("compile_flags.txt"), "\n-std=c++17\n").unwrap();
    for (file, name) in [("a.c", "a"), ("b.cpp", "b")] {
        std::fs::write(
            root.join(file),
            format!("#if __cplusplus == 201703L\nnamespace N {{ void {name}() {{}} }}\n#endif\n"),
        )
        .unwrap();
    }
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    for name in ["N::a", "N::b"] {
        assert!(program.symbols.resolve_function(name).is_some());
    }
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

#[test]
fn conflicting_shared_language_and_standard_discard_all_flags() {
    for flags in ["-DKEEP\n-x\nc++\n-std=c11\n", "-DKEEP\n-std=c11\n-x\nc++\n"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("compile_flags.txt"), flags).unwrap();
        std::fs::write(root.join("main.cpp"),
            "#ifdef KEEP\nvoid kept() {}\n#endif\n#ifdef __cplusplus\nnamespace N { void inferred() {} }\n#endif\n").unwrap();
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
        assert!(program.symbols.resolve_function("kept").is_none());
        assert!(program.symbols.resolve_function("N::inferred").is_some());
        assert!(program
            .diagnostics
            .iter()
            .any(|d| d.stage == "compile_commands"
                && d.message.contains("compile_flags.txt")
                && d.message.contains("does not match source language")));
    }
}

#[test]
fn shared_function_like_macro_definitions_are_valid() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("compile_flags.txt"),
        "-DVALUE(x)=((x) + 1)\n-DAPPLY(f,...)=f(__VA_ARGS__)\n\
         -DOBJECT=(2 + 3)\n-DREL=(5 >= 3 && 2 != 0)\n-DDEFAULT\n\
         -DEMPTY=\n-DCONST()=(2 + 3)\n-DPAIR(x, y)=((x) + (y))\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.c"),
        "#if APPLY(VALUE, 2) == 3 && OBJECT == 5 && REL && DEFAULT == 1 && CONST() == 5 && PAIR(2, 3) == 5\n\
         EMPTY void selected(void) {}\n#endif\n",
    )
    .unwrap();
    let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
    assert!(program.symbols.resolve_function("selected").is_some());
    assert!(program.diagnostics.is_empty(), "{:?}", program.diagnostics);
}

#[test]
fn shared_flags_ignore_linker_tail() {
    for link in ["/link", "-link"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("compile_flags.txt"),
            format!("-DKEEP\n{link}\n-DIGNORED\n/OUT:app.exe\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("main.c"),
            "#ifdef KEEP\nvoid kept(void) {}\n#endif\n#ifdef IGNORED\nvoid ignored(void) {}\n#endif\n",
        )
        .unwrap();
        let program = build_program_with_jobs(root, &PreprocessOptions::new(), 1).unwrap();
        assert!(program.symbols.resolve_function("kept").is_some(), "{link}");
        assert!(
            program.symbols.resolve_function("ignored").is_none(),
            "{link}"
        );
        assert!(
            program.diagnostics.is_empty(),
            "{link}: {:?}",
            program.diagnostics
        );
    }
}

#[test]
fn progress_identifies_inferred_configuration_after_shared_flags_fail() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.c"), "void entry(void) {}\n").unwrap();
    // Link metadata routes indexing through configured::build even when no
    // usable preprocessing configuration remains.
    std::fs::write(
        root.join("link_commands.json"),
        json!([{
            "directory": root,
            "arguments": ["cc", "main.c", "-o", "app"]
        }])
        .to_string(),
    )
    .unwrap();
    for (index, flags) in ["-I\n", "-DA+B=1\n"].into_iter().enumerate() {
        std::fs::write(root.join("compile_flags.txt"), flags).unwrap();
        let database = root.join(format!("inferred-{index}.db"));
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("analyze")
            .arg(root)
            .args(["--jobs", "1", "-o"])
            .arg(&database)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        let conn = rusqlite::Connection::open(database).unwrap();
        let warnings: i64 = conn.query_row(
            "SELECT count(*) FROM diagnostics WHERE stage='compile_commands' AND message LIKE '%using inferred configuration%'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(warnings, 1);
        assert!(
            stderr.contains("inferred: 0 commands for 0 sources (jobs=1)"),
            "{stderr}"
        );
        assert!(!stderr.contains("compile_flags: 0 commands"), "{stderr}");
    }
}

#[test]
fn progress_identifies_shared_flags_and_json_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("include space")).unwrap();
    std::fs::write(root.join("include space/config.h"), "#define HEADER_OK 1\n").unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <config.h>\nvoid entry(void) {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("compile_flags.txt"),
        "-I\ninclude space\n-D\nFLAG=1\n",
    )
    .unwrap();
    for label in ["compile_flags", "compile_commands"] {
        if label == "compile_commands" {
            std::fs::write(
                root.join("compile_commands.json"),
                json!([{
                    "directory": root,
                    "file": "main.c",
                    "arguments": ["cc", "-I", "include space", "main.c"]
                }])
                .to_string(),
            )
            .unwrap();
        }
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("analyze")
            .arg(root)
            .args(["--jobs", "1", "-o"])
            .arg(root.join(format!("{label}.db")))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains(&format!("{label}: 1 commands for 1 sources (jobs=1)")),
            "{stderr}"
        );
        let other = if label == "compile_flags" {
            "compile_commands"
        } else {
            "compile_flags"
        };
        assert!(
            !stderr.contains(&format!("{other}: 1 commands")),
            "{stderr}"
        );
    }
}
