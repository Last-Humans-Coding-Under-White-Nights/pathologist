//! Build-target metadata, read as data without executing build commands.
//!
//! Command databases describe object membership; CMake replies describe source
//! membership directly. Both feed the same target representation.
use crate::compile_commands::{driver_stem, split_command, CompilationDatabase};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use trace_ir::resolve_against;

#[derive(Default, Debug)]
pub(crate) struct LinkDatabase {
    pub targets: Vec<LinkTargetSpec>,
    pub warnings: Vec<String>,
}
#[derive(Debug)]
pub(crate) struct LinkTargetSpec {
    pub name: String,
    pub output: PathBuf,
    pub sources: Vec<PathBuf>,
    pub dependencies: Vec<usize>,
    pub configurations: BTreeMap<PathBuf, BTreeSet<usize>>,
}

#[derive(Deserialize)]
struct Command {
    directory: PathBuf,
    output: Option<PathBuf>,
    arguments: Option<Vec<String>>,
    command: Option<String>,
}
struct Link {
    output: PathBuf,
    inputs: Vec<PathBuf>,
    libraries: Vec<String>,
    search: Vec<PathBuf>,
}

impl LinkDatabase {
    pub fn load(
        root: &Path,
        compilation: &CompilationDatabase,
        links: Option<&Path>,
    ) -> Result<Self, String> {
        let root = trace_ir::canonicalize(root);
        let root = if root.is_file() {
            root.parent().unwrap_or(&root)
        } else {
            &root
        };
        let mut db = Self::default();
        let database = compilation.path.as_ref().and_then(|p| p.parent());
        let link_path = database_path(root, database, links, "link_commands.json")?;
        let mut batches = Vec::new();
        if let Some(path) = &compilation.path {
            batches.push((path.clone(), compilation.link_entries.clone()));
        }
        if let Some(path) = link_path {
            match read_json(&path)
                .and_then(|v| serde_json::from_value::<Vec<Value>>(v).map_err(|e| e.to_string()))
            {
                Ok(entries) => batches.push((path, entries.into_iter().enumerate().collect())),
                Err(e) => db.warnings.push(format!("{}: {e}", path.display())),
            }
        }
        let mut commands = Vec::new();
        for (path, entries) in batches {
            for (index, value) in entries {
                let parsed = (|| -> Result<(), String> {
                    let entry: Command =
                        serde_json::from_value(value).map_err(|e| e.to_string())?;
                    let directory =
                        resolve_against(path.parent().unwrap_or(root), &entry.directory);
                    let args = match entry.arguments {
                        Some(args) => args,
                        None => split_command(
                            entry
                                .command
                                .as_deref()
                                .ok_or("missing arguments or command")?,
                        )
                        .ok_or("unterminated command quoting")?,
                    };
                    let args = expand_response_files(&directory, args, 0)?;
                    if args.is_empty() {
                        return Err("empty command".into());
                    }
                    if is_compile_command(&args) {
                        return Err(
                            "compilation command in link database; use compile_commands.json"
                                .into(),
                        );
                    }
                    // Normalize forwarding before anything reads the
                    // operands, so `-o` is found wherever the driver spelled it.
                    let args = forwarded_operands(&args)?;
                    let output = entry
                        .output
                        .map(|p| resolve_against(&directory, &p))
                        .or_else(|| {
                            command_output(&args).map(|p| resolve_against(&directory, Path::new(p)))
                        });
                    let link = parse_link(&directory, &args, output)?
                        .ok_or("unsupported link command (no recognizable output and inputs)")?;
                    commands.push(link);
                    Ok(())
                })();
                if let Err(e) = parsed {
                    db.warnings
                        .push(format!("{} entry {}: {e}", path.display(), index + 1));
                }
            }
        }
        for build in metadata_roots(root, database) {
            let start = db.targets.len();
            if let Err(e) = db.read_cmake(&build) {
                db.targets.truncate(start);
                db.warnings
                    .push(format!("CMake metadata {}: {e}", build.display()));
            } else if db.targets.len() > start {
                // Use one build tree, as with a multi-config reply. Another
                // reply can describe the same artifacts and duplicate scopes.
                break;
            }
        }
        let mut by_output: BTreeMap<PathBuf, usize> = db
            .targets
            .iter()
            .enumerate()
            .map(|(i, t)| (t.output.clone(), i))
            .collect();
        for link in &commands {
            if !by_output.contains_key(&link.output) {
                let index = db.targets.len();
                by_output.insert(link.output.clone(), index);
                db.targets.push(LinkTargetSpec {
                    // A path with no final component still needs a name.
                    name: link
                        .output
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| link.output.display().to_string()),
                    output: link.output.clone(),
                    sources: Vec::new(),
                    dependencies: Vec::new(),
                    configurations: BTreeMap::new(),
                });
            }
        }
        for link in commands {
            let index = by_output[&link.output];
            let target = &mut db.targets[index];
            for input in link.inputs {
                if let Some(&dependency) = by_output.get(&input) {
                    if dependency != index {
                        target.dependencies.push(dependency);
                    }
                } else if let Some(configurations) = compilation.objects.get(&input) {
                    for (source, ordinal) in configurations {
                        target.sources.push(source.clone());
                        target
                            .configurations
                            .entry(source.clone())
                            .or_default()
                            .insert(*ordinal);
                    }
                } else if is_source(&input) {
                    target.sources.push(input);
                } else if is_object(&input) {
                    db.warnings.push(format!(
                        "{}: no compilation source for object {}",
                        link.output.display(),
                        input.display()
                    ));
                }
            }
            for library in link.libraries {
                // Resolve only known build outputs. System libraries need no
                // target and filesystem availability must not affect the graph.
                let names = if let Some(exact) = library.strip_prefix(':') {
                    vec![exact.to_owned()]
                } else {
                    vec![
                        format!("lib{library}.so"),
                        format!("lib{library}.a"),
                        format!("lib{library}.dylib"),
                    ]
                };
                // A versioned soname (`libfoo.so.1.2.3`, what CMake's VERSION
                // and every libtool build produce) is the artifact's real file
                // name, and `-lfoo` has to find it. Exact keys are tried first
                // so an unversioned target still wins when both exist.
                let versioned = library
                    .strip_prefix(':')
                    .is_none()
                    .then(|| format!("lib{library}.so."));
                for directory in &link.search {
                    let mut dependency = names
                        .iter()
                        .find_map(|name| {
                            by_output.get(&resolve_against(directory, Path::new(name)))
                        })
                        .copied();
                    if dependency.is_none() {
                        if let Some(prefix) = &versioned {
                            dependency = by_output
                                .iter()
                                .find(|(output, _)| {
                                    output.parent() == Some(directory.as_path())
                                        && output.file_name().is_some_and(|n| {
                                            n.to_string_lossy().starts_with(prefix.as_str())
                                        })
                                })
                                .map(|(_, &i)| i);
                        }
                    }
                    if let Some(dependency) = dependency {
                        if dependency != index {
                            target.dependencies.push(dependency);
                        }
                        break;
                    }
                }
            }
        }
        for target in &mut db.targets {
            target.sources.sort();
            target.sources.dedup();
            target.dependencies.sort_unstable();
            target.dependencies.dedup();
        }
        Ok(db)
    }

    fn read_cmake(&mut self, build: &Path) -> Result<(), String> {
        let reply = build.join(".cmake/api/v1/reply");
        let entries = match std::fs::read_dir(&reply) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        let index = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|s| s.starts_with("index-") && s.ends_with(".json"))
            })
            .max();
        let Some(index) = index else { return Ok(()) };
        let index = read_json(&index)?;
        let Some(reference) = array(&index, "objects")
            .iter()
            .find(|v| v["kind"] == "codemodel" && v["version"]["major"] == 2)
        else {
            return Ok(());
        };
        let model = read_json(&reply.join(required_str(reference, "jsonFile")?))?;
        let source_root = PathBuf::from(required_str(&model["paths"], "source")?);
        let build_root = PathBuf::from(required_str(&model["paths"], "build")?);
        // A multi-config reply (Xcode, Visual Studio, Ninja Multi-Config)
        // repeats one target per configuration, differing only in artifact
        // path. Reading every one would index each source once per
        // configuration — the same facts at N times the cost — so only the
        // first is taken and the rest are reported.
        let configurations = array(&model, "configurations");
        let skipped: Vec<&str> = configurations
            .iter()
            .skip(1)
            .filter_map(|config| config["name"].as_str())
            .collect();
        if !skipped.is_empty() {
            let first = configurations[0]["name"].as_str().unwrap_or("the first");
            self.warnings.push(format!(
                "CMake metadata {}: reading configuration {first}; ignoring {}",
                build.display(),
                skipped.join(", ")
            ));
        }
        for config in configurations.iter().take(1) {
            let mut ids = BTreeMap::new();
            let mut pending = Vec::new();
            for reference in array(config, "targets") {
                let target = read_json(&reply.join(required_str(reference, "jsonFile")?))?;
                let kind = required_str(&target, "type")?;
                if !matches!(
                    kind,
                    "EXECUTABLE"
                        | "SHARED_LIBRARY"
                        | "STATIC_LIBRARY"
                        | "MODULE_LIBRARY"
                        | "OBJECT_LIBRARY"
                ) {
                    continue;
                }
                let name = required_str(&target, "name")?.to_owned();
                let output = array(&target, "artifacts")
                    .first()
                    .and_then(|a| a["path"].as_str())
                    .map(|s| resolve_against(&build_root, Path::new(s)))
                    .unwrap_or_else(|| build_root.join(format!(".trace-object-target-{name}")));
                let index = self.targets.len();
                ids.insert(required_str(reference, "id")?.to_owned(), index);
                let sources = array(&target, "sources")
                    .iter()
                    .filter_map(|s| {
                        let path = Path::new(s["path"].as_str()?);
                        is_source(path).then(|| resolve_against(&source_root, path))
                    })
                    .collect();
                self.targets.push(LinkTargetSpec {
                    name,
                    output,
                    sources,
                    dependencies: Vec::new(),
                    configurations: BTreeMap::new(),
                });
                pending.push((index, target));
            }
            for (index, target) in pending {
                self.targets[index].dependencies = array(&target, "dependencies")
                    .iter()
                    .filter_map(|dependency| {
                        let id = *ids.get(dependency["id"].as_str()?)?;
                        cmake_link_dependency(&target, dependency, &self.targets[id], &build_root)
                            .then_some(id)
                    })
                    .filter(|&id| id != index)
                    .collect();
            }
        }
        Ok(())
    }
}

