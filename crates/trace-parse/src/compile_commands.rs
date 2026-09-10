//! Optional, per-source compilation configurations. Commands are data: neither
//! the compiler nor a shell is executed when reading a database.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use trace_preproc::{CommandMacro, Language, PreprocessOptions};

#[derive(Default)]
pub(crate) struct CompilationDatabase {
    pub commands: BTreeMap<PathBuf, Vec<PreprocessOptions>>,
    pub warnings: Vec<String>,
}

#[derive(Deserialize)]
struct Entry {
    directory: PathBuf,
    file: PathBuf,
    arguments: Option<Vec<String>>,
    command: Option<String>,
}

impl CompilationDatabase {
    pub fn load(root: &Path, overrides: &PreprocessOptions) -> Result<Self, String> {
        let root = trace_ir::canonicalize(root);
        let directory = if root.is_file() {
            root.parent().unwrap_or(&root)
        } else {
            &root
        };
        let path = if let Some(explicit) = &overrides.compilation_database {
            let canonical = trace_ir::canonicalize(explicit);
            if !canonical.is_file() {
                return Err(format!(
                    "compilation database does not exist or is not a file: {}",
                    explicit.display()
                ));
            }
            canonical
        } else {
            let direct = directory.join("compile_commands.json");
            let build = directory.join("build").join("compile_commands.json");
            if !direct.exists() && build.exists() {
                build
            } else {
                direct
            }
        };
        let database_dir = trace_ir::canonicalize(path.parent().unwrap_or(Path::new(".")));
        let mut db = Self::default();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && overrides.compilation_database.is_none() =>
            {
                return Ok(db);
            }
            Err(e) => return Ok(db.warned(&path, e)),
        };
        let entries: Vec<serde_json::Value> = match serde_json::from_str(&text) {
            Ok(entries) => entries,
            Err(e) => return Ok(db.warned(&path, e)),
        };
        let dep_roots: Vec<PathBuf> = overrides
            .dep_roots
            .iter()
            .map(|dep| trace_ir::canonicalize(dep))
            .collect();
        for (i, entry) in entries.into_iter().enumerate() {
            let entry = match serde_json::from_value::<Entry>(entry) {
                Ok(entry) => entry,
                Err(e) => {
                    db.warn_entry(&path, i, e);
                    continue;
                }
            };
            // A shared database routinely describes the whole monorepo. An
            // entry this run cannot use is not the user's problem, so filter on
            // the paths alone before parsing arguments or touching the disk.
            let directory = trace_ir::canonicalize(&database_dir.join(&entry.directory));
            let file = trace_ir::canonicalize(&directory.join(&entry.file));
            let in_root = file == root || (root.is_dir() && file.starts_with(&root));
            if !in_root || dep_roots.iter().any(|dep| file.starts_with(dep)) {
                continue;
            }
            match entry.options(directory, &file, overrides) {
                Ok(opts) => db.commands.entry(file).or_default().push(opts),
                Err(e) => db.warn_entry(&path, i, e),
            }
        }
        Ok(db)
    }

    /// Record why one entry is unusable. Only entries this run would have used
    /// are reported; the rest are not the user's concern.
    fn warn_entry(&mut self, path: &Path, index: usize, e: impl std::fmt::Display) {
        self.warnings.push(format!(
            "{} entry {}: {e}; using inferred configuration if no usable command remains",
            path.display(),
            index + 1
        ));
    }

    /// Record why the database was unusable and fall back to inferred options.
    fn warned(mut self, path: &Path, e: impl std::fmt::Display) -> Self {
        self.warnings.push(format!(
            "{}: {e}; using inferred configuration",
            path.display()
        ));
        self
    }
}

