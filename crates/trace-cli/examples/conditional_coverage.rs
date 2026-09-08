//! Measure what the current configuration excludes (#57): every
//! conditional-compilation chain in a tree, which arm each preprocess run
//! took, how many source lines the arms not taken hold, and what is known
//! about the names the conditions depend on.
//!
//! Usage:
//!   cargo run -p trace-cli --release --example conditional_coverage -- \
//!     /path/to/project [--include DIR]... [-D NAME[=VALUE]]... > coverage.tsv
//!
//! Every translation unit is preprocessed from the command-line defines
//! alone, its includes expanded inline, which is the environment `trace
//! analyze` gives each unit; headers no unit reaches are preprocessed
//! standalone, as the indexer does with orphans. No expansion cache is used:
//! a cache hit replays a header's text without re-evaluating its
//! conditionals, and this tool exists to see those evaluations. A header
//! reached from several units is therefore evaluated once per unit, and
//! the record says how often each arm was taken across those runs.
//!
//! Output is TSV, one record per line, read by
//! `scripts/gen_conditional_coverage_report.py`:
//!
//!   META   key  value
//!   FILE   path  kind(tu|header)  lines  runs
//!   CHAIN  path  line  depth  guard  terminated  runs
//!   ARM    path  chain-line  index  directive  line  end-line  taken  skipped  unevaluated  evaluated  expression
//!   READ   path  chain-line  index  name  bound  unbound  unknown
//!   NAME   name  class  cli  defines  define-site  build-file
//!   END    number-of-preceding-records
//!
//! `path` is relative to the root. Counts on ARM / READ rows are over the
//! runs that met the chain: `taken` / `skipped` / `unevaluated` per arm, and
//! per name how often the condition found it bound, unbound, or was not
//! evaluated at all. `class` is one of `include-guard`, `toolchain`,
//! `configuration`, `unknown` — see `classify`.