/// CMake's dependency list is a build graph, so `add_dependencies` alone
/// supplies ordering, not objects. A target can be both ordered and linked;
/// explicit link relationships or library fragments take precedence then.
fn cmake_link_dependency(
    target: &Value,
    dependency: &Value,
    candidate: &LinkTargetSpec,
    build_root: &Path,
) -> bool {
    let id = &dependency["id"];
    if ["linkLibraries", "objectDependencies"]
        .iter()
        .any(|key| array(target, key).iter().any(|entry| &entry["id"] == id))
    {
        return true;
    }
    let graph = &target["backtraceGraph"];
    let mut node = dependency["backtrace"].as_u64();
    let mut ordering = array(target, "orderDependencies")
        .iter()
        .any(|entry| &entry["id"] == id);
    // Bound traversal even for malformed reply graphs containing cycles.
    for _ in 0..array(graph, "nodes").len() {
        let Some(current) = node.and_then(|n| array(graph, "nodes").get(n as usize)) else {
            break;
        };
        if let Some(command) = current["command"]
            .as_u64()
            .and_then(|n| array(graph, "commands").get(n as usize))
            .and_then(Value::as_str)
        {
            if command == "target_link_libraries" {
                return true;
            }
            if command == "add_dependencies" {
                ordering = true;
                break;
            }
        }
        node = current["parent"].as_u64();
    }
    if !ordering {
        return true;
    }
    array(&target["link"], "commandFragments")
        .iter()
        .filter(|fragment| fragment["role"] == "libraries")
        .filter_map(|fragment| split_command(fragment["fragment"].as_str()?))
        .flatten()
        .any(|argument| resolve_against(build_root, Path::new(&argument)) == candidate.output)
}

