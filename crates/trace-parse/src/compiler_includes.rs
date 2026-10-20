//! Query driver-owned headers and allowlisted predefines using empty input only.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use trace_preproc::{Language, PreprocessOptions};

pub(super) fn cpath_directories(directory: &Path, value: Option<&OsStr>) -> BTreeSet<PathBuf> {
    value
        .map(|value| {
            std::env::split_paths(value)
                .map(|path| trace_ir::canonicalize(&directory.join(path)))
                .collect()
        })
        .unwrap_or_default()
}

/// The compiler ignores command -I directories that belong to its system
/// search list. CLI includes remain trace overrides; CPATH duplicates keep
/// their earlier -I position unless explicitly classified as -isystem.
pub(super) fn remove_system_include_duplicates(
    opts: &mut PreprocessOptions,
    system_paths: &[PathBuf],
    cli_include_paths: &BTreeSet<PathBuf>,
    cpath_dirs: &BTreeSet<PathBuf>,
) {
    opts.include_paths.retain(|path| {
        cli_include_paths.contains(path)
            || !system_paths.contains(path)
            || (cpath_dirs.contains(path) && !opts.system_include_paths.contains(path))
    });
}

#[derive(Default)]
pub(super) struct CompilerIncludes {
    drivers: HashMap<(String, PathBuf), Option<PathBuf>>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(super) struct CompilerSearch {
    pub paths: Vec<PathBuf>,
    pub target_defines: Vec<(String, String)>,
    pub failure: Option<String>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(super) struct DiscoveryMode {
    pub includes: bool,
    pub predefines: bool,
}

impl DiscoveryMode {
    pub fn from_options(opts: &PreprocessOptions) -> Self {
        Self {
            includes: opts.system_includes,
            predefines: opts.compiler_predefines,
        }
    }
}

type ProbeKey = (
    PathBuf,
    PathBuf,
    Language,
    Vec<String>,
    Vec<Option<std::ffi::OsString>>,
    DiscoveryMode,
    bool, // Unsupported macro driver; CL and GNU modes can share an executable.
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
        mode: DiscoveryMode,
    ) -> CompilerSearch {
        let Some(driver) = args.get(crate::compile_commands::driver_index(args)) else {
            return failed("no compiler driver in command");
        };
        let stem = crate::compile_commands::driver_stem(args).to_ascii_lowercase();
        // Intel's icc[-VERSION] and icpc accept GNU-style probes. Match known
        // IAR names explicitly, then validate other drivers by their output.
        let unsupported_predefines = mode.predefines
            && (matches!(
                stem.as_str(),
                "cl" | "clang-cl" | "iccarm" | "iccrx" | "iccavr" | "iar"
            ) || args.iter().any(|arg| arg == "--driver-mode=cl"));
        let Some(executable) = self.locate(driver, directory) else {
            return failed("compiler executable not found");
        };
        let mut flags = probe_flags(args, directory);
        if mode.includes {
            append_system_paths(&mut flags, system_paths);
        }
        self.probe(
            executable,
            directory,
            language,
            flags,
            mode,
            unsupported_predefines,
        )
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

    /// Macro discovery never guesses a host driver. Without compiler context,
    /// include discovery can still use the historical project driver, but its
    /// macro dump must not become the project's target or identity seed.
    pub(super) fn for_fallback(
        &mut self,
        directory: &Path,
        opts: &PreprocessOptions,
        system_paths: &[Vec<PathBuf>; 2],
        shared_args: &[Vec<String>; 2],
        languages: &[Language],
        warnings: &mut Vec<(Language, String)>,
    ) -> (CompilerSearch, CompilerSearch) {
        let include_fallback = if opts.system_includes && opts.compiler.is_none() {
            let mut paths = Vec::new();
            for path in system_paths.iter().flatten() {
                if !paths.contains(path) {
                    paths.push(path.clone());
                }
            }
            self.for_project(directory, &paths)
        } else {
            None
        };
        let mut probe = |language| {
            if !languages.contains(&language) {
                return CompilerSearch::default();
            }
            let slot = match language {
                Language::C => 0,
                Language::Cpp => 1,
            };
            let args: Vec<String> = opts
                .compiler
                .as_ref()
                .map(|compiler| {
                    std::iter::once(compiler.to_string_lossy().into_owned())
                        .chain(fallback_language_args(&opts.compiler_args, language))
                        .chain(shared_args[slot].iter().cloned())
                        .collect()
                })
                .unwrap_or_default();
            let search = if args.is_empty() {
                let mut search = failed("no compiler context; --compiler-predefines requires an explicit compiler or compilation command to discover macros");
                if let Some((c, cpp)) = &include_fallback {
                    search.paths.clone_from(if language == Language::C {
                        &c.paths
                    } else {
                        &cpp.paths
                    });
                }
                search
            } else {
                self.for_command(
                    &args,
                    directory,
                    language,
                    &system_paths[slot],
                    DiscoveryMode::from_options(opts),
                )
            };
            if let Some(reason) = &search.failure {
                let warning = configuration_warning(
                    &args,
                    directory,
                    language,
                    reason,
                    "supply a usable compilation-command driver or --compiler-predefines --compiler <EXECUTABLE> [--compiler-arg <ARG>], or manual -D definitions",
                );
                if !warnings.iter().any(|(_, existing)| existing == &warning) {
                    warnings.push((language, warning));
                }
            }
            search
        };
        (probe(Language::C), probe(Language::Cpp))
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
            let c = self.probe(
                executable.clone(),
                directory,
                Language::C,
                flags.clone(),
                DiscoveryMode {
                    includes: true,
                    predefines: false,
                },
                false,
            );
            let cpp = self.probe(
                executable,
                directory,
                Language::Cpp,
                flags.clone(),
                DiscoveryMode {
                    includes: true,
                    predefines: false,
                },
                false,
            );
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
        mode: DiscoveryMode,
        unsupported_predefines: bool,
    ) -> CompilerSearch {
        // CPATH and friends can alter a driver's reported search list. A
        // process-wide cache avoids repeatedly probing a launcher across
        // independent projects while respecting changes to that environment.
        const ENV: &[&str] = &[
            "CPATH",
            "C_INCLUDE_PATH",
            "CPLUS_INCLUDE_PATH",
            "OBJC_INCLUDE_PATH",
            "PATH",
            "SDKROOT",
            "DEVELOPER_DIR",
            "GCC_EXEC_PREFIX",
            "COMPILER_PATH",
            "MACOSX_DEPLOYMENT_TARGET",
        ];
        let environment = ENV.iter().map(std::env::var_os).collect();
        let key = (
            executable.clone(),
            directory.to_path_buf(),
            language,
            flags.clone(),
            environment,
            mode,
            unsupported_predefines,
        );
        let cache = PROBE_PATHS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(key)
            .or_insert_with(|| {
                if !unsupported_predefines {
                    return run_probe(&executable, directory, language, &flags, mode);
                }
                let mut search = if mode.includes {
                    run_probe(
                        &executable,
                        directory,
                        language,
                        &flags,
                        DiscoveryMode {
                            includes: true,
                            predefines: false,
                        },
                    )
                } else {
                    CompilerSearch::default()
                };
                search.target_defines.clear();
                let reason = "unsupported macro probe: requires a GNU-compatible -dM driver (native MSVC/IAR is unsupported)";
                search.failure = Some(match search.failure {
                    Some(failure) => format!("{failure}; {reason}"),
                    None => reason.into(),
                });
                search
            })
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

fn failed(reason: impl Into<String>) -> CompilerSearch {
    CompilerSearch {
        failure: Some(reason.into()),
        ..CompilerSearch::default()
    }
}

pub(super) fn configuration_warning(
    args: &[String],
    directory: &Path,
    language: Language,
    reason: &str,
    remedy: &str,
) -> String {
    let driver = args
        .get(crate::compile_commands::driver_index(args))
        .map(String::as_str)
        .unwrap_or("<unspecified>");
    format!(
        "compiler {driver:?}, directory {}, language {language:?}, flags {:?}: {reason}; {remedy}",
        directory.display(),
        probe_flags(args, directory)
    )
}

fn run_probe(
    executable: &Path,
    directory: &Path,
    language: Language,
    flags: &[String],
    mode: DiscoveryMode,
) -> CompilerSearch {
    run_probe_with_timeout(
        executable,
        directory,
        language,
        flags,
        mode,
        Duration::from_secs(30),
    )
}

fn run_probe_with_timeout(
    executable: &Path,
    directory: &Path,
    language: Language,
    flags: &[String],
    mode: DiscoveryMode,
    timeout: Duration,
) -> CompilerSearch {
    let lang = match language {
        Language::C => "c",
        Language::Cpp => "c++",
    };
    // Independent handles retain a completed include list even when a launcher
    // lingers. Macro dumps require successful completion within the overall
    // timeout, even when the include list is already complete.
    let run = || -> Result<CompilerSearch, String> {
        let mut stderr = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        let mut stdout = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        let mut command = Command::new(executable);
        command
            .current_dir(directory)
            .env("LC_ALL", "C")
            .args(flags)
            .arg("-E");
        if mode.includes {
            command.arg("-v");
        }
        command.args(["-dM", "-x", lang, "-"]);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.reopen().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(stderr.reopen().map_err(|e| e.to_string())?))
            .spawn()
            .map_err(|e| format!("cannot start compiler: {e}"))?;
        let start = Instant::now();
        let mut marker_at = None;
        let mut output = Vec::new();
        const END_MARKER: &[u8] = b"End of search list.";
        let completion = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    break if status.success() {
                        None
                    } else {
                        Some(format!("macro probe exited with {status}"))
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Some(format!("cannot wait for compiler: {error}"));
                }
            }
            if mode.includes
                && !mode.predefines
                && stderr.as_file_mut().seek(SeekFrom::Start(0)).is_ok()
            {
                output.clear();
                if stderr.as_file_mut().read_to_end(&mut output).is_ok()
                    && output.windows(END_MARKER.len()).any(|w| w == END_MARKER)
                {
                    marker_at.get_or_insert_with(Instant::now);
                }
            }
            if marker_at.is_some_and(|seen| seen.elapsed() >= Duration::from_millis(250))
                || start.elapsed() >= timeout
            {
                let _ = child.kill();
                let _ = child.wait();
                break Some("compiler probe timed out before successful completion".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        output.clear();
        stderr
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(|e| e.to_string())?;
        stderr
            .as_file_mut()
            .read_to_end(&mut output)
            .map_err(|e| e.to_string())?;
        let paths = if mode.includes {
            parse_search_list(&String::from_utf8_lossy(&output), directory)
        } else {
            Vec::new()
        };
        let mut macro_bytes = Vec::new();
        if (mode.predefines && completion.is_none()) || (!mode.predefines && !paths.is_empty()) {
            stdout
                .as_file_mut()
                .seek(SeekFrom::Start(0))
                .map_err(|e| e.to_string())?;
            stdout
                .as_file_mut()
                .read_to_end(&mut macro_bytes)
                .map_err(|e| e.to_string())?;
        }
        let macro_output = String::from_utf8_lossy(&macro_bytes);
        let failure = if mode.predefines {
            completion.or_else(|| {
                (!macro_output
                    .lines()
                    .any(|line| line.starts_with("#define ")))
                .then(|| "unsupported or empty GNU-compatible macro dump".into())
            })
        } else {
            None
        };
        let target_defines = if failure.is_none() {
            parse_target_defines(&macro_output, mode.predefines)
        } else {
            Vec::new()
        };
        Ok(CompilerSearch {
            paths,
            target_defines,
            failure,
        })
    };
    run().unwrap_or_else(failed)
}

fn parse_target_defines(output: &str, identity: bool) -> Vec<(String, String)> {
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
    const IDENTITY: &[&str] = &[
        "__GNUC__",
        "__GNUC_MINOR__",
        "__GNUC_PATCHLEVEL__",
        "__GNUG__",
        "__clang__",
        "__clang_major__",
        "__clang_minor__",
        "__clang_patchlevel__",
        "__clang_version__",
        "__MINGW32__",
        "__MINGW64__",
        "_MSC_VER",
        "_MSC_FULL_VER",
        "_MSC_BUILD",
    ];
    let mut defines = output
        .lines()
        .filter_map(|line| line.strip_prefix("#define "))
        .filter_map(|line| {
            let (name, value) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            (NAMES.contains(&name) || identity && IDENTITY.contains(&name))
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
    parse_probe_flags(args, directory, false).expect("non-strict probe flags")
}

/// Fallback arguments must be entirely understood; command arguments are
/// filtered because they also contain sources, outputs and build switches.
pub(super) fn validate_options(opts: &PreprocessOptions) -> Result<(), String> {
    if opts.compiler.is_some() && !opts.compiler_predefines && !opts.system_includes {
        return Err("--compiler requires --compiler-predefines or --system-includes".into());
    }
    if !opts.compiler_args.is_empty() && opts.compiler.is_none() {
        return Err("--compiler-arg requires --compiler".into());
    }
    let args = std::iter::once("compiler".to_string())
        .chain(opts.compiler_args.iter().cloned())
        .collect::<Vec<_>>();
    parse_probe_flags(&args, Path::new("."), true)?;
    Ok(())
}

fn parse_probe_flags(
    args: &[String],
    directory: &Path,
    strict: bool,
) -> Result<Vec<String>, String> {
    let mut flags = Vec::new();
    let mut args = args
        .iter()
        .skip(crate::compile_commands::driver_index(args) + 1);
    while let Some(arg) = args.next() {
        if matches!(
            arg.as_str(),
            "-m32"
                | "-m64"
                | "-mx32"
                | "-nostdinc"
                | "-nostdinc++"
                | "-undef"
                | "-ffreestanding"
                | "-fhosted"
                | "-fms-compatibility"
                | "-fno-ms-compatibility"
                | "-fms-extensions"
                | "-fno-ms-extensions"
        ) || [
            "-march=",
            "-mcpu=",
            "-mabi=",
            "-fms-compatibility-version=",
            "-fmsc-version=",
            "-fgnuc-version=",
        ]
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
                | "--std"
                | "-B"
        ) {
            if let Some(value) = args.next() {
                if arg == "-std" || arg == "--std" {
                    flags.push(format!("-std={value}"));
                    continue;
                }
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
            } else if strict {
                return Err(format!("missing operand for --compiler-arg {arg}"));
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
            || arg.starts_with("--std=")
        {
            flags.push(arg.clone());
        } else if strict {
            return Err(format!("unsupported --compiler-arg {arg:?}"));
        }
    }
    Ok(flags)
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

/// Keep each language's explicit fallback standards independent. Unknown
/// standards still reach the driver so invalid configuration is reported.
fn fallback_language_args(args: &[String], language: Language) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let split = arg == "-std" || arg == "--std";
        let standard = if split {
            args.next().map(String::as_str)
        } else {
            arg.strip_prefix("-std=")
                .or_else(|| arg.strip_prefix("--std="))
        };
        if standard.is_some_and(|value| {
            crate::compile_commands::standard_macros(value)
                .is_ok_and(|(expected, _)| expected != language)
        }) {
            continue;
        }
        filtered.push(arg.clone());
        if split {
            if let Some(value) = standard {
                filtered.push(value.to_string());
            }
        }
    }
    filtered
}

/// Seed standard macros from explicit fallback arguments, not compiler dumps.
/// Already-parsed shared flags and command operations follow this seed and
/// therefore override it; CLI definitions retain their final precedence.
pub(super) fn apply_fallback_standard(opts: &mut PreprocessOptions, language: Language) {
    if opts.compiler.is_none() {
        return;
    }
    let mut standard = None;
    let filtered = fallback_language_args(&opts.compiler_args, language);
    let mut args = filtered.iter();
    while let Some(arg) = args.next() {
        if arg == "-std" || arg == "--std" {
            standard = args.next().map(String::as_str);
        } else if let Some(value) = arg
            .strip_prefix("-std=")
            .or_else(|| arg.strip_prefix("--std="))
        {
            standard = Some(value);
        }
    }
    if let Some((expected, mut ops)) =
        standard.and_then(|s| crate::compile_commands::standard_macros(s).ok())
    {
        if expected == language {
            // Warming starts from a CLI-seeded shared table and restores CLI
            // definitions only when absent. Do not overwrite that seed with
            // standard operations that ordinary TU preprocessing would undo.
            ops.retain(|op| {
                let (trace_preproc::CommandMacro::Define(name, _)
                | trace_preproc::CommandMacro::Undef(name)) = op;
                !opts.defines.contains_key(name)
            });
            ops.append(&mut opts.command_macros);
            opts.command_macros = ops;
        }
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
                    .for_command(
                        &args,
                        &working_directory,
                        Language::C,
                        &[],
                        DiscoveryMode {
                            includes: true,
                            predefines: false
                        }
                    )
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
            run_probe(
                &driver,
                dir.path(),
                Language::C,
                &[],
                DiscoveryMode {
                    includes: true,
                    predefines: false
                }
            )
            .paths,
            vec![trace_ir::canonicalize(&include)]
        );
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[cfg(unix)]
    #[test]
    fn macro_probe_requires_success_and_discards_timeout_dumps() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let include = dir.path().join("include");
        std::fs::create_dir(&include).unwrap();
        for (name, tail, expected) in [
            ("nonzero", "exit 3", "exited"),
            ("timeout", "exec sleep 20", "timed out"),
        ] {
            let driver = dir.path().join(name);
            std::fs::write(&driver, format!("#!/bin/sh\nprintf '#define __GNUC__ 13\\n'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\n{tail}\n", include.display())).unwrap();
            std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
            let started = Instant::now();
            let result = run_probe_with_timeout(
                &driver,
                dir.path(),
                Language::C,
                &[],
                DiscoveryMode {
                    includes: true,
                    predefines: true,
                },
                Duration::from_secs(5),
            );
            assert!(started.elapsed() < Duration::from_secs(8), "{name}");
            assert!(result.target_defines.is_empty());
            assert!(
                result.failure.as_ref().unwrap().contains(expected),
                "{name}: {result:?}"
            );
            assert_eq!(result.paths, vec![trace_ir::canonicalize(&include)]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn macro_only_probe_waits_for_success_after_a_search_marker() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let driver = dir.path().join("compiler");
        std::fs::write(
            &driver,
            "#!/bin/sh\nprintf '#include <...> search starts here:\\nEnd of search list.\\n' >&2\nsleep 1\nprintf '#define __GNUC__ 13\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = run_probe_with_timeout(
            &driver,
            dir.path(),
            Language::C,
            &[],
            DiscoveryMode {
                includes: false,
                predefines: true,
            },
            Duration::from_secs(5),
        );
        assert_eq!(result.failure, None);
        assert_eq!(result.target_defines, [("__GNUC__".into(), "13".into())]);
    }

    #[cfg(unix)]
    #[test]
    fn combined_probe_imports_a_dump_completed_after_the_search_marker() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let driver = dir.path().join("compiler");
        std::fs::write(
            &driver,
            "#!/bin/sh\nprintf '#include <...> search starts here:\\nEnd of search list.\\n' >&2\nsleep 1\nprintf '#define __GNUC__ 13\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = run_probe(
            &driver,
            dir.path(),
            Language::C,
            &[],
            DiscoveryMode {
                includes: true,
                predefines: true,
            },
        );
        assert_eq!(result.failure, None);
        assert_eq!(result.target_defines, [("__GNUC__".into(), "13".into())]);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_dump_preserves_search_paths_and_allowlisted_macros() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let include = dir.path().join("include");
        std::fs::create_dir(&include).unwrap();
        let driver = dir.path().join("compiler");
        std::fs::write(
            &driver,
            format!(
                "#!/bin/sh\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\nprintf '#define __VENDOR__ \"\\377\\376\"\\n#define __GNUC__ 13\\n#define __x86_64__ 1\\n'\n",
                include.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        for predefines in [false, true] {
            let result = run_probe(
                &driver,
                dir.path(),
                Language::C,
                &[],
                DiscoveryMode {
                    includes: true,
                    predefines,
                },
            );
            assert_eq!(result.failure, None, "{result:?}");
            assert_eq!(result.paths, [trace_ir::canonicalize(&include)]);
            let mut expected = vec![("__x86_64__".into(), "1".into())];
            if predefines {
                expected.insert(0, ("__GNUC__".into(), "13".into()));
            }
            assert_eq!(result.target_defines, expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_driver_results_are_cached_separately_from_gnu_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let driver = dir.path().join("clang");
        let log = dir.path().join("probes");
        std::fs::write(
            &driver,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\nprintf '#include <...> search starts here:\\n {}\\nEnd of search list.\\n' >&2\nprintf '#define __GNUC__ 13\\n'\n",
                log.display(),
                dir.path().display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..2 {
            // Independent project queries share both supported and unsupported
            // results, without letting CL mode poison GNU-mode macro discovery.
            let mut probes = CompilerIncludes::default();
            for cl_mode in [true, false] {
                let mut args = vec![driver.to_string_lossy().into_owned()];
                if cl_mode {
                    args.push("--driver-mode=cl".into());
                }
                let result = probes.for_command(
                    &args,
                    dir.path(),
                    Language::C,
                    &[],
                    DiscoveryMode {
                        includes: true,
                        predefines: true,
                    },
                );
                assert_eq!(result.paths, [trace_ir::canonicalize(dir.path())]);
                if cl_mode {
                    assert!(result.failure.unwrap().contains("unsupported macro probe"));
                    assert!(result.target_defines.is_empty());
                } else {
                    assert_eq!(result.failure, None);
                    assert_eq!(result.target_defines, [("__GNUC__".into(), "13".into())]);
                }
            }
        }
        assert_eq!(std::fs::read_to_string(log).unwrap(), "xx");
    }

    #[test]
    fn identity_allowlist_excludes_language_build_and_header_macros() {
        let dump = "#define __GNUC__ 13\n#define __GNUC_MINOR__ 2\n#define __GNUC_PATCHLEVEL__ 1\n#define __GNUG__ 13\n#define __clang__ 1\n#define __clang_major__ 19\n#define __clang_minor__ 0\n#define __clang_patchlevel__ 1\n#define __clang_version__ \"19.0.1\"\n#define __MINGW32__ 1\n#define __MINGW64__ 1\n#define _MSC_VER 1930\n#define _MSC_FULL_VER 193000000\n#define _MSC_BUILD 1\n#define __cplusplus 202002L\n#define __BYTE_ORDER 1234\n#define __LITTLE_ENDIAN 1234\n#define __BIG_ENDIAN 4321\n#define __USER__ 1\n#define __LITEOS__ 1\n#define __HIVIEW_OHOS__ 1\n#define __GNUC__(x) x\n";
        let imported = parse_target_defines(dump, true);
        assert_eq!(imported.len(), 14);
        assert!(parse_target_defines(dump, false).is_empty());
        assert_eq!(
            imported
                .iter()
                .find(|(name, _)| name == "__clang_version__")
                .unwrap()
                .1,
            "\"19.0.1\""
        );
    }

    #[test]
    fn imports_only_target_and_hosted_macros() {
        let output = "#define __GNUC__ 13\n#define __x86_64__ 1\n#define __STDC_HOSTED__ 1\n#define __LP64__ 1\n";
        assert_eq!(
            parse_target_defines(output, false),
            vec![
                ("__LP64__".into(), "1".into()),
                ("__STDC_HOSTED__".into(), "1".into()),
                ("__x86_64__".into(), "1".into()),
            ]
        );
    }

    #[test]
    fn probe_preserves_flags_that_change_target_hosted_or_identity_macros() {
        let args = [
            "clang",
            "-m32",
            "-ffreestanding",
            "-march=haswell",
            "-undef",
            "-fms-compatibility-version=19.30",
            "-fmsc-version=1931",
            "-fgnuc-version=9.3.1",
            "-c",
            "main.c",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            probe_flags(&args, Path::new("/project")),
            [
                "-m32",
                "-ffreestanding",
                "-march=haswell",
                "-undef",
                "-fms-compatibility-version=19.30",
                "-fmsc-version=1931",
                "-fgnuc-version=9.3.1"
            ]
        );
        assert_eq!(
            parse_probe_flags(
                &[
                    "compiler".into(),
                    "-undef".into(),
                    "-fms-compatibility-version=19.30".into(),
                    "-fmsc-version=1931".into(),
                    "-fgnuc-version=9.3.1".into(),
                ],
                Path::new("/project"),
                true
            )
            .unwrap(),
            [
                "-undef",
                "-fms-compatibility-version=19.30",
                "-fmsc-version=1931",
                "-fgnuc-version=9.3.1"
            ]
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
            let search = probes.for_command(
                &args,
                dir.path(),
                Language::Cpp,
                &[],
                DiscoveryMode {
                    includes: true,
                    predefines: false,
                },
            );
            assert_eq!(search.paths, vec![trace_ir::canonicalize(&include)]);
            assert!(search
                .target_defines
                .iter()
                .any(|(name, _)| name == macro_name));
            // A repeated command reuses the same cached probe.
            assert_eq!(
                probes.for_command(
                    &args,
                    dir.path(),
                    Language::Cpp,
                    &[],
                    DiscoveryMode {
                        includes: true,
                        predefines: false
                    }
                ),
                search
            );
        }
        assert_eq!(std::fs::read_to_string(log).unwrap(), "arm64\nx86_64\n");
    }
}