impl Entry {
    /// `directory` and `file` are already resolved against the database.
    fn options(
        self,
        directory: PathBuf,
        file: &Path,
        overrides: &PreprocessOptions,
    ) -> Result<PreprocessOptions, String> {
        if !file.is_file() {
            return Err(format!("source does not exist: {}", file.display()));
        }
        let args = match self.arguments {
            Some(args) => args,
            None => split_command(&self.command.ok_or("missing arguments or command")?)
                .ok_or("unterminated quoting in command")?,
        };
        if args.is_empty() {
            return Err("empty arguments".into());
        }
        let mut opts = overrides.clone();
        opts.working_directory = Some(directory.clone());
        opts.strict_include_search = true;
        // Compiler launchers wrap the real driver, and can be chained
        // (`ccache distcc g++`). The driver decides the default language, so
        // skip past them rather than reading the launcher's name.
        const LAUNCHERS: [&str; 8] = [
            "ccache",
            "sccache",
            "distcc",
            "distcc-pump",
            "gomacc",
            "icecc",
            "icerun",
            "buildcache",
        ];
        let is_launcher = |stem: &str| {
            let stem_lower = stem.to_ascii_lowercase();
            LAUNCHERS.iter().any(|&l| {
                stem_lower == l
                    || stem_lower
                        .strip_prefix(l)
                        .and_then(|r| r.strip_prefix('-'))
                        .is_some_and(|version| {
                            // A version suffix only. `ccache-4.10` is a launcher;
                            // a `ccache-clang++` wrapper is the driver itself, and
                            // skipping it would eat the argument that follows.
                            !version.is_empty()
                                && version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                        })
            })
        };
        let mut driver_idx = 0;
        while driver_idx + 1 < args.len() && is_launcher(binary_stem(&args[driver_idx])) {
            driver_idx += 1;
        }
        let msvc = matches!(
            binary_stem(&args[driver_idx]).to_ascii_lowercase().as_str(),
            "cl" | "clang-cl"
        ) || args.iter().any(|arg| arg == "--driver-mode=cl");
        let driver_cpp = binary_stem(&args[driver_idx]).contains("++");
        let default_language = if driver_cpp {
            Language::Cpp
        } else {
            Language::from_path(file)
        };
        opts.language = Some(default_language);
        let mut explicit_x = false;
        // `-idirafter`/`-iwithprefix` land after every `-isystem` directory,
        // so they are collected separately and appended once parsing is done.
        let mut after_include_paths: Vec<PathBuf> = Vec::new();
        let mut iprefix = String::new();
        let mut source_language = None;
        let mut source_explicit_x = false;
        let mut msvc_language = None;
        let mut updated_cplusplus = !binary_stem(&args[driver_idx]).eq_ignore_ascii_case("cl");
        let mut macros = Vec::new();
        let mut standard = None;
        let mut args = args.iter().skip(driver_idx + 1);
        while let Some(arg) = args.next() {
            // Check the actual input before slash options: an absolute Unix
            // path can start with /I, /D, or /U too.
            if !arg.starts_with('-') && trace_ir::canonicalize(&directory.join(arg)) == file {
                source_language = opts.language;
                source_explicit_x = explicit_x;
                continue;
            }
            // MSVC switches are case-sensitive. Normalize only known driver
            // options, so Unix absolute source paths remain paths.
            let normalized;
            let arg = if msvc {
                let switch = arg
                    .strip_prefix('/')
                    .or_else(|| arg.strip_prefix('-'))
                    .unwrap_or("");
                if matches!(switch, "Zc:__cplusplus" | "Zc:__cplusplus-") {
                    updated_cplusplus = switch == "Zc:__cplusplus";
                    continue;
                }
                if switch == "link" {
                    break;
                }
                if matches!(switch, "TC" | "TP") {
                    msvc_language = Some(if switch == "TC" {
                        Language::C
                    } else {
                        Language::Cpp
                    });
                    continue;
                }
                if let Some((flag, tail)) = ["Tc", "Tp"]
                    .iter()
                    .find_map(|flag| Some((*flag, switch.strip_prefix(flag)?)))
                {
                    let input = if tail.is_empty() {
                        args.next()
                            .ok_or_else(|| format!("missing operand for {arg}"))?
                            .as_str()
                    } else {
                        tail
                    };
                    if trace_ir::canonicalize(&directory.join(input)) == file {
                        source_language = Some(if flag == "Tc" {
                            Language::C
                        } else {
                            Language::Cpp
                        });
                        source_explicit_x = true;
                    }
                    continue;
                }
                if let Some((flag, tail)) = [
                    ("external:I", "-isystem"),
                    ("FI", "-include"),
                    ("I", "-I"),
                    ("D", "-D"),
                    ("U", "-U"),
                    ("std:", "-std="),
                ]
                .iter()
                .find_map(|(prefix, flag)| Some((*flag, switch.strip_prefix(prefix)?)))
                {
                    normalized = format!("{flag}{tail}");
                    &normalized
                } else {
                    if ["Fo", "Fe", "Fd", "Fi", "Fp"].contains(&switch) {
                        args.next()
                            .ok_or_else(|| format!("missing operand for {arg}"))?;
                        continue;
                    }
                    arg
                }
            } else {
                arg
            };
            if arg == "--" {
                break;
            }
            if arg.starts_with('@')
                || arg == "-I-"
                || arg.starts_with("-include-pch")
                || arg.starts_with("-imacros")
                || arg.starts_with("--include-pch")
                || arg.starts_with("--imacros")
            {
                return Err(format!("unsupported preprocessing option {arg}"));
            }
            let std_value = match arg
                .strip_prefix("-std=")
                .or_else(|| arg.strip_prefix("--std="))
            {
                Some(value) => Some(value.to_string()),
                None if arg == "-std" || arg == "--std" => Some(
                    args.next()
                        .ok_or_else(|| format!("missing operand for {arg}"))?
                        .clone(),
                ),
                None => None,
            };
            if let Some(value) = std_value {
                if !explicit_x && !msvc {
                    if let Ok((expected, _)) = standard_macros(&value) {
                        opts.language = Some(expected);
                    }
                }
                standard = Some(value);
                continue;
            }
            // Order is load-bearing once: `-iwithprefixbefore` must precede
            // `-iwithprefix`, or the longer flag parses as the shorter one with
            // a `before` operand.
            const FLAGS: [&str; 11] = [
                "-isystem",
                "-iquote",
                "-idirafter",
                "-iwithprefixbefore",
                "-iwithprefix",
                "-iprefix",
                "-include",
                "-I",
                "-D",
                "-U",
                "-x",
            ];
            if let Some((flag, tail)) = FLAGS.iter().find_map(|flag| {
                let tail = arg.strip_prefix(flag)?;
                // Target options such as -xhost and -x86-asm-syntax
                // are not attached language selectors.
                if *flag == "-x"
                    && !tail.is_empty()
                    && !matches!(
                        tail,
                        "c" | "c++"
                            | "c-header"
                            | "c++-header"
                            | "none"
                            | "assembler"
                            | "assembler-with-cpp"
                            | "objective-c"
                            | "objective-c++"
                            | "c-cpp-output"
                            | "c++-cpp-output"
                    )
                {
                    return None;
                }
                Some((*flag, tail))
            }) {
                let value = if tail.is_empty() {
                    args.next()
                        .ok_or_else(|| format!("missing operand for {flag}"))?
                        .as_str()
                } else {
                    tail
                };
                if value.is_empty() {
                    return Err(format!("empty operand for {flag}"));
                }
                match flag {
                    "-I" => opts.include_paths.push(directory.join(value)),
                    "-iquote" => opts.quote_include_paths.push(directory.join(value)),
                    "-isystem" => opts.system_include_paths.push(directory.join(value)),
                    "-idirafter" => after_include_paths.push(directory.join(value)),
                    // GCC concatenates the prefix textually; it normally ends in `/`.
                    "-iprefix" => iprefix = value.to_string(),
                    "-iwithprefix" => {
                        after_include_paths.push(directory.join(format!("{iprefix}{value}")))
                    }
                    "-iwithprefixbefore" => opts
                        .include_paths
                        .push(directory.join(format!("{iprefix}{value}"))),
                    "-include" => opts.forced_includes.push(PathBuf::from(value)),
                    "-D" => {
                        let (name, value) = value.split_once('=').unwrap_or((value, "1"));
                        macros.push(CommandMacro::Define(name.into(), value.into()));
                    }
                    "-U" => macros.push(CommandMacro::Undef(value.into())),
                    "-x" => {
                        explicit_x = true;
                        opts.language = Some(match value {
                            "c" | "c-header" => Language::C,
                            "c++" | "c++-header" => Language::Cpp,
                            "none" => default_language,
                            _ => return Err(format!("unsupported language {value}")),
                        })
                    }
                    _ => unreachable!(),
                }
                continue;
            }
            if matches!(
                arg.as_str(),
                "-o" | "-MF"
                    | "-MT"
                    | "-MQ"
                    | "-isysroot"
                    | "--sysroot"
                    | "-target"
                    | "--target"
                    | "-arch"
                    | "-Xclang"
                    | "-Xpreprocessor"
                    | "-Xlinker"
                    | "-F"
                    | "-framework"
                    | "-B"
                    | "-mllvm"
                    | "-plugin"
                    | "--config"
            ) {
                args.next()
                    .ok_or_else(|| format!("missing operand for {arg}"))?;
            }
        }
        // A standard is command-wide, but -x applies only to following inputs.
        // Do not let the default captured at the source hide a later -std.
        if !source_explicit_x && !msvc {
            if let Some((language, _)) = standard.as_deref().and_then(|s| standard_macros(s).ok()) {
                source_language = Some(language);
            }
        }
        opts.language = overrides
            .language
            .or(if source_explicit_x {
                source_language
            } else {
                msvc_language.or(source_language)
            })
            .or(opts.language);
        // cl accepts shared project flags for mixed C/C++ sources and ignores
        // a standard switch that does not apply to this source's language.
        if msvc
            && standard
                .as_deref()
                .and_then(|s| standard_macros(s).ok())
                .is_some_and(|(language, _)| opts.language != Some(language))
        {
            standard = None;
        }
        if msvc && standard.is_none() && opts.language == Some(Language::Cpp) {
            standard = Some("c++14".into());
        }
        if let Some(standard) = standard {
            // An unknown `-std` costs the version macros, never the command.
            // The table ages out as compilers gain standards, and discarding
            // the entry would throw away its includes and defines too, so a
            // tree built with a newer standard would silently get no database
            // at all. Language inference at the top of the loop already
            // ignores this same error.
            if let Ok((expected_lang, std_macros)) = standard_macros(&standard) {
                if opts.language != Some(expected_lang) {
                    return Err(format!(
                        "standard {standard} does not match source language"
                    ));
                }
                if msvc {
                    // MSVC dialects do not define GCC's strict-ANSI marker.
                    for op in std_macros {
                        match op {
                            CommandMacro::Define(name, value) if name == "__cplusplus" => {
                                opts.command_macros
                                    .push(CommandMacro::Define("_MSVC_LANG".into(), value.clone()));
                                opts.command_macros.push(CommandMacro::Define(
                                    name,
                                    if updated_cplusplus {
                                        value
                                    } else {
                                        "199711L".into()
                                    },
                                ));
                            }
                            CommandMacro::Define(name, _) | CommandMacro::Undef(name)
                                if name == "__STRICT_ANSI__" => {}
                            op => opts.command_macros.push(op),
                        }
                    }
                } else {
                    opts.command_macros.extend(std_macros);
                }
            }
        }
        opts.command_macros.extend(macros);
        opts.system_include_paths.extend(after_include_paths);
        for paths in [
            &mut opts.include_paths,
            &mut opts.quote_include_paths,
            &mut opts.system_include_paths,
        ] {
            for path in paths.iter_mut() {
                *path = trace_ir::canonicalize(path);
            }
            let mut seen = std::collections::HashSet::new();
            paths.retain(|path| seen.insert(path.clone()));
        }
        // A directory supplied as both -I and -isystem belongs in the system
        // class, as in GCC/Clang. Keep order within each class.
        let systems: std::collections::HashSet<PathBuf> = opts
            .system_include_paths
            .iter()
            .map(|p| trace_ir::canonicalize(p))
            .collect();
        opts.include_paths
            .retain(|p| !systems.contains(&trace_ir::canonicalize(p)));
        Ok(opts)
    }
}