/// Fast path for ordinary compilation records; parse arguments only for
/// records whose file is absent or names an artifact.
pub(crate) fn is_link_only_entry(entry: &Value) -> bool {
    if entry["file"]
        .as_str()
        .is_some_and(|file| is_source(Path::new(file)))
    {
        return false;
    }
    // Borrow out of the JSON rather than copying the argument list: link lines
    // are the long ones, and this runs for every entry that is not plainly a
    // source. Only the `command` spelling has to allocate, since it is split.
    let split;
    let args: Vec<&str> = match entry["arguments"].as_array() {
        Some(values) => values.iter().filter_map(Value::as_str).collect(),
        None => {
            let Some(parts) = entry["command"].as_str().and_then(split_command) else {
                return false;
            };
            split = parts;
            split.iter().map(String::as_str).collect()
        }
    };
    if args.is_empty() || is_compile_command(&args) {
        return false;
    }
    command_output(&args).is_some()
        || args
            .iter()
            .skip(1)
            .any(|s| is_object(Path::new(s)) || is_library(Path::new(s)))
}

/// Whether any link metadata is reachable, without reading it. Lets the
/// compilation reader skip per-command object bookkeeping that only the target
/// reader consumes; a handful of stats against tens of thousands of commands.
pub(crate) fn metadata_exists(
    root: &Path,
    database: Option<&Path>,
    explicit: Option<&Path>,
) -> bool {
    if explicit.is_some() {
        return true;
    }
    let root = trace_ir::canonicalize(root);
    let root = if root.is_file() {
        root.parent().unwrap_or(&root).to_path_buf()
    } else {
        root
    };
    metadata_roots(&root, database).iter().any(|dir| {
        dir.join("link_commands.json").is_file() || dir.join(".cmake/api/v1/reply").is_dir()
    })
}

/// Where link metadata is looked for, in priority order. An out-of-source
/// build keeps its link commands and its CMake reply beside the compilation
/// database it also wrote, which is neither the analysis root nor `root/build`
/// — and a probe that omitted that directory answered "no metadata" for the
/// reader that had already found it there.
fn metadata_roots(root: &Path, database: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![root.to_path_buf(), root.join("build")];
    if let Some(directory) = database {
        if !roots.iter().any(|dir| dir == directory) {
            roots.push(directory.to_path_buf());
        }
    }
    roots
}

fn database_path(
    root: &Path,
    database: Option<&Path>,
    explicit: Option<&Path>,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    if let Some(path) = explicit {
        if !path.is_file() {
            return Err(format!(
                "metadata database does not exist or is not a file: {}",
                path.display()
            ));
        }
        return Ok(Some(trace_ir::canonicalize(path)));
    }
    Ok(metadata_roots(root, database)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|p| p.is_file()))
}
fn read_json(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}
fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key].as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("missing string field {key}"))
}
/// Discovery decides what a translation unit is; a link input is a source
/// only when indexing would produce a unit for it. A second spelling of that
/// list would silently drop targets built from `.c++` files, or admit sources
/// that never become units.
fn is_source(path: &Path) -> bool {
    crate::discover::has_extension_in(path, crate::discover::TU_EXTENSIONS)
}
pub(crate) fn is_object(path: &Path) -> bool {
    matches!(path.extension().and_then(|s| s.to_str()), Some("o" | "obj"))
}
fn is_library(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("a" | "so" | "dylib" | "lib" | "dll")
    ) || path.to_string_lossy().contains(".so.")
}
/// Valueless options that begin with `-o`. Options that begin with `-o` and
/// *take* an operand are in [`OPERAND_IS_NOT_INPUT`]; both are consulted when
/// deciding whether `-o…` is an output joined to its operand, so neither list
/// has to repeat the other.
const VALUELESS_O_OPTIONS: &[&str] = &["-onelevel_namespace", "-output_format", "-openmp"];

/// The artifact a command writes.
///
/// Explicit spellings are searched first and the joined `-oapp` form only if
/// none matched. A single left-to-right scan let a valueless flag that merely
/// begins with `-o` — ld64's `-onelevel_namespace` — match before the real
/// `-o <path>` later on the line and name the target after it.
pub(crate) fn command_output<S: AsRef<str>>(args: &[S]) -> Option<&str> {
    if args.is_empty() {
        return None;
    }
    // The MSVC spellings are honoured only under an MSVC driver. `/Fo` matches
    // any argument whose first three bytes are `/fo`, so unguarded it claimed
    // `-isystem /foundation/include` — and, being a left-to-right scan, it won
    // over the real `-o <path>` further along the line.
    let msvc = is_msvc_driver(args);
    let explicit = args.iter().enumerate().find_map(|(i, arg)| {
        let arg = arg.as_ref();
        // `/Fo` names the object file, joined (`/Foout.obj`), joined through a
        // colon (`/Fo:out.obj`), or — as generators and clang-cl also spell it
        // — as its own token. The dash spelling is matched case-sensitively:
        // case-folded, its three bytes are the head of every `-f…` option, and
        // `-fopenmp` would name the object `penmp`.
        let object = msvc
            .then(|| strip_prefix_ignore_case(arg, "/Fo").or_else(|| arg.strip_prefix("-Fo")))
            .flatten()
            .map(|rest| rest.strip_prefix(':').unwrap_or(rest));
        if arg == "-o" || arg == "--output" || object == Some("") {
            args.get(i + 1).map(|next| next.as_ref())
        } else {
            arg.strip_prefix("--output=")
                .or_else(|| {
                    msvc.then(|| strip_prefix_ignore_case(arg, "/OUT:"))
                        .flatten()
                })
                .or(object)
                // An option that carries no path names no output; taking the
                // empty string ended the search and discarded the real one.
                .filter(|rest| !rest.is_empty())
        }
    });
    explicit.or_else(|| {
        args.iter().find_map(|arg| {
            let arg = arg.as_ref();
            // `-oformat=elf32` and friends carry the value in the same token,
            // so the option name is whatever precedes the `=`.
            let name = arg.split_once('=').map_or(arg, |(name, _)| name);
            arg.strip_prefix("-o").filter(|rest| {
                !rest.is_empty()
                    && !VALUELESS_O_OPTIONS.contains(&name)
                    && !OPERAND_IS_NOT_INPUT.contains(&name)
            })
        })
    })
}

/// Whether `args` is driven by an MSVC-style tool, which spells options with
/// `/`. Under every other driver a leading `/` starts an absolute path — and
/// CMake and Bazel emit absolute paths for inputs and for `-isystem`/`-L`
/// operands, so reading `/foundation/include` as `/Fo…` would name the target
/// after it.
pub(crate) fn is_msvc_driver<S: AsRef<str>>(args: &[S]) -> bool {
    let driver = driver_stem(args);
    let librarian = driver == "lib" || driver == "llvm-lib" || driver.ends_with("-lib");
    matches!(driver.as_str(), "link" | "lld-link" | "cl" | "clang-cl")
        || driver.ends_with("-link")
        || driver.ends_with("-cl")
        || librarian
        || args.iter().any(|a| a.as_ref() == "--driver-mode=cl")
}

