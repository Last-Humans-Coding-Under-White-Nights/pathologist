# Preprocessor specification

trace implements its own C preprocessor in `trace-preproc`. It runs before tree-sitter parsing so `#include` and `#define` are resolved without invoking gcc/clang.

## API

Compiler attribute normalization removes balanced noise groups only. GNU
`__attribute__`/`__attribute` and MSVC `__declspec` preserve semantic attributes:
`constructor`, `destructor`, `weak`, `visibility`, `alias`, `cleanup`, `noreturn`,
`selectany`, `dllexport`, and `dllimport`, including double-underscore spellings.
Attribute arguments and chained object aliases are expanded before deciding
whether a group is noise. Groups containing directives are processed normally.
The expanded tokens must still form exactly one complete attribute group before
the raw group may be discarded. An argument macro can close the group early and
introduce a declaration, so escaped or unbalanced expansions are retained for
normal processing even when they contain no semantic attribute names.
Retained GNU spellings can still produce tree-sitter diagnostics; preserving
their declarations takes precedence over suppressing those diagnostics.
Malformed groups stop at declaration boundaries instead of consuming later code.
Balancing is local to the attribute group: a surrounding `for` initializer or
C++ lambda argument can legitimately leave an enclosing parenthesis open before
the next semicolon or brace. Elision never consumes that following delimiter;
an unclosed surrounding declaration remains a parser error after normalization.
Newlines and expansion origins remain available to the LineMap.

```rust
pub fn preprocess_file(path: &Path, opts: &PreprocessOptions) -> Result<PreprocessResult>;

pub struct PreprocessResult {
    pub output: String,
    pub line_map: LineMap,
    pub diagnostics: Vec<SharedDiagnostic>,
    pub conditionals: Vec<ConditionalChain>, // only with `record_conditionals`
}
```

