# Preprocessor specification

trace implements its own C preprocessor in `trace-preproc`. It runs before tree-sitter parsing so `#include` and `#define` are resolved without invoking gcc/clang.

## API

```rust
pub fn preprocess_file(path: &Path, opts: &PreprocessOptions) -> Result<PreprocessResult>;

pub struct PreprocessResult {
    pub output: String,
    pub line_map: LineMap,
    pub diagnostics: Vec<Diagnostic>,
}
```

CLI equivalents: `--include PATH`, `-D NAME=VALUE`.

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
- Preprocessed output is **cached** per file (parallel cache fill when `--jobs > 1`).
- If preprocessing fails hard (the unit's own file cannot be read), the unit is dropped and an error diagnostic is recorded; a stop *inside* a file keeps the output produced so far (see Error recovery).

## Phases

### P0 (implemented)

| Feature | Notes |
|---------|-------|
| Comments | `//`, `/* */` |
| Line splicing | Translation phase 2 (C11 5.1.1.2p1) runs inside the lexer, at the character level: every `\`-newline (`\n` or `\r\n`, and — as gcc and clang accept with a warning — with horizontal whitespace between the `\` and the newline) is deleted before any token is recognized, so a spliced identifier is one identifier (`int c\`-newline-`d;` is `int cd;`), a spliced multi-character punctuator is munched whole (`.\`-newline-`..` is `...`, so `#define F(x, .\`-newline-`..)` is the variadic macro gcc and clang see — #38), a spliced encoding prefix or string body stays one literal, a `//` comment ending in `\` continues onto the next line and `/\`-newline-`*` opens a block comment. A `\r` counts only as the first half of a `\r\n`: `\`-CR-CR-LF and `\`-CR-space-LF are not splices, as in clang. Inside a C++11 raw string literal the splice is reverted ([lex.pptoken]p3) and the body is kept as written; in C, where `R"(…)"` is the identifier `R` followed by an ordinary string, a splice in that body is deleted like in any string — clang's `-std=c11` behaviour, while gcc and clang in their default GNU modes lex a raw string there and keep the splice as written. Token positions stay physical — a token starts where its first character sits in the file, which is what the LineMap needs — so adjacency can no longer be read off `line`/`col`; instead the lexer records on every token whether it touched the previous one (`Token::adjacent_before`, with a splice counting as nothing and whitespace, comments and newlines as a gap), and that flag is what the function-like `#define` test and `#` stringizing read. Before #38 the lexer kept `\` + newline as two tokens and each consumer that needed phase 2 skipped the pair itself, so the splice was still visible everywhere else: `int c\`-newline-`d;` came out as `int c\ d;` and a spliced ellipsis stayed three `.` tokens |
| Punctuators | The multi-character forms the lexer knows are one token each, so the re-speller writes them back unbroken: `##`, the two-character operators (`<<` `>>` `<=` `>=` `==` `!=` `&&` `\|\|` `++` `--` `+=` `-=` `*=` `/=` `%=` `&=` `\|=` `^=` `->`) and the three-character `...` and `->*`. Anything else is one token per character; the ones that still come out glued do so because the output spacing rule happens not to separate them (`::`, `.*`, and the `=` after `>>` / `<<`), not because they are single tokens. `->*` needed its own token because the spacing rule *does* separate it: the space before `*` after `>` that keeps `shared_ptr<T> &p` from gluing into `>&` also split `c->*m` into `c-> * m`, which is not C++, and tree-sitter then recovered the operand as a callee — fabricating a function named after it, in one corpus case named after a template parameter (#37). Recovering the token does not make the construct parse: tree-sitter-cpp knows `->*` only as an overloadable operator name and as a fold operator — `struct S { int operator->*(int); };` and `(a ->* ...)` both parse — and has no rule for it as a binary operator in an ordinary expression, so `c->*m` is an ERROR site even in pristine source, while `c.*m` parses. `->*` is also lexed as one token only in C++: gcc and clang tokenize `a->*b` in C as `->` then `*`. The re-speller gives `...` a leading space when the output already ends in a preprocessing number, because a pp-number absorbs `.` and alphanumerics (C11 6.4.8) and would otherwise swallow it — the GNU case range `case 0x0300 ... 0x0307:` would come back as the single number `0x0300...0x0307`; after an identifier (`Args...`) no space is added. Before #28 there was no `...` case, so every ellipsis lexed as three `.` tokens and, since the spacing rule *does* put a space before `.`, came out as `. . .` — a tree-sitter ERROR site in every variadic declaration, 111 of the 756 catalogued ERROR sites across the pinned corpora |
| Literals | String and character literals are one token each, spelled exactly as written — encoding prefix (`u8` / `u` / `U` / `L`), quotes and escapes included — and re-emitted verbatim; only `#include "…"` (an unprefixed `"…"`) and `#if 'c'` (any prefix) look at the body. Before #14 a prefix was a separate identifier, so `L'x'` came out as `L 'x'` (a tree-sitter ERROR site) and `L"w"` as `L "w"`. **Lexing is language-aware**: a translation unit is lexed as C or C++ by its extension (`.cpp` / `.cc` / `.cxx` / `.c++` / `.C` and the C++ header spellings are C++, `.c` and the ambiguous `.h` are C; `PreprocessOptions::with_language` overrides), and every header it includes is lexed as the including unit, since a header has no language of its own. The include-expansion cache is keyed by (canonical path, language), and the index warm pass warms a header once per language it is reachable in, so a header shared by C and C++ units replays into each the tokenization its own lexer would produce; the header's own IR is parsed as C++ when any C++ TU can reach it, matching the grammar choice below. The two shapes that differ are C++-only: in C, `R"(x)"` is the identifier `R` (which may be a macro) followed by an ordinary string, and `'a'C` is the literal followed by the identifier `C`, so `#define R …` / `#define C …` still expand there (valid C that the C++ rules would swallow). In C++, C++11 **raw string literals** (`R"delim(…)delim"`, with the same prefixes and a d-char-sequence of up to 16 characters) are lexed the same way, so the inner `"`, `(`/`)`, `\`, comment markers and newlines are neither escapes nor token or line boundaries; a literal may span lines (the LineMap maps the whole token to its first character, the tokens after it to their real positions). A C++11 **user-defined-literal suffix** — an identifier glued to the closing quote or to a number, as in `R"(json)"_json`, `"text"s`, `'c'_w` or `10_km` — is part of the literal token, so it is re-emitted without the space every other adjacent identifier gets (`R"(json)" _json` is no longer a user-defined literal) and `#` stringizes it as part of the literal. `#`-stringizing any literal escapes its `"` / `\` like gcc/clang, and a newline inside a raw string as `\n` (gcc's `cpp_quote_string`) so the result is still a one-line literal: `STR(R"(a "b")")` is `"R\"(a \"b\")\""`; a CRLF inside the literal is one newline (`\n`, not `\r\n`), as translation phase 1 normalizes it before the raw string is read. Something that starts like a raw string but is not one — no `(` within 16 characters of the quote, a delimiter containing space / `\` / `)` / a control character, or no matching `)delim"` before end of input — falls back to identifier + ordinary string, so a malformed literal costs a couple of bad tokens instead of swallowing the rest of the file. Before #14 `R"(a "q" b)"` lexed as `R "(a " q " b)"`, the dominant parse-failure class in the hiview corpus (JSON templates and regexes in tests) |
| `#include "..."` / `<...>` | Include path stack + `--include` |
| `#include` macro operand | C11 6.10.2: if the tokens are not already `"..."` / `<...>`, the rest of the line is macro-expanded and must then form a header-name (`#include FOO` with `#define FOO "n.h"`) |
| `#define` | Object-like and **function-like**, including variadics: anonymous `...` / `__VA_ARGS__` and GNU named `args...` (internally the anonymous form registers `__VA_ARGS__` as the last parameter, so both styles share the "last parameter collects the remaining arguments" rule). GNU `, ## args` comma elision drops the comma only when the variadic arguments are **omitted** — an explicitly empty argument (`F(1,)`) keeps the comma, matching gcc/clang — and works through nested expansions (`LOG(fmt, ...)` forwarding `##__VA_ARGS__` into another variadic macro). A parameter list ends at the line end like the rest of the directive: an unterminated (`#define P(x, ...` with no `)`) or malformed (`#define P(x y)`) list warns, drops that definition, and preprocessing continues with the next line — the list never runs on to a `)` in later code (gcc behaviour). A definition is function-like only when `(` immediately follows the name (C11 6.10.3p10, decided from the lexer's adjacency flag since tokens carry no whitespace; a `\`-newline is deleted before tokenizing, so `F\` + `(x)` on the next line is function-like): `#define ALIAS (VALUE)` and `#define HALF (.5)` are object macros whose replacement starts with `(`, and `#define F (x) x` makes `F(1)` expand to `(x) x(1)` as in gcc/clang |
| Macro rescanning | Function-like macros invoked inside another macro's expansion are expanded too (C11 6.10.3.4); uninvoked function-like names are emitted verbatim |
| Macro hide set | Replacement-list tokens are painted with the macro name (and the invoking token's hide set) so self-referential macros such as `#define FOO FOO, BAR` terminate; nested `MIN(MIN(a,b),c)` still expands because argument tokens are not painted |
| Expansion depth cap | 256 nested expansions; further expansion is skipped with a warning (backstop if hide-set does not apply) |
| Runaway caps | Per-file limits (defaults): 64 nested `#include`s, 32 MiB live output, 8M tokens (macro rescan included). The token budget counts tokens **materialized**, not merely walked: a function-like invocation walks O(1) — the argument list is skipped wholesale — and then copies each argument once per parameter occurrence, so before #30 an 80k-token argument reached 397 MB of peak allocation under a 2,000-token budget. The projected replacement is charged before it is built, and the rescan charges the result again as it walks it, so a function-like expansion costs roughly twice its width against the budget. Exceeding output/token budget stops that file with an error diagnostic; include-depth skips the nested include. CLI `--timeout-secs N` aborts the whole process. |
| `##` token pasting | In macro bodies after argument substitution; chained pastes (`a ## b ## c`) collapse left to right, empty operands are placemarkers on either side (the surviving operand keeps the left operand's adjacency), so an empty left parameter never pastes a preceding token (it also never hides the `##` from GNU comma elision: `f(a, x ## __VA_ARGS__)` with `x` and the varargs both omitted is `f(a)`, not `f(a,)`); a dangling `##` with no operand is dropped |
| `#` stringize | `#param` in a function-like body becomes one string literal spelling the argument **as written** (C11 6.10.3.2): token spellings with the inter-token whitespace collapsed to a single space (tokens carry no whitespace, so "was there a gap" is the lexer's `Token::adjacent_before`; a newline inside the argument counts as a space, but a `\`-newline splice does not — phase 2 deletes it before tokenizing, so `STR(a\`-newline-`b)` is `"ab"` and `ALL(p,\`-newline-`q)` under `#__VA_ARGS__` is `"p,q"`, while `STR(a \`-newline-`b)` keeps the real space and is `"a b"`; an argument substituted into a body takes the parameter's adjacency, not the call site's, so `S((x))` invoked as `G( a )` is `"(a)"`; a parameter that expands to nothing leaves its whitespace behind, so `S(a x+b)` with `x` empty — omitted, or written as nothing but newlines — is `"a +b"` while `S(a(x+b))` is `"a(+b)"`, and an argument that starts with a newline takes the parameter's adjacency on its first real token; and the literal `#x` produces stands where the `#` stood, so `S((#x))` is `"(\"a\")"` — all as clang spells them), `"` and `\` inside string and character literals escaped, and `#__VA_ARGS__` spelling the variadic arguments with their commas exactly as written (the argument parser keeps the top-level `,` tokens, so `F(p,q)` gives `"p,q"` and `F(p , q)` gives `"p , q"`). The literal maps to the expansion site (the invoking macro name) in the LineMap. The argument is not macro-expanded first, so `STR(VALUE)` is `"VALUE"`; the two-level `XSTR(x) STR(x)` idiom that expands first needs C11 argument prescan (unsupported, see below) and also yields `"VALUE"`. A `#` not followed by a parameter is emitted verbatim. Before #13 the `#` and its argument were deleted, leaving `#(`/stray-`)` parse errors in every stringizing log/assert macro |
| Conditionals | `#ifdef`, `#ifndef`, `#if` / `#elif` / `#else` / `#endif`. `#if` conditions get full constant-expression evaluation: the `defined X` / `defined(X)` operator is resolved over unexpanded tokens (C11 6.10.1p4), object **and** function-like macros expand (hide-set painted, depth-capped), and the result is parsed with C operator precedence (`?:`, `\|\|`, `&&`, bitwise, `==`/`!=`, relationals, shifts, arithmetic, unary `!`/`~`/`-`/`+`, parens). Integer literals accept `0x`/`0b`/octal prefixes and `u`/`U`/`l`/`L` suffixes; a number or character constant that is not an integer constant — a floating literal, or a C++ user-defined literal such as `10_km` or `'a'_x` — makes the expression malformed (gcc/clang reject it) instead of evaluating as if the suffix were absent; arithmetic models 64-bit intmax_t/uintmax_t with the usual arithmetic conversions (an operand mixed with an unsigned one converts to unsigned, so `-1 < 1U` is false; `>>` is arithmetic for signed, logical for unsigned; a literal is unsigned when suffixed `u`/`U` or too large for intmax_t). Identifiers surviving expansion evaluate to 0; malformed expressions (trailing tokens, unbalanced parens) conservatively skip the branch; per chain at most one branch activates. `\`-newline continuations inside conditions are spliced by the lexer like everywhere else. Conditions in skipped groups are not evaluated (and malformed `#ifdef` operands there are tolerated). Condition macro expansion runs under its own budget (64K tokens / 1M steps); exceeding it warns and conservatively skips the branch. `#elif` after `#else` warns and is ignored. Conditional groups are **file-scoped** in both directions: a group must be closed in the file that opened it, so at the end of every file (root or `#include`d) the frames it left open are reported as unterminated and popped, and the includer resumes in the state it had at the `#include`; and `#elif` / `#else` / `#endif` may only act on a group opened in the same file — in a header they never see the includer's frames, so a stray one is a `… without #if` error there even when the includer is inside an `#if` (before this, a header ending inside `#if 0` silently swallowed the rest of the translation unit, and a header starting with `#endif` consumed the includer's frame so the includer's own `#endif` failed — #8). A header that *stops early* (budget, malformed argument list) is rebalanced the same way but reports only its stop diagnostic, since its unprocessed remainder may still hold the `#endif`. |
| `#line` | Location tracking in `LineMap` |
| `#undef` | |
| Predefined | `__FILE__`, `__LINE__` — inside a macro body `__LINE__` is the line of the (outermost) invocation, C11 6.10.8.1, and both work in object-like as well as function-like bodies; builtin fallback macros for headers the indexed tree does not ship (see [Builtin fallback macros](#builtin-fallback-macros)) |
| Token spacing | No space before `)` / `]`; space between `>` and `&` / `*` so `operator()` and `shared_ptr<T> &p` survive re-lexing |

### P1 (planned)

- `#pragma once` / include-guard detection
- `__VA_OPT__` (C23)

### P2 (planned)

- `_Pragma`, additional standard predefined macros

## Builtin fallback macros

Code is indexed without a real toolchain, so macros whose definitions live in
headers the tree does not ship (gtest, Linux kernel headers, `<inttypes.h>`)
survive preprocessing, produce tree-sitter ERROR nodes, and can drop whole
functions from the index (`docs/PARSE_FAILURES.md` catalogs the impact on the
eval corpora). The preprocessor installs fallback definitions for the common
offenders:

| Macros | Fallback | Failure mode avoided |
|--------|----------|----------------------|
| `__UNUSED` | empty | `T &x __UNUSED` breaks the function definition |
| `__user`, `__iomem`, `__percpu`, `__rcu`, `__force`, `__init`, `__exit`, `__initdata`, `__exitdata`, `__read_mostly` | empty | kernel address-space/section annotations are syntax errors in declarators (`char __user *buf`, `int __init foo(void)`) |
| `PRI[diuxXo](8\|16\|32\|64)` | format-specifier string literal (e.g. `PRIu64` → `"llu"`) | `"%" PRIu64` leaves an identifier between string literals |
| `container_of(ptr, type, member)` | `((type *)(void *)(ptr))` | a type keyword in expression position; the fallback keeps the pointer flow and target type |
| `HWTEST(a, b, level)`, `HWTEST_F`, `HWTEST_P` | `static void a##_##b()` | gtest/OpenHarmony test macros followed by a body are unparseable, losing every test body in a file |
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

## LineMap

The preprocessor records mappings from **output byte offsets** to original `(file, line, col)` in `LineMap`.

**Current behavior:** tree-sitter parses **preprocessed** source; IR spans (`Span` in `trace-ir`) are resolved through the `LineMap` to original `(file, line, col)` — `#include`d code attributes to its header, TU-local code keeps its original pre-expansion position, and macro-expanded code attributes to the expansion site's origin (identical coordinates when nothing was expanded). Every token that comes out of a replacement list carries the `(line, col)` of the **outermost** invocation that produced it (`Token::origin`, set when the token is painted with its hide set and inherited through forwarding macros such as `#define WRAP(x) STR(x)`), while its own `line`/`col` keep the definition-site coordinates; the LineMap and `__LINE__` read the former. Argument tokens are not painted and keep their source position. Cached `#include` expansions store their own sub-`LineMap`, which is spliced back on replay so origins survive caching.

The `LineMap` must keep byte-accurate offset mapping when extending the preprocessor.

## Include resolution

For `#include "header.h"` / `#include <header.h>`:

1. Directory of the including file
2. Paths from `IncludeGraph.include_dirs` (discovered + `--include`)
3. Warning diagnostic `include file not found, skipping: <path>` if not found; the directive is skipped

Only **project-local** files under the analysis root are linked; system headers outside the tree are not resolved unless present in the project.

## Include graph and header indexing

| Behavior | Notes |
|----------|-------|
| `needs_preprocess` set | Files with `#include` edges (or included by another) run through the preprocessor |
| `source_cache` | Reuse file text while scanning `#include` edges |
| Reachable headers | Preprocessed file-locally, parsed/lowered **once** (PCH-style header IR), then merged into TUs |
| Orphan headers | Project `.h` never reached from any `.c` are indexed as their own units (may contain calls) |
| Parallel index | Header IR, orphan headers, and `.c` TUs: parallel parse/lower, sequential merge |

### Determinism

Indexing output must be identical across runs of the same tree. Two mechanisms guarantee this:

- **Macro warm pass** runs sequentially over TU-reachable headers in canonical (`index_order`) order, once per language the header is reachable in (C++ when a C++ TU reaches it or the extension is a C++ header spelling, C when a C TU reaches it; `PreprocessOptions::with_language` forces one language for everything). Each warm runs under a **fresh macro table** seeded only from command-line defines lexed in that language; the per-header final states are merged into a **per-language union table** — every warmed header lands in both unions, the same-language warm preferred and the other language's re-lexed for the destination as fallback (`MacroDef::relexed` spells the tokens back with their adjacency intact, which is all the two lexers disagree about), so each union stays the full macro superset (twin-guard dedup, orphan and PCH headers) whichever language reached a header — and each later phase hands a file the union (and option set) of the language it is lexed and parsed as. The first language warmed is the one the header is parsed as and feeds the source cache; a second only fills the expansion cache and its union. Reachability comes from the include graph and warming grows that graph (a `#include MACRO` is discovered only while preprocessing the header spelling it), so the pass runs to a **fixed point**: after each round the discovered edges are added, every header's language list is recomputed, and a header whose list changed — a `.h` first reached from C alone that a C++ unit turns out to reach through a macro include, and is therefore parsed as C++ — is evicted from the source cache and warmed again under a fresh table, so the text the parser sees is always the preprocess in the language it is parsed as. The root file of a preprocess run is never replayed from the expansion cache (a nested include may have cached it first); only nested `#include`s replay. Sharing one accumulating table across headers let include guards defined by earlier-warmed headers starve later headers' expansions (the starved text was then frozen into the expansion cache). Dedup between headers comes from the shared expansion cache, not from shared guard state.
- **Expansion-cache freeze**: the cache is keyed by (canonical path, language), so a C unit never replays a header's C++ tokenization or vice versa. During parallel phases the include-expansion cache is read-only (`PreprocessOptions::frozen_expansion_cache`). Hits replay warm-pass entries (produced deterministically); misses expand inline under each TU's own macro/guard state and are *not* inserted — first-writer-wins inserts would make results scheduling-dependent.

Translation units inherit the **union** of all warm-pass macro states: cached expansions replay without executing their `#define` directives, so TU-local code still needs those macros.

### Header IR (PCH-style)

Indexing sets `inline_include_bodies = false`. Nested cacheable `#include`s replay **macros and include-once state** but do not copy header tokens into the consumer's live output. Each header's preprocessed text is therefore file-local.

After the warm pass, reachable headers are parsed and lowered **once**. PCH order uses the include graph **plus preprocess `included_headers`** (macro includes the raw scanner misses). Independent leaves may run in parallel waves; a header is never in the same wave as a nested include it needs. Include **cycles** are not a parallel wave: leftovers are indexed in include-graph order so nested layouts stay visible. Nested `#include` IR merges **types and typedefs** from **direct** includes (plus this header's preprocess `included_headers`) so `struct StreamHost { struct IDeviceIoService service; }` sees `Dispatch`, and `GpioIrqFunc func` sees the typedef, without copying every descendant's functions/flow into ancestor units. Child PCH units already nested-merged grandchild types. Parallel isolation *without* those preprocess edges interned empty tags / `Int` and dropped field stores (`DeviceNodeExtDispatch` lost `DispatchToMessage`, `GpioOnDevEventReceive` lost `gpio->func`).

Headers that become reachable only after those preprocess edges are added join the PCH set (and leave the orphan path) so translation units can merge their prototypes.

Translation units parse only their own remainder and merge already-built header `UnitIndex`es for every header **reachable** via the include graph, plus preprocessor `included_headers` (a cached splice can omit a nested path from the graph edge, and types-only nested PCH does not copy nested prototypes into ancestors). That merge is **symbols only** (types + prototypes): header call sites and flow are already in the global program from PCH. Merge also rewrites leftover incomplete nested tags. That is the analogue of a PCH / clangd preamble. Merging only direct includes dropped `DispatchToMessage` from `DeviceNodeExtDispatch` (designated `.Dispatch =` in `hdf_wifi_core.c` when `sidecar.h` was not in that TU's `included_headers`).

Grammar follows the including language, not the extension alone: `.hpp`/`.hh`/`.hxx`/`.inl`/`.ipp` always use the C++ parser; a `.h` uses C++ if any C++ TU can reach it via the include graph, otherwise C. (Before PCH, header tokens were spliced into the TU and parsed with that TU's grammar, so `plugin.h` included from `plugin.cpp` was already C++.)

Standalone `preprocess_file` still inlines by default so a single-file expansion remains self-contained.

### Macro operations in cached entries

A cached expansion replays its text **without** executing the `#define`s it contains, so a header whose body *invokes* macros defined by an earlier-included header would starve: at warm time the dependency was processed inline (fine), but a consumer warmed later splices the dependency's cached body and never learns its macros. Therefore each `IncludeExpansion` records the **ordered log** of `#define` / `#undef` directives its processing executed, nested replays included (`IncludeExpansion::ops`). An ordered log rather than a table diff: a diff cannot represent a no-op `#undef` (name absent at capture, defined in a later consumer) or an undef-then-redefine of a name present at both boundaries.

`splice_cached` replays the log through the same mutation helpers live directives use, so a cache hit and a cache miss agree on everything a directive touches: the local table (a replayed `#define` overwrites, like live execution), builtin-fallback marks, the shared table under `accumulate_macros`, and the op log feeding an enclosing cached header's own entry.

### Cache self-containment

Cached expansions are flat text — nested `#include`s inside an entry were already resolved when the entry was built. An entry built while a nested header was already in this run's include-once set would otherwise freeze *without* that header's content, permanently hiding its definitions from every consumer routed through the entry.

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
- `#warning` / `#error` (partially recognized)
- Full C11 macro prescan/rescan semantics
- System include paths outside project tree (unless copied into tree)

## Testing

- Unit tests: `trace-preproc/src/`
- Integration fixtures: `tests/fixtures/preproc/` (including `empty_left_paste.c` for empty left `##` operands — stringized spacing, non-stringized literal/punctuation preservation and the GNU `, ## __VA_ARGS__` elision an empty parameter must not shadow, `self_ref_macro.c` for C11 hide-set / X-macro lists, `include_macro.c` for `#include FOO`, `unterminated_if_include.c` + `unterminated_if_header.h` for an `#if` left open by a header, `stray_closer_include.c` + `stray_{endif,else,elif}.h` for closers that would otherwise act on the includer's frame, `stringize.c` for `#param` in log/assert-shaped macros, `raw_string.cpp` for C++11 raw string literals in the shapes the corpora use and `raw_string_shapes.c` for the same text as valid C, where `R` and the would-be ud-suffix are macros that must still expand; the `\`-newline splice cases are unit tests, checked against gcc/clang)
- Builtin fallback fixtures: `tests/fixtures/builtin_macros/` (`kdriver.c` for the
  kernel/driver table, `hwtest.cpp` for the gtest/OpenHarmony test macros and
  `gmock.cpp` for the gMock declaration macros in their modern, legacy,
  `_WITH_CALLTYPE`, comma-protected, pointer-to-member and
  wrapped-across-lines forms)

See [ARCHITECTURE.md](ARCHITECTURE.md) for how preprocessing fits the full workflow.
