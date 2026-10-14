use crate::common::fixture;
use std::path::Path;
use std::process::Command;

fn analyze(root: &Path, output: &Path, options: &[&str]) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(root)
        .arg("-o")
        .arg(output)
        .args(options)
        .env_remove("CPATH")
        .env_remove("C_INCLUDE_PATH")
        .env_remove("CPLUS_INCLUDE_PATH")
        .output()
        .unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(result.status.success(), "{stderr}");
    stderr
}

fn missing_count(output: &Path) -> i64 {
    rusqlite::Connection::open(output).unwrap().query_row(
        "SELECT count(*) FROM diagnostics WHERE stage='preprocess' AND message LIKE '%include file not found, skipping:%'",
        [], |row| row.get(0),
    ).unwrap()
}

#[test]
fn missing_standard_and_external_headers_keep_original_diagnostics_and_group_locations() {
    let scratch = tempfile::tempdir().unwrap();
    let root = fixture("preproc/missing_headers");
    let mut expected = None;
    for options in [&["--jobs", "1"][..], &["--jobs", "4", "--full-export"][..]] {
        let output = scratch.path().join("out.db");
        let stderr = analyze(&root, &output, options);
        assert!(
            stderr.contains("6 distinct, 7 diagnostic locations"),
            "{stderr}"
        );
        assert!(stderr.contains("vendor/api.h: 2 location(s)"), "{stderr}");
        assert!(stderr.contains("main.cpp:1"), "{stderr}");
        for hint in [
            "--compile-commands",
            "--system-includes",
            "--dep <root>",
            "--include <directory>",
            "Host headers must not substitute",
            "configured kernel build tree",
            "availability outside indexed files is unknown",
        ] {
            assert!(stderr.contains(hint), "missing {hint}: {stderr}");
        }
        assert_eq!(missing_count(&output), 7);
        let conn = rusqlite::Connection::open(&output).unwrap();
        let mut statement = conn.prepare("SELECT f.path, d.line, d.severity, d.message FROM diagnostics d JOIN files f ON f.id=d.file_id WHERE d.stage='preprocess' AND d.message LIKE 'include file not found, skipping:%' ORDER BY f.path, d.line, d.message").unwrap();
        let rows: Vec<(String, i64, String, String)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 7);
        assert!(rows.iter().all(|row| row.2 == "warning"));
        assert!(rows.iter().any(|row| row.0.ends_with("main.cpp")
            && row.1 == 1
            && row.3 == "include file not found, skipping: vector"));
        let summary = stderr
            .split("warning: unresolved headers:")
            .nth(1)
            .unwrap()
            .split("analyze:")
            .next()
            .unwrap()
            .to_string();
        if let Some((expected_rows, expected_summary)) = &expected {
            assert_eq!(&rows, expected_rows);
            assert_eq!(&summary, expected_summary);
        } else {
            expected = Some((rows, summary));
        }
    }
}

fn configured_tree(scratch: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = scratch.join("project");
    let sdk = scratch.join("sdk");
    std::fs::create_dir(&root).unwrap();
    for name in ["main.cpp", "second.c"] {
        std::fs::copy(
            fixture("preproc/missing_headers").join(name),
            root.join(name),
        )
        .unwrap();
    }
    for name in [
        "vector",
        "stdio.h",
        "vendor/api.h",
        "linux/device.h",
        "linux/dma-mapping.h",
        "linux/of.h",
    ] {
        let header = sdk.join(name);
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::write(header, "#pragma once\nvoid sdk_declaration(void);\n").unwrap();
    }
    (root, sdk)
}