fn standard_macros(standard: &str) -> Result<(Language, Vec<CommandMacro>), String> {
    use CommandMacro::{Define, Undef};
    let standard_lower = standard.to_ascii_lowercase();
    let (expected, value) = match standard_lower.as_str() {
        "c89" | "c90" | "gnu89" | "gnu90" | "iso9899:1990" => (Language::C, None),
        "iso9899:199409" => (Language::C, Some("199409L")),
        "c99" | "gnu99" | "c9x" | "gnu9x" | "iso9899:1999" | "iso9899:199x" => {
            (Language::C, Some("199901L"))
        }
        "c11" | "gnu11" | "c1x" | "gnu1x" | "iso9899:2011" => (Language::C, Some("201112L")),
        "c17" | "c18" | "gnu17" | "gnu18" | "iso9899:2017" | "iso9899:2018" => {
            (Language::C, Some("201710L"))
        }
        "c23" | "gnu23" | "c2x" | "gnu2x" | "iso9899:2024" => (Language::C, Some("202311L")),
        "c++98" | "gnu++98" | "c++03" | "gnu++03" => (Language::Cpp, Some("199711L")),
        "c++11" | "gnu++11" | "c++0x" | "gnu++0x" => (Language::Cpp, Some("201103L")),
        "c++14" | "gnu++14" | "c++1y" | "gnu++1y" => (Language::Cpp, Some("201402L")),
        "c++17" | "gnu++17" | "c++1z" | "gnu++1z" => (Language::Cpp, Some("201703L")),
        "c++20" | "gnu++20" | "c++2a" | "gnu++2a" => (Language::Cpp, Some("202002L")),
        "c++23" | "gnu++23" | "c++2b" | "gnu++2b" => (Language::Cpp, Some("202302L")),
        "c++26" | "gnu++26" | "c++2c" | "gnu++2c" => (Language::Cpp, Some("202400L")),
        _ => return Err(format!("unsupported language standard {standard}")),
    };
    // The version macro is fixed by the language the standard selects.
    let name = if expected == Language::C {
        "__STDC_VERSION__"
    } else {
        "__cplusplus"
    };
    let macros = vec![
        match value {
            Some(value) => Define(name.into(), value.into()),
            None => Undef(name.into()),
        },
        if standard_lower.starts_with("gnu") {
            Undef("__STRICT_ANSI__".into())
        } else {
            Define("__STRICT_ANSI__".into(), "1".into())
        },
    ];
    Ok((expected, macros))
}