use indexmap::IndexMap;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use trace_parse::{discover_source_files, is_cpp_header_path, is_cpp_path, IncludeGraph};
use trace_preproc::{
    preprocess_file, ArmOutcome, ConditionalArm, ConditionalChain, Language, PreprocessOptions,
};
use walkdir::WalkDir;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .ok_or("usage: conditional_coverage ROOT [--include DIR]... [-D NAME[=VALUE]]...")?,
    );
    let mut opts = PreprocessOptions::default().with_record_conditionals(true);
    let mut cli_defines: Vec<String> = Vec::new();
    let mut cli_definitions: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--include" => {
                let dir = args.next().ok_or("--include requires DIR")?;
                opts = opts.with_include(PathBuf::from(dir));
            }
            "-D" => {
                let def = args.next().ok_or("-D requires NAME[=VALUE]")?;
                let (name, value) = def.split_once('=').unwrap_or((def.as_str(), "1"));
                cli_defines.push(name.to_string());
                cli_definitions.push(format!("{name}={value}"));
                opts = opts.with_define(name, value);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if !root.is_dir() {
        return Err(format!(
            "corpus root is not a directory: {}",
            root.display()
        ));
    }
    let (tus, headers) = discover_source_files(&root);
    if tus.is_empty() && headers.is_empty() {
        return Err(format!("no source files found in {}", root.display()));
    }
    let include_graph = IncludeGraph::build(&root, &tus, &headers);
    for dir in &include_graph.include_dirs {
        if !opts.include_paths.iter().any(|p| p == dir) {
            opts.include_paths.push(dir.clone());
        }
    }
    if !include_graph.source_cache.is_empty() {
        opts.source_cache = Some(std::sync::Arc::new(include_graph.source_cache.clone()));
    }
    let root = include_graph.root.clone();

    let mut records = Records::default();
    let tu_paths: Vec<PathBuf> = tus.iter().map(|p| include_graph.intern_path(p)).collect();
    let header_paths: Vec<PathBuf> = headers
        .iter()
        .map(|p| include_graph.intern_path(p))
        .collect();
    let cpp_tus: HashSet<PathBuf> = tu_paths
        .iter()
        .filter(|p| is_cpp_path(p))
        .cloned()
        .collect();
    let no_c_units = cpp_tus.len() == tu_paths.len();
    let cpp_reach = include_graph.reachable_from(&cpp_tus);

    for tu in &tu_paths {
        let run_opts = opts.clone().with_language(Language::from_path(tu));
        records.run(tu, &run_opts)?;
    }
    // Orphans: headers no unit reached, in a stable order.
    let mut orphans: Vec<&PathBuf> = header_paths
        .iter()
        .filter(|h| !records.runs.contains_key(*h))
        .collect();
    orphans.sort();
    for header in orphans {
        let language = if is_cpp_header_path(header) || cpp_reach.contains(header) || no_c_units {
            Language::Cpp
        } else {
            Language::C
        };
        records.run(header, &opts.clone().with_language(language))?;
    }

    let evidence = Evidence::gather(&root, &include_graph, &cli_defines, &records);
    let tu_set: HashSet<&PathBuf> = tu_paths.iter().collect();
    // Fields are tab-separated, records newline-separated; a path or an
    // expression spelling either would split its row.
    let field = |s: &str| s.replace(['\t', '\n'], " ");
    let rel = |p: &Path| -> String { field(&p.strip_prefix(&root).unwrap_or(p).to_string_lossy()) };

    let mut out = String::new();
    out.push_str(&format!(
        "META\tdefines\t{}\n",
        field(&cli_definitions.join(" "))
    ));
    let mut files: Vec<(&PathBuf, &usize)> = records.runs.iter().collect();
    files.sort();
    for (path, runs) in files {
        let lines = source_line_count(path, &include_graph)?;
        let kind = if tu_set.contains(path) {
            "tu"
        } else {
            "header"
        };
        out.push_str(&format!("FILE\t{}\t{kind}\t{}\t{runs}\n", rel(path), lines));
    }
    for ((path, line), chain) in &records.chains {
        let path = rel(path);
        out.push_str(&format!(
            "CHAIN\t{path}\t{line}\t{}\t{}\t{}\t{}\n",
            chain.depth,
            u8::from(chain.include_guard),
            u8::from(chain.terminated),
            chain.runs
        ));
        for (idx, arm) in chain.arms.iter().enumerate() {
            out.push_str(&format!(
                "ARM\t{path}\t{line}\t{idx}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                arm.directive,
                arm.line,
                arm.end_line,
                arm.taken,
                arm.skipped,
                arm.unevaluated,
                arm.evaluated,
                field(&arm.expression)
            ));
            for (name, tally) in &arm.reads {
                out.push_str(&format!(
                    "READ\t{path}\t{line}\t{idx}\t{name}\t{}\t{}\t{}\n",
                    tally.bound, tally.unbound, tally.unknown
                ));
            }
        }
    }
    let mut names: Vec<&String> = evidence.names.iter().collect();
    names.sort();
    for name in names {
        let class = evidence.classify(name);
        let (defines, site) = evidence
            .defines
            .get(name)
            .map(|(count, site)| (*count, rel(&site.0) + ":" + &site.1.to_string()))
            .unwrap_or((0, "-".to_string()));
        let build = evidence
            .build_files
            .get(name)
            .map(|p| rel(p))
            .unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "NAME\t{name}\t{}\t{}\t{defines}\t{site}\t{build}\n",
            class.as_str(),
            u8::from(evidence.cli.contains(name))
        ));
    }
    // A newline alone cannot distinguish a complete capture from one cut
    // short between records. Count only LF, since CR can occur in fields.
    let rows = out.bytes().filter(|&b| b == b'\n').count();
    out.push_str(&format!("END\t{rows}\n"));
    print!("{out}");
    Ok(())
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

/// Includes outside the discovered tree (or with a non-source extension)
/// are read by the preprocessor but are absent from the graph's cache.
fn source_line_count(path: &Path, graph: &IncludeGraph) -> Result<usize, String> {
    if let Some(text) = graph.source_cache.get(path) {
        return Ok(line_count(text));
    }
    std::fs::read_to_string(path)
        .map(|text| line_count(&text))
        .map_err(|e| format!("cannot count source lines in {}: {e}", path.display()))
}

