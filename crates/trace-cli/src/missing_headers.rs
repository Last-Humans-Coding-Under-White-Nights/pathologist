//! Missing-header summaries and opt-in candidate include roots.
//! See docs/PREPROCESSOR.md, "Missing-header guidance".

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Component, Path, PathBuf};
use trace_ir::{DiagnosticSeverity, Program};
use trace_preproc::PreprocessOptions;

const DISPLAY_LIMIT: usize = 10;

#[derive(Default)]
struct Header<'a> {
    count: usize,
    example: Option<(&'a Path, u32)>,
    candidate: Option<&'a Path>,
}

fn missing_header(message: &str) -> Option<&str> {
    message
        .strip_prefix("include file not found, skipping: ")
        .or_else(|| message.strip_prefix("forced include file not found, skipping: "))
        .filter(|name| !name.is_empty())
}

fn headers(program: &Program) -> BTreeMap<&str, Header<'_>> {
    let mut headers: BTreeMap<&str, Header<'_>> = BTreeMap::new();
    for diagnostic in &program.diagnostics {
        if diagnostic.stage != "preprocess" || diagnostic.severity != DiagnosticSeverity::Warning {
            continue;
        }
        let Some(name) = missing_header(&diagnostic.message) else {
            continue;
        };
        let header = headers.entry(name).or_default();
        header.count += 1;
        if let Some(file) = diagnostic
            .file
            .and_then(|file| program.symbols.files.get(file.0 as usize))
        {
            let location = (file.path.as_path(), diagnostic.line);
            if header.example.is_none_or(|example| location < example) {
                header.example = Some(location);
            }
        }
    }
    if headers.is_empty() {
        return headers;
    }

    // Index requested basenames once, then visit known files once. Full suffix
    // matching avoids proposing a different vendor's same-named header.
    let mut by_basename: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for &name in headers.keys() {
        let normalized = include_path(name);
        if let Some(basename) = normalized.file_name() {
            by_basename
                .entry(basename.to_owned())
                .or_default()
                .push((name, normalized));
        }
    }
    for file in &program.symbols.files {
        let Some(names) = file.path.file_name().and_then(|name| by_basename.get(name)) else {
            continue;
        };
        for (name, normalized) in names {
            if file.path.ends_with(normalized) {
                let header = headers.get_mut(name).unwrap();
                let path = file.path.as_path();
                if header.candidate.is_none_or(|previous| path < previous) {
                    header.candidate = Some(path);
                }
            }
        }
    }

    headers
}

fn include_path(name: &str) -> PathBuf {
    // Remove redundant `.` components, but retain ParentDir: folding `..`
    // could change symlink traversal.
    Path::new(name)
        .components()
        .filter(|component| *component != Component::CurDir)
        .collect()
}

fn include_root<'a>(name: &str, candidate: &'a Path) -> Option<&'a Path> {
    let name = include_path(name);
    if name.is_absolute() {
        return None;
    }
    candidate.ancestors().nth(name.components().count())
}

struct Candidate {
    file: PathBuf,
    root: PathBuf,
}

#[derive(Default)]
pub(super) struct IncludeCandidates {
    // Keep selections from earlier passes: a later root must not shadow a
    // header that a previous retry successfully repaired.
    selected: BTreeMap<String, Candidate>,
    // Recheck rejected roots: earlier retries can change their precedence.
    // Suppress only identical reports, not subsequent conflict validation.
    reported_conflicts: BTreeSet<String>,
    include_searches: trace_parse::CompilationIncludeSearches,
}