#[test]
fn explicit_include_dependency_and_compilation_paths_resolve_headers() {
    for mode in ["include", "dep", "database", "flags"] {
        let scratch = tempfile::tempdir().unwrap();
        let (root, sdk) = configured_tree(scratch.path());
        let output = scratch.path().join("out.db");
        let sdk_path = sdk.to_str().unwrap();
        let options = match mode {
            "include" => vec!["--include", sdk_path],
            "dep" => vec!["--dep", sdk_path, "--include", sdk_path],
            "database" => {
                let commands: Vec<_> = ["main.cpp", "second.c"].iter().map(|file| serde_json::json!({"directory": root, "file": file, "arguments": ["trace-missing-target-compiler", "-I", sdk, "-c", file]})).collect();
                std::fs::write(
                    root.join("compile_commands.json"),
                    serde_json::to_string(&commands).unwrap(),
                )
                .unwrap();
                vec![]
            }
            "flags" => {
                std::fs::write(root.join("compile_flags.txt"), format!("-I\n{sdk_path}\n"))
                    .unwrap();
                vec![]
            }
            _ => unreachable!(),
        };
        let stderr = analyze(&root, &output, &options);
        assert!(!stderr.contains("unresolved headers:"), "{mode}: {stderr}");
        assert_eq!(missing_count(&output), 0, "{mode}");
    }
}

#[test]
fn indexed_header_is_evidence_of_a_search_configuration_problem() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("headers/vendor")).unwrap();
    std::fs::write(root.join("headers/vendor/api.h"), "void declared(void);\n").unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <vendor/api.h>\nvoid entry(void) {}\n",
    )
    .unwrap();
    std::fs::write(root.join("compile_commands.json"), serde_json::json!([{"directory": root, "file": "main.c", "arguments": ["trace-missing-target-compiler", "-c", "main.c"]}]).to_string()).unwrap();
    let output = root.join("out.db");
    let stderr = analyze(root, &output, &[]);
    assert!(stderr.contains("matching indexed file:"), "{stderr}");
    assert!(stderr.contains("headers/vendor/api.h"), "{stderr}");
    assert!(stderr.contains("search restrictions"), "{stderr}");
    assert_eq!(missing_count(&output), 1);
    let stderr = analyze(
        root,
        &output,
        &["--include", root.join("headers").to_str().unwrap()],
    );
    assert!(!stderr.contains("unresolved headers:"), "{stderr}");
    assert_eq!(missing_count(&output), 0);
}

#[test]
fn unavailable_target_compiler_does_not_silently_replace_sdk_headers() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    std::fs::write(
        root.join("main.cpp"),
        "#include <vector>\nvoid entry() {}\n",
    )
    .unwrap();
    std::fs::write(root.join("compile_commands.json"), serde_json::json!([{"directory": root, "file": "main.cpp", "arguments": ["trace-missing-target-compiler", "--sysroot=/missing/target-sdk", "-c", "main.cpp"]}]).to_string()).unwrap();
    let output = root.join("out.db");
    let stderr = analyze(root, &output, &["--system-includes"]);
    assert!(
        stderr.contains("remain unresolved with --system-includes"),
        "{stderr}"
    );
    assert_eq!(missing_count(&output), 1);
}

#[cfg(unix)]
#[test]
fn target_compiler_system_paths_are_opt_in() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let (root, sdk) = configured_tree(scratch.path());
    let driver = scratch.path().join("target-compiler");
    // The probe receives an empty input, never project source. The controlled
    // driver reports a target SDK, so the test does not depend on host headers.
    std::fs::write(&driver, format!("#!/bin/sh\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\n", sdk.display())).unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
    let commands: Vec<_> = ["main.cpp", "second.c"].iter().map(|file| serde_json::json!({"directory": root, "file": file, "arguments": [driver, "-c", file]})).collect();
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::to_string(&commands).unwrap(),
    )
    .unwrap();
    let output = scratch.path().join("out.db");
    let stderr = analyze(&root, &output, &[]);
    assert!(
        stderr.contains("6 distinct, 7 diagnostic locations"),
        "{stderr}"
    );
    assert_eq!(missing_count(&output), 7);
    let stderr = analyze(&root, &output, &["--system-includes"]);
    assert!(!stderr.contains("unresolved headers:"), "{stderr}");
    assert_eq!(missing_count(&output), 0);
}

