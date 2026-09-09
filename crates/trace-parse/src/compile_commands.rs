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
        let mut macros = Vec::new();
        let mut standard = None;
        let mut args = args.iter().skip(driver_idx + 1);
        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
            if !arg.starts_with('-') && trace_ir::canonicalize(&directory.join(arg)) == file {
                source_language = opts.language;
                continue;
            }
            if arg.starts_with('@')
                || arg == "-I-"
                || arg.starts_with("-include-pch")
                || arg.starts_with("-imacros")
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
                if !explicit_x {
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
            if let Some((flag, tail)) = FLAGS
                .iter()
                .find_map(|flag| Some((*flag, arg.strip_prefix(flag)?)))
            {
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
        opts.language = overrides.language.or(source_language).or(opts.language);
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
                opts.command_macros.extend(std_macros);
            }
        }
        opts.command_macros.extend(macros);
        opts.system_include_paths.extend(after_include_paths);
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