/// How often a condition found a name bound / unbound, or was not evaluated.
#[derive(Debug, Default, Clone, Copy)]
struct ReadTally {
    bound: u32,
    unbound: u32,
    unknown: u32,
}

#[derive(Debug)]
struct ArmStats {
    directive: &'static str,
    expression: String,
    line: u32,
    end_line: u32,
    taken: u32,
    skipped: u32,
    unevaluated: u32,
    /// Runs in which the condition was evaluated.
    evaluated: u32,
    reads: IndexMap<String, ReadTally>,
}

impl ArmStats {
    fn new(arm: &ConditionalArm) -> Self {
        Self {
            directive: arm.directive.as_str(),
            expression: arm.expression.clone(),
            line: arm.line,
            end_line: arm.end_line,
            taken: 0,
            skipped: 0,
            unevaluated: 0,
            evaluated: 0,
            reads: IndexMap::new(),
        }
    }

    fn add(&mut self, arm: &ConditionalArm) {
        match arm.outcome {
            ArmOutcome::Taken => self.taken += 1,
            ArmOutcome::Skipped => self.skipped += 1,
            ArmOutcome::Unevaluated => self.unevaluated += 1,
        }
        self.evaluated += u32::from(arm.evaluated);
        for read in &arm.reads {
            let tally = self.reads.entry(read.name.clone()).or_default();
            match read.bound {
                Some(true) => tally.bound += 1,
                Some(false) => tally.unbound += 1,
                None => tally.unknown += 1,
            }
        }
    }
}

#[derive(Debug)]
struct ChainStats {
    depth: usize,
    include_guard: bool,
    terminated: bool,
    runs: u32,
    arms: Vec<ArmStats>,
}

/// Every chain met by any run, merged on `(file, opening line)`.
#[derive(Debug, Default)]
struct Records {
    /// How many runs processed each file.
    runs: HashMap<PathBuf, usize>,
    chains: BTreeMap<(PathBuf, u32), ChainStats>,
}

impl Records {
    fn run(&mut self, path: &Path, opts: &PreprocessOptions) -> Result<(), String> {
        let result = preprocess_file(path, opts)
            .map_err(|e| format!("preprocess failed for {}: {e}", path.display()))?;
        for file in &result.included_headers {
            *self.runs.entry(file.clone()).or_default() += 1;
        }
        for chain in &result.conditionals {
            self.add(chain);
        }
        Ok(())
    }

    fn add(&mut self, chain: &ConditionalChain) {
        let key = (chain.file.clone(), chain.line());
        let stats = self.chains.entry(key).or_insert_with(|| ChainStats {
            depth: chain.depth,
            include_guard: false,
            terminated: chain.terminated,
            runs: 0,
            arms: chain.arms.iter().map(ArmStats::new).collect(),
        });
        stats.runs += 1;
        stats.include_guard |= chain.include_guard;
        stats.terminated &= chain.terminated;
        // Same file text, same structure — except that a run cut short by
        // an error records fewer arms and ends the last one it saw at the
        // end of the file. Whichever run met the chain first must not fix
        // that shape for the rest: arms a later run adds are appended, and
        // an arm ends at the earliest end any run recorded, which is the
        // real one (a cut-short run can only overshoot).
        for (idx, arm) in chain.arms.iter().enumerate() {
            if idx == stats.arms.len() {
                stats.arms.push(ArmStats::new(arm));
            }
            let arm_stats = &mut stats.arms[idx];
            arm_stats.end_line = arm_stats.end_line.min(arm.end_line);
            arm_stats.add(arm);
        }
    }
}

/// What is known about a name a condition depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameClass {
    /// Tested by a chain that is a file's include guard.
    IncludeGuard,
    /// A macro the compiler predefines for the language, compiler, target OS,
    /// architecture or type sizes.
    Toolchain,
    /// Something in the checkout provides or names it: a `-D`, an in-tree
    /// `#define`, or an in-tree build file.
    Configuration,
    /// Nothing in the checkout accounts for it. Kept as a real category: the
    /// names the analysis cannot reason about are what #59 needs to know.
    Unknown,
}