fn write_commands(root: &Path, sources: &[&str], extra: &[&str]) {
    let commands: Vec<_> = sources
        .iter()
        .map(|file| {
            let mut arguments = vec!["trace-missing-target-compiler", "-c", file];
            arguments.extend_from_slice(extra);
            serde_json::json!({"directory": root, "file": file, "arguments": arguments})
        })
        .collect();
    std::fs::write(
        root.join("compile_commands.json"),
        serde_json::to_string(&commands).unwrap(),
    )
    .unwrap();
}

#[test]
fn applying_candidates_uses_the_proposed_file_and_preserves_explicit_include_priority() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    for directory in ["a-sdk/vendor", "z-sdk/vendor", "0-wrong/other", "explicit"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    std::fs::write(
        root.join("a-sdk/vendor/api.h"),
        "static void chosen(void) {}\n",
    )
    .unwrap();
    std::fs::write(root.join("a-sdk/vendor/reuse.h"), "void reuse(void);\n").unwrap();
    for directory in ["z-sdk/vendor", "0-wrong/other"] {
        std::fs::write(
            root.join(directory).join("api.h"),
            "static void alternative(void) {}\n",
        )
        .unwrap();
    }
    std::fs::write(
        root.join("explicit/existing.h"),
        "#define SELECTED chosen\n",
    )
    .unwrap();
    std::fs::write(
        root.join("a-sdk/existing.h"),
        "#define SELECTED alternative\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <vendor/api.h>\n#include <vendor/reuse.h>\n#include <existing.h>\nvoid entry(void) { SELECTED(); }\n",
    )
    .unwrap();
    write_commands(root, &["main.c"], &[]);
    let explicit = root.join("explicit");
    let explicit = explicit.to_str().unwrap();
    let baseline = root.join("baseline.db");
    let stderr = analyze(root, &baseline, &["--include", explicit]);
    assert_eq!(missing_count(&baseline), 2);
    assert!(stderr.contains("a-sdk/vendor/api.h"), "{stderr}");
    assert!(!stderr.contains("include-candidates: applying"));

    for options in [
        vec![
            "--apply-include-candidates",
            "--include",
            explicit,
            "--jobs",
            "1",
        ],
        vec![
            "--apply-include-candidates",
            "--include",
            explicit,
            "--jobs",
            "4",
            "--full-export",
        ],
    ] {
        let output = root.join("applied.db");
        let stderr = analyze(root, &output, &options);
        assert!(
            stderr.contains("include-candidates: applying 1 candidate include root(s) (pass 1)"),
            "{stderr}"
        );
        assert!(!stderr.contains("(pass 2)"), "{stderr}");
        assert!(!stderr.contains("unresolved headers:"), "{stderr}");
        assert_eq!(missing_count(&output), 0);
        let conn = rusqlite::Connection::open(&output).unwrap();
        let path: String = conn.query_row(
            "SELECT f.path FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id JOIN files f ON f.id=callee.file_id WHERE caller.name='entry' AND callee.name='chosen' AND callee.is_defined=1",
            [], |row| row.get(0),
        ).unwrap();
        assert!(path.ends_with("a-sdk/vendor/api.h"), "{path}");
    }
}