impl IncludeCandidates {
    pub(super) fn additional_roots(
        &mut self,
        program: &Program,
        opts: &PreprocessOptions,
    ) -> Result<(Vec<PathBuf>, String), String> {
        let mut roots = BTreeSet::new();
        for (name, header) in headers(program) {
            if let Some(file) = header.candidate {
                if let Some(root) = include_root(name, file) {
                    let selected =
                        self.selected
                            .entry(name.to_owned())
                            .or_insert_with(|| Candidate {
                                file: trace_ir::canonicalize(file),
                                root: trace_ir::canonicalize(root),
                            });
                    roots.insert(selected.root.clone());
                }
            }
        }
        let included: BTreeSet<_> = opts
            .include_paths
            .iter()
            .map(|path| trace_ir::canonicalize(path))
            .collect();
        roots.retain(|root| !included.contains(root));
        let mut diagnostics = String::new();
        let files: Vec<_> = program
            .symbols
            .files
            .iter()
            .filter(|file| !file.is_dep)
            .map(|file| file.path.clone())
            .collect();
        // Validate each command's normalized search order with the prospective
        // CLI roots appended in path order. Remove the first offending new
        // root, then recheck all selections, including those whose roots were
        // removed: leaving another root could still pick the wrong file.
        loop {
            if roots.is_empty() {
                break;
            }
            let mut prospective = opts.clone();
            prospective.include_paths.extend(roots.iter().cloned());
            let searches = self
                .include_searches
                .searches(&program.root, &prospective, &files)?;
            let conflict = self.selected.iter().find_map(|(name, selected)| {
                searches.iter().find_map(|search| {
                    search
                        .paths
                        .iter()
                        .find_map(|root| {
                            let file = root.join(name);
                            trace_ir::is_file_cached(&file)
                                .then(|| (root, trace_ir::canonicalize(&file)))
                        })
                        .and_then(|(root, file)| {
                            (file != selected.file)
                                .then(|| (name, selected, root.clone(), file, &search.source))
                        })
                })
            });
            let Some((name, selected, root, file, source)) = conflict else {
                break;
            };
            let removed = roots.remove(&root);
            let action = if removed {
                "skipping conflicting root"
            } else {
                "existing include path conflicts"
            };
            let mut report = String::new();
            writeln!(
                report,
                "warning: include-candidates: {action} {}: {name} would resolve to {}, selected {}",
                root.display(),
                file.display(),
                selected.file.display()
            )
            .unwrap();
            if let Some(source) = source {
                writeln!(
                    report,
                    "  effective include search for TU: {}",
                    source.display()
                )
                .unwrap();
            }
            if !removed {
                // Explicit paths cannot be removed or reordered. Refuse this
                // batch if they prevent preserving a previous selection.
                report.push_str(
                    "  existing include paths conflict; skipping all new candidate roots\n",
                );
                roots.clear();
            } else {
                for (name, candidate) in &self.selected {
                    if candidate.root == root {
                        writeln!(
                            report,
                            "  skipped candidate: {name} -> {}",
                            candidate.file.display()
                        )
                        .unwrap();
                    }
                }
            }
            if self.reported_conflicts.insert(report.clone()) {
                diagnostics.push_str(&report);
            }
        }
        Ok((roots.into_iter().collect(), diagnostics))
    }
}

pub(super) fn summary(program: &Program, system_includes: bool) -> String {
    let headers = headers(program);
    if headers.is_empty() {
        return String::new();
    }

    let mut ranked: Vec<_> = headers.iter().collect();
    ranked.sort_by(|(a, x), (b, y)| y.count.cmp(&x.count).then_with(|| a.cmp(b)));
    let count: usize = headers.values().map(|header| header.count).sum();
    let mut output = format!(
        "warning: unresolved headers: {} distinct, {count} diagnostic locations; missing declarations may reduce analysis coverage\n",
        headers.len()
    );
    for (name, header) in ranked.iter().take(DISPLAY_LIMIT) {
        write!(output, "  {name}: {} location(s)", header.count).unwrap();
        if let Some((path, line)) = header.example {
            write!(output, " (e.g. {}:{line})", path.display()).unwrap();
        }
        output.push('\n');
        if let Some(path) = header.candidate {
            writeln!(output, "    matching indexed file: {}; check per-TU --include paths and search restrictions", path.display()).unwrap();
            if let Some(root) = include_root(name, path) {
                writeln!(
                    output,
                    "    candidate include root: {} (verify it belongs to the target)",
                    root.display()
                )
                .unwrap();
            }
        } else {
            output.push_str(
                "    not found in search paths; availability outside indexed files is unknown\n",
            );
        }
    }
    if headers.len() > DISPLAY_LIMIT {
        writeln!(
            output,
            "  ... {} more unresolved header spelling(s)",
            headers.len() - DISPLAY_LIMIT
        )
        .unwrap();
    }
    output.push_str("  Use target build metadata: --compile-commands <path> (auto: <root>/compile_commands.json or <root>/build/compile_commands.json), or compile_flags.txt for shared flags.\n");
    if headers.keys().any(|name| is_standard_header(name)) {
        if system_includes {
            output.push_str("  Standard C/C++ headers remain unresolved with --system-includes: check the target compiler, sysroot, SDK installation and per-TU command coverage.\n");
        } else {
            output.push_str("  Standard C/C++ headers: use --system-includes with the target compiler and sysroot recorded in compilation metadata.\n");
        }
        output.push_str("  Units without usable compilation commands, shared flags and orphan headers use host GCC/Clang for --system-includes; use this fallback only if it matches the target. Host headers must not substitute for the target SDK.\n");
    }
    output.push_str("  SDK/dependency headers: obtain the target SDK/dependency, use --dep <root> for declarations and --include <directory> for its search root; generated headers need the target build's generated include paths.\n");
    if headers.keys().any(|name| is_internal_kernel_header(name)) {
        output.push_str("  Internal Linux kernel headers require the matching configured kernel build tree, generated headers and architecture paths; userspace /usr/include/linux UAPI headers cannot replace them.\n");
    }
    output.push_str(
        "  All original diagnostics are retained in the output database's diagnostics table.\n",
    );
    output
}