impl NameClass {
    fn as_str(self) -> &'static str {
        match self {
            NameClass::IncludeGuard => "include-guard",
            NameClass::Toolchain => "toolchain",
            NameClass::Configuration => "configuration",
            NameClass::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Default)]
struct Evidence {
    /// Every name some condition read.
    names: HashSet<String>,
    /// Names read only by include-guard chains (see `guard_names`).
    guards: HashSet<String>,
    cli: HashSet<String>,
    /// Name → (number of in-tree `#define`s, first site).
    defines: HashMap<String, (u32, (PathBuf, u32))>,
    /// Name → first in-tree build file that spells it.
    build_files: HashMap<String, PathBuf>,
}

impl Evidence {
    fn gather(
        root: &Path,
        graph: &IncludeGraph,
        cli_defines: &[String],
        records: &Records,
    ) -> Self {
        let mut ev = Evidence {
            cli: cli_defines.iter().cloned().collect(),
            ..Default::default()
        };
        for chain in records.chains.values() {
            for arm in &chain.arms {
                ev.names.extend(arm.reads.keys().cloned());
            }
        }
        ev.guards = guard_names(records);
        let mut sources: Vec<(&PathBuf, &std::sync::Arc<str>)> =
            graph.source_cache.iter().collect();
        sources.sort_by(|a, b| a.0.cmp(b.0));
        for (path, text) in sources {
            for (name, line) in scan_defines(text) {
                if !ev.names.contains(&name) {
                    continue;
                }
                let entry = ev
                    .defines
                    .entry(name.to_string())
                    .or_insert_with(|| (0, (path.clone(), line)));
                entry.0 += 1;
            }
        }
        let mut build_files: Vec<PathBuf> = WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file() && is_build_file(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect();
        build_files.sort();
        for path in build_files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for ident in identifiers(&text) {
                if ev.names.contains(ident) && !ev.build_files.contains_key(ident) {
                    ev.build_files.insert(ident.to_string(), path.clone());
                }
            }
        }
        ev
    }

    fn classify(&self, name: &str) -> NameClass {
        if self.guards.contains(name) {
            NameClass::IncludeGuard
        } else if is_toolchain_fact(name) {
            NameClass::Toolchain
        } else if self.cli.contains(name)
            || self.defines.contains_key(name)
            || self.build_files.contains_key(name)
        {
            NameClass::Configuration
        } else {
            NameClass::Unknown
        }
    }
}

/// The names that are include guards and nothing else: tested by some
/// guard chain and read by no other chain. A guard-shaped file that is in
/// fact a default-value idiom (`#ifndef LOG_LEVEL / #define LOG_LEVEL 2 /
/// #endif` on its own) tests a name that `#if LOG_LEVEL > 1` elsewhere
/// depends on; that name is configuration, and classifying it as a guard
/// would hide the lines it excludes.
fn guard_names(records: &Records) -> HashSet<String> {
    let mut guards: HashSet<String> = HashSet::new();
    let mut elsewhere: HashSet<&String> = HashSet::new();
    for chain in records.chains.values() {
        if chain.include_guard {
            if let Some(name) = chain.arms.first().and_then(|a| a.reads.keys().next()) {
                guards.insert(name.clone());
            }
        } else {
            elsewhere.extend(chain.arms.iter().flat_map(|a| a.reads.keys()));
        }
    }
    guards.retain(|name| !elsewhere.contains(name));
    guards
}

/// `(name, line)` of every `#define` in `text`, whether or not the
/// preprocessor would reach it: a definition in an excluded region or an
/// unincluded config header still says the tree knows the name.
fn scan_defines(text: &str) -> Vec<(String, u32)> {
    use trace_preproc::{Lexer, TokenKind};

    // C++ tokenization also keeps raw string contents out of the directive
    // scan in ambiguous .h files. Phase-2 splices and comments follow the
    // same lexer rules as preprocessing.
    let tokens = Lexer::new(text, Language::Cpp).tokenize();
    let mut out = Vec::new();
    for line in tokens.split(|t| matches!(t.kind, TokenKind::Newline)) {
        if let [hash, directive, name, ..] = line {
            if matches!(hash.kind, TokenKind::Hash)
                && matches!(&directive.kind, TokenKind::Identifier(n) if n == "define")
            {
                if let TokenKind::Identifier(name) = &name.kind {
                    out.push((name.clone(), hash.line));
                }
            }
        }
    }
    out
}