#[test]
fn applying_candidates_rejects_roots_that_shadow_selected_headers_across_retries() {
    // Exercise both a conflict within one batch and a conflict introduced by
    // a newly reached header on the next pass.
    for nested in [false, true] {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        // Orphan-header indexing must not expose the nested include before
        // main.c has reached foo.h on retry.
        let nested_include = if nested {
            "#ifdef FROM_MAIN\n#include <b/bar.h>\n#endif\n"
        } else {
            ""
        };
        std::fs::write(
            root.join("a/b/foo.h"),
            format!("{nested_include}#define SELECTED chosen\nstatic void chosen(void) {{}}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("a/foo.h"),
            "#define SELECTED alternative\nstatic void alternative(void) {}\n",
        )
        .unwrap();
        std::fs::write(root.join("a/b/bar.h"), "#pragma once\n").unwrap();
        let direct_include = if nested { "" } else { "#include <b/bar.h>\n" };
        std::fs::write(
            root.join("main.c"),
            format!("#define FROM_MAIN 1\n#include <foo.h>\n{direct_include}void entry(void) {{ SELECTED(); }}\n"),
        )
        .unwrap();
        write_commands(root, &["main.c"], &[]);

        let output = root.join("out.db");
        for options in [
            vec!["--apply-include-candidates", "--jobs", "1"],
            vec!["--apply-include-candidates", "--jobs", "4", "--full-export"],
        ] {
            let stderr = analyze(root, &output, &options);
            if !nested {
                assert!(stderr.contains("skipping conflicting root"), "{stderr}");
                assert!(stderr.contains("foo.h would resolve to"), "{stderr}");
                assert!(stderr.contains("a/foo.h, selected"), "{stderr}");
                assert!(stderr.contains("skipped candidate: b/bar.h ->"), "{stderr}");
            }
            assert!(
                stderr.contains("applying 1 candidate include root(s) (pass 1)"),
                "{stderr}"
            );
            // Once a/b has been applied, a can safely be appended after it.
            assert!(
                stderr.contains("applying 1 candidate include root(s) (pass 2)"),
                "{stderr}"
            );
            assert!(!stderr.contains("(pass 3)"), "{stderr}");
            assert_eq!(missing_count(&output), 0);
            let conn = rusqlite::Connection::open(&output).unwrap();
            let callees: Vec<(String, String)> = conn
                .prepare("SELECT callee.name, f.path FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id JOIN files f ON f.id=callee.file_id WHERE caller.name='entry' AND callee.is_defined=1 ORDER BY callee.name, f.path")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(callees.len(), 1, "{callees:?}");
            assert_eq!(callees[0].0, "chosen");
            assert!(callees[0].1.ends_with("a/b/foo.h"), "{callees:?}");
        }
    }
}

#[test]
fn applying_candidates_can_shadow_previously_resolved_build_headers() {
    for search_class in ["-I", "-isystem", "-idirafter"] {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        for name in ["candidate", "configured"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        std::fs::write(root.join("candidate/missing.h"), "#pragma once\n").unwrap();
        std::fs::write(
            root.join("configured/existing.h"),
            "static void configured(void) {}\n#define SELECTED configured\n",
        )
        .unwrap();
        std::fs::write(
            root.join("candidate/existing.h"),
            "static void candidate(void) {}\n#define SELECTED candidate\n",
        )
        .unwrap();
        std::fs::write(
            root.join("main.c"),
            "#include <missing.h>\n#include <existing.h>\nvoid entry(void) { SELECTED(); }\n",
        )
        .unwrap();
        std::fs::write(root.join("compile_commands.json"), serde_json::json!([
            {"directory": root, "file": "main.c", "arguments": ["trace-missing-target-compiler", search_class, "configured", "-c", "main.c"]}
        ]).to_string()).unwrap();

        for (options, expected) in [
            (vec![], "configured"),
            (
                vec!["--apply-include-candidates", "--jobs", "1"],
                "candidate",
            ),
            (
                vec!["--apply-include-candidates", "--jobs", "4", "--full-export"],
                "candidate",
            ),
        ] {
            let output = root.join("out.db");
            let stderr = analyze(root, &output, &options);
            assert_eq!(
                missing_count(&output),
                if options.is_empty() { 1 } else { 0 },
                "{search_class}: {stderr}"
            );
            let conn = rusqlite::Connection::open(&output).unwrap();
            let callees: Vec<String> = conn.prepare(
                "SELECT callee.name FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id WHERE caller.name='entry' AND callee.is_defined=1 ORDER BY callee.name"
            ).unwrap().query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
            assert_eq!(callees, [expected], "{search_class}: {stderr}");
        }
    }
}

#[test]
fn applying_candidates_respects_per_tu_system_and_after_search_classes() {
    for search_class in ["-isystem", "-idirafter"] {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir(root.join("c")).unwrap();
        std::fs::write(root.join("c/extra.h"), "#pragma once\n").unwrap();
        std::fs::write(
            root.join("a/b/foo.h"),
            "#ifdef FROM_MAIN\n#include <extra.h>\n#endif\n#define SELECTED chosen\nstatic void chosen(void) {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("a/foo.h"),
            "#define SELECTED alternative\nstatic void alternative(void) {}\n",
        )
        .unwrap();
        std::fs::write(root.join("a/b/bar.h"), "#pragma once\n").unwrap();
        std::fs::write(
            root.join("main.c"),
            "#define FROM_MAIN 1\n#include <foo.h>\n#include <b/bar.h>\nvoid entry(void) { SELECTED(); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("second.c"),
            "#include <foo.h>\nvoid second_entry(void) { SELECTED(); }\n",
        )
        .unwrap();
        let commands = serde_json::json!([
            {"directory": root, "file": "main.c", "arguments": ["trace-missing-target-compiler", "-c", "main.c"]},
            {"directory": root, "file": "second.c", "arguments": ["trace-missing-target-compiler", search_class, "a/b", "-c", "second.c"]}
        ]);
        std::fs::write(root.join("compile_commands.json"), commands.to_string()).unwrap();
        let output = root.join("out.db");
        for options in [
            vec!["--apply-include-candidates", "--jobs", "1"],
            vec!["--apply-include-candidates", "--jobs", "4", "--full-export"],
        ] {
            let stderr = analyze(root, &output, &options);
            let conn = rusqlite::Connection::open(&output).unwrap();
            let callees: Vec<(String, String, String)> = conn
                .prepare("SELECT caller.name, callee.name, f.path FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id JOIN files f ON f.id=callee.file_id WHERE caller.name IN ('entry', 'second_entry') AND callee.is_defined=1 ORDER BY caller.name, callee.name, f.path")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(callees.len(), 2, "{search_class}: {callees:?}\n{stderr}");
            for (caller, callee, path) in &callees {
                assert_eq!(
                    callee, "chosen",
                    "{search_class}: {caller}: {callees:?}\n{stderr}"
                );
                assert!(path.ends_with("a/b/foo.h"), "{path}");
            }
            assert_eq!(missing_count(&output), 1);
            assert!(stderr.contains("skipped candidate: b/bar.h ->"), "{stderr}");
            // The first pass reports the fallback search conflict. The
            // second pass reports second.c's search order; reaching extra.h
            // then forces a third check of that same per-TU conflict.
            assert_eq!(
                stderr.matches("skipping conflicting root").count(),
                2,
                "{stderr}"
            );
            assert_eq!(
                stderr.matches("skipped candidate: b/bar.h ->").count(),
                2,
                "{stderr}"
            );
            assert!(
                stderr
                    .matches(&format!(
                        "effective include search for TU: {}",
                        trace_ir::canonicalize(&root.join("second.c")).display()
                    ))
                    .count()
                    == 1,
                "{stderr}"
            );
            assert!(
                stderr.contains("applying 1 candidate include root(s) (pass 1)"),
                "{stderr}"
            );
            assert!(
                stderr.contains("applying 1 candidate include root(s) (pass 2)"),
                "{stderr}"
            );
            assert!(!stderr.contains("(pass 3)"), "{stderr}");
        }
    }
}

#[test]
fn applying_candidates_normalizes_dot_components_and_preserves_the_requested_spelling() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    std::fs::create_dir_all(root.join("sdk/vendor")).unwrap();
    std::fs::write(
        root.join("sdk/vendor/api.h"),
        "static void chosen(void) {}\n",
    )
    .unwrap();
    write_commands(root, &["main.c"], &[]);
    let output = root.join("out.db");
    for spelling in ["./vendor/api.h", "././vendor/./api.h"] {
        std::fs::write(
            root.join("main.c"),
            format!("#include <{spelling}>\nvoid entry(void) {{ chosen(); }}\n"),
        )
        .unwrap();
        let stderr = analyze(root, &output, &[]);
        assert_eq!(missing_count(&output), 1);
        assert!(
            stderr.contains(&format!("{spelling}: 1 location(s)")),
            "{stderr}"
        );
        assert!(stderr.contains("sdk/vendor/api.h"), "{stderr}");
        assert!(
            stderr.contains(&format!(
                "candidate include root: {}",
                trace_ir::canonicalize(&root.join("sdk")).display()
            )),
            "{stderr}"
        );
        let stderr = analyze(root, &output, &["--apply-include-candidates"]);
        assert!(
            stderr.contains("applying 1 candidate include root(s) (pass 1)"),
            "{stderr}"
        );
        assert!(!stderr.contains("(pass 2)"), "{stderr}");
        assert_eq!(missing_count(&output), 0);
        let conn = rusqlite::Connection::open(&output).unwrap();
        let path: String = conn.query_row(
            "SELECT f.path FROM call_edges e JOIN functions caller ON caller.id=e.caller_fn_id JOIN functions callee ON callee.id=e.callee_fn_id JOIN files f ON f.id=callee.file_id WHERE caller.name='entry' AND callee.name='chosen' AND callee.is_defined=1",
            [], |row| row.get(0),
        ).unwrap();
        assert!(path.ends_with("sdk/vendor/api.h"), "{path}");
    }
}

#[test]
fn applying_candidates_includes_headers_beyond_the_display_limit_and_keeps_other_warnings() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    let mut source = String::new();
    for n in 0..12 {
        let directory = root.join(format!("sdk{n:02}/vendor"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(format!("api{n:02}.h")), "#pragma once\n").unwrap();
        source.push_str(&format!("#include <vendor/api{n:02}.h>\n"));
    }
    source.push_str("#include <unavailable.h>\n#unknown_directive\nvoid entry(void) {}\n");
    std::fs::write(root.join("main.c"), source).unwrap();
    write_commands(root, &["main.c"], &[]);
    let output = root.join("out.db");
    let stderr = analyze(root, &output, &[]);
    assert!(stderr.contains("13 distinct, 13 diagnostic locations"));
    assert!(!stderr.contains("vendor/api11.h:"));
    let stderr = analyze(root, &output, &["--apply-include-candidates"]);
    assert!(
        stderr.contains("applying 12 candidate include root(s) (pass 1)"),
        "{stderr}"
    );
    assert_eq!(missing_count(&output), 1);
    assert!(stderr.contains("unavailable.h: 1 location(s)"), "{stderr}");
    let conn = rusqlite::Connection::open(&output).unwrap();
    let missing: (String, i64) = conn.query_row(
        "SELECT message,line FROM diagnostics WHERE message LIKE 'include file not found, skipping:%'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(
        missing,
        ("include file not found, skipping: unavailable.h".into(), 13)
    );
    let unrelated: i64 = conn
        .query_row(
            "SELECT count(*) FROM diagnostics WHERE message LIKE 'unknown directive%' AND line=14",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unrelated, 1);
}

#[test]
fn applying_candidates_retries_new_nested_dependency_includes_and_forced_headers() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("project");
    let sdk = scratch.path().join("sdk");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(sdk.join("outer/vendor")).unwrap();
    std::fs::create_dir_all(sdk.join("inner/detail")).unwrap();
    std::fs::write(
        sdk.join("outer/vendor/api.h"),
        "#include <detail/inner.h>\nstatic void sdk_function(void) {}\n",
    )
    .unwrap();
    std::fs::write(sdk.join("inner/detail/inner.h"), "#define SDK_READY 1\n").unwrap();
    std::fs::write(
        sdk.join("dependency.c"),
        "void dependency_must_not_be_indexed(void) {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <vendor/api.h>\n#if SDK_READY\nvoid entry(void) { sdk_function(); }\n#endif\n",
    )
    .unwrap();
    write_commands(&root, &["main.c"], &["-include", "vendor/api.h"]);
    let output = scratch.path().join("out.db");
    let stderr = analyze(
        &root,
        &output,
        &["--apply-include-candidates", "--dep", sdk.to_str().unwrap()],
    );
    assert!(stderr.contains("(pass 1)"), "{stderr}");
    assert!(stderr.contains("(pass 2)"), "{stderr}");
    assert!(!stderr.contains("(pass 3)"), "{stderr}");
    assert_eq!(missing_count(&output), 0);
    let conn = rusqlite::Connection::open(&output).unwrap();
    let sdk_function: (bool, bool) = conn
        .query_row(
            "SELECT is_dep,is_defined FROM functions WHERE name='sdk_function'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(sdk_function, (true, false));
    let definitions: Vec<String> = conn
        .prepare("SELECT name FROM functions WHERE is_defined=1 ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(definitions, vec!["entry"]);
}

#[test]
fn applying_candidates_without_matches_does_not_repeat_indexing() {
    let scratch = tempfile::tempdir().unwrap();
    let output = scratch.path().join("out.db");
    let stderr = analyze(
        &fixture("preproc/missing_headers"),
        &output,
        &["--apply-include-candidates"],
    );
    assert!(!stderr.contains("include-candidates: applying"), "{stderr}");
    assert_eq!(missing_count(&output), 7);
}

#[test]
fn applying_candidates_does_not_bypass_the_local_input_guard() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("project");
    let sdk = scratch.path().join("sdk");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(sdk.join("vendor")).unwrap();
    std::fs::write(sdk.join("vendor/api.h"), "void sdk_function(void);\n").unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include <vendor/api.h>\nvoid entry(void) {}\n",
    )
    .unwrap();
    write_commands(&root, &["main.c"], &[]);
    let output = scratch.path().join("out.db");
    let result = Command::new(env!("CARGO_BIN_EXE_trace"))
        .arg("analyze")
        .arg(&root)
        .arg("-o")
        .arg(&output)
        .arg("--dep")
        .arg(&sdk)
        .args(["--apply-include-candidates", "--require-local-inputs"])
        .output()
        .unwrap();
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(!result.status.success(), "{stderr}");
    assert!(stderr.contains("include-candidates: applying"), "{stderr}");
    assert!(
        stderr.contains("external analysis input cannot be verified"),
        "{stderr}"
    );
    assert!(!output.exists());
}