CLI equivalents: `--include PATH`, `-D NAME=VALUE`. `PreprocessOptions::with_record_conditionals(true)` turns on the
[conditional-coverage record](#conditional-coverage-record) (off by default; nothing in indexing needs it).

### Compilation database options (#62)

`trace analyze --require-local-inputs` checks the canonical paths observed by
preprocessing and compilation/link metadata readers, including cached, nested
and forced includes, recursive response files and CMake JSON references. It rejects an
analysis that consumed an input outside the target tree after each indexing
pass, before candidate retries, solving or exporting. This is opt-in; the
[PGO workflow](PGO.md) enables it for input verification. The shared `PreprocessOptions::input_boundary` collector also
records macro-only headers that may not contribute exported file entities.

When `compile_commands.json` exists at the analysis root or its `build/` directory
(or is selected with `--compile-commands PATH`), indexing reads each
command's working directory, include directories, macro operations, forced
includes and language settings. Every command for a source is indexed and its
facts are merged; explicit commands are independent of the `--explore` budget.
Files without a usable entry retain inferred configuration. A missing or invalid
database never prevents indexing; invalid databases/entries produce diagnostics.

`-iquote` directories apply only to quoted includes, followed by `-I`, then
explicit `-isystem` directories and the `-idirafter` chain
(`-idirafter`/`-iwithprefix`), preserving order within each search class.
With `--system-includes`, the compiler's search list also supplies `CPATH` and
default system directories in their effective order relative to `-isystem`.
An `-I` entry that duplicates an explicit `-isystem` or `-idirafter` directory,
or a probed system directory, is removed from the command's earlier `-I`
class, as the compiler ignores that duplicate. A CLI
`--include` remains an explicit override; a `CPATH` directory repeated with
`-I` also retains its earlier position. These search rules apply to both JSON
commands and shared flags, including orphan headers. Configured units use
explicit paths without project-wide basename guessing. `-D` and `-U` execute in
command order after language predefines; explicit CLI defines override the
database. `-include` files are processed in order before the TU, in the same
macro environment and with their original locations. Their first search
directory is the command's working directory. `-x` selects both lexer and
parser; `-std` sets the standard version macro and strict/GNU dialect marker.
This does not emulate a compiler's vendor or target-specific builtins, nor
supply missing SDK/generated headers.

`--system-includes` enables compiler include discovery; the library equivalent
is `PreprocessOptions::with_system_includes(true)`. It is off by default.
When enabled, trace asks the compiler for its include search directories with
an empty `-E -v` preprocessing run. With compilation commands,
the command's driver, working directory, environment include paths and
include-affecting target/sysroot options, including `-arch`, select the search;
identical probes are cached across commands. The probe includes explicit
`-isystem` directories so its one ordered result preserves their position
relative to `CPATH`.
Relative executable paths are resolved from the command directory; relative
`PATH` entries are resolved from trace's working directory, and Windows
executable suffixes are honored. Both `-isysroot=/sdk` and `-isysroot/sdk`
forms are forwarded with the command's path context.
With `--system-includes` alone, sources without a usable database entry and
orphan project headers share one fallback compiler. Trace tries `gcc` first,
then `clang` when GCC cannot supply both C and C++ search lists; a usable
single-language list is retained if neither compiler supplies both. C and C++
have separate search lists. A failed probe leaves the existing explicit and
inferred paths in place. Trace continues to preprocess sources itself.
The probe also seeds a small set
of target architecture, data-model, endianness, and hosted-mode macros from the
selected compiler. Database `-D`/`-U` operations and CLI `-D` can override them.
Compiler identity macros require the separate
[compiler-predefines option](#compiler-predefined-macros-230). Unsupported SDK
headers remain outside discovery.
The probe stops when the complete search-list marker appears, even if a launcher
remains alive; a 30-second limit applies only when the marker never appears.

The indexing source cache also shares raw tokens for files outside its preloaded
project sources, including installed system headers. Tokens are keyed by canonical
path, language and exact file contents; macro expansion still runs in each
consumer's environment. Raw-token payload retention is bounded to 32 MiB per
source cache and released with that cache. Files that do not fit are lexed
normally. Original source locations survive both inline expansion and shared
header units. See the [evaluation report](EVAL_REPORT.md#shared-system-header-units--2026-10-16)
for measurements and remaining limits.

### Compiler predefined macros (#230)

`--compiler-predefines` (library: `with_compiler_predefines(true)`) explicitly
imports compiler identity in addition to the existing target/data-model set.
It is off by default and independent of `--system-includes`. Trace still uses
its custom preprocessor; the compiler receives **empty input only**.
Compilation-command drivers, past the existing ccache/sccache/distcc launchers,
take precedence. A launcher's `--` separator is skipped before reading the
driver in compilation commands, link commands and compiler probes. Commands
without a driver (including those starting with an option) warn and use the
inferred or explicit fallback configuration, even when both discovery options
are off. The rejected command's flags, including `-D`/`-U`, are discarded.
`--compiler EXECUTABLE` (`with_compiler`) supplies a fallback
for sources without a usable command and orphan headers; it requires
`--compiler-predefines` or `--system-includes`. CLI compiler paths containing
a directory component resolve against the current working directory; bare
executable names use `PATH`. Library relative compiler paths resolve against
the analysis root. Repeatable `--compiler-arg ARG` (`with_compiler_arg`)
requires `--compiler`. Supported fallback arguments are `-m32`, `-m64`, `-mx32`,
`-nostdinc`, `-nostdinc++`, `-undef`, `-ffreestanding`, `-fhosted`, `-march=`, `-mcpu=`,
`-mabi=`, and the shared probe's target, sysroot, architecture, toolchain,
resource-directory, library, standard and executable-prefix options:
`--target`/`-target`, `--sysroot`/`-isysroot`, `-arch`, `--gcc-toolchain`,
`-resource-dir`, `-stdlib`, `-std`/`--std`, `-B`. Joined forms: `--target=`,
`--sysroot=`, `-isysroot=`, `-isysrootPATH`, `--gcc-toolchain=`,
`-resource-dir=`, `-stdlib=`, `-std=`/`--std=`, `-BPATH`.
Command and fallback probes also preserve Clang's compatibility toggles
`-fms-compatibility`, `-fno-ms-compatibility`, `-fms-extensions`, and
`-fno-ms-extensions`, alongside its identity-version options
`-fms-compatibility-version=`, `-fmsc-version=`, and `-fgnuc-version=`. Their
order participates in probe cache keys so different compatibility modes and
MSVC or GNU versions retain independent macro dumps.
`-undef` is forwarded for command and fallback probes, so compiler identities
and target macros explicitly suppressed by that configuration stay absent.
Ordered command `-D`/`-U` and CLI `-D` still apply after the resulting dump.
Unsupported fallback arguments are configuration errors.
Shared `compile_flags.txt` options reach fallback probes only after validation
for the effective language. Accepted shared options follow `--compiler-arg`
options, so the shared value wins for last-option-wins settings such as target,
sysroot and standard. Rejected shared options and linker-tail arguments
cannot override the explicit fallback configuration. Explicit fallback standard
arguments (`-std=`, `--std=`, or their separate-operand forms) apply only to
their matching language. Probes normalize split `-std VALUE` and `--std VALUE`
arguments to `-std=VALUE` for drivers that require the joined spelling.
The last matching standard wins independently for C
and C++, so mixed trees can supply both `-std=c11` and `-std=c++20` in either
order. The same selection seeds language and strict-mode macros in both
include-only and compiler-predefines modes. Configured
translation units (including headers named by `-x c-header`) determine orphan
header language inference but do not themselves require fallback probes.
For header-only trees, accepted shared `-x` options select the header language
before probe slots are populated; otherwise the normal inferred language applies.

The exact object-like allowlist is:

- Target/hosted: `__STDC_HOSTED__`, `__x86_64__`, `__amd64__`, `__i386__`,
  `__aarch64__`, `__arm__`, `__riscv`, `__LP64__`, `_LP64`, `__ILP32__`,
  `__linux__`, `__unix__`, `__APPLE__`, `__MACH__`, `_WIN32`, `_WIN64`,
  `__BYTE_ORDER__`, `__ORDER_LITTLE_ENDIAN__`, `__ORDER_BIG_ENDIAN__`,
  `__ORDER_PDP_ENDIAN__`, `__SIZEOF_POINTER__`, `__SIZEOF_LONG__`.
- Identity (only with `--compiler-predefines`): `__GNUC__`, `__GNUC_MINOR__`,
  `__GNUC_PATCHLEVEL__`, `__GNUG__`, `__clang__`, `__clang_major__`,
  `__clang_minor__`, `__clang_patchlevel__`, `__clang_version__`,
  `__MINGW32__`, `__MINGW64__`, `_MSC_VER`, `_MSC_FULL_VER`, `_MSC_BUILD`.

Only definitions actually reported by a successful GNU-compatible `-dM` dump
are imported. Native MSVC (`cl`, `clang-cl`/CL driver mode) and IAR probes are
unsupported; use manual `-D`. The IAR name check matches known driver names
without rejecting Intel's `icc`, versioned `icc` or `icpc` drivers; other
drivers are validated by their exit status and dump. No compiler identity is
inferred from a name.
Language/standard predefines remain governed by trace's language configuration.
Compiler definitions precede ordered database `-D`/`-U`; CLI `-D` wins last.
Fallback standard operations also respect CLI definitions during header warming,
so nested cached expansions and translation units select the same standard branches.
System-header spellings `__BYTE_ORDER`, `__LITTLE_ENDIAN`, `__BIG_ENDIAN` are
not synthesized. Build definitions such as `__USER__`, `__LITEOS__`, and
`__HIVIEW_OHOS__` still come from build flags or manual `-D`.

Macro discovery requires successful exit within 30 seconds, including when
include discovery is also enabled. The 250 ms grace period after the complete
search-list marker applies only to include-only probes. Compiler output is
decoded lossily so non-UTF-8 bytes do not discard search paths or valid macros.
Failed or timed-out probes discard macro dumps and retain complete include
lists. A valid dump works
with zero system directories. Include-only discovery retains its early
search-list completion behavior and existing target macros. Both modes share
one probe, cached by executable, directory, language, supported flags, relevant
environment, discovery mode and macro-driver support before parallel indexing.
Unsupported-driver results use the same cache. Missing compiler
context, executables, unsupported drivers and failed dumps emit deduplicated
configuration warnings naming driver, directory/language/flags, reason and
remedy; original source diagnostics remain. Without an explicit fallback,
macro discovery continues with a warning recommending compilation commands,
`--compiler-predefines --compiler ...`, or manual `-D`. It never substitutes
host GCC/Clang for macro discovery. A command whose driver cannot be probed
keeps its command configuration and warns to fix that driver or supply manual
`-D` definitions; `--compiler` does not replace an existing command's driver.
With both discovery flags enabled and no explicit fallback compiler, header
discovery retains the historical GCC/Clang
fallback; its macro dump is discarded so it does not claim a host target or
compiler identity. Unsupported macro drivers still receive the include-only
probe when system headers are requested. Macro-only fallback probes for a
fully configured source tree wait until indexing identifies actual orphan
headers. Include discovery runs before indexing so dependency ownership is
settled before symbols are interned.

Each command gets independent preprocessing and source caches, with inline header
bodies, so include-path changes cannot reuse expansions from a different command.
Bare-tree indexing shares compatible system bundles as described in
[Shared system header bundles](#shared-system-header-bundles).

`validate_command_macros` checks command definitions with the same parser used
by preprocessing, including function-like and variadic parameter lists, without
reading source files. Before joining the name and replacement text, command
definitions require the entire name portion to be an identifier, optionally
followed by an immediately adjacent valid parameter list. Extra tokens before
`=` are rejected; spaces and operators in the replacement text after `=` remain
valid. Undefine names must be complete identifiers; a valid name need not already
be defined. Shared flags are parsed once, then finalized and validated once per
effective language (C or C++). Shared configuration errors are reported once,
even when several sources or both languages use the same invalid arguments.
Shared flags use this check before indexing; selection and
fallback rules are defined in [Compilation databases](ANALYSIS.md#compilation-databases-62).

### Missing-header guidance

After indexing, `trace analyze` prints a summary on stderr when ordinary or
forced includes remain unresolved. It groups preprocessing warnings by the
requested header spelling, counts retained diagnostic locations, and shows at
most ten headers, ordered by count (then spelling), with an original file/line
example. Other preprocessing warnings are excluded. All original diagnostic
rows remain in the SQLite `diagnostics` table in minimal and full exports;
the summary neither changes include resolution nor suppresses warnings.
Standard and internal kernel header guidance recognizes redundant `.` components
while preserving the requested spelling in the summary and diagnostics.

A matching indexed file with the complete requested path suffix (ignoring
redundant `.` components, but retaining `..`) is evidence
that a file exists but the failing configuration did not reach it. The summary
shows that file and its include search root; check per-TU paths and restrictions
such as quoted-only search or the test partition before adding it. This is a
candidate, not proof that the file belongs to the target SDK. Without a match,
trace reports that availability outside indexed files is unknown: it does not
search the host filesystem or infer that a dependency is uninstalled.

`--apply-include-candidates` opts into applying these suggestions. For every
unresolved header spelling, including those beyond the ten-entry display limit,
it takes the same lexicographically first matching indexed file shown by the
summary. Distinct candidate include roots are appended in path order after
explicit CLI `--include` paths, with the same search precedence as `--include`
(before compilation database paths). Absolute include spellings do not produce
candidate roots. Existing CLI paths are not added again.

Conflict checks protect only spellings selected from unresolved-header
diagnostics, including selections repaired by earlier retries. Already-resolved
headers are not protected: a new candidate root containing the same spelling
can shadow a header previously found through compilation database `-I`,
`-isystem` or `-idirafter` paths. Use explicit CLI `--include` paths or correct
the build metadata when those existing resolutions must be preserved.

Before each retry, the CLI combines cached command search classes with the
proposed CLI paths and verifies every TU's explicit ordinary, system and after
search paths, along with shared flags and CLI fallback paths, against the
selected files. This includes search-class normalization: a path also named by
`-isystem` or `-idirafter` moves out of the ordinary include list even when it was supplied by the CLI. It checks selected
spellings in sorted order, removes the first conflicting new root in the
effective search, and rechecks the remaining roots. Stderr names the rejected
root, the shadowed selection, the file it would load instead, the skipped
candidates, and the TU when its command causes the conflict. Selections remain protected across retry passes, even
after their missing-include warnings disappear. Existing CLI paths are never
removed or reordered; if they conflict with a selection, the new batch is
skipped. Conflicting candidates may therefore remain unresolved: use explicit
paths or build metadata to choose the intended search order.

These checks omit inferred project include directories and the unique-basename
fallback, which follow the explicit search classes. If no explicit path finds
a selected spelling, a retry can still resolve it to a different file through
those fallbacks without a conflict report. Includer-relative and quoted-only
paths also remain outside these checks; adding ordinary roots does not change
their earlier precedence.

A rejected root can be added on a later pass if the previously applied paths
now give it safe search precedence. Conflicts are checked again on every pass,
but identical conflict reports are printed only once per analysis run. When an
existing search path causes a conflict, the report names it as an existing path
and explains that the new batch is skipped.

Compilation metadata and command options are read and parsed once for these
conflict checks across all retry passes. Only the proposed CLI roots are merged
with the cached search classes on subsequent checks, preserving search-class
normalization without reprocessing the database for every rejected root.

Indexing repeats with those roots before analysis and export. If newly reached
headers expose further missing includes, their candidate roots are applied too,
until no new root can be added. Each root is added at most once; unresolved
includes without a usable new candidate remain diagnostic warnings. Stderr
lists the paths applied in each pass. The database retains the final pass's
diagnostics; missing-include warnings repaired by a retry are no longer current.
Without the flag, the summary and diagnostic retention behave as described above.

Applying candidates uses the existing custom preprocessor and does not fetch
headers, probe additional compilers, change dependency ownership or bypass
`--require-local-inputs`. This option trusts matching indexed paths across the
project: a matching spelling does not establish the right SDK version or per-TU
configuration. Use compilation metadata or explicit includes when that choice
must be controlled.

Configure unresolved headers using the target build environment:

- Prefer `--compile-commands PATH`, or automatic discovery at the root or
  `build/`, for per-TU paths, macros, forced includes and language settings.
  Root `compile_flags.txt` can supply shared flags when no JSON database is
  selected; it does not identify a target compiler.
- For standard C/C++ headers, opt into `--system-includes` with compilation
  commands naming the target compiler and its target/sysroot options. A failed
  command probe keeps explicit paths; it does not replace that compiler with
  a host compiler. Sources without usable commands, shared flags and orphan
  headers use the project GCC/Clang fallback described
  [above](#compilation-database-options-62). Use that fallback only when it
  matches the target. Host headers must not substitute for the target SDK.
- Obtain the target SDK/dependencies and generated headers. Use `--dep ROOT`
  to give dependency headers declaration-only ownership and `--include DIR`
  for the directory above the requested include spelling, or record that path
  in compilation metadata. Dependency ownership and include search paths serve
  different purposes.
- Internal kernel headers, such as `linux/device.h`, `linux/dma-mapping.h`
  and `linux/of.h`, need the matching configured kernel build tree, generated
  headers and architecture paths. Userspace `/usr/include/linux` UAPI headers
  cannot replace them.

Header categories are recognized by known spellings, not angle brackets or
the presence/absence of `.h`. Compiler probes discover configuration only;
the custom preprocessor still processes every project source.

## Role in the pipeline

```mermaid
flowchart LR
  Raw[Raw .c on disk]
  IG[IncludeGraph]
  PP[preprocess_file]
  Out[Expanded source]
  TS[tree-sitter parse]

  Raw --> IG
  IG --> PP --> Out --> TS
```

- **`IncludeGraph`** (`trace-parse/src/deps.rs`) scans project files for `#include` directives, builds dependency edges, discovers include directories, and marks which files need preprocessing.
- Preprocessed output is **cached** per file (parallel cache fill when `--jobs > 1`) and released once
  the unit is lowered, so the text held at once is bounded by the parse batch in flight.
  Final resident LineMap entry vectors shed unused capacity when their retained text/map
  size is at most the 512 KiB individual spill threshold. Larger vectors retain
  their capacity until the spill decision, which must still account for reserved
  storage rather than just live entries.
- The shared indexing source cache retains at most 32 MiB of text and LineMap
  entry capacity after each warm/committed/settled source, and spills any single
  payload over 512 KiB. Aggregate victims are selected by canonical path order.
  Active preprocessing/loading workers and provenance metadata are outside this
  payload budget. Spills retain original bytes and all mapping provenance, use
  independent read cursors, and are deleted on eviction or release. LineMap
  blocks store every source position and a bitmap indicating which entries
  additionally carry macro-expansion provenance; absent expansion fields are
  restored to their exact defaults. Both encoding and decoding use bounded
  scratch buffers and 256 KiB I/O buffers, coalescing block bitmaps and payloads
  into larger operations. Spill decisions still use the in-memory payload size.
  Spills share a randomly named `trace-spill-*` directory under the temporary directory,
  removed when its last spill is released (also on normal completion or error).
  CLI `--timeout-secs` blocks new spills and removes this directory on a
  best-effort basis before exiting with code 124. SIGKILL cannot run cleanup;
  any leftovers remain grouped in the `trace-spill-*` directory. Discovery
  texts that must be settled are released directly. Spill or load failures abort
  indexing rather than silently dropping facts. This budget changes storage only;
  expansion-cache publication still follows unit order.
- Macro definitions share immutable `Arc` token and parameter slices between live
  tables and cached directive logs. Expansion still creates invocation-specific
  tokens, preserving token origins, hide sets, and ordered define/undef replay.
- If preprocessing fails hard (the unit's own file cannot be read), the unit is dropped and an error diagnostic is recorded; a stop *inside* a file keeps the output produced so far (see Error recovery).

## Phases

### P0 (implemented)

| Feature | Notes |
|---------|-------|
| Input-boundary recording | Optional shared collector for consumed headers and compilation/link metadata; see [compilation database options](#compilation-database-options-62). |
| Missing-header guidance | Bounded CLI summary and opt-in candidate include roots; see [missing-header guidance](#missing-header-guidance). |
| Compilation options (#62) | Ordered command macro operations, quoted/normal/system/after include search classes, opt-in compiler default path discovery (`--system-includes`), opt-in [compiler predefined macro discovery](#compiler-predefined-macros-230) (#230), forced headers with original LineMap origins; language and standard-version predefines. Inferred paths respect the [test partition](ANALYSIS.md#declaring-header-eligibility). |
| External-header lexing | Bounded sharing of raw tokens by path, language and contents; macro expansion remains per consumer. See [compilation database options](#compilation-database-options-62). |
| Shared system bundles | Compatible leading system includes containing only dependency-owned inline text share expansions and declaration IR; see [Shared system header bundles](#shared-system-header-bundles). |
| Comments | `//`, `/* */` |
| Line splicing | Translation phase 2 (C11 5.1.1.2p1) runs inside the lexer, at the character level: every `\`-newline (`\n` or `\r\n`, and — as gcc and clang accept with a warning — with horizontal whitespace between the `\` and the newline) is deleted before any token is recognized, so a spliced identifier is one identifier (`int c\`-newline-`d;` is `int cd;`), a spliced multi-character punctuator is munched whole (`.\`-newline-`..` is `...`, so `#define F(x, .\`-newline-`..)` is the variadic macro gcc and clang see — #38), a spliced encoding prefix or string body stays one literal, a `//` comment ending in `\` continues onto the next line and `/\`-newline-`*` opens a block comment. A `\r` counts only as the first half of a `\r\n`: `\`-CR-CR-LF and `\`-CR-space-LF are not splices, as in clang. Inside a C++11 raw string literal the splice is reverted ([lex.pptoken]p3) and the body is kept as written; in C, where `R"(…)"` is the identifier `R` followed by an ordinary string, a splice in that body is deleted like in any string — clang's `-std=c11` behaviour, while gcc and clang in their default GNU modes lex a raw string there and keep the splice as written. Token positions stay physical — a token starts where its first character sits in the file, which is what the LineMap needs — so adjacency can no longer be read off `line`/`col`; instead the lexer records on every token whether it touched the previous one (`Token::adjacent_before`, with a splice counting as nothing and whitespace, comments and newlines as a gap), and that flag is what the function-like `#define` test and `#` stringizing read. Before #38 the lexer kept `\` + newline as two tokens and each consumer that needed phase 2 skipped the pair itself, so the splice was still visible everywhere else: `int c\`-newline-`d;` came out as `int c\ d;` and a spliced ellipsis stayed three `.` tokens |
| Punctuators | The multi-character forms the lexer knows are one token each, so the re-speller writes them back unbroken: `##`, the two-character operators (`<<` `>>` `<=` `>=` `==` `!=` `&&` `\|\|` `++` `--` `+=` `-=` `*=` `/=` `%=` `&=` `\|=` `^=` `->`) and the three-character `...` and `->*`. Anything else is one token per character; the ones that still come out glued do so because the output spacing rule happens not to separate them (`::`, `.*`, and the `=` after `>>` / `<<`), not because they are single tokens. `->*` needed its own token because the spacing rule *does* separate it: the space before `*` after `>` that keeps `shared_ptr<T> &p` from gluing into `>&` also split `c->*m` into `c-> * m`, which is not C++, and tree-sitter then recovered the operand as a callee — fabricating a function named after it, in one corpus case named after a template parameter (#37). Recovering the token does not make the construct parse: tree-sitter-cpp knows `->*` only as an overloadable operator name and as a fold operator — `struct S { int operator->*(int); };` and `(a ->* ...)` both parse — and has no rule for it as a binary operator in an ordinary expression, so `c->*m` is an ERROR site even in pristine source, while `c.*m` parses. `->*` is also lexed as one token only in C++: gcc and clang tokenize `a->*b` in C as `->` then `*`. The re-speller gives `...` a leading space when the output already ends in a preprocessing number, because a pp-number absorbs `.` and alphanumerics (C11 6.4.8) and would otherwise swallow it — the GNU case range `case 0x0300 ... 0x0307:` would come back as the single number `0x0300...0x0307`; after an identifier (`Args...`) no space is added. Before #28 there was no `...` case, so every ellipsis lexed as three `.` tokens and, since the spacing rule *does* put a space before `.`, came out as `. . .` — a tree-sitter ERROR site in every variadic declaration, 111 of the 756 catalogued ERROR sites across the pinned corpora |
| Literals | String and character literals are one token each, spelled exactly as written — encoding prefix (`u8` / `u` / `U` / `L`), quotes and escapes included — and re-emitted verbatim; only `#include "…"` (an unprefixed `"…"`) and `#if 'c'` (any prefix) look at the body. Before #14 a prefix was a separate identifier, so `L'x'` came out as `L 'x'` (a tree-sitter ERROR site) and `L"w"` as `L "w"`. **Lexing is language-aware**: a translation unit is lexed as C or C++ by its extension (`.cpp` / `.cc` / `.cxx` / `.c++` / `.C` and the C++ header spellings are C++, `.c` and the ambiguous `.h` are C; `PreprocessOptions::with_language` overrides), and every header it includes is lexed as the including unit, since a header has no language of its own. The include-expansion cache is keyed by (canonical path, language), and the index warm pass warms a header once per language it is reachable in, so a header shared by C and C++ units replays into each the tokenization its own lexer would produce; the header's own IR is parsed as C++ when any C++ TU can reach it, matching the grammar choice below. The two shapes that differ are C++-only: in C, `R"(x)"` is the identifier `R` (which may be a macro) followed by an ordinary string, and `'a'C` is the literal followed by the identifier `C`, so `#define R …` / `#define C …` still expand there (valid C that the C++ rules would swallow). In C++, C++11 **raw string literals** (`R"delim(…)delim"`, with the same prefixes and a d-char-sequence of up to 16 characters) are lexed the same way, so the inner `"`, `(`/`)`, `\`, comment markers and newlines are neither escapes nor token or line boundaries; a literal may span lines (the LineMap maps the whole token to its first character, the tokens after it to their real positions). A C++11 **user-defined-literal suffix** — an identifier glued to the closing quote or to a number, as in `R"(json)"_json`, `"text"s`, `'c'_w` or `10_km` — is part of the literal token, so it is re-emitted without the space every other adjacent identifier gets (`R"(json)" _json` is no longer a user-defined literal) and `#` stringizes it as part of the literal. `#`-stringizing any literal escapes its `"` / `\` like gcc/clang, and a newline inside a raw string as `\n` (gcc's `cpp_quote_string`) so the result is still a one-line literal: `STR(R"(a "b")")` is `"R\"(a \"b\")\""`; a CRLF inside the literal is one newline (`\n`, not `\r\n`), as translation phase 1 normalizes it before the raw string is read. Something that starts like a raw string but is not one — no `(` within 16 characters of the quote, a delimiter containing space / `\` / `)` / a control character, or no matching `)delim"` before end of input — falls back to identifier + ordinary string, so a malformed literal costs a couple of bad tokens instead of swallowing the rest of the file. Before #14 `R"(a "q" b)"` lexed as `R "(a " q " b)"`, the dominant parse-failure class in the hiview corpus (JSON templates and regexes in tests) |
| `#include "..."` / `<...>` | Include path stack + `--include` |
| `#include` macro operand | C11 6.10.2: if the tokens are not already `"..."` / `<...>`, the rest of the line is macro-expanded and must then form a header-name (`#include FOO` with `#define FOO "n.h"`) |
| `#define` | Object-like and **function-like**, including variadics: anonymous `...` / `__VA_ARGS__` and GNU named `args...` (internally the anonymous form registers `__VA_ARGS__` as the last parameter, so both styles share the "last parameter collects the remaining arguments" rule). GNU `, ## args` comma elision drops the comma only when the variadic arguments are **omitted** — an explicitly empty argument (`F(1,)`) keeps the comma, matching gcc/clang — and works through nested expansions (`LOG(fmt, ...)` forwarding `##__VA_ARGS__` into another variadic macro). Which `##` the rule governs is read off the **body** (#65): a `,` spelled immediately before the operator with the variadic tail parameter right after it. A parameter standing between the comma and the `##` is not that form, so `#define F(v, x, ...) g(v, x ## __VA_ARGS__)` invoked as `F(1,)` is an ordinary placemarker paste and keeps its separator (`g(1, )`) — gcc's reading. clang instead collapses the placemarker first and applies the comma rule to whatever that exposes (`g(1)`); reading the body means the answer depends only on how the macro was written, so the same invocation cannot expand two ways because an argument was spelled as a newline rather than as nothing. A parameter list ends at the line end like the rest of the directive: an unterminated (`#define P(x, ...` with no `)`) or malformed (`#define P(x y)`) list warns, drops that definition, and preprocessing continues with the next line — the list never runs on to a `)` in later code (gcc behaviour). A definition is function-like only when `(` immediately follows the name (C11 6.10.3p10, decided from the lexer's adjacency flag since tokens carry no whitespace; a `\`-newline is deleted before tokenizing, so `F\` + `(x)` on the next line is function-like): `#define ALIAS (VALUE)` and `#define HALF (.5)` are object macros whose replacement starts with `(`, and `#define F (x) x` makes `F(1)` expand to `(x) x(1)` as in gcc/clang |
| Macro rescanning | Function-like macros invoked inside another macro's expansion are expanded too (C11 6.10.3.4); uninvoked function-like names are emitted verbatim |
| LineMap macro provenance | Replacement tokens retain definition spelling, outermost invocation coordinates, outermost macro attribution, and a deterministic fingerprint of nested invocation and parameter-substitution steps; cache splice and spill/reload preserve all four (see [Macro expansion provenance and attribution](#macro-expansion-provenance-and-attribution)) |
| Macro hide set | Replacement-list tokens carry the active macro name (and the invoking token's hide set) on rescan, so self-referential macros such as `#define FOO FOO, BAR` terminate. In emitted source, prescanned arguments acquire that hide state after prescan; nested `MIN(MIN(a,b),c)` still expands. Conditional expressions rescan raw arguments with their existing hide state, allowing nested same-name wrappers in `#if` and `#elif`. |
| Compiler attribute groups | Balanced noise groups are elided at rescan boundaries after expanding arguments and chained marker aliases. GNU semantic attributes (including underscore spellings and `noreturn`) and MSVC linkage attributes survive. Directives and malformed groups are retained; elision preserves newlines and LineMap origins. |
| Weak linkage pragmas | Active `#pragma weak name` survives preprocessing for IR annotation capture; inactive directives are discarded. Alias spelling marks the named declaration weak without synthesizing an alias body. |
| Pragma operator | Active `_Pragma("...")` (also macro-generated, wide and UTF string spellings) destringizes its operand and uses the same handling as `#pragma`: `once` records the guard, `weak` retains annotation input, other pragmas are ignored. The operator contributes no syntax or include-boundary tokens. An incomplete rescan spelling stays as tokens; a balanced operator with an unsupported operand warns and is dropped without truncating the file. Resource limits still apply. |
| Expansion depth cap | 256 nested expansions; further expansion is skipped with a warning (backstop if hide-set does not apply) |
| Runaway caps | Per-file limits (defaults): 64 nested `#include`s, 64 inclusions of any one path per run (`max_file_expansions`, see the repeated-`#include` row above — depth alone does not bound repetition), 32 MiB live output, 8M tokens (macro rescan included). The token budget counts tokens **materialized**, not merely walked: a function-like invocation walks O(1) — the argument list is skipped wholesale — and then copies each argument once per parameter occurrence, so before #30 an 80k-token argument reached 397 MB of peak allocation under a 2,000-token budget. The projected replacement is charged before it is built, and the rescan charges the result again as it walks it, so a function-like expansion costs roughly twice its width against the budget. Exceeding output/token budget stops that file with an error diagnostic; include-depth and the per-path expansion cap skip the nested include. CLI `--timeout-secs N` aborts the whole process. |
| `##` token pasting | In macro bodies after argument substitution; chained pastes (`a ## b ## c`) collapse left to right, empty operands are placemarkers on either side (the surviving operand keeps the left operand's adjacency), so an empty left parameter never pastes a preceding token; a dangling `##` with no operand is dropped |
| `#` stringize | `#param` in a function-like body becomes one string literal spelling the argument **as written** (C11 6.10.3.2): token spellings with the inter-token whitespace collapsed to a single space (tokens carry no whitespace, so "was there a gap" is the lexer's `Token::adjacent_before`; a newline inside the argument counts as a space, but a `\`-newline splice does not — phase 2 deletes it before tokenizing, so `STR(a\`-newline-`b)` is `"ab"` and `ALL(p,\`-newline-`q)` under `#__VA_ARGS__` is `"p,q"`, while `STR(a \`-newline-`b)` keeps the real space and is `"a b"`; an argument substituted into a body takes the parameter's adjacency, not the call site's, so `S((x))` invoked as `G( a )` is `"(a)"`; a parameter that expands to nothing leaves its whitespace behind, so `S(a x+b)` with `x` empty — omitted, or written as nothing but newlines — is `"a +b"` while `S(a(x+b))` is `"a(+b)"`, and an argument that starts with a newline takes the parameter's adjacency on its first real token; and the literal `#x` produces stands where the `#` stood, so `S((#x))` is `"(\"a\")"` — all as clang spells them), `"` and `\` inside string and character literals escaped, and `#__VA_ARGS__` spelling the variadic arguments with their commas exactly as written (the argument parser keeps the top-level `,` tokens, so `F(p,q)` gives `"p,q"` and `F(p , q)` gives `"p , q"`). The literal maps to the expansion site (the invoking macro name) in the LineMap. The argument is not macro-expanded first, so `STR(VALUE)` is `"VALUE"`; the two-level `XSTR(x) STR(x)` idiom expands `VALUE` in the outer macro and yields `"42"`. A `#` not followed by a parameter is emitted verbatim. Before #13 the `#` and its argument were deleted, leaving `#(`/stray-`)` parse errors in every stringizing log/assert macro |
| Conditionals | `#ifdef`, `#ifndef`, `#if` / `#elif` / `#else` / `#endif`. `#if` conditions get full constant-expression evaluation: the `defined X` / `defined(X)` operator is resolved over unexpanded tokens (C11 6.10.1p4), object **and** function-like macros expand (hide-set painted, depth-capped), and the result is parsed with C operator precedence (`?:`, `\|\|`, `&&`, bitwise, `==`/`!=`, relationals, shifts, arithmetic, unary `!`/`~`/`-`/`+`, parens). Integer literals accept `0x`/`0b`/octal prefixes and `u`/`U`/`l`/`L` suffixes; a number or character constant that is not an integer constant — a floating literal, or a C++ user-defined literal such as `10_km` or `'a'_x` — makes the expression malformed (gcc/clang reject it) instead of evaluating as if the suffix were absent; arithmetic models 64-bit intmax_t/uintmax_t with the usual arithmetic conversions (an operand mixed with an unsigned one converts to unsigned, so `-1 < 1U` is false; `>>` is arithmetic for signed, logical for unsigned; a literal is unsigned when suffixed `u`/`U` or too large for intmax_t). Identifiers surviving expansion evaluate to 0; malformed expressions (trailing tokens, unbalanced parens) conservatively skip the branch; per chain at most one branch activates. `\`-newline continuations inside conditions are spliced by the lexer like everywhere else. Conditions in skipped groups are not evaluated (and malformed `#ifdef` operands there are tolerated, as is a `#` followed by something other than a directive name — a line marker `# 1 "x.c"`, a `#!` line — which in an active group is an error). Condition macro expansion runs under its own budget (64K tokens / 1M steps); exceeding it warns and conservatively skips the branch. `#elif` after `#else` warns and is ignored. Conditional groups are **file-scoped** in both directions: a group must be closed in the file that opened it, so at the end of every file (root or `#include`d) the frames it left open are reported as unterminated and popped, and the includer resumes in the state it had at the `#include`; and `#elif` / `#else` / `#endif` may only act on a group opened in the same file — in a header they never see the includer's frames, so a stray one is a `… without #if` error there even when the includer is inside an `#if` (before this, a header ending inside `#if 0` silently swallowed the rest of the translation unit, and a header starting with `#endif` consumed the includer's frame so the includer's own `#endif` failed — #8). A header that *stops early* (budget, malformed argument list) is rebalanced the same way but reports only its stop diagnostic, since its unprocessed remainder may still hold the `#endif`. |
| Repeated `#include` | A path already processed in this run is skipped only on a reason the file itself stated, and re-expands otherwise (#56). Two reasons exist. An **include guard** — `#ifndef NAME` (or `#if !defined(NAME)`) as the file's first token, `#define NAME` as the very next directive, and the matching `#endif` with nothing but whitespace after it — suppresses while `NAME` is defined, so `#undef`ing it and including the file again expands the body a second time, as in cpp. It is read off the token stream **before** the body runs (`detect_include_guard`), not observed at the `#endif`: a guarded header that reaches itself back through another header re-enters while its own `#endif` is still ahead, and a guard learned on the way out cannot answer for that inclusion. A **`#pragma once`** reached with the enclosing conditionals active holds for the rest of the translation unit whatever later happens to the controlling condition; in a skipped group it states nothing. A file with neither re-expands on every `#include`, which is what X-macro tables (`#define ENTRY …` / `#include "list.def"`, once per definition) depend on — before #56 every inclusion after the first was suppressed unconditionally and the whole table after the first expansion was lost, silently. Repetition is bounded by `max_file_expansions` (default 64 inclusions of one path per run, separate from the include-depth cap); past it the include is skipped with a diagnostic, since an unguarded pair of files that include each other recurs at every site. Guards learned inside a cached header travel with its entry (`IncludeExpansion::guards`), for the files that entry actually covers — a consumer never visits them and would otherwise re-expand what the entry already holds, or, if the list were wider than the entry's coverage, suppress an `#include` whose body it never received |
| `#line` | Location tracking in `LineMap` |
| `#error` / `#warning` | Active directives report their spelled message as an error or warning and indexing continues. Directives inside inactive conditional arms are ignored. Unknown directives in inactive arms are ignored too. |
| Macro argument prescan | In emitted source, function-like macro arguments are expanded before ordinary parameter substitution. Operands of `#` and `##` keep their original tokens. The active macro is suppressed on substituted tokens during the later rescan, including an identifier exposed by prescan next to an argument-produced `(`. The same rule applies to nested forwarding macros such as glibc's `__SIMD_DECL(__MATH_PRECNAME(...))`. |
| `#undef` | |
| Access-specifier renames | In C++, `#define private public` and its like are not applied; see [Access-specifier renames](#access-specifier-renames) |
| Predefined | `__FILE__`, `__LINE__` — inside a macro body `__LINE__` is the line of the (outermost) invocation, C11 6.10.8.1, and both work in object-like as well as function-like bodies. **Language predefines** (#70): every run's macro table is seeded with what the language it is lexed as implies — `__STDC__ 1` in both, `__STDC_VERSION__ 201710L` in a C unit, `__cplusplus 201703L` in a C++ unit (what the OpenHarmony clang defaults to; the corpora only compare against `201103L`) — so `#ifdef __cplusplus`, `defined(__cplusplus)` and `#if __cplusplus >= 201103L` take the C++ arm in a C++ unit and the C arm in a C unit. A command-line `-D` of the same name outranks the predefined value, and a source `#undef` removes it: these are ordinary definitions, not builtin fallbacks. The warm pass seeds each header's table the same way, so a header shared by a C and a C++ unit reads `__cplusplus` bound in one run and unbound in the other — an ordinary fingerprint dependency (#55) that keeps each unit's expansion its own. Before #70 nothing was predefined and every `__cplusplus` read resolved to `0`, so `extern "C"` wrappers and C++-only declarations were silently excluded from C++ units (930 reads across the eval corpora, all unbound). No compiler is claimed: `__GNUC__` / `__clang__` stay unbound. Builtin fallback macros for headers the indexed tree does not ship are separate (see [Builtin fallback macros](#builtin-fallback-macros)) |
| Token spacing | No space before `)` / `]`; space between `>` and `&` / `*` so `operator()` and `shared_ptr<T> &p` survive re-lexing |

### P1 (planned)

- `__VA_OPT__` (C23)

### P2 (planned)

- `_Pragma`, the remaining standard predefined macros (`__DATE__` / `__TIME__`, the C++ feature-test macros); `__STDC_HOSTED__` is imported only by compiler discovery

## Builtin fallback macros

Code is indexed without a real toolchain, so macros whose definitions live in
headers the tree does not ship (gtest, Linux kernel headers, `<inttypes.h>`)
survive preprocessing, produce tree-sitter ERROR nodes, and can drop whole
functions from the index ([parse-failure reporting](PARSE_FAILURES.md) helps
investigate the affected files).

`FFI_EXPORT` and `DECLARE_SINGLETON` are not preprocessor fallbacks. Their
available definitions expand normally; narrowly supported unresolved forms
can receive a diagnosed parser-input approximation afterward. See
[C++ parse-input normalization](ANALYSIS.md#c-parse-input-normalization) for
the authoritative recovery forms and their missing semantics.

The preprocessor installs fallback definitions for the common offenders:

| Macros | Fallback | Failure mode avoided |
|--------|----------|----------------------|
| `__UNUSED` | empty | `T &x __UNUSED` breaks the function definition |
| `__user`, `__iomem`, `__percpu`, `__rcu`, `__force`, `__init`, `__exit`, `__initdata`, `__exitdata`, `__read_mostly` | empty | kernel address-space/section annotations are syntax errors in declarators (`char __user *buf`, `int __init foo(void)`) |
| `PRI[diuxXo]{8,16,32,64}` | format-specifier string literal (e.g. `PRIu64` → `"llu"`) | `"%" PRIu64` leaves an identifier between string literals |
| `container_of(ptr, type, member)` | `((type *)(void *)(ptr))` | a type keyword in expression position; the fallback keeps the pointer flow and target type |
| `HWTEST(a, b, level)` | `static void a##_##b()` | gtest/OpenHarmony test macros followed by a body are unparseable, losing every test body in a file |
| `HWTEST_F(a, b, level)`, `HWTEST_P` | `namespace { struct a##_##b##_Test : a { void TestBody(); }; } void a##_##b##_Test::TestBody()` | the same, for a fixture test, expanded to what gtest generates: a class derived from the fixture `a`, whose `TestBody` the source's `{ ... }` defines. The body keeps the fields and methods it inherits in scope, `this` has the derived type, and the fixture gains no member it never declared. The anonymous namespace gives the class internal linkage, as the generated one has, so the same fixture and test name in two test files are two tests rather than one definition and a duplicate |
| `MOCK_METHOD(ret, name, params[, specs])` | `ret name params [const] [noexcept] [override] [final];` | unexpanded gMock declarations corrupt the enclosing mock class and drop its member prototypes (and their virtual-dispatch targets) from the index. gMock's comma-protecting parentheses — one pair around a comma-containing return type, one around such a parameter type — are the macro's own syntax and are removed; of the spec list only what C++ accepts on a declaration survives, spelled in C++'s order — cv-qualifier, `ref(&)`/`ref(&&)`, `noexcept` (with its expression, if any), `override`/`final`; `Calltype(...)` has no declaration spelling and is dropped. Only the *top level* of that list holds specifiers: an identifier nested in one of their argument lists belongs to an expression, so the `const` of `noexcept(is_nothrow<const T&>::value)` is part of the type `noexcept` asks about and `Calltype(final)` names a calling convention, neither being a qualifier of the member |
| `MOCK_METHOD0`–`MOCK_METHOD10` and `MOCK_CONST_METHOD0`–`MOCK_CONST_METHOD10`, each also in its `_T`, `_WITH_CALLTYPE` and `_T_WITH_CALLTYPE` spelling | `ret name(params) [const];` | legacy gMock passes one function type `ret(params)`, split here at its parameter list so the declaration matches the mocked signature; the leading calling-convention argument is dropped |

Two shapes cannot be recovered exactly. A return type that is itself a
parenthesized declarator (`void (*)(int)`, `int (&)[4]`, `void (C::*)(int)`,
`void (C<T>::*)(int)`, and the legacy `void (*())(int)` whose trailing group
belongs to the returned pointer rather than to the method) would have to be
re-spelled around the member name, so it degrades to `void` — the member and
its class survive, the type does not.

A parenthesized group counts as that declarator only when it holds a
ptr-operator sequence and nothing else — `(*)`, `(&)`, `(&&)`, `(*const)`, or
a nested declarator behind one, as in the `(&())` of `int (&())[4]`. Only a
nested-name-specifier may precede it, naming the class a pointer to member
points into (`(C::*)`, `(::C::*)`, `(C<T>::*)`), and it has to end in `::` or
the group is an argument list — `(int *, char)`, which a macro spelling a
comma-containing type leaves behind. What follows the ptr-operators separates
a declarator from an expression that merely starts with one: a declarator runs
on into the group's `)`, a nested declarator or an array bound, never into a
name. So the other things that put parentheses in a return type keep their
spelling and are expanded by the rescan: template arguments
(`std::function<void(int)>`) and `decltype(...)`, whose operand is never a
declarator however it starts — `decltype(*p)` and `decltype(*(p))` alike.

Because the arguments are read structurally rather than rescanned in place,
they are macro-expanded *before* they are read — unlike a replacement list,
whose arguments the rescan reaches later. An alias would otherwise be
invisible to every one of those tests: `#define RET (std::pair<int, int>)` is
one identifier, so its protecting parentheses would survive into the
declaration, and `#define PARAMS (int, int)` would look like no parameter list
at all. For the same reason the expansion carries the macro's hide set onto
the argument tokens it promotes into the declaration, so a member that happens
to be named after the macro is declared rather than rescanned as a fresh
invocation.

An invocation gMock itself would reject expands to nothing instead of to a
broken declaration: a comma-containing type left unparenthesized (or still
holding a comma once its protecting parentheses come off, so it was a list
rather than a type), an invocation naming no return type or no member, and a
legacy signature whose parameter list is not the last thing in it
(`int(int) const`) — the group cannot be split off, and spelling the signature
whole in front of the member name is not a declaration.

Expanding an argument ahead of the rescan builds a token vector instead of
writing to the output, so it is charged to the same per-token expansion budget
the emitting path uses; nothing else would bound its width, and an expansion
bomb reached through a gMock argument would otherwise allocate without limit
before the emitting path saw its first token.

The gMock entries are not replacement lists: a replacement list cannot split
one macro argument or unwrap parentheses, so the preprocessor expands them in
code (`expand_gmock_method`). They obey the same override and conditional
rules as every other fallback.

Semantics — a fallback is a definition of last resort, never an answer to
"is this defined?":

- Each fallback is installed only when the name is not already defined; any
  real definition — CLI `-D`, source or header `#define`, a cached include's
  macro delta — **overrides** it.
- Fallbacks behave as **undefined** throughout conditional evaluation: they do
  not satisfy `#ifdef` / `#ifndef` / `defined()` — so the ubiquitous guard
  idiom (`#ifndef container_of` + `#define container_of(...)`) takes its
  branch and the tree's genuine definition wins — and inside a `#if`
  expression they stay unexpanded identifiers evaluating to 0 (`#if 1 ||
  __init` is true; an empty expansion would mangle the expression). A source
  `#define` of the name then makes it a normal macro.
- Installation happens per preprocess, after cloning the shared warm table, so
  fallbacks apply even in warm-cache runs (and stay overridden if the warm
  table carries a real definition). CLI `-D` defines absent from the warm
  table are re-applied first-wins so they beat fallbacks in that path too.
- The include-expansion cache records the **ordered log** of `#define` /
  `#undef` directives a header executed (nested replays included), not a
  before/after table diff, and replays it through the same mutation helpers
  live directives use — so a cache hit and a cache miss agree on macro state,
  fallbacks included (see [Macro operations in cached
  entries](#macro-operations-in-cached-entries)).

## Access-specifier renames

Unit tests open the classes they test by defining an access specifier as
another before including the class's header:

```cpp
#define private public
#define protected public
#include "ability_manager_service.h"
#undef private
#undef protected
```

In C++ an object-like definition of `public`, `protected` or `private` whose
replacement list is one of those three names is **not applied**: the name
stays undefined, the header keeps the specifier it spells, and the matching
`#undef` finds nothing to remove. Any other definition of those names is an
ordinary macro (`#define private`, `#define private public: int opened;
private`), and so is every definition of them in C, where they are ordinary
identifiers.

Nothing lowered from the text depends on which specifier a member has: a
call through a private member binds as one through a public member does. What
the definition did change is the text of every header declaring a class, and
with it the expansion cache. A header read `private` bound, so the
expansions stored by units that do not define it matched no test unit. The
replacement `public` is spelled where the test source defines it, which made
the binding of each test source a different one, and an expansion made under
one matched no other test either. Past the eight variants a header keeps,
each such unit expanded the header, and what it includes, into its own text.
On `ability_ability_runtime`, where 1,016 sources rename a specifier, that
was half of all text parsed and lowered (measurements in
`docs/EVAL_REPORT.md`). A body whose text spells a specifier (a local class in
a header's `static` function, a lambda in a member) was also one function
entry per distinct expansion, and is one entry now.

The rule applies to every C++ file, production sources included, since it is
about what lowering reads and not where the definition is. What it gives up
is what could observe the definition: a conditional on the name (`#ifdef
private`, `#if defined(private)`) reads it as undefined, and a macro whose
replacement list spells the name (`#define ACCESS private`) expands to the
specifier as written rather than to the renamed one. Stringizing is
unaffected, since `#x` spells its argument before expansion. None of the four
trees measured has a conditional on one of the three names.

## LineMap

The normal indexing path stores preprocessed source payloads larger than 512 KiB
(text plus allocated LineMap entry storage) in automatically cleaned temporary
files once discovery and settling are done, and only for sources that are read
back. Discovery immediately drops the text and LineMap of a unit that expanded
a header: settling must rebuild that unit, so the discovery payload is never
parsed. The settle pass spills the rebuilt payload alone, and a warmed header
that is not indexed on its own drops its text without a file.
Include provenance, variants, conditionals,
diagnostics, and original paths remain in memory. Parsing loads the original
payload without rerunning preprocessing; cache hits retain the same expansion
contents and source mappings.
Temporary storage uses the OS temporary directory and may reside on a tmpfs.
Spill/load errors fail indexing with a diagnostic rather than changing source
contents. Once header and orphan units have been lowered, the frozen expansion
cache is released before translation-unit merging when every TU has a usable
settled payload and no exploration variant will re-preprocess it. The include
graph's raw source copies are also released then when indexing owns that cache;
a caller-supplied source cache remains available. This limits
retained source and expansion payloads, not the final IR, and trades
temporary-file I/O for lower process RSS.

Cache composition appends origin mappings only within each live output chunk's
half-open byte interval. Both interval endpoints are found by binary search;
entries at the start are retained (including equal-offset entries), entries at
the end are excluded, and no preceding mapping is added. Only referenced files
are interned into the destination map. This avoids copying overlapping mapping
suffixes while preserving original file/line/column attribution.

The preprocessor records mappings from **output byte offsets** to original `(file, line, col)` in `LineMap`.

**Current behavior:** tree-sitter parses **preprocessed** source; IR spans (`Span` in `trace-ir`) are resolved through the `LineMap` to original `(file, line, col)`. `#include`d code attributes to its header and TU-local code keeps its original pre-expansion position. A token spelled in a source macro replacement list maps primarily to that spelling in the macro definition; the same entry records an optional expansion `(file, line, col)` for the **outermost** invocation. Calls produced by a replacement list therefore use the macro-body spelling as their request position and retain the invocation position separately. Repeated invocations share the spelling position but remain distinct through their expansion positions. Every replacement token carries the invocation coordinates in `Token::origin`, inherited through forwarding macros, and its definition file in `Token::spelling_file`; `__LINE__` continues to read the invocation coordinate required by C11. Substituted argument tokens retain their own source position; emitted-source prescanned arguments also inherit active-macro suppression for rescan. In addition, each emitted mapping carries a deterministic expansion-chain fingerprint. The fingerprint incorporates nested invocation spellings and replacement-list parameter occurrences, so two uses of one helper macro inside the same outer invocation remain distinguishable even when their spelling and outermost expansion coordinates match. It is derived only from source paths, coordinates, macro names, and the preceding chain; allocation order and hash-table seeds do not affect it. Token pasting combines operand fingerprints deterministically and selects file, line, and column together from one operand, so it cannot combine a definition file with argument coordinates. Builtin and command-line macros have no source spelling file and continue to map to the invocation. During emission, replacement-list spelling paths use a per-run path-to-ID cache, so repeated tokens from one definition do not rescan the growing `LineMap` file table. Cached `#include` expansions store both halves and the chain fingerprint of each mapping in their sub-`LineMap`; splice and spill/load preserve them, and macro fingerprints include replacement-token spelling provenance so an expansion cached for one definition location cannot replay for an identical definition elsewhere. Cached and live preprocessing therefore produce identical positions and occurrence provenance.

The `LineMap` must keep byte-accurate offset mapping when extending the preprocessor.

### Macro expansion provenance and attribution

To support downstream AST lowering and noise macro filtering (`--ignore-macro`, `--ignore-logging`), `LineMap` tracks the macro origin of emitted tokens alongside their source coordinates:

- **Outermost macro ownership**: Each emitted token carries `Token::expansion_macro` identifying the outermost macro invocation in the expansion chain. If macro `A` calls helper macro `B`, tokens resulting from `B` retain attribution to `A`. In `LineMap`, macro names are interned into a deduplicated table (`LineMap.macros`) via `intern_macro` (accelerated by an `FxHashMap` index to avoid linear scans during emission).
- **Macro arguments and forwarding**: When an argument expression is passed into a function-like macro (such as `#define LOG(x) x` invoked as `LOG(foo())`, including nested argument forwarding), `with_substitution_provenance` attaches the outer macro name to substituted argument tokens while leaving `origin: None`. In the `LineMap`, substituted argument tokens are emitted with `expansion_file: u32::MAX`, preserving their original source file/line/col and avoiding the fabrication of a spurious macro-body expansion span (satisfying Invariant 1: calls spelled in source macro arguments keep their own source position). At the same time, the entry records `macro_id`, enabling AST lowering to identify that the expression was wrapped in the macro and suppress it when the macro is ignored.
- **Builtin, predefined, and CLI `-D` macros**: Tokens originating from command-line definitions (`-D`), compilation database operations, predefined macros (`__FILE__`, `__LINE__`), or builtin fallback definitions (`install_builtin_macros`) have no file on disk (`spelling_file: None`). When expanding, their replacement tokens have `origin: Some(...)` and `expansion_macro: Some(...)`. The preprocessor falls back their spelling file ID to `lm_current_file()`, ensuring that `LineMap.push_expansion` records their outermost macro attribution rather than discarding it as `u32::MAX`.
- **Cache splice and spill/reload**: Precomputed include expansions (`IncludeExpansion`) store sub-`LineMap`s. When an expansion is spliced into a consumer translation unit (`LineMap::splice_submap`), macro names are remapped into the parent unit's macro table via `intern_macro`. Spilled translation units (`IndexCache`) serialize `macros` and entry `macro_id` fields, guaranteeing identical provenance across cache hits, disk spills, and live preprocessing.

## Include resolution

For `#include "header.h"` / `#include <header.h>`:

1. Directory of the including file
2. Paths from `IncludeGraph.include_dirs` (discovered + `--include`)
3. Warning diagnostic `include file not found, skipping: <path>` if not found; the directive is skipped

The static include-graph scan links **project-local** files under the analysis
root. External system headers resolve later during preprocessing and contribute
dependency declarations; they are not added to the static graph's file scan.

A candidate counts as found when it is a file on disk **or** a `source_cache` key that names none — a
virtual header, which only a caller that invents one has (tests, and a host embedding the
preprocessor); `SourceCache` derives that set from its own keys, so a cache built by reading files
off disk asks the filesystem and nothing else. Both halves are memoized, because step 1 builds a
fresh candidate for every (including file, spelling) pair and runs on every `#include` a unit
executes: the filesystem answer is memoized per path for the whole indexing run
(`trace_ir::is_file_cached`, reset by `start_file_probe_epoch` at the start of each run, so a
long-lived host sees the tree as it is when the run begins), a first probe is answered from the
parent directory's listing when that settles it (a name the directory does not hold is a miss
without a `stat`; a listed, case-folded or non-ASCII name still asks the filesystem, as does one
under a directory that could not be listed), and step 2's directory walk is memoized per
(spelling, quoted). Only answers the filesystem gave are memoized per path: a miss the listing
settled is not (the listing answers a repeat as cheaply), and a candidate under a directory that
is not there is settled from the nearest listing already in hand — the listed ancestor that does
not hold the child on the way down — with the absent directories neither read nor recorded. Reading
starts only below a directory a probe asked for directly: the existing levels between it and the
candidate are read and recorded on the way down, and no ancestor is ever read on demand (that
would snapshot directories above the analyzed tree). The
per-thread memo and the shared listings therefore grow with the files and directories probed
that exist, not with (search directories × spellings): a spelling such as
`llvm/ADT/StringRef.h` names an absent `llvm/ADT` under every search directory it is tried in,
and on the LLVM monorepo, whose inferred search list has 2,823 directories, an entry per
candidate took more than 5 GiB before the include graph was built (#209; measurements in
[MEMORY_PROFILE.md](MEMORY_PROFILE.md#llvm-monorepo-startup-peak-209)). The static include
graph's scan shares the same walk per (spelling, includer partition) across every file that
spells it, as the preprocessor does. When a `SourceCache` is supplied, explicit directory-search
results (including misses) are shared across preprocessing runs with the same ordered quote,
include, and system paths. These results expire at the next file-probe epoch. The
includer-relative candidate is still probed before this cache under the rules of step 1 (quoted
includes, or any include when strict search is off); basename fallback stays specific to each
caller, since it depends on that caller's index and strictness. Camera issues 4,343,229
candidate probes naming 224,945 distinct paths (#83).

## Include graph and header indexing

| Behavior | Notes |
|----------|-------|
| `needs_preprocess` set | Files with `#include` edges (or included by another) run through the preprocessor |
| `source_cache` | Reuse file text while scanning `#include` edges |
| Reachable headers | Preprocessed file-locally (a nested header that cannot be stored stays with its includer), parsed/lowered **once** (PCH-style header IR), then merged into TUs |
| Orphan headers | Project `.h` never reached from any `.c` are indexed as their own units (may contain calls) |
| Parallel index | Header IR, orphan headers, and `.c` TUs: parallel parse/lower, sequential merge |

### Shared system header bundles

Bundle declaration fingerprints hash each interned source path and macro name
once, then hash those content fingerprints at token positions. Intern table
numbering and unused paths do not affect declaration identity.

Bare-tree indexing caches top-level includes resolved under system search
roots as separate header units, including extensionless standard-library headers.
Only leading includes before project declarations are separated. An external
include bundle keeps its nested implementation headers inline, so
fragments inside namespaces and classes retain their enclosing syntax. Includes
inside an unfinished declaration or scope, and bundles that leave unmatched
delimiters or an unfinished token boundary, remain inline. Separation
accepts a semicolon ending or a known namespace/linkage block;
ambiguous global brace endings stay inline so an includer can continue a
class or initializer declaration.
Bundles containing inline text from outside the established dependency roots
remain inline, preserving project body ownership by original source path.
This includes project `.inc` files omitted by file discovery; absence from the
discovered-file set does not establish dependency ownership. Indexing supplies
its classified dependency roots to preprocessing, including external system
roots. Installed fragments within those roots remain shareable.
Ownership follows each include occurrence that emits inline text, including
unguarded fragments included earlier with an inactive conditional body. A
run-wide set of previously included paths cannot establish which files supplied
text to a particular bundle.
Bundles containing namespace directives, using-declarations or weak pragmas
remain inline: these affect lookup or declarations in the consumer and are
not carried by declaration IR. Type aliases (`using Name = Type;`) remain
shareable. During system-header sharing, this fallback also applies to nested
project cache entries so an enclosing bundle cannot hide these effects.
`_Pragma` operators, including macro-generated libc++ macro-cleanup sequences,
are processed as directives and removed before scope tracking. A header ending
in those operators can finish a bundle without preventing the following system
include from sharing. Macro push/pop stacks are not modeled: both pragma
spellings retain the existing ignore policy for `push_macro`/`pop_macro`.

Project declarations and a nonempty preceding project-header preamble still
prevent system bundle separation. System-root membership alone does not prove
independence from project types or overload context; relaxing this gate also
requires preserving include-position imports. In particular, own-header-first
layouts can still keep later system headers inline. The
[indexing diagnostics](MEMORY_PROFILE.md#indexing-subphase-timings) expose
refused bundle attempts by reason, so successful sharing must be checked on
each platform rather than inferred from Linux measurements.

Existing macro fingerprints, directive replay and ordered discovery select compatible variants;
lowering imports the matching language and variant with original locations and
dependency ownership. Each system bundle records the complete preceding shared
system-header context, including its size and stable declaration identities;
matching final macro values cannot substitute a different earlier declaration
variant, even when the later header does not include its prerequisite itself.
Declaration identities hash expanded text, the language standard and original
source/macro provenance, with intern-table indices resolved to paths and names.
Macro-read fingerprints still validate expansion compatibility separately.
Different macro histories producing identical declarations therefore do not
recursively split later declaration contexts.
Existing project-header caching retains its macro/read-dependency policy;
an explicit include of an already supplied system fragment records the exact
bundle declarations supplying it. Implicit incoming type dependencies of
project headers retain the [existing sharing approximation](ANALYSIS.md#shared-header-functions).
Consumed prerequisites also enter a system variant's declaration preamble,
including factory prototypes and return types used to infer global initializers.
Cached project headers also import consumed system declarations before lowering
initializers; see [Shared header functions](ANALYSIS.md#shared-header-functions).
Reference-binding facts also survive declaration imports and reusable preambles;
see [Dereferenced operands](ANALYSIS.md#dereferenced-operands).
Deferred discovery records header adoption in its ordered history even
when the header executes no macro directives. Prerequisites supplied inside a
cached parent are local to that parent, rather than requirements on its caller.
Compilation-database indexing retains its command-local
inline preprocessing path. Conditional recording and exploration also keep
system headers inline so their conditional chains remain observable.
Repeated unguarded system includes remain inline after their first shared
expansion, preserving deliberate re-inclusion under changed macros.

Once ordinary translation units and standalone headers all have settled,
spillable preprocessing results, indexing can release expansion-cache payloads
as their final header-lowering consumer finishes. Standalone headers are
preprocessed before that release; their later AST lowering reads the same
settled text and source map. Cache keys with no planned header-lowering
consumer are released first. Other keys remain until every selected
language/variant has been lowered, keeping variant indices intact. Exploration
or an unsettled input retains the cache through the ordinary lifetime instead.

The final header-variant plan is built after standalone orphan preprocessing.
Its consumed set includes both TUs and orphan headers and is closed over nested
variant prerequisites. An orphan can replay a warmed configuration that no TU
used; that configuration must still receive declaration IR before expansion
retirement. TU-only provenance may classify initial header paths, but cannot
choose the final variants or determine which cache keys have no consumers.
This avoids retaining expanded text beside the corresponding declaration IR
when no future preprocessing can use it.

These caches live for one indexing run. The existing variant cap applies;
entries that cannot be stored remain in the consuming unit. Cached system
bodies contribute declarations only, under the existing dependency-root rules.
Header lowering schedules dependencies between exact language/variant units;
opposite include orders may form a path-level cycle while the variant
dependencies remain acyclic. Genuine cycles retain a deterministic sequential
fallback.

### Determinism

Indexing output must be identical across runs of the same tree. Two mechanisms guarantee this:

- **Macro warm pass** runs sequentially over TU-reachable headers in canonical (`index_order`) order, once per language the header is reachable in (C++ when a C++ TU reaches it or the extension is a C++ header spelling, C when a C TU reaches it; `PreprocessOptions::with_language` forces one language for everything). Each warm runs under a **fresh macro table** seeded only from command-line defines lexed in that language; the per-header final states are merged into a **per-language union table** — every warmed header lands in both unions, the same-language warm preferred and the other language's re-lexed for the destination as fallback (`MacroDef::relexed` spells the tokens back with their adjacency intact, which is all the two lexers disagree about), so each union stays the full macro superset (twin-guard dedup, orphan and PCH headers) whichever language reached a header — and each later phase hands a file the union (and option set) of the language it is lexed and parsed as. The first language warmed is the one the header is parsed as and feeds the source cache; a second only fills the expansion cache and its union. Reachability comes from the include graph and warming grows that graph (a `#include MACRO` is discovered only while preprocessing the header spelling it), so the pass runs to a **fixed point**: after each round the discovered edges are added, every header's language list is recomputed, and a header whose list changed — a `.h` first reached from C alone that a C++ unit turns out to reach through a macro include, and is therefore parsed as C++ — is evicted from the source cache and warmed again under a fresh table, so the text the parser sees is always the preprocess in the language it is parsed as. The root file of a preprocess run is never replayed from the expansion cache (a nested include may have cached it first); only nested `#include`s replay. Sharing one accumulating table across headers let include guards defined by earlier-warmed headers starve later headers' expansions (the starved text was then frozen into the expansion cache). Dedup between headers comes from the shared expansion cache, not from shared guard state.
- **No output reads a hash-map iteration order.** This is not a mechanism so much as an invariant the other two rest on, and it is worth stating because the maps are hashed with `rustc-hash`, which unlike `std`'s `RandomState` is *not* seeded per process: a change that started ordering output by a `HashMap`/`HashSet` traversal would no longer be caught by comparing two runs. Where an order matters it comes from a sort, a `Vec`, an `IndexMap`, or `index_order`. Byte-identical output across the hasher swap is the evidence that nothing did (#83).
- **Expansion-cache freeze**: the cache is keyed by (canonical path, language), so a C unit never replays a header's C++ tokenization or vice versa. Every parallel phase after discovery reads the include-expansion cache read-only (`PreprocessOptions::frozen_expansion_cache`): hits replay stored entries, misses expand inline under each TU's own macro/guard state and are *not* inserted — first-writer-wins inserts would make results scheduling-dependent. The one pass that writes it, discovery, runs its units on the pool too, but publishes in unit order (see [Parallel discovery](#parallel-discovery-88)).

Translation units inherit the **union** of all warm-pass macro states: cached expansions replay without executing their `#define` directives, so TU-local code still needs those macros.

### Parallel discovery (#88)

Before any text reaches the parser, every translation unit is preprocessed once against a
**writable** expansion cache (the discovery pass), so that the macro environments the tree
actually presents at each `#include` end up stored and shared; units that expanded a header
themselves are then re-run against the frozen result (the settle pass). Discovery used to run one
unit at a time on the main thread. It now runs on the worker pool and produces the same cache and
unit text as the one-at-a-time pass in `index_order`; discarded unit text is released after commit.

**The cache key.** The cache maps `(canonical path, language)` to that header's **variants**: the
expansions stored so far, in publication order, at most `max_expansion_variants` of them. A variant
is found by its `trace_preproc::MacroFingerprint` (#55) — the bindings the expansion read,
unbound names included — not by key alone: at an `#include`, a run takes the *first* stored
variant whose fingerprint its current environment satisfies, and otherwise expands the header and
publishes the result, unless a stored variant already has that fingerprint's signature or the list
is full. A variant is named everywhere downstream by its **index** in that list:
`PreprocessResult::replayed_variants` and `IncludeExpansion::nested_variants` record indices, and
header units are lowered and merged per `(header, language, index)`.

**Which variant a record names.** A unit merges only the header units its records name, so a
record must name the variant that holds a header's declarations — the one the unit (or an entry's
publisher) reached the header through first. An include replayed later under the header's guard
matches a whitespace-only variant; the unit still records reaching it, but the earlier variant
keeps standing for the header, both in the unit's own record and in the nested records of the
entries it composes. An entry records every nested header it contains: those it replayed, those it
skipped by their guard (the variant the run reached them through, or the one it expanded and
published itself), and those it expanded live, whose own nested records it inherits. A
whitespace-only expansion that includes nothing never stands for its header, and a variant a
replayed entry's nested record carries in does not replace one the run expanded itself (it may be
that empty one). A header without a guard can be expanded several times in one entry, differently
each time (`#define NAME First`, `#include "decl.h"`, then `Second`): the record keeps one pair per
expansion, not one per header. Without these rules a
replayed entry could define a header's guard while naming no variant, or only the empty one, and
the consumer lost the header's declarations (`PermissionDataBrief` in `security_access_token`,
#121).

**An expansion that cannot be stored.** A header a run expands itself is published as a new
variant, or recorded as the stored variant with its signature. When neither is possible — the list
is full, the fingerprint is one no consumer can satisfy, or the run was cut short — no record can
name it. Its text is then not dropped from its includer's output as a stored child's is: it stays
there, so the includer's entry (or the unit, which the settle pass rebuilds anyway) holds its
declarations. `IncludeExpansion::inlined` lists the headers an entry holds this way, theirs
included, and the records of the headers such an expansion replayed pass to the includer's entry
as well, since they are reached through that entry alone.

Lowering an entry merges what the headers it holds include, as it merges what its own header
includes, so their declarations see the types they would in a unit of their own; and what a held
header declares belongs to that header's unit, not to the holder's, so a production header held by
a test header stays production code.

An entry that skips such a header by its guard has neither its text nor a record of it. That is
harmless where the entry, at the time of the skip, holds the header itself or records an entry
that does (`IncludeExpansion::covers`): whoever replays it lowers the declarations with the
holder's. Otherwise the entry would hand its consumers the guard without the declarations, and it
is not stored; its own text stays with its includer in turn. The rule is judged when the skip
happens, for every entry being composed, so an entry that would come to hold the header once the
entries between them close unstored (`outer.h → mid.h → sib.h` skipping the `decl.h` that
`mid.h → inner.h` holds) is not stored either, and a unit expands all of it. That is the
conservative side: text a unit expands itself is lowered with everything the unit has in scope,
where a header's own unit sees only the types of what it includes. Storing such entries was
measured and put aside for the same reason as the header-unit limits in `docs/EVAL_REPORT.md`.

Before, the includer's entry defined the header's guard, held none of its text and named no
variant of it or of what it included: a unit replaying the entry was lowered without the header's
classes, and a member call on one stayed unbound (`CaptureSession::AddOutput` in camera, #123).

**Origins are canonical paths.** The line map names each origin file by its canonical path, not
by the spelling include resolution found it under, so a header reached through two search
directories (`-I inc`, `-I src/../inc`) is one file to everything downstream.

**Why the order of publication matters.** Two things about a variant are decided by who publishes
it first, not by the header and the fingerprint alone. Its index is its position, and a consumer
takes the first match, so the order decides which of two overlapping variants a unit replays. And
its content is taken relative to what its publisher had already included (`files`, `ops`,
`guards`, `nested_variants`), so two units that reach a header under the same fingerprint can build
entries that differ in exactly the parts later consumers inherit. Content-addressing the entries
does not remove this, because the variation is in the content. Running the writes in scheduling
order made the output a thread race — camera moved over 20,299 / 20,326 / 20,847 direct edges
across three runs of one tree. So the publication order is fixed as **unit order**, and parallelism
must not be able to change it: what varies between runs is only which thread does the work.

**Why scheduling cannot affect what a unit sees.** A worker runs a unit ahead of the commit point
with `PreprocessOptions::defer_expansion_publish`. The run keeps what it would publish in a
`trace_preproc::ExpansionJournal` instead, sees its own kept-back entries exactly as a publishing run
sees what it published, and records, per header it looked up:

- the variants it was shown, by `IncludeExpansion::id` (an identity unique to the process);
- for each lookup, what it took — a shown variant, or its own entry / nothing — and the **point
  in its macro history** it happened at. The run keeps an undo log of every `#define` / `#undef`
  (the binding each one replaced) and its final table, so the environment of any past lookup can be
  asked any fingerprint later.

Its result never records an index for a variant that is not stored yet: it records a provisional
handle (a tag bit plus the id), and the commit turns handles into indices.

The committing thread takes units strictly in order. When unit *n*'s turn comes, units 0..*n*−1 have
been published, exactly as in the serial pass, and `ExpansionJournal::commit` decides whether the
parallel run is the run a publishing preprocess of *n* against that cache would be. It is if,
for every header the run looked up, the variants it was shown are all stored, in the order it saw
them, and no stored variant it was *not* shown would have been taken instead: none stored ahead of
what a lookup took (or anywhere, for a lookup that took its own entry or nothing) satisfies that
lookup's environment. Its own entries must also not duplicate a stored signature or overflow the
cap. Then every lookup would have taken what it took, the run would have built the same text and
published the same entries, and they land at the end of each list, where the handles now resolve.
Otherwise nothing is written and the unit runs again. Whether a run commits depends only on the
cache as units 0..*n*−1 left it and on the run itself, never on when it ran or what it was shown, so
thread timing can make a run fail and repeat but cannot change what is committed.

**Scheduling**, which affects only speed (`trace-parse/src/expansion_discovery.rs`). Workers take
units up to four per worker past the commit point. Each run is shown the stored variants plus the
kept-back entries of the finished, uncommitted runs ahead of it
(`PreprocessOptions::expansion_journals_ahead`), so a unit that only replays an earlier unit's new
variant need not wait for that unit to commit. A run is discarded, and its unit queued again, as soon
as the cache or a finished run ahead of it rules it out (`ExpansionJournal::is_contradicted` /
`rules_out`), or a run it was shown is discarded and had fed it (`feeds`). A queued unit waits for
the unfinished units ahead of it whose own discarded run likely rules it out (`likely_rules_out`). A
unit that reaches the commit point with no run that stands — or whose worker run failed — is run by
the committing thread itself, the same way with nothing shown ahead, which always commits since
nothing else writes the cache. `--jobs 1` runs the serial pass itself.

The `preprocess-done` progress line reports the pass separately:
`preprocess-done: Xs (Ys discovery, R runs discarded, K in order + Zs settle of N of M units)`,
where `K` counts the units the committing thread ran itself. Measurements are in
`docs/EVAL_REPORT.md` ("Parallel include-expansion discovery").

### Header IR (PCH-style)

Indexing sets `inline_include_bodies = false`. Nested cacheable `#include`s replay **macros and include-once state** but do not copy header tokens into the consumer's live output. Each header's preprocessed text is therefore file-local, except for a nested header whose expansion cannot be stored: its text stays in its includer's ("An expansion that cannot be stored", above).

After the warm pass, reachable headers are parsed and lowered **once**. PCH order uses the include graph **plus preprocess `included_headers`** (macro includes the raw scanner misses). Independent leaves may run in parallel waves; a header is never in the same wave as a nested include it needs. Include **cycles** are not a parallel wave: leftovers are indexed in include-graph order so nested layouts stay visible. Between cached project headers, nested `#include` IR merges **types and typedefs** from **direct** includes (plus this header's preprocess `included_headers`) so `struct StreamHost { struct IDeviceIoService service; }` sees `Dispatch`, and `GpioIrqFunc func` sees the typedef, without copying every descendant's functions/flow into ancestor units. Child PCH units already nested-merged grandchild types. Types-only merges also preserve the type table's C++ reference-member metadata, each class's own default/copy constructor eligibility and explicit constructor imports, including nested classes and member class templates; see [member-initializer construction](ANALYSIS.md#c-support-first-step). Parallel isolation *without* those preprocess edges interned empty tags / `Int` and dropped field stores (`DeviceNodeExtDispatch` lost `DispatchToMessage`, `GpioOnDevEventReceive` lost `gpio->func`).

Headers that become reachable only after those preprocess edges are added join the PCH set (and leave the orphan path) so translation units can merge their prototypes.

Header merge order is represented by precomputed global ranks. A unit orders
only the headers it needs by those ranks; cached-expansion paths absent from
the global order follow in lexical order. This preserves the same merge
sequence without scanning every project header for every unit.

Parallel unit lowering with settled source text uses a byte budget and a count
safety cap, with ordered merging on the caller thread. Configured indexing
retains its previous source-family count window. Admission limits are
defined in
[Parsing memory limits](PERFORMANCE_REVIEW.md#6-make-parsing-memory-limits-independent-of-cpu-count).
The coordinator stays
outside the worker pool so all requested indexing workers can run. Sequential
lowering still runs on the pool's 16 MiB stack; only parallel merging moves
to the caller. Preprocessing cache publication retains its separate unit-order
contract described in “Parallel discovery.”

Translation units parse only their own remainder and merge already-built header `UnitIndex`es for every header **reachable** via the include graph, plus preprocessor `included_headers` (a cached splice can omit a nested path from the graph edge, and types-only nested PCH does not copy nested prototypes into ancestors). That merge is **symbols only** (types + prototypes): header call sites and flow are already in the global program from PCH. Merge also rewrites leftover incomplete nested tags. That is the analogue of a PCH / clangd preamble. Merging only direct includes dropped `DispatchToMessage` from `DeviceNodeExtDispatch` (designated `.Dispatch =` in `hdf_wifi_core.c` when `sidecar.h` was not in that TU's `included_headers`).

Grammar follows the including language, not the extension alone: `.hpp`/`.hh`/`.hxx`/`.inl`/`.ipp` always use the C++ parser; a `.h` uses C++ if any C++ TU can reach it via the include graph, otherwise C. (Before PCH, header tokens were spliced into the TU and parsed with that TU's grammar, so `plugin.h` included from `plugin.cpp` was already C++.)

Standalone `preprocess_file` still inlines by default so a single-file expansion remains self-contained.

C++ semantic lowering retains the effective `__cplusplus` value with each
preprocessed unit and cached header expansion. Header cache fingerprints read
that value even when the header has no language-version conditional, so C++17
and C++20 constructor bodies do not share an incompatible lowering environment.

### Macro operations in cached entries

A cached expansion replays its text **without** executing the `#define`s it contains, so a header whose body *invokes* macros defined by an earlier-included header would starve: at warm time the dependency was processed inline (fine), but a consumer warmed later splices the dependency's cached body and never learns its macros. Therefore each `IncludeExpansion` records the **ordered log** of `#define` / `#undef` directives its processing executed, nested replays included (`IncludeExpansion::ops`). An ordered log rather than a table diff: a diff cannot represent a no-op `#undef` (name absent at capture, defined in a later consumer) or an undef-then-redefine of a name present at both boundaries.

Macro operations are stored as `MacroOp::Define(Arc<str>, Arc<MacroDef>)` and `MacroOp::Undef(Arc<str>)` in a contiguous `Arc<[MacroOp]>` slice (24 bytes per entry), eliminating heap string allocations during directive logging, cache-frame capture, and replay.

`splice_cached` replays the log through the same mutation helpers live directives use, so a cache hit and a cache miss agree on everything a directive touches: the local table (a replayed `#define` overwrites, like live execution), builtin-fallback marks, the shared table under `accumulate_macros`, and the op log feeding an enclosing cached header's own entry.

### Cache self-containment

When an already-included header is embedded in a cache entry, its affected macro names are logged with their **current** bindings (including absence), without changing the live table. Reusing its original operations would undo intervening directives in an `A → B, D, B` diamond. External overrides of the nested header's final bindings become fingerprint dependencies. If an override conflicts with a historical dependency needed by the embedded text, the expansion is marked incompatible and is not published to the shared cache. That mark propagates through enclosing entries, so consumers expand those headers live too. Unchanged bindings, including ordinary include guards, need no extra dependency.

Cached expansions are flat text — nested `#include`s inside an entry were already resolved when the entry was built. An entry built while a nested header was already suppressed by its guard in this run would otherwise freeze *without* that header's content, permanently hiding its definitions from every consumer routed through the entry.

Re-splicing the nested cached blob into **live output** on every such skip exponentiates on diamond include graphs (each copy contains previous copies). Instead, the skip is recorded on the in-progress cache frame and the nested expansion is **embedded only into that frame's cache entry** at the `#include` site. Live output stays unique per file; frozen-phase guard-skips stay silent as before. Duplicate definitions inside a cache entry are still harmless downstream (merge deduplicates same-origin entities; re-declarations remain valid C).

Entries also record which files they claim (`IncludeExpansion.files`); files whose expansion emitted nothing are not claimed, so symbol-scope registration (`headers_of`) does not attribute phantom contributions. A cached-header include whose entire body was skipped emits a visible Warning during non-frozen phases ("resolved include expanded to nothing") — silence here is how starvation bugs historically went unnoticed.

Three conditions keep an entry **out of the cache entirely**, because publishing one would hand a wrong expansion to every later consumer (#33):

- **Nothing but diagnostics.** A header that contributed no text, no macro ops and no claimed files is not cached — its own "expanded to nothing" warning is not content. An entry holding only that replays as an empty expansion, and `splice_cached` reports the hit as a success, so every consumer reaching the header with its guard *undefined* silently loses the body. Left uncached, those runs expand it and re-derive the same reports.
- **A run that hit a run-wide limit.** After the output cap, token budget or include depth fires, everything composed is short of what the abort swallowed, so the run publishes nothing further. The scope is the whole remaining run, not just the aborted header: an enclosing header can finish "successfully" while missing exactly that content, so a narrower rule leaves the same starvation reachable one level up. The cost is lost caching on a run that was already degraded.
- **A nested include that failed to preprocess.** `handle_include` swallows the error and lets the includer continue, but the include is already in the include-once set and contributed nothing, so this expansion — and every frame enclosing it — is short that header's content. The run is marked incomplete and publishes nothing further; otherwise consumers routed through those entries stay starved even *after* the underlying failure clears (an unreadable header made readable again). Comparing a cached and an uncached expansion while the failure persists starves both sides equally and proves nothing — the regression test restores readability first. Independently, the frame is opened only once the source is in hand, so a failed read cannot leave one on the stack for the enclosing header to pop as though it were its own.

`index_order` itself is canonical: input files are sorted and dependents are visited in sorted order, so unordered `HashSet`/`HashMap` iteration cannot leak into processing order.

### Include-dir self-sufficiency

Project headers must resolve **without manual `-I` flags**: `discover_include_dirs` adds the root, every discovered header's parent directory, and every directory named `include`; a unique-basename fallback resolves names that match exactly one project file. Analyzing a tree root (e.g. an entire source checkout) therefore needs no include-path configuration.

Manual `-I` remains appropriate only for things the tool cannot discover:

- headers **outside** the analyzed root (system SDKs, vendored deps, sibling trees) — when analyzing a subdirectory whose dependencies live elsewhere;
- **platform selection**: when several dirs contain same-basename twins (e.g. per-OS adapter layers), `-I` order picks the intended one — discovery order is sorted-path and not platform-aware;
- paired with `-D` for the matching platform macros (e.g. `-D __LITEOS__`).

**Limitation:** The raw include scanner only sees literal `#include "..."` / `<...>` lines (no macro expansion). After each warm round, preprocess `included_headers` — including headers reached via `#include FOO` — are added as graph edges, so those files join PCH instead of staying orphan, and a header the new edges reclassify (C++ instead of C) is re-warmed in that language. A macro include spelled directly in a TU is not seen before the parse phase, so it cannot reclassify a header on its own. Headers excluded by `#if 0` in the preprocessor but visible in the raw graph are treated as reachable and not indexed separately — if the TU also omits them at preprocess time, calls in those headers can be missed.

## Conditional-coverage record

Most user scenarios pass no `-D` and have no compilation database, so every condition controlled by a name the run never
sees resolves against a default of `0`. With `PreprocessOptions::record_conditionals` on, the preprocessor records what
that decided (#57): one `ConditionalChain` per `#if` / `#ifdef` / `#ifndef` it meets, in the file it is in (headers
included), with one `ConditionalArm` per `#elif` / `#else` arm. Preprocessing behaviour is unchanged; the option only
adds the record.

| Field | Meaning |
|-------|---------|
| `file` | The file the chain is in, **canonical**: include resolution may spell one header several ways (`src/../common/x.h` from one includer, `common/x.h` through an include dir), and consumers merge chains across runs by file |
| `arms[i].expression` | The controlling condition **as written** — macro names unexpanded, `defined` kept; the bare operand for `#ifdef` / `#ifndef`, empty for `#else` |
| `arms[i].line`, `end_line` | Physical line of the opening `#` (even when spliced before the keyword), and the line of the chain's next directive (or one past the file's last line for a chain left open); `body_lines()` is the source lines strictly between them, including continuation lines of a multiline condition — lines, not reachable code |
| `arms[i].outcome` | `Taken` (lines emitted), `Skipped` (condition false, or an earlier arm already taken), or `Unevaluated` (the whole chain sits in excluded code, C11 6.10.1p6, so nothing was evaluated and the lines are excluded by the *enclosing* chain) |
| `arms[i].evaluated` | Whether the condition was evaluated at all: false for `#else`, for an arm after a taken one, and inside excluded code |
| `arms[i].reads` | Names the condition depends on, first read first, once each. For an evaluated arm, every name the evaluation **consulted, expansion included** (`#if HAS_X` with `#define HAS_X defined(X)` reads `HAS_X` and `X`), each with whether a macro (builtin fallbacks excepted) was bound to it at that moment. For an arm that was not evaluated, the identifiers spelled in the expression with `bound = None`. `defined`, `__LINE__` and the alternative operator spellings (`and`, `or`, `not`, …) are operators, not names, and are never reads unless a macro is actually bound to the spelling; explicit operands of `#ifdef`, `#ifndef` and `defined` are always names |
| `depth` | Nesting within the file; includer frames do not count |
| `terminated` | Whether an `#endif` closed the chain (an open one is closed at the file's end, with the usual error diagnostic) |
| `include_guard` | `#ifndef X` (or `#if !defined(X)`) before any other token of the file, `#define X` as the very next directive, no `#elif` / `#else` arm, and nothing but whitespace after its `#endif`. Judged per file: a header included inside another header's guard does not consume its includer's pending `#define`. The default-value idiom (`#ifndef TAG` / `#define TAG 1` / `#endif` followed by code) is not a guard |

Unterminated arms include the final physical line even when it contains only a comment or whitespace without a trailing newline.

Two consequences of recording at evaluation time. First, **which arm was taken is a fact, not a direction**: `#if !X`
with `X` unknown takes the *first* arm, and the record says so per arm rather than assuming the excluded code is under
`#else`. Second, **a cached header replays its text without re-evaluating its conditionals**, so a run that replays
expansions records nothing for them; the record is complete only for a run that expands every include itself, which is
what the measurement tool does.

**Measurement.** `crates/trace-cli/examples/conditional_coverage.rs` preprocesses every translation unit of a tree
from the command-line defines alone with its includes expanded inline — the environment `trace analyze` gives each
unit — plus each orphan header standalone, no expansion cache, and prints one TSV record per file, chain, arm and read,
with the arm outcomes counted across runs (a header reached from several units is evaluated once per unit). It then
classifies every name a condition read: **include-guard**; **toolchain** (a fixed list of what gcc/clang predefine —
a reserved spelling the list does not know, `__KERNEL__` say, stays unknown); **configuration** (a `-D`, an in-tree
`#define` in *any* region, or a name an in-tree GN/CMake/Make/Kconfig file spells — spelled, not parsed; #58 is the
ranked version); **unknown**, kept as a real category because #59 needs to know which names it cannot reason about.
`scripts/gen_conditional_coverage_report.py` renders the three eval corpora into
[docs/CONDITIONAL_COVERAGE.md](CONDITIONAL_COVERAGE.md). The record is the chain, not the name: excluded lines are
credited to a name alone only when it is the chain's sole dependency, and reported as *shared* otherwise, so `#if A
&& B` is not counted twice.

File totals include headers resolved outside the root through `--include`. Definition evidence is
lexed, so comments and string literals do not contribute phantom `#define` sites. A missing or
empty source tree, a hard preprocessing failure, or an unreadable file needed for totals makes
the example fail without publishing TSV output. The report reader preserves carriage returns
inside fields; only newlines separate records. A final `END\tN` record gives the number of
preceding records; missing or mismatched completion records are rejected, including captures
cut off at a complete line. Regenerate older TSV files before using the reader. Run its regression tests with
`python3 -m unittest discover -s scripts -p test_conditional_coverage_report.py`.

## Error recovery

| Condition | Behavior | In the `diagnostics` table |
|-----------|----------|----------------------------|
| Unknown `#directive` | Warning `unknown directive #name`, skip line | `preprocess` / `warning`, file and line of the directive |
| Missing include | Warning `include file not found, skipping: <path>`; the directive is skipped and preprocessing continues | `preprocess` / `warning`, file and line of the `#include` |
| Unterminated `#if` | Error diagnostic at the opening `#if`/`#ifdef`/`#ifndef` (one per open group, in the file that opened it); the open groups are closed at that file's end and preprocessing continues. Output already produced is kept | `preprocess` / `error`, the file that opened the group |
| Stray `#elif` / `#else` / `#endif` in a header | Error `… without #if`; the header stops there (its `preprocess stopped in <file>` warning follows) and the includer resumes | `preprocess` / `error` plus the `warning`, both on the header |
| Macro-argument parse failure, malformed directive, budget exceeded | Error at the offending line, then warning `preprocess stopped in <file>: …`; the file stops, output produced so far is kept. A run-wide limit (output cap, token budget, include depth) additionally stops the rest of the run publishing to the expansion cache | `preprocess` / `error` at that line, plus the `warning` on line 1 of the file that stopped |
| Resolved include expands to nothing | Warning `resolved include expanded to nothing (guard already defined?)` on the header, warm/index phases only; the header is not cached | `preprocess` / `warning` |
| Preprocess failure (hard error) | The unit's own file cannot be read: no output, the unit is dropped with an error diagnostic carrying the I/O message. For a header in the warm pass the same failure is the warning `macro warm preprocess failed for <header>` | `parse` / `error` (no file) for a unit; `preprocess` / `warning` for the warm pass |

### Diagnostics in the export

`SharedDiagnostic` is `Arc<Diagnostic>`: results and cached expansions retain
ordered references to immutable reports. Indexing options share a per-run
`DiagnosticPool` that interns the full record, including severity. It disappears
with the indexing options and does not retain data in a process-wide registry.
Standalone preprocessing may omit the pool; nested frames and cache replay still
share their report references. Pooling changes ownership only: the existing
within-run and export deduplication identities and order below remain unchanged.

Every entry of `PreprocessResult::diagnostics` reaches the SQLite `diagnostics` table as a
`stage = 'preprocess'` row (#20): `severity` is the preprocessor's own (`error` / `warning`),
`file_id` is the file the condition occurred in — a nested header, not the translation unit that
included it — and `line` is its line in that file. A header is preprocessed once as its own unit
and again inline by every translation unit whose expansion-cache lookup misses, so the same
header condition would otherwise be reported once per consumer. Duplicate cached paths are
suppressed within each preprocess run, then cross-unit duplicates are discarded as units merge
on `(file, line, message)`, keeping the first copy in index order so the row set does not depend
on `--jobs` scheduling and duplicate strings do not accumulate until the end. A header reached
from both C and C++ units is warmed
under both lexers but cached in one; what the other run reports (a `#` line inside a C++ raw
string literal is an unknown directive in C) is forwarded from the warm pass so it is not lost
with the discarded text. Include-expansion cache entries carry their diagnostics together with
their text, line map, and macro effects, so a cache hit preserves the same reports as a live
expansion. A header that would contribute nothing *but* diagnostics is not cached at all, so its
reports survive by re-expansion rather than by replay. Nothing is filtered: on a checkout
analyzed without a sysroot, `include file not found` for every system and toolchain header is
the bulk of the table (see `docs/EVAL_REPORT.md`).

A mid-run stop inside ONE nested header must not invalidate the whole TU: indexing keeps the truncated-but-LineMap-consistent prefix rather than falling back to raw source, because raw text drops every `#include`d declaration and feeds the parser unexpanded function-like macros. The stop message names the file where processing stopped so downstream tools can report the truncation point.

## Unsupported (v1)

- `_Pragma`
- `#import` (Objective-C)
- Full C11 macro prescan/rescan semantics in conditional expressions
- System headers absent from the installed compiler search paths or missing SDKs

## Testing

- Unit tests: `trace-preproc/src/` (the conditional-coverage record: chain outcomes for `#if`/`#elif`/`#else`, the
  `#if !X` first-arm case, unevaluated nested chains, reads through macro expansion, include-guard recognition against
  the default-value and late-guard shapes, per-file guard candidates across includes, unterminated chains)
- Integration fixtures: `tests/fixtures/preproc/` (`access_rename/` for a test unit that renames access specifiers and shares the header's functions with the production unit; also `empty_left_paste.c` for empty left `##` operands — stringized spacing, non-stringized literal/punctuation preservation, and which `##` the GNU `, ## __VA_ARGS__` rule governs — including the same invocation spelled with and without a newline for the empty argument, `self_ref_macro.c` for C11 hide-set / X-macro lists, `include_macro.c` for `#include FOO`, `unterminated_if_include.c` + `unterminated_if_header.h` for an `#if` left open by a header, `stray_closer_include.c` + `stray_{endif,else,elif}.h` for closers that would otherwise act on the includer's frame, `stringize.c` for `#param` in log/assert-shaped macros, `raw_string.cpp` for C++11 raw string literals in the shapes the corpora use and `raw_string_shapes.c` for the same text as valid C, where `R` and the would-be ud-suffix are macros that must still expand; the `\`-newline splice cases are unit tests, checked against gcc/clang)
- Builtin fallback fixtures: `tests/fixtures/builtin_macros/` (`kdriver.c` for the
  kernel/driver table, `hwtest.cpp` for the gtest/OpenHarmony test macros and
  `gmock.cpp` for the gMock declaration macros in their modern, legacy,
  `_WITH_CALLTYPE`, comma-protected, pointer-to-member and
  wrapped-across-lines forms)

See [ARCHITECTURE.md](ARCHITECTURE.md) for how preprocessing fits the full workflow.

### Conditional exploration consumers

`--explore` consumes recorded conditional chains in `trace-parse`; it does not change
preprocessor branch semantics. Candidate checks include preceding-arm exclusions for
`#elif`, and grouped definitions must keep all targeted predicates true. Each selected
configuration is then preprocessed independently using the frozen expansion cache.
Budget diagnostics count omitted activation goals; they do not certify reachability
or exhaustive coverage. See [analysis limits](ANALYSIS.md#limits-of---explore).

Preprocessor cleanup retains the same token spelling and origin semantics while
borrowing token text during output rendering and token pasting. Synthetic
exploration predicates omit LineMap construction because they consume only a
boolean result; file indexing continues to track every original source location.

Command-line macro replacement text is lexed without an appended newline, so a
terminal backslash is preserved. Include lookup uses platform path semantics for
relative paths and recognizes canonical source-cache entries in every search
class, including the source directory and forced includes.

### Weak linkage annotations

Meaningful GNU attributes `weak` and `__weak__` survive macro expansion and
are captured as weak linkage during lowering, for external functions and
globals only. Active
`#pragma weak name` directives are retained for the same purpose, including
pragmas following the declaration. The alias form `#pragma weak name = target`
marks an existing `name` weak; it does not synthesize an alias definition or
copy the target body. Inactive conditional arms have no effect.