/// MSVC options are case-insensitive, and generators emit both `/OUT:` and
/// `/out:`.
fn strip_prefix_ignore_case<'a>(arg: &'a str, prefix: &str) -> Option<&'a str> {
    arg.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &arg[prefix.len()..])
}

/// A `-c` (or MSVC `/c`) operand marks a compilation, not a link.
pub(crate) fn is_compile_command<S: AsRef<str>>(args: &[S]) -> bool {
    args.iter()
        .any(|s| s.as_ref() == "-c" || s.as_ref().eq_ignore_ascii_case("/c"))
}

/// A Darwin dynamic-loader path. These share `@` with response-file syntax but
/// name a load-time location, and no file exists at them now.
fn is_loader_path(arg: &str) -> bool {
    arg.starts_with("@rpath")
        || arg.starts_with("@executable_path")
        || arg.starts_with("@loader_path")
}

fn expand_response_files(
    directory: &Path,
    args: Vec<String>,
    depth: usize,
) -> Result<Vec<String>, String> {
    if depth > 8 {
        return Err("response file nesting exceeds eight levels".into());
    }
    let mut expanded = Vec::new();
    for arg in args {
        // `@rpath/libfoo.dylib`, `@executable_path/...` and `@loader_path/...`
        // are Darwin dynamic-loader paths, not response files, and a bare `@`
        // names no file at all — joined to the command's directory it reopens
        // that directory. Reading any of them fails, and the error drops the
        // whole link command.
        if let Some(path) = arg
            .strip_prefix('@')
            .filter(|path| !path.is_empty() && !is_loader_path(&arg))
        {
            let text = std::fs::read_to_string(directory.join(path))
                .map_err(|e| format!("response file {path}: {e}"))?;
            let args = split_command(&text).ok_or("unterminated response file quoting")?;
            expanded.extend(expand_response_files(directory, args, depth + 1)?);
        } else {
            expanded.push(arg);
        }
    }
    Ok(expanded)
}
/// Linker options whose separate operand is a path but *not* a link input.
/// Left unconsumed, `-object_path_lto out.o` contributes a phantom object and
/// `-install_name @rpath/lib.dylib` a phantom library. Options whose operand
/// really is an input (`-force_load lib.a`) are deliberately absent.
const OPERAND_IS_NOT_INPUT: &[&str] = &[
    "-object_path_lto",
    "-order_file",
    "-objc_abi_version",
    "-exported_symbols_list",
    "-unexported_symbols_list",
    "-filelist",
    "-install_name",
    // A library's own soname is not one of its inputs; left unconsumed,
    // `-Wl,-soname,libfoo.so.1` makes the target depend on itself.
    "-soname",
    "--soname",
    "-h",
    "-rpath",
    "--rpath",
    // A search directory for a shared library's own dependencies, not an
    // input; left unconsumed it is read as one whenever it happens to look
    // like a library path.
    "-rpath-link",
    "--rpath-link",
    "-R",
    "-syslibroot",
    "-bundle_loader",
    // A linker script is not an input either, and failing the command over one
    // would drop the whole target — every embedded build emits `-T`.
    "-T",
    "--script",
    "-oformat",
];

/// Unwrap the two spellings a compiler driver uses to forward operands to the
/// linker: `-Wl,a,b` splits on commas and `-Xlinker a` passes its single
/// operand through.
///
/// Everything that interprets a *link* command works on the result, output
/// detection included — `-Xlinker -o -Xlinker libhook.dylib` names its output
/// through the forwarded form, and scanning the raw arguments picked
/// `-Xlinker` as the filename and registered the target under it.
///
/// Two callers of the shared helpers deliberately stay on raw arguments:
/// `is_link_only_entry`, which classifies tens of thousands of compilation
/// entries and is kept allocation-light, and the compilation reader, whose
/// commands carry no linker forwarding. Neither would change its answer.
fn forwarded_operands(args: &[String]) -> Result<Vec<String>, String> {
    let mut forwarded: Vec<String> = Vec::with_capacity(args.len());
    let mut source = args.iter();
    while let Some(arg) = source.next() {
        match arg.strip_prefix("-Wl,") {
            Some(tail) => forwarded.extend(tail.split(',').map(str::to_owned)),
            None if arg == "-Xlinker" => {
                let value = source
                    .next()
                    .ok_or_else(|| format!("missing operand for {arg}"))?;
                forwarded.push(value.clone());
            }
            None => forwarded.push(arg.clone()),
        }
    }
    Ok(forwarded)
}