#[test]
fn local_input_guard_stops_before_any_further_candidate_retry() {
    for external_on_retry in [false, true] {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().join("project");
        std::fs::create_dir_all(root.join("headers/vendor")).unwrap();
        std::fs::create_dir_all(root.join("more/next")).unwrap();
        let external = scratch.path().join("external.h");
        std::fs::write(&external, "#define EXTERNAL 1\n").unwrap();
        let initial_include = if external_on_retry {
            String::new()
        } else {
            format!("#include \"{}\"\n", external.display())
        };
        let main = format!(
            "#define FROM_TU 1\n{initial_include}#include <vendor/api.h>\nvoid entry(void) {{}}\n"
        );
        std::fs::write(root.join("main.c"), main).unwrap();
        // The orphan-header pass does not read the external input. The first
        // candidate retry reaches it with the TU's macro environment, and exposes
        // another missing header that would otherwise trigger a second retry.
        std::fs::write(
            root.join("headers/vendor/api.h"),
            format!(
                "#ifdef FROM_TU\n#include \"{}\"\n#include <next/inner.h>\n#endif\n",
                external.display()
            ),
        )
        .unwrap();
        std::fs::write(root.join("more/next/inner.h"), "#pragma once\n").unwrap();
        write_commands(&root, &["main.c"], &[]);
        let output = scratch.path().join("out.db");
        let result = Command::new(env!("CARGO_BIN_EXE_trace"))
            .arg("analyze")
            .arg(&root)
            .arg("-o")
            .arg(&output)
            .args(["--apply-include-candidates", "--require-local-inputs"])
            .env_remove("CPATH")
            .env_remove("C_INCLUDE_PATH")
            .env_remove("CPLUS_INCLUDE_PATH")
            .output()
            .unwrap();
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(!result.status.success(), "{stderr}");
        assert_eq!(stderr.contains("(pass 1)"), external_on_retry, "{stderr}");
        assert!(!stderr.contains("(pass 2)"), "{stderr}");
        assert!(
            stderr.contains("external analysis input cannot be verified"),
            "{stderr}"
        );
        assert!(stderr.contains(external.to_str().unwrap()), "{stderr}");
        assert!(!output.exists());
    }
}
