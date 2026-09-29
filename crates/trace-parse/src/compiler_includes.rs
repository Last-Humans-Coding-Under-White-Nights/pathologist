//! Query driver-owned default header directories without preprocessing project code.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use trace_preproc::Language;

#[derive(Default)]
pub(super) struct CompilerIncludes {
    drivers: HashMap<(String, PathBuf), Option<PathBuf>>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(super) struct CompilerSearch {
    pub paths: Vec<PathBuf>,
    pub target_defines: Vec<(String, String)>,
}

type ProbeKey = (
    PathBuf,
    PathBuf,
    Language,
    Vec<String>,
    Vec<Option<std::ffi::OsString>>,
);
static PROBE_PATHS: OnceLock<Mutex<HashMap<ProbeKey, CompilerSearch>>> = OnceLock::new();

impl CompilerIncludes {
    /// The driver is the real compiler argument, past ccache-like launchers.
    /// Its executable is located once; each distinct language and target
    /// configuration is probed once, before parallel indexing begins.
    pub(super) fn for_command(
        &mut self,
        args: &[String],
        directory: &Path,
        language: Language,
        system_paths: &[PathBuf],
    ) -> CompilerSearch {
        let Some(driver) = args.get(crate::compile_commands::driver_index(args)) else {
            return CompilerSearch::default();
        };
        let Some(executable) = self.locate(driver, directory) else {
            return CompilerSearch::default();
        };
        let mut flags = probe_flags(args, directory);
        append_system_paths(&mut flags, system_paths);
        self.probe(executable, directory, language, flags)
    }

    /// A bare tree uses one compiler for all translation units. Prefer `gcc`
    /// when it supplies both C and C++ search lists (`-x c++` selects the
    /// latter), then try `clang`.
    pub(super) fn for_project(
        &mut self,
        directory: &Path,
        system_paths: &[PathBuf],
    ) -> Option<(CompilerSearch, CompilerSearch)> {
        self.for_project_drivers(["gcc", "clang"], directory, system_paths)
    }

    fn for_project_drivers<'a>(
        &mut self,
        drivers: impl IntoIterator<Item = &'a str>,
        directory: &Path,
        system_paths: &[PathBuf],
    ) -> Option<(CompilerSearch, CompilerSearch)> {
        let mut flags = Vec::new();
        append_system_paths(&mut flags, system_paths);
        let mut partial = None;
        for driver in drivers {
            let Some(executable) = self.locate(driver, directory) else {
                continue;
            };
            let c = self.probe(executable.clone(), directory, Language::C, flags.clone());
            let cpp = self.probe(executable, directory, Language::Cpp, flags.clone());
            if !c.paths.is_empty() && !cpp.paths.is_empty() {
                return Some((c, cpp));
            }
            if partial.is_none() && (!c.paths.is_empty() || !cpp.paths.is_empty()) {
                partial = Some((c, cpp));
            }
        }
        partial
    }

    fn locate(&mut self, driver: &str, directory: &Path) -> Option<PathBuf> {
        let key = (driver.to_owned(), directory.to_path_buf());
        self.drivers
            .entry(key)
            .or_insert_with(|| locate_driver(driver, directory))
            .clone()
    }

    fn probe(
        &mut self,
        executable: PathBuf,
        directory: &Path,
        language: Language,
        flags: Vec<String>,
    ) -> CompilerSearch {
        // CPATH and friends can alter a driver's reported search list. A
        // process-wide cache avoids repeatedly probing a launcher across
        // independent projects while respecting changes to that environment.
        const ENV: [&str; 4] = [
            "CPATH",
            "C_INCLUDE_PATH",
            "CPLUS_INCLUDE_PATH",
            "OBJC_INCLUDE_PATH",
        ];
        let environment = ENV.iter().map(std::env::var_os).collect();
        let key = (
            executable.clone(),
            directory.to_path_buf(),
            language,
            flags.clone(),
            environment,
        );
        let cache = PROBE_PATHS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(key)
            .or_insert_with(|| run_probe(&executable, directory, language, &flags))
            .clone()
    }
}