fn is_internal_kernel_header(name: &str) -> bool {
    let normalized = include_path(name);
    let Some(name) = normalized.to_str() else {
        return false;
    };
    matches!(
        name,
        "linux/device.h"
            | "linux/dma-mapping.h"
            | "linux/of.h"
            | "linux/module.h"
            | "linux/slab.h"
            | "linux/platform_device.h"
    )
}

fn is_standard_header(name: &str) -> bool {
    let normalized = include_path(name);
    let Some(name) = normalized.to_str() else {
        return false;
    };
    // Known standard spellings, rather than guessing from angle brackets or a
    // missing .h suffix (both are also common for external SDK headers).
    matches!(
        name,
        "assert.h"
            | "complex.h"
            | "ctype.h"
            | "errno.h"
            | "fenv.h"
            | "float.h"
            | "inttypes.h"
            | "iso646.h"
            | "limits.h"
            | "locale.h"
            | "math.h"
            | "setjmp.h"
            | "signal.h"
            | "stdalign.h"
            | "stdarg.h"
            | "stdatomic.h"
            | "stdbit.h"
            | "stdbool.h"
            | "stdckdint.h"
            | "stddef.h"
            | "stdint.h"
            | "stdio.h"
            | "stdlib.h"
            | "stdnoreturn.h"
            | "string.h"
            | "tgmath.h"
            | "threads.h"
            | "time.h"
            | "uchar.h"
            | "wchar.h"
            | "wctype.h"
            | "algorithm"
            | "any"
            | "array"
            | "atomic"
            | "bit"
            | "bitset"
            | "cassert"
            | "ccomplex"
            | "cctype"
            | "cerrno"
            | "cfenv"
            | "cfloat"
            | "charconv"
            | "chrono"
            | "cinttypes"
            | "climits"
            | "clocale"
            | "cmath"
            | "codecvt"
            | "compare"
            | "complex"
            | "concepts"
            | "condition_variable"
            | "coroutine"
            | "csetjmp"
            | "csignal"
            | "cstdarg"
            | "cstdalign"
            | "cstdbool"
            | "cstddef"
            | "cstdint"
            | "cstdnoreturn"
            | "cstdio"
            | "cstdlib"
            | "cstring"
            | "ctime"
            | "cuchar"
            | "cwchar"
            | "cwctype"
            | "deque"
            | "exception"
            | "execution"
            | "expected"
            | "filesystem"
            | "format"
            | "flat_map"
            | "flat_set"
            | "forward_list"
            | "fstream"
            | "functional"
            | "future"
            | "initializer_list"
            | "iomanip"
            | "ios"
            | "iosfwd"
            | "iostream"
            | "istream"
            | "iterator"
            | "latch"
            | "limits"
            | "list"
            | "locale"
            | "map"
            | "memory"
            | "memory_resource"
            | "mutex"
            | "new"
            | "numbers"
            | "numeric"
            | "optional"
            | "ostream"
            | "print"
            | "queue"
            | "random"
            | "ranges"
            | "ratio"
            | "regex"
            | "scoped_allocator"
            | "semaphore"
            | "set"
            | "shared_mutex"
            | "source_location"
            | "span"
            | "sstream"
            | "spanstream"
            | "stack"
            | "stacktrace"
            | "stdexcept"
            | "stop_token"
            | "streambuf"
            | "string"
            | "string_view"
            | "strstream"
            | "system_error"
            | "syncstream"
            | "thread"
            | "tuple"
            | "type_traits"
            | "typeindex"
            | "typeinfo"
            | "unordered_map"
            | "unordered_set"
            | "utility"
            | "valarray"
            | "variant"
            | "vector"
            | "version"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use trace_ir::Diagnostic;

    #[test]
    fn rejected_roots_cannot_leave_other_roots_selecting_wrong_files() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        for directory in ["a/vendor", "z/vendor"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        // The twins exist on disk even when they are outside the file index.
        for name in ["a/foo.h", "a/vendor/api.h", "z/foo.h", "z/vendor/api.h"] {
            std::fs::write(root.join(name), "#pragma once\n").unwrap();
        }
        let mut program = Program::new(root.to_path_buf());
        for name in ["a/foo.h", "z/vendor/api.h"] {
            program.symbols.add_file(root.join(name));
        }
        for name in ["foo.h", "vendor/api.h"] {
            program.add_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: None,
                line: 1,
                message: format!("include file not found, skipping: {name}"),
                stage: "preprocess".into(),
            });
        }
        let mut candidates = IncludeCandidates::default();
        let (roots, diagnostics) = candidates
            .additional_roots(&program, &PreprocessOptions::default())
            .unwrap();
        assert!(roots.is_empty(), "{roots:?}");
        assert!(
            diagnostics.contains("vendor/api.h would resolve to"),
            "{diagnostics}"
        );
        assert!(
            diagnostics.contains("foo.h would resolve to"),
            "{diagnostics}"
        );
        assert!(
            diagnostics.contains("skipped candidate: foo.h ->"),
            "{diagnostics}"
        );
        assert!(
            diagnostics.contains("skipped candidate: vendor/api.h ->"),
            "{diagnostics}"
        );
        // A retry must still reject both roots, without repeating its report.
        assert_eq!(
            candidates
                .additional_roots(&program, &PreprocessOptions::default())
                .unwrap(),
            (Vec::new(), String::new())
        );
        program.diagnostics.reverse();
        assert_eq!(
            (roots, diagnostics),
            IncludeCandidates::default()
                .additional_roots(&program, &PreprocessOptions::default())
                .unwrap()
        );
    }

    #[test]
    fn existing_include_conflicts_report_the_path_and_skip_only_the_new_batch() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        for directory in ["chosen", "existing", "new"] {
            std::fs::create_dir(root.join(directory)).unwrap();
        }
        for name in ["chosen/foo.h", "existing/foo.h", "new/bar.h"] {
            std::fs::write(root.join(name), "#pragma once\n").unwrap();
        }
        let mut program = Program::new(root.to_path_buf());
        for name in ["chosen/foo.h", "new/bar.h"] {
            program.symbols.add_file(root.join(name));
        }
        for name in ["foo.h", "bar.h"] {
            program.add_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: None,
                line: 1,
                message: format!("include file not found, skipping: {name}"),
                stage: "preprocess".into(),
            });
        }
        let opts = PreprocessOptions {
            include_paths: vec![root.join("existing")],
            ..Default::default()
        };
        let (roots, diagnostics) = IncludeCandidates::default()
            .additional_roots(&program, &opts)
            .unwrap();
        assert!(roots.is_empty(), "{roots:?}");
        assert!(
            diagnostics.contains(&format!(
                "existing include path conflicts {}: foo.h would resolve to",
                root.join("existing").display()
            )),
            "{diagnostics}"
        );
        assert!(diagnostics.contains("skipping all new candidate roots"));
        assert!(!diagnostics.contains("skipping conflicting root"));
        assert!(opts.include_paths[0].join("foo.h").is_file());
    }

    #[test]
    fn invalid_diagnostic_file_ids_keep_the_header_count_without_an_example() {
        let mut program = Program::new(PathBuf::from("/project"));
        program.add_diagnostic(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: Some(trace_ir::FileId(u32::MAX)),
            line: 42,
            message: "include file not found, skipping: stdio.h".into(),
            stage: "preprocess".into(),
        });
        let text = summary(&program, false);
        assert!(
            text.contains("1 distinct, 1 diagnostic locations"),
            "{text}"
        );
        assert!(text.contains("stdio.h: 1 location(s)"), "{text}");
        assert!(!text.contains("(e.g."), "{text}");
    }

    #[test]
    fn dotted_standard_and_kernel_headers_get_guidance_without_folding_parents() {
        for name in [
            "./stdio.h",
            "././vector",
            "./linux/device.h",
            "linux/./of.h",
        ] {
            let mut program = Program::new(PathBuf::from("/project"));
            program.add_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: None,
                line: 1,
                message: format!("include file not found, skipping: {name}"),
                stage: "preprocess".into(),
            });
            let text = summary(&program, false);
            assert!(text.contains(&format!("{name}: 1 location(s)")), "{text}");
            if name.contains("linux") {
                assert!(
                    text.contains("Internal Linux kernel headers require"),
                    "{text}"
                );
            } else {
                assert!(
                    text.contains("Standard C/C++ headers: use --system-includes"),
                    "{text}"
                );
                assert!(
                    summary(&program, true).contains("remain unresolved with --system-includes")
                );
            }
        }
        assert!(!is_standard_header("vendor/../stdio.h"));
        assert!(!is_internal_kernel_header("vendor/../linux/device.h"));
    }

    #[test]
    fn include_paths_remove_dots_without_folding_parent_components() {
        assert_eq!(
            include_path("././vendor/./api.h"),
            Path::new("vendor/api.h")
        );
        assert_eq!(
            include_root("././vendor/./api.h", Path::new("/sdk/vendor/api.h")),
            Some(Path::new("/sdk"))
        );
        assert_eq!(
            include_path("./vendor/../api.h"),
            Path::new("vendor/../api.h")
        );
        assert!(!Path::new("/sdk/api.h").ends_with(include_path("./vendor/../api.h")));
        assert_eq!(
            include_root("/vendor/api.h", Path::new("/vendor/api.h")),
            None
        );
    }

    #[test]
    fn summary_is_bounded_and_deterministic_and_ignores_other_warnings() {
        let mut program = Program::new(PathBuf::from("/project"));
        let file = program.symbols.add_file(PathBuf::from("/project/main.cpp"));
        for n in (0..12).rev() {
            program.add_diagnostic(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                file: Some(file),
                line: n + 1,
                message: format!("include file not found, skipping: header{n:02}.h"),
                stage: "preprocess".into(),
            });
        }
        program.add_diagnostic(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: Some(file),
            line: 20,
            message: "resolved include expanded to nothing (guard already defined?)".into(),
            stage: "preprocess".into(),
        });
        let text = summary(&program, false);
        assert!(
            text.contains("12 distinct, 12 diagnostic locations"),
            "{text}"
        );
        assert!(text.contains("2 more unresolved"));
        assert!(text.find("header00.h").unwrap() < text.find("header09.h").unwrap());
        assert!(!text.contains("header10.h"));
        assert!(!text.contains("Standard C/C++"));
        program.diagnostics.reverse();
        assert_eq!(text, summary(&program, false));
    }

    #[test]
    fn forced_includes_and_complete_spelling_evidence_are_reported() {
        let mut program = Program::new(PathBuf::from("/project"));
        program.symbols.add_file(PathBuf::from("/dep/other/api.h"));
        program.add_diagnostic(Diagnostic {
            severity: DiagnosticSeverity::Warning,
            file: None,
            line: 1,
            message: "forced include file not found, skipping: vendor/api.h".into(),
            stage: "preprocess".into(),
        });
        assert!(!summary(&program, false).contains("matching indexed file"));
        program.symbols.add_file(PathBuf::from("/dep/vendor/api.h"));
        assert!(summary(&program, false).contains("matching indexed file: /dep/vendor/api.h"));
        assert!(summary(&program, false).contains("candidate include root: /dep"));
        program
            .symbols
            .add_file(PathBuf::from("/another/vendor/api.h"));
        assert!(summary(&program, false).contains("matching indexed file: /another/vendor/api.h"));
        program.diagnostics[0].stage = "parse".into();
        assert!(summary(&program, false).is_empty());
    }
}