/// `args` must already have been through [`forwarded_operands`].
fn parse_link(
    directory: &Path,
    args: &[String],
    output: Option<PathBuf>,
) -> Result<Option<Link>, String> {
    if args
        .iter()
        .any(|s| matches!(s.as_str(), "&&" | ";" | "||" | "|"))
    {
        return Err("compound shell commands are unsupported".into());
    }
    // Shared with the compilation reader, so a launcher-prefixed link line
    // (CMake's `RULE_LAUNCH_LINK`) reads the driver rather than `ccache`, and
    // Windows' `LINK.EXE` folds to the name matched below.
    let driver = driver_stem(args);
    // `lib` / `llvm-lib` are the MSVC librarian. Anchored, so an unrelated
    // `customlib` is not mistaken for one.
    let librarian = driver == "lib" || driver == "llvm-lib" || driver.ends_with("-lib");
    let archive = driver == "ar" || driver.ends_with("-ar") || librarian;
    let msvc = is_msvc_driver(args);
    let mut output = output;
    let mut inputs = Vec::new();
    let mut libraries = Vec::new();
    let mut search = Vec::new();
    // Skip the launcher chain and the driver itself, not just `args[0]`.
    let mut iter = args
        .iter()
        .skip(crate::compile_commands::driver_index(args) + 1);
    while let Some(arg) = iter.next() {
        if matches!(arg.as_str(), "-o" | "--output" | "-L" | "-l")
            || OPERAND_IS_NOT_INPUT.contains(&arg.as_str())
        {
            let value = iter
                .next()
                .ok_or_else(|| format!("missing operand for {arg}"))?;
            match arg.as_str() {
                "-L" => search.push(resolve_against(directory, Path::new(value))),
                "-l" => libraries.push(value.clone()),
                _ => {}
            }
        } else if let Some(value) = arg.strip_prefix("-L") {
            search.push(resolve_against(directory, Path::new(value)));
        } else if let Some(value) = arg.strip_prefix("-l") {
            libraries.push(value.to_owned());
        } else if arg.starts_with('-') || (msvc && arg.starts_with('/')) || is_loader_path(arg) {
            // An MSVC `/flag` is an option, not a path: `/implib:foo.lib` and
            // `/NODEFAULTLIB:libcmt.lib` end in a library extension and would
            // otherwise be read as link inputs. Under any other driver `/…` is
            // an absolute path and must reach the input branch below.
            continue;
        } else {
            let input = resolve_against(directory, Path::new(arg));
            if archive && output.is_none() && is_library(&input) {
                output = Some(input);
            } else if is_source(&input) || is_object(&input) || is_library(&input) {
                inputs.push(input);
            }
        }
    }
    Ok(output
        .filter(|_| !inputs.is_empty() || !libraries.is_empty())
        .map(|output| Link {
            output,
            inputs,
            libraries,
            search,
        }))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn load(
        root: &Path,
        compilation: Option<&Path>,
        links: Option<&Path>,
    ) -> Result<LinkDatabase, String> {
        let mut options = trace_preproc::PreprocessOptions::new();
        options.compilation_database = compilation.map(Path::to_path_buf);
        let compilation = CompilationDatabase::load(root, &options)?;
        LinkDatabase::load(root, &compilation, links)
    }
    fn write(root: &Path, name: &str, value: serde_json::Value) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }
    #[test]
    fn review_empty_command_helpers() {
        let args: &[&str] = &[];
        assert_eq!(command_output(args), None);
        assert!(!is_msvc_driver(args));
        assert_eq!(driver_stem(args), "");
    }

    #[test]
    fn review_library_search_stops_at_first_directory() {
        let dir = tempfile::tempdir().unwrap();
        for first in ["libfoo.a", "libfoo.so.1"] {
            write(
                dir.path(),
                "link_commands.json",
                json!([
                    {"directory":".","arguments":["cc","a.c","-o",format!("first/{first}")]},
                    {"directory":".","arguments":["cc","b.c","-o","second/libfoo.so"]},
                    {"directory":".","arguments":["cc","main.c","-Lfirst","-Lsecond","-lfoo","-o","app"]}
                ]),
            );
            let db = load(dir.path(), None, None).unwrap();
            assert!(db.warnings.is_empty(), "{:?}", db.warnings);
            assert_eq!(db.targets[2].dependencies, vec![0]);
        }
    }

    #[test]
    fn review_wrapped_cmake_dependencies_follow_backtrace() {
        let candidate = LinkTargetSpec {
            name: "lib".into(),
            output: PathBuf::from("/build/lib.a"),
            sources: Vec::new(),
            dependencies: Vec::new(),
            configurations: BTreeMap::new(),
        };
        for (command, expected) in [("target_link_libraries", true), ("add_dependencies", false)] {
            let target = json!({
                "orderDependencies":[{"id":"lib"}],
                "backtraceGraph":{"commands":["wrapper", command],
                    "nodes":[{"command":0,"parent":1},{"command":1}]}
            });
            assert_eq!(
                cmake_link_dependency(
                    &target,
                    &json!({"id":"lib","backtrace":0}),
                    &candidate,
                    Path::new("/build")
                ),
                expected
            );
        }
    }

    fn review_cmake_reply(root: &Path, reply: &str, dependencies: Value) {
        write(
            root,
            &format!("{reply}/index-001.json"),
            json!({"objects":[{"kind":"codemodel","version":{"major":2},"jsonFile":"model.json"}]}),
        );
        write(
            root,
            &format!("{reply}/model.json"),
            json!({"paths":{"source":root,"build":root.join("build")},"configurations":[{"targets":[{"id":"a","jsonFile":"a.json"}]}]}),
        );
        write(
            root,
            &format!("{reply}/a.json"),
            json!({"name":"a","type":"STATIC_LIBRARY","artifacts":[{"path":"liba.a"}],"sources":[{"path":"a.c"}],"dependencies":dependencies}),
        );
    }

    #[test]
    fn review_cmake_ignores_self_dependency() {
        let dir = tempfile::tempdir().unwrap();
        review_cmake_reply(dir.path(), "build/.cmake/api/v1/reply", json!([{"id":"a"}]));
        let db = load(dir.path(), None, None).unwrap();
        assert_eq!(db.targets.len(), 1);
        assert!(db.targets[0].dependencies.is_empty());
    }

    #[test]
    fn review_cmake_multiple_roots_do_not_duplicate_targets() {
        let dir = tempfile::tempdir().unwrap();
        for reply in [".cmake/api/v1/reply", "build/.cmake/api/v1/reply"] {
            review_cmake_reply(dir.path(), reply, json!([]));
        }
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets.len(), 1);
    }

    #[test]
    fn review_linker_option_operands_are_not_inputs() {
        for option in ["--rpath", "--soname", "-R"] {
            let args = ["cc", "main.c", option, "unrelated.so", "-o", "app"].map(str::to_owned);
            let link = parse_link(
                Path::new("/build"),
                &args,
                Some(PathBuf::from("/build/app")),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                link.inputs,
                vec![PathBuf::from("/build/main.c")],
                "{option}"
            );
        }
    }

    #[test]
    fn missing_explicit_metadata_is_an_error_but_malformed_auto_metadata_warns() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), None, Some(&dir.path().join("absent.json"))).is_err());
        std::fs::write(dir.path().join("link_commands.json"), "{").unwrap();
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.targets.is_empty());
        assert_eq!(db.warnings.len(), 1);
    }

    #[test]
    fn build_database_normalizes_unbuilt_artifacts_and_separate_link_commands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.cpp"), "").unwrap();
        write(
            dir.path(),
            "build/compile_commands.json",
            json!([
                {"directory":".","file":"../source.cpp","arguments":["c++","-c","../source.cpp","-o","nested/../source.o"]}
            ]),
        );
        write(
            dir.path(),
            "build/link_commands.json",
            json!([
                {"directory":".","arguments":["c++","./source.o","-o","app"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets[0].name, "app");
        assert_eq!(
            db.targets[0].sources,
            vec![trace_ir::canonicalize(&dir.path().join("source.cpp"))]
        );
    }

    #[test]
    fn mixed_link_records_do_not_warn_in_preprocessing_reader() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([
                {"directory":".","arguments":["cc","main.o","-o","app"]}
            ]),
        );
        let db = crate::compile_commands::CompilationDatabase::load(
            dir.path(),
            &trace_preproc::PreprocessOptions::new(),
        )
        .unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert!(db.commands.is_empty());
    }

    #[test]
    fn recursive_response_file_and_compound_shell_commands_warn_without_targets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("recursive.rsp"), "@recursive.rsp").unwrap();
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory":".","command":"cc @recursive.rsp"},
                {"directory":".","command":"cc main.c -o app && touch marker"}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.targets.is_empty());
        assert_eq!(db.warnings.len(), 2);
        assert!(!dir.path().join("marker").exists());
    }

    #[test]
    fn linker_forwarded_libraries_resolve_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory":".","command":"cc helper.c -shared -o libhelper.so"},
                {"directory":".","command":"cc main.c -Wl,-L,.,-lhelper -o app"}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets[1].dependencies, vec![0]);
    }

    #[test]
    fn incomplete_cmake_reply_does_not_publish_partial_target_graph() {
        let dir = tempfile::tempdir().unwrap();
        let reply = "build/.cmake/api/v1/reply";
        write(
            dir.path(),
            &format!("{reply}/index-001.json"),
            json!({"objects":[{"kind":"codemodel","version":{"major":2},"jsonFile":"model.json"}]}),
        );
        write(
            dir.path(),
            &format!("{reply}/model.json"),
            json!({"paths":{"source":dir.path(),"build":dir.path().join("build")},"configurations":[{"targets":[{"id":"a","jsonFile":"a.json"},{"id":"b","jsonFile":"missing.json"}]}]}),
        );
        write(
            dir.path(),
            &format!("{reply}/a.json"),
            json!({"name":"a","type":"STATIC_LIBRARY","artifacts":[{"path":"liba.a"}],"sources":[{"path":"a.c"}]}),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert_eq!(db.warnings.len(), 1);
        assert!(db.targets.is_empty());
    }

    #[test]
    fn same_source_objects_keep_their_successful_compilation_ordinals() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("hook.c");
        std::fs::write(&source, "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([
                {"directory":".","file":"hook.c","arguments":["cc","-c","hook.c","-imacrosmissing.h","-obad.o"]},
                {"directory":".","file":"hook.c","arguments":["cc","-c","hook.c","-DFULL=1","-ofull/hook.o"]},
                {"directory":".","file":"hook.c","arguments":["cc","-c","hook.c","-ofallback/hook.o"]}
            ]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory":".","command":"cc full/hook.o -o full-app"},
                {"directory":".","command":"cc fallback/hook.o -o fallback-app"}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        let source = trace_ir::canonicalize(&source);
        assert_eq!(
            db.targets[0].configurations.get(&source),
            Some(&BTreeSet::from([0]))
        );
        assert_eq!(
            db.targets[1].configurations.get(&source),
            Some(&BTreeSet::from([1]))
        );
    }
    #[test]
    fn mixed_commands_map_objects_and_library_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.c"), "").unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([
                {"directory":".", "file":"lib.c", "output":"obj/lib.o", "arguments":["cc","-c","lib.c"]},
                {"directory":".", "file":"main.c", "arguments":["cc","-c","main.c","-oobj/main.o"]},
                {"directory":".", "arguments":["ar","rcs","liba.a","obj/lib.o"]},
                {"directory":".", "arguments":["cc","obj/main.o","-L.","-la","-o","app"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets.len(), 2);
        assert_eq!(
            db.targets[0].sources,
            vec![trace_ir::canonicalize(&dir.path().join("lib.c"))]
        );
        assert_eq!(db.targets[1].dependencies, vec![0]);
        assert_eq!(
            db.targets[1].sources,
            vec![trace_ir::canonicalize(&dir.path().join("main.c"))]
        );
    }
    #[test]
    fn explicit_links_expand_response_files_and_diagnose_missing_objects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("args.rsp"), "main.c missing.o -o app").unwrap();
        write(
            dir.path(),
            "custom.json",
            json!([{ "directory":".", "command":"cc @args.rsp" }]),
        );
        let db = load(dir.path(), None, Some(&dir.path().join("custom.json"))).unwrap();
        assert_eq!(db.targets.len(), 1);
        assert_eq!(db.targets[0].sources.len(), 1);
        assert!(db.warnings.iter().any(|s| s.contains("missing.o")));
    }
    #[test]
    fn reads_only_cmake_index_references() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "").unwrap();
        let reply = "build/.cmake/api/v1/reply";
        write(
            dir.path(),
            &format!("{reply}/index-001.json"),
            json!({"objects":[{"kind":"codemodel","version":{"major":2},"jsonFile":"model.json"}]}),
        );
        write(
            dir.path(),
            &format!("{reply}/model.json"),
            json!({"paths":{"source":dir.path(),"build":dir.path().join("build")},"configurations":[{"targets":[{"id":"a","jsonFile":"a.json"},{"id":"b","jsonFile":"b.json"}]}]}),
        );
        write(
            dir.path(),
            &format!("{reply}/a.json"),
            json!({"name":"a","type":"STATIC_LIBRARY","artifacts":[{"path":"liba.a"}],"sources":[{"path":"a.c","compileGroupIndex":0}]}),
        );
        write(
            dir.path(),
            &format!("{reply}/b.json"),
            json!({"name":"b","type":"EXECUTABLE","artifacts":[{"path":"b"}],"sources":[{"path":"b.c","compileGroupIndex":0}],"dependencies":[{"id":"a"}]}),
        );
        write(
            dir.path(),
            &format!("{reply}/target-stale.json"),
            json!({"name":"stale"}),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets.len(), 2);
        assert_eq!(db.targets[1].dependencies, vec![0]);
        assert_eq!(
            db.targets[0].sources,
            vec![trace_ir::canonicalize(&dir.path().join("a.c"))]
        );
    }
    #[test]
    fn multi_config_reply_reads_one_configuration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "").unwrap();
        let reply = "build/.cmake/api/v1/reply";
        write(
            dir.path(),
            &format!("{reply}/index-001.json"),
            json!({"objects":[{"kind":"codemodel","version":{"major":2},"jsonFile":"model.json"}]}),
        );
        write(
            dir.path(),
            &format!("{reply}/model.json"),
            json!({"paths":{"source":dir.path(),"build":dir.path().join("build")},
                   "configurations":[
                     {"name":"Debug","targets":[{"id":"a","jsonFile":"debug.json"}]},
                     {"name":"Release","targets":[{"id":"a","jsonFile":"release.json"}]}]}),
        );
        for (config, file) in [("Debug", "debug.json"), ("Release", "release.json")] {
            write(
                dir.path(),
                &format!("{reply}/{file}"),
                json!({"name":"a","type":"STATIC_LIBRARY",
                       "artifacts":[{"path":format!("{config}/liba.a")}],
                       "sources":[{"path":"a.c"}]}),
            );
        }
        let db = load(dir.path(), None, None).unwrap();
        // One record per target, not one per configuration: the extra copies
        // carry the same sources and would index the corpus twice.
        assert_eq!(db.targets.len(), 1);
        assert_eq!(
            db.targets[0].output,
            resolve_against(&dir.path().join("build"), Path::new("Debug/liba.a"))
        );
        assert_eq!(db.warnings.len(), 1, "{:?}", db.warnings);
        assert!(db.warnings[0].contains("Release"), "{:?}", db.warnings);
    }

    #[test]
    fn long_options_beginning_with_o_are_not_read_as_the_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([{"directory":".","file":"a.c","arguments":["cc","-c","a.c","-oa.o"]}]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([{"directory":".","arguments":[
                "clang","-object_path_lto","lto.o","-order_file","order.txt",
                "-Wl,-objc_abi_version,2","a.o","-o","app"]}]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets.len(), 1);
        assert_eq!(
            db.targets[0].output,
            resolve_against(&trace_ir::canonicalize(dir.path()), Path::new("app"))
        );
        // The joined form still reads a path-shaped operand.
        assert_eq!(
            db.targets[0].sources,
            vec![trace_ir::canonicalize(&dir.path().join("a.c"))]
        );
    }

    #[test]
    fn darwin_loader_paths_msvc_flags_and_soname_are_not_inputs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([{"directory":".","file":"a.c","arguments":["cc","-c","a.c","-o","a.o"]}]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory":".","arguments":[
                    "clang","-dynamiclib","a.o",
                    "-install_name","@rpath/liba.dylib",
                    "-Wl,-rpath,@executable_path/../lib",
                    "-Wl,-soname,liba.so.1",
                    "-o","liba.dylib"]},
                {"directory":".","arguments":[
                    "link.exe","a.o","/implib:b.lib","/NODEFAULTLIB:libcmt.lib",
                    "/out:b.exe"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let source = trace_ir::canonicalize(&dir.path().join("a.c"));
        let named = |name: &str| {
            db.targets
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("missing {name}: {:?}", db.targets))
        };
        // Loader paths, an soname and MSVC switches are options, not inputs,
        // so neither target gains a phantom dependency.
        for name in ["liba.dylib", "b.exe"] {
            let target = named(name);
            assert_eq!(target.sources, vec![source.clone()], "{name}");
            assert!(target.dependencies.is_empty(), "{name}: {target:?}");
        }
    }

    #[test]
    fn absolute_unix_paths_are_inputs_not_msvc_switches() {
        let dir = tempfile::tempdir().unwrap();
        let root = trace_ir::canonicalize(dir.path());
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([{"directory": root, "file": "main.c",
                    "arguments": ["cc", "-c", "main.c", "-o", root.join("main.o")]}]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory": root, "arguments":
                    ["ar", "rcs", root.join("liba.a"), root.join("main.o")]},
                {"directory": root, "arguments":
                    ["cc", root.join("main.o"), root.join("liba.a"), "-o", root.join("app")]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let app = db
            .targets
            .iter()
            .find(|t| t.name == "app")
            .unwrap_or_else(|| panic!("no app target: {:?}", db.targets));
        // A leading `/` is an absolute path under a non-MSVC driver, so the
        // object maps back to its source and the archive is a dependency.
        assert_eq!(app.sources, vec![root.join("main.c")]);
        assert_eq!(app.dependencies.len(), 1, "{app:?}");
        let archive = &db.targets[app.dependencies[0]];
        assert_eq!(archive.output, root.join("liba.a"));
    }

    #[test]
    fn a_valueless_o_flag_does_not_shadow_the_real_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([{"directory":".","file":"a.c","arguments":["cc","-c","a.c","-o","a.o"]}]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([
                // No `output` key, and a valueless flag beginning with `-o`
                // ahead of the real one.
                {"directory":".","arguments":
                    ["ld","-onelevel_namespace","a.o","-o","libfoo.dylib"]},
                // Windows spells the librarian in upper case.
                {"directory":".","arguments":["LIB.EXE","/OUT:bar.lib","a.o"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let mut names: Vec<&str> = db.targets.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["bar.lib", "libfoo.dylib"], "{:?}", db.targets);
    }

    #[test]
    fn a_forwarded_output_operand_names_the_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hook.c"), "").unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([
                {"directory":".","file":"hook.c","arguments":["cc","-c","hook.c","-o","hook.o"]},
                {"directory":".","file":"main.c","arguments":["cc","-c","main.c","-o","main.o"]}
            ]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([
                // The output is spelled through the forwarding form only.
                {"directory":".","arguments":
                    ["cc","-shared","hook.o","-Xlinker","-o","-Xlinker","libhook.dylib"]},
                {"directory":".","arguments":["cc","main.o","libhook.dylib","-o","app"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let mut names: Vec<&str> = db.targets.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["app", "libhook.dylib"], "{:?}", db.targets);
        // Named correctly, the library is matched by the executable that links it.
        let app = db.targets.iter().find(|t| t.name == "app").unwrap();
        assert_eq!(app.dependencies.len(), 1, "{app:?}");
        assert_eq!(
            db.targets[app.dependencies[0]].sources,
            vec![trace_ir::canonicalize(&dir.path().join("hook.c"))]
        );
    }

    #[test]
    fn xlinker_forwards_its_operand_instead_of_swallowing_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        std::fs::write(dir.path().join("hook.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([
                {"directory":".","file":"main.c","arguments":["cc","-c","main.c","-o","main.o"]},
                {"directory":".","file":"hook.c","arguments":["cc","-c","hook.c","-o","hook.o"]}
            ]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory":".","arguments":["ar","rcs","libhook.a","hook.o"]},
                {"directory":".","arguments":["cc","main.o",
                    "-Xlinker","-force_load","-Xlinker","libhook.a","-o","app"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let app = db
            .targets
            .iter()
            .find(|t| t.name == "app")
            .unwrap_or_else(|| panic!("no app target: {:?}", db.targets));
        // The archive reaches the parser as a plain operand, so it is a
        // dependency rather than a discarded `-Xlinker` payload.
        assert_eq!(app.dependencies.len(), 1, "{app:?}");
        assert_eq!(
            db.targets[app.dependencies[0]].output,
            resolve_against(&trace_ir::canonicalize(dir.path()), Path::new("libhook.a"))
        );
    }

    #[test]
    fn a_short_joined_output_and_an_unsupported_option_keep_the_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([{"directory":".","file":"main.c","arguments":["cc","-c","main.c","-omain.o"]}]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([{"directory":".","arguments":
                ["cc","main.o","-Xlinker","-znow","-T","link.ld","-oapp"]}]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets.len(), 1, "{:?}", db.targets);
        assert_eq!(db.targets[0].name, "app");
        assert_eq!(
            db.targets[0].sources,
            vec![trace_ir::canonicalize(&dir.path().join("main.c"))]
        );
    }

    #[test]
    fn msvc_output_spellings_need_an_msvc_driver() {
        // `/Fo` matches any `/fo…`; under a GNU driver those are paths.
        let gnu: Vec<String> = ["cc", "/foo/bar/main.o", "-o", "app"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(command_output(&gnu), Some("app"));
        let isystem: Vec<String> = [
            "cc",
            "-isystem",
            "/foundation/include",
            "-c",
            "w.c",
            "-o",
            "w.o",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(command_output(&isystem), Some("w.o"));
        // Under an MSVC driver they are options again.
        let msvc: Vec<String> = ["cl.exe", "/Fofoo.obj", "/c", "w.c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(command_output(&msvc), Some("foo.obj"));
        let linker: Vec<String> = ["link.exe", "/out:app.exe", "a.obj"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(command_output(&linker), Some("app.exe"));
    }

    #[test]
    fn a_versioned_soname_resolves_from_its_l_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.c"), "").unwrap();
        std::fs::write(dir.path().join("main.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([
                {"directory":".","file":"lib.c","arguments":["cc","-c","lib.c","-o","lib.o"]},
                {"directory":".","file":"main.c","arguments":["cc","-c","main.c","-o","main.o"]}
            ]),
        );
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory":".","arguments":["cc","-shared","lib.o","-o","libfoo.so.1.2.3"]},
                {"directory":".","arguments":["cc","main.o","-L.","-lfoo","-o","app"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let app = db.targets.iter().find(|t| t.name == "app").unwrap();
        assert_eq!(app.dependencies.len(), 1, "{:?}", db.targets);
        assert_eq!(
            db.targets[app.dependencies[0]].sources,
            vec![trace_ir::canonicalize(&dir.path().join("lib.c"))]
        );
    }

    #[test]
    fn an_object_output_survives_a_space_a_colon_or_a_dash() {
        // `cl` spells the object file `/Fo`, and generators emit every one of
        // these. A bare `/Fo` used to yield an empty output, which is not a
        // path at all, and `/Fo:out.obj` kept the option's colon.
        let cases: [(&[&str], Option<&str>); 6] = [
            (&["cl", "/c", "/Fo", "out.obj", "foo.c"], Some("out.obj")),
            (&["cl", "/c", "/Fo:out.obj", "foo.c"], Some("out.obj")),
            (&["cl", "/c", "/Foout.obj", "foo.c"], Some("out.obj")),
            (&["cl", "/c", "-Fo", "out.obj", "foo.c"], Some("out.obj")),
            (&["cl", "/c", "-Foout.obj", "foo.c"], Some("out.obj")),
            // The dash spelling is matched case-sensitively: folded, its three
            // bytes are the head of every `-f…` option, and `-fopenmp` would
            // name the object `penmp`.
            (
                &["clang-cl", "/c", "-fopenmp", "foo.c", "-o", "out.obj"],
                Some("out.obj"),
            ),
        ];
        for (args, expected) in cases {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            assert_eq!(command_output(&args), expected, "{args:?}");
        }
    }

    #[test]
    fn a_bare_at_sign_and_rpath_link_operands_keep_the_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.c"), "").unwrap();
        write(
            dir.path(),
            "compile_commands.json",
            json!([{"directory":".","file":"a.c","arguments":["cc","-c","a.c","-o","a.o"]}]),
        );
        // `@` alone names no response file; reading it opened the command's
        // own directory and dropped the whole target. `-rpath-link` takes a
        // directory, which left unconsumed is read as an input path.
        write(
            dir.path(),
            "link_commands.json",
            json!([
                {"directory":".","arguments":[
                    "cc","a.o","@","-Wl,-rpath-link,/usr/lib",
                    "--rpath-link","/opt/lib","-o","app"]}
            ]),
        );
        let db = load(dir.path(), None, None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        let app = db
            .targets
            .iter()
            .find(|t| t.name == "app")
            .unwrap_or_else(|| panic!("missing app: {:?}", db.targets));
        assert_eq!(
            app.sources,
            vec![trace_ir::canonicalize(&dir.path().join("a.c"))]
        );
        assert!(app.dependencies.is_empty(), "{app:?}");
    }

    #[test]
    fn link_metadata_is_read_beside_an_out_of_source_compilation_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = trace_ir::canonicalize(dir.path());
        std::fs::write(root.join("main.c"), "").unwrap();
        // An out-of-source build keeps both databases in its own directory,
        // which is neither the analysis root nor `root/build`.
        write(
            &root,
            "out/compile_commands.json",
            json!([{"directory":root,"file":"main.c",
                    "arguments":["cc","-c","main.c","-o","out/main.o"]}]),
        );
        write(
            &root,
            "out/link_commands.json",
            json!([{"directory":root,"arguments":["cc","out/main.o","-o","out/app"]}]),
        );
        let db = load(&root, Some(&root.join("out/compile_commands.json")), None).unwrap();
        assert!(db.warnings.is_empty(), "{:?}", db.warnings);
        assert_eq!(db.targets.len(), 1, "{:?}", db.targets);
        // The object resolves back to its source only when the compilation
        // reader knew link metadata was reachable and recorded object outputs.
        assert_eq!(db.targets[0].sources, vec![root.join("main.c")]);
    }
}