/// Split a `command` string into arguments.
///
/// POSIX-shell rules, with one deliberate departure: a backslash escapes only
/// a quote or a space. A database written on Windows spells its paths
/// `C:\proj\include`, and a shell-faithful split would silently swallow those
/// separators, leaving a corrupted include directory that never resolves —
/// while `-DVERSION=\"1.0\"`, the escape that does appear in generated
/// commands, keeps working.
fn split_command(command: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '\'' => {
                // Single quotes take everything literally, as in a shell.
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return None,
                    }
                }
            }
            '"' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if matches!(chars.peek(), Some('"' | '\'' | '\\')) => {
                            current.push(chars.next()?)
                        }
                        Some(c) => current.push(c),
                        None => return None,
                    }
                }
            }
            '\\' if matches!(chars.peek(), Some(c) if *c == '"' || *c == '\'' || c.is_whitespace()) =>
            {
                has_token = true;
                current.push(chars.next()?);
            }
            c => {
                has_token = true;
                current.push(c);
            }
        }
    }
    if has_token {
        args.push(current);
    }
    Some(args)
}

fn binary_stem(arg: &str) -> &str {
    let filename = arg.rsplit(['/', '\\']).next().unwrap_or(arg);
    // `get` rather than a slice: a compiler path may end in a multi-byte
    // character, and `filename[len - 4..]` panics when that index falls
    // inside one.
    match filename.len().checked_sub(4) {
        Some(cut)
            if filename
                .get(cut..)
                .is_some_and(|s| s.eq_ignore_ascii_case(".exe")) =>
        {
            &filename[..cut]
        }
        _ => filename,
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    fn options(args: &[&str]) -> Result<PreprocessOptions, String> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.c");
        std::fs::write(&file, "").unwrap();
        Entry {
            directory: dir.path().into(),
            file: file.clone(),
            arguments: Some(args.iter().map(|s| s.to_string()).collect()),
            command: None,
        }
        .options(
            trace_ir::canonicalize(dir.path()),
            &trace_ir::canonicalize(&file),
            &PreprocessOptions::new(),
        )
    }

    #[test]
    fn review_standard_after_source() {
        let opts = options(&["clang", "main.c", "-std=c++17"]).unwrap();
        assert_eq!(opts.language, Some(Language::Cpp));
    }

    #[test]
    fn review_x_remains_positional() {
        assert_eq!(
            options(&["cc", "-xc++", "main.c", "-xc"]).unwrap().language,
            Some(Language::Cpp)
        );
        assert_eq!(
            options(&["cc", "main.c", "-xc++"]).unwrap().language,
            Some(Language::C)
        );
    }

    #[test]
    fn review_non_language_x_flags() {
        assert!(options(&["clang", "-x86-asm-syntax=intel", "-xhost", "main.c"]).is_ok());
        assert!(options(&["cc", "-x", "assembler", "main.c"]).is_err());
    }

    #[test]
    fn review_attached_output_flags_do_not_consume_next_option() {
        for flags in [["-omain.o", "-MFfile.d"], ["-MTtarget", "-MQquoted"]] {
            let opts = options(&["cc", flags[0], flags[1], "-DKEEP=1", "main.c"]).unwrap();
            assert!(opts
                .command_macros
                .iter()
                .any(|m| matches!(m, CommandMacro::Define(n, v) if n == "KEEP" && v == "1")));
        }
    }

    #[test]
    fn review_double_dash_unsupported() {
        for flag in [
            "--imacros",
            "--imacros=defs.h",
            "--include-pch",
            "--include-pch=defs.pch",
        ] {
            assert!(
                options(&["cc", flag, "defs.h", "main.c"]).is_err(),
                "{flag}"
            );
        }
    }

    #[test]
    fn review_msvc_preprocessing_flags() {
        let opts = options(&[
            "cl.exe",
            "/Iinc",
            "/external:I",
            "sys",
            "/DVALUE=2",
            "/UOLD",
            "/FIforced.h",
            "main.c",
            "/TP",
            "/std:c++17",
        ])
        .unwrap();
        assert_eq!(opts.language, Some(Language::Cpp));
        assert_eq!(opts.include_paths.len(), 1);
        assert_eq!(opts.system_include_paths.len(), 1);
        assert_eq!(opts.forced_includes, vec![PathBuf::from("forced.h")]);
        assert!(opts
            .command_macros
            .iter()
            .any(|m| matches!(m, CommandMacro::Define(n, v) if n == "VALUE" && v == "2")));
        assert!(opts
            .command_macros
            .iter()
            .any(|m| matches!(m, CommandMacro::Undef(n) if n == "OLD")));
    }

    #[test]
    fn review_msvc_language_and_output_operands() {
        for args in [
            vec!["CL.EXE", "/TP", "main.c"],
            vec!["clang-cl", "main.c", "-TP"],
            vec!["cl", "/TC", "/Tpmain.c"],
            vec!["cl", "/Tp", "main.c", "/TC"],
            vec!["cl", "/Fo", "main.c", "/TP", "main.c"],
        ] {
            assert_eq!(
                options(&args).unwrap().language,
                Some(Language::Cpp),
                "{args:?}"
            );
        }
    }

    #[test]
    fn review_msvc_inapplicable_standard_preserves_source_language() {
        for args in [
            vec!["cl", "/std:c++17", "main.c"],
            vec!["clang-cl", "/TC", "/std:c++17", "main.c"],
        ] {
            assert_eq!(options(&args).unwrap().language, Some(Language::C));
        }
    }

    #[test]
    fn review_msvc_standard_macros() {
        let opts = options(&["cl", "/TP", "/std:c++20", "main.c"]).unwrap();
        assert!(opts.command_macros.iter().any(
            |m| matches!(m, CommandMacro::Define(n,v) if n == "_MSVC_LANG" && v == "202002L")
        ));
        assert!(opts.command_macros.iter().any(
            |m| matches!(m, CommandMacro::Define(n,v) if n == "__cplusplus" && v == "199711L")
        ));
        assert!(!opts
            .command_macros
            .iter()
            .any(|m| matches!(m, CommandMacro::Define(n,_) if n == "__STRICT_ANSI__")));
        let opts = options(&["cl", "/TP", "/std:c++20", "/Zc:__cplusplus", "main.c"]).unwrap();
        assert!(opts.command_macros.iter().any(
            |m| matches!(m, CommandMacro::Define(n,v) if n == "__cplusplus" && v == "202002L")
        ));
    }

    #[test]
    fn review_include_paths_are_canonical() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("inc")).unwrap();
        std::fs::create_dir(dir.path().join("build")).unwrap();
        let file = dir.path().join("main.c");
        std::fs::write(&file, "").unwrap();
        let opts = Entry {
            directory: dir.path().into(),
            file: file.clone(),
            command: None,
            arguments: Some(vec![
                "cc".into(),
                "-Ibuild/../inc".into(),
                "-iquoteinc".into(),
                "-isystembuild/../inc".into(),
            ]),
        }
        .options(
            trace_ir::canonicalize(dir.path()),
            &trace_ir::canonicalize(&file),
            &PreprocessOptions::new(),
        )
        .unwrap();
        let expected = trace_ir::canonicalize(&dir.path().join("inc"));
        assert_eq!(opts.quote_include_paths, vec![expected.clone()]);
        assert_eq!(opts.system_include_paths, vec![expected]);
        assert!(opts.include_paths.is_empty());
    }
}