/// GN, CMake, Make and Kconfig files: what a checkout ships that names its
/// configuration macros (#58). Whether a name is *spelled* in one is all
/// that is read here; what the entry means is #58's job.
fn is_build_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        name,
        "BUILD.gn" | "CMakeLists.txt" | "Makefile" | "makefile" | "GNUmakefile"
    ) || matches!(ext, "gni" | "gn" | "cmake" | "mk")
        || name.starts_with("Kconfig")
}

/// Identifier-shaped words in `text`.
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty() && !w.starts_with(|c: char| c.is_ascii_digit()))
}

/// Macros gcc and clang predefine: language and compiler identity, target
/// OS, architecture and ABI, type sizes and limits, and the `__has_*`
/// operators. Deliberately a list rather than "anything reserved": a
/// reserved spelling the list does not know (`__KERNEL__`, `__WORDSIZE`)
/// stays `unknown`, because it comes from a build system or a library
/// header this tool cannot see.
fn is_toolchain_fact(name: &str) -> bool {
    const EXACT: &[&str] = &[
        // language
        "__STDC__",
        "__STDC_VERSION__",
        "__STDC_HOSTED__",
        "__STDC_IEC_559__",
        "__STDC_IEC_559_COMPLEX__",
        "__STDC_NO_ATOMICS__",
        "__STDC_NO_THREADS__",
        "__STDC_NO_VLA__",
        "__STDC_UTF_16__",
        "__STDC_UTF_32__",
        "__STDCPP_THREADS__",
        "__cplusplus",
        "__ASSEMBLER__",
        "__OBJC__",
        "__EXCEPTIONS",
        "__GXX_RTTI",
        "__GXX_ABI_VERSION",
        "__GXX_EXPERIMENTAL_CXX0X__",
        "__STRICT_ANSI__",
        // `true` / `false` are literals in a C++ `#if` (and stdbool.h
        // macros in C); the evaluator treats them as such.
        "true",
        "false",
        // compiler
        "__GNUC__",
        "__GNUC_MINOR__",
        "__GNUC_PATCHLEVEL__",
        "__GNUG__",
        "__clang__",
        "__clang_major__",
        "__clang_minor__",
        "__clang_patchlevel__",
        "__clang_version__",
        "__llvm__",
        "_MSC_VER",
        "_MSC_FULL_VER",
        "_MSVC_LANG",
        "__INTEL_COMPILER",
        "__ICC",
        "__TINYC__",
        "__ARMCC_VERSION",
        "__CC_ARM",
        "__ICCARM__",
        "__IAR_SYSTEMS_ICC__",
        "__TI_COMPILER_VERSION__",
        "__VERSION__",
        "__OPTIMIZE__",
        "__OPTIMIZE_SIZE__",
        "__NO_INLINE__",
        "__PIC__",
        "__pic__",
        "__PIE__",
        "__pie__",
        "__SANITIZE_ADDRESS__",
        "__SANITIZE_THREAD__",
        "__COUNTER__",
        "__DATE__",
        "__TIME__",
        "__FILE__",
        "__LINE__",
        "__BASE_FILE__",
        "__INCLUDE_LEVEL__",
        // target OS / environment
        "__linux__",
        "__linux",
        "linux",
        "__gnu_linux__",
        "__unix__",
        "__unix",
        "unix",
        "__APPLE__",
        "__MACH__",
        "__FreeBSD__",
        "__NetBSD__",
        "__OpenBSD__",
        "__DragonFly__",
        "__sun",
        "__QNX__",
        "__QNXNTO__",
        "_WIN32",
        "_WIN64",
        "__WIN32__",
        "__MINGW32__",
        "__MINGW64__",
        "__CYGWIN__",
        "__ANDROID__",
        "__ANDROID_API__",
        "__ELF__",
        "__wasm__",
        "__wasm32__",
        "__wasm64__",
        "__EMSCRIPTEN__",
        "__Fuchsia__",
        // The OpenHarmony LLVM toolchain predefines these for its `*-ohos`
        // target triples, the way `__ANDROID__` is predefined for
        // `*-android`.
        "__OHOS__",
        "__OHOS_FAMILY__",
        // architecture / ABI
        "__x86_64__",
        "__x86_64",
        "__amd64__",
        "__amd64",
        "__i386__",
        "__i386",
        "i386",
        "_M_IX86",
        "_M_X64",
        "_M_AMD64",
        "_M_ARM",
        "_M_ARM64",
        "__arm__",
        "__arm",
        "__thumb__",
        "__thumb2__",
        "__ARM_ARCH",
        "__ARMEL__",
        "__ARMEB__",
        "__ARM_EABI__",
        "__ARM_NEON",
        "__ARM_NEON__",
        "__ARM_FP",
        "__ARM_PCS_VFP",
        "__ARM_32BIT_STATE",
        "__ARM_64BIT_STATE",
        "__ARM_BIG_ENDIAN",
        "__VFP_FP__",
        "__SOFTFP__",
        "__aarch64__",
        "__AARCH64EL__",
        "__AARCH64EB__",
        "__arm64__",
        "__arm64",
        "__riscv",
        "__riscv_xlen",
        "__mips__",
        "__mips",
        "__mips64",
        "__MIPSEL__",
        "__MIPSEB__",
        "__powerpc__",
        "__powerpc64__",
        "__PPC__",
        "__PPC64__",
        "__s390x__",
        "__sparc__",
        "__loongarch__",
        "__loongarch64",
        "__xtensa__",
        "__csky__",
        "__sh__",
        "__m68k__",
        "__hexagon__",
        "__ia64__",
        "__alpha__",
        "__hppa__",
        "__AVR__",
        "__MSP430__",
        "__LP64__",
        "_LP64",
        "__ILP32__",
        "_ILP32",
        "__LLP64__",
        "__CHAR_UNSIGNED__",
        "__WCHAR_UNSIGNED__",
        "__BYTE_ORDER__",
        "__FLOAT_WORD_ORDER__",
        "__BIG_ENDIAN__",
        "__LITTLE_ENDIAN__",
        "__BIGGEST_ALIGNMENT__",
        "__CHAR_BIT__",
        "__POINTER_WIDTH__",
        "__MMX__",
        "__SSE__",
        "__SSE2__",
        "__SSE3__",
        "__SSSE3__",
        "__SSE4_1__",
        "__SSE4_2__",
        "__AVX__",
        "__AVX2__",
        "__AVX512F__",
        "__FMA__",
        "__BMI__",
        "__BMI2__",
        "__POPCNT__",
        "__F16C__",
    ];
    const PREFIXES: &[&str] = &[
        "__has_",
        "__SIZEOF_",
        "__ARM_FEATURE_",
        "__ARM_ARCH_",
        "__ATOMIC_",
        "__GCC_",
        "__ORDER_",
        "__INT",
        "__UINT",
        "__SCHAR_",
        "__SHRT_",
        "__LONG_",
        "__LLONG_",
        "__WCHAR_",
        "__WINT_",
        "__SIG_ATOMIC_",
        "__PTRDIFF_",
        "__SIZE_",
        "__CHAR16_",
        "__CHAR32_",
        "__FLT",
        "__DBL_",
        "__LDBL_",
        "__DEC",
    ];
    if EXACT.contains(&name) {
        return true;
    }
    // `__cpp_lib_*` come from library headers (`<version>`), not the
    // compiler; the language feature-test macros do.
    if name.starts_with("__cpp_") {
        return !name.starts_with("__cpp_lib_");
    }
    PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trace_preproc::ArmDirective;

    fn arm(
        directive: ArmDirective,
        line: u32,
        end_line: u32,
        outcome: ArmOutcome,
        reads: &[&str],
    ) -> ConditionalArm {
        ConditionalArm {
            directive,
            expression: reads.join(" "),
            line,
            end_line,
            outcome,
            evaluated: outcome != ArmOutcome::Unevaluated && directive != ArmDirective::Else,
            reads: reads
                .iter()
                .map(|n| trace_preproc::ConditionRead {
                    name: n.to_string(),
                    bound: Some(false),
                })
                .collect(),
        }
    }

    fn chain(
        file: &str,
        include_guard: bool,
        terminated: bool,
        arms: Vec<ConditionalArm>,
    ) -> ConditionalChain {
        ConditionalChain {
            file: PathBuf::from(file),
            arms,
            depth: 0,
            terminated,
            include_guard,
        }
    }

    /// A run cut short by an error records one arm ending at end of file;
    /// the full run that follows must still contribute its second arm and
    /// the real end of the first, whichever order the runs come in.
    #[test]
    fn records_merge_a_cut_short_run_with_a_complete_one() {
        let cut_short = chain(
            "/h.h",
            false,
            false,
            vec![arm(ArmDirective::If, 10, 301, ArmOutcome::Skipped, &["A"])],
        );
        let complete = chain(
            "/h.h",
            false,
            true,
            vec![
                arm(ArmDirective::If, 10, 100, ArmOutcome::Skipped, &["A"]),
                arm(ArmDirective::Else, 100, 200, ArmOutcome::Taken, &[]),
            ],
        );
        for order in [[&cut_short, &complete], [&complete, &cut_short]] {
            let mut records = Records::default();
            for c in order {
                records.add(c);
            }
            let stats = &records.chains[&(PathBuf::from("/h.h"), 10)];
            assert_eq!(stats.runs, 2);
            assert!(!stats.terminated);
            assert_eq!(stats.arms.len(), 2);
            assert_eq!((stats.arms[0].end_line, stats.arms[0].skipped), (100, 2));
            assert_eq!((stats.arms[1].end_line, stats.arms[1].taken), (200, 1));
        }
    }

    /// A name is a guard only if nothing but guard chains read it: the
    /// default-value idiom on its own in a file looks like a guard, and the
    /// `#if LOG_LEVEL > 1` elsewhere says it is configuration.
    #[test]
    fn guard_names_exclude_names_other_chains_depend_on() {
        let mut records = Records::default();
        records.add(&chain(
            "/x.h",
            true,
            true,
            vec![arm(ArmDirective::Ifndef, 1, 5, ArmOutcome::Taken, &["X_H"])],
        ));
        records.add(&chain(
            "/defaults.h",
            true,
            true,
            vec![arm(
                ArmDirective::Ifndef,
                1,
                3,
                ArmOutcome::Taken,
                &["LOG_LEVEL"],
            )],
        ));
        records.add(&chain(
            "/main.c",
            false,
            true,
            vec![arm(
                ArmDirective::If,
                3,
                300,
                ArmOutcome::Skipped,
                &["LOG_LEVEL"],
            )],
        ));
        assert_eq!(guard_names(&records), HashSet::from(["X_H".to_string()]));
    }

    #[test]
    fn scan_defines_finds_names_in_any_region_and_skips_non_defines() {
        let text = "#ifndef X_H\n#define X_H\n  #  define CONFIG_A 1\n#define FN(a) a\n#undef CONFIG_A\n#defined_not\n#define 1bad\nint x;\n";
        let found = scan_defines(text);
        assert_eq!(
            found,
            vec![
                ("X_H".to_string(), 2),
                ("CONFIG_A".to_string(), 3),
                ("FN".to_string(), 4)
            ]
        );
    }

    #[test]
    fn scan_defines_ignores_comments_literals_and_continued_lines() {
        let text = "/*\n#define COMMENT 1\n*/\nconst char *s = R\"(\n#define STRING 1\n)\";\n#define WRAP \\\n#define CONTINUED 1\n#/**/define REAL 1\n#def\\\nine SPLICED 1\n";
        assert_eq!(
            scan_defines(text),
            vec![
                ("WRAP".to_string(), 7),
                ("REAL".to_string(), 9),
                ("SPLICED".to_string(), 10)
            ]
        );
    }

    #[test]
    fn external_headers_have_source_line_counts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let external = dir.path().join("external");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let main = root.join("main.c");
        let header = external.join("cfg.h");
        std::fs::write(&main, "#include \"cfg.h\"\nint main(void) { return 0; }\n").unwrap();
        std::fs::write(
            &header,
            "#if FEATURE\nint a;\nint b;\nint c;\nint d;\n#endif\n",
        )
        .unwrap();
        let (tus, headers) = discover_source_files(&root);
        let graph = IncludeGraph::build(&root, &tus, &headers);
        let mut records = Records::default();
        records
            .run(
                &main,
                &PreprocessOptions::new()
                    .with_include(external)
                    .with_record_conditionals(true),
            )
            .unwrap();
        let header = trace_ir::canonicalize(&header);
        assert!(records.runs.contains_key(&header));
        assert!(!graph.source_cache.contains_key(&header));
        assert_eq!(source_line_count(&header, &graph).unwrap(), 6);
    }

    #[test]
    fn toolchain_facts_are_a_list_not_a_spelling_rule() {
        assert!(is_toolchain_fact("__GNUC__"));
        assert!(is_toolchain_fact("__cplusplus"));
        assert!(is_toolchain_fact("__aarch64__"));
        assert!(is_toolchain_fact("__has_include"));
        assert!(is_toolchain_fact("__SIZEOF_POINTER__"));
        assert!(is_toolchain_fact("__cpp_constexpr"));
        assert!(is_toolchain_fact("true"));
        assert!(!is_toolchain_fact("__cpp_lib_optional"));
        assert!(!is_toolchain_fact("__KERNEL__"));
        assert!(!is_toolchain_fact("__WORDSIZE"));
        assert!(!is_toolchain_fact("CONFIG_FOO"));
        assert!(!is_toolchain_fact("_GNU_SOURCE"));
    }

    #[test]
    fn classification_order_is_guard_toolchain_configuration_unknown() {
        let mut ev = Evidence::default();
        ev.guards.insert("FOO_H".into());
        ev.cli.insert("FROM_CLI".into());
        ev.defines
            .insert("IN_TREE".into(), (1, (PathBuf::from("/x/cfg.h"), 3)));
        ev.build_files
            .insert("FROM_GN".into(), PathBuf::from("/x/BUILD.gn"));
        // A guard name that is also #defined stays a guard.
        ev.defines
            .insert("FOO_H".into(), (1, (PathBuf::from("/x/foo.h"), 2)));
        assert_eq!(ev.classify("FOO_H"), NameClass::IncludeGuard);
        assert_eq!(ev.classify("__linux__"), NameClass::Toolchain);
        assert_eq!(ev.classify("FROM_CLI"), NameClass::Configuration);
        assert_eq!(ev.classify("IN_TREE"), NameClass::Configuration);
        assert_eq!(ev.classify("FROM_GN"), NameClass::Configuration);
        assert_eq!(ev.classify("CONFIG_NOWHERE"), NameClass::Unknown);
        assert_eq!(ev.classify("__KERNEL__"), NameClass::Unknown);
    }

    #[test]
    fn build_files_are_recognized_by_name_or_extension() {
        assert!(is_build_file(Path::new("a/BUILD.gn")));
        assert!(is_build_file(Path::new("a/config.gni")));
        assert!(is_build_file(Path::new("a/CMakeLists.txt")));
        assert!(is_build_file(Path::new("a/toolchain.cmake")));
        assert!(is_build_file(Path::new("a/Makefile")));
        assert!(is_build_file(Path::new("a/rules.mk")));
        assert!(is_build_file(Path::new("a/Kconfig")));
        assert!(is_build_file(Path::new("a/Kconfig.debug")));
        assert!(!is_build_file(Path::new("a/main.c")));
        assert!(!is_build_file(Path::new("a/README.md")));
    }

    #[test]
    fn identifiers_split_on_non_word_characters() {
        let words: Vec<&str> =
            identifiers("defines = [ \"FOO_ENABLE\", \"BAR=1\" ] # 7x").collect();
        assert_eq!(words, vec!["defines", "FOO_ENABLE", "BAR"]);
    }
}
