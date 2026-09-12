# Changelog

All notable changes to `trace` are documented in this file.

## Unreleased

### Added

- Compilation database support (`compile_commands.json`, #62): automatically reads `compile_commands.json`
  at the analysis root, then `build/compile_commands.json`, or an explicit `--compile-commands PATH`.
  Each entry supplies its working directory, ordered `-I`/`-iquote`/`-isystem` search paths, ordered
  `-D`/`-U` macro operations, forced includes (`-include`), and language/standard selection (`-x`, `-std`).
  Multiple commands for the same translation unit are all indexed and their facts are preserved via
  variant-aware merging (`merge_unit_variants`). Files without a database entry retain inferred
  configuration without requiring a compilation database.
- Bounded conditional-variant exploration with fact unioning (#59): `--explore` and `--explore-budget <N>`
  (default: 4) recover platform and feature implementations excluded by the single default
  configuration without requiring a build. Feasible variants are derived from in-tree GN define
  candidates (`BUILD.gn`, `*.gni`, `*.gn`) and verified via semantic preprocessing against condition
  expressions (`trace_preproc::preprocess_string`). Feasible variants are preprocessed and lowered
  independently (never concatenating arms into a single stream). A variant-aware merge
  (`merge_unit_variants`) preserves calls, parameters, and flows across variants sharing
  `(file, name, line)` rather than dropping later bodies wholesale, and unions aggregate layouts by
  field name. Variant parameters are paired with the base signature *by name*, and a parameter a
  variant adds is recorded as a local: the canonical arity stays that of the base configuration,
  which the solver reads as the arity of an indirect-call target. Conditional chains are identified
  by `(file, line)`, since a translation unit spans every header it expands. Two defines that open
  the *same* arm can share one variant when their combined definitions preserve all target
  conditions, including earlier-arm precedence for `#elif`. Activation candidates come from the
  whole chain prefix, not from the target arm alone: an `#else` arm reads no macro of its own, so
  drawing them from that arm would leave the `#ifndef FOO / #else` shape unexplored. Calls whose
  arguments change across variants retain separate facts even at the same source location.
  Arms inside an excluded region
  (C11 6.10.1p6) are explored last, after every arm a define really does open. Budget exhaustion is
  reported with stage `"explore"` as a count of omitted candidate activation goals. The
  solver's name-based GEP fallback is gated on the number of
  variant units actually merged rather than on the flag, so a run that asks for exploration and
  generates no variant keeps baseline semantics exactly: `--explore --explore-budget 0` reproduces
  baseline analysis facts (run metadata records the requested options), and `--explore` is off by default.
  Locals are recorded on their function by the merge — lowering leaves the field empty — so a
  variant's body pairs with the base's rather than re-allocating every local, and synthesized
  temporaries, which lowering names after the unit-local id it just allocated (`_gep6` versus
  `_gep772`), pair positionally instead of by name. Without both, a configuration-independent
  call was recorded once per variant and raw edge counts read the repetition as coverage.
  A variant definition of a function the base spells on a *different* line — the
  `#ifdef X / #else` shape this feature targets — extends the base entry instead of registering a
  second definition, which would overwrite the surviving span and parameters and drop the call
  sites bound to them; a name several defined functions in one file share (C++ overloads) does not
  identify a function and keeps the line-keyed behavior. Placement re-checks an arm only when the
  added define can reach it, decided from the identifier closure of its condition through macro
  aliases, and evaluates each condition against only the macros it can reach, so the search is
  linear in candidate arms rather than quadratic.
- Reduce exploration bookkeeping and synthetic predicate mapping work; preserve GN
  discovery through non-UTF-8 directory names. Borrow preprocessor token text during
  rendering and use a guarded direct lookup for dense call-site IDs.
- GN define evidence in conditional coverage reports (#58): candidates from `BUILD.gn`,
  `*.gni` and `*.gn` retain values, source locations, enclosing conditions, and confidence.
  Reports rank the evidence without applying inferred defines or claiming per-TU accuracy.
- Source revision, dirty-state, and build-date metadata in `trace --version` and exported databases.
- Explicit database schema-version metadata.
- Validated, version-tagged GitHub releases alongside the rolling `master-latest` prerelease.
- OpenHarmony IPC proxy-to-stub call edges for matching `SendRequest` methods.
- Dependency roots (#60): a repeatable `--dep <PATH>` names a tree the target builds against but
  that is not the code under analysis — a vendored SDK, a framework checkout, a sibling repository.
  Its headers are discovered and merged for what they *declare*: types, class definitions,
  inheritance, prototypes and declared return types, including a class's `operator->` return, which
  is what lets a wrapper-typed receiver (`sptr<CaptureSession> s; s->AddOutput(...)`) resolve on the
  wrapped class instead of falling back to an edge on the wrapper. Nothing a dependency *defines*
  crosses over: its sources are never translation units, its headers are never indexed as standalone
  orphan units, and a body written in one contributes no call site, local variable, value-flow
  constraint or return flow — such functions merge as declarations (`is_defined = 0`). Bodies and
  variable initializers are skipped during lowering, avoiding discarded IR and preventing flow
  through parameters, globals, or function-local statics from leaking into the target. Files and
  functions from a dependency root export with `is_dep = 1`, and `trace inspect calls --exclude-deps`
  drops the edges that touch them.
- Conditional-compilation coverage report (#57): with `PreprocessOptions::record_conditionals`
  the preprocessor records every `#if` / `#ifdef` / `#ifndef` chain it meets — the condition as
  written, each arm's line range and whether it was taken, skipped or never evaluated, and the
  names the evaluation consulted with whether each was bound. The `conditional_coverage` example
  measures a tree under the indexer's per-unit environment and classifies each name (include
  guard / toolchain / configuration / unknown); `scripts/gen_conditional_coverage_report.py`
  renders `docs/CONDITIONAL_COVERAGE.md` for the eval corpora. The record is off by default, and the
  one preprocessing change is that a `#` followed by something other than a directive name (a line
  marker, a `#!` line) inside a skipped group no longer aborts the file. Recording retains
  explicit macro operands with operator-shaped names and physical directive/EOF line ranges.
  A completion record prevents truncated TSV captures from producing partial reports.

- `->` through an undeclared smart-pointer wrapper resolves to its argument class (#86). A template
  spelling whose class body is not in the tree keeps its arguments in its tag, and an arrow on it
  with exactly one argument naming a declared class looks the member up on that class with the usual
  hierarchy fan-out, on locals, parameters and fields alike: OpenHarmony's `sptr<T>` / `wptr<T>` and
  namespace-qualified spellings resolve without their header. A wrapper whose body is in the tree
  still goes through its own `operator->` (a forward declaration alone does not count as one), `.`
  stays on the wrapper, and two or more arguments, a scalar, pointer or reference argument or an
  unknown class leave the site unresolved instead of inventing a member on the wrapper. The argument
  is looked up through the enclosing namespaces innermost first and then the global scope, on bare
  and partially qualified spellings, a typedef standing for the class it names and a trailing
  `const` ignored, and a leading `::` naming the global class and nothing else; `using namespace`
  directives are not searched, because an out-of-line member defined under one is indexed under the
  bare class name and only the bare spelling reaches that body. `(*p).m()` on such a wrapper is the
  same guess as `p->m()`. A nested type of such a template (`Outer<A>::Inner`, an iterator) keeps
  its tail on the tag and is neither guessed through nor given an invented member; a scalar,
  fixed-width integer or function-type argument is spelled as written rather than qualified to the
  enclosing namespace (`missing<int>` inside `namespace N` no longer interned `N::missing<N::int>`).
  The wrapper's own name is looked up through the enclosing namespaces the same way, so `sptr<T>`
  spelled inside `namespace OHOS::CameraStandard` is `OHOS::sptr` and keeps its declared
  `operator->` and its `.` members instead of reading as an undeclared
  `OHOS::CameraStandard::sptr`, and `::sptr<T>` is the global `sptr` tagged without the prefix; a
  defined class template spelled with its arguments takes the class that lookup found as its tag
  (camera's `BlockingQueue<std::any>` inside `DeferredProcessing` is the `CameraStandard::BlockingQueue`
  its header includes, not a same-named class the header never saw), and a member type of one
  (`BlockingQueue<std::any>::Iterator`) keeps that class as its prefix. A member type spelled with
  its own arguments (`Outer<A>::Inner<B>`) keeps its `::` and its own name even when an unrelated
  class shares that name. `nullptr_t`, `intmax_t`, `uintmax_t` and `auto` are never qualified to a
  namespace, nor is a literal argument (`Buffer<1024>` inside `namespace N` is not `N::Buffer<N::1024>`);
  `T*const` reads as the pointer argument `T*`, and every pointer level survives a qualifier between
  them (`T * const *` is `T**`). A `::`-prefixed head is handled in one place, so a declared type
  spelled `::N::Defined<int>` is the same class as `Defined<int>` with its layout. A class is never
  recorded as its own base.
  A typedef is registered under its qualified name as well as its bare
  one, and a template argument is matched against it whole: `A::B::T` never lands on an unrelated
  namespace's `T`. An out-of-line `operator->` whose class header is not in the tree records its
  return type under the class its name spells, so the arrow follows the declared return rather than
  being dropped. `sp->f` reads and writes the pointee's field through a wrapper -- declared,
  standard or out of tree -- as `sp->m()` already reached the pointee's member. Camera's direct
  edges rise 21,059 -> 37,646 and its 254 `OHOS::sptr::*` phantoms are gone.

### Changed
- `TypeTable::intern` and `intern_ref` answer an empty named tag from the tag map (#86). Canonicalizing
  such a tag cloned the richest layout under that name into it and then looked the clone up, when
  the tag map already holds the id that lookup lands on; merging re-interns every header tag per
  unit, so the clone was paid once per included header per unit. Output is byte-identical.
- A unit merge remaps type ids through a dense `Vec` instead of an `FxHashMap` (#86): a source
  table's ids are a dense index into it.

- Database schema is now **v2**: `analysis_run` carries a `schema_version` column. Databases
  written by earlier versions have no such column and report themselves through the existing
  stale-schema errors.
- Database schema is now **v3**: `call_edges.call_site_id` is nullable for synthetic edges and
  `call_edges.caller_fn_id` records their caller independently of a source call site. Inspecting
  call data from an older database reports an actionable re-analysis message.
- Database schema is now **v4**: `files` and `functions` carry `is_dep INTEGER NOT NULL DEFAULT 0`,
  separating dependency-root entities from the target's own, and `analysis_run.options_json` records
  `dep_roots`. `trace inspect calls --exclude-deps` against an older database reports an actionable
  re-analysis message rather than silently returning unfiltered edges.

- Include resolution no longer canonicalizes a path that cannot exist (#83). #62's source-cache
  probe ran `std::fs::canonicalize` under the canonical key so a virtual header could be found, but
  `resolve_include` builds a fresh includer-relative candidate for every (including file, spelling)
  pair, so the memo in `trace_ir::canonicalize` almost never hit and each miss paid a `realpath` —
  338 of 1338 sampled `process_tokens` frames on HDF. Reaching the probe means `is_file` said no,
  and `fs::canonicalize` needs every component to exist, so it could only fail there and fall back
  to the path as given; the probe now uses that path directly. HDF preprocessing drops from 5.0s to
  2.6s and its index from 9.0s to 5.7s; camera from 11.2s to 6.7s and 24.0s to 16.8s. Applied on
  its own this leaves all three corpora byte-identical to `7fee472`; the release as a whole does
  move them, through the `extern "C"` fix below.
- Indexing reports the two preprocessing passes separately (`preprocess-done: Xs (Ys serial
  discovery + Zs settle of N of M units)`), so the serial discovery pass is attributable on its
  own. It is the dominant cost of the phase — 2.2s of 2.7s on HDF, 5.5s of 7.0s on camera — and
  the three changes below cut it to 1.1s and 1.8s without touching its shape, so the redesign that
  would let it run in parallel is still open (#55).
- Include existence is probed once per path, not once per `#include` that names it (#83).
  `resolve_include` builds an includer-relative candidate for every (including file, spelling) pair
  and probes it on every include a unit executes, and the tree under analysis is read-only for the
  whole run, so `trace_ir::is_file_cached` memoizes `Path::is_file` per thread. Camera probes
  4,343,229 candidates naming 224,945 distinct paths — 95% of the probes were repeat `stat` calls,
  and they were 60% of the serial discovery pass. HDF probes 1,445,203 naming 135,227; hiview
  1,546,148 naming 136,908. The memo is keyed on the path's encoded bytes rather than on a
  `PathBuf`, because `Path`'s own `Hash` walks components and normalizes separators as it goes,
  which on these paths costs more than hashing them flat.
- The preprocessor's maps are hashed with `rustc-hash` instead of SipHash (#83), including the
  shared include-expansion cache and `IncludeGraph`'s source cache and basename index. Path keys
  dominate them, and after the probe memo above, hashing paths was the largest single cost left in
  discovery. Nothing in the pipeline reads a hash-map iteration order: `std`'s `RandomState` is
  seeded per process, so any output that depended on one could not have been reproducible run to
  run in the first place — and every corpus stays byte-identical through this change.
- A source cache built by reading files off disk no longer costs a second probe per include (#83).
  `include_exists` consults the source cache only for a candidate the filesystem rejected, which a
  cache of on-disk files can never rescue, so `PreprocessOptions::source_cache` is now a
  `trace_preproc::SourceCache` that answers which of its own keys name no file (computed on the
  first candidate that reaches it) and the probe stops at the memoized `stat` when none does. The
  indexer's cache qualifies by construction; a caller that seeds a header the filesystem does not
  have still resolves it. This probe was 17% of camera's serial discovery pass. Deriving the answer
  inside the cache rather than beside it is deliberate (review): a `bool` on the options next to a
  separately assignable `source_cache` field can go stale, and the failure it produces is a bare
  `include file not found`.
- `is_file_cached` memoizes only within one indexing run. `build_program_with_jobs` calls
  `trace_ir::start_file_probe_epoch` before it reads anything, so a host that indexes repeatedly in
  one process (the C API does) sees the tree as it is at the start of each run rather than as the
  previous run found it, and the memo's memory is reclaimed with the epoch (review).
- Together, on `--jobs 8` (median of two runs): HDF preprocessing 2.7s to 1.35s (discovery 2.2s to
  1.1s), index 6.0s to 4.3s, wall 7.9s to 5.9s; hiview 1.9s to 0.75s (1.6s to 0.5s), 4.6s to 2.9s,
  5.0s to 3.2s; camera 7.0s to 2.4s (5.5s to 1.8s), 18.0s to 10.8s, 18.7s to 11.6s. All three
  corpora stay byte-identical to the branch without these changes and bit-reproducible over three
  `--jobs 8` runs and one `--jobs 1` run, and the eval checker stays at 90 checks, 0 failures.
- The directory walk behind `#include` is shared across preprocessing runs (#83). Step 2 of
  `resolve_include` was memoized per (spelling, quoted) only for the rest of one file's
  preprocessing, so every unit that reached a guarded header re-probed the same search lists.
  `SourceCache` now keeps those results (hits and misses) keyed by the ordered quote, include and
  system paths, so units preprocessed under the same configuration share one walk; the
  includer-relative candidate is still probed first on every lookup, and basename fallback stays
  with the caller because it depends on its own index and strictness. The map expires with the
  file-probe epoch. `TypeTable`'s interner and alias map hash with `rustc-hash` too, since header
  merging re-hashes nested descriptors and typedef names on every unit; `IndexMap` keeps insertion
  order so type ids do not move. The parallel index phases also process units in batches of four per worker and release
  each unit's cached source once it is lowered, so the live per-unit text is bounded by the batch
  rather than by the corpus; merge order stays that of `file_order`. On `--jobs 8`, against the
  branch without these changes: hiview wall 3.1–3.3s to 2.9s, camera 10.8–11.1s to 9.9–10.3s; HDF
  moves within run-to-run noise (5.6–6.5s to 5.5–6.2s). All three corpora stay byte-identical to
  that branch and bit-reproducible over three `--jobs 8` runs and one `--jobs 1` run.
- Six more costs taken off the index (#83, follow-up). Synthesizing external callees rescanned
  every call site once per synthesized name: quadratic, and 1.5s of camera's index on its own;
  the sites each name may claim are gathered once. The include graph's resolver probed each
  candidate with its own `stat` -- an include naming no project file walked all 205-291 search
  directories, once per file that spelled it -- which was ten seconds of kernel time on camera at
  any job count; it now goes through the run's probe memo, and a first probe is answered from the
  parent directory's listing when that settles it (a name the directory does not hold is a miss
  without a `stat`; a listed, case-folded or non-ASCII name still asks the filesystem, so a
  case-folding or normalizing filesystem answers as before). `TypeTable::intern` asks the table
  before rebuilding or cloning a descriptor whose canonical form cannot differ, canonicalizes in
  place, and merging interns by reference (`intern_ref`), so a header type re-interned per unit
  costs a lookup. The lowering path's path-keyed maps, the analysis PAG's index maps and the
  solver's location sets hash with `rustc-hash` (insertion order is what `IndexMap` iterates, so
  nothing observable moves). And the parallel index phases merge one batch while the pool parses
  the next, so the serial merge (0.35s on camera) overlaps parsing instead of stalling it. On
  `--jobs 8`, against the branch before this round: camera 9.9-10.3s to 6.1-6.4s, HDF 5.5-6.2s to
  4.0-4.7s, hiview 2.9s to 1.7s; camera's system time 10.4s to 1.3s. All three corpora stay byte-identical
  and bit-reproducible over three `--jobs 8` runs and one `--jobs 1` run; eval 90/90.
- Removed avoidable scans on the indexing path (#83): `LineMap::truncate_at` cuts at a
  `partition_point` instead of walking every entry with `retain` (the preprocessor cuts there after
  every cacheable nested include, on a map holding one entry per emitted token); the symbol table's
  redeclaration merge reaches an entry through its O(1) `fn_slots` slot instead of scanning
  `functions`; the cross-TU merge indexes a unit's parameter types once instead of scanning
  `unit.variables` per parameter; the include-graph topological order finishes a cycle through a
  set rather than an O(V^2) `Vec::contains`; and `deps::resolve_include` probes its candidates
  lazily instead of first allocating one `PathBuf` per search directory (205-291 of them on the
  corpora) for every include. These remove real quadratic and linear paths but are not what moves
  the timings above.

### Fixed
- A C++17 `namespace A::B {` definition opens two scopes (#86 review). tree-sitter spells the name
  as one `nested_namespace_specifier`, which the lowering did not look for, so the block read as an
  anonymous namespace and everything inside registered under the bare name with internal linkage.
  83 hiview files and 62 camera files open a namespace that way; hiview's external functions fall
  3,623 -> 3,191 as bare-named prototypes fold into their qualified definitions. A C++20
  `namespace A::inline B {` names its inner scope `B`, not `inline B`.

- An unresolvable parameter type no longer matches every type in the symbol table's overload check
  (#83 review). `same_type_shape` treats `TypeDesc::Unknown` as a wildcard, which is right for the
  variant merge it was written for — that path takes a candidate only when exactly one matches, so
  an ambiguous answer falls through to the ordinary merge — but `register_function` takes the FIRST
  compatible candidate out of an overload bucket. A C++ prototype whose parameter type no header in
  the include path declares therefore absorbed `f(int)`, and then `f(double)`'s body landed on the
  same entry, so callers of one overload resolved into another's body. The exact-`TypeId` check
  this branch replaced could not do that, since `Unknown` has one id. `same_param_type` now treats
  an unresolvable type as a type — matching only another unresolvable one, at every depth — and
  `same_param_type_or_unresolved` is the spelling the variant merge asks for. All three corpora are
  unchanged, so the fault was latent there.
- `IncludeGraph::index_order` no longer appends a duplicate cyclic leftover twice (#83 review).
  Replacing the O(V^2) `order.contains` with a set snapshot dropped the growth the linear scan had:
  `contains` was re-evaluated as `order` grew, the snapshot was not. Every in-tree caller dedups
  `files` first, so this was latent, but the function is `pub`.
- `scripts/eval_expected.json`'s hdf `arg_flow_rows_per_call_edge` probe pinned 65960 while the
  global `arg_flow_edges` it counts the same rows as moved to 65961 (#83 review). Both measure
  65961; the probe passed on tolerance alone.

- A C++ prototype spelling a parameter as an array reaches its definition (#83 review). `int a[]`
  and `int *a` declare the same parameter, but they interned as `Array` and `Ptr` and the overload
  check read them as two functions, so callers kept an `external` edge to the undefined prototype —
  the same symptom as the tag-completeness split, from a different cause. `same_param_type` now
  decays a top-level array to a pointer; nested it does not, since `int (*)[10]` and `int **` are
  different types. Pure C never showed this, because a C prototype and its definition collapse
  without consulting parameter types.
- Anonymous tags no longer all compare as one type (#83 review, corrected in second review).
  Tags compare by name, so every anonymous aggregate matched every other one and unrelated
  parameter types folded together. They now compare structurally, and a named tag never matches
  an anonymous one. The first attempt tested the name for emptiness, which no lowered type ever
  satisfies: `lower_tag` names an unnamed struct or union `anon_<n>` off a counter each unit seeds
  from the program's, and `extract_tag_name` falls back to a bare `anon`. So the check was dead
  code, and the name comparison it was meant to replace stayed in force — two units both numbering
  their first anonymous tag `anon_1` had unrelated types compare equal, while one shared tag
  numbered `anon_1` in one unit and `anon_7` in another compared unequal. Anonymity is decided by
  that prefix now, the way `lower.rs` already reads it. One reader of the prefix in `lower.rs` had
  not caught up: the constructor-call check tested `starts_with("anon_")` without the digits, so a
  tree's own `struct anon_vma v(x)` emitted no constructor call; it asks `is_anonymous_tag` now.
  Latent on the corpora.
- A nested array's extent is part of its type again (#83 review). The structural comparison
  ignored `size`, so `int (*)[10]` and `int (*)[20]` matched and distinct overloads collapsed.
  Stated bounds must now agree; an unstated one still matches any. The decay stays top-level
  only, and it now covers array-against-array as well as array-against-pointer, because a
  parameter's own bound is discarded — `int a[10]` and `int a[20]` declare one function.
  The extent check cannot fire on lowered input yet: `walk_declarator_shape` is the only place
  that builds a `TypeDesc::Array` and it hardcodes `size: None`, so every lowered array is
  unbounded and two of them always compare equal. It guards the descriptor's contract rather
  than a reachable fault, and becomes live the day lowering reads a bound.
- An array parameter of a function-pointer type decays too (#83 review). `void (*)(int a[])` and
  `void (*)(int *a)` are one type — a parameter list decays wherever the language spells one, not
  only at the outermost level — but the `FnPtr` arm compared its parameters with the plain shape
  rule, so a callback prototype spelling an array rejected the definition spelling a pointer. The
  return type is not a parameter and still does not decay.
- Anonymous-tag detection no longer claims ordinary tags (#83 review). Requiring only the `anon_`
  prefix caught `anon_vma`, `anon_inode` and any other real tag spelled that way, and anonymous
  tags compare structurally, so any two such tags sharing one field became the same type — the
  inverse of the fault the predicate exists to fix. The digits are now required, and the test is
  applied to the leaf: a C++ anonymous class is registered qualified (`ns::anon_1`), which the
  whole-name test called named, so two units numbering one tag differently could never match.
  The test that was meant to cover this used `anonymizer`, which does not even carry the prefix.
- A parameter whose variable did not lower no longer blocks the merge (#83 review). `merge_unit`
  fell back to `TypeId(0)` for such a parameter; zero is the first descriptor the prelude interns,
  `Void`, and no parameter has that type, so the fallback guaranteed a signature mismatch. It uses
  `Unknown`, which is what the comparison already documents for a type neither side can resolve.
- A pointer typedef to a struct keeps its pointer (#83 review). `typedef struct Session *SessionPtr`
  registered the alias as the bare tag without walking the declarator, so every `SessionPtr s` was
  a struct value and `s->fd` decomposed against a non-pointer. Only the one-statement form was
  affected; `typedef struct S S;` followed by `typedef S *P;` takes the branch that walks it.
- `LineMap::truncate_at` no longer narrows its argument to `u32` before comparing (#83 review).
  Offsets beyond `u32::MAX` wrapped to a small cut point and truncated the whole map; it widens
  the entry instead, matching `slice_from`.
- A pure-C definition lands on the overload it shares a signature with (#83 review). A `.c` body
  and a C++-parsed header prototype are matched on arity alone, because the body's parameter types
  are decayed and would not compare equal. Widening the candidate search to the whole
  `externals_by_name` bucket made arity ambiguous: with two same-arity C++ prototypes under one
  name the definition merged into whichever registered first. Candidates are now tried once
  demanding the signature agree as well, falling back to arity only when none does — so this
  decides which arity-compatible candidate is taken, never whether any is.
- The internal-linkage merge moves `param_type_ids` with the parameters it adopts (#83 review).
  Nothing matches `static` entries by signature, so a stale cache there was latent, but
  `adopted_params` is what tells `merge_unit` to remap the entry, and the two branches encoded
  different cache semantics.
- The reported call path is guarded by `cargo test` alone (#83 review). A `.c` caller reaching a
  `.cpp` definition through one `extern "C"` prototype whose parameter tag is complete in the C
  unit and opaque in the C++ one — the shape of `hdf_remote_service.c:68` — had only the corpora
  and unit-level mocks behind it. `c_caller_reaches_a_cpp_extern_c_definition_across_units` builds
  that tree end to end; under the previous exact-`TypeId` comparison it splits into a prototype
  and a body, as the corpora did.
- A C declaration no longer clears a C++ definition's `is_cpp` (#83 review). Dropping `is_cpp` is
  how a C *definition* merging into a C++-parsed header prototype keeps a later TU from refusing
  its body, but any C-parsed redeclaration did it, stripping a C++ body's overload identity so an
  unrelated overload could merge into it afterwards.
- A definition keeps its own parameters, and its cached parameter types, when a later prototype
  merges into it (#83). Both follow from one rule the redeclaration merge did not state: a
  surviving entry's parameter list and the `param_type_ids` describing it belong together, and a
  merge that does not hand over a new list must not touch either. `merge_unit` used to infer
  whether the survivor had taken the incoming list by comparing its `params` to the list just
  passed in, which is not proof — unit-local ids are small and can equal an unrelated list of
  already-global ids by coincidence — and the entry was then remapped as though it had adopted,
  replacing a definition's parameters with the prototype's variables and cutting the body off from
  the parameters its flow facts reference. The cached types were overwritten unconditionally on
  every merge, so a shape-compatible prototype could leave a definition described by the
  prototype's ids; since a pair of definitions compares by exact id, that both split a repeated
  definition in two and collapsed two genuinely distinct bodies into one. `register_function` now
  reports adoption directly (`FnRegistration::adopted_params`) and the cache moves only with the
  list it describes, so the rule lives where the decision is made rather than being mirrored in the
  caller. No corpus reaches any of these collisions — applied on their own these two fixes leave
  all three corpora byte-identical to `7fee472` — so they are latent faults, pinned by four tests: a prototype must not take a definition's parameters, a
  definition merging into a prototype must have the list it hands over remapped, and a later
  prototype must neither split a repeated definition nor collapse two distinct ones.

- A C caller reaches its `extern "C"` C++ implementation again (#83). The C++ overload check in
  `SymbolTable::add_function_with_param_types` compared parameter types by `TypeId`, but a `.h`
  prototype and the `.cpp` definition of one function reach the symbol table from two different
  units, and the same C type is interned twice whenever those units disagreed on how complete a
  nested tag was: HDF's `struct HdfRemoteService *` split in two because the defining unit had not
  seen `struct HdfObject`'s fields, so the prototype read as an overload of its own definition and
  every C caller of the IPC interface stopped at the undefined prototype
  (`hdf_remote_service.c:68` no longer reached `hdf_remote_adapter.cpp:469`). Resolved parameter
  pairs now compare by *shape* — the comparison `merge_unit_variants` already used for the same
  reason, moved to `trace_ir::same_param_type` so `merge.rs` and `symbol.rs` share one copy — while
  a pair of *definitions* keeps the exact id comparison, because one qualified name legitimately
  carries two bodies (camera declares the same class in unrelated fuzzer targets and puts a test
  mock beside the production implementation) and folding those together would evict a body's facts.
  Candidate search scans the undefined prototypes in `externals_by_name` so a definition does not
  miss its declaration when another overload was registered after it, and `fn_by_name` prefers a
  defined function so a later declaration cannot shadow a body. Tree-sitter C++
  `optional_parameter_declaration` nodes retain default parameters in prototypes to prevent arity
  mismatches against definitions, self-typedefs (`typedef struct Foo Foo;`) register so forward
  struct pointer types do not degrade to `Int`, and a leading `::` is normalized in type shapes. On
  the corpora the count of names carrying both a definition and a declaration-only entity falls
  from 10 to 0 on HDF, 40 to 17 on hiview and 159 to 40 on camera, moving 867 camera call sites off
  declaration-only entities; the residue is real overload sets where one member is declared and
  another defined, which is not the same fault. Every exact metric (`diagnostics`,
  `edges_indirect`, `edges_ipc`, `dlsym_edges`, `files`) and camera's defined-overload-group count
  are unchanged, and all three corpora stay bit-identical across three runs and at `--jobs 1`.

- Diagnostic deduplication keys on the reporting stage as well as the origin, so a
  report from one stage no longer stands in for a different stage's report of the same
  text at the same position, and a report's origin is registered even when it is kept
  unconditionally — otherwise a configuration variant re-lowering the same code
  reported everything the base configuration had already reported.
- Unioning an aggregate layout lets the named-tag maps reconsider the type it mutated
  in place, which they previously kept pointing past.
- The GNU `, ## __VA_ARGS__` comma rule is decided from the macro body (#65), not from the last
  token already emitted: the form is a `,` spelled immediately before the `##` with the variadic
  tail parameter right after it. Reading the emitted token asked a different question, and a
  parameter that substituted to whitespace changed its answer — `CALL(o,)` and the same call with
  the empty argument spelled as a newline expanded two different ways (`g(o)` against `g(o ,)`),
  one following clang and the other gcc. Both spellings now expand alike. A parameter standing
  between the comma and the operator is no longer treated as that form either, so
  `#define F(v, x, ...) g(v, x ## __VA_ARGS__)` invoked as `F(1,)` keeps the separator it was
  deleting (`g(1, )`, gcc's reading) instead of emitting `g(1)`. The two sites that used to
  decide this collapse to one: reading the body, the placemarker branch's exception cannot hold,
  since the token before that `##` is the parameter being placemarked. The index is
  byte-identical on all three eval corpora.
- A repeated `#include` of a path is suppressed only on a reason the file itself stated (#56).
  The preprocessor used to skip any path it had already processed in this run, before looking at
  include guards at all, so every deliberate re-inclusion was lost — an X-macro table included
  once per `#define` of its entry macro contributed only its first expansion, silently and
  without a diagnostic. A file is now skipped when an include guard wrapping it is defined
  (`#undef`ing that guard and including the file again re-expands it, as in cpp), or when a
  `#pragma once` was reached with the enclosing conditionals active — which then holds for the
  rest of the translation unit, whatever later happens to the controlling condition. The guard is
  read off the token stream before the body runs, so a header that reaches itself back through
  another header answers for that inclusion too; guarded recursion still terminates on the guard
  rather than on the include-depth cap. Guard-driven skips keep feeding the cache frames, so a
  diamond include graph costs one expansion per header as before, and the guards a cached
  expansion learned travel with it for the files it actually covers. Genuine repetition is bounded
  by `max_file_expansions` (64 inclusions of one path per run), which reports rather than loses.
  A header a unit includes twice under different macros now contributes both of its expansions to
  that unit instead of only the last. The index is byte-identical to the previous release on all
  three eval corpora, at unchanged index time and peak RSS.
- A `#pragma once` header embedded in a cached expansion carries its guard to the consumer along
  with its text. Recording the guard returned early once `#pragma once` was already known for that
  path, which kept it out of every cache frame opened afterwards, so an entry could hold such a
  header's body without the reason to skip it and the consuming unit expanded it a second time.
- The preprocessor predefines what the language implies (#70): `__cplusplus` (`201703L`) in a
  unit lexed as C++, `__STDC_VERSION__` (`201710L`) in a C unit, `__STDC__` in both. A
  command-line `-D` of the same name outranks the predefined value and a source `#undef`
  removes it, since these are ordinary definitions rather than builtin fallbacks. Previously nothing
  was predefined, so every `#ifdef __cplusplus` / `#if __cplusplus >= …` in a C++ unit took the
  C arm and the declarations it guarded were silently missing from the index. A header shared
  by a C and a C++ unit reads the name bound in one and unbound in the other, so each unit's
  expansion stays its own. `docs/CONDITIONAL_COVERAGE.md` records the effect on the eval
  corpora: `__cplusplus` was read by 930 chains with every evaluation unbound, and HDF's
  always-excluded line count falls from 7,302 to 6,980.
- `x->m()` resolves the receiver through the **declared** `operator->` rather than a hardcoded
  list of smart-pointer names (#64). A wrapper's instantiation keeps the wrapper as its class,
  and a `->` on it looks the member up on what its `operator->` returns: for a class template,
  the argument at the declared parameter's position (OHOS `sptr<T>` / `RefPtr<T>` and HDI
  `AutoPtr<T>` need no list entry); for a named class, that class, followed on through a chain
  up to eight links deep with cycles cut; for a raw pointer, its pointee. `*w` on a wrapper is
  the same pointee. Previously a wrapper outside the three listed names kept its own class as
  the receiver and had a member **invented** on it — `sptr::AddOutput` is an edge to an
  undefined external function, indistinguishable downstream from a real call out of tree —
  while a listed one was unwrapped on the *type*, so `sp.reset()` bound to the pointee's
  `reset`. A `.` call now stays on the wrapper, and where nothing names a class (overloads that
  disagree, a return type the index cannot name, a cycle) the site is left unresolved instead.
  The name list survives only as a fallback for a wrapper whose class is absent from the index
  (`std::shared_ptr` with its header out of tree), a guess from a name rather than a
  resolution. What an `operator->` returns is carried as a fact alongside a header's types, so
  a wrapper-typed field declared in a different header from the wrapper resolves too. A member
  prototype declared in a class body now also carries its declared return type, which the merge
  keeps over the definition's; it used to be a `void` placeholder.
- The preprocessor lexer now runs translation phase 2 (`\`-newline splicing) before it
  recognizes tokens, so an identifier, a multi-character punctuator, an encoding prefix or a
  string body written across a line splice is one token. `#define F(x, .\`-newline-`..)` is
  the variadic macro gcc and clang accept instead of a rejected definition, and `int c\`-newline-
  `d;` preprocesses to `int cd;` instead of `int c\ d;`. Whether two tokens touched is now
  recorded by the lexer rather than recomputed from physical positions, and an argument
  substituted into a macro body takes the parameter's adjacency for `#` stringizing, as gcc does
  (an argument that is empty or newline-only leaves the parameter's whitespace behind).
- Operator names containing `<` are no longer truncated to the bare keyword `operator`.
  `operator<`, `operator<=`, `operator<<` and `operator<=>` had their `<...>` stripped as if it
  were a template-argument list, so a class's comparison operators collapsed into one symbol and
  those spelled as declarations were dropped entirely. Only what directly follows the keyword is
  read as part of the operator, so a conversion to a template type drops its arguments the way
  every other name does. A conversion operator's target keeps its arguments, though — see
  below — so `operator Vec<int>` and `operator ns::Vec<int>` are one member by scope alone.
- C++ conversion operators (`operator T()`) are indexed as `Cls::operator T` instead of being
  dropped (declarations) or named after a declarator fragment such as `Cls::()const`
  (definitions). A target type that is itself qualified (`operator std::string`) keeps the
  member in its class, one that is a function pointer keeps its `(*)`, and a definition records
  the type it converts to as its return type — subject to the existing rule that a class
  prototype's return type wins the merge with an out-of-line definition. The target drops the
  scopes the member itself sits in, so how far the author had to spell it out no longer decides
  which member it is: `ns::Handle::operator ns::S` and the in-class `operator S` are one symbol,
  where before the out-of-line definition stranded its declaration as an undefined phantom.
  Scopes the member does *not* sit in are kept, since no spelling of the declaration could have
  elided them — `C::operator a::S` and `C::operator b::S` stay two members, as do
  `operator Vec<int>` and `operator Vec<double>`, whose target arguments are likewise kept.
- A conversion operator to a function pointer records a function type. `operator void (*)()`
  lowered to a bare `Ptr(Void)`, indistinguishable from a conversion to `void *`, so nothing
  downstream could see the target as callable; it is now `Ptr(FnPtr{..})` — the same descriptor
  the `typedef void (*FP)(); operator FP()` spelling of that type already produced, so the two
  spellings intern as one type.
- A function whose return type is preceded by an unknown attribute macro (`FFI_EXPORT T f();`,
  with no `#define` in the include path) is indexed under its own name rather than under the
  return type glued to it, which hid the function from every call site. tree-sitter recovers
  such a declaration as a qualified name with a missing `::` at namespace scope, and as an
  `ERROR` node holding the leftover type inside a class body. A definition whose own name is
  qualified (`FFI_EXPORT void C::M() {}`) recovers a third way — a real `::`, with the class
  segment parked in an `ERROR` node — and was indexed as `void C::M`, leaving the body
  unreachable behind a phantom `C::M` that every call site resolved to. A macro that *trails*
  the declarator (`int j() const NOEXCEPT;`, `virtual int m() OVERRIDE;`) puts the declarator
  itself in the `ERROR` node and the macro beside it, so the member was named after the macro
  and a class annotating all its members alike collapsed into one symbol. All four are handled.
- An unknown attribute macro in front of a *conversion* operator costs it the `operator_cast`
  the two paths above key on: the keyword is stranded in an `ERROR` and the type it converts to
  is left standing where the declared name belongs. In a class body
  (`MACRO operator ns::S() const;`) the member was indexed as `C::S`, a name that collides with
  the class `S` itself and matches no declaration of the real member; out of one
  (`EXPORT C::operator int() {}`) the leading `C::` was read as the leftover-type half of a
  fabricated qualification and cut off, so the definition escaped to global scope as
  `operator int` and left its declaration stranded and undefined. Both are now spelled
  `C::operator int` / `C::operator ns::S`, the same as every other path spells them — the
  `ERROR` swallows the target's own scope along with the keyword, so it is read back from there
  rather than from the declarator, which holds only the last segment. The repair covers the
  in-class *definition* too, whose declarator likewise names the target rather than the member.
  A globally-qualified target (`operator ::ns::S`) keeps the space the keyword needs, and drops
  the leading `::` when what follows re-spells a scope the member sits in, so it meets the same
  member. On a target naming no scope of the member's, the `::` is kept: inside a `namespace n`
  that declares its own `S`, `operator ::S` converts to the global type and `operator S` to
  `n::S`, and collapsing the two put two bodies under one symbol. A pointer target
  (`MACRO operator char *() const;`) keeps a real `operator_name` and never had the problem —
  though see the phantom a macro *trailing* that shape used to declare, below.
- Both attribute-macro repairs above are found however deep the name nests them. A qualified
  name nests one `qualified_identifier` per scope it carries, and recovery leaves its mark at
  whichever level the fabricated segment landed on, so every scope either half of the name
  spells pushes that mark a level further from the top: `FFI_EXPORT n::S C::M() {}` was still
  indexed as `n::S C::M`, and `EXPORT ns::C::operator ns::S() {}` split into a defined
  `ns::C::operator ns::S` beside the undefined `ns::C::operator S` it should have merged with.
- A definition annotated by a trailing attribute macro (`void C::M() OVERRIDE {}`,
  `void M() ACQUIRE(mu_) {}`) is indexed under its own name. Only a *nullary* declarator
  recovers this way — with parameters it parses and the macro is the leftover — because
  `C::M()` is as good a call as it is a declarator, so tree-sitter parks the real declarator in
  an `ERROR` and hands the `declarator` field to the macro. The definition landed on the macro:
  a *defined* function called `OVERRIDE`, one per class that annotates a nullary member and all
  merging into one symbol, while the real member stayed undefined and its body unreachable.
- A member wearing an unknown macro on *both* sides
  (`EXPORT_API int Get(long) GUARDED_BY(mu_);`) is named by its declarator. tree-sitter puts the
  leftover return type and the real declarator in the *same* `ERROR` rather than one in it and
  one beside it, so the rule that tells the leading repair from the trailing one — does this
  `ERROR` hold a declarator? — said yes and the walk read the whole node, taking the leftover
  type first. Every member sharing a return type collapsed into `Cls::int` / `Cls::void`, and
  the real members survived only as externals synthesized by their call sites. Only the
  `ERROR`'s declarators are read now. A *conversion* operator wearing both
  (`EXPORT_API operator int() const GUARDED_BY(m);`) has its target swallowed by the same
  `ERROR`, with the trailing macro left outside it, so it was named `Cls::operator GUARDED_BY`;
  a declarator inside the `ERROR` is the target whenever there is one.
- A macro-annotated conversion operator keeps the whole spelling of its target, not just the
  target's last segment. The declarator the `ERROR` parks the target in was *walked*, and
  walking a declarator yields the one identifier it is named by — so a qualified target lost its
  own scope (`Cls::operator S` where every unannotated spelling is `Cls::operator ns::S`, and
  `S` collides with the class of that name), a template target lost its arguments
  (`Cls::operator Vec`, merging `Vec<int>` with `Vec<double>` and meeting neither's plain
  declaration), and a function-pointer target lost its `(*)` (`Cls::operator int`, the name of
  the class's conversion *to* `int`, so one symbol held two unrelated members). Only conversions
  to a primitive came out right, the walk's answer coinciding with the full target there. The
  target is now read from the source text between the keyword and the end of the declarator the
  member's own parameter list hangs off, so every target kind spells the same under a leading
  macro, a trailing one, or both, and meets the unannotated declaration and the out-of-class
  definition of the same member. A multi-word primitive target
  (`MACRO operator unsigned long() const;`) is recovered as loose keywords with no declarator
  anywhere, so the member-vs-data test read it as a data field and dropped it, or — with a
  trailing macro to fall through to — named it `C::operator unsigned long()const GUARDED_BY`; the
  keyword opening an `ERROR` now marks a conversion operator whatever the `ERROR` holds, and the
  target still ends where the parameter list starts. A *globally* qualified target behind a
  leading macro (`MACRO operator ::ns::S() const;`) is the one shape still not repaired: its
  `ERROR` lands at class-body level rather than inside the member, out of reach of the member
  walk. Recorded in `docs/ANALYSIS.md`.
- A macro *trailing* a conversion operator to a pointer or reference
  (`EXPORT_API operator Payload *() const GUARDED_BY(m);`) no longer declares a phantom member.
  That target kind is the one recovery keeps a real `function_declarator` for, and the cost is
  paid elsewhere: the member's `;` goes *missing* and the trailing macro is parked after it as a
  class-body `declaration` of its own, which registered an undefined `Cls::GUARDED_BY` — one per
  class annotating such a member, and the symbol every call site on any annotated member resolved
  to. A member closed by a missing `;` is one the author wrote no `;` after, so a `declaration`
  following it is the rest of that member and declares nothing. A genuinely separate member after
  a missing `;` recovers as a `field_declaration`, not a `declaration`, so the rule does not reach
  it; the one shape it does swallow is a ctor declaration after a member whose `;` the author
  actually forgot, which is not valid C++ either way.
- A member carrying a standard or GNU attribute (`[[nodiscard]]`, `[[gnu::pure]]`,
  `__attribute__((pure))`) keeps its own name. These parse cleanly — no error recovery involved
  — but each holds an identifier in front of the declaration and the member walk took it, so
  every annotated member of a class collapsed into one `Cls::nodiscard`. Conversion operators
  made this reachable for the first time, their declarations having only begun to register
  above.
- Multi-word operator names (`operator new`, `operator delete`) keep the space that separates
  their words; they were exported as `operatornew` / `operatordelete`. Call sites still resolve
  to them: the guard that decides which unresolved callee becomes a synthesized `external`
  rejects names containing a space — a rule calibrated to the old invariant that no name had
  one — and now exempts the space an operator keyword carries, so `::operator new(n)` keeps its
  callee and its edge.
- A macro-annotated destructor (`MACRO ~D();`) is indexed as `D::~D`. Recovery strands the `~`
  alone in an `ERROR` and leaves `D` standing as the declarator, so the member was filed under
  the *constructor* name `D::D` and classified `MethodKind::Ctor` — which dropped it from the
  override set `delete p` expands over, silently losing virtual-destructor edges.
- A member behind `__declspec(dllexport)` / `__declspec(dllimport)` keeps its own name. MSVC's
  spelling of the attribute collapse above hides in `ms_declspec_modifier`, which the standard
  and GNU attribute guard did not cover.
- A conversion operator whose target is a template type survives a leading macro
  (`MACRO operator Vec<int>() const;`). Recovery leaves the target's argument list attached to
  the declarator, making it a `template_method` that the member-vs-data test did not recognise,
  so the member was read as a data field and dropped from the index entirely.
- A pointer- or reference-returning definition behind a trailing macro
  (`void *C::P() OVERRIDE {}`) is indexed under its own name. The return type's pointer wraps
  the declarator, so the parked-declarator repair — which looked only at the definition's own
  children — never saw the `ERROR` one level down, and the body stayed under the macro.
- `TypeTable::int()` returned the id of `Bool`, so every entity built on the default scalar type
  — a function with no readable return type, a synthesized temporary — carried `bool` internally.
  `TypeTable::resolve_type_id` had drifted the same way, falling back to a raw `TypeId(5)` for
  a type the table never interned: that named `Long`, where the function means the `Unknown`
  placeholder. Every one of these is a non-pointer scalar, so no points-to result changes; the
  descriptors are simply now the ones the accessors name. `unknown()` joins `void()` and
  `int()` as a named accessor, and a unit test pins each of the three to its own type — plus one
  that pins the fallback itself, which no accessor test can reach. None of the three names a raw
  index any more: each asks the intern table for the id its descriptor was interned under, so
  reordering or extending the prelude cannot silently re-point them the way it did these two.