fn locate_driver(driver: &str, command_directory: &Path) -> Option<PathBuf> {
    let path = Path::new(driver);
    if path.components().count() > 1 || path.is_absolute() {
        return existing_executable(&command_directory.join(path));
    }
    let current_directory = std::env::current_dir().ok()?;
    let search = std::env::var_os("PATH")?;
    locate_on_path(path, &search, &current_directory)
}

fn locate_on_path(driver: &Path, search: &OsStr, current_directory: &Path) -> Option<PathBuf> {
    std::env::split_paths(search)
        .find_map(|entry| existing_executable(&current_directory.join(entry).join(driver)))
}

fn existing_executable(candidate: &Path) -> Option<PathBuf> {
    if candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    #[cfg(windows)]
    if candidate.extension().is_none() {
        let extensions = std::env::var_os("PATHEXT")
            .unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD").to_os_string());
        for extension in extensions.to_string_lossy().split(';') {
            let extension = extension.trim_start_matches('.');
            if !extension.is_empty() {
                let path = candidate.with_extension(extension);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn run_probe(
    executable: &Path,
    directory: &Path,
    language: Language,
    flags: &[String],
) -> CompilerSearch {
    let lang = match language {
        Language::C => "c",
        Language::Cpp => "c++",
    };
    // A launcher may print the complete search list and then wait for a
    // service that never answers. A named scratch file lets us inspect output
    // through an independent file handle while the compiler is still writing.
    let Ok(mut stderr) = tempfile::NamedTempFile::new() else {
        return CompilerSearch::default();
    };
    let Ok(stderr_child) = stderr.reopen() else {
        return CompilerSearch::default();
    };
    let Ok(mut stdout) = tempfile::NamedTempFile::new() else {
        return CompilerSearch::default();
    };
    let Ok(stdout_child) = stdout.reopen() else {
        return CompilerSearch::default();
    };
    let Ok(mut child) = Command::new(executable)
        .current_dir(directory)
        .env("LC_ALL", "C")
        .args(flags)
        .args(["-E", "-v", "-dM", "-x", lang, "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_child))
        .stderr(Stdio::from(stderr_child))
        .spawn()
    else {
        return CompilerSearch::default();
    };
    let start = Instant::now();
    let mut marker_at = None;
    let mut output = Vec::new();
    const END_MARKER: &[u8] = b"End of search list.";
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
        if stderr.as_file_mut().seek(SeekFrom::Start(0)).is_ok() {
            output.clear();
            if stderr.as_file_mut().read_to_end(&mut output).is_ok()
                && output.windows(END_MARKER.len()).any(|w| w == END_MARKER)
            {
                marker_at.get_or_insert_with(Instant::now);
            }
        }
        // GCC/Clang usually exit after writing both streams. A short grace
        // period lets them finish -dM; a launcher that lingers is still cut
        // off promptly after the complete search list has appeared.
        if marker_at.is_some_and(|seen| seen.elapsed() >= Duration::from_millis(250)) {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        // Only a compiler that never completes the list needs this fallback.
        if start.elapsed() >= Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    output.clear();
    if stderr.as_file_mut().seek(SeekFrom::Start(0)).is_ok()
        && stderr.as_file_mut().read_to_end(&mut output).is_ok()
    {
        let paths = parse_search_list(&String::from_utf8_lossy(&output), directory);
        let mut macro_output = String::new();
        if !paths.is_empty() && stdout.as_file_mut().seek(SeekFrom::Start(0)).is_ok() {
            let _ = stdout.as_file_mut().read_to_string(&mut macro_output);
        }
        CompilerSearch {
            paths,
            target_defines: parse_target_defines(&macro_output),
        }
    } else {
        CompilerSearch::default()
    }
}

fn parse_target_defines(output: &str) -> Vec<(String, String)> {
    const NAMES: &[&str] = &[
        "__STDC_HOSTED__",
        "__x86_64__",
        "__amd64__",
        "__i386__",
        "__aarch64__",
        "__arm__",
        "__riscv",
        "__LP64__",
        "_LP64",
        "__ILP32__",
        "__linux__",
        "__unix__",
        "__APPLE__",
        "__MACH__",
        "_WIN32",
        "_WIN64",
        "__BYTE_ORDER__",
        "__ORDER_LITTLE_ENDIAN__",
        "__ORDER_BIG_ENDIAN__",
        "__ORDER_PDP_ENDIAN__",
        "__SIZEOF_POINTER__",
        "__SIZEOF_LONG__",
    ];
    let mut defines = output
        .lines()
        .filter_map(|line| line.strip_prefix("#define "))
        .filter_map(|line| {
            let (name, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            NAMES
                .contains(&name)
                .then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect::<Vec<_>>();
    defines.sort_by(|a, b| a.0.cmp(&b.0));
    defines
}

fn append_system_paths(flags: &mut Vec<String>, paths: &[PathBuf]) {
    for path in paths {
        flags.push("-isystem".into());
        flags.push(trace_ir::canonicalize(path).to_string_lossy().into_owned());
    }
}

fn probe_flags(args: &[String], directory: &Path) -> Vec<String> {
    let mut flags = Vec::new();
    let mut args = args
        .iter()
        .skip(crate::compile_commands::driver_index(args) + 1);
    while let Some(arg) = args.next() {
        if matches!(
            arg.as_str(),
            "-m32" | "-m64" | "-mx32" | "-nostdinc" | "-nostdinc++" | "-ffreestanding" | "-fhosted"
        ) || ["-march=", "-mcpu=", "-mabi="]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
        {
            flags.push(arg.clone());
            continue;
        }
        if matches!(
            arg.as_str(),
            "--sysroot"
                | "-isysroot"
                | "--target"
                | "-target"
                | "-arch"
                | "--gcc-toolchain"
                | "-resource-dir"
                | "-stdlib"
                | "-std"
                | "-B"
        ) {
            if let Some(value) = args.next() {
                flags.push(arg.clone());
                flags.push(
                    if matches!(
                        arg.as_str(),
                        "--sysroot" | "-isysroot" | "--gcc-toolchain" | "-resource-dir" | "-B"
                    ) {
                        directory.join(value).to_string_lossy().into_owned()
                    } else {
                        value.clone()
                    },
                );
            }
            continue;
        }
        if let Some((prefix, value)) = [
            "--sysroot=",
            "-isysroot=",
            "-isysroot",
            "--gcc-toolchain=",
            "-resource-dir=",
            "-B",
        ]
        .iter()
        .find_map(|prefix| {
            arg.strip_prefix(prefix)
                .filter(|v| !v.is_empty())
                .map(|value| (*prefix, value))
        }) {
            flags.push(format!("{prefix}{}", directory.join(value).display()));
        } else if arg.starts_with("--target=")
            || arg.starts_with("-stdlib=")
            || arg.starts_with("-std=")
        {
            flags.push(arg.clone());
        }
    }
    flags
}

fn parse_search_list(stderr: &str, directory: &Path) -> Vec<PathBuf> {
    let mut inside = false;
    let mut complete = false;
    let mut paths = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        if line == "#include <...> search starts here:" {
            inside = true;
        } else if inside && line == "End of search list." {
            complete = true;
            break;
        } else if inside && !line.is_empty() && !line.ends_with("(framework directory)") {
            let path = directory.join(line);
            if path.is_dir() {
                let path = trace_ir::canonicalize(&path);
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    if complete {
        paths
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_existing_directories_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        std::fs::create_dir(&one).unwrap();
        std::fs::create_dir(&two).unwrap();
        let output = format!(
            "noise\n#include <...> search starts here:\n {}\n {}\n {}\nEnd of search list.\n",
            one.display(),
            two.display(),
            one.display()
        );
        assert_eq!(
            parse_search_list(&output, dir.path()),
            vec![trace_ir::canonicalize(&one), trace_ir::canonicalize(&two)]
        );
        assert!(
            parse_search_list(output.trim_end_matches("End of search list.\n"), dir.path())
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_uses_first_working_driver_for_both_languages() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let include = dir.path().join("include");
        std::fs::create_dir(&include).unwrap();
        let driver = dir.path().join("gcc");
        let log = dir.path().join("calls");
        std::fs::write(
            &driver,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\n",
                log.display(),
                include.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let missing = dir.path().join("missing");
        let mut cache = CompilerIncludes::default();
        let (c, cpp) = cache
            .for_project_drivers(
                [
                    missing.to_str().unwrap(),
                    driver.to_str().unwrap(),
                    missing.to_str().unwrap(),
                ],
                dir.path(),
                &[],
            )
            .unwrap();
        assert_eq!(c.paths, vec![trace_ir::canonicalize(&include)]);
        assert_eq!(cpp.paths, vec![trace_ir::canonicalize(&include)]);
        assert_eq!(std::fs::read(&log).unwrap(), b"xx");
    }

    #[cfg(unix)]
    #[test]
    fn command_directory_resolves_relative_paths_and_separates_probe_cache_entries() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let driver = dir.path().join("compiler");
        let log = dir.path().join("working_directories");
        std::fs::write(
            &driver,
            format!(
                "#!/bin/sh\npwd >> '{}'\nprintf '#include <...> search starts here:\\n relative/include\\nEnd of search list.\\n' >&2\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let args = vec![driver.to_string_lossy().into_owned()];
        let mut cache = CompilerIncludes::default();
        for name in ["first", "second"] {
            let working_directory = dir.path().join(name);
            let include = working_directory.join("relative/include");
            std::fs::create_dir_all(&include).unwrap();
            assert_eq!(
                cache
                    .for_command(&args, &working_directory, Language::C, &[])
                    .paths,
                vec![trace_ir::canonicalize(&include)]
            );
        }
        let lines = std::fs::read_to_string(&log).unwrap();
        let first = trace_ir::canonicalize(&dir.path().join("first"));
        let second = trace_ir::canonicalize(&dir.path().join("second"));
        assert_eq!(
            lines.lines().collect::<Vec<_>>(),
            [first.to_str().unwrap(), second.to_str().unwrap()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn waits_for_a_slow_list_but_not_for_a_lingering_launcher() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let include = dir.path().join("include");
        std::fs::create_dir(&include).unwrap();
        let driver = dir.path().join("compiler");
        std::fs::write(
            &driver,
            format!(
                "#!/bin/sh\nsleep 3\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\nexec sleep 10\n",
                include.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = Instant::now();
        assert_eq!(
            run_probe(&driver, dir.path(), Language::C, &[]).paths,
            vec![trace_ir::canonicalize(&include)]
        );
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn imports_only_target_and_hosted_macros() {
        let output = "#define __GNUC__ 13\n#define __x86_64__ 1\n#define __STDC_HOSTED__ 1\n#define __LP64__ 1\n";
        assert_eq!(
            parse_target_defines(output),
            vec![
                ("__LP64__".into(), "1".into()),
                ("__STDC_HOSTED__".into(), "1".into()),
                ("__x86_64__".into(), "1".into()),
            ]
        );
    }

    #[test]
    fn probe_preserves_flags_that_change_target_or_hosted_macros() {
        let args = [
            "gcc",
            "-m32",
            "-ffreestanding",
            "-march=haswell",
            "-c",
            "main.c",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            probe_flags(&args, Path::new("/project")),
            ["-m32", "-ffreestanding", "-march=haswell"]
        );
    }

    #[test]
    fn probe_preserves_both_isysroot_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path();
        let args = [
            "clang",
            "-isysroot=first-sdk",
            "-isysroot",
            "other-sdk",
            "-isysrootthird-sdk",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            probe_flags(&args, directory),
            vec![
                format!("-isysroot={}", directory.join("first-sdk").display()),
                "-isysroot".into(),
                directory.join("other-sdk").to_string_lossy().into_owned(),
                format!("-isysroot{}", directory.join("third-sdk").display()),
            ]
        );
    }

    #[test]
    fn relative_path_entries_use_trace_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let process_directory = dir.path().join("trace");
        let command_directory = dir.path().join("build");
        let bin = process_directory.join("relative-bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir(&command_directory).unwrap();
        let driver = bin.join(if cfg!(windows) { "gcc.exe" } else { "gcc" });
        std::fs::write(&driver, "compiler").unwrap();
        let search = std::env::join_paths([Path::new("relative-bin")]).unwrap();
        assert_eq!(
            locate_on_path(Path::new("gcc"), &search, &process_directory),
            Some(driver)
        );
        assert!(!command_directory.join("relative-bin").exists());
    }

    #[cfg(windows)]
    #[test]
    fn executable_lookup_adds_exe_suffix_for_direct_driver() {
        let dir = tempfile::tempdir().unwrap();
        let driver = dir.path().join("gcc.exe");
        std::fs::write(&driver, "compiler").unwrap();
        assert_eq!(existing_executable(&dir.path().join("gcc")), Some(driver));
    }

    #[cfg(unix)]
    #[test]
    fn project_tries_clang_when_gcc_has_no_cpp_search_list() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let gcc_include = dir.path().join("gcc-include");
        let clang_include = dir.path().join("clang-include");
        std::fs::create_dir(&gcc_include).unwrap();
        std::fs::create_dir(&clang_include).unwrap();
        let gcc = dir.path().join("gcc");
        let clang = dir.path().join("clang");
        std::fs::write(
            &gcc,
            format!(
                "#!/bin/sh\ncase \"$*\" in *'-x c++'*) printf '#include <...> search starts here:\\nEnd of search list.\\n' >&2 ;; *) printf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2 ;; esac\n",
                gcc_include.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &clang,
            format!(
                "#!/bin/sh\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\n",
                clang_include.display()
            ),
        )
        .unwrap();
        for path in [&gcc, &clang] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (c, cpp) = CompilerIncludes::default()
            .for_project_drivers(
                [gcc.to_str().unwrap(), clang.to_str().unwrap()],
                dir.path(),
                &[],
            )
            .unwrap();
        let expected = vec![trace_ir::canonicalize(&clang_include)];
        assert_eq!(c.paths, expected);
        assert_eq!(cpp.paths, expected);
    }

    #[cfg(unix)]
    #[test]
    fn architecture_pair_reaches_probe_and_separates_cache_entries() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let include = dir.path().join("include");
        std::fs::create_dir(&include).unwrap();
        let driver = dir.path().join("clang");
        let log = dir.path().join("architectures");
        std::fs::write(
            &driver,
            format!(
                "#!/bin/sh\narch=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -arch ]; then arch=$2; shift 2; else shift; fi\ndone\nprintf '%s\\n' \"$arch\" >> '{}'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\nif [ \"$arch\" = arm64 ]; then printf '#define __aarch64__ 1\\n'; else printf '#define __x86_64__ 1\\n'; fi\n",
                log.display(),
                include.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut probes = CompilerIncludes::default();
        for (arch, macro_name) in [("arm64", "__aarch64__"), ("x86_64", "__x86_64__")] {
            let args = vec![
                driver.to_string_lossy().into_owned(),
                "-arch".into(),
                arch.into(),
                "-c".into(),
                "main.cpp".into(),
            ];
            let search = probes.for_command(&args, dir.path(), Language::Cpp, &[]);
            assert_eq!(search.paths, vec![trace_ir::canonicalize(&include)]);
            assert!(search
                .target_defines
                .iter()
                .any(|(name, _)| name == macro_name));
            // A repeated command reuses the same cached probe.
            assert_eq!(
                probes.for_command(&args, dir.path(), Language::Cpp, &[]),
                search
            );
        }
        assert_eq!(std::fs::read_to_string(log).unwrap(), "arm64\nx86_64\n");
    }
}
